//! Painting a pane's command blocks as a scrolling list. Each block shows
//! a context line (environments, directory, branch, duration) and its
//! command above its output, separated from the next block by a hairline;
//! failed blocks are tinted red and selected ones take the accent color.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, UNIX_EPOCH},
};

use chrono::{DateTime, Local};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, FontWeight, Hsla, ListOffset, ListState, Pixels, Point,
    SharedString, Window, canvas, div, fill, list, point, prelude::*, px, size,
};
use libghostty_vt::style::RgbColor;

use crate::{
    blocks::{Block, BlockPoint, BlockSelection, Rows},
    components::icon,
    git::DiffStats,
    grid::{CellMetrics, Frame, paint_rows},
    process_info::shorten_home,
    runtime_protocol::BlockContext,
    theme::{self, Theme},
    tooltip::text_tooltip,
};

/// Space between a block's content and the pane's sides, on top of the
/// terminal padding. The pane's grid is narrowed to match.
pub const HORIZONTAL_INSET: Pixels = px(16.);

/// Vertical spacing inside a block, in line heights, so it scales with the
/// font: above the context line, between it and the command, between the
/// command and the output, and below the output.
const TOP_LINES: f32 = 1.1;
const CONTEXT_TO_COMMAND_LINES: f32 = 0.19;
const COMMAND_TO_OUTPUT_LINES: f32 = 0.5;
const BOTTOM_LINES: f32 = 1.0;
/// The context line's text, relative to the terminal font.
const CONTEXT_SCALE: f32 = 0.9;
const FAILURE_STRIPE: f32 = 5.;
const SELECTION_BORDER: f32 = 2.;
const TOOLBELT_BUTTON: f32 = 26.;

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
    /// Whether a hairline divides the block from what is above it.
    divider: bool,
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
    /// Scroll so the block's header is at the top.
    ScrollToTop,
}

/// Handles a block action for the block at an index.
pub type OnBlockAction = Rc<dyn Fn(BlockAction, usize, &mut Window, &mut App)>;

struct Header {
    command: SharedString,
    context: BlockContext,
    /// How long the command ran, or has been running.
    duration: Duration,
    started_at_ms: u64,
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
                duration: block
                    .outcome
                    .map_or_else(|| block.started.elapsed(), |outcome| outcome.duration),
                started_at_ms: block.started_at_ms,
                failed: block.failed(),
                running: block.is_running(),
            }),
            content,
            collapsed: block.collapsed,
            selected,
            selection: None,
            divider: true,
        }
    }

    pub fn with_divider(mut self, divider: bool) -> Self {
        self.divider = divider;
        self
    }

    pub fn with_selection(mut self, selection: Option<BlockSelection>) -> Self {
        self.selection = selection;
        self
    }

    fn failed(&self) -> bool {
        self.header.as_ref().is_some_and(|header| header.failed)
    }
}

/// Colors the list needs, derived from the terminal theme and copied out
/// so items can be drawn from a `'static` closure.
#[derive(Clone, Copy)]
struct Palette {
    background: Hsla,
    text: Hsla,
    /// Secondary text: the foreground at 60%.
    muted: Hsla,
    /// Hairlines: the foreground at 10%.
    outline: Hsla,
    /// The background raised by 5% and 10% of the foreground.
    surface: Hsla,
    surface_raised: Hsla,
    accent: Hsla,
    error: Hsla,
    selection: Hsla,
}

/// `base` moved `amount` of the way toward `toward`.
fn mix(base: RgbColor, toward: RgbColor, amount: f32) -> Hsla {
    let channel =
        |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
    theme::to_hsla(RgbColor {
        r: channel(base.r, toward.r),
        g: channel(base.g, toward.g),
        b: channel(base.b, toward.b),
    })
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let terminal = &theme.terminal;
        let foreground = theme::to_hsla(terminal.foreground);
        Self {
            background: theme::to_hsla(terminal.background),
            text: foreground,
            muted: foreground.opacity(0.6),
            outline: foreground.opacity(0.1),
            surface: mix(terminal.background, terminal.foreground, 0.05),
            surface_raised: mix(terminal.background, terminal.foreground, 0.1),
            accent: theme.text_accent,
            error: theme::to_hsla(terminal.ansi[1]),
            selection: theme.text_accent.opacity(0.3),
        }
    }
}

