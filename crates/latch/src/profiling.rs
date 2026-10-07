//! Performance tracing. With `TERMINAL_TRACE` set, the app and the session
//! runtime each record their tracing spans to the config directory's
//! `traces` folder in Chrome's trace format, which ui.perfetto.dev opens.
//! Without it, spans cost a check of a disabled subscriber.

use std::{
    sync::Mutex,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::prelude::*;

use crate::settings::SettingsStore;

/// Environment variable that turns tracing on; programs started from a
/// traced app (the session runtime) inherit it.
pub const TRACE_VARIABLE: &str = "TERMINAL_TRACE";

/// How often a trace is written out, so a process stopped outright loses
/// at most this much of it.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

static GUARD: Mutex<Option<FlushGuard>> = Mutex::new(None);

/// Start recording a trace for this process, named `process`, if tracing
/// is on.
pub fn init(process: &str) {
    if std::env::var_os(TRACE_VARIABLE).is_none() {
        return;
    }
    let dir = SettingsStore::config_dir().join("traces");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!("cannot record a trace in {}: {error}", dir.display());
        return;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let file = dir.join(format!("{process}-{stamp}.json"));
    let (layer, guard) = ChromeLayerBuilder::new()
        .file(&file)
        .include_args(true)
        .build();
    if tracing_subscriber::registry()
        .with(layer)
        .try_init()
        .is_err()
    {
        return;
    }
    if let Ok(mut slot) = GUARD.lock() {
        *slot = Some(guard);
    }
    log::warn!("recording a performance trace to {}", file.display());
    let _ = thread::Builder::new().name("trace-flush".into()).spawn(|| {
        loop {
            thread::sleep(FLUSH_INTERVAL);
            flush();
        }
    });
}

/// Write out what the trace has recorded so far.
pub fn flush() {
    if let Ok(slot) = GUARD.lock()
        && let Some(guard) = slot.as_ref()
    {
        guard.flush();
    }
}
