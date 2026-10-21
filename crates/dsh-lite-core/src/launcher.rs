//! Starting `dsh web`, reading its readiness line, and owning the child.
//!
//! The readiness protocol is one stdout line, `dsh web: <tokenized url>`. This
//! module turns that line into a [`Url`], keeps the child in its own process
//! group so a hang can be killed as a unit, and guarantees the child is gone
//! when the owning shell stops it or drops it. Child output stays in bounded
//! tail buffers because the token in the readiness line must never be logged.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use url::Url;

use crate::dsh_locate::DshCommand;
use crate::url_policy;

/// Prefix of the readiness line printed by `dsh web`.
pub const READY_PREFIX: &str = "dsh web: ";

/// Grace period used when a child is dropped without an explicit stop.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(2);

/// Bytes of child output retained for diagnostics.
const TAIL_BYTES: usize = 64 * 1024;

/// Poll interval while waiting for a terminated child to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Hides the console window of a child started from a GUI process.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

type SharedTail = Arc<Mutex<Vec<u8>>>;

/// Everything needed to start `dsh web` for the managed connection mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// Resolved `dsh` command, including any script-wrapper arguments.
    pub command: DshCommand,
    /// Profile passed to `--profile`.
    pub profile: String,
    /// Requested port; `0` lets the operating system choose one.
    pub port: u16,
    /// Extra arguments appended verbatim after the fixed ones.
    pub extra_args: Vec<String>,
    /// Complete child environment; empty inherits this process' environment.
    pub env: Vec<(OsString, OsString)>,
}

/// A `dsh web` process that announced its readiness line.
#[derive(Debug)]
pub struct ReadyDsh {
    /// Loopback URL carrying the one-shot process token.
    pub url: Url,
    /// Child process the shell owns.
    pub child: DshChild,
}

/// Owning handle of the spawned `dsh` process.
#[derive(Debug)]
pub struct DshChild {
    child: Option<Child>,
    pid: u32,
    stdout_tail: SharedTail,
    stderr_tail: SharedTail,
}

/// Launch failures.
#[derive(Debug)]
pub enum LaunchError {
    /// The process could not be started.
    Spawn {
        program: PathBuf,
        source: std::io::Error,
    },
    /// No readiness line arrived before the deadline.
    Timeout {
        after: Duration,
        stderr_tail: String,
    },
    /// The process exited before announcing readiness.
    Exited {
        code: Option<i32>,
        stderr_tail: String,
    },
    /// The process announced readiness but its URL is unusable; redacted.
    BadLine(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(f, "could not start dsh ({}): {source}", program.display())
            }
            Self::Timeout { after, stderr_tail } => {
                write!(
                    f,
                    "dsh did not print \"dsh web:\" within {} ms",
                    after.as_millis()
                )?;
                write_tail(f, stderr_tail)
            }
            Self::Exited { code, stderr_tail } => {
                match code {
                    Some(code) => write!(f, "dsh exited with code {code} before it was ready")?,
                    None => write!(f, "dsh was terminated before it was ready")?,
                }
                write_tail(f, stderr_tail)
            }
            Self::BadLine(line) => write!(f, "dsh printed an unusable readiness line: {line}"),
        }
    }
}

fn write_tail(f: &mut std::fmt::Formatter<'_>, tail: &str) -> std::fmt::Result {
    if tail.trim().is_empty() {
        return Ok(());
    }
    write!(f, "\n--- dsh stderr ---\n{tail}")
}

impl std::error::Error for LaunchError {}

/// Arguments passed to `dsh`, in this fixed order.
///
/// `--no-open` keeps `dsh` from opening the user's browser, `--host 127.0.0.1`
/// keeps the server on loopback, and `port = 0` lets the operating system pick
/// a free port instead of colliding with a terminal-run `dsh web`.
pub fn build_args(request: &SpawnRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--profile".into(),
        request.profile.clone().into(),
        "--no-open".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        request.port.to_string().into(),
    ];
    args.extend(request.extra_args.iter().map(OsString::from));
    args
}

/// Returns the single token of a line shaped like `dsh web: <token>`.
///
/// Shape only: the token is not yet validated as a usable readiness URL.
/// Informational lines that merely start with `dsh web:` — for example
/// `dsh web: opening the default browser; pass --no-open to disable` — carry
/// more than one word after the prefix and are not readiness lines.
fn readiness_token(line: &str) -> Option<&str> {
    let trimmed = line.trim_end_matches(['\r', '\n']).trim();
    let rest = trimmed.strip_prefix(READY_PREFIX.trim_end())?;
    let candidate = rest.trim();
    if candidate.is_empty() || candidate.split_whitespace().count() != 1 {
        return None;
    }
    Some(candidate)
}

