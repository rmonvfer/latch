//! Splitting PTY output into what the terminal should draw and the events
//! the app reacts to, in order: lifecycle hooks from shell integration
//! (private DCS, removed from the output), command boundaries (OSC 133),
//! and desktop notification requests (OSC 9 and OSC 777). OSC sequences
//! stay in the output, since libghostty uses OSC 133 itself.

use crate::hooks::{self, Hook};

#[derive(Clone, Debug, PartialEq)]
pub enum OscEvent {
    /// OSC 133;A — the shell is drawing a prompt.
    PromptStarted,
    /// OSC 133;C — a command started running.
    CommandStarted,
    /// OSC 133;D — the command finished, with its exit status if reported.
    CommandFinished(Option<i32>),
    /// OSC 9;<body> or OSC 777;notify;<title>;<body>.
    Notify { title: Option<String>, body: String },
    /// A lifecycle hook from this pane's shell.
    Hook(Hook),
}

/// A run of output for the terminal, or an event found between runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    Output(Vec<u8>),
    Event(OscEvent),
}

const OSC: &[u8] = b"\x1b]";
const HOOK: &[u8] = b"\x1bP$d";
const STRING_TERMINATOR: &[u8] = b"\x1b\\";
/// Longest notification text kept; anything longer is cut.
const MAX_NOTIFICATION_CHARS: usize = 240;
/// Longest OSC sequence worth holding across reads.
const MAX_PENDING_OSC: usize = 4096;
/// Longest hook worth holding across reads; the startup hook lists every
/// alias, function, and builtin.
const MAX_PENDING_HOOK: usize = 1 << 20;

/// Splits PTY output, including sequences split across reads. Hooks are
/// accepted only from `session`, the id handed to this pane's shell.
pub struct OscScanner {
    pending: Vec<u8>,
    /// How much of a pending hook's payload is already known to be hex,
    /// so each read checks only the new bytes.
    verified_payload: usize,
    session: String,
}

impl OscScanner {
    pub fn new(session: String) -> Self {
        Self {
            pending: Vec::new(),
            verified_payload: 0,
            session,
        }
    }

    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Piece> {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        let mut pieces = Vec::new();
        // Start of the output not yet handed out.
        let mut flushed = 0;
        let mut index = 0;

        let flush = |pieces: &mut Vec<Piece>, from: usize, to: usize, buffer: &[u8]| {
            if to > from {
                pieces.push(Piece::Output(buffer[from..to].to_vec()));
            }
        };

        while let Some(offset) = buffer[index..].iter().position(|byte| *byte == 0x1b) {
            let start = index + offset;
            let rest = &buffer[start..];

            if rest.starts_with(HOOK) {
                let body_start = start + HOOK.len();
                // Resume where the previous read stopped checking.
                let resume = if start == 0 {
                    std::mem::take(&mut self.verified_payload)
                } else {
                    0
                };
                let body_len = match hook_payload(&buffer[body_start..], resume) {
                    HookPayload::Complete(body_len) => body_len,
                    HookPayload::Incomplete(verified)
                        if buffer.len() - start <= MAX_PENDING_HOOK =>
                    {
                        flush(&mut pieces, flushed, start, &buffer);
                        self.pending = buffer[start..].to_vec();
                        self.verified_payload = verified;
                        return pieces;
                    }
                    // Not hex, or too long: not a hook, so it is ordinary
                    // output and reaches the terminal unchanged.
                    HookPayload::Incomplete(_) | HookPayload::NotAHook => {
                        index = start + 1;
                        continue;
                    }
                };
                // The hook never reaches the terminal, valid or not.
                flush(&mut pieces, flushed, start, &buffer);
                let body = &buffer[body_start..body_start + body_len];
                if let Some(hook) = hooks::decode(body, &self.session) {
                    pieces.push(Piece::Event(OscEvent::Hook(hook)));
                }
                index = body_start + body_len + STRING_TERMINATOR.len();
                flushed = index;
                continue;
            }

            if rest.starts_with(OSC) {
                let body_start = start + OSC.len();
                let Some((body_len, terminator_len)) = terminator(&buffer[body_start..]) else {
                    flush(&mut pieces, flushed, start, &buffer);
                    if buffer.len() - start <= MAX_PENDING_OSC {
                        self.pending = buffer[start..].to_vec();
                    }
                    return pieces;
                };
                let end = body_start + body_len + terminator_len;
                flush(&mut pieces, flushed, end, &buffer);
                flushed = end;
                if let Some(event) = parse(&buffer[body_start..body_start + body_len]) {
                    pieces.push(Piece::Event(event));
                }
                index = end;
                continue;
            }

            // Too short to tell yet: wait for the rest of the sequence.
            if HOOK.starts_with(rest) || OSC.starts_with(rest) {
                flush(&mut pieces, flushed, start, &buffer);
                self.pending = rest.to_vec();
                return pieces;
            }
            index = start + 1;
        }
        flush(&mut pieces, flushed, buffer.len(), &buffer);
        pieces
    }
}

