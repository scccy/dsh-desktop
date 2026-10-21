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

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use url::Url;

use dsh_lite_core::cli::{CliAction, RunOptions};
use dsh_lite_core::config::{CloseAction, Config, ConnectMode, LogLevel};
use dsh_lite_core::logging::Logger;

use crate::connection::ErrorKind;
use crate::lifecycle::effective_close_action;
use crate::state::{AppState, WINDOW_STATE_FILE};
use crate::window::{MAIN_WINDOW, MonitorRect};

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
        .invoke_handler(tauri::generate_handler![
            shell_info,
            save_settings,
            open_logs,
            retry_connection,
            use_web_profile,
            return_to_app,
            shell_shortcut,
            open_external_link
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

/// Creates the single main window on the loading page.
fn build_window(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<WebviewWindow> {
    let config = state.config();

    let navigation_state = Arc::clone(state);
    let load_handle = app.clone();
    let builder =
        WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("loading.html".into()))
            .title(dsh_lite_core::PRODUCT_NAME)
            .inner_size(1200.0, 800.0)
            .min_inner_size(window::MIN_WIDTH as f64, window::MIN_HEIGHT as f64)
            .visible(true)
            .on_navigation(move |url| handle_navigation(&navigation_state, url))
            .on_page_load(move |_window, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    connection::handle_page_load(&load_handle, payload.url());
                }
            });

    let window = builder.build()?;
    restore_geometry(&window, state, &config);
    Ok(window)
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
fn restore_geometry(window: &WebviewWindow, state: &Arc<AppState>, config: &Config) {
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
fn monitors(window: &WebviewWindow) -> Vec<MonitorRect> {
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
    window: &WebviewWindow,
) -> tauri::Result<()> {
    let config = state.config();
    platform::install_shell_entry(app)?;

    // A release build injects nothing into the page unless the configuration
    // opts in, which is also what makes DevTools reachable.
    if (cfg!(debug_assertions) || config.window.devtools)
        && let Err(error) = shortcuts::install(window)
    {
        state.logger.warn(&format!(
            "could not install the keyboard shortcuts: {error}"
        ));
    }
    Ok(())
}

/// Connects the window's close, move and resize events to the shell.
fn attach_window_events(app: &AppHandle, window: &WebviewWindow) {
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
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_) => {
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
            let window = app
                .get_webview_window(MAIN_WINDOW)
                .ok_or_else(|| "the main window is closed".to_owned())?;
            window.navigate(url).map_err(|error| error.to_string())
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
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                if window.is_devtools_open() {
                    window.close_devtools();
                } else {
                    window.open_devtools();
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
