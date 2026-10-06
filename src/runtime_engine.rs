//! Persistent terminal sessions, each owned by a dedicated runtime thread.

use std::{
    cell::Cell,
    fmt::Write as _,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, RwLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use libghostty_vt::{
    Terminal,
    fmt::Format,
    key, mouse, paste,
    render::{
        self, CellIterator, CursorVisualStyle, Dirty, RenderState, RowIteration, RowIterator,
    },
    screen::{CellWide, RowSemanticPrompt},
    selection::{
        FormatOptions, Selection,
        gesture::{DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent},
    },
    style::{Palette, RgbColor, Style, StyleColor, Underline},
    terminal::{
        ColorScheme, ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
        Options, Point, PointCoordinate, PrimaryDeviceAttributes, ScrollViewport,
        SecondaryDeviceAttributes, SizeReportSize,
    },
};
use portable_pty::CommandBuilder;

use crate::{
    agents::{self, Activity, Agent, AgentStatus},
    hooks::{Bootstrapped, Hook},
    osc::{OscEvent, OscScanner, Piece},
    process_info,
    pty::{MAX_INPUT_BYTES, Pty, PtyDimensions},
    runtime_blocks::{self, Blocks as BlockLog},
    runtime_protocol::{
        Attention, BlockContext, Blocks, Colors, Completions, Cursor, Dimensions, Launch, Match,
        Operation, Request, Response, SessionInfo, Snapshot,
    },
    search, shell_integration,
};

/// Scrollback kept per terminal. libghostty measures it in bytes of its
/// page storage, not lines: this keeps roughly 17,000 rows at 200 columns.
/// Pages are allocated as output arrives, so it is a ceiling, not a cost.
const SCROLLBACK_BYTES: usize = 32 * 1024 * 1024;
/// Most lines `read_output` returns.
const MAX_READ_LINES: usize = 10_000;
const COMMAND_CAPACITY: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_INTERVAL: Duration = Duration::from_millis(2);
const METADATA_INTERVAL: Duration = Duration::from_millis(400);
const STARTUP_FALLBACK: Duration = Duration::from_secs(4);
const MAX_GRID_CELLS: usize = 64 * 1024;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEARCH_MATCHES: usize = 20_000;
const AGENT_MIN_WORK: Duration = Duration::from_secs(3);
/// How long the width or colors must stay put before finished blocks are
/// rebuilt for them.
const REBUILD_DELAY: Duration = Duration::from_millis(250);
/// How long one slice of rebuilding blocks may take before the session
/// serves its terminal again.
const REBUILD_SLICE: Duration = Duration::from_millis(8);
/// Most command blocks a session keeps; older ones are dropped.
const MAX_BLOCKS: usize = 1000;

type Command = (Request, mpsc::SyncSender<Result<Response>>);

/// A handle to a runtime-owned session; releasing a client does not end it.
#[derive(Clone)]
pub struct SessionHandle {
    id: u64,
    commands: mpsc::SyncSender<Command>,
    info: Arc<RwLock<SessionInfo>>,
    worker: thread::Thread,
}

impl SessionHandle {
    pub fn info(&self) -> SessionInfo {
        self.info
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn request(&self, request: Request) -> Result<Response> {
        ensure!(
            matches!(&request, Request::Poll { session, .. } | Request::Operate { session, .. } if *session == self.id),
            "request targets another session"
        );
        let (reply, receive) = mpsc::sync_channel(1);
        self.commands
            .try_send((request, reply))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => anyhow!("session is busy; try again"),
                mpsc::TrySendError::Disconnected(_) => anyhow!("session runtime is unavailable"),
            })?;
        self.worker.unpark();
        receive
            .recv_timeout(REQUEST_TIMEOUT)
            .context("session runtime did not respond")?
    }
}

