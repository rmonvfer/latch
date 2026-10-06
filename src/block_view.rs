//! Painting a pane's command blocks as a scrolling list: a header per block
//! with its context, command, and outcome, above the block's output rows.

use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Hsla, ListState, Pixels, Point, SharedString, Window,
    canvas, div, fill, list, point, prelude::*, px, size,
};

use crate::{
    blocks::{Block, BlockPoint, BlockSelection, Outcome, Rows},
    components::icon,
    grid::{CellMetrics, Frame, paint_rows},
    process_info::shorten_home,
    runtime_protocol::BlockContext,
    theme::{self, Theme},
};

/// Space between a block's content and the pane's sides, on top of the
/// terminal padding. The pane's grid is narrowed to match.
pub const HORIZONTAL_INSET: Pixels = px(12.);
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
}

pub struct Item {
    header: Option<Header>,
    content: ItemContent,
    collapsed: bool,
    selected: bool,
    /// Text selected across blocks, painted where it covers this block.
    selection: Option<BlockSelection>,
}

/// Where each visible block's output was last painted, for mapping the
/// pointer to a cell.
#[derive(Clone, Default)]
pub struct PaintedOutputs(Rc<RefCell<Vec<PaintedOutput>>>);

#[derive(Clone, Copy)]
struct PaintedOutput {
    block: usize,
    /// Top-left of the block's first row, first column.
    origin: Point<Pixels>,
    rows: usize,
}

impl PaintedOutputs {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    fn record(&self, output: PaintedOutput) {
        self.0.borrow_mut().push(output);
    }

    /// The cell at `position`, or for a point between or beyond blocks,
    /// the nearest cell of the nearest block, so drags keep selecting.
    pub fn hit(&self, position: Point<Pixels>, metrics: CellMetrics) -> Option<BlockPoint> {
        let outputs = self.0.borrow();
        let distance = |output: &PaintedOutput| {
            let top = output.origin.y;
            let bottom = top + metrics.height * output.rows as f32;
            if position.y < top {
                top - position.y
            } else if position.y >= bottom {
                position.y - bottom
            } else {
                px(0.)
            }
        };
        let nearest = outputs
            .iter()
            .filter(|output| output.rows > 0)
            .min_by(|a, b| f32::from(distance(a)).total_cmp(&f32::from(distance(b))))?;
        let row = ((position.y - nearest.origin.y) / metrics.height).floor();
        let col = ((position.x - nearest.origin.x) / metrics.width)
            .round()
            .max(0.) as usize;
        let (row, col) = if row < 0. {
            (0, 0)
        } else if row as usize >= nearest.rows {
            (nearest.rows - 1, usize::MAX)
        } else {
            (row as usize, col)
        };
        Some(BlockPoint {
            block: nearest.block,
            row,
            col,
        })
    }
}

/// Something done to a block from its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockAction {
    Select,
    ToggleCollapsed,
    CopyCommand,
    CopyOutput,
    Rerun,
}

/// Handles a block action for the block at an index.
pub type OnBlockAction = Rc<dyn Fn(BlockAction, usize, &mut Window, &mut App)>;

struct Header {
    command: SharedString,
    context: BlockContext,
    outcome: Option<Outcome>,
    failed: bool,
    running: bool,
}

impl Item {
    pub fn block(block: &Block, content: ItemContent, selected: bool) -> Self {
        Self {
            // Startup output has no command to show.
            header: (!block.command.is_empty()).then(|| Header {
                command: block.command.clone().into(),
                context: block.context.clone(),
                outcome: block.outcome,
                failed: block.failed(),
                running: block.is_running(),
            }),
            content,
            collapsed: block.collapsed,
            selected,
            selection: None,
        }
    }

    pub fn with_selection(mut self, selection: Option<BlockSelection>) -> Self {
        self.selection = selection;
        self
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
    selected: Hsla,
    selection: Hsla,
    hover: Hsla,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        Self {
            text: theme::to_hsla(theme.terminal.foreground),
            muted: theme.text_muted,
            border: theme.border_variant,
            chip: theme.element_background,
            error: theme::to_hsla(theme.terminal.ansi[1]),
            selected: theme.ghost_selected.opacity(0.5),
            selection: theme.text_accent.opacity(0.3),
            hover: theme.ghost_hover,
        }
    }
}

/// The block list. `live` is the frame of the running command's terminal,
/// painted under its scrollback.
pub fn render_list(
    state: &ListState,
    items: Rc<Vec<Item>>,
    live: Option<Rc<Frame>>,
    metrics: CellMetrics,
    theme: &Theme,
    on_action: OnBlockAction,
    painted: PaintedOutputs,
) -> AnyElement {
    let palette = Palette::new(theme);
    list(state.clone(), move |index, _window, _cx| {
        let Some(item) = items.get(index) else {
            return div().into_any_element();
        };
        render_item(
            index,
            item,
            live.clone(),
            metrics,
            palette,
            on_action.clone(),
            painted.clone(),
        )
    })
    .size_full()
    .into_any_element()
}

