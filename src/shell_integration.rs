//! Shell integration: scripts that make shells emit semantic prompt marks
//! (OSC 133), and parsing of the command marks they emit.
//!
//! zsh is pointed at our `.zshenv` through `ZDOTDIR`; it restores the user's
//! own `ZDOTDIR` and startup files before adding hooks. bash starts in POSIX
//! mode so that it runs our script from `ENV`, which leaves POSIX mode and
//! loads the user's startup files. fish 4 emits the marks on its own.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
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

fn install(dir: &Path) -> Result<()> {
    for (relative, contents) in SCRIPTS {
        let path = dir.join(relative);
        if fs::read_to_string(&path).is_ok_and(|existing| existing == contents) {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("failed to create integration directory")?;
        }
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
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
    fn installs_scripts_once() {
        let dir = std::env::temp_dir().join(format!("integration-test-{}", std::process::id()));
        install(&dir).unwrap();
        assert!(dir.join("zsh/.zshenv").is_file());
        assert!(dir.join("bash/integration.bash").is_file());
        install(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
