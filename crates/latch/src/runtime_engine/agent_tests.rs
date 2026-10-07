use std::collections::BTreeMap;

use super::*;
use crate::agent_resume::ResumableAgent;

fn launch() -> Launch {
    Launch {
        id: 71,
        argv: vec!["/bin/sh".into(), "-c".into(), "read line".into()],
        env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        cwd: Some(std::env::temp_dir()),
        startup: None,
        agent_session: None,
        dimensions: Dimensions {
            cols: 60,
            rows: 8,
            cell_width: 8,
            cell_height: 16,
        },
        colors: Colors {
            foreground: [230, 230, 230],
            background: [20, 20, 20],
            cursor: [255, 255, 255],
            palette: [[100, 100, 100]; 16],
            dark: true,
        },
        control_token: "test-agent-capability".into(),
        command_blocks: false,
    }
}

fn engine() -> Engine {
    Engine::new(71, launch(), Arc::new(RwLock::new(SessionInfo::default()))).unwrap()
}

fn session(agent: ResumableAgent, id: &str) -> AgentSession {
    AgentSession::new(agent, id).unwrap()
}

fn event(engine: &mut Engine, agent: ResumableAgent, event: AgentEvent) {
    engine
        .handle(Request::ReportAgent {
            token: "test-agent-capability".into(),
            report: AgentReport::Event { agent, event },
        })
        .unwrap();
}

fn snapshot(engine: &mut Engine) -> Snapshot {
    let snapshot = engine.capture().unwrap();
    let serialized = serde_json::to_vec(&snapshot).unwrap();
    let decoded: Snapshot = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(decoded.info, snapshot.info);
    assert_eq!(*engine.shared.read().unwrap(), snapshot.info);
    decoded
}

#[test]
fn hook_turns_survive_without_a_gui_and_repeated_stops_do_not_notify() {
    let mut engine = engine();
    let agent = ResumableAgent::Claude;
    let session = session(agent, "claude-session");
    engine
        .report_agent(AgentReport::Session {
            session: session.clone(),
        })
        .unwrap();
    event(
        &mut engine,
        agent,
        AgentEvent::PromptSubmitted {
            prompt: "fix\nthe build".into(),
        },
    );
    let working = snapshot(&mut engine);
    assert_eq!(working.info.agent_session, Some(session));
    assert_eq!(working.info.status, "working");
    assert_eq!(
        working.info.agent_turn,
        Some((
            agent,
            TurnState {
                turn: Some(AgentTurn::InProgress),
                prompt: Some("fix the build".into()),
                message: None,
            }
        ))
    );
    assert_eq!(working.info.attention_serial, 0);

    event(
        &mut engine,
        agent,
        AgentEvent::Blocked {
            message: Some("Wants\nto use Bash".into()),
        },
    );
    let blocked = snapshot(&mut engine);
    assert_eq!(blocked.info.status, "needs_input");
    assert_eq!(blocked.info.attention_serial, 1);
    assert_eq!(
        blocked.info.attention,
        Some(Attention::AgentTurn {
            agent,
            turn: AgentTurn::Blocked,
            prompt: Some("fix the build".into()),
            message: Some("Wants to use Bash".into()),
        })
    );

    event(&mut engine, agent, AgentEvent::ToolCompleted);
    let working = snapshot(&mut engine);
    assert_eq!(working.info.status, "working");
    assert_eq!(working.info.attention_serial, 1);
    assert_eq!(working.info.attention, None);
    let (_, state) = working.info.agent_turn.unwrap();
    assert_eq!(state.turn, Some(AgentTurn::InProgress));
    assert_eq!(state.message, None);

    event(&mut engine, agent, AgentEvent::Stopped);
    let done = snapshot(&mut engine);
    assert_eq!(done.info.status, "idle");
    assert_eq!(done.info.attention_serial, 2);
    assert_eq!(
        done.info.attention,
        Some(Attention::AgentTurn {
            agent,
            turn: AgentTurn::Done,
            prompt: Some("fix the build".into()),
            message: None,
        })
    );
    event(&mut engine, agent, AgentEvent::Stopped);
    let duplicate = snapshot(&mut engine);
    assert_eq!(duplicate.info, done.info);
    assert_eq!(duplicate.revision, done.revision);
}

#[test]
fn replacing_an_agent_session_discards_its_turn_and_attention() {
    let mut engine = engine();
    let agent = ResumableAgent::Codex;
    let original = session(agent, "original-session");
    engine
        .report_agent(AgentReport::Session {
            session: original.clone(),
        })
        .unwrap();
    event(&mut engine, agent, AgentEvent::Blocked { message: None });
    engine
        .report_agent(AgentReport::Session { session: original })
        .unwrap();
    let same = snapshot(&mut engine);
    assert!(same.info.agent_turn.is_some());
    assert!(same.info.attention.is_some());

    let replacement = session(agent, "replacement-session");
    engine
        .report_agent(AgentReport::Session {
            session: replacement.clone(),
        })
        .unwrap();
    let replaced = snapshot(&mut engine);
    assert_eq!(replaced.info.agent_session, Some(replacement));
    assert_eq!(replaced.info.agent_turn, None);
    assert_eq!(replaced.info.attention, None);
    assert_eq!(replaced.info.attention_serial, same.info.attention_serial);
    assert_eq!(replaced.info.status, "idle");
}