fn render_item(
    index: usize,
    item: &Item,
    live: Option<Rc<Frame>>,
    metrics: CellMetrics,
    palette: Palette,
    on_action: OnBlockAction,
    painted: PaintedOutputs,
) -> AnyElement {
    let (rows, scrollback, live_frame) = match &item.content {
        _ if item.collapsed => (None, Vec::new(), None),
        ItemContent::Finished(rows) => (Some(rows.clone()), Vec::new(), None),
        ItemContent::Running(scrollback) => (None, scrollback.clone(), live),
    };
    let row_count = rows.as_ref().map_or(0, |rows| rows.len())
        + scrollback.iter().map(|page| page.len()).sum::<usize>()
        + live_frame.as_ref().map_or(0, |frame| frame.content_rows());
    let output_height = metrics.height * row_count as f32;
    let padding = metrics.padding + HORIZONTAL_INSET;
    let selection = item.selection;

    let output = canvas(
        |_, _, _| {},
        move |bounds, (), window, cx| {
            let mut origin = bounds.origin + point(padding, px(0.));
            painted.record(PaintedOutput {
                block: index,
                origin,
                rows: row_count,
            });
            let selection_origin = origin;
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
            if let Some(selection) = selection {
                let right = bounds.right() - padding;
                for row in 0..row_count {
                    let Some((from, to)) = selection.columns_in(index, row) else {
                        continue;
                    };
                    let top = selection_origin.y + metrics.height * row as f32;
                    let left = selection_origin.x + metrics.width * from as f32;
                    let end = if to == usize::MAX {
                        right
                    } else {
                        (selection_origin.x + metrics.width * to as f32).min(right)
                    };
                    if end > left {
                        window.paint_quad(fill(
                            Bounds::new(point(left, top), size(end - left, metrics.height)),
                            palette.selection,
                        ));
                    }
                }
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
    let select = on_action.clone();
    div()
        .id(("block", index))
        .group("block")
        .flex()
        .flex_col()
        .w_full()
        .pb(px(BLOCK_GAP))
        .border_l(px(FAILURE_EDGE))
        .border_color(edge)
        .when(item.selected, |this| this.bg(palette.selected))
        .when(item.header.is_some(), |this| {
            this.border_t_1().border_color(palette.border)
        })
        .when(item.header.is_none(), |this| this.pt(metrics.padding))
        .on_click(move |_: &ClickEvent, window, cx| select(BlockAction::Select, index, window, cx))
        .children(item.header.as_ref().map(|header| {
            render_header(index, header, item.collapsed, padding, palette, on_action)
        }))
        .child(output)
        .into_any_element()
}

fn render_header(
    index: usize,
    header: &Header,
    collapsed: bool,
    padding: Pixels,
    palette: Palette,
    on_action: OnBlockAction,
) -> AnyElement {
    let chips = chips(&header.context, None, palette);
    let button = |name: &'static str, action: BlockAction| {
        let on_action = on_action.clone();
        div()
            .id((name, index))
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded_sm()
            .hover(|this| this.bg(palette.hover))
            .child(icon(name, px(12.), palette.muted))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                on_action(action, index, window, cx);
            })
    };
    let chevron = if collapsed {
        "chevron-right"
    } else {
        "chevron-down"
    };
    // Actions show on hover so they don't crowd every header.
    let actions = div()
        .flex()
        .flex_none()
        .items_center()
        .gap_0p5()
        .invisible()
        .group_hover("block", |this| this.visible())
        .child(button("terminal", BlockAction::CopyCommand))
        .when(!header.running, |this| {
            this.child(button("copy", BlockAction::CopyOutput))
                .child(button("rotate-ccw", BlockAction::Rerun))
        });
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
        .pl(padding - px(20.))
        .pr(padding)
        .text_xs()
        .font_family(theme::UI_FONT_FAMILY)
        .child(button(chevron, BlockAction::ToggleCollapsed))
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
        .child(actions)
        .children(outcome)
        .into_any_element()
}

/// Chips for a command's context: directory, git branch, and Python
/// environments.
pub fn context_chips(
    context: &BlockContext,
    branch: Option<&str>,
    theme: &Theme,
) -> Vec<AnyElement> {
    chips(context, branch, Palette::new(theme))
}

fn chips(context: &BlockContext, branch: Option<&str>, palette: Palette) -> Vec<AnyElement> {
    let mut chips = Vec::new();
    if let Some(cwd) = &context.cwd {
        chips.push(chip("folder", shorten_home(cwd), palette).into_any_element());
    }
    if let Some(branch) = branch {
        chips.push(chip("git-branch", branch.to_string(), palette).into_any_element());
    }
    if let Some(venv) = &context.virtualenv {
        chips.push(chip("package", venv.clone(), palette).into_any_element());
    }
    if let Some(conda) = &context.conda_env {
        chips.push(chip("flask-conical", conda.clone(), palette).into_any_element());
    }
    chips
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