enum HookPayload {
    /// Hex digits followed by the string terminator; the payload length.
    Complete(usize),
    /// Hex digits so far with no terminator yet; how many were checked.
    Incomplete(usize),
    NotAHook,
}

/// Check a hook payload, starting `from` bytes in (those are already
/// known to be hex).
fn hook_payload(bytes: &[u8], from: usize) -> HookPayload {
    for (index, byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            byte if byte.is_ascii_hexdigit() => {}
            0x1b => {
                return match bytes.get(index + 1) {
                    Some(b'\\') => HookPayload::Complete(index),
                    Some(_) => HookPayload::NotAHook,
                    None => HookPayload::Incomplete(index),
                };
            }
            _ => return HookPayload::NotAHook,
        }
    }
    HookPayload::Incomplete(bytes.len())
}

/// Length of the body and of its terminator (BEL or ESC \).
fn terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            0x07 => return Some((index, 1)),
            0x1b => {
                return match bytes.get(index + 1) {
                    Some(b'\\') => Some((index, 2)),
                    // A new escape before a terminator: drop the broken sequence.
                    Some(_) => Some((index, 0)),
                    None => None,
                };
            }
            _ => {}
        }
    }
    None
}

fn parse(body: &[u8]) -> Option<OscEvent> {
    let body = String::from_utf8_lossy(body);
    let (code, rest) = body.split_once(';').unwrap_or((&body, ""));
    match code {
        "133" => {
            let mut parts = rest.split(';');
            match parts.next()? {
                "A" => Some(OscEvent::PromptStarted),
                "C" => Some(OscEvent::CommandStarted),
                "D" => Some(OscEvent::CommandFinished(
                    parts.next().and_then(|status| status.trim().parse().ok()),
                )),
                _ => None,
            }
        }
        // OSC 9;4 is a progress report, not a notification.
        "9" if !rest.starts_with("4;") && rest != "4" && !rest.is_empty() => {
            Some(OscEvent::Notify {
                title: None,
                body: clean_text(rest),
            })
        }
        "777" => {
            let mut parts = rest.splitn(3, ';');
            if parts.next()? != "notify" {
                return None;
            }
            let title = clean_text(parts.next().unwrap_or_default());
            let body = clean_text(parts.next().unwrap_or_default());
            Some(OscEvent::Notify {
                title: (!title.is_empty()).then_some(title),
                body,
            })
        }
        _ => None,
    }
}