/// Parses one stdout line of `dsh web`.
///
/// The line must be exactly `dsh web:` followed by one URL token. The URL must
/// be loopback HTTP and carry a non-empty token query parameter; anything else
/// is not a usable readiness line.
pub fn parse_ready_line(line: &str) -> Option<Url> {
    let candidate = readiness_token(line)?;
    let url = Url::parse(candidate).ok()?;
    if !url_policy::is_loopback_http(&url) {
        return None;
    }
    let token = url_policy::token_of(&url)?;
    (!token.is_empty()).then_some(url)
}

/// Starts `dsh`, waits for its readiness line, and returns the owned child.
///
/// `deadline` bounds the wait for that line. Every failure path terminates the
/// child before returning, so a failed launch leaves no process behind.
pub fn spawn(request: &SpawnRequest, deadline: Duration) -> Result<ReadyDsh, LaunchError> {
    let mut command = Command::new(request.command.program());
    command.args(request.command.prefix_args());
    command.args(build_args(request));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !request.env.is_empty() {
        command.env_clear();
        for (name, value) in &request.env {
            command.env(name, value);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // A dedicated process group lets `stop` signal the whole tree.
        command.process_group(0);
    }

    let program = request.command.program().to_path_buf();
    let mut child = command.spawn().map_err(|source| LaunchError::Spawn {
        program: program.clone(),
        source,
    })?;

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate(&mut child, DEFAULT_STOP_GRACE);
        return Err(LaunchError::Spawn {
            program,
            source: std::io::Error::other("child pipes were not captured"),
        });
    };

    let stdout_tail: SharedTail = Arc::new(Mutex::new(Vec::new()));
    let stderr_tail: SharedTail = Arc::new(Mutex::new(Vec::new()));
    let (line_sender, line_receiver) = mpsc::channel::<String>();

    drain_stdout(stdout, Arc::clone(&stdout_tail), line_sender);
    let stderr_reader = drain_stderr(stderr, Arc::clone(&stderr_tail));

    let started = Instant::now();
    loop {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate(&mut child, DEFAULT_STOP_GRACE);
            let _ = stderr_reader.join();
            return Err(LaunchError::Timeout {
                after: deadline,
                stderr_tail: tail_text(&stderr_tail),
            });
        }

        match line_receiver.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(url) = parse_ready_line(&line) {
                    return Ok(ReadyDsh {
                        url,
                        child: DshChild::new(child, stdout_tail, stderr_tail),
                    });
                }
                if readiness_token(&line).is_some() {
                    // Shaped like a readiness line but unusable: fail loudly
                    // instead of waiting for the deadline.
                    let reported = url_policy::redact_text(line.trim());
                    terminate(&mut child, DEFAULT_STOP_GRACE);
                    let _ = stderr_reader.join();
                    return Err(LaunchError::BadLine(reported));
                }
                // Any other stdout chatter is ignored; the deadline still bounds
                // the wait.
            }
            Err(RecvTimeoutError::Timeout) => {
                terminate(&mut child, DEFAULT_STOP_GRACE);
                let _ = stderr_reader.join();
                return Err(LaunchError::Timeout {
                    after: deadline,
                    stderr_tail: tail_text(&stderr_tail),
                });
            }
            Err(RecvTimeoutError::Disconnected) => {
                let code = child.wait().ok().and_then(|status| status.code());
                let _ = stderr_reader.join();
                return Err(LaunchError::Exited {
                    code,
                    stderr_tail: tail_text(&stderr_tail),
                });
            }
        }
    }
}

/// Reads stdout lines, keeping the tail and forwarding each line to the caller.
fn drain_stdout(stdout: std::process::ChildStdout, tail: SharedTail, lines: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => return,
                Ok(_) => {
                    push_tail(&tail, &buffer);
                    // A closed receiver means readiness was reached; keep
                    // draining so the child never blocks on a full pipe.
                    let _ = lines.send(String::from_utf8_lossy(&buffer).into_owned());
                }
                Err(_) => return,
            }
        }
    });
}

/// Keeps reading stderr so the child never blocks; the caller joins the handle
/// before reading the tail once the child is gone.
fn drain_stderr(
    stderr: std::process::ChildStderr,
    tail: SharedTail,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => push_tail(&tail, &buffer[..count]),
                Err(_) => return,
            }
        }
    })
}

