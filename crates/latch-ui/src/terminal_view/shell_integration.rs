//! The shell integration scripts installed for this app's shells, and
//! whether new shells use them.

use std::path::PathBuf;

use gpui::{App, Global};

use latch_core::{paths, shell::integration};

use crate::settings::SettingsStore;

/// Where the integration scripts were installed, if installing worked.
pub struct ShellIntegration(Option<PathBuf>);

impl Global for ShellIntegration {}

impl ShellIntegration {
    /// Write the scripts to the config directory for shells to load.
    pub fn init(cx: &mut App) {
        let dir = paths::config_dir().join("shell-integration");
        let installed = match integration::install(&dir) {
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
