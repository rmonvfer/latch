use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
use gpui::{
    App, AsyncApp, Bounds, ClipboardItem, Context, CursorStyle, Entity, EventEmitter, FocusHandle,
    Focusable, KeyDownEvent, KeyUpEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    WeakEntity, Window, actions, canvas, div, prelude::*,
};
use libghostty_vt::{
    Terminal,
    fmt::Format,
    key, mouse, paste,
    selection::{
        FormatOptions,
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
    grid::{CellMetrics, GridRenderer},
    input::{to_mods, translate_keystroke},
    process_info,
    pty::{Pty, PtyDimensions},
    settings::SettingsStore,
    theme::{ActiveTheme, ActiveThemeExt, TerminalColors},
};

actions!(terminal, [Copy, Paste, SelectAll, ClearScrollback]);

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
}

pub enum TerminalEvent {
    MetadataChanged,
    Exited,
}

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
    pub fn build(cwd: Option<&Path>, cx: &mut App) -> Result<Entity<Self>> {
        let initial = PtyDimensions {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 18,
        };
        let (pty, output) = Pty::spawn(initial, cwd)?;
        let dimensions = Rc::new(Cell::new(initial));

        let mut terminal = Terminal::new(Options {
            cols: initial.cols,
            rows: initial.rows,
            max_scrollback: SCROLLBACK_LINES,
        })?;
        configure_colors(&mut terminal, &cx.theme().terminal)?;
        register_effects(&mut terminal, &pty, dimensions.clone())?;

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

    /// Current grid size as (columns, rows).
    pub fn grid_size(&self) -> (u16, u16) {
        let dimensions = self.dimensions.get();
        (dimensions.cols, dimensions.rows)
    }

    pub fn metadata(&self) -> &TabMetadata {
        &self.metadata
    }

    fn process_output(&mut self, chunks: &[Vec<u8>], cx: &mut Context<Self>) {
        for chunk in chunks {
            self.terminal.vt_write(chunk);
        }
        cx.notify();
    }

    fn refresh_metadata(&mut self, cx: &mut Context<Self>) {
        let metadata = self.read_metadata();
        if metadata != self.metadata {
            self.metadata = metadata;
            cx.emit(TerminalEvent::MetadataChanged);
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
        self.pty.write(bytes);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Command shortcuts belong to the app, never to the shell.
        if event.keystroke.modifiers.platform {
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

    fn on_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform {
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
            Ok(frame) => Some(frame),
            Err(error) => {
                log::error!("failed to build terminal frame: {error}");
                None
            }
        };
        let view = cx.entity();

        div()
            .id("terminal")
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_scrollback))
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
    }
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
) -> Result<()> {
    let replies = pty.input_sender();
    terminal
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
