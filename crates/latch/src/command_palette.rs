//! The command palette ⌘K opens over the window: one fuzzy search across
//! the open tabs, the commands that apply to what is focused, and themes.
//! Themes only show once something is typed, since Ghostty can bring
//! hundreds of them.

use std::{any::TypeId, collections::HashSet, ops::Range};

use gpui::{
    Action, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    HighlightStyle, KeyBinding, MouseButton, ScrollHandle, SharedString, StyledText, Subscription,
    Window, actions, div, hsla, prelude::*, px,
};

use crate::{
    app_menus::{
        CompactTabs, DecreaseFontSize, ExpandedTabs, IncreaseFontSize, OpenSettingsFile,
        ResetFontSize, ToggleStatusBar,
    },
    components::{elevated_shadow, icon},
    history,
    pane_group::{ClosePane, EqualizePanes, SplitDown, SplitRight, ToggleZoom},
    settings::SettingsStore,
    tabs::TabId,
    terminal_view::{ClearScrollback, Find},
    text_input::{TextInput, TextInputEvent},
    theme::{self, ActiveThemeExt, ThemeRegistry},
    workspace::{
        CloseTab, CloseWindow, NewAgent, NewTab, NextAttention, OpenSettings, Quit, RenameTab,
        StopTab, ToggleSidebar,
    },
};

actions!(command_palette, [SelectNext, SelectPrevious]);

pub const KEY_CONTEXT: &str = "CommandPalette";

const WIDTH: f32 = 580.;
const LIST_MAX_HEIGHT: f32 = 360.;
/// Distance from the top of the window to the palette.
const TOP_OFFSET: f32 = 64.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Tabs,
    Commands,
    Themes,
}

impl Section {
    fn caption(self) -> &'static str {
        match self {
            Section::Tabs => "Tabs",
            Section::Commands => "Commands",
            Section::Themes => "Themes",
        }
    }

    /// The short tag shown on a row when results from every section mix.
    fn tag(self) -> &'static str {
        match self {
            Section::Tabs => "Tab",
            Section::Commands => "Command",
            Section::Themes => "Theme",
        }
    }
}

/// What choosing an item does.
pub enum PaletteTarget {
    Tab(TabId),
    Action(Box<dyn Action>),
    Theme(SharedString),
}

impl Clone for PaletteTarget {
    fn clone(&self) -> Self {
        match self {
            PaletteTarget::Tab(id) => PaletteTarget::Tab(*id),
            PaletteTarget::Action(action) => PaletteTarget::Action(action.boxed_clone()),
            PaletteTarget::Theme(name) => PaletteTarget::Theme(name.clone()),
        }
    }
}

pub struct PaletteItem {
    pub section: Section,
    pub icon: &'static str,
    pub label: SharedString,
    /// Muted text after the label, such as a tab's directory.
    pub detail: Option<SharedString>,
    pub keystroke: Option<SharedString>,
    /// The active tab or theme.
    pub current: bool,
    pub target: PaletteTarget,
}

impl PaletteItem {
    /// Words the query is matched against: an optional section word, so
    /// "theme ayu" finds Ayu themes, then the label and the detail.
    fn search_prefix(&self) -> &'static str {
        match self.section {
            Section::Themes => "theme ",
            Section::Tabs | Section::Commands => "",
        }
    }

    fn search_text(&self) -> String {
        let mut text = format!("{}{}", self.search_prefix(), self.label);
        if let Some(detail) = &self.detail {
            text.push(' ');
            text.push_str(detail);
        }
        text
    }
}

