//! Connection planning and the worker that brings `dsh web` into the window.
//!
//! One worker thread owns the whole startup path — locate `dsh`, check its
//! version, preflight the profile, read the login shell environment, start the
//! child, then load the tokenized URL — so the window thread never blocks and
//! every failure has a single place to be reported from. The token only ever
//! lives in memory: log lines, error pages and crash reports are built from the
//! redacting helpers of `dsh_lite_core::url_policy`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use url::Url;

use dsh_lite_core::cli::RunOptions;
use dsh_lite_core::config::{Config, ConnectMode, LogLevel, Platform};
use dsh_lite_core::dsh_locate::{
    self, DshCommand, LocateInputs, PreflightOutcome, parse_version, version_satisfies_engines,
};
use dsh_lite_core::env_probe::{self, EnvProbeConfig};
use dsh_lite_core::launcher::{self, SpawnRequest};
use dsh_lite_core::logging::{CrashReport, CrashSource, write_crash_report};
use dsh_lite_core::url_policy::{clean_url, is_loopback_http, redact_text, redact_url};

use crate::state::{AppState, ConnectionInfo, Status};
use crate::window::{PageOutcome, classify_page_load};

/// How long the shell waits for the redirect that consumes the token.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the first load of the token URL gets before it is tried again.
///
/// A cold `dsh` start prints its ready line the moment the port is taken, so
/// the first request can arrive before the server answers.
const AUTH_RETRY_AFTER: Duration = Duration::from_secs(8);

/// Deadline for the short `dsh --version` and `node --version` probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Install command offered by the error page for a missing `dsh`.
pub const INSTALL_COMMAND: &str = "npm install -g @deepseek-ai/dsh";

/// Install command offered when the installed `dsh` is too old.
pub const UPGRADE_COMMAND: &str = "npm install -g @deepseek-ai/dsh@latest";

/// Kinds of failure the error page knows how to explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No `dsh` executable could be resolved.
    DshNotFound,
    /// The resolved `dsh` does not satisfy the accepted version range.
    Version,
    /// The configured profile cannot be started.
    Profile,
    /// The child never announced a usable `dsh web:` URL.
    Ready,
    /// The token was not consumed by the loaded page.
    Auth,
}

impl ErrorKind {
    /// Value used in the `kind` query parameter of `error.html`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DshNotFound => "dsh-not-found",
            Self::Version => "version",
            Self::Profile => "profile",
            Self::Ready => "ready",
            Self::Auth => "auth",
        }
    }
}

/// A failure the window reports on the error page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError {
    /// Which explanation the page shows.
    pub kind: ErrorKind,
    /// Redacted detail; already free of tokens.
    pub detail: String,
}

impl ShellError {
    /// Builds a failure from a kind and an already redacted detail.
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: redact_text(&detail.into()),
        }
    }
}

/// Settings needed to start `dsh web` in managed mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Explicit executable from `--dsh-path` or `dsh.path`.
    pub dsh_path: Option<PathBuf>,
    /// Profile passed to `--profile`.
    pub profile: String,
    /// Requested port; `0` lets the operating system choose one.
    pub port: u16,
    /// Extra arguments appended after the fixed ones.
    pub extra_args: Vec<String>,
    /// Readiness deadline for the `dsh web:` line.
    pub timeout_ms: u64,
    /// Login-shell probe settings.
    pub env_probe: EnvProbeConfig,
}

/// How this launch reaches `dsh web`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionPlan {
    /// Start and own the process.
    Spawn(SpawnPlan),
    /// Load a URL the user supplied or the shell probed.
    Attach {
        /// Target to load; may carry a token.
        url: Url,
        /// Whether the URL came from `--port` or `connect.port`.
        mode: ConnectMode,
    },
}

/// Plan problems detected before any process starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The attach URL could not be parsed or is not loopback HTTP.
    InvalidUrl(String),
    /// A port-based connection was requested with port `0`.
    MissingPort,
}

