//! The editor commands are typed into in block mode: multi-line, laid out
//! on the terminal's cell grid in the terminal font, with selection,
//! clipboard, and IME support.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementInputHandler, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight, Hsla, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, SharedString, TextRun, UTF16Selection,
    UnderlineStyle, Window, actions, canvas, div, fill, point, prelude::*, px, size,
};

use crate::{
    editor_buffer::{self, EditorBuffer, VisualRow},
    grid::CellMetrics,
    settings::SettingsStore,
    theme::{self, ActiveThemeExt},
};

actions!(
    command_editor,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteToLineStart,
        DeleteToLineEnd,
        Left,
        Right,
        Up,
        Down,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        LineStart,
        LineEnd,
        Paste,
        Cut,
        Copy,
        Submit,
        Newline,
        Clear,
        EndOfFile,
        Unselect,
        SearchHistory,
        Complete,
        CompletePrevious,
    ]
);

pub const KEY_CONTEXT: &str = "CommandEditor";

/// Rows shown before the editor scrolls.
const MAX_VISIBLE_ROWS: usize = 12;

pub enum CommandEditorEvent {
    /// Enter was pressed; the text is the command to run.
    Submitted(String),
    /// Ctrl-D in an empty editor.
    EndOfFile,
    /// Up on the first row, or any Up while a menu is open.
    HistoryPrevious,
    /// Down on the last row, or any Down while a menu is open.
    HistoryNext,
    /// Enter while a menu is open: pick its selection; the text stays.
    Confirmed,
    /// Ctrl-R.
    SearchHistory,
    /// Escape with nothing selected.
    Escaped,
    /// Tab: complete the word before the cursor, or move to the next
    /// completion.
    Complete,
    /// Shift-Tab: move to the previous completion.
    CompletePrevious,
    Changed,
}

pub struct CommandEditor {
    focus_handle: FocusHandle,
    buffer: EditorBuffer,
    marked_range: Option<Range<usize>>,
    placeholder: SharedString,
    /// Layout from the last frame, used to map points to text.
    bounds: Option<Bounds<Pixels>>,
    metrics: Option<CellMetrics>,
    rows: Vec<VisualRow>,
    /// First visual row shown when the text has more rows than fit.
    scroll_row: usize,
    selecting: bool,
    /// Colors for ranges of the text; the rest is the terminal foreground.
    highlights: Vec<(Range<usize>, Hsla)>,
    /// Text that would complete the command, shown after the cursor when it
    /// is at the end.
    suggestion: Option<String>,
    /// Whether a menu attached to the editor (completions, history search)
    /// takes Up, Down, and Enter.
    menu_open: bool,
}

impl EventEmitter<CommandEditorEvent> for CommandEditor {}

