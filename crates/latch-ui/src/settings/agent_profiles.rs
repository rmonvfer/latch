//! Ways to start a coding agent in a new tab, as listed in the `+` menu.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::workspace::tabs::{TabColor, TabIcon};

/// A way to start an agent in a new tab, as listed in the `+` menu.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub icon: Option<TabIcon>,
    #[serde(default)]
    pub color: Option<TabColor>,
}

impl AgentProfile {
    fn new(name: &str, command: &str, color: TabColor) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            icon: None,
            color: Some(color),
        }
    }

    /// The line typed into the shell to start the agent, with environment
    /// assignments first and every word quoted for POSIX shells.
    pub fn command_line(&self) -> String {
        let assignments = self
            .env
            .iter()
            .filter(|(key, _)| is_valid_name(key))
            .map(|(key, value)| format!("{key}={}", shell_quote(value)));
        let words = std::iter::once(&self.command)
            .chain(&self.args)
            .map(|word| shell_quote(word));
        assignments.chain(words).collect::<Vec<_>>().join(" ")
    }
}

pub fn default_profiles() -> Vec<AgentProfile> {
    vec![
        AgentProfile::new("Claude Code", "claude", TabColor::Yellow),
        AgentProfile::new("Codex", "codex", TabColor::Green),
        AgentProfile::new("Gemini", "gemini", TabColor::Blue),
        AgentProfile::new("Aider", "aider", TabColor::Magenta),
        AgentProfile::new("OpenCode", "opencode", TabColor::Cyan),
    ]
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Quote a word for POSIX shells, leaving plainly safe words as they are.
fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./=:@%+,".contains(ch));
    if safe {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_words_and_env() {
        let profile = AgentProfile {
            name: "Claude".into(),
            command: "claude".into(),
            args: vec!["--model".into(), "opus 5".into(), "it's".into()],
            env: BTreeMap::from([
                ("ANTHROPIC_LOG".to_string(), "debug".to_string()),
                ("bad name".to_string(), "x".to_string()),
            ]),
            icon: None,
            color: None,
        };
        assert_eq!(
            profile.command_line(),
            r"ANTHROPIC_LOG=debug claude --model 'opus 5' 'it'\''s'"
        );
    }
}