#[test]
fn changing_the_running_agent_discards_reports_for_the_previous_agent() {
    let mut engine = engine();
    let agent = ResumableAgent::Claude;
    engine
        .report_agent(AgentReport::Session {
            session: session(agent, "claude-session"),
        })
        .unwrap();
    event(&mut engine, agent, AgentEvent::Blocked { message: None });
    engine.clear_agent_reports(Some(Agent::ClaudeCode));
    let retained = snapshot(&mut engine);
    assert!(retained.info.agent_session.is_some());
    assert!(retained.info.agent_turn.is_some());

    engine.clear_agent_reports(Some(Agent::Codex));
    let changed = snapshot(&mut engine);
    assert_eq!(changed.info.agent_session, None);
    assert_eq!(changed.info.agent_turn, None);
    assert_eq!(changed.info.attention, None);
    assert_eq!(
        changed.info.attention_serial,
        retained.info.attention_serial
    );
}

#[test]
fn exiting_a_session_discards_reports_and_rejects_hook_events() {
    let mut engine = engine();
    let agent = ResumableAgent::Codex;
    engine
        .report_agent(AgentReport::Session {
            session: session(agent, "codex-session"),
        })
        .unwrap();
    event(&mut engine, agent, AgentEvent::Blocked { message: None });
    engine.stop().unwrap();
    let exited = snapshot(&mut engine);
    assert!(exited.info.exited);
    assert_eq!(exited.info.status, "exited");
    assert_eq!(exited.info.agent_session, None);
    assert_eq!(exited.info.agent_turn, None);
    assert_eq!(exited.info.attention, None);
    assert!(
        engine
            .report_agent(AgentReport::Event {
                agent,
                event: AgentEvent::Stopped,
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut engine).info, exited.info);
}

#[test]
fn startup_keeps_the_resume_identity_in_the_first_snapshot() {
    let saved = session(ResumableAgent::Codex, "saved-session");
    let mut launch = launch();
    launch.agent_session = Some(saved.clone());
    let mut resumed =
        Engine::new(71, launch, Arc::new(RwLock::new(SessionInfo::default()))).unwrap();
    assert_eq!(snapshot(&mut resumed).info.agent_session, Some(saved));
    resumed.stop().unwrap();
}

#[test]
fn fresh_metadata_clears_a_short_lived_agent_but_stale_metadata_does_not() {
    let mut engine = engine();
    let stale = Instant::now();
    let agent = ResumableAgent::Claude;
    engine
        .report_agent(AgentReport::Session {
            session: session(agent, "short-lived-session"),
        })
        .unwrap();
    event(&mut engine, agent, AgentEvent::Blocked { message: None });
    let (send, receive) = mpsc::sync_channel(1);
    engine.metadata_results = receive;
    let foreground = engine.pty.foreground_pid();
    send.send(Metadata {
        requested_at: stale,
        foreground,
        process: None,
        cwd: None,
        agent: None,
    })
    .unwrap();
    engine.update_metadata();
    let retained = snapshot(&mut engine);
    assert_eq!(retained.info.agent.as_deref(), Some("Claude Code"));
    assert!(retained.info.agent_session.is_some());
    assert!(retained.info.agent_turn.is_some());
    send.send(Metadata {
        requested_at: Instant::now(),
        foreground,
        process: None,
        cwd: None,
        agent: None,
    })
    .unwrap();
    engine.update_metadata();
    let cleared = snapshot(&mut engine);
    assert!(!cleared.info.exited);
    assert_eq!(cleared.info.agent_session, None);
    assert_eq!(cleared.info.agent_turn, None);
    assert_eq!(cleared.info.attention, None);
    engine.stop().unwrap();
}

#[test]
fn switching_agents_without_a_session_start_discards_the_resume_identity() {
    let mut engine = engine();
    engine
        .report_agent(AgentReport::Session {
            session: session(ResumableAgent::Claude, "claude-session"),
        })
        .unwrap();
    event(
        &mut engine,
        ResumableAgent::Codex,
        AgentEvent::PromptSubmitted {
            prompt: "fix the build".into(),
        },
    );
    let current = snapshot(&mut engine);
    assert_eq!(current.info.agent.as_deref(), Some("Codex"));
    assert_eq!(current.info.agent_session, None);
    assert_eq!(current.info.agent_turn.unwrap().0, ResumableAgent::Codex);
    engine.stop().unwrap();
}
