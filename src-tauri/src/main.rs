//! `dsh desktop` — a lightweight native shell that hosts a local `dsh web`.
//!
//! The process is assembled in one place: parse the command line, load the
//! configuration, open the rolling log, resolve the connection, then build the
//! single window and hand the connection to the worker in `connection.rs`. The
//! window starts on a local loading page so the user sees the shell immediately
//! instead of a blank WebView while `dsh` starts.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod connection;
mod lifecycle;
mod platform;
mod shortcuts;
mod state;
mod text;
mod window;

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::menu::{ContextMenu, MenuBuilder, MenuItemBuilder};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl};
use tauri_plugin_dialog::DialogExt;
use url::Url;

use dsh_lite_core::cli::{CliAction, RunOptions};
use dsh_lite_core::config::{CloseAction, Config, ConnectMode, LogLevel};
use dsh_lite_core::logging::Logger;

use crate::connection::ErrorKind;
use crate::lifecycle::effective_close_action;
use crate::state::{AppState, WINDOW_STATE_FILE};
use crate::window::{CONTENT_WEBVIEW, HEADER_WEBVIEW, MAIN_WINDOW, MonitorRect};

/// Exit code used when the configuration cannot be read.
const EXIT_CONFIGURATION: i32 = 1;

/// Exit code used when the command line is unusable.
const EXIT_USAGE: i32 = 2;

fn main() {
    let action = match dsh_lite_core::cli::parse(std::env::args().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("{}: {error}\n", dsh_lite_core::PRODUCT_NAME);
            eprint!("{}", dsh_lite_core::cli::HELP);
            std::process::exit(EXIT_USAGE);
        }
    };

    match action {
        CliAction::Help => print!("{}", dsh_lite_core::cli::HELP),
        CliAction::Version => {
            println!("{} {}", dsh_lite_core::PRODUCT_NAME, dsh_lite_core::VERSION)
        }
        CliAction::Run(options) => run(options),
    }
}