impl PlanError {
    /// Maps the plan problem onto the error page.
    pub fn into_shell_error(self) -> ShellError {
        match self {
            Self::InvalidUrl(detail) => ShellError::new(ErrorKind::Auth, detail),
            Self::MissingPort => ShellError::new(
                ErrorKind::Ready,
                "port-based attachment needs a port greater than 0; pass --port <n> or set connect.port",
            ),
        }
    }
}

/// Resolves the connection plan from the configuration and the command line.
///
/// Command-line overrides win over the file, in the order the project
/// specification uses: `--url` first, then `--port`, then `connect.mode`.
pub fn resolve_plan(config: &Config, options: &RunOptions) -> Result<ConnectionPlan, PlanError> {
    let profile = options
        .profile
        .clone()
        .unwrap_or_else(|| config.dsh.profile.clone());
    let configured_path = config.dsh.path.trim();
    let dsh_path = options
        .dsh_path
        .clone()
        .or_else(|| (!configured_path.is_empty()).then(|| PathBuf::from(configured_path)));
    let mut env_probe = EnvProbeConfig::from(&config.env);
    if options.no_env_probe {
        env_probe.inherit = false;
    }

    if let Some(raw) = &options.url {
        return Ok(ConnectionPlan::Attach {
            url: attach_url(raw)?,
            mode: ConnectMode::Url,
        });
    }
    if let Some(port) = options.port {
        return Ok(ConnectionPlan::Attach {
            url: loopback_url(port)?,
            mode: ConnectMode::Port,
        });
    }

    match config.connect.mode {
        ConnectMode::Spawn => Ok(ConnectionPlan::Spawn(SpawnPlan {
            dsh_path,
            profile,
            port: config.connect.port,
            extra_args: config.dsh.extra_args.clone(),
            timeout_ms: config.dsh.start_timeout_ms,
            env_probe,
        })),
        ConnectMode::Url => Ok(ConnectionPlan::Attach {
            url: attach_url(&config.connect.url)?,
            mode: ConnectMode::Url,
        }),
        ConnectMode::Port => Ok(ConnectionPlan::Attach {
            url: loopback_url(config.connect.port)?,
            mode: ConnectMode::Port,
        }),
    }
}

/// Parses a user-supplied attach URL, rejecting anything but loopback HTTP.
pub fn attach_url(raw: &str) -> Result<Url, PlanError> {
    let url = Url::parse(raw.trim()).map_err(|error| {
        PlanError::InvalidUrl(format!("could not parse the attach URL: {error}"))
    })?;
    if !is_loopback_http(&url) {
        return Err(PlanError::InvalidUrl(format!(
            "{} is not a loopback dsh web URL",
            redact_url(url.as_str())
        )));
    }
    Ok(url)
}

/// Builds `http://127.0.0.1:<port>/` for a port-based attachment.
pub fn loopback_url(port: u16) -> Result<Url, PlanError> {
    if port == 0 {
        return Err(PlanError::MissingPort);
    }
    Url::parse(&format!("http://127.0.0.1:{port}/"))
        .map_err(|error| PlanError::InvalidUrl(format!("could not build the probe URL: {error}")))
}

/// Starts the worker that brings the connection up.
pub fn start(app: AppHandle, state: Arc<AppState>, plan: Result<ConnectionPlan, PlanError>) {
    thread::spawn(move || match plan {
        Err(error) => fail(&app, &state, error.into_shell_error()),
        Ok(plan) => connect(&app, &state, plan),
    });
}

