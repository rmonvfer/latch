use std::{fs, sync::Arc};

use gpui::{
    App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, Focusable, ScrollStrategy,
    SharedString, Stateful, Subscription, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use crate::{
    components::icon,
    process_info::shorten_home,
    settings::{
        FONT_SIZE_RANGE, LINE_HEIGHT_RANGE, Settings, SettingsStore, TERMINAL_PADDING_RANGE,
    },
    status_bar::{Side, StatusBarSettings, StatusItem},
    text_input::{TextInput, TextInputEvent},
    theme::{self, ActiveTheme, ActiveThemeExt, Appearance, Theme, ThemeRegistry, ThemeSource},
};

const THEME_ROW_HEIGHT: f32 = 30.;
const THEME_LIST_HEIGHT: f32 = 320.;
const THEME_LIST_WIDTH: f32 = 290.;

/// A numeric setting edited with − / + buttons.
struct Stepper {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    step: f32,
    range: (f32, f32),
    read: fn(&Settings) -> f32,
    write: fn(&mut Settings, f32),
    format: fn(f32) -> String,
}

const APPEARANCE: [Stepper; 3] = [
    Stepper {
        id: "font-size",
        label: "Font Size",
        description: "Size of terminal text.",
        step: 0.5,
        range: FONT_SIZE_RANGE,
        read: |settings| settings.font_size,
        write: |settings, value| settings.font_size = value,
        format: |value| format!("{value} pt"),
    },
    Stepper {
        id: "line-height",
        label: "Line Height",
        description: "Spacing between rows, as a multiple of the font size.",
        step: 0.05,
        range: LINE_HEIGHT_RANGE,
        read: |settings| settings.line_height,
        write: |settings, value| settings.line_height = value,
        format: |value| format!("{value:.2}×"),
    },
    Stepper {
        id: "terminal-padding",
        label: "Terminal Padding",
        description: "Space between the terminal text and the edges of its area.",
        step: 2.,
        range: TERMINAL_PADDING_RANGE,
        read: |settings| settings.terminal_padding,
        write: |settings, value| settings.terminal_padding = value,
        format: |value| format!("{value} px"),
    },
];

/// Which themes the picker lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppearanceFilter {
    All,
    Dark,
    Light,
}

impl AppearanceFilter {
    const ALL: [AppearanceFilter; 3] = [
        AppearanceFilter::All,
        AppearanceFilter::Dark,
        AppearanceFilter::Light,
    ];

    fn label(self) -> &'static str {
        match self {
            AppearanceFilter::All => "All",
            AppearanceFilter::Dark => "Dark",
            AppearanceFilter::Light => "Light",
        }
    }

    fn matches(self, theme: &Theme) -> bool {
        match self {
            AppearanceFilter::All => true,
            AppearanceFilter::Dark => theme.appearance == Appearance::Dark,
            AppearanceFilter::Light => theme.appearance == Appearance::Light,
        }
    }
}

