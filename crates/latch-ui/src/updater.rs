//! App-wide access to Sparkle's native update driver and preferences.

use gpui::{App, Global};
use latch_platform::updater::{self, Availability, Channel};

struct Updater(Availability);
impl Global for Updater {}

pub fn init(cx: &mut App) {
    cx.set_global(Updater(updater::start()));
}

pub fn available(cx: &App) -> bool {
    cx.try_global::<Updater>()
        .is_some_and(|updater| updater.0 == Availability::Ready)
}

pub fn status(cx: &App) -> &'static str {
    match cx.try_global::<Updater>().map(|updater| updater.0) {
        Some(Availability::Ready) => match updater::channel() {
            Channel::Production => "Production channel",
            Channel::Beta => "Beta channel",
            Channel::Development => "Development channel",
        },
        Some(Availability::Unbundled) => "Updates are available in the installed app.",
        _ => "The updater could not start. Install Latch from its GitHub releases.",
    }
}

pub fn can_check(cx: &App) -> bool {
    available(cx) && updater::can_check()
}

pub fn check(cx: &App) {
    if available(cx) {
        updater::check();
    }
}

pub fn automatic_checks() -> bool {
    updater::automatic_checks()
}

pub fn set_automatic_checks(enabled: bool) {
    updater::set_automatic_checks(enabled);
}
