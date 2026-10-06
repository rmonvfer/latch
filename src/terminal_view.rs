use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, AsyncApp, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity,
    EventEmitter, FocusHandle, Focusable, FollowMode, KeyBinding, KeyContext, KeyDownEvent,
    KeyUpEvent, ListAlignment, ListState, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    SharedString, Subscription, Task, WeakEntity, Window, actions, anchored, canvas, deferred, div,
    fill, prelude::*, px,
};
use libghostty_vt::{
    Terminal, key, mouse,
    screen::CellWide,
    style::Palette,
    terminal::{Options, Point, PointCoordinate},
};

use crate::{
    agent_resume::AgentSession,
    agents::{Agent, AgentStatus},
    block_filter::{self, FilterOptions},
    block_view::{
        self, BlockAction, FilterBar, FilterChange, Item, ItemContent, OnBlockAction,
        PaintedOutputs, RowMatch,
    },
    blocks::{self, BlockList, BlockPoint, BlockSelection, ListChange, Rows},
    command_editor::{CommandEditor, CommandEditorEvent, Highlight},
    completion::{self, CompletionMenu},
    components::{self, elevated_shadow, icon, icon_button, keybinding},
    control, editor_buffer,
    editor_menus::{self, HistoryMenu, HistorySearch},
    git::{self, DiffStats},
    grid::{CellMetrics, Frame, GridRenderer},
    highlight::{self, CommandIndex, TokenKind},
    history::{self, History},
    hooks::{Bootstrapped, Completion},
    input::{to_mods, translate_keystroke},
    links::{self, LinkTarget},
    process_info,
    runtime::{self, ClientEvent, SessionClient},
    runtime_display::DisplayState,
    runtime_protocol::{
        BlockContext, Blocks, Colors, Dimensions, Launch, Operation, SessionInfo, Snapshot,
    },
    search::SearchMatch,
    selected_blocks::SelectedBlocks,
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
        NextPrompt,
        CopyBlockCommands,
        CopyBlockOutput,
        SelectPreviousBlock,
        SelectNextBlock,
        ExtendBlockSelectionUp,
        ExtendBlockSelectionDown,
        ToggleBlockBookmark,
        PreviousBookmark,
        NextBookmark,
        ScrollToBlockTop,
        ScrollToBlockBottom,
        ToggleBlockFilter,
        FindInBlock,
        OpenBlockMenu
    ]
);

const SEARCH_CONTEXT: &str = "TerminalSearch";
/// Key context of a pane with blocks selected, where arrows move the
/// selection.
const BLOCK_SELECTION_CONTEXT: &str = "BlockSelection";
/// Key context of a pane showing blocks, where block shortcuts apply.
const BLOCKS_CONTEXT: &str = "TerminalBlocks";

const FALLBACK_TITLE: &str = "shell";

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

/// Environment variable telling programs which session they run in.
pub const PANE_ID_VARIABLE: &str = "TERMINAL_PANE_ID";

/// How often uncommitted changes are recounted.
const DIFF_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// A view attached to a persistent session. The terminal is a display mirror;
/// the runtime owns the process, parser, scrollback, and input encoding.
pub struct TerminalView {
    terminal: Terminal<'static, 'static>,
    renderer: GridRenderer,
    display: DisplayState,
    client: Option<SessionClient>,
    dimensions: Dimensions,
    info: SessionInfo,
    connected: bool,
    connection_error: Option<String>,
    input_error: Option<String>,
    stop_requested: bool,
    mouse_tracking: bool,
    attention_serial: Option<u64>,
    focus_handle: FocusHandle,
    mouse: MouseInput,
    selecting: bool,
    bounds: Option<Bounds<Pixels>>,
    metrics: Option<CellMetrics>,
    metadata: TabMetadata,
    _output_task: Task<()>,
    _metadata_task: Task<()>,
    _settings_subscription: Subscription,
    _theme_subscription: Subscription,
    search: Option<SearchBar>,
    /// The link under the pointer while ⌘ is held.
    link_hover: Option<HoveredLink>,
    /// Last pointer position relative to the terminal, for ⌘ presses.
    last_mouse: Option<gpui::Point<Pixels>>,
    /// Secret proving a control request comes from a program in this pane.
    control_token: String,
    diff: Option<DiffStats>,
    branch: Option<String>,
    command_started: Option<Instant>,
    /// The session's command blocks, once the runtime reports them.
    blocks: Option<BlockList>,
    /// The session's default background, which block rows are drawn over.
    background: [u8; 3],
    /// A program has the alternate screen, so it gets the whole pane.
    full_screen: bool,
    /// Repaints a running block's duration once a second.
    duration_tick: Option<Task<()>>,
    /// When the grid's size was last sent, and the pending send of a later
    /// one.
    resize_sent: Option<Instant>,
    resize_task: Option<Task<()>>,
    /// Scroll and layout state of the block list, one item per block.
    block_list: ListState,
    /// Whether the block list is anchored to the input, per the setting
    /// it was made with.
    blocks_from_bottom: bool,
    /// The blocks picked by clicking or with ⌘↑ and ⌘↓.
    selected_blocks: SelectedBlocks,
    /// Text selected in the block list.
    block_selection: Option<BlockSelection>,
    /// The press in the block list being held, until its release.
    block_press: Option<BlockPress>,
    /// Where the block list last painted each block's rows.
    painted_outputs: PaintedOutputs,
    /// The block whose menu is open, and where.
    block_menu: Option<(usize, gpui::Point<Pixels>)>,
    block_filter: Option<BlockFilter>,
    /// Where commands are typed while the shell waits at its prompt.
    editor: Entity<CommandEditor>,
    _editor_subscription: Subscription,
    /// Context of the shell's prompt, once it has drawn one; commands are
    /// typed into it only then.
    prompt: Option<BlockContext>,
    /// A command submitted before the shell was ready.
    queued_command: Option<String>,
    /// What the shell reported about itself.
    shell: Option<Bootstrapped>,
    history: History,
    /// Where Up and Down have moved through history, if they have.
    history_menu: Option<HistoryMenu>,
    /// The editor change a history preview caused is still to be reported,
    /// and must not close the menu.
    previewing_history: bool,
    /// The open Ctrl-R search, if any.
    history_search: Option<HistorySearch>,
    /// Names the shell can run, once indexed.
    commands: Option<Rc<CommandIndex>>,
    /// Text before the cursor that completions were asked for, while the
    /// shell works them out.
    pending_completion: Option<String>,
    /// The newest completions taken from the runtime.
    completion_serial: u64,
    /// Completions shown over the editor.
    completion: Option<CompletionMenu>,
    /// The coding agent session running here, as its hook reported it,
    /// which a restored pane resumes.
    agent_session: Option<AgentSession>,
    /// Whether a coding agent was running at the last metadata refresh.
    agent_was_running: bool,
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
    /// In a pane showing blocks, the search runs over the blocks instead.
    blocks: Option<BlockSearch>,
    _subscription: Subscription,
}

/// A search over blocks: all of them, or the one found within.
struct BlockSearch {
    /// The block searched within, by id; all blocks when `None`.
    scope: Option<u64>,
    /// Matches by block index, in order.
    matches: Vec<(usize, RowMatch)>,
}

/// The filter on one block's output.
struct BlockFilter {
    block: u64,
    input: Entity<TextInput>,
    regex: bool,
    case_sensitive: bool,
    invert: bool,
    /// Lines kept around each matching line.
    context: usize,
    /// The last filtering, reused while the rows and options stay the same.
    cache: RefCell<Option<FilteredRows>>,
    _subscription: Subscription,
}

/// A block's rows as its filter leaves them.
#[derive(Clone)]
struct FilteredRows {
    source: Rows,
    options: FilterOptions,
    rows: Rows,
    /// Positions in `rows` before which rows were left out.
    gaps: Vec<usize>,
    matches: Vec<RowMatch>,
    invalid: bool,
}

impl BlockFilter {
    fn options(&self, cx: &App) -> FilterOptions {
        FilterOptions {
            query: self.input.read(cx).text().to_string(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            invert: self.invert,
            context: self.context,
        }
    }

    /// `rows` filtered by the current options.
    fn apply(&self, rows: &Rows, cx: &App) -> FilteredRows {
        let options = self.options(cx);
        if let Some(cached) = self.cache.borrow().as_ref()
            && Rc::ptr_eq(&cached.source, rows)
            && cached.options == options
        {
            return cached.clone();
        }
        let texts: Vec<String> = rows.iter().map(|row| row.text()).collect();
        let filtered = block_filter::filter(&texts, &options);
        let result = FilteredRows {
            source: rows.clone(),
            rows: Rc::new(filtered.kept.iter().map(|&row| rows[row].clone()).collect()),
            options,
            gaps: filtered.gaps,
            matches: filtered.matches,
            invalid: filtered.invalid,
        };
        *self.cache.borrow_mut() = Some(result.clone());
        result
    }

    fn change(&mut self, change: FilterChange) {
        match change {
            FilterChange::ToggleRegex => self.regex = !self.regex,
            FilterChange::ToggleCaseSensitive => self.case_sensitive = !self.case_sensitive,
            FilterChange::ToggleInvert => self.invert = !self.invert,
            FilterChange::MoreContext => {
                self.context = (self.context + 1).min(block_filter::MAX_CONTEXT);
            }
            FilterChange::LessContext => self.context = self.context.saturating_sub(1),
        }
    }
}

/// A press of the left button over a block.
struct BlockPress {
    /// The block a release over it selects, when the press can select one.
    block: Option<usize>,
    origin: gpui::Point<Pixels>,
    modifiers: Modifiers,
    /// The pointer moved far enough to select text rather than the block.
    dragged: bool,
    /// What a drag selects by, from the click count.
    unit: SelectionUnit,
    /// The cells, word, or row first pressed, kept selected while dragging.
    start: BlockPoint,
    end: BlockPoint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionUnit {
    Cell,
    Word,
    Row,
}

/// What copying a block takes from it.
#[derive(Clone, Copy)]
enum BlockText {
    Command,
    Output,
    CommandAndOutput,
}

/// How far the pointer moves before a press selects text, as in Warp.
const DRAG_THRESHOLD: f32 = 0.5;

struct MouseInput {
    pressed: Option<mouse::Button>,
    scroll_remainder: f32,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    /// Start a persistent shell in `cwd`. `startup` is typed once its prompt is ready.
    pub fn build(cwd: Option<&Path>, startup: Option<&str>, cx: &mut App) -> Result<Entity<Self>> {
        let launch = launch(runtime::new_session_id()?, cwd, startup, cx)?;
        let view = Self::from_session(
            SessionInfo {
                id: launch.id,
                cwd: launch
                    .cwd
                    .clone()
                    .or_else(|| launch.env.get("HOME").map(PathBuf::from)),
                control_token: launch.control_token.clone(),
                ..Default::default()
            },
            cx,
        )?;
        view.update(cx, |view, cx| view.start_session(launch, cx));
        Ok(view)
    }

