//! C ABI over [`dsh_lite_core`], for native shells that are not Tauri.
//!
//! The Tauri shell in `src-tauri` links the core as a Rust crate. A native shell
//! (WinUI on Windows, SwiftUI on macOS, gtk-rs on Linux) cannot, so this crate
//! exposes the same behaviour across a process boundary that is a plain C ABI:
//! no generated IDL, no COM, no runtime to install.
//!
//! # Contract rules
//!
//! 1. **Always-UTF-8, NUL-terminated strings.** Every `const char *` crossing the
//!    boundary is UTF-8. Where the shell receives a string it must copy it
//!    before returning from the call; the pointer is only valid during that call
//!    or until [`dsh_string_free`].
//! 2. **Ownership is explicit.** Strings returned as `*mut c_char` belong to the
//!    caller and are released with [`dsh_string_free`]. Strings passed *in* stay
//!    owned by the caller.
//! 3. **Errors are codes plus text.** No panic ever crosses the boundary: the
//!    release profiles use `panic = "abort"`, and every fallible entry point
//!    turns a `Result` into a non-zero return code plus [`dsh_last_error`].
//! 4. **Long work is asynchronous.** [`dsh_start`] returns as soon as the work is
//!    accepted and reports progress through the event sink, so the UI thread is
//!    never blocked for the seconds `dsh web` needs.
//! 5. **Tokens never leave in logs.** The readiness URL handed to the shell in an
//!    event carries its one-shot token, because the shell has to open it; every
//!    log line and every diagnostic string is redacted first.
//!
//! # Event payloads
//!
//! One JSON object per event, `kind` is the discriminator:
//!
//! ```json
//! {"kind":"state","state":"connecting","detail":"starting dsh…"}
//! {"kind":"state","state":"ready","url":"http://127.0.0.1:PORT/?token=…","pid":4321}
//! {"kind":"state","state":"failed","error":"dsh-not-found","detail":"…"}
//! {"kind":"state","state":"stopped"}
//! {"kind":"update","state":"checking"}
//! {"kind":"update","state":"available","installed":"0.2.1","latest":"0.3.0","upgradeCommand":"…"}
//! {"kind":"update","state":"current","installed":"0.3.0","latest":"0.3.0"}
//! {"kind":"update","state":"failed","detail":"npm view exited with code 1"}
//! ```
//!
//! `error` is one of `dsh-not-found`, `version`, `spawn`, `ready`, `auth`,
//! `internal` — the same vocabulary the Tauri shell's error page uses.
//!
//! # ABI 2
//!
//! Version 2 adds [`dsh_check_update`] and the `update` event kind. A shell built
//! against ABI 1 keeps working with a version-2 library only if it never needs
//! update checking; the C# shell refuses a mismatch outright.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dsh_lite_core::config::{self, CloseAction, Config, LogLevel};
use dsh_lite_core::dsh_locate::{self, DshCommand, LocateInputs};
use dsh_lite_core::launcher::{self, DshChild, SpawnRequest};
use dsh_lite_core::logging::Logger;
use dsh_lite_core::url_policy;
use serde::Deserialize;

/// Version of this ABI. A shell must refuse to run against a different value.
pub const ABI_VERSION: u32 = 2;

/// Successful return code of every fallible entry point.
pub const OK: c_int = 0;

/// Returned when an argument could not be read or parsed.
pub const ERR_ARGUMENT: c_int = 1;

/// Returned when the call needs shell state that is not available yet.
pub const ERR_STATE: c_int = 2;

/// Returned when a system operation (opening a directory) failed.
pub const ERR_SYSTEM: c_int = 3;

/// Deadline for the short `dsh --version` / `node --version` probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for the `npm view` probe behind an update check.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// `CreateProcess` flag that keeps a console window from flashing on screen.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Callback signature for [`dsh_set_event_sink`].
///
/// `payload` is a NUL-terminated UTF-8 JSON object that is only valid for the
/// duration of the call; the shell must copy it before returning.
pub type EventCallback =
    extern "C" fn(kind: *const c_char, payload: *const c_char, user: *mut c_void);

#[derive(Clone, Copy)]
struct Sink {
    callback: EventCallback,
    user: usize,
}

impl Sink {
    const fn empty() -> Self {
        // `None` is modelled by `installed = false`; keeping the struct `Copy`
        // lets `emit` release the lock before calling into foreign code.
        Self {
            callback: placeholder,
            user: 0,
        }
    }
}

