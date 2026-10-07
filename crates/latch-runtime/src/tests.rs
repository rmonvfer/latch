use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    client::{CLIENT_QUEUE_BYTES, ClientEvent, SessionClient},
    directory::{Paths, private_file},
    lifecycle::{STARTUP_TIMEOUT, configure_file_limit},
    protocol::{
        AgentReport, Colors, Dimensions, Envelope, Launch, MAX_MESSAGE_BYTES, Operation, Request,
        Response, SessionInfo, VERSION,
    },
    server::Server,
    sessions::{new_session_id, read_session_at},
    token::{read_token, write_token},
    transport::{IO_TIMEOUT, Rpc, peer_is_this_program, read_frame, write_frame},
};

/// Block rows as the runtime encodes them, for views that do not paint.
fn text_rows(rows: &[String], _columns: u16, _background: [u8; 3]) -> Result<Vec<String>> {
    Ok(rows.to_vec())
}

const TEST_DIRECTORY: &str = "TERMINAL_RUNTIME_TEST_DIRECTORY";
const TEST_SESSION: &str = "TERMINAL_RUNTIME_TEST_SESSION";

struct TestRuntime {
    paths: Paths,
    child: Child,
}

impl TestRuntime {
    fn start() -> Self {
        Self::start_with_file_limit(None)
    }

