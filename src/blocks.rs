//! A view's copy of its session's command blocks: what the runtime reports
//! about each block, with its rows decoded for painting.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
use libghostty_vt::{Terminal, style::RgbColor, terminal::Options};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    grid::{FrameRow, GridRenderer},
    runtime_protocol::{BlockContext, BlockSummary},
};

pub type Rows = Rc<Vec<Rc<FrameRow>>>;

/// Rows decoded per scratch terminal.
const DECODE_PAGE_ROWS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub exit_code: i32,
    pub duration: Duration,
}

pub struct Block {
    pub id: u64,
    version: u64,
    pub command: String,
    pub context: BlockContext,
    pub outcome: Option<Outcome>,
    /// When this view first saw the block running.
    pub started: Instant,
    /// When the command started, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// Whether only the header shows.
    pub collapsed: bool,
    rows: Vec<Rc<FrameRow>>,
    /// `rows` shared with the painter, rebuilt when rows arrive.
    shared: Rows,
}

impl Block {
    fn from_summary(summary: &BlockSummary) -> Self {
        let mut block = Self {
            id: summary.id,
            version: summary.version,
            command: String::new(),
            context: BlockContext::default(),
            outcome: None,
            started: Instant::now(),
            started_at_ms: summary.started_at_ms,
            collapsed: false,
            rows: Vec::new(),
            shared: Rows::default(),
        };
        block.update(summary);
        block
    }

    fn update(&mut self, summary: &BlockSummary) {
        self.command.clone_from(&summary.command);
        self.context.clone_from(&summary.context);
        self.outcome = summary
            .exit_code
            .filter(|_| !summary.running)
            .map(|exit_code| Outcome {
                exit_code,
                duration: Duration::from_millis(summary.duration_ms.unwrap_or(0)),
            });
    }

    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    /// Whether the command failed. Exiting on Ctrl-C (130) or a closed
    /// pipe (141) is how commands are stopped, not a failure.
    pub fn failed(&self) -> bool {
        self.outcome
            .is_some_and(|outcome| !matches!(outcome.exit_code, 0 | 130 | 141))
    }

    /// The block's rows: all of them once finished; while running, those
    /// that scrolled off its terminal, whose live screen is drawn below.
    pub fn rows(&self) -> Rows {
        Rc::clone(&self.shared)
    }

    /// The block's output as plain text, one line per row.
    pub fn output_text(&self) -> String {
        self.rows
            .iter()
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A cell in the block list: a block, a row of its output (scrollback and
/// live screen together for a running block), and a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockPoint {
    pub block: usize,
    pub row: usize,
    pub col: usize,
}

/// Text selected across blocks, from where the drag started to where the
/// pointer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSelection {
    pub anchor: BlockPoint,
    pub head: BlockPoint,
}

impl BlockSelection {
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Start and end in order; the end column is exclusive.
    pub fn range(&self) -> (BlockPoint, BlockPoint) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// The selected columns of `row` in `block`, end exclusive, if any.
    pub fn columns_in(&self, block: usize, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.range();
        let here = (block, row);
        if here < (start.block, start.row) || here > (end.block, end.row) {
            return None;
        }
        let from = if here == (start.block, start.row) {
            start.col
        } else {
            0
        };
        let to = if here == (end.block, end.row) {
            end.col
        } else {
            usize::MAX
        };
        (from < to).then_some((from, to))
    }
}

/// The part of `text` covering grid columns `start..end`, counting wide
/// characters as two columns.
pub fn slice_columns(text: &str, start: usize, end: usize) -> &str {
    let mut col = 0;
    let mut from = text.len();
    let mut to = text.len();
    for (index, grapheme) in text.grapheme_indices(true) {
        if col >= start && from == text.len() {
            from = index;
        }
        if col >= end {
            to = index;
            break;
        }
        col += grapheme.width().max(1);
    }
    if from > to { "" } else { &text[from..to] }
}

/// Columns of the word around `col` in `text`: a run of non-whitespace, or
/// the single column when there is none.
pub fn word_columns(text: &str, col: usize) -> (usize, usize) {
    let cells: Vec<(usize, usize, bool)> = text
        .graphemes(true)
        .scan(0, |position, grapheme| {
            let width = grapheme.width().max(1);
            let start = *position;
            *position += width;
            Some((start, start + width, grapheme.trim().is_empty()))
        })
        .collect();
    let Some(hit) = cells
        .iter()
        .position(|(start, end, _)| (*start..*end).contains(&col))
    else {
        // Past the end of the row (a click beyond its text).
        return (col, col.saturating_add(1));
    };
    if cells[hit].2 {
        return (cells[hit].0, cells[hit].1);
    }
    let first = cells[..hit]
        .iter()
        .rposition(|cell| cell.2)
        .map_or(0, |index| index + 1);
    let last = cells[hit..]
        .iter()
        .position(|cell| cell.2)
        .map_or(cells.len(), |index| hit + index);
    (cells[first].0, cells[last - 1].1)
}

/// How the list of blocks changed in a sync, for keeping a list view's
/// items in step.
#[derive(Debug, PartialEq, Eq)]
pub enum ListChange {
    Unchanged,
    /// The oldest `dropped` blocks went away and `added` new ones came.
    Shifted {
        dropped: usize,
        added: usize,
    },
    /// Anything else; the list starts over.
    Reset,
}

#[derive(Default)]
pub struct BlockList {
    blocks: Vec<Block>,
}

impl BlockList {
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn blocks_mut(&mut self) -> &mut [Block] {
        &mut self.blocks
    }

