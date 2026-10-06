use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, Global};
use serde::{Deserialize, Serialize};

use crate::{
    agents::{self, AgentProfile},
    sidebar::SidebarSettings,
    status_bar::StatusBarSettings,
};

const RELOAD_INTERVAL: Duration = Duration::from_secs(1);

/// User-adjustable settings, persisted as JSON. Missing fields fall back to
/// their defaults so a partial file is valid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the active theme.
    pub theme: String,
    /// Terminal text size in points.
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Space between the terminal grid and the edges of its area, in points.
    pub terminal_padding: f32,
    pub status_bar: StatusBarSettings,
    pub sidebar: SidebarSettings,
    /// Inject prompt marks into zsh and bash for prompt navigation and
    /// command status. Applies to newly opened terminals.
    pub shell_integration: bool,
    /// Desktop notifications for background tabs while the window is
    /// inactive: program notifications and long commands finishing.
    pub notifications: bool,
    /// Ask before closing a pane, tab, or window with a running program.
    pub confirm_close: bool,
    /// Agents offered in the sidebar's `+` menu.
    pub agent_profiles: Vec<AgentProfile>,
    /// Start each agent in a new git worktree of the current repository.
    pub agent_worktrees: bool,
    /// Serve the control socket used by the `terminal` command and the MCP
    /// server. Applies at launch.
    pub control_api: bool,
    /// Show each command and its output as a block, with an input editor,
    /// in shells with integration. Applies to new terminals.
    pub command_blocks: bool,
    /// Stack command blocks up from the input, as in a classic terminal,
    /// rather than down from the top of the pane.
    pub blocks_from_bottom: bool,
    /// Resume the coding agent session a pane was running when it is
    /// restored after the session runtime stopped.
    pub resume_agents: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: crate::theme::DEFAULT_THEME.to_string(),
            font_size: 13.5,
            line_height: 1.35,
            terminal_padding: 0.,
            status_bar: StatusBarSettings::default(),
            sidebar: SidebarSettings::default(),
            shell_integration: true,
            notifications: true,
            confirm_close: true,
            agent_profiles: agents::default_profiles(),
            agent_worktrees: false,
            control_api: true,
            command_blocks: true,
            resume_agents: true,
            blocks_from_bottom: true,
        }
    }
}

pub const FONT_SIZE_RANGE: (f32, f32) = (8., 32.);
pub const LINE_HEIGHT_RANGE: (f32, f32) = (1., 2.);
pub const TERMINAL_PADDING_RANGE: (f32, f32) = (0., 48.);

impl Settings {
    /// Keep every value inside the range the renderer supports.
    fn clamped(mut self) -> Self {
        self.font_size = self.font_size.clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1);
        self.line_height = self
            .line_height
            .clamp(LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1);
        self.terminal_padding = self
            .terminal_padding
            .clamp(TERMINAL_PADDING_RANGE.0, TERMINAL_PADDING_RANGE.1);
        self.status_bar = self.status_bar.deduplicated();
        self
    }
}

