use std::{
    fs::{self, File, TryLockError},
    io::ErrorKind,
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::Path,
};

use anyhow::{Context as _, Result};

/// Hold one GUI's ownership of a profile until the application exits.
pub fn acquire(directory: &Path) -> Result<Option<File>> {
    fs::create_dir_all(directory).context("cannot create application profile")?;
    let socket = directory.join("control.sock");
    if serving(&socket)? {
        return Ok(None);
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("gui.lock"))
        .context("cannot open application profile lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(error)) => {
            return Err(error).context("cannot lock application profile");
        }
    }
    if serving(&socket)? {
        return Ok(None);
    }
    Ok(Some(lock))
}

fn serving(socket: &Path) -> Result<bool> {
    match UnixStream::connect(socket) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("cannot inspect application control socket"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn profile_has_one_owner_and_unlocks_without_losing_saved_state() {
        let directory = std::env::temp_dir().join(format!("latch-gui-lock-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let session = directory.join("session.json");
        fs::write(&session, "retained layout").unwrap();
        let owner = acquire(&directory).unwrap().unwrap();
        assert!(acquire(&directory).unwrap().is_none());
        let other_profile = directory.join("other-profile");
        let other_owner = acquire(&other_profile).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&session).unwrap(), "retained layout");
        drop(owner);
        let replacement = acquire(&directory).unwrap().unwrap();
        drop(replacement);
        drop(other_owner);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_serving_control_socket_prevents_profile_writes() {
        let directory =
            std::env::temp_dir().join(format!("latch-gui-socket-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(acquire(&directory).unwrap().is_none());
        assert!(!directory.join("gui.lock").exists());
        drop(listener);
        let owner = acquire(&directory).unwrap().unwrap();
        drop(owner);
        fs::remove_dir_all(directory).unwrap();
    }
}