/// Starts the shell for one `run` invocation.
fn run(options: RunOptions) {
    let data_dir = dsh_lite_core::config::default_config_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("dsh desktop"));
    let config_path = dsh_lite_core::config::default_config_path();
    let log_dir = data_dir.join("logs");

    let (config, created) = match &config_path {
        Some(path) => match Config::load_or_create(path) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{}: {error}", dsh_lite_core::PRODUCT_NAME);
                if let Ok(logger) = Logger::open(&log_dir, LogLevel::Error, true) {
                    logger.error(&format!("configuration: {error}"));
                }
                std::process::exit(EXIT_CONFIGURATION);
            }
        },
        None => {
            eprintln!(
                "{}: no configuration directory is available on this system",
                dsh_lite_core::PRODUCT_NAME
            );
            (Config::default(), false)
        }
    };

    let logger = Arc::new(open_logger(&log_dir, &config, &options));
    logger.info(&format!(
        "{} {} starting",
        dsh_lite_core::PRODUCT_NAME,
        dsh_lite_core::VERSION
    ));
    match &config_path {
        Some(path) if created => {
            logger.info(&format!("created the configuration at {}", path.display()))
        }
        Some(path) => logger.info(&format!("loaded the configuration from {}", path.display())),
        None => logger.warn("running without a configuration file"),
    }
    logger.info(&format!("log directory: {}", log_dir.display()));

    let state = Arc::new(AppState::new(
        data_dir,
        config_path,
        Arc::clone(&logger),
        config,
        options,
        created,
    ));

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(
            |app, arguments, _cwd| on_second_instance(app, &arguments),
        ))
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::clone(&state))
        .on_menu_event(|app, event| run_menu_entry(app, event.id().as_ref()))
        .invoke_handler(tauri::generate_handler![
            shell_info,
            save_settings,
            open_logs,
            retry_connection,
            use_web_profile,
            return_to_app,
            shell_shortcut,
            open_external_link,
            shell_window_action,
            shell_drag,
            shell_window_state,
            shell_menu,
            shell_check_update,
            shell_about,
            open_settings,
            quit_shell
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let window = build_window(&handle, &state)?;
            install_shell_chrome(&handle, &state, &window)?;
            attach_window_events(&handle, &window);
            lifecycle::remember_layout(&handle, &window);
            let plan = connection::resolve_plan(&state.config(), &state.cli);
            connection::start(handle.clone(), Arc::clone(&state), plan);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("the shell window configuration is invalid");

    app.run(move |app, event| platform::handle_run_event(app, &event));
}

/// Opens the rolling log, falling back to the temporary directory.
fn open_logger(dir: &Path, config: &Config, options: &RunOptions) -> Logger {
    match Logger::open(dir, config.log.level, options.verbose) {
        Ok(logger) => logger,
        Err(error) => {
            eprintln!(
                "{}: could not open the log directory {}: {error}",
                dsh_lite_core::PRODUCT_NAME,
                dir.display()
            );
            let fallback = std::env::temp_dir().join("dsh-desktop-logs");
            match Logger::open(&fallback, config.log.level, options.verbose) {
                Ok(logger) => logger,
                Err(fallback_error) => {
                    eprintln!(
                        "{}: could not open a log directory: {fallback_error}",
                        dsh_lite_core::PRODUCT_NAME
                    );
                    std::process::exit(EXIT_CONFIGURATION);
                }
            }
        }
    }
}

/// Creates the single main window with its two child webviews.
///
/// The window owns no webview of its own: a 40 DIP `header` webview draws the
/// shell's title bar, and a `content` webview below it hosts either a shell
/// page or the connected `dsh web`. Splitting them is what lets the title bar
/// stay put while the content navigates, without injecting anything into the
/// `dsh` page.
fn build_window(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<tauri::Window> {
    let config = state.config();
    let width = 1200.0;
    let height = 800.0;

    let window = tauri::window::WindowBuilder::new(app, MAIN_WINDOW)
        .title(dsh_lite_core::PRODUCT_NAME)
        .inner_size(width, height)
        .min_inner_size(window::MIN_WIDTH as f64, window::MIN_HEIGHT as f64)
        .decorations(false)
        .visible(true)
        .build()?;

    window.add_child(
        tauri::webview::WebviewBuilder::new(HEADER_WEBVIEW, WebviewUrl::App("header.html".into())),
        tauri::LogicalPosition::new(0.0, 0.0),
        tauri::LogicalSize::new(width, window::HEADER_HEIGHT),
    )?;

    let navigation_state = Arc::clone(state);
    let load_handle = app.clone();
    let log_state = Arc::clone(state);
    window.add_child(
        tauri::webview::WebviewBuilder::new(
            CONTENT_WEBVIEW,
            WebviewUrl::App("loading.html".into()),
        )
        .on_navigation(move |url| handle_navigation(&navigation_state, url))
        .on_page_load(move |_webview, payload| {
            log_state.logger.debug(&format!(
                "content page load: {:?} {}",
                payload.event(),
                dsh_lite_core::url_policy::redact_url(payload.url().as_str())
            ));
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                connection::handle_page_load(&load_handle, payload.url());
            }
        }),
        tauri::LogicalPosition::new(0.0, window::HEADER_HEIGHT),
        tauri::LogicalSize::new(width, height - window::HEADER_HEIGHT),
    )?;

    restore_geometry(&window, state, &config);
    layout_webviews(&window);
    Ok(window)
}

/// Places the header and content webviews inside the window's client area.
///
/// Every size change has to come through here: child webviews keep the size
/// they were built with, so a resize or a DPI change would otherwise leave the
/// content stuck at its original rectangle.
fn layout_webviews(window: &tauri::Window) {
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let header_height = window::header_height(scale).min(size.height as f64) as u32;
    let content_height = size.height.saturating_sub(header_height);
    let app = window.app_handle();

    if let Some(header) = app.get_webview(HEADER_WEBVIEW) {
        let _ = header.set_position(PhysicalPosition::new(0, 0));
        let _ = header.set_size(PhysicalSize::new(size.width, header_height));
    }
    if let Some(content) = app.get_webview(CONTENT_WEBVIEW) {
        let _ = content.set_position(PhysicalPosition::new(0, header_height as i32));
        let _ = content.set_size(PhysicalSize::new(size.width, content_height));
    }
}

/// Decides whether a navigation stays in the window or leaves for the browser.
fn handle_navigation(state: &Arc<AppState>, url: &Url) -> bool {
    let target = state
        .runtime()
        .connection
        .as_ref()
        .map(|connection| connection.web_url.clone());
    match window::classify_navigation(target.as_ref(), url) {
        window::Navigation::External => {
            state.logger.info(&format!(
                "opening an external link in the system browser: {}",
                dsh_lite_core::url_policy::redact_url(url.as_str())
            ));
            if let Err(error) = window::open_external(url.as_str()) {
                state
                    .logger
                    .warn(&format!("could not open the system browser: {error}"));
            }
            false
        }
        window::Navigation::Shell | window::Navigation::App => true,
    }
}

/// Applies the remembered geometry, corrected against the connected monitors.
fn restore_geometry(window: &tauri::Window, state: &Arc<AppState>, config: &Config) {
    if !config.window.remember_bounds || state.cli.reset_window {
        let _ = window.center();
        return;
    }
    let Some(remembered) = window::load_layout(&state.window_state_path()) else {
        let _ = window.center();
        return;
    };
    let corrected = window::clamp_layout(remembered, &monitors(window));
    let _ = window.set_size(PhysicalSize::new(corrected.width, corrected.height));
    let _ = window.set_position(PhysicalPosition::new(corrected.x, corrected.y));
    if corrected.maximized {
        let _ = window.maximize();
    }
    state.logger.info(&format!(
        "restored the window at {},{} with {}x{}",
        corrected.x, corrected.y, corrected.width, corrected.height
    ));
}

/// Monitor rectangles of the current desktop, used for the boundary correction.
fn monitors(window: &tauri::Window) -> Vec<MonitorRect> {
    let Ok(list) = window.available_monitors() else {
        return Vec::new();
    };
    list.iter()
        .map(|monitor| {
            let position = monitor.position();
            let size = monitor.size();
            MonitorRect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            }
        })
        .collect()
}