/// Runs one connection attempt on the worker thread.
pub fn connect(app: &AppHandle, state: &Arc<AppState>, plan: ConnectionPlan) {
    state.runtime().status = Status::Connecting;
    match plan {
        ConnectionPlan::Attach { url, mode } => {
            let port = url.port_or_known_default();
            let web_url = clean_url(&url);
            if dsh_lite_core::url_policy::token_of(&url).is_none() {
                state.logger.warn(
                    "the attached URL carries no token; the shell relies on an existing session cookie",
                );
            }
            {
                let mut runtime = state.runtime();
                runtime.connection = Some(ConnectionInfo {
                    mode,
                    web_url,
                    port,
                });
            }
            navigate_token_url(app, state, url);
        }
        ConnectionPlan::Spawn(plan) => spawn_managed(app, state, &plan),
    }
}

/// Runs the managed-mode startup path.
fn spawn_managed(app: &AppHandle, state: &Arc<AppState>, plan: &SpawnPlan) {
    let config = state.config();

    // 1. Locate the executable.
    let mut inputs = LocateInputs::from_process_environment();
    inputs.cli_path = plan.dsh_path.clone();
    let configured_path = config.dsh.path.trim();
    inputs.config_path = (!configured_path.is_empty()).then(|| PathBuf::from(configured_path));
    let command = match dsh_locate::locate(&inputs) {
        Ok(command) => command,
        Err(error) => {
            return fail(
                app,
                state,
                ShellError::new(
                    ErrorKind::DshNotFound,
                    format!("{error}\n\n{INSTALL_COMMAND}"),
                ),
            );
        }
    };
    state
        .logger
        .info(&format!("located dsh: {}", redact_text(&command.display())));
    state.runtime().dsh.command = Some(redact_text(&command.display()));

    // 2. Check the version range.
    let version = run_capture(&command, &["--version".to_owned()], PROBE_TIMEOUT)
        .ok()
        .and_then(|capture| parse_version(&capture.output));
    state.runtime().dsh.version = version.clone();
    match &version {
        Some(version) if version_satisfies_engines(version) => {
            state.logger.info(&format!("dsh version {version}"));
        }
        Some(version) => {
            return fail(
                app,
                state,
                ShellError::new(
                    ErrorKind::Version,
                    format!(
                        "dsh {version} is outside the accepted range (>= {} < {}.{}.{}); upgrade with:\n\n{UPGRADE_COMMAND}",
                        version_floor(),
                        dsh_lite_core::dsh_locate::MAX_VERSION_EXCLUSIVE.0,
                        dsh_lite_core::dsh_locate::MAX_VERSION_EXCLUSIVE.1,
                        dsh_lite_core::dsh_locate::MAX_VERSION_EXCLUSIVE.2,
                    ),
                ),
            );
        }
        None => {
            return fail(
                app,
                state,
                ShellError::new(
                    ErrorKind::Version,
                    format!("could not read `dsh --version`; upgrade with:\n\n{UPGRADE_COMMAND}"),
                ),
            );
        }
    }

    // 3. Environment for the settings page.
    state.runtime().dsh.node_version = node_version();

    // 4. Preflight the profile.
    let preflight = run_capture(
        &command,
        &[
            "--profile".to_owned(),
            plan.profile.clone(),
            "--dump-config".to_owned(),
        ],
        Duration::from_millis(plan.timeout_ms),
    );
    match preflight {
        Ok(capture) => match dsh_locate::classify_preflight(capture.code, &capture.output) {
            PreflightOutcome::Ready => {
                state
                    .logger
                    .info(&format!("profile {} is available", plan.profile));
            }
            PreflightOutcome::MissingProfile { profile, hint } => {
                return fail(
                    app,
                    state,
                    ShellError::new(
                        ErrorKind::Profile,
                        format!(
                            "profile \"{profile}\" does not exist\n\n{hint}\n\nswitch to the built-in web profile, or create the profile first"
                        ),
                    ),
                );
            }
            PreflightOutcome::Failed { summary } => {
                return fail(
                    app,
                    state,
                    ShellError::new(
                        ErrorKind::Profile,
                        format!(
                            "profile \"{}\" could not be validated\n\n{summary}",
                            plan.profile
                        ),
                    ),
                );
            }
        },
        Err(detail) => {
            return fail(
                app,
                state,
                ShellError::new(
                    ErrorKind::Profile,
                    format!("could not validate profile \"{}\": {detail}", plan.profile),
                ),
            );
        }
    }

    // 5. Environment for the child.
    let own: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let environment = env_probe::collect(Platform::current(), &plan.env_probe, &own);
    state.logger.info(&format!(
        "child environment: {:?}, {} variables, {} overridden",
        environment.report.source, environment.report.variable_count, environment.report.overridden
    ));
    for warning in &environment.report.warnings {
        state.logger.warn(&format!("environment probe: {warning}"));
    }

    // 6. Start the child and wait for its readiness line.
    let request = SpawnRequest {
        command: command.clone(),
        profile: plan.profile.clone(),
        port: plan.port,
        extra_args: plan.extra_args.clone(),
        env: environment.vars,
    };
    let arguments: Vec<String> = launcher::build_args(&request)
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    state.logger.info(&format!(
        "starting {} {}",
        command.display(),
        arguments.join(" ")
    ));

    let ready = match launcher::spawn(&request, Duration::from_millis(plan.timeout_ms)) {
        Ok(ready) => ready,
        Err(error) => {
            return fail(
                app,
                state,
                ShellError::new(ErrorKind::Ready, format!("{error}")),
            );
        }
    };

    let url = ready.url.clone();
    state.logger.info(&format!(
        "dsh is ready (pid {}) at {}",
        ready.child.pid(),
        redact_url(url.as_str())
    ));

    let port = url.port_or_known_default();
    let web_url = clean_url(&url);
    {
        let mut runtime = state.runtime();
        runtime.child = Some(ready.child);
        runtime.connection = Some(ConnectionInfo {
            mode: ConnectMode::Spawn,
            web_url,
            port,
        });
    }

    navigate_token_url(app, state, url);
}