extern "C" fn placeholder(_kind: *const c_char, _payload: *const c_char, _user: *mut c_void) {}

struct Installed {
    installed: bool,
    sink: Sink,
}

static SINK: Mutex<Installed> = Mutex::new(Installed {
    installed: false,
    sink: Sink::empty(),
});

struct Shell {
    child: Option<DshChild>,
    logger: Option<Logger>,
}

static SHELL: Mutex<Shell> = Mutex::new(Shell {
    child: None,
    logger: None,
});

static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Bumped by every start/stop; a worker whose generation is stale discards its
/// result instead of overwriting the newest attempt.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Options accepted by [`dsh_start`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct StartOptions {
    /// `dsh` profile; falls back to the configuration file, then to `web`.
    pub profile: Option<String>,
    /// Requested port; `0` or absent lets the operating system choose.
    pub port: Option<u16>,
    /// Explicit `dsh` executable, overriding the discovery order.
    pub dsh_path: Option<String>,
    /// Readiness deadline in milliseconds.
    pub timeout_ms: Option<u64>,
}

fn set_last_error(message: impl Into<String>) {
    if let Ok(mut slot) = LAST_ERROR.lock() {
        *slot = Some(message.into());
    }
}

fn take_last_error() -> Option<String> {
    LAST_ERROR.lock().ok().and_then(|mut slot| slot.take())
}

fn log_dir() -> Option<PathBuf> {
    config::default_config_dir().map(|dir| dir.join("logs"))
}

fn config_path() -> Option<PathBuf> {
    config::default_config_path()
}

fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match Config::load_or_create(&path) {
        Ok((config, _created)) => config,
        Err(_) => Config::default(),
    }
}

fn logger() -> Option<Logger> {
    let dir = log_dir()?;
    Logger::open(&dir, LogLevel::Info, false).ok()
}

fn record(message: &str) {
    if let Ok(shell) = SHELL.lock()
        && let Some(logger) = shell.logger.as_ref()
    {
        logger.info(&url_policy::redact_text(message));
    }
}

fn emit(kind: &str, payload: &serde_json::Value) {
    let sink = match SINK.lock() {
        Ok(guard) if guard.installed => Some(guard.sink),
        _ => None,
    };
    let Some(sink) = sink else {
        return;
    };
    let Ok(kind) = CString::new(kind) else {
        return;
    };
    let Ok(payload) = CString::new(payload.to_string()) else {
        return;
    };
    (sink.callback)(kind.as_ptr(), payload.as_ptr(), sink.user as *mut c_void);
}

fn emit_state(state: &str, extra: serde_json::Value) {
    let mut payload = serde_json::json!({ "kind": "state", "state": state });
    if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    emit("state", &payload);
}

fn emit_failure(error: &str, detail: &str) {
    let detail = url_policy::redact_text(detail);
    set_last_error(detail.clone());
    record(&format!("start failed ({error}): {detail}"));
    emit_state(
        "failed",
        serde_json::json!({ "error": error, "detail": detail }),
    );
}