/// The settings page, shown as a tab. Every change applies immediately and
/// is saved to the settings file.
pub struct SettingsPage {
    focus_handle: FocusHandle,
    theme_search: Entity<TextInput>,
    theme_list: UniformListScrollHandle,
    filter: AppearanceFilter,
    matches: Vec<Arc<Theme>>,
    /// The theme under the pointer, shown in the preview until it leaves.
    hovered: Option<Arc<Theme>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SettingsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let theme_search = cx.new(|cx| TextInput::new("Search themes…", cx));
        let subscriptions = vec![
            cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
            cx.observe_global::<ActiveTheme>(|_, cx| cx.notify()),
            cx.observe_global::<ThemeRegistry>(|page, cx| page.refresh_matches(cx)),
            cx.subscribe(&theme_search, |page, _, event: &TextInputEvent, cx| {
                if let TextInputEvent::Changed = event {
                    page.refresh_matches(cx);
                }
            }),
        ];
        let mut page = Self {
            focus_handle: cx.focus_handle(),
            theme_search,
            theme_list: UniformListScrollHandle::new(),
            filter: AppearanceFilter::All,
            matches: Vec::new(),
            hovered: None,
            _subscriptions: subscriptions,
        };
        page.refresh_matches(cx);
        page.reveal_active_theme(cx);
        page
    }

    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.theme_search.read(cx).text().trim().to_lowercase();
        let filter = self.filter;
        self.matches = ThemeRegistry::themes(cx)
            .iter()
            .filter(|theme| filter.matches(theme))
            .filter(|theme| query.is_empty() || theme.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        cx.notify();
    }

    fn set_filter(&mut self, filter: AppearanceFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.refresh_matches(cx);
        self.reveal_active_theme(cx);
    }

    fn reveal_active_theme(&self, cx: &App) {
        let active = &cx.theme().name;
        if let Some(index) = self.matches.iter().position(|theme| &theme.name == active) {
            self.theme_list
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    fn render_section_title(&self, title: &'static str, theme: &Theme) -> impl IntoElement {
        div()
            .pt(px(24.))
            .pb(px(8.))
            .text_size(theme::TEXT_SMALL)
            .text_color(theme.text_muted)
            .child(title)
    }

    fn render_theme_picker(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.matches.len();
        let border = theme.border;
        let previewed = self.hovered.clone().unwrap_or_else(|| cx.theme().clone());

        div()
            .flex()
            .flex_col()
            .rounded(theme::RADIUS_LG)
            .border_1()
            .border_color(border)
            .bg(theme.surface)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(38.))
                    .pl(px(12.))
                    .pr(px(6.))
                    .border_b_1()
                    .border_color(border)
                    .child(icon("search", theme::ICON_SMALL, theme.text_muted))
                    .child(div().flex_1().min_w_0().child(self.theme_search.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::TEXT_SMALL)
                            .text_color(theme.text_placeholder)
                            .child(SharedString::from(format!("{count}"))),
                    )
                    .child(self.render_filter(theme, cx)),
            )
            .child(
                div()
                    .flex()
                    .h(px(THEME_LIST_HEIGHT))
                    .child(
                        div()
                            .flex_none()
                            .w(px(THEME_LIST_WIDTH))
                            .h_full()
                            .border_r_1()
                            .border_color(border)
                            .child(
                                uniform_list(
                                    "theme-list",
                                    count,
                                    cx.processor(|page, range: std::ops::Range<usize>, _, cx| {
                                        let active = cx.theme().clone();
                                        range
                                            .filter_map(|index| page.matches.get(index).cloned())
                                            .map(|candidate| {
                                                page.render_theme_row(candidate, &active, cx)
                                            })
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .track_scroll(&self.theme_list)
                                .size_full(),
                            ),
                    )
                    .child(render_theme_preview(&previewed, theme)),
            )
    }

    fn render_filter(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = theme.ghost_hover;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .p(px(2.))
            .rounded(theme::RADIUS_SM)
            .bg(theme.element_background)
            .border_1()
            .border_color(theme.border_variant)
            .children(
                AppearanceFilter::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, filter)| {
                        let selected = filter == self.filter;
                        div()
                            .id(("theme-filter", index))
                            .flex()
                            .items_center()
                            .h(px(20.))
                            .px(px(8.))
                            .rounded(px(3.))
                            .cursor_pointer()
                            .text_size(theme::TEXT_SMALL)
                            .map(|segment| {
                                if selected {
                                    segment.bg(theme.ghost_selected).text_color(theme.text)
                                } else {
                                    segment
                                        .text_color(theme.text_muted)
                                        .hover(move |style| style.bg(hover))
                                }
                            })
                            .on_click(cx.listener(move |page, _: &ClickEvent, _, cx| {
                                page.set_filter(filter, cx);
                            }))
                            .child(filter.label())
                    }),
            )
    }

    fn render_theme_row(
        &self,
        candidate: Arc<Theme>,
        active: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = candidate.name == active.name;
        let hover = active.ghost_hover;
        let name = candidate.name.clone();
        let source = match candidate.source {
            ThemeSource::Bundled => "",
            ThemeSource::User => "Custom",
            ThemeSource::Ghostty => "Ghostty",
        };
        let hovered = candidate.clone();

        div()
            .id(SharedString::from(format!("theme-{}", candidate.name)))
            .w_full()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(THEME_ROW_HEIGHT))
            .pl(px(10.))
            .pr(px(10.))
            .cursor_pointer()
            .when(selected, |row| row.bg(active.ghost_selected))
            .when(!selected, |row| row.hover(move |style| style.bg(hover)))
            .on_hover(cx.listener(move |page, hovering: &bool, _, cx| {
                if *hovering {
                    page.hovered = Some(hovered.clone());
                } else if page
                    .hovered
                    .as_ref()
                    .is_some_and(|current| current.name == hovered.name)
                {
                    page.hovered = None;
                }
                cx.notify();
            }))
            .on_click(move |_: &ClickEvent, _window, cx| {
                let name = name.to_string();
                SettingsStore::update(cx, |settings| settings.theme = name);
            })
            .child(theme_chip(&candidate))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::TEXT_SMALL)
                    .text_color(if selected {
                        active.text
                    } else {
                        active.text_muted
                    })
                    .child(candidate.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(48.))
                    .flex()
                    .justify_end()
                    .text_size(px(11.))
                    .text_color(active.text_placeholder)
                    .child(source),
            )
            .child(
                div()
                    .flex_none()
                    .size(theme::ICON_SMALL)
                    .when(selected, |slot| {
                        slot.child(icon("check", theme::ICON_SMALL, active.text_accent))
                    }),
            )
    }

    fn render_stepper(
        &self,
        stepper: &'static Stepper,
        settings: &Settings,
        theme: &Theme,
    ) -> impl IntoElement {
        let value = (stepper.read)(settings);
        let at_min = value <= stepper.range.0;
        let at_max = value >= stepper.range.1;

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(theme.border_variant)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(theme::TEXT_DEFAULT)
                            .text_color(theme.text)
                            .child(stepper.label),
                    )
                    .child(
                        div()
                            .text_size(theme::TEXT_SMALL)
                            .text_color(theme.text_muted)
                            .child(stepper.description),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(2.))
                    .p(px(1.))
                    .rounded(theme::RADIUS_SM)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.element_background)
                    .child(step_button(
                        (stepper.id, 0),
                        "minus",
                        at_min,
                        theme,
                        move |settings| {
                            let next = round_to_step(
                                (stepper.read)(settings) - stepper.step,
                                stepper.step,
                            );
                            (stepper.write)(settings, next);
                        },
                    ))
                    .child(
                        div()
                            .w(px(60.))
                            .flex()
                            .justify_center()
                            .text_size(theme::TEXT_SMALL)
                            .font_family(theme::FONT_FAMILY)
                            .text_color(theme.text)
                            .child(SharedString::from((stepper.format)(value))),
                    )
                    .child(step_button(
                        (stepper.id, 1),
                        "plus",
                        at_max,
                        theme,
                        move |settings| {
                            let next = round_to_step(
                                (stepper.read)(settings) + stepper.step,
                                stepper.step,
                            );
                            (stepper.write)(settings, next);
                        },
                    )),
            )
    }
}