pub fn spawn(id: u64, launch: Launch) -> Result<SessionHandle> {
    validate_dimensions(launch.dimensions)?;
    ensure!(
        launch
            .startup
            .as_ref()
            .is_none_or(|value| value.len() < MAX_INPUT_BYTES),
        "startup command exceeds 256 KiB"
    );
    let (commands, receive) = mpsc::sync_channel(COMMAND_CAPACITY);
    let info = Arc::new(RwLock::new(SessionInfo {
        id,
        ..Default::default()
    }));
    let shared = Arc::clone(&info);
    let (ready, initialized) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name(format!("session-{id}"))
        .spawn(move || match Engine::new(id, launch, shared) {
            Ok(mut engine) => {
                engine.refresh_info();
                if ready.send(Ok(())).is_ok() {
                    engine.run(receive);
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        })
        .context("failed to start session worker")?;
    initialized
        .recv_timeout(REQUEST_TIMEOUT)
        .context("session did not start")??;
    Ok(SessionHandle {
        id,
        commands,
        info,
        worker: worker.thread().clone(),
    })
}

fn validate_dimensions(dimensions: Dimensions) -> Result<()> {
    ensure!(
        dimensions.cols > 0
            && dimensions.rows > 0
            && dimensions.cell_width > 0
            && dimensions.cell_height > 0,
        "terminal dimensions must be positive"
    );
    ensure!(
        dimensions.cols <= 1024
            && dimensions.rows <= 512
            && usize::from(dimensions.cols) * usize::from(dimensions.rows) <= MAX_GRID_CELLS,
        "terminal dimensions are too large"
    );
    Ok(())
}

fn pty_dimensions(value: Dimensions) -> PtyDimensions {
    PtyDimensions {
        cols: value.cols,
        rows: value.rows,
        cell_width: value.cell_width,
        cell_height: value.cell_height,
    }
}

struct Metadata {
    foreground: Option<i32>,
    process: Option<String>,
    cwd: Option<PathBuf>,
    agent: Option<Agent>,
}

struct Engine {
    /// The terminal receiving output: the shell's, or with command blocks
    /// the running command's or the prompt's.
    terminal: Terminal<'static, 'static>,
    colors: Colors,
    /// Command blocks, once the shell's integration reports in and the
    /// launch asked for them.
    blocks: Option<BlockLog>,
    command_blocks: bool,
    shell: Option<Bootstrapped>,
    shell_serial: u64,
    prompt: Option<BlockContext>,
    completions: Option<Completions>,
    /// The terminal was swapped since the last snapshot, so every row is new.
    terminal_replaced: bool,
    /// When finished blocks are rebuilt for a new width or colors: shortly
    /// after the last change, so a window being dragged wider rebuilds once.
    rebuild_due: Option<Instant>,
    pty: Pty,
    output: async_channel::Receiver<Vec<u8>>,
    shared: Arc<RwLock<SessionInfo>>,
    info: SessionInfo,
    dimensions: Rc<Cell<Dimensions>>,
    dark: Rc<Cell<bool>>,
    bell: Rc<Cell<bool>>,
    reply_error: Rc<Cell<bool>>,
    osc: OscScanner,
    activity: Activity,
    agent: Option<Agent>,
    working_since: Option<Instant>,
    command_started: Option<Instant>,
    startup: Option<String>,
    started: Instant,
    metadata_requests: mpsc::SyncSender<(Option<i32>, Option<i32>)>,
    metadata_results: mpsc::Receiver<Metadata>,
    metadata_at: Instant,
    metadata_pending: bool,
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    render: RenderState<'static>,
    render_rows: RowIterator<'static>,
    render_cells: CellIterator<'static>,
    snapshot: Option<Snapshot>,
    revision: u64,
    stopped: bool,
    exit_checked_at: Instant,
}

impl Engine {
    fn new(id: u64, launch: Launch, shared: Arc<RwLock<SessionInfo>>) -> Result<Self> {
        let mut command = CommandBuilder::from_argv(launch.argv.iter().map(Into::into).collect());
        command.env_clear();
        for (key, value) in &launch.env {
            command.env(key, value);
        }
        command.env(crate::terminal_view::PANE_ID_VARIABLE, id.to_string());
        command.env(crate::control::TOKEN_VARIABLE, &launch.control_token);
        // Lifecycle hooks carry this id; hooks with any other are ignored.
        let session_id = crate::control::new_token()?;
        command.env(shell_integration::SESSION_ID_VARIABLE, &session_id);
        if launch.command_blocks {
            command.env(shell_integration::COMMAND_BLOCKS_VARIABLE, "1");
        }
        let (pty, output) = Pty::spawn(
            command,
            pty_dimensions(launch.dimensions),
            launch.cwd.as_deref(),
        )?;
        let dimensions = Rc::new(Cell::new(launch.dimensions));
        let dark = Rc::new(Cell::new(launch.colors.dark));
        let bell = Rc::new(Cell::new(false));
        let reply_error = Rc::new(Cell::new(false));
        let mut terminal = Terminal::new(Options {
            cols: launch.dimensions.cols,
            rows: launch.dimensions.rows,
            max_scrollback: SCROLLBACK_BYTES,
        })?;
        configure_colors(&mut terminal, &launch.colors)?;
        register_effects(
            &mut terminal,
            &pty,
            Rc::clone(&dimensions),
            Rc::clone(&dark),
            Rc::clone(&bell),
            Rc::clone(&reply_error),
        )?;
        let (metadata_requests, requested) = mpsc::sync_channel::<(Option<i32>, Option<i32>)>(1);
        let (results, metadata_results) = mpsc::sync_channel(1);
        let worker = thread::current();
        thread::Builder::new()
            .name(format!("session-metadata-{id}"))
            .spawn(move || {
                while let Ok((foreground, shell)) = requested.recv() {
                    let metadata = Metadata {
                        foreground,
                        process: foreground.and_then(process_info::process_name),
                        cwd: shell.and_then(process_info::working_directory),
                        agent: foreground
                            .filter(|pid| Some(*pid) != shell)
                            .and_then(process_info::process_args)
                            .and_then(|args| agents::detect(&args)),
                    };
                    if results.send(metadata).is_err() {
                        break;
                    }
                    worker.unpark();
                }
            })
            .context("failed to start session metadata worker")?;
        let mut press = PressEvent::new()?;
        press.set_repeat_interval(Duration::from_millis(500))?;
        let info = SessionInfo {
            id,
            shell_pid: pty.shell_pid(),
            cwd: pty.initial_cwd().map(PathBuf::from),
            title: "shell".into(),
            status: "idle".into(),
            control_token: launch.control_token,
            ..Default::default()
        };
        Ok(Self {
            terminal,
            colors: launch.colors,
            blocks: None,
            command_blocks: launch.command_blocks,
            shell: None,
            shell_serial: 0,
            prompt: None,
            completions: None,
            terminal_replaced: false,
            rebuild_due: None,
            pty,
            output,
            shared,
            info,
            dimensions,
            dark,
            bell,
            reply_error,
            osc: OscScanner::new(session_id),
            activity: Activity::default(),
            agent: None,
            working_since: None,
            command_started: None,
            startup: launch.startup,
            started: Instant::now(),
            metadata_requests,
            metadata_results,
            metadata_at: Instant::now() - METADATA_INTERVAL,
            metadata_pending: false,
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            gesture: Gesture::new()?,
            press,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            render: RenderState::new()?,
            render_rows: RowIterator::new()?,
            render_cells: CellIterator::new()?,
            snapshot: None,
            revision: 0,
            stopped: false,
            exit_checked_at: Instant::now() - METADATA_INTERVAL,
        })
    }

    fn run(&mut self, requests: mpsc::Receiver<Command>) {
        loop {
            self.consume_output();
            self.update_metadata();
            if self.startup.is_some() && self.started.elapsed() >= STARTUP_FALLBACK {
                self.send_startup();
            }
            if self.rebuild_due.is_some_and(|due| Instant::now() >= due) {
                self.rebuild_due = None;
                if let Some(blocks) = &mut self.blocks {
                    blocks.mark_stale();
                }
            }
            if self.rebuilding()
                && let Err(error) = self.rebuild_blocks()
            {
                self.attention(Attention::Notification {
                    title: Some("Command blocks".into()),
                    body: format!("{error:#}"),
                });
            }
            self.refresh_info();
            match requests.try_recv() {
                Ok((request, reply)) => {
                    let result = self.handle(request);
                    self.refresh_info();
                    let _ = reply.send(result);
                }
                Err(mpsc::TryRecvError::Empty) if self.output.is_empty() && !self.rebuilding() => {
                    thread::park_timeout(METADATA_INTERVAL)
                }
                Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.info.exited {
                        return;
                    }
                    // Runtime ownership survives the last GUI connection.
                    if self.output.is_empty() {
                        thread::park_timeout(METADATA_INTERVAL);
                    }
                }
            }
        }
    }

    #[tracing::instrument(skip_all)]
    fn consume_output(&mut self) {
        let output = self.output.clone();
        for chunk in crate::output::OutputBatch::new(&output) {
            for piece in self.osc.scan(&chunk) {
                let event = match piece {
                    Piece::Output(bytes) => {
                        if let Some(blocks) = &mut self.blocks {
                            blocks.record(&bytes);
                        }
                        self.terminal.vt_write(&bytes);
                        continue;
                    }
                    Piece::Event(event) => event,
                };
                match event {
                    OscEvent::Hook(hook) => self.apply_hook(hook),
                    OscEvent::PromptStarted => self.send_startup(),
                    OscEvent::CommandStarted => {
                        self.command_started = Some(Instant::now());
                        self.info.command_serial = self.info.command_serial.saturating_add(1);
                    }
                    OscEvent::CommandFinished(code) => {
                        // A finish without a start (e.g. the first prompt) has no command to report.
                        if let Some(started) = self.command_started.take() {
                            self.info.last_command_exit = code;
                            self.info.last_command_duration_ms =
                                Some(started.elapsed().as_millis() as u64);
                            self.attention(Attention::CommandFinished {
                                exit_code: code,
                                duration_ms: self.info.last_command_duration_ms.unwrap_or(0),
                            });
                        }
                    }
                    OscEvent::Notify { title, body } => {
                        self.activity.attention();
                        self.attention(Attention::Notification { title, body });
                    }
                }
            }
            self.activity.output(Instant::now());
        }
        if self.bell.replace(false) {
            self.activity.attention();
            self.attention(Attention::Bell);
        }
        if self.reply_error.replace(false) {
            self.attention(Attention::Notification {
                title: Some("Session input".into()),
                body: "Terminal input is busy; a protocol reply could not be delivered".into(),
            });
        }
    }

    #[tracing::instrument(skip_all)]
    fn apply_hook(&mut self, hook: Hook) {
        let result = match hook {
            Hook::Bootstrapped(shell) => {
                self.shell = Some(shell);
                self.shell_serial += 1;
                self.enable_blocks()
            }
            Hook::Precmd(precmd) => {
                if let Some(blocks) = &mut self.blocks {
                    blocks.set_context(&precmd);
                }
                self.prompt = Some(BlockContext::from(&precmd));
                Ok(())
            }
            Hook::Preexec { command } => match self.blocks.is_some() {
                // The command's output goes to a terminal of its own.
                true => self.replace_terminal().map(|()| {
                    if let Some(blocks) = &mut self.blocks {
                        blocks.start(command);
                    }
                }),
                false => Ok(()),
            },
            Hook::CommandFinished { exit_code } => match &mut self.blocks {
                Some(blocks) => blocks
                    .finish(exit_code, &mut self.terminal)
                    // The next prompt is drawn into a fresh terminal.
                    .and_then(|()| self.replace_terminal()),
                None => Ok(()),
            },
            Hook::Completions { prefix, matches } => {
                let serial = self.completions.as_ref().map_or(0, |found| found.serial) + 1;
                self.completions = Some(Completions {
                    serial,
                    prefix,
                    matches,
                });
                Ok(())
            }
        };
        if let Err(error) = result {
            self.attention(Attention::Notification {
                title: Some("Command blocks".into()),
                body: format!("{error:#}"),
            });
        }
    }

    /// Turn command blocks on when the launch asked for them, keeping what
    /// the shell printed while starting as a block of its own.
    fn enable_blocks(&mut self) -> Result<()> {
        if !self.command_blocks || self.blocks.is_some() {
            return Ok(());
        }
        let mut blocks = BlockLog::new(MAX_BLOCKS)?;
        blocks.push_startup(&mut self.terminal)?;
        self.blocks = Some(blocks);
        // The prompt is drawn into a fresh terminal, hidden behind the
        // window's own command editor.
        self.replace_terminal()
    }

    /// Swap in a fresh terminal at the session's size and colors.
    #[tracing::instrument(skip_all)]
    fn replace_terminal(&mut self) -> Result<()> {
        let dimensions = self.dimensions.get();
        let mut terminal = Terminal::new(Options {
            cols: dimensions.cols,
            rows: dimensions.rows,
            max_scrollback: SCROLLBACK_BYTES,
        })?;
        terminal.resize(
            dimensions.cols,
            dimensions.rows,
            u32::from(dimensions.cell_width),
            u32::from(dimensions.cell_height),
        )?;
        configure_colors(&mut terminal, &self.colors)?;
        register_effects(
            &mut terminal,
            &self.pty,
            Rc::clone(&self.dimensions),
            Rc::clone(&self.dark),
            Rc::clone(&self.bell),
            Rc::clone(&self.reply_error),
        )?;
        self.terminal = terminal;
        self.terminal_replaced = true;
        Ok(())
    }

    /// Rebuild some stale blocks' rows at the session's width and colors,
    /// as much as fits one slice of the loop.
    #[tracing::instrument(skip_all)]
    fn rebuild_blocks(&mut self) -> Result<()> {
        let Some(blocks) = &mut self.blocks else {
            return Ok(());
        };
        let dimensions = self.dimensions.get();
        let colors = &self.colors;
        blocks.rebuild_stale(
            || {
                let mut terminal = Terminal::new(Options {
                    cols: dimensions.cols,
                    rows: dimensions.rows,
                    max_scrollback: SCROLLBACK_BYTES,
                })?;
                configure_colors(&mut terminal, colors)?;
                Ok(terminal)
            },
            REBUILD_SLICE,
        )
    }

    fn rebuilding(&self) -> bool {
        self.blocks.as_ref().is_some_and(BlockLog::has_stale)
    }

    /// Write `text` into the shell's line editor after clearing it, then
    /// `suffix`: Enter to run it, or the key that asks for completions.
    fn type_into_shell(&mut self, text: String, suffix: &[u8]) -> Result<()> {
        ensure!(
            text.len() + 64 <= MAX_INPUT_BYTES,
            "command exceeds 256 KiB"
        );
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let mut data = text.into_bytes();
        let mut encoded = vec![0; data.len() + 32];
        let len = paste::encode(&mut data, bracketed, &mut encoded)?;
        let mut bytes = shell_integration::CLEAR_LINE_KEY.to_vec();
        bytes.extend_from_slice(&encoded[..len]);
        bytes.extend_from_slice(suffix);
        self.write_input(&bytes)
    }

    fn attention(&mut self, message: Attention) {
        self.info.attention_serial = self.info.attention_serial.saturating_add(1);
        self.info.attention = Some(message);
    }

    fn send_startup(&mut self) {
        if let Some(command) = self.startup.take()
            && let Err(error) = self.write_input(format!("{command}\n").as_bytes())
        {
            self.attention(Attention::Notification {
                title: Some("Session startup".into()),
                body: format!("Could not start command: {error}"),
            });
        }
    }

    #[tracing::instrument(skip_all)]
    fn update_metadata(&mut self) {
        if let Ok(metadata) = self.metadata_results.try_recv() {
            self.metadata_pending = false;
            if metadata.foreground == self.pty.foreground_pid() && self.pty.exit_status().is_none()
            {
                self.info.foreground_pid = metadata.foreground;
                self.info.process = metadata.process;
                if metadata.cwd.is_some() {
                    self.info.cwd = metadata.cwd;
                }
                if self.agent != metadata.agent {
                    self.working_since = None;
                }
                self.agent = metadata.agent;
            }
        }
        if !self.metadata_pending
            && self.metadata_at.elapsed() >= METADATA_INTERVAL
            && self.pty.exit_status().is_none()
        {
            self.metadata_at = Instant::now();
            self.metadata_pending = self
                .metadata_requests
                .try_send((self.pty.foreground_pid(), self.pty.shell_pid()))
                .is_ok();
        }
    }

    #[tracing::instrument(skip_all)]
    fn refresh_info(&mut self) {
        let title = self.terminal.title().unwrap_or_default().trim();
        self.info.title = if title.is_empty() {
            self.info.process.as_deref().unwrap_or("shell").to_string()
        } else {
            title
                .chars()
                .filter(|ch| !ch.is_control())
                .take(512)
                .collect()
        };
        self.info.exit_code = self.pty.exit_status();
        if !self.stopped
            && self.info.exit_code.is_some()
            && self.output.is_closed()
            && self.output.is_empty()
            && self.exit_checked_at.elapsed() >= METADATA_INTERVAL
        {
            self.exit_checked_at = Instant::now();
            self.stopped = self
                .pty
                .session_processes()
                .is_ok_and(|processes| processes.is_empty());
        }
        self.info.exited = self.stopped;
        self.info.agent = self.agent.map(|agent| agent.name().to_string());
        let activity = self.activity.status(Instant::now());
        if let Some(agent) = self.agent.filter(|_| !self.info.exited) {
            if activity == AgentStatus::Working {
                self.working_since.get_or_insert_with(Instant::now);
            } else if let Some(started) = self.working_since.take()
                && started.elapsed() >= AGENT_MIN_WORK
            {
                self.attention(Attention::AgentWaiting {
                    agent: agent.name().to_string(),
                    needs_input: activity == AgentStatus::NeedsInput,
                });
            }
        } else {
            self.working_since = None;
        }
        self.info.status = if self.info.exited {
            "exited"
        } else {
            match activity {
                AgentStatus::Working => "working",
                AgentStatus::NeedsInput => "needs_input",
                AgentStatus::Idle => "idle",
            }
        }
        .to_string();
        self.info.command_elapsed_ms = self
            .command_started
            .map(|started| started.elapsed().as_millis() as u64);
        if self.info.exited {
            self.info.foreground_pid = None;
            self.info.command_elapsed_ms = None;
        }
        let mut shared = self
            .shared
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if *shared != self.info {
            *shared = self.info.clone();
        }
    }

    fn handle(&mut self, request: Request) -> Result<Response> {
        match request {
            Request::Poll { revision, .. } => {
                let snapshot = self.capture()?;
                if revision == Some(snapshot.revision) {
                    Ok(Response::Unchanged)
                } else {
                    Ok(Response::Snapshot(snapshot))
                }
            }
            Request::Operate { operation, .. } => self.operate(operation),
            _ => bail!("unsupported session request"),
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.pty.try_write(bytes)?;
        if !bytes.is_empty() {
            self.terminal.scroll_viewport(ScrollViewport::Bottom);
            self.terminal.set_selection(None)?;
            self.activity.user_input(Instant::now());
        }
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn operate(&mut self, operation: Operation) -> Result<Response> {
        match operation {
            Operation::Input { bytes } => self.write_input(&bytes)?,
            Operation::Paste { text } => {
                ensure!(text.len() + 32 <= MAX_INPUT_BYTES, "paste exceeds 256 KiB");
                let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
                let mut data = text.into_bytes();
                let mut encoded = vec![0; data.len() + 32];
                let len = paste::encode(&mut data, bracketed, &mut encoded)?;
                self.write_input(&encoded[..len])?;
            }
            Operation::Key {
                key: code,
                action,
                mods,
                consumed_mods,
                unshifted,
                text,
            } => {
                let action =
                    key::Action::try_from(action).map_err(|_| anyhow!("invalid key action"))?;
                let key = key::Key::try_from(code).map_err(|_| anyhow!("invalid key code"))?;
                ensure!(
                    text.as_ref()
                        .is_none_or(|text| text.len() <= MAX_INPUT_BYTES),
                    "key text exceeds input limit"
                );
                let text = if action == key::Action::Release {
                    None
                } else {
                    text
                };
                self.key_event
                    .set_action(action)
                    .set_key(key)
                    .set_mods(key::Mods::from_bits_retain(mods))
                    .set_consumed_mods(key::Mods::from_bits_retain(consumed_mods))
                    .set_unshifted_codepoint(unshifted)
                    .set_utf8(text.clone());
                let mut bytes = Vec::with_capacity(16);
                self.key_encoder
                    .set_options_from_terminal(&self.terminal)
                    .encode_to_vec(&self.key_event, &mut bytes)?;
                // Keys the encoder has no mapping for still carry text the shell should receive.
                if bytes.is_empty()
                    && action != key::Action::Release
                    && mods & (key::Mods::CTRL | key::Mods::SUPER).bits() == 0
                    && let Some(text) = text
                {
                    bytes.extend_from_slice(text.as_bytes());
                }
                if action == key::Action::Release {
                    self.pty.try_write(&bytes)?;
                } else {
                    self.write_input(&bytes)?;
                }
            }
            Operation::Mouse {
                action,
                button,
                mods,
                x,
                y,
                width,
                height,
                padding,
                pressed,
            } => {
                ensure!(x.is_finite() && y.is_finite(), "invalid mouse position");
                let action =
                    mouse::Action::try_from(action).map_err(|_| anyhow!("invalid mouse action"))?;
                let button = button
                    .map(mouse::Button::try_from)
                    .transpose()
                    .map_err(|_| anyhow!("invalid mouse button"))?;
                self.mouse_event
                    .set_action(action)
                    .set_button(button)
                    .set_mods(key::Mods::from_bits_retain(mods))
                    .set_position(mouse::Position { x, y });
                let dimensions = self.dimensions.get();
                let mut bytes = Vec::with_capacity(16);
                self.mouse_encoder
                    .set_options_from_terminal(&self.terminal)
                    .set_size(mouse::EncoderSize {
                        screen_width: width,
                        screen_height: height,
                        cell_width: u32::from(dimensions.cell_width),
                        cell_height: u32::from(dimensions.cell_height),
                        padding_top: padding,
                        padding_bottom: padding,
                        padding_left: padding,
                        padding_right: padding,
                    })
                    .set_any_button_pressed(pressed)
                    .set_track_last_cell(true)
                    .encode_to_vec(&self.mouse_event, &mut bytes)?;
                self.pty.try_write(&bytes)?;
            }
            Operation::Resize { dimensions } => {
                validate_dimensions(dimensions)?;
                self.terminal.resize(
                    dimensions.cols,
                    dimensions.rows,
                    u32::from(dimensions.cell_width),
                    u32::from(dimensions.cell_height),
                )?;
                let previous = self.dimensions.replace(dimensions);
                self.pty.resize(pty_dimensions(dimensions));
                if let Some(blocks) = &mut self.blocks {
                    blocks.terminal_resized();
                }
                if previous.cols != dimensions.cols {
                    self.rebuild_due = Some(Instant::now() + REBUILD_DELAY);
                }
            }
            Operation::Scroll { delta } => {
                let delta = delta.clamp(-10_000, 10_000);
                if self.terminal.active_screen()? == libghostty_vt::screen::Screen::Alternate
                    && self.terminal.mode(Mode::ALT_SCROLL)?
                {
                    let application = self.terminal.mode(Mode::DECCKM)?;
                    let arrow: &[u8] = match (delta < 0, application) {
                        (true, true) => b"\x1bOA",
                        (true, false) => b"\x1b[A",
                        (false, true) => b"\x1bOB",
                        (false, false) => b"\x1b[B",
                    };
                    self.pty.try_write(&arrow.repeat(delta.unsigned_abs()))?;
                } else {
                    self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
                }
            }
            Operation::SelectionPress {
                col,
                row,
                x,
                y,
                cell_width,
            } => {
                ensure!(
                    x.is_finite() && y.is_finite() && cell_width.is_finite() && cell_width > 0.,
                    "invalid selection position"
                );
                let grid = self
                    .terminal
                    .grid_ref(Point::Viewport(PointCoordinate { x: col, y: row }))?;
                let selection = self
                    .press
                    .set_repeat_distance(cell_width)?
                    .set_time(self.started.elapsed())?
                    .set_position(x, y)?
                    .apply(&mut self.gesture, &self.terminal, grid)?;
                self.terminal.set_selection(selection.as_ref())?;
            }
            Operation::SelectionDrag {
                col,
                row,
                x,
                y,
                rectangle,
                cell_width,
                padding,
                height,
            } => {
                ensure!(
                    x.is_finite() && y.is_finite() && cell_width > 0,
                    "invalid selection position"
                );
                let grid = self
                    .terminal
                    .grid_ref(Point::Viewport(PointCoordinate { x: col, y: row }))?;
                let selection = self
                    .drag
                    .set_rectangle(rectangle)?
                    .set_position(x, y)?
                    .apply(
                        &mut self.gesture,
                        &self.terminal,
                        grid,
                        Geometry {
                            columns: u32::from(self.dimensions.get().cols),
                            cell_width,
                            padding_left: padding,
                            screen_height: height,
                        },
                    )?;
                self.terminal.set_selection(selection.as_ref())?;
            }
            Operation::SelectionRelease { col, row } => {
                let grid = self
                    .terminal
                    .grid_ref(Point::Viewport(PointCoordinate { x: col, y: row }))
                    .ok();
                self.release
                    .apply(&mut self.gesture, &self.terminal, grid)?;
            }
            Operation::Copy => {
                let options = FormatOptions::new()
                    .with_emit_format(Format::Plain)
                    .with_trim(true)
                    .with_unwrap(true);
                let Some(bytes) = self.terminal.format_selection_alloc(None, options)? else {
                    return Ok(Response::Done);
                };
                ensure!(
                    bytes.len() <= MAX_TEXT_BYTES,
                    "selection is too large to copy; select a smaller range"
                );
                return Ok(Response::Text(String::from_utf8_lossy(&bytes).into_owned()));
            }
            Operation::SelectAll => {
                let selection = self.terminal.select_all()?;
                self.terminal.set_selection(selection.as_ref())?;
            }
            Operation::ClearSelection => {
                self.terminal.set_selection(None)?;
            }
            Operation::ClearScrollback => {
                if let Some(blocks) = &mut self.blocks {
                    blocks.clear();
                }
                // Erase scrollback (CSI 3 J), then ask the shell to redraw its prompt.
                self.terminal.vt_write(b"\x1b[3J");
                if self.pty.exit_status().is_none() {
                    self.write_input(b"\x0c")?;
                }
            }
            Operation::Search { query } => {
                ensure!(query.len() <= 4096, "search query is too long");
                let text = search::screen_text(&self.terminal)?;
                let matches = search::find_matches(&text, &query)
                    .into_iter()
                    .take(MAX_SEARCH_MATCHES)
                    .map(|found| Match {
                        row: found.row,
                        start_col: found.start_col,
                        end_col: found.end_col,
                    })
                    .collect();
                return Ok(Response::Matches { query, matches });
            }
            Operation::SelectMatch { found } => {
                let start = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                    x: found.start_col,
                    y: found.row,
                }))?;
                let end = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                    x: found.end_col.saturating_sub(1),
                    y: found.row,
                }))?;
                self.terminal
                    .set_selection(Some(&Selection::new(start, end, false)))?;
                self.terminal.scroll_viewport(ScrollViewport::Row(
                    found
                        .row
                        .saturating_sub(u32::from(self.dimensions.get().rows) / 2)
                        as usize,
                ));
            }
            Operation::Prompt { forward } => self.jump_to_prompt(forward)?,
            Operation::Colors { colors } => {
                configure_colors(&mut self.terminal, &colors)?;
                self.dark.set(colors.dark);
                let changed = self.colors != colors;
                self.colors = colors;
                if changed {
                    self.rebuild_due = Some(Instant::now() + REBUILD_DELAY);
                }
            }
            Operation::RunCommand { command } => self.type_into_shell(command, b"\r")?,
            Operation::Complete { text } => {
                self.type_into_shell(text, shell_integration::COMPLETE_KEY)?
            }
            Operation::BlockRows { block, from } => {
                let (version, rows) = self
                    .blocks
                    .as_ref()
                    .and_then(|blocks| blocks.rows(block, from))
                    .ok_or_else(|| anyhow!("no block {block}"))?;
                return Ok(Response::BlockRows {
                    block,
                    version,
                    from,
                    rows,
                });
            }
            Operation::Shell => return Ok(Response::Shell(Box::new(self.shell.clone()))),
            Operation::Read { lines } => {
                let mut text = self
                    .blocks
                    .as_ref()
                    .map(|blocks| blocks.text(lines.min(MAX_READ_LINES)))
                    .unwrap_or_default();
                text.push_str(&search::screen_text(&self.terminal)?);
                let lines: Vec<&str> = text
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| !line.is_empty())
                    .rev()
                    .take(lines.min(MAX_READ_LINES))
                    .collect();
                let text = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
                ensure!(
                    text.len() <= MAX_TEXT_BYTES,
                    "output is too large; request fewer lines"
                );
                return Ok(Response::Text(text));
            }
            Operation::AcknowledgeAttention { serial } => {
                if serial == self.info.attention_serial {
                    self.info.attention = None;
                }
            }
            Operation::Stop => self.stop()?,
        }
        Ok(Response::Done)
    }

    fn jump_to_prompt(&mut self, forward: bool) -> Result<()> {
        let top = self.terminal.scrollbar()?.offset as u32;
        let total = self.terminal.total_rows()? as u32;
        let rows: Box<dyn Iterator<Item = u32>> = if forward {
            Box::new(top.saturating_add(1)..total)
        } else {
            Box::new((0..top).rev())
        };
        for row in rows {
            let prompt = self
                .terminal
                .grid_ref(Point::Screen(PointCoordinate { x: 0, y: row }))
                .and_then(|grid| grid.row())
                .and_then(|row| row.semantic_prompt());
            if prompt.is_ok_and(|prompt| prompt == RowSemanticPrompt::Prompt) {
                self.terminal
                    .scroll_viewport(ScrollViewport::Row(row as usize));
                return Ok(());
            }
        }
        if forward {
            self.terminal.scroll_viewport(ScrollViewport::Bottom);
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if self.info.exited && self.pty.session_processes()?.is_empty() {
            return Ok(());
        }
        self.startup = None;
        self.pty.signal_session(libc::SIGTERM)?;
        let started = Instant::now();
        let mut escalated = false;
        while started.elapsed() < Duration::from_secs(2) {
            self.consume_output();
            self.refresh_info();
            if self.pty.exit_status().is_some() && self.pty.session_processes()?.is_empty() {
                self.stopped = true;
                self.refresh_info();
                return Ok(());
            }
            if !escalated && started.elapsed() >= Duration::from_millis(500) {
                self.pty.signal_session(libc::SIGKILL)?;
                escalated = true;
            }
            thread::sleep(IDLE_INTERVAL);
        }
        bail!(
            "session did not stop (exit {:?}, queued {}, output closed {}, processes {:?})",
            self.pty.exit_status(),
            self.output.len(),
            self.output.is_closed(),
            self.pty.session_processes()?
        )
    }

    #[tracing::instrument(skip_all)]
    fn capture(&mut self) -> Result<Snapshot> {
        self.refresh_info();
        let full_screen = runtime_blocks::is_full_screen(&self.terminal);
        if let Some(blocks) = &mut self.blocks
            && !full_screen
        {
            blocks.capture_scrollback(&mut self.terminal)?;
        }
        let blocks = self.blocks.as_ref().map(|blocks| {
            Box::new(Blocks {
                items: blocks.summaries(),
                prompt: self.prompt.clone(),
                shell_serial: self.shell_serial,
                completions: self.completions.clone(),
                full_screen,
            })
        });
        let replaced = std::mem::take(&mut self.terminal_replaced);
        let frame = self.render.update(&self.terminal)?;
        let colors = frame.colors()?;
        let dimensions = self.dimensions.get();
        let full = replaced
            || frame.dirty()? == Dirty::Full
            || self.snapshot.as_ref().is_none_or(|snapshot| {
                snapshot.dimensions != dimensions
                    || snapshot.background != rgb_bytes(colors.background)
            });
        let cursor_position = frame.cursor_viewport()?;
        let cursor = Cursor {
            col: cursor_position.map_or(0, |cursor| {
                cursor.x.saturating_sub(u16::from(cursor.at_wide_tail))
            }),
            row: cursor_position.map_or(0, |cursor| cursor.y),
            visible: frame.cursor_visible()? && cursor_position.is_some(),
            style: match frame.cursor_visual_style()? {
                CursorVisualStyle::BlockHollow => 0,
                CursorVisualStyle::Bar => 6,
                CursorVisualStyle::Underline => 4,
                _ => 2,
            },
            color: rgb_bytes(colors.cursor.unwrap_or(colors.foreground)),
        };
        let palette = self.terminal.color_palette()?;
        frame.set_dirty(Dirty::Full)?;
        let mut rows = self.render_rows.update(&frame)?;
        let mut encoded_rows = Vec::with_capacity(usize::from(dimensions.rows));
        let mut row_index = 0u32;
        let mut encoded_bytes = 0usize;
        let mut graphemes = String::new();
        let mut link_buffer = [0u8; 2048];
        while let Some(row) = rows.next() {
            if !full
                && !row.dirty()?
                && let Some(cached) = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.rows.get(row_index as usize))
            {
                encoded_bytes += cached.len();
                ensure!(
                    encoded_bytes <= MAX_TEXT_BYTES,
                    "viewport is too large to display; reduce the window size"
                );
                encoded_rows.push(cached.clone());
                row_index += 1;
                continue;
            }
            let encoded = encode_row(
                row,
                &mut self.render_cells,
                &colors,
                &palette,
                &self.terminal,
                row_index,
                &mut graphemes,
                &mut link_buffer,
            )?;
            ensure!(
                encoded_bytes + encoded.len() <= MAX_TEXT_BYTES,
                "viewport is too large to display; reduce the window size"
            );
            encoded_bytes += encoded.len();
            row.set_dirty(false)?;
            encoded_rows.push(encoded);
            row_index += 1;
        }
        frame.set_dirty(Dirty::Clean)?;
        let mouse_tracking = self.terminal.is_mouse_tracking()?;
        let scroll_offset = self.terminal.scrollbar()?.offset as usize;
        let background = rgb_bytes(colors.background);
        let changed = self.snapshot.as_ref().is_none_or(|snapshot| {
            snapshot.rows != encoded_rows
                || snapshot.cursor != cursor
                || snapshot.dimensions != dimensions
                || snapshot.background != background
                || snapshot.info != self.info
                || snapshot.mouse_tracking != mouse_tracking
                || snapshot.scroll_offset != scroll_offset
                || snapshot.blocks != blocks
        });
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        let snapshot = Snapshot {
            revision: self.revision,
            info: self.info.clone(),
            dimensions,
            rows: encoded_rows,
            cursor,
            background,
            mouse_tracking,
            scroll_offset,
            blocks,
        };
        self.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }
}

