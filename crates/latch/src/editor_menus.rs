//! Menus that open over the command editor: completions, the history
//! menu Up opens, and the Ctrl-R history search. They share one frame and
//! row style, with matched text in bold and the selection in the accent
//! color.

use std::ops::Range;

use gpui::{
    AnyElement, FontWeight, HighlightStyle, Hsla, SharedString, StyledText, div, prelude::*, px,
};

use crate::{
    completion::CompletionMenu,
    components::{elevated_shadow, icon},
    grid::CellMetrics,
    history::{self, History},
    theme::{self, Theme},
};

/// Rows shown at once; the rest scroll with the selection.
const VISIBLE_ROWS: usize = 9;
const MENU_MIN_WIDTH: f32 = 330.;
const MENU_MAX_WIDTH: f32 = 640.;

/// The history menu Up opens: entries starting with what was typed,
/// previewed in the editor as they are selected.
pub struct HistoryMenu {
    /// What was typed when the menu opened, restored on Escape.
    pub original: String,
    /// Indices into history, newest first.
    pub matches: Vec<usize>,
    /// Position in `matches`; 0 is the newest.
    pub selected: usize,
}

/// The Ctrl-R search: the editor's text is the query.
#[derive(Default)]
pub struct HistorySearch {
    /// Indices into history, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
}

#[derive(Clone, Copy)]
struct Colors {
    surface: Hsla,
    outline: Hsla,
    selected: Hsla,
    text: Hsla,
    muted: Hsla,
}

impl Colors {
    fn new(theme: &Theme) -> Self {
        let foreground = theme::to_hsla(theme.terminal.foreground);
        Self {
            surface: theme.elevated_surface,
            outline: foreground.opacity(0.1),
            selected: theme.text_accent.opacity(0.3),
            text: foreground,
            muted: foreground.opacity(0.6),
        }
    }
}

/// The rows of `count` to show so the selection stays in view.
fn window(count: usize, selected: usize) -> Range<usize> {
    let first = selected
        .saturating_sub(VISIBLE_ROWS / 2)
        .min(count.saturating_sub(VISIBLE_ROWS));
    first..(first + VISIBLE_ROWS).min(count)
}

fn frame(caption: &'static str, rows: Vec<AnyElement>, colors: Colors) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .min_w(px(MENU_MIN_WIDTH))
        .max_w(px(MENU_MAX_WIDTH))
        .p(px(4.))
        .rounded(px(6.))
        .border_1()
        .border_color(colors.outline)
        .bg(colors.surface)
        .shadow(elevated_shadow())
        .child(
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .px(px(8.))
                .font_family(theme::UI_FONT_FAMILY)
                .text_xs()
                .text_color(colors.muted)
                .child(caption),
        )
        .children(rows)
        .into_any_element()
}

fn row(selected: bool, colors: Colors) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .py(px(3.))
        .rounded(px(4.))
        .when(selected, |row| row.bg(colors.selected))
}

/// `text` with the bytes in `bold` drawn bold.
fn emphasized(text: String, bold: Vec<Range<usize>>) -> StyledText {
    let highlights = bold.into_iter().map(|range| {
        (
            range,
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            },
        )
    });
    StyledText::new(SharedString::from(text)).with_highlights(highlights.collect::<Vec<_>>())
}

/// The first line of a command, for one-line rows.
fn first_line(command: &str) -> String {
    let mut lines = command.lines();
    let first = lines.next().unwrap_or_default().to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

pub fn render_completions(
    menu: &CompletionMenu,
    metrics: CellMetrics,
    theme: &Theme,
) -> AnyElement {
    let colors = Colors::new(theme);
    let count = menu.visible().count();
    let shown = window(count, menu.selected());
    let matched = menu.matched_len();
    let rows = menu
        .visible()
        .skip(shown.start)
        .take(shown.len())
        .map(|(index, completion)| {
            let kind = if completion.word.ends_with('/') {
                "folder"
            } else if completion.word.starts_with('-') {
                "minus"
            } else {
                "code"
            };
            let bold_end = (0..=matched.min(completion.word.len()))
                .rev()
                .find(|&end| completion.word.is_char_boundary(end))
                .unwrap_or(0);
            row(index == menu.selected(), colors)
                .child(icon(kind, metrics.font_size - px(1.), colors.muted))
                .child(
                    div()
                        .flex_none()
                        .font_family(theme::FONT_FAMILY)
                        .text_size(metrics.font_size - px(1.))
                        .text_color(colors.text)
                        .child(emphasized(
                            completion.word.clone(),
                            (bold_end > 0).then_some(0..bold_end).into_iter().collect(),
                        )),
                )
                .children(completion.description.clone().map(|description| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(theme::UI_FONT_FAMILY)
                        .text_xs()
                        .text_color(colors.muted)
                        .child(description)
                }))
                .into_any_element()
        })
        .collect();
    frame("Completions", rows, colors)
}