/// The commands the palette lists, available or not; the workspace drops
/// the ones nothing focused can handle.
pub fn commands(cx: &App) -> Vec<(&'static str, SharedString, Box<dyn Action>)> {
    let settings = SettingsStore::get(cx);
    let status_bar = if settings.status_bar.visible {
        "Hide Status Bar"
    } else {
        "Show Status Bar"
    };
    let mut commands: Vec<(&'static str, SharedString, Box<dyn Action>)> =
        vec![("plus", "New Tab".into(), Box::new(NewTab))];
    commands.extend(settings.agent_profiles.iter().enumerate().map(
        |(index, profile)| -> (&'static str, SharedString, Box<dyn Action>) {
            (
                "bot",
                format!("New Agent: {}", profile.name).into(),
                Box::new(NewAgent(index)),
            )
        },
    ));
    commands.extend([
        (
            "panel-left",
            SharedString::from("Split Right"),
            Box::new(SplitRight) as Box<dyn Action>,
        ),
        ("panel-bottom", "Split Down".into(), Box::new(SplitDown)),
        ("layers", "Zoom Pane".into(), Box::new(ToggleZoom)),
        ("layers", "Equalize Panes".into(), Box::new(EqualizePanes)),
        ("pencil", "Rename Tab…".into(), Box::new(RenameTab)),
        ("search", "Find…".into(), Box::new(Find)),
        (
            "rotate-ccw",
            "Clear Scrollback".into(),
            Box::new(ClearScrollback),
        ),
        (
            "circle-alert",
            "Next Session Needing Attention".into(),
            Box::new(NextAttention),
        ),
        (
            "panel-left",
            "Toggle Sidebar".into(),
            Box::new(ToggleSidebar),
        ),
        ("panel-bottom", status_bar.into(), Box::new(ToggleStatusBar)),
        ("list-filter", "Compact Tabs".into(), Box::new(CompactTabs)),
        (
            "list-filter",
            "Expanded Tabs".into(),
            Box::new(ExpandedTabs),
        ),
        (
            "plus",
            "Increase Font Size".into(),
            Box::new(IncreaseFontSize),
        ),
        (
            "minus",
            "Decrease Font Size".into(),
            Box::new(DecreaseFontSize),
        ),
        (
            "rotate-ccw",
            "Reset Font Size".into(),
            Box::new(ResetFontSize),
        ),
        ("settings", "Settings".into(), Box::new(OpenSettings)),
        (
            "file-pen-line",
            "Open Settings File".into(),
            Box::new(OpenSettingsFile),
        ),
        ("x", "Close Pane".into(), Box::new(ClosePane)),
        ("x", "Close Tab".into(), Box::new(CloseTab)),
        ("stop-filled", "Stop Sessions…".into(), Box::new(StopTab)),
        ("x", "Close Window".into(), Box::new(CloseWindow)),
        ("x", "Quit".into(), Box::new(Quit)),
    ]);
    commands
}

/// Palette items for the commands that something along the focused
/// element's path, or the app itself, handles, with their shortcuts.
pub fn command_items(window: &Window, cx: &App) -> Vec<PaletteItem> {
    let available: HashSet<TypeId> = window
        .available_actions(cx)
        .iter()
        .map(|action| action.as_any().type_id())
        .collect();
    commands(cx)
        .into_iter()
        .filter(|(_, _, action)| available.contains(&action.as_any().type_id()))
        .map(|(icon, label, action)| PaletteItem {
            section: Section::Commands,
            icon,
            label,
            detail: None,
            keystroke: window
                .highest_precedence_binding_for_action(action.as_ref())
                .map(|binding| keystroke_label(&binding)),
            current: false,
            target: PaletteTarget::Action(action),
        })
        .collect()
}

/// Palette items for every loaded theme.
pub fn theme_items(cx: &App) -> Vec<PaletteItem> {
    let current = SettingsStore::get(cx).theme.clone();
    ThemeRegistry::themes(cx)
        .iter()
        .map(|theme| PaletteItem {
            section: Section::Themes,
            icon: "palette",
            label: theme.name.clone(),
            detail: None,
            keystroke: None,
            current: theme.name.as_ref() == current,
            target: PaletteTarget::Theme(theme.name.clone()),
        })
        .collect()
}

fn keystroke_label(binding: &KeyBinding) -> SharedString {
    binding
        .keystrokes()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

pub enum CommandPaletteEvent {
    Chosen(PaletteTarget),
    Dismissed,
}

/// One shown row: an item and the characters of its label that matched.
struct Match {
    item: usize,
    positions: Vec<usize>,
}

pub struct CommandPalette {
    items: Vec<PaletteItem>,
    input: Entity<TextInput>,
    matches: Vec<Match>,
    selected: usize,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl EventEmitter<CommandPaletteEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl CommandPalette {
    pub fn new(items: Vec<PaletteItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search tabs, commands, and themes…", cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &TextInputEvent, _, cx| match event {
                TextInputEvent::Changed => this.update_matches(cx),
                TextInputEvent::Confirmed => this.confirm(this.selected, cx),
                TextInputEvent::Cancelled => cx.emit(CommandPaletteEvent::Dismissed),
            },
        );
        let mut palette = Self {
            items,
            input,
            matches: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        };
        palette.update_matches(cx);
        palette
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).text().trim().to_string()
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        self.matches = rank(&self.items, &query);
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn confirm(&mut self, position: usize, cx: &mut Context<Self>) {
        if let Some(found) = self.matches.get(position) {
            let target = self.items[found.item].target.clone();
            cx.emit(CommandPaletteEvent::Chosen(target));
        }
    }

    fn select(&mut self, position: usize, cx: &mut Context<Self>) {
        self.selected = position;
        self.scroll.scroll_to_item(self.child_index(position, cx));
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.select((self.selected + 1) % self.matches.len(), cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            let count = self.matches.len();
            self.select((self.selected + count - 1) % count, cx);
        }
    }

    /// Section captions show only while browsing without a query.
    fn grouped(&self, cx: &App) -> bool {
        self.query(cx).is_empty()
    }

    /// Position of the row for `position` among the list's children,
    /// counting the section captions before it.
    fn child_index(&self, position: usize, cx: &App) -> usize {
        if !self.grouped(cx) {
            return position;
        }
        let captions = section_starts(&self.items, &self.matches)
            .filter(|&start| start <= position)
            .count();
        position + captions
    }
}

/// Matches for `query`, best first. Without a query, every item except
/// themes, in the order given.
fn rank(items: &[PaletteItem], query: &str) -> Vec<Match> {
    if query.is_empty() {
        return items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.section != Section::Themes)
            .map(|(item, _)| Match {
                item,
                positions: Vec::new(),
            })
            .collect();
    }
    let lowered: Vec<char> = query.to_lowercase().chars().collect();
    let mut scored: Vec<((usize, usize), Match)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let text = item.search_text();
            let (start, end) = history::fuzzy_match(&text, &lowered)?;
            let prefix = item.search_prefix().chars().count();
            let label = item.label.chars().count();
            let positions = history::fuzzy_positions(&text, query)
                .into_iter()
                .filter(|&position| position >= prefix && position < prefix + label)
                .map(|position| position - prefix)
                .collect();
            Some((
                (end - start, start),
                Match {
                    item: index,
                    positions,
                },
            ))
        })
        .collect();
    // Tighter matches first, then earlier ones; the sort is stable, so
    // ties keep the given order.
    scored.sort_by_key(|(score, _)| *score);
    scored.into_iter().map(|(_, found)| found).collect()
}

