use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::Result;
use gpui::{
    AnyElement, App, AsyncApp, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity,
    EventEmitter, FocusHandle, Focusable, FollowMode, KeyBinding, KeyDownEvent, KeyUpEvent,
    ListAlignment, ListState, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta, ScrollWheelEvent, SharedString,
    Subscription, Task, WeakEntity, Window, actions, canvas, div, fill, prelude::*, px,
};
use libghostty_vt::{
    Terminal,
    fmt::Format,
    key, mouse, paste,
    screen::{CellWide, RowSemanticPrompt},
    selection::{
        FormatOptions, Selection,
        gesture::{DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent},
    },
    style::Palette,
    terminal::{
        ColorScheme, ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
        Options, Point, PointCoordinate, PrimaryDeviceAttributes, ScrollViewport,
        SecondaryDeviceAttributes, SizeReportSize,
    },
};

use crate::{
    agents::{self, Activity, Agent, AgentStatus},
    block_view::{self, Item, ItemContent},
    blocks::{self, BlockContext, BlockList},
    command_editor::{CommandEditor, CommandEditorEvent},
    components::{elevated_shadow, icon, icon_button},
    control,
    git::{self, DiffStats},
    grid::{CellMetrics, Frame, GridRenderer},
    highlight::{self, CommandIndex, TokenKind},
    history::{self, History},
    hooks::{Bootstrapped, Hook, Precmd},
    input::{to_mods, translate_keystroke},
    links::{self, LinkTarget},
    osc::{OscEvent, OscScanner, Piece},
    output, process_info,
    pty::{Pty, PtyDimensions},
    search::{self, SearchMatch},
    settings::SettingsStore,
    shell_integration::{self, ShellIntegration},
    text_input::{TextInput, TextInputEvent},
    theme::{self, ActiveTheme, ActiveThemeExt, TerminalColors},
};

actions!(
    terminal,
    [
        Copy,
        Paste,
        SelectAll,
        ClearScrollback,
        Find,
        SearchNext,
        SearchPrevious,
        PreviousPrompt,
        NextPrompt
    ]
);

const SEARCH_CONTEXT: &str = "TerminalSearch";

const SCROLLBACK_LINES: usize = 10_000;
const FALLBACK_TITLE: &str = "shell";
const METADATA_REFRESH_INTERVAL: Duration = Duration::from_millis(400);

