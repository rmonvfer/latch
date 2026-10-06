//! Messages exchanged with the local session runtime.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
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
    Ready { version: u32 },
    Created(SessionInfo),
    Sessions(Vec<SessionInfo>),
    Snapshot(Snapshot),
    Unchanged,
    Done,
    Text(String),
    Matches { query: String, matches: Vec<Match> },
    Error(String),
}

#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub version: u32,
    pub token: String,
    pub request: Request,
}
