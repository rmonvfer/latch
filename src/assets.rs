use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

/// Icons and fonts compiled into the binary.
pub struct Assets;

const ICONS: [(&str, &[u8]); 41] = [
    (
        "icons/arrow-down-to-line.svg",
        include_bytes!("../assets/icons/arrow-down-to-line.svg"),
    ),
    (
        "icons/arrow-up-to-line.svg",
        include_bytes!("../assets/icons/arrow-up-to-line.svg"),
    ),
    (
        "icons/bookmark.svg",
        include_bytes!("../assets/icons/bookmark.svg"),
    ),
    (
        "icons/ellipsis.svg",
        include_bytes!("../assets/icons/ellipsis.svg"),
    ),
    (
        "icons/list-filter.svg",
        include_bytes!("../assets/icons/list-filter.svg"),
    ),
    (
        "icons/arrow-down.svg",
        include_bytes!("../assets/icons/arrow-down.svg"),
    ),
    (
        "icons/arrow-up.svg",
        include_bytes!("../assets/icons/arrow-up.svg"),
    ),
    ("icons/bot.svg", include_bytes!("../assets/icons/bot.svg")),
    (
        "icons/check.svg",
        include_bytes!("../assets/icons/check.svg"),
    ),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    (
        "icons/chevron-down.svg",
        include_bytes!("../assets/icons/chevron-down.svg"),
    ),
    (
        "icons/chevron-right.svg",
        include_bytes!("../assets/icons/chevron-right.svg"),
    ),
    (
        "icons/circle-alert.svg",
        include_bytes!("../assets/icons/circle-alert.svg"),
    ),
    (
        "icons/circle-check.svg",
        include_bytes!("../assets/icons/circle-check.svg"),
    ),
    ("icons/code.svg", include_bytes!("../assets/icons/code.svg")),
    (
        "icons/database.svg",
        include_bytes!("../assets/icons/database.svg"),
    ),
    (
        "icons/file-pen-line.svg",
        include_bytes!("../assets/icons/file-pen-line.svg"),
    ),
    (
        "icons/flask-conical.svg",
        include_bytes!("../assets/icons/flask-conical.svg"),
    ),
    (
        "icons/folder.svg",
        include_bytes!("../assets/icons/folder.svg"),
    ),
    (
        "icons/git-branch.svg",
        include_bytes!("../assets/icons/git-branch.svg"),
    ),
    (
        "icons/globe.svg",
        include_bytes!("../assets/icons/globe.svg"),
    ),
    (
        "icons/keyboard.svg",
        include_bytes!("../assets/icons/keyboard.svg"),
    ),
    (
        "icons/layers.svg",
        include_bytes!("../assets/icons/layers.svg"),
    ),
    (
        "icons/loader-circle.svg",
        include_bytes!("../assets/icons/loader-circle.svg"),
    ),
    (
        "icons/minus.svg",
        include_bytes!("../assets/icons/minus.svg"),
    ),
    (
        "icons/package.svg",
        include_bytes!("../assets/icons/package.svg"),
    ),
    (
        "icons/palette.svg",
        include_bytes!("../assets/icons/palette.svg"),
    ),
    (
        "icons/panel-bottom.svg",
        include_bytes!("../assets/icons/panel-bottom.svg"),
    ),
    (
        "icons/panel-left.svg",
        include_bytes!("../assets/icons/panel-left.svg"),
    ),
    (
        "icons/pencil.svg",
        include_bytes!("../assets/icons/pencil.svg"),
    ),
    ("icons/pin.svg", include_bytes!("../assets/icons/pin.svg")),
    ("icons/plus.svg", include_bytes!("../assets/icons/plus.svg")),
    (
        "icons/refresh-cw.svg",
        include_bytes!("../assets/icons/refresh-cw.svg"),
    ),
    (
        "icons/rotate-ccw.svg",
        include_bytes!("../assets/icons/rotate-ccw.svg"),
    ),
    (
        "icons/rocket.svg",
        include_bytes!("../assets/icons/rocket.svg"),
    ),
    (
        "icons/search.svg",
        include_bytes!("../assets/icons/search.svg"),
    ),
    (
        "icons/server.svg",
        include_bytes!("../assets/icons/server.svg"),
    ),
    (
        "icons/settings.svg",
        include_bytes!("../assets/icons/settings.svg"),
    ),
    (
        "icons/settings-2.svg",
        include_bytes!("../assets/icons/settings-2.svg"),
    ),
    (
        "icons/terminal.svg",
        include_bytes!("../assets/icons/terminal.svg"),
    ),
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
];

const FONTS: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(icon_path, _)| *icon_path == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(icon_path, _)| icon_path.starts_with(path))
            .map(|(icon_path, _)| SharedString::from(*icon_path))
            .collect())
    }
}

/// Register the bundled UI font with the text system.
pub fn load_fonts(cx: &App) {
    let fonts = FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        log::warn!("failed to load bundled fonts: {error:#}");
    }
}
