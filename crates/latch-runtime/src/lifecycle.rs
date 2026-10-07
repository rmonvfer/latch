//! Starting, replacing and running the detached runtime process.

use std::{
    fs,
    os::unix::{net::UnixStream, process::CommandExt},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};

use latch_platform::{
    peer::{peer_audit, same_user},
    process::{self, pid_version},
};

use crate::{
    directory::{FileLock, Paths},
    server::Server,
    transport::Rpc,
};

pub(crate) const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const FILE_DESCRIPTOR_BUDGET: libc::rlim_t = 1024;

/// Whether the runtime that is running can be talked to: `false` for one
/// from an older build, which `replace_outdated` replaces.
pub fn is_outdated(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains(OUTDATED) || message.contains(INCOMPATIBLE)
    })
}

/// How a runtime speaking another protocol version is reported.
const INCOMPATIBLE: &str = "session runtime protocol version";

pub(crate) const OUTDATED: &str = "the running session runtime is from an older build";

/// Stop a session runtime from an older build, which ends its sessions,
/// and start the current one. Only a runtime running this same executable
/// is stopped.
pub fn replace_outdated() -> Result<()> {
    let paths = Paths::configured();
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        ensure!(
            same_user(&stream),
            "session runtime belongs to another user"
        );
        let (pid, version) = peer_audit(&stream).context("cannot identify the session runtime")?;
        let runs_this_program = process::executable_path(pid)
            .and_then(|path| fs::canonicalize(path).ok())
            .zip(std::env::current_exe().and_then(fs::canonicalize).ok())
            .is_some_and(|(theirs, ours)| theirs == ours)
            && pid_version(pid) == Some(version);
        ensure!(
            runs_this_program,
            "the session runtime is not this program; stop it yourself"
        );
        drop(stream);
        // The runtime waits for its sessions to end before stopping on a
        // terminate signal, so it is stopped outright.
        // SAFETY: kill has no memory preconditions.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let started = Instant::now();
        while unsafe { libc::kill(pid, 0) } == 0 {
            ensure!(
                started.elapsed() < STARTUP_TIMEOUT,
                "the old session runtime did not stop"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    ensure_running()
}

/// Start the detached runtime once, leaving an existing runtime and its sessions intact.
pub fn ensure_running() -> Result<()> {
    let paths = Paths::configured();
    paths.prepare()?;
    let _lock = FileLock::acquire(&paths.directory.join("startup.lock"), STARTUP_TIMEOUT)?;
    match UnixStream::connect(paths.socket()) {
        Ok(stream) => {
            Rpc::handshake(stream, &paths)?;
            return Ok(());
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error).context("cannot connect to session runtime"),
    }

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--session-runtime")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("failed to launch session runtime")?;
    let started = Instant::now();
    loop {
        match UnixStream::connect(paths.socket()) {
            Ok(stream) => {
                Rpc::handshake(stream, &paths)?;
                thread::Builder::new()
                    .name("runtime-reaper".into())
                    .spawn(move || {
                        let _ = child.wait();
                    })?;
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => return Err(error).context("cannot connect to launched session runtime"),
        }
        if let Some(status) = child.try_wait()? {
            bail!("session runtime exited before becoming ready ({status})");
        }
        ensure!(
            started.elapsed() < STARTUP_TIMEOUT,
            "session runtime did not become ready within 8 seconds"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run the headless owner of the PTYs until explicitly shut down with no running sessions.
pub fn serve() -> Result<()> {
    configure_file_limit()?;
    Server::bind(Paths::configured())?.run()
}

pub(crate) fn configure_file_limit() -> Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read runtime file limit");
    }
    // Each PTY owns a master, reader, and writer; connections and shell
    // startup also need descriptors beyond the persistent session handles.
    let target = FILE_DESCRIPTOR_BUDGET.min(limit.rlim_max);
    if limit.rlim_cur < target {
        limit.rlim_cur = target;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to raise runtime file limit");
        }
    }
    if limit.rlim_cur < FILE_DESCRIPTOR_BUDGET {
        log::warn!(
            "runtime file limit is {}; the hard limit may restrict session capacity",
            limit.rlim_cur
        );
    }
    Ok(())
}