/// The block list. `live` is the frame of the running command's terminal,
/// painted under its scrollback.
#[tracing::instrument(skip_all)]
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
    let sticky = render_sticky_header(state, &items, metrics, palette, on_action.clone());
    let list = list(state.clone(), move |index, _window, _cx| {
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
    .size_full();
    div()
        .relative()
        .size_full()
        .child(list)
        .children(sticky)
        .into_any_element()
}

/// The header of the block whose output is scrolled under the top of the
/// list, pinned there; clicking it scrolls back to the block's start.
#[tracing::instrument(skip_all)]
fn render_sticky_header(
    state: &ListState,
    items: &[Item],
    metrics: CellMetrics,
    palette: Palette,
    on_action: OnBlockAction,
) -> Option<AnyElement> {
    let top = state.logical_scroll_top();
    let item = items.get(top.item_ix)?;
    let header = item.header.as_ref()?;
    // Only once the block's own header has scrolled out of sight.
    if item.collapsed || top.offset_in_item <= header_height(metrics) {
        return None;
    }
    let index = top.item_ix;
    Some(
        div()
            .id(("sticky-header", index))
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .pb(metrics.height * COMMAND_TO_OUTPUT_LINES)
            .pt(metrics.height * 0.4)
            .bg(palette.background)
            .border_b_1()
            .border_color(palette.outline)
            .cursor_pointer()
            .hover(move |this| this.bg(palette.surface))
            .on_click(move |_: &ClickEvent, window, cx| {
                on_action(BlockAction::ScrollToTop, index, window, cx)
            })
            .child(render_header_lines(index, header, metrics, palette))
            .into_any_element(),
    )
}

/// Height of a block's top padding and header lines.
fn header_height(metrics: CellMetrics) -> Pixels {
    metrics.height * (TOP_LINES + CONTEXT_SCALE + CONTEXT_TO_COMMAND_LINES + 1.)
}

#[tracing::instrument(skip_all)]
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

    let failed = item.failed();
    let select = on_action.clone();
    let has_output = row_count > 0;
    div()
        .id(("block", index))
        .group("block")
        .relative()
        .flex()
        .flex_col()
        .w_full()
        .when(item.divider, |this| {
            this.border_t_1().border_color(palette.outline)
        })
        .when(failed, |this| this.bg(palette.error.opacity(0.1)))
        .when(item.selected, |this| this.bg(palette.accent.opacity(0.25)))
        .pb(metrics.height * BOTTOM_LINES)
        .when(item.header.is_none(), |this| this.pt(metrics.height * 0.5))
        .on_click(move |_: &ClickEvent, window, cx| select(BlockAction::Select, index, window, cx))
        .children(item.header.as_ref().map(|header| {
            div()
                .pt(metrics.height * TOP_LINES)
                .when(has_output, |this| {
                    this.pb(metrics.height * COMMAND_TO_OUTPUT_LINES)
                })
                .child(render_header_lines(index, header, metrics, palette))
        }))
        .child(output)
        // The stripe and the selection border sit over the block, so they
        // never change its size.
        .when(failed && !item.selected, |this| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(FAILURE_STRIPE))
                    .bg(palette.error),
            )
        })
        .when(item.selected, |this| {
            this.child(
                div()
                    .absolute()
                    .inset_0()
                    .border(px(SELECTION_BORDER))
                    .border_color(palette.accent),
            )
        })
        .children(item.header.as_ref().map(|header| {
            render_toolbelt(index, header, item.collapsed, metrics, palette, on_action)
        }))
        .into_any_element()
}

