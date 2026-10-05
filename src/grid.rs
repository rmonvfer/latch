use anyhow::Result;
use gpui::{
    App, Bounds, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString, StrikethroughStyle,
    TextAlign, TextRun, UnderlineStyle, Window, fill, outline, point, px, size,
};
use libghostty_vt::{
    Terminal,
    render::{CellIterator, CursorVisualStyle, Dirty, RenderState, RowIterator},
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

struct BackgroundSpan {
    row: u16,
    col: u16,
    cols: u16,
    color: Hsla,
}

enum CursorShape {
    Block,
    Hollow,
    Bar,
    Underline,
}

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
    backgrounds: Vec<BackgroundSpan>,
    texts: Vec<TextBatch>,
    cursor: Option<Cursor>,
    link: Option<LinkUnderline>,
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
}

impl GridRenderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            text: String::with_capacity(16),
        })
    }

    pub fn build_frame(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        focused: bool,
    ) -> Result<Frame> {
        let snapshot = self.render_state.update(terminal)?;
        let colors = snapshot.colors()?;
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
                    col: viewport.x,
                    wide: false,
                    shape,
                    color: theme::to_hsla(colors.cursor.unwrap_or(colors.foreground)),
                }
            })
        } else {
            None
        };
        let block_cursor_at = cursor
            .as_ref()
            .filter(|cursor| matches!(cursor.shape, CursorShape::Block))
            .map(|cursor| (cursor.row, cursor.col));

        let mut frame = Frame {
            background: theme::to_hsla(default_background),
            backgrounds: Vec::new(),
            texts: Vec::new(),
            cursor,
            link: None,
        };

        let mut rows = self.rows.update(&snapshot)?;
        let mut row_index: u16 = 0;
        while let Some(row) = rows.next() {
            let mut cells = self.cells.update(row)?;
            let mut col: u16 = 0;
            let mut batch: Option<TextBatch> = None;

            while let Some(cell) = cells.next() {
                let wide = cell.raw_cell()?.wide()?;
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    flush(&mut batch, &mut frame.texts);
                    col += 1;
                    continue;
                }
                let span = if matches!(wide, CellWide::Wide) { 2 } else { 1 };

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
                if cell.is_selected()? {
                    std::mem::swap(&mut fg, &mut bg);
                    paint_bg = true;
                }

                let mut fg_hsla = theme::to_hsla(fg);
                let mut bg_hsla = theme::to_hsla(bg);
                if block_cursor_at == Some((row_index, col)) {
                    if let Some(cursor) = frame.cursor.as_mut() {
                        cursor.wide = span == 2;
                        bg_hsla = cursor.color;
                    }
                    fg_hsla = theme::to_hsla(default_background);
                    paint_bg = true;
                }
                if style.is_some_and(|style| style.faint) {
                    fg_hsla = fg_hsla.opacity(0.6);
                }

                if paint_bg {
                    push_background(&mut frame.backgrounds, row_index, col, span, bg_hsla);
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
                    flush(&mut batch, &mut frame.texts);
                } else if span == 2 {
                    flush(&mut batch, &mut frame.texts);
                    frame.texts.push(TextBatch {
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
                            flush(&mut batch, &mut frame.texts);
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
            flush(&mut batch, &mut frame.texts);
            row.set_dirty(false)?;
            row_index += 1;
        }

        snapshot.set_dirty(Dirty::Clean)?;
        Ok(frame)
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

        for span in &self.backgrounds {
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
        for batch in &self.texts {
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
