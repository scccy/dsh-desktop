//! Three-layer startup: Desktop Host, authenticated proxy, renderer document.
//!
//! This is the Rust counterpart of the official Electron boot order:
//!
//! 1. locate the globally installed `dsh` and gate its version;
//! 2. resolve the packaged `dsh-web-frontend/dist` that the window will show;
//! 3. collect the user's login-shell environment;
//! 4. start the Desktop Host (`runProfile({ profile: "desktop" })`) and wait for
//!    its `ready` line;
//! 5. exchange the Host's one-shot token URL for the browser cookie;
//! 6. start the loopback app server that serves the document and proxies every
//!    dynamic request with that cookie;
//! 7. publish the boot payload (`injections` + `streamBaseUrl`) to the bridge
//!    and navigate the window to `dsh-app://app/`.
//!
//! Nothing here logs a token or a cookie: those values only travel between the
//! Host, this module and the proxy layers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager};
use url::Url;

use dsh_lite_core::app_server::{
    self, AppServer, AppServerConfig, BOOT_SCRIPT, ShellAsset, ShellAssets,
};
use dsh_lite_core::config::Platform;
use dsh_lite_core::desktop_host::{self, HostError, HostSpec};
use dsh_lite_core::dsh_locate::{self, DshCommand, LocateInputs};
use dsh_lite_core::env_probe::{self, EnvProbeConfig};
use dsh_lite_core::frontend_dist::dist_dir;
use dsh_lite_core::version_gate;

use crate::connection::{self, ErrorKind, INSTALL_COMMAND, ShellError};
use crate::state::{AppRuntime, AppState, ConnectionInfo, Status};
use crate::window::CONTENT_WEBVIEW;

/// Deadline for the Host to report `ready`.
const HOST_DEADLINE: Duration = Duration::from_secs(90);

/// Deadline for `dsh --version` / `node --version`.
const PROBE_DEADLINE: Duration = Duration::from_secs(15);

/// How long the Host gets to exit politely during shutdown.
const HOST_STOP_GRACE: Duration = Duration::from_secs(8);

/// Everything the window needs once the three layers are up.
pub struct StartedApp {
    /// Loopback origin the renderer talks to (and `streamBaseUrl`).
    pub app_origin: Url,
    /// Authenticated Desktop Host origin.
    pub host_origin: Url,
    /// Index injections reported by the Host.
    pub injections: Vec<serde_json::Value>,
}