/// The context line and the command, as a block shows them at its top.
fn render_header_lines(
    index: usize,
    header: &Header,
    metrics: CellMetrics,
    palette: Palette,
) -> AnyElement {
    let padding = metrics.padding + HORIZONTAL_INSET;
    let context = context_text(&header.context);
    let duration = format!("({})", format_duration(header.duration));
    let started = header.started_at_ms;
    let finished =
        (!header.running).then(|| header.started_at_ms + header.duration.as_millis() as u64);
    div()
        .flex()
        .flex_col()
        .px(padding)
        .font_family(theme::FONT_FAMILY)
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_x(metrics.width)
                .text_size(metrics.font_size * CONTEXT_SCALE)
                .line_height(metrics.height * CONTEXT_SCALE)
                .text_color(palette.muted)
                .when(!context.is_empty(), |this| this.child(context))
                .child(
                    div()
                        .id(("duration", index))
                        .tooltip(text_tooltip(time_lines(started, finished)))
                        .child(duration),
                ),
        )
        .child(
            div()
                .pt(metrics.height * CONTEXT_TO_COMMAND_LINES)
                .text_size(metrics.font_size)
                .line_height(metrics.height)
                .text_color(palette.text)
                .child(header.command.clone()),
        )
        .into_any_element()
}

/// The block's actions, shown on hover at its top right.
fn render_toolbelt(
    index: usize,
    header: &Header,
    collapsed: bool,
    metrics: CellMetrics,
    palette: Palette,
    on_action: OnBlockAction,
) -> AnyElement {
    let button = |name: &'static str, label: &'static str, action: BlockAction| {
        let on_action = on_action.clone();
        div()
            .id((name, index))
            .flex()
            .items_center()
            .justify_center()
            .size(px(TOOLBELT_BUTTON))
            .rounded(px(5.))
            .hover(move |this| this.bg(palette.surface_raised))
            .tooltip(text_tooltip(vec![label.into()]))
            .child(icon(name, px(14.), palette.muted))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                on_action(action, index, window, cx);
            })
    };
    let (chevron, collapse_label) = if collapsed {
        ("chevron-right", "Expand")
    } else {
        ("chevron-down", "Collapse")
    };
    div()
        .absolute()
        .top(metrics.height * TOP_LINES * 0.5)
        .right(metrics.padding + HORIZONTAL_INSET)
        .flex()
        .items_center()
        .gap(px(4.))
        .p(px(4.))
        .rounded(px(4.))
        .bg(palette.surface)
        .invisible()
        .group_hover("block", |this| this.visible())
        .child(button("terminal", "Copy command", BlockAction::CopyCommand))
        .when(!header.running, |this| {
            this.child(button("copy", "Copy output", BlockAction::CopyOutput))
                .child(button("rotate-ccw", "Run again", BlockAction::Rerun))
        })
        .child(button(
            chevron,
            collapse_label,
            BlockAction::ToggleCollapsed,
        ))
        .into_any_element()
}

/// The context line's words: `(conda) (venv) ~/path git:(branch)`.
fn context_text(context: &BlockContext) -> String {
    let mut words = Vec::new();
    if let Some(conda) = &context.conda_env {
        words.push(format!("({conda})"));
    }
    if let Some(venv) = &context.virtualenv {
        words.push(format!("({venv})"));
    }
    if let Some(cwd) = &context.cwd {
        words.push(shorten_home(cwd));
    }
    if let Some(branch) = &context.git_branch {
        words.push(format!("git:({branch})"));
    }
    words.join(" ")
}

/// Tooltip lines for when a command started and, if it has, finished.
fn time_lines(started_ms: u64, finished_ms: Option<u64>) -> Vec<SharedString> {
    let at = |ms: u64| {
        let time: DateTime<Local> = (UNIX_EPOCH + Duration::from_millis(ms)).into();
        time.format("%a %b %-d at %-I:%M:%S %p").to_string()
    };
    let mut lines = vec![format!("Started at: {}", at(started_ms)).into()];
    if let Some(finished) = finished_ms {
        lines.push(format!("Completed at: {}", at(finished)).into());
    }
    lines
}