/// Installs the platform entry point and the keyboard shortcuts.
fn install_shell_chrome(
    app: &AppHandle,
    state: &Arc<AppState>,
    _window: &tauri::Window,
) -> tauri::Result<()> {
    let config = state.config();
    platform::install_shell_entry(app)?;

    // A release build injects nothing into the page unless the configuration
    // opts in, which is also what makes DevTools reachable. The listener goes
    // to the content webview only: the header has no user content to protect.
    if (cfg!(debug_assertions) || config.window.devtools)
        && let Some(content) = app.get_webview(CONTENT_WEBVIEW)
        && let Err(error) = shortcuts::install(&content)
    {
        state.logger.warn(&format!(
            "could not install the keyboard shortcuts: {error}"
        ));
    }
    Ok(())
}

/// Connects the window's close, move and resize events to the shell.
fn attach_window_events(app: &AppHandle, window: &tauri::Window) {
    let handle = app.clone();
    let window_clone = window.clone();
    window.on_window_event(move |event| {
        let Some(state) = handle.try_state::<Arc<AppState>>() else {
            return;
        };
        match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let configured = state.config().window.close_action;
                match effective_close_action(state.platform(), configured, state.created_config) {
                    CloseAction::Hide => {
                        let _ = window_clone.hide();
                        lifecycle::note_background(&state);
                    }
                    CloseAction::Quit => lifecycle::quit(&handle),
                }
            }
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::ScaleFactorChanged { .. } => {
                layout_webviews(&window_clone);
                lifecycle::remember_layout(&handle, &window_clone);
            }
            tauri::WindowEvent::Moved(_) => {
                lifecycle::remember_layout(&handle, &window_clone);
            }
            _ => {}
        }
    });
}

/// Shows and focuses the existing window when a second instance starts.
fn on_second_instance(app: &AppHandle, arguments: &[String]) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let state = Arc::clone(state.inner());
    state.logger.info(&format!(
        "a second instance asked for the window with {} arguments",
        arguments.len()
    ));
    lifecycle::show_main_window(app);

    let Ok(CliAction::Run(options)) = dsh_lite_core::cli::parse(arguments.iter().skip(1).cloned())
    else {
        return;
    };
    if let Some(raw) = options.url {
        match connection::attach_url(&raw) {
            Ok(url) => connection::switch_to_attach(app, &state, url, ConnectMode::Url),
            Err(error) => connection::fail(app, &state, error.into_shell_error()),
        }
    } else if let Some(port) = options.port {
        match connection::loopback_url(port) {
            Ok(url) => connection::switch_to_attach(app, &state, url, ConnectMode::Port),
            Err(error) => connection::fail(app, &state, error.into_shell_error()),
        }
    }
}

