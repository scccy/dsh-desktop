//! Launching the Desktop Host (the dsh `desktop` profile) from Rust.
//!
//! The official Electron shell starts a Node child (`apps/desktop-host`) that
//! calls the internal `runProfile({ profile: "desktop" })` — the dsh CLI refuses
//! `--profile desktop` outright. This module runs our equivalent launcher
//! (`src-tauri/host/desktop-host.mjs`) with the *system* Node and dsh install,
//! and reads its one-line protocol from stdout:
//!
//! ```text
//! DSH_HOST_READY {"url":"http://127.0.0.1:PORT/?token=…","origin":"http://127.0.0.1:PORT",
//!                 "protocol":"http:","protocolVersion":1,"injections":[{…},…]}
//! DSH_HOST_FATAL {"code":"version","message":"…","found":"0.2.0","minimum":"0.2.1-rc.2"}
//! ```
//!
//! The ready report carries the *whole* index injection rows array, not just its
//! length: the preload bridge replays those rows into the renderer's boot
//! contract. Older launchers only reported a count, which is still accepted (the
//! rows are then empty and [`HostReady::injection_count`] carries the number).
//!
//! The child is owned here: it leads its own process group (Unix) / job object
//! (Windows), `shutdown` writes `shutdown\n` to its stdin for a graceful stop,
//! and every failure path terminates the tree before returning.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use url::Url;

use crate::launcher::{
    DEFAULT_STOP_GRACE, SharedTail, drain_stderr, drain_stdout, tail_text, terminate, waits_within,
};
use crate::url_policy;
use crate::version_gate::{self, ProbeError, SemVer, UPGRADE_COMMAND, VersionGate, VersionProbe};

/// Prefix of the ready line printed by the launcher.
pub const READY_PREFIX: &str = "DSH_HOST_READY ";

/// Prefix of the fatal line printed by the launcher.
pub const FATAL_PREFIX: &str = "DSH_HOST_FATAL ";

/// Environment variable the desktop profile's telemetry validates.
pub const CLIENT_VERSION_VAR: &str = "DSH_CLIENT_VERSION";

/// Profile the desktop shell owns; it is never the `web` profile.
pub const PROFILE_NAME: &str = "desktop";

/// Protocol version of the launcher's stdout contract.
pub const HOST_PROTOCOL_VERSION: u32 = 1;

/// Everything needed to start the Desktop Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    /// Node executable (`node`).
    pub node: PathBuf,
    /// Absolute path of `desktop-host.mjs`.
    pub script: PathBuf,
    /// Installed `@deepseek-ai/dsh` package directory.
    pub dsh_package: PathBuf,
    /// Profile directory, usually `$DSH_HOME/profiles/desktop`.
    pub project_dir: PathBuf,
    /// Value injected as `DSH_CLIENT_VERSION`.
    pub version: String,
    /// Complete child environment; empty inherits this process' environment.
    pub env: Vec<(OsString, OsString)>,
}

impl HostSpec {
    /// Profile directory the desktop shell owns: `$DSH_HOME/profiles/desktop`.
    ///
    /// `dsh_home` is `DSH_HOME` when the user set it, `home` is the account's
    /// home directory. Returns `None` only when neither is known.
    pub fn profile_dir(dsh_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
        let root = match dsh_home {
            Some(path) if !path.as_os_str().is_empty() => path.to_path_buf(),
            _ => home?.join(".dsh"),
        };
        Some(root.join("profiles").join(PROFILE_NAME))
    }

    /// [`HostSpec::profile_dir`] fed from this process' environment.
    pub fn profile_dir_from_process_env() -> Option<PathBuf> {
        let dsh_home = std::env::var_os("DSH_HOME").map(PathBuf::from);
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        Self::profile_dir(dsh_home.as_deref(), home.as_deref())
    }

