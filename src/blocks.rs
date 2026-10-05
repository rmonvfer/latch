//! Command blocks: a pane's history as one block per command, each with its
//! own output.
//!
//! While a command runs, the pane gives it a fresh terminal that receives
//! exactly that command's output; the block captures rows from it. When
//! the command finishes, its rows are captured once into an immutable
//! snapshot and the terminal is replaced, which keeps memory bounded and
//! makes painting finished blocks a cheap row list. Each block also keeps
//! its raw output, within a budget, so its rows can be rebuilt when the
//! pane's width or colors change.

use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
use libghostty_vt::{
    Terminal,
    terminal::{Mode, ScrollViewport},
};

use crate::{
    grid::{FrameRow, GridRenderer},
    hooks::Precmd,
};

pub type Rows = Rc<Vec<Rc<FrameRow>>>;

/// Most raw output kept across all blocks for rebuilding their rows. Past
/// this the oldest blocks give theirs up and keep their rows as captured.
const OUTPUT_BUDGET: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockContext {
    pub cwd: Option<PathBuf>,
    pub virtualenv: Option<String>,
    pub conda_env: Option<String>,
}

impl From<&Precmd> for BlockContext {
    fn from(precmd: &Precmd) -> Self {
        Self {
            cwd: precmd.cwd.clone(),
            virtualenv: precmd.virtualenv.clone(),
            conda_env: precmd.conda_env.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub exit_code: i32,
    pub duration: Duration,
}

/// Rows captured so far from a running command's terminal.
pub struct LiveOutput {
    renderer: GridRenderer,
    /// Rows that have scrolled off the top, captured in order as pages, so
    /// handing them to the painter clones a page list rather than every row.
    history: Vec<Rows>,
    history_len: usize,
}

pub enum Output {
    Live(Box<LiveOutput>),
    Done(Rows),
}

pub struct Block {
    pub command: String,
    pub context: BlockContext,
    pub started: Instant,
    pub outcome: Option<Outcome>,
    pub output: Output,
    /// Whether only the header shows.
    pub collapsed: bool,
    /// Everything the command wrote, while it fits the budget.
    raw: Option<Vec<u8>>,
}

impl Block {
    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    pub fn failed(&self) -> bool {
        self.outcome.is_some_and(|outcome| outcome.exit_code != 0)
    }

    /// Every row of the block's output, trimmed of trailing blank rows.
    /// A running block reads from `terminal`, the one its command writes to.
    pub fn rows(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<Rows> {
        match &mut self.output {
            Output::Done(rows) => Ok(Rc::clone(rows)),
            Output::Live(live) => live.rows(terminal),
        }
    }

    /// Rows of a running block that scrolled off its terminal, as pages in
    /// order. The rest of its output is the terminal's live screen.
    pub fn scrollback(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<Vec<Rows>> {
        match &mut self.output {
            Output::Done(_) => Ok(Vec::new()),
            Output::Live(live) => {
                live.capture_scrollback(terminal)?;
                Ok(live.history.clone())
            }
        }
    }

    /// A finished block's output as plain text, one line per row.
    pub fn output_text(&self) -> Option<String> {
        let rows = self.finished_rows()?;
        let lines: Vec<String> = rows
            .iter()
            .map(|row| row.text().trim_end().to_string())
            .collect();
        Some(lines.join("\n"))
    }

    /// Rows of a finished block.
    pub fn finished_rows(&self) -> Option<Rows> {
        match &self.output {
            Output::Done(rows) => Some(Rc::clone(rows)),
            Output::Live(_) => None,
        }
    }
}

/// Whether a program switched `terminal` to the alternate screen, where it
/// expects the whole pane (vim, htop).
pub fn is_full_screen(terminal: &Terminal<'static, 'static>) -> bool {
    terminal.mode(Mode::ALT_SCREEN_SAVE).unwrap_or(false)
        || terminal.mode(Mode::ALT_SCREEN).unwrap_or(false)
}

impl LiveOutput {
    pub fn new() -> Result<Self> {
        Ok(Self {
            renderer: GridRenderer::new()?,
            history: Vec::new(),
            history_len: 0,
        })
    }

    /// Forget captured scrollback, which a resize reflows.
    pub fn reset_history(&mut self) {
        self.history.clear();
        self.history_len = 0;
    }

    /// Capture rows that scrolled off since the last call, so streaming
    /// output costs work in proportion to what is new. Leaves the terminal
    /// scrolled to the bottom.
    fn capture_scrollback(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<()> {
        let screen_rows = terminal.rows()? as usize;
        let scrollback = terminal.total_rows()?.saturating_sub(screen_rows);
        while self.history_len < scrollback {
            terminal.scroll_viewport(ScrollViewport::Row(self.history_len));
            let page = self.renderer.viewport_rows(terminal)?;
            let wanted = (scrollback - self.history_len).min(page.len());
            if wanted == 0 {
                break;
            }
            self.history
                .push(Rc::new(page.iter().take(wanted).cloned().collect()));
            self.history_len += wanted;
        }
        terminal.scroll_viewport(ScrollViewport::Bottom);
        Ok(())
    }

    /// Captured scrollback plus the live screen.
    fn rows(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<Rows> {
        self.capture_scrollback(terminal)?;
        let screen = self.renderer.viewport_rows(terminal)?;
        let mut rows: Vec<Rc<FrameRow>> = self
            .history
            .iter()
            .flat_map(|page| page.iter().cloned())
            .collect();
        rows.extend(screen.iter().cloned());
        trim_blank_tail(&mut rows);
        Ok(Rc::new(rows))
    }
}

fn trim_blank_tail(rows: &mut Vec<Rc<FrameRow>>) {
    while rows.last().is_some_and(|row| row.is_blank()) {
        rows.pop();
    }
}

/// The blocks of one pane, oldest first.
#[derive(Default)]
pub struct BlockList {
    blocks: Vec<Block>,
    /// Context from the shell's latest prompt, applied to the next block.
    context: BlockContext,
    /// Most blocks kept; the oldest are dropped beyond this.
    limit: usize,
}

impl BlockList {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            ..Self::default()
        }
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn blocks_mut(&mut self) -> &mut [Block] {
        &mut self.blocks
    }

    pub fn set_context(&mut self, precmd: &Precmd) {
        self.context = BlockContext::from(precmd);
    }

    /// Start a block for `command`. Its output goes to a terminal the pane
    /// dedicates to it until it finishes.
    pub fn start(&mut self, command: String) -> Result<()> {
        // A block still running when another starts never got its finish
        // hook (the shell was killed or exec'd); close it as unknown.
        if let Some(running) = self.running_mut() {
            running.outcome = Some(Outcome {
                exit_code: -1,
                duration: running.started.elapsed(),
            });
        }
        self.blocks.push(Block {
            command,
            context: self.context.clone(),
            started: Instant::now(),
            outcome: None,
            output: Output::Live(Box::new(LiveOutput::new()?)),
            collapsed: false,
            raw: Some(Vec::new()),
        });
        if self.blocks.len() > self.limit {
            let excess = self.blocks.len() - self.limit;
            self.blocks.drain(..excess);
        }
        Ok(())
    }

    pub fn running(&self) -> Option<&Block> {
        self.blocks.last().filter(|block| block.is_running())
    }

    pub fn running_mut(&mut self) -> Option<&mut Block> {
        self.blocks.last_mut().filter(|block| block.is_running())
    }

    /// Close the running block with `exit_code`, capturing its output from
    /// `terminal`.
    pub fn finish(
        &mut self,
        exit_code: i32,
        terminal: &mut Terminal<'static, 'static>,
    ) -> Result<()> {
        let Some(block) = self.running_mut() else {
            return Ok(());
        };
        block.outcome = Some(Outcome {
            exit_code,
            duration: block.started.elapsed(),
        });
        let rows = block.rows(terminal)?;
        block.output = Output::Done(rows);
        Ok(())
    }

    /// Keep what the shell printed while starting up (a greeting, messages
    /// from its config) as a block without a command, if there is any.
    pub fn push_startup(&mut self, terminal: &mut Terminal<'static, 'static>) -> Result<()> {
        let rows = LiveOutput::new()?.rows(terminal)?;
        if rows.is_empty() {
            return Ok(());
        }
        self.blocks.push(Block {
            command: String::new(),
            context: self.context.clone(),
            started: Instant::now(),
            outcome: Some(Outcome {
                exit_code: 0,
                duration: Duration::ZERO,
            }),
            output: Output::Done(rows),
            collapsed: false,
            raw: None,
        });
        Ok(())
    }

    /// Keep `bytes` the running command wrote, for rebuilding its rows.
    pub fn record(&mut self, bytes: &[u8]) {
        let Some(raw) = self
            .blocks
            .last_mut()
            .filter(|block| block.is_running())
            .and_then(|block| block.raw.as_mut())
        else {
            return;
        };
        raw.extend_from_slice(bytes);
        self.enforce_budget();
    }

    /// Drop the oldest blocks' raw output until the total fits the budget.
    fn enforce_budget(&mut self) {
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

    /// Rebuild finished blocks' rows by replaying their output into
    /// terminals from `new_terminal`, after the pane's width or colors
    /// changed. Those terminals must not be connected to the PTY, since
    /// replayed queries would otherwise be answered again.
    pub fn rebuild_rows(
        &mut self,
        mut new_terminal: impl FnMut() -> Result<Terminal<'static, 'static>>,
    ) -> Result<()> {
        for block in &mut self.blocks {
            let (Output::Done(rows), Some(raw)) = (&mut block.output, &block.raw) else {
                continue;
            };
            let mut terminal = new_terminal()?;
            terminal.vt_write(raw);
            *rows = LiveOutput::new()?.rows(&mut terminal)?;
        }
        Ok(())
    }

    /// The running terminal was resized, which reflows its scrollback.
    pub fn terminal_resized(&mut self) {
        if let Some(Block {
            output: Output::Live(live),
            ..
        }) = self.running_mut()
        {
            live.reset_history();
        }
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

    fn text(rows: &Rows) -> usize {
        rows.iter().filter(|row| !row.is_blank()).count()
    }

    #[test]
    fn blocks_capture_all_output_including_scrollback() {
        let mut list = BlockList::new(10);
        let mut output_terminal = terminal(20, 5);
        list.start("seq 1 30".into()).unwrap();
        let output: String = (1..=30).map(|n| format!("{n}\r\n")).collect();
        output_terminal.vt_write(output.as_bytes());

        // Streaming: rows are captured incrementally while running.
        let live = list
            .running_mut()
            .unwrap()
            .rows(&mut output_terminal)
            .unwrap();
        assert_eq!(text(&live), 30);
        output_terminal.vt_write(b"31\r\n");
        let live = list
            .running_mut()
            .unwrap()
            .rows(&mut output_terminal)
            .unwrap();
        assert_eq!(text(&live), 31);

        list.finish(0, &mut output_terminal).unwrap();
        let block = &list.blocks()[0];
        assert!(!block.is_running());
        assert_eq!(text(&block.finished_rows().unwrap()), 31);
        let output = block.output_text().unwrap();
        assert!(output.starts_with("1\n2\n3\n"));
        assert!(output.ends_with("30\n31"));
    }

    #[test]
    fn new_blocks_take_the_latest_prompt_context() {
        let mut list = BlockList::new(10);
        list.set_context(&Precmd {
            exit_code: 0,
            cwd: Some(PathBuf::from("/repo")),
            virtualenv: Some("venv".into()),
            conda_env: None,
        });
        list.start("ls".into()).unwrap();
        assert_eq!(list.blocks()[0].context.cwd, Some(PathBuf::from("/repo")));
        assert_eq!(list.blocks()[0].context.virtualenv.as_deref(), Some("venv"));
    }

    #[test]
    fn a_new_block_closes_an_unfinished_one_and_the_list_is_capped() {
        let mut list = BlockList::new(2);
        let mut output_terminal = terminal(10, 3);
        list.start("a".into()).unwrap();
        list.start("b".into()).unwrap();
        assert_eq!(
            list.blocks()[0].outcome.map(|outcome| outcome.exit_code),
            Some(-1)
        );
        list.finish(1, &mut output_terminal).unwrap();
        list.start("c".into()).unwrap();
        let commands: Vec<&str> = list
            .blocks()
            .iter()
            .map(|block| block.command.as_str())
            .collect();
        assert_eq!(commands, vec!["b", "c"]);
        assert!(list.blocks()[0].failed());
    }

    #[test]
    fn finished_blocks_rewrap_at_a_new_width() {
        let mut list = BlockList::new(10);
        let mut output_terminal = terminal(20, 5);
        list.start("echo".into()).unwrap();
        let line = "x".repeat(30);
        list.record(line.as_bytes());
        output_terminal.vt_write(line.as_bytes());
        list.finish(0, &mut output_terminal).unwrap();
        assert_eq!(list.blocks()[0].finished_rows().unwrap().len(), 2);

        list.rebuild_rows(|| Ok(terminal(10, 5))).unwrap();
        assert_eq!(list.blocks()[0].finished_rows().unwrap().len(), 3);
    }

    #[test]
    fn startup_output_becomes_a_block_only_when_there_is_some() {
        let mut list = BlockList::new(10);
        list.push_startup(&mut terminal(10, 3)).unwrap();
        assert!(list.blocks().is_empty());
        let mut startup = terminal(10, 3);
        startup.vt_write(b"welcome\r\n");
        list.push_startup(&mut startup).unwrap();
        assert_eq!(list.blocks()[0].command, "");
        assert!(!list.blocks()[0].is_running());
    }

    #[test]
    fn full_screen_programs_are_detected() {
        let mut output_terminal = terminal(10, 3);
        assert!(!is_full_screen(&output_terminal));
        output_terminal.vt_write(b"\x1b[?1049h");
        assert!(is_full_screen(&output_terminal));
    }
}
