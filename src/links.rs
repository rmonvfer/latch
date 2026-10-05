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
    // Checking whether a network location exists can mount it, so such
    // paths are never looked up just because the pointer passed over them.
    if is_network_location(&path) {
        return None;
    }
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
    if token.is_empty() || !token.contains('/') && !token.contains('.') || token.starts_with("//") {
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

/// Open a link with the system. Only directories and files of known
/// document and source types are opened; anything else (executables, app
/// bundles, installers, scripts, unknown types) is revealed in Finder
/// instead, since link text comes from whatever is printed to the terminal.
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
    if is_network_location(path) {
        return;
    }
    if is_safe_to_open(path) {
        cx.open_with_system(path);
    } else {
        cx.reveal_path(path);
    }
}

/// File types that open in a viewer or editor rather than running anything.
const OPENABLE_EXTENSIONS: [&str; 52] = [
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "jsonc", "yaml", "yml", "toml",
    "ini", "cfg", "conf", "xml", "html", "htm", "css", "scss", "rs", "go", "py", "rb", "js", "mjs",
    "cjs", "ts", "tsx", "jsx", "java", "kt", "swift", "c", "h", "cpp", "hpp", "cc", "cs", "php",
    "lua", "sql", "proto", "diff", "patch", "png", "jpg", "jpeg", "gif", "webp", "pdf", "svg",
];

fn is_safe_to_open(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    if meta.is_dir() {
        // Packages such as Foo.app are directories with an extension.
        return extension.is_none();
    }
    let executable = meta.permissions().mode() & 0o111 != 0;
    meta.is_file()
        && !executable
        && extension.is_some_and(|extension| OPENABLE_EXTENSIONS.contains(&extension.as_str()))
}

fn is_network_location(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("//") || text.starts_with("/net/") || text.starts_with("/Network/")
}

#[cfg(test)]
mod tests {
    use std::fs;

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
    fn only_known_document_types_are_opened() {
        let dir = std::env::temp_dir().join(format!("links-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("Tool.app")).unwrap();
        for name in ["notes.md", "script.scpt", "plain"] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let executable = dir.join("run.py");
        fs::write(&executable, "x").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(is_safe_to_open(&dir.join("notes.md")));
        assert!(is_safe_to_open(&dir));
        assert!(!is_safe_to_open(&dir.join("Tool.app")));
        assert!(!is_safe_to_open(&dir.join("script.scpt")));
        assert!(!is_safe_to_open(&dir.join("plain")));
        assert!(!is_safe_to_open(&executable));
        assert!(!is_safe_to_open(Path::new("/bin/ls")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn network_paths_are_never_checked() {
        let checked = std::cell::Cell::new(false);
        let found = link_at(&chars("/net/server/share/file.txt"), 3, None, |_| {
            checked.set(true);
            true
        });
        assert!(found.is_none());
        assert!(!checked.get());
        assert!(link_at(&chars("//server/share.txt"), 3, None, |_| true).is_none());
    }
}
