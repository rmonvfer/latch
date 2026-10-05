//! The settings page: the terminal's settings grouped into sections picked
//! from a left column. Every change applies immediately and is saved to the
//! settings file.

use std::{fs, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use gpui::{
    Animation, AnimationExt, AnyElement, App, ClickEvent, Context, Div, Entity, FocusHandle,
    Focusable, ScrollHandle, SharedString, Stateful, Subscription, Window, div, ease_in_out, point,
    prelude::*, px, relative,
};

use crate::{
    components::{button, icon},
    process_info::shorten_home,
    session,
    settings::{
        FONT_SIZE_RANGE, LINE_HEIGHT_RANGE, Settings, SettingsStore, TERMINAL_PADDING_RANGE,
    },
    sidebar::TabDensity,
    status_bar::{Side, StatusItem},
    text_input::{TextInput, TextInputEvent},
    theme::{self, ActiveTheme, ActiveThemeExt, Appearance, Theme, ThemeRegistry, ThemeSource},
};

const NAV_WIDTH: f32 = 208.;
const CONTENT_MAX_WIDTH: f32 = 720.;
const CONTROL_HEIGHT: f32 = 28.;
const THEME_PREVIEW_HEIGHT: f32 = 320.;
/// Theme cards shown before "Show more"; Ghostty alone ships hundreds.
const THEME_PAGE_SIZE: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    General,
    Appearance,
    Terminal,
    StatusBar,
    Keyboard,
    About,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::General,
        Section::Appearance,
        Section::Terminal,
        Section::StatusBar,
        Section::Keyboard,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Terminal => "Terminal",
            Section::StatusBar => "Status Bar",
            Section::Keyboard => "Keyboard",
            Section::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::General => "settings",
            Section::Appearance => "palette",
            Section::Terminal => "terminal",
            Section::StatusBar => "panel-bottom",
            Section::Keyboard => "keyboard",
            Section::About => "rocket",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Section::General => "Closing, notifications, agents, and outside control.",
            Section::Appearance => "Theme and how the tab sidebar is laid out.",
            Section::Terminal => "Text, shell integration, and command blocks.",
            Section::StatusBar => "The bar along the bottom of the window and what it shows.",
            Section::Keyboard => "Shortcuts available throughout the app.",
            Section::About => "Version and the files settings live in.",
        }
    }
}

/// Which themes the picker lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppearanceFilter {
    All,
    Dark,
    Light,
}

impl AppearanceFilter {
    fn matches(self, theme: &Theme) -> bool {
        match self {
            AppearanceFilter::All => true,
            AppearanceFilter::Dark => theme.appearance == Appearance::Dark,
            AppearanceFilter::Light => theme.appearance == Appearance::Light,
        }
    }
}

type Change<T> = Rc<dyn Fn(T, &mut SettingsPage, &mut Context<SettingsPage>)>;

