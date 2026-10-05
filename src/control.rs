//! The control API: a Unix socket through which local programs (the CLI,
//! the MCP server, scripts, agents) list, open, drive, and read tabs.
//!
//! Requests and replies are single JSON lines. The socket is created
//! owner-only and connections from other users are refused. Anything
//! beyond listing tabs (reading output, typing, opening or closing tabs)
//! needs the user's approval, since a sandboxed process able to reach the
//! socket could otherwise run commands in an unsandboxed shell or read
//! other tabs.
//!
//! Callers are identified by a secret token each pane passes to its
//! programs (`TERMINAL_CONTROL_TOKEN`), not by process names or ids, which
//! a program can fake. Approval for a pane covers every program in it.
//! Callers without a valid token are approved per connection only.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, AsyncApp, Context, Entity, PromptLevel, Window, WindowHandle};
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agent_badge, notifications,
    pane_tree::Axis,
    process_info,
    settings::SettingsStore,
    tabs::{TabDestination, TabId, TabStyle},
    terminal_view::TerminalView,
    workspace::Workspace,
};

/// Environment variable holding the socket path for programs in a pane.
pub const SOCKET_VARIABLE: &str = "TERMINAL_SOCKET";

/// Most lines `read_output` returns.
const MAX_READ_LINES: usize = 10_000;

/// Which pane a request is about: a pane, a tab's focused pane, or (with
/// neither) the focused pane of the active tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Typed into the shell once it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Name of an agent profile to launch instead of `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Group to put the tab in, created if no group has this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    ListTabs,
    NewTab(NewTab),
    FocusTab {
        tab: u64,
    },
    CloseTab {
        tab: u64,
    },
    /// Set a tab's name; `None` restores the program's own title.
    RenameTab {
        tab: u64,
        name: Option<String>,
    },
    SendInput {
        #[serde(default)]
        target: PaneTarget,
        text: String,
    },
    ReadOutput {
        #[serde(default)]
        target: PaneTarget,
        lines: usize,
    },
    Split {
        #[serde(default)]
        target: PaneTarget,
        direction: SplitDirection,
        #[serde(default)]
        command: Option<String>,
    },
    Notify {
        #[serde(default)]
        target: PaneTarget,
        title: String,
        #[serde(default)]
        body: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    /// The calling pane's control token, if it runs inside one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A random 128-bit secret, hex encoded.
pub fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut source| std::io::Read::read_exact(&mut source, &mut bytes))
        .context("failed to read random bytes")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn socket_path() -> PathBuf {
    SettingsStore::config_dir().join("control.sock")
}

fn bin_dir() -> PathBuf {
    SettingsStore::config_dir().join("bin")
}

/// Give a shell what it needs to reach the app: the socket path, and the
/// `terminal` command on its PATH.
pub fn configure_command(command: &mut CommandBuilder, cx: &App) {
    if !SettingsStore::get(cx).control_api {
        return;
    }
    command.env(SOCKET_VARIABLE, socket_path());
    let bin = bin_dir();
    if bin.is_dir() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut joined = bin.into_os_string();
        joined.push(":");
        joined.push(path);
        command.env("PATH", joined);
    }
}

/// Environment variable holding a pane's control token.
pub const TOKEN_VARIABLE: &str = "TERMINAL_CONTROL_TOKEN";

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

/// Where a request came from.
struct Caller {
    connection: u64,
    /// Reported by the OS for the dialog; never trusted for identity.
    peer: Option<i32>,
    token: Option<String>,
}

enum Message {
    Request(Request, Caller, mpsc::Sender<Result<Value, String>>),
    /// A connection closed; its per-connection approval is forgotten.
    Closed(u64),
}

impl Request {
    /// Listing tabs reveals only titles and directories; everything else
    /// reads terminal contents or acts on the user's behalf.
    fn needs_approval(&self) -> bool {
        !matches!(self, Request::ListTabs)
    }
}

/// Who is calling, as far as the token proves.
struct Client {
    /// Approval is remembered under this key: a pane, or one connection.
    key: String,
    description: String,
}