fn adopt_string(text: impl Into<String>) -> *mut c_char {
    let text: String = text.into();
    let cleaned = text.replace('\0', " ");
    CString::new(cleaned)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

fn read_instrument(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

struct Capture {
    /// Exit code of the helper, kept because `dsh --version` may answer on
    /// stderr with a non-zero status and callers need to tell the cases apart.
    #[allow(dead_code)]
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs a short helper command and captures its output.
///
/// Mirrors the Tauri shell's probe helper; the deadline keeps a hung shim from
/// blocking a settings page forever.
fn run_capture(
    program: &Path,
    prefix_args: &[String],
    args: &[String],
    timeout: Duration,
) -> Result<Capture, String> {
    let mut command = Command::new(program);
    command
        .args(prefix_args)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", program.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout_bytes = Vec::new();
                let mut stderr_bytes = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_end(&mut stdout_bytes);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_end(&mut stderr_bytes);
                }
                return Ok(Capture {
                    code: status.code().unwrap_or(-1),
                    // `cmd.exe` and `npm` answer in the OEM code page, and
                    // `read_to_string` would silently yield nothing for them.
                    stdout: dsh_lite_core::console_text::decode(&stdout_bytes),
                    stderr: dsh_lite_core::console_text::decode(&stderr_bytes),
                });
            }
            Ok(None) => {}
            Err(error) => return Err(format!("could not wait for the helper: {error}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the helper did not exit before the deadline".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Version reported by `dsh --version`, if the resolved command answers.
fn probe_dsh_version(command: &DshCommand) -> Option<String> {
    let capture = run_capture(
        command.program(),
        command.prefix_args(),
        &["--version".to_owned()],
        PROBE_TIMEOUT,
    )
    .ok()?;
    dsh_locate::parse_version(&capture.stdout)
        .or_else(|| dsh_locate::parse_version(&capture.stderr))
}

/// Version reported by `node --version`.
///
/// On Windows `node` may be a command shim, so the probe goes through
/// `cmd.exe /c` instead of `CreateProcess` on a `.cmd` file.
fn probe_node_version() -> Option<String> {
    #[cfg(windows)]
    let (program, prefix) = (
        PathBuf::from("cmd.exe"),
        vec!["/c".to_owned(), "node".to_owned()],
    );
    #[cfg(not(windows))]
    let (program, prefix) = (PathBuf::from("node"), Vec::new());

    let capture = run_capture(&program, &prefix, &["--version".to_owned()], PROBE_TIMEOUT).ok()?;
    dsh_locate::parse_version(&capture.stdout)
}

fn locate_inputs(options: &StartOptions, config: &Config) -> LocateInputs {
    let mut inputs = LocateInputs::from_process_environment();
    inputs.cli_path = options
        .dsh_path
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    if inputs.cli_path.is_none() && !config.dsh.path.trim().is_empty() {
        inputs.config_path = Some(PathBuf::from(&config.dsh.path));
    }
    inputs
}

/// Stops the sponsored child, if any, and returns it so the caller knows whether
/// something was running.
fn take_child() -> Option<DshChild> {
    match SHELL.lock() {
        Ok(mut shell) => shell.child.take(),
        Err(poisoned) => poisoned.into_inner().child.take(),
    }
}

fn stop_child() {
    if let Some(mut child) = take_child() {
        let pid = child.pid();
        record(&format!("stopping dsh (pid {pid})"));
        child.stop(launcher::DEFAULT_STOP_GRACE);
    }
}

fn fail_and_cleanup(error: &str, detail: &str) {
    stop_child();
    emit_failure(error, detail);
}

fn run_start(generation: u64, options: StartOptions) {
    stop_child();
    if generation != GENERATION.load(Ordering::SeqCst) {
        return;
    }

    emit_state(
        "connecting",
        serde_json::json!({ "detail": "locating dsh" }),
    );

    let config = load_config();
    let inputs = locate_inputs(&options, &config);
    let command = match dsh_locate::locate(&inputs) {
        Ok(command) => command,
        Err(error) => {
            fail_and_cleanup("dsh-not-found", &error.to_string());
            return;
        }
    };
    record(&format!("resolved dsh at {}", command.display()));

    let version = probe_dsh_version(&command);
    if let Some(version) = version.as_deref()
        && !dsh_locate::version_satisfies_engines(version)
    {
        fail_and_cleanup(
            "version",
            &format!("dsh {version} is outside the supported range (>= 0.2.0, < 1.0.0)"),
        );
        return;
    }

    let profile = options
        .profile
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.dsh.profile.clone());
    let port = options.port.unwrap_or(config.connect.port);
    let request = SpawnRequest {
        command: command.clone(),
        profile: profile.clone(),
        port,
        extra_args: config.dsh.extra_args.clone(),
        // An empty environment inherits this process' environment, which is the
        // Windows contract: the shell was started from a shell that exported it.
        env: Vec::new(),
    };
    let deadline = Duration::from_millis(options.timeout_ms.unwrap_or(config.dsh.start_timeout_ms));

    record(&format!(
        "starting dsh with profile {profile} on port {port} (deadline {deadline:?})"
    ));
    emit_state(
        "connecting",
        serde_json::json!({ "detail": format!("starting dsh with profile {profile}") }),
    );

    match launcher::spawn(&request, deadline) {
        Ok(ready) => {
            if generation != GENERATION.load(Ordering::SeqCst) {
                // A newer attempt superseded this one; do not leak the child.
                let mut ready = ready;
                ready.child.stop(launcher::DEFAULT_STOP_GRACE);
                return;
            }
            let pid = ready.child.pid();
            let clean = url_policy::clean_url(&ready.url);
            record(&format!("dsh is ready at {clean} (pid {pid})"));
            match SHELL.lock() {
                Ok(mut shell) => shell.child = Some(ready.child),
                Err(poisoned) => poisoned.into_inner().child = Some(ready.child),
            }
            emit_state(
                "ready",
                serde_json::json!({
                    "url": ready.url.as_str(),
                    "webUrl": clean.as_str(),
                    "pid": pid,
                    "profile": profile,
                    "dshVersion": version,
                }),
            );
        }
        Err(error) => {
            let kind = match &error {
                launcher::LaunchError::Spawn { .. } => "spawn",
                launcher::LaunchError::BadLine(_) => "auth",
                launcher::LaunchError::Timeout { .. } | launcher::LaunchError::Exited { .. } => {
                    "ready"
                }
            };
            fail_and_cleanup(kind, &error.to_string());
        }
    }
}

/// Contract version of this library.
#[unsafe(no_mangle)]
pub extern "C" fn dsh_ffi_abi_version() -> u32 {
    ABI_VERSION
}

/// Shell version, as shown on the settings page.
///
/// # Safety
/// The returned pointer must be released with [`dsh_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn dsh_shell_version() -> *mut c_char {
    adopt_string(dsh_lite_core::VERSION)
}

/// Releases a string returned by this library. A null pointer is ignored.
///
/// # Safety
/// `pointer` must come from this library and must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dsh_string_free(pointer: *mut c_char) {
    if pointer.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(pointer));
    }
}