/// Loads the tokenized URL and arms the authentication check.
pub fn navigate_token_url(app: &AppHandle, state: &Arc<AppState>, url: Url) {
    state.runtime().pending_auth = Some(url.clone());
    state
        .logger
        .info(&format!("loading {}", redact_url(url.as_str())));

    if let Some(content) = app.get_webview(crate::window::CONTENT_WEBVIEW)
        && let Err(error) = content.navigate(url.clone())
    {
        fail(
            app,
            state,
            ShellError::new(ErrorKind::Ready, format!("could not load the URL: {error}")),
        );
        return;
    }

    arm_auth_watchdog(app.clone(), Arc::clone(state), url);
}

/// Reports an authentication failure when the token is still pending after
/// [`AUTH_TIMEOUT`], which covers a page that never reported a finished load.
fn arm_auth_watchdog(app: AppHandle, state: Arc<AppState>, watched: Url) {
    thread::spawn(move || {
        thread::sleep(AUTH_RETRY_AFTER);
        if !auth_still_pending(&state, &watched) {
            return;
        }
        // `dsh web:` is printed as soon as the server claims the port, so the
        // very first request can still race the listener on a cold start and
        // never report a finished load. Loading the same token URL again is
        // harmless: the token is consumed at most once, and a page that did
        // authenticate cleared `pending_auth` before this line.
        state
            .logger
            .warn("no page load was reported for the token URL; loading it once more");
        if let Some(content) = app.get_webview(crate::window::CONTENT_WEBVIEW)
            && let Err(error) = content.navigate(watched.clone())
        {
            state
                .logger
                .warn(&format!("the second attempt could not start: {error}"));
        }

        thread::sleep(AUTH_TIMEOUT - AUTH_RETRY_AFTER);
        if auth_still_pending(&state, &watched) {
            auth_failure(
                &app,
                &state,
                "the shell never saw the token consumed; reload with a fresh `dsh web:` URL",
            );
        }
    });
}