/// The settings page, shown as a tab.
pub struct SettingsPage {
    focus_handle: FocusHandle,
    section: Section,
    scroll: ScrollHandle,
    theme_filter: AppearanceFilter,
    theme_search: Entity<TextInput>,
    theme_limit: usize,
    /// The theme under the pointer, shown in the preview until it leaves.
    hovered: Option<Arc<Theme>>,
    /// The switch flipped last, which animates its knob.
    toggled: Option<&'static str>,
    reset_armed: bool,
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
            cx.observe_global::<ThemeRegistry>(|_, cx| cx.notify()),
            cx.subscribe(&theme_search, |page, _, event: &TextInputEvent, cx| {
                if let TextInputEvent::Changed = event {
                    page.theme_limit = THEME_PAGE_SIZE;
                    cx.notify();
                }
            }),
        ];
        Self {
            focus_handle: cx.focus_handle(),
            section: Section::General,
            scroll: ScrollHandle::new(),
            theme_filter: AppearanceFilter::All,
            theme_search,
            theme_limit: THEME_PAGE_SIZE,
            hovered: None,
            toggled: None,
            reset_armed: false,
            _subscriptions: subscriptions,
        }
    }

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section == section {
            return;
        }
        self.section = section;
        self.hovered = None;
        self.reset_armed = false;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn render_nav(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = theme.ghost_hover;
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(NAV_WIDTH))
            .h_full()
            .gap(px(2.))
            .px(px(8.))
            .pt(px(24.))
            .bg(theme.title_bar)
            .border_r_1()
            .border_color(theme.border_variant)
            .child(
                div()
                    .px(px(10.))
                    .pb(px(12.))
                    .text_size(theme::TEXT_DEFAULT)
                    .text_color(theme.text)
                    .child("Settings"),
            )
            .children(Section::ALL.into_iter().map(|section| {
                let selected = section == self.section;
                div()
                    .id(("settings-nav", section as usize))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .text_size(theme::TEXT_SMALL)
                    .map(|item| {
                        if selected {
                            item.bg(theme.ghost_selected).text_color(theme.text)
                        } else {
                            item.text_color(theme.text_muted)
                                .hover(move |style| style.bg(hover))
                        }
                    })
                    .on_click(cx.listener(move |page, _: &ClickEvent, _, cx| {
                        page.select_section(section, cx);
                    }))
                    .child(icon(
                        section.icon(),
                        theme::ICON_SMALL,
                        if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        },
                    ))
                    .child(section.label())
            }))
    }

    fn render_general(
        &self,
        settings: &Settings,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let settings_path = SettingsStore::path(cx);
        let profiles = match settings.agent_profiles.len() {
            1 => "1 agent".to_string(),
            count => format!("{count} agents"),
        };

        vec![
            group(
                "Alerts",
                vec![
                    row(
                        "Confirm close",
                        "Ask before closing a pane, tab, or window with a program still running.",
                        theme,
                    )
                    .child(self.toggle(
                        "confirm-close",
                        settings.confirm_close,
                        true,
                        theme,
                        cx,
                        |on, _, cx| SettingsStore::update(cx, |settings| settings.confirm_close = on),
                    ))
                    .into_any_element(),
                    row(
                        "Notifications",
                        "Notify when a background tab's program asks to, or a long command finishes, while the window is inactive.",
                        theme,
                    )
                    .child(self.toggle(
                        "notifications",
                        settings.notifications,
                        true,
                        theme,
                        cx,
                        |on, _, cx| SettingsStore::update(cx, |settings| settings.notifications = on),
                    ))
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
            group(
                "Agents",
                vec![
                    row(
                        "Agent worktrees",
                        "Start each agent from the sidebar's + menu in a new git worktree of the current repository.",
                        theme,
                    )
                    .child(self.toggle(
                        "agent-worktrees",
                        settings.agent_worktrees,
                        true,
                        theme,
                        cx,
                        |on, _, cx| SettingsStore::update(cx, |settings| settings.agent_worktrees = on),
                    ))
                    .into_any_element(),
                    row(
                        "Agent profiles",
                        format!(
                            "{profiles} in the sidebar's + menu. Add or change them in the settings file."
                        ),
                        theme,
                    )
                    .child(
                        button("edit-agent-profiles", Some("file-pen-line"), "Open in Editor", theme)
                            .on_click(move |_: &ClickEvent, _, cx| {
                                cx.open_with_system(&settings_path)
                            }),
                    )
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
            group(
                "Automation",
                vec![
                    row(
                        "Control API",
                        "Serve the control socket used by the terminal command and the MCP server. Applies at launch.",
                        theme,
                    )
                    .child(self.toggle(
                        "control-api",
                        settings.control_api,
                        true,
                        theme,
                        cx,
                        |on, _, cx| SettingsStore::update(cx, |settings| settings.control_api = on),
                    ))
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
        ]
    }

    fn render_appearance(
        &self,
        settings: &Settings,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let sidebar = &settings.sidebar;
        let expanded = sidebar.density == TabDensity::Expanded;

        vec![
            self.render_theme_group(theme, window, cx)
                .into_any_element(),
            group(
                "Sidebar",
                vec![
                    row("Tab density", "How much each tab row shows.", theme)
                        .child(self.segmented(
                            "tab-density",
                            &[
                                (TabDensity::Compact, "Compact"),
                                (TabDensity::Expanded, "Expanded"),
                            ],
                            sidebar.density,
                            theme,
                            cx,
                            |density, _, cx| {
                                SettingsStore::update(cx, |settings| {
                                    settings.sidebar.density = density
                                })
                            },
                        ))
                        .into_any_element(),
                    row(
                        "Working directory",
                        "Expanded tab rows show the shell's current directory.",
                        theme,
                    )
                    .child(self.toggle(
                        "sidebar-directory",
                        sidebar.show_directory,
                        expanded,
                        theme,
                        cx,
                        |on, _, cx| {
                            SettingsStore::update(cx, |settings| {
                                settings.sidebar.show_directory = on
                            })
                        },
                    ))
                    .into_any_element(),
                    row(
                        "Git branch",
                        "Expanded tab rows show the repository's branch and changed files.",
                        theme,
                    )
                    .child(self.toggle(
                        "sidebar-branch",
                        sidebar.show_branch,
                        expanded,
                        theme,
                        cx,
                        |on, _, cx| {
                            SettingsStore::update(cx, |settings| settings.sidebar.show_branch = on)
                        },
                    ))
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
        ]
    }

    fn render_theme_group(
        &self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.theme_search.read(cx).text().trim().to_lowercase();
        let filter = self.theme_filter;
        let matches: Vec<Arc<Theme>> = ThemeRegistry::themes(cx)
            .iter()
            .filter(|candidate| filter.matches(candidate))
            .filter(|candidate| query.is_empty() || candidate.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let total = matches.len();
        let hidden = total.saturating_sub(self.theme_limit);
        let active = cx.theme().clone();
        let previewed = self.hovered.clone().unwrap_or_else(|| active.clone());
        let search_focused = self.theme_search.focus_handle(cx).is_focused(window);

        div()
            .flex()
            .flex_col()
            .child(group_caption("Theme", theme))
            .child(
                card(theme)
                    .p(px(16.))
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                field_frame(search_focused, theme)
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(8.))
                                    .child(icon("search", theme::ICON_SMALL, theme.text_muted))
                                    .child(
                                        div().flex_1().min_w_0().child(self.theme_search.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(theme::TEXT_SMALL)
                                            .text_color(theme.text_placeholder)
                                            .child(SharedString::from(total.to_string())),
                                    ),
                            )
                            .child(self.segmented(
                                "theme-filter",
                                &[
                                    (AppearanceFilter::All, "All"),
                                    (AppearanceFilter::Dark, "Dark"),
                                    (AppearanceFilter::Light, "Light"),
                                ],
                                self.theme_filter,
                                theme,
                                cx,
                                |filter, page, cx| {
                                    page.theme_filter = filter;
                                    page.theme_limit = THEME_PAGE_SIZE;
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .h(px(THEME_PREVIEW_HEIGHT))
                            .rounded(theme::RADIUS_LG)
                            .border_1()
                            .border_color(theme.border_variant)
                            .overflow_hidden()
                            .child(render_theme_preview(&previewed, theme)),
                    )
                    .child(if matches.is_empty() {
                        div()
                            .py(px(24.))
                            .flex()
                            .justify_center()
                            .text_size(theme::TEXT_SMALL)
                            .text_color(theme.text_muted)
                            .child("No themes match.")
                            .into_any_element()
                    } else {
                        div()
                            .grid()
                            .grid_cols(3)
                            .gap(px(12.))
                            .children(matches.into_iter().take(self.theme_limit).map(|candidate| {
                                let selected = candidate.name == active.name;
                                let hovered = candidate.clone();
                                theme_card(candidate, selected, theme).on_hover(cx.listener(
                                    move |page, hovering: &bool, _, cx| {
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
                                    },
                                ))
                            }))
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .when(hidden > 0, |footer| {
                                footer.child(
                                    button(
                                        "more-themes",
                                        Some("chevron-down"),
                                        format!("Show more ({hidden})"),
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        |page, _: &ClickEvent, _, cx| {
                                            page.theme_limit += THEME_PAGE_SIZE;
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme::TEXT_SMALL)
                                    .text_color(theme.text_placeholder)
                                    .child(
                                        "Zed .json themes and Ghostty theme files are supported.",
                                    ),
                            )
                            .child(
                                button(
                                    "open-themes-folder",
                                    Some("folder"),
                                    "Themes Folder",
                                    theme,
                                )
                                .on_click(
                                    |_: &ClickEvent, _, cx| {
                                        let directory = theme::user_theme_dir();
                                        if let Err(error) = fs::create_dir_all(&directory) {
                                            log::warn!(
                                                "failed to create {}: {error}",
                                                directory.display()
                                            );
                                        }
                                        cx.open_with_system(&directory);
                                    },
                                ),
                            )
                            .child(
                                button("reload-themes", Some("refresh-cw"), "Reload", theme)
                                    .on_click(|_: &ClickEvent, _, cx| theme::reload_themes(cx)),
                            ),
                    ),
            )
    }

    fn render_terminal(
        &self,
        settings: &Settings,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        vec![
            group(
                "Text",
                vec![
                    row("Font size", "Size of terminal text.", theme)
                        .child(self.stepper(
                            "font-size",
                            settings.font_size,
                            FONT_SIZE_RANGE,
                            0.5,
                            format!("{} pt", settings.font_size),
                            theme,
                            cx,
                            |size, _, cx| SettingsStore::update(cx, |settings| settings.font_size = size),
                        ))
                        .into_any_element(),
                    row(
                        "Line height",
                        "Spacing between rows, as a multiple of the font size.",
                        theme,
                    )
                    .child(self.stepper(
                        "line-height",
                        settings.line_height,
                        LINE_HEIGHT_RANGE,
                        0.05,
                        format!("{:.2}×", settings.line_height),
                        theme,
                        cx,
                        |height, _, cx| SettingsStore::update(cx, |settings| settings.line_height = height),
                    ))
                    .into_any_element(),
                    row(
                        "Padding",
                        "Space between the terminal text and the edges of its area.",
                        theme,
                    )
                    .child(self.stepper(
                        "terminal-padding",
                        settings.terminal_padding,
                        TERMINAL_PADDING_RANGE,
                        2.,
                        format!("{} px", settings.terminal_padding),
                        theme,
                        cx,
                        |padding, _, cx| {
                            SettingsStore::update(cx, |settings| settings.terminal_padding = padding)
                        },
                    ))
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
            group(
                "Shell",
                vec![
                    row(
                        "Shell integration",
                        "Prompt marks for ⌘↑/⌘↓ navigation and command status in zsh and bash. Applies to new terminals.",
                        theme,
                    )
                    .child(self.toggle(
                        "shell-integration",
                        settings.shell_integration,
                        true,
                        theme,
                        cx,
                        |on, _, cx| SettingsStore::update(cx, |settings| settings.shell_integration = on),
                    ))
                    .into_any_element(),
                    row(
                        "Command blocks",
                        "Show each command and its output as a block, with an input editor, in shells with integration. Applies to new terminals.",
                        theme,
                    )
                    .child(self.toggle(
                        "command-blocks",
                        settings.command_blocks,
                        true,
                        theme,
                        cx,
                        |on, _, cx| SettingsStore::update(cx, |settings| settings.command_blocks = on),
                    ))
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
        ]
    }

    fn render_status_bar(
        &self,
        settings: &Settings,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let config = &settings.status_bar;
        let items = StatusItem::ALL
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let side = config.side_of(item);
                let list = match side {
                    Some(Side::Left) => config.left.as_slice(),
                    Some(Side::Right) => config.right.as_slice(),
                    None => &[],
                };
                let position = list.iter().position(|existing| *existing == item);
                let can_move_up = position.is_some_and(|position| position > 0);
                let can_move_down = position.is_some_and(|position| position + 1 < list.len());

                row(item.label(), item.description(), theme)
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(6.))
                            .child(self.segmented(
                                item.label(),
                                &[
                                    (Some(Side::Left), "Left"),
                                    (Some(Side::Right), "Right"),
                                    (None, "Off"),
                                ],
                                side,
                                theme,
                                cx,
                                move |side, _, cx| {
                                    SettingsStore::update(cx, |settings| {
                                        settings.status_bar.place(item, side)
                                    })
                                },
                            ))
                            .child(move_button(
                                ("status-up", index),
                                "arrow-up",
                                !can_move_up,
                                theme,
                                move |settings| settings.status_bar.shift(item, -1),
                            ))
                            .child(move_button(
                                ("status-down", index),
                                "arrow-down",
                                !can_move_down,
                                theme,
                                move |settings| settings.status_bar.shift(item, 1),
                            )),
                    )
                    .into_any_element()
            })
            .collect();

        vec![
            group(
                "Bar",
                vec![
                    row(
                        "Show status bar",
                        "The bar along the bottom of the window.",
                        theme,
                    )
                    .child(self.toggle(
                        "status-bar-visible",
                        config.visible,
                        true,
                        theme,
                        cx,
                        |on, _, cx| {
                            SettingsStore::update(cx, |settings| settings.status_bar.visible = on)
                        },
                    ))
                    .into_any_element(),
                    row("Dividers", "Thin rules between status bar items.", theme)
                        .child(self.toggle(
                            "status-bar-dividers",
                            config.dividers,
                            true,
                            theme,
                            cx,
                            |on, _, cx| {
                                SettingsStore::update(cx, |settings| {
                                    settings.status_bar.dividers = on
                                })
                            },
                        ))
                        .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
            group("Items", items, theme).into_any_element(),
        ]
    }

    fn render_about(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings_path = SettingsStore::path(cx);
        let armed = self.reset_armed;

        vec![
            card(theme)
                .flex_row()
                .items_center()
                .gap(px(16.))
                .p(px(16.))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(48.))
                        .rounded(theme::RADIUS_LG)
                        .bg(theme.element_background)
                        .child(icon("terminal", px(24.), theme.text_muted)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(18.))
                                .text_color(theme.text)
                                .child("Terminal"),
                        )
                        .child(
                            div()
                                .text_size(theme::TEXT_SMALL)
                                .text_color(theme.text_muted)
                                .child(concat!("Version ", env!("CARGO_PKG_VERSION"))),
                        ),
                )
                .into_any_element(),
            group(
                "Files",
                vec![
                    row("Settings file", shorten_home(&settings_path), theme)
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .gap(px(8.))
                                .child({
                                    let path = settings_path.clone();
                                    button("reveal-settings", Some("folder"), "Reveal", theme)
                                        .on_click(move |_: &ClickEvent, _, cx| {
                                            cx.reveal_path(&path)
                                        })
                                })
                                .child({
                                    let path = settings_path.clone();
                                    button(
                                        "open-settings",
                                        Some("file-pen-line"),
                                        "Open in Editor",
                                        theme,
                                    )
                                    .on_click(
                                        move |_: &ClickEvent, _, cx| cx.open_with_system(&path),
                                    )
                                }),
                        )
                        .into_any_element(),
                    row(
                        "Reset settings",
                        "Put every setting back to its default, including agent profiles.",
                        theme,
                    )
                    .child(
                        button(
                            "reset-settings",
                            Some(if armed { "circle-alert" } else { "refresh-cw" }),
                            if armed {
                                "Click to Confirm"
                            } else {
                                "Reset…"
                            },
                            theme,
                        )
                        .when(armed, |button| {
                            button.text_color(theme::to_hsla(theme.terminal.ansi[1]))
                        })
                        .on_click(cx.listener(
                            |page, _: &ClickEvent, _, cx| {
                                if page.reset_armed {
                                    page.reset_armed = false;
                                    SettingsStore::update(cx, |settings| {
                                        *settings = Settings::default()
                                    });
                                } else {
                                    page.reset_armed = true;
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .into_any_element(),
                ],
                theme,
            )
            .into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(file_row(
                    "Session",
                    "reveal-session",
                    session::session_path(),
                    theme,
                ))
                .child(file_row(
                    "Themes folder",
                    "reveal-themes",
                    theme::user_theme_dir(),
                    theme,
                ))
                .into_any_element(),
        ]
    }

    /// An on/off switch. `change` receives the new state.
    fn toggle(
        &self,
        id: &'static str,
        on: bool,
        enabled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
        change: impl Fn(bool, &mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        const TRAVEL: f32 = 14.;
        const INSET: f32 = 2.;
        let knob = div()
            .absolute()
            .top(px(INSET))
            .size(px(14.))
            .rounded_full()
            .bg(if on { gpui::white() } else { theme.text_muted })
            .shadow_sm();
        let knob: AnyElement = if self.toggled == Some(id) {
            knob.with_animation(
                (id, on as usize),
                Animation::new(Duration::from_millis(160)).with_easing(ease_in_out),
                move |knob, delta| {
                    let progress = if on { delta } else { 1. - delta };
                    knob.left(px(INSET + TRAVEL * progress))
                },
            )
            .into_any_element()
        } else {
            knob.left(px(INSET + if on { TRAVEL } else { 0. }))
                .into_any_element()
        };
        let accent = theme.text_accent;
        let border = theme.border;

        div()
            .id(id)
            .relative()
            .flex_none()
            .w(px(34.))
            .h(px(20.))
            .rounded_full()
            .border_1()
            .map(|track| {
                if on {
                    track.bg(accent).border_color(accent)
                } else {
                    track.bg(theme.element_background).border_color(border)
                }
            })
            .map(|track| {
                if enabled {
                    track
                        .cursor_pointer()
                        .hover(move |style| style.border_color(accent))
                        .on_click(cx.listener(move |page, _: &ClickEvent, _, cx| {
                            page.toggled = Some(id);
                            change(!on, page, cx);
                            cx.notify();
                        }))
                } else {
                    track.opacity(0.4)
                }
            })
            .child(knob)
    }

    /// A row of mutually exclusive choices.
    fn segmented<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(T, &'static str)],
        selected: T,
        theme: &Theme,
        cx: &mut Context<Self>,
        change: impl Fn(T, &mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let change: Change<T> = Rc::new(change);
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
            .children(options.iter().enumerate().map(|(index, (value, label))| {
                let value = *value;
                let is_selected = value == selected;
                let change = change.clone();
                div()
                    .id((id, index))
                    .flex()
                    .items_center()
                    .h(px(CONTROL_HEIGHT - 6.))
                    .px(px(10.))
                    .rounded(px(3.))
                    .text_size(theme::TEXT_SMALL)
                    .map(|segment| {
                        if is_selected {
                            segment
                                .bg(theme.ghost_selected)
                                .text_color(theme.text)
                                .shadow_sm()
                        } else {
                            segment
                                .cursor_pointer()
                                .text_color(theme.text_muted)
                                .hover(move |style| style.bg(hover))
                                .on_click(cx.listener(move |page, _: &ClickEvent, _, cx| {
                                    change(value, page, cx);
                                    cx.notify();
                                }))
                        }
                    })
                    .child(*label)
            }))
    }

    /// − value + buttons for a number, kept inside `range` on the `step` grid.
    #[allow(clippy::too_many_arguments)]
    fn stepper(
        &self,
        id: &'static str,
        value: f32,
        range: (f32, f32),
        step: f32,
        label: String,
        theme: &Theme,
        cx: &mut Context<Self>,
        change: impl Fn(f32, &mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let change: Change<f32> = Rc::new(change);
        let step_button = |index: usize, icon_name: &'static str, direction: f32| {
            let next = step_value(value, step, range, direction);
            let disabled = next == value;
            let change = change.clone();
            let hover = theme.ghost_hover;
            let active = theme.ghost_selected;
            div()
                .id((id, index))
                .flex()
                .items_center()
                .justify_center()
                .size(px(CONTROL_HEIGHT - 6.))
                .rounded(px(3.))
                .map(|button| {
                    if disabled {
                        button.child(icon(icon_name, theme::ICON_XSMALL, theme.text_disabled()))
                    } else {
                        button
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .active(move |style| style.bg(active))
                            .on_click(cx.listener(move |page, _: &ClickEvent, _, cx| {
                                change(next, page, cx);
                                cx.notify();
                            }))
                            .child(icon(icon_name, theme::ICON_XSMALL, theme.text_muted))
                    }
                })
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .p(px(2.))
            .rounded(theme::RADIUS_SM)
            .border_1()
            .border_color(theme.border_variant)
            .bg(theme.element_background)
            .child(step_button(0, "minus", -1.))
            .child(
                div()
                    .w(px(64.))
                    .flex()
                    .justify_center()
                    .text_size(theme::TEXT_SMALL)
                    .font_family(theme::FONT_FAMILY)
                    .text_color(theme.text)
                    .child(SharedString::from(label)),
            )
            .child(step_button(1, "plus", 1.))
    }
}

/// One shortcut on the Keyboard page: the key combinations that trigger it
/// and what it does.
struct Shortcut {
    keys: &'static [&'static [&'static str]],
    action: &'static str,
}

const SHORTCUT_GROUPS: [(&str, &[Shortcut]); 5] = [
    (
        "Tabs",
        &[
            Shortcut {
                keys: &[&["⌘", "T"]],
                action: "New tab",
            },
            Shortcut {
                keys: &[&["⌘", "W"]],
                action: "Close pane, or the tab when it has one pane",
            },
            Shortcut {
                keys: &[&["⌥", "⌘", "W"]],
                action: "Close tab",
            },
            Shortcut {
                keys: &[&["⌘", "}"], &["⌃", "Tab"]],
                action: "Next tab",
            },
            Shortcut {
                keys: &[&["⌘", "{"], &["⌃", "⇧", "Tab"]],
                action: "Previous tab",
            },
            Shortcut {
                keys: &[&["⌘", "1–8"]],
                action: "Go to tab 1 to 8",
            },
            Shortcut {
                keys: &[&["⌘", "9"]],
                action: "Go to last tab",
            },
            Shortcut {
                keys: &[&["F2"]],
                action: "Rename tab",
            },
        ],
    ),
    (
        "Panes",
        &[
            Shortcut {
                keys: &[&["⌘", "D"]],
                action: "Split right",
            },
            Shortcut {
                keys: &[&["⇧", "⌘", "D"]],
                action: "Split down",
            },
            Shortcut {
                keys: &[&["⌥", "⌘", "←↑↓→"]],
                action: "Focus the pane in that direction",
            },
            Shortcut {
                keys: &[&["⇧", "⌘", "Enter"]],
                action: "Zoom the focused pane",
            },
            Shortcut {
                keys: &[&["⌃", "⌘", "="]],
                action: "Make panes equal size",
            },
        ],
    ),
    (
        "Terminal",
        &[
            Shortcut {
                keys: &[&["⌘", "F"]],
                action: "Find",
            },
            Shortcut {
                keys: &[&["⌘", "G"]],
                action: "Next match",
            },
            Shortcut {
                keys: &[&["⇧", "⌘", "G"], &["⇧", "Enter"]],
                action: "Previous match",
            },
            Shortcut {
                keys: &[&["⌘", "↑"]],
                action: "Previous prompt",
            },
            Shortcut {
                keys: &[&["⌘", "↓"]],
                action: "Next prompt",
            },
            Shortcut {
                keys: &[&["⌘", "C"]],
                action: "Copy",
            },
            Shortcut {
                keys: &[&["⌘", "V"]],
                action: "Paste",
            },
            Shortcut {
                keys: &[&["⌘", "A"]],
                action: "Select all",
            },
            Shortcut {
                keys: &[&["⌘", "K"]],
                action: "Clear scrollback",
            },
        ],
    ),
    (
        "Command Editor",
        &[
            Shortcut {
                keys: &[&["Enter"]],
                action: "Run command",
            },
            Shortcut {
                keys: &[&["⇧", "Enter"], &["⌥", "Enter"]],
                action: "New line",
            },
            Shortcut {
                keys: &[&["↑"], &["⌃", "P"]],
                action: "Previous command from history",
            },
            Shortcut {
                keys: &[&["↓"], &["⌃", "N"]],
                action: "Next command from history",
            },
            Shortcut {
                keys: &[&["⌃", "R"]],
                action: "Search history",
            },
            Shortcut {
                keys: &[&["⌃", "C"]],
                action: "Clear the command",
            },
            Shortcut {
                keys: &[&["⌃", "D"]],
                action: "Send end-of-file when empty",
            },
        ],
    ),
    (
        "Window",
        &[
            Shortcut {
                keys: &[&["⌘", "B"]],
                action: "Toggle sidebar",
            },
            Shortcut {
                keys: &[&["⌘", ","]],
                action: "Settings",
            },
            Shortcut {
                keys: &[&["⌘", "="]],
                action: "Increase font size",
            },
            Shortcut {
                keys: &[&["⌘", "-"]],
                action: "Decrease font size",
            },
            Shortcut {
                keys: &[&["⌘", "0"]],
                action: "Reset font size",
            },
            Shortcut {
                keys: &[&["⇧", "⌘", "W"]],
                action: "Close window",
            },
            Shortcut {
                keys: &[&["⌘", "M"]],
                action: "Minimize",
            },
            Shortcut {
                keys: &[&["⌘", "H"]],
                action: "Hide",
            },
            Shortcut {
                keys: &[&["⌥", "⌘", "H"]],
                action: "Hide others",
            },
            Shortcut {
                keys: &[&["⌘", "Q"]],
                action: "Quit",
            },
        ],
    ),
];

fn render_keyboard(theme: &Theme) -> Vec<AnyElement> {
    SHORTCUT_GROUPS
        .iter()
        .map(|(title, shortcuts)| {
            group(
                title,
                shortcuts
                    .iter()
                    .map(|shortcut| {
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(16.))
                            .px(px(16.))
                            .h(px(40.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme::TEXT_DEFAULT)
                                    .text_color(theme.text)
                                    .child(shortcut.action),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap(px(8.))
                                    .children(shortcut.keys.iter().enumerate().map(
                                        |(index, keys)| {
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(8.))
                                                .when(index > 0, |combo| {
                                                    combo.child(
                                                        div()
                                                            .text_size(theme::TEXT_SMALL)
                                                            .text_color(theme.text_placeholder)
                                                            .child("or"),
                                                    )
                                                })
                                                .child(div().flex().gap(px(4.)).children(
                                                    keys.iter().map(|key| keycap(key, theme)),
                                                ))
                                        },
                                    )),
                            )
                            .into_any_element()
                    })
                    .collect(),
                theme,
            )
            .into_any_element()
        })
        .collect()
}

fn keycap(key: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(22.))
        .h(px(22.))
        .px(px(6.))
        .rounded(theme::RADIUS_SM)
        .border_1()
        .border_color(theme.border)
        .border_b_2()
        .bg(theme.element_background)
        .text_size(theme::TEXT_SMALL)
        .text_color(theme.text_muted)
        .child(key)
}

/// The value one step from `value` in `direction` (±1), snapped to the step
/// grid so repeated presses never accumulate float error, and clamped.
fn step_value(value: f32, step: f32, range: (f32, f32), direction: f32) -> f32 {
    let next = ((value + step * direction) / step).round() * step;
    next.clamp(range.0, range.1)
}

/// A small arrow button that moves a status bar item within its side.
fn move_button(
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
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(CONTROL_HEIGHT - 6.))
        .rounded(px(3.));
    if disabled {
        button.child(icon(icon_name, theme::ICON_XSMALL, theme.text_disabled()))
    } else {
        button
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .active(move |style| style.bg(active))
            .on_click(move |_: &ClickEvent, _, cx| SettingsStore::update(cx, &change))
            .child(icon(icon_name, theme::ICON_XSMALL, theme.text_muted))
    }
}

fn group_caption(title: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .px(px(4.))
        .pb(px(8.))
        .text_size(theme::TEXT_SMALL)
        .text_color(theme.text_muted)
        .child(title)
}

fn card(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(theme::RADIUS_LG)
        .border_1()
        .border_color(theme.border_variant)
        .bg(theme.elevated_surface)
}

/// A card whose rows are separated by hairlines.
fn rows_card(rows: Vec<AnyElement>, theme: &Theme) -> Div {
    let separator = theme.border_variant;
    card(theme).children(rows.into_iter().enumerate().map(move |(index, row)| {
        div()
            .when(index > 0, |wrapper| {
                wrapper.border_t_1().border_color(separator)
            })
            .child(row)
    }))
}

/// A titled card of rows.
fn group(title: &'static str, rows: Vec<AnyElement>, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .child(group_caption(title, theme))
        .child(rows_card(rows, theme))
}

/// A setting's label and description; the caller adds the control.
fn row(label: &'static str, description: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(24.))
        .px(px(16.))
        .py(px(12.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
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
                        .child(description.into()),
                ),
        )
}

/// The bordered box around a text input, highlighted while focused.
fn field_frame(focused: bool, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(CONTROL_HEIGHT))
        .px(px(8.))
        .rounded(theme::RADIUS_SM)
        .border_1()
        .border_color(if focused {
            theme.text_accent
        } else {
            theme.border
        })
        .bg(theme.element_background)
}

/// A file's location with a button that shows it in Finder.
fn file_row(
    label: &'static str,
    id: &'static str,
    path: PathBuf,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .px(px(4.))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .text_size(theme::TEXT_SMALL)
                        .text_color(theme.text_muted)
                        .child(label),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(theme::TEXT_SMALL)
                        .font_family(theme::FONT_FAMILY)
                        .text_color(theme.text_placeholder)
                        .child(SharedString::from(shorten_home(&path))),
                ),
        )
        .child(
            button(id, Some("folder"), "Reveal", theme)
                .on_click(move |_: &ClickEvent, _, cx| cx.reveal_path(&path)),
        )
}

/// A clickable miniature of a theme: its chrome, text, accent, and
/// terminal colors.
fn theme_card(candidate: Arc<Theme>, selected: bool, chrome: &Theme) -> Stateful<Div> {
    let terminal = &candidate.terminal;
    let swatch = |index: usize| {
        div()
            .size(px(8.))
            .rounded(px(2.))
            .bg(theme::to_hsla(terminal.ansi[index]))
    };
    let bar = |width: f32, color: gpui::Hsla| {
        div().h(px(4.)).w(relative(width)).rounded(px(2.)).bg(color)
    };
    let source = match candidate.source {
        ThemeSource::Bundled => None,
        ThemeSource::User => Some("Custom"),
        ThemeSource::Ghostty => Some("Ghostty"),
    };
    let ring = if selected {
        chrome.text_accent
    } else {
        gpui::transparent_black()
    };
    let hover_ring = chrome.border;
    let name = candidate.name.to_string();

    div()
        .id(SharedString::from(format!("theme-card-{}", candidate.name)))
        .p(px(2.))
        .rounded(px(11.))
        .border_2()
        .border_color(ring)
        .cursor_pointer()
        .when(!selected, |card| {
            card.hover(move |style| style.border_color(hover_ring))
        })
        .on_click(move |_: &ClickEvent, _, cx| {
            let name = name.clone();
            SettingsStore::update(cx, |settings| settings.theme = name);
        })
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(theme::RADIUS_LG)
                .border_1()
                .border_color(chrome.border_variant)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .h(px(76.))
                        .bg(candidate.surface)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_none()
                                .gap(px(5.))
                                .w(px(34.))
                                .h_full()
                                .p(px(6.))
                                .bg(candidate.title_bar)
                                .border_r_1()
                                .border_color(candidate.border_variant)
                                .child(bar(0.9, candidate.text_muted.opacity(0.6)))
                                .child(bar(0.6, candidate.text_muted.opacity(0.6)))
                                .child(bar(0.75, candidate.text_accent)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .gap(px(5.))
                                .p(px(8.))
                                .child(bar(0.75, candidate.text))
                                .child(bar(0.5, candidate.text_muted))
                                .child(
                                    div()
                                        .w(px(26.))
                                        .h(px(8.))
                                        .rounded(px(4.))
                                        .bg(candidate.text_accent),
                                )
                                .child(
                                    div()
                                        .mt_auto()
                                        .flex()
                                        .gap(px(3.))
                                        .p(px(4.))
                                        .rounded(px(3.))
                                        .bg(theme::to_hsla(terminal.background))
                                        .children([1, 2, 3, 4, 5, 6].map(swatch)),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(px(30.))
                        .px(px(8.))
                        .bg(chrome.elevated_surface)
                        .border_t_1()
                        .border_color(chrome.border_variant)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(theme::TEXT_SMALL)
                                .text_color(if selected {
                                    chrome.text
                                } else {
                                    chrome.text_muted
                                })
                                .child(candidate.name.clone()),
                        )
                        .children(source.map(|source| {
                            div()
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(chrome.text_placeholder)
                                .child(source)
                        }))
                        .when(selected, |footer| {
                            footer.child(icon("check", theme::ICON_XSMALL, chrome.text))
                        }),
                ),
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

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let settings = SettingsStore::get(cx).clone();
        let section = self.section;
        let body = match section {
            Section::General => self.render_general(&settings, &theme, cx),
            Section::Appearance => self.render_appearance(&settings, &theme, window, cx),
            Section::Terminal => self.render_terminal(&settings, &theme, cx),
            Section::StatusBar => self.render_status_bar(&settings, &theme, cx),
            Section::Keyboard => render_keyboard(&theme),
            Section::About => self.render_about(&theme, cx),
        };

        div()
            .key_context("SettingsPage")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .bg(theme.surface)
            .font_family(theme::UI_FONT_FAMILY)
            .text_color(theme.text)
            .child(self.render_nav(&theme, cx))
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .mx_auto()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .px(px(32.))
                            .pt(px(32.))
                            .pb(px(48.))
                            .flex()
                            .flex_col()
                            .gap(px(24.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .text_size(px(20.))
                                            .text_color(theme.text)
                                            .child(section.label()),
                                    )
                                    .child(
                                        div()
                                            .text_size(theme::TEXT_SMALL)
                                            .text_color(theme.text_muted)
                                            .child(section.description()),
                                    ),
                            )
                            .children(body),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_snap_to_the_grid_and_stay_in_range() {
        assert_eq!(step_value(13.5, 0.5, FONT_SIZE_RANGE, 1.), 14.);
        assert_eq!(step_value(13.3, 0.5, FONT_SIZE_RANGE, -1.), 13.);
        assert_eq!(step_value(32., 0.5, FONT_SIZE_RANGE, 1.), 32.);
        assert_eq!(step_value(0., 2., TERMINAL_PADDING_RANGE, -1.), 0.);
        let mut height = 1.;
        for _ in 0..7 {
            height = step_value(height, 0.05, LINE_HEIGHT_RANGE, 1.);
        }
        assert!((height - 1.35).abs() < 1e-5);
    }
}
