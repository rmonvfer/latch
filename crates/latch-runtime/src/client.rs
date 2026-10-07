//! A view's connection to one session: operations go out on a dedicated
//! thread, and frames, block rows and replies come back as events.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};

use latch_core::shell::hooks::Bootstrapped;

use crate::{
    directory::Paths,
    protocol::{Blocks, Match, Operation, Request, Response, Snapshot},
    transport::{RemoteError, Rpc, reserve},
};

const POLL_INTERVAL: Duration = Duration::from_millis(16);
pub(crate) const CLIENT_QUEUE_BYTES: usize = 512 * 1024;
const CLIENT_QUEUE_CAPACITY: usize = 128;

/// Turns a block's encoded rows into the rows a view paints, given the
/// session's column count and background color.
pub type RowDecoder<Row> = fn(&[String], u16, [u8; 3]) -> Result<Vec<Row>>;

pub enum ClientEvent<Row> {
    Frame(Box<Snapshot>),
    /// Rows of a command block from `from` on, at `version`, decoded for
    /// painting on this connection's thread; rows from an earlier version
    /// are replaced.
    BlockRows {
        block: u64,
        version: u64,
        from: usize,
        rows: Vec<Row>,
    },
    /// What the shell reported about itself.
    Shell(Box<Option<Bootstrapped>>),
    Text(String),
    Matches {
        query: String,
        matches: Vec<Match>,
    },
    Disconnected(String),
    Error(String),
}

pub(crate) struct QueuedOperation {
    pub(crate) operation: Operation,
    pub(crate) bytes: usize,
}

pub struct SessionClient {
    pub(crate) operations: mpsc::SyncSender<QueuedOperation>,
    pub(crate) pending_bytes: Arc<AtomicUsize>,
    pub(crate) stopped: Arc<AtomicBool>,
    pub(crate) refresh: Arc<AtomicBool>,
}