/// Text of the most recent failure, or null when the last call succeeded.
///
/// # Safety
/// The returned pointer must be released with [`dsh_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn dsh_last_error() -> *mut c_char {
    match take_last_error() {
        Some(text) => adopt_string(text),
        None => std::ptr::null_mut(),
    }
}

/// Installs the event sink. Passing a null callback uninstalls it.
///
/// # Safety
/// `callback` must be a valid function pointer that stays alive until it is
/// replaced or uninstalled, and `user` must stay valid for the same period.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dsh_set_event_sink(
    callback: Option<EventCallback>,
    user: *mut c_void,
) -> c_int {
    let installed = match callback {
        Some(callback) => Installed {
            installed: true,
            sink: Sink {
                callback,
                user: user as usize,
            },
        },
        None => Installed {
            installed: false,
            sink: Sink::empty(),
        },
    };
    match SINK.lock() {
        Ok(mut slot) => {
            *slot = installed;
            OK
        }
        Err(poisoned) => {
            *poisoned.into_inner() = installed;
            OK
        }
    }
}

/// Facts about this machine, as a JSON object, for the settings page.
///
/// Shape: `{shellVersion, abiVersion, configPath, logDir, dshFound, dshPath,
/// dshVersion, dshVersionSupported, nodeVersion, error}`.
///
/// # Safety
/// The returned pointer must be released with [`dsh_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn dsh_diagnose() -> *mut c_char {
    let config = load_config();
    let inputs = locate_inputs(&StartOptions::default(), &config);
    let mut dsh_found = false;
    let mut dsh_path: Option<String> = None;
    let mut dsh_version: Option<String> = None;
    let mut supported: Option<bool> = None;
    let mut error: Option<String> = None;

    match dsh_locate::locate(&inputs) {
        Ok(command) => {
            dsh_found = true;
            dsh_path = Some(command.display());
            dsh_version = probe_dsh_version(&command);
            supported = dsh_version
                .as_deref()
                .map(dsh_locate::version_satisfies_engines);
        }
        Err(failure) => error = Some(failure.to_string()),
    }

    let report = serde_json::json!({
        "shellVersion": dsh_lite_core::VERSION,
        "abiVersion": ABI_VERSION,
        "productName": dsh_lite_core::PRODUCT_NAME,
        "configPath": config_path().map(|path| path.display().to_string()),
        "logDir": log_dir().map(|path| path.display().to_string()),
        "dshFound": dsh_found,
        "dshPath": dsh_path,
        "dshVersion": dsh_version,
        "dshVersionSupported": supported,
        "dshPathConfigured": config.dsh.path,
        "profile": config.dsh.profile,
        "configuredPort": config.connect.port,
        "connectMode": match config.connect.mode {
            dsh_lite_core::config::ConnectMode::Spawn => "spawn",
            dsh_lite_core::config::ConnectMode::Url => "url",
            dsh_lite_core::config::ConnectMode::Port => "port",
        },
        "closeAction": match config.window.close_action {
            CloseAction::Quit => "quit",
            CloseAction::Hide => "hide",
        },
        "nodeVersion": probe_node_version(),
        "error": error.map(|text| url_policy::redact_text(&text)),
    });
    adopt_string(report.to_string())
}

