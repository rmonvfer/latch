//! Sessions and block rows for tests in this crate and in crates that
//! build on it, through the `test-support` feature.

use std::{
    collections::BTreeMap,
    thread,
    time::{Duration, Instant},
};

use libghostty_vt::{Terminal, terminal::Options};

use crate::{
    engine::{MAX_READ_LINES, SessionHandle, blocks::Blocks, spawn},
    protocol::{Colors, Dimensions, Launch, Operation, Request, Response, Snapshot},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(8);

/// A session running `script` under `/bin/sh`, small and with plain colors.
pub fn launch(script: &str) -> Launch {
    Launch {
        id: 51,
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        cwd: Some(std::env::temp_dir()),
        startup: None,
        agent_session: None,
        dimensions: Dimensions {
            cols: 60,
            rows: 8,
            cell_width: 8,
            cell_height: 16,
        },
        colors: Colors {
            foreground: [230, 230, 230],
            background: [20, 20, 20],
            cursor: [255, 255, 255],
            palette: [[100, 100, 100]; 16],
            dark: true,
        },
        control_token: "test-control-capability".into(),
        command_blocks: false,
    }
}

/// A session the test owns, stopped when dropped.
pub struct TestSession(pub(crate) SessionHandle);

impl Drop for TestSession {
    fn drop(&mut self) {
        let _ = self.0.request(Request::Operate {
            session: self.0.id,
            operation: Operation::Stop,
        });
    }
}

impl TestSession {
    pub fn new(script: &str) -> Self {
        Self::spawn(51, launch(script))
    }

    pub fn spawn(id: u64, launch: Launch) -> Self {
        Self(spawn(id, launch).unwrap())
    }

    pub fn operate(&self, operation: Operation) -> Response {
        self.0
            .request(Request::Operate {
                session: self.0.id,
                operation,
            })
            .unwrap()
    }

    pub fn snapshot(&self) -> Snapshot {
        match self
            .0
            .request(Request::Poll {
                session: self.0.id,
                revision: None,
            })
            .unwrap()
        {
            Response::Snapshot(snapshot) => snapshot,
            response => panic!("expected snapshot: {response:?}"),
        }
    }

    pub fn text(&self) -> String {
        match self.operate(Operation::Read {
            lines: MAX_READ_LINES,
        }) {
            Response::Text(text) => text,
            response => panic!("expected text: {response:?}"),
        }
    }

    pub fn wait_text(&self, expected: &str) {
        wait_until(|| self.text().contains(expected));
    }
}

/// Wait for `predicate`, failing the test after a few seconds.
pub fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !predicate() {
        assert!(Instant::now() < deadline, "session condition timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

/// The rows the runtime sends for a finished block that printed `output`,
/// captured at `cols` columns.
pub fn block_rows(output: &[u8], cols: u16) -> Vec<String> {
    let mut blocks = Blocks::new(10).unwrap();
    let mut terminal = Terminal::new(Options {
        cols,
        rows: 60,
        max_scrollback: 64 * 1024 * 1024,
    })
    .unwrap();
    blocks.start("build".into());
    blocks.record(output);
    terminal.vt_write(output);
    blocks.finish(0, &mut terminal).unwrap();
    let id = blocks.summaries()[0].id;
    blocks.rows(id, 0).unwrap().1
}
