//! What a coding agent's hooks report about its turns, and the status that
//! follows: working on a prompt, done, failed, or blocked on the user.
//!
//! Agents run `terminal agent-hook <agent>` for each hook event with the
//! event's JSON on stdin; the hook turns it into an [`AgentEvent`] for the
//! pane it runs in. Without hooks a pane knows only that an agent runs,
//! not how its turn is going, as in Warp.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest prompt or message kept from a hook, in characters.
const MAX_TEXT_CHARS: usize = 240;

/// Hook events this terminal listens to, per agent.
pub const CLAUDE_EVENTS: [&str; 6] = [
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Notification",
    "PermissionRequest",
    "PostToolUse",
];
pub const CODEX_EVENTS: [&str; 4] = [
    "UserPromptSubmit",
    "Stop",
    "PermissionRequest",
    "PostToolUse",
];

/// Where an agent's current turn stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTurn {
    InProgress,
    Done,
    Failed,
    /// Waiting on the user: a permission prompt or a question.
    Blocked,
}

/// Something an agent's hook reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    PromptSubmitted {
        prompt: String,
    },
    Stopped,
    Failed {
        message: Option<String>,
    },
    Blocked {
        message: Option<String>,
    },
    /// A tool finished, so a permission the agent waited on was answered.
    ToolCompleted,
}

/// A pane's agent turn, as its hooks have told it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnState {
    pub turn: Option<AgentTurn>,
    /// The prompt the turn works on.
    pub prompt: Option<String>,
    /// Why the agent is blocked or failed.
    pub message: Option<String>,
}

impl TurnState {
    /// Apply `event`, returning whether the turn's status changed.
    pub fn apply(&mut self, event: AgentEvent) -> bool {
        let before = self.turn;
        match event {
            AgentEvent::PromptSubmitted { prompt } => {
                self.turn = Some(AgentTurn::InProgress);
                self.prompt = (!prompt.is_empty()).then_some(prompt);
                self.message = None;
            }
            AgentEvent::Stopped => {
                self.turn = Some(AgentTurn::Done);
                self.message = None;
            }
            AgentEvent::Failed { message } => {
                self.turn = Some(AgentTurn::Failed);
                self.message = message;
            }
            AgentEvent::Blocked { message } => {
                self.turn = Some(AgentTurn::Blocked);
                self.message = message;
            }
            AgentEvent::ToolCompleted => {
                if self.turn == Some(AgentTurn::Blocked) {
                    self.turn = Some(AgentTurn::InProgress);
                    self.message = None;
                }
            }
        }
        self.turn != before
    }
}

/// The event a hook payload reports, if it is one worth showing. Events of
/// subagents are left out: only the main conversation's turn shows.
pub fn event_from_hook(payload: &Value) -> Option<AgentEvent> {
    if payload.get("agent_id").is_some_and(|id| !id.is_null()) {
        return None;
    }
    let text = |key: &str| payload.get(key).and_then(Value::as_str).map(clean_text);
    match payload.get("hook_event_name")?.as_str()? {
        "UserPromptSubmit" => Some(AgentEvent::PromptSubmitted {
            prompt: text("prompt").unwrap_or_default(),
        }),
        "Stop" => Some(AgentEvent::Stopped),
        "StopFailure" => Some(AgentEvent::Failed {
            message: text("error").or_else(|| text("message")),
        }),
        "PermissionRequest" => Some(AgentEvent::Blocked {
            message: text("tool_name").map(|tool| format!("Wants to use {tool}")),
        }),
        // Claude also notifies when it has merely sat idle at its prompt,
        // which is no news after its turn ended.
        "Notification" => {
            let idle = payload
                .get("notification_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "idle_prompt");
            let message = text("message");
            let waiting_for_prompt = message
                .as_deref()
                .is_some_and(|message| message.contains("waiting for your input"));
            (!idle && !waiting_for_prompt).then_some(AgentEvent::Blocked { message })
        }
        "PostToolUse" => Some(AgentEvent::ToolCompleted),
        _ => None,
    }
}

/// `text` on one line, without control characters, and cut to a length a
/// sidebar row can use.
pub fn clean_text(text: &str) -> String {
    let single_line: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let words: Vec<&str> = single_line.split_whitespace().collect();
    let joined = words.join(" ");
    match joined.char_indices().nth(MAX_TEXT_CHARS) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

/// A title an agent set for itself, without the spinner or status glyphs
/// it prefixes, which change many times a second.
pub fn clean_title(title: &str) -> &str {
    title.trim_start_matches(|character: char| !character.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hook_payloads_become_events() {
        let prompt = json!({ "hook_event_name": "UserPromptSubmit", "prompt": "fix\nthe build" });
        assert_eq!(
            event_from_hook(&prompt),
            Some(AgentEvent::PromptSubmitted {
                prompt: "fix the build".into()
            })
        );
        let permission = json!({ "hook_event_name": "PermissionRequest", "tool_name": "Bash" });
        assert_eq!(
            event_from_hook(&permission),
            Some(AgentEvent::Blocked {
                message: Some("Wants to use Bash".into())
            })
        );
        let idle = json!({
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
            "message": "Claude is waiting for your input"
        });
        assert_eq!(event_from_hook(&idle), None);
        let subagent = json!({ "hook_event_name": "Stop", "agent_id": "a1" });
        assert_eq!(event_from_hook(&subagent), None);
        assert_eq!(
            event_from_hook(&json!({ "hook_event_name": "PreCompact" })),
            None
        );
    }

    #[test]
    fn turns_follow_their_events() {
        let mut state = TurnState::default();
        assert!(state.apply(AgentEvent::PromptSubmitted {
            prompt: "add tests".into()
        }));
        assert!(state.apply(AgentEvent::Blocked { message: None }));
        assert!(state.apply(AgentEvent::ToolCompleted));
        assert_eq!(state.turn, Some(AgentTurn::InProgress));
        assert!(!state.apply(AgentEvent::ToolCompleted));
        assert!(state.apply(AgentEvent::Stopped));
        assert_eq!(state.turn, Some(AgentTurn::Done));
        assert_eq!(state.prompt.as_deref(), Some("add tests"));
    }

    #[test]
    fn titles_lose_their_spinners() {
        assert_eq!(clean_title("✳ Fix the build"), "Fix the build");
        assert_eq!(clean_title("⠋ Claude Code"), "Claude Code");
        assert_eq!(clean_title("zsh"), "zsh");
        assert_eq!(
            clean_text(&"a".repeat(300)).chars().count(),
            MAX_TEXT_CHARS + 1
        );
    }
}