/// Start serving the control API for the workspace in `window`.
pub fn start(window: WindowHandle<Workspace>, cx: &mut App) {
    if !SettingsStore::get(cx).control_api {
        return;
    }
    if let Err(error) = link_cli() {
        log::warn!("could not put the terminal command on PATH: {error:#}");
    }
    let listener = match bind(&socket_path()) {
        Ok(listener) => listener,
        Err(error) => {
            log::warn!("control API unavailable: {error:#}");
            return;
        }
    };

    let (requests_tx, requests_rx) = async_channel::unbounded::<Message>();
    if let Err(error) = thread::Builder::new()
        .name("control-accept".into())
        .spawn(move || accept_connections(listener, requests_tx))
    {
        log::warn!("control API unavailable: {error}");
        return;
    }

    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut decisions: HashMap<String, bool> = HashMap::new();
        while let Ok(message) = requests_rx.recv().await {
            let (request, caller, reply) = match message {
                Message::Request(request, caller, reply) => (request, caller, reply),
                Message::Closed(connection) => {
                    decisions.remove(&connection_key(connection));
                    continue;
                }
            };
            if request.needs_approval() {
                let client = cx
                    .update(|cx| {
                        window.update(cx, |workspace, _, cx| workspace.identify_client(&caller, cx))
                    })
                    .ok();
                let Some(client) = client else {
                    let _ = reply.send(Err("the window is closed".to_string()));
                    continue;
                };
                let allowed = match decisions.get(&client.key) {
                    Some(allowed) => *allowed,
                    None => {
                        let answer = cx.update(|cx| {
                            window.update(cx, |_, window, cx| {
                                let detail = format!(
                                    "{} to control your terminals: read their output, type into them, and open or close tabs.",
                                    client.description
                                );
                                window.prompt(
                                    PromptLevel::Warning,
                                    "Allow terminal control?",
                                    Some(&detail),
                                    &["Allow", "Deny"],
                                    cx,
                                )
                            })
                        });
                        let allowed = match answer {
                            Ok(answer) => answer.await == Ok(0),
                            Err(_) => false,
                        };
                        decisions.insert(client.key, allowed);
                        allowed
                    }
                };
                if !allowed {
                    let _ = reply.send(Err(
                        "the user denied terminal control to this program".to_string(),
                    ));
                    continue;
                }
            }
            let result = cx
                .update(|cx| {
                    window.update(cx, |workspace, window, cx| {
                        workspace.handle_control(request, window, cx)
                    })
                })
                .unwrap_or_else(|_| Err("the window is closed".to_string()));
            let _ = reply.send(result);
        }
    })
    .detach();

    cx.on_app_quit(|_| {
        let _ = fs::remove_file(socket_path());
        async {}
    })
    .detach();
}

/// Bind the socket owner-only, replacing a stale socket file but never a
/// running instance's.
fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another instance is serving {}", path.display());
        }
        fs::remove_file(path).context("failed to remove a stale socket")?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Point `<config>/bin/terminal` at this executable so shells can run the
/// CLI by name.
fn link_cli() -> Result<()> {
    let executable = std::env::current_exe()?;
    let bin = bin_dir();
    fs::create_dir_all(&bin)?;
    let link = bin.join(env!("CARGO_PKG_NAME"));
    if fs::read_link(&link).ok().as_deref() == Some(executable.as_path()) {
        return Ok(());
    }
    if link.exists() || link.is_symlink() {
        fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(&executable, &link)?;
    Ok(())
}

fn accept_connections(listener: UnixListener, requests: async_channel::Sender<Message>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        if !same_user(&stream) {
            log::warn!("refused a control connection from another user");
            continue;
        }
        let requests = requests.clone();
        let _ = thread::Builder::new()
            .name("control-connection".into())
            .spawn(move || serve_connection(stream, requests));
    }
}

fn same_user(stream: &UnixStream) -> bool {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the descriptor is a live socket and both out-pointers are valid.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    // SAFETY: getuid has no preconditions.
    result == 0 && uid == unsafe { libc::getuid() }
}

/// The process on the other end of a local socket.
fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let mut size = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the descriptor is a live socket and `pid`/`size` describe a
    // valid buffer for LOCAL_PEERPID.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        )
    };
    (result == 0 && pid > 0).then_some(pid)
}

fn connection_key(connection: u64) -> String {
    format!("connection:{connection}")
}

