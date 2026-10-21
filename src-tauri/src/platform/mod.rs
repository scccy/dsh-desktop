//! The one place in the shell where operating-system differences live.
//!
//! Everything outside this module calls the functions below and never branches
//! on the target itself. A platform that eventually needs a different native
//! shell only replaces its own file here; `dsh-lite-core` stays reusable as it
//! is, because it never reached into the windowing layer.

use std::process::Command;

use tauri::{AppHandle, RunEvent};
use url::Url;

#[cfg(target_os = "macos")]
#[path = "../menu.rs"]
pub mod menu;

#[cfg(not(target_os = "macos"))]
#[path = "../menubar.rs"]
pub mod menubar;

#[cfg(not(target_os = "macos"))]
#[path = "../tray.rs"]
pub mod tray;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(unix, not(target_os = "macos")))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(all(unix, not(target_os = "macos")))]
use unix as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// Installs the platform's application entry point.
///
/// macOS uses the application menu and the Dock, the other platforms use a
/// menu bar plus a tray icon; every platform exposes the same actions.
pub fn install_shell_entry(app: &AppHandle) -> tauri::Result<()> {
    imp::install_shell_entry(app)
}

/// Enables the menu entries that only make sense once `dsh web` is showing.
pub fn set_connection_ready(app: &AppHandle, ready: bool) {
    imp::set_connection_ready(app, ready);
}

/// Handles the run-loop events this platform adds to the portable ones.
pub fn handle_run_event(app: &AppHandle, event: &RunEvent) {
    match event {
        // Every platform stops a sponsored child and remembers the window.
        RunEvent::ExitRequested { .. } | RunEvent::Exit => crate::lifecycle::shutdown(app),
        other => imp::handle_other_event(app, other),
    }
}

/// Origin of the pages shipped with the shell.
pub fn shell_base() -> Url {
    imp::shell_base()
}

/// Opens a URL, a directory or a file with the user's own handler.
pub fn open_with_system(target: &str) -> std::io::Result<()> {
    imp::open_with_system(target)
}

/// Keeps a helper process from flashing a console window.
pub fn hide_console(command: &mut Command) {
    imp::hide_console(command);
}

/// Command line that starts a tool installed as a script shim.
///
/// Windows installs `npm` as `npm.cmd`, which `CreateProcess` cannot start
/// directly, so the shim runs through `cmd.exe /c`; every other platform starts
/// the name as it is.
pub fn shell_program(name: &str) -> (String, Vec<String>) {
    imp::shell_program(name)
}
