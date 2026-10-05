//! The tab sidebar: its settings, search filtering, group headers, tab
//! rows, drag and drop, and the menus for tabs, groups, and view options.

use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, Hsla, MouseButton, MouseDownEvent, SharedString,
    anchored, deferred, div, prelude::*, px,
};

use serde::{Deserialize, Serialize};

use crate::{
    agent_badge,
    components::{
        DragPreview, icon, icon_button, menu_caption, menu_choice, menu_item, menu_separator,
        menu_surface,
    },
    pane_group::DraggedPane,
    settings::{Settings, SettingsStore},
    tabs::{Entry, GroupId, Row, TabColor, TabDestination, TabIcon, TabId, TabLayout},
    terminal_view::AgentState,
    theme::{self, Theme},
    workspace::{MenuKind, Target, Workspace},
};

/// How much each tab row shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabDensity {
    /// One line: icon and title.
    Compact,
    /// A second line with the directory and git branch.
    #[default]
    Expanded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarSettings {
    pub density: TabDensity,
    pub show_directory: bool,
    pub show_branch: bool,
}

impl Default for SidebarSettings {
    fn default() -> Self {
        Self {
            density: TabDensity::Expanded,
            show_directory: true,
            show_branch: true,
        }
    }
}

/// The sidebar rows matching `query`. With no query this is the normal
/// layout; otherwise groups open up to show their matching tabs, and a group
/// whose name matches shows all of its tabs.
fn filter_rows(layout: &TabLayout, query: &str, text_of: impl Fn(TabId) -> String) -> Vec<Row> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return layout.rows();
    }
    let matches = |id: TabId| text_of(id).to_lowercase().contains(&query);
    let mut rows = Vec::new();
    for entry in layout.entries() {
        match *entry {
            Entry::Tab(id) => {
                if matches(id) {
                    rows.push(Row::Tab { id, group: None });
                }
            }
            Entry::Group(group_id) => {
                let Some(group) = layout.group(group_id) else {
                    continue;
                };
                let name_matches = group.name.to_lowercase().contains(&query);
                let tabs: Vec<TabId> = group
                    .tabs()
                    .iter()
                    .copied()
                    .filter(|id| name_matches || matches(*id))
                    .collect();
                if !tabs.is_empty() {
                    rows.push(Row::Group(group_id));
                    rows.extend(tabs.into_iter().map(|id| Row::Tab {
                        id,
                        group: Some(group_id),
                    }));
                }
            }
        }
    }
    rows
}

/// Indentation of tabs inside a group.
const GROUP_INDENT: f32 = 14.;

/// Payload while a tab is being dragged.
#[derive(Clone)]
pub(crate) struct DraggedTab {
    pub id: TabId,
    label: SharedString,
    icon: &'static str,
}

/// Payload while a group header is being dragged.
#[derive(Clone)]
struct DraggedGroup {
    id: GroupId,
    label: SharedString,
}

fn tab_color(color: Option<TabColor>, theme: &Theme) -> Option<Hsla> {
    color.map(|color| theme::to_hsla(theme.terminal.ansi[color.ansi_index()]))
}

