//! Completing the word before the editor's cursor: a menu over the shell's
//! completions, and a local fallback (command names and paths) for shells
//! that cannot report their own.

use std::{fs, path::Path};

use crate::{highlight::CommandIndex, hooks::Completion, links};

/// Completions offered for the text before the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletionMenu {
    /// The text before the cursor when completion was asked for.
    anchor: String,
    /// The end of `anchor` each completion replaces.
    prefix: String,
    matches: Vec<Completion>,
    /// Indices into `matches` that fit what has been typed since.
    visible: Vec<usize>,
    selected: usize,
    /// Bytes at the start of each visible completion that match what is
    /// typed, for showing them in bold.
    matched_len: usize,
}

impl CompletionMenu {
    pub fn new(anchor: String, prefix: String, matches: Vec<Completion>) -> Self {
        let visible = (0..matches.len()).collect();
        let matched_len = prefix.len();
        Self {
            matched_len,
            anchor,
            prefix,
            matches,
            visible,
            selected: 0,
        }
    }

    /// The completions still fitting, with the selected one's index.
    pub fn visible(&self) -> impl Iterator<Item = (usize, &Completion)> {
        self.visible
            .iter()
            .enumerate()
            .map(|(index, &match_index)| (index, &self.matches[match_index]))
    }

    pub fn matched_len(&self) -> usize {
        self.matched_len
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    pub fn select_next(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + 1) % self.visible.len();
        }
    }

    pub fn select_previous(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + self.visible.len() - 1) % self.visible.len();
        }
    }

    /// Narrow the menu to what has been typed since it opened. Returns
    /// false when `before_cursor` no longer extends the anchor, and the
    /// menu no longer applies.
    pub fn refine(&mut self, before_cursor: &str) -> bool {
        let Some(typed) = before_cursor.strip_prefix(self.anchor.as_str()) else {
            return false;
        };
        if typed.contains(char::is_whitespace) {
            return false;
        }
        let wanted = format!("{}{typed}", self.prefix).to_lowercase();
        self.matched_len = self.prefix.len() + typed.len();
        self.visible = (0..self.matches.len())
            .filter(|&index| self.matches[index].word.to_lowercase().starts_with(&wanted))
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        true
    }

    /// How to apply the selected completion to `before_cursor`: the number
    /// of bytes to replace before the cursor and the text to put there.
    pub fn apply(&self, before_cursor: &str) -> Option<(usize, String)> {
        let completion = &self.matches[*self.visible.get(self.selected)?];
        Some(replacement(before_cursor, &self.prefix, &completion.word))
    }
}

/// Bytes to replace before the cursor, and the text replacing them, to
/// complete `before_cursor` with `word` in place of `prefix`.
pub fn replacement(before_cursor: &str, prefix: &str, word: &str) -> (usize, String) {
    let typed_since = before_cursor
        .len()
        .checked_sub(current_word(before_cursor).len())
        .map_or("", |start| &before_cursor[start..]);
    // The shell's prefix is normally the tail of the text; when quoting
    // makes them differ, the whole current word is replaced.
    let replaced = if before_cursor.ends_with(prefix) {
        prefix.len()
    } else {
        typed_since.len()
    };
    let mut text = word.to_string();
    if !word.ends_with(['/', '=']) {
        text.push(' ');
    }
    (replaced, text)
}

/// The word the cursor is at the end of: everything after the last
/// unescaped whitespace or shell operator.
pub fn current_word(before_cursor: &str) -> &str {
    let bytes = before_cursor.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            byte if byte.is_ascii_whitespace() || b"|&;()<>".contains(&byte) => start = index + 1,
            _ => {}
        }
        index += 1;
    }
    &before_cursor[start.min(before_cursor.len())..]
}

/// Whether the current word is in command position: first on the line or
/// after an operator.
fn in_command_position(before_cursor: &str) -> bool {
    let word = current_word(before_cursor);
    let earlier = before_cursor[..before_cursor.len() - word.len()].trim_end();
    earlier.is_empty() || earlier.ends_with(['|', '&', ';', '(', '\n'])
}

/// Completions found without the shell: command names in command
/// position, otherwise paths relative to `cwd`. Returns the prefix they
/// replace and the matches.
pub fn local_completions(
    before_cursor: &str,
    commands: Option<&CommandIndex>,
    cwd: Option<&Path>,
) -> (String, Vec<Completion>) {
    let word = current_word(before_cursor);
    let matches = if in_command_position(before_cursor) && !word.contains('/') {
        commands.map_or_else(Vec::new, |commands| {
            commands
                .names_with_prefix(word)
                .into_iter()
                .map(|name| Completion {
                    word: name,
                    description: None,
                })
                .collect()
        })
    } else {
        path_completions(word, cwd)
    };
    (word.to_string(), matches)
}

