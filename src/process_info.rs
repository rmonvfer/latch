use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// The checked-out branch of the git repository containing `directory`,
/// or a short commit hash when HEAD is detached.
pub fn git_branch(directory: &Path) -> Option<String> {
    let git_dir = find_git_dir(directory)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        ),
        None => Some(head.chars().take(7).collect()),
    }
}

fn find_git_dir(directory: &Path) -> Option<PathBuf> {
    for ancestor in directory.ancestors() {
        let candidate = ancestor.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        // Worktrees and submodules use a `.git` file pointing at the real dir.
        if candidate.is_file() {
            let contents = fs::read_to_string(&candidate).ok()?;
            let target = contents.trim().strip_prefix("gitdir: ")?;
            return Some(ancestor.join(target));
        }
    }
    None
}

/// Replace the home directory prefix with `~`.
pub fn shorten_home(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branch_from_head_file() {
        let root = std::env::temp_dir().join(format!("git-branch-test-{}", std::process::id()));
        let nested = root.join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/tabs\n").unwrap();

        assert_eq!(git_branch(&nested).as_deref(), Some("feature/tabs"));

        fs::write(root.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(&nested).as_deref(), Some("0123456"));

        fs::remove_dir_all(&root).unwrap();
    }

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