/// What the sidebar shows about a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabMetadata {
    /// The title the running program set, or the foreground process name.
    pub title: SharedString,
    /// The shell's working directory, with the home prefix shortened.
    pub directory: Option<SharedString>,
    pub cwd: Option<PathBuf>,
    pub branch: Option<SharedString>,
    /// Name of the foreground process (the shell itself at a prompt).
    pub process: Option<SharedString>,
    /// Whether a program other than the shell is in the foreground.
    pub running: bool,
    /// When the current command started, per shell integration.
    pub command_started: Option<Instant>,
    /// How the most recent command ended, per shell integration.
    pub last_command: Option<CommandOutcome>,
    /// The coding agent in the foreground, if any, and what it is doing.
    pub agent: Option<AgentState>,
    /// Uncommitted changes in the shell's repository.
    pub diff: Option<DiffStats>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentState {
    pub agent: Agent,
    pub status: AgentStatus,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CommandOutcome {
    pub exit_code: Option<i32>,
    pub duration: Duration,
}

impl CommandOutcome {
    pub fn failed(&self) -> bool {
        self.exit_code.is_some_and(|code| code != 0)
    }
}

pub enum TerminalEvent {
    MetadataChanged,
    Exited,
    Attention(Attention),
}

/// Something in a terminal that may deserve the user's attention.
#[derive(Clone, Debug, PartialEq)]
pub enum Attention {
    Bell,
    /// A program asked for a desktop notification (OSC 9 or OSC 777).
    Notification {
        title: Option<String>,
        body: String,
    },
    /// A command tracked by shell integration finished.
    CommandFinished(CommandOutcome),
    /// An agent that had been working went quiet.
    AgentWaiting {
        agent: Agent,
        needs_input: bool,
    },
}

/// How long to wait for a first prompt before typing a startup command
/// anyway.
const STARTUP_FALLBACK: Duration = Duration::from_secs(4);

/// Environment variable telling programs which pane they run in.
pub const PANE_ID_VARIABLE: &str = "TERMINAL_PANE_ID";
/// Matches listed by the Ctrl-R history search.
const HISTORY_SEARCH_RESULTS: usize = 8;

/// A place in history reached with Up and Down.
struct HistoryPosition {
    index: usize,
    /// What was typed before walking history.
    draft: String,
    /// The editor change this position caused is still to be reported,
    /// and must not end the walk.
    applying: bool,
}

/// The Ctrl-R search: the editor's text is the query.
#[derive(Default)]
struct HistorySearch {
    /// Best match first.
    matches: Vec<String>,
    selected: usize,
}

/// How long a command runs before the editor hides and keys go to it.
const EDITOR_GRACE: Duration = Duration::from_millis(50);

/// Most command blocks a pane keeps; older ones are dropped.
const MAX_BLOCKS: usize = 1000;

/// Environment variable carrying the id shell integration stamps on hooks.
pub const SESSION_ID_VARIABLE: &str = "TERMINAL_SESSION_ID";
static NEXT_PANE_ID: AtomicU64 = AtomicU64::new(1);

/// How often uncommitted changes are recounted.
const DIFF_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// An agent must work at least this long before going quiet is news.
const AGENT_MIN_WORK: Duration = Duration::from_secs(3);

/// A single shell session: PTY, libghostty terminal state, and the view that
/// paints it and turns GPUI input into VT sequences.
pub struct TerminalView {
    terminal: Terminal<'static, 'static>,
    renderer: GridRenderer,
    pty: Pty,
    dimensions: Rc<Cell<PtyDimensions>>,
    focus_handle: FocusHandle,
    keys: KeyInput,
    mouse: MouseInput,
    selection: SelectionInput,
    bounds: Option<Bounds<Pixels>>,
    metrics: Option<CellMetrics>,
    metadata: TabMetadata,
    process_metadata: ProcessMetadata,
    _output_task: Task<()>,
    _metadata_task: Task<()>,
    _settings_subscription: Subscription,
    _theme_subscription: Subscription,
    search: Option<SearchBar>,
    /// The link under the pointer while ⌘ is held.
    link_hover: Option<HoveredLink>,
    /// Last pointer position relative to the terminal, for ⌘ presses.
    last_mouse: Option<gpui::Point<Pixels>>,
    osc: OscScanner,
    /// Set by libghostty when the program rings the bell.
    bell: Rc<Cell<bool>>,
    activity: Activity,
    /// A command to type once the shell shows its first prompt.
    pending_startup: Option<String>,
    /// What shell integration reported when the shell finished starting.
    shell: Option<Bootstrapped>,
    /// The pane's command blocks, once shell integration has bootstrapped
    /// and blocks are enabled. `terminal` then belongs to whatever is
    /// running: the current command, or the shell's prompt between them.
    blocks: Option<BlockList>,
    /// Scroll and layout state of the block list, one item per block.
    block_list: ListState,
    /// Where commands are typed while the shell waits at its prompt.
    editor: Entity<CommandEditor>,
    _editor_subscription: Subscription,
    /// Whether the shell has drawn a prompt since starting, so injected
    /// commands are no longer at risk of being read as replies to its
    /// startup queries.
    shell_ready: bool,
    /// A command submitted before the shell was ready.
    queued_command: Option<String>,
    history: History,
    /// Where Up and Down have moved through history, if they have.
    history_position: Option<HistoryPosition>,
    /// The open Ctrl-R search, if any.
    history_search: Option<HistorySearch>,
    /// Names the shell can run, once indexed.
    commands: Option<Rc<CommandIndex>>,
    /// The shell's state at its most recent prompt.
    prompt: Option<Precmd>,
    pane_id: u64,
    /// Secret proving a control request comes from a program in this pane.
    control_token: String,
    diff: Option<DiffStats>,
    /// When the foreground agent started its current stretch of work.
    working_since: Option<Instant>,
    command_started: Option<Instant>,
    last_command: Option<CommandOutcome>,
}

/// A link in the viewport, in grid coordinates (end column exclusive).
struct HoveredLink {
    row: u16,
    start_col: u16,
    end_col: u16,
    target: LinkTarget,
}

/// Schemes an OSC 8 hyperlink may use; anything else is ignored rather
/// than handed to the system to open.
const HYPERLINK_SCHEMES: [&str; 5] = ["https://", "http://", "ftp://", "file://", "mailto:"];

/// The find bar and its results.
struct SearchBar {
    input: Entity<TextInput>,
    matches: Vec<SearchMatch>,
    current: usize,
    _subscription: Subscription,
}

/// Process and filesystem observations collected away from the UI thread.
#[derive(Default)]
struct ProcessMetadata {
    foreground: Option<i32>,
    process: Option<String>,
    directory: Option<PathBuf>,
    branch: Option<String>,
    agent: Option<Agent>,
}

impl ProcessMetadata {
    fn read(foreground: Option<i32>, shell: Option<i32>) -> Self {
        let directory = shell.and_then(process_info::working_directory);
        Self {
            foreground,
            process: foreground.and_then(process_info::process_name),
            branch: directory.as_deref().and_then(process_info::git_branch),
            directory,
            agent: foreground
                .filter(|pid| Some(*pid) != shell)
                .and_then(process_info::process_args)
                .and_then(|args| agents::detect(&args)),
        }
    }
}

struct KeyInput {
    encoder: key::Encoder<'static>,
    event: key::Event<'static>,
}

struct MouseInput {
    encoder: mouse::Encoder<'static>,
    event: mouse::Event<'static>,
    pressed: Option<mouse::Button>,
    scroll_remainder: f32,
}

struct SelectionInput {
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    release: ReleaseEvent<'static>,
    drag: DragEvent<'static>,
    started_at: Instant,
    selecting: bool,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    /// Spawn a shell and create the view that displays it.
    /// Start a shell in `cwd`. `startup` is typed into it once it starts,
    /// as if the user had entered it at the prompt.
    pub fn build(cwd: Option<&Path>, startup: Option<&str>, cx: &mut App) -> Result<Entity<Self>> {
        let initial = PtyDimensions {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 18,
        };
        let pane_id = NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed);
        let mut command =
            shell_integration::shell_command(ShellIntegration::active_dir(cx).as_deref());
        // Lets programs in the pane, such as the control CLI, address it.
        command.env(PANE_ID_VARIABLE, pane_id.to_string());
        let control_token = control::new_token()?;
        command.env(control::TOKEN_VARIABLE, &control_token);
        // Shell integration stamps its hooks with this id; others are ignored.
        let session_id = control::new_token()?;
        command.env(SESSION_ID_VARIABLE, &session_id);
        control::configure_command(&mut command, cx);
        let (pty, output) = Pty::spawn(command, initial, cwd)?;
        let initial_cwd = pty.initial_cwd().map(Path::to_path_buf);
        let dimensions = Rc::new(Cell::new(initial));

        let bell = Rc::new(Cell::new(false));
        let terminal = new_terminal(&pty, dimensions.clone(), bell.clone(), &cx.theme().terminal)?;

        let renderer = GridRenderer::new()?;
        let keys = KeyInput {
            encoder: key::Encoder::new()?,
            event: key::Event::new()?,
        };
        let mouse = MouseInput {
            encoder: mouse::Encoder::new()?,
            event: mouse::Event::new()?,
            pressed: None,
            scroll_remainder: 0.,
        };
        let mut press = PressEvent::new()?;
        press.set_repeat_interval(Duration::from_millis(500))?;
        let selection = SelectionInput {
            gesture: Gesture::new()?,
            press,
            release: ReleaseEvent::new()?,
            drag: DragEvent::new()?,
            started_at: Instant::now(),
            selecting: false,
        };

        Ok(cx.new(|cx| {
            let output_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let executor = cx.background_executor().clone();
                let mut last_frame = Instant::now() - output::FRAME_INTERVAL;
                let pending = output.clone();
                output::consume(
                    output,
                    |batch| {
                        this.update(cx, |view, cx| {
                            view.process_output(batch, cx);
                            if pending.is_empty() || last_frame.elapsed() >= output::FRAME_INTERVAL
                            {
                                last_frame = Instant::now();
                                cx.notify();
                            }
                        })
                        .is_ok()
                    },
                    || executor.timer(output::YIELD_INTERVAL),
                )
                .await;
                let _ = this.update(cx, |_, cx| cx.emit(TerminalEvent::Exited));
            });
            if startup.is_some() {
                // Shells without prompt marks never announce a prompt.
                cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                    cx.background_executor().timer(STARTUP_FALLBACK).await;
                    let _ = this.update(cx, |view, cx| view.send_startup_command(cx));
                })
                .detach();
            }
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let Ok(cwd) = this.update(cx, |view, _| view.metadata.cwd.clone()) else {
                        return;
                    };
                    let diff = match cwd.clone() {
                        Some(cwd) => {
                            cx.background_executor()
                                .spawn(async move { git::diff_stats(&cwd) })
                                .await
                        }
                        None => None,
                    };
                    let updated = this.update(cx, |view, cx| {
                        if view.metadata.cwd == cwd && view.diff != diff {
                            view.diff = diff;
                            view.refresh_metadata(cx);
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                    cx.background_executor().timer(DIFF_REFRESH_INTERVAL).await;
                }
            })
            .detach();
            let metadata_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let Ok((foreground, shell)) = this.update(cx, |view, _| {
                        (view.pty.foreground_pid(), view.pty.shell_pid())
                    }) else {
                        return;
                    };
                    let metadata = cx
                        .background_executor()
                        .spawn(async move { ProcessMetadata::read(foreground, shell) })
                        .await;
                    let refreshed = this.update(cx, |view, cx| {
                        if view.pty.foreground_pid() == foreground {
                            view.process_metadata = metadata;
                            view.refresh_metadata(cx);
                        }
                    });
                    if refreshed.is_err() {
                        return;
                    }
                    cx.background_executor()
                        .timer(METADATA_REFRESH_INTERVAL)
                        .await;
                }
            });
            let editor = cx.new(|cx| CommandEditor::new("Run a command", cx));

            Self {
                terminal,
                renderer,
                pty,
                dimensions,
                focus_handle: cx.focus_handle(),
                keys,
                mouse,
                selection,
                bounds: None,
                metrics: None,
                metadata: TabMetadata {
                    title: FALLBACK_TITLE.into(),
                    directory: initial_cwd
                        .as_deref()
                        .map(|path| process_info::shorten_home(path).into()),
                    cwd: initial_cwd.clone(),
                    ..Default::default()
                },
                process_metadata: ProcessMetadata {
                    directory: initial_cwd,
                    ..Default::default()
                },
                _output_task: output_task,
                _metadata_task: metadata_task,
                // Font and padding changes take effect on the next frame.
                _settings_subscription: cx.observe_global::<SettingsStore>(|view, cx| {
                    view.metrics = None;
                    cx.notify();
                }),
                search: None,
                link_hover: None,
                last_mouse: None,
                osc: OscScanner::new(session_id),
                bell,
                activity: Activity::default(),
                pending_startup: startup.map(str::to_string),
                shell: None,
                blocks: None,
                block_list: {
                    let state = ListState::new(0, ListAlignment::Top, px(400.));
                    state.set_follow_mode(FollowMode::Tail);
                    state
                },
                _editor_subscription: cx.subscribe(&editor, Self::on_editor_event),
                editor,
                shell_ready: false,
                queued_command: None,
                history: History::default(),
                history_position: None,
                history_search: None,
                commands: None,
                prompt: None,
                pane_id,
                control_token,
                diff: None,
                working_since: None,
                command_started: None,
                last_command: None,
                _theme_subscription: cx.observe_global::<ActiveTheme>(|view, cx| {
                    let colors = cx.theme().terminal.clone();
                    if let Err(error) = configure_colors(&mut view.terminal, &colors) {
                        log::warn!("failed to apply theme: {error}");
                    }
                    view.rebuild_block_rows(cx);
                    cx.notify();
                }),
            }
        }))
    }

    /// Where the terminal was last laid out, in window coordinates.
    /// Whether `token` is this pane's control token, compared in constant
    /// time.
    pub fn has_control_token(&self, token: &str) -> bool {
        let expected = self.control_token.as_bytes();
        let given = token.as_bytes();
        expected.len() == given.len()
            && expected
                .iter()
                .zip(given)
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0
    }

    /// Identifies this pane to the control API for the app's lifetime.
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// The last `lines` non-empty lines of the screen and scrollback.
    pub fn recent_text(&self, lines: usize) -> String {
        let text = search::screen_text(&self.terminal).unwrap_or_default();
        let kept: Vec<&str> = text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .collect();
        kept[kept.len().saturating_sub(lines)..].join("\n")
    }

    /// Type `text` into the pane as if the user had.
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.write_input(text.as_bytes(), cx);
    }

    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds
    }

    /// Current grid size as (columns, rows).
    pub fn grid_size(&self) -> (u16, u16) {
        let dimensions = self.dimensions.get();
        (dimensions.cols, dimensions.rows)
    }

    pub fn metadata(&self) -> &TabMetadata {
        &self.metadata
    }

    fn process_output(&mut self, chunks: &mut output::OutputBatch<'_>, cx: &mut Context<Self>) {
        let mut command_changed = false;
        for chunk in chunks {
            for piece in self.osc.scan(&chunk) {
                match piece {
                    Piece::Output(bytes) => {
                        if let Some(blocks) = &mut self.blocks {
                            blocks.record(&bytes);
                        }
                        self.terminal.vt_write(&bytes);
                    }
                    Piece::Event(event) => command_changed |= self.apply_osc_event(event, cx),
                }
            }
        }
        self.activity.output(Instant::now());
        if self.bell.replace(false) {
            self.activity.attention();
            cx.emit(TerminalEvent::Attention(Attention::Bell));
        }
        if command_changed {
            self.refresh_metadata(cx);
        }
    }

    fn apply_hook(&mut self, hook: Hook, cx: &mut Context<Self>) {
        match hook {
            Hook::Bootstrapped(bootstrapped) => {
                self.index_shell(bootstrapped.clone(), cx);
                self.shell = Some(bootstrapped);
                if SettingsStore::get(cx).command_blocks && self.blocks.is_none() {
                    let mut blocks = BlockList::new(MAX_BLOCKS);
                    if let Err(error) = blocks.push_startup(&mut self.terminal) {
                        log::error!("failed to keep startup output: {error:#}");
                    }
                    self.block_list.reset(blocks.blocks().len());
                    self.blocks = Some(blocks);
                    // The prompt is drawn into a fresh terminal and hidden
                    // behind the editor.
                    if let Err(error) = self.replace_terminal(cx) {
                        log::error!("failed to reset the terminal: {error:#}");
                    }
                }
            }
            Hook::Precmd(precmd) => {
                if let Some(blocks) = &mut self.blocks {
                    blocks.set_context(&precmd);
                }
                self.prompt = Some(precmd);
                self.shell_ready = true;
                if let Some(command) = self.queued_command.take() {
                    self.inject_command(&command, cx);
                }
            }
            Hook::Preexec { command } => {
                if self.blocks.is_none() {
                    return;
                }
                // The command's output goes to a terminal of its own.
                if let Err(error) = self.replace_terminal(cx) {
                    log::error!("failed to start a block: {error:#}");
                    return;
                }
                let Some(blocks) = &mut self.blocks else {
                    return;
                };
                let count = blocks.blocks().len();
                if let Err(error) = blocks.start(command) {
                    log::error!("failed to start a block: {error:#}");
                    return;
                }
                // Blocks beyond the cap leave from the top.
                self.block_list.splice(count..count, 1);
                let dropped = count + 1 - blocks.blocks().len();
                if dropped > 0 {
                    self.block_list.splice(0..dropped, 0);
                }
                // The editor gives way to the command once it has run long
                // enough to be more than a flicker.
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(EDITOR_GRACE).await;
                    let _ = view.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
            Hook::CommandFinished { exit_code } => {
                let Some(blocks) = &mut self.blocks else {
                    return;
                };
                if let Err(error) = blocks.finish(exit_code, &mut self.terminal) {
                    log::error!("failed to capture a block's output: {error:#}");
                }
                // The next prompt is drawn into a fresh terminal.
                if let Err(error) = self.replace_terminal(cx) {
                    log::error!("failed to reset the terminal: {error:#}");
                }
            }
        }
    }

    /// The list items to show, when the pane shows blocks. Capturing a
    /// running block's scrollback leaves its terminal scrolled to the
    /// bottom, ready for its live screen to be drawn.
    fn block_items(&mut self) -> Option<Vec<Item>> {
        if !self.shows_blocks() {
            return None;
        }
        let blocks = self.blocks.as_mut()?;
        let mut items = Vec::with_capacity(blocks.blocks().len() + 1);
        for block in blocks.blocks_mut() {
            let content = if block.is_running() {
                match block.scrollback(&mut self.terminal) {
                    Ok(pages) => ItemContent::Running(pages),
                    Err(error) => {
                        log::error!("failed to read a block's output: {error:#}");
                        ItemContent::Running(Vec::new())
                    }
                }
            } else {
                ItemContent::Finished(block.finished_rows().unwrap_or_default())
            };
            items.push(Item::block(block, content));
        }
        // A running block is drawn from the live terminal and grows with it.
        if blocks.running().is_some() {
            let last = items.len() - 1;
            self.block_list.remeasure_items(last..items.len());
        }
        Some(items)
    }

    /// Rebuild finished blocks' rows for the pane's current width and
    /// colors.
    fn rebuild_block_rows(&mut self, cx: &mut Context<Self>) {
        let Some(blocks) = &mut self.blocks else {
            return;
        };
        let size = self.dimensions.get();
        let colors = cx.theme().terminal.clone();
        let rebuilt = blocks.rebuild_rows(|| {
            let mut terminal = Terminal::new(Options {
                cols: size.cols,
                rows: size.rows,
                max_scrollback: SCROLLBACK_LINES,
            })?;
            configure_colors(&mut terminal, &colors)?;
            Ok(terminal)
        });
        if let Err(error) = rebuilt {
            log::error!("failed to rebuild blocks: {error:#}");
        }
        self.block_list.remeasure();
    }

    /// Whether the editor is shown: in block mode, unless a command has
    /// been running past the grace period and takes the keyboard.
    fn shows_editor(&self) -> bool {
        self.shows_blocks()
            && self
                .blocks
                .as_ref()
                .and_then(BlockList::running)
                .is_none_or(|block| block.started.elapsed() < EDITOR_GRACE)
    }

    fn on_editor_event(
        &mut self,
        _: Entity<CommandEditor>,
        event: &CommandEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            CommandEditorEvent::Submitted(command) => {
                if self.history_search.is_some() {
                    self.pick_history_match(cx);
                    return;
                }
                self.history_position = None;
                if command.trim().is_empty() {
                    return;
                }
                self.history.push(command);
                if self.shell_ready {
                    self.inject_command(command, cx);
                } else {
                    self.queued_command = Some(command.clone());
                }
            }
            CommandEditorEvent::EndOfFile => self.write_input(b"\x04", cx),
            CommandEditorEvent::HistoryPrevious => match &mut self.history_search {
                Some(search) => {
                    search.selected =
                        (search.selected + 1).min(search.matches.len().saturating_sub(1));
                    cx.notify();
                }
                None => self.walk_history(false, cx),
            },
            CommandEditorEvent::HistoryNext => match &mut self.history_search {
                Some(search) => {
                    search.selected = search.selected.saturating_sub(1);
                    cx.notify();
                }
                None => self.walk_history(true, cx),
            },
            CommandEditorEvent::SearchHistory => {
                if self.history_search.take().is_none() {
                    self.history_search = Some(HistorySearch::default());
                    self.update_history_search(cx);
                }
                cx.notify();
            }
            CommandEditorEvent::Escaped => {
                if self.history_search.take().is_some() {
                    cx.notify();
                }
            }
            CommandEditorEvent::Changed => {
                if let Some(position) = &mut self.history_position {
                    if position.applying {
                        position.applying = false;
                    } else {
                        self.history_position = None;
                    }
                }
                self.update_history_search(cx);
                self.decorate_editor(cx);
            }
        }
    }

    /// Load the shell's history and index the commands it can run, off
    /// the main thread.
    fn index_shell(&mut self, shell: Bootstrapped, cx: &mut Context<Self>) {
        let work = cx.background_executor().spawn(async move {
            let history = shell.histfile.as_deref().map(|path| {
                history::load(path, &shell.shell).unwrap_or_else(|error| {
                    log::warn!("failed to read {}: {error:#}", path.display());
                    History::default()
                })
            });
            (history, CommandIndex::new(&shell))
        });
        cx.spawn(async move |view, cx| {
            let (history, commands) = work.await;
            let _ = view.update(cx, |view, cx| {
                if let Some(history) = history {
                    view.history.prepend(history);
                }
                view.commands = Some(Rc::new(commands));
                view.decorate_editor(cx);
            });
        })
        .detach();
    }

    /// Show the previous (or next) history entry in the editor, keeping
    /// what was typed to come back to past the newest entry.
    fn walk_history(&mut self, forward: bool, cx: &mut Context<Self>) {
        let len = self.history.len();
        let next = match (&self.history_position, forward) {
            (None, true) => return,
            (None, false) => len.checked_sub(1),
            (Some(position), false) => position.index.checked_sub(1),
            (Some(position), true) => Some(position.index + 1),
        };
        let Some(index) = next else {
            return;
        };
        let draft = match self.history_position.take() {
            Some(position) => position.draft,
            None => self.editor.read(cx).text().to_string(),
        };
        let text = match self.history.get(index) {
            Some(entry) => {
                self.history_position = Some(HistoryPosition {
                    index,
                    draft,
                    applying: true,
                });
                entry.to_string()
            }
            None => draft,
        };
        self.editor
            .update(cx, |editor, cx| editor.set_text(text, cx));
    }

    fn update_history_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &mut self.history_search else {
            return;
        };
        let query = self.editor.read(cx).text();
        search.matches = self
            .history
            .search(query, HISTORY_SEARCH_RESULTS)
            .into_iter()
            .map(str::to_string)
            .collect();
        search.selected = 0;
        cx.notify();
    }

    /// Put the selected search match in the editor.
    fn pick_history_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        if let Some(entry) = search.matches.get(search.selected) {
            let entry = entry.clone();
            self.editor
                .update(cx, |editor, cx| editor.set_text(entry, cx));
        }
        cx.notify();
    }

    /// Refresh the editor's highlighting and history suggestion.
    fn decorate_editor(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let suggestion = self
            .history
            .suggestion(&text)
            .filter(|_| self.history_search.is_none())
            .map(str::to_string);
        let highlights = match &self.commands {
            Some(commands) => {
                let colors = &cx.theme().terminal;
                let cwd = self
                    .prompt
                    .as_ref()
                    .and_then(|prompt| prompt.cwd.as_deref());
                highlight::highlight(&text, commands, cwd)
                    .into_iter()
                    .map(|token| {
                        let color = match token.kind {
                            TokenKind::Command => colors.ansi[2],
                            TokenKind::UnknownCommand => colors.ansi[1],
                            TokenKind::Flag => colors.ansi[6],
                            TokenKind::String => colors.ansi[3],
                            TokenKind::Operator => colors.ansi[5],
                        };
                        (token.range, theme::to_hsla(color))
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_suggestion(suggestion, cx);
            editor.set_highlights(highlights, cx);
        });
    }

    /// The editor with the shell's context above it.
    fn render_editor_panel(&self, metrics: CellMetrics, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let context = self
            .prompt
            .as_ref()
            .map(BlockContext::from)
            .unwrap_or_default();
        let chips =
            block_view::context_chips(&context, self.process_metadata.branch.as_deref(), theme);
        // Ctrl-R matches, best at the bottom, next to the editor.
        let search = self.history_search.as_ref().map(|search| {
            let rows = search
                .matches
                .iter()
                .enumerate()
                .rev()
                .map(|(index, entry)| {
                    let first_line = entry.lines().next().unwrap_or_default().to_string();
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .when(index == search.selected, |row| {
                            row.bg(theme.ghost_selected).text_color(theme.text)
                        })
                        .child(first_line)
                });
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .pb_1()
                .font_family(theme::FONT_FAMILY)
                .text_sm()
                .text_color(theme.text_muted)
                .when(search.matches.is_empty(), |list| {
                    list.child(div().px_2().child("No matching commands"))
                })
                .children(rows)
        });
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1p5()
            .px(metrics.padding + block_view::HORIZONTAL_INSET)
            .py_2()
            .border_t_1()
            .border_color(theme.border_variant)
            .children(search)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .font_family(theme::UI_FONT_FAMILY)
                    .children(chips),
            )
            .child(self.editor.clone())
            .into_any_element()
    }

    /// Run `command` in the shell: clear its line editor, paste the
    /// command, and press Enter.
    fn inject_command(&mut self, command: &str, cx: &mut Context<Self>) {
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
        let mut data = command.as_bytes().to_vec();
        let mut encoded = vec![0u8; data.len() + 32];
        let len = match paste::encode(&mut data, bracketed, &mut encoded) {
            Ok(len) => len,
            Err(error) => {
                log::warn!("failed to encode a command: {error}");
                return;
            }
        };
        let mut bytes = shell_integration::CLEAR_LINE_KEY.to_vec();
        bytes.extend_from_slice(&encoded[..len]);
        bytes.push(b'\r');
        self.write_input(&bytes, cx);
    }

    /// Whether the pane shows its block list rather than a single terminal
    /// screen, which full-screen programs still get.
    fn shows_blocks(&self) -> bool {
        self.blocks.is_some() && !blocks::is_full_screen(&self.terminal)
    }

    /// Swap in a fresh terminal at the pane's size and theme.
    fn replace_terminal(&mut self, cx: &App) -> Result<()> {
        self.terminal = new_terminal(
            &self.pty,
            self.dimensions.clone(),
            self.bell.clone(),
            &cx.theme().terminal,
        )?;
        Ok(())
    }

    /// Returns whether the command state changed.
    fn apply_osc_event(&mut self, event: OscEvent, cx: &mut Context<Self>) -> bool {
        match event {
            OscEvent::Hook(hook) => {
                self.apply_hook(hook, cx);
                true
            }
            OscEvent::PromptStarted => {
                self.send_startup_command(cx);
                false
            }
            OscEvent::CommandStarted => {
                self.command_started = Some(Instant::now());
                true
            }
            OscEvent::CommandFinished(exit_code) => {
                // A finish without a start (e.g. the first prompt) has no
                // command to report.
                let Some(started) = self.command_started.take() else {
                    return false;
                };
                let outcome = CommandOutcome {
                    exit_code,
                    duration: started.elapsed(),
                };
                self.last_command = Some(outcome);
                cx.emit(TerminalEvent::Attention(Attention::CommandFinished(
                    outcome,
                )));
                true
            }
            OscEvent::Notify { title, body } => {
                self.activity.attention();
                cx.emit(TerminalEvent::Attention(Attention::Notification {
                    title,
                    body,
                }));
                false
            }
        }
    }

    /// Type the startup command into the shell. This waits for the first
    /// prompt because startup files that query the terminal consume any
    /// input typed before they finish.
    fn send_startup_command(&mut self, cx: &mut Context<Self>) {
        if let Some(command) = self.pending_startup.take() {
            self.write_input(format!("{command}\n").as_bytes(), cx);
        }
    }

    /// Screen rows where a shell prompt begins, oldest first.
    fn prompt_rows(&self) -> Vec<u32> {
        let total = self.terminal.total_rows().unwrap_or(0) as u32;
        (0..total)
            .filter(|row| {
                self.terminal
                    .grid_ref(Point::Screen(PointCoordinate { x: 0, y: *row }))
                    .and_then(|grid_ref| grid_ref.row())
                    .and_then(|row| row.semantic_prompt())
                    .is_ok_and(|prompt| prompt == RowSemanticPrompt::Prompt)
            })
            .collect()
    }

    fn jump_to_prompt(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Ok(scrollbar) = self.terminal.scrollbar() else {
            return;
        };
        let top = scrollbar.offset as u32;
        let prompts = self.prompt_rows();
        let target = if forward {
            prompts.into_iter().find(|row| *row > top)
        } else {
            prompts.into_iter().rev().find(|row| *row < top)
        };
        match target {
            Some(row) => self
                .terminal
                .scroll_viewport(ScrollViewport::Row(row as usize)),
            None if forward => self.terminal.scroll_viewport(ScrollViewport::Bottom),
            None => return,
        }
        cx.notify();
    }

    fn previous_prompt(&mut self, _: &PreviousPrompt, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_prompt(false, cx);
    }

    fn next_prompt(&mut self, _: &NextPrompt, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_prompt(true, cx);
    }

    fn refresh_metadata(&mut self, cx: &mut Context<Self>) {
        let metadata = self.read_metadata();
        self.track_agent_work(metadata.agent, cx);
        if metadata != self.metadata {
            self.metadata = metadata;
            cx.emit(TerminalEvent::MetadataChanged);
        }
    }

    /// Announce when an agent that worked for a while stops producing
    /// output, which means it finished or is waiting on the user.
    fn track_agent_work(&mut self, agent: Option<AgentState>, cx: &mut Context<Self>) {
        match agent {
            Some(state) if state.status == AgentStatus::Working => {
                self.working_since.get_or_insert_with(Instant::now);
            }
            Some(state) => {
                if let Some(since) = self.working_since.take()
                    && since.elapsed() >= AGENT_MIN_WORK
                {
                    cx.emit(TerminalEvent::Attention(Attention::AgentWaiting {
                        agent: state.agent,
                        needs_input: state.status == AgentStatus::NeedsInput,
                    }));
                }
            }
            None => self.working_since = None,
        }
    }

    fn read_metadata(&self) -> TabMetadata {
        let foreground = self.process_metadata.foreground;
        let process = self.process_metadata.process.clone();
        let running = foreground.is_some() && foreground != self.pty.shell_pid();
        let program_title = self.terminal.title().unwrap_or_default().trim().to_string();
        let title = if program_title.is_empty() {
            process
                .clone()
                .unwrap_or_else(|| FALLBACK_TITLE.to_string())
        } else {
            program_title
        };

        // Ask the kernel rather than trusting OSC 7: any program writing to
        // the terminal can emit that sequence and claim an arbitrary path,
        // which new tabs, splits, and session restore would then open.
        let directory = self.process_metadata.directory.clone();
        let branch = self.process_metadata.branch.clone();

        TabMetadata {
            title: title.into(),
            directory: directory
                .as_deref()
                .map(|path| process_info::shorten_home(path).into()),
            cwd: directory,
            branch: branch.map(Into::into),
            process: process.map(Into::into),
            running,
            command_started: self.command_started,
            last_command: self.last_command,
            diff: self.diff,
            agent: self.process_metadata.agent.map(|agent| AgentState {
                agent,
                status: self.activity.status(Instant::now()),
            }),
        }
    }

    /// Fit the terminal grid to the space the layout gave us.
    fn fit_to_bounds(
        &mut self,
        bounds: Bounds<Pixels>,
        metrics: CellMetrics,
        cx: &mut Context<Self>,
    ) {
        self.bounds = Some(bounds);
        // Blocks are inset from the sides; full-screen programs are not.
        let grid_bounds = if self.shows_blocks() {
            let inset = block_view::HORIZONTAL_INSET;
            Bounds::new(
                bounds.origin + gpui::point(inset, px(0.)),
                gpui::size(bounds.size.width - inset * 2., bounds.size.height),
            )
        } else {
            bounds
        };
        let (cols, rows) = metrics.grid_size(grid_bounds);
        let next = PtyDimensions {
            cols,
            rows,
            cell_width: f32::from(metrics.width).round() as u16,
            cell_height: f32::from(metrics.height).round() as u16,
        };
        let previous = self.dimensions.get();
        if next == previous {
            return;
        }
        if let Err(error) = self.terminal.resize(
            next.cols,
            next.rows,
            next.cell_width as u32,
            next.cell_height as u32,
        ) {
            log::warn!("failed to resize terminal: {error}");
            return;
        }
        self.dimensions.set(next);
        self.pty.resize(next);
        if let Some(blocks) = &mut self.blocks {
            blocks.terminal_resized();
        }
        if next.cols != previous.cols {
            self.rebuild_block_rows(cx);
        }
        cx.notify();
    }

    fn write_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        let _ = self.terminal.set_selection(None);
        self.activity.user_input(Instant::now());
        self.pty.write(bytes);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Command shortcuts belong to the app, never to the shell, and keys
        // typed into the find bar bubble through here without being sent.
        if event.keystroke.modifiers.platform || !self.focus_handle.is_focused(window) {
            return;
        }
        let action = if event.is_held {
            key::Action::Repeat
        } else {
            key::Action::Press
        };
        let bytes = self.encode_key(&event.keystroke, action);
        self.write_input(&bytes, cx);
        cx.stop_propagation();
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform || !self.focus_handle.is_focused(window) {
            return;
        }
        // Release events only produce output when the Kitty keyboard
        // protocol asks for them, so this is usually empty.
        let bytes = self.encode_key(&event.keystroke, key::Action::Release);
        if !bytes.is_empty() {
            self.pty.write(&bytes);
        }
        cx.stop_propagation();
    }

    fn encode_key(&mut self, keystroke: &gpui::Keystroke, action: key::Action) -> Vec<u8> {
        let translated = translate_keystroke(keystroke);
        let text = match action {
            key::Action::Release => None,
            _ => translated.text.clone(),
        };
        self.keys
            .event
            .set_action(action)
            .set_key(translated.key)
            .set_mods(translated.mods)
            .set_consumed_mods(translated.consumed_mods)
            .set_unshifted_codepoint(translated.unshifted)
            .set_utf8(text.clone());

        let mut bytes = Vec::with_capacity(16);
        if let Err(error) = self
            .keys
            .encoder
            .set_options_from_terminal(&self.terminal)
            .encode_to_vec(&self.keys.event, &mut bytes)
        {
            log::warn!("failed to encode key: {error}");
        }
        // Keys the encoder has no mapping for (non-US layouts, dead keys)
        // still carry text the shell should receive.
        if bytes.is_empty()
            && let Some(text) = text
        {
            bytes.extend_from_slice(text.as_bytes());
        }
        bytes
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_link_hover(event.modifiers.platform, cx);
    }

    fn update_link_hover(&mut self, command_held: bool, cx: &mut Context<Self>) {
        let hovered = match (command_held, self.last_mouse) {
            (true, Some(local)) => self.link_under(local),
            _ => None,
        };
        let changed = match (&hovered, &self.link_hover) {
            (Some(new), Some(old)) => {
                (new.row, new.start_col, new.end_col) != (old.row, old.start_col, old.end_col)
            }
            (None, None) => false,
            _ => true,
        };
        if changed {
            self.link_hover = hovered;
            cx.notify();
        }
    }

    /// The hyperlink, URL, or existing file path under a pointer position.
    fn link_under(&self, local: gpui::Point<Pixels>) -> Option<HoveredLink> {
        let Point::Viewport(PointCoordinate { x, y }) = self.viewport_point(local) else {
            return None;
        };
        let row = y as u16;
        let cols = self.dimensions.get().cols;
        let cell = |col: u16| {
            self.terminal
                .grid_ref(Point::Viewport(PointCoordinate { x: col, y }))
        };

        // An explicit OSC 8 hyperlink wins over anything detected in text.
        let uri_at = |col: u16| -> Option<String> {
            let mut buffer = vec![0u8; 2048];
            let len = cell(col).ok()?.hyperlink_uri(&mut buffer).ok()?;
            (len > 0).then(|| String::from_utf8_lossy(&buffer[..len]).into_owned())
        };
        if let Some(uri) = uri_at(x) {
            if !HYPERLINK_SCHEMES
                .iter()
                .any(|scheme| uri.starts_with(scheme))
            {
                return None;
            }
            let mut start_col = x;
            while start_col > 0 && uri_at(start_col - 1).as_ref() == Some(&uri) {
                start_col -= 1;
            }
            let mut end_col = x + 1;
            while end_col < cols && uri_at(end_col).as_ref() == Some(&uri) {
                end_col += 1;
            }
            return Some(HoveredLink {
                row,
                start_col,
                end_col,
                target: LinkTarget::Url(uri),
            });
        }

        // Rebuild the row's text, remembering the column of each character.
        let mut chars = Vec::with_capacity(cols as usize);
        let mut columns = Vec::with_capacity(cols as usize);
        let mut graphemes = ['\0'; 16];
        for col in 0..cols {
            let Ok(grid_ref) = cell(col) else {
                continue;
            };
            if grid_ref
                .cell()
                .and_then(|cell| cell.wide())
                .is_ok_and(|wide| matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead))
            {
                continue;
            }
            let count = grid_ref.graphemes(&mut graphemes).unwrap_or(0);
            let ch = if count == 0 { ' ' } else { graphemes[0] };
            chars.push(ch);
            columns.push(col);
        }
        let index = columns.iter().rposition(|col| *col <= x)?;
        let cwd = self.metadata.cwd.clone();
        let found = links::link_at(&chars, index, cwd.as_deref(), |path| path.exists())?;
        let last = found.end - 1;
        let last_width = unicode_width::UnicodeWidthChar::width(chars[last])
            .unwrap_or(1)
            .max(1);
        Some(HoveredLink {
            row,
            start_col: columns[found.start],
            end_col: columns[last] + last_width as u16,
            target: found.target,
        })
    }

    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            let focus = search.input.focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        let input = cx.new(|cx| TextInput::new("Find", cx));
        let subscription =
            cx.subscribe_in(&input, window, |view, _, event, window, cx| match event {
                TextInputEvent::Changed => view.refresh_matches(cx),
                TextInputEvent::Confirmed => view.step_match(1, cx),
                TextInputEvent::Cancelled => view.close_search(window, cx),
            });
        window.focus(&input.focus_handle(cx), cx);
        self.search = Some(SearchBar {
            input,
            matches: Vec::new(),
            current: 0,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            let _ = self.terminal.set_selection(None);
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let query = search.input.read(cx).text().to_string();
        search.matches = match search::screen_text(&self.terminal) {
            Ok(text) => search::find_matches(&text, &query),
            Err(error) => {
                log::warn!("failed to read terminal text: {error}");
                Vec::new()
            }
        };
        // Start from the newest match, nearest the prompt.
        search.current = search.matches.len().saturating_sub(1);
        self.reveal_current_match(cx);
    }

    fn step_match(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let count = search.matches.len();
        if count == 0 {
            return;
        }
        search.current = (search.current as isize + delta).rem_euclid(count as isize) as usize;
        self.reveal_current_match(cx);
    }

    fn search_next(&mut self, _: &SearchNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(1, cx);
    }

    fn search_previous(&mut self, _: &SearchPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(-1, cx);
    }

    /// Select the current match and scroll it to the middle of the view.
    fn reveal_current_match(&mut self, cx: &mut Context<Self>) {
        let found = self
            .search
            .as_ref()
            .and_then(|search| search.matches.get(search.current).copied());
        let Some(found) = found else {
            let _ = self.terminal.set_selection(None);
            cx.notify();
            return;
        };
        let start = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: found.start_col,
            y: found.row,
        }));
        let end = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: found.end_col.saturating_sub(1),
            y: found.row,
        }));
        if let (Ok(start), Ok(end)) = (start, end) {
            let _ = self
                .terminal
                .set_selection(Some(&Selection::new(start, end, false)));
        }
        let rows = self.dimensions.get().rows as u32;
        self.terminal.scroll_viewport(ScrollViewport::Row(
            found.row.saturating_sub(rows / 2) as usize
        ));
        cx.notify();
    }

    /// The real destination of the hovered link, so text that merely looks
    /// like a trusted URL can't hide where a click actually goes.
    fn render_link_tooltip(
        &self,
        link: &HoveredLink,
        metrics: CellMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let destination = match &link.target {
            LinkTarget::Url(url) => url.clone(),
            LinkTarget::Path(path) => process_info::shorten_home(path),
        };
        let left = metrics.padding + metrics.width * link.start_col as f32;
        let rows = self.dimensions.get().rows;
        // Below the link, or above it on the bottom rows.
        let top = if link.row + 3 < rows {
            metrics.padding + metrics.height * (link.row + 1) as f32 + px(4.)
        } else {
            metrics.padding + metrics.height * link.row as f32 - px(28.)
        };

        div()
            .absolute()
            .left(left)
            .top(top)
            .max_w(px(520.))
            .px(px(8.))
            .py(px(3.))
            .rounded(px(4.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated_surface)
            .shadow(elevated_shadow())
            .font_family(theme::UI_FONT_FAMILY)
            .text_size(theme::TEXT_SMALL)
            .text_color(theme.text)
            .truncate()
            .child(SharedString::from(destination))
            .into_any_element()
    }

    fn render_search_bar(&self, search: &SearchBar, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let count = search.matches.len();
        let has_query = !search.input.read(cx).text().is_empty();
        let status = if count > 0 {
            format!("{}/{}", search.current + 1, count)
        } else if has_query {
            "No results".to_string()
        } else {
            String::new()
        };

        div()
            .absolute()
            .top(px(8.))
            .right(px(12.))
            .key_context(SEARCH_CONTEXT)
            .on_action(cx.listener(Self::search_next))
            .on_action(cx.listener(Self::search_previous))
            .child(
                div()
                    .id("terminal-search")
                    .occlude()
                    .cursor(CursorStyle::Arrow)
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .w(px(320.))
                    .h(px(32.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.elevated_surface)
                    .shadow(elevated_shadow())
                    .font_family(theme::UI_FONT_FAMILY)
                    .child(icon("search", theme::ICON_SMALL, theme.text_muted))
                    .child(div().flex_1().min_w_0().child(search.input.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::TEXT_SMALL)
                            .text_color(if has_query && count == 0 {
                                theme::to_hsla(theme.terminal.ansi[1])
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(status)),
                    )
                    .child(icon_button("search-previous", "arrow-up", &theme).on_click(
                        cx.listener(|view, _: &ClickEvent, _, cx| view.step_match(-1, cx)),
                    ))
                    .child(icon_button("search-next", "arrow-down", &theme).on_click(
                        cx.listener(|view, _: &ClickEvent, _, cx| view.step_match(1, cx)),
                    ))
                    .child(
                        icon_button("search-close", "x", &theme).on_click(cx.listener(
                            |view, _: &ClickEvent, window, cx| view.close_search(window, cx),
                        )),
                    ),
            )
            .into_any_element()
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let options = FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_trim(true)
            .with_unwrap(true);
        match self.terminal.format_selection_alloc(None, options) {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            Ok(None) => {}
            Err(error) => log::warn!("failed to copy selection: {error}"),
        }
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
        let mut data = text.into_bytes();
        let mut encoded = vec![0u8; data.len() + 32];
        match paste::encode(&mut data, bracketed, &mut encoded) {
            Ok(len) => {
                encoded.truncate(len);
                self.write_input(&encoded, cx);
            }
            Err(error) => log::warn!("failed to encode paste: {error}"),
        }
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(selection) = self.terminal.select_all() {
            let _ = self.terminal.set_selection(selection.as_ref());
            cx.notify();
        }
    }

    fn clear_scrollback(
        &mut self,
        _: &ClearScrollback,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Erase scrollback (CSI 3 J), then ask the shell to redraw its prompt.
        self.terminal.vt_write(b"\x1b[3J");
        self.write_input(b"\x0c", cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shows_editor() {
            window.focus(&self.editor.focus_handle(cx), cx);
            return;
        }
        window.focus(&self.focus_handle, cx);
        if self.shows_blocks() {
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        let local = event.position - bounds.origin;
        let mods = to_mods(&event.modifiers);

        if event.button == MouseButton::Left
            && event.modifiers.platform
            && let Some(link) = self.link_under(local)
        {
            links::open(&link.target, cx);
            return;
        }

        if event.button == MouseButton::Left && self.selection_allowed(&event.modifiers) {
            self.selection.selecting = true;
            let point = self.viewport_point(local);
            let Ok(grid_ref) = self.terminal.grid_ref(point) else {
                return;
            };
            let Some(metrics) = self.metrics else {
                return;
            };
            let result = self
                .selection
                .press
                .set_repeat_distance(f32::from(metrics.width).into())
                .and_then(|press| press.set_time(self.selection.started_at.elapsed()))
                .and_then(|press| {
                    press.set_position(f32::from(local.x).into(), f32::from(local.y).into())
                })
                .and_then(|press| {
                    press.apply(&mut self.selection.gesture, &self.terminal, grid_ref)
                });
            if let Ok(selection) = result {
                let _ = self.terminal.set_selection(selection.as_ref());
            }
            cx.notify();
            return;
        }

        let Some(button) = to_mouse_button(event.button) else {
            return;
        };
        self.mouse.pressed = Some(button);
        self.send_mouse(mouse::Action::Press, Some(button), local, mods, cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_blocks() {
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        let local = event.position - bounds.origin;

        if event.button == MouseButton::Left && self.selection.selecting {
            self.selection.selecting = false;
            let grid_ref = self.terminal.grid_ref(self.viewport_point(local)).ok();
            let _ =
                self.selection
                    .release
                    .apply(&mut self.selection.gesture, &self.terminal, grid_ref);
            cx.notify();
            return;
        }

        let Some(button) = to_mouse_button(event.button) else {
            return;
        };
        self.mouse.pressed = None;
        self.send_mouse(
            mouse::Action::Release,
            Some(button),
            local,
            to_mods(&event.modifiers),
            cx,
        );
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shows_blocks() {
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        let local = event.position - bounds.origin;
        self.last_mouse = Some(local);
        self.update_link_hover(event.modifiers.platform, cx);

        if self.selection.selecting {
            let Some(metrics) = self.metrics else {
                return;
            };
            let Ok(grid_ref) = self.terminal.grid_ref(self.viewport_point(local)) else {
                return;
            };
            let geometry = Geometry {
                columns: self.dimensions.get().cols.into(),
                cell_width: f32::from(metrics.width) as u32,
                padding_left: f32::from(metrics.padding) as u32,
                screen_height: f32::from(bounds.size.height) as u32,
            };
            let result = self
                .selection
                .drag
                .set_rectangle(event.modifiers.alt)
                .and_then(|drag| {
                    drag.set_position(f32::from(local.x).into(), f32::from(local.y).into())
                })
                .and_then(|drag| {
                    drag.apply(
                        &mut self.selection.gesture,
                        &self.terminal,
                        grid_ref,
                        geometry,
                    )
                });
            if let Ok(selection) = result {
                let _ = self.terminal.set_selection(selection.as_ref());
            }
            cx.notify();
            return;
        }

        if self.terminal.is_mouse_tracking().unwrap_or(false) {
            let button = self.mouse.pressed;
            self.send_mouse(
                mouse::Action::Motion,
                button,
                local,
                to_mods(&event.modifiers),
                cx,
            );
        }
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The block list scrolls itself.
        if self.shows_blocks() {
            return;
        }
        let Some(metrics) = self.metrics else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / f32::from(metrics.height),
        };
        self.mouse.scroll_remainder += lines;
        let whole = self.mouse.scroll_remainder.trunc();
        self.mouse.scroll_remainder -= whole;
        if whole == 0. {
            return;
        }
        let steps = whole.abs() as usize;

        if self.terminal.is_mouse_tracking().unwrap_or(false) {
            let Some(bounds) = self.bounds else {
                return;
            };
            let local = event.position - bounds.origin;
            let button = if whole > 0. {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            };
            let mods = to_mods(&event.modifiers);
            for _ in 0..steps {
                self.send_mouse(mouse::Action::Press, Some(button), local, mods, cx);
                self.send_mouse(mouse::Action::Release, Some(button), local, mods, cx);
            }
            return;
        }

        // Full-screen programs without mouse support (less, man) scroll with
        // arrow keys when alternate scroll mode is on.
        let alternate_screen = self.terminal.mode(Mode::ALT_SCREEN_SAVE).unwrap_or(false)
            || self.terminal.mode(Mode::ALT_SCREEN).unwrap_or(false);
        if alternate_screen && self.terminal.mode(Mode::ALT_SCROLL).unwrap_or(false) {
            let cursor_app = self.terminal.mode(Mode::DECCKM).unwrap_or(false);
            let arrow: &[u8] = match (whole > 0., cursor_app) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.pty.write(&arrow.repeat(steps));
            return;
        }

        self.terminal
            .scroll_viewport(ScrollViewport::Delta(-(whole as isize)));
        cx.notify();
    }

    fn send_mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        local: gpui::Point<Pixels>,
        mods: key::Mods,
        _cx: &mut Context<Self>,
    ) {
        let (Some(bounds), Some(metrics)) = (self.bounds, self.metrics) else {
            return;
        };
        let padding = f32::from(metrics.padding) as u32;
        self.mouse
            .event
            .set_action(action)
            .set_button(button)
            .set_mods(mods)
            .set_position(mouse::Position {
                x: f32::from(local.x),
                y: f32::from(local.y),
            });
        let mut bytes = Vec::with_capacity(16);
        let result = self
            .mouse
            .encoder
            .set_options_from_terminal(&self.terminal)
            .set_size(mouse::EncoderSize {
                screen_width: f32::from(bounds.size.width) as u32,
                screen_height: f32::from(bounds.size.height) as u32,
                cell_width: f32::from(metrics.width) as u32,
                cell_height: f32::from(metrics.height) as u32,
                padding_top: padding,
                padding_bottom: padding,
                padding_left: padding,
                padding_right: padding,
            })
            .set_any_button_pressed(self.mouse.pressed.is_some())
            .set_track_last_cell(true)
            .encode_to_vec(&self.mouse.event, &mut bytes);
        if let Err(error) = result {
            log::warn!("failed to encode mouse event: {error}");
        }
        self.pty.write(&bytes);
    }

    /// Holding shift lets the user select text even when a program has
    /// taken over the mouse.
    fn selection_allowed(&self, modifiers: &Modifiers) -> bool {
        !self.terminal.is_mouse_tracking().unwrap_or(false) || modifiers.shift
    }

    fn viewport_point(&self, local: gpui::Point<Pixels>) -> Point {
        let Some(metrics) = self.metrics else {
            return Point::Viewport(PointCoordinate { x: 0, y: 0 });
        };
        let dimensions = self.dimensions.get();
        let x = (f32::from(local.x - metrics.padding) / f32::from(metrics.width))
            .clamp(0., dimensions.cols.saturating_sub(1) as f32) as u16;
        let y = (f32::from(local.y - metrics.padding) / f32::from(metrics.height))
            .clamp(0., dimensions.rows.saturating_sub(1) as f32) as u32;
        Point::Viewport(PointCoordinate { x, y })
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = *self
            .metrics
            .get_or_insert_with(|| CellMetrics::measure(SettingsStore::get(cx), window));
        // Keys go to the editor while it shows and to the terminal
        // otherwise, wherever the pane's focus was put.
        let editor_focus = self.editor.focus_handle(cx);
        if self.shows_editor() {
            if self.focus_handle.is_focused(window) {
                window.focus(&editor_focus, cx);
            }
        } else if editor_focus.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        let focused = self.focus_handle.is_focused(window);
        let block_items = self.block_items();
        let frame = match self.renderer.build_frame(&self.terminal, focused) {
            Ok(mut frame) => {
                if let Some(link) = &self.link_hover {
                    frame.set_link(
                        link.row,
                        link.start_col,
                        link.end_col,
                        cx.theme().text_accent,
                    );
                }
                Some(frame)
            }
            Err(error) => {
                log::error!("failed to build terminal frame: {error}");
                None
            }
        };
        // With blocks, the canvas only sizes the terminal and paints the
        // background; the list paints the content.
        let (screen, block_list) = match (block_items, frame) {
            (Some(items), frame) => {
                let background = frame
                    .as_ref()
                    .map_or(cx.theme().terminal_background(), |frame| frame.background);
                let list = block_view::render_list(
                    &self.block_list,
                    Rc::new(items),
                    frame.map(Rc::new),
                    metrics,
                    cx.theme(),
                );
                let editor_panel = self
                    .shows_editor()
                    .then(|| self.render_editor_panel(metrics, cx));
                let content = div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .child(div().flex_1().min_h_0().child(list))
                    .children(editor_panel)
                    .into_any_element();
                (Screen::Blocks(background), Some(content))
            }
            (None, frame) => (Screen::Terminal(frame), None),
        };
        let view = cx.entity();
        let search_bar = self
            .search
            .as_ref()
            .map(|search| self.render_search_bar(search, cx));
        let link_tooltip = self
            .link_hover
            .as_ref()
            .map(|link| self.render_link_tooltip(link, metrics, cx));

        div()
            .id("terminal")
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .cursor(if self.link_hover.is_some() {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            })
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_scrollback))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::previous_prompt))
            .on_action(cx.listener(Self::next_prompt))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        view.update(cx, |view, cx| view.fit_to_bounds(bounds, metrics, cx));
                    },
                    move |bounds, (), window, cx| match screen {
                        Screen::Terminal(Some(frame)) => frame.paint(bounds, metrics, window, cx),
                        Screen::Terminal(None) => {}
                        Screen::Blocks(background) => window.paint_quad(fill(bounds, background)),
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(block_list)
            .children(search_bar)
            .children(link_tooltip)
    }
}

/// What the pane's canvas paints.
enum Screen {
    /// A single terminal screen.
    Terminal(Option<Frame>),
    /// The background behind the block list, in this color.
    Blocks(gpui::Hsla),
}

/// Key bindings for the find bar.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-f", Find, Some("Terminal")),
        KeyBinding::new("cmd-up", PreviousPrompt, Some("Terminal")),
        KeyBinding::new("cmd-down", NextPrompt, Some("Terminal")),
        KeyBinding::new("cmd-g", SearchNext, Some("Terminal")),
        KeyBinding::new("cmd-shift-g", SearchPrevious, Some("Terminal")),
        KeyBinding::new("shift-enter", SearchPrevious, Some(SEARCH_CONTEXT)),
    ]
}

