//! Saving and restoring the window layout: groups, tab customizations, and
//! each terminal's runtime identity, working directory, and agent session.
//!
//! The file is written atomically. One this build cannot read is backed up
//! before it is replaced, one from a newer build is never overwritten, and
//! a rolling series of copies lets an older layout be recovered by hand.
//! The safeguards follow herdr (github.com/herdrdev/herdr, Apache-2.0).

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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

/// The session format this build writes.
pub const SESSION_VERSION: u32 = 1;
/// Unreadable session files kept before they are replaced.
const BACKUPS_KEPT: usize = 3;
/// Rolling copies of the session kept, and how often one is taken.
const SNAPSHOTS_KEPT: usize = 48;
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Cleared when the saved session comes from a newer build, which this one
/// must not overwrite.
static WRITABLE: AtomicBool = AtomicBool::new(true);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// The format version; files from before versioning are version 1.
    #[serde(default = "default_version")]
    pub version: u32,
    pub entries: Vec<EntryState>,
    /// Position of the active tab in display order.
    #[serde(default)]
    pub active: Option<usize>,
    #[serde(default = "default_true")]
    pub sidebar_open: bool,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
}

fn default_version() -> u32 {
    1
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

/// The saved session, if there is a readable one this build understands.
pub fn load() -> Option<SessionState> {
    let path = session_path();
    let contents = fs::read_to_string(&path).ok()?;
    let version = serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|value| value.get("version")?.as_u64());
    if version.is_some_and(|version| version > u64::from(SESSION_VERSION)) {
        log::warn!(
            "{} is from a newer version of this app; it is left as it is",
            path.display()
        );
        WRITABLE.store(false, Ordering::Release);
        return None;
    }
    match serde_json::from_str(&contents) {
        Ok(state) => Some(state),
        Err(error) => {
            log::warn!("ignoring invalid {}: {error}", path.display());
            if let Err(error) = keep_copy(&backups_dir(), &contents, BACKUPS_KEPT, None) {
                log::warn!("failed to back up {}: {error:#}", path.display());
            }
            None
        }
    }
}

pub fn save(state: &SessionState) -> Result<()> {
    if !WRITABLE.load(Ordering::Acquire) {
        return Ok(());
    }
    let path = session_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create config directory")?;
    }
    // Dotfile managers link the file elsewhere; write through the link.
    let path = fs::canonicalize(&path).unwrap_or(path);
    let json = serde_json::to_string_pretty(state)? + "\n";
    // Write then rename so a crash mid-write never leaves a truncated file.
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, &json).context("failed to write session")?;
    fs::rename(&temporary, &path).context("failed to replace session")?;
    if let Err(error) = keep_copy(
        &snapshots_dir(),
        &json,
        SNAPSHOTS_KEPT,
        Some(SNAPSHOT_INTERVAL),
    ) {
        log::warn!("failed to keep a copy of the session: {error:#}");
    }
    Ok(())
}

fn backups_dir() -> PathBuf {
    SettingsStore::config_dir().join("session-backups")
}

fn snapshots_dir() -> PathBuf {
    SettingsStore::config_dir().join("session-snapshots")
}

/// Keep `contents` as a timestamped copy in `dir`, keeping the newest
/// `kept`. With an `interval`, no copy is taken sooner than that after the
/// last one, nor one identical to it.
fn keep_copy(dir: &Path, contents: &str, kept: usize, interval: Option<Duration>) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut copies: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    copies.sort();
    let now = SystemTime::now();
    if let (Some(interval), Some(newest)) = (interval, copies.last()) {
        let recent = fs::metadata(newest)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() < interval);
        if recent || fs::read_to_string(newest).is_ok_and(|newest| newest == contents) {
            return Ok(());
        }
    }
    let stamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let copy = dir.join(format!("session-{stamp:015}.json"));
    fs::write(&copy, contents)?;
    copies.push(copy);
    for old in &copies[..copies.len().saturating_sub(kept)] {
        let _ = fs::remove_file(old);
    }
    Ok(())
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
            version: SESSION_VERSION,
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
    fn copies_are_capped_spaced_and_not_repeated() {
        let dir = std::env::temp_dir().join(format!("terminal-copies-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for index in 0..5 {
            keep_copy(&dir, &format!("{index}"), 3, None).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut kept: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, vec!["2", "3", "4"]);
        // Within the interval, or identical to the last copy, none is taken.
        keep_copy(&dir, "5", 10, Some(Duration::from_secs(60))).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn minimal_session_uses_defaults() {
        let state: SessionState =
            serde_json::from_str(r#"{ "entries": [ { "type": "tab", "kind": "terminal" } ] }"#)
                .unwrap();
        assert!(state.sidebar_open);
        assert_eq!(state.version, 1);
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
