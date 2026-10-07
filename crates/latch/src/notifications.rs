//! Desktop notifications that bring the user back to the tab that sent
//! them when clicked.

use gpui::{AnyWindowHandle, App, Global, SystemNotification, WeakEntity};

use crate::{tabs::TabId, workspace::Workspace};

const TAG_PREFIX: &str = "tab-";

/// The window whose tabs notifications refer to.
struct NotificationTarget {
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
}

impl Global for NotificationTarget {}

/// Route clicks on notifications to the workspace in `window`.
pub fn init(window: AnyWindowHandle, workspace: WeakEntity<Workspace>, cx: &mut App) {
    cx.set_global(NotificationTarget { window, workspace });
    cx.on_system_notification_response(|response, cx| {
        let Some(id) = response
            .tag
            .strip_prefix(TAG_PREFIX)
            .and_then(|id| id.parse::<usize>().ok())
        else {
            return;
        };
        let Some(target) = cx.try_global::<NotificationTarget>() else {
            return;
        };
        let workspace = target.workspace.clone();
        let window = target.window;
        cx.activate(true);
        let _ = window.update(cx, |_, window, cx| {
            window.activate_window();
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.activate_by_element_id(id, window, cx);
            });
        });
    });
}

pub fn show(tab: TabId, title: String, body: String, cx: &App) {
    cx.show_system_notification(SystemNotification {
        tag: format!("{TAG_PREFIX}{}", tab.element_id()).into(),
        title: title.into(),
        body: body.into(),
        actions: Vec::new(),
    });
}
