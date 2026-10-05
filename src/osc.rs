//! Spotting the escape sequences the app reacts to in PTY output, before
//! libghostty consumes them: command boundaries from shell integration
//! (OSC 133) and desktop notification requests (OSC 9 and OSC 777).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 133;C — a command started running.
    CommandStarted,
    /// OSC 133;D — the command finished, with its exit status if reported.
    CommandFinished(Option<i32>),
    /// OSC 9;<body> or OSC 777;notify;<title>;<body>.
    Notify { title: Option<String>, body: String },
}

const INTRODUCER: &[u8] = b"\x1b]";
/// Longest sequence worth holding across reads; longer ones are dropped.
const MAX_PENDING: usize = 4096;

/// Finds OSC events in PTY output, including sequences split across reads.
#[derive(Default)]
pub struct OscScanner {
    pending: Vec<u8>,
}

impl OscScanner {
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut index = 0;

        while let Some(offset) = find(&buffer[index..], INTRODUCER) {
            let start = index + offset;
            let body_start = start + INTRODUCER.len();
            match terminator(&buffer[body_start..]) {
                Some((body_len, terminator_len)) => {
                    if let Some(event) = parse(&buffer[body_start..body_start + body_len]) {
                        events.push(event);
                    }
                    index = body_start + body_len + terminator_len;
                }
                None => {
                    if buffer.len() - start <= MAX_PENDING {
                        self.pending = buffer[start..].to_vec();
                    }
                    return events;
                }
            }
        }
        // A trailing ESC may begin an introducer split across reads.
        if buffer.len() > index && buffer.last() == Some(&0x1b) {
            self.pending = vec![0x1b];
        }
        events
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
                body: rest.to_string(),
            })
        }
        "777" => {
            let mut parts = rest.splitn(3, ';');
            if parts.next()? != "notify" {
                return None;
            }
            let title = parts.next().unwrap_or_default().to_string();
            let body = parts.next().unwrap_or_default().to_string();
            Some(OscEvent::Notify {
                title: (!title.is_empty()).then_some(title),
                body,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_marks_with_both_terminators() {
        let mut scanner = OscScanner::default();
        let events = scanner.scan(b"out\x1b]133;A\x07$ \x1b]133;C\x1b\\ls\x1b]133;D;2\x07");
        assert_eq!(
            events,
            vec![OscEvent::CommandStarted, OscEvent::CommandFinished(Some(2))]
        );
    }

    #[test]
    fn sequences_split_across_reads_are_joined() {
        let mut scanner = OscScanner::default();
        assert!(scanner.scan(b"text\x1b").is_empty());
        assert!(scanner.scan(b"]133;D;").is_empty());
        assert_eq!(
            scanner.scan(b"0\x07more"),
            vec![OscEvent::CommandFinished(Some(0))]
        );
    }

    #[test]
    fn parses_notifications() {
        let mut scanner = OscScanner::default();
        let events = scanner.scan(b"\x1b]9;Build done\x07\x1b]777;notify;Claude;Needs input\x1b\\");
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
    fn progress_reports_and_other_sequences_are_ignored() {
        let mut scanner = OscScanner::default();
        assert!(
            scanner
                .scan(b"\x1b]9;4;1;50\x07\x1b]0;title\x07\x1b]133;A\x07")
                .is_empty()
        );
    }

    #[test]
    fn oversized_sequences_are_not_kept() {
        let mut scanner = OscScanner::default();
        let mut long = b"\x1b]9;".to_vec();
        long.extend(std::iter::repeat_n(b'x', MAX_PENDING + 10));
        assert!(scanner.scan(&long).is_empty());
        assert_eq!(
            scanner.scan(b"\x1b]133;C\x07"),
            vec![OscEvent::CommandStarted]
        );
    }
}
