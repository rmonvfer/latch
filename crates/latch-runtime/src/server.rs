//! The runtime's server: the sessions it owns and the connections it serves.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, ensure};

use latch_core::agents::resume;
use latch_platform::{
    peer::{peer_audit, same_user},
    process::pid_version,
};

use crate::{
    directory::{FileLock, Paths},
    engine::{self, SessionHandle},
    protocol::{Envelope, Launch, Operation, Request, Response, VERSION},
    token::{forget_stale_secrets, forget_token, same_token, write_token},
    transport::{IO_TIMEOUT, peer_is_this_program, read_length, reserve, write_frame},
};

const MAX_SESSIONS: usize = 128;
const MAX_CONNECTIONS: usize = 160;
const MAX_INFLIGHT_BYTES: usize = 32 * 1024 * 1024;

struct State {
    registry: Mutex<Registry>,
    session_count: AtomicUsize,
    connections: AtomicUsize,
    inflight_bytes: AtomicUsize,
    stopping: AtomicBool,
    token: String,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<u64, SessionHandle>,
    pending: HashSet<u64>,
}

impl State {
    fn handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::ReportAgent { token, report } => {
                ensure!(!token.is_empty(), "agent report has no pane token");
                let handle = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("session registry unavailable"))?
                    .sessions
                    .values()
                    .find(|handle| same_token(&handle.info().control_token, &token))
                    .cloned()
                    .context("no session has that token")?;
                handle.request(Request::ReportAgent { token, report })
            }
            Request::Ping => Ok(Response::Ready { version: VERSION }),
            Request::Create(launch) => self.create(launch),
            Request::List => {
                let handles: Vec<_> = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("session registry unavailable"))?
                    .sessions
                    .values()
                    .cloned()
                    .collect();
                let mut infos: Vec<_> = handles.iter().map(SessionHandle::info).collect();
                infos.sort_by_key(|info| info.id);
                Ok(Response::Sessions(infos))
            }
            Request::Remove { session } => {
                let handle = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("session registry unavailable"))?
                    .sessions
                    .get(&session)
                    .cloned()
                    .context("session no longer exists")?;
                ensure!(handle.info().exited, "stop the session before removing it");
                handle.request(Request::Operate {
                    session,
                    operation: Operation::Stop,
                })?;
                if self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("session registry unavailable"))?
                    .sessions
                    .remove(&session)
                    .is_some()
                {
                    self.session_count.fetch_sub(1, Ordering::AcqRel);
                }
                Ok(Response::Done)
            }
            Request::Shutdown => {
                let registry = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("session registry unavailable"))?;
                ensure!(
                    registry.pending.is_empty() && registry.sessions.is_empty(),
                    "remove all sessions before runtime shutdown"
                );
                self.stopping.store(true, Ordering::Release);
                Ok(Response::Done)
            }
            request @ (Request::Poll { session, .. } | Request::Operate { session, .. }) => {
                let handle = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("session registry unavailable"))?
                    .sessions
                    .get(&session)
                    .cloned()
                    .context("session no longer exists")?;
                handle.request(request)
            }
        }
    }

    fn create(&self, launch: Launch) -> Result<Response> {
        let id = launch.id;
        ensure!(
            id > 0 && id < 1u64 << 53,
            "session ID must be a nonzero 53-bit integer"
        );
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("session registry unavailable"))?;
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "session runtime is shutting down"
        );
        ensure!(
            !registry.sessions.contains_key(&id) && !registry.pending.contains(&id),
            "session ID already exists"
        );
        reserve(&self.session_count, 1, MAX_SESSIONS)
            .context("session limit reached; remove finished sessions first")?;
        registry.pending.insert(id);
        drop(registry);
        let result = engine::spawn(id, launch);
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("session registry unavailable"))?;
        registry.pending.remove(&id);
        match result {
            Ok(handle) => {
                let info = handle.info();
                registry.sessions.insert(id, handle);
                Ok(Response::Created(info))
            }
            Err(error) => {
                self.session_count.fetch_sub(1, Ordering::AcqRel);
                Err(error)
            }
        }
    }
}