/// A history row: the command, with `bold` characters, and how long ago
/// it ran.
fn history_row(
    history: &History,
    index: usize,
    bold: Vec<Range<usize>>,
    selected: bool,
    metrics: CellMetrics,
    colors: Colors,
) -> Option<AnyElement> {
    let entry = history.get(index)?;
    Some(
        row(selected, colors)
            .child(icon("terminal", metrics.font_size - px(1.), colors.muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(theme::FONT_FAMILY)
                    .text_size(metrics.font_size - px(1.))
                    .text_color(colors.text)
                    .child(emphasized(first_line(&entry.command), bold)),
            )
            .children(entry.time.map(|time| {
                div()
                    .flex_none()
                    .font_family(theme::UI_FONT_FAMILY)
                    .text_xs()
                    .text_color(colors.muted)
                    .child(history::relative_time(time))
            }))
            .into_any_element(),
    )
}

/// The history menu, newest entry at the bottom next to the editor.
pub fn render_history_menu(
    menu: &HistoryMenu,
    history: &History,
    metrics: CellMetrics,
    theme: &Theme,
) -> AnyElement {
    let colors = Colors::new(theme);
    let typed = menu.original.len();
    let shown = window(menu.matches.len(), menu.selected);
    let rows = shown
        .rev()
        .filter_map(|position| {
            let index = menu.matches[position];
            let bold = (typed > 0).then_some(0..typed).into_iter().collect();
            history_row(
                history,
                index,
                bold,
                position == menu.selected,
                metrics,
                colors,
            )
        })
        .collect();
    frame("History", rows, colors)
}

/// The Ctrl-R search, best match at the bottom next to the editor.
pub fn render_history_search(
    search: &HistorySearch,
    history: &History,
    query: &str,
    metrics: CellMetrics,
    theme: &Theme,
) -> AnyElement {
    let colors = Colors::new(theme);
    let shown = window(search.matches.len(), search.selected);
    let mut rows: Vec<AnyElement> = shown
        .rev()
        .filter_map(|position| {
            let index = search.matches[position];
            let line = first_line(&history.get(index)?.command);
            history_row(
                history,
                index,
                char_ranges(&line, &history::fuzzy_positions(&line, query)),
                position == search.selected,
                metrics,
                colors,
            )
        })
        .collect();
    if rows.is_empty() {
        rows.push(
            row(false, colors)
                .font_family(theme::UI_FONT_FAMILY)
                .text_xs()
                .text_color(colors.muted)
                .child("No matching commands")
                .into_any_element(),
        );
    }
    frame("Search history", rows, colors)
}

/// Byte ranges of the characters at `positions` in `text`.
fn char_ranges(text: &str, positions: &[usize]) -> Vec<Range<usize>> {
    text.char_indices()
        .enumerate()
        .filter(|(position, _)| positions.contains(position))
        .map(|(_, (start, ch))| start..start + ch.len_utf8())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_follows_the_selection() {
        assert_eq!(window(3, 0), 0..3);
        assert_eq!(window(30, 0), 0..9);
        assert_eq!(window(30, 20), 16..25);
        assert_eq!(window(30, 29), 21..30);
    }

    #[test]
    fn matched_characters_map_to_byte_ranges() {
        assert_eq!(char_ranges("héllo", &[1, 3]), vec![1..3, 4..5]);
        assert_eq!(first_line("one\ntwo"), "one …");
    }
}
