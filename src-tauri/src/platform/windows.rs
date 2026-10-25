//! Windows behaviour: tray icon, `url.dll` handoff and hidden consoles.
//!
//! The `dsh://` scheme is registered per user under
//! `HKCU\Software\Classes\dsh`, the same keys Electron's
//! `setAsDefaultProtocolClient` writes. Per-user rather than machine-wide on
//! purpose: registering a protocol handler must not need an elevation prompt on
//! first launch.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use tauri::{AppHandle, RunEvent};
use url::Url;

use super::Registration;

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

/// No window chrome to apply on this platform; see `platform::apply_window_chrome`.
///
/// Windows draws the 40 DIP `header` webview instead of a material, so there
/// is nothing to apply.
pub fn apply_window_chrome(_window: &tauri::Window) -> Result<String, String> {
    Ok(String::new())
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

/// Writes the scheme handler into the per-user registry.
///
/// `reg.exe` instead of a registry crate: the shell only needs four writes, and
/// a binder would add a dependency for something the operating system ships.
pub fn register_deep_link(executable: &Path) -> Registration {
    let executable = executable.display().to_string();
    for value in super::deeplink::windows_registration(&executable) {
        let mut command = Command::new("reg.exe");
        command
            .args(value.add_arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_console(&mut command);
        match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                return Registration::Failed(format!(
                    "reg.exe exited with {status} while writing {}",
                    value.key
                ));
            }
            Err(error) => {
                return Registration::Failed(format!("could not run reg.exe: {error}"));
            }
        }
    }
    Registration::Registered
}

/// No-op: scroll bounce is a macOS WKWebView behaviour.
pub fn tame_scrolling(_window: &tauri::Window) {}