/// Chips for the prompt's context above the command editor: environments,
/// directory, git branch, and uncommitted changes.
pub fn context_chips(
    context: &BlockContext,
    diff: Option<DiffStats>,
    metrics: CellMetrics,
    theme: &Theme,
) -> Vec<AnyElement> {
    let palette = Palette::new(theme);
    let ansi = |index: usize| theme::to_hsla(theme.terminal.ansi[index]);
    // Warp's chip: text one point under the terminal's, at its line height
    // ratio, in a box bordered by the background mixed 15% toward the
    // foreground.
    let font_size = metrics.font_size - px(1.);
    let line_height = font_size * (metrics.height / metrics.font_size);
    let chip_border = mix(theme.terminal.background, theme.terminal.foreground, 0.15);
    let chip = |icon_name: &'static str, color: Hsla, label: AnyElement| {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .px(px(4.))
            .py(px(2.))
            .rounded(px(4.))
            .border_1()
            .border_color(chip_border)
            .bg(palette.surface)
            .text_color(color)
            .child(icon(icon_name, font_size, color))
            .child(label)
            .into_any_element()
    };
    let text = |label: String| div().child(label).into_any_element();
    let mut chips = Vec::new();
    if let Some(conda) = &context.conda_env {
        chips.push(chip("terminal", ansi(3), text(conda.clone())));
    }
    if let Some(venv) = &context.virtualenv {
        chips.push(chip("terminal", ansi(3), text(venv.clone())));
    }
    if let Some(cwd) = &context.cwd {
        chips.push(chip("folder", ansi(5), text(shorten_home(cwd))));
    }
    if let Some(branch) = &context.git_branch {
        chips.push(chip("git-branch", ansi(3), text(branch.clone())));
    }
    if let Some(diff) = diff.filter(|diff| diff.files > 0) {
        let label = div()
            .flex()
            .gap(px(4.))
            .child(format!("{} •", diff.files))
            .child(
                div()
                    .text_color(ansi(2))
                    .child(format!("+{}", diff.insertions)),
            )
            .child(
                div()
                    .text_color(ansi(1))
                    .child(format!("-{}", diff.deletions)),
            )
            .into_any_element();
        chips.push(chip("file-pen-line", ansi(3), label));
    }
    chips
        .into_iter()
        .map(|chip| {
            div()
                .font_family(theme::FONT_FAMILY)
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(font_size)
                .line_height(line_height)
                .child(chip)
                .into_any_element()
        })
        .collect()
}

/// How long a command ran: `16.471s`, `1m 8.92s`, `1h 2m 3s`.
fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{:.3}s", duration.as_secs_f64())
    } else if seconds < 3600 {
        let rest = duration.as_secs_f64() - (seconds / 60 * 60) as f64;
        format!("{}m {:.2}s", seconds / 60, rest)
    } else {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    }
}

/// Lets a list scroll back to the top of a block.
pub fn block_start(index: usize) -> ListOffset {
    ListOffset {
        item_ix: index,
        offset_in_item: px(0.),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn durations_read_like_warp() {
        assert_eq!(format_duration(Duration::from_millis(16_471)), "16.471s");
        assert_eq!(format_duration(Duration::from_millis(68_920)), "1m 8.92s");
        assert_eq!(format_duration(Duration::from_secs(3723)), "1h 2m 3s");
    }

    #[test]
    fn context_lines_list_environments_directory_and_branch() {
        let context = BlockContext {
            cwd: Some(PathBuf::from("/tmp/project")),
            git_branch: Some("main".into()),
            virtualenv: Some("venv".into()),
            conda_env: None,
        };
        assert_eq!(context_text(&context), "(venv) /tmp/project git:(main)");
        assert_eq!(context_text(&BlockContext::default()), "");
    }
}
