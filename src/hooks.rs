//! Lifecycle hooks sent by shell integration: hex-encoded JSON carried in a
//! private DCS (`ESC P $ d <hex> ESC \`).

use std::{collections::HashSet, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the shell reported once its startup files finished loading.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bootstrapped {
    pub shell: String,
    pub version: String,
    pub histfile: Option<PathBuf>,
    /// The shell's PATH, which its startup files may have changed.
    pub path: String,
    pub aliases: Vec<String>,
    pub functions: Vec<String>,
    pub builtins: Vec<String>,
}

/// State reported as the shell is about to draw a prompt.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Precmd {
    pub exit_code: i32,
    pub cwd: Option<PathBuf>,
    pub virtualenv: Option<String>,
    pub conda_env: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Hook {
    Bootstrapped(Bootstrapped),
    Precmd(Precmd),
    /// The shell is about to run this command line.
    Preexec {
        command: String,
    },
    CommandFinished {
        exit_code: i32,
    },
    /// Completions for the text typed before the completion key: each
    /// replaces `prefix`, the end of that text.
    Completions {
        prefix: String,
        matches: Vec<Completion>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    pub word: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct CompletionsValue {
    prefix: String,
    words: Vec<String>,
    #[serde(default)]
    descriptions: Vec<String>,
}

#[derive(Deserialize)]
struct Envelope {
    hook: String,
    session: String,
    value: Value,
}

/// Decode a hook's hex payload. Hooks from any session other than
/// `session` are rejected, so output that merely imitates a hook (a `cat`
/// of a hostile file) cannot fake command boundaries.
pub fn decode(hex: &[u8], session: &str) -> Option<Hook> {
    let bytes = decode_hex(hex)?;
    let envelope: Envelope = serde_json::from_slice(&bytes).ok()?;
    if envelope.session != session {
        return None;
    }
    let empty_to_none = |value: Option<String>| value.filter(|value| !value.is_empty());
    match envelope.hook.as_str() {
        "Bootstrapped" => {
            let mut bootstrapped: Bootstrapped = serde_json::from_value(envelope.value).ok()?;
            bootstrapped.histfile = bootstrapped
                .histfile
                .filter(|path| !path.as_os_str().is_empty());
            Some(Hook::Bootstrapped(bootstrapped))
        }
        "Precmd" => {
            let mut precmd: Precmd = serde_json::from_value(envelope.value).ok()?;
            precmd.virtualenv = empty_to_none(precmd.virtualenv);
            precmd.conda_env = empty_to_none(precmd.conda_env);
            Some(Hook::Precmd(precmd))
        }
        "Preexec" => Some(Hook::Preexec {
            command: envelope.value.get("command")?.as_str()?.to_string(),
        }),
        "CommandFinished" => Some(Hook::CommandFinished {
            exit_code: envelope.value.get("exit_code")?.as_i64()? as i32,
        }),
        "Completions" => {
            let value: CompletionsValue = serde_json::from_value(envelope.value).ok()?;
            let mut seen = HashSet::new();
            let matches = value
                .words
                .into_iter()
                .enumerate()
                .filter(|(_, word)| seen.insert(word.clone()))
                .map(|(index, word)| Completion {
                    word,
                    description: empty_to_none(value.descriptions.get(index).cloned()),
                })
                .collect();
            Some(Hook::Completions {
                prefix: value.prefix,
                matches,
            })
        }
        _ => None,
    }
}

fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.chunks(2)
        .map(|pair| {
            let digit = |byte: u8| (byte as char).to_digit(16);
            Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

#[cfg(test)]
pub fn encode(hook: &str, session: &str, value: &str) -> Vec<u8> {
    let message = format!(r#"{{"hook":"{hook}","session":"{session}","value":{value}}}"#);
    let hex: String = message.bytes().map(|byte| format!("{byte:02x}")).collect();
    format!("\x1bP$d{hex}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(sequence: &[u8]) -> &[u8] {
        &sequence[4..sequence.len() - 2]
    }

    #[test]
    fn decodes_every_hook() {
        let precmd = encode(
            "Precmd",
            "s1",
            r#"{"exit_code":1,"cwd":"/tmp","virtualenv":"","conda_env":"base"}"#,
        );
        assert_eq!(
            decode(payload(&precmd), "s1"),
            Some(Hook::Precmd(Precmd {
                exit_code: 1,
                cwd: Some(PathBuf::from("/tmp")),
                virtualenv: None,
                conda_env: Some("base".into()),
            }))
        );
        let preexec = encode("Preexec", "s1", r#"{"command":"echo \"hi\""}"#);
        assert_eq!(
            decode(payload(&preexec), "s1"),
            Some(Hook::Preexec {
                command: "echo \"hi\"".into()
            })
        );
        let completions = encode(
            "Completions",
            "s1",
            r#"{"prefix":"che","words":["checkout","cherry","checkout"],"descriptions":["switch branches","","switch branches"]}"#,
        );
        assert_eq!(
            decode(payload(&completions), "s1"),
            Some(Hook::Completions {
                prefix: "che".into(),
                matches: vec![
                    Completion {
                        word: "checkout".into(),
                        description: Some("switch branches".into()),
                    },
                    Completion {
                        word: "cherry".into(),
                        description: None,
                    },
                ],
            })
        );
        let finished = encode("CommandFinished", "s1", r#"{"exit_code":130}"#);
        assert_eq!(
            decode(payload(&finished), "s1"),
            Some(Hook::CommandFinished { exit_code: 130 })
        );
        let bootstrapped = encode(
            "Bootstrapped",
            "s1",
            r#"{"shell":"zsh","version":"5.9","histfile":"","aliases":["ll"],"functions":[],"builtins":["cd"]}"#,
        );
        let Some(Hook::Bootstrapped(bootstrapped)) = decode(payload(&bootstrapped), "s1") else {
            panic!("expected bootstrapped");
        };
        assert_eq!(bootstrapped.histfile, None);
        assert_eq!(bootstrapped.aliases, vec!["ll"]);
    }

    #[test]
    fn rejects_other_sessions_and_garbage() {
        let hook = encode("CommandFinished", "s1", r#"{"exit_code":0}"#);
        assert_eq!(decode(payload(&hook), "s2"), None);
        assert_eq!(decode(b"zz", "s1"), None);
        assert_eq!(decode(b"abc", "s1"), None);
    }
}