impl SessionClient {
    pub fn attach<Row: Send + 'static>(
        session: u64,
        decode: RowDecoder<Row>,
    ) -> Result<(Self, async_channel::Receiver<ClientEvent<Row>>)> {
        Self::attach_at(session, Paths::configured(), decode)
    }

    pub(crate) fn attach_at<Row: Send + 'static>(
        session: u64,
        paths: Paths,
        decode: RowDecoder<Row>,
    ) -> Result<(Self, async_channel::Receiver<ClientEvent<Row>>)> {
        let (operations, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
        let (events, output) = async_channel::bounded(2);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(false));
        let connection_paths = paths.clone();
        let pending = Arc::clone(&pending_bytes);
        let cancellation = Arc::clone(&stopped);
        let refresh_requested = Arc::clone(&refresh);
        thread::Builder::new()
            .name(format!("session-view-{session}"))
            .spawn(move || {
                client_loop(
                    session,
                    &connection_paths,
                    decode,
                    receiver,
                    &pending,
                    &events,
                    &cancellation,
                    &refresh_requested,
                );
            })
            .context("cannot start session connection")?;
        Ok((
            Self {
                operations,
                pending_bytes,
                stopped,
                refresh,
            },
            output,
        ))
    }

    pub fn refresh(&self) {
        self.refresh.store(true, Ordering::Release);
    }

    pub fn send(&self, operation: Operation) -> Result<()> {
        let bytes = operation_size(&operation);
        reserve(&self.pending_bytes, bytes, CLIENT_QUEUE_BYTES)
            .context("session input is busy; try again")?;
        if let Err(error) = self
            .operations
            .try_send(QueuedOperation { operation, bytes })
        {
            self.pending_bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err(match error {
                mpsc::TrySendError::Full(_) => anyhow!("session input is busy; try again"),
                mpsc::TrySendError::Disconnected(_) => anyhow!("session connection is closed"),
            });
        }
        Ok(())
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn operation_size(operation: &Operation) -> usize {
    let payload = match operation {
        Operation::Input { bytes } => bytes.len(),
        Operation::Paste { text } | Operation::Search { query: text } => text.len(),
        Operation::Key {
            text: Some(text), ..
        } => text.len(),
        _ => 0,
    };
    payload.saturating_add(256)
}

fn deliver<Row>(
    events: &async_channel::Sender<ClientEvent<Row>>,
    mut event: ClientEvent<Row>,
    stopped: &AtomicBool,
) -> bool {
    loop {
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        match events.try_send(event) {
            Ok(()) => return true,
            Err(async_channel::TrySendError::Closed(_)) => return false,
            Err(async_channel::TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(Duration::from_millis(4));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn client_loop<Row>(
    session: u64,
    paths: &Paths,
    decode: RowDecoder<Row>,
    operations: mpsc::Receiver<QueuedOperation>,
    pending_bytes: &AtomicUsize,
    events: &async_channel::Sender<ClientEvent<Row>>,
    stopped: &AtomicBool,
    refresh: &AtomicBool,
) {
    let mut connection = None;
    let mut revision = None;
    let mut poll_at = Instant::now();
    let mut retry_at = Instant::now();
    let mut reported_error = None;
    let mut block_state = BlockFetchState::default();
    while !stopped.load(Ordering::Acquire) && !events.is_closed() {
        if refresh.swap(false, Ordering::AcqRel) {
            retry_at = Instant::now();
            poll_at = Instant::now();
            revision = None;
        }
        if connection.is_none() && Instant::now() >= retry_at {
            match Rpc::connect(paths) {
                Ok(client) => {
                    connection = Some(client);
                    revision = None;
                    poll_at = Instant::now();
                }
                Err(error) => {
                    let message = format!("Session disconnected: {error:#}");
                    if reported_error.as_ref() != Some(&message) {
                        if !deliver(events, ClientEvent::Disconnected(message.clone()), stopped) {
                            return;
                        }
                        reported_error = Some(message);
                    }
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }

        let timeout = if connection.is_some() {
            poll_at.saturating_duration_since(Instant::now())
        } else {
            Duration::from_millis(16)
        };
        match operations.recv_timeout(timeout) {
            Ok(queued) => {
                pending_bytes.fetch_sub(queued.bytes, Ordering::AcqRel);
                if let Some(client) = connection.as_mut() {
                    match client.request(Request::Operate {
                        session,
                        operation: queued.operation,
                    }) {
                        Ok(Response::Text(text)) => {
                            if !deliver(events, ClientEvent::Text(text), stopped) {
                                return;
                            }
                        }
                        Ok(Response::Matches { query, matches }) => {
                            if !deliver(events, ClientEvent::Matches { query, matches }, stopped) {
                                return;
                            }
                        }
                        Ok(Response::Done) => {}
                        Ok(_) => {
                            if !deliver(
                                events,
                                ClientEvent::Error(
                                    "Session returned an invalid operation response".into(),
                                ),
                                stopped,
                            ) {
                                return;
                            }
                        }
                        Err(error) => {
                            let remote_error = error.downcast_ref::<RemoteError>().is_some();
                            if !deliver(
                                events,
                                ClientEvent::Error(format!(
                                    "Session operation failed; it was not retried: {error:#}"
                                )),
                                stopped,
                            ) {
                                return;
                            }
                            if !remote_error {
                                if !deliver(
                                    events,
                                    ClientEvent::Disconnected(format!(
                                        "Session disconnected: {error:#}"
                                    )),
                                    stopped,
                                ) {
                                    return;
                                }
                                connection = None;
                                retry_at = Instant::now() + Duration::from_millis(250);
                            }
                        }
                    }
                } else if !deliver(
                    events,
                    ClientEvent::Error("Session is disconnected; input was not sent".into()),
                    stopped,
                ) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if Instant::now() < poll_at {
            continue;
        }
        if let Some(client) = connection.as_mut() {
            match client.request(Request::Poll { session, revision }) {
                Ok(Response::Snapshot(snapshot)) => {
                    let next_revision = snapshot.revision;
                    // The frame goes first: block rows are drawn over its
                    // background.
                    let blocks = snapshot.blocks.clone();
                    let painting = (snapshot.dimensions.cols, snapshot.background);
                    if events
                        .try_send(ClientEvent::Frame(Box::new(snapshot)))
                        .is_ok()
                    {
                        revision = Some(next_revision);
                        reported_error = None;
                    }
                    if let Some(blocks) = &blocks
                        && !fetch_block_updates(
                            client,
                            session,
                            decode,
                            blocks,
                            painting,
                            &mut block_state,
                            events,
                            stopped,
                        )
                    {
                        return;
                    }
                }
                Ok(Response::Unchanged) => {}
                Ok(_) => {
                    if !deliver(
                        events,
                        ClientEvent::Disconnected(
                            "Session returned an invalid frame response".into(),
                        ),
                        stopped,
                    ) {
                        return;
                    }
                    connection = None;
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
                Err(error) => {
                    let message = format!("Session disconnected: {error:#}");
                    if reported_error.as_ref() != Some(&message) {
                        if !deliver(events, ClientEvent::Disconnected(message.clone()), stopped) {
                            return;
                        }
                        reported_error = Some(message);
                    }
                    connection = None;
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        poll_at = Instant::now() + POLL_INTERVAL;
    }
}

/// What a view has been sent of a session's blocks.
#[derive(Default)]
struct BlockFetchState {
    shell_serial: u64,
    /// Version and number of rows sent, per block.
    rows: HashMap<u64, (u64, usize)>,
}

/// Send the view block rows and shell details a snapshot announces that it
/// does not have yet. Returns false once the view is gone.
#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
fn fetch_block_updates<Row>(
    client: &mut Rpc,
    session: u64,
    decode: RowDecoder<Row>,
    blocks: &Blocks,
    (columns, background): (u16, [u8; 3]),
    state: &mut BlockFetchState,
    events: &async_channel::Sender<ClientEvent<Row>>,
    stopped: &AtomicBool,
) -> bool {
    if blocks.shell_serial != state.shell_serial
        && let Ok(Response::Shell(shell)) = client.request(Request::Operate {
            session,
            operation: Operation::Shell,
        })
    {
        state.shell_serial = blocks.shell_serial;
        if !deliver(events, ClientEvent::Shell(shell), stopped) {
            return false;
        }
    }
    state
        .rows
        .retain(|id, _| blocks.items.iter().any(|item| item.id == *id));
    for item in &blocks.items {
        let sent = state.rows.entry(item.id).or_insert((item.version, 0));
        if sent.0 != item.version {
            *sent = (item.version, 0);
        }
        while sent.1 < item.rows {
            let Ok(Response::BlockRows {
                version,
                from,
                rows,
                ..
            }) = client.request(Request::Operate {
                session,
                operation: Operation::BlockRows {
                    block: item.id,
                    from: sent.1,
                },
            })
            else {
                break;
            };
            // The block changed since the snapshot; the next one says how.
            if version != item.version || rows.is_empty() {
                break;
            }
            sent.1 = from + rows.len();
            let rows = match decode(&rows, columns, background) {
                Ok(rows) => rows,
                Err(error) => {
                    log::error!("failed to decode block rows: {error:#}");
                    break;
                }
            };
            if !deliver(
                events,
                ClientEvent::BlockRows {
                    block: item.id,
                    version,
                    from,
                    rows,
                },
                stopped,
            ) {
                return false;
            }
        }
    }
    true
}