/// Facts the settings and loading pages display.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShellInfo {
    shell_version: String,
    dsh_path: Option<String>,
    dsh_version: Option<String>,
    node_version: Option<String>,
    profile: String,
    connect_mode: &'static str,
    url_set: bool,
    close_action: &'static str,
    remember_bounds: bool,
    devtools: bool,
    /// Port `dsh web` is actually listening on right now; read-only display.
    port: Option<u16>,
    /// `connect.port` exactly as configured, including `0` for "let the
    /// operating system choose"; the settings form binds to this value only.
    configured_port: u16,
    log_dir: String,
    config_path: Option<String>,
    window_state_file: String,
    status: &'static str,
    detail: Option<String>,
}

/// Values the settings page writes back into `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsPayload {
    profile: String,
    connect_mode: String,
    /// Attach URL; `None` keeps the stored value, which the page never shows
    /// because it may carry a token.
    url: Option<String>,
    port: u16,
    close_action: String,
    remember_bounds: bool,
    devtools: bool,
}

#[tauri::command]
fn shell_info(state: State<'_, Arc<AppState>>) -> ShellInfo {
    let state = state.inner();
    let config = state.config();
    let runtime = state.runtime();
    let port = runtime
        .connection
        .as_ref()
        .and_then(|connection| connection.port);
    ShellInfo {
        shell_version: dsh_lite_core::VERSION.to_owned(),
        dsh_path: runtime.dsh.command.clone(),
        dsh_version: runtime.dsh.version.clone(),
        node_version: runtime.dsh.node_version.clone(),
        profile: config.dsh.profile.clone(),
        connect_mode: connect_mode_str(config.connect.mode),
        url_set: !config.connect.url.trim().is_empty(),
        close_action: close_action_str(config.window.close_action),
        remember_bounds: config.window.remember_bounds,
        devtools: config.window.devtools,
        port,
        configured_port: config.connect.port,
        log_dir: state.log_dir.display().to_string(),
        config_path: state
            .config_path
            .as_ref()
            .map(|path| path.display().to_string()),
        window_state_file: state
            .window_state_path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| WINDOW_STATE_FILE.to_owned()),
        status: runtime.status.as_str(),
        detail: runtime
            .last_error
            .as_ref()
            .map(|error| error.detail.clone()),
    }
}

#[tauri::command]
fn save_settings(state: State<'_, Arc<AppState>>, settings: SettingsPayload) -> Result<(), String> {
    let state = state.inner();
    let mut config = state.config();
    config.dsh.profile = settings.profile.trim().to_owned();
    config.connect.mode = parse_connect_mode(&settings.connect_mode)?;
    if let Some(url) = &settings.url {
        config.connect.url = url.trim().to_owned();
    }
    config.connect.port = settings.port;
    config.window.close_action = parse_close_action(&settings.close_action)?;
    config.window.remember_bounds = settings.remember_bounds;
    config.window.devtools = settings.devtools;
    config.validate().map_err(|error| error.to_string())?;

    let Some(path) = state.config_path.clone() else {
        return Err("this platform has no configuration file location".to_owned());
    };
    write_config(&path, &config)?;
    state.replace_config(config);
    state
        .logger
        .info(&format!("settings saved to {}", path.display()));
    Ok(())
}

#[tauri::command]
fn open_logs(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    window::open_path(&state.log_dir).map_err(|error| error.to_string())
}

/// What the shell's own title bar can ask the window to do.
///
/// The requests come from `header.html`, which is a page shipped with the
/// shell; the `dsh` page never sees them.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WindowAction {
    Minimize,
    ToggleMaximize,
    Close,
}

#[tauri::command]
fn shell_window_action(app: AppHandle, action: WindowAction) {
    let Some(window) = app.get_window(MAIN_WINDOW) else {
        return;
    };
    match action {
        WindowAction::Minimize => {
            let _ = window.minimize();
        }
        WindowAction::ToggleMaximize => {
            let maximized = window.is_maximized().unwrap_or(false);
            let _ = if maximized {
                window.unmaximize()
            } else {
                window.maximize()
            };
        }
        WindowAction::Close => {
            // Goes through `CloseRequested`, so `window.closeAction` decides
            // between hiding and quitting exactly as the caption button does.
            let _ = window.close();
        }
    }
}

