use std::rc::Rc;

use anyhow::Result;
use gpui::{
    App, Bounds, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString, StrikethroughStyle,
    TextAlign, TextRun, UnderlineStyle, Window, fill, outline, point, px, size,
};
use libghostty_vt::{
    Terminal,
    render::{CellIterator, Colors, CursorVisualStyle, Dirty, RenderState, RowIterator},
    screen::CellWide,
    style::{RgbColor, Underline},
};

use crate::{settings::Settings, theme};

/// Text size, cell size, and padding of a terminal grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub font_size: Pixels,
    pub width: Pixels,
    pub height: Pixels,
    pub padding: Pixels,
}

impl CellMetrics {
    pub fn measure(settings: &Settings, window: &Window) -> Self {
        let font_size = px(settings.font_size);
        let text_system = window.text_system();
        let font_id =
            text_system.resolve_font(&theme::terminal_font(FontWeight::NORMAL, FontStyle::Normal));
        let width = text_system
            .advance(font_id, font_size, 'M')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);
        Self {
            font_size,
            width,
            height: font_size * settings.line_height,
            padding: px(settings.terminal_padding),
        }
    }

    /// The number of whole cells that fit inside `bounds` after padding.
    pub fn grid_size(&self, bounds: Bounds<Pixels>) -> (u16, u16) {
        let usable_width = bounds.size.width - self.padding * 2.;
        let usable_height = bounds.size.height - self.padding * 2.;
        let cols = (usable_width / self.width).floor().max(2.) as u16;
        let rows = (usable_height / self.height).floor().max(1.) as u16;
        (cols, rows)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TextStyle {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: Underline,
    strikethrough: bool,
}

#[derive(Debug, PartialEq)]
struct TextBatch {
    row: u16,
    col: u16,
    text: String,
    /// Number of grid cells covered; batches of narrow cells hold one
    /// grapheme cluster per cell.
    cells: u16,
    style: TextStyle,
    /// Wide glyphs are shaped on their own and not snapped to the cell grid.
    wide: bool,
}

#[derive(Debug, PartialEq)]
struct BackgroundSpan {
    row: u16,
    col: u16,
    cols: u16,
    color: Hsla,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorShape {
    Block,
    Hollow,
    Bar,
    Underline,
}

#[derive(Debug, PartialEq)]
struct Cursor {
    row: u16,
    col: u16,
    wide: bool,
    shape: CursorShape,
    color: Hsla,
}

/// Everything needed to paint one frame of a terminal, detached from
/// libghostty's borrowed render state so it can move into a paint closure.
pub struct Frame {
    pub background: Hsla,
    rows: Rc<Vec<Rc<FrameRow>>>,
    cursor: Option<Cursor>,
    link: Option<LinkUnderline>,
}

#[derive(Default, Debug, PartialEq)]
struct FrameRow {
    backgrounds: Vec<BackgroundSpan>,
    texts: Vec<TextBatch>,
    wide_cells: Vec<u16>,
}

/// An underline marking the link under the pointer.
struct LinkUnderline {
    row: u16,
    start_col: u16,
    end_col: u16,
    color: Hsla,
}

/// Reusable libghostty render objects for one terminal.
pub struct GridRenderer {
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    text: String,
    frame_rows: Rc<Vec<Rc<FrameRow>>>,
    dimensions: (u16, u16),
    colors: Option<Colors>,
    block_cursor: Option<(u16, u16, Hsla)>,
}

impl GridRenderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            text: String::with_capacity(16),
            frame_rows: Rc::new(Vec::new()),
            dimensions: (0, 0),
            colors: None,
            block_cursor: None,
        })
    }

    pub fn build_frame(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        focused: bool,
    ) -> Result<Frame> {
        let snapshot = self.render_state.update(terminal)?;
        let colors = snapshot.colors()?;
        let dimensions = (snapshot.cols()?, snapshot.rows()?);
        let dirty = snapshot.dirty()?;
        let rebuild_all = dirty == Dirty::Full
            || self.dimensions != dimensions
            || self.colors.as_ref() != Some(&colors);
        let default_background = colors.background;

        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|viewport| {
                let shape = if !focused {
                    CursorShape::Hollow
                } else {
                    match snapshot.cursor_visual_style() {
                        Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
                        Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
                        Ok(CursorVisualStyle::BlockHollow) => CursorShape::Hollow,
                        _ => CursorShape::Block,
                    }
                };
                Cursor {
                    row: viewport.y,
                    col: viewport.x.saturating_sub(u16::from(viewport.at_wide_tail)),
                    wide: false,
                    shape,
                    color: theme::to_hsla(colors.cursor.unwrap_or(colors.foreground)),
                }
            })
        } else {
            None
        };
        let block_cursor = cursor
            .as_ref()
            .filter(|cursor| matches!(cursor.shape, CursorShape::Block))
            .map(|cursor| (cursor.row, cursor.col, cursor.color));
        let cursor_changed = block_cursor != self.block_cursor;

        if !rebuild_all && dirty == Dirty::Clean && !cursor_changed {
            return Ok(Frame::new(
                theme::to_hsla(default_background),
                Rc::clone(&self.frame_rows),
                cursor,
            ));
        }

        // A failed extraction must leave the snapshot fully dirty so a retry
        // cannot reuse cached rows whose dirty flags were already consumed.
        snapshot.set_dirty(Dirty::Full)?;
        let mut frame_rows = Vec::with_capacity(dimensions.1 as usize);
        let mut rows = self.rows.update(&snapshot)?;
        let mut row_index: u16 = 0;
        while let Some(row) = rows.next() {
            // Cursor position and focus can change while cell data stays clean.
            let cursor_row_changed = cursor_changed
                && [self.block_cursor, block_cursor]
                    .iter()
                    .flatten()
                    .any(|(y, _, _)| *y == row_index);
            if !rebuild_all
                && !row.dirty()?
                && !cursor_row_changed
                && let Some(cached) = self.frame_rows.get(row_index as usize)
            {
                frame_rows.push(Rc::clone(cached));
                row_index += 1;
                continue;
            }

            let mut frame_row = FrameRow::default();
            let selected = row.selection()?;
            let mut cells = self.cells.update(row)?;
            let mut col: u16 = 0;
            let mut batch: Option<TextBatch> = None;

            while let Some(cell) = cells.next() {
                let wide = cell.raw_cell()?.wide()?;
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    flush(&mut batch, &mut frame_row.texts);
                    col += 1;
                    continue;
                }
                let span = if matches!(wide, CellWide::Wide) {
                    frame_row.wide_cells.push(col);
                    2
                } else {
                    1
                };

                let style = if cell.has_styling()? {
                    Some(cell.style()?)
                } else {
                    None
                };
                let mut fg: RgbColor = cell.fg_color()?.unwrap_or(colors.foreground);
                let explicit_bg = cell.bg_color()?;
                let mut bg = explicit_bg.unwrap_or(default_background);
                let mut paint_bg = explicit_bg.is_some();

                if style.is_some_and(|style| style.inverse) {
                    std::mem::swap(&mut fg, &mut bg);
                    paint_bg = true;
                }
                if selected.is_some_and(|range| col >= range.start_x && col <= range.end_x) {
                    std::mem::swap(&mut fg, &mut bg);
                    paint_bg = true;
                }

                let mut fg_hsla = theme::to_hsla(fg);
                let mut bg_hsla = theme::to_hsla(bg);
                if let Some((cursor_row, cursor_col, color)) = block_cursor
                    && (cursor_row, cursor_col) == (row_index, col)
                {
                    bg_hsla = color;
                    fg_hsla = theme::to_hsla(default_background);
                    paint_bg = true;
                }
                if style.is_some_and(|style| style.faint) {
                    fg_hsla = fg_hsla.opacity(0.6);
                }

                if paint_bg {
                    push_background(&mut frame_row.backgrounds, row_index, col, span, bg_hsla);
                }

                let invisible = style.is_some_and(|style| style.invisible);
                let has_text = cell.graphemes_len()? > 0;
                if has_text && !invisible {
                    cell.graphemes_utf8(&mut self.text)?;
                } else {
                    self.text.clear();
                    self.text.push(' ');
                }

                let text_style = TextStyle {
                    color: fg_hsla,
                    bold: style.is_some_and(|style| style.bold),
                    italic: style.is_some_and(|style| style.italic),
                    underline: style.map_or(Underline::None, |style| style.underline),
                    strikethrough: style.is_some_and(|style| style.strikethrough),
                };
                let decorated = text_style.underline != Underline::None || text_style.strikethrough;

                if (!has_text || invisible) && !decorated {
                    flush(&mut batch, &mut frame_row.texts);
                } else if span == 2 {
                    flush(&mut batch, &mut frame_row.texts);
                    frame_row.texts.push(TextBatch {
                        row: row_index,
                        col,
                        text: self.text.clone(),
                        cells: 2,
                        style: text_style,
                        wide: true,
                    });
                } else {
                    match batch.as_mut() {
                        Some(current)
                            if current.style == text_style
                                && current.col + current.cells == col =>
                        {
                            current.text.push_str(&self.text);
                            current.cells += 1;
                        }
                        _ => {
                            flush(&mut batch, &mut frame_row.texts);
                            batch = Some(TextBatch {
                                row: row_index,
                                col,
                                text: self.text.clone(),
                                cells: 1,
                                style: text_style,
                                wide: false,
                            });
                        }
                    }
                }

                // A wide cell is followed by a spacer tail cell, which
                // advances the column on its own iteration.
                col += 1;
            }
            flush(&mut batch, &mut frame_row.texts);
            row.set_dirty(false)?;
            frame_rows.push(Rc::new(frame_row));
            row_index += 1;
        }

        snapshot.set_dirty(Dirty::Clean)?;
        self.frame_rows = Rc::new(frame_rows);
        self.dimensions = dimensions;
        self.colors = Some(colors);
        self.block_cursor = block_cursor;
        Ok(Frame::new(
            theme::to_hsla(default_background),
            Rc::clone(&self.frame_rows),
            cursor,
        ))
    }
}