/// Most matches a local completion lists.
const MAX_LOCAL_MATCHES: usize = 200;

fn path_completions(word: &str, cwd: Option<&Path>) -> Vec<Completion> {
    let unescaped = unescape(word);
    let (directory, file_prefix) = match unescaped.rfind('/') {
        Some(slash) => (&unescaped[..=slash], &unescaped[slash + 1..]),
        None => ("", unescaped.as_str()),
    };
    let expanded = match directory.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => Path::new(&home).join(rest),
            None => return Vec::new(),
        },
        None if directory.starts_with('/') => Path::new(directory).to_path_buf(),
        None => match cwd {
            Some(cwd) => cwd.join(directory),
            None => return Vec::new(),
        },
    };
    // Listing a network share can mount it.
    if links::is_network_location(&expanded) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&expanded) else {
        return Vec::new();
    };
    let mut matches: Vec<Completion> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let hidden_unasked = name.starts_with('.') && !file_prefix.starts_with('.');
            if hidden_unasked || !name.starts_with(file_prefix) {
                return None;
            }
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let suffix = if is_dir { "/" } else { "" };
            Some(Completion {
                word: format!("{}{}{suffix}", escape(directory), escape(&name)),
                description: None,
            })
        })
        .take(MAX_LOCAL_MATCHES)
        .collect();
    matches.sort_by(|a, b| a.word.cmp(&b.word));
    matches
}

fn unescape(word: &str) -> String {
    let mut output = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => output.extend(chars.next()),
            ch => output.push(ch),
        }
    }
    output
}

/// Backslash-escape characters the shell would otherwise interpret.
fn escape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace() || "\\'\"`$&|;<>()*?[]{}!#".contains(ch) {
            output.push('\\');
        }
        output.push(ch);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completions(words: &[&str]) -> Vec<Completion> {
        words
            .iter()
            .map(|word| Completion {
                word: word.to_string(),
                description: None,
            })
            .collect()
    }

    #[test]
    fn the_menu_narrows_as_the_user_types_and_closes_on_other_edits() {
        let mut menu = CompletionMenu::new(
            "git che".into(),
            "che".into(),
            completions(&["checkout", "cherry-pick", "cherry"]),
        );
        assert!(menu.refine("git cher"));
        let words: Vec<&str> = menu.visible().map(|(_, c)| c.word.as_str()).collect();
        assert_eq!(words, vec!["cherry-pick", "cherry"]);
        menu.select_next();
        assert_eq!(menu.apply("git cher"), Some((4, "cherry ".into())));
        assert!(!menu.refine("git ch"));
        assert!(!menu.refine("git cher x"));
    }

    #[test]
    fn directories_and_assignments_take_no_space() {
        assert_eq!(
            replacement("ls ~/co", "~/co", "~/code/"),
            (4, "~/code/".into())
        );
        assert_eq!(replacement("echo $HO", "HO", "HOME"), (2, "HOME ".into()));
        assert_eq!(
            replacement("ls my\\ fi", "my fi", "my\\ file"),
            (6, "my\\ file ".into())
        );
    }

    #[test]
    fn current_word_respects_escapes_and_operators() {
        assert_eq!(current_word("cat a|gr"), "gr");
        assert_eq!(current_word("ls my\\ fi"), "my\\ fi");
        assert_eq!(current_word("ls "), "");
        assert!(in_command_position("cd /tmp && gi"));
        assert!(!in_command_position("git che"));
    }

    #[test]
    fn local_completion_lists_commands_and_paths() {
        let index = CommandIndex::from_names(&["git", "gitk", "grep"]);
        let (prefix, matches) = local_completions("gi", Some(&index), None);
        assert_eq!(prefix, "gi");
        let words: Vec<&str> = matches.iter().map(|c| c.word.as_str()).collect();
        assert_eq!(words, vec!["git", "gitk"]);

        let (_, matches) = local_completions("ls /usr/lo", None, None);
        assert_eq!(matches[0].word, "/usr/local/");
        let (_, matches) = local_completions("ls /net/host/", None, None);
        assert!(matches.is_empty());
    }
}
