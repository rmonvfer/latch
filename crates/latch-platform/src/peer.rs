//! Who is on the other end of a local socket.

use std::{
    fs,
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use crate::process;

/// The signing identity accepted for distributed Latch executables.
pub const RELEASE_REQUIREMENT: &str = "anchor apple generic and identifier \"me.egrati.terminal\" and certificate leaf[subject.OU] = \"888FLB45R8\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";

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
    peer_token(stream).map(|token| (token[5] as i32, token[7] as i32))
}

fn peer_token(stream: &UnixStream) -> Option<[u32; 8]> {
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
    (result == 0 && size as usize == std::mem::size_of_val(&token)).then_some(token)
}

/// Authenticate a socket peer against the running application's identity.
/// Signed releases accept other versions with the same Developer ID identity;
/// development builds require the same canonical executable path.
pub fn peer_is_this_program(stream: &UnixStream) -> bool {
    let Some(token) = peer_token(stream) else {
        return false;
    };
    let pid = token[5] as i32;
    let version = token[7] as i32;
    let unchanged = || process::pid_version(pid) == Some(version);
    if !unchanged() {
        return false;
    }
    let authenticated = match signing::own_identity() {
        signing::Identity::Release => signing::trusted_peer(&token),
        signing::Identity::Development => process::executable_path(pid)
            .and_then(|path| fs::canonicalize(path).ok())
            .zip(std::env::current_exe().and_then(fs::canonicalize).ok())
            .is_some_and(|(theirs, ours)| theirs == ours),
        signing::Identity::Untrusted => false,
    };
    authenticated && unchanged()
}

/// Whether the running executable is a verified Developer ID release.
pub fn is_signed_release() -> bool {
    matches!(signing::own_identity(), signing::Identity::Release)
}

#[cfg(target_os = "macos")]
mod signing {
    use std::{
        ffi::{CString, c_void},
        ptr,
    };