/// One row of the terminal as self-contained styled VT text: explicit
/// colors and attributes per run, hyperlinks limited to safe schemes, and
/// no other escape sequences.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_row(
    row: &RowIteration<'static, '_>,
    render_cells: &mut CellIterator<'static>,
    colors: &render::Colors,
    palette: &Palette,
    terminal: &Terminal<'static, 'static>,
    row_index: u32,
    graphemes: &mut String,
    link_buffer: &mut [u8],
) -> Result<String> {
    let selection = row.selection()?;
    let mut cells = render_cells.update(row)?;
    let mut encoded = String::from("\x1b[0m\x1b]8;;\x1b\\");
    let mut previous_style = None;
    let mut previous_link: Option<String> = None;
    let mut column = 0u16;
    while let Some(cell) = cells.next() {
        let raw = cell.raw_cell()?;
        let wide = raw.wide()?;
        if wide == CellWide::SpacerTail {
            column += 1;
            continue;
        }
        let mut style = if cell.has_styling()? {
            cell.style()?
        } else {
            Style::default()
        };
        let mut foreground = cell.fg_color()?.unwrap_or(colors.foreground);
        let mut background = cell.bg_color()?.unwrap_or(colors.background);
        if style.inverse {
            std::mem::swap(&mut foreground, &mut background);
            style.inverse = false;
        }
        if selection.is_some_and(|range| column >= range.start_x && column <= range.end_x) {
            std::mem::swap(&mut foreground, &mut background);
        }
        let underline = match style.underline_color {
            StyleColor::Rgb(color) => Some(color),
            StyleColor::Palette(index) => Some(palette.0[usize::from(index.0)]),
            StyleColor::None => None,
        };
        let signature = (style, foreground, background, underline);
        if previous_style != Some(signature) {
            append_style(&mut encoded, style, foreground, background, underline);
            previous_style = Some(signature);
        }
        let link = if raw.has_hyperlink()? {
            terminal
                .grid_ref(Point::Viewport(PointCoordinate {
                    x: column,
                    y: row_index,
                }))
                .ok()
                .and_then(|grid| grid.hyperlink_uri(link_buffer).ok())
                .filter(|length| *length > 0)
                .and_then(|length| std::str::from_utf8(&link_buffer[..length]).ok())
                .filter(|uri| safe_hyperlink(uri))
                .map(str::to_owned)
        } else {
            None
        };
        if link != previous_link {
            encoded.push_str("\x1b]8;;");
            if let Some(link) = &link {
                encoded.push_str(link);
            }
            encoded.push_str("\x1b\\");
            previous_link = link;
        }
        if cell.graphemes_len()? > 0 && wide != CellWide::SpacerHead {
            cell.graphemes_utf8(graphemes)?;
            for character in graphemes.chars() {
                if !character.is_control() {
                    encoded.push(character);
                }
            }
        } else {
            encoded.push(' ');
        }
        column += 1;
        ensure!(
            encoded.len() <= MAX_TEXT_BYTES,
            "a row is too large to display"
        );
    }
    encoded.push_str("\x1b]8;;\x1b\\\x1b[0m");
    Ok(encoded)
}