/// Starts a window drag from the title bar's empty strip.
#[tauri::command]
fn shell_drag(app: AppHandle) {
    if let Some(window) = app.get_window(MAIN_WINDOW) {
        let _ = window.start_dragging();
    }
}

/// Whether the window is maximized, so the title bar can draw the right glyph.
#[tauri::command]
fn shell_window_state(app: AppHandle) -> bool {
    app.get_window(MAIN_WINDOW)
        .and_then(|window| window.is_maximized().ok())
        .unwrap_or(false)
}

/// Shows the settings page without touching the connection.
#[tauri::command]
fn open_settings(app: AppHandle, state: State<'_, Arc<AppState>>) {
    let base = state.runtime().local_base.clone();
    connection::navigate_shell_page(&app, base.as_ref(), "settings.html", &[]);
}

/// Ids of the entries in the title bar's popup menus.
const MENU_SETTINGS: &str = "shell:settings";
const MENU_RECONNECT: &str = "shell:reconnect";
const MENU_LOGS: &str = "shell:logs";
const MENU_UPDATE: &str = "shell:update";
const MENU_QUIT: &str = "shell:quit";
const MENU_RELOAD: &str = "shell:reload";
const MENU_ABOUT: &str = "shell:about";

/// Deadline for the `npm view` probe behind an update check.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds one popup menu entry, optionally showing its shortcut on the right.
///
/// The accelerators are display-only here: the shortcuts themselves are
/// handled in `shortcuts.rs`, so a popup that never becomes a menu bar still
/// lists the same keys the shell actually listens for.
fn menu_item<R: tauri::Runtime>(
    manager: &impl Manager<R>,
    id: &'static str,
    text: &str,
    accelerator: Option<&str>,
) -> Result<tauri::menu::MenuItem<R>, String> {
    let builder = MenuItemBuilder::with_id(id, text);
    let builder = match accelerator {
        Some(accelerator) => builder.accelerator(accelerator),
        None => builder,
    };
    builder
        .build(manager)
        .map_err(|error| error.to_string())
}

/// Pops a menu anchored under the title bar button that asked for it.
///
/// The menu is a native popup rather than HTML inside the header webview: the
/// header is only 40 DIP tall, so anything drawn below its edge would be
/// clipped by the webview rectangle.
#[tauri::command]
fn shell_menu(app: AppHandle, menu: String, x: f64, y: f64) -> Result<(), String> {
    let Some(window) = app.get_window(MAIN_WINDOW) else {
        return Ok(());
    };
    let built = match menu.as_str() {
        "app" => MenuBuilder::new(&app)
            .item(&menu_item(&app, MENU_SETTINGS, "打开设置…", None)?)
            .item(&menu_item(
                &app,
                MENU_RECONNECT,
                "重试连接",
                Some("CmdOrCtrl+Shift+L"),
            )?)
            .item(&menu_item(&app, MENU_LOGS, "打开日志目录", None)?)
            .item(&menu_item(&app, MENU_UPDATE, "检查 dsh 更新…", None)?)
            .separator()
            .item(&menu_item(
                &app,
                MENU_QUIT,
                "退出",
                Some("CmdOrCtrl+Q"),
            )?)
            .build()
            .map_err(|error| error.to_string()),
        "view" => MenuBuilder::new(&app)
            .item(&menu_item(
                &app,
                MENU_RELOAD,
                "刷新页面",
                Some("CmdOrCtrl+R"),
            )?)
            .separator()
            .item(&menu_item(&app, MENU_ABOUT, "关于 dsh desktop", None)?)
            .build()
            .map_err(|error| error.to_string()),
        other => return Err(format!("unknown menu {other:?}")),
    };
    built.and_then(|menu| {
        menu.popup_at(window, tauri::LogicalPosition::new(x, y))
            .map_err(|error| error.to_string())
    })
}

