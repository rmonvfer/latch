//! Colors, fonts, and sizes for the app chrome and terminal.
//!
//! Themes come from three places: Zed-format theme files bundled with the
//! app (One, Ayu, Gruvbox; MIT licensed), the themes that ship with Ghostty
//! when it is installed, and user theme files in the config directory.
//! Ghostty themes only describe terminal colors, so their interface colors
//! are derived by blending the terminal background toward its foreground.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, Global, Hsla, Pixels, Rgba,
    SharedString, px,
};
use libghostty_vt::style::RgbColor;
use serde::Deserialize;

use crate::settings::SettingsStore;

/// Primary terminal font family, with fallbacks that ship with macOS.
pub const FONT_FAMILY: &str = "JetBrainsMono Nerd Font";
const FONT_FALLBACKS: [&str; 2] = ["JetBrains Mono", "Menlo"];
/// Interface chrome font, bundled with the app.
pub const UI_FONT_FAMILY: &str = "IBM Plex Sans";

pub const SIDEBAR_WIDTH: Pixels = px(260.);
pub const TITLEBAR_HEIGHT: Pixels = px(34.);
pub const STATUS_BAR_HEIGHT: Pixels = px(30.);
/// Horizontal space reserved for the macOS traffic lights.
pub const TRAFFIC_LIGHT_PADDING: Pixels = px(78.);

pub const TEXT_DEFAULT: Pixels = px(14.);
pub const TEXT_SMALL: Pixels = px(12.);

pub const ICON_SMALL: Pixels = px(14.);
pub const ICON_XSMALL: Pixels = px(12.);

pub const RADIUS_SM: Pixels = px(4.);
pub const RADIUS_LG: Pixels = px(8.);

pub const DEFAULT_THEME: &str = "One Dark";

const BUNDLED_THEME_FILES: [&str; 3] = [
    include_str!("../assets/themes/one.json"),
    include_str!("../assets/themes/ayu.json"),
    include_str!("../assets/themes/gruvbox.json"),
];
const GHOSTTY_THEME_DIRS: [&str; 1] =
    ["/Applications/Ghostty.app/Contents/Resources/ghostty/themes"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeSource {
    Bundled,
    User,
    Ghostty,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalColors {
    pub background: RgbColor,
    pub foreground: RgbColor,
    pub cursor: RgbColor,
    pub ansi: [RgbColor; 16],
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: SharedString,
    pub appearance: Appearance,
    pub source: ThemeSource,
    pub surface: Hsla,
    pub elevated_surface: Hsla,
    pub title_bar: Hsla,
    pub status_bar: Hsla,
    pub border: Hsla,
    pub border_variant: Hsla,
    pub element_background: Hsla,
    pub ghost_hover: Hsla,
    pub ghost_selected: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_placeholder: Hsla,
    pub text_accent: Hsla,
    pub terminal: TerminalColors,
}

impl Theme {
    pub fn text_disabled(&self) -> Hsla {
        self.text_placeholder.opacity(0.5)
    }

    pub fn terminal_background(&self) -> Hsla {
        to_hsla(self.terminal.background)
    }
}

/// Every theme available to pick from, in display order.
pub struct ThemeRegistry {
    themes: Vec<Arc<Theme>>,
}

impl Global for ThemeRegistry {}

impl ThemeRegistry {
    pub fn themes(cx: &App) -> &[Arc<Theme>] {
        &cx.global::<Self>().themes
    }

    fn find(&self, name: &str) -> Option<Arc<Theme>> {
        self.themes
            .iter()
            .find(|theme| theme.name.as_ref() == name)
            .cloned()
    }
}

/// The theme currently applied to the interface and terminals.
pub struct ActiveTheme(Arc<Theme>);

impl Global for ActiveTheme {}

pub trait ActiveThemeExt {
    fn theme(&self) -> &Arc<Theme>;
}

impl ActiveThemeExt for App {
    fn theme(&self) -> &Arc<Theme> {
        &self.global::<ActiveTheme>().0
    }
}

/// Load every theme, activate the one named in settings, and follow
/// settings changes. Must run after the settings store is initialized.
pub fn init(cx: &mut App) {
    let registry = ThemeRegistry {
        themes: load_all_themes(),
    };
    let active = resolve(&registry, &SettingsStore::get(cx).theme);
    cx.set_global(registry);
    cx.set_global(ActiveTheme(active));

    cx.observe_global::<SettingsStore>(|cx| {
        let name = SettingsStore::get(cx).theme.clone();
        if cx.theme().name.as_ref() == name {
            return;
        }
        let theme = resolve(cx.global::<ThemeRegistry>(), &name);
        cx.set_global(ActiveTheme(theme));
    })
    .detach();
}

/// Rescan theme files so newly added user themes show up.
pub fn reload_themes(cx: &mut App) {
    cx.set_global(ThemeRegistry {
        themes: load_all_themes(),
    });
}

fn resolve(registry: &ThemeRegistry, name: &str) -> Arc<Theme> {
    registry
        .find(name)
        .or_else(|| {
            log::warn!("theme \"{name}\" not found, using {DEFAULT_THEME}");
            registry.find(DEFAULT_THEME)
        })
        .or_else(|| registry.themes.first().cloned())
        .expect("bundled themes are always present")
}

fn load_all_themes() -> Vec<Arc<Theme>> {
    let mut themes: Vec<Arc<Theme>> = Vec::new();
    let mut push = |theme: Theme| {
        if !themes.iter().any(|existing| existing.name == theme.name) {
            themes.push(Arc::new(theme));
        }
    };

    for file in BUNDLED_THEME_FILES {
        match parse_zed_theme_family(file, ThemeSource::Bundled) {
            Ok(family) => family.into_iter().for_each(&mut push),
            Err(error) => log::error!("invalid bundled theme: {error:#}"),
        }
    }
    for theme in load_theme_dir(&user_theme_dir(), ThemeSource::User) {
        push(theme);
    }
    for dir in GHOSTTY_THEME_DIRS {
        for theme in load_theme_dir(Path::new(dir), ThemeSource::Ghostty) {
            push(theme);
        }
    }
    themes
}

pub fn user_theme_dir() -> PathBuf {
    SettingsStore::config_dir().join("themes")
}

/// Load a directory of theme files: `.json` files in Zed's format and any
/// other file in Ghostty's `key = value` format, named after the file.
fn load_theme_dir(dir: &Path, source: ThemeSource) -> Vec<Theme> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();

    let mut themes = Vec::new();
    for path in paths {
        let loaded = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .and_then(|contents| {
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    parse_zed_theme_family(&contents, source)
                } else {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    parse_ghostty_theme(&name, &contents, source).map(|theme| vec![theme])
                }
            });
        match loaded {
            Ok(family) => themes.extend(family),
            Err(error) => log::warn!("skipping theme {}: {error:#}", path.display()),
        }
    }
    themes
}

