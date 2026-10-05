//! The status bar: which items it shows, on which side, in what order, and
//! how each item renders.

use gpui::{AnyElement, ClickEvent, Context, SharedString, div, prelude::*, px};
use serde::{Deserialize, Serialize};

use crate::{
    components::{icon, status_divider, status_item},
    settings::SettingsStore,
    theme::{self, Theme},
    workspace::{OpenSettings, ToggleSidebar, Workspace},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusItem {
    SidebarToggle,
    Process,
    LastCommand,
    Directory,
    GitBranch,
    TabCount,
    Theme,
    FontSize,
    GridSize,
    Settings,
}

impl StatusItem {
    pub const ALL: [StatusItem; 10] = [
        StatusItem::SidebarToggle,
        StatusItem::Process,
        StatusItem::LastCommand,
        StatusItem::Directory,
        StatusItem::GitBranch,
        StatusItem::TabCount,
        StatusItem::Theme,
        StatusItem::FontSize,
        StatusItem::GridSize,
        StatusItem::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StatusItem::SidebarToggle => "Sidebar Toggle",
            StatusItem::Process => "Running Program",
            StatusItem::LastCommand => "Last Command",
            StatusItem::Directory => "Working Directory",
            StatusItem::GitBranch => "Git Branch",
            StatusItem::TabCount => "Tab Count",
            StatusItem::Theme => "Theme",
            StatusItem::FontSize => "Font Size",
            StatusItem::GridSize => "Grid Size",
            StatusItem::Settings => "Settings Button",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            StatusItem::SidebarToggle => "Shows or hides the tab sidebar.",
            StatusItem::Process => "Foreground program, highlighted while one is running.",
            StatusItem::LastCommand => {
                "Exit status and duration of the last command (needs shell integration)."
            }
            StatusItem::Directory => "The shell's current directory.",
            StatusItem::GitBranch => "Branch of the repository the shell is in.",
            StatusItem::TabCount => "Number of open tabs.",
            StatusItem::Theme => "Active theme; opens Settings.",
            StatusItem::FontSize => "Terminal font size with − / + buttons.",
            StatusItem::GridSize => "Terminal size in columns × rows.",
            StatusItem::Settings => "Opens Settings.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusBarSettings {
    pub visible: bool,
    /// Draw thin rules between items.
    pub dividers: bool,
    pub left: Vec<StatusItem>,
    pub right: Vec<StatusItem>,
}

impl Default for StatusBarSettings {
    fn default() -> Self {
        Self {
            visible: true,
            dividers: true,
            left: vec![StatusItem::Process, StatusItem::LastCommand],
            right: vec![
                StatusItem::GitBranch,
                StatusItem::Theme,
                StatusItem::GridSize,
                StatusItem::Settings,
            ],
        }
    }
}

impl StatusBarSettings {
    pub fn side_of(&self, item: StatusItem) -> Option<Side> {
        if self.left.contains(&item) {
            Some(Side::Left)
        } else if self.right.contains(&item) {
            Some(Side::Right)
        } else {
            None
        }
    }

    /// Show `item` at the end of `side`, or hide it with `None`.
    pub fn place(&mut self, item: StatusItem, side: Option<Side>) {
        if self.side_of(item) == side {
            return;
        }
        self.left.retain(|existing| *existing != item);
        self.right.retain(|existing| *existing != item);
        match side {
            Some(Side::Left) => self.left.push(item),
            Some(Side::Right) => self.right.push(item),
            None => {}
        }
    }

    /// Move `item` one step earlier (negative) or later (positive) on its side.
    pub fn shift(&mut self, item: StatusItem, delta: isize) {
        let list = match self.side_of(item) {
            Some(Side::Left) => &mut self.left,
            Some(Side::Right) => &mut self.right,
            None => return,
        };
        let Some(index) = list.iter().position(|existing| *existing == item) else {
            return;
        };
        let target = index as isize + delta;
        if target >= 0 && (target as usize) < list.len() {
            list.swap(index, target as usize);
        }
    }

    /// Drop repeated items so each appears at most once.
    pub fn deduplicated(mut self) -> Self {
        let mut seen = Vec::new();
        for list in [&mut self.left, &mut self.right] {
            list.retain(|item| {
                if seen.contains(item) {
                    false
                } else {
                    seen.push(*item);
                    true
                }
            });
        }
        self
    }
}

impl Workspace {
    pub(crate) fn render_status_bar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let config = SettingsStore::get(cx).status_bar.clone();
        if !config.visible {
            return None;
        }
        let left = self.render_status_side(&config.left, config.dividers, theme, cx);
        let right = self.render_status_side(&config.right, config.dividers, theme, cx);

        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(theme::STATUS_BAR_HEIGHT)
                // Wide enough to keep items clear of the window's rounded corners.
                .px(px(10.))
                .bg(theme.status_bar)
                .border_t_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .min_w_0()
                        .overflow_hidden()
                        .children(left),
                )
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(2.))
                        .children(right),
                ),
        )
    }

    fn render_status_side(
        &self,
        items: &[StatusItem],
        dividers: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for item in items {
            let Some(element) = self.render_status_entry(*item, theme, cx) else {
                continue;
            };
            if dividers && !elements.is_empty() {
                elements.push(status_divider(theme).into_any_element());
            }
            elements.push(element);
        }
        elements
    }

    /// One status item, or `None` when it has nothing to show right now
    /// (e.g. no git repository, or the Settings tab is active).
    fn render_status_entry(
        &self,
        item: StatusItem,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let metadata = self.active_metadata(cx);
        let element = match item {
            StatusItem::SidebarToggle => status_item("status-sidebar-toggle", theme)
                .when(self.sidebar_open, |item| item.bg(theme.ghost_selected))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.toggle_sidebar(&ToggleSidebar, window, cx);
                }))
                .child(icon(
                    "panel-left",
                    theme::ICON_SMALL,
                    if self.sidebar_open {
                        theme.text
                    } else {
                        theme.text_muted
                    },
                ))
                .into_any_element(),
            StatusItem::Process => {
                let metadata = metadata?;
                let process = metadata.process?;
                let running = metadata.running;
                status_label()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .size(px(6.))
                            .rounded_full()
                            .bg(if running {
                                theme.text_accent
                            } else {
                                theme.text_placeholder
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(if running {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(process),
                    )
                    .when(!running, |item| {
                        item.child(
                            div()
                                .flex_none()
                                .text_color(theme.text_placeholder)
                                .child("idle"),
                        )
                    })
                    .into_any_element()
            }
            StatusItem::LastCommand => {
                let metadata = metadata?;
                let (glyph, color, text) = match (metadata.command_started, metadata.last_command) {
                    (Some(started), _) => (
                        "●",
                        theme.text_accent,
                        format!("running {}", format_duration(started.elapsed())),
                    ),
                    (None, Some(outcome)) if outcome.failed() => (
                        "✗",
                        theme::to_hsla(theme.terminal.ansi[1]),
                        format!(
                            "exit {} · {}",
                            outcome.exit_code.unwrap_or_default(),
                            format_duration(outcome.duration)
                        ),
                    ),
                    (None, Some(outcome)) => (
                        "✓",
                        theme::to_hsla(theme.terminal.ansi[2]),
                        format_duration(outcome.duration),
                    ),
                    (None, None) => return None,
                };
                status_label()
                    .text_color(theme.text_muted)
                    .child(div().text_color(color).child(glyph))
                    .child(SharedString::from(text))
                    .into_any_element()
            }
            StatusItem::Directory => {
                let directory = metadata?.directory?;
                status_label()
                    .min_w_0()
                    .text_color(theme.text_muted)
                    .child(icon("folder", theme::ICON_XSMALL, theme.text_muted))
                    .child(div().truncate().child(directory))
                    .into_any_element()
            }
            StatusItem::GitBranch => {
                let branch = metadata?.branch?;
                status_label()
                    .max_w(px(180.))
                    .text_color(theme.text_muted)
                    .child(icon("git-branch", theme::ICON_XSMALL, theme.text_muted))
                    .child(div().truncate().child(branch))
                    .into_any_element()
            }
            StatusItem::TabCount => {
                let count = self.layout.ordered_tabs().len();
                status_label()
                    .text_color(theme.text_muted)
                    .child(icon("layers", theme::ICON_XSMALL, theme.text_muted))
                    .child(SharedString::from(if count == 1 {
                        "1 tab".to_string()
                    } else {
                        format!("{count} tabs")
                    }))
                    .into_any_element()
            }
            StatusItem::Theme => status_item("status-theme", theme)
                .gap(px(5.))
                .px(px(6.))
                .text_size(theme::TEXT_SMALL)
                .text_color(theme.text_muted)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.open_settings(&OpenSettings, window, cx);
                }))
                .child(icon("palette", theme::ICON_XSMALL, theme.text_muted))
                .child(theme.name.clone())
                .into_any_element(),
            StatusItem::FontSize => {
                let font_size = SettingsStore::get(cx).font_size;
                div()
                    .flex()
                    .items_center()
                    .text_size(theme::TEXT_SMALL)
                    .text_color(theme.text_muted)
                    .child(
                        status_item("status-font-smaller", theme)
                            .on_click(|_: &ClickEvent, _, cx| {
                                SettingsStore::update(cx, |settings| settings.font_size -= 0.5);
                            })
                            .child(icon("minus", theme::ICON_XSMALL, theme.text_muted)),
                    )
                    .child(
                        div()
                            .px(px(2.))
                            .font_family(theme::FONT_FAMILY)
                            .child(SharedString::from(format!("{font_size}pt"))),
                    )
                    .child(
                        status_item("status-font-larger", theme)
                            .on_click(|_: &ClickEvent, _, cx| {
                                SettingsStore::update(cx, |settings| settings.font_size += 0.5);
                            })
                            .child(icon("plus", theme::ICON_XSMALL, theme.text_muted)),
                    )
                    .into_any_element()
            }
            StatusItem::GridSize => {
                let (cols, rows) = self.active_grid_size(cx)?;
                status_label()
                    .font_family(theme::FONT_FAMILY)
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("{cols}×{rows}")))
                    .into_any_element()
            }
            StatusItem::Settings => status_item("status-settings", theme)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.open_settings(&OpenSettings, window, cx);
                }))
                .child(icon("settings", theme::ICON_SMALL, theme.text_muted))
                .into_any_element(),
        };
        Some(element)
    }
}

