//! Private local transport for sessions that outlive their attached windows.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    control,
    hooks::Bootstrapped,
    process_info,
    runtime_engine::{self, SessionHandle},
    runtime_protocol::{
        Blocks, Envelope, Launch, MAX_MESSAGE_BYTES, Match, Operation, Request, Response,
        SessionInfo, Snapshot, VERSION,
    },
    settings::SettingsStore,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const MAX_SESSIONS: usize = 128;
const MAX_CONNECTIONS: usize = 160;
const MAX_INFLIGHT_BYTES: usize = 32 * 1024 * 1024;
const CLIENT_QUEUE_BYTES: usize = 512 * 1024;
const CLIENT_QUEUE_CAPACITY: usize = 128;

#[derive(Clone)]
struct Paths {
    directory: PathBuf,
}

impl Paths {
    fn configured() -> Self {
        Self {
            directory: SettingsStore::config_dir().join("runtime"),
        }
    }

    fn socket(&self) -> PathBuf {
        self.directory.join("sessions.sock")
    }

    /// The name of the keychain item holding the runtime's secret. The
    /// name is not secret; the item is.
    fn token_id(&self) -> PathBuf {
        self.directory.join("token-id")
    }

    fn prepare(&self) -> Result<()> {
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

struct FileLock(File);

impl FileLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
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

fn private_file(path: &Path, create: bool) -> Result<File> {
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

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn random_hex<const N: usize>() -> Result<String> {
    Ok(random_bytes::<N>()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_token_id(paths: &Paths) -> Result<String> {
    let mut id = String::new();
    private_file(&paths.token_id(), false)?
        .take(33)
        .read_to_string(&mut id)?;
    ensure!(is_hex(&id, 32), "runtime authentication id is invalid");
    Ok(id)
}

fn read_token(paths: &Paths) -> Result<String> {
    let id = read_token_id(paths)?;
    let token = secret_store::load(paths, &id)?;
    ensure!(
        is_hex(&token, 64),
        "runtime authentication secret is invalid"
    );
    Ok(token)
}

/// Make a secret for this run of the runtime, keep it where only this
/// program can read it, and record which item holds it.
fn write_token(paths: &Paths) -> Result<String> {
    // Opening the id file first refuses a planted symlink before anything
    // is stored.
    let mut file = private_file(&paths.token_id(), true)?;
    let id = random_hex::<16>()?;
    let token = random_hex::<32>()?;
    secret_store::store(paths, &id, &token)?;
    file.set_len(0)?;
    file.write_all(id.as_bytes())?;
    file.sync_all()?;
    Ok(token)
}

/// Remove the running runtime's secret.
fn forget_token(paths: &Paths) {
    if let Ok(id) = read_token_id(paths) {
        secret_store::delete(paths, &id);
    }
    let _ = fs::remove_file(paths.token_id());
}

/// The runtime's secret lives in the login keychain rather than a file:
/// the item's access list trusts only the program that created it, so
/// other programs running as the user, sandboxed agents among them, cannot
/// read it without the user's approval.
mod keychain {
    use anyhow::{Context as _, Result};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::Paths;

    const SERVICE: &str = "Terminal session runtime";

    pub fn store(_paths: &Paths, id: &str, token: &str) -> Result<()> {
        set_generic_password(SERVICE, id, token.as_bytes())
            .context("failed to store the session runtime secret in the keychain")
    }

    pub fn load(_paths: &Paths, id: &str) -> Result<String> {
        let bytes = get_generic_password(SERVICE, id)
            .context("failed to read the session runtime secret from the keychain")?;
        Ok(String::from_utf8(bytes)?)
    }

    pub fn delete(_paths: &Paths, id: &str) {
        let _ = delete_generic_password(SERVICE, id);
    }
}

/// Tests keep the secret in a private file in the runtime directory, so
/// they never touch the user's keychain.
#[cfg(test)]
mod test_secret_store {
    use std::{
        fs,
        io::{Read, Write},
        path::PathBuf,
    };

    use anyhow::Result;

    use super::{Paths, private_file};

    fn path(paths: &Paths, id: &str) -> PathBuf {
        paths.directory.join(format!("secret-{id}"))
    }

    pub fn store(paths: &Paths, id: &str, token: &str) -> Result<()> {
        let mut file = private_file(&path(paths, id), true)?;
        file.set_len(0)?;
        file.write_all(token.as_bytes())?;
        Ok(())
    }

    pub fn load(paths: &Paths, id: &str) -> Result<String> {
        let mut token = String::new();
        private_file(&path(paths, id), false)?.read_to_string(&mut token)?;
        Ok(token)
    }

    pub fn delete(paths: &Paths, id: &str) {
        let _ = fs::remove_file(path(paths, id));
    }
}

#[cfg(not(test))]
use keychain as secret_store;
#[cfg(test)]
use test_secret_store as secret_store;

fn same_token(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    expected
        .bytes()
        .zip(candidate.bytes())
        .fold(0, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// Whether the process on the other end of `stream` runs this same
/// program. Clients check it before presenting the runtime's secret, so a
/// program that swaps in its own socket cannot collect the secret, and the
/// runtime checks it before serving a connection.
fn peer_is_this_program(stream: &UnixStream) -> bool {
    let Some(peer) = control::peer_pid(stream).and_then(process_info::executable_path) else {
        return false;
    };
    let (Ok(peer), Ok(own)) = (
        fs::canonicalize(peer),
        std::env::current_exe().and_then(fs::canonicalize),
    ) else {
        return false;
    };
    peer == own
}

fn same_user(stream: &UnixStream) -> bool {
    let mut uid = 0;
    let mut gid = 0;
    unsafe {
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::geteuid()
    }
}

struct Rpc {
    stream: UnixStream,
    token: String,
}

#[derive(Debug)]
struct RemoteError(String);

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RemoteError {}

impl Rpc {
    fn connect(paths: &Paths) -> Result<Self> {
        let stream = UnixStream::connect(paths.socket())?;
        Self::handshake(stream, paths)
    }

    fn handshake(stream: UnixStream, paths: &Paths) -> Result<Self> {
        ensure!(
            same_user(&stream),
            "session runtime belongs to another user"
        );
        ensure!(
            peer_is_this_program(&stream),
            "the session runtime socket is served by another program"
        );
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut client = Self {
            stream,
            token: read_token(paths)?,
        };
        match client.request(Request::Ping)? {
            Response::Ready { version } if version == VERSION => Ok(client),
            Response::Ready { version } => {
                bail!("session runtime protocol version {version} is incompatible with {VERSION}")
            }
            _ => bail!("session runtime returned an invalid handshake"),
        }
    }

    fn request(&mut self, request: Request) -> Result<Response> {
        write_frame(
            &mut self.stream,
            &Envelope {
                version: VERSION,
                token: self.token.clone(),
                request,
            },
        )?;
        match read_frame(&mut self.stream)? {
            Response::Error(message) => Err(RemoteError(message).into()),
            response => Ok(response),
        }
    }
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
    Server::bind(Paths::configured())?.run()
}

pub fn create_session(launch: Launch) -> Result<SessionInfo> {
    ensure_running()?;
    match Rpc::connect(&Paths::configured())?.request(Request::Create(launch))? {
        Response::Created(info) => Ok(info),
        _ => bail!("session runtime returned an invalid create response"),
    }
}

pub fn new_session_id() -> Result<u64> {
    loop {
        let id = u64::from_ne_bytes(random_bytes::<8>()?) & ((1u64 << 53) - 1);
        if id != 0 {
            return Ok(id);
        }
    }
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    ensure_running()?;
    match Rpc::connect(&Paths::configured())?.request(Request::List)? {
        Response::Sessions(sessions) => Ok(sessions),
        _ => bail!("session runtime returned an invalid list response"),
    }
}

pub fn stop_session(id: u64) -> Result<()> {
    let mut client = Rpc::connect(&Paths::configured())?;
    match client.request(Request::Operate {
        session: id,
        operation: Operation::Stop,
    })? {
        Response::Done => {}
        _ => bail!("session runtime returned an invalid stop response"),
    }
    let started = Instant::now();
    loop {
        if let Response::Sessions(sessions) = client.request(Request::List)? {
            let session = sessions
                .into_iter()
                .find(|session| session.id == id)
                .context("session no longer exists")?;
            if session.exited {
                return Ok(());
            }
        } else {
            bail!("session runtime returned an invalid list response");
        }
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "session has not exited yet"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn remove_session(id: u64) -> Result<()> {
    match Rpc::connect(&Paths::configured())?.request(Request::Remove { session: id })? {
        Response::Done => Ok(()),
        _ => bail!("session runtime returned an invalid remove response"),
    }
}

pub fn read_session(id: u64, lines: usize) -> Result<String> {
    read_session_at(&Paths::configured(), id, lines)
}

fn read_session_at(paths: &Paths, id: u64, lines: usize) -> Result<String> {
    match Rpc::connect(paths)?.request(Request::Operate {
        session: id,
        operation: Operation::Read {
            lines: lines.min(10_000),
        },
    })? {
        Response::Text(text) => Ok(text),
        _ => bail!("session runtime returned an invalid read response"),
    }
}

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
        let result = runtime_engine::spawn(id, launch);
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

struct Server {
    listener: UnixListener,
    paths: Paths,
    state: Arc<State>,
    _lock: FileLock,
}

impl Server {
    fn bind(paths: Paths) -> Result<Self> {
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

    fn run(self) -> Result<()> {
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

fn reserve(count: &AtomicUsize, amount: usize, limit: usize) -> Result<()> {
    count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(amount).filter(|total| *total <= limit)
        })
        .map(|_| ())
        .map_err(|_| anyhow!("capacity exceeded"))
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

fn read_length(stream: &mut impl Read) -> Result<usize> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        length > 0 && length <= MAX_MESSAGE_BYTES,
        "runtime message exceeds its size limit"
    );
    Ok(length)
}

fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> Result<T> {
    let length = read_length(stream)?;
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).context("invalid runtime response")
}

fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "runtime message exceeds its size limit"
    );
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

pub enum ClientEvent {
    Frame(Box<Snapshot>),
    /// Rows of a command block from `from` on, at `version`; rows from an
    /// earlier version are replaced.
    BlockRows {
        block: u64,
        version: u64,
        from: usize,
        rows: Vec<String>,
    },
    /// What the shell reported about itself.
    Shell(Box<Option<Bootstrapped>>),
    Text(String),
    Matches {
        query: String,
        matches: Vec<Match>,
    },
    Disconnected(String),
    Error(String),
}

struct QueuedOperation {
    operation: Operation,
    bytes: usize,
}

pub struct SessionClient {
    operations: mpsc::SyncSender<QueuedOperation>,
    pending_bytes: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    refresh: Arc<AtomicBool>,
}

impl SessionClient {
    pub fn attach(session: u64) -> Result<(Self, async_channel::Receiver<ClientEvent>)> {
        Self::attach_at(session, Paths::configured())
    }

    fn attach_at(
        session: u64,
        paths: Paths,
    ) -> Result<(Self, async_channel::Receiver<ClientEvent>)> {
        let (operations, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
        let (events, output) = async_channel::bounded(2);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(false));
        let connection_paths = paths.clone();
        let pending = Arc::clone(&pending_bytes);
        let cancellation = Arc::clone(&stopped);
        let refresh_requested = Arc::clone(&refresh);
        thread::Builder::new()
            .name(format!("session-view-{session}"))
            .spawn(move || {
                client_loop(
                    session,
                    &connection_paths,
                    receiver,
                    &pending,
                    &events,
                    &cancellation,
                    &refresh_requested,
                );
            })
            .context("cannot start session connection")?;
        Ok((
            Self {
                operations,
                pending_bytes,
                stopped,
                refresh,
            },
            output,
        ))
    }

    pub fn refresh(&self) {
        self.refresh.store(true, Ordering::Release);
    }

    pub fn send(&self, operation: Operation) -> Result<()> {
        let bytes = operation_size(&operation);
        reserve(&self.pending_bytes, bytes, CLIENT_QUEUE_BYTES)
            .context("session input is busy; try again")?;
        if let Err(error) = self
            .operations
            .try_send(QueuedOperation { operation, bytes })
        {
            self.pending_bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err(match error {
                mpsc::TrySendError::Full(_) => anyhow!("session input is busy; try again"),
                mpsc::TrySendError::Disconnected(_) => anyhow!("session connection is closed"),
            });
        }
        Ok(())
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn operation_size(operation: &Operation) -> usize {
    let payload = match operation {
        Operation::Input { bytes } => bytes.len(),
        Operation::Paste { text } | Operation::Search { query: text } => text.len(),
        Operation::Key {
            text: Some(text), ..
        } => text.len(),
        _ => 0,
    };
    payload.saturating_add(256)
}

fn deliver(
    events: &async_channel::Sender<ClientEvent>,
    mut event: ClientEvent,
    stopped: &AtomicBool,
) -> bool {
    loop {
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        match events.try_send(event) {
            Ok(()) => return true,
            Err(async_channel::TrySendError::Closed(_)) => return false,
            Err(async_channel::TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(Duration::from_millis(4));
            }
        }
    }
}

fn client_loop(
    session: u64,
    paths: &Paths,
    operations: mpsc::Receiver<QueuedOperation>,
    pending_bytes: &AtomicUsize,
    events: &async_channel::Sender<ClientEvent>,
    stopped: &AtomicBool,
    refresh: &AtomicBool,
) {
    let mut connection = None;
    let mut revision = None;
    let mut poll_at = Instant::now();
    let mut retry_at = Instant::now();
    let mut reported_error = None;
    let mut block_state = BlockFetchState::default();
    while !stopped.load(Ordering::Acquire) && !events.is_closed() {
        if refresh.swap(false, Ordering::AcqRel) {
            retry_at = Instant::now();
            poll_at = Instant::now();
            revision = None;
        }
        if connection.is_none() && Instant::now() >= retry_at {
            match Rpc::connect(paths) {
                Ok(client) => {
                    connection = Some(client);
                    revision = None;
                    poll_at = Instant::now();
                }
                Err(error) => {
                    let message = format!("Session disconnected: {error:#}");
                    if reported_error.as_ref() != Some(&message) {
                        if !deliver(events, ClientEvent::Disconnected(message.clone()), stopped) {
                            return;
                        }
                        reported_error = Some(message);
                    }
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }

        let timeout = if connection.is_some() {
            poll_at.saturating_duration_since(Instant::now())
        } else {
            Duration::from_millis(16)
        };
        match operations.recv_timeout(timeout) {
            Ok(queued) => {
                pending_bytes.fetch_sub(queued.bytes, Ordering::AcqRel);
                if let Some(client) = connection.as_mut() {
                    match client.request(Request::Operate {
                        session,
                        operation: queued.operation,
                    }) {
                        Ok(Response::Text(text)) => {
                            if !deliver(events, ClientEvent::Text(text), stopped) {
                                return;
                            }
                        }
                        Ok(Response::Matches { query, matches }) => {
                            if !deliver(events, ClientEvent::Matches { query, matches }, stopped) {
                                return;
                            }
                        }
                        Ok(Response::Done) => {}
                        Ok(_) => {
                            if !deliver(
                                events,
                                ClientEvent::Error(
                                    "Session returned an invalid operation response".into(),
                                ),
                                stopped,
                            ) {
                                return;
                            }
                        }
                        Err(error) => {
                            let remote_error = error.downcast_ref::<RemoteError>().is_some();
                            if !deliver(
                                events,
                                ClientEvent::Error(format!(
                                    "Session operation failed; it was not retried: {error:#}"
                                )),
                                stopped,
                            ) {
                                return;
                            }
                            if !remote_error {
                                if !deliver(
                                    events,
                                    ClientEvent::Disconnected(format!(
                                        "Session disconnected: {error:#}"
                                    )),
                                    stopped,
                                ) {
                                    return;
                                }
                                connection = None;
                                retry_at = Instant::now() + Duration::from_millis(250);
                            }
                        }
                    }
                } else if !deliver(
                    events,
                    ClientEvent::Error("Session is disconnected; input was not sent".into()),
                    stopped,
                ) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if Instant::now() < poll_at {
            continue;
        }
        if let Some(client) = connection.as_mut() {
            match client.request(Request::Poll { session, revision }) {
                Ok(Response::Snapshot(snapshot)) => {
                    if let Some(blocks) = &snapshot.blocks
                        && !fetch_block_updates(
                            client,
                            session,
                            blocks,
                            &mut block_state,
                            events,
                            stopped,
                        )
                    {
                        return;
                    }
                    let next_revision = snapshot.revision;
                    if events
                        .try_send(ClientEvent::Frame(Box::new(snapshot)))
                        .is_ok()
                    {
                        revision = Some(next_revision);
                        reported_error = None;
                    }
                }
                Ok(Response::Unchanged) => {}
                Ok(_) => {
                    if !deliver(
                        events,
                        ClientEvent::Disconnected(
                            "Session returned an invalid frame response".into(),
                        ),
                        stopped,
                    ) {
                        return;
                    }
                    connection = None;
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
                Err(error) => {
                    let message = format!("Session disconnected: {error:#}");
                    if reported_error.as_ref() != Some(&message) {
                        if !deliver(events, ClientEvent::Disconnected(message.clone()), stopped) {
                            return;
                        }
                        reported_error = Some(message);
                    }
                    connection = None;
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        poll_at = Instant::now() + POLL_INTERVAL;
    }
}

/// What a view has been sent of a session's blocks.
#[derive(Default)]
struct BlockFetchState {
    shell_serial: u64,
    /// Version and number of rows sent, per block.
    rows: HashMap<u64, (u64, usize)>,
}

/// Send the view block rows and shell details a snapshot announces that it
/// does not have yet. Returns false once the view is gone.
fn fetch_block_updates(
    client: &mut Rpc,
    session: u64,
    blocks: &Blocks,
    state: &mut BlockFetchState,
    events: &async_channel::Sender<ClientEvent>,
    stopped: &AtomicBool,
) -> bool {
    if blocks.shell_serial != state.shell_serial
        && let Ok(Response::Shell(shell)) = client.request(Request::Operate {
            session,
            operation: Operation::Shell,
        })
    {
        state.shell_serial = blocks.shell_serial;
        if !deliver(events, ClientEvent::Shell(shell), stopped) {
            return false;
        }
    }
    state
        .rows
        .retain(|id, _| blocks.items.iter().any(|item| item.id == *id));
    for item in &blocks.items {
        let sent = state.rows.entry(item.id).or_insert((item.version, 0));
        if sent.0 != item.version {
            *sent = (item.version, 0);
        }
        while sent.1 < item.rows {
            let Ok(Response::BlockRows {
                version,
                from,
                rows,
                ..
            }) = client.request(Request::Operate {
                session,
                operation: Operation::BlockRows {
                    block: item.id,
                    from: sent.1,
                },
            })
            else {
                break;
            };
            // The block changed since the snapshot; the next one says how.
            if version != item.version || rows.is_empty() {
                break;
            }
            sent.1 = from + rows.len();
            if !deliver(
                events,
                ClientEvent::BlockRows {
                    block: item.id,
                    version,
                    from,
                    rows,
                },
                stopped,
            ) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_protocol::{Colors, Dimensions};
    use std::{collections::BTreeMap, io::Cursor, process::Child};

    const TEST_DIRECTORY: &str = "TERMINAL_RUNTIME_TEST_DIRECTORY";
    const TEST_SESSION: &str = "TERMINAL_RUNTIME_TEST_SESSION";

    struct TestRuntime {
        paths: Paths,
        child: Child,
    }

    impl TestRuntime {
        fn start() -> Self {
            let paths = temporary_paths();
            paths.prepare().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "runtime::tests::daemon_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env(TEST_DIRECTORY, &paths.directory)
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
            let mut runtime = Self {
                paths,
                child: command.spawn().unwrap(),
            };
            let deadline = Instant::now() + STARTUP_TIMEOUT;
            loop {
                if Rpc::connect(&runtime.paths).is_ok() {
                    return runtime;
                }
                assert!(
                    runtime.child.try_wait().unwrap().is_none(),
                    "test runtime exited during startup"
                );
                assert!(Instant::now() < deadline, "test runtime startup timed out");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn rpc(&self) -> Rpc {
            Rpc::connect(&self.paths).unwrap()
        }

        fn create(&self, command: &str) -> SessionInfo {
            match self
                .rpc()
                .request(Request::Create(launch(command)))
                .unwrap()
            {
                Response::Created(info) => info,
                _ => panic!("invalid create response"),
            }
        }

        fn list(&self) -> Vec<SessionInfo> {
            match self.rpc().request(Request::List).unwrap() {
                Response::Sessions(sessions) => sessions,
                _ => panic!("invalid list response"),
            }
        }
    }

    impl Drop for TestRuntime {
        fn drop(&mut self) {
            if let Ok(mut client) = Rpc::connect(&self.paths) {
                if let Ok(Response::Sessions(sessions)) = client.request(Request::List) {
                    for session in sessions {
                        let _ = client.request(Request::Operate {
                            session: session.id,
                            operation: Operation::Stop,
                        });
                        let _ = client.request(Request::Remove {
                            session: session.id,
                        });
                    }
                }
                let _ = client.request(Request::Shutdown);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_dir_all(&self.paths.directory);
        }
    }

    fn temporary_paths() -> Paths {
        Paths {
            directory: PathBuf::from(format!(
                "/tmp/terminal-runtime-{}",
                new_session_id().unwrap()
            )),
        }
    }

    fn launch(command: &str) -> Launch {
        Launch {
            id: new_session_id().unwrap(),
            argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
            env: BTreeMap::new(),
            cwd: Some(PathBuf::from("/tmp")),
            startup: None,
            dimensions: Dimensions {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            colors: Colors {
                foreground: [240; 3],
                background: [20; 3],
                cursor: [240; 3],
                palette: [[128; 3]; 16],
                dark: true,
            },
            control_token: "test-control-capability".into(),
            command_blocks: false,
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "runtime condition timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "headless runtime subprocess entry point"]
    fn daemon_process() {
        let Some(directory) = std::env::var_os(TEST_DIRECTORY) else {
            return;
        };
        Server::bind(Paths {
            directory: PathBuf::from(directory),
        })
        .unwrap()
        .run()
        .unwrap();
    }

    #[test]
    #[ignore = "session client subprocess entry point"]
    fn view_process() {
        let Some(directory) = std::env::var_os(TEST_DIRECTORY) else {
            return;
        };
        let paths = Paths {
            directory: PathBuf::from(directory),
        };
        let id = std::env::var(TEST_SESSION).unwrap().parse().unwrap();
        let (_client, events) = SessionClient::attach_at(id, paths.clone()).unwrap();
        while let Ok(event) = events.recv_blocking() {
            if matches!(event, ClientEvent::Frame(_)) {
                private_file(&paths.directory.join("view-ready"), true)
                    .unwrap()
                    .write_all(b"ready")
                    .unwrap();
            }
        }
    }

    #[test]
    fn frames_reject_oversized_and_truncated_messages() {
        let mut oversized = Cursor::new(((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes());
        assert!(read_frame::<Response>(&mut oversized).is_err());
        let mut empty = Cursor::new(0u32.to_be_bytes());
        assert!(read_frame::<Response>(&mut empty).is_err());
        let mut truncated = Cursor::new(vec![0, 0, 0, 10, b'{']);
        assert!(read_frame::<Response>(&mut truncated).is_err());
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &Response::Ready { version: VERSION }).unwrap();
        assert!(matches!(
            read_frame::<Response>(&mut Cursor::new(encoded)).unwrap(),
            Response::Ready { version: VERSION }
        ));
    }

    #[test]
    fn private_runtime_rejects_shared_permissions_and_symlinks() {
        let paths = temporary_paths();
        paths.prepare().unwrap();
        fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(paths.prepare().is_err());
        fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o700)).unwrap();
        let target = paths.directory.join("target");
        fs::write(&target, "unrelated").unwrap();
        std::os::unix::fs::symlink(&target, paths.token_id()).unwrap();
        assert!(write_token(&paths).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "unrelated");
        fs::remove_dir_all(paths.directory).unwrap();
    }

    #[test]
    fn peers_running_this_program_are_recognized() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        assert!(peer_is_this_program(&ours));
        assert!(peer_is_this_program(&theirs));
    }

    #[test]
    fn the_keychain_keeps_and_forgets_a_secret() {
        let paths = temporary_paths();
        let id = random_hex::<16>().unwrap();
        keychain::store(&paths, &id, "secret").unwrap();
        assert_eq!(keychain::load(&paths, &id).unwrap(), "secret");
        keychain::delete(&paths, &id);
        assert!(keychain::load(&paths, &id).is_err());
    }

    #[test]
    fn runtime_authentication_and_version_are_checked() {
        let runtime = TestRuntime::start();
        let token = read_token(&runtime.paths).unwrap();
        for (token, version, message) in [
            ("incorrect".to_owned(), VERSION, "authentication"),
            (token, VERSION + 1, "version"),
        ] {
            let mut stream = UnixStream::connect(runtime.paths.socket()).unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            write_frame(
                &mut stream,
                &Envelope {
                    version,
                    token,
                    request: Request::List,
                },
            )
            .unwrap();
            match read_frame::<Response>(&mut stream).unwrap() {
                Response::Error(error) => assert!(error.contains(message)),
                _ => panic!("unauthorized request was accepted"),
            }
        }
        assert_eq!(
            fs::metadata(runtime.paths.socket()).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(runtime.paths.token_id()).unwrap().mode() & 0o777,
            0o600
        );
        assert!(Server::bind(runtime.paths.clone()).is_err());
        assert!(matches!(
            runtime.rpc().request(Request::Ping).unwrap(),
            Response::Ready { .. }
        ));
    }

    #[test]
    fn detached_runtime_preserves_process_output_input_and_exit_status() {
        let runtime = TestRuntime::start();
        assert_eq!(
            unsafe { libc::getsid(runtime.child.id() as i32) },
            runtime.child.id() as i32
        );
        let info = runtime.create("printf 'READY\\n'; sleep 0.1; printf 'DETACHED\\n'; IFS= read -r line; printf 'INPUT:%s\\n' \"$line\"; exit 7");
        let pid = info.shell_pid.unwrap();
        let (client, events) = SessionClient::attach_at(info.id, runtime.paths.clone()).unwrap();
        wait_until(|| matches!(events.try_recv(), Ok(ClientEvent::Frame(_))));
        drop(client);
        drop(events);
        let mut crashed_view = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::tests::view_process",
                "--ignored",
                "--nocapture",
            ])
            .env(TEST_DIRECTORY, &runtime.paths.directory)
            .env(TEST_SESSION, info.id.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until(|| runtime.paths.directory.join("view-ready").exists());
        crashed_view.kill().unwrap();
        crashed_view.wait().unwrap();
        wait_until(|| {
            read_session_at(&runtime.paths, info.id, 100)
                .unwrap()
                .contains("DETACHED")
        });
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        assert_eq!(
            runtime
                .list()
                .iter()
                .find(|item| item.id == info.id)
                .unwrap()
                .shell_pid,
            Some(pid)
        );
        let (client, events) = SessionClient::attach_at(info.id, runtime.paths.clone()).unwrap();
        wait_until(|| matches!(events.try_recv(), Ok(ClientEvent::Frame(_))));
        client
            .send(Operation::Input {
                bytes: b"still-here\n".to_vec(),
            })
            .unwrap();
        wait_until(|| {
            runtime
                .list()
                .iter()
                .any(|item| item.id == info.id && item.exited)
        });
        assert!(
            read_session_at(&runtime.paths, info.id, 100)
                .unwrap()
                .contains("INPUT:still-here")
        );
        assert_eq!(
            runtime
                .list()
                .iter()
                .find(|item| item.id == info.id)
                .unwrap()
                .exit_code,
            Some(7)
        );
        drop(client);
        drop(events);
        assert!(matches!(
            runtime
                .rpc()
                .request(Request::Remove { session: info.id })
                .unwrap(),
            Response::Done
        ));
        assert!(runtime.list().is_empty());
    }

    #[test]
    fn flooded_session_and_slow_view_leave_other_sessions_responsive() {
        let runtime = TestRuntime::start();
        let noisy = runtime.create("while :; do printf '0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\\n'; done");
        let quiet = runtime.create("IFS= read -r line; printf 'QUIET:%s\\n' \"$line\"");
        let (view, unread_events) =
            SessionClient::attach_at(noisy.id, runtime.paths.clone()).unwrap();
        wait_until(|| unread_events.len() == 2);
        view.send(Operation::Search {
            query: "012345".into(),
        })
        .unwrap();
        let mut slow = UnixStream::connect(runtime.paths.socket()).unwrap();
        slow.write_all(&1024u32.to_be_bytes()).unwrap();
        let started = Instant::now();
        let mut client = runtime.rpc();
        assert_eq!(runtime.list().len(), 2);
        assert!(matches!(
            client
                .request(Request::Operate {
                    session: quiet.id,
                    operation: Operation::Input {
                        bytes: b"responsive\n".to_vec()
                    }
                })
                .unwrap(),
            Response::Done
        ));
        wait_until(|| {
            read_session_at(&runtime.paths, quiet.id, 100)
                .unwrap()
                .contains("QUIET:responsive")
        });
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "independent session was blocked by output flood"
        );
        wait_until(
            || matches!(unread_events.try_recv(), Ok(ClientEvent::Matches { query, .. }) if query == "012345"),
        );
        assert!(
            client
                .request(Request::Remove { session: noisy.id })
                .is_err()
        );
        assert!(client.request(Request::Shutdown).is_err());
        assert!(matches!(
            client
                .request(Request::Operate {
                    session: noisy.id,
                    operation: Operation::Stop
                })
                .unwrap(),
            Response::Done
        ));
    }

    #[test]
    fn duplicate_session_ids_do_not_start_another_process() {
        let runtime = TestRuntime::start();
        let command = launch("IFS= read -r line");
        let duplicate = command.clone();
        runtime.rpc().request(Request::Create(command)).unwrap();
        assert!(
            runtime
                .rpc()
                .request(Request::Create(duplicate))
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(runtime.list().len(), 1);
    }

    #[test]
    fn missing_session_reconnects_to_reserved_identity_without_spawning() {
        let runtime = TestRuntime::start();
        let command = launch("IFS= read -r line");
        let id = command.id;
        let (client, events) = SessionClient::attach_at(id, runtime.paths.clone()).unwrap();
        wait_until(
            || matches!(events.try_recv(), Ok(ClientEvent::Disconnected(message)) if message.contains("no longer exists")),
        );
        assert!(runtime.list().is_empty());
        runtime.rpc().request(Request::Create(command)).unwrap();
        client.refresh();
        wait_until(
            || matches!(events.try_recv(), Ok(ClientEvent::Frame(frame)) if frame.info.id == id),
        );
        assert_eq!(runtime.list().len(), 1);
    }

    #[test]
    fn rejected_input_reports_an_error_and_keeps_the_connection_usable() {
        let runtime = TestRuntime::start();
        let session = runtime.create("IFS= read -r line; printf 'INPUT:%s\\n' \"$line\"");
        let (client, events) = SessionClient::attach_at(session.id, runtime.paths.clone()).unwrap();
        wait_until(|| matches!(events.try_recv(), Ok(ClientEvent::Frame(_))));
        client
            .send(Operation::Input {
                bytes: vec![b'x'; 256 * 1024 + 1],
            })
            .unwrap();
        wait_until(
            || matches!(events.try_recv(), Ok(ClientEvent::Error(message)) if message.contains("not retried")),
        );
        client
            .send(Operation::Input {
                bytes: b"accepted\n".to_vec(),
            })
            .unwrap();
        wait_until(|| {
            read_session_at(&runtime.paths, session.id, 100)
                .unwrap()
                .contains("INPUT:accepted")
        });
    }

    #[test]
    fn queued_input_is_bounded_and_rejects_without_losing_accepted_bytes() {
        let (operations, receiver) = mpsc::sync_channel(1);
        let client = SessionClient {
            operations,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
            refresh: Arc::new(AtomicBool::new(false)),
        };
        client
            .send(Operation::Input {
                bytes: b"accepted".to_vec(),
            })
            .unwrap();
        let reserved = client.pending_bytes.load(Ordering::Acquire);
        assert!(
            client
                .send(Operation::Input {
                    bytes: b"rejected".to_vec()
                })
                .is_err()
        );
        assert_eq!(client.pending_bytes.load(Ordering::Acquire), reserved);
        assert!(
            client
                .send(Operation::Paste {
                    text: "x".repeat(CLIENT_QUEUE_BYTES)
                })
                .is_err()
        );
        assert!(
            matches!(receiver.recv().unwrap().operation, Operation::Input { bytes } if bytes == b"accepted")
        );
    }
}