#[derive(Deserialize)]
struct ZedThemeFamily {
    themes: Vec<ZedTheme>,
}

#[derive(Deserialize)]
struct ZedTheme {
    name: String,
    appearance: String,
    style: serde_json::Map<String, serde_json::Value>,
}

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

fn parse_zed_theme_family(json: &str, source: ThemeSource) -> Result<Vec<Theme>> {
    let family: ZedThemeFamily = serde_json::from_str(json).context("invalid theme JSON")?;
    family
        .themes
        .into_iter()
        .map(|theme| {
            let name = theme.name.clone();
            zed_theme(theme, source).with_context(|| format!("theme \"{name}\""))
        })
        .collect()
}

fn zed_theme(theme: ZedTheme, source: ThemeSource) -> Result<Theme> {
    let style = &theme.style;
    let color = |key: &str| -> Result<Hsla> {
        style
            .get(key)
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("missing \"{key}\""))
            .and_then(parse_hex)
    };
    let rgb = |key: &str| -> Result<RgbColor> { color(key).map(to_rgb) };

    let mut ansi = [RgbColor { r: 0, g: 0, b: 0 }; 16];
    for (index, name) in ANSI_NAMES.iter().enumerate() {
        ansi[index] = rgb(&format!("terminal.ansi.{name}"))?;
        ansi[index + 8] = rgb(&format!("terminal.ansi.bright_{name}"))?;
    }
    let cursor = style
        .get("players")
        .and_then(|players| players.get(0))
        .and_then(|player| player.get("cursor"))
        .and_then(|cursor| cursor.as_str())
        .map(parse_hex)
        .transpose()?
        .map(to_rgb);
    let text_accent = color("text.accent")?;

    Ok(Theme {
        name: theme.name.into(),
        appearance: if theme.appearance == "light" {
            Appearance::Light
        } else {
            Appearance::Dark
        },
        source,
        surface: color("surface.background")?,
        elevated_surface: color("elevated_surface.background")?,
        title_bar: color("title_bar.background")?,
        status_bar: color("status_bar.background")?,
        border: color("border")?,
        border_variant: color("border.variant")?,
        element_background: color("element.background")?,
        ghost_hover: color("ghost_element.hover")?,
        ghost_selected: color("ghost_element.selected")?,
        text: color("text")?,
        text_muted: color("text.muted")?,
        text_placeholder: color("text.placeholder")?,
        text_accent,
        terminal: TerminalColors {
            background: rgb("terminal.background")?,
            foreground: rgb("terminal.foreground")?,
            cursor: cursor.unwrap_or_else(|| to_rgb(text_accent)),
            ansi,
        },
    })
}