/// A miniature of a theme: its background with three accent bars.
fn theme_chip(candidate: &Theme) -> impl IntoElement {
    let terminal = &candidate.terminal;
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.))
        .h(px(18.))
        .px(px(4.))
        .rounded(px(4.))
        .border_1()
        .border_color(candidate.border)
        .bg(theme::to_hsla(terminal.background))
        .children(
            [
                terminal.ansi[1],
                terminal.ansi[2],
                terminal.ansi[4],
                terminal.foreground,
            ]
            .map(|color| {
                div()
                    .w(px(4.))
                    .h(px(8.))
                    .rounded(px(1.))
                    .bg(theme::to_hsla(color))
            }),
        )
}

/// A sample terminal session and palette drawn in `previewed`'s colors.
fn render_theme_preview(previewed: &Theme, chrome: &Theme) -> impl IntoElement {
    let terminal = &previewed.terminal;
    let color = |index: usize| theme::to_hsla(terminal.ansi[index]);
    let foreground = theme::to_hsla(terminal.foreground);
    let dim = foreground.opacity(0.55);
    let line = |spans: Vec<(&'static str, gpui::Hsla)>| {
        div().flex().h(px(18.)).children(
            spans
                .into_iter()
                .map(|(text, color)| div().text_color(color).child(text)),
        )
    };
    let appearance = match previewed.appearance {
        Appearance::Dark => "Dark",
        Appearance::Light => "Light",
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(theme::to_hsla(terminal.background))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(px(32.))
                .px(px(14.))
                .border_b_1()
                .border_color(previewed.border)
                .bg(previewed.title_bar)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(theme::TEXT_SMALL)
                        .text_color(previewed.text)
                        .child(previewed.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(previewed.text_muted)
                        .child(appearance),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .p(px(14.))
                .font_family(theme::FONT_FAMILY)
                .text_size(px(12.))
                .child(line(vec![
                    ("~/code/app", color(4)),
                    (" on ", dim),
                    (" main", color(5)),
                ]))
                .child(line(vec![("❯ ", color(2)), ("ls -a", foreground)]))
                .child(line(vec![
                    ("src/  ", color(4)),
                    ("assets/  ", color(4)),
                    ("Cargo.toml  ", foreground),
                    ("run.sh", color(2)),
                ]))
                .child(line(vec![
                    ("❯ ", color(2)),
                    ("git status --short", foreground),
                ]))
                .child(line(vec![(" M ", color(1)), ("src/theme.rs", foreground)]))
                .child(line(vec![
                    ("A  ", color(2)),
                    ("src/preview.rs", foreground),
                ]))
                .child(line(vec![("?? ", color(3)), ("notes.md", dim)]))
                .child(line(vec![("❯ ", color(2)), ("cargo test", foreground)]))
                .child(line(vec![
                    ("warning", color(3)),
                    (": unused import", foreground),
                ]))
                .child(line(vec![
                    ("test result: ", foreground),
                    ("ok", color(2)),
                    (". 30 passed", foreground),
                ]))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(18.))
                        .child(div().text_color(color(2)).child("❯ "))
                        .child(
                            div()
                                .w(px(7.))
                                .h(px(14.))
                                .bg(theme::to_hsla(terminal.cursor)),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(14.))
                .pb(px(14.))
                .children([0usize, 8].map(|start| {
                    div()
                        .flex()
                        .gap(px(3.))
                        .children((start..start + 8).map(|index| {
                            div()
                                .flex_1()
                                .h(px(12.))
                                .rounded(px(2.))
                                .border_1()
                                .border_color(chrome.border_variant.opacity(0.5))
                                .bg(color(index))
                        }))
                })),
        )
}