    /// Start a session in this view's identity again after it was lost
    /// (the runtime stopped, the machine restarted): a fresh shell in the
    /// directory it was last in, running `startup` once it is ready. A
    /// directory that no longer exists is reported rather than replaced.
    pub fn respawn(&mut self, startup: Option<String>, cx: &mut Context<Self>) {
        // A pane that was running an agent session picks it up again.
        let startup = startup.or_else(|| {
            SettingsStore::get(cx)
                .resume_agents
                .then(|| {
                    self.agent_session
                        .as_ref()
                        .map(AgentSession::resume_command)
                })
                .flatten()
        });
        if let Some(cwd) = self.info.cwd.clone().filter(|cwd| !cwd.is_dir()) {
            self.connection_error =
                Some(format!("Saved directory {} is unavailable", cwd.display()));
            cx.emit(TerminalEvent::MetadataChanged);
            cx.notify();
            return;
        }
        match launch(
            self.info.id,
            self.info.cwd.as_deref(),
            startup.as_deref(),
            cx,
        ) {
            Ok(launch) => {
                self.control_token.clone_from(&launch.control_token);
                self.start_session(launch, cx);
            }
            Err(error) => {
                self.connection_error = Some(format!("Could not start session: {error:#}"));
                cx.notify();
            }
        }
    }