    /// Entry point of the installed dsh CLI, used for the `--version` probe.
    ///
    /// Read from `package.json`'s `bin` so an upstream rename cannot silently
    /// break the probe; `lib/bin.js` is the fallback for a manifest we cannot
    /// read.
    pub fn dsh_cli_entry(&self) -> PathBuf {
        let manifest = self.dsh_package.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let bin = value.get("bin");
            let relative = bin
                .and_then(|bin| bin.as_str().map(str::to_owned))
                .or_else(|| {
                    bin.and_then(|bin| bin.get("dsh"))
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                });
            if let Some(relative) = relative {
                return self.dsh_package.join(relative);
            }
        }
        self.dsh_package.join("lib").join("bin.js")
    }
}

/// What the Host reported once its web server was listening.
#[derive(Debug, Clone, PartialEq)]
pub struct HostReady {
    /// Authenticated loopback URL carrying the one-shot token.
    pub url: Url,
    /// `scheme://host:port` of that URL, without the token.
    pub origin: Url,
    /// `http:` or `https:`, as the Host's web server reported it.
    pub protocol: String,
    /// Launcher protocol version (currently [`HOST_PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Complete index injection rows, in Host order. Empty for old launchers.
    pub injections: Vec<serde_json::Value>,
    /// Number of injection rows; for an old launcher that only reported a
    /// count, that reported count.
    pub injection_count: usize,
}

impl HostReady {
    /// Whether the rows themselves arrived (a current launcher), as opposed to
    /// only a count (an old one).
    pub fn has_injection_rows(&self) -> bool {
        !self.injections.is_empty()
    }
}

/// A structured `DSH_HOST_FATAL` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostFatal {
    /// The installed dsh is older than the launcher requires.
    Version {
        /// Message printed by the launcher, token-redacted.
        message: String,
        /// Version the launcher found, when it could report one.
        found: Option<String>,
        /// Version the launcher requires.
        minimum: Option<String>,
    },
    /// Any other fatal error, including reports without a `code`.
    Other {
        /// Machine-readable code, when the launcher sent one.
        code: Option<String>,
        /// Message printed by the launcher, token-redacted.
        message: String,
    },
}

impl HostFatal {
    /// One line for the error page, with the upgrade command when it applies.
    pub fn summary(&self) -> String {
        match self {
            Self::Version {
                message: _,
                found: Some(found),
                minimum: Some(minimum),
            } => format!(
                "dsh {found} is older than the required {minimum}; upgrade with `{UPGRADE_COMMAND}` and try again"
            ),
            Self::Version {
                message,
                found: _,
                minimum: _,
            }
            | Self::Other { code: _, message } => message.clone(),
        }
    }

    /// Whether the Host refused to start because of the dsh version.
    pub fn is_version_refusal(&self) -> bool {
        matches!(self, Self::Version { .. })
    }
}

/// Launch failures.
#[derive(Debug)]
pub enum HostError {
    /// The node process could not be started.
    Spawn {
        /// Program that failed to start.
        program: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// No ready line arrived before the deadline.
    Timeout {
        /// How long the shell waited.
        after: Duration,
        /// Retained stderr tail.
        stderr_tail: String,
    },
    /// The process exited before announcing readiness.
    Exited {
        /// Exit code, when the platform reported one.
        code: Option<i32>,
        /// Retained stderr tail.
        stderr_tail: String,
    },
    /// The launcher reported a fatal startup error.
    Fatal {
        /// Structured report from the launcher.
        fatal: HostFatal,
        /// Retained stderr tail.
        stderr_tail: String,
    },
}

impl HostError {
    /// The structured report, when the launcher sent one.
    pub fn fatal(&self) -> Option<&HostFatal> {
        match self {
            Self::Fatal { fatal, .. } => Some(fatal),
            Self::Spawn { .. } | Self::Timeout { .. } | Self::Exited { .. } => None,
        }
    }

