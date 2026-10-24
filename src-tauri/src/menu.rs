//! Native application menu for macOS.
//!
//! macOS reaches the shell through the menu bar and the Dock icon instead of a
//! tray icon. The menu bar carries the standard application menu (About, Hide,
//! Hide Others, Show All, Quit with the product name), a **应用** menu and a
//! **视图** menu that mirror the Windows/Linux title-bar menus, the Edit menu
//! (which defers to the WebView's own responder chain so undo, redo, cut, copy,
//! paste and select all keep native behaviour) and the Window menu.
//!
//! Every shell item reuses the same `shell:*` id as the header menu, so the
//! crate-wide `run_menu_entry` handles both surfaces with one code path.
//!
//! Tauri v2 exposes no hook for an `NSApplication` Dock menu — AppKit builds it
//! from the `applicationDockMenu:` delegate method, which the shell cannot add
//! to the framework's delegate — so the Dock provides the equivalent actions
//! through the menu bar and a Dock icon click re-opens the window through
//! `RunEvent::Reopen`.

use tauri::AppHandle;
use tauri::menu::{AboutMetadataBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder};

/// Builds and installs the application, 应用, 视图, Edit and Window menus.
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
            &MenuItemBuilder::with_id(crate::MENU_QUIT, format!("Quit {product}"))
                .accelerator("CmdOrCtrl+Q")
                .build(app)?,
        )
        .build()?;

    // Same actions as the Windows/Linux header's "应用" menu.
    let application_menu = SubmenuBuilder::new(app, "应用")
        .item(&MenuItemBuilder::with_id(crate::MENU_SETTINGS, "打开设置…").build(app)?)
        .item(
            &MenuItemBuilder::with_id(crate::MENU_RECONNECT, "重试连接")
                .accelerator("CmdOrCtrl+Shift+L")
                .build(app)?,
        )
        .item(&MenuItemBuilder::with_id(crate::MENU_LOGS, "打开日志目录").build(app)?)
        .item(&MenuItemBuilder::with_id(crate::MENU_UPDATE, "检查 dsh 更新…").build(app)?)
        .build()?;

    // Same actions as the header's "视图" menu.
    let view_menu = SubmenuBuilder::new(app, "视图")
        .item(
            &MenuItemBuilder::with_id(crate::MENU_RELOAD, "刷新页面")
                .accelerator("CmdOrCtrl+R")
                .build(app)?,
        )
        .separator()
        .item(&MenuItemBuilder::with_id(crate::MENU_ABOUT, "关于 DSH Desktop").build(app)?)
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
        .item(&application_menu)
        .item(&view_menu)
        .item(&edit_menu)
        .item(&window_menu)
        .build()?;
    app.set_menu(menu)?;
    Ok(())
}