    /// Ask the runtime to start `launch`, then show it.
    fn start_session(&mut self, launch: Launch, cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { runtime::create_session(launch) })
                .await;
            let _ = view.update(cx, |view, cx| match result {
                Ok(_) => {
                    view.connection_error = None;
                    if let Some(client) = &view.client {
                        client.refresh();
                    }
                    cx.notify();
                }
                Err(error) => {
                    view.connection_error = Some(format!("Could not start session: {error:#}"));
                    cx.emit(TerminalEvent::MetadataChanged);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Attach to the same session identity, including an unavailable session.
    pub fn attach(session_id: u64, cx: &mut App) -> Result<Entity<Self>> {
        Self::attach_in(session_id, None, None, cx)
    }

    /// Attach to session `session_id`, which ran in `cwd` and, if it had
    /// one, the coding agent session `agent`.
    pub fn attach_in(
        session_id: u64,
        cwd: Option<&Path>,
        agent: Option<AgentSession>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let view = Self::from_session(
            SessionInfo {
                id: session_id,
                cwd: cwd.map(Path::to_path_buf),
                ..Default::default()
            },
            cx,
        )?;
        view.update(cx, |view, _| view.agent_session = agent);
        Ok(view)
    }

    pub fn set_agent_session(&mut self, session: AgentSession, cx: &mut Context<Self>) {
        self.agent_session = Some(session);
        cx.emit(TerminalEvent::MetadataChanged);
    }

    pub fn agent_session(&self) -> Option<&AgentSession> {
        self.agent_session.as_ref()
    }

    fn from_session(info: SessionInfo, cx: &mut App) -> Result<Entity<Self>> {
        let dimensions = Dimensions {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 18,
        };
        let mut terminal = Terminal::new(Options {
            cols: dimensions.cols,
            rows: dimensions.rows,
            max_scrollback: 0,
        })?;
        configure_colors(&mut terminal, &cx.theme().terminal)?;
        let renderer = GridRenderer::new()?;
        let (client, events, connection_error) = match SessionClient::attach(info.id) {
            Ok((client, events)) => (Some(client), events, None),
            Err(error) => {
                let (_, events) = async_channel::bounded(1);
                (None, events, Some(error.to_string()))
            }
        };
        Ok(cx.new(|cx| {
            let editor = cx.new(|cx| CommandEditor::new("", cx));
            let output_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                while let Ok(event) = events.recv().await {
                    if this.update(cx, |view, cx| view.receive(event, cx)).is_err() {
                        break;
                    }
                }
            });
            let metadata_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let Ok(cwd) = this.update(cx, |view, _| view.info.cwd.clone()) else {
                        return;
                    };
                    let (diff, branch) = match cwd.clone() {
                        Some(cwd) => {
                            cx.background_executor()
                                .spawn(async move {
                                    (git::diff_stats(&cwd), process_info::git_branch(&cwd))
                                })
                                .await
                        }
                        None => (None, None),
                    };
                    if this
                        .update(cx, |view, cx| {
                            if view.info.cwd == cwd {
                                view.diff = diff;
                                view.branch = branch;
                                view.refresh_metadata(cx);
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                    cx.background_executor().timer(DIFF_REFRESH_INTERVAL).await;
                }
            });
            Self {
                terminal,
                renderer,
                display: DisplayState::default(),
                client,
                dimensions,
                control_token: info.control_token.clone(),
                connected: false,
                connection_error,
                input_error: None,
                stop_requested: false,
                mouse_tracking: false,
                attention_serial: None,
                focus_handle: cx.focus_handle(),
                mouse: MouseInput {
                    pressed: None,
                    scroll_remainder: 0.,
                },
                selecting: false,
                bounds: None,
                metrics: None,
                metadata: TabMetadata {
                    title: FALLBACK_TITLE.into(),
                    directory: info
                        .cwd
                        .as_deref()
                        .map(|path| process_info::shorten_home(path).into()),
                    cwd: info.cwd.clone(),
                    ..Default::default()
                },
                info,
                _output_task: output_task,
                _metadata_task: metadata_task,
                // Font and padding changes take effect on the next frame.
                _settings_subscription: cx.observe_global::<SettingsStore>(|view, cx| {
                    view.metrics = None;
                    let from_bottom = SettingsStore::get(cx).blocks_from_bottom;
                    if from_bottom != view.blocks_from_bottom {
                        view.blocks_from_bottom = from_bottom;
                        view.block_list =
                            block_list_state(view.block_list.item_count(), from_bottom);
                    }
                    cx.notify();
                }),
                _theme_subscription: cx.observe_global::<ActiveTheme>(|view, cx| {
                    if view.connected {
                        view.send(
                            Operation::Colors {
                                colors: runtime_colors(cx),
                            },
                            cx,
                        );
                    }
                }),
                search: None,
                link_hover: None,
                last_mouse: None,
                diff: None,
                branch: None,
                command_started: None,
                blocks: None,
                background: [0, 0, 0],
                full_screen: false,
                duration_tick: None,
                resize_sent: None,
                resize_task: None,
                block_list: block_list_state(0, SettingsStore::get(cx).blocks_from_bottom),
                blocks_from_bottom: SettingsStore::get(cx).blocks_from_bottom,
                selected_blocks: SelectedBlocks::default(),
                block_selection: None,
                block_press: None,
                painted_outputs: PaintedOutputs::default(),
                block_menu: None,
                block_filter: None,
                _editor_subscription: cx.subscribe(&editor, Self::on_editor_event),
                editor,
                prompt: None,
                queued_command: None,
                shell: None,
                history: History::default(),
                history_menu: None,
                previewing_history: false,
                history_search: None,
                commands: None,
                pending_completion: None,
                completion_serial: 0,
                completion: None,
                agent_session: None,
                agent_was_running: false,
            }
        }))
    }

    pub fn session_id(&self) -> u64 {
        self.info.id
    }

    pub fn is_exited(&self) -> bool {
        self.info.exited
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.stop_requested = true;
        if self.connected {
            self.send(Operation::Stop, cx);
        }
        cx.notify();
    }

    pub fn acknowledge_attention(&mut self, cx: &mut Context<Self>) {
        if self.connected && self.info.attention.is_some() {
            self.send(
                Operation::AcknowledgeAttention {
                    serial: self.info.attention_serial,
                },
                cx,
            );
        }
    }

    fn send(&mut self, operation: Operation, cx: &mut Context<Self>) {
        if let Some(client) = &self.client
            && let Err(error) = client.send(operation)
        {
            self.input_error = Some(error.to_string());
            cx.notify();
        }
    }

    #[tracing::instrument(skip_all)]
    fn receive(&mut self, event: ClientEvent, cx: &mut Context<Self>) {
        match event {
            ClientEvent::Frame(frame) => self.apply_frame(*frame, cx),
            ClientEvent::BlockRows {
                block,
                version,
                from,
                rows,
            } => {
                if let Some(blocks) = &mut self.blocks
                    && let Some(index) = blocks.apply_rows(block, version, from, rows)
                {
                    self.block_list.remeasure_items(index..index + 1);
                }
            }
            ClientEvent::Shell(shell) => {
                if let Some(shell) = *shell {
                    self.index_shell(shell.clone(), cx);
                    self.shell = Some(shell);
                }
            }
            ClientEvent::Text(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            ClientEvent::Matches { query, matches } => {
                if let Some(search) = &mut self.search
                    && search.input.read(cx).text() == query
                {
                    search.matches = matches
                        .into_iter()
                        .map(|found| SearchMatch {
                            row: found.row,
                            start_col: found.start_col,
                            end_col: found.end_col,
                        })
                        .collect();
                    // Start from the newest match, nearest the prompt.
                    search.current = search.matches.len().saturating_sub(1);
                    self.reveal_current_match(cx);
                }
            }
            ClientEvent::Disconnected(error) => {
                self.connected = false;
                self.connection_error = Some(error);
                cx.emit(TerminalEvent::MetadataChanged);
            }
            ClientEvent::Error(error) => self.input_error = Some(error),
        }
        cx.notify();
    }

    #[tracing::instrument(skip_all)]
    fn apply_frame(&mut self, frame: Snapshot, cx: &mut Context<Self>) {
        if let Err(error) = self.display.apply(&mut self.terminal, &frame) {
            self.connection_error = Some(error.to_string());
            return;
        }
        let reconnected = !self.connected;
        if self.bounds.is_none() {
            self.dimensions = frame.dimensions;
        }
        let exited = !self.info.exited && frame.info.exited;
        let attention_changed = frame.info.attention_serial > self.attention_serial.unwrap_or(0);
        self.attention_serial = Some(frame.info.attention_serial);
        self.connected = true;
        self.connection_error = None;
        self.mouse_tracking = frame.mouse_tracking;
        self.background = frame.background;
        self.apply_blocks(frame.blocks.map(|blocks| *blocks), cx);
        self.control_token.clone_from(&frame.info.control_token);
        self.command_started = match frame.info.command_elapsed_ms {
            Some(elapsed) => {
                if self.info.command_serial == frame.info.command_serial {
                    self.command_started
                        .or_else(|| Instant::now().checked_sub(Duration::from_millis(elapsed)))
                } else {
                    Instant::now().checked_sub(Duration::from_millis(elapsed))
                }
            }
            None => None,
        };
        self.info = frame.info;
        if self.info.exited {
            self.stop_requested = false;
        }
        self.refresh_metadata(cx);
        if attention_changed && let Some(event) = &self.info.attention {
            let attention = match event {
                crate::runtime_protocol::Attention::Bell => Attention::Bell,
                crate::runtime_protocol::Attention::Notification { title, body } => {
                    Attention::Notification {
                        title: title.clone(),
                        body: body.clone(),
                    }
                }
                crate::runtime_protocol::Attention::CommandFinished {
                    exit_code,
                    duration_ms,
                } => Attention::CommandFinished(CommandOutcome {
                    exit_code: *exit_code,
                    duration: Duration::from_millis(*duration_ms),
                }),
                crate::runtime_protocol::Attention::AgentWaiting { needs_input, .. } => {
                    match self.metadata.agent {
                        Some(state) => Attention::AgentWaiting {
                            agent: state.agent,
                            needs_input: *needs_input,
                        },
                        None => return,
                    }
                }
            };
            cx.emit(TerminalEvent::Attention(attention));
        }
        if exited {
            cx.emit(TerminalEvent::Exited);
        }
        if reconnected || exited {
            cx.emit(TerminalEvent::MetadataChanged);
        }
        if reconnected {
            if self.stop_requested {
                self.send(Operation::Stop, cx);
            }
            if self.bounds.is_some() {
                self.send(
                    Operation::Resize {
                        dimensions: self.dimensions,
                    },
                    cx,
                );
            }
            self.send(
                Operation::Colors {
                    colors: runtime_colors(cx),
                },
                cx,
            );
        }
    }

    /// Follow the runtime's command blocks: the list, the prompt, and the
    /// shell's completions.
    #[tracing::instrument(skip_all)]
    fn apply_blocks(&mut self, reported: Option<Blocks>, cx: &mut Context<Self>) {
        let Some(reported) = reported else {
            self.blocks = None;
            self.full_screen = false;
            return;
        };
        self.full_screen = reported.full_screen;
        let blocks = self.blocks.get_or_insert_with(BlockList::default);
        match blocks.sync(&reported.items) {
            ListChange::Unchanged => {}
            ListChange::Shifted { dropped, added } => {
                if dropped > 0 {
                    self.block_list.splice(0..dropped, 0);
                    self.selected_blocks.shift(dropped);
                    self.block_selection = self.block_selection.and_then(|selection| {
                        let shift = |point: BlockPoint| {
                            Some(BlockPoint {
                                block: point.block.checked_sub(dropped)?,
                                ..point
                            })
                        };
                        Some(BlockSelection {
                            anchor: shift(selection.anchor)?,
                            head: shift(selection.head)?,
                        })
                    });
                    self.block_press = None;
                }
                let count = self.block_list.item_count();
                self.block_list.splice(count..count, added);
                if added > 0 {
                    // The editor gives way to a new command once it has run
                    // long enough to be more than a flicker.
                    cx.spawn(async move |view, cx| {
                        cx.background_executor().timer(EDITOR_GRACE).await;
                        let _ = view.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                }
            }
            ListChange::Reset => {
                self.block_list.reset(blocks.blocks().len());
                self.selected_blocks.clear();
                self.block_selection = None;
                self.block_press = None;
            }
        }
        let became_ready = self.prompt.is_none() && reported.prompt.is_some();
        self.prompt = reported.prompt;
        if became_ready && let Some(command) = self.queued_command.take() {
            self.run_command(&command, cx);
        }
        if let Some(found) = reported.completions
            && found.serial > self.completion_serial
        {
            self.completion_serial = found.serial;
            if let Some(anchor) = self.pending_completion.take()
                && self.editor.read(cx).text_before_cursor() == anchor
            {
                self.show_completions(anchor, found.prefix, found.matches, cx);
            }
        }
    }

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

    /// Identifies the persistent session to the control API.
    pub fn pane_id(&self) -> u64 {
        self.info.id
    }

    /// Type `text` into the pane as if the user had.
    pub fn send_text(&mut self, text: &str, _cx: &mut Context<Self>) -> Result<()> {
        anyhow::ensure!(
            self.connected && !self.info.exited,
            "session is not accepting input"
        );
        self.client
            .as_ref()
            .context("session is unavailable")?
            .send(Operation::Input {
                bytes: text.as_bytes().to_vec(),
            })
    }

    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds
    }

    /// Current grid size as (columns, rows).
    pub fn grid_size(&self) -> (u16, u16) {
        let dimensions = self.dimensions;
        (dimensions.cols, dimensions.rows)
    }

    pub fn metadata(&self) -> &TabMetadata {
        &self.metadata
    }

    fn previous_prompt(&mut self, _: &PreviousPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_blocks() {
            // In the input, ⌘↑ first moves to its start; from there it
            // selects the newest block.
            let editor = self.editor.clone();
            if editor.focus_handle(cx).is_focused(window)
                && !editor.read(cx).text_before_cursor().is_empty()
            {
                editor.update(cx, |editor, cx| editor.move_to_start(cx));
                return;
            }
            self.select_adjacent_block(false, true, window, cx);
        } else {
            self.send(Operation::Prompt { forward: false }, cx);
        }
    }

    fn next_prompt(&mut self, _: &NextPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_blocks() {
            self.select_adjacent_block(true, true, window, cx);
        } else {
            self.send(Operation::Prompt { forward: true }, cx);
        }
    }

    fn on_block_action(&self, cx: &Context<Self>) -> OnBlockAction {
        let view = cx.entity().downgrade();
        Rc::new(move |action, index, window, cx| {
            let _ = view.update(cx, |view, cx| view.block_action(action, index, window, cx));
        })
    }

    /// Run `command` in the shell, now or once it is ready, and remember it.
    fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        if command.trim().is_empty() {
            return;
        }
        self.selected_blocks.clear();
        self.history.push(command);
        if self.prompt.is_some() {
            self.send(
                Operation::RunCommand {
                    command: command.to_string(),
                },
                cx,
            );
        } else {
            self.queued_command = Some(command.to_string());
        }
    }

    /// Carry out an action on the block at `index`.
    fn block_action(
        &mut self,
        action: BlockAction,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(block) = self
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.blocks().get(index))
        else {
            return;
        };
        let id = block.id;
        let command = block.command.clone();
        let cwd = block.context.cwd.clone();
        let branch = block.context.git_branch.clone();
        if !matches!(action, BlockAction::OpenMenu(_)) {
            self.block_menu = None;
        }
        // Copying and scrolling from a selected block's menu act on every
        // selected block.
        let targets: Vec<usize> = if self.selected_blocks.contains(index) {
            self.selected_blocks.indices().collect()
        } else {
            vec![index]
        };
        match action {
            BlockAction::OpenMenu(position) => {
                // The menu of an unselected block is for that block alone.
                if !self.selected_blocks.contains(index) {
                    self.select_blocks(|selection| selection.select(index), window, cx);
                }
                self.block_menu = Some((index, position));
            }
            BlockAction::ToggleCollapsed => {
                if let Some(block) = self.block_mut(index) {
                    block.collapsed = !block.collapsed;
                }
                self.block_list.remeasure_items(index..index + 1);
            }
            BlockAction::ToggleBookmark => {
                if let Some(block) = self.block_mut(index) {
                    block.bookmarked = !block.bookmarked;
                }
            }
            BlockAction::CopyAll => {
                let text = self.blocks_text(&targets, BlockText::CommandAndOutput, cx);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            BlockAction::CopyCommand => {
                let text = self.blocks_text(&targets, BlockText::Command, cx);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            BlockAction::CopyOutput => {
                let text = self.blocks_text(&targets, BlockText::Output, cx);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            BlockAction::CopyDirectory => {
                if let Some(cwd) = cwd {
                    cx.write_to_clipboard(ClipboardItem::new_string(cwd.display().to_string()));
                }
            }
            BlockAction::CopyBranch => {
                if let Some(branch) = branch {
                    cx.write_to_clipboard(ClipboardItem::new_string(branch));
                }
            }
            BlockAction::Rerun => self.run_command(&command, cx),
            BlockAction::ScrollToTop => {
                let top = targets.first().copied().unwrap_or(index);
                self.block_list.scroll_to(block_view::block_start(top));
            }
            BlockAction::ScrollToBottom => {
                self.scroll_to_block_bottom(targets.last().copied().unwrap_or(index));
            }
            BlockAction::ToggleFilter => self.toggle_block_filter(id, index, window, cx),
            BlockAction::Filter(change) => {
                if let Some(filter) = self
                    .block_filter
                    .as_mut()
                    .filter(|filter| filter.block == id)
                {
                    filter.change(change);
                    self.block_selection = None;
                    self.block_list.remeasure_items(index..index + 1);
                    self.refresh_matches(cx);
                }
            }
            BlockAction::FindInBlock => {
                self.selected_blocks.select(index);
                self.open_search(Some(id), window, cx);
            }
        }
        cx.notify();
    }

    fn block_mut(&mut self, index: usize) -> Option<&mut crate::blocks::Block> {
        self.blocks.as_mut()?.blocks_mut().get_mut(index)
    }

    /// The block at which shortcuts act: the last selected, else the newest.
    fn target_block(&self) -> Option<usize> {
        let count = self.blocks.as_ref()?.blocks().len();
        self.selected_blocks
            .tail()
            .filter(|index| *index < count)
            .or(count.checked_sub(1))
    }

    /// Change which blocks are selected. While any are, the pane takes the
    /// keys from the input, as Warp's block list does.
    fn select_blocks(
        &mut self,
        change: impl FnOnce(&mut SelectedBlocks),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.selected_blocks);
        if self.selected_blocks.is_empty() {
            self.focus_input(window, cx);
        } else {
            self.block_selection = None;
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Drop the selected blocks and text, and give the keys back to the
    /// input.
    fn clear_block_selections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_blocks.clear();
        self.block_selection = None;
        self.focus_input(window, cx);
        cx.notify();
    }

    fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_editor() {
            window.focus(&self.editor.focus_handle(cx), cx);
        }
    }

    /// The text of blocks `indices`, each as `kind` asks, skipping blank
    /// ones, one block after another.
    fn blocks_text(&self, indices: &[usize], kind: BlockText, cx: &App) -> String {
        let Some(blocks) = &self.blocks else {
            return String::new();
        };
        indices
            .iter()
            .filter_map(|&index| {
                let command = blocks.blocks().get(index)?.command.as_str();
                let text = match kind {
                    BlockText::Command => command.to_string(),
                    BlockText::Output => self.block_output_text(index, cx),
                    BlockText::CommandAndOutput if command.is_empty() => {
                        self.block_output_text(index, cx)
                    }
                    BlockText::CommandAndOutput => {
                        format!("{command}\n{}", self.block_output_text(index, cx))
                    }
                };
                (!text.trim().is_empty()).then_some(text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn copy_block_commands(
        &mut self,
        _: &CopyBlockCommands,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::CopyCommand, window, cx);
    }

    fn select_previous_block(
        &mut self,
        _: &SelectPreviousBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_adjacent_block(false, false, window, cx);
    }

    fn select_next_block(
        &mut self,
        _: &SelectNextBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_adjacent_block(true, false, window, cx);
    }

    fn extend_selection_up(
        &mut self,
        _: &ExtendBlockSelectionUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_block_selection(false, window, cx);
    }

    fn extend_selection_down(
        &mut self,
        _: &ExtendBlockSelectionDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_block_selection(true, window, cx);
    }

    /// Grow or shrink the selected range by a block from its last end.
    fn extend_block_selection(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(tail), Some(count)) = (
            self.selected_blocks.tail(),
            self.blocks.as_ref().map(|blocks| blocks.blocks().len()),
        ) else {
            return;
        };
        let next = if down {
            (tail + 1).min(count.saturating_sub(1))
        } else {
            tail.saturating_sub(1)
        };
        self.select_blocks(|selection| selection.extend_to(next), window, cx);
        self.reveal_block(next);
    }

    /// Scroll so block `index` starts at the top of the list, unless its
    /// top is already in view.
    fn reveal_block(&mut self, index: usize) {
        let viewport = self.block_list.viewport_bounds();
        let top_visible = self
            .block_list
            .bounds_for_item(index)
            .is_some_and(|bounds| {
                bounds.top() >= viewport.top() && bounds.top() < viewport.bottom()
            });
        if !top_visible {
            self.block_list.scroll_to(block_view::block_start(index));
        }
    }

    fn act_on_target_block(
        &mut self,
        action: BlockAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.target_block() {
            Some(index) if self.shows_blocks() => self.block_action(action, index, window, cx),
            _ => cx.propagate(),
        }
    }

    fn copy_block_output(
        &mut self,
        _: &CopyBlockOutput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::CopyOutput, window, cx);
    }

    fn toggle_block_bookmark(
        &mut self,
        _: &ToggleBlockBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ToggleBookmark, window, cx);
    }

    fn scroll_to_block_top(
        &mut self,
        _: &ScrollToBlockTop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ScrollToTop, window, cx);
    }

    fn scroll_to_block_end(
        &mut self,
        _: &ScrollToBlockBottom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ScrollToBottom, window, cx);
    }

    fn toggle_target_filter(
        &mut self,
        _: &ToggleBlockFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::ToggleFilter, window, cx);
    }