/// Snap to the step grid so repeated presses never accumulate float error.
fn round_to_step(value: f32, step: f32) -> f32 {
    (value / step).round() * step
}

fn step_button(
    id: (&'static str, usize),
    icon_name: &'static str,
    disabled: bool,
    theme: &Theme,
    change: impl Fn(&mut Settings) + 'static,
) -> impl IntoElement {
    let hover = theme.ghost_hover;
    let active = theme.ghost_selected;
    let button = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(20.))
        .rounded(px(3.));
    if disabled {
        button.child(icon(icon_name, theme::ICON_XSMALL, theme.text_disabled()))
    } else {
        button
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .active(move |style| style.bg(active))
            .on_click(move |_: &ClickEvent, _window, cx| {
                SettingsStore::update(cx, &change);
            })
            .child(icon(icon_name, theme::ICON_XSMALL, theme.text_muted))
    }
}

fn setting_row(label: &'static str, description: &'static str, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .py(px(10.))
        .border_b_1()
        .border_color(theme.border_variant)
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .text_size(theme::TEXT_DEFAULT)
                        .text_color(theme.text)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(theme::TEXT_SMALL)
                        .text_color(theme.text_muted)
                        .child(description),
                ),
        )
}

/// An on/off switch bound to a boolean setting.
fn switch(
    id: impl Into<ElementId>,
    on: bool,
    theme: &Theme,
    toggle: impl Fn(&mut Settings) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .w(px(30.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .cursor_pointer()
        .bg(if on {
            theme.text_accent
        } else {
            theme.element_background
        })
        .border_1()
        .border_color(if on { theme.text_accent } else { theme.border })
        .when(on, |track| track.justify_end())
        .on_click(move |_: &ClickEvent, _window, cx| SettingsStore::update(cx, &toggle))
        .child(div().size(px(12.)).rounded_full().bg(if on {
            theme.elevated_surface
        } else {
            theme.text_muted
        }))
}

fn render_status_bar_settings(config: &StatusBarSettings, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            setting_row(
                "Show Status Bar",
                "The bar along the bottom of the window.",
                theme,
            )
            .child(switch(
                "status-bar-visible",
                config.visible,
                theme,
                |settings| {
                    settings.status_bar.visible = !settings.status_bar.visible;
                },
            )),
        )
        .child(
            setting_row("Dividers", "Thin rules between status bar items.", theme).child(switch(
                "status-bar-dividers",
                config.dividers,
                theme,
                |settings| settings.status_bar.dividers = !settings.status_bar.dividers,
            )),
        )
        .children(
            StatusItem::ALL
                .into_iter()
                .enumerate()
                .map(|(index, item)| render_status_item_row(index, item, config, theme)),
        )
}

fn render_status_item_row(
    index: usize,
    item: StatusItem,
    config: &StatusBarSettings,
    theme: &Theme,
) -> impl IntoElement {
    let side = config.side_of(item);
    let list = match side {
        Some(Side::Left) => config.left.as_slice(),
        Some(Side::Right) => config.right.as_slice(),
        None => &[],
    };
    let position = list.iter().position(|existing| *existing == item);
    let can_move_up = position.is_some_and(|position| position > 0);
    let can_move_down = position.is_some_and(|position| position + 1 < list.len());
    let options = [
        (Some(Side::Left), "Left"),
        (Some(Side::Right), "Right"),
        (None, "Off"),
    ];

    setting_row(item.label(), item.description(), theme).child(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .p(px(1.))
                    .rounded(theme::RADIUS_SM)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.element_background)
                    .children(options.into_iter().enumerate().map(
                        |(option_index, (option, label))| {
                            let selected = option == side;
                            let hover = theme.ghost_hover;
                            div()
                                .id(("status-side", index * 3 + option_index))
                                .flex()
                                .items_center()
                                .h(px(20.))
                                .px(px(8.))
                                .rounded(px(3.))
                                .cursor_pointer()
                                .text_size(theme::TEXT_SMALL)
                                .map(|segment| {
                                    if selected {
                                        segment.bg(theme.ghost_selected).text_color(theme.text)
                                    } else {
                                        segment
                                            .text_color(theme.text_muted)
                                            .hover(move |style| style.bg(hover))
                                    }
                                })
                                .on_click(move |_: &ClickEvent, _window, cx| {
                                    SettingsStore::update(cx, |settings| {
                                        settings.status_bar.place(item, option);
                                    });
                                })
                                .child(label)
                        },
                    )),
            )
            .child(step_button(
                ("status-up", index),
                "arrow-up",
                !can_move_up,
                theme,
                move |settings| {
                    settings.status_bar.shift(item, -1);
                },
            ))
            .child(step_button(
                ("status-down", index),
                "arrow-down",
                !can_move_down,
                theme,
                move |settings| {
                    settings.status_bar.shift(item, 1);
                },
            )),
    )
}