/// Carries out one of the title bar's menu entries.
fn run_menu_entry(app: &AppHandle, id: &str) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.logger.info(&format!("menu entry {id}"));
    }
    match id {
        MENU_SETTINGS => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                let base = state.runtime().local_base.clone();
                connection::navigate_shell_page(app, base.as_ref(), "settings.html", &[]);
            }
        }
        MENU_RECONNECT => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                connection::restart(app, state.inner());
            }
        }
        MENU_LOGS => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                if let Err(error) = window::open_path(&state.log_dir) {
                    state
                        .logger
                        .warn(&format!("could not open the log directory: {error}"));
                }
            }
        }
        MENU_UPDATE => check_update(app),
        MENU_RELOAD => {
            if let Some(content) = app.get_webview(CONTENT_WEBVIEW) {
                let _ = content.reload();
            }
        }
        MENU_ABOUT => show_about(app),
        MENU_QUIT => lifecycle::quit(app),
        _ => {}
    }
}

/// Asks npm for the published `dsh` version and reports it in a dialog.
///
/// Nothing is installed: the upgrade command is shown so the user can run it
/// with their own package manager, exactly as the requirement document says.
fn check_update(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let state = Arc::clone(state.inner());
    let handle = app.clone();
    thread::spawn(move || {
        let installed = state.runtime().dsh.version.clone();
        let (program, prefix) = platform::shell_program("npm");
        let npm = dsh_lite_core::dsh_locate::DshCommand::new(program, prefix);
        let args = vec![
            "view".to_owned(),
            dsh_lite_core::update::PACKAGE.to_owned(),
            "version".to_owned(),
        ];
        state.logger.info(&format!(
            "checking {} with npm view",
            dsh_lite_core::update::PACKAGE
        ));

        let message = match connection::run_capture(&npm, &args, UPDATE_TIMEOUT) {
            Ok(capture) => match dsh_lite_core::update::interpret(
                &capture.output,
                capture.code,
                installed.as_deref(),
            ) {
                Ok(outcome) if outcome.newer => format!(
                    "有新版本：{}\n当前安装：{}\n\n升级命令（只展示，不会自动执行）：\n{}",
                    outcome.latest,
                    outcome.installed.as_deref().unwrap_or("未知"),
                    dsh_lite_core::update::UPGRADE_COMMAND
                ),
                Ok(outcome) => format!("已是最新版本：{}", outcome.latest),
                Err(error) => format!("无法判断版本：{error}"),
            },
            Err(error) => format!("检查失败：{error}"),
        };
        state.logger.info(&format!("update check: {message}"));
        handle
            .dialog()
            .message(message)
            .title("检查 dsh 更新")
            .show(|_| {});
    });
}

/// Shows the product name and version.
fn show_about(app: &AppHandle) {
    app.dialog()
        .message(format!(
            "{} {}\n\n轻量原生外壳：不自带运行时，只负责启动并承载本机已安装的 dsh web。",
            dsh_lite_core::PRODUCT_NAME,
            dsh_lite_core::VERSION
        ))
        .title("关于 dsh desktop")
        .show(|_| {});
}

/// Update check reachable from a page (the title bar uses the same path).
#[tauri::command]
fn shell_check_update(app: AppHandle) {
    check_update(&app);
}

/// About dialog reachable from a page.
#[tauri::command]
fn shell_about(app: AppHandle) {
    show_about(&app);
}

/// Quits the shell, stopping a sponsored `dsh` first.
#[tauri::command]
fn quit_shell(app: AppHandle) {
    lifecycle::quit(&app);
}

#[tauri::command]
fn retry_connection(app: AppHandle, state: State<'_, Arc<AppState>>) {
    connection::restart(&app, state.inner());
}

#[tauri::command]
fn use_web_profile(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let state = state.inner();
    let mut config = state.config();
    config.dsh.profile = "web".to_owned();
    if let Some(path) = state.config_path.clone() {
        write_config(&path, &config)?;
    }
    state.replace_config(config);
    state.logger.info("switched to the built-in web profile");
    connection::restart(&app, state);
    Ok(())
}

#[tauri::command]
fn return_to_app(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let state = state.inner();
    let target = state
        .runtime()
        .connection
        .as_ref()
        .map(|connection| connection.web_url.clone());
    match target {
        Some(url) => {
            let content = app
                .get_webview(CONTENT_WEBVIEW)
                .ok_or_else(|| "the main window is closed".to_owned())?;
            content.navigate(url).map_err(|error| error.to_string())
        }
        None => {
            connection::restart(&app, state);
            Ok(())
        }
    }
}