    /// Whether the Host refused to start because the installed dsh is too old.
    pub fn is_version_refusal(&self) -> bool {
        self.fatal().is_some_and(HostFatal::is_version_refusal)
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(
                    f,
                    "could not start the desktop host ({}): {source}",
                    program.display()
                )
            }
            Self::Timeout { after, stderr_tail } => {
                write!(
                    f,
                    "the desktop host did not become ready within {} ms",
                    after.as_millis()
                )?;
                write_tail(f, stderr_tail)
            }
            Self::Exited { code, stderr_tail } => {
                match code {
                    Some(code) => write!(
                        f,
                        "the desktop host exited with code {code} before it was ready"
                    )?,
                    None => write!(f, "the desktop host was terminated before it was ready")?,
                }
                write_tail(f, stderr_tail)
            }
            Self::Fatal { fatal, stderr_tail } => {
                write!(f, "the desktop host failed to start: {}", fatal.summary())?;
                write_tail(f, stderr_tail)
            }
        }
    }
}

fn write_tail(f: &mut std::fmt::Formatter<'_>, tail: &str) -> std::fmt::Result {
    if tail.trim().is_empty() {
        return Ok(());
    }
    write!(f, "\n--- desktop host stderr ---\n{tail}")
}

impl std::error::Error for HostError {}

#[derive(Debug, Deserialize)]
struct ReadyLine {
    url: String,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default, rename = "protocolVersion")]
    protocol_version: Option<u32>,
    /// Full row array from a current launcher, or a bare count from an old one.
    #[serde(default)]
    injections: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct FatalLine {
    #[serde(default)]
    code: Option<String>,
    message: String,
    #[serde(default)]
    found: Option<String>,
    #[serde(default)]
    minimum: Option<String>,
}

/// Parses one stdout line into a ready report.
///
/// The URL must be loopback HTTP carrying a one-shot token; anything else is not
/// a usable ready line. Fields a launcher does not send are derived from that
/// URL, so both the current and the earlier contract parse.
pub fn parse_ready_line(line: &str) -> Option<HostReady> {
    let rest = line.trim().strip_prefix(READY_PREFIX)?;
    let parsed: ReadyLine = serde_json::from_str(rest).ok()?;
    let url = Url::parse(&parsed.url).ok()?;
    // The desktop Host serves plain HTTP on loopback; nothing else is usable.
    if url.scheme() != "http" || !url_policy::is_loopback_http(&url) {
        return None;
    }
    let token = url_policy::token_of(&url)?;
    if token.is_empty() {
        return None;
    }

    // A reported protocol that contradicts the URL is ignored, not trusted.
    let protocol = match parsed.protocol.as_deref() {
        Some(value) if value == format!("{}:", url.scheme()) => value.to_owned(),
        Some(_) | None => format!("{}:", url.scheme()),
    };

    let origin = match parsed.origin.as_deref() {
        Some(text) => {
            let origin = Url::parse(text).ok()?;
            if !url_policy::is_loopback_http(&origin) || !url_policy::shares_target(&origin, &url) {
                return None;
            }
            origin
        }
        None => url_policy::clean_url(&url),
    };

    let (injections, injection_count) = match parsed.injections {
        Some(serde_json::Value::Array(rows)) => {
            let count = rows.len();
            (rows, count)
        }
        // An old launcher sent the number of rows it prepared. Keep the count so
        // the shell can still report it, and start with no rows.
        Some(serde_json::Value::Number(count)) => (Vec::new(), count.as_u64()? as usize),
        Some(serde_json::Value::Null) | None => (Vec::new(), 0),
        Some(_) => return None,
    };

    Some(HostReady {
        url,
        origin,
        protocol,
        protocol_version: parsed.protocol_version.unwrap_or(HOST_PROTOCOL_VERSION),
        injections,
        injection_count,
    })
}