/// Positions in `matches` where a new section begins.
fn section_starts<'a>(
    items: &'a [PaletteItem],
    matches: &'a [Match],
) -> impl Iterator<Item = usize> + 'a {
    (0..matches.len()).filter(move |&position| {
        position == 0
            || items[matches[position].item].section != items[matches[position - 1].item].section
    })
}

/// Byte ranges of the characters at `positions` in `text`.
fn char_ranges(text: &str, positions: &[usize]) -> Vec<Range<usize>> {
    text.char_indices()
        .enumerate()
        .filter(|(position, _)| positions.contains(position))
        .map(|(_, (start, ch))| start..start + ch.len_utf8())
        .collect()
}

/// `text` with the characters at `positions` drawn bold.
fn emphasized(text: &SharedString, positions: &[usize]) -> StyledText {
    let highlights = char_ranges(text, positions).into_iter().map(|range| {
        (
            range,
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            },
        )
    });
    StyledText::new(text.clone()).with_highlights(highlights.collect::<Vec<_>>())
}

fn key_hint(keys: &'static str, label: &'static str, theme: &theme::Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .child(
            div()
                .px(px(4.))
                .rounded(theme::RADIUS_SM)
                .bg(theme.element_background)
                .text_color(theme.text_muted)
                .child(keys),
        )
        .child(label)
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let grouped = self.grouped(cx);
        let starts: HashSet<usize> = section_starts(&self.items, &self.matches).collect();
        let hover = theme.ghost_hover;

        let mut rows = Vec::with_capacity(self.matches.len() + 3);
        for (position, found) in self.matches.iter().enumerate() {
            let item = &self.items[found.item];
            if grouped && starts.contains(&position) {
                rows.push(
                    div()
                        .px(px(8.))
                        .pt(px(if position == 0 { 4. } else { 10. }))
                        .pb(px(4.))
                        .text_size(theme::TEXT_SMALL)
                        .text_color(theme.text_muted)
                        .child(item.section.caption())
                        .into_any_element(),
                );
            }
            let selected = position == self.selected;
            let trailing = match (&item.keystroke, grouped) {
                (Some(keys), _) => Some(
                    div()
                        .flex_none()
                        .px(px(5.))
                        .rounded(theme::RADIUS_SM)
                        .bg(theme.element_background)
                        .text_size(theme::TEXT_SMALL)
                        .text_color(theme.text_muted)
                        .child(keys.clone())
                        .into_any_element(),
                ),
                (None, false) => Some(
                    div()
                        .flex_none()
                        .text_size(theme::TEXT_SMALL)
                        .text_color(theme.text_placeholder)
                        .child(item.section.tag())
                        .into_any_element(),
                ),
                (None, true) => None,
            };
            rows.push(
                div()
                    .id(("palette-row", position))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(10.))
                    .h(px(30.))
                    .px(px(8.))
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .when(selected, |row| row.bg(theme.ghost_selected))
                    .when(!selected, |row| row.hover(move |style| style.bg(hover)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.confirm(position, cx);
                    }))
                    .child(icon(
                        item.icon,
                        theme::ICON_SMALL,
                        if selected {
                            theme.text_accent
                        } else {
                            theme.text_muted
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.))
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(WIDTH * 0.6))
                                    .truncate()
                                    .text_size(theme::TEXT_DEFAULT)
                                    .text_color(theme.text)
                                    .child(emphasized(&item.label, &found.positions)),
                            )
                            .children(item.detail.clone().map(|detail| {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme::TEXT_SMALL)
                                    .text_color(theme.text_muted)
                                    .child(detail)
                            })),
                    )
                    .when(item.current, |row| {
                        row.child(icon("check", theme::ICON_SMALL, theme.text_accent))
                    })
                    .children(trailing)
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px(px(8.))
                    .py(px(10.))
                    .text_size(theme::TEXT_SMALL)
                    .text_color(theme.text_muted)
                    .child("Nothing matches")
                    .into_any_element(),
            );
        }

        // The backdrop covers the window; a click on it closes the palette.
        div()
            .id("command-palette-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(TOP_OFFSET))
            .px(px(16.))
            .bg(hsla(0., 0., 0., 0.2))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(CommandPaletteEvent::Dismissed)),
            )
            .child(
                div()
                    .id("command-palette")
                    .key_context(KEY_CONTEXT)
                    .on_action(cx.listener(Self::select_next))
                    .on_action(cx.listener(Self::select_previous))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .flex()
                    .flex_col()
                    .w(px(WIDTH))
                    .max_w_full()
                    .rounded(theme::RADIUS_LG)
                    .border_1()
                    .border_color(theme.border_variant)
                    .bg(theme.elevated_surface)
                    .shadow(elevated_shadow())
                    .font_family(theme::UI_FONT_FAMILY)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(10.))
                            .h(px(42.))
                            .px(px(14.))
                            .border_b_1()
                            .border_color(theme.border_variant)
                            .child(icon("search", theme::ICON_SMALL, theme.text_muted))
                            .child(div().flex_1().min_w_0().child(self.input.clone())),
                    )
                    .child(
                        div()
                            .id("command-palette-list")
                            .flex()
                            .flex_col()
                            .max_h(px(LIST_MAX_HEIGHT))
                            .p(px(4.))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(rows),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(14.))
                            .h(px(28.))
                            .px(px(14.))
                            .border_t_1()
                            .border_color(theme.border_variant)
                            .text_size(theme::TEXT_SMALL)
                            .text_color(theme.text_placeholder)
                            .child(key_hint("↑↓", "Navigate", &theme))
                            .child(key_hint("↵", "Open", &theme))
                            .child(key_hint("esc", "Close", &theme)),
                    ),
            )
    }
}

