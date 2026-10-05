//! Shell integration: scripts that make shells emit semantic prompt marks
//! (OSC 133), and parsing of the command marks they emit.
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

/// A command boundary reported by the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandMark {
    /// OSC 133;C — a command started running.
    Started,
    /// OSC 133;D — the command finished, with its exit status if reported.
    Finished(Option<i32>),
}

const PREFIX: &[u8] = b"\x1b]133;";
/// Longest mark worth waiting for across reads.
const MAX_PENDING: usize = 64;

/// Finds command marks in PTY output, including marks split across reads.
#[derive(Default)]
pub struct MarkScanner {
    pending: Vec<u8>,
}

impl MarkScanner {
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<CommandMark> {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        let mut marks = Vec::new();
        let mut index = 0;

        while let Some(offset) = find(&buffer[index..], PREFIX) {
            let start = index + offset;
            let body_start = start + PREFIX.len();
            match terminator(&buffer[body_start..]) {
                Some((body_len, terminator_len)) => {
                    if let Some(mark) = parse_mark(&buffer[body_start..body_start + body_len]) {
                        marks.push(mark);
                    }
                    index = body_start + body_len + terminator_len;
                }
                None => {
                    if buffer.len() - start <= MAX_PENDING {
                        self.pending = buffer[start..].to_vec();
                    }
                    return marks;
                }
            }
        }

        // Keep a trailing partial prefix (e.g. a lone ESC) for the next read.
        let tail_start = buffer.len().saturating_sub(PREFIX.len() - 1).max(index);
        for split in tail_start..buffer.len() {
            if PREFIX.starts_with(&buffer[split..]) {
                self.pending = buffer[split..].to_vec();
                break;
            }
        }
        marks
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Length of the body and of its terminator (BEL or ESC \).
fn terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            0x07 => return Some((index, 1)),
            0x1b => {
                return match bytes.get(index + 1) {
                    Some(b'\\') => Some((index, 2)),
                    // A new escape before a terminator: drop the broken mark.
                    Some(_) => Some((index, 0)),
                    None => None,
                };
            }
            _ => {}
        }
    }
    None
}

fn parse_mark(body: &[u8]) -> Option<CommandMark> {
    let body = std::str::from_utf8(body).ok()?;
    let mut parts = body.split(';');
    match parts.next()? {
        "C" => Some(CommandMark::Started),
        "D" => Some(CommandMark::Finished(
            parts.next().and_then(|status| status.trim().parse().ok()),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_marks_with_both_terminators() {
        let mut scanner = MarkScanner::default();
        let marks = scanner.scan(b"out\x1b]133;A\x07$ \x1b]133;C\x1b\\ls\x1b]133;D;2\x07");
        assert_eq!(
            marks,
            vec![CommandMark::Started, CommandMark::Finished(Some(2))]
        );
    }

    #[test]
    fn marks_split_across_reads_are_joined() {
        let mut scanner = MarkScanner::default();
        assert!(scanner.scan(b"text\x1b]13").is_empty());
        assert!(scanner.scan(b"3;D;").is_empty());
        assert_eq!(
            scanner.scan(b"0\x07more"),
            vec![CommandMark::Finished(Some(0))]
        );
    }

    #[test]
    fn lone_escape_at_end_is_kept() {
        let mut scanner = MarkScanner::default();
        assert!(scanner.scan(b"abc\x1b").is_empty());
        assert_eq!(scanner.scan(b"]133;C\x07"), vec![CommandMark::Started]);
    }

    #[test]
    fn finish_without_status_is_reported() {
        let mut scanner = MarkScanner::default();
        assert_eq!(
            scanner.scan(b"\x1b]133;D\x07"),
            vec![CommandMark::Finished(None)]
        );
    }

    #[test]
    fn unterminated_garbage_is_not_kept_forever() {
        let mut scanner = MarkScanner::default();
        let mut long = b"\x1b]133;".to_vec();
        long.extend(std::iter::repeat_n(b'x', 200));
        assert!(scanner.scan(&long).is_empty());
        assert_eq!(scanner.scan(b"\x1b]133;C\x07"), vec![CommandMark::Started]);
    }

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
