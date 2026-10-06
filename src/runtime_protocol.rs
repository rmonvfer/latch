//! Messages exchanged with the local session runtime.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::hooks::{Bootstrapped, Completion};

pub const VERSION: u32 = 2;
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimensions {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colors {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: [u8; 3],
    pub palette: [[u8; 3]; 16],
    pub dark: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Launch {
    pub id: u64,
    /// An empty argument list selects the user's default login shell.
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub startup: Option<String>,
    pub dimensions: Dimensions,
    pub colors: Colors,
    /// Capability used by the existing GUI control API, never the runtime socket.
    pub control_token: String,
    /// Show each command as a block once the shell's integration reports in.
    #[serde(default)]
    pub command_blocks: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u64,
    pub shell_pid: Option<i32>,
    pub foreground_pid: Option<i32>,
    pub cwd: Option<PathBuf>,
    pub title: String,
    pub process: Option<String>,
    pub agent: Option<String>,
    pub status: String,
    pub exited: bool,
    pub exit_code: Option<u32>,
    pub command_elapsed_ms: Option<u64>,
    pub command_serial: u64,
    pub last_command_exit: Option<i32>,
    pub last_command_duration_ms: Option<u64>,
    pub attention_serial: u64,
    pub attention: Option<Attention>,
    pub control_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Attention {
    Bell,
    Notification {
        title: Option<String>,
        body: String,
    },
    CommandFinished {
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    AgentWaiting {
        agent: String,
        needs_input: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    /// Cursor shape: hollow block 0, block 2, underline 4, bar 6.
    pub style: u8,
    pub color: [u8; 3],
}

/// Where a command ran: the prompt's directory and Python environments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockContext {
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub git_branch: Option<String>,
    pub virtualenv: Option<String>,
    pub conda_env: Option<String>,
}

/// One command block. Its rows are fetched separately, since they change
/// far less often than the screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSummary {
    pub id: u64,
    /// Changes when the block's rows are rebuilt, such as after a resize.
    pub version: u64,
    pub command: String,
    pub context: BlockContext,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    /// When the command started, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub started_at_ms: u64,
    pub running: bool,
    /// Rows available: all of a finished block's output, or the rows of a
    /// running command that scrolled off its terminal, whose live screen is
    /// the snapshot.
    pub rows: usize,
}

/// Completions the shell reported for text typed before the completion key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completions {
    pub serial: u64,
    pub prefix: String,
    pub matches: Vec<Completion>,
}

/// The session's command blocks and shell state, once blocks are on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocks {
    pub items: Vec<BlockSummary>,
    /// Context of the shell's current prompt, once it has drawn one.
    pub prompt: Option<BlockContext>,
    /// Changes when the shell reports its aliases, functions, and history.
    pub shell_serial: u64,
    pub completions: Option<Completions>,
    /// A program has switched the screen to the alternate buffer.
    pub full_screen: bool,
}

/// A complete viewport, independent of parser history and missed updates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub info: SessionInfo,
    pub dimensions: Dimensions,
    pub background: [u8; 3],
    /// Self-contained styled VT rows; no program-originated terminal effects.
    pub rows: Vec<String>,
    pub cursor: Cursor,
    pub mouse_tracking: bool,
    pub scroll_offset: usize,
    pub blocks: Option<Box<Blocks>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub row: u32,
    pub start_col: u16,
    pub end_col: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    Input {
        bytes: Vec<u8>,
    },
    Paste {
        text: String,
    },
    Key {
        key: u32,
        action: u32,
        mods: u16,
        consumed_mods: u16,
        unshifted: char,
        text: Option<String>,
    },
    Mouse {
        action: u32,
        button: Option<u32>,
        mods: u16,
        x: f32,
        y: f32,
        width: u32,
        height: u32,
        padding: u32,
        pressed: bool,
    },
    Resize {
        dimensions: Dimensions,
    },
    Scroll {
        delta: isize,
    },
    SelectionPress {
        col: u16,
        row: u32,
        x: f64,
        y: f64,
        cell_width: f64,
    },
    SelectionDrag {
        col: u16,
        row: u32,
        x: f64,
        y: f64,
        rectangle: bool,
        cell_width: u32,
        padding: u32,
        height: u32,
    },
    SelectionRelease {
        col: u16,
        row: u32,
    },
    Copy,
    SelectAll,
    ClearSelection,
    AcknowledgeAttention {
        serial: u64,
    },
    ClearScrollback,
    Search {
        query: String,
    },
    SelectMatch {
        found: Match,
    },
    Prompt {
        forward: bool,
    },
    Colors {
        colors: Colors,
    },
    Read {
        lines: usize,
    },
    Stop,
    /// Clear the shell's line editor, type `command` into it, and run it.
    RunCommand {
        command: String,
    },
    /// Ask the shell for completions of `text`.
    Complete {
        text: String,
    },
    /// Encoded rows of a block from `from` on, if it is still at `version`.
    BlockRows {
        block: u64,
        from: usize,
    },
    /// What the shell reported about itself.
    Shell,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Create(Launch),
    List,
    Poll { session: u64, revision: Option<u64> },
    Operate { session: u64, operation: Operation },
    Remove { session: u64 },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ready {
        version: u32,
    },
    Created(SessionInfo),
    Sessions(Vec<SessionInfo>),
    Snapshot(Snapshot),
    Unchanged,
    Done,
    Text(String),
    Matches {
        query: String,
        matches: Vec<Match>,
    },
    BlockRows {
        block: u64,
        version: u64,
        from: usize,
        rows: Vec<String>,
    },
    Shell(Box<Option<Bootstrapped>>),
    Error(String),
}

#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub version: u32,
    pub token: String,
    pub request: Request,
}
