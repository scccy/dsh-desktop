//! Native menu bar for the Windows and Linux windows.
//!
//! The bar sits under the title bar in the system's own style; the shell draws
//! nothing itself. macOS uses the application menu instead (see `menu.rs`), so
//! this module is only compiled and installed on the other platforms.
//!
//! Only the **Application** menu is installed. A **Edit** menu built from
//! `PredefinedMenuItem` was tried and removed: measured in a real WebView2
//! window on Windows, "Select All" and "Copy" do reach the page (the clipboard
//! received the selected document text), while "Paste" with a focused input and
//! a non-empty clipboard changed nothing. A menu whose items silently do
//! nothing is worse than no menu, so text editing stays with the WebView's own
//! `Ctrl+Z`/`Ctrl+X`/`Ctrl+C`/`Ctrl+V`/`Ctrl+A`, which work natively.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{MenuBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::connection;
use crate::state::AppState;
use crate::text;
use crate::window;

/// Identifier of the "open settings" item.
pub const OPEN_SETTINGS: &str = "shell-open-settings";

/// Identifier of the "open log directory" item.
pub const OPEN_LOGS: &str = "shell-open-logs";

/// Identifier of the "check dsh updates" item.
pub const CHECK_UPDATE: &str = "shell-check-update";

/// Identifier of the "quit" item.
pub const QUIT: &str = "shell-quit";

/// Deadline for the `npm view` probe behind "check dsh updates".
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds and installs the menu bar.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let strings = text::menu_strings();

    let application = SubmenuBuilder::new(app, strings.application)
        .text(OPEN_SETTINGS, strings.open_settings)
        .text(OPEN_LOGS, strings.open_logs)
        .text(CHECK_UPDATE, strings.check_update)
        .separator()
        .text(QUIT, strings.quit)
        .build()?;

    app.set_menu(MenuBuilder::new(app).item(&application).build()?)?;

    // Nothing to open until the shell has shown its own pages.
    set_connection_ready(app, false);
    app.on_menu_event(|app, event| handle(app, event.id().as_ref()));
    Ok(())
}

/// Enables the connection-dependent items once the window shows `dsh web`.
pub fn set_connection_ready(app: &AppHandle, ready: bool) {
    if let Some(item) = app
        .menu()
        .and_then(|menu| menu.get(OPEN_SETTINGS))
        .and_then(|item| item.as_menuitem().cloned())
    {
        let _ = item.set_enabled(ready);
    }
}

/// Runs the action behind one menu item.
fn handle(app: &AppHandle, id: &str) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let state = Arc::clone(state.inner());
    match id {
        OPEN_SETTINGS => {
            let base = state.runtime().local_base.clone();
            connection::navigate_shell_page(app, base.as_ref(), "settings.html", &[]);
        }
        OPEN_LOGS => {
            if let Err(error) = window::open_path(&state.log_dir) {
                state
                    .logger
                    .warn(&format!("could not open the log directory: {error}"));
            }
        }
        CHECK_UPDATE => check_update(app, &state),
        QUIT => crate::lifecycle::quit(app),
        other => state.logger.debug(&format!("ignoring menu item {other}")),
    }
}

/// Asks npm for the published version and reports it without installing.
///
/// Only one check runs at a time: the menu bar and the tray share one event
/// stream, and a user can click twice before the first `npm view` returns.
fn check_update(app: &AppHandle, state: &Arc<AppState>) {
    if UPDATE_CHECK_RUNNING.swap(true, Ordering::SeqCst) {
        state
            .logger
            .debug("an update check is already running; ignoring the request");
        return;
    }
    state
        .logger
        .info("checking the published dsh version with npm view");
    let app = app.clone();
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        let strings = text::menu_strings();
        let installed = state.runtime().dsh.version.clone();
        let (program, prefix_args) = crate::platform::shell_program("npm");
        let probe = connection::helper_output(
            &program,
            &prefix_args,
            &[
                "view".to_owned(),
                "@deepseek-ai/dsh".to_owned(),
                "version".to_owned(),
            ],
            UPDATE_TIMEOUT,
        );

        let (title, body) = match probe {
            Ok((0, output)) => match dsh_lite_core::dsh_locate::parse_version(&output) {
                Some(latest) => {
                    let installed = installed.unwrap_or_else(|| "unknown".to_owned());
                    let body = format!(
                        "{}: {latest}\n{}: {installed}",
                        strings.latest_label, strings.installed_label
                    );
                    if connection::update_is_newer(&latest, &installed) {
                        (strings.update_available_title, body)
                    } else {
                        (strings.update_current_title, body)
                    }
                }
                None => (
                    strings.update_failed_title,
                    format!("npm view returned no version\n{}", output.trim()),
                ),
            },
            Ok((code, output)) => {
                state.logger.warn(&format!(
                    "npm view exited with code {code}: {}",
                    output.trim()
                ));
                (
                    strings.update_failed_title,
                    strings.update_npm_missing.to_owned(),
                )
            }
            Err(error) => {
                state
                    .logger
                    .warn(&format!("could not run npm view: {error}"));
                (
                    strings.update_failed_title,
                    strings.update_npm_missing.to_owned(),
                )
            }
        };

        state.logger.info(&format!("dsh update check: {body}"));
        app.dialog()
            .message(dsh_lite_core::url_policy::redact_text(&body))
            .title(title)
            .show(|_| {});
        UPDATE_CHECK_RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Whether a `npm view` probe is already in flight.
static UPDATE_CHECK_RUNNING: AtomicBool = AtomicBool::new(false);
