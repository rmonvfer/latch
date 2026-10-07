//! Requests a client makes of the running runtime about its sessions.

use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};

use crate::{
    directory::Paths,
    lifecycle::ensure_running,
    protocol::{AgentReport, Launch, Operation, Request, Response, SessionInfo},
    token::random_bytes,
    transport::Rpc,
};

/// Deliver a hook report without starting or replacing a runtime.
pub fn report_agent(token: String, report: AgentReport) -> Result<()> {
    let mut client = Rpc::connect(&Paths::configured())?;
    match client.request(Request::ReportAgent { token, report })? {
        Response::Done => Ok(()),
        _ => bail!("session runtime returned an invalid hook response"),
    }
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

pub(crate) fn read_session_at(paths: &Paths, id: u64, lines: usize) -> Result<String> {
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
