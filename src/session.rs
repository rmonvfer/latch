//! Saving and restoring the window layout: groups, tab customizations, and
//! each terminal's runtime identity and working directory.

use std::{fs, path::PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{
    pane_group::PaneState,
    settings::SettingsStore,
    tabs::{TabColor, TabStyle},
    theme,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabKind {
    Terminal,
    Settings,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    pub kind: TabKind,
    #[serde(default)]
    pub hidden: bool,
    /// The pane arrangement of a terminal tab.
    #[serde(default)]
    pub panes: Option<PaneState>,
    #[serde(default)]
    pub style: TabStyle,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupState {
    pub name: String,
    #[serde(default)]
    pub color: Option<TabColor>,
    #[serde(default)]
    pub collapsed: bool,
    pub tabs: Vec<TabState>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryState {
    Tab(TabState),
    Group(GroupState),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub entries: Vec<EntryState>,
    /// Position of the active tab in display order.
    #[serde(default)]
    pub active: Option<usize>,
    #[serde(default = "default_true")]
    pub sidebar_open: bool,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
}

fn default_true() -> bool {
    true
}

fn default_sidebar_width() -> f32 {
    f32::from(theme::SIDEBAR_WIDTH)
}

pub fn session_path() -> PathBuf {
    SettingsStore::config_dir().join("session.json")
}

/// The saved session, if there is a readable one.
pub fn load() -> Option<SessionState> {
    let path = session_path();
    let contents = fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&contents) {
        Ok(state) => Some(state),
        Err(error) => {
            log::warn!("ignoring invalid {}: {error}", path.display());
            None
        }
    }
}

pub fn save(state: &SessionState) -> Result<()> {
    let path = session_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create config directory")?;
    }
    let json = serde_json::to_string_pretty(state)?;
    // Write then rename so a crash mid-write never leaves a truncated file.
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, json + "\n").context("failed to write session")?;
    fs::rename(&temporary, &path).context("failed to replace session")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_resume::{AgentSession, ResumableAgent},
        pane_tree::Axis,
        tabs::TabIcon,
    };

    #[test]
    fn session_round_trips_through_json() {
        let state = SessionState {
            entries: vec![
                EntryState::Tab(TabState {
                    kind: TabKind::Terminal,
                    hidden: true,
                    panes: Some(PaneState::Split {
                        axis: Axis::Horizontal,
                        ratios: vec![0.6, 0.4],
                        children: vec![
                            PaneState::Terminal {
                                session_id: Some(42),
                                cwd: Some(PathBuf::from("/tmp")),
                                agent: Some(
                                    AgentSession::new(ResumableAgent::Claude, "s-1").unwrap(),
                                ),
                            },
                            PaneState::Terminal {
                                session_id: Some(43),
                                cwd: None,
                                agent: None,
                            },
                        ],
                    }),
                    style: TabStyle {
                        name: Some("api".into()),
                        color: Some(TabColor::Green),
                        icon: Some(TabIcon::Server),
                        pinned: true,
                        worktree: Some(PathBuf::from("/repo-worktrees/claude-code-1")),
                    },
                }),
                EntryState::Group(GroupState {
                    name: "Infra".into(),
                    color: Some(TabColor::Blue),
                    collapsed: true,
                    tabs: vec![TabState {
                        kind: TabKind::Settings,
                        hidden: false,
                        panes: None,
                        style: TabStyle::default(),
                    }],
                }),
            ],
            active: Some(1),
            sidebar_open: false,
            sidebar_width: 320.,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<SessionState>(&json).unwrap(), state);
    }

    #[test]
    fn minimal_session_uses_defaults() {
        let state: SessionState =
            serde_json::from_str(r#"{ "entries": [ { "type": "tab", "kind": "terminal" } ] }"#)
                .unwrap();
        assert!(state.sidebar_open);
        assert_eq!(state.sidebar_width, f32::from(theme::SIDEBAR_WIDTH));
        assert_eq!(state.active, None);
        let EntryState::Tab(tab) = &state.entries[0] else {
            panic!("expected a tab");
        };
        assert_eq!(tab.style, TabStyle::default());
        assert!(!tab.hidden);
    }

    #[test]
    fn hidden_sessions_preserve_identity_without_an_active_view() {
        let json = r#"{
            "entries": [{
                "type": "tab",
                "kind": "terminal",
                "hidden": true,
                "panes": { "type": "terminal", "session_id": 9001, "cwd": "/tmp" }
            }],
            "active": null
        }"#;
        let saved: SessionState = serde_json::from_str(json).unwrap();
        let restored: SessionState =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert!(restored.active.is_none());
        let EntryState::Tab(tab) = &restored.entries[0] else {
            panic!("expected a terminal tab");
        };
        assert!(tab.hidden);
        assert!(matches!(
            &tab.panes,
            Some(PaneState::Terminal { session_id: Some(9001), cwd, agent: None })
                if cwd.as_deref() == Some(std::path::Path::new("/tmp"))
        ));
    }
}