impl Focusable for CommandEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CommandEditor {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: EditorBuffer::default(),
            marked_range: None,
            placeholder: placeholder.into(),
            bounds: None,
            metrics: None,
            rows: Vec::new(),
            scroll_row: 0,
            selecting: false,
            highlights: Vec::new(),
            suggestion: None,
            menu_open: false,
        }
    }

    pub fn set_menu_open(&mut self, open: bool) {
        self.menu_open = open;
    }

    /// The text before the cursor.
    pub fn text_before_cursor(&self) -> &str {
        &self.buffer.text()[..self.buffer.cursor()]
    }

    /// Replace the `len` bytes before the cursor with `text`.
    pub fn replace_before_cursor(&mut self, len: usize, text: &str, cx: &mut Context<Self>) {
        let cursor = self.buffer.cursor();
        self.buffer
            .replace(cursor.saturating_sub(len)..cursor, text);
        self.marked_range = None;
        self.edited(cx);
    }

    pub fn set_highlights(
        &mut self,
        highlights: Vec<(Range<usize>, Hsla)>,
        cx: &mut Context<Self>,
    ) {
        self.highlights = highlights;
        cx.notify();
    }

    pub fn set_suggestion(&mut self, suggestion: Option<String>, cx: &mut Context<Self>) {
        self.suggestion = suggestion;
        cx.notify();
    }

    /// Take the suggestion if the cursor is at the end, where it shows.
    fn accept_suggestion(&mut self, cx: &mut Context<Self>) -> bool {
        let at_end =
            self.buffer.selection().is_empty() && self.buffer.cursor() == self.buffer.text().len();
        let Some(suggestion) = self.suggestion.take().filter(|_| at_end) else {
            return false;
        };
        self.buffer.insert(&suggestion);
        self.edited(cx);
        true
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    /// Replace the text, with the cursor at its end.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.marked_range = None;
        self.scroll_row = 0;
        self.edited(cx);
    }

    fn edited(&mut self, cx: &mut Context<Self>) {
        cx.emit(CommandEditorEvent::Changed);
        cx.notify();
    }

    fn moved(&mut self, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }

    fn cols(&self) -> usize {
        match (self.bounds, self.metrics) {
            (Some(bounds), Some(metrics)) => (bounds.size.width / metrics.width).floor() as usize,
            _ => 80,
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.buffer.previous_boundary(self.buffer.cursor());
        self.buffer.delete_to(offset);
        self.edited(cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.buffer.next_boundary(self.buffer.cursor());
        self.buffer.delete_to(offset);
        self.edited(cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.buffer.previous_word(self.buffer.cursor());
        self.buffer.delete_to(offset);
        self.edited(cx);
    }

    fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.buffer.line_start(self.buffer.cursor());
        self.buffer.delete_to(offset);
        self.edited(cx);
    }

    fn delete_to_line_end(&mut self, _: &DeleteToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.buffer.line_end(self.buffer.cursor());
        self.buffer.delete_to(offset);
        self.edited(cx);
    }

    /// Move left or right: by one grapheme, or to the selection's edge.
    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let selection = self.buffer.selection();
        let offset = match (selection.is_empty(), forward) {
            (true, true) => self.buffer.next_boundary(self.buffer.cursor()),
            (true, false) => self.buffer.previous_boundary(self.buffer.cursor()),
            (false, true) => selection.end,
            (false, false) => selection.start,
        };
        self.buffer.move_to(offset, false);
        self.moved(cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if !self.accept_suggestion(cx) {
            self.step(true, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open {
            cx.emit(CommandEditorEvent::HistoryPrevious);
            return;
        }
        match self.buffer.vertical(&self.rows, false) {
            Some(offset) => {
                self.buffer.move_to(offset, false);
                self.moved(cx);
            }
            None => cx.emit(CommandEditorEvent::HistoryPrevious),
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open {
            cx.emit(CommandEditorEvent::HistoryNext);
            return;
        }
        match self.buffer.vertical(&self.rows, true) {
            Some(offset) => {
                self.buffer.move_to(offset, false);
                self.moved(cx);
            }
            None => cx.emit(CommandEditorEvent::HistoryNext),
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.buffer.move_to(offset, true);
        self.moved(cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer
            .move_to(self.buffer.previous_word(self.buffer.cursor()), false);
        self.moved(cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer
            .move_to(self.buffer.next_word(self.buffer.cursor()), false);
        self.moved(cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buffer.previous_boundary(self.buffer.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buffer.next_boundary(self.buffer.cursor()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.buffer.vertical(&self.rows, false).unwrap_or(0);
        self.select_to(offset, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self
            .buffer
            .vertical(&self.rows, true)
            .unwrap_or(self.buffer.text().len());
        self.select_to(offset, cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buffer.previous_word(self.buffer.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buffer.next_word(self.buffer.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_all();
        self.moved(cx);
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer
            .move_to(self.buffer.line_start(self.buffer.cursor()), false);
        self.moved(cx);
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        if self.accept_suggestion(cx) {
            return;
        }
        self.buffer
            .move_to(self.buffer.line_end(self.buffer.cursor()), false);
        self.moved(cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.buffer
                .insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
            self.edited(cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.buffer.selected_text();
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected.to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.buffer.selection().is_empty() {
            self.copy(&Copy, window, cx);
            self.buffer.insert("");
            self.edited(cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open {
            cx.emit(CommandEditorEvent::Confirmed);
            return;
        }
        let command = self.buffer.take();
        self.marked_range = None;
        self.scroll_row = 0;
        cx.emit(CommandEditorEvent::Submitted(command));
        self.edited(cx);
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.insert("\n");
        self.edited(cx);
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.take();
        self.marked_range = None;
        self.scroll_row = 0;
        self.edited(cx);
    }

    fn end_of_file(&mut self, _: &EndOfFile, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.is_empty() {
            cx.emit(CommandEditorEvent::EndOfFile);
        } else {
            self.delete(&Delete, window, cx);
        }
    }

    fn unselect(&mut self, _: &Unselect, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.selection().is_empty() {
            cx.emit(CommandEditorEvent::Escaped);
            return;
        }
        self.buffer.move_to(self.buffer.cursor(), false);
        self.moved(cx);
    }

    fn search_history(&mut self, _: &SearchHistory, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CommandEditorEvent::SearchHistory);
    }

    fn complete(&mut self, _: &Complete, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CommandEditorEvent::Complete);
    }

    fn complete_previous(&mut self, _: &CompletePrevious, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CommandEditorEvent::CompletePrevious);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.selecting = true;
        let offset = self.offset_for_point(event.position);
        self.buffer.move_to(offset, event.modifiers.shift);
        self.moved(cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let offset = self.offset_for_point(event.position);
            self.select_to(offset, cx);
        }
    }

    fn offset_for_point(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(metrics)) = (self.bounds, self.metrics) else {
            return self.buffer.text().len();
        };
        let local = position - bounds.origin;
        let row = (local.y / metrics.height).floor().max(0.) as usize + self.scroll_row;
        let col = (local.x / metrics.width).round().max(0.) as usize;
        if row >= self.rows.len() {
            return self.buffer.text().len();
        }
        editor_buffer::offset_at(self.buffer.text(), &self.rows, row, col)
    }

    /// Top-left of the cell at byte `offset`, in window coordinates.
    fn point_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
        let (bounds, metrics) = (self.bounds?, self.metrics?);
        let (row, col) = editor_buffer::position_of(self.buffer.text(), &self.rows, offset);
        Some(
            bounds.origin
                + point(
                    metrics.width * col as f32,
                    metrics.height * row.saturating_sub(self.scroll_row) as f32,
                ),
        )
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.buffer.text().chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.buffer.text()[..offset.min(self.buffer.text().len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Keep the cursor's row inside the visible rows.
    fn scroll_to_cursor(&mut self) {
        let (row, _) =
            editor_buffer::position_of(self.buffer.text(), &self.rows, self.buffer.cursor());
        if row < self.scroll_row {
            self.scroll_row = row;
        } else if row >= self.scroll_row + MAX_VISIBLE_ROWS {
            self.scroll_row = row + 1 - MAX_VISIBLE_ROWS;
        }
        let max_scroll = self.rows.len().saturating_sub(MAX_VISIBLE_ROWS);
        self.scroll_row = self.scroll_row.min(max_scroll);
    }
}

impl EntityInputHandler for CommandEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.buffer.text()[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.buffer.selection()),
            reversed: self.buffer.is_reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.buffer.selection());
        self.buffer.replace(range, text);
        self.marked_range = None;
        self.edited(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.buffer.selection());
        self.buffer.replace(range.clone(), text);
        self.marked_range = (!text.is_empty()).then(|| range.start..range.start + text.len());
        if let Some(selected) = new_selected_range_utf16 {
            let marked_text = &text[..];
            let to_utf8 = |utf16: usize| {
                let mut units = 0;
                marked_text
                    .char_indices()
                    .find_map(|(index, ch)| {
                        let found = (units >= utf16).then_some(index);
                        units += ch.len_utf16();
                        found
                    })
                    .unwrap_or(marked_text.len())
            };
            self.buffer.select(
                range.start + to_utf8(selected.start)..range.start + to_utf8(selected.end),
                false,
            );
        }
        self.edited(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let metrics = self.metrics?;
        let range = self.range_from_utf16(&range_utf16);
        let start = self.point_for_offset(range.start)?;
        let end = self.point_for_offset(range.end)?;
        let end = if end.y > start.y { start } else { end };
        Some(Bounds::from_corners(
            start,
            point(end.x.max(start.x + metrics.width), start.y + metrics.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.offset_for_point(position)))
    }
}

/// One visual row, ready to paint.
struct PaintRow {
    text: SharedString,
    runs: Vec<TextRun>,
    /// Selected columns on this row.
    selection: Option<Range<usize>>,
}

impl Render for CommandEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = CellMetrics::measure(SettingsStore::get(cx), window);
        self.metrics = Some(metrics);
        self.rows = editor_buffer::layout_rows(self.buffer.text(), self.cols());
        self.scroll_to_cursor();

        let theme = cx.theme().clone();
        let focused = self.focus_handle.is_focused(window);
        let text = self.buffer.text();
        let selection = self.buffer.selection();
        let font = theme::terminal_font(FontWeight::NORMAL, FontStyle::Normal);
        let foreground = theme::to_hsla(theme.terminal.foreground);
        let visible = self.scroll_row..(self.scroll_row + MAX_VISIBLE_ROWS).min(self.rows.len());

        let paint_rows: Vec<PaintRow> = if text.is_empty() {
            vec![PaintRow {
                text: self.placeholder.clone(),
                runs: vec![run(
                    &font,
                    self.placeholder.len(),
                    theme.text_placeholder,
                    false,
                )],
                selection: None,
            }]
        } else {
            self.rows[visible.clone()]
                .iter()
                .map(|row| {
                    let row_text = &text[row.start..row.end];
                    let marked = self
                        .marked_range
                        .as_ref()
                        .map(|marked| {
                            marked.start.clamp(row.start, row.end) - row.start
                                ..marked.end.clamp(row.start, row.end) - row.start
                        })
                        .filter(|marked| !marked.is_empty());
                    let runs = row_runs(
                        row.start..row.end,
                        &self.highlights,
                        marked.map(|marked| row.start + marked.start..row.start + marked.end),
                        &font,
                        foreground,
                    );
                    let selected =
                        (selection.start.max(row.start)..selection.end.min(row.end)).clone();
                    let selection = (!selected.is_empty()).then(|| {
                        editor_buffer::columns(&text[row.start..selected.start])
                            ..editor_buffer::columns(&text[row.start..selected.end])
                    });
                    PaintRow {
                        text: row_text.to_string().into(),
                        runs,
                        selection,
                    }
                })
                .collect()
        };
        let cursor = (focused && selection.is_empty()).then(|| {
            let (row, col) = editor_buffer::position_of(text, &self.rows, self.buffer.cursor());
            (row - self.scroll_row, col)
        });
        let ghost = self
            .suggestion
            .as_ref()
            .filter(|_| focused && selection.is_empty() && self.buffer.cursor() == text.len())
            .and_then(|suggestion| {
                let (row, col) = editor_buffer::position_of(text, &self.rows, text.len());
                let room = self.cols().saturating_sub(col);
                let first_line = suggestion.lines().next().unwrap_or_default();
                let fitted = fit_columns(first_line, room);
                (!fitted.is_empty() && row >= self.scroll_row).then(|| {
                    let fitted: SharedString = fitted.to_string().into();
                    let runs = vec![run(&font, fitted.len(), theme.text_placeholder, false)];
                    (row - self.scroll_row, col, fitted, runs)
                })
            });
        let height = metrics.height * visible.len().max(1) as f32;
        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();
        let selection_color = theme.text_accent.opacity(0.3);
        let cursor_color = theme::to_hsla(theme.terminal.cursor);

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::delete_to_line_end))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::end_of_file))
            .on_action(cx.listener(Self::unselect))
            .on_action(cx.listener(Self::search_history))
            .on_action(cx.listener(Self::complete))
            .on_action(cx.listener(Self::complete_previous))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .h(height)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        entity.update(cx, |editor, _| editor.bounds = Some(bounds));
                        entity
                    },
                    move |bounds, entity, window, cx| {
                        let cell = |row: usize, col: usize| {
                            bounds.origin
                                + point(metrics.width * col as f32, metrics.height * row as f32)
                        };
                        for (index, row) in paint_rows.iter().enumerate() {
                            if let Some(selected) = &row.selection {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        cell(index, selected.start),
                                        size(
                                            metrics.width * selected.len().max(1) as f32,
                                            metrics.height,
                                        ),
                                    ),
                                    selection_color,
                                ));
                            }
                            let line = window.text_system().shape_line(
                                row.text.clone(),
                                metrics.font_size,
                                &row.runs,
                                Some(metrics.width),
                            );
                            let _ = line.paint(
                                cell(index, 0),
                                metrics.height,
                                gpui::TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                        if let Some((row, col, text, runs)) = &ghost {
                            let line = window.text_system().shape_line(
                                text.clone(),
                                metrics.font_size,
                                runs,
                                Some(metrics.width),
                            );
                            let _ = line.paint(
                                cell(*row, *col),
                                metrics.height,
                                gpui::TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                        if let Some((row, col)) = cursor {
                            window.paint_quad(fill(
                                Bounds::new(cell(row, col), size(px(2.), metrics.height)),
                                cursor_color,
                            ));
                        }
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity),
                            cx,
                        );
                    },
                )
                .size_full(),
            )
    }
}

/// Text runs for the bytes `row` of the text: highlighted ranges in their
/// colors, the IME's marked range underlined.
fn row_runs(
    row: Range<usize>,
    highlights: &[(Range<usize>, Hsla)],
    marked: Option<Range<usize>>,
    font: &gpui::Font,
    foreground: Hsla,
) -> Vec<TextRun> {
    let mut edges = vec![row.start, row.end];
    for range in highlights
        .iter()
        .map(|(range, _)| range)
        .chain(marked.as_ref())
    {
        edges.extend([range.start, range.end]);
    }
    edges.retain(|edge| (row.start..=row.end).contains(edge));
    edges.sort_unstable();
    edges.dedup();
    edges
        .windows(2)
        .map(|pair| {
            let color = highlights
                .iter()
                .find(|(range, _)| range.start <= pair[0] && pair[0] < range.end)
                .map_or(foreground, |(_, color)| *color);
            let underline = marked
                .as_ref()
                .is_some_and(|marked| marked.start <= pair[0] && pair[0] < marked.end);
            run(font, pair[1] - pair[0], color, underline)
        })
        .collect()
}

/// The longest prefix of `text` that fits in `cols` grid columns.
fn fit_columns(text: &str, cols: usize) -> &str {
    let mut width = 0;
    for (index, grapheme) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(text, true)
    {
        width += editor_buffer::columns(grapheme);
        if width > cols {
            return &text[..index];
        }
    }
    text
}

fn run(font: &gpui::Font, len: usize, color: Hsla, underline: bool) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: None,
    }
}

/// Key bindings for the command editor.
pub fn key_bindings() -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    vec![
        KeyBinding::new("backspace", Backspace, context),
        KeyBinding::new("shift-backspace", Backspace, context),
        KeyBinding::new("delete", Delete, context),
        KeyBinding::new("alt-backspace", DeleteWordLeft, context),
        KeyBinding::new("ctrl-w", DeleteWordLeft, context),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, context),
        KeyBinding::new("ctrl-u", DeleteToLineStart, context),
        KeyBinding::new("ctrl-k", DeleteToLineEnd, context),
        KeyBinding::new("left", Left, context),
        KeyBinding::new("ctrl-b", Left, context),
        KeyBinding::new("right", Right, context),
        KeyBinding::new("ctrl-f", Right, context),
        KeyBinding::new("up", Up, context),
        KeyBinding::new("ctrl-p", Up, context),
        KeyBinding::new("down", Down, context),
        KeyBinding::new("ctrl-n", Down, context),
        KeyBinding::new("alt-left", WordLeft, context),
        KeyBinding::new("alt-right", WordRight, context),
        KeyBinding::new("shift-left", SelectLeft, context),
        KeyBinding::new("shift-right", SelectRight, context),
        KeyBinding::new("shift-up", SelectUp, context),
        KeyBinding::new("shift-down", SelectDown, context),
        KeyBinding::new("alt-shift-left", SelectWordLeft, context),
        KeyBinding::new("alt-shift-right", SelectWordRight, context),
        KeyBinding::new("cmd-a", SelectAll, context),
        KeyBinding::new("home", LineStart, context),
        KeyBinding::new("cmd-left", LineStart, context),
        KeyBinding::new("ctrl-a", LineStart, context),
        KeyBinding::new("end", LineEnd, context),
        KeyBinding::new("cmd-right", LineEnd, context),
        KeyBinding::new("ctrl-e", LineEnd, context),
        KeyBinding::new("cmd-v", Paste, context),
        KeyBinding::new("cmd-x", Cut, context),
        KeyBinding::new("cmd-c", Copy, context),
        KeyBinding::new("enter", Submit, context),
        KeyBinding::new("shift-enter", Newline, context),
        KeyBinding::new("alt-enter", Newline, context),
        KeyBinding::new("ctrl-c", Clear, context),
        KeyBinding::new("ctrl-d", EndOfFile, context),
        KeyBinding::new("escape", Unselect, context),
        KeyBinding::new("ctrl-r", SearchHistory, context),
        KeyBinding::new("tab", Complete, context),
        KeyBinding::new("shift-tab", CompletePrevious, context),
    ]
}
