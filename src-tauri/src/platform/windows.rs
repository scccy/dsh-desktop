//! Windows behaviour: tray icon, `url.dll` handoff and hidden consoles.

use std::io;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use tauri::{AppHandle, RunEvent};
use url::Url;

/// Hides the console window of a child started from a GUI process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    Url::parse("http://tauri.localhost/loading.html").expect("the built-in shell origin is valid")
}

pub fn open_with_system(target: &str) -> io::Result<()> {
    // `url.dll` is the shell's own protocol handler: it opens URLs in the
    // default browser and paths in the file manager, without a console window.
    Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

pub fn hide_console(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Windows installs npm as a `.cmd` shim, so it runs through `cmd.exe /c`.
pub fn shell_program(name: &str) -> (String, Vec<String>) {
    ("cmd.exe".to_owned(), vec!["/c".to_owned(), name.to_owned()])
}
