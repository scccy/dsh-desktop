//! Linux and other Unix behaviour: tray icon and `xdg-open` handoff.

use std::io;
use std::process::{Command, Stdio};

use tauri::{AppHandle, RunEvent};
use url::Url;

pub fn install_shell_entry(app: &AppHandle) -> tauri::Result<()> {
    super::menubar::install(app)?;
    super::tray::install(app)
}

pub fn set_connection_ready(app: &AppHandle, ready: bool) {
    super::menubar::set_connection_ready(app, ready);
}

pub fn handle_other_event(app: &AppHandle, event: &RunEvent) {
    let _ = (app, event);
}

pub fn shell_base() -> Url {
    Url::parse("tauri://localhost/loading.html").expect("the built-in shell origin is valid")
}

pub fn open_with_system(target: &str) -> io::Result<()> {
    Command::new("xdg-open")
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
