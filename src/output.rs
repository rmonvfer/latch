//! Cooperative delivery of terminal output to the foreground thread.

use std::time::{Duration, Instant};

use async_channel::Receiver;

/// Both limits apply between parser calls; one read is the smallest work unit.
const SLICE_BYTES: usize = 64 * 1024;
const SLICE_TIME: Duration = Duration::from_millis(2);
pub const YIELD_INTERVAL: Duration = Duration::from_millis(1);
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// A bounded amount of queued output, consumed without changing byte order.
pub struct OutputBatch<'a> {
    first: Option<Vec<u8>>,
    receiver: &'a Receiver<Vec<u8>>,
    started: Instant,
    bytes: usize,
}

impl Iterator for OutputBatch<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes >= SLICE_BYTES || (self.bytes > 0 && self.started.elapsed() >= SLICE_TIME) {
            return None;
        }
        let chunk = self
            .first
            .take()
            .or_else(|| self.receiver.try_recv().ok())?;
        self.bytes += chunk.len();
        Some(chunk)
    }
}

/// Every batch yields even when the next receive could complete immediately.
/// `process` returns false when the session has been removed.
pub async fn consume<F, P, W>(receiver: Receiver<Vec<u8>>, mut process: F, mut pause: P)
where
    F: FnMut(&mut OutputBatch<'_>) -> bool,
    P: FnMut() -> W,
    W: Future<Output = ()>,
{
    while let Ok(first) = receiver.recv().await {
        let mut batch = OutputBatch {
            first: Some(first),
            receiver: &receiver,
            started: Instant::now(),
            bytes: 0,
        };
        if !process(&mut batch) {
            return;
        }
        pause().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        pin::Pin,
        task::{Context, Poll, Waker},
        thread,
    };

    use libghostty_vt::{Terminal, terminal::Options};

    use crate::{
        osc::{OscScanner, Piece},
        pty::OUTPUT_CHUNK_BYTES,
        search,
    };

    #[derive(Default)]
    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn continuous_output_yields_and_preserves_order() {
        let (sender, receiver) = async_channel::bounded(64);
        let total = SLICE_BYTES * 4;
        let source: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
        for chunk in source.chunks(OUTPUT_CHUNK_BYTES) {
            sender.try_send(chunk.to_vec()).unwrap();
        }
        drop(sender);
        let mut received = Vec::new();
        let progress = Cell::new(0);
        let mut task = Box::pin(consume(
            receiver,
            |batch| {
                let before = received.len();
                for chunk in batch {
                    received.extend_from_slice(&chunk);
                }
                assert!(received.len() - before <= SLICE_BYTES);
                progress.set(received.len());
                true
            },
            YieldOnce::default,
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(task.as_mut().poll(&mut cx).is_pending());
        assert!(progress.get() > 0 && progress.get() < total);
        let mut heartbeats = 1;
        while task.as_mut().poll(&mut cx).is_pending() {
            heartbeats += 1;
        }
        drop(task);
        assert!(heartbeats >= total / SLICE_BYTES);
        assert_eq!(received, source);
    }

    #[test]
    fn slow_parser_returns_after_one_chunk() {
        let (sender, receiver) = async_channel::bounded(2);
        sender.try_send(vec![2]).unwrap();
        let mut batch = OutputBatch {
            first: Some(vec![1]),
            receiver: &receiver,
            started: Instant::now(),
            bytes: 0,
        };
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
        let busy_bytes = Cell::new(0);
        let quiet_bytes = Cell::new(0);
        let mut busy = Box::pin(consume(
            busy_rx,
            |batch| {
                for bytes in batch {
                    busy_bytes.set(busy_bytes.get() + bytes.len());
                }
                true
            },
            YieldOnce::default,
        ));
        let mut quiet = Box::pin(consume(
            quiet_rx,
            |batch| {
                for bytes in batch {
                    quiet_bytes.set(quiet_bytes.get() + bytes.len());
                }
                true
            },
            YieldOnce::default,
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(busy.as_mut().poll(&mut cx).is_pending());
        assert!(quiet.as_mut().poll(&mut cx).is_pending());
        assert_eq!(quiet_bytes.get(), 6);
        assert!(busy_bytes.get() <= SLICE_BYTES);
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
        let mut task = Box::pin(consume(
            receiver,
            |batch| {
                for chunk in batch {
                    for piece in scanner.scan(&chunk) {
                        match piece {
                            Piece::Output(bytes) => actual.vt_write(&bytes),
                            Piece::Event(event) => events.push(event),
                        }
                    }
                }
                true
            },
            YieldOnce::default,
        ));
        let mut cx = Context::from_waker(Waker::noop());
        while task.as_mut().poll(&mut cx).is_pending() {}
        drop(task);
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
}