/// Parses one stdout line into a structured fatal report.
pub fn parse_fatal_line(line: &str) -> Option<HostFatal> {
    let rest = line.trim().strip_prefix(FATAL_PREFIX)?;
    let parsed: FatalLine = serde_json::from_str(rest).ok()?;
    let message = url_policy::redact_text(&parsed.message);
    Some(match parsed.code.as_deref() {
        Some("version") => HostFatal::Version {
            message,
            found: parsed.found.as_deref().map(url_policy::redact_text),
            minimum: parsed.minimum.as_deref().map(url_policy::redact_text),
        },
        code => HostFatal::Other {
            code: code.map(str::to_owned),
            message,
        },
    })
}

/// Reads the version the installed dsh reports, and gates it.
///
/// The launcher repeats this check inside the Host process (that copy is what
/// emits `DSH_HOST_FATAL {"code":"version"}`); this preflight exists so the shell
/// can show the upgrade hint before paying for a Host start. A probe failure is
/// reported as such — it is not a verdict about the version.
pub fn probe_version(spec: &HostSpec, deadline: Duration) -> Result<VersionGate, ProbeError> {
    let request = VersionProbe {
        program: spec.node.clone(),
        args: vec![
            spec.dsh_cli_entry().into_os_string(),
            OsString::from("--version"),
        ],
        env: spec.env.clone(),
        timeout: deadline,
    };
    let found: SemVer = version_gate::probe(&request)?;
    Ok(version_gate::gate(found))
}

/// A running Desktop Host that announced its ready line.
#[derive(Debug)]
pub struct DesktopHost {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pid: u32,
    ready: HostReady,
    stderr_tail: SharedTail,
    #[cfg(windows)]
    job: Option<crate::job::ChildJob>,
}

impl DesktopHost {
    /// Operating-system process id, recorded by the shell for its logs.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// What the Host reported once it was listening.
    pub fn ready(&self) -> &HostReady {
        &self.ready
    }

    /// Retained stderr tail, token-redacted.
    pub fn stderr_tail(&self) -> String {
        tail_text(&self.stderr_tail)
    }

    /// Asks the Host to stop, escalating after `grace`. Repeated calls are no-ops.
    pub fn stop(&mut self, grace: Duration) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"shutdown\n");
            let _ = stdin.flush();
        }
        let mut child = self.child.take();
        if let Some(child) = child.as_mut() {
            if waits_within(child, grace) {
                let _ = child.wait();
            } else {
                #[cfg(windows)]
                if let Some(job) = self.job.take() {
                    job.terminate();
                }
                terminate(child, grace);
            }
        }
        #[cfg(windows)]
        {
            self.job = None;
        }
    }
}

impl Drop for DesktopHost {
    fn drop(&mut self) {
        self.stop(DEFAULT_STOP_GRACE);
    }
}

