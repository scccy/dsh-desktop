//! Close behaviour, remembered geometry and the exit sequence.
//!
//! Closing the window either quits the shell (stopping a sponsored `dsh`
//! first) or hides it, depending on `window.closeAction` and the platform
//! default. The `dsh` child is stopped on every exit path, so closing the shell
//! never leaves an orphan process behind.

use std::sync::Arc;

use tauri::{AppHandle, Manager, WebviewWindow};

use dsh_lite_core::config::{CloseAction, Platform};

use crate::connection;
use crate::state::AppState;
use crate::window::{MAIN_WINDOW, WindowLayout};

/// Close behaviour for `platform` given the configured value.
///
/// macOS keeps the application (and its `dsh`) alive when the window closes, so
/// a first launch on macOS treats the untouched shared default — `quit` — as
/// `hide`. An explicit `quit` in an existing file is always honoured.
pub fn effective_close_action(
    platform: Platform,
    configured: CloseAction,
    created_config: bool,
) -> CloseAction {
    match (platform, configured, created_config) {
        (Platform::MacOs, CloseAction::Quit, true) => CloseAction::Hide,
        _ => configured,
    }
}

/// Records the current geometry in memory; it is written when the shell exits.
pub fn remember_layout(app: &AppHandle, window: &WebviewWindow) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    if let Some(layout) = window_layout(window) {
        state.inner().remember_layout(layout);
    }
}

/// Reads the window geometry straight from the window manager.
fn window_layout(window: &WebviewWindow) -> Option<WindowLayout> {
    let (Ok(position), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return None;
    };
    Some(WindowLayout {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        maximized: window.is_maximized().unwrap_or(false),
    })
}

/// Writes the geometry to `window.json` when the configuration asks for it.
///
/// The in-memory value only exists after a move or a resize, so a window that
/// was never touched falls back to its current geometry.
pub fn persist_layout(app: &AppHandle, state: &AppState) {
    if !state.config().window.remember_bounds {
        return;
    }
    let layout = state.layout().or_else(|| {
        app.get_webview_window(MAIN_WINDOW)
            .as_ref()
            .and_then(window_layout)
    });
    let Some(layout) = layout else {
        return;
    };
    match crate::window::save_layout(&state.window_state_path(), &layout) {
        Ok(()) => state.logger.debug(&format!(
            "remembered the window geometry {},{} {}x{}",
            layout.x, layout.y, layout.width, layout.height
        )),
        Err(error) => state
            .logger
            .warn(&format!("could not remember the window geometry: {error}")),
    }
}

/// Logs — once — that hiding the window keeps `dsh` running.
pub fn note_background(state: &AppState) {
    let marker = state.background_notice_path();
    if marker.exists() {
        state
            .logger
            .debug("the window was hidden; the background notice was already shown");
        return;
    }
    if let Err(error) = std::fs::write(&marker, "shown\n") {
        state
            .logger
            .warn(&format!("could not record the background notice: {error}"));
    }
    state.logger.info(
        "the window was hidden; the shell and dsh keep running in the background, quit from the tray or the menu",
    );
}

/// Stops everything the shell owns without exiting the process.
///
/// The run loop reaches this from the close request and again from the exit
/// events, so only the first call does the work.
pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner();
        if !state.begin_shutdown() {
            return;
        }
        connection::stop_child(state);
        persist_layout(app, state);
        state.logger.info("shell shut down");
    }
}

/// Stops the sponsored `dsh` and exits the process.
pub fn quit(app: &AppHandle) {
    shutdown(app);
    app.exit(0);
}

/// Shows, restores and focuses the single main window.
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Hides the main window, keeping the process alive.
pub fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.hide();
    }
}

/// Whether the main window is currently visible.
pub fn main_window_visible(app: &AppHandle) -> bool {
    app.get_webview_window(MAIN_WINDOW)
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_close_action_of_the_file_is_used_once_it_was_written_by_the_user() {
        assert_eq!(
            effective_close_action(Platform::Windows, CloseAction::Quit, false),
            CloseAction::Quit
        );
        assert_eq!(
            effective_close_action(Platform::Unix, CloseAction::Hide, false),
            CloseAction::Hide
        );
        assert_eq!(
            effective_close_action(Platform::MacOs, CloseAction::Quit, false),
            CloseAction::Quit,
            "an explicit quit survives"
        );
    }

    #[test]
    fn a_fresh_macos_configuration_hides_instead_of_quitting() {
        assert_eq!(
            effective_close_action(Platform::MacOs, CloseAction::Quit, true),
            CloseAction::Hide
        );
        assert_eq!(
            effective_close_action(Platform::MacOs, CloseAction::Hide, true),
            CloseAction::Hide
        );
        assert_eq!(
            effective_close_action(Platform::Windows, CloseAction::Quit, true),
            CloseAction::Quit
        );
    }
}