#[tauri::command]
fn shell_shortcut(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    key: String,
    control: bool,
    meta: bool,
    shift: bool,
    alt: bool,
) {
    let state = state.inner();
    let allow_devtools = cfg!(debug_assertions) || state.config().window.devtools;
    let modifiers = shortcuts::Modifiers {
        control,
        meta,
        shift,
        alt,
    };
    let Some(action) = shortcuts::classify(state.platform(), modifiers, &key, allow_devtools)
    else {
        return;
    };
    state
        .logger
        .debug(&format!("shortcut {} requested", action.as_str()));
    match action {
        shortcuts::ShortcutAction::ToggleDevTools => {
            if let Some(content) = app.get_webview(CONTENT_WEBVIEW) {
                if content.is_devtools_open() {
                    content.close_devtools();
                } else {
                    content.open_devtools();
                }
            }
        }
        shortcuts::ShortcutAction::Reload => connection::restart(&app, state),
        shortcuts::ShortcutAction::ConnectionPanel => {
            let base = state.runtime().local_base.clone();
            connection::navigate_shell_page(&app, base.as_ref(), "settings.html", &[]);
        }
        shortcuts::ShortcutAction::Quit => lifecycle::quit(&app),
    }
}

#[tauri::command]
fn open_external_link(url: String) -> Result<(), String> {
    window::open_external(&url).map_err(|error| error.to_string())
}

/// Writes the configuration file, keeping the parent directory present.
fn write_config(path: &Path, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = toml::to_string(config).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}

/// Parses the `connect.mode` value coming from the settings page.
fn parse_connect_mode(value: &str) -> Result<ConnectMode, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "spawn" => Ok(ConnectMode::Spawn),
        "url" => Ok(ConnectMode::Url),
        "port" => Ok(ConnectMode::Port),
        other => Err(format!("unknown connection mode {other:?}")),
    }
}

/// Parses the `window.closeAction` value coming from the settings page.
fn parse_close_action(value: &str) -> Result<CloseAction, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "quit" => Ok(CloseAction::Quit),
        "hide" => Ok(CloseAction::Hide),
        other => Err(format!("unknown close action {other:?}")),
    }
}

/// Configuration name of a connection mode.
fn connect_mode_str(mode: ConnectMode) -> &'static str {
    match mode {
        ConnectMode::Spawn => "spawn",
        ConnectMode::Url => "url",
        ConnectMode::Port => "port",
    }
}

/// Configuration name of a close action.
fn close_action_str(action: CloseAction) -> &'static str {
    match action {
        CloseAction::Quit => "quit",
        CloseAction::Hide => "hide",
    }
}

/// Every kind the error page understands, in the order it documents them.
#[allow(dead_code)]
const ERROR_KINDS: [&str; 5] = [
    ErrorKind::DshNotFound.as_str(),
    ErrorKind::Version.as_str(),
    ErrorKind::Profile.as_str(),
    ErrorKind::Ready.as_str(),
    ErrorKind::Auth.as_str(),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_values_round_trip_through_their_names() {
        for mode in [ConnectMode::Spawn, ConnectMode::Url, ConnectMode::Port] {
            assert_eq!(parse_connect_mode(connect_mode_str(mode)), Ok(mode));
        }
        for action in [CloseAction::Quit, CloseAction::Hide] {
            assert_eq!(parse_close_action(close_action_str(action)), Ok(action));
        }
    }

    #[test]
    fn unknown_settings_values_are_rejected_with_their_name() {
        assert!(parse_connect_mode("socket").is_err());
        assert!(parse_close_action("minimize").is_err());
        assert_eq!(
            parse_connect_mode(" URL "),
            Ok(ConnectMode::Url),
            "values are trimmed and case-folded"
        );
    }

    #[test]
    fn the_error_page_contract_covers_every_kind() {
        assert_eq!(
            ERROR_KINDS,
            ["dsh-not-found", "version", "profile", "ready", "auth"]
        );
    }

    #[test]
    fn a_written_configuration_can_be_read_back() {
        let directory = std::env::temp_dir().join(format!("dsh-config-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("create directory");
        let path = directory.join("config.toml");

        let mut config = Config::default();
        config.dsh.profile = "dsh-desktop".to_owned();
        config.connect.mode = ConnectMode::Port;
        config.connect.port = 3080;
        config.window.close_action = CloseAction::Hide;
        write_config(&path, &config).expect("written");

        let (loaded, created) = Config::load_or_create(&path).expect("read back");
        assert!(!created);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
