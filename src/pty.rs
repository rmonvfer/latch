use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    thread,
};

use anyhow::{Context as _, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

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
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let mut writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;

        let (output_tx, output_rx) = async_channel::unbounded();
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(len) => {
                            if output_tx.send_blocking(buffer[..len].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
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