/// Whether the token of `watched` is still waiting to be consumed.
fn auth_still_pending(state: &Arc<AppState>, watched: &Url) -> bool {
    let runtime = state.runtime();
    runtime
        .pending_auth
        .as_ref()
        .is_some_and(|pending| dsh_lite_core::url_policy::shares_target(pending, watched))
}

/// Applies the FR-3 decision to a finished page load.
pub fn handle_page_load(app: &AppHandle, loaded: &Url) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let (pending, shell_origin) = {
        let runtime = state.runtime();
        (runtime.pending_auth.clone(), runtime.shell_origin.clone())
    };
    match classify_page_load(pending.as_ref(), shell_origin.as_ref(), loaded) {
        PageOutcome::Shell => {
            state
                .logger
                .debug(&format!("shell page loaded: {}", loaded.as_str()));
            // Do not overwrite `local_base` with the loopback http loading
            // origin: error/settings pages must stay on the Tauri shell origin
            // so their `invoke` works. `local_base` is seeded from
            // `default_shell_base()` at startup.
        }
        PageOutcome::Authenticated { url } => {
            let mut runtime = state.runtime();
            runtime.pending_auth = None;
            runtime.status = Status::Ready;
            drop(runtime);
            crate::platform::set_connection_ready(app, true);
            state.logger.info(&format!(
                "token consumed; the window authenticated at {}",
                redact_url(url.as_str())
            ));
        }
        PageOutcome::AuthFailed { detail } => auth_failure(app, &state, &detail),
        PageOutcome::InApp => {
            let mut runtime = state.runtime();
            runtime.pending_auth = None;
            runtime.status = Status::Ready;
            drop(runtime);
            crate::platform::set_connection_ready(app, true);
        }
        PageOutcome::External => {}
    }
}

/// Marks the connection as failed and shows the error page.
pub fn fail(app: &AppHandle, state: &Arc<AppState>, error: ShellError) {
    let (dsh_version, local_base) = {
        let runtime = state.runtime();
        (runtime.dsh.version.clone(), runtime.local_base.clone())
    };
    state.logger.error(&format!(
        "connection failed ({}): {}",
        error.kind.as_str(),
        error.detail
    ));
    {
        let mut runtime = state.runtime();
        runtime.status = Status::Failed;
        runtime.pending_auth = None;
        runtime.last_error = Some(error.clone());
    }
    crate::platform::set_connection_ready(app, false);
    let _ = write_crash_report(
        &state.log_dir,
        &CrashReport {
            source: CrashSource::Startup,
            error_chain: vec![error.detail.clone()],
            shell_version: dsh_lite_core::VERSION.to_owned(),
            dsh_version,
            platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        },
    );
    navigate_shell_page(
        app,
        local_base.as_ref(),
        "error.html",
        &[
            ("kind", error.kind.as_str()),
            ("detail", error.detail.as_str()),
        ],
    );
}

/// Stops the worker-visible connection and records the authentication failure.
pub fn auth_failure(app: &AppHandle, state: &Arc<AppState>, detail: &str) {
    fail(app, state, ShellError::new(ErrorKind::Auth, detail));
}

/// Navigates the main window to a page shipped with the shell.
pub fn navigate_shell_page(
    app: &AppHandle,
    base: Option<&Url>,
    page: &str,
    query: &[(&str, &str)],
) {
    // Leaving for a page the shell owns abandons whatever token handshake was
    // in flight. Without this, a user who opens the settings page while `dsh`
    // is still starting would be told authentication failed twenty seconds
    // later — for a page they navigated away from on purpose.
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.runtime().pending_auth = None;
    }
    let base = base
        .cloned()
        .unwrap_or_else(crate::window::default_shell_base);
    let url = crate::window::shell_page_url_with_query(&base, page, query);
    match app.get_webview(crate::window::CONTENT_WEBVIEW) {
        Some(content) => match content.navigate(url.clone()) {
            Ok(()) => state_free_log(app, LogLevel::Debug, &format!("showing {url}")),
            Err(error) => state_free_log(
                app,
                LogLevel::Warn,
                &format!("could not show {url}: {error}"),
            ),
        },
        None => state_free_log(
            app,
            LogLevel::Warn,
            "the main window is gone; cannot show a shell page",
        ),
    }
}

