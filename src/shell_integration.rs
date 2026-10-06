//! Shell integration: scripts that make shells emit semantic prompt marks
//! (OSC 133).
//!
//! zsh is pointed at our `.zshenv` through `ZDOTDIR`; it restores the user's
//! own `ZDOTDIR` and startup files before adding hooks. bash starts in POSIX
//! mode so that it runs our script from `ENV`, which leaves POSIX mode and
//! loads the user's startup files. fish 4 emits the marks on its own.

use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, Global};
use portable_pty::CommandBuilder;

use crate::settings::SettingsStore;

/// Environment variable carrying the id the scripts put in every hook.
pub const SESSION_ID_VARIABLE: &str = "TERMINAL_SESSION_ID";

/// The key the scripts bind to clear the shell's line editor, sent before
/// typing a command into it.
pub const CLEAR_LINE_KEY: &[u8] = b"\x1b[9876~";

/// The key the zsh script binds to report completions for the text typed
/// before it.
pub const COMPLETE_KEY: &[u8] = b"\x1b[9877~";

const SCRIPTS: [(&str, &str); 3] = [
    (
        "zsh/.zshenv",
        include_str!("../assets/shell-integration/zsh/.zshenv"),
    ),
    (
        "zsh/integration.zsh",
        include_str!("../assets/shell-integration/zsh/integration.zsh"),
    ),
    (
        "bash/integration.bash",
        include_str!("../assets/shell-integration/bash/integration.bash"),
    ),
];

/// Where the integration scripts were installed, if installing worked.
pub struct ShellIntegration(Option<PathBuf>);

impl Global for ShellIntegration {}

impl ShellIntegration {
    /// Write the scripts to the config directory for shells to load.
    pub fn init(cx: &mut App) {
        let dir = SettingsStore::config_dir().join("shell-integration");
        let installed = match install(&dir) {
            Ok(()) => Some(dir),
            Err(error) => {
                log::warn!("shell integration unavailable: {error:#}");
                None
            }
        };
        cx.set_global(Self(installed));
    }

    /// The script directory to use for new shells, or `None` when shell
    /// integration is off or unavailable.
    pub fn active_dir(cx: &App) -> Option<PathBuf> {
        if !SettingsStore::get(cx).shell_integration {
            return None;
        }
        cx.global::<Self>().0.clone()
    }
}

/// Write the scripts into `dir`, which every new shell then sources. The
/// directory and scripts must belong to the current user, be writable only
/// by them, and not be symlinks; otherwise integration is refused, since
/// anyone able to change these files could run code in every shell.
fn install(dir: &Path) -> Result<()> {
    ensure_private_dir(dir)?;
    for (relative, contents) in SCRIPTS {
        let path = dir.join(relative);
        let parent = path.parent().expect("scripts live in a subdirectory");
        ensure_private_dir(parent)?;
        if path.exists() || path.is_symlink() {
            ensure_private_file(&path)?;
            if fs::read_to_string(&path).is_ok_and(|existing| existing == contents) {
                continue;
            }
        }
        // Write then rename so the shell never sources a partial script, and
        // so a planted symlink at `path` is replaced rather than followed.
        let temporary = parent.join(format!(".{}.tmp", std::process::id()));
        fs::write(&temporary, contents)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("failed to install {}", path.display()))?;
    }
    Ok(())
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() && !dir.is_symlink() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let meta =
        fs::symlink_metadata(dir).with_context(|| format!("cannot inspect {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    check_owner_only(dir, &meta)
}

fn ensure_private_file(path: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    check_owner_only(path, &meta)
}

fn check_owner_only(path: &Path, meta: &fs::Metadata) -> Result<()> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        bail!("{} is owned by another user", path.display());
    }
    if meta.mode() & 0o022 != 0 {
        bail!("{} is writable by other users", path.display());
    }
    Ok(())
}

/// The command that starts the user's login shell, with integration
/// injected for shells we support.
pub fn shell_command(integration: Option<&Path>) -> CommandBuilder {
    let default = CommandBuilder::new_default_prog();
    let Some(dir) = integration else {
        return default;
    };
    let shell = default.get_shell();
    let name = Path::new(&shell)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    match name.as_str() {
        "zsh" => {
            let mut command = default;
            if let Some(original) = std::env::var_os("ZDOTDIR") {
                command.env("TERMINAL_ORIG_ZDOTDIR", original);
            }
            command.env("ZDOTDIR", dir.join("zsh"));
            command.env("TERMINAL_SHELL_INTEGRATION_DIR", dir);
            command
        }
        "bash" => {
            let mut command = CommandBuilder::new(&shell);
            command.arg("--posix");
            command.env("ENV", dir.join("bash/integration.bash"));
            command.env("TERMINAL_BASH_LOGIN", "1");
            command
        }
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_group_writable_directory() {
        let dir = std::env::temp_dir().join(format!("integration-perm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(install(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replaces_planted_symlink_instead_of_following_it() {
        let dir = std::env::temp_dir().join(format!("integration-link-{}", std::process::id()));
        let victim =
            std::env::temp_dir().join(format!("integration-victim-{}", std::process::id()));
        fs::write(&victim, "original").unwrap();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("zsh"))
            .unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("zsh/.zshenv")).unwrap();
        // A symlinked script is not a regular file, so installation refuses.
        assert!(install(&dir).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_file(&victim).unwrap();
    }

    #[test]
    fn installs_scripts_once() {
        let dir = std::env::temp_dir().join(format!("integration-test-{}", std::process::id()));
        install(&dir).unwrap();
        assert!(dir.join("zsh/.zshenv").is_file());
        assert!(dir.join("bash/integration.bash").is_file());
        install(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