fn safe_hyperlink(uri: &str) -> bool {
    !uri.chars().any(char::is_control)
        && ["https://", "http://", "ftp://", "file://", "mailto:"]
            .iter()
            .any(|scheme| uri.starts_with(scheme))
}

fn append_style(
    encoded: &mut String,
    style: Style,
    foreground: RgbColor,
    background: RgbColor,
    underline: Option<RgbColor>,
) {
    let _ = write!(
        encoded,
        "\x1b[0;38;2;{};{};{};48;2;{};{};{}",
        foreground.r, foreground.g, foreground.b, background.r, background.g, background.b
    );
    for (enabled, code) in [
        (style.bold, 1),
        (style.faint, 2),
        (style.italic, 3),
        (style.blink, 5),
        (style.invisible, 8),
        (style.strikethrough, 9),
        (style.overline, 53),
    ] {
        if enabled {
            let _ = write!(encoded, ";{code}");
        }
    }
    if style.underline != Underline::None {
        let _ = write!(encoded, ";4:{}", style.underline as u32);
    }
    if let Some(color) = underline {
        let _ = write!(encoded, ";58:2::{}:{}:{}", color.r, color.g, color.b);
    }
    encoded.push('m');
}

fn rgb_bytes(color: RgbColor) -> [u8; 3] {
    [color.r, color.g, color.b]
}
fn rgb(value: [u8; 3]) -> RgbColor {
    RgbColor {
        r: value[0],
        g: value[1],
        b: value[2],
    }
}