/// Boots the three layers and leaves them running in [`AppState`].
///
/// Runs on a worker thread: the Host launch, the authentication exchange and
/// the server bind all block (or block an async runtime) and must never stall
/// the window thread.
pub fn start(app: AppHandle, state: Arc<AppState>) -> Result<StartedApp, ShellError> {
    let config = state.config();

    // 1. The global dsh this shell drives.
    let mut inputs = LocateInputs::from_process_environment();
    inputs.cli_path = state.cli.dsh_path.clone();
    let configured_path = config.dsh.path.trim();
    inputs.config_path = (!configured_path.is_empty()).then(|| PathBuf::from(configured_path));
    let command = dsh_locate::locate(&inputs).map_err(|error| {
        ShellError::new(
            ErrorKind::DshNotFound,
            format!("{error}\n\n{INSTALL_COMMAND}"),
        )
    })?;
    let display = dsh_lite_core::url_policy::redact_text(&command.display());
    state.logger.info(&format!("located dsh: {display}"));
    state.runtime().dsh.command = Some(display);

    let package = dsh_package_dir(&command).ok_or_else(|| {
        ShellError::new(
            ErrorKind::DshNotFound,
            format!(
                "could not resolve the @deepseek-ai/dsh package directory from {}\n\n{INSTALL_COMMAND}",
                command.display()
            ),
        )
    })?;

    // 2. Version gate: the Host itself refuses older releases, but the shell
    //    wants to say why before the child prints its fatal line.
    let capture = connection::run_capture(&command, &["--version".to_owned()], PROBE_DEADLINE)
        .map_err(|error| {
            ShellError::new(
                ErrorKind::Version,
                format!("could not run `dsh --version`: {error}\n\n{INSTALL_COMMAND}"),
            )
        })?;
    let version_text = capture.output.trim().to_owned();
    state.runtime().dsh.version = Some(version_text.clone());
    let gate = version_gate::check(&version_text).ok_or_else(|| {
        ShellError::new(
            ErrorKind::Version,
            format!(
                "could not read a version from `dsh --version` ({}); upgrade with:\n\n{}",
                version_gate::UPGRADE_COMMAND,
                version_text
            ),
        )
    })?;
    if let Some(refusal) = gate.refusal() {
        return Err(ShellError::new(ErrorKind::Version, refusal));
    }
    if let Some(warning) = gate.warning() {
        state.logger.warn(&warning);
    }
    let host_version = version_text.clone();
    state.logger.info(&format!("dsh version {version_text}"));

    // 3. The renderer build that ships inside that install.
    let dist_dir = dsh_lite_core::frontend_dist(&package).ok_or_else(|| {
        ShellError::new(
            ErrorKind::Profile,
            format!(
                "the installed dsh has no renderer at {}\n\nreinstall it with:\n\n{}",
                dist_dir(&package).display(),
                version_gate::UPGRADE_COMMAND
            ),
        )
    })?;
    state
        .logger
        .info(&format!("renderer dist: {}", dist_dir.display()));

    // 4. The user's login-shell environment, the same one the official shell
    //    hands to its Host.
    let own: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let probe = EnvProbeConfig {
        inherit: config.env.inherit,
        timeout_ms: config.env.timeout_ms,
        shell: (!config.env.shell.is_empty()).then(|| config.env.shell.clone()),
    };
    let environment = env_probe::collect(Platform::current(), &probe, &own);
    state.logger.info(&format!(
        "host environment: {:?}, {} variables, {} overridden",
        environment.report.source, environment.report.variable_count, environment.report.overridden
    ));
    for warning in &environment.report.warnings {
        state.logger.warn(&format!("environment probe: {warning}"));
    }

    // 5. The profile the desktop shell owns.
    let project_dir = HostSpec::profile_dir_from_process_env().ok_or_else(|| {
        ShellError::new(
            ErrorKind::Profile,
            "could not resolve $DSH_HOME/profiles/desktop (neither DSH_HOME nor HOME is set)",
        )
    })?;
    prepare_profile(&project_dir, &state)?;

    let script = host_script(&app).ok_or_else(|| {
        ShellError::new(
            ErrorKind::Profile,
            "the bundled desktop-host.mjs is missing from the installation",
        )
    })?;

    let spec = HostSpec {
        node: PathBuf::from("node"),
        script,
        dsh_package: package.clone(),
        project_dir: project_dir.clone(),
        version: host_version,
        env: environment.vars,
    };

    let host =
        desktop_host::spawn(&spec, HOST_DEADLINE).map_err(|error| host_error(&error, &state))?;
    let ready = host.ready().clone();
    let origin = ready.origin.clone();
    state.logger.info(&format!(
        "desktop host ready (pid {}) at {}",
        host.pid(),
        origin
    ));

    // 6. Exchange the one-shot token URL for the Host cookie. The URL is never
    //    logged: it carries the token.
    let cookie = tauri::async_runtime::block_on(app_server::authenticate_host(&ready.url))
        .map_err(|error| ShellError::new(ErrorKind::Auth, error.to_string()))?;

    // 7. The loopback origin every dynamic request goes through.
    let assets = shell_assets(&app);
    let mut server_config = AppServerConfig::new(dist_dir.clone(), origin.clone(), cookie.clone());
    server_config.boot_script = BOOT_SCRIPT.to_owned();
    server_config.shell_assets = Some(assets);
    server_config.logger = Some(Arc::clone(&state.logger) as Arc<dyn app_server::RouteLog>);
    let server =
        tauri::async_runtime::block_on(AppServer::start(server_config)).map_err(|error| {
            ShellError::new(
                ErrorKind::Ready,
                format!("could not start the loopback app server: {error}"),
            )
        })?;
    let app_origin = server.origin().clone();
    state
        .logger
        .info(&format!("app server listening at {app_origin}"));

    let injections = ready.injections.clone();
    let app_base = app_origin.as_str().trim_end_matches('/').to_owned();

    {
        let mut runtime = state.runtime();
        runtime.host = Some(host);
        runtime.app = Some(AppRuntime {
            host_origin: origin.clone(),
            dist_dir: dist_dir.clone(),
            cookie,
            server,
        });
        runtime.connection = Some(ConnectionInfo {
            mode: dsh_lite_core::config::ConnectMode::Spawn,
            web_url: origin.clone(),
            port: origin.port_or_known_default(),
        });
        runtime.status = Status::Ready;
        runtime.pending_auth = None;
    }

    // 8. Hand the boot payload to the preload bridge and show the app.
    if let Some(bridge) = app.try_state::<crate::bridge::BridgeState>() {
        bridge.set_boot(injections.clone(), app_base.clone());
        bridge.set_app_base(app_base);
    }
    navigate_to_app(&app, &state)?;

    Ok(StartedApp {
        app_origin,
        host_origin: origin,
        injections,
    })
}

