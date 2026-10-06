//! Resuming coding agents in restored panes: which session an agent runs,
//! how to resume it, and the hooks that let agents report their sessions.
//!
//! Agents report their session through a `SessionStart` hook that runs
//! `terminal agent-hook <agent>` with the hook's JSON on stdin. A restored
//! pane types the resume command into its fresh shell, so quitting the
//! agent leaves a usable shell. The approach follows herdr
//! (github.com/herdrdev/herdr, Apache-2.0).

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Longest session id accepted from a hook.
const MAX_SESSION_ID_LEN: usize = 128;

/// Agents whose sessions can be resumed, by the name their hooks report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumableAgent {
    Claude,
    Codex,
}

impl ResumableAgent {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// The agent session a pane runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent: ResumableAgent,
    pub session_id: String,
}

impl AgentSession {
    /// A session with a plausible id: letters, digits, `-`, `_`, `.`, so it
    /// is safe to type into a shell.
    pub fn new(agent: ResumableAgent, session_id: &str) -> Option<Self> {
        let valid = !session_id.is_empty()
            && session_id.len() <= MAX_SESSION_ID_LEN
            && !session_id.starts_with('-')
            && session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        valid.then(|| Self {
            agent,
            session_id: session_id.to_string(),
        })
    }

    /// The command that picks the session up again.
    pub fn resume_command(&self) -> String {
        match self.agent {
            ResumableAgent::Claude => format!("claude --resume {}", self.session_id),
            ResumableAgent::Codex => format!("codex resume {}", self.session_id),
        }
    }
}

/// The session a `SessionStart` hook payload names, if it is one worth
/// resuming: not another event, and not a subagent's.
pub fn session_from_hook(agent: ResumableAgent, payload: &Value) -> Option<AgentSession> {
    let event = payload.get("hook_event_name").and_then(Value::as_str);
    if event.is_some_and(|event| event != "SessionStart") {
        return None;
    }
    if payload.get("agent_id").is_some_and(|id| !id.is_null()) {
        return None;
    }
    AgentSession::new(agent, payload.get("session_id")?.as_str()?)
}

/// The command agents run as their hook.
fn hook_command(agent: ResumableAgent) -> Result<String> {
    let exe = std::env::current_exe().context("cannot locate this program")?;
    let exe = exe
        .to_str()
        .context("this program's path is not valid UTF-8")?;
    let name = match agent {
        ResumableAgent::Claude => "claude",
        ResumableAgent::Codex => "codex",
    };
    Ok(format!(
        "'{}' agent-hook {name}",
        exe.replace('\'', "'\\''")
    ))
}

/// Add a `SessionStart` command hook to a hooks config (Claude's
/// `settings.json` and Codex's `hooks.json` share the shape), replacing an
/// earlier one of ours and keeping every other hook.
fn add_session_hook(config: &mut Value, command: &str) -> Result<()> {
    let Some(root) = config.as_object_mut() else {
        bail!("the config is not a JSON object");
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("the config's hooks are not an object");
    };
    let session_start = hooks
        .entry("SessionStart")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(entries) = session_start.as_array_mut() else {
        bail!("the config's SessionStart hooks are not a list");
    };
    let ours = |hook: &Value| {
        hook.get("command")
            .and_then(Value::as_str)
            .is_some_and(|existing| existing.contains(" agent-hook "))
    };
    for entry in entries.iter_mut() {
        if let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
            list.retain(|hook| !ours(hook));
        }
    }
    entries.retain(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|list| !list.is_empty())
    });
    entries.push(json!({
        "hooks": [{ "type": "command", "command": command, "timeout": 10 }]
    }));
    Ok(())
}

/// Turn on `[features] hooks` in a Codex `config.toml`, which Codex needs
/// before it runs hooks.
fn enable_codex_hooks(config: &str) -> String {
    let mut lines: Vec<String> = config.lines().map(str::to_string).collect();
    let mut in_features = false;
    let mut features_at = None;
    for (index, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
            if in_features {
                features_at = Some(index);
            }
            continue;
        }
        let key = trimmed.split('=').next().map(str::trim);
        if in_features && key == Some("hooks") {
            *line = "hooks = true".to_string();
            return lines.join("\n") + "\n";
        }
    }
    match features_at {
        Some(index) => lines.insert(index + 1, "hooks = true".to_string()),
        None => {
            if lines.last().is_some_and(|line| !line.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[features]".to_string());
            lines.push("hooks = true".to_string());
        }
    }
    lines.join("\n") + "\n"
}