    type Ref = *const c_void;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecGuestAttributeAudit: Ref;
        static kSecGuestAttributePid: Ref;
        static kSecGuestAttributeDynamicCode: Ref;
        static kSecCodeInfoFlags: Ref;
        fn SecCodeCopyGuestWithAttributes(
            host: Ref,
            attributes: Ref,
            flags: u32,
            guest: *mut Ref,
        ) -> i32;
        fn SecCodeCopySigningInformation(code: Ref, flags: u32, info: *mut Ref) -> i32;
        fn SecRequirementCreateWithString(text: Ref, flags: u32, requirement: *mut Ref) -> i32;
        fn SecCodeCheckValidity(code: Ref, flags: u32, requirement: Ref) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: Ref;
        fn CFRelease(value: Ref);
        fn CFDataCreate(allocator: Ref, bytes: *const u8, length: isize) -> Ref;
        fn CFNumberCreate(allocator: Ref, kind: isize, value: *const c_void) -> Ref;
        fn CFNumberGetValue(number: Ref, kind: isize, value: *mut c_void) -> bool;
        fn CFStringCreateWithCString(allocator: Ref, text: *const i8, encoding: u32) -> Ref;
        fn CFDictionaryCreate(
            allocator: Ref,
            keys: *const Ref,
            values: *const Ref,
            count: isize,
            key_callbacks: Ref,
            value_callbacks: Ref,
        ) -> Ref;
        fn CFDictionaryGetValue(dictionary: Ref, key: Ref) -> Ref;
    }

    struct Owned(Ref);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: every Owned value comes from a successful Create/Copy call.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    pub(super) enum Identity {
        Release,
        Development,
        Untrusted,
    }

    fn guest(key: Ref, value: Ref) -> Option<Owned> {
        if value.is_null() {
            return None;
        }
        // SAFETY: the two keys and values remain alive until the synchronous
        // Security call finishes; null callbacks leave their ownership unchanged.
        unsafe {
            let keys = [key, kSecGuestAttributeDynamicCode];
            let values = [value, kCFBooleanTrue];
            let attributes = Owned(CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                2,
                ptr::null(),
                ptr::null(),
            ));
            if attributes.0.is_null() {
                return None;
            }
            let mut code = ptr::null();
            (SecCodeCopyGuestWithAttributes(ptr::null(), attributes.0, 0, &mut code) == 0)
                .then_some(Owned(code))
        }
    }

    fn trusted(code: &Owned) -> bool {
        let text = CString::new(super::RELEASE_REQUIREMENT).expect("code requirement has no NUL");
        // SAFETY: all arguments are live CF objects and valid out-pointers.
        unsafe {
            let text = Owned(CFStringCreateWithCString(
                ptr::null(),
                text.as_ptr(),
                0x08000100,
            ));
            if text.0.is_null() {
                return false;
            }
            let mut requirement = ptr::null();
            if SecRequirementCreateWithString(text.0, 0, &mut requirement) != 0 {
                return false;
            }
            let requirement = Owned(requirement);
            SecCodeCheckValidity(code.0, 0, requirement.0) == 0
        }
    }

    pub(super) fn own_identity() -> Identity {
        let pid = std::process::id() as i32;
        // SAFETY: kCFNumberSInt32Type reads a live i32; all CF objects are retained.
        unsafe {
            let number = Owned(CFNumberCreate(ptr::null(), 3, (&pid as *const i32).cast()));
            let Some(code) = guest(kSecGuestAttributePid, number.0) else {
                return Identity::Untrusted;
            };
            let mut info = ptr::null();
            let status = SecCodeCopySigningInformation(code.0, 0, &mut info);
            if status == -67062 {
                return Identity::Development;
            } // errSecCSUnsigned
            if status != 0 {
                return Identity::Untrusted;
            }
            let info = Owned(info);
            let flags = CFDictionaryGetValue(info.0, kSecCodeInfoFlags);
            let mut value = 0i32;
            if flags.is_null() || !CFNumberGetValue(flags, 3, (&mut value as *mut i32).cast()) {
                return Identity::Untrusted;
            }
            if value & 2 != 0 {
                return Identity::Development;
            } // kSecCodeSignatureAdhoc
            if trusted(&code) {
                Identity::Release
            } else {
                Identity::Untrusted
            }
        }
    }

    pub(super) fn trusted_peer(token: &[u32; 8]) -> bool {
        // SAFETY: audit_token_t is exactly eight u32 words; CFDataCreate copies it.
        unsafe {
            let data = Owned(CFDataCreate(
                ptr::null(),
                token.as_ptr().cast(),
                std::mem::size_of_val(token) as isize,
            ));
            guest(kSecGuestAttributeAudit, data.0).is_some_and(|code| trusted(&code))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn authenticates_same_executable() {
        let (client, server) = UnixStream::pair().unwrap();
        assert!(peer_is_this_program(&client));
        assert!(peer_is_this_program(&server));
    }

    #[test]
    #[ignore = "subprocess fixture for signed_peer_identity"]
    fn signed_peer_child() {
        let path = std::env::var_os("LATCH_PEER_TEST_SOCKET").unwrap();
        let listener = UnixListener::bind(path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let identity = match signing::own_identity() {
            signing::Identity::Release => 1,
            signing::Identity::Development => 2,
            signing::Identity::Untrusted => 3,
        };
        stream.write_all(&[identity]).unwrap();
        let mut stop = [0];
        stream.read_exact(&mut stop).unwrap();
    }

    #[test]
    #[ignore = "requires a Developer ID identity in LATCH_TEST_SIGNING_IDENTITY"]
    fn signed_peer_identity() {
        let identity = std::env::var("LATCH_TEST_SIGNING_IDENTITY").unwrap();
        let directory =
            std::env::temp_dir().join(format!("latch-signed-peer-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        for (index, signer, identifier, expected) in [
            (0, identity.as_str(), "me.egrati.terminal", true),
            (1, identity.as_str(), "me.egrati.other", false),
            (2, "-", "me.egrati.terminal", false),
        ] {
            let executable = directory.join(format!("peer-{index}"));
            fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            assert!(
                Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", signer, "--identifier", identifier])
                    .arg(&executable)
                    .status()
                    .unwrap()
                    .success()
            );
            let socket = directory.join(format!("socket-{index}"));
            let mut child = Command::new(&executable)
                .args([
                    "--exact",
                    "peer::tests::signed_peer_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("LATCH_PEER_TEST_SOCKET", &socket)
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let started = Instant::now();
            let mut stream = loop {
                if let Ok(stream) = UnixStream::connect(&socket) {
                    break stream;
                }
                assert!(child.try_wait().unwrap().is_none(), "child exited");
                assert!(started.elapsed() < Duration::from_secs(10));
                thread::sleep(Duration::from_millis(10));
            };
            let mut own_identity = [0];
            stream.read_exact(&mut own_identity).unwrap();
            assert_eq!(
                own_identity[0],
                if expected {
                    1
                } else if signer == "-" {
                    2
                } else {
                    3
                }
            );
            let token = peer_token(&stream).unwrap();
            assert_eq!(signing::trusted_peer(&token), expected);
            let moved = directory.join(format!("moved-{index}"));
            fs::rename(&executable, &moved).unwrap();
            assert_eq!(signing::trusted_peer(&token), expected, "moved executable");
            fs::remove_file(&moved).unwrap();
            assert!(
                !signing::trusted_peer(&token),
                "unlinked code must fail closed"
            );
            stream.write_all(&[0]).unwrap();
            assert!(child.wait().unwrap().success());
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
