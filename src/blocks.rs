//! A view's copy of its session's command blocks: what the runtime reports
//! about each block, with its rows decoded for painting.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
use libghostty_vt::{Terminal, terminal::Options};

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

    pub fn failed(&self) -> bool {
        self.outcome.is_some_and(|outcome| outcome.exit_code != 0)
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
    decoder: Option<GridRenderer>,
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

    /// Take rows the runtime sent for a block, `cols` wide. Returns the
    /// block's index when it changed.
    pub fn apply_rows(
        &mut self,
        id: u64,
        version: u64,
        from: usize,
        rows: &[String],
        cols: u16,
    ) -> Result<Option<usize>> {
        let Some(index) = self.blocks.iter().position(|block| block.id == id) else {
            return Ok(None);
        };
        let decoder = match &mut self.decoder {
            Some(decoder) => decoder,
            None => self.decoder.insert(GridRenderer::new()?),
        };
        let decoded = decode(decoder, rows, cols)?;
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
fn decode(decoder: &mut GridRenderer, rows: &[String], cols: u16) -> Result<Vec<Rc<FrameRow>>> {
    let mut decoded = Vec::with_capacity(rows.len());
    for page in rows.chunks(DECODE_PAGE_ROWS) {
        let mut terminal = Terminal::new(Options {
            cols: cols.max(1),
            rows: page.len() as u16,
            max_scrollback: 0,
        })?;
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
            running,
            rows,
        }
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
        assert_eq!(list.apply_rows(7, 1, 0, &rows, 20).unwrap(), Some(0));
        assert_eq!(list.blocks()[0].output_text(), "red text\nsecond");
        list.apply_rows(7, 2, 0, &rows[1..], 20).unwrap();
        assert_eq!(list.blocks()[0].output_text(), "second");
        assert_eq!(list.apply_rows(99, 1, 0, &rows, 20).unwrap(), None);
    }
}