impl Workspace {
    /// Rows currently shown, after applying the search query.
    pub(crate) fn visible_rows(&self, cx: &gpui::App) -> Vec<Row> {
        let query = self.tab_search.read(cx).text().to_string();
        filter_rows(&self.layout, &query, |id| {
            self.display(id, cx)
                .map(|display| {
                    [Some(display.title), display.directory, display.branch]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
        })
    }

    pub(crate) fn render_sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.tab_search.read(cx).text().trim().is_empty();
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in self.visible_rows(cx) {
            let starts_top_level_item = starts_top_level_item(&row);
            let element = match row {
                Row::Group(id) => Some(self.render_group_header(id, theme, cx).into_any_element()),
                Row::Tab { id, group } => self
                    .render_tab_row(id, group, theme, cx)
                    .map(IntoElement::into_any_element),
            };
            let Some(element) = element else {
                continue;
            };
            if starts_top_level_item && !rows.is_empty() {
                rows.push(
                    div()
                        .flex_none()
                        .h(px(1.))
                        .bg(theme.border_variant)
                        .into_any_element(),
                );
            }
            rows.push(element);
        }
        let accent = theme.text_accent;

        div()
            .flex()
            .flex_col()
            .flex_none()
            .relative()
            .w(self.sidebar_width)
            .h_full()
            .min_h_0()
            .bg(theme.surface)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .h(px(32.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .border_b_1()
                    .border_color(theme.border_variant)
                    .child(icon("search", theme::ICON_SMALL, theme.text_muted))
                    .child(div().flex_1().min_w_0().child(self.tab_search.clone()))
                    .child(
                        icon_button("view-options", "settings-2", theme)
                            .size(px(22.))
                            .rounded(px(5.))
                            .when(
                                self.context_menu
                                    .as_ref()
                                    .is_some_and(|menu| menu.kind == MenuKind::ViewOptions),
                                |button| button.bg(theme.ghost_selected),
                            )
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.open_context_menu(MenuKind::ViewOptions, event.position(), cx);
                            })),
                    )
                    .child(
                        icon_button("new-tab", "plus", theme)
                            .size(px(22.))
                            .rounded(px(5.))
                            .when(
                                self.context_menu
                                    .as_ref()
                                    .is_some_and(|menu| menu.kind == MenuKind::NewTab),
                                |button| button.bg(theme.ghost_selected),
                            )
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.open_context_menu(MenuKind::NewTab, event.position(), cx);
                            })),
                    ),
            )
            .when(searching && rows.is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .text_size(theme::TEXT_SMALL)
                        .text_color(theme.text_placeholder)
                        .child("No matching tabs"),
                )
            })
            .child(
                div()
                    .id("tab-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(rows)
                    // The space below the last row: dropping here moves to the end.
                    .child(
                        div()
                            .id("tab-list-end")
                            .flex_1()
                            .min_h(px(24.))
                            .border_t_2()
                            .border_color(gpui::transparent_black())
                            .drag_over::<DraggedTab>(move |style, _, _, _| {
                                style.border_color(accent)
                            })
                            .drag_over::<DraggedGroup>(move |style, _, _, _| {
                                style.border_color(accent)
                            })
                            .on_drop(cx.listener(|this, dragged: &DraggedTab, _, cx| {
                                this.layout.move_tab(dragged.id, TabDestination::End);
                                this.layout_changed(cx);
                            }))
                            .drag_over::<DraggedPane>(move |style, _, _, _| {
                                style.border_color(accent)
                            })
                            .on_drop(cx.listener(|this, dragged: &DraggedPane, window, cx| {
                                this.pane_to_tab(dragged, TabDestination::End, window, cx);
                            }))
                            .on_drop(cx.listener(|this, dragged: &DraggedGroup, _, cx| {
                                this.layout.move_group(dragged.id, None);
                                this.layout_changed(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("sidebar-resize-handle")
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(theme::SIDEBAR_RESIZE_HANDLE_WIDTH)
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.sidebar_resizing = true;
                            cx.stop_propagation();
                        }),
                    )
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        // Double-clicking the edge restores the default width.
                        if event.click_count() == 2 {
                            this.sidebar_width = theme::SIDEBAR_WIDTH;
                            this.layout_changed(cx);
                        }
                    })),
            )
    }

    fn render_group_header(
        &self,
        id: GroupId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(group) = self.layout.group(id) else {
            return div().into_any_element();
        };
        let contains_active = self
            .active
            .is_some_and(|active| group.tabs().contains(&active));
        let color = tab_color(group.color, theme);
        let hover = theme.ghost_hover;
        let accent = theme.text_accent;
        let hover_group = SharedString::from(format!("group-{id:?}"));
        let renaming = self
            .renaming
            .as_ref()
            .filter(|renaming| renaming.target == Target::Group(id))
            .map(|renaming| renaming.input.clone());
        let collapsed = group.collapsed;
        let label = SharedString::from(group.name.clone());
        let count = group.tabs().len();

        div()
            .id(("group", id.element_id()))
            .group(hover_group.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .pl(px(8.))
            .pr(px(6.))
            .border_t_2()
            .border_color(gpui::transparent_black())
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.start_rename(Target::Group(id), window, cx);
                    return;
                }
                if let Some(group) = this.layout.group_mut(id) {
                    group.collapsed = !group.collapsed;
                }
                this.layout_changed(cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_context_menu(MenuKind::Group(id), event.position, cx);
                }),
            )
            .on_drag(
                DraggedGroup {
                    id,
                    label: label.clone(),
                },
                |dragged, _, _, cx| {
                    cx.new(|_| DragPreview {
                        label: dragged.label.clone(),
                        icon: "layers",
                    })
                },
            )
            .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(accent.opacity(0.15)))
            .drag_over::<DraggedGroup>(move |style, _, _, _| style.border_color(accent))
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                this.layout
                    .move_tab(dragged.id, TabDestination::IntoGroup(id));
                this.layout_changed(cx);
            }))
            .drag_over::<DraggedPane>(move |style, _, _, _| style.bg(accent.opacity(0.15)))
            .on_drop(cx.listener(move |this, dragged: &DraggedPane, window, cx| {
                this.pane_to_tab(dragged, TabDestination::IntoGroup(id), window, cx);
            }))
            .on_drop(cx.listener(move |this, dragged: &DraggedGroup, _, cx| {
                this.layout.move_group(dragged.id, Some(Entry::Group(id)));
                this.layout_changed(cx);
            }))
            .child(icon(
                if collapsed {
                    "chevron-right"
                } else {
                    "chevron-down"
                },
                theme::ICON_XSMALL,
                theme.text_muted,
            ))
            .children(color.map(|color| div().flex_none().size(px(8.)).rounded_full().bg(color)))
            .child(match renaming {
                Some(input) => div().flex_1().min_w_0().child(input).into_any_element(),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::TEXT_SMALL)
                    .text_color(if contains_active {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .child(label)
                    .into_any_element(),
            })
            // The tab count and the new-tab button share one slot: the
            // count shows at rest, the button on hover.
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(18.))
                    .child(
                        div()
                            .text_size(theme::TEXT_SMALL)
                            .text_color(theme.text_placeholder)
                            .group_hover(hover_group.clone(), |style| style.invisible())
                            .child(SharedString::from(count.to_string())),
                    )
                    .child(
                        icon_button(("group-new-tab", id.element_id()), "plus", theme)
                            .absolute()
                            .inset_0()
                            .size(px(18.))
                            .invisible()
                            .group_hover(hover_group, |style| style.visible())
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.new_tab_in(Some(id), window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_tab_row(
        &self,
        id: TabId,
        group: Option<GroupId>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let display = self.display(id, cx)?;
        let active = self.active == Some(id);
        let hover_group = SharedString::from(format!("tab-{id:?}"));
        let color = tab_color(display.color, theme);
        let group_color = group
            .and_then(|group| self.layout.group(group))
            .and_then(|group| tab_color(group.color, theme))
            .unwrap_or(theme.border);
        let indent = if group.is_some() { GROUP_INDENT } else { 0. };
        let hover = theme.ghost_hover;
        let accent = theme.text_accent;
        let renaming = self
            .renaming
            .as_ref()
            .filter(|renaming| renaming.target == Target::Tab(id))
            .map(|renaming| renaming.input.clone());
        let sidebar = SettingsStore::get(cx).sidebar.clone();
        let expanded = sidebar.density == TabDensity::Expanded;
        let directory = display
            .directory
            .filter(|_| expanded && sidebar.show_directory);
        let branch = display.branch.filter(|_| expanded && sidebar.show_branch);
        let agent = display.agent.filter(|_| expanded);
        let has_details = directory.is_some() || branch.is_some() || agent.is_some();
        let icon_color = if active { theme.text } else { theme.text_muted };
        let title_color = if active { theme.text } else { theme.text_muted };
        let in_group = group.is_some();

        Some(
            div()
                .id(("tab", id.element_id()))
                .group(hover_group.clone())
                .relative()
                .flex()
                .flex_col()
                .flex_none()
                .pt(px(2.))
                .pb(px(if has_details { 4. } else { 2. }))
                .pl(px(8. + indent))
                .pr(px(6.))
                .border_t_2()
                .border_color(gpui::transparent_black())
                .cursor_pointer()
                // A tab's color tints its whole row; the tint deepens on
                // hover and when the tab is active.
                .map(|row| match (color, active) {
                    (Some(color), true) => row.bg(color.opacity(0.32)),
                    (Some(color), false) => row
                        .bg(color.opacity(0.14))
                        .hover(move |style| style.bg(color.opacity(0.22))),
                    (None, true) => row.bg(theme.ghost_selected),
                    (None, false) => row.hover(move |style| style.bg(hover)),
                })
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() == 2 {
                        this.start_rename(Target::Tab(id), window, cx);
                    } else {
                        this.activate(id, window, cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.open_context_menu(MenuKind::Tab(id), event.position, cx);
                    }),
                )
                .on_drag(
                    DraggedTab {
                        id,
                        label: display.title.clone(),
                        icon: display.icon,
                    },
                    |dragged, _, _, cx| {
                        cx.new(|_| DragPreview {
                            label: dragged.label.clone(),
                            icon: dragged.icon,
                        })
                    },
                )
                .drag_over::<DraggedTab>(move |style, _, _, _| style.border_color(accent))
                .drag_over::<DraggedGroup>(move |style, _, _, _| {
                    if in_group {
                        style
                    } else {
                        style.border_color(accent)
                    }
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                    this.layout.move_tab(dragged.id, TabDestination::Before(id));
                    this.layout_changed(cx);
                }))
                .drag_over::<DraggedPane>(move |style, _, _, _| style.border_color(accent))
                .on_drop(cx.listener(move |this, dragged: &DraggedPane, window, cx| {
                    this.pane_to_tab(dragged, TabDestination::Before(id), window, cx);
                }))
                .on_drop(cx.listener(move |this, dragged: &DraggedGroup, _, cx| {
                    // Groups only live at the top level: land before this
                    // tab, or before the group that contains it.
                    let before = match this.layout.group_of(id) {
                        Some(group) => Entry::Group(group),
                        None => Entry::Tab(id),
                    };
                    this.layout.move_group(dragged.id, Some(before));
                    this.layout_changed(cx);
                }))
                .when(in_group, |row| {
                    row.child(
                        div()
                            .absolute()
                            .left(px(14.))
                            .top_0()
                            .bottom_0()
                            .w(px(1.))
                            .bg(group_color),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .h(px(22.))
                        .child(match display.agent {
                            Some(state) => {
                                agent_badge::status_icon(state, theme::ICON_SMALL, theme)
                            }
                            None => {
                                icon(display.icon, theme::ICON_SMALL, icon_color).into_any_element()
                            }
                        })
                        .child(match renaming {
                            Some(input) => div().flex_1().min_w_0().child(input).into_any_element(),
                            None => div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(theme::TEXT_DEFAULT)
                                .text_color(title_color)
                                .child(display.title)
                                .into_any_element(),
                        })
                        // A failed command outranks other activity.
                        .when(display.failed || display.attention, |line| {
                            line.child(div().flex_none().size(px(6.)).rounded_full().bg(
                                if display.failed {
                                    theme::to_hsla(theme.terminal.ansi[1])
                                } else {
                                    theme.text_accent
                                },
                            ))
                        })
                        .child(if display.pinned {
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .justify_center()
                                .size(px(18.))
                                .child(icon("pin", theme::ICON_XSMALL, theme.text_muted))
                                .into_any_element()
                        } else {
                            icon_button(("close-tab", id.element_id()), "x", theme)
                                .size(px(18.))
                                .invisible()
                                .group_hover(hover_group, |style| style.visible())
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.request_close(id, window, cx);
                                }))
                                .into_any_element()
                        }),
                )
                .when(has_details, |row| {
                    row.child(tab_details(agent, directory, branch, theme))
                }),
        )
    }

    pub(crate) fn render_context_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let menu = self.context_menu.as_ref()?;
        let body = match menu.kind {
            MenuKind::Tab(id) => self.render_tab_menu(id, theme, cx)?.into_any_element(),
            MenuKind::Group(id) => self.render_group_menu(id, theme, cx)?.into_any_element(),
            MenuKind::ViewOptions => {
                render_view_options_menu(&SettingsStore::get(cx).sidebar, theme).into_any_element()
            }
            MenuKind::NewTab => self.render_new_tab_menu(theme, cx).into_any_element(),
        };
        Some(deferred(
            anchored()
                .position(menu.position)
                .snap_to_window_with_margin(px(8.))
                .child(
                    menu_surface(theme)
                        .id("context-menu")
                        .occlude()
                        .on_mouse_down_out(
                            cx.listener(|this, _, _, cx| this.close_context_menu(cx)),
                        )
                        .child(body),
                ),
        ))
    }

    fn render_tab_menu(
        &self,
        id: TabId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let style = self.layout.style(id);
        let current_group = self.layout.group_of(id);
        let other_groups: Vec<(GroupId, String)> = self
            .layout
            .groups()
            .iter()
            .filter(|group| Some(group.id) != current_group)
            .map(|group| (group.id, group.name.clone()))
            .collect();
        let pinned = style.pinned;

        Some(
            div()
                .flex()
                .flex_col()
                .child(
                    menu_item("menu-rename", "pencil", "Rename", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.start_rename(Target::Tab(id), window, cx);
                        },
                    )),
                )
                .child(
                    menu_item(
                        "menu-pin",
                        "pin",
                        if pinned { "Unpin" } else { "Pin" },
                        theme,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.update_style(id, |style| style.pinned = !style.pinned, cx);
                            this.close_context_menu(cx);
                        },
                    )),
                )
                .child(menu_separator(theme))
                .child(menu_caption("Color", theme))
                .child(self.render_color_choices(Target::Tab(id), style.color, theme, cx))
                .child(menu_caption("Icon", theme))
                .child(self.render_icon_choices(id, style.icon, theme, cx))
                .child(menu_separator(theme))
                .child(
                    menu_item("menu-new-group", "layers", "New Group with Tab", theme).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.new_group_with(id, window, cx);
                        }),
                    ),
                )
                .children(other_groups.into_iter().map(|(group, name)| {
                    menu_item(
                        ("menu-move", group.element_id()),
                        "folder",
                        format!("Move to {name}"),
                        theme,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.layout.move_tab(id, TabDestination::IntoGroup(group));
                            this.close_context_menu(cx);
                            this.layout_changed(cx);
                        },
                    ))
                }))
                .when(current_group.is_some(), |menu| {
                    menu.child(
                        menu_item("menu-ungroup-tab", "layers", "Remove from Group", theme)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.layout.move_tab(id, TabDestination::End);
                                this.close_context_menu(cx);
                                this.layout_changed(cx);
                            })),
                    )
                })
                .child(menu_separator(theme))
                .child(
                    menu_item("menu-close", "x", "Close Tab", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.close_context_menu(cx);
                            this.request_close(id, window, cx);
                        },
                    )),
                ),
        )
    }

    fn render_group_menu(
        &self,
        id: GroupId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let group = self.layout.group(id)?;
        let collapsed = group.collapsed;

        Some(
            div()
                .flex()
                .flex_col()
                .child(
                    menu_item("menu-new-tab", "plus", "New Tab in Group", theme).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.close_context_menu(cx);
                            this.new_tab_in(Some(id), window, cx);
                        }),
                    ),
                )
                .child(
                    menu_item("menu-rename-group", "pencil", "Rename Group", theme).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.start_rename(Target::Group(id), window, cx);
                        }),
                    ),
                )
                .child(
                    menu_item(
                        "menu-collapse",
                        if collapsed {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        if collapsed { "Expand" } else { "Collapse" },
                        theme,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            if let Some(group) = this.layout.group_mut(id) {
                                group.collapsed = !group.collapsed;
                            }
                            this.close_context_menu(cx);
                            this.layout_changed(cx);
                        },
                    )),
                )
                .child(menu_separator(theme))
                .child(menu_caption("Color", theme))
                .child(self.render_color_choices(Target::Group(id), group.color, theme, cx))
                .child(menu_separator(theme))
                .child(
                    menu_item("menu-ungroup", "layers", "Ungroup", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.layout.ungroup(id);
                            this.close_context_menu(cx);
                            this.layout_changed(cx);
                        },
                    )),
                )
                .child(
                    menu_item("menu-close-group", "x", "Close Group", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.close_context_menu(cx);
                            this.request_close_group(id, window, cx);
                        },
                    )),
                ),
        )
    }

    fn render_new_tab_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let profiles = SettingsStore::get(cx).agent_profiles.clone();
        let settings_path = SettingsStore::path(cx);

        div()
            .flex()
            .flex_col()
            .child(
                menu_item("menu-new-terminal", "terminal", "New Terminal", theme).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.close_context_menu(cx);
                        this.new_tab_in(None, window, cx);
                    }),
                ),
            )
            .when(!profiles.is_empty(), |menu| {
                menu.child(menu_separator(theme))
                    .child(menu_caption("Agents", theme))
            })
            .children(profiles.into_iter().enumerate().map(|(index, profile)| {
                let icon_name = profile.icon.map(TabIcon::asset).unwrap_or("bot");
                menu_item(
                    ("menu-agent", index),
                    icon_name,
                    profile.name.clone(),
                    theme,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.close_context_menu(cx);
                    this.open_agent(&profile, window, cx);
                }))
            }))
            .child(menu_separator(theme))
            .child(
                menu_item("menu-edit-agents", "pencil", "Edit Agent Profiles…", theme).on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        cx.open_with_system(&settings_path);
                    }),
                ),
            )
    }

    fn render_color_choices(
        &self,
        target: Target,
        current: Option<TabColor>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let choices = std::iter::once(None).chain(TabColor::ALL.into_iter().map(Some));
        div()
            .flex()
            .gap(px(2.))
            .px(px(4.))
            .pb(px(4.))
            .children(choices.enumerate().map(|(index, color)| {
                let swatch = match tab_color(color, theme) {
                    Some(fill) => div().size(px(12.)).rounded_full().bg(fill),
                    // "No color" is drawn as an empty ring.
                    None => div()
                        .size(px(12.))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.text_muted),
                };
                menu_choice(("menu-color", index), color == current, theme)
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| match target {
                            Target::Tab(id) => {
                                this.update_style(id, |style| style.color = color, cx)
                            }
                            Target::Group(id) => {
                                if let Some(group) = this.layout.group_mut(id) {
                                    group.color = color;
                                }
                                this.layout_changed(cx);
                            }
                        }),
                    )
                    .child(swatch)
            }))
    }

    fn render_icon_choices(
        &self,
        id: TabId,
        current: Option<TabIcon>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .gap(px(2.))
            .px(px(4.))
            .pb(px(4.))
            .children(TabIcon::ALL.into_iter().enumerate().map(|(index, choice)| {
                let selected = current.unwrap_or(TabIcon::Terminal) == choice;
                menu_choice(("menu-icon", index), selected, theme)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        // The terminal icon is the default, so store it as unset.
                        let icon = (choice != TabIcon::Terminal).then_some(choice);
                        this.update_style(id, |style| style.icon = icon, cx);
                    }))
                    .child(icon(choice.asset(), theme::ICON_SMALL, theme.text))
            }))
    }
}