pub(crate) struct Server {
    listener: UnixListener,
    paths: Paths,
    state: Arc<State>,
    _lock: FileLock,
}

impl Server {
    pub(crate) fn bind(paths: Paths) -> Result<Self> {
        paths.prepare()?;
        let lock = FileLock::acquire(&paths.directory.join("runtime.lock"), Duration::ZERO)?;
        match fs::symlink_metadata(paths.socket()) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket()
                        && metadata.uid() == unsafe { libc::geteuid() },
                    "runtime socket path is not an owned socket"
                );
                ensure!(
                    UnixStream::connect(paths.socket()).is_err(),
                    "session runtime is already running"
                );
                fs::remove_file(paths.socket())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect runtime socket"),
        }
        // A runtime that was stopped outright leaves its secret behind.
        forget_stale_secrets(&paths);
        let token = write_token(&paths)?;
        let listener =
            UnixListener::bind(paths.socket()).context("cannot bind session runtime socket")?;
        fs::set_permissions(paths.socket(), fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            paths,
            state: Arc::new(State {
                registry: Mutex::new(Registry::default()),
                session_count: AtomicUsize::new(0),
                connections: AtomicUsize::new(0),
                inflight_bytes: AtomicUsize::new(0),
                stopping: AtomicBool::new(false),
                token,
            }),
            _lock: lock,
        })
    }

    pub(crate) fn run(self) -> Result<()> {
        while !self.state.stopping.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if !same_user(&stream)
                        || !peer_is_this_program(&stream)
                        || reserve(&self.state.connections, 1, MAX_CONNECTIONS).is_err()
                    {
                        continue;
                    }
                    let state = Arc::clone(&self.state);
                    let spawned = thread::Builder::new()
                        .name("session-connection".into())
                        .spawn(move || {
                            let _lease = CountLease {
                                count: &state.connections,
                                amount: 1,
                            };
                            if let Err(error) = serve_connection(stream, &state) {
                                log::debug!("session connection closed: {error:#}");
                            }
                        });
                    if let Err(error) = spawned {
                        self.state.connections.fetch_sub(1, Ordering::AcqRel);
                        log::warn!("failed to start session connection: {error}");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut descriptor = libc::pollfd {
                        fd: self.listener.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let result = unsafe { libc::poll(&mut descriptor, 1, 100) };
                    if result < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::Interrupted {
                            return Err(error).context("session runtime listener wait failed");
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("session runtime accept failed"),
            }
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.paths.socket());
        forget_token(&self.paths);
    }
}

fn serve_connection(mut stream: UnixStream, state: &State) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    loop {
        let length = read_length(&mut stream)?;
        reserve(&state.inflight_bytes, length, MAX_INFLIGHT_BYTES)
            .context("session runtime message capacity reached")?;
        let lease = CountLease {
            count: &state.inflight_bytes,
            amount: length,
        };
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes)?;
        let envelope: Envelope =
            serde_json::from_slice(&bytes).context("invalid runtime message")?;
        if !same_token(&state.token, &envelope.token) {
            write_frame(
                &mut stream,
                &Response::Error("session runtime authentication failed".into()),
            )?;
            return Ok(());
        }
        if envelope.version != VERSION {
            write_frame(
                &mut stream,
                &Response::Error(format!(
                    "session runtime protocol version {} is incompatible with {VERSION}",
                    envelope.version
                )),
            )?;
            return Ok(());
        }
        if let Request::ReportAgent { report, .. } = &envelope.request {
            let valid = peer_audit(&stream).is_some_and(|(pid, version)| {
                resume::reported_by(report.agent(), pid) && pid_version(pid) == Some(version)
            });
            if !valid {
                write_frame(
                    &mut stream,
                    &Response::Error("not a report from the agent's own hook".into()),
                )?;
                continue;
            }
        }
        drop(bytes);
        let response = state
            .handle(envelope.request)
            .unwrap_or_else(|error| Response::Error(format!("{error:#}")));
        drop(lease);
        write_frame(&mut stream, &response)?;
        if state.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
    }
}

struct CountLease<'a> {
    count: &'a AtomicUsize,
    amount: usize,
}

impl Drop for CountLease<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(self.amount, Ordering::AcqRel);
    }
}
