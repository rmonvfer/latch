//! Starting, replacing and running the detached runtime process.

use std::{
    fs,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};

use latch_platform::{
    peer::{RELEASE_REQUIREMENT, is_signed_release, peer_audit, same_user},
    process::pid_version,
};

use crate::{
    directory::{FileLock, Paths, private_directory},
    server::Server,
    transport::{Rpc, peer_is_this_program},
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
/// and start the current one. Only an authenticated runtime is stopped.
pub fn replace_outdated() -> Result<()> {
    let paths = Paths::configured();
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        ensure!(
            same_user(&stream),
            "session runtime belongs to another user"
        );
        let (pid, version) = peer_audit(&stream).context("cannot identify the session runtime")?;
        ensure!(
            peer_is_this_program(&stream) && pid_version(pid) == Some(version),
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

    let mut command = Command::new(runtime_executable(&paths)?);
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

/// Keep signed runtime code outside the application bundle replaced by the updater.
fn runtime_executable(paths: &Paths) -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    if !is_signed_release() {
        return Ok(executable);
    }
    let bundle = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("signed runtime executable is outside an application bundle")?;
    ensure!(
        bundle
            .extension()
            .is_some_and(|extension| extension == "app"),
        "signed runtime executable is outside an application bundle"
    );
    stage_runtime_bundle(bundle, &paths.directory.join("executables"))
}

fn verified_bundle_hash(bundle: &Path) -> Result<String> {
    ensure!(
        fs::symlink_metadata(bundle)?.is_dir(),
        "runtime bundle must be a real directory"
    );
    let verification = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R"])
        .arg(format!("={RELEASE_REQUIREMENT}"))
        .arg(bundle)
        .output()
        .context("failed to verify runtime signature")?;
    ensure!(
        verification.status.success(),
        "runtime bundle signature is invalid: {}",
        String::from_utf8_lossy(&verification.stderr)
    );
    let information = Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(bundle)
        .output()?;
    ensure!(
        information.status.success(),
        "cannot inspect runtime signature"
    );
    let output = String::from_utf8_lossy(&information.stderr);
    let hash = output
        .lines()
        .find_map(|line| line.strip_prefix("CDHash="))
        .context("runtime signature does not contain a code directory hash")?;
    ensure!(
        hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "runtime code directory hash is invalid"
    );
    Ok(hash.to_owned())
}

fn stage_runtime_bundle(bundle: &Path, directory: &Path) -> Result<PathBuf> {
    let hash = verified_bundle_hash(bundle)?;
    private_directory(directory)?;
    let version = directory.join(&hash);
    let cached = version.join("Latch.app");
    let exists = match fs::symlink_metadata(&version) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("cannot inspect runtime version directory"),
    };
    if exists {
        private_directory(&version)?;
        ensure!(
            verified_bundle_hash(&cached)? == hash,
            "cached runtime signature does not match its version"
        );
    } else {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = directory.join(format!(".stage-{}-{nonce}", std::process::id()));
        private_directory(&temporary)?;
        let staged = temporary.join("Latch.app");
        let result = (|| -> Result<()> {
            let copied = Command::new("/usr/bin/ditto")
                .arg("--noqtn")
                .arg(bundle)
                .arg(&staged)
                .status()
                .context("failed to copy signed runtime bundle")?;
            ensure!(copied.success(), "failed to copy signed runtime bundle");
            ensure!(
                verified_bundle_hash(&staged)? == hash,
                "staged runtime signature does not match its source"
            );
            fs::rename(&temporary, &version).context("failed to publish signed runtime bundle")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result?;
    }
    let executable = cached.join("Contents/MacOS/terminal");
    ensure!(
        executable
            .canonicalize()?
            .starts_with(cached.canonicalize()?),
        "cached runtime executable escapes its bundle"
    );
    Ok(executable)
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

#[cfg(test)]
mod update_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };

    #[test]
    #[ignore = "subprocess fixture for staged_runtime_survives_bundle_removal"]
    fn signed_runtime_child() {
        let socket = std::env::var_os("LATCH_PEER_TEST_SOCKET").unwrap();
        if std::env::var_os("LATCH_PEER_TEST_SERVER").is_some() {
            let listener = UnixListener::bind(socket).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(STARTUP_TIMEOUT)).unwrap();
            assert!(peer_is_this_program(&stream));
            let mut authenticated = [0];
            stream.read_exact(&mut authenticated).unwrap();
            assert_eq!(authenticated, [1]);
            stream.write_all(&[1]).unwrap();
        } else {
            let started = Instant::now();
            let mut stream = loop {
                if let Ok(stream) = UnixStream::connect(&socket) {
                    break stream;
                }
                assert!(started.elapsed() < STARTUP_TIMEOUT);
                thread::sleep(Duration::from_millis(10));
            };
            stream.set_read_timeout(Some(STARTUP_TIMEOUT)).unwrap();
            assert!(peer_is_this_program(&stream));
            stream.write_all(&[1]).unwrap();
            let mut authenticated = [0];
            stream.read_exact(&mut authenticated).unwrap();
            assert_eq!(authenticated, [1]);
        }
    }

    #[test]
    #[ignore = "requires a Developer ID identity in LATCH_TEST_SIGNING_IDENTITY"]
    fn staged_runtime_survives_bundle_removal() {
        let identity = std::env::var("LATCH_TEST_SIGNING_IDENTITY").unwrap();
        let directory =
            std::env::temp_dir().join(format!("latch-runtime-update-{}", std::process::id()));
        private_directory(&directory).unwrap();
        let source = directory.join("Source.app");
        let executable = source.join("Contents/MacOS/terminal");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        fs::write(source.join("Contents/Info.plist"), r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>me.egrati.terminal</string><key>CFBundleExecutable</key><string>terminal</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>1</string></dict></plist>"#).unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", &identity])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let installed = directory.join("Installed.app");
        assert!(
            Command::new("/usr/bin/ditto")
                .arg(&source)
                .arg(&installed)
                .status()
                .unwrap()
                .success()
        );
        let plist = installed.join("Contents/Info.plist");
        let contents = fs::read_to_string(&plist)
            .unwrap()
            .replace("<string>1</string>", "<string>2</string>");
        fs::write(&plist, contents).unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", &identity])
                .arg(&installed)
                .status()
                .unwrap()
                .success()
        );
        assert_ne!(
            verified_bundle_hash(&source).unwrap(),
            verified_bundle_hash(&installed).unwrap()
        );
        let staged = stage_runtime_bundle(&source, &directory.join("executables")).unwrap();
        assert_eq!(
            staged,
            stage_runtime_bundle(&source, &directory.join("executables")).unwrap()
        );
        let socket = directory.join("sessions.sock");
        let arguments = [
            "--exact",
            "lifecycle::update_tests::signed_runtime_child",
            "--ignored",
            "--nocapture",
        ];
        let mut server = Command::new(&staged)
            .args(arguments)
            .env("LATCH_PEER_TEST_SOCKET", &socket)
            .env("LATCH_PEER_TEST_SERVER", "1")
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !socket.exists() {
            assert!(server.try_wait().unwrap().is_none());
            assert!(started.elapsed() < STARTUP_TIMEOUT);
            thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(&source).unwrap();
        assert!(
            Command::new(installed.join("Contents/MacOS/terminal"))
                .args(arguments)
                .env("LATCH_PEER_TEST_SOCKET", &socket)
                .status()
                .unwrap()
                .success()
        );
        assert!(server.wait().unwrap().success());
        let contents = staged.parent().unwrap().parent().unwrap();
        fs::write(contents.join("Info.plist"), "tampered").unwrap();
        let cached = staged.parent().unwrap().parent().unwrap().parent().unwrap();
        assert!(verified_bundle_hash(cached).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
