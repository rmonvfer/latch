//! Command history for the editor: the shell's history file merged with
//! commands run in the pane, newest last, each command once.

use std::{fs, path::Path};

use anyhow::Result;

/// Most entries kept.
const MAX_ENTRIES: usize = 20_000;

/// zsh writes bytes above 0x82 escaped: this marker, then the byte XOR 0x20.
const ZSH_META: u8 = 0x83;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    entries: Vec<String>,
}

impl History {
    /// History of `entries`, oldest first; repeats keep their latest place.
    pub fn from_entries(entries: impl IntoIterator<Item = String>) -> Self {
        let mut history = Self::default();
        for entry in entries {
            history.push(&entry);
        }
        history
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The `index`th entry, oldest first.
    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    /// Record a command that ran, moving it to the newest place.
    pub fn push(&mut self, command: &str) {
        let command = command.trim_end();
        if command.trim().is_empty() {
            return;
        }
        if let Some(index) = self.entries.iter().rposition(|entry| entry == command) {
            self.entries.remove(index);
        }
        self.entries.push(command.to_string());
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Append `older` entries before the current ones, as when the history
    /// file finishes loading after commands already ran.
    pub fn prepend(&mut self, older: History) {
        let current = std::mem::take(&mut self.entries);
        self.entries = older.entries;
        for entry in current {
            self.push(&entry);
        }
    }

    /// The rest of the newest entry that starts with `prefix`.
    pub fn suggestion(&self, prefix: &str) -> Option<&str> {
        if prefix.trim().is_empty() {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.len() > prefix.len() && entry.starts_with(prefix))
            .map(|entry| &entry[prefix.len()..])
    }

    /// Entries matching `query` as a fuzzy subsequence, best first: tighter
    /// matches rank higher, then newer ones.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&str> {
        let query: Vec<char> = query.to_lowercase().chars().collect();
        let mut matches: Vec<(usize, usize, &str)> = self
            .entries
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(age, entry)| {
                fuzzy_spread(entry, &query).map(|spread| (spread, age, entry.as_str()))
            })
            .collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        matches
            .into_iter()
            .take(limit)
            .map(|(_, _, entry)| entry)
            .collect()
    }
}

/// How many characters a match of `query` in `text` spans, if all of
/// `query` appears in order (case-insensitively). An empty query matches
/// everything with no spread.
fn fuzzy_spread(text: &str, query: &[char]) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut best: Option<usize> = None;
    // Try each start of the first query character and take the tightest.
    for start in (0..text.len()).filter(|&index| text[index] == query[0]) {
        let mut position = start;
        let mut matched = 1;
        for &ch in &query[1..] {
            match text[position + 1..]
                .iter()
                .position(|&candidate| candidate == ch)
            {
                Some(offset) => {
                    position += offset + 1;
                    matched += 1;
                }
                None => break,
            }
        }
        if matched == query.len() {
            let spread = position - start;
            best = Some(best.map_or(spread, |best| best.min(spread)));
        }
    }
    best
}

/// Commands in a history file, oldest first. `shell` picks the format.
pub fn load(path: &Path, shell: &str) -> Result<History> {
    let bytes = fs::read(path)?;
    let entries = if shell == "zsh" {
        parse_zsh(&bytes)
    } else {
        parse_bash(&String::from_utf8_lossy(&bytes))
    };
    // Only the newest entries are kept, so skip parsing work on the rest.
    let start = entries.len().saturating_sub(MAX_ENTRIES);
    Ok(History::from_entries(entries.into_iter().skip(start)))
}

/// Entries of a zsh history file, plain or extended (`: <time>:<secs>;cmd`).
/// A line ending in a backslash continues on the next line.
pub fn parse_zsh(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(&unmetafy(bytes)).into_owned();
    let mut entries = Vec::new();
    let mut current: Option<String> = None;
    for line in text.split('\n') {
        let line = match current.take() {
            Some(mut pending) => {
                pending.push('\n');
                pending.push_str(line);
                pending
            }
            None => strip_extended_prefix(line).to_string(),
        };
        if let Some(continued) = line.strip_suffix('\\') {
            current = Some(continued.to_string());
            continue;
        }
        if !line.trim().is_empty() {
            entries.push(line);
        }
    }
    if let Some(pending) = current.filter(|pending| !pending.trim().is_empty()) {
        entries.push(pending);
    }
    entries
}

fn strip_extended_prefix(line: &str) -> &str {
    let Some(rest) = line.strip_prefix(": ") else {
        return line;
    };
    match rest.split_once(';') {
        Some((stamp, command))
            if stamp
                .split(':')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())) =>
        {
            command
        }
        _ => line,
    }
}

fn unmetafy(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut iter = bytes.iter();
    while let Some(&byte) = iter.next() {
        if byte == ZSH_META {
            if let Some(&next) = iter.next() {
                output.push(next ^ 0x20);
            }
        } else {
            output.push(byte);
        }
    }
    output
}

/// Entries of a bash history file, skipping `#<time>` stamp lines.
pub fn parse_bash(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| {
            !line.trim().is_empty()
                && !(line.starts_with('#')
                    && line.len() > 1
                    && line[1..].bytes().all(|byte| byte.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zsh_extended_and_multiline_entries() {
        let file = b": 1696512345:0;git status\n: 1696512350:3;echo one\\\ntwo\nls -la\n";
        assert_eq!(
            parse_zsh(file),
            vec!["git status", "echo one\ntwo", "ls -la"]
        );
    }

    #[test]
    fn unmetafies_zsh_bytes() {
        // "é" is 0xC3 0xA9; zsh stores 0xA9 as 0x83 0x89.
        let file = [
            b'e',
            b'c',
            b'h',
            b'o',
            b' ',
            0xC3,
            ZSH_META,
            0xA9 ^ 0x20,
            b'\n',
        ];
        assert_eq!(parse_zsh(&file), vec!["echo é"]);
    }

    #[test]
    fn parses_bash_with_timestamps() {
        assert_eq!(
            parse_bash("#1696512345\nmake test\n\n#1696512399\ncd /tmp\n"),
            vec!["make test", "cd /tmp"]
        );
    }

    #[test]
    fn repeats_move_to_the_newest_place() {
        let mut history = History::from_entries(["a", "b", "a"].map(String::from));
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(1), Some("a"));
        history.prepend(History::from_entries(["old", "b"].map(String::from)));
        let entries: Vec<&str> = (0..history.len()).filter_map(|i| history.get(i)).collect();
        assert_eq!(entries, vec!["old", "b", "a"]);
    }

    #[test]
    fn suggests_the_newest_extension() {
        let history =
            History::from_entries(["git status", "git stash pop", "git commit"].map(String::from));
        assert_eq!(history.suggestion("git st"), Some("ash pop"));
        assert_eq!(history.suggestion("git commit"), None);
        assert_eq!(history.suggestion(""), None);
    }

    #[test]
    fn fuzzy_search_prefers_tight_then_recent_matches() {
        let history = History::from_entries(
            [
                "docker compose up",
                "cargo build",
                "cd build",
                "cargo bench",
            ]
            .map(String::from),
        );
        assert_eq!(
            history.search("cb", 10),
            vec!["cd build", "cargo bench", "cargo build"]
        );
        assert_eq!(history.search("bld", 10), vec!["cd build", "cargo build"]);
        assert_eq!(history.search("", 2), vec!["cargo bench", "cd build"]);
    }
}
