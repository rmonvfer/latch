//! Command history for the editor: the shell's history file merged with
//! commands run in the pane, newest last, each command once, with when it
//! last ran where that is known.

use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;

/// Most entries kept.
const MAX_ENTRIES: usize = 20_000;

/// zsh writes bytes above 0x82 escaped: this marker, then the byte XOR 0x20.
const ZSH_META: u8 = 0x83;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub command: String,
    /// When the command last ran, in seconds since the Unix epoch.
    pub time: Option<i64>,
}

impl Entry {
    pub fn new(command: impl Into<String>, time: Option<i64>) -> Self {
        Self {
            command: command.into(),
            time,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    entries: Vec<Entry>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

impl History {
    /// History of `entries`, oldest first; repeats keep their latest place.
    pub fn from_entries(entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut history = Self::default();
        for entry in entries {
            history.insert(entry);
        }
        history
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The `index`th entry, oldest first.
    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// Record a command that ran now, moving it to the newest place.
    pub fn push(&mut self, command: &str) {
        self.insert(Entry::new(command, Some(now())));
    }

    fn insert(&mut self, entry: Entry) {
        let command = entry.command.trim_end();
        if command.trim().is_empty() {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|existing| existing.command == command)
        {
            self.entries.remove(index);
        }
        self.entries.push(Entry::new(command, entry.time));
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Put `older` entries before the current ones, as when the history
    /// file finishes loading after commands already ran.
    pub fn prepend(&mut self, older: History) {
        let current = std::mem::take(&mut self.entries);
        self.entries = older.entries;
        for entry in current {
            self.insert(entry);
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
            .find(|entry| entry.command.len() > prefix.len() && entry.command.starts_with(prefix))
            .map(|entry| &entry.command[prefix.len()..])
    }

    /// Indices of entries starting with `prefix`, newest first.
    pub fn starting_with(&self, prefix: &str, limit: usize) -> Vec<usize> {
        (0..self.entries.len())
            .rev()
            .filter(|&index| self.entries[index].command.starts_with(prefix))
            .take(limit)
            .collect()
    }

    /// Indices of entries matching `query` as a fuzzy subsequence, best
    /// first: tighter matches rank higher, then newer ones.
    pub fn search(&self, query: &str, limit: usize) -> Vec<usize> {
        let query: Vec<char> = query.to_lowercase().chars().collect();
        let mut matches: Vec<(usize, usize)> = (0..self.entries.len())
            .rev()
            .filter_map(|index| {
                fuzzy_match(&self.entries[index].command, &query)
                    .map(|(start, end)| (end - start, index))
            })
            .collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        matches
            .into_iter()
            .take(limit)
            .map(|(_, index)| index)
            .collect()
    }
}

/// Character positions of the tightest match of `query` in `text`, if all
/// of `query` appears in order (case-insensitively), for showing what
/// matched.
pub fn fuzzy_positions(text: &str, query: &str) -> Vec<usize> {
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let Some((start, _)) = fuzzy_match(text, &query) else {
        return Vec::new();
    };
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let mut positions = Vec::with_capacity(query.len());
    let mut position = start;
    for &ch in &query {
        while position < lower.len() && lower[position] != ch {
            position += 1;
        }
        positions.push(position);
        position += 1;
    }
    positions
}

/// Start and end character positions of the tightest match of `query` in
/// `text`. An empty query matches at the start.
fn fuzzy_match(text: &str, query: &[char]) -> Option<(usize, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut best: Option<(usize, usize)> = None;
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
        if matched == query.len()
            && best.is_none_or(|(best_start, best_end)| position - start < best_end - best_start)
        {
            best = Some((start, position));
        }
    }
    best
}

/// How long ago `time` was, briefly: `now`, `5m`, `3h`, `2d`.
pub fn relative_time(time: i64) -> String {
    let seconds = (now() - time).max(0);
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Commands in a history file, oldest first. `shell` picks the format.
pub fn load(path: &Path, shell: &str) -> Result<History> {
    let bytes = fs::read(path)?;
    let entries = if shell == "zsh" {
        parse_zsh(&bytes)
    } else {
        parse_bash(&String::from_utf8_lossy(&bytes))
    };
    // Only the newest entries are kept, so skip work on the rest.
    let start = entries.len().saturating_sub(MAX_ENTRIES);
    Ok(History::from_entries(entries.into_iter().skip(start)))
}

/// Entries of a zsh history file, plain or extended (`: <time>:<secs>;cmd`).
/// A line ending in a backslash continues on the next line.
pub fn parse_zsh(bytes: &[u8]) -> Vec<Entry> {
    let text = String::from_utf8_lossy(&unmetafy(bytes)).into_owned();
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    for line in text.split('\n') {
        let mut entry = match current.take() {
            Some(mut pending) => {
                pending.command.push('\n');
                pending.command.push_str(line);
                pending
            }
            None => {
                let (time, command) = split_extended_prefix(line);
                Entry::new(command, time)
            }
        };
        if let Some(continued) = entry.command.strip_suffix('\\') {
            entry.command = continued.to_string();
            current = Some(entry);
            continue;
        }
        if !entry.command.trim().is_empty() {
            entries.push(entry);
        }
    }
    if let Some(pending) = current.filter(|pending| !pending.command.trim().is_empty()) {
        entries.push(pending);
    }
    entries
}

/// The time and command of a zsh history line, which in the extended format
/// starts with `: <time>:<seconds it ran>;`.
fn split_extended_prefix(line: &str) -> (Option<i64>, &str) {
    let Some(rest) = line.strip_prefix(": ") else {
        return (None, line);
    };
    match rest.split_once(';') {
        Some((stamp, command))
            if stamp
                .split(':')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())) =>
        {
            (
                stamp.split(':').next().and_then(|time| time.parse().ok()),
                command,
            )
        }
        _ => (None, line),
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

/// Entries of a bash history file, timed by `#<time>` lines where bash
/// wrote them.
pub fn parse_bash(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut time = None;
    for line in text.lines() {
        if let Some(stamp) = line
            .strip_prefix('#')
            .filter(|stamp| !stamp.is_empty() && stamp.bytes().all(|byte| byte.is_ascii_digit()))
        {
            time = stamp.parse().ok();
            continue;
        }
        if !line.trim().is_empty() {
            entries.push(Entry::new(line, time.take()));
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.command.as_str()).collect()
    }

    fn history(commands: &[&str]) -> History {
        History::from_entries(commands.iter().map(|command| Entry::new(*command, None)))
    }

    #[test]
    fn parses_zsh_extended_and_multiline_entries() {
        let file = b": 1696512345:0;git status\n: 1696512350:3;echo one\\\ntwo\nls -la\n";
        let entries = parse_zsh(file);
        assert_eq!(
            commands(&entries),
            vec!["git status", "echo one\ntwo", "ls -la"]
        );
        assert_eq!(entries[0].time, Some(1_696_512_345));
        assert_eq!(entries[2].time, None);
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
        assert_eq!(commands(&parse_zsh(&file)), vec!["echo é"]);
    }

    #[test]
    fn parses_bash_with_timestamps() {
        let entries = parse_bash("#1696512345\nmake test\n\ncd /tmp\n");
        assert_eq!(commands(&entries), vec!["make test", "cd /tmp"]);
        assert_eq!(entries[0].time, Some(1_696_512_345));
        assert_eq!(entries[1].time, None);
    }

    #[test]
    fn repeats_move_to_the_newest_place() {
        let mut history = history(&["a", "b", "a"]);
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(1).unwrap().command, "a");
        history.prepend(self::history(&["old", "b"]));
        let entries: Vec<&str> = (0..history.len())
            .filter_map(|index| history.get(index))
            .map(|entry| entry.command.as_str())
            .collect();
        assert_eq!(entries, vec!["old", "b", "a"]);
        history.push("b");
        assert!(history.get(2).unwrap().time.is_some());
    }

    #[test]
    fn suggests_the_newest_extension() {
        let history = history(&["git status", "git stash pop", "git commit"]);
        assert_eq!(history.suggestion("git st"), Some("ash pop"));
        assert_eq!(history.suggestion("git commit"), None);
        assert_eq!(history.suggestion(""), None);
        assert_eq!(history.starting_with("git st", 10), vec![1, 0]);
    }

    #[test]
    fn fuzzy_search_prefers_tight_then_recent_matches() {
        let history = history(&[
            "docker compose up",
            "cargo build",
            "cd build",
            "cargo bench",
        ]);
        let found = |query| {
            history
                .search(query, 10)
                .into_iter()
                .map(|index| history.get(index).unwrap().command.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(found("cb"), vec!["cd build", "cargo bench", "cargo build"]);
        assert_eq!(found("bld"), vec!["cd build", "cargo build"]);
        assert_eq!(fuzzy_positions("cargo build", "bld"), vec![6, 9, 10]);
        assert!(fuzzy_positions("cargo", "x").is_empty());
    }
}