/// A compact duration: "0.4s", "12s", "3m 05s", "1h 02m".
pub fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs_f32();
    if seconds < 10. {
        format!("{seconds:.1}s")
    } else if seconds < 60. {
        format!("{}s", seconds as u64)
    } else if seconds < 3600. {
        let total = seconds as u64;
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        let total = seconds as u64;
        format!("{}h {:02}m", total / 3600, (total % 3600) / 60)
    }
}

/// Non-interactive status text with an optional leading icon.
fn status_label() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .h(px(22.))
        .px(px(6.))
        .text_size(theme::TEXT_SMALL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placing_moves_between_sides_and_hides() {
        let mut config = StatusBarSettings::default();
        config.place(StatusItem::Theme, Some(Side::Left));
        assert_eq!(config.side_of(StatusItem::Theme), Some(Side::Left));
        assert_eq!(config.left.last(), Some(&StatusItem::Theme));
        assert!(!config.right.contains(&StatusItem::Theme));

        config.place(StatusItem::Theme, None);
        assert_eq!(config.side_of(StatusItem::Theme), None);
    }

    #[test]
    fn shifting_stays_within_bounds() {
        let mut config = StatusBarSettings {
            left: vec![StatusItem::Process, StatusItem::SidebarToggle],
            ..Default::default()
        };
        config.shift(StatusItem::SidebarToggle, -1);
        assert_eq!(
            config.left,
            vec![StatusItem::SidebarToggle, StatusItem::Process]
        );
        config.shift(StatusItem::SidebarToggle, -1);
        assert_eq!(config.left[0], StatusItem::SidebarToggle);
        config.shift(StatusItem::SidebarToggle, 1);
        config.shift(StatusItem::SidebarToggle, 1);
        assert_eq!(config.left[1], StatusItem::SidebarToggle);
    }

    #[test]
    fn duplicates_keep_first_occurrence() {
        let config = StatusBarSettings {
            left: vec![StatusItem::Theme, StatusItem::Theme],
            right: vec![StatusItem::Theme, StatusItem::Settings],
            ..Default::default()
        }
        .deduplicated();
        assert_eq!(config.left, vec![StatusItem::Theme]);
        assert_eq!(config.right, vec![StatusItem::Settings]);
    }

    #[test]
    fn durations_are_compact() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(420)), "0.4s");
        assert_eq!(format_duration(Duration::from_secs(12)), "12s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 02m");
    }

    #[test]
    fn parses_from_settings_json() {
        let config: StatusBarSettings =
            serde_json::from_str(r#"{ "left": ["git_branch"], "dividers": false }"#).unwrap();
        assert_eq!(config.left, vec![StatusItem::GitBranch]);
        assert!(!config.dividers);
        assert!(config.visible);
        assert_eq!(config.right, StatusBarSettings::default().right);
    }
}