/// A terminal at the pane's current size that answers program queries
/// through `pty` and reports the bell through `bell`.
fn new_terminal(
    pty: &Pty,
    dimensions: Rc<Cell<PtyDimensions>>,
    bell: Rc<Cell<bool>>,
    colors: &TerminalColors,
) -> Result<Terminal<'static, 'static>> {
    let size = dimensions.get();
    let mut terminal = Terminal::new(Options {
        cols: size.cols,
        rows: size.rows,
        max_scrollback: SCROLLBACK_LINES,
    })?;
    terminal.resize(
        size.cols,
        size.rows,
        size.cell_width as u32,
        size.cell_height as u32,
    )?;
    configure_colors(&mut terminal, colors)?;
    register_effects(&mut terminal, pty, dimensions, bell)?;
    Ok(terminal)
}

fn configure_colors(
    terminal: &mut Terminal<'static, 'static>,
    colors: &TerminalColors,
) -> Result<()> {
    let mut palette: Palette = terminal.default_color_palette()?;
    palette.0[..16].copy_from_slice(&colors.ansi);
    terminal
        .set_default_fg_color(Some(colors.foreground))?
        .set_default_bg_color(Some(colors.background))?
        .set_default_cursor_color(Some(colors.cursor))?
        .set_default_color_palette(Some(palette))?;
    Ok(())
}

