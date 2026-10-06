//! The macOS menu bar and the app-wide actions only it offers. Menus are
//! rebuilt whenever settings change so checkmarks and agent profiles stay
//! current.

use gpui::{App, Menu, MenuItem, OsAction, PromptLevel, SystemMenuType, Window, actions};

use crate::{
    APP_NAME,
    pane_group::{
        ClosePane, EqualizePanes, FocusDown, FocusLeft, FocusRight, FocusUp, SplitDown, SplitRight,
        ToggleZoom,
    },
    settings::{Settings, SettingsStore},
    sidebar::TabDensity,
    terminal_view::{ClearScrollback, Copy, Find, Paste, SearchNext, SearchPrevious, SelectAll},
    workspace::{
        ActivateTab, CloseTab, CloseWindow, NewAgent, NewTab, NextAttention, NextTab, OpenSettings,
        PreviousTab, Quit, RenameTab, StopTab, ToggleSidebar,
    },
};

actions!(
    app,
    [
        About,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        ToggleStatusBar,
        CompactTabs,
        ExpandedTabs,
        OpenSettingsFile
    ]
);

/// Points added or removed by one font size step.
const FONT_SIZE_STEP: f32 = 1.;

/// Install the menu bar and the handlers for its app-wide actions. Call
/// after key bindings are registered so menu items show their shortcuts.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &About, cx| {
        with_active_window(cx, |window, cx| {
            let detail = format!("Version {}", env!("CARGO_PKG_VERSION"));
            // Only informs; the answer is not needed.
            let _acknowledged =
                window.prompt(PromptLevel::Info, APP_NAME, Some(&detail), &["OK"], cx);
        });
    });
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| {
        with_active_window(cx, |window, _| window.minimize_window());
    });
    cx.on_action(|_: &Zoom, cx| {
        with_active_window(cx, |window, _| window.zoom_window());
    });
    cx.on_action(|_: &IncreaseFontSize, cx| {
        SettingsStore::update(cx, |settings| settings.font_size += FONT_SIZE_STEP);
    });
    cx.on_action(|_: &DecreaseFontSize, cx| {
        SettingsStore::update(cx, |settings| settings.font_size -= FONT_SIZE_STEP);
    });
    cx.on_action(|_: &ResetFontSize, cx| {
        SettingsStore::update(cx, |settings| {
            settings.font_size = Settings::default().font_size;
        });
    });
    cx.on_action(|_: &ToggleStatusBar, cx| {
        SettingsStore::update(cx, |settings| {
            settings.status_bar.visible = !settings.status_bar.visible;
        });
    });
    cx.on_action(|_: &CompactTabs, cx| {
        SettingsStore::update(cx, |settings| {
            settings.sidebar.density = TabDensity::Compact;
        });
    });
    cx.on_action(|_: &ExpandedTabs, cx| {
        SettingsStore::update(cx, |settings| {
            settings.sidebar.density = TabDensity::Expanded;
        });
    });
    cx.on_action(|_: &OpenSettingsFile, cx| {
        cx.open_with_system(&SettingsStore::path(cx));
    });

    cx.set_menus(app_menus(cx));
    cx.observe_global::<SettingsStore>(|cx| cx.set_menus(app_menus(cx)))
        .detach();
}

/// Run `f` against the frontmost window of the app, if there is one.
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App)) {
    if let Some(handle) = cx.active_window() {
        let _ = handle.update(cx, |_, window, cx| f(window, cx));
    }
}

pub fn app_menus(cx: &App) -> Vec<Menu> {
    let settings = SettingsStore::get(cx);
    let status_bar_label = if settings.status_bar.visible {
        "Hide Status Bar"
    } else {
        "Show Status Bar"
    };
    let density = settings.sidebar.density;
    let agents = settings
        .agent_profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| MenuItem::action(profile.name.clone(), NewAgent(index)));
    let tabs =
        (0..8).map(|index| MenuItem::action(format!("Tab {}", index + 1), ActivateTab(index)));

    vec![
        Menu::new(APP_NAME).items([
            MenuItem::action(format!("About {APP_NAME}"), About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(format!("Hide {APP_NAME}"), Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action(format!("Quit {APP_NAME}"), Quit),
        ]),
        Menu::new("Shell").items([
            MenuItem::action("New Tab", NewTab),
            MenuItem::submenu(
                Menu::new("New Agent")
                    .items(agents)
                    .disabled(settings.agent_profiles.is_empty()),
            ),
            MenuItem::separator(),
            MenuItem::action("Split Right", SplitRight),
            MenuItem::action("Split Down", SplitDown),
            MenuItem::separator(),
            MenuItem::action("Rename Tab…", RenameTab),
            MenuItem::separator(),
            MenuItem::action("Close Pane", ClosePane),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::action("Stop Sessions…", StopTab),
            MenuItem::action("Close Window", CloseWindow),
            MenuItem::separator(),
            MenuItem::action("Next Session Needing Attention", NextAttention),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find…", Find),
            MenuItem::action("Find Next", SearchNext),
            MenuItem::action("Find Previous", SearchPrevious),
            MenuItem::separator(),
            MenuItem::action("Clear Scrollback", ClearScrollback),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action(status_bar_label, ToggleStatusBar),
            MenuItem::submenu(Menu::new("Tab Density").items([
                MenuItem::action("Compact", CompactTabs).checked(density == TabDensity::Compact),
                MenuItem::action("Expanded", ExpandedTabs).checked(density == TabDensity::Expanded),
            ])),
            MenuItem::separator(),
            MenuItem::action("Zoom Pane", ToggleZoom),
            MenuItem::action("Equalize Panes", EqualizePanes),
            MenuItem::separator(),
            MenuItem::action("Increase Font Size", IncreaseFontSize),
            MenuItem::action("Decrease Font Size", DecreaseFontSize),
            MenuItem::action("Reset Font Size", ResetFontSize),
        ]),
        // Named "Window" so macOS lists open windows and adds its own
        // arrangement items to it.
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PreviousTab),
            MenuItem::submenu(
                Menu::new("Select Tab")
                    .items(tabs.chain([MenuItem::action("Last Tab", ActivateTab(usize::MAX))])),
            ),
            MenuItem::separator(),
            MenuItem::action("Focus Pane Left", FocusLeft),
            MenuItem::action("Focus Pane Right", FocusRight),
            MenuItem::action("Focus Pane Above", FocusUp),
            MenuItem::action("Focus Pane Below", FocusDown),
        ]),
        Menu::new("Help").items([MenuItem::action("Open Settings File", OpenSettingsFile)]),
    ]
}
