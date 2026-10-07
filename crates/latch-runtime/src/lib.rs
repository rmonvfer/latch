//! Private local transport for sessions that outlive their attached windows.

mod client;
mod directory;
pub mod display;
mod engine;
mod lifecycle;
pub mod protocol;
pub mod search;
mod server;
mod sessions;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;
mod token;
mod transport;

pub use client::{ClientEvent, SessionClient};
pub use lifecycle::{ensure_running, is_outdated, replace_outdated, serve};
pub use sessions::{
    create_session, list_sessions, new_session_id, read_session, remove_session, report_agent,
    stop_session,
};
