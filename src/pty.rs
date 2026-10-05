use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    thread,
};

use anyhow::{Context as _, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Maximum bytes read from a PTY before passing output to its consumer.
pub const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
/// Maximum queued output chunks per PTY, totaling at most 512 KiB.
pub const OUTPUT_QUEUE_CAPACITY: usize = 64;

/// Grid and pixel dimensions reported to the child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtyDimensions {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl PtyDimensions {
    fn to_pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: self.cols.saturating_mul(self.cell_width),
            pixel_height: self.rows.saturating_mul(self.cell_height),
        }
    }
}

/// A shell process running inside a pseudo-terminal.
///
/// Reading and writing happen on dedicated threads so the UI thread never
/// blocks on the PTY. Output arrives on the channel returned by [`Pty::spawn`];
/// the channel closes when the shell exits.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    input: mpsc::Sender<Vec<u8>>,
    shell_pid: Option<i32>,
}

impl Pty {
    /// Run `command` (normally the user's shell) in `cwd`, or the home
    /// directory when it is absent or no longer exists.
    pub fn spawn(
        mut command: CommandBuilder,
        dimensions: PtyDimensions,
        cwd: Option<&Path>,
    ) -> Result<(Self, async_channel::Receiver<Vec<u8>>)> {
        let pair = native_pty_system()
            .openpty(dimensions.to_pty_size())
            .context("failed to open pty")?;

        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", env!("CARGO_PKG_NAME"));
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        match cwd.filter(|cwd| cwd.is_dir()) {
            Some(cwd) => command.cwd(cwd),
            None => {
                if let Some(home) = std::env::var_os("HOME") {
                    command.cwd(home);
                }
            }
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .context("failed to spawn shell")?;
        // The slave side belongs to the child now; holding it open would keep
        // the reader from seeing EOF when the shell exits.
        drop(pair.slave);

        let killer = child.clone_killer();
        let shell_pid = child.process_id().map(|pid| pid as i32);
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let mut writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;

        let (output_tx, output_rx) = async_channel::bounded(OUTPUT_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                read_output(reader, &output_tx);
                // Reap the shell so it does not linger as a zombie.
                let _ = child.wait();
            })
            .context("failed to start pty reader thread")?;

        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
                for bytes in input_rx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("failed to start pty writer thread")?;

        Ok((
            Self {
                master: pair.master,
                killer,
                input: input_tx,
                shell_pid,
            },
            output_rx,
        ))
    }

    /// A cloneable handle for sending bytes to the shell from callbacks.
    pub fn input_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.input.clone()
    }

    pub fn shell_pid(&self) -> Option<i32> {
        self.shell_pid
    }

    /// The process currently in the foreground of the PTY: the shell itself
    /// at a prompt, or the program it is running.
    pub fn foreground_pid(&self) -> Option<i32> {
        self.master.process_group_leader()
    }

    pub fn write(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.input.send(bytes.to_vec());
        }
    }

    pub fn resize(&self, dimensions: PtyDimensions) {
        if let Err(error) = self.master.resize(dimensions.to_pty_size()) {
            log::warn!("failed to resize pty: {error}");
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

fn read_output(mut reader: impl Read, output: &async_channel::Sender<Vec<u8>>) {
    let mut buffer = [0u8; OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(len) => {
                // Blocking here applies backpressure without occupying the UI thread.
                if output.send_blocking(buffer[..len].to_vec()).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        time::{Duration, Instant},
    };

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for PTY output"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn output_queue_applies_backpressure_without_losing_bytes() {
        let expected: Vec<u8> = (0..OUTPUT_CHUNK_BYTES * (OUTPUT_QUEUE_CAPACITY + 3))
            .map(|index| (index % 251) as u8)
            .collect();
        let bytes = expected.clone();
        let (output_tx, output_rx) = async_channel::bounded(OUTPUT_QUEUE_CAPACITY);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            read_output(Cursor::new(bytes), &output_tx);
            finished_tx.send(()).unwrap();
        });

        wait_until(|| output_rx.is_full());
        assert_eq!(output_rx.len(), OUTPUT_QUEUE_CAPACITY);
        assert_eq!(finished_rx.try_recv(), Err(mpsc::TryRecvError::Empty));

        let deadline = Instant::now() + TEST_TIMEOUT;
        let mut actual = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "output reader did not finish");
            match output_rx.try_recv() {
                Ok(bytes) => {
                    assert!(bytes.len() <= OUTPUT_CHUNK_BYTES);
                    actual.extend_from_slice(&bytes);
                }
                Err(async_channel::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(async_channel::TryRecvError::Closed) => break,
            }
        }
        finished_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        reader.join().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn dropping_output_unblocks_a_reader_waiting_for_capacity() {
        let bytes = vec![0u8; OUTPUT_CHUNK_BYTES * (OUTPUT_QUEUE_CAPACITY + 2)];
        let (output_tx, output_rx) = async_channel::bounded(OUTPUT_QUEUE_CAPACITY);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            read_output(Cursor::new(bytes), &output_tx);
            finished_tx.send(()).unwrap();
        });

        wait_until(|| output_rx.is_full());
        assert_eq!(finished_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        drop(output_rx);
        finished_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pty_output_preserves_order_across_a_full_queue() {
        let records = 2048;
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            &format!(
                "i=0; while [ \"$i\" -lt {records} ]; do printf '%08d%0512d' \"$i\" 0; i=$((i+1)); done"
            ),
        ]);
        let dimensions = PtyDimensions {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 18,
        };
        let (_pty, output) = Pty::spawn(command, dimensions, None).unwrap();
        assert_eq!(output.capacity(), Some(OUTPUT_QUEUE_CAPACITY));
        wait_until(|| output.is_full());

        let deadline = Instant::now() + TEST_TIMEOUT;
        let mut actual = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "PTY output did not finish");
            match output.try_recv() {
                Ok(bytes) => {
                    assert!(bytes.len() <= OUTPUT_CHUNK_BYTES);
                    actual.extend_from_slice(&bytes);
                }
                Err(async_channel::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(async_channel::TryRecvError::Closed) => break,
            }
        }
        let expected: String = (0..records)
            .map(|index| format!("{index:08}{:0512}", 0))
            .collect();
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected.as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn closing_a_pty_with_a_full_queue_reaps_its_child() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "i=0; while [ \"$i\" -lt 2048 ]; do printf '%0512d' 0; i=$((i+1)); done",
        ]);
        let dimensions = PtyDimensions {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 18,
        };
        let (pty, output) = Pty::spawn(command, dimensions, None).unwrap();
        let pid = pty.shell_pid().unwrap();
        wait_until(|| output.is_full());
        drop(output);
        drop(pty);
        wait_until(|| {
            // SAFETY: signal zero only checks whether this process still exists.
            let result = unsafe { libc::kill(pid, 0) };
            result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
    }
}
