//! Command blocks as the session runtime keeps them: one block per command,
//! each with its own terminal while it runs and its rows encoded as styled
//! VT text once it finishes, the same form snapshots use.
//!
//! Each block also keeps its raw output, within a budget, so its rows can
//! be rebuilt when the session's width or colors change.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use libghostty_vt::{
    Terminal,
    render::{CellIterator, Dirty, RenderState, RowIterator},
    terminal::{Mode, ScrollViewport},
};

use crate::{
    hooks::Precmd,
    process_info,
    runtime_engine::encode_row,
    runtime_protocol::{BlockContext, BlockSummary},
};

/// Most raw output kept across all blocks for rebuilding their rows. Past
/// this the oldest blocks give theirs up and keep their rows as captured.
const OUTPUT_BUDGET: usize = 64 * 1024 * 1024;

/// Most raw output replayed to rebuild one block; a larger block keeps the
/// rows it was captured with, so a rebuild's cost stays bounded.
const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;

/// Most encoded bytes sent in one reply for a block's rows; the rest
/// follows in later replies.
const ROWS_REPLY_BYTES: usize = 2 * 1024 * 1024;

impl From<&Precmd> for BlockContext {
    fn from(precmd: &Precmd) -> Self {
        Self {
            cwd: precmd.cwd.clone(),
            // Read from the repository's files, never by running git.
            git_branch: precmd.cwd.as_deref().and_then(process_info::git_branch),
            virtualenv: precmd.virtualenv.clone(),
            conda_env: precmd.conda_env.clone(),
        }
    }
}

/// Encodes rows of any terminal, including its scrollback.
pub struct RowEncoder {
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    graphemes: String,
    links: Vec<u8>,
}

impl RowEncoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            graphemes: String::new(),
            links: vec![0; 2048],
        })
    }

    /// Rows `from..to` of everything `terminal` holds, scrollback first,
    /// leaving its viewport at the bottom.
    pub fn encode(
        &mut self,
        terminal: &mut Terminal<'static, 'static>,
        from: usize,
        to: usize,
    ) -> Result<Vec<String>> {
        let screen = terminal.rows()? as usize;
        let total = terminal.total_rows()?;
        let to = to.min(total);
        let mut encoded = Vec::with_capacity(to.saturating_sub(from));
        // A render state copies only rows the terminal marks dirty, and the
        // session's own renderer clears those marks as it draws, so each
        // encoding starts from a render state that has seen nothing.
        let mut render = RenderState::new()?;
        let mut next = from;
        while next < to {
            let top = next.min(total.saturating_sub(screen));
            terminal.scroll_viewport(ScrollViewport::Row(top));
            let palette = terminal.color_palette()?;
            let frame = render.update(terminal)?;
            let colors = frame.colors()?;
            frame.set_dirty(Dirty::Full)?;
            let mut rows = self.rows.update(&frame)?;
            let mut index = 0u32;
            while let Some(row) = rows.next() {
                let absolute = top + index as usize;
                if (next..to).contains(&absolute) {
                    encoded.push(encode_row(
                        row,
                        &mut self.cells,
                        &colors,
                        &palette,
                        terminal,
                        index,
                        &mut self.graphemes,
                        &mut self.links,
                    )?);
                }
                index += 1;
            }
            frame.set_dirty(Dirty::Clean)?;
            next = top + screen;
        }
        terminal.scroll_viewport(ScrollViewport::Bottom);
        Ok(encoded)
    }

    /// Every row `terminal` holds, without the blank rows at the end.
    pub fn encode_all(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<Vec<String>> {
        let total = terminal.total_rows()?;
        let mut rows = self.encode(terminal, 0, total)?;
        while rows.last().is_some_and(|row| is_blank(row)) {
            rows.pop();
        }
        Ok(rows)
    }
}