    fn find_in_target_block(
        &mut self,
        _: &FindInBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.act_on_target_block(BlockAction::FindInBlock, window, cx);
    }

    fn open_target_menu(&mut self, _: &OpenBlockMenu, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.target_block() else {
            return;
        };
        // From the keyboard the menu opens by the block's top, or failing
        // that the pane's.
        let position = self
            .block_list
            .bounds_for_item(index)
            .map(|bounds| bounds.origin + gpui::point(px(40.), px(24.)))
            .or(self
                .bounds
                .map(|bounds| bounds.origin + gpui::point(px(40.), px(24.))))
            .unwrap_or_default();
        self.block_action(BlockAction::OpenMenu(position), index, window, cx);
    }

    fn previous_bookmark(&mut self, _: &PreviousBookmark, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_bookmark(false, cx);
    }

    fn next_bookmark(&mut self, _: &NextBookmark, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_bookmark(true, cx);
    }

    /// Select and scroll to the nearest bookmarked block before (or after)
    /// the selected one.
    fn jump_to_bookmark(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(blocks) = &self.blocks else {
            return;
        };
        let bookmarked: Vec<usize> = blocks
            .blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| block.bookmarked)
            .map(|(index, _)| index)
            .collect();
        let from = self.selected_blocks.tail();
        let next = if forward {
            bookmarked
                .iter()
                .copied()
                .find(|index| from.is_none_or(|from| *index > from))
        } else {
            bookmarked
                .iter()
                .rev()
                .copied()
                .find(|index| from.is_none_or(|from| *index < from))
        };
        if let Some(index) = next {
            self.selected_blocks.select(index);
            self.block_list.scroll_to(block_view::block_start(index));
            cx.notify();
        }
    }

    /// Scroll so the last rows of the block at `index` sit at the bottom
    /// of the list.
    fn scroll_to_block_bottom(&mut self, index: usize) {
        let viewport = self.block_list.viewport_bounds().size.height;
        let height = self
            .block_list
            .bounds_for_item(index)
            .map(|bounds| bounds.size.height)
            .unwrap_or(viewport);
        self.block_list.scroll_to(gpui::ListOffset {
            item_ix: index,
            offset_in_item: (height - viewport).max(px(0.)),
        });
    }

    /// Filter the output of block `id` (at `index`), or stop filtering it.
    fn toggle_block_filter(
        &mut self,
        id: u64,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .block_filter
            .as_ref()
            .is_some_and(|filter| filter.block == id)
        {
            self.close_block_filter(window, cx);
            return;
        }
        let input = cx.new(|cx| TextInput::new("Filter output", cx));
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                move |view, _, event, window, cx| match event {
                    TextInputEvent::Changed => {
                        view.block_selection = None;
                        view.block_list.remeasure_items(index..index + 1);
                        view.refresh_matches(cx);
                        cx.notify();
                    }
                    TextInputEvent::Confirmed => {}
                    TextInputEvent::Cancelled => view.close_block_filter(window, cx),
                },
            );
        window.focus(&input.focus_handle(cx), cx);
        self.block_selection = None;
        self.block_filter = Some(BlockFilter {
            block: id,
            input,
            regex: false,
            case_sensitive: false,
            invert: false,
            context: 0,
            cache: RefCell::new(None),
            _subscription: subscription,
        });
        self.block_list.remeasure_items(index..index + 1);
    }

    fn close_block_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(filter) = self.block_filter.take() else {
            return;
        };
        self.block_selection = None;
        if let Some(index) = self.block_index(filter.block) {
            self.block_list.remeasure_items(index..index + 1);
        }
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn block_index(&self, id: u64) -> Option<usize> {
        self.blocks
            .as_ref()?
            .blocks()
            .iter()
            .position(|block| block.id == id)
    }

    /// The filter's query for block `id`, if it is filtered by one.
    fn filter_query(&self, id: u64, cx: &App) -> Option<String> {
        self.filter_of(id)
            .map(|filter| filter.input.read(cx).text().to_string())
            .filter(|query| !query.is_empty())
    }

    fn filter_of(&self, id: u64) -> Option<&BlockFilter> {
        self.block_filter
            .as_ref()
            .filter(|filter| filter.block == id)
    }

    /// A block's rows as shown: all of them, or those its filter keeps.
    fn shown_rows(&self, block: &crate::blocks::Block, cx: &App) -> Rows {
        let rows = block.rows();
        match self.filter_of(block.id) {
            Some(filter) => filter.apply(&rows, cx).rows,
            None => rows,
        }
    }

