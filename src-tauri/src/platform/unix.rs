//! Linux and other Unix behaviour: tray icon and `xdg-open` handoff.
//!
//! The `dsh://` scheme is registered with a `.desktop` file carrying
//! `MimeType=x-scheme-handler/dsh`, exactly the mechanism a browser or any other
//! scheme handler uses. Both mime-cache commands are best effort: a desktop
//! environment that reads `~/.local/share/applications` on demand (or a headless
//! session without `xdg-mime`) still picks the handler up.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tauri::{AppHandle, RunEvent};
use url::Url;

use super::Registration;

/// File name of the handler entry inside the applications directory.
const DESKTOP_FILE: &str = "dsh-desktop-handler.desktop";

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

/// No window chrome to apply on this platform; see `platform::apply_window_chrome`.
///
/// Linux draws the 40 DIP `header` webview instead of a material, so there is
/// nothing to apply.
pub fn apply_window_chrome(_window: &tauri::Window) -> Result<String, String> {
    Ok(String::new())
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

/// Writes the `.desktop` handler and points the scheme at it.
pub fn register_deep_link(executable: &Path) -> Registration {
    let Some(directory) = applications_dir() else {
        return Registration::Failed(
            "neither XDG_DATA_HOME nor HOME is set, so no application directory exists".to_owned(),
        );
    };
    if let Err(error) = std::fs::create_dir_all(&directory) {
        return Registration::Failed(format!("could not create {}: {error}", directory.display()));
    }
    let entry = directory.join(DESKTOP_FILE);
    let executable = executable.display().to_string();
    if let Err(error) = std::fs::write(&entry, super::deeplink::unix_desktop_entry(&executable)) {
        return Registration::Failed(format!("could not write {}: {error}", entry.display()));
    }

    // Refresh the mime cache; a failure here is not fatal, the file is in place.
    let _ = Command::new("update-desktop-database")
        .arg(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("xdg-mime")
        .args(["default", DESKTOP_FILE, "x-scheme-handler/dsh"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Registration::Registered
}

/// `$XDG_DATA_HOME/applications`, falling back to `$HOME/.local/share/applications`.
fn applications_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|base| base.join("applications"))
}

/// No-op: scroll bounce is a macOS WKWebView behaviour.
pub fn tame_scrolling(_window: &tauri::Window) {}