    pub fn running(&self) -> Option<&Block> {
        self.blocks.last().filter(|block| block.is_running())
    }

    /// Bring the list in line with the runtime's summaries.
    pub fn sync(&mut self, summaries: &[BlockSummary]) -> ListChange {
        let old: Vec<u64> = self.blocks.iter().map(|block| block.id).collect();
        let new: Vec<u64> = summaries.iter().map(|summary| summary.id).collect();
        let mut kept: Vec<Block> = Vec::with_capacity(summaries.len());
        let mut previous = std::mem::take(&mut self.blocks).into_iter().peekable();
        for summary in summaries {
            while previous
                .next_if(|block| block.id != summary.id && !new.contains(&block.id))
                .is_some()
            {}
            let block = match previous.next_if(|block| block.id == summary.id) {
                Some(mut block) => {
                    block.update(summary);
                    block
                }
                None => Block::from_summary(summary),
            };
            kept.push(block);
        }
        self.blocks = kept;
        if old == new {
            return ListChange::Unchanged;
        }
        let dropped = old.iter().take_while(|id| !new.contains(id)).count();
        let kept_old = &old[dropped..];
        if new.starts_with(kept_old) {
            ListChange::Shifted {
                dropped,
                added: new.len() - kept_old.len(),
            }
        } else {
            ListChange::Reset
        }
    }