/// Install the callbacks libghostty uses to answer queries from programs
/// (device attributes, size reports, ...). Without them vim and tmux stall
/// waiting for replies during startup.
fn register_effects(
    terminal: &mut Terminal<'static, 'static>,
    pty: &Pty,
    dimensions: Rc<Cell<PtyDimensions>>,
    bell: Rc<Cell<bool>>,
) -> Result<()> {
    let replies = pty.input_sender();
    terminal
        .on_bell(move |_terminal| bell.set(true))?
        .on_pty_write(move |_terminal, data| {
            let _ = replies.send(data.to_vec());
        })?
        .on_size(move |_terminal| {
            let current = dimensions.get();
            Some(SizeReportSize {
                rows: current.rows,
                columns: current.cols,
                cell_width: current.cell_width as u32,
                cell_height: current.cell_height as u32,
            })
        })?
        .on_device_attributes(|_terminal| {
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
        .on_xtversion(|_terminal| {
            Some(concat!(
                env!("CARGO_PKG_NAME"),
                " ",
                env!("CARGO_PKG_VERSION")
            ))
        })?
        .on_color_scheme(|_terminal| Some(ColorScheme::Dark))?;
    Ok(())
}

fn to_mouse_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Right => Some(mouse::Button::Right),
        MouseButton::Middle => Some(mouse::Button::Middle),
        _ => None,
    }
}