/// Write `contents` to `path` through a temporary file, keeping a copy of
/// what was there.
fn replace_file(path: &Path, contents: &str) -> Result<()> {
    // Dotfile managers link configs elsewhere; write through the link.
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if path.exists() {
        fs::copy(&path, path.with_extension("terminal-backup"))
            .with_context(|| format!("failed to back up {}", path.display()))?;
    }
    let temporary = path.with_extension("terminal-tmp");
    fs::write(&temporary, contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("failed to replace {}", path.display()))
}

fn read_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&contents).with_context(|| format!("{} is not valid JSON", path.display()))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Set up `agent` to report its sessions to this terminal, returning the
/// files changed.
pub fn install(agent: ResumableAgent) -> Result<Vec<PathBuf>> {
    let command = hook_command(agent)?;
    match agent {
        ResumableAgent::Claude => {
            let dir = home()?.join(".claude");
            fs::create_dir_all(&dir)?;
            let settings = dir.join("settings.json");
            let mut config = read_json(&settings)?;
            add_session_hook(&mut config, &command)?;
            replace_file(&settings, &(serde_json::to_string_pretty(&config)? + "\n"))?;
            Ok(vec![settings])
        }
        ResumableAgent::Codex => {
            let dir = home()?.join(".codex");
            if !dir.is_dir() {
                bail!("Codex is not set up here: {} is missing", dir.display());
            }
            let hooks = dir.join("hooks.json");
            let mut config = read_json(&hooks)?;
            add_session_hook(&mut config, &command)?;
            replace_file(&hooks, &(serde_json::to_string_pretty(&config)? + "\n"))?;
            let toml = dir.join("config.toml");
            let existing = fs::read_to_string(&toml).unwrap_or_default();
            let enabled = enable_codex_hooks(&existing);
            if enabled != existing {
                replace_file(&toml, &enabled)?;
            }
            Ok(vec![hooks, toml])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_safe_to_type() {
        let session = AgentSession::new(ResumableAgent::Claude, "3f2a-9c_1.x").unwrap();
        assert_eq!(session.resume_command(), "claude --resume 3f2a-9c_1.x");
        assert_eq!(
            AgentSession::new(ResumableAgent::Codex, "abc")
                .unwrap()
                .resume_command(),
            "codex resume abc"
        );
        for bad in ["", "-rf", "a b", "a;b", "$(x)", &"a".repeat(200)] {
            assert!(
                AgentSession::new(ResumableAgent::Claude, bad).is_none(),
                "{bad}"
            );
        }
    }

    #[test]
    fn hooks_report_only_main_session_starts() {
        let start = json!({ "hook_event_name": "SessionStart", "session_id": "s1" });
        assert_eq!(
            session_from_hook(ResumableAgent::Claude, &start)
                .unwrap()
                .session_id,
            "s1"
        );
        let stop = json!({ "hook_event_name": "Stop", "session_id": "s1" });
        assert!(session_from_hook(ResumableAgent::Claude, &stop).is_none());
        let subagent = json!({ "session_id": "s1", "agent_id": "sub" });
        assert!(session_from_hook(ResumableAgent::Claude, &subagent).is_none());
    }

    #[test]
    fn our_hook_joins_existing_hooks_once() {
        let mut config = json!({
            "model": "opus",
            "hooks": { "SessionStart": [
                { "hooks": [{ "type": "command", "command": "echo mine" }] },
                { "hooks": [{ "type": "command", "command": "'/old/terminal' agent-hook claude" }] }
            ]}
        });
        add_session_hook(&mut config, "'/new/terminal' agent-hook claude").unwrap();
        add_session_hook(&mut config, "'/new/terminal' agent-hook claude").unwrap();
        let entries = config["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["hooks"][0]["command"], "echo mine");
        assert_eq!(
            entries[1]["hooks"][0]["command"],
            "'/new/terminal' agent-hook claude"
        );
        assert_eq!(config["model"], "opus");
    }

    #[test]
    fn codex_hooks_get_enabled() {
        assert_eq!(enable_codex_hooks(""), "[features]\nhooks = true\n");
        assert_eq!(
            enable_codex_hooks("model = \"o3\"\n[features]\nweb = true\n"),
            "model = \"o3\"\n[features]\nhooks = true\nweb = true\n"
        );
        assert_eq!(
            enable_codex_hooks("[features]\nhooks = false\n"),
            "[features]\nhooks = true\n"
        );
        assert_eq!(
            enable_codex_hooks("model = \"o3\"\n"),
            "model = \"o3\"\n\n[features]\nhooks = true\n"
        );
    }
}