    /// Take rows the runtime sent for a block, `cols` wide, painted over
    /// `background`. Returns the block's index when it changed.
    pub fn apply_rows(
        &mut self,
        id: u64,
        version: u64,
        from: usize,
        rows: &[String],
        cols: u16,
        background: [u8; 3],
    ) -> Result<Option<usize>> {
        let Some(index) = self.blocks.iter().position(|block| block.id == id) else {
            return Ok(None);
        };
        let decoded = decode(rows, cols, background)?;
        let block = &mut self.blocks[index];
        if block.version != version {
            block.version = version;
            block.rows.clear();
        }
        block.rows.truncate(from);
        if block.rows.len() < from {
            // Rows before `from` are missing; wait for the next full send.
            return Ok(None);
        }
        block.rows.extend(decoded);
        block.shared = Rc::new(block.rows.clone());
        Ok(Some(index))
    }
}

/// Paint encoded rows into scratch terminals and read them back as rows.
fn decode(rows: &[String], cols: u16, background: [u8; 3]) -> Result<Vec<Rc<FrameRow>>> {
    let mut decoded = Vec::with_capacity(rows.len());
    for page in rows.chunks(DECODE_PAGE_ROWS) {
        // A renderer copies only rows marked dirty, and blank rows of a
        // fresh terminal are not, so each page gets a renderer of its own.
        let mut decoder = GridRenderer::new()?;
        let mut terminal = Terminal::new(Options {
            cols: cols.max(1),
            rows: page.len() as u16,
            max_scrollback: 0,
        })?;
        // Cells in the session's own background are then left unpainted.
        // The renderer takes a default background only alongside a default
        // foreground; rows carry their own foreground colors, so any does.
        let [r, g, b] = background;
        terminal
            .set_default_fg_color(Some(RgbColor {
                r: 255 - r,
                g: 255 - g,
                b: 255 - b,
            }))?
            .set_default_bg_color(Some(RgbColor { r, g, b }))?;
        // Rows are exactly one line each; wrapping would shift the rest.
        terminal.vt_write(b"\x1b[?7l");
        for (index, row) in page.iter().enumerate() {
            terminal.vt_write(format!("\x1b[{};1H", index + 1).as_bytes());
            terminal.vt_write(row.as_bytes());
        }
        decoded.extend(decoder.viewport_rows(&terminal)?.iter().cloned());
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: u64, command: &str, rows: usize, running: bool) -> BlockSummary {
        BlockSummary {
            id,
            version: 1,
            command: command.into(),
            context: BlockContext::default(),
            exit_code: (!running).then_some(0),
            duration_ms: (!running).then_some(5),
            started_at_ms: 0,
            running,
            rows,
        }
    }

    #[test]
    fn selections_cover_rows_across_blocks() {
        let selection = BlockSelection {
            anchor: BlockPoint {
                block: 1,
                row: 2,
                col: 4,
            },
            head: BlockPoint {
                block: 0,
                row: 1,
                col: 3,
            },
        };
        assert_eq!(selection.columns_in(0, 0), None);
        assert_eq!(selection.columns_in(0, 1), Some((3, usize::MAX)));
        assert_eq!(selection.columns_in(1, 0), Some((0, usize::MAX)));
        assert_eq!(selection.columns_in(1, 2), Some((0, 4)));
        assert_eq!(selection.columns_in(1, 3), None);
    }

    #[test]
    fn columns_map_to_text_with_wide_characters() {
        assert_eq!(slice_columns("hello world", 6, usize::MAX), "world");
        assert_eq!(slice_columns("日本語 ok", 2, 6), "本語");
        assert_eq!(slice_columns("short", 10, 20), "");
        assert_eq!(word_columns("git commit --amend", 6), (4, 10));
        assert_eq!(word_columns("a  b", 1), (1, 2));
        assert_eq!(word_columns("ab", usize::MAX), (usize::MAX, usize::MAX));
    }

    #[test]
    fn cells_in_the_session_background_are_left_unpainted() {
        let rows = vec!["\x1b[0;38;2;0;0;0;48;2;250;250;250m   ".to_string()];
        assert!(!decode(&rows, 10, [250, 250, 250]).unwrap()[0].paints_background());
        assert!(decode(&rows, 10, [0, 0, 0]).unwrap()[0].paints_background());
    }

    #[test]
    fn syncing_reports_how_the_list_moved() {
        let mut list = BlockList::default();
        assert_eq!(
            list.sync(&[summary(1, "a", 0, false), summary(2, "b", 0, true)]),
            ListChange::Shifted {
                dropped: 0,
                added: 2
            }
        );
        assert!(list.running().is_some());
        assert_eq!(
            list.sync(&[summary(1, "a", 0, false), summary(2, "b", 0, false)]),
            ListChange::Unchanged
        );
        assert!(list.running().is_none());
        assert_eq!(
            list.sync(&[summary(2, "b", 0, false), summary(3, "c", 0, true)]),
            ListChange::Shifted {
                dropped: 1,
                added: 1
            }
        );
        assert_eq!(list.blocks()[0].command, "b");
    }

    #[test]
    fn rows_decode_and_replace_on_a_new_version() {
        let mut list = BlockList::default();
        list.sync(&[summary(7, "ls", 2, false)]);
        let rows = vec![
            "\x1b[0m\x1b]8;;\x1b\\\x1b[0;38;2;200;0;0;48;2;0;0;0mred \x1b[0mtext".to_string(),
            "second".to_string(),
        ];
        assert_eq!(
            list.apply_rows(7, 1, 0, &rows, 20, [0, 0, 0]).unwrap(),
            Some(0)
        );
        assert_eq!(list.blocks()[0].output_text(), "red text\nsecond");
        list.apply_rows(7, 2, 0, &rows[1..], 20, [0, 0, 0]).unwrap();
        assert_eq!(list.blocks()[0].output_text(), "second");
        assert_eq!(
            list.apply_rows(99, 1, 0, &rows, 20, [0, 0, 0]).unwrap(),
            None
        );
    }
}