/// The characters of an encoded row, without its escape sequences.
pub fn plain_text(encoded: &str) -> String {
    let mut text = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            text.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameters up to a final byte.
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
            // OSC: up to the string terminator.
            Some(']') => {
                while let Some(ch) = chars.next() {
                    if ch == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    text
}

fn is_blank(encoded: &str) -> bool {
    plain_text(encoded).trim().is_empty()
}

/// Whether a program switched `terminal` to the alternate screen, where it
/// expects the whole pane (vim, htop).
pub fn is_full_screen(terminal: &Terminal<'static, 'static>) -> bool {
    terminal.mode(Mode::ALT_SCREEN_SAVE).unwrap_or(false)
        || terminal.mode(Mode::ALT_SCREEN).unwrap_or(false)
}

struct Block {
    id: u64,
    version: u64,
    command: String,
    context: BlockContext,
    started: Instant,
    started_at: SystemTime,
    outcome: Option<(i32, Duration)>,
    /// All rows once finished; while running, the rows that scrolled off
    /// its terminal.
    rows: Vec<String>,
    /// Everything the command wrote, while it fits the budget.
    raw: Option<Vec<u8>>,
}

impl Block {
    fn is_running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// The blocks of one session, oldest first.
pub struct Blocks {
    blocks: Vec<Block>,
    next_id: u64,
    /// Context from the shell's latest prompt, applied to the next block.
    context: BlockContext,
    /// Most blocks kept; the oldest are dropped beyond this.
    limit: usize,
    encoder: RowEncoder,
}

impl Blocks {
    pub fn new(limit: usize) -> Result<Self> {
        Ok(Self {
            blocks: Vec::new(),
            next_id: 0,
            context: BlockContext::default(),
            limit: limit.max(1),
            encoder: RowEncoder::new()?,
        })
    }

    pub fn set_context(&mut self, precmd: &Precmd) {
        self.context = BlockContext::from(precmd);
    }

    pub fn is_running(&self) -> bool {
        self.blocks.last().is_some_and(Block::is_running)
    }

    fn running_mut(&mut self) -> Option<&mut Block> {
        self.blocks.last_mut().filter(|block| block.is_running())
    }

    fn push(&mut self, mut block: Block) {
        self.next_id += 1;
        block.id = self.next_id;
        self.blocks.push(block);
        if self.blocks.len() > self.limit {
            let excess = self.blocks.len() - self.limit;
            self.blocks.drain(..excess);
        }
    }

    /// Keep what the shell printed while starting up (a greeting, messages
    /// from its config) as a block without a command, if there is any.
    pub fn push_startup(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<()> {
        let rows = self.encoder.encode_all(terminal)?;
        if rows.is_empty() {
            return Ok(());
        }
        self.push(Block {
            id: 0,
            version: 0,
            command: String::new(),
            context: self.context.clone(),
            started: Instant::now(),
            started_at: SystemTime::now(),
            outcome: Some((0, Duration::ZERO)),
            rows,
            raw: None,
        });
        Ok(())
    }

    /// Start a block for `command`, whose output goes to a terminal the
    /// session dedicates to it until it finishes.
    pub fn start(&mut self, command: String) {
        // A block still running when another starts never got its finish
        // hook (the shell was killed or exec'd); close it as unknown.
        if let Some(running) = self.running_mut() {
            running.outcome = Some((-1, running.started.elapsed()));
        }
        self.push(Block {
            id: 0,
            version: 0,
            command,
            context: self.context.clone(),
            started: Instant::now(),
            started_at: SystemTime::now(),
            outcome: None,
            rows: Vec::new(),
            raw: Some(Vec::new()),
        });
    }

    /// Keep `bytes` the running command wrote, for rebuilding its rows.
    pub fn record(&mut self, bytes: &[u8]) {
        let Some(raw) = self.running_mut().and_then(|block| block.raw.as_mut()) else {
            return;
        };
        raw.extend_from_slice(bytes);
        let mut total: usize = self
            .blocks
            .iter()
            .filter_map(|block| block.raw.as_ref())
            .map(Vec::len)
            .sum();
        for block in &mut self.blocks {
            if total <= OUTPUT_BUDGET {
                break;
            }
            if let Some(raw) = block.raw.take() {
                total -= raw.len();
            }
        }
    }

    /// Capture rows of the running command that scrolled off `terminal`
    /// since the last call, so streaming output costs work in proportion to
    /// what is new.
    pub fn capture_scrollback(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<()> {
        let screen = terminal.rows()? as usize;
        let scrollback = terminal.total_rows()?.saturating_sub(screen);
        let Some(captured) = self.running_mut().map(|block| block.rows.len()) else {
            return Ok(());
        };
        if captured >= scrollback {
            return Ok(());
        }
        let rows = self.encoder.encode(terminal, captured, scrollback)?;
        if let Some(block) = self.running_mut() {
            block.rows.extend(rows);
        }
        Ok(())
    }

    /// Close the running block with `exit_code`, capturing all of its
    /// output from `terminal`.
    pub fn finish(
        &mut self,
        exit_code: i32,
        terminal: &mut Terminal<'static, 'static>,
    ) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        let rows = self.encoder.encode_all(terminal)?;
        if let Some(block) = self.running_mut() {
            block.outcome = Some((exit_code, block.started.elapsed()));
            block.rows = rows;
            block.version += 1;
        }
        Ok(())
    }

    /// The running terminal was resized, which reflows its scrollback, so
    /// its captured rows start over.
    pub fn terminal_resized(&mut self) {
        if let Some(block) = self.running_mut() {
            block.rows.clear();
            block.version += 1;
        }
    }

    /// Rebuild finished blocks' rows by replaying their output into
    /// terminals from `new_terminal`, after the width or colors changed.
    /// Those terminals must not be connected to the PTY, since replayed
    /// queries would otherwise be answered again.
    pub fn rebuild_rows(
        &mut self,
        mut new_terminal: impl FnMut() -> Result<Terminal<'static, 'static>>,
    ) -> Result<()> {
        for block in &mut self.blocks {
            let (Some(raw), false) = (&block.raw, block.is_running()) else {
                continue;
            };
            if raw.len() > MAX_REPLAY_BYTES {
                continue;
            }
            let mut terminal = new_terminal()?;
            terminal.vt_write(raw);
            block.rows = self.encoder.encode_all(&mut terminal)?;
            block.version += 1;
        }
        Ok(())
    }

    pub fn summaries(&self) -> Vec<BlockSummary> {
        self.blocks
            .iter()
            .map(|block| BlockSummary {
                id: block.id,
                version: block.version,
                command: block.command.clone(),
                context: block.context.clone(),
                exit_code: block.outcome.map(|(code, _)| code),
                duration_ms: block
                    .outcome
                    .map(|(_, duration)| duration.as_millis() as u64),
                started_at_ms: block
                    .started_at
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis() as u64),
                running: block.is_running(),
                rows: block.rows.len(),
            })
            .collect()
    }

    /// A block's rows from `from` on, as many as fit one reply, with the
    /// block's version.
    pub fn rows(&self, id: u64, from: usize) -> Option<(u64, Vec<String>)> {
        let block = self.blocks.iter().find(|block| block.id == id)?;
        let mut bytes = 0;
        let rows = block
            .rows
            .iter()
            .skip(from)
            .take_while(|row| {
                bytes += row.len();
                bytes <= ROWS_REPLY_BYTES
            })
            .cloned()
            .collect();
        Some((block.version, rows))
    }

    /// The newest `lines` non-empty lines of the blocks as plain text:
    /// each command after a `$`, then its output. Only the lines kept are
    /// read, newest first. A running command's live screen is not included.
    pub fn text(&self, lines: usize) -> String {
        let mut kept: Vec<String> = Vec::new();
        'blocks: for block in self.blocks.iter().rev() {
            for row in block.rows.iter().rev() {
                if kept.len() >= lines {
                    break 'blocks;
                }
                let line = plain_text(row).trim_end().to_string();
                if !line.is_empty() {
                    kept.push(line);
                }
            }
            if !block.command.is_empty() {
                if kept.len() >= lines {
                    break;
                }
                kept.push(format!("$ {}", block.command));
            }
        }
        let mut text: String = kept.into_iter().rev().map(|line| line + "\n").collect();
        text.shrink_to_fit();
        text
    }
}

#[cfg(test)]
mod tests {
    use libghostty_vt::terminal::Options;

    use super::*;

    fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
        Terminal::new(Options {
            cols,
            rows,
            max_scrollback: 1000,
        })
        .unwrap()
    }

    fn texts(blocks: &Blocks, id: u64) -> Vec<String> {
        blocks
            .rows(id, 0)
            .unwrap()
            .1
            .iter()
            .map(|row| plain_text(row).trim_end().to_string())
            .collect()
    }

    #[test]
    fn blocks_capture_all_output_including_scrollback() {
        let mut blocks = Blocks::new(10).unwrap();
        let mut output = terminal(20, 5);
        blocks.start("seq 1 30".into());
        let bytes: String = (1..=30).map(|n| format!("{n}\r\n")).collect();
        blocks.record(bytes.as_bytes());
        output.vt_write(bytes.as_bytes());

        // While running, rows that scrolled off are captured incrementally.
        blocks.capture_scrollback(&mut output).unwrap();
        let running = blocks.summaries()[0].rows;
        assert_eq!(running, 26);
        output.vt_write(b"31\r\n");
        blocks.capture_scrollback(&mut output).unwrap();
        assert_eq!(blocks.summaries()[0].rows, 27);

        blocks.finish(0, &mut output).unwrap();
        let summary = &blocks.summaries()[0];
        assert!(!summary.running);
        assert_eq!(summary.exit_code, Some(0));
        let rows = texts(&blocks, summary.id);
        assert_eq!(rows.len(), 31);
        assert_eq!(rows[0], "1");
        assert_eq!(rows[30], "31");
        assert!(blocks.text(100).starts_with("$ seq 1 30\n1\n2\n"));
        assert_eq!(blocks.text(2), "30\n31\n");
    }

    #[test]
    fn finished_blocks_rewrap_at_a_new_width() {
        let mut blocks = Blocks::new(10).unwrap();
        let mut output = terminal(20, 5);
        blocks.start("echo".into());
        let line = "x".repeat(30);
        blocks.record(line.as_bytes());
        output.vt_write(line.as_bytes());
        blocks.finish(0, &mut output).unwrap();
        let id = blocks.summaries()[0].id;
        assert_eq!(texts(&blocks, id).len(), 2);
        blocks.rebuild_rows(|| Ok(terminal(10, 5))).unwrap();
        assert_eq!(texts(&blocks, id).len(), 3);
        assert_eq!(blocks.summaries()[0].version, 2);
    }

    #[test]
    fn a_new_block_closes_an_unfinished_one_and_the_list_is_capped() {
        let mut blocks = Blocks::new(2).unwrap();
        blocks.start("a".into());
        blocks.start("b".into());
        assert_eq!(blocks.summaries()[0].exit_code, Some(-1));
        blocks.finish(1, &mut terminal(10, 3)).unwrap();
        blocks.start("c".into());
        let commands: Vec<String> = blocks
            .summaries()
            .into_iter()
            .map(|summary| summary.command)
            .collect();
        assert_eq!(commands, vec!["b", "c"]);
    }

    #[test]
    fn startup_output_becomes_a_block_only_when_there_is_some() {
        let mut blocks = Blocks::new(10).unwrap();
        blocks.push_startup(&mut terminal(10, 3)).unwrap();
        assert!(blocks.summaries().is_empty());
        let mut startup = terminal(10, 3);
        startup.vt_write(b"welcome\r\n");
        blocks.push_startup(&mut startup).unwrap();
        assert_eq!(blocks.summaries()[0].command, "");
        assert!(!blocks.is_running());
    }

    #[test]
    fn plain_text_drops_styles_and_links() {
        let encoded = "\x1b[0m\x1b]8;;\x1b\\\x1b[0;38;2;1;2;3;48;2;4;5;6;1mhi\x1b]8;;https://x\x1b\\ there\x1b]8;;\x1b\\\x1b[0m";
        assert_eq!(plain_text(encoded), "hi there");
    }

    #[test]
    fn full_screen_programs_are_detected() {
        let mut output = terminal(10, 3);
        assert!(!is_full_screen(&output));
        output.vt_write(b"\x1b[?1049h");
        assert!(is_full_screen(&output));
    }
}