/// A rule separates top-level items; tabs inside a group stay together
/// under their header.
fn starts_top_level_item(row: &Row) -> bool {
    matches!(row, Row::Group(_) | Row::Tab { group: None, .. })
}

/// Position of a tab among the tab list's children, which include the
/// divider rules drawn between top-level items.
pub(crate) fn sidebar_child_index(rows: &[Row], tab: TabId) -> Option<usize> {
    let mut index = 0;
    for (position, row) in rows.iter().enumerate() {
        if position > 0 && starts_top_level_item(row) {
            index += 1;
        }
        if matches!(row, Row::Tab { id, .. } if *id == tab) {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// The sliders menu: row density and which details expanded rows show.
fn render_view_options_menu(config: &SidebarSettings, theme: &Theme) -> impl IntoElement {
    let check = |on: bool| {
        div().flex_none().size(theme::ICON_SMALL).when(on, |slot| {
            slot.child(icon("check", theme::ICON_SMALL, theme.text_accent))
        })
    };
    let option = |id: &'static str, label: &'static str, on: bool, change: fn(&mut Settings)| {
        let hover = theme.ghost_hover;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .h(px(26.))
            .px(px(8.))
            .rounded(theme::RADIUS_SM)
            .cursor_pointer()
            .text_size(theme::TEXT_SMALL)
            .text_color(theme.text)
            .hover(move |style| style.bg(hover))
            .on_click(move |_: &ClickEvent, _, cx| SettingsStore::update(cx, change))
            .child(label)
            .child(check(on))
    };
    let expanded = config.density == TabDensity::Expanded;

    div()
        .flex()
        .flex_col()
        .child(menu_caption("Density", theme))
        .child(option("view-compact", "Compact", !expanded, |settings| {
            settings.sidebar.density = TabDensity::Compact;
        }))
        .child(option("view-expanded", "Expanded", expanded, |settings| {
            settings.sidebar.density = TabDensity::Expanded;
        }))
        .child(menu_separator(theme))
        .child(menu_caption("Expanded rows show", theme))
        .child(option(
            "view-directory",
            "Working Directory",
            config.show_directory,
            |settings| {
                settings.sidebar.show_directory = !settings.sidebar.show_directory;
            },
        ))
        .child(option(
            "view-branch",
            "Git Branch",
            config.show_branch,
            |settings| {
                settings.sidebar.show_branch = !settings.sidebar.show_branch;
            },
        ))
}

fn tab_details(
    agent: Option<AgentState>,
    directory: Option<SharedString>,
    branch: Option<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    let has_agent = agent.is_some();
    let has_branch = branch.is_some();
    let separator_color = theme.text_muted.opacity(0.4);
    let muted = theme.text_muted;

    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w_0()
        // Align with the title, past the 14px icon and 8px gap.
        .pl(px(22.))
        .text_size(theme::TEXT_SMALL)
        .text_color(muted)
        .children(agent.map(|state| {
            div()
                .flex_none()
                .text_color(agent_badge::status_color(state.status, theme))
                .child(SharedString::from(format!(
                    "{} · {}",
                    state.agent.name(),
                    agent_badge::status_label(state.status)
                )))
        }))
        .when(has_agent && (has_branch || directory.is_some()), |row| {
            row.child(div().flex_none().text_color(separator_color).child("•"))
        })
        .children(branch.map(|branch| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .max_w(px(120.))
                .child(icon("git-branch", theme::ICON_XSMALL, muted))
                .child(div().truncate().child(branch))
        }))
        .when(has_branch && directory.is_some(), |row| {
            row.child(div().flex_none().text_color(separator_color).child("•"))
        })
        .children(directory.map(|directory| div().min_w_0().truncate().child(directory)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::TabStyle;

    fn named_layout(names: &[&str]) -> (TabLayout, Vec<TabId>) {
        let mut layout = TabLayout::default();
        let ids = names
            .iter()
            .map(|name| {
                layout.add_tab(
                    TabStyle {
                        name: Some(name.to_string()),
                        ..Default::default()
                    },
                    None,
                )
            })
            .collect();
        (layout, ids)
    }

    fn text_of(layout: &TabLayout) -> impl Fn(TabId) -> String + '_ {
        |id| layout.style(id).name.unwrap_or_default()
    }

    #[test]
    fn child_index_accounts_for_dividers() {
        let (mut layout, ids) = named_layout(&["a", "b", "c"]);
        let group = layout.group_tab(ids[1], "G".into());
        layout.move_tab(ids[2], TabDestination::IntoGroup(group));
        // Children: a, rule, G, b, c
        let rows = layout.rows();
        assert_eq!(sidebar_child_index(&rows, ids[0]), Some(0));
        assert_eq!(sidebar_child_index(&rows, ids[1]), Some(3));
        assert_eq!(sidebar_child_index(&rows, ids[2]), Some(4));
    }

    #[test]
    fn empty_query_returns_normal_rows() {
        let (layout, _) = named_layout(&["api", "web"]);
        assert_eq!(filter_rows(&layout, "  ", text_of(&layout)), layout.rows());
    }

    #[test]
    fn query_filters_case_insensitively() {
        let (layout, ids) = named_layout(&["API server", "web"]);
        assert_eq!(
            filter_rows(&layout, "api", text_of(&layout)),
            vec![Row::Tab {
                id: ids[0],
                group: None
            }]
        );
    }

    #[test]
    fn matches_inside_collapsed_groups_are_shown() {
        let (mut layout, ids) = named_layout(&["db", "logs"]);
        let group = layout.group_tab(ids[0], "Infra".into());
        layout.group_mut(group).unwrap().collapsed = true;
        assert_eq!(
            filter_rows(&layout, "db", text_of(&layout)),
            vec![
                Row::Group(group),
                Row::Tab {
                    id: ids[0],
                    group: Some(group)
                }
            ]
        );
    }

    #[test]
    fn matching_group_name_shows_all_its_tabs() {
        let (mut layout, ids) = named_layout(&["one", "two", "three"]);
        let group = layout.group_tab(ids[0], "Infra".into());
        layout.move_tab(ids[1], TabDestination::IntoGroup(group));
        assert_eq!(
            filter_rows(&layout, "infra", text_of(&layout)),
            vec![
                Row::Group(group),
                Row::Tab {
                    id: ids[0],
                    group: Some(group)
                },
                Row::Tab {
                    id: ids[1],
                    group: Some(group)
                },
            ]
        );
    }
}
