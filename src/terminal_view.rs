use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
use gpui::{
    AnyElement, App, AsyncApp, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity,
    EventEmitter, FocusHandle, Focusable, KeyBinding, KeyDownEvent, KeyUpEvent, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task, WeakEntity, Window, actions,
    canvas, div, prelude::*, px,
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
    components::{elevated_shadow, icon, icon_button},
    grid::{CellMetrics, GridRenderer},
    input::{to_mods, translate_keystroke},
    links::{self, LinkTarget},
    osc::{OscEvent, OscScanner},
    process_info,
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
        let command = shell_integration::shell_command(ShellIntegration::active_dir(cx).as_deref());
        let (pty, output) = Pty::spawn(command, initial, cwd)?;
        let dimensions = Rc::new(Cell::new(initial));

        let mut terminal = Terminal::new(Options {
            cols: initial.cols,
            rows: initial.rows,
            max_scrollback: SCROLLBACK_LINES,
        })?;
        configure_colors(&mut terminal, &cx.theme().terminal)?;
        let bell = Rc::new(Cell::new(false));
        register_effects(&mut terminal, &pty, dimensions.clone(), bell.clone())?;

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
                while let Ok(first) = output.recv().await {
                    let mut chunks = vec![first];
                    while let Ok(next) = output.try_recv() {
                        chunks.push(next);
                    }
                    let updated = this.update(cx, |view, cx| view.process_output(&chunks, cx));
                    if updated.is_err() {
                        return;
                    }
                }
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
            let metadata_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let refreshed = this.update(cx, |view, cx| view.refresh_metadata(cx));
                    if refreshed.is_err() {
                        return;
                    }
                    cx.background_executor()
                        .timer(METADATA_REFRESH_INTERVAL)
                        .await;
                }
            });

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
                osc: OscScanner::default(),
                bell,
                activity: Activity::default(),
                pending_startup: startup.map(str::to_string),
                working_since: None,
                command_started: None,
                last_command: None,
                _theme_subscription: cx.observe_global::<ActiveTheme>(|view, cx| {
                    let colors = cx.theme().terminal.clone();
                    if let Err(error) = configure_colors(&mut view.terminal, &colors) {
                        log::warn!("failed to apply theme: {error}");
                    }
                    cx.notify();
                }),
            }
        }))
    }

    /// Where the terminal was last laid out, in window coordinates.
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

    fn process_output(&mut self, chunks: &[Vec<u8>], cx: &mut Context<Self>) {
        let mut command_changed = false;
        for chunk in chunks {
            for event in self.osc.scan(chunk) {
                command_changed |= self.apply_osc_event(event, cx);
            }
            self.terminal.vt_write(chunk);
        }
        self.activity.output(Instant::now());
        if self.bell.replace(false) {
            self.activity.attention();
            cx.emit(TerminalEvent::Attention(Attention::Bell));
        }
        if command_changed {
            self.refresh_metadata(cx);
        }
        cx.notify();
    }

    /// Returns whether the command state changed.
    fn apply_osc_event(&mut self, event: OscEvent, cx: &mut Context<Self>) -> bool {
        match event {
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
        let foreground = self.pty.foreground_pid();
        let process = foreground.and_then(process_info::process_name);
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
        let directory = self
            .pty
            .shell_pid()
            .and_then(process_info::working_directory);
        let branch = directory.as_deref().and_then(process_info::git_branch);

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
            agent: running
                .then(|| foreground.and_then(process_info::process_args))
                .flatten()
                .and_then(|args| agents::detect(&args))
                .map(|agent| AgentState {
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
        let (cols, rows) = metrics.grid_size(bounds);
        let next = PtyDimensions {
            cols,
            rows,
            cell_width: f32::from(metrics.width).round() as u16,
            cell_height: f32::from(metrics.height).round() as u16,
        };
        if next == self.dimensions.get() {
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
        window.focus(&self.focus_handle, cx);
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
        let focused = self.focus_handle.is_focused(window);
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
                    move |bounds, (), window, cx| {
                        if let Some(frame) = frame {
                            frame.paint(bounds, metrics, window, cx);
                        }
                    },
                )
                .size_full(),
            )
            .children(search_bar)
            .children(link_tooltip)
    }
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
