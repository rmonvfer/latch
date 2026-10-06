//! The text and selection of the command editor, with cursor movement laid
//! out on a monospaced grid of `cols` columns.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One row of the editor as displayed: a byte range of the text, either a
/// whole line or the part of a long line that fits the width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisualRow {
    pub start: usize,
    pub end: usize,
}

/// Columns a grapheme occupies on the grid.
fn grapheme_width(grapheme: &str) -> usize {
    grapheme.width().max(1)
}

/// Rows of `text` wrapped at `cols` columns. Every line, even an empty one,
/// has at least one row.
pub fn layout_rows(text: &str, cols: usize) -> Vec<VisualRow> {
    let cols = cols.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        let mut row_start = line_start;
        let mut width = 0;
        for (index, grapheme) in line.grapheme_indices(true) {
            let grapheme_cols = grapheme_width(grapheme);
            if width + grapheme_cols > cols && width > 0 {
                rows.push(VisualRow {
                    start: row_start,
                    end: line_start + index,
                });
                row_start = line_start + index;
                width = 0;
            }
            width += grapheme_cols;
        }
        rows.push(VisualRow {
            start: row_start,
            end: line_start + line.len(),
        });
        line_start += line.len() + 1;
    }
    rows
}

/// Row and column of byte `offset`. An offset where a long line wraps
/// belongs to the start of the following row.
pub fn position_of(text: &str, rows: &[VisualRow], offset: usize) -> (usize, usize) {
    let row = rows
        .iter()
        .rposition(|row| row.start <= offset)
        .unwrap_or(0);
    let start = rows.get(row).map_or(0, |row| row.start);
    (row, columns(&text[start..offset.max(start)]))
}

/// Byte offset closest to column `col` of `row`.
pub fn offset_at(text: &str, rows: &[VisualRow], row: usize, col: usize) -> usize {
    let Some(visual) = rows.get(row) else {
        return text.len();
    };
    let mut width = 0;
    for (index, grapheme) in text[visual.start..visual.end].grapheme_indices(true) {
        let grapheme_cols = grapheme_width(grapheme);
        if width + grapheme_cols > col {
            return visual.start + index;
        }
        width += grapheme_cols;
    }
    visual.end
}

/// Columns `text` occupies on the grid.
pub fn columns(text: &str) -> usize {
    text.graphemes(true).map(grapheme_width).sum()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punctuation,
}

fn class_of(grapheme: &str) -> CharClass {
    match grapheme.chars().next() {
        Some(ch) if ch.is_whitespace() => CharClass::Space,
        Some(ch) if ch.is_alphanumeric() || ch == '_' => CharClass::Word,
        _ => CharClass::Punctuation,
    }
}

/// Text with a selection running from `anchor` to `head`, the cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorBuffer {
    text: String,
    anchor: usize,
    head: usize,
}

