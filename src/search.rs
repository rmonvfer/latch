//! Finding text in a terminal's screen and scrollback.

use anyhow::Result;
use libghostty_vt::{
    Terminal,
    fmt::{Format, Formatter, FormatterOptions},
};
use unicode_width::UnicodeWidthChar;

/// A match on one screen row, in grid columns (end exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    /// Row in screen coordinates: 0 is the oldest scrollback line.
    pub row: u32,
    pub start_col: u16,
    pub end_col: u16,
}

/// The terminal's full contents as plain text, one line per grid row.
pub fn screen_text(terminal: &Terminal<'static, 'static>) -> Result<String> {
    let mut formatter = Formatter::new(
        terminal,
        FormatterOptions::new()
            .with_format(Format::Plain)
            .with_unwrap(false)
            .with_trim(false),
    )?;
    let bytes = formatter.format_alloc(None)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Every occurrence of `query` in `text`. Matching ignores case unless the
/// query contains an uppercase letter.
pub fn find_matches(text: &str, query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let case_sensitive = query.chars().any(char::is_uppercase);
    let normalize = |ch: char| {
        if case_sensitive {
            ch
        } else {
            ch.to_lowercase().next().unwrap_or(ch)
        }
    };
    let needle: Vec<char> = query.chars().map(normalize).collect();

    let mut matches = Vec::new();
    for (row, line) in text.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        if chars.len() < needle.len() {
            continue;
        }
        // Grid column where each character starts; wide characters take two.
        let mut columns = Vec::with_capacity(chars.len() + 1);
        let mut column = 0u16;
        for ch in &chars {
            columns.push(column);
            column += ch.width().unwrap_or(0).max(1) as u16;
        }
        columns.push(column);

        let mut start = 0;
        while start + needle.len() <= chars.len() {
            let found = chars[start..start + needle.len()]
                .iter()
                .zip(&needle)
                .all(|(ch, wanted)| normalize(*ch) == *wanted);
            if found {
                matches.push(SearchMatch {
                    row: row as u32,
                    start_col: columns[start],
                    end_col: columns[start + needle.len()],
                });
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::terminal::Options;

    #[test]
    fn finds_matches_with_smart_case() {
        let text = "Error: boom\nno errors here\nERROR again";
        let matches = find_matches(text, "error");
        assert_eq!(matches.len(), 3);
        assert_eq!(
            matches[1],
            SearchMatch {
                row: 1,
                start_col: 3,
                end_col: 8
            }
        );
        assert_eq!(find_matches(text, "ERROR").len(), 1);
    }

    #[test]
    fn wide_characters_shift_columns() {
        let matches = find_matches("中文 ok", "ok");
        assert_eq!(
            matches,
            vec![SearchMatch {
                row: 0,
                start_col: 5,
                end_col: 7
            }]
        );
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(find_matches("aaaa", "aa").len(), 2);
        assert!(find_matches("abc", "").is_empty());
    }

    #[test]
    fn screen_text_includes_scrollback_rows() {
        let mut terminal = Terminal::new(Options {
            cols: 10,
            rows: 3,
            max_scrollback: 100,
        })
        .unwrap();
        for index in 0..8 {
            terminal.vt_write(format!("line{index}\r\n").as_bytes());
        }
        let text = screen_text(&terminal).unwrap();
        let matches = find_matches(&text, "line2");
        assert_eq!(
            matches,
            vec![SearchMatch {
                row: 2,
                start_col: 0,
                end_col: 5
            }]
        );
        // Trailing blank rows are omitted, so rows never outnumber the grid.
        assert!(text.split('\n').count() <= terminal.total_rows().unwrap());
    }
}
