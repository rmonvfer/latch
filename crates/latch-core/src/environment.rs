//! Variables every pane's shell gets, naming the pane and authorizing the
//! control requests made from it.

use std::fs;

use anyhow::{Context as _, Result};

/// Environment variable telling programs which session they run in.
pub const PANE_ID_VARIABLE: &str = "TERMINAL_PANE_ID";

/// Environment variable holding a pane's control token.
pub const TOKEN_VARIABLE: &str = "TERMINAL_CONTROL_TOKEN";

/// A random 128-bit secret, hex encoded.
pub fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut source| std::io::Read::read_exact(&mut source, &mut bytes))
        .context("failed to read random bytes")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