fn parse_ghostty_theme(name: &str, contents: &str, source: ThemeSource) -> Result<Theme> {
    let mut background = None;
    let mut foreground = None;
    let mut cursor = None;
    let mut ansi: [Option<RgbColor>; 16] = [None; 16];

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "background" => background = Some(parse_rgb(value)?),
            "foreground" => foreground = Some(parse_rgb(value)?),
            "cursor-color" => cursor = Some(parse_rgb(value)?),
            "palette" => {
                let (index, color) = value
                    .split_once('=')
                    .ok_or_else(|| anyhow!("invalid palette entry \"{value}\""))?;
                let index: usize = index.trim().parse().context("invalid palette index")?;
                if index < 16 {
                    ansi[index] = Some(parse_rgb(color.trim())?);
                }
            }
            _ => {}
        }
    }

    let background = background.ok_or_else(|| anyhow!("missing background"))?;
    let foreground = foreground.ok_or_else(|| anyhow!("missing foreground"))?;
    let defaults = default_ansi();
    let ansi: [RgbColor; 16] = std::array::from_fn(|index| ansi[index].unwrap_or(defaults[index]));
    Ok(derive_theme(
        name,
        source,
        TerminalColors {
            background,
            foreground,
            cursor: cursor.unwrap_or(foreground),
            ansi,
        },
    ))
}

/// Build interface colors from terminal colors by stepping the background
/// toward the foreground, which works for light and dark palettes alike.
fn derive_theme(name: &str, source: ThemeSource, terminal: TerminalColors) -> Theme {
    let background = terminal.background;
    let foreground = terminal.foreground;
    let step = |amount: f32| to_hsla(mix(background, foreground, amount));
    let appearance = if luminance(background) > 0.5 {
        Appearance::Light
    } else {
        Appearance::Dark
    };

    Theme {
        name: SharedString::from(name.to_string()),
        appearance,
        source,
        surface: step(0.04),
        elevated_surface: step(0.06),
        title_bar: step(0.08),
        status_bar: step(0.08),
        border: step(0.16),
        border_variant: step(0.10),
        element_background: step(0.03),
        ghost_hover: step(0.10),
        ghost_selected: step(0.16),
        text: step(0.95),
        text_muted: step(0.65),
        text_placeholder: step(0.48),
        text_accent: to_hsla(terminal.ansi[12]),
        terminal,
    }
}

/// xterm's default 16 colors, used for palette entries a theme omits.
fn default_ansi() -> [RgbColor; 16] {
    [
        0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
        0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
    ]
    .map(hex_to_rgb)
}

fn mix(from: RgbColor, to: RgbColor, amount: f32) -> RgbColor {
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    RgbColor {
        r: channel(from.r, to.r),
        g: channel(from.g, to.g),
        b: channel(from.b, to.b),
    }
}

fn luminance(color: RgbColor) -> f32 {
    (0.2126 * color.r as f32 + 0.7152 * color.g as f32 + 0.0722 * color.b as f32) / 255.
}

/// Parse `#rgb`, `#rrggbb`, or `#rrggbbaa` (the leading `#` is optional).
fn parse_hex(value: &str) -> Result<Hsla> {
    let hex = value.trim().trim_start_matches('#');
    let expanded: String = if hex.len() == 3 {
        hex.chars().flat_map(|ch| [ch, ch]).collect()
    } else {
        hex.to_string()
    };
    let parsed =
        u32::from_str_radix(&expanded, 16).with_context(|| format!("invalid color \"{value}\""))?;
    let (rgb, alpha) = match expanded.len() {
        6 => (parsed, 0xff),
        8 => (parsed >> 8, parsed & 0xff),
        _ => return Err(anyhow!("invalid color \"{value}\"")),
    };
    Ok(Rgba {
        r: ((rgb >> 16) & 0xff) as f32 / 255.,
        g: ((rgb >> 8) & 0xff) as f32 / 255.,
        b: (rgb & 0xff) as f32 / 255.,
        a: alpha as f32 / 255.,
    }
    .into())
}