/// Stops the Host and the app server without touching the process exit path.
///
/// Idempotent: whichever shutdown path runs second finds nothing to stop.
pub fn stop(state: &Arc<AppState>) {
    let (server, mut host) = {
        let mut runtime = state.runtime();
        (
            runtime.app.take().map(|app| app.server),
            runtime.host.take(),
        )
    };
    if let Some(server) = server {
        tauri::async_runtime::block_on(server.shutdown());
    }
    if let Some(host) = host.as_mut() {
        host.stop(HOST_STOP_GRACE);
    }
}

/// Points the content webview at the application document.
fn navigate_to_app(app: &AppHandle, state: &Arc<AppState>) -> Result<(), ShellError> {
    let target = Url::parse("dsh-app://app/").expect("the application origin is a valid URL");
    let Some(content) = app.get_webview(CONTENT_WEBVIEW) else {
        return Err(ShellError::new(
            ErrorKind::Ready,
            "the main window content webview is gone",
        ));
    };
    content.navigate(target).map_err(|error| {
        state
            .logger
            .warn(&format!("could not show the application document: {error}"));
        ShellError::new(
            ErrorKind::Ready,
            format!("could not show the application document: {error}"),
        )
    })
}

/// Resolver for the shell's own pages, backed by Tauri's asset resolver.
fn shell_assets(app: &AppHandle) -> Arc<dyn ShellAssets> {
    let app = app.clone();
    Arc::new(move |path: &str| {
        app.asset_resolver()
            .get(path.to_owned())
            .map(|asset| ShellAsset {
                status: 200,
                content_type: asset.mime_type().to_owned(),
                body: asset.bytes().to_vec(),
            })
    })
}