fn push_tail(tail: &SharedTail, bytes: &[u8]) {
    let Ok(mut tail) = tail.lock() else {
        return;
    };
    tail.extend_from_slice(bytes);
    if tail.len() > TAIL_BYTES {
        let excess = tail.len() - TAIL_BYTES;
        tail.drain(..excess);
    }
}

fn tail_text(tail: &SharedTail) -> String {
    match tail.lock() {
        Ok(bytes) => url_policy::redact_text(&String::from_utf8_lossy(&bytes)),
        Err(_) => String::new(),
    }
}

impl DshChild {
    fn new(child: Child, stdout_tail: SharedTail, stderr_tail: SharedTail) -> Self {
        let pid = child.id();
        Self {
            child: Some(child),
            pid,
            stdout_tail,
            stderr_tail,
        }
    }

    /// Operating-system process id, recorded by the shell for its logs.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Retained stdout tail, token-redacted.
    pub fn stdout_tail(&self) -> String {
        tail_text(&self.stdout_tail)
    }

    /// Retained stderr tail, token-redacted.
    pub fn stderr_tail(&self) -> String {
        tail_text(&self.stderr_tail)
    }

    /// Asks the child to exit, then forces the whole process group after
    /// `grace`. Repeated calls after the first are no-ops.
    pub fn stop(&mut self, grace: Duration) {
        if let Some(mut child) = self.child.take() {
            terminate(&mut child, grace);
        }
    }
}

impl Drop for DshChild {
    fn drop(&mut self) {
        // Never leave an orphaned `dsh` behind when the shell exits.
        self.stop(DEFAULT_STOP_GRACE);
    }
}