fn parse_rgb(value: &str) -> Result<RgbColor> {
    parse_hex(value).map(to_rgb)
}

pub fn terminal_font(weight: FontWeight, style: FontStyle) -> Font {
    Font {
        family: FONT_FAMILY.into(),
        features: FontFeatures::disable_ligatures(),
        fallbacks: Some(FontFallbacks::from_fonts(
            FONT_FALLBACKS
                .iter()
                .map(|family| family.to_string())
                .collect(),
        )),
        weight,
        style,
    }
}

pub fn hex_to_rgb(hex: u32) -> RgbColor {
    RgbColor {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

pub fn to_hsla(color: RgbColor) -> Hsla {
    Rgba {
        r: color.r as f32 / 255.,
        g: color.g as f32 / 255.,
        b: color.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn to_rgb(color: Hsla) -> RgbColor {
    let rgba = color.to_rgb();
    RgbColor {
        r: (rgba.r * 255.).round() as u8,
        g: (rgba.g * 255.).round() as u8,
        b: (rgba.b * 255.).round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_themes_all_parse() {
        let names: Vec<String> = BUNDLED_THEME_FILES
            .iter()
            .flat_map(|file| parse_zed_theme_family(file, ThemeSource::Bundled).unwrap())
            .map(|theme| theme.name.to_string())
            .collect();
        assert_eq!(names.len(), 11);
        assert!(names.contains(&DEFAULT_THEME.to_string()));
    }

    #[test]
    fn one_dark_matches_known_colors() {
        let themes = parse_zed_theme_family(BUNDLED_THEME_FILES[0], ThemeSource::Bundled).unwrap();
        let one_dark = themes
            .iter()
            .find(|theme| theme.name == DEFAULT_THEME)
            .unwrap();
        assert_eq!(one_dark.terminal.background, hex_to_rgb(0x282c34));
        assert_eq!(one_dark.terminal.ansi[1], hex_to_rgb(0xe06c75));
        assert_eq!(one_dark.terminal.cursor, hex_to_rgb(0x74ade8));
        assert_eq!(one_dark.appearance, Appearance::Dark);
    }

    #[test]
    fn ghostty_theme_parses_and_derives_ui() {
        let contents = "\
# comment
palette = 0=#45475a
palette = 1=#f38ba8
palette = 12=#74a8fc
background = #1e1e2e
foreground = #cdd6f4
cursor-color = #f5e0dc
";
        let theme =
            parse_ghostty_theme("Catppuccin Mocha", contents, ThemeSource::Ghostty).unwrap();
        assert_eq!(theme.terminal.background, hex_to_rgb(0x1e1e2e));
        assert_eq!(theme.terminal.ansi[1], hex_to_rgb(0xf38ba8));
        // Missing entries fall back to xterm defaults.
        assert_eq!(theme.terminal.ansi[2], hex_to_rgb(0x00cd00));
        assert_eq!(theme.terminal.cursor, hex_to_rgb(0xf5e0dc));
        assert_eq!(theme.appearance, Appearance::Dark);
        assert_eq!(to_rgb(theme.text_accent), hex_to_rgb(0x74a8fc));
    }

    #[test]
    fn light_ghostty_theme_is_detected() {
        let theme = parse_ghostty_theme(
            "Paper",
            "background = #f8f8f8\nforeground = #222222\n",
            ThemeSource::Ghostty,
        )
        .unwrap();
        assert_eq!(theme.appearance, Appearance::Light);
    }

    #[test]
    fn ghostty_theme_without_background_is_rejected() {
        assert!(
            parse_ghostty_theme("Broken", "foreground = #ffffff\n", ThemeSource::Ghostty).is_err()
        );
    }

    #[test]
    fn parses_short_and_alpha_hex() {
        assert_eq!(to_rgb(parse_hex("#abc").unwrap()), hex_to_rgb(0xaabbcc));
        let translucent = parse_hex("#74ade83d").unwrap();
        assert!((translucent.a - 0x3d as f32 / 255.).abs() < 0.001);
    }
}
