//! Native application menu for macOS.
//!
//! macOS reaches the shell through the menu bar and the Dock icon instead of a
//! tray icon: the application menu carries About, Hide, Hide Others, Show All
//! and Quit with the product name, the Window menu carries Minimize, Zoom and
//! Bring All to Front, and the Edit menu defers to the WebView's own responder
//! chain so undo, redo, cut, copy, paste and select all keep native behaviour.
//!
//! Tauri v2 exposes no hook for an `NSApplication` Dock menu — AppKit builds it
//! from the `applicationDockMenu:` delegate method, which the shell cannot add
//! to the framework's delegate — so the Dock provides the equivalent actions
//! through the menu bar (Reload `⌘R`, Quit `⌘Q`) and a Dock icon click
//! re-opens the window through `RunEvent::Reopen`.

use tauri::menu::{
    AboutMetadataBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{AppHandle, Manager};

use crate::connection;
use crate::lifecycle;
use crate::state::AppState;
use crate::window;

/// Builds and installs the application, Edit and Window menus.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let product = dsh_lite_core::PRODUCT_NAME;
    let about = AboutMetadataBuilder::new()
        .name(Some(product))
        .version(Some(dsh_lite_core::VERSION))
        .build();

    let app_menu = SubmenuBuilder::new(app, product)
        .about(Some(about))
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .item(
            &MenuItemBuilder::with_id("reload", "重新加载 (Reload)")
                .accelerator("CmdOrCtrl+R")
                .build(app)?,
        )
        .item(&MenuItemBuilder::with_id("show", "显示窗口 (Show window)").build(app)?)
        .item(&MenuItemBuilder::with_id("logs", "打开日志目录 (Open logs)").build(app)?)
        .item(&MenuItemBuilder::with_id("settings", "打开设置 (Settings)").build(app)?)
        .separator()
        .item(
            &MenuItemBuilder::with_id("quit", format!("Quit {product}"))
                .accelerator("CmdOrCtrl+Q")
                .build(app)?,
        )
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    let window_menu = SubmenuBuilder::new(app, "Window")
        .minimize()
        .maximize_with_text("Zoom")
        .separator()
        .bring_all_to_front_with_text("Bring All to Front")
        .build()?;

    let menu = MenuBuilder::new(app)
        .item(&app_menu)
        .item(&edit_menu)
        .item(&window_menu)
        .build()?;
    app.set_menu(menu)?;

    app.on_menu_event(|app, event| handle_menu(app, event.id().as_ref()));
    Ok(())
}

/// Runs the action behind one menu item.
fn handle_menu(app: &AppHandle, id: &str) {
    let Some(state) = app.try_state::<std::sync::Arc<AppState>>() else {
        return;
    };
    let state = std::sync::Arc::clone(state.inner());
    match id {
        "reload" => {
            state.logger.info("reload requested from the menu bar");
            connection::restart(app, &state);
        }
        "show" => lifecycle::show_main_window(app),
        "logs" => {
            if let Err(error) = window::open_path(&state.log_dir) {
                state
                    .logger
                    .warn(&format!("could not open the log directory: {error}"));
            }
        }
        "settings" => {
            let base = state.runtime().local_base.clone();
            connection::navigate_shell_page(app, base.as_ref(), "settings.html", &[]);
        }
        "quit" => lifecycle::quit(app),
        _ => {}
    }
}

/// Keeps `PredefinedMenuItem` in the import list of this platform build.
#[allow(dead_code)]
fn separator(app: &AppHandle) -> tauri::Result<PredefinedMenuItem<tauri::Wry>> {
    PredefinedMenuItem::separator(app)
}
