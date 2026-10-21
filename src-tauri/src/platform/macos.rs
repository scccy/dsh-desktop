//! macOS behaviour: menu bar and Dock, `open` handoff, no console concept.

use std::io;
use std::process::{Command, Stdio};

use tauri::{AppHandle, RunEvent};
use url::Url;

pub fn install_shell_entry(app: &AppHandle) -> tauri::Result<()> {
    super::menu::install(app)
}

/// The application menu holds every action, so nothing is disabled per state.
pub fn set_connection_ready(app: &AppHandle, ready: bool) {
    let _ = (app, ready);
}

pub fn handle_other_event(app: &AppHandle, event: &RunEvent) {
    // A click on the Dock icon re-opens the window instead of quitting.
    if let RunEvent::Reopen { .. } = event {
        crate::lifecycle::show_main_window(app);
    }
}

pub fn shell_base() -> Url {
    Url::parse("tauri://localhost/loading.html").expect("the built-in shell origin is valid")
}

pub fn open_with_system(target: &str) -> io::Result<()> {
    Command::new("open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

pub fn hide_console(_command: &mut Command) {}

pub fn shell_program(name: &str) -> (String, Vec<String>) {
    (name.to_owned(), Vec::new())
}