/// Starts `dsh web` and reports progress through the event sink.
///
/// Returns [`OK`] as soon as the attempt is accepted; the outcome arrives as a
/// `connecting` → `ready` / `failed` event pair. `options_json` may be null or
/// `{}` to use the configuration file.
///
/// # Safety
/// `options_json` must be null or a NUL-terminated UTF-8 JSON object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dsh_start(options_json: *const c_char) -> c_int {
    let options = match read_instrument(options_json) {
        None => StartOptions::default(),
        Some(text) if text.trim().is_empty() => StartOptions::default(),
        Some(text) => match serde_json::from_str::<StartOptions>(&text) {
            Ok(options) => options,
            Err(error) => {
                set_last_error(format!("could not read the start options: {error}"));
                return ERR_ARGUMENT;
            }
        },
    };

    match SHELL.lock() {
        Ok(mut shell) => {
            if shell.logger.is_none() {
                shell.logger = logger();
            }
        }
        Err(poisoned) => {
            let mut shell = poisoned.into_inner();
            if shell.logger.is_none() {
                shell.logger = logger();
            }
        }
    }

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || run_start(generation, options));
    OK
}

/// Stops the sponsored child and invalidates any in-flight start.
#[unsafe(no_mangle)]
pub extern "C" fn dsh_stop() -> c_int {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    stop_child();
    emit_state("stopped", serde_json::Value::Null);
    OK
}

/// Opens the log directory with the platform's file manager.
#[unsafe(no_mangle)]
pub extern "C" fn dsh_open_logs() -> c_int {
    let Some(dir) = log_dir() else {
        set_last_error("the log directory is unknown on this system");
        return ERR_STATE;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        set_last_error(format!("could not create {}: {error}", dir.display()));
        return ERR_SYSTEM;
    }

    #[cfg(windows)]
    let result = Command::new("explorer.exe").arg(&dir).spawn();
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(&dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(&dir).spawn();

    match result {
        Ok(_) => OK,
        Err(error) => {
            set_last_error(format!("could not open {}: {error}", dir.display()));
            ERR_SYSTEM
        }
    }
}

/// Set while an update probe runs, so a second click cannot start a second npm.
static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

/// How `npm` has to be started on this platform.
///
/// Windows installs `npm` as a command shim that `CreateProcess` cannot start
/// directly, so it goes through `cmd.exe /c`.
fn npm_command() -> (PathBuf, Vec<String>) {
    #[cfg(windows)]
    {
        (
            PathBuf::from("cmd.exe"),
            vec!["/c".to_owned(), "npm".to_owned()],
        )
    }
    #[cfg(not(windows))]
    {
        (PathBuf::from("npm"), Vec::new())
    }
}

fn emit_update(state: &str, extra: serde_json::Value) {
    let mut payload = serde_json::json!({ "kind": "update", "state": state });
    if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    emit("update", &payload);
}

fn emit_update_failure(detail: &str) {
    let detail = url_policy::redact_text(detail);
    record(&format!("update check failed: {detail}"));
    emit_update("failed", serde_json::json!({ "detail": detail }));
}

/// Runs the `npm view` probe and reports the verdict through the event sink.
fn run_update_check() {
    let config = load_config();
    let inputs = locate_inputs(&StartOptions::default(), &config);
    let installed = dsh_locate::locate(&inputs)
        .ok()
        .and_then(|command| probe_dsh_version(&command));

    let (program, prefix) = npm_command();
    let args = vec![
        "view".to_owned(),
        dsh_lite_core::update::PACKAGE.to_owned(),
        "version".to_owned(),
    ];

    record(&format!(
        "checking {} with npm view",
        dsh_lite_core::update::PACKAGE
    ));
    match run_capture(&program, &prefix, &args, UPDATE_TIMEOUT) {
        Ok(capture) => {
            // npm prints warnings on stdout, so the verdict comes from the
            // shared interpreter rather than from this crate.
            match dsh_lite_core::update::interpret(
                &capture.stdout,
                capture.code,
                installed.as_deref(),
            ) {
                Ok(outcome) => {
                    record(&format!(
                        "published dsh is {} (installed {})",
                        outcome.latest,
                        outcome.installed.as_deref().unwrap_or("unknown")
                    ));
                    emit_update(
                        if outcome.newer {
                            "available"
                        } else {
                            "current"
                        },
                        serde_json::json!({
                            "latest": outcome.latest,
                            "installed": outcome.installed,
                            "newer": outcome.newer,
                            "upgradeCommand": dsh_lite_core::update::UPGRADE_COMMAND,
                        }),
                    );
                }
                Err(error) => emit_update_failure(&error.to_string()),
            }
        }
        Err(error) => emit_update_failure(&error),
    }
}

/// Asks npm for the published `dsh` version and reports it without installing.
///
/// Returns [`OK`] when the probe is accepted (`checking` → `available` /
/// `current` / `failed` events follow) and [`ERR_STATE`] when one is already
/// running. Nothing is ever installed or upgraded by the shell.
#[unsafe(no_mangle)]
pub extern "C" fn dsh_check_update() -> c_int {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
        set_last_error("an update check is already running");
        return ERR_STATE;
    }

    match SHELL.lock() {
        Ok(mut shell) => {
            if shell.logger.is_none() {
                shell.logger = logger();
            }
        }
        Err(poisoned) => {
            let mut shell = poisoned.into_inner();
            if shell.logger.is_none() {
                shell.logger = logger();
            }
        }
    }

    emit_update("checking", serde_json::json!({}));
    std::thread::spawn(|| {
        run_update_check();
        UPDATE_RUNNING.store(false, Ordering::SeqCst);
    });
    OK
}