impl EditorBuffer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replace the text and put the cursor at its end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.anchor = self.text.len();
        self.head = self.text.len();
    }

    /// Remove all text, returning it.
    pub fn take(&mut self) -> String {
        self.anchor = 0;
        self.head = 0;
        std::mem::take(&mut self.text)
    }

    pub fn cursor(&self) -> usize {
        self.head
    }

    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    /// Select `range`, with the cursor at its end unless `reversed`.
    pub fn select(&mut self, range: Range<usize>, reversed: bool) {
        let range = self.clamp(range.start)..self.clamp(range.end);
        (self.anchor, self.head) = if reversed {
            (range.end, range.start)
        } else {
            (range.start, range.end)
        };
    }

    /// Move the cursor to `offset`, extending the selection if `extend`.
    pub fn move_to(&mut self, offset: usize, extend: bool) {
        self.head = self.clamp(offset);
        if !extend {
            self.anchor = self.head;
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.head = self.text.len();
    }

    /// Replace `range` with `text`, leaving the cursor after it.
    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        let range = self.clamp(range.start)..self.clamp(range.end);
        self.text.replace_range(range.clone(), text);
        self.move_to(range.start + text.len(), false);
    }

    /// Type `text` over the selection.
    pub fn insert(&mut self, text: &str) {
        self.replace(self.selection(), text);
    }

    /// Delete the selection, or from the cursor to `offset` when nothing is
    /// selected.
    pub fn delete_to(&mut self, offset: usize) {
        let range = if self.anchor == self.head {
            let offset = self.clamp(offset);
            self.head.min(offset)..self.head.max(offset)
        } else {
            self.selection()
        };
        self.replace(range, "");
    }

    pub fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    pub fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.text.len())
    }

    /// Start of the word before `offset`, skipping whitespace first.
    pub fn previous_word(&self, offset: usize) -> usize {
        let graphemes: Vec<(usize, &str)> = self.text[..offset].grapheme_indices(true).collect();
        let mut iter = graphemes.iter().rev().peekable();
        while iter
            .next_if(|(_, g)| class_of(g) == CharClass::Space)
            .is_some()
        {}
        let Some(&&(mut start, first)) = iter.peek() else {
            return 0;
        };
        let class = class_of(first);
        while let Some((index, _)) = iter.next_if(|(_, g)| class_of(g) == class) {
            start = *index;
        }
        start
    }

    /// End of the word after `offset`, skipping whitespace first.
    pub fn next_word(&self, offset: usize) -> usize {
        let mut iter = self.text[offset..]
            .grapheme_indices(true)
            .map(|(index, grapheme)| (offset + index, grapheme))
            .peekable();
        while iter
            .next_if(|(_, g)| class_of(g) == CharClass::Space)
            .is_some()
        {}
        let Some(&(_, first)) = iter.peek() else {
            return self.text.len();
        };
        let class = class_of(first);
        let mut end = self.text.len();
        for (index, grapheme) in iter {
            if class_of(grapheme) != class {
                end = index;
                break;
            }
        }
        end
    }

    /// Start of the line holding `offset`.
    pub fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |index| index + 1)
    }

    /// End of the line holding `offset`.
    pub fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |index| offset + index)
    }

    /// The offset one visual row up or down from the cursor, keeping its
    /// column, or `None` at the first or last row.
    pub fn vertical(&self, rows: &[VisualRow], down: bool) -> Option<usize> {
        let (row, col) = position_of(&self.text, rows, self.head);
        let target = if down {
            (row + 1 < rows.len()).then_some(row + 1)?
        } else {
            row.checked_sub(1)?
        };
        Some(offset_at(&self.text, rows, target, col))
    }

    fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> EditorBuffer {
        let mut buffer = EditorBuffer::default();
        buffer.set_text(text);
        buffer
    }

    #[test]
    fn rows_wrap_long_lines_and_keep_empty_ones() {
        let rows = layout_rows("abcdef\n\nxy", 4);
        assert_eq!(
            rows,
            vec![
                VisualRow { start: 0, end: 4 },
                VisualRow { start: 4, end: 6 },
                VisualRow { start: 7, end: 7 },
                VisualRow { start: 8, end: 10 },
            ]
        );
        // Wide characters take two columns and are never split.
        let rows = layout_rows("a日本", 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], VisualRow { start: 0, end: 4 });
    }

    #[test]
    fn positions_round_trip_through_rows() {
        let text = "abcdef\nxy";
        let rows = layout_rows(text, 4);
        assert_eq!(position_of(text, &rows, 4), (1, 0));
        assert_eq!(position_of(text, &rows, 6), (1, 2));
        assert_eq!(position_of(text, &rows, 9), (2, 2));
        assert_eq!(offset_at(text, &rows, 1, 1), 5);
        assert_eq!(offset_at(text, &rows, 2, 10), 9);
    }

    #[test]
    fn vertical_moves_keep_the_column() {
        let mut editor = buffer("hello\nhi\nworld");
        let rows = layout_rows(editor.text(), 80);
        editor.move_to(4, false);
        let down = editor.vertical(&rows, true).unwrap();
        assert_eq!(down, 8, "clamped to the end of the short line");
        editor.move_to(down, false);
        assert_eq!(editor.vertical(&rows, false), Some(2));
        editor.move_to(0, false);
        assert_eq!(editor.vertical(&rows, false), None);
    }

    #[test]
    fn words_skip_whitespace_and_stop_at_punctuation() {
        let editor = buffer("git commit --amend");
        assert_eq!(editor.previous_word(18), 13);
        assert_eq!(editor.previous_word(13), 11);
        assert_eq!(editor.previous_word(11), 4);
        assert_eq!(editor.next_word(0), 3);
        assert_eq!(editor.next_word(3), 10);
    }

    #[test]
    fn editing_replaces_the_selection() {
        let mut editor = buffer("echo hi");
        editor.select(5..7, false);
        editor.insert("there");
        assert_eq!(editor.text(), "echo there");
        assert_eq!(editor.cursor(), 10);
        let start = editor.previous_word(editor.cursor());
        editor.delete_to(start);
        assert_eq!(editor.text(), "echo ");
        assert_eq!(editor.take(), "echo ");
        assert!(editor.is_empty());
    }
}
