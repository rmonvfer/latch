//! What the system reports about running processes.

#[cfg(not(target_os = "macos"))]
use std::fs;
use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

use anyhow::Result;

/// The short name of a running process (e.g. "zsh", "nvim").
#[cfg(target_os = "macos")]
pub fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for `buffer.len()` bytes and proc_name
    // writes at most that many.
    let len = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buffer[..len as usize]).into_owned())
}

/// The executable path and arguments of a running process.
#[cfg(target_os = "macos")]
pub fn process_args(pid: i32) -> Option<Vec<String>> {
    let mut size: libc::size_t = 0;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: a null buffer asks sysctl for the required size only.
    let sized = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if sized != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: `buffer` holds `size` writable bytes, as sysctl was told.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs(&buffer)
}

#[cfg(not(target_os = "macos"))]
pub fn process_args(pid: i32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// Decode `KERN_PROCARGS2`: argc, the executable path, padding, then the
/// arguments, all NUL-separated. Returns the path followed by argv.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?) as usize;
    let mut parts = buffer[4..]
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned());
    let executable = parts.next()?;
    let mut args = vec![executable];
    args.extend(parts.take(argc));
    Some(args)
}

/// The parent of a running process.
#[cfg(target_os = "macos")]
pub fn parent_pid(pid: i32) -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is a zeroed proc_bsdinfo of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: proc_pidinfo filled the whole struct.
    (written == size).then(|| unsafe { info.assume_init() }.pbi_ppid as i32)
}

#[cfg(not(target_os = "macos"))]
pub fn parent_pid(pid: i32) -> Option<i32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|ppid| ppid.trim().parse().ok())
}

/// The path of a running process's executable.
#[cfg(target_os = "macos")]
pub fn executable_path(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is valid for `buffer.len()` bytes and proc_pidpath
    // writes at most that many.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(OsString::from_vec(buffer)))
}

#[cfg(not(target_os = "macos"))]
pub fn executable_path(pid: i32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The current working directory of a running process.
#[cfg(target_os = "macos")]
pub fn working_directory(pid: i32) -> Option<PathBuf> {
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: `info` is a zeroed proc_vnodepathinfo of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the whole struct.
    let info = unsafe { info.assume_init() };
    let raw = info.pvi_cdir.vip_path.as_flattened();
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(not(target_os = "macos"))]
pub fn process_name(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn working_directory(pid: i32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// `struct proc_uniqidentifierinfo` from XNU's `sys/proc_info.h`.
#[repr(C)]
struct ProcessIdentity {
    uuid: [u8; 16],
    unique_id: u64,
    parent_unique_id: u64,
    pid_version: i32,
    original_parent_pid_version: i32,
    reserved: [u64; 2],
}

/// `PROC_PIDUNIQIDENTIFIERINFO` from `sys/proc_info.h`.
const PROC_PID_UNIQUE_IDENTIFIER_INFO: libc::c_int = 17;

/// The current pid version of process `pid`.
pub fn pid_version(pid: i32) -> Option<i32> {
    let mut identity = std::mem::MaybeUninit::<ProcessIdentity>::zeroed();
    let size = std::mem::size_of::<ProcessIdentity>() as libc::c_int;
    // SAFETY: `identity` is a zeroed ProcessIdentity of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PID_UNIQUE_IDENTIFIER_INFO,
            0,
            identity.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: proc_pidinfo filled the whole struct.
    (written == size).then(|| unsafe { identity.assume_init() }.pid_version)
}

/// Every process id on the system.
#[cfg(target_os = "macos")]
pub fn process_ids() -> Result<Vec<i32>> {
    // SAFETY: the null buffer requests the process count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    anyhow::ensure!(count > 0, "cannot enumerate session processes");
    let mut processes = vec![0i32; count as usize + 256];
    // SAFETY: the vector has exactly the writable byte capacity passed here.
    let count = unsafe {
        libc::proc_listallpids(
            processes.as_mut_ptr().cast(),
            std::mem::size_of_val(processes.as_slice()) as i32,
        )
    };
    anyhow::ensure!(count >= 0, "cannot enumerate session processes");
    processes.truncate(count as usize);
    Ok(processes)
}

#[cfg(not(target_os = "macos"))]
pub fn process_ids() -> Result<Vec<i32>> {
    Ok(std::fs::read_dir("/proc")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_procargs_layout() {
        let mut raw = 2i32.to_ne_bytes().to_vec();
        raw.extend_from_slice(b"/usr/local/bin/node\0\0\0node\0/opt/bin/claude\0HOME=/x\0");
        assert_eq!(
            parse_procargs(&raw).unwrap(),
            vec!["/usr/local/bin/node", "node", "/opt/bin/claude"]
        );
    }

    #[test]
    fn reads_own_process_details() {
        let pid = std::process::id() as i32;
        assert!(process_name(pid).is_some());
        assert!(process_args(pid).is_some_and(|args| !args.is_empty()));
        assert_eq!(
            working_directory(pid),
            Some(std::env::current_dir().unwrap())
        );
    }
}