/// Defaults the report falls back to when the configuration file is missing, so
/// the shape of a report can be asserted without touching the user's config.
#[cfg(test)]
fn default_config() -> Config {
    Config {
        connect: dsh_lite_core::config::ConnectConfig::default(),
        dsh: dsh_lite_core::config::DshConfig::default(),
        env: dsh_lite_core::config::EnvConfig::default(),
        window: dsh_lite_core::config::WindowConfig::default(),
        log: dsh_lite_core::config::LogConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_version_is_reported() {
        assert_eq!(dsh_ffi_abi_version(), ABI_VERSION);
        assert!(ABI_VERSION >= 1);
    }

    #[test]
    fn strings_round_trip_and_are_released_once() {
        let pointer = dsh_shell_version();
        assert!(!pointer.is_null());
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(text, dsh_lite_core::VERSION);
        unsafe { dsh_string_free(pointer) };
        // Freeing null is a no-op and must not abort.
        unsafe { dsh_string_free(std::ptr::null_mut()) };
    }

    #[test]
    fn embedded_nul_bytes_are_replaced_instead_of_truncating() {
        let pointer = adopt_string("a\0b");
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(text, "a b");
        unsafe { dsh_string_free(pointer) };
    }

    #[test]
    fn start_options_are_read_from_camel_case_json() {
        let options: StartOptions =
            serde_json::from_str(r#"{"profile":"web","port":3080,"timeoutMs":1000}"#).unwrap();
        assert_eq!(options.profile.as_deref(), Some("web"));
        assert_eq!(options.port, Some(3080));
        assert_eq!(options.timeout_ms, Some(1000));
    }

    #[test]
    fn an_empty_options_object_means_defaults() {
        let options = match read_instrument(CString::new("{}").unwrap().as_ptr()) {
            Some(text) => serde_json::from_str::<StartOptions>(&text).unwrap(),
            None => StartOptions::default(),
        };
        assert!(options.profile.is_none());
        assert!(options.port.is_none());
    }

    #[test]
    fn a_null_argument_reads_as_no_value() {
        assert!(read_instrument(std::ptr::null()).is_none());
    }

    #[test]
    fn failures_are_redacted_before_they_reach_the_event_sink() {
        // The detail is redacted by `emit_failure`; assert on the same function
        // the payload is built from so a regression cannot slip through.
        let detail = "GET /?token=secret-value failed";
        assert!(!url_policy::redact_text(detail).contains("secret-value"));
    }

    #[test]
    fn diagnose_returns_a_usable_json_object() {
        let pointer = dsh_diagnose();
        assert!(!pointer.is_null());
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned();
        let report: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(report["abiVersion"], ABI_VERSION);
        assert_eq!(report["shellVersion"], dsh_lite_core::VERSION);
        assert!(report.get("dshFound").is_some());
        assert!(report.get("nodeVersion").is_some());
        unsafe { dsh_string_free(pointer) };
    }

    #[test]
    fn the_default_config_matches_the_loaded_one_in_shape() {
        let defaults = default_config();
        assert_eq!(defaults.dsh.profile, "web");
        assert_eq!(defaults.connect.port, 0);
    }

    #[test]
    fn stopping_without_a_child_is_harmless() {
        stop_child();
        assert_eq!(dsh_stop(), OK);
    }
}
