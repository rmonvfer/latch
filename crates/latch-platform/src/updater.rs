//! The bundled Sparkle updater. Native operations are confined to the main thread.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Ready,
    Unbundled,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Production,
    Beta,
    Development,
}

#[cfg(target_os = "macos")]
mod native {
    unsafe extern "C" {
        pub fn latch_updater_start() -> i32;
        pub fn latch_updater_restore_relaunch_environment() -> bool;
        pub fn latch_updater_can_check() -> bool;
        pub fn latch_updater_check();
        pub fn latch_updater_automatic_checks() -> bool;
        pub fn latch_updater_set_automatic_checks(enabled: bool);
        pub fn latch_updater_channel() -> i32;
    }
}

/// Restore a one-shot profile handoff from a completed Sparkle installation.
///
/// # Safety
/// Call on the main thread before starting threads or reading application
/// configuration. This can change the process's `XDG_CONFIG_HOME` environment.
pub unsafe fn restore_relaunch_environment() -> bool {
    #[cfg(target_os = "macos")]
    // The caller guarantees there are no concurrent environment readers.
    return unsafe { native::latch_updater_restore_relaunch_environment() };
    #[cfg(not(target_os = "macos"))]
    false
}

pub fn start() -> Availability {
    #[cfg(target_os = "macos")]
    // The bridge checks the current thread before accessing Sparkle and retains
    // its controller for the lifetime of the process.
    match unsafe { native::latch_updater_start() } {
        1 => Availability::Ready,
        0 => Availability::Unbundled,
        _ => Availability::Unavailable,
    }
    #[cfg(not(target_os = "macos"))]
    Availability::Unavailable
}

pub fn can_check() -> bool {
    #[cfg(target_os = "macos")]
    // The bridge returns false off the main thread or before initialization.
    return unsafe { native::latch_updater_can_check() };
    #[cfg(not(target_os = "macos"))]
    false
}

pub fn check() {
    #[cfg(target_os = "macos")]
    // The bridge validates initialization and thread affinity before invoking Sparkle.
    unsafe {
        native::latch_updater_check();
    }
}

pub fn automatic_checks() -> bool {
    #[cfg(target_os = "macos")]
    // The bridge returns false off the main thread or before initialization.
    return unsafe { native::latch_updater_automatic_checks() };
    #[cfg(not(target_os = "macos"))]
    false
}

pub fn set_automatic_checks(enabled: bool) {
    #[cfg(target_os = "macos")]
    // Sparkle persists this preference; the bridge guards access to its controller.
    unsafe {
        native::latch_updater_set_automatic_checks(enabled);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = enabled;
}

pub fn channel() -> Channel {
    #[cfg(target_os = "macos")]
    // This reads immutable metadata from the process's main bundle.
    match unsafe { native::latch_updater_channel() } {
        0 => Channel::Production,
        1 => Channel::Beta,
        _ => Channel::Development,
    }
    #[cfg(not(target_os = "macos"))]
    Channel::Development
}