/// Starts the launcher, waits for its ready line, and returns the owned host.
///
/// `deadline` bounds the wait. Every failure path terminates the child before
/// returning, so a failed launch leaves no process behind.
pub fn spawn(spec: &HostSpec, deadline: Duration) -> Result<DesktopHost, HostError> {
    let mut command = Command::new(&spec.node);
    command
        .arg(&spec.script)
        .arg("--dsh")
        .arg(&spec.dsh_package)
        .arg("--project")
        .arg(&spec.project_dir)
        .arg("--version")
        .arg(&spec.version)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !spec.env.is_empty() {
        command.env_clear();
        for (name, value) in &spec.env {
            command.env(name, value);
        }
    }
    // Set after `env_clear` so an explicit environment cannot drop it.
    command.env(CLIENT_VERSION_VAR, &spec.version);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::launcher::CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let program = spec.node.clone();
    let mut child = command.spawn().map_err(|source| HostError::Spawn {
        program: program.clone(),
        source,
    })?;

    #[cfg(windows)]
    let job = match crate::job::ChildJob::attach(&child) {
        Ok(job) => Some(job),
        Err(source) => {
            terminate(&mut child, DEFAULT_STOP_GRACE);
            return Err(HostError::Spawn { program, source });
        }
    };

    let (Some(stdout), Some(stderr), Some(stdin)) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take())
    else {
        terminate(&mut child, DEFAULT_STOP_GRACE);
        return Err(HostError::Spawn {
            program,
            source: std::io::Error::other("desktop host pipes were not captured"),
        });
    };

    let stdout_tail: SharedTail = Arc::new(Mutex::new(Vec::new()));
    let stderr_tail: SharedTail = Arc::new(Mutex::new(Vec::new()));
    let (line_sender, line_receiver) = mpsc::channel::<String>();
    drain_stdout(stdout, Arc::clone(&stdout_tail), line_sender);
    let stderr_reader = drain_stderr(stderr, Arc::clone(&stderr_tail));

    let pid = child.id();
    let started = Instant::now();
    loop {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate(&mut child, DEFAULT_STOP_GRACE);
            let _ = stderr_reader.join();
            return Err(HostError::Timeout {
                after: deadline,
                stderr_tail: tail_text(&stderr_tail),
            });
        }
        match line_receiver.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(ready) = parse_ready_line(&line) {
                    return Ok(DesktopHost {
                        child: Some(child),
                        stdin: Some(stdin),
                        pid,
                        ready,
                        stderr_tail,
                        #[cfg(windows)]
                        job,
                    });
                }
                if let Some(fatal) = parse_fatal_line(&line) {
                    terminate(&mut child, DEFAULT_STOP_GRACE);
                    let _ = stderr_reader.join();
                    return Err(HostError::Fatal {
                        fatal,
                        stderr_tail: tail_text(&stderr_tail),
                    });
                }
                // Ordinary stdout chatter: ignore, the deadline still bounds us.
            }
            Err(RecvTimeoutError::Timeout) => {
                terminate(&mut child, DEFAULT_STOP_GRACE);
                let _ = stderr_reader.join();
                return Err(HostError::Timeout {
                    after: deadline,
                    stderr_tail: tail_text(&stderr_tail),
                });
            }
            Err(RecvTimeoutError::Disconnected) => {
                let code = child.wait().ok().and_then(|status| status.code());
                let _ = stderr_reader.join();
                return Err(HostError::Exited {
                    code,
                    stderr_tail: tail_text(&stderr_tail),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY_URL: &str = "http://127.0.0.1:54570/?token=abc";

    fn ready_line(injections: &str) -> String {
        format!(
            r#"DSH_HOST_READY {{"url":"{READY_URL}","origin":"http://127.0.0.1:54570","protocol":"http:","protocolVersion":1,"injections":{injections}}}"#
        )
    }

    #[test]
    fn a_ready_line_reports_the_full_contract() {
        let ready = parse_ready_line(&ready_line(
            r#"[{"kind":"style","content":"body{}"},{"kind":"script","content":"1+1"}]"#,
        ))
        .expect("ready");
        assert_eq!(ready.url.as_str(), READY_URL);
        assert_eq!(ready.origin.as_str(), "http://127.0.0.1:54570/");
        assert_eq!(ready.protocol, "http:");
        assert_eq!(ready.protocol_version, HOST_PROTOCOL_VERSION);
        assert_eq!(ready.injections.len(), 2);
        assert_eq!(ready.injection_count, 2);
        assert_eq!(ready.injections[0]["kind"], "style");
        assert!(ready.has_injection_rows());
    }

    #[test]
    fn an_old_host_reporting_only_a_count_still_starts() {
        let ready = parse_ready_line(&format!(
            r#"DSH_HOST_READY {{"url":"{READY_URL}","injections":12}}"#
        ))
        .expect("old ready line is still usable");
        assert!(ready.injections.is_empty());
        assert_eq!(ready.injection_count, 12);
        assert_eq!(ready.protocol, "http:");
        assert_eq!(ready.protocol_version, HOST_PROTOCOL_VERSION);
        assert_eq!(ready.origin.as_str(), "http://127.0.0.1:54570/");
    }

    #[test]
    fn a_ready_line_without_a_one_shot_token_is_rejected() {
        assert!(
            parse_ready_line(r#"DSH_HOST_READY {"url":"http://127.0.0.1:1/","injections":[]}"#)
                .is_none()
        );
        assert!(
            parse_ready_line(
                r#"DSH_HOST_READY {"url":"http://127.0.0.1:1/?token=","injections":[]}"#
            )
            .is_none()
        );
    }

    #[test]
    fn ready_lines_on_non_loopback_hosts_are_rejected() {
        assert!(
            parse_ready_line(
                r#"DSH_HOST_READY {"url":"http://example.com/?token=x","injections":[]}"#
            )
            .is_none()
        );
        assert!(
            parse_ready_line(
                r#"DSH_HOST_READY {"url":"https://127.0.0.1:1/?token=x","injections":[]}"#
            )
            .is_none()
        );
    }

    #[test]
    fn an_origin_from_another_server_is_rejected() {
        assert!(
            parse_ready_line(
                r#"DSH_HOST_READY {"url":"http://127.0.0.1:54570/?token=abc","origin":"http://127.0.0.1:8080","injections":[]}"#
            )
            .is_none()
        );
    }

    #[test]
    fn an_unexpected_injections_shape_is_not_a_ready_line() {
        assert!(parse_ready_line(&ready_line(r#""thirteen""#)).is_none());
    }

    #[test]
    fn unrelated_lines_are_not_ready_lines() {
        assert!(parse_ready_line("dsh web: http://127.0.0.1:1/?token=x").is_none());
        assert!(parse_ready_line("DSH_HOST_READY not-json").is_none());
        assert!(parse_ready_line("").is_none());
    }

    #[test]
    fn a_plain_fatal_line_keeps_its_message() {
        let fatal = parse_fatal_line(r#"DSH_HOST_FATAL {"message":"boom"}"#).expect("fatal");
        assert_eq!(
            fatal,
            HostFatal::Other {
                code: None,
                message: "boom".to_owned()
            }
        );
        assert!(!fatal.is_version_refusal());
        assert!(parse_fatal_line("DSH_HOST_READY {}").is_none());
    }

    #[test]
    fn a_version_fatal_line_is_structured_and_points_at_the_upgrade() {
        let fatal = parse_fatal_line(
            r#"DSH_HOST_FATAL {"code":"version","message":"dsh 0.2.0 is too old","found":"0.2.0","minimum":"0.2.1-rc.2"}"#,
        )
        .expect("fatal");
        assert!(fatal.is_version_refusal());
        assert_eq!(
            fatal,
            HostFatal::Version {
                message: "dsh 0.2.0 is too old".to_owned(),
                found: Some("0.2.0".to_owned()),
                minimum: Some("0.2.1-rc.2".to_owned()),
            }
        );
        let summary = fatal.summary();
        assert!(summary.contains("0.2.0"), "summary: {summary}");
        assert!(summary.contains("0.2.1-rc.2"), "summary: {summary}");
        assert!(summary.contains(UPGRADE_COMMAND), "summary: {summary}");
    }

    #[test]
    fn fatal_messages_are_redacted() {
        let fatal = parse_fatal_line(
            r#"DSH_HOST_FATAL {"message":"failed at http://127.0.0.1:1/?token=secret"}"#,
        )
        .expect("fatal");
        let summary = fatal.summary();
        assert!(!summary.contains("secret"), "summary: {summary}");
    }

    #[test]
    fn the_desktop_profile_is_never_the_web_profile() {
        let with_home =
            HostSpec::profile_dir(None, Some(Path::new("/Users/someone"))).expect("path");
        assert_eq!(
            with_home,
            PathBuf::from("/Users/someone/.dsh/profiles/desktop")
        );
        let with_dsh_home = HostSpec::profile_dir(
            Some(Path::new("/custom/dsh")),
            Some(Path::new("/Users/someone")),
        )
        .expect("path");
        assert_eq!(with_dsh_home, PathBuf::from("/custom/dsh/profiles/desktop"));
        let empty_dsh_home = HostSpec::profile_dir(Some(Path::new("")), Some(Path::new("/home/x")))
            .expect("falls back to the home directory");
        assert_eq!(
            empty_dsh_home,
            PathBuf::from("/home/x/.dsh/profiles/desktop")
        );
        assert_eq!(HostSpec::profile_dir(None, None), None);
        assert!(!with_home.ends_with("web"));
    }

    #[test]
    fn the_cli_entry_comes_from_the_package_manifest() {
        let directory = tempfile::tempdir().expect("temp dir");
        let package = directory.path().join("dsh");
        std::fs::create_dir_all(&package).expect("package dir");
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"lib/bin.js"}}"#,
        )
        .expect("manifest");
        let spec = HostSpec {
            node: PathBuf::from("node"),
            script: PathBuf::from("desktop-host.mjs"),
            dsh_package: package.clone(),
            project_dir: PathBuf::from("profiles/desktop"),
            version: "0.1.0".to_owned(),
            env: Vec::new(),
        };
        assert_eq!(spec.dsh_cli_entry(), package.join("lib/bin.js"));

        std::fs::write(package.join("package.json"), "not json").expect("broken manifest");
        assert_eq!(spec.dsh_cli_entry(), package.join("lib/bin.js"));

        let missing = HostSpec {
            dsh_package: directory.path().join("absent"),
            ..spec
        };
        assert_eq!(
            missing.dsh_cli_entry(),
            directory.path().join("absent").join("lib").join("bin.js")
        );
    }

    #[cfg(unix)]
    fn fake_node(directory: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("fake-node");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write fake node");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod");
        path
    }

    #[cfg(unix)]
    fn spec_for(node: PathBuf, directory: &Path) -> HostSpec {
        HostSpec {
            node,
            script: directory.join("desktop-host.mjs"),
            dsh_package: directory.join("dsh"),
            project_dir: directory.join("profiles/desktop"),
            version: "0.1.0".to_owned(),
            env: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_child_environment_is_forwarded_to_the_host() {
        let directory = tempfile::tempdir().expect("temp dir");
        let node = fake_node(
            directory.path(),
            r#"printf 'DSH_HOST_READY {"url":"http://127.0.0.1:54570/?token=abc","injections":[{"kind":"env","value":"%s"}]}\n' "$DSH_LITE_TEST_VALUE"
"#,
        );
        let mut spec = spec_for(node, directory.path());
        spec.env = vec![(
            OsString::from("DSH_LITE_TEST_VALUE"),
            OsString::from("exported-in-login-shell"),
        )];
        let mut host = spawn(&spec, Duration::from_secs(5)).expect("ready");
        assert_eq!(
            host.ready().injections[0]["value"],
            "exported-in-login-shell"
        );
        host.stop(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn a_ready_line_larger_than_a_pipe_chunk_is_parsed_whole() {
        let directory = tempfile::tempdir().expect("temp dir");
        let filler = "x".repeat(200 * 1024);
        let payload = directory.path().join("payload.json");
        std::fs::write(
            &payload,
            ready_line(&format!(r#"[{{"kind":"html","content":"{filler}"}}]"#)) + "\n",
        )
        .expect("payload");
        let node = fake_node(directory.path(), "cat \"$(dirname \"$0\")/payload.json\"\n");
        let spec = spec_for(node, directory.path());
        let mut host = spawn(&spec, Duration::from_secs(5)).expect("ready");
        assert_eq!(host.ready().injection_count, 1);
        let content = host.ready().injections[0]["content"]
            .as_str()
            .expect("content string");
        assert_eq!(content.len(), filler.len());
        host.stop(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn an_old_host_count_arrives_over_the_real_pipe() {
        let directory = tempfile::tempdir().expect("temp dir");
        let line = format!(r#"DSH_HOST_READY {{"url":"{READY_URL}","injections":12}}"#);
        let node = fake_node(directory.path(), &format!("printf '%s\\n' '{line}'\n"));
        let spec = spec_for(node, directory.path());
        let mut host = spawn(&spec, Duration::from_secs(5)).expect("ready");
        assert!(host.ready().injections.is_empty());
        assert_eq!(host.ready().injection_count, 12);
        host.stop(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn a_version_refusal_is_a_typed_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let node = fake_node(
            directory.path(),
            r#"printf '%s\n' 'DSH_HOST_FATAL {"code":"version","message":"dsh 0.2.0 is too old","found":"0.2.0","minimum":"0.2.1-rc.2"}'
exit 1
"#,
        );
        let spec = spec_for(node, directory.path());
        let error = spawn(&spec, Duration::from_secs(5)).expect_err("refused");
        assert!(error.is_version_refusal());
        let HostFatal::Version { found, minimum, .. } = error.fatal().expect("structured") else {
            panic!("expected a version refusal: {error:?}");
        };
        assert_eq!(found.as_deref(), Some("0.2.0"));
        assert_eq!(minimum.as_deref(), Some("0.2.1-rc.2"));
        let message = error.to_string();
        assert!(message.contains(UPGRADE_COMMAND), "message: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn an_exit_before_readiness_is_reported() {
        let directory = tempfile::tempdir().expect("temp dir");
        let node = fake_node(directory.path(), "echo 'boot failed' >&2\nexit 7\n");
        let spec = spec_for(node, directory.path());
        let error = spawn(&spec, Duration::from_secs(5)).expect_err("exited");
        match error {
            HostError::Exited { code, stderr_tail } => {
                assert_eq!(code, Some(7));
                assert!(stderr_tail.contains("boot failed"), "tail: {stderr_tail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_version_preflight_refuses_an_old_install() {
        let directory = tempfile::tempdir().expect("temp dir");
        let package = directory.path().join("dsh");
        std::fs::create_dir_all(package.join("lib")).expect("package dir");
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"0.2.0-rc.2","bin":{"dsh":"lib/bin.js"}}"#,
        )
        .expect("manifest");

        let node = fake_node(directory.path(), "printf '0.2.0-rc.2\\n'\n");
        let spec = spec_for(node, directory.path());
        let gate = probe_version(&spec, Duration::from_secs(5)).expect("probe");
        assert!(gate.is_refusal(), "0.2.0-rc.2 must be refused");

        let node = fake_node(directory.path(), "printf '0.2.1-rc.2\\n'\n");
        let spec = spec_for(node, directory.path());
        let gate = probe_version(&spec, Duration::from_secs(5)).expect("probe");
        assert!(!gate.is_refusal(), "0.2.1-rc.2 must be accepted");
    }

    #[cfg(unix)]
    #[test]
    fn stopping_writes_shutdown_to_stdin() {
        let directory = tempfile::tempdir().expect("temp dir");
        let marker = directory.path().join("stdin.txt");
        let node = fake_node(
            directory.path(),
            r#"printf '%s\n' 'DSH_HOST_READY {"url":"http://127.0.0.1:54570/?token=abc","origin":"http://127.0.0.1:54570","protocol":"http:","protocolVersion":1,"injections":[{"kind":"style","content":"body{}"}]}'
read line
printf '%s' "$line" > "$(dirname "$0")/stdin.txt"
"#,
        );
        let spec = spec_for(node, directory.path());
        let mut host = spawn(&spec, Duration::from_secs(5)).expect("ready");
        assert_eq!(host.ready().injection_count, 1);
        host.stop(Duration::from_secs(5));
        let recorded = std::fs::read_to_string(&marker).expect("stdin marker");
        assert_eq!(recorded, "shutdown");
        // The marker path is only used to prove stdin was written.
        assert!(marker.exists());
    }
}
