//! Launching the Desktop Host (the dsh `desktop` profile) from Rust.
//!
//! The official Electron shell starts a Node child (`apps/desktop-host`) that
//! calls the internal `runProfile({ profile: "desktop" })` — the dsh CLI refuses
//! `--profile desktop` outright. This module runs our equivalent launcher
//! (`src-tauri/host/desktop-host.mjs`) with the *system* Node and dsh install,
//! and reads its one-line protocol from stdout:
//!
//! ```text
//! DSH_HOST_READY {"url":"http://127.0.0.1:PORT/?token=…","injections":12}
//! DSH_HOST_FATAL {"message":"…","diagnostic":"…"}
//! ```
//!
//! The child is owned here: it leads its own process group (Unix) / job object
//! (Windows), `shutdown` writes `shutdown\n` to its stdin for a graceful stop,
//! and every failure path terminates the tree before returning.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
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

/// Prefix of the ready line printed by the launcher.
pub const READY_PREFIX: &str = "DSH_HOST_READY ";

/// Prefix of the fatal line printed by the launcher.
pub const FATAL_PREFIX: &str = "DSH_HOST_FATAL ";

/// Environment variable the desktop profile's telemetry validates.
pub const CLIENT_VERSION_VAR: &str = "DSH_CLIENT_VERSION";

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

/// What the Host reported once its web server was listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReady {
    /// Authenticated loopback URL carrying the one-shot token.
    pub url: Url,
    /// Number of index injection rows the Host prepared.
    pub injections: usize,
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
        /// Redacted message from the launcher.
        message: String,
        /// Retained stderr tail.
        stderr_tail: String,
    },
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
            Self::Fatal {
                message,
                stderr_tail,
            } => {
                write!(f, "the desktop host failed to start: {message}")?;
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
    injections: usize,
}

#[derive(Debug, Deserialize)]
struct FatalLine {
    message: String,
}

/// Parses one stdout line into a ready report.
///
/// The URL must be loopback HTTP; anything else is not a usable ready line.
pub fn parse_ready_line(line: &str) -> Option<HostReady> {
    let rest = line.trim().strip_prefix(READY_PREFIX)?;
    let parsed: ReadyLine = serde_json::from_str(rest).ok()?;
    let url = Url::parse(&parsed.url).ok()?;
    if url.scheme() != "http" || !url_policy::is_loopback_http(&url) {
        return None;
    }
    Some(HostReady {
        url,
        injections: parsed.injections,
    })
}

/// Parses one stdout line into a redacted fatal message.
pub fn parse_fatal_line(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(FATAL_PREFIX)?;
    let parsed: FatalLine = serde_json::from_str(rest).ok()?;
    Some(url_policy::redact_text(&parsed.message))
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
                if let Some(message) = parse_fatal_line(&line) {
                    terminate(&mut child, DEFAULT_STOP_GRACE);
                    let _ = stderr_reader.join();
                    return Err(HostError::Fatal {
                        message,
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

    #[test]
    fn a_ready_line_with_a_loopback_url_is_accepted() {
        let ready = parse_ready_line(
            r#"DSH_HOST_READY {"url":"http://127.0.0.1:54570/?token=abc","injections":12}"#,
        )
        .expect("ready");
        assert_eq!(ready.url.as_str(), "http://127.0.0.1:54570/?token=abc");
        assert_eq!(ready.injections, 12);
    }

    #[test]
    fn ready_lines_on_non_loopback_hosts_are_rejected() {
        assert!(
            parse_ready_line(r#"DSH_HOST_READY {"url":"http://example.com/","injections":0}"#)
                .is_none()
        );
        assert!(
            parse_ready_line(r#"DSH_HOST_READY {"url":"https://127.0.0.1:1/","injections":0}"#)
                .is_none()
        );
    }

    #[test]
    fn unrelated_lines_are_not_ready_lines() {
        assert!(parse_ready_line("dsh web: http://127.0.0.1:1/?token=x").is_none());
        assert!(parse_ready_line("DSH_HOST_READY not-json").is_none());
        assert!(parse_ready_line("").is_none());
    }

    #[test]
    fn a_fatal_line_is_reported_and_redacted() {
        let message = parse_fatal_line(r#"DSH_HOST_FATAL {"message":"boom","diagnostic":"stack"}"#)
            .expect("fatal");
        assert_eq!(message, "boom");
        assert!(parse_fatal_line("DSH_HOST_READY {}").is_none());
    }
}
