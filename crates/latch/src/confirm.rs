//! Asking before stopping sessions that still have programs running.

use gpui::{AsyncWindowContext, Context, PromptLevel, SharedString, WeakEntity, Window};

use crate::settings::SettingsStore;

/// Run `action` now if nothing is running (or confirmation is turned off);
/// otherwise ask first and run it only if the user agrees.
pub fn confirm_close<T: 'static>(
    running: Vec<SharedString>,
    question: &'static str,
    window: &mut Window,
    cx: &mut Context<T>,
    action: impl FnOnce(&mut T, &mut Window, &mut Context<T>) + 'static,
) {
    if running.is_empty() || !SettingsStore::get(cx).confirm_close {
        cx.defer_in(window, action);
        return;
    }
    let detail = describe(&running);
    let answer = window.prompt(
        PromptLevel::Warning,
        question,
        Some(&detail),
        &["Stop", "Cancel"],
        cx,
    );
    cx.spawn_in(
        window,
        async move |this: WeakEntity<T>, cx: &mut AsyncWindowContext| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, action);
            }
        },
    )
    .detach();
}

fn describe(running: &[SharedString]) -> String {
    match running {
        [one] => format!("“{one}” is still running."),
        [first, second] => format!("“{first}” and “{second}” are still running."),
        [first, rest @ ..] => format!(
            "“{first}” and {} other programs are still running.",
            rest.len()
        ),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_running_programs() {
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| SharedString::from(name.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(describe(&names(&["vim"])), "“vim” is still running.");
        assert_eq!(
            describe(&names(&["vim", "npm"])),
            "“vim” and “npm” are still running."
        );
        assert_eq!(
            describe(&names(&["vim", "npm", "cargo"])),
            "“vim” and 2 other programs are still running."
        );
    }
}
