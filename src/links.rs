//! Finding URLs and file paths in terminal text, and opening them safely.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use gpui::App;

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    Url(String),
    Path(PathBuf),
}

/// A link found in a line, as a range of character indices (end exclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedLink {
    pub start: usize,
    pub end: usize,
    pub target: LinkTarget,
}

const URL_SCHEMES: [&str; 4] = ["https://", "http://", "ftp://", "file://"];

/// The URL or existing file path covering character `index` of `line`.
/// Relative paths resolve against `cwd`; `exists` decides whether a path is
/// real (injected so detection stays testable).
pub fn link_at(
    line: &[char],
    index: usize,
    cwd: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<DetectedLink> {
    if index >= line.len() || is_boundary(line[index]) {
        return None;
    }
    let mut start = index;
    while start > 0 && !is_boundary(line[start - 1]) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < line.len() && !is_boundary(line[end]) {
        end += 1;
    }
    let (start, end) = trim_token(line, start, end);
    if index < start || index >= end {
        return None;
    }
    let token: String = line[start..end].iter().collect();

    if URL_SCHEMES.iter().any(|scheme| token.starts_with(scheme)) || token.starts_with("mailto:") {
        return Some(DetectedLink {
            start,
            end,
            target: LinkTarget::Url(token),
        });
    }

    let path = resolve_path(strip_position(&token), cwd)?;
    exists(&path).then_some(DetectedLink {
        start,
        end,
        target: LinkTarget::Path(path),
    })
}

fn is_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | '<' | '>' | '|')
}

/// Drop surrounding punctuation that usually belongs to the sentence, such
/// as a trailing period or the parentheses around "(see https://…)".
fn trim_token(line: &[char], mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end && matches!(line[start], '(' | '[' | '{') {
        start += 1;
    }
    loop {
        if start >= end {
            break;
        }
        let last = line[end - 1];
        let unbalanced_closer = match last {
            ')' => count(line, start, end, '(') < count(line, start, end, ')'),
            ']' => count(line, start, end, '[') < count(line, start, end, ']'),
            '}' => count(line, start, end, '{') < count(line, start, end, '}'),
            _ => false,
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') || unbalanced_closer {
            end -= 1;
        } else {
            break;
        }
    }
    (start, end)
}

fn count(line: &[char], start: usize, end: usize, wanted: char) -> usize {
    line[start..end].iter().filter(|ch| **ch == wanted).count()
}

/// Remove a compiler-style `:line` or `:line:column` suffix.
fn strip_position(token: &str) -> &str {
    let mut path = token;
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':')
            && !tail.is_empty()
            && tail.chars().all(|ch| ch.is_ascii_digit())
        {
            path = head;
        }
    }
    path
}

fn resolve_path(token: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if token.is_empty() || !token.contains('/') && !token.contains('.') {
        return None;
    }
    if let Some(rest) = token.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest));
    }
    let path = Path::new(token);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        cwd.map(|cwd| cwd.join(path))
    }
}

/// Open a link with the system. Executables and app bundles are revealed
/// in Finder instead of launched, since link text comes from whatever is
/// printed to the terminal.
pub fn open(target: &LinkTarget, cx: &App) {
    match target {
        LinkTarget::Url(url) => match url.strip_prefix("file://") {
            Some(rest) => {
                let path = rest.find('/').map_or(rest, |slash| &rest[slash..]);
                open_path(Path::new(path), cx);
            }
            None => cx.open_url(url),
        },
        LinkTarget::Path(path) => open_path(path, cx),
    }
}

fn open_path(path: &Path, cx: &App) {
    if is_launchable(path) {
        cx.reveal_path(path);
    } else {
        cx.open_with_system(path);
    }
}

fn is_launchable(path: &Path) -> bool {
    let bundle_like = path.extension().is_some_and(|extension| {
        matches!(
            extension.to_string_lossy().as_ref(),
            "app" | "command" | "tool" | "terminal" | "workflow" | "pkg" | "dmg"
        )
    });
    let executable = std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    bundle_like || executable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    fn url_at(text: &str, index: usize) -> Option<String> {
        match link_at(&chars(text), index, None, |_| false)?.target {
            LinkTarget::Url(url) => Some(url),
            LinkTarget::Path(_) => None,
        }
    }

    #[test]
    fn finds_url_and_trims_sentence_punctuation() {
        let text = "see https://example.com/a?b=1. ok";
        assert_eq!(
            url_at(text, 10).as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(url_at(text, 1), None);
    }

    #[test]
    fn keeps_balanced_parentheses() {
        let text = "(https://en.wikipedia.org/wiki/Rust_(language))";
        assert_eq!(
            url_at(text, 5).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
    }

    #[test]
    fn detects_existing_relative_path_with_position() {
        let cwd = Path::new("/project");
        let found = link_at(
            &chars("error at src/main.rs:42:7 here"),
            12,
            Some(cwd),
            |path| path == Path::new("/project/src/main.rs"),
        )
        .unwrap();
        assert_eq!(
            found.target,
            LinkTarget::Path(PathBuf::from("/project/src/main.rs"))
        );
        assert_eq!((found.start, found.end), (9, 25));
    }

    #[test]
    fn ignores_missing_paths_and_plain_words() {
        let cwd = Path::new("/project");
        assert!(link_at(&chars("src/gone.rs"), 2, Some(cwd), |_| false).is_none());
        assert!(link_at(&chars("hello world"), 2, Some(cwd), |_| true).is_none());
    }

    #[test]
    fn executables_are_not_launched() {
        assert!(is_launchable(Path::new("/Applications/Calculator.app")));
        assert!(is_launchable(Path::new("/bin/ls")));
        assert!(!is_launchable(Path::new("/etc/hosts")));
    }
}