fn serve_connection(stream: UnixStream, requests: async_channel::Sender<Message>) {
    let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    let peer = peer_pid(&stream);
    serve_lines(stream, connection, peer, &requests);
    let _ = requests.send_blocking(Message::Closed(connection));
}

fn serve_lines(
    stream: UnixStream,
    connection: u64,
    peer: Option<i32>,
    requests: &async_channel::Sender<Message>,
) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Envelope>(&line) {
            Ok(envelope) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                let outcome = requests
                    .send_blocking(Message::Request(
                        envelope.request,
                        Caller {
                            connection,
                            peer,
                            token: envelope.token,
                        },
                        reply_tx,
                    ))
                    .map_err(|_| "the app is shutting down".to_string())
                    .and_then(|_| {
                        reply_rx
                            .recv()
                            .map_err(|_| "the request was dropped".to_string())
                    })
                    .and_then(|result| result);
                match outcome {
                    Ok(result) => Reply {
                        id: envelope.id,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => Reply {
                        id: envelope.id,
                        result: None,
                        error: Some(error),
                    },
                }
            }
            Err(error) => Reply {
                id: 0,
                result: None,
                error: Some(format!("invalid request: {error}")),
            },
        };
        let Ok(mut encoded) = serde_json::to_string(&reply) else {
            return;
        };
        encoded.push('\n');
        if writer.write_all(encoded.as_bytes()).is_err() {
            return;
        }
    }
}

/// A connection to a running app. Approval given to a caller outside any
/// pane lasts for the connection, so long-lived clients keep one open.
pub struct ControlClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    token: Option<String>,
    next_id: u64,
}

impl ControlClient {
    pub fn connect() -> Result<Self> {
        let path = std::env::var_os(SOCKET_VARIABLE)
            .map(PathBuf::from)
            .unwrap_or_else(socket_path);
        let writer = UnixStream::connect(&path).with_context(|| {
            format!(
                "cannot reach the terminal app at {} — is it running?",
                path.display()
            )
        })?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self {
            writer,
            reader,
            token: std::env::var(TOKEN_VARIABLE).ok(),
            next_id: 1,
        })
    }

    pub fn request(&mut self, request: Request) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&Envelope {
            id,
            token: self.token.clone(),
            request,
        })?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut response = String::new();
        if self.reader.read_line(&mut response)? == 0 {
            bail!("the terminal app closed the connection");
        }
        let reply: Reply = serde_json::from_str(&response).context("invalid reply from the app")?;
        match (reply.result, reply.error) {
            (_, Some(error)) => bail!(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }
}

/// Send one request to a running app over a fresh connection.
pub fn call(request: Request) -> Result<Value> {
    ControlClient::connect()?.request(request)
}

impl Workspace {
    /// Identify a caller by its pane token. Without a valid token the
    /// caller is only this connection, described by the name the OS
    /// reports (shown to the user, never trusted).
    fn identify_client(&self, caller: &Caller, cx: &App) -> Client {
        let pane = caller.token.as_deref().and_then(|token| {
            self.layout.ordered_tabs().into_iter().find_map(|tab| {
                let panes = self.panes_of(tab)?;
                panes
                    .read(cx)
                    .views()
                    .into_iter()
                    .find(|view| view.read(cx).has_control_token(token))
                    .map(|view| (tab, view.read(cx).pane_id()))
            })
        });
        if let Some((tab, pane_id)) = pane {
            let title = self
                .display(tab, cx)
                .map(|display| display.title.to_string())
                .unwrap_or_default();
            return Client {
                key: format!("pane:{pane_id}"),
                description: format!("Programs in the tab “{title}” want"),
            };
        }
        let name = caller
            .peer
            .and_then(process_info::process_name)
            .map(|name| {
                format!("A program outside this app's tabs (calling itself “{name}”) wants")
            })
            .unwrap_or_else(|| "An unidentified program wants".to_string());
        Client {
            key: connection_key(caller.connection),
            description: name,
        }
    }

