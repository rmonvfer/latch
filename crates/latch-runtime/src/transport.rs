//! Length-prefixed JSON frames over the runtime socket, and the client
//! connection that authenticates both ends before sending requests.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};

use latch_platform::peer::same_user;

use crate::{
    directory::Paths,
    lifecycle::OUTDATED,
    protocol::{Envelope, MAX_MESSAGE_BYTES, Request, Response, VERSION},
    token::read_token,
};

pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether the process on the other end of `stream` runs this same
/// program. Clients check it before presenting the runtime's secret, so a
/// program that swaps in its own socket cannot collect the secret, and the
/// runtime checks it before serving a connection.
///
/// The peer is identified by the audit token the kernel recorded when it
/// connected. Its pid version changes whenever a process execs, so a
/// program cannot connect, pass the socket on, and then exec this binary to
/// pass the check.
pub(crate) fn peer_is_this_program(stream: &UnixStream) -> bool {
    latch_platform::peer::peer_is_this_program(stream)
}

pub(crate) struct Rpc {
    stream: UnixStream,
    token: String,
}

#[derive(Debug)]
pub(crate) struct RemoteError(String);

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RemoteError {}

impl Rpc {
    pub(crate) fn connect(paths: &Paths) -> Result<Self> {
        let stream = UnixStream::connect(paths.socket())?;
        Self::handshake(stream, paths)
    }

    pub(crate) fn handshake(stream: UnixStream, paths: &Paths) -> Result<Self> {
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
        let token = read_token(paths).context(OUTDATED)?;
        let mut client = Self { stream, token };
        match client.request(Request::Ping)? {
            Response::Ready { version } if version == VERSION => Ok(client),
            Response::Ready { version } => {
                bail!("session runtime protocol version {version} is incompatible with {VERSION}")
            }
            _ => bail!("session runtime returned an invalid handshake"),
        }
    }

    #[tracing::instrument(skip_all)]
    pub(crate) fn request(&mut self, request: Request) -> Result<Response> {
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

pub(crate) fn reserve(count: &AtomicUsize, amount: usize, limit: usize) -> Result<()> {
    count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(amount).filter(|total| *total <= limit)
        })
        .map(|_| ())
        .map_err(|_| anyhow!("capacity exceeded"))
}

pub(crate) fn read_length(stream: &mut impl Read) -> Result<usize> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        length > 0 && length <= MAX_MESSAGE_BYTES,
        "runtime message exceeds its size limit"
    );
    Ok(length)
}

pub(crate) fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> Result<T> {
    let length = read_length(stream)?;
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).context("invalid runtime response")
}

pub(crate) fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
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