/// Logs without borrowing the state, for callbacks that may run during teardown.
fn state_free_log(app: &AppHandle, level: LogLevel, message: &str) {
    match app.try_state::<Arc<AppState>>() {
        Some(state) => state.logger.record(level, message),
        None => eprintln!("{}: {message}", dsh_lite_core::PRODUCT_NAME),
    }
}

/// Stops the sponsored child, if any.
pub fn stop_child(state: &Arc<AppState>) {
    if let Some(mut child) = state.take_child() {
        let pid = child.pid();
        state.logger.info(&format!("stopping dsh (pid {pid})"));
        child.stop(launcher::DEFAULT_STOP_GRACE);
        state.logger.info(&format!("dsh (pid {pid}) stopped"));
    }
}

/// Shows the shell's loading page on the loopback http origin before a
/// reconnect.
///
/// The loading page and `dsh web` share the loopback http origin (same host;
/// SameSite ignores the port), so navigating from it to the fresh token URL is
/// same-site and the `SameSite=Strict` auth cookie survives the token redirect.
/// Navigating from the Tauri origin (`tauri://localhost`) instead would be
/// cross-site and land on the 401 page. `default_shell_base()` is that Tauri
/// origin, so it must not be used here.
pub fn show_loading_page(app: &AppHandle, state: &Arc<AppState>) {
    let origin = state.runtime().shell_origin.clone();
    let base = crate::window::loading_page_base(origin.as_ref());
    navigate_shell_page(app, Some(&base), "loading.html", &[]);
}

/// Restarts the connection with the current configuration.
pub fn restart(app: &AppHandle, state: &Arc<AppState>) {
    stop_child(state);
    state.runtime().pending_auth = None;
    show_loading_page(app, state);
    let plan = resolve_plan(&state.config(), &state.cli);
    start(app.clone(), Arc::clone(state), plan);
}

/// Replaces the current connection with a URL the user supplied.
pub fn switch_to_attach(app: &AppHandle, state: &Arc<AppState>, url: Url, mode: ConnectMode) {
    stop_child(state);
    state.runtime().pending_auth = None;
    connect(app, state, ConnectionPlan::Attach { url, mode });
}

/// Version floor rendered in the error page, taken from the core crate.
fn version_floor() -> String {
    let (major, minor, patch) = dsh_lite_core::dsh_locate::MIN_VERSION;
    format!("{major}.{minor}.{patch}")
}