/// Notification text from a program: control characters removed, length
/// capped.
fn clean_text(text: &str) -> String {
    let visible: Vec<char> = text.chars().filter(|ch| !ch.is_control()).collect();
    let mut cleaned: String = visible.iter().take(MAX_NOTIFICATION_CHARS).collect();
    if visible.len() > MAX_NOTIFICATION_CHARS {
        cleaned.push('…');
    }
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    impl OscScanner {
        fn scan_events(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
            events(self.scan(bytes))
        }
    }

    fn events(pieces: Vec<Piece>) -> Vec<OscEvent> {
        pieces
            .into_iter()
            .filter_map(|piece| match piece {
                Piece::Event(event) => Some(event),
                Piece::Output(_) => None,
            })
            .collect()
    }

    fn output(pieces: &[Piece]) -> Vec<u8> {
        pieces
            .iter()
            .flat_map(|piece| match piece {
                Piece::Output(bytes) => bytes.clone(),
                Piece::Event(_) => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn hooks_are_removed_from_output_and_ordered_with_it() {
        let mut scanner = OscScanner::new("s1".into());
        let mut stream = b"prompt$ ".to_vec();
        stream.extend(hooks::encode("Preexec", "s1", r#"{"command":"ls"}"#));
        stream.extend(b"file.txt\r\n");
        stream.extend(hooks::encode("CommandFinished", "s1", r#"{"exit_code":0}"#));
        let pieces = scanner.scan(&stream);
        assert_eq!(
            pieces,
            vec![
                Piece::Output(b"prompt$ ".to_vec()),
                Piece::Event(OscEvent::Hook(Hook::Preexec {
                    command: "ls".into()
                })),
                Piece::Output(b"file.txt\r\n".to_vec()),
                Piece::Event(OscEvent::Hook(Hook::CommandFinished { exit_code: 0 })),
            ]
        );
    }

    #[test]
    fn forged_hooks_are_dropped_and_split_hooks_joined() {
        let mut scanner = OscScanner::new("s1".into());
        let forged = hooks::encode("CommandFinished", "evil", r#"{"exit_code":0}"#);
        let pieces = scanner.scan(&forged);
        assert!(events(pieces.clone()).is_empty());
        assert!(output(&pieces).is_empty());

        let real = hooks::encode("CommandFinished", "s1", r#"{"exit_code":2}"#);
        let (head, tail) = real.split_at(9);
        assert!(scanner.scan(head).is_empty());
        assert_eq!(
            scanner.scan_events(tail),
            vec![OscEvent::Hook(Hook::CommandFinished { exit_code: 2 })]
        );
    }

    #[test]
    fn lookalike_hooks_with_other_content_are_ordinary_output() {
        let mut scanner = OscScanner::new("s1".into());
        let text = b"\x1bP$dvisible text\x1b\\after";
        let pieces = scanner.scan(text);
        assert_eq!(output(&pieces), text.to_vec());
        assert!(events(pieces).is_empty());
    }

    #[test]
    fn long_hooks_split_into_many_reads_are_checked_once() {
        let mut scanner = OscScanner::new("s1".into());
        let padding = "0".repeat(400_000);
        let mut hook = hooks::encode("CommandFinished", "s1", r#"{"exit_code":3}"#);
        // Pad inside the hex payload with whitespace-free hex the decoder
        // rejects, to exercise scanning cost without a valid hook.
        hook.splice(4..4, padding.bytes());
        let started = std::time::Instant::now();
        for chunk in hook.chunks(1024) {
            let _ = scanner.scan(chunk);
        }
        // Linear scanning finishes in well under a second; rescanning the
        // whole pending buffer on every read would not.
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(scanner.pending.is_empty());
    }

    #[test]
    fn osc_sequences_stay_in_the_output() {
        let mut scanner = OscScanner::new("s1".into());
        let pieces = scanner.scan(b"a\x1b]133;A\x07b");
        assert_eq!(output(&pieces), b"a\x1b]133;A\x07b".to_vec());
        assert_eq!(events(pieces), vec![OscEvent::PromptStarted]);
    }

    #[test]
    fn parses_command_marks_with_both_terminators() {
        let mut scanner = OscScanner::new("s1".into());
        let events = scanner.scan_events(b"out\x1b]133;A\x07$ \x1b]133;C\x1b\\ls\x1b]133;D;2\x07");
        assert_eq!(
            events,
            vec![
                OscEvent::PromptStarted,
                OscEvent::CommandStarted,
                OscEvent::CommandFinished(Some(2))
            ]
        );
    }

    #[test]
    fn sequences_split_across_reads_are_joined() {
        let mut scanner = OscScanner::new("s1".into());
        assert!(scanner.scan_events(b"text\x1b").is_empty());
        assert!(scanner.scan_events(b"]133;D;").is_empty());
        assert_eq!(
            scanner.scan_events(b"0\x07more"),
            vec![OscEvent::CommandFinished(Some(0))]
        );
    }

    #[test]
    fn every_sequence_boundary_preserves_events() {
        let output = b"out\x1b]133;A\x07$ \x1b]133;C\x1b\\ls\x1b]133;D;0\x07";
        let expected = OscScanner::new("s1".into()).scan_events(output);
        for split in 0..=output.len() {
            let mut scanner = OscScanner::new("s1".into());
            let mut events = scanner.scan_events(&output[..split]);
            events.extend(scanner.scan_events(&output[split..]));
            assert_eq!(events, expected, "split at {split}");
            assert!(scanner.pending.is_empty());
        }
        let mut scanner = OscScanner::new("s1".into());
        let mut events = Vec::new();
        for byte in output {
            events.extend(scanner.scan_events(std::slice::from_ref(byte)));
        }
        assert_eq!(events, expected);
    }

    #[test]
    fn ordinary_output_keeps_no_pending_bytes() {
        let mut scanner = OscScanner::new("s1".into());
        let output = b"ordinary terminal output\r\n".repeat(4096);
        for _ in 0..16 {
            assert!(scanner.scan_events(&output).is_empty());
            assert_eq!(scanner.pending.capacity(), 0);
        }
        assert!(
            scanner
                .scan_events(b"\x1b[31mcolored\x1b[0m\r\n")
                .is_empty()
        );
        assert!(scanner.pending.is_empty());
    }

    #[test]
    fn broken_sequences_preserve_following_introducers() {
        let mut scanner = OscScanner::new("s1".into());
        assert_eq!(
            scanner.scan_events(b"\x1b]9;first\x1b]133;C\x07"),
            vec![
                OscEvent::Notify {
                    title: None,
                    body: "first".into(),
                },
                OscEvent::CommandStarted,
            ]
        );
    }

    #[test]
    fn parses_notifications() {
        let mut scanner = OscScanner::new("s1".into());
        let events =
            scanner.scan_events(b"\x1b]9;Build done\x07\x1b]777;notify;Claude;Needs input\x1b\\");
        assert_eq!(
            events,
            vec![
                OscEvent::Notify {
                    title: None,
                    body: "Build done".into()
                },
                OscEvent::Notify {
                    title: Some("Claude".into()),
                    body: "Needs input".into()
                },
            ]
        );
    }

    #[test]
    fn notification_text_is_cleaned_and_capped() {
        let mut scanner = OscScanner::new("s1".into());
        let mut sequence = b"\x1b]9;line\rbreak ".to_vec();
        sequence.extend(std::iter::repeat_n(b'x', 400));
        sequence.push(0x07);
        let events = scanner.scan_events(&sequence);
        let OscEvent::Notify { body, .. } = &events[0] else {
            panic!("expected a notification");
        };
        assert!(body.starts_with("linebreak "));
        assert!(body.chars().count() <= MAX_NOTIFICATION_CHARS + 1);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn progress_reports_and_other_sequences_are_ignored() {
        let mut scanner = OscScanner::new("s1".into());
        assert!(
            scanner
                .scan_events(b"\x1b]9;4;1;50\x07\x1b]0;title\x07\x1b]133;B\x07")
                .is_empty()
        );
    }

    #[test]
    fn oversized_sequences_are_not_kept() {
        let mut scanner = OscScanner::new("s1".into());
        let mut long = b"\x1b]9;".to_vec();
        long.extend(std::iter::repeat_n(b'x', MAX_PENDING_OSC + 10));
        assert!(scanner.scan_events(&long).is_empty());
        assert_eq!(
            scanner.scan_events(b"\x1b]133;C\x07"),
            vec![OscEvent::CommandStarted]
        );
    }

    #[test]
    fn pending_sequences_stay_bounded_across_reads() {
        let mut scanner = OscScanner::new("s1".into());
        assert!(scanner.scan_events(b"\x1b]9;").is_empty());
        let body = vec![b'x'; MAX_PENDING_OSC - 4];
        assert!(scanner.scan_events(&body).is_empty());
        assert_eq!(scanner.pending.len(), MAX_PENDING_OSC);
        assert!(scanner.scan_events(b"x").is_empty());
        assert!(scanner.pending.is_empty());
        assert_eq!(
            scanner.scan_events(b"\x1b]133;A\x07"),
            vec![OscEvent::PromptStarted]
        );
    }
}
