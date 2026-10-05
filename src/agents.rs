//! Recognizing coding agents running in a terminal and inferring whether
//! they are working or waiting for the user.

use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
    Gemini,
    Aider,
    OpenCode,
    Amp,
    Cursor,
    Goose,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Gemini => "Gemini",
            Agent::Aider => "Aider",
            Agent::OpenCode => "OpenCode",
            Agent::Amp => "Amp",
            Agent::Cursor => "Cursor Agent",
            Agent::Goose => "Goose",
        }
    }

    fn from_command(command: &str) -> Option<Self> {
        match command {
            "claude" => Some(Agent::ClaudeCode),
            "codex" => Some(Agent::Codex),
            "gemini" => Some(Agent::Gemini),
            "aider" => Some(Agent::Aider),
            "opencode" => Some(Agent::OpenCode),
            "amp" => Some(Agent::Amp),
            "cursor-agent" => Some(Agent::Cursor),
            "goose" => Some(Agent::Goose),
            _ => None,
        }
    }
}

/// Interpreters that agents commonly run under; their first script
/// argument names the actual program.
const LAUNCHERS: [&str; 6] = ["node", "bun", "deno", "python", "python3", "uv"];

/// The agent a process is running, judged from its executable and the
/// script an interpreter was given (`node …/bin/claude`).
pub fn detect(args: &[String]) -> Option<Agent> {
    let command_name = |arg: &String| {
        Path::new(arg)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut names = args.iter().map(command_name);
    let first = names.next()?;
    if let Some(agent) = Agent::from_command(&first) {
        return Some(agent);
    }
    let launcher = first.trim_end_matches(|ch: char| ch.is_ascii_digit() || ch == '.');
    if !LAUNCHERS.contains(&launcher) && !LAUNCHERS.contains(&first.as_str()) {
        return None;
    }
    // argv[0] repeats the interpreter; the script follows, after any flags.
    args.iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .take(3)
        .find_map(|arg| Agent::from_command(&command_name(arg)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Producing output.
    Working,
    /// Quiet after ringing the bell or sending a notification: the agent
    /// wants the user.
    NeedsInput,
    /// Quiet with nothing pending.
    Idle,
}

/// Output within this window counts as the agent working.
const ACTIVE_WINDOW: Duration = Duration::from_millis(1500);
/// Output this soon after a keystroke is taken to be its echo.
const ECHO_WINDOW: Duration = Duration::from_millis(300);

/// Activity a terminal has seen, for inferring agent status.
#[derive(Default)]
pub struct Activity {
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    /// A bell or notification arrived since the user last typed.
    wants_user: bool,
}

impl Activity {
    pub fn output(&mut self, at: Instant) {
        let echo = self
            .last_input
            .is_some_and(|input| at.saturating_duration_since(input) < ECHO_WINDOW);
        if !echo {
            self.last_output = Some(at);
        }
    }

    pub fn attention(&mut self) {
        self.wants_user = true;
    }

    pub fn user_input(&mut self, at: Instant) {
        self.last_input = Some(at);
        self.wants_user = false;
    }

    pub fn status(&self, now: Instant) -> AgentStatus {
        let recently_active = self
            .last_output
            .is_some_and(|last| now.saturating_duration_since(last) < ACTIVE_WINDOW);
        if self.wants_user {
            AgentStatus::NeedsInput
        } else if recently_active {
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn detects_native_binaries() {
        assert_eq!(
            detect(&args(&["/Users/me/.local/bin/claude", "claude"])),
            Some(Agent::ClaudeCode)
        );
        assert_eq!(
            detect(&args(&["/opt/homebrew/bin/codex", "codex", "--full-auto"])),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn detects_agents_behind_interpreters() {
        assert_eq!(
            detect(&args(&[
                "/usr/local/bin/node",
                "node",
                "--no-warnings",
                "/opt/homebrew/bin/gemini"
            ])),
            Some(Agent::Gemini)
        );
        assert_eq!(
            detect(&args(&[
                "/usr/bin/python3.12",
                "python3",
                "/Users/me/.local/bin/aider"
            ])),
            Some(Agent::Aider)
        );
    }

    #[test]
    fn detects_a_live_process_through_its_arguments() {
        let dir = std::env::temp_dir().join(format!("agent-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("claude");
        std::os::unix::fs::symlink("/bin/sleep", &fake).unwrap();
        let mut child = std::process::Command::new(&fake).arg("5").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let args = crate::process_info::process_args(child.id() as i32).unwrap();
        child.kill().unwrap();
        let _ = child.wait();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(detect(&args), Some(Agent::ClaudeCode));
    }

    #[test]
    fn ignores_other_programs() {
        assert_eq!(detect(&args(&["/usr/bin/vim", "vim", "claude"])), None);
        assert_eq!(
            detect(&args(&["/usr/local/bin/node", "node", "server.js"])),
            None
        );
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn status_follows_output_and_attention() {
        let start = Instant::now();
        let mut activity = Activity::default();
        assert_eq!(activity.status(start), AgentStatus::Idle);
        activity.output(start);
        assert_eq!(
            activity.status(start + Duration::from_millis(500)),
            AgentStatus::Working
        );
        assert_eq!(
            activity.status(start + Duration::from_secs(3)),
            AgentStatus::Idle
        );
        activity.attention();
        assert_eq!(
            activity.status(start + Duration::from_secs(3)),
            AgentStatus::NeedsInput
        );
        activity.user_input(start + Duration::from_secs(3));
        assert_eq!(
            activity.status(start + Duration::from_secs(3)),
            AgentStatus::Idle
        );
    }

    #[test]
    fn keystroke_echo_is_not_work() {
        let start = Instant::now();
        let mut activity = Activity::default();
        activity.user_input(start);
        activity.output(start + Duration::from_millis(20));
        assert_eq!(
            activity.status(start + Duration::from_millis(100)),
            AgentStatus::Idle
        );
        activity.output(start + Duration::from_millis(800));
        assert_eq!(
            activity.status(start + Duration::from_millis(900)),
            AgentStatus::Working
        );
    }
}