/// Requests a normal exit and escalates to an unconditional kill after `grace`.
fn terminate(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: the child leads its own process group (see `spawn`); signaling
        // a group that is already gone fails with ESRCH and is ignored.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        if waits_within(child, grace) {
            return;
        }
        // SAFETY: as above; the group still exists because the child has not exited.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }
    #[cfg(windows)]
    {
        let _ = taskkill(child.id(), false);
        if waits_within(child, grace) {
            return;
        }
        let _ = taskkill(child.id(), true);
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = grace;
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Polls for exit until `grace` elapses.
fn waits_within(child: &mut Child, grace: Duration) -> bool {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return true,
        }
        if started.elapsed() >= grace {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Terminates a child and every process it started.
#[cfg(windows)]
fn taskkill(pid: u32, force: bool) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    let mut command = Command::new("taskkill");
    command.arg("/PID").arg(pid.to_string()).arg("/T");
    if force {
        command.arg("/F");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SpawnRequest {
        SpawnRequest {
            command: DshCommand::new("dsh", Vec::new()),
            profile: "web".to_owned(),
            port: 0,
            extra_args: Vec::new(),
            env: Vec::new(),
        }
    }

    fn arguments(request: &SpawnRequest) -> Vec<String> {
        build_args(request)
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_args_keeps_the_documented_order() {
        let mut spawn_request = request();
        assert_eq!(
            arguments(&spawn_request),
            vec![
                "--profile",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "0"
            ]
        );

        spawn_request.profile = "dsh-desktop".to_owned();
        spawn_request.port = 3080;
        spawn_request.extra_args = vec![
            "--verbose".to_owned(),
            "--log-level".to_owned(),
            "debug".to_owned(),
        ];
        assert_eq!(
            arguments(&spawn_request),
            vec![
                "--profile",
                "dsh-desktop",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "3080",
                "--verbose",
                "--log-level",
                "debug",
            ]
        );
    }

    #[test]
    fn readiness_line_yields_the_tokenized_url() {
        let url = parse_ready_line("dsh web: http://127.0.0.1:63849/?token=abc").expect("parses");
        assert_eq!(url.as_str(), "http://127.0.0.1:63849/?token=abc");
        assert_eq!(url.host_str(), Some("127.0.0.1"));

        let url = parse_ready_line("dsh web:   http://[::1]:3080/?token=abc\r\n")
            .expect("tolerates padding");
        assert_eq!(url.port(), Some(3080));
    }

    #[test]
    fn readiness_line_rejects_unusable_urls() {
        assert_eq!(
            parse_ready_line("dsh web: http://example.com/?token=abc"),
            None,
            "not loopback"
        );
        assert_eq!(
            parse_ready_line("dsh web: http://localhost:3080/?token=abc"),
            None,
            "localhost is not accepted"
        );
        assert_eq!(
            parse_ready_line("dsh web: http://127.0.0.1:3080/"),
            None,
            "no token"
        );
        assert_eq!(
            parse_ready_line("dsh web: http://127.0.0.1:3080/?token="),
            None,
            "empty token"
        );
        assert_eq!(
            parse_ready_line("dsh web: not a url"),
            None,
            "not one token"
        );
        assert_eq!(
            parse_ready_line("dsh web: http://127.0.0.1:3080/?token=abc trailing"),
            None
        );
        assert_eq!(
            parse_ready_line("dsh web: opening the default browser; pass --no-open to disable"),
            None,
            "informational notice, not a readiness line"
        );
        assert_eq!(
            parse_ready_line("listening on http://127.0.0.1:3080/?token=abc"),
            None,
            "unrelated line"
        );
        assert_eq!(parse_ready_line(""), None);
    }

    fn script(extension: &str, body: &str) -> DshCommand {
        // Tests run in parallel, so every fixture needs its own file name.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let directory =
            std::env::temp_dir().join(format!("dsh-launcher-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("create directory");
        let path = directory.join(format!(
            "fake-{}-{sequence}.{extension}",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("write script");

        #[cfg(windows)]
        {
            DshCommand::new(
                "powershell.exe",
                vec![
                    "-NoProfile".to_owned(),
                    "-ExecutionPolicy".to_owned(),
                    "Bypass".to_owned(),
                    "-File".to_owned(),
                    path.display().to_string(),
                ],
            )
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod");
            DshCommand::new("/bin/sh", vec![path.display().to_string()])
        }
    }

    /// Body of a fake `dsh` that prints one readiness line and exits.
    fn ready_body() -> &'static str {
        if cfg!(windows) {
            "Write-Output 'dsh web: http://127.0.0.1:63849/?token=secret'\n"
        } else {
            "#!/bin/sh\necho 'dsh web: http://127.0.0.1:63849/?token=secret'\n"
        }
    }

    fn failing_body() -> &'static str {
        if cfg!(windows) {
            "[Console]::Error.WriteLine('boom')\nexit 7\n"
        } else {
            "#!/bin/sh\necho boom 1>&2\nexit 7\n"
        }
    }

    fn hanging_body() -> &'static str {
        if cfg!(windows) {
            "[Console]::Error.WriteLine('still starting')\nStart-Sleep -Seconds 30\n"
        } else {
            "#!/bin/sh\necho 'still starting' 1>&2\nsleep 30\n"
        }
    }

    fn fake_request(command: DshCommand) -> SpawnRequest {
        SpawnRequest {
            command,
            ..request()
        }
    }

    #[test]
    fn spawn_returns_the_parsed_url_and_a_managed_child() {
        let command = script(if cfg!(windows) { "ps1" } else { "sh" }, ready_body());
        let deadline = Duration::from_secs(30);
        let mut ready = spawn(&fake_request(command), deadline).expect("announces readiness");
        assert_eq!(ready.url.as_str(), "http://127.0.0.1:63849/?token=secret");
        assert_ne!(ready.child.pid(), 0);
        assert!(!ready.url.as_str().is_empty());

        ready.child.stop(Duration::from_secs(10));
        ready.child.stop(Duration::from_secs(1));
    }

    #[test]
    fn an_early_exit_is_reported_with_its_stderr_tail() {
        let command = script(if cfg!(windows) { "ps1" } else { "sh" }, failing_body());
        let error = spawn(&fake_request(command), Duration::from_secs(30))
            .expect_err("does not become ready");
        let LaunchError::Exited { code, stderr_tail } = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(code, Some(7));
        assert!(stderr_tail.contains("boom"), "stderr tail: {stderr_tail}");
    }

    #[test]
    fn a_silent_child_times_out_and_is_killed() {
        let command = script(if cfg!(windows) { "ps1" } else { "sh" }, hanging_body());
        let started = Instant::now();
        let error = spawn(&fake_request(command), Duration::from_millis(800))
            .expect_err("never becomes ready");
        let LaunchError::Timeout { after, stderr_tail } = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(after, Duration::from_millis(800));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "took {:?}",
            started.elapsed()
        );
        assert!(
            stderr_tail.contains("still starting"),
            "stderr tail: {stderr_tail}"
        );
    }

    #[test]
    fn a_readiness_line_without_a_token_is_rejected_and_redacted() {
        let body = if cfg!(windows) {
            "Write-Output 'dsh web: http://127.0.0.1:3080/'\n"
        } else {
            "#!/bin/sh\necho 'dsh web: http://127.0.0.1:3080/'\n"
        };
        let command = script(if cfg!(windows) { "ps1" } else { "sh" }, body);
        let error = spawn(&fake_request(command), Duration::from_secs(30)).expect_err("no token");
        let LaunchError::BadLine(line) = error else {
            panic!("unexpected error: {error}");
        };
        assert!(line.contains("dsh web:"), "line: {line}");
        assert!(!line.contains("token="), "line: {line}");
    }

    #[test]
    fn launch_errors_explain_themselves() {
        let timeout = LaunchError::Timeout {
            after: Duration::from_secs(120),
            stderr_tail: "kaboom".to_owned(),
        };
        assert!(timeout.to_string().contains("120000 ms"));
        assert!(timeout.to_string().contains("kaboom"));

        let exited = LaunchError::Exited {
            code: Some(3),
            stderr_tail: String::new(),
        };
        assert!(exited.to_string().contains("code 3"));

        let spawn = LaunchError::Spawn {
            program: PathBuf::from("/opt/dsh"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert!(spawn.to_string().contains("/opt/dsh"));
    }

    /// Whether a process is still running, used to prove that no orphan survives.
    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Whether a process is still running, used to prove that no orphan survives.
    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        use std::os::windows::process::CommandExt;

        let output = Command::new("tasklist")
            .arg("/FI")
            .arg(format!("PID eq {pid}"))
            .arg("/NH")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()),
            // An unanswerable probe must not turn into a false "no orphan" claim.
            Err(_) => true,
        }
    }

    #[test]
    fn spawn_ignores_informational_lines_before_readiness() {
        let body = if cfg!(windows) {
            "Write-Output 'dsh web: opening the default browser; pass --no-open to disable'\nWrite-Output 'dsh web: http://127.0.0.1:63849/?token=secret'\n"
        } else {
            "#!/bin/sh\necho 'dsh web: opening the default browser; pass --no-open to disable'\necho 'dsh web: http://127.0.0.1:63849/?token=secret'\n"
        };
        let command = script(if cfg!(windows) { "ps1" } else { "sh" }, body);
        let mut ready = spawn(&fake_request(command), Duration::from_secs(30))
            .expect("the notice is not a readiness line");
        assert_eq!(ready.url.as_str(), "http://127.0.0.1:63849/?token=secret");
        ready.child.stop(Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn stop_ends_the_process_group() {
        let body = "#!/bin/sh\necho 'dsh web: http://127.0.0.1:63849/?token=secret'\nwhile true; do sleep 1; done\n";
        let mut ready = spawn(&fake_request(script("sh", body)), Duration::from_secs(10))
            .expect("child starts");
        let pid = ready.child.pid();
        assert!(
            process_alive(pid),
            "child {pid} should still run before stop"
        );

        ready.child.stop(Duration::from_secs(5));

        assert!(!process_alive(pid), "child {pid} survived stop");
    }

    /// Polls for a file written by a fake child, so its grandchild pid is known.
    #[cfg(windows)]
    fn wait_for_pid_file(path: &std::path::Path) -> Option<u32> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return Some(pid);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        None
    }

    #[cfg(windows)]
    #[test]
    fn stop_ends_the_whole_process_tree() {
        let marker =
            std::env::temp_dir().join(format!("dsh-grandchild-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let body = format!(
            "Write-Output 'dsh web: http://127.0.0.1:63849/?token=secret'\n\
             $grandchild = Start-Process -PassThru -WindowStyle Hidden -FilePath ping -ArgumentList '-n','90','127.0.0.1'\n\
             Set-Content -LiteralPath '{}' -Value $grandchild.Id\n\
             Start-Sleep -Seconds 90\n",
            marker.display()
        );
        let mut ready = spawn(&fake_request(script("ps1", &body)), Duration::from_secs(60))
            .expect("child starts");
        let pid = ready.child.pid();
        let grandchild = wait_for_pid_file(&marker).expect("grandchild pid recorded");
        assert!(
            process_alive(pid),
            "child {pid} should still run before stop"
        );
        assert!(
            process_alive(grandchild),
            "grandchild {grandchild} should be running"
        );

        // `taskkill /T` first requests a tree shutdown, then forces it.
        ready.child.stop(Duration::from_secs(5));

        assert!(!process_alive(pid), "child {pid} survived stop");
        assert!(
            !process_alive(grandchild),
            "grandchild {grandchild} survived stop"
        );
        let _ = std::fs::remove_file(&marker);
    }
}
