//! Tray icon for Windows and Linux.
//!
//! macOS uses the native application menu instead (see `menu.rs`), so this
//! module is only compiled for the platforms whose only application-wide entry
//! point is the tray. Every item that stops the shell stops a sponsored `dsh`
//! first.

use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::connection;
use crate::lifecycle;
use crate::state::AppState;
use crate::window;

/// Identifier of the single tray icon.
const TRAY_ID: &str = "dsh-desktop";

/// Identifier of the tray menu carrying the shell actions.
const TRAY_MENU_ID: &str = "dsh-desktop-tray";

/// Creates the tray icon with its menu.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItemBuilder::with_id("toggle", "显示 / 隐藏窗口 (Show / Hide)").build(app)?;
    let reload = MenuItemBuilder::with_id("reload", "重新加载 (Reload)")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let logs = MenuItemBuilder::with_id("logs", "打开日志目录 (Open logs)").build(app)?;
    let settings = MenuItemBuilder::with_id("settings", "打开设置 (Settings)").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出 (Quit)")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;

    let menu = MenuBuilder::with_id(app, TRAY_MENU_ID)
        .items(&[
            &toggle,
            &reload,
            &PredefinedMenuItem::separator(app)?,
            &logs,
            &settings,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ])
        .build()?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(dsh_lite_core::PRODUCT_NAME)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

/// Runs the action behind one tray menu item.
fn handle_menu(app: &AppHandle, id: &str) {
    let Some(state) = app.try_state::<std::sync::Arc<AppState>>() else {
        return;
    };
    let state = std::sync::Arc::clone(state.inner());
    match id {
        "toggle" => toggle_window(app),
        "reload" => {
            state.logger.info("reload requested from the tray");
            connection::restart(app, &state);
        }
        "logs" => {
            state.logger.info("opening the log directory from the tray");
            if let Err(error) = window::open_path(&state.log_dir) {
                state
                    .logger
                    .warn(&format!("could not open the log directory: {error}"));
            }
        }
        "settings" => {
            state.logger.info("opening the settings page from the tray");
            let base = state.runtime().local_base.clone();
            connection::navigate_shell_page(app, base.as_ref(), "settings.html", &[]);
        }
        "quit" => {
            state.logger.info("quit requested from the tray");
            lifecycle::quit(app);
        }
        // The window's menu bar shares the process-wide menu event stream, so
        // every other identifier belongs to `menubar.rs` and is ignored here.
        _ => {}
    }
}

/// Shows the window when it is hidden and hides it otherwise.
fn toggle_window(app: &AppHandle) {
    if lifecycle::main_window_visible(app) {
        lifecycle::hide_main_window(app);
    } else {
        lifecycle::show_main_window(app);
    }
}