/// Runs a short probe command and returns its exit code and combined output.
///
/// Output is redirected to a temporary file rather than a pipe: a probe that
/// floods its pipe while the shell only waits for exit would otherwise block
/// forever, and a file also keeps the wait bounded by the deadline.
pub(crate) fn run_capture(
    command: &DshCommand,
    args: &[String],
    timeout: Duration,
) -> Result<Capture, String> {
    let path = std::env::temp_dir().join(format!(
        "dsh-desktop-probe-{}-{}.log",
        std::process::id(),
        probe_counter()
    ));
    let file = File::create(&path).map_err(|error| error.to_string())?;
    let clone = file.try_clone().map_err(|error| error.to_string())?;

    let mut process = Command::new(command.program());
    process
        .args(command.prefix_args())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(clone));
    crate::platform::hide_console(&mut process);

    let mut child = process.spawn().map_err(|error| error.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // `npm`/`cmd.exe` answer in the OEM code page on Windows, and
                // `read_to_string` would turn every non-UTF-8 byte into an
                // empty capture. Decode tolerantly instead.
                let output = fs::read(&path)
                    .map(|bytes| dsh_lite_core::console_text::decode(&bytes))
                    .unwrap_or_default();
                let _ = fs::remove_file(&path);
                return Ok(Capture {
                    code: status.code().unwrap_or(-1),
                    output,
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error.to_string());
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&path);
            return Err(format!("timed out after {} ms", timeout.as_millis()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Result of one probe command.
pub(crate) struct Capture {
    pub(crate) code: i32,
    pub(crate) output: String,
}

/// Distinguishes temporary files of concurrent probes in one process.
fn probe_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Version of the `node` found on `PATH`, for the settings page.
fn node_version() -> Option<String> {
    run_capture(
        &DshCommand::new("node", Vec::new()),
        &["--version".to_owned()],
        PROBE_TIMEOUT,
    )
    .ok()
    .and_then(|capture| parse_version(&capture.output))
}

/// Runs a short helper command such as `npm view` and returns its exit code
/// with the combined output, so the shell can report a new `dsh` release
/// without installing anything.
///
/// `prefix_args` carry the wrapper a script shim needs; callers obtain them
/// from [`crate::platform::shell_program`] instead of passing a bare `.cmd`
/// name to `CreateProcess`.
pub fn helper_output(
    program: &str,
    prefix_args: &[String],
    args: &[String],
    timeout: Duration,
) -> Result<(i32, String), String> {
    run_capture(
        &DshCommand::new(program, prefix_args.to_vec()),
        args,
        timeout,
    )
    .map(|capture| (capture.code, capture.output))
}

/// Whether `latest` is a newer release than `installed`.
///
/// Delegates to `dsh_lite_core::update` so the Tauri shell and the WinUI shell
/// cannot drift on what counts as an update.
pub use dsh_lite_core::update::update_is_newer;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    fn options() -> RunOptions {
        RunOptions::default()
    }

    #[test]
    fn the_default_configuration_starts_a_managed_dsh() {
        let plan = resolve_plan(&config(), &options()).expect("plan");
        let ConnectionPlan::Spawn(spawn) = plan else {
            panic!("expected a managed spawn");
        };
        assert_eq!(spawn.profile, "web");
        assert_eq!(spawn.port, 0);
        assert!(spawn.env_probe.inherit);
    }

    #[test]
    fn a_command_line_url_attaches_and_wins_over_the_file() {
        let mut options = options();
        options.url = Some("http://127.0.0.1:3080/?token=abc".to_owned());
        let plan = resolve_plan(&config(), &options).expect("plan");
        let ConnectionPlan::Attach { url, mode } = plan else {
            panic!("expected an attachment");
        };
        assert_eq!(url.as_str(), "http://127.0.0.1:3080/?token=abc");
        assert_eq!(mode, ConnectMode::Url);
    }

    #[test]
    fn a_command_line_port_attaches_to_the_loopback_port() {
        let mut options = options();
        options.port = Some(3080);
        let ConnectionPlan::Attach { url, mode } = resolve_plan(&config(), &options).expect("plan")
        else {
            panic!("expected an attachment");
        };
        assert_eq!(url.as_str(), "http://127.0.0.1:3080/");
        assert_eq!(mode, ConnectMode::Port);

        options.port = Some(0);
        assert_eq!(
            resolve_plan(&config(), &options),
            Err(PlanError::MissingPort),
            "port 0 cannot be probed"
        );
    }

    #[test]
    fn a_dsh_path_and_profile_override_the_file_and_the_probe_opt_out_is_kept() {
        let mut configured = config();
        configured.dsh.path = "  ".to_owned();
        let mut overrides = options();
        overrides.dsh_path = Some(PathBuf::from("/opt/dsh/bin/dsh"));
        overrides.profile = Some("custom".to_owned());
        overrides.no_env_probe = true;

        let ConnectionPlan::Spawn(spawn) = resolve_plan(&configured, &overrides).expect("plan")
        else {
            panic!("expected a managed spawn");
        };
        assert_eq!(spawn.dsh_path, Some(PathBuf::from("/opt/dsh/bin/dsh")));
        assert_eq!(spawn.profile, "custom");
        assert!(!spawn.env_probe.inherit);

        let mut configured = config();
        configured.dsh.path = "C:\\tools\\dsh.exe".to_owned();
        let ConnectionPlan::Spawn(spawn) = resolve_plan(&configured, &options()).expect("plan")
        else {
            panic!("expected a managed spawn");
        };
        assert_eq!(spawn.dsh_path, Some(PathBuf::from("C:\\tools\\dsh.exe")));
    }

    #[test]
    fn the_configuration_file_can_select_url_and_port_modes() {
        let mut configured = config();
        configured.connect.mode = ConnectMode::Url;
        configured.connect.url = "http://127.0.0.1:3080/?token=abc".to_owned();
        let ConnectionPlan::Attach { mode, .. } =
            resolve_plan(&configured, &options()).expect("plan")
        else {
            panic!("expected an attachment");
        };
        assert_eq!(mode, ConnectMode::Url);

        configured.connect.mode = ConnectMode::Port;
        configured.connect.port = 3080;
        let ConnectionPlan::Attach { mode, url } =
            resolve_plan(&configured, &options()).expect("plan")
        else {
            panic!("expected an attachment");
        };
        assert_eq!(mode, ConnectMode::Port);
        assert_eq!(url.as_str(), "http://127.0.0.1:3080/");

        configured.connect.port = 0;
        assert_eq!(
            resolve_plan(&configured, &options()),
            Err(PlanError::MissingPort)
        );
    }

    #[test]
    fn a_non_loopback_attach_url_is_refused_without_leaking_it() {
        let mut options = options();
        options.url = Some("https://example.com/?token=secret".to_owned());
        let error = resolve_plan(&config(), &options).expect_err("refused");
        let shell_error = error.into_shell_error();
        assert_eq!(shell_error.kind, ErrorKind::Auth);
        assert!(!shell_error.detail.contains("secret"), "{:?}", shell_error);
    }

    #[test]
    fn localhost_is_not_accepted_as_a_loopback_target() {
        let mut options = options();
        options.url = Some("http://localhost:3080/?token=abc".to_owned());
        assert!(resolve_plan(&config(), &options).is_err());
    }

    #[test]
    fn a_bare_string_is_not_an_attach_url() {
        let mut options = options();
        options.url = Some("not a url".to_owned());
        let error = resolve_plan(&config(), &options).expect_err("refused");
        assert_eq!(error.into_shell_error().kind, ErrorKind::Auth);
    }

    #[test]
    fn error_kinds_map_to_the_error_page_values() {
        assert_eq!(ErrorKind::DshNotFound.as_str(), "dsh-not-found");
        assert_eq!(ErrorKind::Version.as_str(), "version");
        assert_eq!(ErrorKind::Profile.as_str(), "profile");
        assert_eq!(ErrorKind::Ready.as_str(), "ready");
        assert_eq!(ErrorKind::Auth.as_str(), "auth");
    }

    #[test]
    fn failure_details_are_redacted_before_they_reach_the_page() {
        let error = ShellError::new(ErrorKind::Ready, "seed?token=secret failed");
        assert!(!error.detail.contains("secret"), "{:?}", error);
        assert!(error.detail.contains("<redacted>"), "{:?}", error);
    }

    #[test]
    fn only_a_higher_release_counts_as_an_update() {
        assert!(update_is_newer("0.2.2", "0.2.1-rc.2"));
        assert!(update_is_newer("0.3.0", "0.2.9"));
        assert!(update_is_newer("1.0.0", "0.9.9"));
        assert!(!update_is_newer("0.2.1", "0.2.1-rc.2"));
        assert!(!update_is_newer("0.1.9", "0.2.0"));
        assert!(!update_is_newer("not a version", "0.2.0"));
        assert!(!update_is_newer("0.2.2", "unknown"));
    }
}