    pub(crate) fn handle_control(
        &mut self,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        match request {
            Request::ListTabs => Ok(self.describe_tabs(cx)),
            Request::NewTab(new_tab) => self.control_new_tab(new_tab, window, cx),
            Request::FocusTab { tab } => {
                let id = self.find_tab(tab)?;
                self.activate(id, window, cx);
                Ok(json!({}))
            }
            Request::CloseTab { tab } => {
                let id = self.find_tab(tab)?;
                self.close(id, window, cx);
                Ok(json!({}))
            }
            Request::RenameTab { tab, name } => {
                let id = self.find_tab(tab)?;
                self.update_style(id, |style| style.name = name, cx);
                Ok(json!({}))
            }
            Request::SendInput { target, text } => {
                let (_, view) = self.resolve_pane(&target, cx)?;
                view.update(cx, |view, cx| view.send_text(&text, cx));
                Ok(json!({}))
            }
            Request::ReadOutput { target, lines } => {
                let (_, view) = self.resolve_pane(&target, cx)?;
                let text = view.read(cx).recent_text(lines.clamp(1, MAX_READ_LINES));
                Ok(json!({ "text": text }))
            }
            Request::Split {
                target,
                direction,
                command,
            } => {
                let (tab, view) = self.resolve_pane(&target, cx)?;
                let panes = self.panes_of(tab).ok_or("that tab has no terminals")?;
                let axis = match direction {
                    SplitDirection::Right => Axis::Horizontal,
                    SplitDirection::Down => Axis::Vertical,
                };
                let pane = panes
                    .update(cx, |group, cx| {
                        group.split_pane(&view, axis, command.as_deref(), window, cx)
                    })
                    .map_err(|error| format!("{error:#}"))?;
                self.activate(tab, window, cx);
                Ok(json!({ "tab": tab.element_id(), "pane": pane.read(cx).pane_id() }))
            }
            Request::Notify {
                target,
                title,
                body,
            } => {
                let (tab, _) = self.resolve_pane(&target, cx)?;
                let tab_title = self
                    .display(tab, cx)
                    .map(|display| display.title.to_string())
                    .unwrap_or_default();
                let body = if body.is_empty() {
                    title
                } else {
                    format!("“{title}”: {body}")
                };
                notifications::show(tab, tab_title, body, cx);
                Ok(json!({}))
            }
        }
    }

    fn find_tab(&self, id: u64) -> Result<TabId, String> {
        self.layout
            .ordered_tabs()
            .into_iter()
            .find(|tab| tab.element_id() as u64 == id)
            .ok_or_else(|| format!("no tab {id}"))
    }

    fn resolve_pane(
        &self,
        target: &PaneTarget,
        cx: &App,
    ) -> Result<(TabId, Entity<TerminalView>), String> {
        if let Some(pane) = target.pane {
            return self
                .layout
                .ordered_tabs()
                .into_iter()
                .find_map(|tab| {
                    let panes = self.panes_of(tab)?;
                    let view = panes
                        .read(cx)
                        .views()
                        .into_iter()
                        .find(|view| view.read(cx).pane_id() == pane)?;
                    Some((tab, view))
                })
                .ok_or_else(|| format!("no pane {pane}"));
        }
        let tab = match target.tab {
            Some(tab) => self.find_tab(tab)?,
            None => self.active.ok_or("no tab is open")?,
        };
        let panes = self.panes_of(tab).ok_or("that tab has no terminals")?;
        let view = panes.read(cx).active_view().clone();
        Ok((tab, view))
    }