fn configure_colors(terminal: &mut Terminal<'static, 'static>, colors: &Colors) -> Result<()> {
    let mut palette = terminal.default_color_palette()?;
    for (target, color) in palette.0[..16].iter_mut().zip(colors.palette) {
        *target = rgb(color);
    }
    terminal
        .set_default_fg_color(Some(rgb(colors.foreground)))?
        .set_default_bg_color(Some(rgb(colors.background)))?
        .set_default_cursor_color(Some(rgb(colors.cursor)))?
        .set_default_color_palette(Some(palette))?;
    Ok(())
}

/// Install callbacks that answer terminal queries even with no attached views.
fn register_effects(
    terminal: &mut Terminal<'static, 'static>,
    pty: &Pty,
    dimensions: Rc<Cell<Dimensions>>,
    dark: Rc<Cell<bool>>,
    bell: Rc<Cell<bool>>,
    reply_error: Rc<Cell<bool>>,
) -> Result<()> {
    let replies = pty.input_sender();
    terminal
        .on_bell(move |_| bell.set(true))?
        .on_pty_write(move |_, data| {
            if replies.try_send(data.to_vec()).is_err() {
                reply_error.set(true);
            }
        })?
        .on_size(move |_| {
            let current = dimensions.get();
            Some(SizeReportSize {
                rows: current.rows,
                columns: current.cols,
                cell_width: u32::from(current.cell_width),
                cell_height: u32::from(current.cell_height),
            })
        })?
        .on_device_attributes(|_| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[
                        DeviceAttributeFeature::COLUMNS_132,
                        DeviceAttributeFeature::SELECTIVE_ERASE,
                        DeviceAttributeFeature::ANSI_COLOR,
                    ],
                ),
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                tertiary: Default::default(),
            })
        })?
        .on_xtversion(|_| {
            Some(concat!(
                env!("CARGO_PKG_NAME"),
                " ",
                env!("CARGO_PKG_VERSION")
            ))
        })?
        .on_color_scheme(move |_| {
            Some(if dark.get() {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            })
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::*;
    use crate::shell_integration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(8);

    fn launch(script: &str) -> Launch {
        Launch {
            id: 51,
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            cwd: Some(std::env::temp_dir()),
            startup: None,
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

    struct Session(SessionHandle);

    impl Drop for Session {
        fn drop(&mut self) {
            let _ = self.0.request(Request::Operate {
                session: self.0.id,
                operation: Operation::Stop,
            });
        }
    }

    impl Session {
        fn new(script: &str) -> Self {
            Self(spawn(51, launch(script)).unwrap())
        }

        fn operate(&self, operation: Operation) -> Response {
            self.0
                .request(Request::Operate {
                    session: self.0.id,
                    operation,
                })
                .unwrap()
        }

        fn snapshot(&self) -> Snapshot {
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

        fn text(&self) -> String {
            match self.operate(Operation::Read {
                lines: MAX_READ_LINES,
            }) {
                Response::Text(text) => text,
                response => panic!("expected text: {response:?}"),
            }
        }

        fn wait_text(&self, expected: &str) {
            wait_until(|| self.text().contains(expected));
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !predicate() {
            assert!(Instant::now() < deadline, "session condition timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn display_text(snapshot: &Snapshot) -> String {
        let mut mirror = Terminal::new(Options {
            cols: snapshot.dimensions.cols,
            rows: snapshot.dimensions.rows,
            max_scrollback: 0,
        })
        .unwrap();
        crate::runtime_display::DisplayState::default()
            .apply(&mut mirror, snapshot)
            .unwrap();
        search::screen_text(&mirror).unwrap()
    }

    struct ShellHome(PathBuf);

    impl Drop for ShellHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A launch of the default login shell (zsh) living in `home`, with the
    /// app's shell integration if asked.
    fn zsh_launch(home: &ShellHome, integration: bool) -> Launch {
        let mut command = shell_integration::shell_command(None);
        assert!(command.is_default_prog());
        command.env_clear();
        command.env("SHELL", "/bin/zsh");
        command.env("HOME", &home.0);
        command.env("PATH", "/usr/bin:/bin");
        if integration {
            let dir = home.0.join("integration");
            fs::create_dir_all(dir.join("zsh")).unwrap();
            fs::write(
                dir.join("zsh/.zshenv"),
                include_str!("../assets/shell-integration/zsh/.zshenv"),
            )
            .unwrap();
            fs::write(
                dir.join("zsh/integration.zsh"),
                include_str!("../assets/shell-integration/zsh/integration.zsh"),
            )
            .unwrap();
            command.env("ZDOTDIR", dir.join("zsh"));
            command.env("TERMINAL_SHELL_INTEGRATION_DIR", dir);
        }
        let mut request = launch("");
        request.argv = command
            .get_argv()
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect();
        request.env = command
            .iter_full_env_as_str()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        request.cwd = Some(home.0.clone());
        request
    }

    fn assert_default_login_shell(integration: bool) {
        let home = ShellHome(std::env::temp_dir().join(format!(
            "terminal-login-{}",
            crate::runtime::new_session_id().unwrap()
        )));
        fs::create_dir(&home.0).unwrap();
        fs::write(home.0.join(".zshenv"), "export TERMINAL_USER_ENV=loaded\n").unwrap();
        fs::write(
            home.0.join(".zprofile"),
            "export TERMINAL_LOGIN_PROFILE=loaded\n",
        )
        .unwrap();
        fs::write(
            home.0.join(".zshrc"),
            if integration {
                "stty -echo\n"
            } else {
                "stty -echo\nprintf '\\033]133;A\\007'\n"
            },
        )
        .unwrap();

        let mut request = zsh_launch(&home, integration);
        request.startup = Some("printf 'DEFAULT:%s:%s:%s:%s\\n' \"$options[login]\" \"$options[interactive]\" \"$TERMINAL_LOGIN_PROFILE\" \"$TERMINAL_USER_ENV\"; exit 23".into());
        let request = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        let session = Session(spawn(51, request).unwrap());
        wait_until(|| session.0.info().exited);
        assert_eq!(session.0.info().exit_code, Some(23));
        assert_eq!(
            session.0.info().cwd.unwrap().canonicalize().unwrap(),
            home.0.canonicalize().unwrap()
        );
        let text = session.text();
        assert!(text.contains("DEFAULT:on:on:loaded:loaded"), "{text:?}");
    }

    #[test]
    fn default_shell_launch_preserves_login_environment_and_startup() {
        assert_default_login_shell(false);
    }

    #[test]
    fn default_shell_launch_preserves_zsh_integration() {
        assert_default_login_shell(true);
    }

    #[test]
    #[ignore = "prints the cost of each step of a window resize; run with --release"]
    fn resize_benchmark() {
        // A dense, colored screen with scrollback, as after a build log.
        let session = Session::new(
            "awk 'BEGIN { for (i = 0; i < 3000; i++) { printf \"\\033[3%dm\", i % 7 + 1; \
             for (j = 0; j < 190; j++) printf \"%c\", 65 + (i + j) % 26; printf \"\\033[0m\\n\" } }'; \
             sleep 30",
        );
        let dimensions = |cols| Dimensions {
            cols,
            rows: 60,
            cell_width: 8,
            cell_height: 16,
        };
        session.operate(Operation::Resize {
            dimensions: dimensions(200),
        });
        wait_until(|| session.snapshot().rows.iter().any(|row| row.contains("Z")));
        thread::sleep(Duration::from_millis(500));

        let mut display = Terminal::new(Options {
            cols: 200,
            rows: 60,
            max_scrollback: 0,
        })
        .unwrap();
        let mut mirror = crate::runtime_display::DisplayState::default();
        let mut renderer = crate::grid::GridRenderer::new().unwrap();
        let (mut resize, mut snapshot, mut apply, mut frame) = (
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let steps = 40;
        for step in 0..steps {
            let started = Instant::now();
            session.operate(Operation::Resize {
                dimensions: dimensions(160 + (step % 40) as u16),
            });
            resize += started.elapsed();
            let started = Instant::now();
            let captured = session.snapshot();
            snapshot += started.elapsed();
            let started = Instant::now();
            mirror.apply(&mut display, &captured).unwrap();
            apply += started.elapsed();
            let started = Instant::now();
            renderer.build_frame(&display, true).unwrap();
            frame += started.elapsed();
        }
        let per = |total: Duration| total / steps;
        eprintln!(
            "per resize step: resize {:?}, snapshot {:?}, mirror {:?}, grid {:?}",
            per(resize),
            per(snapshot),
            per(apply),
            per(frame)
        );
    }

    #[test]
    fn command_blocks_hold_each_command_and_its_output() {
        let home = ShellHome(std::env::temp_dir().join(format!(
            "terminal-blocks-{}",
            crate::runtime::new_session_id().unwrap()
        )));
        fs::create_dir(&home.0).unwrap();
        fs::write(home.0.join(".zshrc"), "echo welcome\n").unwrap();
        let mut request = zsh_launch(&home, true);
        request.command_blocks = true;
        let session = Session(spawn(52, request).unwrap());
        let blocks = || session.snapshot().blocks;
        wait_until(|| blocks().is_some_and(|blocks| blocks.prompt.is_some()));
        session.operate(Operation::RunCommand {
            command: "printf 'one\\ntwo\\n'; false".into(),
        });
        wait_until(|| {
            blocks().is_some_and(|blocks| {
                blocks
                    .items
                    .iter()
                    .any(|item| item.command.starts_with("printf") && !item.running)
            })
        });
        let reported = blocks().unwrap();
        // Startup output is a block of its own, without a command.
        assert_eq!(reported.items[0].command, "");
        let block = reported.items.last().unwrap();
        assert_eq!(block.exit_code, Some(1));
        let Response::BlockRows { rows, .. } = session.operate(Operation::BlockRows {
            block: block.id,
            from: 0,
        }) else {
            panic!("expected block rows");
        };
        let text: Vec<String> = rows
            .iter()
            .map(|row| {
                crate::runtime_blocks::plain_text(row)
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(text, vec!["one", "two"]);
        assert!(session.text().contains("$ printf"));
    }

    #[test]
    fn detached_clients_keep_the_same_process_and_receive_output() {
        let session = Session::new(
            "stty -echo; printf 'ready\\n'; read first; sleep 0.05; printf 'output while detached\\n'; read last",
        );
        session.wait_text("ready");
        let client = session.0.clone();
        let pid = client.info().shell_pid.unwrap();
        drop(client);
        session.operate(Operation::Input {
            bytes: b"go\n".to_vec(),
        });
        thread::sleep(Duration::from_millis(100));
        let reattached = session.0.clone();
        assert_eq!(reattached.info().shell_pid, Some(pid));
        assert!(session.text().contains("output while detached"));
        assert!(display_text(&session.snapshot()).contains("output while detached"));
        session.operate(Operation::Input {
            bytes: b"done\n".to_vec(),
        });
        wait_until(|| session.0.info().exited);
        assert_eq!(session.0.info().exit_code, Some(0));
        assert!(session.text().contains("output while detached"));
    }

    #[test]
    fn reconnect_preserves_both_screens_and_incomplete_escape_sequences() {
        let session = Session::new(
            "stty -echo; printf 'primary history\\r\\n'; read first; printf '\x1b[?1049h\x1b[2J\x1b[HALTERNATE'; read second; printf '\x1b[?1049l\x1b['; sleep 0.05; printf '32mAFTER\x1b[0m\\r\\n'; read last",
        );
        session.wait_text("primary history");
        session.operate(Operation::Input {
            bytes: b"first\n".to_vec(),
        });
        session.wait_text("ALTERNATE");
        assert!(display_text(&session.snapshot()).contains("ALTERNATE"));
        session.operate(Operation::Input {
            bytes: b"second\n".to_vec(),
        });
        session.wait_text("AFTER");
        let frame = session.snapshot();
        let visible = display_text(&frame);
        assert!(visible.contains("primary history"), "{visible:?}");
        assert!(visible.contains("AFTER"), "{visible:?}");
        assert!(!visible.contains("ALTERNATE"), "{visible:?}");
    }

    #[test]
    fn device_queries_are_answered_without_polling_or_a_gui() {
        let session = Session::new(
            "stty -echo -icanon min 1 time 0; printf '\x1b[6n'; reply=$(dd bs=1 count=6 2>/dev/null | od -An -tx1); printf 'REPLY:%s\\n' \"$reply\"; exit 9",
        );
        wait_until(|| session.0.info().exited);
        assert_eq!(session.0.info().exit_code, Some(9));
        let text = session.text();
        assert!(
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("1b 5b 31 3b 31 52"),
            "{text:?}"
        );
    }

    #[test]
    fn startup_runs_on_a_prompt_without_an_attached_client() {
        let mut launch = launch(
            "stty -echo; printf '\x1b]133;A\x07'; read command; printf 'START:%s\\n' \"$command\"",
        );
        launch.startup = Some("agent task".into());
        let session = Session(spawn(52, launch).unwrap());
        wait_until(|| session.0.info().exited);
        assert!(session.text().contains("START:agent task"));
    }

    #[test]
    fn stop_escalates_and_retains_output() {
        let session =
            Session::new("trap '' TERM HUP; printf 'ready\\n'; while :; do read value; done");
        session.wait_text("ready");
        let pid = session.0.info().shell_pid.unwrap();
        session.operate(Operation::Stop);
        assert!(session.0.info().exited);
        // SAFETY: signal zero only checks whether the reaped process still exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(session.text().contains("ready"));
    }

    #[test]
    fn stop_includes_background_jobs_in_other_process_groups() {
        let session = Session::new(
            "set -m; (trap '' TERM HUP; while :; do sleep 1; done) >/dev/null 2>&1 & printf 'CHILD:%s\\n' \"$!\"; read value",
        );
        session.wait_text("CHILD:");
        let child: i32 = session
            .text()
            .split("CHILD:")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        session.operate(Operation::Stop);
        // SAFETY: signal zero does not deliver a signal to the process.
        assert_eq!(unsafe { libc::kill(child, 0) }, -1);
        assert!(session.0.info().exited);
    }

    #[test]
    fn continuously_printing_session_does_not_starve_other_sessions() {
        let flood = Session::new("exec yes flood");
        let other = Session::new(
            "stty -echo; printf 'ready\\n'; read value; printf 'REPLY:%s\\n' \"$value\"; read last",
        );
        other.wait_text("ready");
        let began = Instant::now();
        other.operate(Operation::Input {
            bytes: b"responsive\n".to_vec(),
        });
        other.wait_text("REPLY:responsive");
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(flood.snapshot().rows.len(), 8);
        flood.operate(Operation::Stop);
    }

    #[test]
    fn selection_search_and_scrolling_use_authoritative_scrollback() {
        let session = Session::new(
            "stty -echo; i=0; while [ \"$i\" -lt 30 ]; do printf 'history-%s\\r\\n' \"$i\"; i=$((i+1)); done; read value",
        );
        session.wait_text("history-29");
        let matches = match session.operate(Operation::Search {
            query: "history-3".into(),
        }) {
            Response::Matches { matches, .. } => matches,
            other => panic!("unexpected response: {other:?}"),
        };
        assert_eq!(matches.len(), 1);
        session.operate(Operation::SelectMatch { found: matches[0] });
        let frame = session.snapshot();
        assert!(display_text(&frame).contains("history-3"));
        assert!(
            matches!(session.operate(Operation::Copy), Response::Text(text) if text == "history-3")
        );
        session.operate(Operation::ClearSelection);
        assert!(matches!(session.operate(Operation::Copy), Response::Done));
        session.operate(Operation::Scroll { delta: 10_000 });
        assert!(session.snapshot().scroll_offset > frame.scroll_offset);
    }

    #[test]
    fn snapshots_have_stable_revisions_and_no_replayed_terminal_effects() {
        let session = Session::new(
            "stty -echo; printf '\x1b]777;notify;Notice;needs input\x07\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\'; read value",
        );
        session.wait_text("click");
        let first = session.snapshot();
        assert!(
            first
                .rows
                .iter()
                .any(|row| row.contains("https://example.com"))
        );
        assert!(first.rows.iter().all(|row| !row.contains("777;notify")));
        let again = session.snapshot();
        assert_eq!(first.info.attention_serial, again.info.attention_serial);
        let response = session
            .0
            .request(Request::Poll {
                session: session.0.id,
                revision: Some(again.revision),
            })
            .unwrap();
        assert!(matches!(response, Response::Unchanged));
    }

    #[test]
    fn natural_shell_exit_keeps_redirected_background_jobs_active() {
        let session = Session::new(
            "trap '' TERM HUP; set -m; (exec sleep 60) >/dev/null 2>&1 & printf 'CHILD:%s\\n' \"$!\"; exit 0",
        );
        session.wait_text("CHILD:");
        wait_until(|| session.0.info().exit_code.is_some());
        thread::sleep(METADATA_INTERVAL + Duration::from_millis(50));
        assert!(!session.0.info().exited);
        let text = session.text();
        let child: i32 = text
            .split("CHILD:")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        session.operate(Operation::Stop);
        assert!(session.0.info().exited);
        // SAFETY: signal zero only checks process existence.
        assert_eq!(unsafe { libc::kill(child, 0) }, -1);
        session.operate(Operation::Stop);
    }

    #[test]
    fn stale_attention_acknowledgement_preserves_a_later_event() {
        let session = Session::new(
            "stty -echo; printf '\x1b]777;notify;First;one\x07ready\\n'; read next; printf '\x1b]777;notify;Second;two\x07second\\n'; read last",
        );
        session.wait_text("ready");
        let first = session.0.info().attention_serial;
        session.operate(Operation::Input {
            bytes: b"next\n".to_vec(),
        });
        session.wait_text("second");
        let second = session.0.info().attention_serial;
        assert!(second > first);
        session.operate(Operation::AcknowledgeAttention { serial: first });
        assert!(session.0.info().attention.is_some());
        session.operate(Operation::AcknowledgeAttention { serial: second });
        assert!(session.0.info().attention.is_none());
    }

    #[test]
    fn environment_is_explicit_and_pane_identity_is_authoritative() {
        let session =
            Session::new("printf '%s|%s' \"$TERMINAL_PANE_ID\" \"$TERMINAL_CONTROL_TOKEN\"");
        wait_until(|| session.0.info().exited);
        assert!(session.text().starts_with("51|"));
    }

    #[test]
    fn oversized_input_and_dimensions_are_rejected() {
        let session = Session::new("read value");
        assert!(
            session
                .0
                .request(Request::Operate {
                    session: session.0.id,
                    operation: Operation::Input {
                        bytes: vec![b'x'; MAX_INPUT_BYTES + 1]
                    }
                })
                .is_err()
        );
        assert!(
            validate_dimensions(Dimensions {
                cols: u16::MAX,
                rows: u16::MAX,
                cell_width: 8,
                cell_height: 16
            })
            .is_err()
        );
    }
}