    fn start_with_file_limit(file_limit: Option<libc::rlim_t>) -> Self {
        let paths = temporary_paths();
        paths.prepare().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::daemon_process",
                "--ignored",
                "--nocapture",
            ])
            .env(TEST_DIRECTORY, &paths.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                if let Some(file_limit) = file_limit {
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    limit.rlim_cur = file_limit.min(limit.rlim_max);
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
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
        agent_session: None,
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
    configure_file_limit().unwrap();
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
    let (_client, events) = SessionClient::attach_at(id, paths.clone(), text_rows).unwrap();
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
#[ignore = "agent hook subprocess entry point"]
fn agent_hook_process() {
    let Some(directory) = std::env::var_os(TEST_DIRECTORY) else {
        return;
    };
    let paths = Paths {
        directory: PathBuf::from(directory),
    };
    let token = std::env::var(latch_core::environment::TOKEN_VARIABLE).unwrap();
    let agent = latch_core::agents::resume::ResumableAgent::Codex;
    let mut client = Rpc::connect(&paths).unwrap();
    let report = AgentReport::Session {
        session: latch_core::agents::resume::AgentSession::new(agent, "detached-agent-session")
            .unwrap(),
    };
    assert!(
        client
            .request(Request::ReportAgent {
                token: "incorrect-pane-token".into(),
                report: report.clone(),
            })
            .unwrap_err()
            .to_string()
            .contains("no session has that token")
    );
    for report in [
        report,
        AgentReport::Event {
            agent,
            event: latch_core::agents::events::AgentEvent::PromptSubmitted {
                prompt: "fix the build".into(),
            },
        },
        AgentReport::Event {
            agent,
            event: latch_core::agents::events::AgentEvent::Blocked {
                message: Some("Wants to use Bash".into()),
            },
        },
    ] {
        assert!(matches!(
            client
                .request(Request::ReportAgent {
                    token: token.clone(),
                    report
                })
                .unwrap(),
            Response::Done
        ));
    }
}

#[test]
fn agent_hooks_update_detached_sessions_and_restore_on_reattach() {
    let runtime = TestRuntime::start();
    // A named shell supplies an agent ancestor for the hook subprocess.
    let agent = runtime.paths.directory.join("codex");
    fs::copy("/bin/zsh", &agent).unwrap();
    let mut command = launch("");
    command.argv = vec!["/bin/zsh".into(), "-f".into(), "-i".into()];
    command.env.insert(
        "HOOK_PROGRAM".into(),
        std::env::current_exe().unwrap().to_str().unwrap().into(),
    );
    command
        .env
        .insert("TEST_AGENT".into(), agent.to_str().unwrap().into());
    command.env.insert(
        TEST_DIRECTORY.into(),
        runtime.paths.directory.to_str().unwrap().into(),
    );
    let id = command.id;
    runtime.rpc().request(Request::Create(command)).unwrap();
    let (view, frames) = SessionClient::attach_at(id, runtime.paths.clone(), text_rows).unwrap();
    wait_until(|| matches!(frames.try_recv(), Ok(ClientEvent::Frame(_))));
    drop(view);
    drop(frames);
    runtime.rpc().request(Request::Operate {
        session: id,
        operation: Operation::Input {
            bytes: concat!(r#""$TEST_AGENT" -c '"$HOOK_PROGRAM" --exact tests::agent_hook_process --ignored --nocapture; IFS= read -r line'"#, "\n").as_bytes().to_vec(),
        },
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sessions = runtime.list();
        if sessions.iter().any(|info| {
            info.id == id && info.agent_session.is_some() && info.status == "needs_input"
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hook timed out: {sessions:?}; output: {}",
            read_session_at(&runtime.paths, id, 100).unwrap()
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Keep the agent alive across metadata polls with no attached view.
    thread::sleep(METADATA_TEST_INTERVAL);
    let (view, frames) = SessionClient::attach_at(id, runtime.paths.clone(), text_rows).unwrap();
    let mut restored = None;
    wait_until(|| {
        if let Ok(ClientEvent::Frame(frame)) = frames.try_recv() {
            restored = Some(frame.info);
            true
        } else {
            false
        }
    });
    let restored = restored.unwrap();
    assert_eq!(
        restored
            .agent_session
            .as_ref()
            .map(|session| session.session_id.as_str()),
        Some("detached-agent-session"),
        "{restored:?}"
    );
    assert_eq!(restored.agent.as_deref(), Some("Codex"));
    assert_eq!(restored.status, "needs_input");
    assert_eq!(
        restored.agent_turn.unwrap().1.turn,
        Some(latch_core::agents::events::AgentTurn::Blocked)
    );
    assert!(matches!(
        restored.attention,
        Some(crate::protocol::Attention::AgentTurn { .. })
    ));
    drop(view);
    drop(frames);
}

const METADATA_TEST_INTERVAL: Duration = Duration::from_millis(600);

#[test]
fn reports_from_unrelated_processes_do_not_change_the_session() {
    let runtime = TestRuntime::start();
    let session = runtime.create("IFS= read -r line");
    let mut client = runtime.rpc();
    let error = client
        .request(Request::ReportAgent {
            token: session.control_token,
            report: AgentReport::Event {
                agent: latch_core::agents::resume::ResumableAgent::Claude,
                event: latch_core::agents::events::AgentEvent::Blocked { message: None },
            },
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not a report from the agent's own hook")
    );
    let Response::Sessions(sessions) = client.request(Request::List).unwrap() else {
        panic!("expected session list");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_session, None);
    assert_eq!(sessions[0].agent_turn, None);
    assert_eq!(sessions[0].attention_serial, 0);
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
fn runtime_supports_many_sessions_with_a_low_inherited_file_limit() {
    let runtime = TestRuntime::start_with_file_limit(Some(256));
    let sessions: Vec<_> = (0..96)
        .map(|_| {
            runtime.create("printf 'READY\\n'; IFS= read -r line; printf 'INPUT:%s\\n' \"$line\"")
        })
        .collect();
    let _views: Vec<_> = sessions
        .iter()
        .map(|session| {
            SessionClient::attach_at(session.id, runtime.paths.clone(), text_rows).unwrap()
        })
        .collect();
    assert_eq!(runtime.list().len(), sessions.len());
    assert!(runtime.list().iter().all(|session| !session.exited));
    let id = sessions.last().unwrap().id;
    let mut client = runtime.rpc();
    assert!(matches!(
        client
            .request(Request::Operate {
                session: id,
                operation: Operation::Input {
                    bytes: b"available\n".to_vec(),
                },
            })
            .unwrap(),
        Response::Done
    ));
    wait_until(|| {
        read_session_at(&runtime.paths, id, 100)
            .unwrap()
            .contains("INPUT:available")
    });
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
    let (client, events) =
        SessionClient::attach_at(info.id, runtime.paths.clone(), text_rows).unwrap();
    wait_until(|| matches!(events.try_recv(), Ok(ClientEvent::Frame(_))));
    drop(client);
    drop(events);
    let mut crashed_view = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::view_process", "--ignored", "--nocapture"])
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
    let (client, events) =
        SessionClient::attach_at(info.id, runtime.paths.clone(), text_rows).unwrap();
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
        SessionClient::attach_at(noisy.id, runtime.paths.clone(), text_rows).unwrap();
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
    let (client, events) = SessionClient::attach_at(id, runtime.paths.clone(), text_rows).unwrap();
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
    let (client, events) =
        SessionClient::attach_at(session.id, runtime.paths.clone(), text_rows).unwrap();
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