fn flush(batch: &mut Option<TextBatch>, texts: &mut Vec<TextBatch>) {
    if let Some(batch) = batch.take() {
        texts.push(batch);
    }
}

fn push_background(spans: &mut Vec<BackgroundSpan>, row: u16, col: u16, cols: u16, color: Hsla) {
    if let Some(last) = spans.last_mut()
        && last.row == row
        && last.col + last.cols == col
        && last.color == color
    {
        last.cols += cols;
        return;
    }
    spans.push(BackgroundSpan {
        row,
        col,
        cols,
        color,
    });
}

impl Frame {
    fn new(background: Hsla, rows: Rc<Vec<Rc<FrameRow>>>, mut cursor: Option<Cursor>) -> Self {
        if let Some(cursor) = &mut cursor {
            cursor.wide = rows
                .get(cursor.row as usize)
                .is_some_and(|row| row.wide_cells.binary_search(&cursor.col).is_ok());
        }
        Self {
            background,
            rows,
            cursor,
            link: None,
        }
    }

    /// Underline columns `start_col..end_col` of viewport `row`.
    pub fn set_link(&mut self, row: u16, start_col: u16, end_col: u16, color: Hsla) {
        self.link = Some(LinkUnderline {
            row,
            start_col,
            end_col,
            color,
        });
    }

    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        metrics: CellMetrics,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(bounds, self.background));

        let origin = bounds.origin + point(metrics.padding, metrics.padding);
        let cell_origin = |row: u16, col: u16| -> Point<Pixels> {
            origin + point(metrics.width * col as f32, metrics.height * row as f32)
        };

        for span in self.rows.iter().flat_map(|row| &row.backgrounds) {
            let start = cell_origin(span.row, span.col);
            window.paint_quad(fill(
                Bounds::new(
                    start,
                    size(metrics.width * span.cols as f32, metrics.height),
                ),
                span.color,
            ));
        }

        if let Some(link) = &self.link {
            let start = cell_origin(link.row, link.start_col);
            window.paint_quad(fill(
                Bounds::new(
                    start + point(px(0.), metrics.height - px(2.)),
                    size(
                        metrics.width * (link.end_col - link.start_col) as f32,
                        px(1.),
                    ),
                ),
                link.color,
            ));
        }

        if let Some(cursor) = &self.cursor {
            let start = cell_origin(cursor.row, cursor.col);
            let width = if cursor.wide {
                metrics.width * 2.
            } else {
                metrics.width
            };
            match cursor.shape {
                // Block cursors are painted as a cell background during frame building.
                CursorShape::Block => {}
                CursorShape::Hollow => window.paint_quad(outline(
                    Bounds::new(start, size(width, metrics.height)),
                    cursor.color,
                    gpui::BorderStyle::Solid,
                )),
                CursorShape::Bar => window.paint_quad(fill(
                    Bounds::new(start, size(px(2.), metrics.height)),
                    cursor.color,
                )),
                CursorShape::Underline => window.paint_quad(fill(
                    Bounds::new(
                        start + point(px(0.), metrics.height - px(2.)),
                        size(width, px(2.)),
                    ),
                    cursor.color,
                )),
            }
        }

        let text_system = window.text_system().clone();
        for batch in self.rows.iter().flat_map(|row| &row.texts) {
            let style = batch.style;
            let font = theme::terminal_font(
                if style.bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                },
                if style.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
            );
            let run = TextRun {
                len: batch.text.len(),
                font,
                color: style.color,
                background_color: None,
                underline: match style.underline {
                    Underline::None => None,
                    underline => Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(style.color),
                        wavy: underline == Underline::Curly,
                    }),
                },
                strikethrough: style.strikethrough.then_some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(style.color),
                }),
            };
            let force_width = if batch.wide {
                None
            } else {
                Some(metrics.width)
            };
            let line = text_system.shape_line(
                SharedString::from(batch.text.clone()),
                metrics.font_size,
                &[run],
                force_width,
            );
            let _ = line.paint(
                cell_origin(batch.row, batch.col),
                metrics.height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{hint::black_box, time::Instant};

    use libghostty_vt::{
        selection::Selection,
        terminal::{Options, Point as TerminalPoint, PointCoordinate, ScrollViewport},
    };

    use super::*;

    fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
        let mut terminal = Terminal::new(Options {
            cols,
            rows,
            max_scrollback: 100,
        })
        .unwrap();
        terminal
            .set_default_fg_color(Some(RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }))
            .unwrap();
        terminal
            .set_default_bg_color(Some(RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }))
            .unwrap();
        terminal
    }

    fn assert_matches_full_frame(
        frame: &Frame,
        terminal: &Terminal<'static, 'static>,
        focused: bool,
    ) {
        let expected = GridRenderer::new()
            .unwrap()
            .build_frame(terminal, focused)
            .unwrap();
        assert_eq!(frame.background, expected.background);
        assert_eq!(frame.rows, expected.rows);
        assert_eq!(frame.cursor, expected.cursor);
    }

    #[test]
    fn clean_frames_share_rows_and_do_not_retain_hovered_links() {
        let mut terminal = terminal(20, 3);
        terminal.vt_write(b"hello\r\nworld");
        let mut renderer = GridRenderer::new().unwrap();
        let mut first = renderer.build_frame(&terminal, true).unwrap();
        first.set_link(0, 0, 5, first.background);
        let second = renderer.build_frame(&terminal, true).unwrap();
        assert!(Rc::ptr_eq(&first.rows, &second.rows));
        assert!(second.link.is_none());
        assert_matches_full_frame(&second, &terminal, true);
    }

    #[test]
    fn a_cell_change_rebuilds_only_its_row() {
        let mut terminal = terminal(20, 3);
        terminal.vt_write(b"\x1b[?25lfirst\r\nsecond\r\nthird\x1b[2;1H");
        let mut renderer = GridRenderer::new().unwrap();
        let first = renderer.build_frame(&terminal, true).unwrap();
        terminal.vt_write(b"\x1b[2;1HX");
        let second = renderer.build_frame(&terminal, true).unwrap();
        assert!(Rc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(!Rc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert!(Rc::ptr_eq(&first.rows[2], &second.rows[2]));
        assert_matches_full_frame(&second, &terminal, true);
    }

    #[test]
    fn cursor_motion_focus_and_visibility_invalidate_their_cells() {
        let mut terminal = terminal(20, 3);
        terminal.vt_write(b"first\r\nsecond\r\nthird\x1b[H");
        let mut renderer = GridRenderer::new().unwrap();
        let first = renderer.build_frame(&terminal, true).unwrap();
        terminal.vt_write(b"\x1b[2;2H");
        let moved = renderer.build_frame(&terminal, true).unwrap();
        assert!(!Rc::ptr_eq(&first.rows[0], &moved.rows[0]));
        assert!(!Rc::ptr_eq(&first.rows[1], &moved.rows[1]));
        assert!(Rc::ptr_eq(&first.rows[2], &moved.rows[2]));
        assert_matches_full_frame(&moved, &terminal, true);

        let blurred = renderer.build_frame(&terminal, false).unwrap();
        assert!(Rc::ptr_eq(&moved.rows[0], &blurred.rows[0]));
        assert!(!Rc::ptr_eq(&moved.rows[1], &blurred.rows[1]));
        assert_eq!(blurred.cursor.as_ref().unwrap().shape, CursorShape::Hollow);
        assert_matches_full_frame(&blurred, &terminal, false);

        terminal.vt_write(b"\x1b[3;2H");
        let hollow_moved = renderer.build_frame(&terminal, false).unwrap();
        assert!(Rc::ptr_eq(&blurred.rows[0], &hollow_moved.rows[0]));
        assert_matches_full_frame(&hollow_moved, &terminal, false);

        let focused = renderer.build_frame(&terminal, true).unwrap();
        assert_eq!(focused.cursor.as_ref().unwrap().shape, CursorShape::Block);
        terminal.vt_write(b"\x1b[?25l");
        let hidden = renderer.build_frame(&terminal, true).unwrap();
        assert!(hidden.cursor.is_none());
        assert_matches_full_frame(&hidden, &terminal, true);

        terminal.vt_write(b"\x1b[?25h\x1b[6 q");
        let bar = renderer.build_frame(&terminal, true).unwrap();
        assert_eq!(bar.cursor.as_ref().unwrap().shape, CursorShape::Bar);
        assert_matches_full_frame(&bar, &terminal, true);
    }

    #[test]
    fn wide_cursor_covers_the_glyph_in_focused_and_unfocused_frames() {
        let mut terminal = terminal(20, 3);
        terminal.vt_write("界abc\x1b[1;2H".as_bytes());
        let mut renderer = GridRenderer::new().unwrap();
        for focused in [true, false, true] {
            let frame = renderer.build_frame(&terminal, focused).unwrap();
            let cursor = frame.cursor.as_ref().unwrap();
            assert_eq!(cursor.col, 0);
            assert!(cursor.wide);
            assert_matches_full_frame(&frame, &terminal, focused);
        }
    }

    #[test]
    fn selection_and_color_changes_refresh_cached_styles() {
        let mut terminal = terminal(20, 3);
        terminal.vt_write(b"\x1b[?25lfirst\r\n\x1b[31msecond\x1b[0m\r\nthird");
        let mut renderer = GridRenderer::new().unwrap();
        let first = renderer.build_frame(&terminal, true).unwrap();
        let start = terminal
            .grid_ref(TerminalPoint::Viewport(PointCoordinate { x: 1, y: 0 }))
            .unwrap();
        let end = terminal
            .grid_ref(TerminalPoint::Viewport(PointCoordinate { x: 3, y: 0 }))
            .unwrap();
        terminal
            .set_selection(Some(&Selection::new(start, end, false)))
            .unwrap();
        let selected = renderer.build_frame(&terminal, true).unwrap();
        assert_ne!(first.rows[0], selected.rows[0]);
        assert_eq!(selected.rows[0].backgrounds[0].col, 1);
        assert_eq!(selected.rows[0].backgrounds[0].cols, 3);
        assert_matches_full_frame(&selected, &terminal, true);

        terminal.set_selection(None).unwrap();
        let deselected = renderer.build_frame(&terminal, true).unwrap();
        assert_eq!(first.rows, deselected.rows);

        for command in [
            b"\x1b]10;#12ab34\x07".as_slice(),
            b"\x1b]11;#123456\x07".as_slice(),
            b"\x1b]4;1;#aabbcc\x07".as_slice(),
            b"\x1b[?5h".as_slice(),
        ] {
            terminal.vt_write(command);
            let frame = renderer.build_frame(&terminal, true).unwrap();
            assert_matches_full_frame(&frame, &terminal, true);
        }
    }

    #[test]
    fn resize_scrolling_and_screen_switches_replace_viewport_rows() {
        let mut terminal = terminal(20, 3);
        terminal.vt_write(b"\x1b[?25lzero\r\none\r\ntwo\r\nthree\r\nfour");
        let mut renderer = GridRenderer::new().unwrap();
        renderer.build_frame(&terminal, true).unwrap();
        terminal.scroll_viewport(ScrollViewport::Top);
        let scrolled = renderer.build_frame(&terminal, true).unwrap();
        assert_matches_full_frame(&scrolled, &terminal, true);
        terminal.scroll_viewport(ScrollViewport::Bottom);
        let bottom = renderer.build_frame(&terminal, true).unwrap();
        assert_ne!(scrolled.rows, bottom.rows);
        assert_matches_full_frame(&bottom, &terminal, true);

        for (cols, rows) in [(10, 2), (30, 6), (20, 3)] {
            terminal.resize(cols, rows, 10, 20).unwrap();
            let frame = renderer.build_frame(&terminal, true).unwrap();
            assert_eq!(frame.rows.len(), rows as usize);
            assert_matches_full_frame(&frame, &terminal, true);
        }

        terminal.vt_write(b"\x1b[?1049h\x1b[Halternate");
        let alternate = renderer.build_frame(&terminal, true).unwrap();
        assert_matches_full_frame(&alternate, &terminal, true);
        terminal.vt_write(b"\x1b[?1049l");
        let primary = renderer.build_frame(&terminal, true).unwrap();
        assert_ne!(alternate.rows, primary.rows);
        assert_matches_full_frame(&primary, &terminal, true);
    }

    #[test]
    #[ignore = "prints viewport extraction timings"]
    fn frame_build_benchmark() {
        let mut terminal = terminal(160, 48);
        terminal.vt_write(b"\x1b[?25l");
        for row in 1..=48 {
            terminal
                .vt_write(format!("\x1b[{row};1H\x1b[32m{}", "abcdefghij".repeat(14)).as_bytes());
        }
        let mut renderer = GridRenderer::new().unwrap();
        renderer.build_frame(&terminal, true).unwrap();
        let iterations = 2_000;
        let started = Instant::now();
        for _ in 0..iterations {
            renderer.colors = None;
            black_box(renderer.build_frame(&terminal, true).unwrap());
        }
        let full = started.elapsed() / iterations;
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(renderer.build_frame(&terminal, true).unwrap());
        }
        let clean = started.elapsed() / iterations;
        let started = Instant::now();
        for _ in 0..iterations {
            terminal.vt_write(b"\x1b[24;1Hx");
            black_box(renderer.build_frame(&terminal, true).unwrap());
        }
        let partial = started.elapsed() / iterations;
        eprintln!("160x48 frame extraction: full={full:?}, clean={clean:?}, one_row={partial:?}");
    }
}