    fn describe_tabs(&self, cx: &App) -> Value {
        let tabs: Vec<Value> = self
            .layout
            .ordered_tabs()
            .into_iter()
            .filter_map(|tab| {
                let display = self.display(tab, cx)?;
                let group = self
                    .layout
                    .group_of(tab)
                    .and_then(|group| self.layout.group(group))
                    .map(|group| group.name.clone());
                let panes: Vec<Value> = self
                    .panes_of(tab)
                    .map(|panes| {
                        let panes = panes.read(cx);
                        let active = panes.active_view().clone();
                        panes
                            .views()
                            .into_iter()
                            .map(|view| {
                                let metadata = view.read(cx).metadata();
                                json!({
                                    "id": view.read(cx).pane_id(),
                                    "title": metadata.title.to_string(),
                                    "cwd": metadata.cwd,
                                    "process": metadata.process.as_ref().map(ToString::to_string),
                                    "running": metadata.running,
                                    "focused": view == active,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(json!({
                    "id": tab.element_id(),
                    "title": display.title.to_string(),
                    "active": self.active == Some(tab),
                    "group": group,
                    "directory": display.directory.as_ref().map(ToString::to_string),
                    "branch": display.branch.as_ref().map(ToString::to_string),
                    "agent": display.agent.map(|state| json!({
                        "name": state.agent.name(),
                        "status": agent_badge::status_label(state.status),
                    })),
                    "panes": panes,
                }))
            })
            .collect();
        json!({ "tabs": tabs })
    }

    fn control_new_tab(
        &mut self,
        new_tab: NewTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        if let Some(agent) = &new_tab.agent {
            let profile = SettingsStore::get(cx)
                .agent_profiles
                .iter()
                .find(|profile| profile.name.eq_ignore_ascii_case(agent))
                .cloned()
                .ok_or_else(|| format!("no agent profile named “{agent}”"))?;
            self.open_agent(&profile, window, cx);
            return Ok(json!({ "tab": self.active.map(|tab| tab.element_id()) }));
        }
        let style = TabStyle {
            name: new_tab.name.clone(),
            ..TabStyle::default()
        };
        let tab = self
            .open_terminal(
                new_tab.cwd.as_deref(),
                new_tab.command.as_deref(),
                None,
                style,
                window,
                cx,
            )
            .ok_or("the terminal could not be started")?;
        if let Some(group_name) = new_tab.group {
            let existing = self
                .layout
                .groups()
                .iter()
                .find(|group| group.name == group_name)
                .map(|group| group.id);
            match existing {
                Some(group) => self.layout.move_tab(tab, TabDestination::IntoGroup(group)),
                None => {
                    self.layout.group_tab(tab, group_name);
                }
            }
            self.layout_changed(cx);
        }
        let pane = self
            .panes_of(tab)
            .map(|panes| panes.read(cx).active_view().read(cx).pane_id());
        Ok(json!({ "tab": tab.element_id(), "pane": pane }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_carried_in_envelopes() {
        let first = new_token().unwrap();
        let second = new_token().unwrap();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        let envelope: Envelope = serde_json::from_str(&format!(
            r#"{{ "id": 1, "token": "{first}", "method": "list_tabs" }}"#
        ))
        .unwrap();
        assert_eq!(envelope.token.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn only_listing_is_free() {
        assert!(!Request::ListTabs.needs_approval());
        assert!(
            Request::ReadOutput {
                target: PaneTarget::default(),
                lines: 1
            }
            .needs_approval()
        );
        assert!(Request::FocusTab { tab: 1 }.needs_approval());
    }

    #[test]
    fn requests_use_method_and_params() {
        let envelope = Envelope {
            id: 7,
            token: None,
            request: Request::SendInput {
                target: PaneTarget {
                    tab: None,
                    pane: Some(3),
                },
                text: "ls\n".into(),
            },
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            json,
            json!({ "id": 7, "method": "send_input", "params": { "target": { "pane": 3 }, "text": "ls\n" } })
        );
        assert_eq!(serde_json::from_value::<Envelope>(json).unwrap(), envelope);
        let list: Envelope = serde_json::from_str(r#"{ "id": 1, "method": "list_tabs" }"#).unwrap();
        assert_eq!(list.request, Request::ListTabs);
    }

    #[test]
    fn socket_round_trip_refuses_nothing_from_the_same_user() {
        let dir = std::env::temp_dir().join(format!("control-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let listener = bind(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let (requests_tx, requests_rx) = async_channel::unbounded::<Message>();
        thread::spawn(move || accept_connections(listener, requests_tx));
        thread::spawn(move || {
            while let Ok(message) = requests_rx.recv_blocking() {
                let Message::Request(request, caller, reply) = message else {
                    continue;
                };
                assert_eq!(caller.peer, Some(std::process::id() as i32));
                let _ = reply.send(match request {
                    Request::ListTabs => Ok(json!({ "tabs": [] })),
                    _ => Err("unsupported".to_string()),
                });
            }
        });

        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .write_all(b"{\"id\":5,\"method\":\"list_tabs\"}\n{\"id\":6,\"method\":\"focus_tab\",\"params\":{\"tab\":1}}\n")
            .unwrap();
        let mut lines = BufReader::new(stream).lines();
        let first: Reply = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(first.result, Some(json!({ "tabs": [] })));
        let second: Reply = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(second.error.as_deref(), Some("unsupported"));

        // A second instance must not steal a live socket.
        assert!(bind(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