/// Absolute path of the Desktop Host launcher, bundled next to the executable.
fn host_script(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(directory) = app.path().resource_dir() {
        let bundled = directory.join("host").join("desktop-host.mjs");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("host")
        .join("desktop-host.mjs");
    development.is_file().then_some(development)
}

/// Directory of the installed `@deepseek-ai/dsh` package.
///
/// `dsh` may be a symlink on `PATH`, a `.js` entry passed to `node`, or a shim
/// with the script in its prefix arguments; each candidate is canonicalized and
/// walked upwards until its `package.json` names the package.
pub fn dsh_package_dir(command: &DshCommand) -> Option<PathBuf> {
    let mut starts = vec![command.program().to_path_buf()];
    starts.extend(command.prefix_args().iter().map(PathBuf::from));
    for start in starts {
        let start = std::fs::canonicalize(&start).unwrap_or(start);
        if let Some(directory) = walk_to_package(&start) {
            return Some(directory);
        }
    }
    None
}

/// Walks from `start` upwards looking for the dsh package manifest.
fn walk_to_package(start: &Path) -> Option<PathBuf> {
    let mut directory = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    for _ in 0..16 {
        let manifest = directory.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && value.get("name").and_then(|name| name.as_str()) == Some("@deepseek-ai/dsh")
        {
            return Some(directory);
        }
        directory = directory.parent()?.to_path_buf();
    }
    None
}

/// Creates the desktop profile manifest when the profile has never been made.
///
/// The official installer writes this manifest with pnpm; we only write the
/// manifest so the Host's `loadProfileDirectory` can resolve the bundles from
/// the global install anchor. An existing profile is never touched.
fn prepare_profile(project_dir: &Path, state: &AppState) -> Result<(), ShellError> {
    std::fs::create_dir_all(project_dir).map_err(|error| {
        ShellError::new(
            ErrorKind::Profile,
            format!("could not create {}: {error}", project_dir.display()),
        )
    })?;
    let manifest = project_dir.join("package.json");
    if manifest.is_file() {
        return Ok(());
    }
    let template = serde_json::json!({
        "name": "@deepseek-ai/dsh-desktop-runtime",
        "private": true,
        "version": "0.0.0",
        "dsh": {
            "profile": {
                "bundles": [
                    "@deepseek-ai/dsh-base",
                    "@deepseek-ai/dsh-web-app",
                    "@deepseek-ai/dsh-experimental-plugin-market-bundle"
                ]
            }
        }
    });
    let text = serde_json::to_string_pretty(&template).map_err(|error| {
        ShellError::new(ErrorKind::Profile, format!("profile manifest: {error}"))
    })?;
    std::fs::write(&manifest, format!("{text}\n")).map_err(|error| {
        ShellError::new(
            ErrorKind::Profile,
            format!("could not write {}: {error}", manifest.display()),
        )
    })?;
    state.logger.info(&format!(
        "created the desktop profile at {}",
        project_dir.display()
    ));
    Ok(())
}

/// Maps a Host launch failure onto the error page vocabulary.
fn host_error(error: &HostError, state: &Arc<AppState>) -> ShellError {
    let detail = match error {
        HostError::Fatal { fatal, stderr_tail } if fatal.is_version_refusal() => {
            return ShellError::new(ErrorKind::Version, fatal.summary());
        }
        HostError::Fatal { fatal, stderr_tail } => {
            let mut text = fatal.summary();
            if !stderr_tail.is_empty() {
                text.push_str("\n\n");
                text.push_str(stderr_tail);
            }
            text
        }
        HostError::Spawn { program, source } => format!(
            "could not start {} for the Desktop Host: {source}\n\nnode must be on PATH (>= 22.19)",
            program.display()
        ),
        HostError::Timeout { after, stderr_tail } => format!(
            "the Desktop Host did not report ready within {}s\n\n{stderr_tail}",
            after.as_secs()
        ),
        HostError::Exited { code, stderr_tail } => format!(
            "the Desktop Host exited before it was ready (code {})\n\n{stderr_tail}",
            code.map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        ),
    };
    let message = dsh_lite_core::url_policy::redact_text(&detail);
    state
        .logger
        .warn(&format!("desktop host failed: {message}"));
    ShellError::new(ErrorKind::Ready, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_directory_is_found_by_walking_up_from_a_script() {
        let root = std::env::temp_dir().join(format!("dsh-package-{}", std::process::id()));
        let package = root.join("node_modules").join("@deepseek-ai").join("dsh");
        std::fs::create_dir_all(package.join("lib")).expect("package dir");
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"0.0.0"}"#,
        )
        .expect("manifest");

        let found = walk_to_package(&package.join("lib").join("bin.js")).expect("package");
        assert_eq!(
            std::fs::canonicalize(&found).expect("found canonical"),
            std::fs::canonicalize(&package).expect("package canonical")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unrelated_directory_yields_no_package() {
        let root = std::env::temp_dir().join(format!("dsh-nopackage-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("dir");
        assert_eq!(walk_to_package(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_profile_template_is_written_once() {
        let root = std::env::temp_dir().join(format!("dsh-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let logger = Arc::new(
            dsh_lite_core::logging::Logger::open(
                &root.join("logs"),
                dsh_lite_core::config::LogLevel::Error,
                false,
            )
            .expect("logger"),
        );
        let state = AppState::new(
            root.clone(),
            None,
            logger,
            dsh_lite_core::config::Config::default(),
            dsh_lite_core::cli::RunOptions::default(),
            false,
        );
        let profile = root.join("profiles").join("desktop");
        prepare_profile(&profile, &state).expect("created");
        assert!(profile.join("package.json").is_file());

        std::fs::write(profile.join("package.json"), r#"{"name":"custom"}"#).expect("rewrite");
        prepare_profile(&profile, &state).expect("kept");
        let text = std::fs::read_to_string(profile.join("package.json")).expect("read");
        assert!(
            text.contains("custom"),
            "an existing profile is never rewritten"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
