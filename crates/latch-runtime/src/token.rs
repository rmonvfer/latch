//! The secret a client presents to the runtime, generated per runtime and
//! readable only by its user.

use std::{
    fs::{self, File},
    io::{Read, Write},
};

use anyhow::{Result, ensure};

use crate::directory::{Paths, private_file};

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
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

pub(crate) fn read_token(paths: &Paths) -> Result<String> {
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
pub(crate) fn write_token(paths: &Paths) -> Result<String> {
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

/// Remove secrets left by runtimes that stopped without removing theirs.
/// Called before this runtime writes its own, under the runtime lock.
pub(crate) fn forget_stale_secrets(paths: &Paths) {
    let Ok(entries) = fs::read_dir(&paths.directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("secret-") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Remove the running runtime's secret.
pub(crate) fn forget_token(paths: &Paths) {
    if let Ok(id) = read_token_id(paths) {
        secret_store::delete(paths, &id);
    }
    let _ = fs::remove_file(paths.token_id());
}

/// The runtime's secret lives in an owner-only file in the owner-only
/// runtime directory. On its own a file readable by the user's programs
/// would not keep other programs out, but both ends of the socket also
/// check that the other runs this same executable, so the secret is of no
/// use to any other program that reads it.
mod secret_store {
    use std::{
        fs,
        io::{Read, Write},
        path::PathBuf,
    };

    use anyhow::Result;

    use crate::directory::{Paths, private_file};

    fn path(paths: &Paths, id: &str) -> PathBuf {
        paths.directory.join(format!("secret-{id}"))
    }

    pub fn store(paths: &Paths, id: &str, token: &str) -> Result<()> {
        let mut file = private_file(&path(paths, id), true)?;
        file.set_len(0)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn load(paths: &Paths, id: &str) -> Result<String> {
        let mut token = String::new();
        private_file(&path(paths, id), false)?
            .take(65)
            .read_to_string(&mut token)?;
        Ok(token)
    }

    pub fn delete(paths: &Paths, id: &str) {
        let _ = fs::remove_file(path(paths, id));
    }
}

pub(crate) fn same_token(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    expected
        .bytes()
        .zip(candidate.bytes())
        .fold(0, |difference, (a, b)| difference | (a ^ b))
        == 0
}
