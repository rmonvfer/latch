//! Filtering a block's output lines, as Warp's block filter does: by text or
//! regular expression, with or without case, kept or inverted, with lines
//! of context around each match.

use regex::{Regex, RegexBuilder};

use crate::{block_view::RowMatch, editor_buffer};

/// Most lines of context kept around a match.
pub const MAX_CONTEXT: usize = 99;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterOptions {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// Keep the lines that do not match.
    pub invert: bool,
    /// Lines kept before and after each match.
    pub context: usize,
}

/// What a filter keeps of a block's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    /// Indices of the rows kept, in order.
    pub kept: Vec<usize>,
    /// Positions in `kept` before which rows were left out.
    pub gaps: Vec<usize>,
    /// Matches in the kept rows, by position in `kept`.
    pub matches: Vec<RowMatch>,
    /// The regular expression does not parse; nothing is filtered.
    pub invalid: bool,
}

/// Byte ranges of the matches of `pattern` in `text`.
fn find(pattern: &Regex, text: &str) -> Vec<(usize, usize)> {
    pattern
        .find_iter(text)
        .filter(|found| !found.is_empty())
        .map(|found| (found.start(), found.end()))
        .collect()
}

/// Filter `texts`, one per row, by `options`. An empty query keeps every
/// row.
pub fn filter(texts: &[String], options: &FilterOptions) -> Filtered {
    if options.query.is_empty() {
        return Filtered {
            kept: (0..texts.len()).collect(),
            ..Filtered::default()
        };
    }
    // Plain text is matched as an escaped pattern, which handles case
    // folding across all of Unicode.
    let source = if options.regex {
        options.query.clone()
    } else {
        regex::escape(&options.query)
    };
    let Ok(pattern) = RegexBuilder::new(&source)
        .case_insensitive(!options.case_sensitive)
        .build()
    else {
        return Filtered {
            kept: (0..texts.len()).collect(),
            invalid: true,
            ..Filtered::default()
        };
    };
    let found: Vec<Vec<(usize, usize)>> = texts.iter().map(|text| find(&pattern, text)).collect();
    let selected: Vec<bool> = found
        .iter()
        .map(|matches| matches.is_empty() == options.invert)
        .collect();
    let context = options.context.min(MAX_CONTEXT);
    let mut keep = vec![false; texts.len()];
    for (row, _) in selected
        .iter()
        .enumerate()
        .filter(|(_, selected)| **selected)
    {
        let from = row.saturating_sub(context);
        let to = (row + context).min(texts.len().saturating_sub(1));
        keep[from..=to].iter_mut().for_each(|kept| *kept = true);
    }
    let mut filtered = Filtered::default();
    for (row, _) in keep.iter().enumerate().filter(|(_, kept)| **kept) {
        let position = filtered.kept.len();
        let skipped = match filtered.kept.last() {
            Some(previous) => row > previous + 1,
            None => row > 0,
        };
        if skipped {
            filtered.gaps.push(position);
        }
        filtered.kept.push(row);
        if !options.invert {
            let text = &texts[row];
            filtered
                .matches
                .extend(found[row].iter().filter(|(start, end)| end > start).map(
                    |&(start, end)| {
                        let start_col = editor_buffer::columns(&text[..start]);
                        RowMatch {
                            row: position,
                            start: start_col,
                            end: start_col + editor_buffer::columns(&text[start..end]),
                            current: false,
                        }
                    },
                ));
        }
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn options(query: &str) -> FilterOptions {
        FilterOptions {
            query: query.into(),
            ..FilterOptions::default()
        }
    }

    #[test]
    fn text_filters_ignore_case_unless_asked() {
        let texts = lines("Error one\nok\nerror two\nok");
        assert_eq!(filter(&texts, &options("error")).kept, vec![0, 2]);
        let sensitive = FilterOptions {
            case_sensitive: true,
            ..options("error")
        };
        assert_eq!(filter(&texts, &sensitive).kept, vec![2]);
    }

    #[test]
    fn regexes_match_and_bad_ones_filter_nothing() {
        let texts = lines("a1\nb\nc22");
        let pattern = FilterOptions {
            regex: true,
            ..options(r"\d+")
        };
        let filtered = filter(&texts, &pattern);
        assert_eq!(filtered.kept, vec![0, 2]);
        assert_eq!(
            filtered.matches[1],
            RowMatch {
                row: 1,
                start: 1,
                end: 3,
                current: false
            }
        );
        let broken = FilterOptions {
            regex: true,
            ..options("(")
        };
        let filtered = filter(&texts, &broken);
        assert!(filtered.invalid);
        assert_eq!(filtered.kept.len(), 3);
    }

    #[test]
    fn inverting_keeps_the_rest_and_context_marks_gaps() {
        let texts = lines("0\nx\n2\n3\n4\nx\n6");
        let inverted = FilterOptions {
            invert: true,
            ..options("x")
        };
        assert_eq!(filter(&texts, &inverted).kept, vec![0, 2, 3, 4, 6]);
        let with_context = FilterOptions {
            context: 1,
            ..options("x")
        };
        let filtered = filter(&texts, &with_context);
        assert_eq!(filtered.kept, vec![0, 1, 2, 4, 5, 6]);
        assert_eq!(filtered.gaps, vec![3]);
        assert_eq!(filter(&texts, &options("4")).gaps, vec![0]);
    }
}
