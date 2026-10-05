//! Painting a pane's command blocks as a scrolling list: a header per block
//! with its context, command, and outcome, above the block's output rows.

use std::{rc::Rc, time::Duration};

use gpui::{
    AnyElement, Hsla, ListState, Pixels, SharedString, canvas, div, list, point, prelude::*, px,
};

use crate::{
    blocks::{Block, BlockContext, Outcome, Rows},
    components::icon,
    grid::{CellMetrics, Frame, paint_rows},
    process_info::shorten_home,
    theme::{self, Theme},
};

const HEADER_HEIGHT: f32 = 26.;
const BLOCK_GAP: f32 = 8.;
const FAILURE_EDGE: f32 = 2.;

/// What one list entry shows.
pub enum ItemContent {
    /// A finished command's captured output.
    Finished(Rows),
    /// A running command: rows that scrolled off its terminal, followed by
    /// the terminal's live screen.
    Running(Vec<Rows>),
    /// The shell's prompt, drawn from the live terminal.
    Prompt,
}

pub struct Item {
    header: Option<Header>,
    content: ItemContent,
}

struct Header {
    command: SharedString,
    context: BlockContext,
    outcome: Option<Outcome>,
    failed: bool,
}

impl Item {
    pub fn block(block: &Block, content: ItemContent) -> Self {
        Self {
            header: Some(Header {
                command: block.command.clone().into(),
                context: block.context.clone(),
                outcome: block.outcome,
                failed: block.failed(),
            }),
            content,
        }
    }

    pub fn prompt() -> Self {
        Self {
            header: None,
            content: ItemContent::Prompt,
        }
    }

    fn failed(&self) -> bool {
        self.header.as_ref().is_some_and(|header| header.failed)
    }
}

/// Colors the list needs, copied out of the theme so items can be drawn
/// from a `'static` closure.
#[derive(Clone, Copy)]
struct Palette {
    text: Hsla,
    muted: Hsla,
    border: Hsla,
    chip: Hsla,
    error: Hsla,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        Self {
            text: theme::to_hsla(theme.terminal.foreground),
            muted: theme.text_muted,
            border: theme.border_variant,
            chip: theme.element_background,
            error: theme::to_hsla(theme.terminal.ansi[1]),
        }
    }
}

/// The block list. `live` is the frame of the terminal receiving output,
/// painted under the running block's scrollback or as the prompt.
pub fn render_list(
    state: &ListState,
    items: Rc<Vec<Item>>,
    live: Option<Rc<Frame>>,
    metrics: CellMetrics,
    theme: &Theme,
) -> AnyElement {
    let palette = Palette::new(theme);
    list(state.clone(), move |index, _window, _cx| {
        let Some(item) = items.get(index) else {
            return div().into_any_element();
        };
        render_item(item, live.clone(), metrics, palette)
    })
    .size_full()
    .into_any_element()
}

fn render_item(
    item: &Item,
    live: Option<Rc<Frame>>,
    metrics: CellMetrics,
    palette: Palette,
) -> AnyElement {
    let (rows, scrollback, live_frame) = match &item.content {
        ItemContent::Finished(rows) => (Some(rows.clone()), Vec::new(), None),
        ItemContent::Running(scrollback) => (None, scrollback.clone(), live),
        ItemContent::Prompt => (None, Vec::new(), live),
    };
    let row_count = rows.as_ref().map_or(0, |rows| rows.len())
        + scrollback.iter().map(|page| page.len()).sum::<usize>()
        + live_frame.as_ref().map_or(0, |frame| frame.content_rows());
    let output_height = metrics.height * row_count as f32;
    let padding = metrics.padding;

    let output = canvas(
        |_, _, _| {},
        move |bounds, (), window, cx| {
            let mut origin = bounds.origin + point(padding, px(0.));
            if let Some(rows) = &rows {
                paint_rows(rows, origin, metrics, window, cx);
            }
            for page in &scrollback {
                paint_rows(page, origin, metrics, window, cx);
                origin.y += metrics.height * page.len() as f32;
            }
            if let Some(frame) = &live_frame {
                frame.paint_content(origin, metrics, window, cx);
            }
        },
    )
    .w_full()
    .h(output_height);

    let edge = if item.failed() {
        palette.error
    } else {
        gpui::transparent_black()
    };
    div()
        .flex()
        .flex_col()
        .w_full()
        .pb(px(BLOCK_GAP))
        .border_l(px(FAILURE_EDGE))
        .border_color(edge)
        .when(item.header.is_some(), |this| {
            this.border_t_1().border_color(palette.border)
        })
        .when(item.header.is_none(), |this| this.pt(padding))
        .children(
            item.header
                .as_ref()
                .map(|header| render_header(header, padding, palette)),
        )
        .child(output)
        .into_any_element()
}

fn render_header(header: &Header, padding: Pixels, palette: Palette) -> AnyElement {
    let context = &header.context;
    let chips = context
        .cwd
        .as_ref()
        .map(|cwd| chip("folder", shorten_home(cwd), palette))
        .into_iter()
        .chain(
            context
                .virtualenv
                .as_ref()
                .map(|venv| chip("package", venv.clone(), palette)),
        )
        .chain(
            context
                .conda_env
                .as_ref()
                .map(|conda| chip("flask-conical", conda.clone(), palette)),
        );
    let outcome = header.outcome.map(|outcome| {
        let failed = outcome.exit_code != 0;
        div()
            .flex()
            .items_center()
            .gap_2()
            .flex_none()
            .when(failed, |this| {
                this.child(
                    div()
                        .text_color(palette.error)
                        .child(format!("exit {}", outcome.exit_code)),
                )
            })
            .child(
                div()
                    .text_color(palette.muted)
                    .child(format_duration(outcome.duration)),
            )
    });
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(HEADER_HEIGHT))
        .py_1()
        .px(padding)
        .text_xs()
        .font_family(theme::UI_FONT_FAMILY)
        .children(chips)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_family(theme::FONT_FAMILY)
                .text_sm()
                .text_color(palette.text)
                .child(header.command.clone()),
        )
        .children(outcome)
        .into_any_element()
}

fn chip(icon_name: &'static str, label: String, palette: Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .bg(palette.chip)
        .text_color(palette.muted)
        .child(icon(icon_name, px(11.), palette.muted))
        .child(label)
}

fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", duration.as_secs_f32())
    } else {
        let seconds = duration.as_secs();
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(4300)), "4.3s");
        assert_eq!(format_duration(Duration::from_secs(192)), "3m 12s");
    }
}
