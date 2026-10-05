//! The control API: a Unix socket through which local programs (the CLI,
//! the MCP server, scripts, agents) list, open, drive, and read tabs.
//!
//! Requests and replies are single JSON lines. The socket is created
//! owner-only and connections from other users are refused, since sending
//! input to a shell amounts to running commands.

use std::{
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
    sync::mpsc,
    thread,
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, AsyncApp, Context, Entity, Window, WindowHandle};
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agent_badge, notifications,
    pane_tree::Axis,
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

type Pending = (Request, mpsc::Sender<Result<Value, String>>);

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

    let (requests_tx, requests_rx) = async_channel::unbounded::<Pending>();
    if let Err(error) = thread::Builder::new()
        .name("control-accept".into())
        .spawn(move || accept_connections(listener, requests_tx))
    {
        log::warn!("control API unavailable: {error}");
        return;
    }

    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok((request, reply)) = requests_rx.recv().await {
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

fn accept_connections(listener: UnixListener, requests: async_channel::Sender<Pending>) {
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

fn serve_connection(stream: UnixStream, requests: async_channel::Sender<Pending>) {
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
                    .send_blocking((envelope.request, reply_tx))
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

/// Send one request to a running app and wait for its reply.
pub fn call(request: Request) -> Result<Value> {
    let path = std::env::var_os(SOCKET_VARIABLE)
        .map(PathBuf::from)
        .unwrap_or_else(socket_path);
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "cannot reach the terminal app at {} — is it running?",
            path.display()
        )
    })?;
    let mut line = serde_json::to_string(&Envelope { id: 1, request })?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let reply: Reply = serde_json::from_str(&response).context("invalid reply from the app")?;
    match (reply.result, reply.error) {
        (_, Some(error)) => bail!(error),
        (Some(result), None) => Ok(result),
        (None, None) => Ok(Value::Null),
    }
}

impl Workspace {
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
    fn requests_use_method_and_params() {
        let envelope = Envelope {
            id: 7,
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

        let (requests_tx, requests_rx) = async_channel::unbounded::<Pending>();
        thread::spawn(move || accept_connections(listener, requests_tx));
        thread::spawn(move || {
            while let Ok((request, reply)) = requests_rx.recv_blocking() {
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
