//! The app's interface: windows, panes, terminal views and their blocks,
//! the command editor, settings, and the control socket that drives them.

pub mod app_menus;
pub mod assets;
pub mod blocks;
pub mod command_editor;
pub mod components;
pub mod control;
pub mod grid;
pub mod input;
pub mod links;
pub mod settings;
pub mod terminal_view;
pub mod theme;
pub mod workspace;

/// The app's name as people see it, in menus and window titles.
pub const APP_NAME: &str = "Latch";
