//! Syntax highlighting for command lines: command words colored by whether
//! the shell can run them, plus flags, quoted strings, and operators.

use std::{
    collections::HashSet,
    fs,
    ops::Range,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use crate::hooks::Bootstrapped;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// A command the shell knows: alias, function, builtin, keyword, or an
    /// executable on PATH.
    Command,
    /// A command word that names nothing the shell can run.
    UnknownCommand,
    Flag,
    String,
    Operator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub range: Range<usize>,
    pub kind: TokenKind,
}

/// Reserved words that start or shape commands in zsh and bash.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "coproc", "repeat", "foreach", "end", "{", "}", "[[", "]]",
    "!",
];

/// Commands that run the command after them, which is highlighted too.
const PRECOMMANDS: &[&str] = &[
    "sudo",
    "doas",
    "env",
    "nohup",
    "time",
    "exec",
    "command",
    "builtin",
    "noglob",
    "nocorrect",
    "xargs",
    "!",
];

/// Names the shell can run.
#[derive(Clone, Debug, Default)]
pub struct CommandIndex {
    names: HashSet<String>,
}

impl CommandIndex {
    /// Index the shell's aliases, functions, builtins, and the executables
    /// on its PATH. Reads directories, so it belongs off the main thread.
    pub fn new(shell: &Bootstrapped) -> Self {
        let mut names: HashSet<String> = shell
            .aliases
            .iter()
            .chain(&shell.functions)
            .chain(&shell.builtins)
            .cloned()
            .collect();
        names.extend(KEYWORDS.iter().map(|keyword| keyword.to_string()));
        for directory in std::env::split_paths(&shell.path) {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                if is_executable(&entry.path()) {
                    names.insert(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Self { names }
    }

    #[cfg(test)]
    pub fn from_names(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|name| name.to_string()).collect(),
        }
    }

    /// Whether `word` runs something: a known name, or a path to an
    /// executable (relative to `cwd`).
    pub fn knows(&self, word: &str, cwd: Option<&Path>) -> bool {
        if word.contains('/') {
            let path = match word.strip_prefix("~/") {
                Some(rest) => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)),
                None => Some(match cwd {
                    Some(cwd) => cwd.join(word),
                    None => PathBuf::from(word),
                }),
            };
            return path.is_some_and(|path| is_executable(&path));
        }
        self.names.contains(word)
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| {
        metadata.is_dir() || (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

/// Tokens worth coloring in `text`, in order.
pub fn highlight(text: &str, index: &CommandIndex, cwd: Option<&Path>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut position = 0;
    let mut expect_command = true;
    while position < bytes.len() {
        let byte = bytes[position];
        if byte == b'\n' {
            expect_command = true;
            position += 1;
            continue;
        }
        if byte.is_ascii_whitespace() {
            position += 1;
            continue;
        }
        if byte == b'#' {
            // A comment runs to the end of the line.
            position = text[position..]
                .find('\n')
                .map_or(text.len(), |offset| position + offset);
            continue;
        }
        if b"|&;()<>".contains(&byte) {
            let start = position;
            while position < bytes.len() && b"|&;<>".contains(&bytes[position]) {
                position += 1;
            }
            position = position.max(start + 1);
            let operator = &text[start..position];
            if !operator.starts_with(['<', '>']) {
                expect_command = true;
            }
            tokens.push(Token {
                range: start..position,
                kind: TokenKind::Operator,
            });
            continue;
        }

        let (end, strings) = scan_word(text, position);
        let word = &text[position..end];
        let start = position;
        position = end;
        if expect_command {
            if is_assignment(word) {
                tokens.extend(strings);
                continue;
            }
            expect_command = PRECOMMANDS.contains(&word);
            if strings.is_empty() {
                // A word still being typed is not judged yet.
                let kind = if index.knows(word, cwd) {
                    Some(TokenKind::Command)
                } else if end < text.len() {
                    Some(TokenKind::UnknownCommand)
                } else {
                    None
                };
                tokens.extend(kind.map(|kind| Token {
                    range: start..end,
                    kind,
                }));
            } else {
                tokens.extend(strings);
            }
            continue;
        }
        if word.starts_with('-') && strings.is_empty() {
            tokens.push(Token {
                range: start..end,
                kind: TokenKind::Flag,
            });
        } else {
            tokens.extend(strings);
        }
    }
    tokens
}

/// End of the word starting at `start`, with the quoted strings inside it.
fn scan_word(text: &str, start: usize) -> (usize, Vec<Token>) {
    let bytes = text.as_bytes();
    let mut strings = Vec::new();
    let mut position = start;
    while position < bytes.len() {
        let byte = bytes[position];
        if byte.is_ascii_whitespace() || b"|&;()<>".contains(&byte) {
            break;
        }
        match byte {
            b'\\' => position = (position + 2).min(bytes.len()),
            b'\'' | b'"' => {
                let quote = byte;
                let opening = position;
                position += 1;
                while position < bytes.len() && bytes[position] != quote {
                    if quote == b'"' && bytes[position] == b'\\' {
                        position += 1;
                    }
                    position += 1;
                }
                position = (position + 1).min(bytes.len());
                strings.push(Token {
                    range: opening..position,
                    kind: TokenKind::String,
                });
            }
            _ => position += 1,
        }
    }
    (position, strings)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(&str, TokenKind)> {
        let index = CommandIndex::from_names(&["git", "ls", "grep", "sudo", "echo"]);
        highlight(text, &index, None)
            .into_iter()
            .map(|token| (&text[token.range], token.kind))
            .collect()
    }

    #[test]
    fn colors_commands_flags_strings_and_operators() {
        use TokenKind::*;
        assert_eq!(
            kinds("git log --oneline | grep 'fix bug' && nope x"),
            vec![
                ("git", Command),
                ("--oneline", Flag),
                ("|", Operator),
                ("grep", Command),
                ("'fix bug'", String),
                ("&&", Operator),
                ("nope", UnknownCommand),
            ]
        );
    }

    #[test]
    fn handles_assignments_precommands_and_redirections() {
        use TokenKind::*;
        assert_eq!(
            kinds("FOO=1 sudo ls > out.txt"),
            vec![("sudo", Command), ("ls", Command), (">", Operator)]
        );
        assert_eq!(
            kinds("echo \"a \\\" b\""),
            vec![("echo", Command), ("\"a \\\" b\"", String)]
        );
    }

    #[test]
    fn a_word_being_typed_is_not_marked_unknown() {
        assert_eq!(kinds("gi"), vec![]);
        assert_eq!(
            kinds("ls\ngi x"),
            vec![
                ("ls", TokenKind::Command),
                ("gi", TokenKind::UnknownCommand)
            ]
        );
    }

    #[test]
    fn paths_to_executables_are_commands() {
        let index = CommandIndex::default();
        let tokens = highlight("/bin/ls -l", &index, None);
        assert_eq!(tokens[0].kind, TokenKind::Command);
    }
}
