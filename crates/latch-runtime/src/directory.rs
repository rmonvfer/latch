//! The runtime's private directory: its socket, secret and startup lock,
//! and the owner-only files kept there.

use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};

use latch_core::paths;

#[derive(Clone)]
pub(crate) struct Paths {
    pub(crate) directory: PathBuf,
}

impl Paths {
    pub(crate) fn configured() -> Self {
        Self {
            directory: paths::config_dir().join("runtime"),
        }
    }

    pub(crate) fn socket(&self) -> PathBuf {
        self.directory.join("sessions.sock")
    }

    /// The name of the keychain item holding the runtime's secret. The
    /// name is not secret; the item is.
    pub(crate) fn token_id(&self) -> PathBuf {
        self.directory.join("token-id")
    }

    pub(crate) fn prepare(&self) -> Result<()> {
        if let Some(parent) = self.directory.parent() {
            fs::create_dir_all(parent).context("failed to create runtime parent directory")?;
        }
        match fs::DirBuilder::new().mode(0o700).create(&self.directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("failed to create runtime directory"),
        }
        let metadata = fs::symlink_metadata(&self.directory)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "runtime directory must be a real directory"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "runtime directory belongs to another user"
        );
        ensure!(
            metadata.mode() & 0o777 == 0o700,
            "runtime directory must have owner-only permissions (0700)"
        );
        Ok(())
    }
}

pub(crate) struct FileLock(File);

impl FileLock {
    pub(crate) fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let file = private_file(path, true)?;
        let started = Instant::now();
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error).context("failed to lock session runtime");
            }
            ensure!(
                started.elapsed() < timeout,
                "session runtime is already starting or running"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) fn private_file(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "runtime private file must be a regular file without hard links"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o600,
        "runtime private file must belong to this user and have permissions 0600"
    );
    Ok(file)
}