    /// The text of the block at `index` as shown, one line per row.
    fn block_output_text(&self, index: usize, cx: &App) -> String {
        self.block_row_texts(index, cx)
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Matches of `query` in the shown rows of block `scope`, or of every
    /// block, ignoring case.
    fn block_matches(&self, query: &str, scope: Option<u64>, cx: &App) -> Vec<(usize, RowMatch)> {
        let Some(blocks) = &self.blocks else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for (index, block) in blocks.blocks().iter().enumerate() {
            if scope.is_some_and(|scope| scope != block.id) {
                continue;
            }
            for (row, text) in self.block_row_texts(index, cx).iter().enumerate() {
                let lower = text.to_lowercase();
                for (start, found) in lower.match_indices(&query) {
                    let start_col = editor_buffer::columns(&lower[..start]);
                    matches.push((
                        index,
                        RowMatch {
                            row,
                            start: start_col,
                            end: start_col + editor_buffer::columns(found),
                            current: false,
                        },
                    ));
                }
            }
        }
        matches
    }

    /// Scroll the current block match into view.
    fn reveal_block_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.search else {
            return;
        };
        let Some((index, found)) = search
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.matches.get(search.current))
            .copied()
        else {
            return;
        };
        let Some(metrics) = self.metrics else {
            return;
        };
        let has_header = self
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.blocks().get(index))
            .is_some_and(|block| !block.command.is_empty());
        let top = if has_header {
            block_view::output_top(metrics)
        } else {
            metrics.height * 0.5
        };
        // Leave a few rows above the match for context.
        let offset = top + metrics.height * found.row.saturating_sub(3) as f32;
        self.block_list.scroll_to(gpui::ListOffset {
            item_ix: index,
            offset_in_item: offset,
        });
        cx.notify();
    }

    /// The menu of the block at `index`, at `position`.
    fn render_block_menu(
        &self,
        index: usize,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let block = self.blocks.as_ref()?.blocks().get(index)?;
        let theme = cx.theme().clone();
        let filtered = self.filter_query(block.id, cx).is_some();
        let has_command = !block.command.is_empty();
        let has_cwd = block.context.cwd.is_some();
        let has_branch = block.context.git_branch.is_some();
        let bookmarked = block.bookmarked;
        let collapsed = block.collapsed;
        let running = block.is_running();
        // For several selected blocks, the menu copies and scrolls them all
        // and leaves out what only makes sense for one.
        let several = self.selected_blocks.contains(index) && self.selected_blocks.len() > 1;
        let single = !several;
        let item = |id: &'static str,
                    icon_name: &'static str,
                    label: &'static str,
                    shortcut: Option<&'static str>,
                    action: BlockAction| {
            components::menu_item(id, icon_name, label, &theme)
                .children(shortcut.map(|shortcut| {
                    div()
                        .flex_1()
                        .flex()
                        .justify_end()
                        .pl(px(16.))
                        .child(keybinding(shortcut, &theme))
                }))
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    view.block_action(action, index, window, cx);
                }))
        };
        let copy_output_label = if filtered {
            "Copy filtered output"
        } else {
            "Copy output"
        };
        let menu = components::menu_surface(&theme)
            .id("block-menu")
            .occlude()
            .min_w(px(280.))
            // Clicks in the menu are its own, not a click on the blocks.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                view.block_menu = None;
                cx.notify();
            }))
            .child(item(
                "block-copy",
                "copy",
                "Copy",
                Some("⌘C"),
                BlockAction::CopyAll,
            ))
            .when(has_command || several, |menu| {
                menu.child(item(
                    "block-copy-command",
                    "terminal",
                    if several {
                        "Copy commands"
                    } else {
                        "Copy command"
                    },
                    Some("⌘⇧C"),
                    BlockAction::CopyCommand,
                ))
            })
            .child(item(
                "block-copy-output",
                "copy",
                if several {
                    "Copy outputs"
                } else {
                    copy_output_label
                },
                Some("⌥⌘⇧C"),
                BlockAction::CopyOutput,
            ))
            .when(has_cwd && single, |menu| {
                menu.child(item(
                    "block-copy-cwd",
                    "folder",
                    "Copy working directory",
                    None,
                    BlockAction::CopyDirectory,
                ))
            })
            .when(has_branch && single, |menu| {
                menu.child(item(
                    "block-copy-branch",
                    "git-branch",
                    "Copy git branch",
                    None,
                    BlockAction::CopyBranch,
                ))
            })
            .child(components::menu_separator(&theme))
            .when(single, |menu| {
                menu.child(item(
                    "block-find",
                    "search",
                    "Find within block",
                    Some("⌘⇧F"),
                    BlockAction::FindInBlock,
                ))
                .child(item(
                    "block-filter",
                    "list-filter",
                    "Toggle block filter",
                    Some("⌥⇧F"),
                    BlockAction::ToggleFilter,
                ))
            })
            .child(item(
                "block-bookmark",
                "bookmark",
                if bookmarked {
                    "Remove bookmark"
                } else {
                    "Bookmark"
                },
                Some("⌘⇧B"),
                BlockAction::ToggleBookmark,
            ))
            .child(components::menu_separator(&theme))
            .child(item(
                "block-top",
                "arrow-up-to-line",
                if several {
                    "Scroll to top of blocks"
                } else {
                    "Scroll to top of block"
                },
                Some("⌘⇧↑"),
                BlockAction::ScrollToTop,
            ))
            .child(item(
                "block-bottom",
                "arrow-down-to-line",
                if several {
                    "Scroll to bottom of blocks"
                } else {
                    "Scroll to bottom of block"
                },
                Some("⌘⇧↓"),
                BlockAction::ScrollToBottom,
            ))
            .when(has_command && single, |menu| {
                menu.child(components::menu_separator(&theme))
                    .when(!running, |menu| {
                        menu.child(item(
                            "block-rerun",
                            "rotate-ccw",
                            "Run again",
                            None,
                            BlockAction::Rerun,
                        ))
                    })
                    .child(item(
                        "block-collapse",
                        if collapsed {
                            "chevron-right"
                        } else {
                            "chevron-down"
                        },
                        if collapsed { "Expand" } else { "Collapse" },
                        None,
                        BlockAction::ToggleCollapsed,
                    ))
            });
        Some(
            deferred(anchored().position(position).snap_to_window().child(menu))
                .with_priority(1)
                .into_any_element(),
        )
    }

    /// Select the block before (or after) the last selected one, or with
    /// none selected, the newest, and bring its top into view. Past the
    /// newest block, the newest is scrolled fully into view, and once it
    /// is, `to_input` hands the keys back to the input.
    fn select_adjacent_block(
        &mut self,
        forward: bool,
        to_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(count) = self.blocks.as_ref().map(|blocks| blocks.blocks().len()) else {
            return;
        };
        let next = match (self.selected_blocks.tail(), forward) {
            (None, false) => count.checked_sub(1),
            (None, true) => None,
            (Some(index), false) => Some(index.saturating_sub(1)),
            (Some(index), true) if index + 1 < count => Some(index + 1),
            (Some(index), true) => {
                let viewport = self.block_list.viewport_bounds();
                let fully_visible = self
                    .block_list
                    .bounds_for_item(index)
                    .is_some_and(|bounds| bounds.bottom() <= viewport.bottom());
                if !fully_visible {
                    self.scroll_to_block_bottom(index);
                    cx.notify();
                } else if to_input && self.shows_editor() {
                    self.clear_block_selections(window, cx);
                }
                return;
            }
        };
        if let Some(index) = next {
            self.select_blocks(|selection| selection.select(index), window, cx);
            self.reveal_block(index);
        }
    }

    /// Ask for completions of the word before the cursor: from zsh's own
    /// completion system when it is idle at its prompt, otherwise from
    /// command names and paths.
    fn request_completions(&mut self, cx: &mut Context<Self>) {
        let before = self.editor.read(cx).text_before_cursor().to_string();
        let shell_idle = self.prompt.is_some()
            && self
                .blocks
                .as_ref()
                .is_some_and(|blocks| blocks.running().is_none());
        let zsh = self
            .shell
            .as_ref()
            .is_some_and(|shell| shell.shell == "zsh");
        if zsh && shell_idle {
            self.pending_completion = Some(before.clone());
            self.send(Operation::Complete { text: before }, cx);
            return;
        }
        let cwd = self.prompt.as_ref().and_then(|prompt| prompt.cwd.clone());
        let (prefix, matches) =
            completion::local_completions(&before, self.commands.as_deref(), cwd.as_deref());
        self.show_completions(before, prefix, matches, cx);
    }

    /// Complete at once when there is one match; otherwise extend the word
    /// by what every match shares and list them.
    fn show_completions(
        &mut self,
        anchor: String,
        prefix: String,
        matches: Vec<Completion>,
        cx: &mut Context<Self>,
    ) {
        let mut menu = CompletionMenu::new(anchor.clone(), prefix.clone(), matches);
        match menu.visible().count() {
            0 => return,
            1 => {
                if let Some((len, text)) = menu.apply(&anchor) {
                    self.editor.update(cx, |editor, cx| {
                        editor.replace_before_cursor(len, &text, cx)
                    });
                }
                return;
            }
            _ => {}
        }
        let shared = common_prefix(
            menu.visible()
                .map(|(_, completion)| completion.word.as_str()),
        );
        if shared.len() > prefix.len() && anchor.ends_with(&prefix) {
            let before = format!("{}{shared}", &anchor[..anchor.len() - prefix.len()]);
            self.editor.update(cx, |editor, cx| {
                editor.replace_before_cursor(prefix.len(), &shared, cx)
            });
            menu.refine(&before);
        }
        self.completion = Some(menu);
        self.history_search = None;
        self.sync_editor_menu(cx);
        cx.notify();
    }

    fn refine_completions(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.completion else {
            return;
        };
        let before = self.editor.read(cx).text_before_cursor().to_string();
        if !menu.refine(&before) || menu.is_empty() {
            self.completion = None;
            self.sync_editor_menu(cx);
        }
        cx.notify();
    }

    fn accept_completion(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.completion.take() else {
            return;
        };
        let before = self.editor.read(cx).text_before_cursor().to_string();
        self.sync_editor_menu(cx);
        if let Some((len, text)) = menu.apply(&before) {
            self.editor.update(cx, |editor, cx| {
                editor.replace_before_cursor(len, &text, cx)
            });
        }
        cx.notify();
    }

    /// Tell the editor whether a menu takes its Up, Down, and Enter.
    fn sync_editor_menu(&mut self, cx: &mut Context<Self>) {
        let open = self.completion.is_some()
            || self.history_search.is_some()
            || self.history_menu.is_some();
        self.editor
            .update(cx, |editor, _| editor.set_menu_open(open));
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

    /// Move through the history menu, opening it on the first Up with the
    /// entries that start with what was typed, and show the selected entry
    /// in the editor. Moving down past the newest entry closes the menu and
    /// brings back what was typed.
    fn step_history_menu(&mut self, older: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.history_menu.as_mut() else {
            if older {
                let original = self.editor.read(cx).text().to_string();
                let matches = self.history.starting_with(&original, HISTORY_MENU_ENTRIES);
                if !matches.is_empty() {
                    // The first Up shows the newest entry.
                    self.history_menu = Some(HistoryMenu {
                        original,
                        matches,
                        selected: 0,
                    });
                    self.preview_history(cx);
                }
            }
            return;
        };
        if older {
            menu.selected = (menu.selected + 1).min(menu.matches.len() - 1);
        } else if menu.selected == 0 {
            self.close_history_menu(true, cx);
            return;
        } else {
            menu.selected -= 1;
        }
        self.preview_history(cx);
    }

    /// Put the history menu's selection in the editor.
    fn preview_history(&mut self, cx: &mut Context<Self>) {
        let Some(command) = self
            .history_menu
            .as_ref()
            .and_then(|menu| menu.matches.get(menu.selected))
            .and_then(|&index| self.history.get(index))
            .map(|entry| entry.command.clone())
        else {
            return;
        };
        self.previewing_history = true;
        self.sync_editor_menu(cx);
        self.editor
            .update(cx, |editor, cx| editor.set_text(command, cx));
        cx.notify();
    }

    /// Close the history menu, bringing back what was typed if `restore`.
    fn close_history_menu(&mut self, restore: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.history_menu.take() else {
            return;
        };
        self.sync_editor_menu(cx);
        if restore {
            self.previewing_history = true;
            self.editor
                .update(cx, |editor, cx| editor.set_text(menu.original, cx));
        }
        cx.notify();
    }

    fn update_history_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &mut self.history_search else {
            return;
        };
        let query = self.editor.read(cx).text();
        search.matches = self.history.search(query, HISTORY_SEARCH_RESULTS);
        search.selected = 0;
        cx.notify();
    }

    /// Put the selected search match in the editor.
    fn pick_history_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        self.sync_editor_menu(cx);
        if let Some(entry) = search
            .matches
            .get(search.selected)
            .and_then(|&index| self.history.get(index))
        {
            let command = entry.command.clone();
            self.editor
                .update(cx, |editor, cx| editor.set_text(command, cx));
        }
        cx.notify();
    }

    /// Refresh the editor's highlighting and history suggestion.
    fn decorate_editor(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let suggestion = self
            .history
            .suggestion(&text)
            .filter(|_| self.history_search.is_none() && self.history_menu.is_none())
            .map(str::to_string);
        let highlights = match &self.commands {
            Some(commands) => {
                let colors = &cx.theme().terminal;
                let cwd = self
                    .prompt
                    .as_ref()
                    .and_then(|prompt| prompt.cwd.as_deref());
                // Warp's scheme: commands green, flags yellow, arguments
                // cyan, variables magenta; a command the shell cannot run
                // is underlined in red.
                let ansi = |index: usize| Some(theme::to_hsla(colors.ansi[index]));
                highlight::highlight(&text, commands, cwd)
                    .into_iter()
                    .map(|token| {
                        let (color, underline) = match token.kind {
                            TokenKind::Command => (ansi(2), None),
                            TokenKind::UnknownCommand => (None, ansi(1)),
                            TokenKind::Flag => (ansi(3), None),
                            TokenKind::Argument => (ansi(6), None),
                            TokenKind::Variable => (ansi(5), None),
                            TokenKind::Operator => (None, None),
                        };
                        Highlight {
                            range: token.range,
                            color,
                            underline,
                        }
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
    #[tracing::instrument(skip_all)]
    fn render_editor_panel(&self, metrics: CellMetrics, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let context = self.prompt.clone().unwrap_or_default();
        let mut context = context;
        if context.git_branch.is_none() {
            context.git_branch.clone_from(&self.branch);
        }
        let chips = block_view::context_chips(&context, self.diff, metrics, theme);
        // A menu opens above the input, over the blocks.
        let menu = if let Some(menu) = &self.completion {
            Some(editor_menus::render_completions(menu, metrics, theme))
        } else if let Some(menu) = &self.history_menu {
            Some(editor_menus::render_history_menu(
                menu,
                &self.history,
                metrics,
                theme,
            ))
        } else {
            self.history_search.as_ref().map(|search| {
                editor_menus::render_history_search(
                    search,
                    &self.history,
                    self.editor.read(cx).text(),
                    metrics,
                    theme,
                )
            })
        };
        let foreground = theme::to_hsla(theme.terminal.foreground);
        // Like Warp's input: set off from the blocks by a hairline, with the
        // chips and the command lined up with the blocks' text.
        div()
            .flex_none()
            .relative()
            .flex()
            .flex_col()
            // Warp's spacing: a block's top padding (1.1 lines, less the
            // hairline) at 60% above the chips, 15px between them and the
            // command, and 24px below it.
            .gap(px(15.))
            .pl(metrics.padding + block_view::HORIZONTAL_INSET)
            .pr(px(16.))
            .pt((metrics.height * 1.1 - px(1.)) * 0.6)
            .pb(px(24.))
            .border_t_1()
            .border_color(foreground.opacity(0.1))
            .children(menu.map(|menu| {
                div()
                    .absolute()
                    .bottom(gpui::relative(1.))
                    .left_0()
                    .pb(px(6.))
                    .child(menu)
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .children(chips),
            )
            .child(self.editor.clone())
            .into_any_element()
    }

    fn on_editor_event(
        &mut self,
        _: Entity<CommandEditor>,
        event: &CommandEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            CommandEditorEvent::Confirmed => {
                if self.completion.is_some() {
                    self.accept_completion(cx);
                } else if self.history_menu.is_some() {
                    self.close_history_menu(false, cx);
                } else {
                    self.pick_history_match(cx);
                }
            }
            CommandEditorEvent::Complete => match &mut self.completion {
                Some(menu) => {
                    menu.select_next();
                    cx.notify();
                }
                None => self.request_completions(cx),
            },
            CommandEditorEvent::CompletePrevious => {
                if let Some(menu) = &mut self.completion {
                    menu.select_previous();
                    cx.notify();
                }
            }
            CommandEditorEvent::Submitted(command) => self.run_command(command, cx),
            CommandEditorEvent::EndOfFile => self.send(Operation::Input { bytes: vec![4] }, cx),
            CommandEditorEvent::HistoryPrevious if self.completion.is_some() => {
                if let Some(menu) = &mut self.completion {
                    menu.select_previous();
                }
                cx.notify();
            }
            CommandEditorEvent::HistoryNext if self.completion.is_some() => {
                if let Some(menu) = &mut self.completion {
                    menu.select_next();
                }
                cx.notify();
            }
            CommandEditorEvent::HistoryPrevious => match &mut self.history_search {
                Some(search) => {
                    search.selected =
                        (search.selected + 1).min(search.matches.len().saturating_sub(1));
                    cx.notify();
                }
                None => self.step_history_menu(true, cx),
            },
            CommandEditorEvent::HistoryNext => match &mut self.history_search {
                Some(search) => {
                    search.selected = search.selected.saturating_sub(1);
                    cx.notify();
                }
                None => self.step_history_menu(false, cx),
            },
            CommandEditorEvent::SearchHistory => {
                self.completion = None;
                self.close_history_menu(false, cx);
                if self.history_search.take().is_none() {
                    self.history_search = Some(HistorySearch::default());
                    self.update_history_search(cx);
                }
                self.sync_editor_menu(cx);
                cx.notify();
            }
            CommandEditorEvent::Escaped => {
                if self.history_menu.is_some() {
                    self.close_history_menu(true, cx);
                } else if self.completion.take().is_some() || self.history_search.take().is_some() {
                    self.sync_editor_menu(cx);
                } else {
                    self.selected_blocks.clear();
                    self.block_selection = None;
                }
                cx.notify();
            }
            CommandEditorEvent::Changed => {
                // Typing past a previewed entry keeps it and closes the menu.
                if std::mem::take(&mut self.previewing_history) {
                    self.decorate_editor(cx);
                    return;
                }
                self.close_history_menu(false, cx);
                self.update_history_search(cx);
                self.refine_completions(cx);
                self.decorate_editor(cx);
            }
        }
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

    /// The list items to show, when the pane shows blocks.
    #[tracing::instrument(skip_all)]
    fn block_items(&mut self, cx: &App) -> Option<Vec<Item>> {
        if !self.shows_blocks() {
            return None;
        }
        let blocks = self.blocks.as_ref()?;
        let mut items = Vec::with_capacity(blocks.blocks().len());
        let matches = self.search.as_ref().and_then(|search| {
            search
                .blocks
                .as_ref()
                .map(|blocks| (blocks, search.current))
        });
        for block in blocks.blocks() {
            let index = items.len();
            let filtered = self
                .filter_of(block.id)
                .map(|filter| (filter, filter.apply(&block.rows(), cx)));
            let rows = filtered
                .as_ref()
                .map_or_else(|| block.rows(), |(_, view)| view.rows.clone());
            let content = if block.is_running() {
                ItemContent::Running(vec![rows])
            } else {
                ItemContent::Finished(rows)
            };
            let border = self.selected_blocks.border(index);
            // Started at the top, the first block sits against the pane's
            // edge, where a divider would read as a second border; stacked
            // up from the input, it divides the blocks from the space above.
            let divider = !items.is_empty() || self.blocks_from_bottom;
            let (filter, gaps, filter_matches) = match filtered {
                Some((filter, view)) => (
                    Some(FilterBar {
                        input: filter.input.clone(),
                        regex: filter.regex,
                        case_sensitive: filter.case_sensitive,
                        invert: filter.invert,
                        context: filter.context,
                        invalid: view.invalid,
                        kept: view.rows.len(),
                        total: view.source.len(),
                    }),
                    view.gaps,
                    view.matches,
                ),
                None => (None, Vec::new(), Vec::new()),
            };
            let search_matches: Vec<RowMatch> = matches
                .map(|(search, current)| {
                    search
                        .matches
                        .iter()
                        .enumerate()
                        .filter(|(_, (at, _))| *at == index)
                        .map(|(position, (_, found))| RowMatch {
                            current: position == current,
                            ..*found
                        })
                        .collect()
                })
                .unwrap_or_default();
            let block_matches = filter_matches.into_iter().chain(search_matches).collect();
            items.push(
                Item::block(block, content, border)
                    .with_selection(self.block_selection)
                    .with_divider(divider)
                    .with_filter(filter, gaps)
                    .with_matches(block_matches),
            );
        }
        // A running block is drawn from the live terminal and grows with it.
        if blocks.running().is_some() {
            let last = items.len() - 1;
            self.block_list.remeasure_items(last..items.len());
        }
        Some(items)
    }

    /// The pane's key context: block shortcuts apply where blocks show,
    /// and arrows move the selection while blocks are selected.
    fn key_context(&self) -> KeyContext {
        let mut context = KeyContext::default();
        context.add("Terminal");
        if self.shows_blocks() {
            context.add(BLOCKS_CONTEXT);
            if !self.selected_blocks.is_empty() {
                context.add(BLOCK_SELECTION_CONTEXT);
            }
        }
        context
    }

    /// Whether the pane shows its block list rather than a single terminal
    /// screen, which full-screen programs still get.
    fn shows_blocks(&self) -> bool {
        self.blocks.is_some() && !self.full_screen
    }

    fn refresh_metadata(&mut self, cx: &mut Context<Self>) {
        let metadata = self.read_metadata();
        // A session ends when its agent quits; the hook reports a new one
        // when an agent starts again.
        let agent_running = metadata.agent.is_some();
        if self.agent_was_running && !agent_running {
            self.agent_session = None;
        }
        self.agent_was_running = agent_running;
        if metadata != self.metadata {
            self.metadata = metadata;
            cx.emit(TerminalEvent::MetadataChanged);
        }
    }

    fn read_metadata(&self) -> TabMetadata {
        let info = &self.info;
        let title = if info.title.is_empty() {
            info.process.as_deref().unwrap_or(FALLBACK_TITLE)
        } else {
            &info.title
        };
        // The runtime asks the kernel rather than trusting OSC 7: programs
        // must not choose the paths used by new tabs, splits, or session restore.
        TabMetadata {
            title: title.to_owned().into(),
            directory: info
                .cwd
                .as_deref()
                .map(|path| process_info::shorten_home(path).into()),
            cwd: info.cwd.clone(),
            branch: self.branch.clone().map(Into::into),
            process: info.process.clone().map(Into::into),
            running: !info.exited
                && info.foreground_pid.is_some()
                && info.foreground_pid != info.shell_pid,
            command_started: self.command_started,
            last_command: info
                .last_command_duration_ms
                .map(|duration| CommandOutcome {
                    exit_code: info.last_command_exit,
                    duration: Duration::from_millis(duration),
                }),
            diff: self.diff,
            agent: [
                Agent::ClaudeCode,
                Agent::Codex,
                Agent::Gemini,
                Agent::Aider,
                Agent::OpenCode,
                Agent::Amp,
                Agent::Cursor,
                Agent::Goose,
            ]
            .into_iter()
            .find(|agent| Some(agent.name()) == info.agent.as_deref())
            .map(|agent| AgentState {
                agent,
                status: match info.status.as_str() {
                    "working" => AgentStatus::Working,
                    "needs_input" => AgentStatus::NeedsInput,
                    _ => AgentStatus::Idle,
                },
            }),
        }
    }

    /// Fit the terminal grid to the space the layout gave us.
    #[tracing::instrument(skip_all)]
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
        let next = Dimensions {
            cols,
            rows,
            cell_width: f32::from(metrics.width).round() as u16,
            cell_height: f32::from(metrics.height).round() as u16,
        };
        if next == self.dimensions {
            return;
        }
        self.dimensions = next;
        if self.connected {
            self.send_resize(cx);
        }
    }

    /// Tell the runtime the grid's size. Each size change rewraps the
    /// scrollback, so while a window edge is dragged the size is sent at
    /// most once per interval, always ending with the final size.
    fn send_resize(&mut self, cx: &mut Context<Self>) {
        if self.resize_task.is_some() {
            return;
        }
        let wait = self
            .resize_sent
            .map(|sent| RESIZE_INTERVAL.saturating_sub(sent.elapsed()))
            .unwrap_or_default();
        if wait.is_zero() {
            self.resize_sent = Some(Instant::now());
            self.send(
                Operation::Resize {
                    dimensions: self.dimensions,
                },
                cx,
            );
            return;
        }
        self.resize_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(wait).await;
            let _ = view.update(cx, |view, cx| {
                view.resize_task = None;
                view.send_resize(cx);
            });
        }));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Command shortcuts belong to the app, never to the shell, and keys
        // typed into the find bar bubble through here without being sent.
        if event.keystroke.modifiers.platform || !self.focus_handle.is_focused(window) {
            return;
        }
        // With blocks selected, a key drops the selection and goes back to
        // the input, carrying a typed character with it.
        if self.shows_editor() && !self.selected_blocks.is_empty() {
            self.clear_block_selections(window, cx);
            let modifiers = event.keystroke.modifiers;
            if !modifiers.control
                && !modifiers.function
                && let Some(text) = event.keystroke.key_char.as_deref()
                && !text.chars().any(char::is_control)
            {
                self.editor
                    .update(cx, |editor, cx| editor.replace_before_cursor(0, text, cx));
            }
            cx.stop_propagation();
            return;
        }
        let action = if event.is_held {
            key::Action::Repeat
        } else {
            key::Action::Press
        };
        self.send_key(&event.keystroke, action, cx);
        cx.stop_propagation();
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform || !self.focus_handle.is_focused(window) {
            return;
        }
        // The runtime encodes releases only when the terminal protocol requests them.
        self.send_key(&event.keystroke, key::Action::Release, cx);
        cx.stop_propagation();
    }

    fn send_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        action: key::Action,
        cx: &mut Context<Self>,
    ) {
        let translated = translate_keystroke(keystroke);
        let text = if action == key::Action::Release {
            None
        } else {
            translated.text
        };
        self.send(
            Operation::Key {
                key: translated.key as u32,
                action: action as u32,
                mods: translated.mods.bits(),
                consumed_mods: translated.consumed_mods.bits(),
                unshifted: translated.unshifted,
                text,
            },
            cx,
        );
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
        let cols = self.dimensions.cols;
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
        self.open_search(None, window, cx);
    }

    /// Open the find bar: over the screen, or in a pane showing blocks over
    /// all blocks or only the one with id `scope`.
    fn open_search(&mut self, scope: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let blocks = self.shows_blocks().then(|| BlockSearch {
            scope,
            matches: Vec::new(),
        });
        if let Some(search) = &mut self.search {
            let rescoped = blocks.as_ref().map(|blocks| blocks.scope)
                != search.blocks.as_ref().map(|blocks| blocks.scope);
            if rescoped {
                search.blocks = blocks;
            }
            let focus = search.input.focus_handle(cx);
            window.focus(&focus, cx);
            if rescoped {
                self.refresh_matches(cx);
            }
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
            blocks,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            self.send(Operation::ClearSelection, cx);
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let query = search.input.read(cx).text().to_string();
        search.matches.clear();
        search.current = 0;
        if let Some(scope) = search.blocks.as_ref().map(|blocks| blocks.scope) {
            let matches = self.block_matches(&query, scope, cx);
            if let Some(blocks) = self
                .search
                .as_mut()
                .and_then(|search| search.blocks.as_mut())
            {
                blocks.matches = matches;
            }
            self.reveal_block_match(cx);
            cx.notify();
            return;
        }
        self.send(Operation::ClearSelection, cx);
        self.send(Operation::Search { query }, cx);
        cx.notify();
    }

    fn step_match(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let count = match &search.blocks {
            Some(blocks) => blocks.matches.len(),
            None => search.matches.len(),
        };
        if count == 0 {
            return;
        }
        search.current = (search.current as isize + delta).rem_euclid(count as isize) as usize;
        if search.blocks.is_some() {
            self.reveal_block_match(cx);
        } else {
            self.reveal_current_match(cx);
        }
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
            self.send(Operation::ClearSelection, cx);
            cx.notify();
            return;
        };
        self.send(
            Operation::SelectMatch {
                found: crate::runtime_protocol::Match {
                    row: found.row,
                    start_col: found.start_col,
                    end_col: found.end_col,
                },
            },
            cx,
        );
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
        let rows = self.dimensions.rows;
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
        let count = match &search.blocks {
            Some(blocks) => blocks.matches.len(),
            None => search.matches.len(),
        };
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

    /// Copy the selected text, else the selected blocks, each command with
    /// its output.
    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.block_selection_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        if self.shows_blocks() && !self.selected_blocks.is_empty() {
            let selected: Vec<usize> = self.selected_blocks.indices().collect();
            let text = self.blocks_text(&selected, BlockText::CommandAndOutput, cx);
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        self.send(Operation::Copy, cx);
    }

    /// The span a press at `point` selects first: the cell, or the word or
    /// row under it.
    fn selection_span(
        &self,
        point: BlockPoint,
        unit: SelectionUnit,
        cx: &App,
    ) -> (BlockPoint, BlockPoint) {
        let (from, to) = match unit {
            SelectionUnit::Cell => (point.col, point.col),
            SelectionUnit::Row => (0, usize::MAX),
            SelectionUnit::Word => {
                let text = self
                    .block_row_texts(point.block, cx)
                    .into_iter()
                    .nth(point.row)
                    .unwrap_or_default();
                blocks::word_columns(&text, point.col)
            }
        };
        (
            BlockPoint { col: from, ..point },
            BlockPoint { col: to, ..point },
        )
    }

    /// Begin a press over block `block`: it starts selecting text, and a
    /// release without a drag selects the block.
    fn press_block(&mut self, block: usize, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let had_text = self
            .block_selection
            .is_some_and(|selection| !selection.is_empty());
        // A press that only dismisses selected text, or picks a word or a
        // row, leaves the blocks unselected.
        let selects_block = event.click_count == 1 && !had_text;
        if !selects_block {
            self.selected_blocks.clear();
        }
        let unit = match event.click_count {
            1 => SelectionUnit::Cell,
            2 => SelectionUnit::Word,
            _ => SelectionUnit::Row,
        };
        let point = self
            .metrics
            .and_then(|metrics| self.painted_outputs.hit(event.position, metrics));
        let (start, end) = match point {
            Some(point) => self.selection_span(point, unit, cx),
            None => Default::default(),
        };
        self.block_selection = point.map(|_| BlockSelection {
            anchor: start,
            head: end,
        });
        self.block_press = Some(BlockPress {
            block: selects_block.then_some(block),
            origin: event.position,
            modifiers: event.modifiers,
            dragged: unit != SelectionUnit::Cell,
            unit,
            start,
            end,
        });
        cx.notify();
    }

    /// Extend the pressed selection to the pointer, by the press's unit.
    fn drag_block_selection(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(press), Some(metrics)) = (&self.block_press, self.metrics) else {
            return;
        };
        let Some(point) = self.painted_outputs.hit(position, metrics) else {
            return;
        };
        let (start, end, unit) = (press.start, press.end, press.unit);
        let (head_start, head_end) = self.selection_span(point, unit, cx);
        let selection = if point < start {
            BlockSelection {
                anchor: end,
                head: head_start,
            }
        } else {
            BlockSelection {
                anchor: start,
                head: head_end,
            }
        };
        if self.block_selection != Some(selection) {
            self.block_selection = Some(selection);
            cx.notify();
        }
    }

    /// Each row of a block's output as text: its rows, and for a running
    /// block the live screen after them.
    fn block_row_texts(&self, index: usize, cx: &App) -> Vec<String> {
        let Some(block) = self
            .blocks
            .as_ref()
            .and_then(|blocks| blocks.blocks().get(index))
        else {
            return Vec::new();
        };
        let mut texts: Vec<String> = self
            .shown_rows(block, cx)
            .iter()
            .map(|row| row.text())
            .collect();
        if block.is_running()
            && let Ok(frame) = self.renderer_frame_rows()
        {
            texts.extend(frame);
        }
        texts
    }

    /// Text of the live screen's rows down to the last with content.
    fn renderer_frame_rows(&self) -> Result<Vec<String>> {
        let mut renderer = GridRenderer::new()?;
        let frame = renderer.build_frame(&self.terminal, false)?;
        Ok(frame.rows()[..frame.content_rows().min(frame.rows().len())]
            .iter()
            .map(|row| row.text())
            .collect())
    }

    /// The text selected in the block list, if any.
    fn block_selection_text(&self, cx: &App) -> Option<String> {
        let selection = self
            .block_selection
            .filter(|selection| !selection.is_empty())?;
        let (start, end) = selection.range();
        let mut lines = Vec::new();
        for block in start.block..=end.block {
            for (row, text) in self.block_row_texts(block, cx).iter().enumerate() {
                if let Some((from, to)) = selection.columns_in(block, row) {
                    lines.push(blocks::slice_columns(text, from, to).trim_end().to_string());
                }
            }
        }
        Some(lines.join("\n"))
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.send(Operation::Paste { text }, cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_blocks() {
            let count = self
                .blocks
                .as_ref()
                .map_or(0, |blocks| blocks.blocks().len());
            self.select_blocks(|selection| selection.select_all(count), window, cx);
            return;
        }
        self.send(Operation::SelectAll, cx);
    }

    fn clear_scrollback(
        &mut self,
        _: &ClearScrollback,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send(Operation::ClearScrollback, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shows_blocks() {
            let block = self.painted_outputs.block_at(event.position);
            // A press elsewhere than a block, such as in the input, drops
            // the selection and lands in the input.
            let Some(block) = block else {
                self.block_press = None;
                self.selected_blocks.clear();
                self.block_selection = None;
                if self.shows_editor() {
                    window.focus(&self.editor.focus_handle(cx), cx);
                } else {
                    window.focus(&self.focus_handle, cx);
                }
                cx.notify();
                return;
            };
            if !self.focus_handle.contains_focused(window, cx) {
                if self.shows_editor() {
                    window.focus(&self.editor.focus_handle(cx), cx);
                } else {
                    window.focus(&self.focus_handle, cx);
                }
            }
            if event.button == MouseButton::Left {
                self.press_block(block, event, cx);
            }
            return;
        }
        if self.shows_editor() {
            window.focus(&self.editor.focus_handle(cx), cx);
        } else {
            window.focus(&self.focus_handle, cx);
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
            self.selecting = true;
            let Point::Viewport(point) = self.viewport_point(local) else {
                return;
            };
            let Some(metrics) = self.metrics else {
                return;
            };
            self.send(
                Operation::SelectionPress {
                    col: point.x,
                    row: point.y,
                    x: f32::from(local.x).into(),
                    y: f32::from(local.y).into(),
                    cell_width: f32::from(metrics.width).into(),
                },
                cx,
            );
            return;
        }

        let Some(button) = to_mouse_button(event.button) else {
            return;
        };
        self.mouse.pressed = Some(button);
        self.send_mouse(mouse::Action::Press, Some(button), local, mods, cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_blocks() {
            if event.button == MouseButton::Left
                && let Some(press) = self.block_press.take()
            {
                self.release_block_press(press, event, window, cx);
            }
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        let local = event.position - bounds.origin;

        if event.button == MouseButton::Left && self.selecting {
            self.selecting = false;
            if let Point::Viewport(point) = self.viewport_point(local) {
                self.send(
                    Operation::SelectionRelease {
                        col: point.x,
                        row: point.y,
                    },
                    cx,
                );
            }
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

    /// Finish a press in the block list: selected text stays selected
    /// (and is copied, with copy on select), and a click without a drag
    /// selects its block: alone, toggled with ⌘, or as a range with ⇧.
    fn release_block_press(
        &mut self,
        press: BlockPress,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .block_selection
            .is_some_and(|selection| selection.is_empty())
        {
            self.block_selection = None;
        }
        if self.block_selection.is_some() {
            self.selected_blocks.clear();
            if SettingsStore::get(cx).copy_on_select
                && let Some(text) = self.block_selection_text(cx)
            {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.notify();
            return;
        }
        let Some(block) = press
            .block
            .filter(|block| self.painted_outputs.block_at(event.position) == Some(*block))
        else {
            cx.notify();
            return;
        };
        if press.modifiers.platform {
            self.select_blocks(|selection| selection.toggle(block), window, cx);
        } else if press.modifiers.shift && !self.selected_blocks.is_empty() {
            self.select_blocks(|selection| selection.extend_to(block), window, cx);
        } else {
            self.select_blocks(|selection| selection.select(block), window, cx);
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shows_blocks() {
            let Some(press) = &mut self.block_press else {
                return;
            };
            // ⌘ and ⇧ presses pick blocks, so moving during one selects no
            // text.
            if press.modifiers.platform || press.modifiers.shift {
                return;
            }
            if !press.dragged {
                let moved = event.position - press.origin;
                if f32::from(moved.x).abs() <= DRAG_THRESHOLD
                    && f32::from(moved.y).abs() <= DRAG_THRESHOLD
                {
                    return;
                }
                press.dragged = true;
                self.selected_blocks.clear();
            }
            self.drag_block_selection(event.position, cx);
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        let local = event.position - bounds.origin;
        self.last_mouse = Some(local);
        self.update_link_hover(event.modifiers.platform, cx);

        if self.selecting {
            let Some(metrics) = self.metrics else {
                return;
            };
            let Point::Viewport(point) = self.viewport_point(local) else {
                return;
            };
            self.send(
                Operation::SelectionDrag {
                    col: point.x,
                    row: point.y,
                    x: f32::from(local.x).into(),
                    y: f32::from(local.y).into(),
                    rectangle: event.modifiers.alt,
                    cell_width: f32::from(metrics.width) as u32,
                    padding: f32::from(metrics.padding) as u32,
                    height: f32::from(bounds.size.height) as u32,
                },
                cx,
            );
            return;
        }

        if self.mouse_tracking {
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

        if self.mouse_tracking {
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

        self.send(
            Operation::Scroll {
                delta: -(whole as isize),
            },
            cx,
        );
    }

    fn send_mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        local: gpui::Point<Pixels>,
        mods: key::Mods,
        cx: &mut Context<Self>,
    ) {
        let (Some(bounds), Some(metrics)) = (self.bounds, self.metrics) else {
            return;
        };
        self.send(
            Operation::Mouse {
                action: action as u32,
                button: button.map(|button| button as u32),
                mods: mods.bits(),
                x: f32::from(local.x),
                y: f32::from(local.y),
                width: f32::from(bounds.size.width) as u32,
                height: f32::from(bounds.size.height) as u32,
                padding: f32::from(metrics.padding) as u32,
                pressed: self.mouse.pressed.is_some(),
            },
            cx,
        );
    }

    /// Holding shift lets the user select text even when a program has
    /// taken over the mouse.
    fn selection_allowed(&self, modifiers: &Modifiers) -> bool {
        !self.mouse_tracking || modifiers.shift
    }

    fn viewport_point(&self, local: gpui::Point<Pixels>) -> Point {
        let Some(metrics) = self.metrics else {
            return Point::Viewport(PointCoordinate { x: 0, y: 0 });
        };
        let dimensions = self.dimensions;
        let x = (f32::from(local.x - metrics.padding) / f32::from(metrics.width))
            .clamp(0., dimensions.cols.saturating_sub(1) as f32) as u16;
        let y = (f32::from(local.y - metrics.padding) / f32::from(metrics.height))
            .clamp(0., dimensions.rows.saturating_sub(1) as f32) as u32;
        Point::Viewport(PointCoordinate { x, y })
    }
}

impl Render for TerminalView {
    #[tracing::instrument(name = "TerminalView::render", skip_all)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = *self
            .metrics
            .get_or_insert_with(|| CellMetrics::measure(SettingsStore::get(cx), window));
        // Keys go to the editor while it shows and to the terminal
        // otherwise, wherever the pane's focus was put.
        let editor_focus = self.editor.focus_handle(cx);
        if self.shows_editor() {
            if self.focus_handle.is_focused(window) && self.selected_blocks.is_empty() {
                window.focus(&editor_focus, cx);
            }
        } else if editor_focus.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        let focused = self.focus_handle.is_focused(window);
        self.painted_outputs.clear();
        let block_items = self.block_items(cx);
        let running = self
            .blocks
            .as_ref()
            .is_some_and(|blocks| blocks.running().is_some());
        if running && self.duration_tick.is_none() {
            self.duration_tick = Some(cx.spawn(async move |view, cx| {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let _ = view.update(cx, |view, cx| {
                    view.duration_tick = None;
                    cx.notify();
                });
            }));
        }
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
        let (screen, block_content) = match (block_items, frame) {
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
                    self.on_block_action(cx),
                    self.painted_outputs.clone(),
                );
                let editor_panel = self
                    .shows_editor()
                    .then(|| self.render_editor_panel(metrics, cx));
                let block_menu = self
                    .block_menu
                    .and_then(|(index, position)| self.render_block_menu(index, position, cx));
                let content = div()
                    .absolute()
                    .inset_0()
                    .children(block_menu)
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
            .key_context(self.key_context())
            .when(self.shows_blocks(), |this| {
                this.on_action(cx.listener(Self::copy_block_commands))
                    .on_action(cx.listener(Self::copy_block_output))
                    .on_action(cx.listener(Self::toggle_block_bookmark))
                    .on_action(cx.listener(Self::previous_bookmark))
                    .on_action(cx.listener(Self::next_bookmark))
                    .on_action(cx.listener(Self::scroll_to_block_top))
                    .on_action(cx.listener(Self::scroll_to_block_end))
                    .on_action(cx.listener(Self::toggle_target_filter))
                    .on_action(cx.listener(Self::find_in_target_block))
                    .on_action(cx.listener(Self::open_target_menu))
                    .on_action(cx.listener(Self::select_previous_block))
                    .on_action(cx.listener(Self::select_next_block))
                    .on_action(cx.listener(Self::extend_selection_up))
                    .on_action(cx.listener(Self::extend_selection_down))
            })
            .cursor(if self.link_hover.is_some() {
                CursorStyle::PointingHand
            } else if self.shows_blocks() {
                // Over blocks there is nothing to type into; the editor
                // sets its own text cursor.
                CursorStyle::Arrow
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
            .children(block_content)
            .when(
                !self.connected
                    || self.info.exited
                    || self.connection_error.is_some()
                    || self.input_error.is_some()
                    || self.stop_requested,
                |view| {
                    let message = if let Some(error) = &self.input_error {
                        format!("{error} · Click to dismiss")
                    } else if let Some(error) = &self.connection_error {
                        format!("Session unavailable · {error}")
                    } else if self.stop_requested {
                        "Stopping session…".to_owned()
                    } else if self.info.exited {
                        self.info.exit_code.map_or_else(
                            || "Session finished".to_owned(),
                            |code| format!("Session finished · exit {code}"),
                        )
                    } else {
                        "Connecting to session…".to_owned()
                    };
                    view.child(
                        div()
                            .absolute()
                            .bottom(px(8.))
                            .left(px(8.))
                            .px(px(10.))
                            .py(px(6.))
                            .rounded(px(5.))
                            .bg(cx.theme().elevated_surface)
                            .text_color(cx.theme().text_muted)
                            .text_size(px(12.))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, _, _, cx| {
                                    view.input_error = None;
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            .child(message),
                    )
                },
            )
            .children(search_bar)
            .children(link_tooltip)
    }
}

/// The longest start every word shares.
fn common_prefix<'a>(mut words: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut shared = first.len();
    for word in words {
        shared = first
            .char_indices()
            .zip(word.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((index, ch), _)| index + ch.len_utf8())
            .min(shared);
    }
    first[..shared].to_string()
}

/// Matches listed by the Ctrl-R history search.
const HISTORY_SEARCH_RESULTS: usize = 50;
/// Entries the history menu lists.
const HISTORY_MENU_ENTRIES: usize = 200;

/// Shortest time between size changes sent to the runtime: about two
/// frames.
const RESIZE_INTERVAL: Duration = Duration::from_millis(33);

/// How long a command runs before the editor hides and keys go to it.
const EDITOR_GRACE: Duration = Duration::from_millis(50);

/// Scroll state for a block list of `count` blocks that follows new
/// output, anchored to the input or to the top of the pane.
fn block_list_state(count: usize, from_bottom: bool) -> ListState {
    let alignment = if from_bottom {
        ListAlignment::Bottom
    } else {
        ListAlignment::Top
    };
    let state = ListState::new(count, alignment, px(400.));
    state.set_follow_mode(FollowMode::Tail);
    state
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
        // Warp's block shortcuts, where blocks show.
        KeyBinding::new("cmd-shift-c", CopyBlockCommands, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("alt-cmd-shift-c", CopyBlockOutput, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("cmd-shift-b", ToggleBlockBookmark, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("alt-up", PreviousBookmark, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("alt-down", NextBookmark, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("cmd-shift-up", ScrollToBlockTop, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("cmd-shift-down", ScrollToBlockBottom, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("alt-shift-f", ToggleBlockFilter, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("cmd-shift-f", FindInBlock, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("ctrl-m", OpenBlockMenu, Some(BLOCKS_CONTEXT)),
        KeyBinding::new("up", SelectPreviousBlock, Some(BLOCK_SELECTION_CONTEXT)),
        KeyBinding::new("down", SelectNextBlock, Some(BLOCK_SELECTION_CONTEXT)),
        KeyBinding::new(
            "shift-up",
            ExtendBlockSelectionUp,
            Some(BLOCK_SELECTION_CONTEXT),
        ),
        KeyBinding::new(
            "shift-down",
            ExtendBlockSelectionDown,
            Some(BLOCK_SELECTION_CONTEXT),
        ),
    ]
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

/// How to start a login shell in `cwd` for session `id`, running `startup`
/// once its prompt is ready.
fn launch(id: u64, cwd: Option<&Path>, startup: Option<&str>, cx: &App) -> Result<Launch> {
    let mut command = shell_integration::shell_command(ShellIntegration::active_dir(cx).as_deref());
    let control_token = control::new_token()?;
    command.env(control::TOKEN_VARIABLE, &control_token);
    control::configure_command(&mut command, cx);
    Ok(Launch {
        id,
        argv: command
            .get_argv()
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .context("shell command contains invalid Unicode")
            })
            .collect::<Result<_>>()?,
        env: command
            .iter_full_env_as_str()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        cwd: cwd
            .map(Path::to_path_buf)
            .or_else(|| command.get_cwd().map(PathBuf::from)),
        startup: startup.map(str::to_owned),
        dimensions: Dimensions {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 18,
        },
        colors: runtime_colors(cx),
        control_token,
        command_blocks: SettingsStore::get(cx).command_blocks,
    })
}

fn runtime_colors(cx: &App) -> Colors {
    let theme = cx.theme();
    let colors = &theme.terminal;
    let rgb = |color: libghostty_vt::style::RgbColor| [color.r, color.g, color.b];
    Colors {
        foreground: rgb(colors.foreground),
        background: rgb(colors.background),
        cursor: rgb(colors.cursor),
        palette: colors.ansi.map(rgb),
        dark: theme.appearance == theme::Appearance::Dark,
    }
}

fn to_mouse_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Right => Some(mouse::Button::Right),
        MouseButton::Middle => Some(mouse::Button::Middle),
        _ => None,
    }
}
