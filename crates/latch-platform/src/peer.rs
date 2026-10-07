//! Who is on the other end of a local socket.

use std::os::{fd::AsRawFd, unix::net::UnixStream};

/// Whether the process on the other end of `stream` runs as this user.
pub fn same_user(stream: &UnixStream) -> bool {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the descriptor is a live socket and both out-pointers are valid.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    // SAFETY: geteuid has no preconditions.
    result == 0 && uid == unsafe { libc::geteuid() }
}

/// The process on the other end of a local socket.
pub fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let mut size = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the descriptor is a live socket and `pid`/`size` describe a
    // valid buffer for LOCAL_PEERPID.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        )
    };
    (result == 0 && pid > 0).then_some(pid)
}

/// The pid and pid version of the process that connected `stream`, from
/// the audit token the kernel recorded at connection.
pub fn peer_audit(stream: &UnixStream) -> Option<(i32, i32)> {
    // audit_token_t: eight words, the pid at index 5 and its version at 7.
    let mut token = [0u32; 8];
    let mut size = std::mem::size_of_val(&token) as libc::socklen_t;
    // SAFETY: the descriptor is a live socket and `token`/`size` describe a
    // buffer of the size LOCAL_PEERTOKEN writes.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            token.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (result == 0 && size as usize == std::mem::size_of_val(&token))
        .then(|| (token[5] as i32, token[7] as i32))
}
