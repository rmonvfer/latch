//! Bounded output parsing so a busy session still services its control queue.

use std::time::{Duration, Instant};

use async_channel::Receiver;

/// Both limits apply between parser calls; one read is the smallest work unit.
const SLICE_BYTES: usize = 256 * 1024;
const SLICE_TIME: Duration = Duration::from_millis(2);

/// A bounded amount of queued output, consumed without changing byte order.
pub struct OutputBatch<'a> {
    receiver: &'a Receiver<Vec<u8>>,
    started: Instant,
    bytes: usize,
}

impl<'a> OutputBatch<'a> {
    pub fn new(receiver: &'a Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            started: Instant::now(),
            bytes: 0,
        }
    }
}

impl Iterator for OutputBatch<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes >= SLICE_BYTES || (self.bytes > 0 && self.started.elapsed() >= SLICE_TIME) {
            return None;
        }
        let chunk = self.receiver.try_recv().ok()?;
        self.bytes += chunk.len();
        Some(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    use libghostty_vt::{Terminal, terminal::Options};

    use crate::{
        osc::{OscScanner, Piece},
        pty::{OUTPUT_CHUNK_BYTES, OUTPUT_QUEUE_CAPACITY},
        search,
    };

    #[test]
    fn continuous_output_is_bounded_and_preserves_order() {
        let total = SLICE_BYTES * 4;
        let (sender, receiver) = async_channel::bounded(total / OUTPUT_CHUNK_BYTES);
        let source: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
        for chunk in source.chunks(OUTPUT_CHUNK_BYTES) {
            sender.try_send(chunk.to_vec()).unwrap();
        }
        drop(sender);
        let mut received = Vec::new();
        let mut batches = 0;
        while !receiver.is_empty() {
            let before = received.len();
            for chunk in OutputBatch::new(&receiver) {
                received.extend_from_slice(&chunk);
            }
            assert!(received.len() - before <= SLICE_BYTES);
            batches += 1;
        }
        assert!(batches >= total / SLICE_BYTES);
        assert_eq!(received, source);
    }

    #[test]
    fn slow_parser_returns_after_one_chunk() {
        let (sender, receiver) = async_channel::bounded(2);
        sender.try_send(vec![1]).unwrap();
        sender.try_send(vec![2]).unwrap();
        let mut batch = OutputBatch::new(&receiver);
        assert_eq!(batch.next(), Some(vec![1]));
        thread::sleep(SLICE_TIME * 2);
        assert_eq!(batch.next(), None);
        assert_eq!(receiver.try_recv().unwrap(), vec![2]);
    }

    #[test]
    fn busy_session_allows_another_session_to_make_progress() {
        let (busy_tx, busy_rx) = async_channel::bounded(64);
        for _ in 0..64 {
            busy_tx.try_send(vec![b'x'; OUTPUT_CHUNK_BYTES]).unwrap();
        }
        let (quiet_tx, quiet_rx) = async_channel::bounded(1);
        quiet_tx.try_send(b"prompt".to_vec()).unwrap();
        let busy_bytes: usize = OutputBatch::new(&busy_rx).map(|chunk| chunk.len()).sum();
        let quiet_bytes: usize = OutputBatch::new(&quiet_rx).map(|chunk| chunk.len()).sum();
        assert_eq!(quiet_bytes, 6);
        assert!(busy_bytes <= SLICE_BYTES);
    }

    #[test]
    fn split_unicode_and_escape_sequences_match_contiguous_parsing() {
        let bytes = "\x1b[31m├── café/中文\x1b[0m\r\n\x1b]133;D;0\x07done".as_bytes();
        let create = || {
            Terminal::new(Options {
                cols: 80,
                rows: 24,
                max_scrollback: 100,
            })
            .unwrap()
        };
        let mut expected = create();
        expected.vt_write(bytes);
        let mut actual = create();
        let mut scanner = OscScanner::new("session".into());
        let mut events = Vec::new();
        let (sender, receiver) = async_channel::bounded(bytes.len());
        for byte in bytes {
            sender.try_send(vec![*byte]).unwrap();
        }
        drop(sender);
        while !receiver.is_empty() {
            for chunk in OutputBatch::new(&receiver) {
                for piece in scanner.scan(&chunk) {
                    match piece {
                        Piece::Output(bytes) => actual.vt_write(&bytes),
                        Piece::Event(event) => events.push(event),
                    }
                }
            }
        }
        assert_eq!(
            search::screen_text(&actual).unwrap(),
            search::screen_text(&expected).unwrap()
        );
        let contiguous: Vec<_> = OscScanner::new("session".into())
            .scan(bytes)
            .into_iter()
            .filter_map(|piece| match piece {
                Piece::Event(event) => Some(event),
                Piece::Output(_) => None,
            })
            .collect();
        assert_eq!(events, contiguous);
    }

    #[test]
    #[ignore = "prints sustained-output parsing and session control scheduling timings"]
    fn output_latency_benchmark() {
        let line = "│   ├── target/debug/deps/terminal-0123456789abcdef.d\r\n";
        let bytes = line.repeat(200_000).into_bytes();
        let create = || {
            Terminal::new(Options {
                cols: 160,
                rows: 48,
                max_scrollback: 10_000,
            })
            .unwrap()
        };
        let mut bulk_terminal = create();
        let mut scanner = OscScanner::new(String::new());
        let started = Instant::now();
        for chunk in bytes.chunks(64 * 1024) {
            scanner.scan(chunk);
            bulk_terminal.vt_write(chunk);
        }
        let bulk_elapsed = started.elapsed();
        let (sender, receiver) = async_channel::bounded(OUTPUT_QUEUE_CAPACITY);
        let byte_count = bytes.len();
        let producer = thread::spawn(move || {
            for chunk in bytes.chunks(OUTPUT_CHUNK_BYTES) {
                if sender.send_blocking(chunk.to_vec()).is_err() {
                    return;
                }
            }
        });
        let mut terminal = create();
        let mut scanner = OscScanner::new(String::new());
        let mut received = 0;
        let mut slices = Vec::new();
        let started = Instant::now();
        let mut heartbeats = 0;
        while !receiver.is_closed() || !receiver.is_empty() {
            let slice = Instant::now();
            for chunk in OutputBatch::new(&receiver) {
                received += chunk.len();
                scanner.scan(&chunk);
                terminal.vt_write(&chunk);
            }
            slices.push(slice.elapsed());
            heartbeats += 1;
            thread::yield_now();
        }
        let elapsed = started.elapsed();
        producer.join().unwrap();
        assert_eq!(received, byte_count);
        assert_eq!(
            search::screen_text(&terminal).unwrap(),
            search::screen_text(&bulk_terminal).unwrap()
        );
        slices.sort_unstable();
        eprintln!(
            "{:.2} MiB: bulk parsing {:?}; bounded parsing wall {:?}, {:.2} MiB/s; {} heartbeats; slice p99 {:?}, max {:?}",
            byte_count as f64 / 1_048_576.,
            bulk_elapsed,
            elapsed,
            byte_count as f64 / 1_048_576. / elapsed.as_secs_f64(),
            heartbeats,
            slices[slices.len() * 99 / 100],
            slices.last().unwrap(),
        );
    }
}
