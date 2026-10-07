//! Where the program keeps its files, and how paths are shown.

use std::path::{Path, PathBuf};

/// The name of the executable, its command line interface, and its
/// configuration directory.
pub const PROGRAM_NAME: &str = "terminal";

/// The app's configuration directory, e.g. `~/.config/terminal`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(PROGRAM_NAME)
}

/// Replace the home directory prefix with `~`.
pub fn shorten_home(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}