/// The live settings for the app, kept in sync with the settings file.
pub struct SettingsStore {
    settings: Settings,
    path: PathBuf,
    modified: Option<SystemTime>,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// Load the settings file (creating it with defaults if absent) and
    /// start watching it for edits.
    pub fn init(cx: &mut App) {
        let path = settings_path();
        let settings = match read_settings(&path) {
            Ok(Some(settings)) => {
                // Write back keys the file is missing, so every setting
                // (such as agent profiles) is there to edit.
                let complete = serde_json::to_string_pretty(&settings).ok();
                let current = fs::read_to_string(&path).ok();
                if complete.as_deref().map(str::trim) != current.as_deref().map(str::trim)
                    && let Err(error) = write_settings(&path, &settings)
                {
                    log::warn!("failed to update {}: {error:#}", path.display());
                }
                settings
            }
            Ok(None) => {
                let settings = Settings::default();
                if let Err(error) = write_settings(&path, &settings) {
                    log::warn!("failed to create {}: {error:#}", path.display());
                }
                settings
            }
            Err(error) => {
                log::warn!("ignoring invalid {}: {error:#}", path.display());
                Settings::default()
            }
        };
        let modified = modified_time(&path);
        cx.set_global(Self {
            settings,
            path,
            modified,
        });

        cx.spawn(async move |cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(RELOAD_INTERVAL).await;
                cx.update(Self::reload_if_changed);
            }
        })
        .detach();
    }

    /// The app's configuration directory, e.g. `~/.config/terminal`.
    pub fn config_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(env!("CARGO_PKG_NAME"))
    }

    pub fn get(cx: &App) -> &Settings {
        &cx.global::<Self>().settings
    }

    pub fn path(cx: &App) -> PathBuf {
        cx.global::<Self>().path.clone()
    }

    /// Apply a change, clamp it, and save it to the settings file.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        let store = cx.global::<Self>();
        let mut settings = store.settings.clone();
        change(&mut settings);
        let settings = settings.clamped();
        if settings == store.settings {
            return;
        }

        let path = store.path.clone();
        if let Err(error) = write_settings(&path, &settings) {
            log::warn!("failed to save {}: {error:#}", path.display());
        }
        let store = cx.global_mut::<Self>();
        store.settings = settings;
        store.modified = modified_time(&path);
    }

    fn reload_if_changed(cx: &mut App) {
        let store = cx.global::<Self>();
        let modified = modified_time(&store.path);
        if modified == store.modified {
            return;
        }
        let path = store.path.clone();
        let current = store.settings.clone();
        match read_settings(&path) {
            Ok(settings) => {
                let settings = settings.unwrap_or_default();
                let store = cx.global_mut::<Self>();
                store.modified = modified;
                if settings != current {
                    store.settings = settings;
                }
            }
            Err(error) => {
                log::warn!("ignoring invalid {}: {error:#}", path.display());
                // Remember this version so the warning is not repeated
                // every interval until the file changes again.
                cx.global_mut::<Self>().modified = modified;
            }
        }
    }
}

fn settings_path() -> PathBuf {
    SettingsStore::config_dir().join("settings.json")
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// `Ok(None)` when the file does not exist yet.
fn read_settings(path: &Path) -> Result<Option<Settings>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read settings"),
    };
    let settings: Settings = serde_json::from_str(&contents).context("failed to parse settings")?;
    Ok(Some(settings.clamped()))
}

fn write_settings(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create settings directory")?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    fs::write(path, json + "\n").context("failed to write settings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_uses_defaults_for_missing_fields() {
        let settings: Settings = serde_json::from_str(r#"{ "font_size": 16 }"#).unwrap();
        assert_eq!(settings.font_size, 16.);
        assert_eq!(settings.line_height, Settings::default().line_height);
        assert_eq!(settings.terminal_padding, 0.);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings = Settings {
            theme: "One Dark".into(),
            font_size: 100.,
            line_height: 0.2,
            terminal_padding: -4.,
            ..Default::default()
        }
        .clamped();
        assert_eq!(settings.font_size, FONT_SIZE_RANGE.1);
        assert_eq!(settings.line_height, LINE_HEIGHT_RANGE.0);
        assert_eq!(settings.terminal_padding, TERMINAL_PADDING_RANGE.0);
    }

    #[test]
    fn settings_round_trip_through_file() {
        let path = std::env::temp_dir()
            .join(format!("settings-test-{}", std::process::id()))
            .join("settings.json");
        let settings = Settings {
            theme: "Gruvbox Dark".into(),
            font_size: 15.,
            line_height: 1.5,
            terminal_padding: 8.,
            ..Default::default()
        };
        write_settings(&path, &settings).unwrap();
        assert_eq!(read_settings(&path).unwrap(), Some(settings));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