pub fn key_bindings() -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    vec![
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("ctrl-n", SelectNext, context),
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("ctrl-p", SelectPrevious, context),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(section: Section, label: &str) -> PaletteItem {
        PaletteItem {
            section,
            icon: "terminal",
            label: label.to_string().into(),
            detail: None,
            keystroke: None,
            current: false,
            target: PaletteTarget::Theme(label.to_string().into()),
        }
    }

    fn labels(items: &[PaletteItem], matches: &[Match]) -> Vec<String> {
        matches
            .iter()
            .map(|found| items[found.item].label.to_string())
            .collect()
    }

    #[test]
    fn browsing_lists_everything_but_themes_in_order() {
        let items = vec![
            item(Section::Tabs, "zsh"),
            item(Section::Commands, "New Tab"),
            item(Section::Themes, "Ayu Dark"),
        ];
        assert_eq!(labels(&items, &rank(&items, "")), ["zsh", "New Tab"]);
    }

    #[test]
    fn queries_rank_tight_matches_first_and_reach_themes() {
        let items = vec![
            item(Section::Commands, "Split Right"),
            item(Section::Commands, "Split Down"),
            item(Section::Themes, "Ayu Dark"),
        ];
        assert_eq!(labels(&items, &rank(&items, "down")), ["Split Down"]);
        assert_eq!(labels(&items, &rank(&items, "theme ayu")), ["Ayu Dark"]);
        assert_eq!(
            labels(&items, &rank(&items, "sp")),
            ["Split Right", "Split Down"]
        );
    }

    #[test]
    fn highlights_only_cover_the_label() {
        let items = vec![item(Section::Themes, "Ayu Dark")];
        let matches = rank(&items, "theme ad");
        assert_eq!(matches[0].positions, vec![0, 4]);
    }

    #[test]
    fn captions_shift_child_indices() {
        let items = vec![
            item(Section::Tabs, "zsh"),
            item(Section::Tabs, "vim"),
            item(Section::Commands, "New Tab"),
        ];
        let matches = rank(&items, "");
        assert_eq!(section_starts(&items, &matches).collect::<Vec<_>>(), [0, 2]);
    }
}
