mod cli;
mod mcp;
mod profiling;

use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px,
    size,
};
use gpui_platform::application;

use latch_ui::{
    APP_NAME,
    app_menus::{DecreaseFontSize, Hide, HideOthers, IncreaseFontSize, Minimize, ResetFontSize},
    assets::Assets,
    settings::SettingsStore,
    terminal_view::{ClearScrollback, Copy, Paste, SelectAll},
    workspace::{
        ActivateTab, CloseTab, CloseWindow, NewTab, NextAttention, NextTab, OpenSettings,
        PreviousTab, Quit, RenameTab, ToggleCommandPalette, ToggleSidebar, Workspace,
    },
};

/// Bundle identifier, matching the app bundle built by script/bundle-mac.
const APP_IDENTIFIER: &str = "me.egrati.terminal";

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--session-runtime")
    {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
        profiling::init("runtime");
        if let Err(error) = latch_runtime::serve() {
            log::error!("session runtime: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    // With arguments, act as the command-line client for a running app.
    if let Some(code) = cli::run_from_args() {
        std::process::exit(code);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    profiling::init("app");

    application().with_assets(Assets).run(|cx: &mut App| {
        cx.set_app_identity(APP_IDENTIFIER, APP_NAME);
        latch_ui::assets::load_fonts(cx);
        SettingsStore::init(cx);
        latch_ui::theme::init(cx);
        latch_ui::terminal_view::shell_integration::ShellIntegration::init(cx);
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        latch_ui::app_menus::init(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(APP_NAME.into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(9.), px(9.))),
                }),
                app_owns_titlebar_drag: true,
                window_min_size: Some(size(px(480.), px(260.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(window, cx)),
        );
        let handle = match opened {
            Ok(handle) => handle,
            Err(error) => {
                log::error!("failed to open window: {error:#}");
                cx.quit();
                return;
            }
        };
        let workspace = handle.update(cx, |_, window, cx| {
            let workspace = cx.entity();
            let guarded = workspace.clone();
            window.on_window_should_close(cx, move |window, cx| {
                guarded.update(cx, |workspace, cx| {
                    workspace.should_close_window(window, cx)
                })
            });
            workspace
        });
        if let Ok(workspace) = workspace {
            latch_ui::workspace::notifications::init(handle.into(), workspace.downgrade(), cx);
            latch_ui::control::start(handle, cx);
        }
        cx.activate(true);
    });
}

fn bind_keys(cx: &mut App) {
    let mut bindings = vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-=", IncreaseFontSize, None),
        KeyBinding::new("cmd--", DecreaseFontSize, None),
        KeyBinding::new("cmd-0", ResetFontSize, None),
        KeyBinding::new("cmd-t", NewTab, Some("Workspace")),
        KeyBinding::new("cmd-alt-w", CloseTab, Some("Workspace")),
        KeyBinding::new("cmd-w", CloseTab, Some("Workspace")),
        KeyBinding::new("cmd-shift-w", CloseWindow, Some("Workspace")),
        KeyBinding::new("cmd-}", NextTab, Some("Workspace")),
        KeyBinding::new("cmd-{", PreviousTab, Some("Workspace")),
        KeyBinding::new("ctrl-tab", NextTab, Some("Workspace")),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some("Workspace")),
        KeyBinding::new("cmd-b", ToggleSidebar, Some("Workspace")),
        KeyBinding::new("cmd-shift-j", NextAttention, Some("Workspace")),
        KeyBinding::new("cmd-,", OpenSettings, Some("Workspace")),
        KeyBinding::new("cmd-k", ToggleCommandPalette, Some("Workspace")),
        KeyBinding::new("f2", RenameTab, Some("Workspace")),
        KeyBinding::new("cmd-9", ActivateTab(usize::MAX), Some("Workspace")),
        KeyBinding::new("cmd-c", Copy, Some("Terminal")),
        KeyBinding::new("cmd-v", Paste, Some("Terminal")),
        KeyBinding::new("cmd-a", SelectAll, Some("Terminal")),
        KeyBinding::new("cmd-shift-k", ClearScrollback, Some("Terminal")),
    ];
    for index in 0..8 {
        bindings.push(KeyBinding::new(
            &format!("cmd-{}", index + 1),
            ActivateTab(index),
            Some("Workspace"),
        ));
    }
    bindings.extend(latch_ui::components::text_input::key_bindings());
    bindings.extend(latch_ui::command_editor::key_bindings());
    bindings.extend(latch_ui::workspace::command_palette::key_bindings());
    bindings.extend(latch_ui::workspace::pane_group::key_bindings());
    bindings.extend(latch_ui::terminal_view::key_bindings());
    cx.bind_keys(bindings);
}