fn text_button(id: &'static str, label: &'static str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    let hover = theme.ghost_hover;
    let active = theme.ghost_selected;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(24.))
        .px(px(8.))
        .rounded(theme::RADIUS_SM)
        .border_1()
        .border_color(theme.border)
        .bg(theme.element_background)
        .cursor_pointer()
        .text_size(theme::TEXT_SMALL)
        .text_color(theme.text)
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(active))
        .child(label)
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let settings = SettingsStore::get(cx).clone();
        let settings_path = SettingsStore::path(cx);
        let themes_dir = theme::user_theme_dir();
        let display_path = SharedString::from(shorten_home(&settings_path));

        div()
            .id("settings-page")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(theme.terminal_background())
            .font_family(theme::UI_FONT_FAMILY)
            .child(
                div()
                    .mx_auto()
                    .w_full()
                    .max_w(px(680.))
                    .px(px(32.))
                    .py(px(28.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(20.))
                            .text_color(theme.text)
                            .child("Settings"),
                    )
                    .child(self.render_section_title("Theme", &theme))
                    .child(self.render_theme_picker(&theme, cx))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .pt(px(8.))
                            .child(
                                text_button("open-themes-folder", "Open Themes Folder", &theme)
                                    .on_click(move |_: &ClickEvent, _window, cx| {
                                        if let Err(error) = fs::create_dir_all(&themes_dir) {
                                            log::warn!(
                                                "failed to create {}: {error}",
                                                themes_dir.display()
                                            );
                                        }
                                        cx.open_with_system(&themes_dir);
                                    }),
                            )
                            .child(
                                text_button("reload-theme-files", "Reload Themes", &theme)
                                    .on_click(|_: &ClickEvent, _window, cx| {
                                        theme::reload_themes(cx)
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(theme::TEXT_SMALL)
                                    .text_color(theme.text_placeholder)
                                    .child(
                                        "Zed .json themes and Ghostty theme files are supported.",
                                    ),
                            ),
                    )
                    .child(self.render_section_title("Appearance", &theme))
                    .children(
                        APPEARANCE
                            .iter()
                            .map(|stepper| self.render_stepper(stepper, &settings, &theme)),
                    )
                    .child(self.render_section_title("Status Bar", &theme))
                    .child(render_status_bar_settings(&settings.status_bar, &theme))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .pt(px(24.))
                            .child(
                                div()
                                    .id("open-settings-file")
                                    .min_w_0()
                                    .truncate()
                                    .cursor_pointer()
                                    .text_size(theme::TEXT_SMALL)
                                    .text_color(theme.text_muted)
                                    .hover({
                                        let accent = theme.text_accent;
                                        move |style| style.text_color(accent)
                                    })
                                    .on_click(move |_: &ClickEvent, _window, cx| {
                                        cx.open_with_system(&settings_path);
                                    })
                                    .child(display_path),
                            )
                            .child(
                                text_button("reset-settings", "Reset to Defaults", &theme)
                                    .on_click(|_: &ClickEvent, _window, cx| {
                                        SettingsStore::update(cx, |settings| {
                                            *settings = Settings::default()
                                        });
                                    }),
                            ),
                    ),
            )
    }
}
