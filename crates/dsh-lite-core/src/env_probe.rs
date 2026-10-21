//! Login-shell environment capture for the `dsh` child process (FR-2).
//!
//! A GUI launch inherits the launcher's environment, which on macOS and Linux
//! lacks everything the user exported in their shell. The shell therefore runs
//! the account's login shell once, reads `env -0` from it, and merges the result
//! over its own environment. Windows needs no probe: a GUI process already
//! receives the registry-defined user and system variables.

use std::ffi::OsString;

#[cfg(unix)]
use std::time::Duration;

use crate::config::{EnvConfig, Platform};

/// Variables set for the probe so prompt and tmux plugins do not block it.
pub const PROBE_ENV: [(&str, &str); 3] = [
    ("DISABLE_AUTO_UPDATE", "true"),
    ("ZSH_TMUX_AUTOSTARTED", "true"),
    ("ZSH_TMUX_AUTOSTART", "false"),
];

/// Default probe deadline, matching `[env] timeoutMs`.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Names the shell owns even when the login shell reports a different value.
pub const SHELL_OWNED_NAMES: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

/// Name prefix reserved for the shell's own settings.
pub const RESERVED_PREFIX: &str = "DSH_LITE_";

/// Shells tried after the account's login shell, in order.
#[cfg(unix)]
const FALLBACK_SHELLS: [&str; 3] = ["/bin/zsh", "/bin/bash", "/bin/sh"];

/// Probe settings from `[env]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvProbeConfig {
    /// Read the login shell; `false` inherits this process' environment.
    pub inherit: bool,
    /// Deadline for one probe attempt.
    pub timeout_ms: u64,
    /// Shell to run; `None` uses the account's login shell.
    pub shell: Option<String>,
}

impl Default for EnvProbeConfig {
    fn default() -> Self {
        Self {
            inherit: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            shell: None,
        }
    }
}

impl From<&EnvConfig> for EnvProbeConfig {
    fn from(config: &EnvConfig) -> Self {
        let shell = config.shell.trim();
        Self {
            inherit: config.inherit,
            timeout_ms: config.timeout_ms,
            shell: (!shell.is_empty()).then(|| shell.to_owned()),
        }
    }
}

/// Where the child environment came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSource {
    /// This process' environment, unchanged.
    Inherited,
    /// A login shell probe.
    LoginShell {
        /// Shell binary that produced the environment.
        shell: String,
    },
}

/// Diagnostics for the FR-9 log: names and counts, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvProbeReport {
    /// Which mechanism produced the environment.
    pub source: EnvSource,
    /// Number of variables handed to the child.
    pub variable_count: usize,
    /// Number of inherited variables the probe replaced.
    pub overridden: usize,
    /// Non-fatal problems, including fallbacks that were tried.
    pub warnings: Vec<String>,
}

/// Environment for the `dsh` child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildEnvironment {
    /// Variables in child order.
    pub vars: Vec<(OsString, OsString)>,
    /// What produced them.
    pub report: EnvProbeReport,
}

/// Merges probe output over the inherited environment.
///
/// Probed values replace inherited ones, except for the names in
/// [`SHELL_OWNED_NAMES`] and anything starting with [`RESERVED_PREFIX`]: those
/// stay under the shell's control so the probe cannot redirect the child's
/// working directory or its configuration. The returned warnings name every
/// protected variable the login shell also set.
pub fn merge_probe_output(
    own: &[(OsString, OsString)],
    probed: &[(OsString, OsString)],
) -> (Vec<(OsString, OsString)>, Vec<String>) {
    let mut vars = own.to_vec();
    let mut warnings = Vec::new();

    for (name, value) in probed {
        match vars
            .iter_mut()
            .find(|(existing, _)| names_equal(existing, name))
        {
            Some(_) if is_shell_owned(name) => {
                warnings.push(format!("kept the shell's own {}", name.to_string_lossy()));
            }
            Some(slot) => slot.1 = value.clone(),
            None => vars.push((name.clone(), value.clone())),
        }
    }

    (vars, warnings)
}

/// Counts inherited variables whose value the probe replaced.
pub fn count_overridden(own: &[(OsString, OsString)], merged: &[(OsString, OsString)]) -> usize {
    own.iter()
        .filter(|(name, value)| {
            merged
                .iter()
                .any(|(other, merged_value)| names_equal(name, other) && merged_value != value)
        })
        .count()
}

fn is_shell_owned(name: &OsString) -> bool {
    let text = name.to_string_lossy();
    if SHELL_OWNED_NAMES
        .iter()
        .any(|owned| text.eq_ignore_ascii_case(owned))
    {
        return true;
    }
    text.len() >= RESERVED_PREFIX.len()
        && text[..RESERVED_PREFIX.len()].eq_ignore_ascii_case(RESERVED_PREFIX)
}

fn names_equal(left: &OsString, right: &OsString) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Parses NUL-terminated `env -0` output.
///
/// Empty segments and segments without `=` are skipped; everything after the
/// first `=` is the value, so values may contain `=`. Empty names are skipped.
pub fn parse_env_nul(bytes: &[u8]) -> Vec<(OsString, OsString)> {
    let mut vars = Vec::new();
    for segment in bytes.split(|byte| *byte == 0) {
        let Some(position) = segment.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (name, value) = segment.split_at(position);
        if name.is_empty() {
            continue;
        }
        vars.push((os_string(name), os_string(&value[1..])));
    }
    vars
}

#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Command line of one probe attempt: `<shell> -ilc 'env -0'`.
pub fn probe_command(shell: &str) -> (String, Vec<String>) {
    (
        shell.to_owned(),
        vec!["-ilc".to_owned(), "env -0".to_owned()],
    )
}

/// Builds the child environment for `platform`.
///
/// Windows and `inherit = false` return this process' environment unchanged.
/// Elsewhere the login shell is probed; when every attempt fails the inherited
/// environment is returned with a warning instead of failing the launch.
pub fn collect(
    platform: Platform,
    config: &EnvProbeConfig,
    own: &[(OsString, OsString)],
) -> ChildEnvironment {
    if platform == Platform::Windows || !config.inherit {
        return inherited(own);
    }

    #[cfg(unix)]
    {
        collect_from_login_shell(config, own)
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        inherited(own)
    }
}

fn inherited(own: &[(OsString, OsString)]) -> ChildEnvironment {
    let vars = own.to_vec();
    ChildEnvironment {
        report: EnvProbeReport {
            source: EnvSource::Inherited,
            variable_count: vars.len(),
            overridden: 0,
            warnings: Vec::new(),
        },
        vars,
    }
}

#[cfg(unix)]
fn collect_from_login_shell(
    config: &EnvProbeConfig,
    own: &[(OsString, OsString)],
) -> ChildEnvironment {
    let mut warnings = Vec::new();
    let mut shells: Vec<String> = Vec::new();
    if let Some(shell) = config.shell.clone() {
        shells.push(shell);
    } else if let Some(shell) = login_shell() {
        shells.push(shell);
    } else {
        warnings.push("could not read the login shell from the account database".to_owned());
    }
    for fallback in FALLBACK_SHELLS {
        if !shells.iter().any(|existing| existing == fallback) {
            shells.push(fallback.to_owned());
        }
    }

    let timeout = Duration::from_millis(config.timeout_ms.max(1));
    for shell in shells {
        match run_probe(&shell, timeout) {
            Ok(probed) if !probed.is_empty() => {
                let (vars, mut kept) = merge_probe_output(own, &probed);
                let overridden = count_overridden(own, &vars);
                warnings.append(&mut kept);
                return ChildEnvironment {
                    report: EnvProbeReport {
                        source: EnvSource::LoginShell { shell },
                        variable_count: vars.len(),
                        overridden,
                        warnings,
                    },
                    vars,
                };
            }
            Ok(_) => warnings.push(format!("{shell} returned no variables")),
            Err(detail) => warnings.push(format!("{shell}: {detail}")),
        }
    }

    warnings.push("login shell probe failed; inheriting this process' environment".to_owned());
    let mut environment = inherited(own);
    environment.report.warnings = warnings;
    environment
}

/// Runs one probe attempt and returns the parsed variables.
#[cfg(unix)]
fn run_probe(shell: &str, timeout: Duration) -> Result<Vec<(OsString, OsString)>, String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let (program, args) = probe_command(shell);
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (name, value) in PROBE_ENV {
        command.env(name, value);
    }
    // SAFETY: `setsid` is async-signal-safe and the closure allocates nothing,
    // which is the contract `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start ({error})"))?;
    let Some(mut stdout) = child.stdout.take() else {
        return Err("stdout was not captured".to_owned());
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = sender.send(buffer);
    });

    match receiver.recv_timeout(timeout) {
        Ok(bytes) => {
            let status = child
                .wait()
                .map_err(|error| format!("could not wait ({error})"))?;
            if !status.success() {
                return Err(format!("exited with {}", status.code().unwrap_or(-1)));
            }
            Ok(parse_env_nul(&bytes))
        }
        Err(_) => {
            kill_process_group(&mut child);
            Err(format!("timed out after {} ms", timeout.as_millis()))
        }
    }
}

/// Kills the whole probe process group so a hung rc script cannot survive.
#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: a negative pid targets the group `setsid` created for this child;
    // a signal to an already dead group fails with ESRCH and is ignored.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads the login shell from the account database, never from `$SHELL`.
#[cfg(unix)]
fn login_shell() -> Option<String> {
    // SAFETY: `getpwuid` returns a pointer into static storage valid until the
    // next lookup; the shell string is copied out before anything else runs.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            return None;
        }
        let shell = (*entry).pw_shell;
        if shell.is_null() {
            return None;
        }
        let text = std::ffi::CStr::from_ptr(shell)
            .to_string_lossy()
            .trim()
            .to_owned();
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(entries: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        entries
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn values(vars: &[(OsString, OsString)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn probed_values_replace_inherited_ones() {
        let own = pairs(&[("PATH", "/usr/bin"), ("LANG", "C")]);
        let probed = pairs(&[("PATH", "/opt/bin:/usr/bin"), ("FOO", "1")]);
        let (vars, warnings) = merge_probe_output(&own, &probed);

        assert_eq!(
            values(&vars),
            vec![
                ("PATH".to_owned(), "/opt/bin:/usr/bin".to_owned()),
                ("LANG".to_owned(), "C".to_owned()),
                ("FOO".to_owned(), "1".to_owned()),
            ]
        );
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(count_overridden(&own, &vars), 1);
    }

    #[test]
    fn shell_owned_names_keep_the_shells_value() {
        let own = pairs(&[
            ("PWD", "/work"),
            ("OLDPWD", "/old"),
            ("SHLVL", "1"),
            ("_", "/bin/fish"),
        ]);
        let probed = pairs(&[
            ("PWD", "/home/me"),
            ("OLDPWD", "/"),
            ("SHLVL", "2"),
            ("_", "/opt/dsh"),
        ]);
        let (vars, warnings) = merge_probe_output(&own, &probed);

        assert_eq!(values(&vars), values(&own));
        assert_eq!(warnings.len(), 4, "warnings: {warnings:?}");
        assert_eq!(count_overridden(&own, &vars), 0);
    }

    #[test]
    fn reserved_prefix_is_never_overridden() {
        let own = pairs(&[("DSH_LITE_DSH_PATH", "/opt/dsh"), ("dsh_lite_port", "0")]);
        let probed = pairs(&[
            ("DSH_LITE_DSH_PATH", "/other"),
            ("dsh_lite_port", "3080"),
            ("DSH_LITE_NEW", "x"),
        ]);
        let (vars, warnings) = merge_probe_output(&own, &probed);

        assert_eq!(
            values(&vars),
            vec![
                ("DSH_LITE_DSH_PATH".to_owned(), "/opt/dsh".to_owned()),
                ("dsh_lite_port".to_owned(), "0".to_owned()),
                ("DSH_LITE_NEW".to_owned(), "x".to_owned()),
            ]
        );
        assert_eq!(warnings.len(), 2, "warnings: {warnings:?}");
    }

    #[test]
    fn names_absent_from_the_inherited_set_are_adopted() {
        let own = pairs(&[("PATH", "/usr/bin")]);
        let probed = pairs(&[("PWD", "/home/me"), ("SHLVL", "1")]);
        let (vars, _) = merge_probe_output(&own, &probed);
        assert_eq!(vars.len(), 3);
        assert!(values(&vars).contains(&("PWD".to_owned(), "/home/me".to_owned())));
    }

    #[test]
    fn nul_output_parsing_skips_junk_and_keeps_equals_in_values() {
        let parsed = parse_env_nul(b"A=1\0B=x=y\0\0NOEQUALS\0C=\0=orphan\0");
        assert_eq!(
            values(&parsed),
            vec![
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "x=y".to_owned()),
                ("C".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn probe_command_matches_the_documented_invocation() {
        assert_eq!(
            probe_command("/bin/zsh"),
            (
                "/bin/zsh".to_owned(),
                vec!["-ilc".to_owned(), "env -0".to_owned()]
            )
        );
        assert_eq!(PROBE_ENV.len(), 3);
        assert!(PROBE_ENV.contains(&("DISABLE_AUTO_UPDATE", "true")));
        assert!(PROBE_ENV.contains(&("ZSH_TMUX_AUTOSTART", "false")));
    }

    #[test]
    fn windows_and_disabled_probes_inherit_unchanged() {
        let own = pairs(&[("PATH", "/usr/bin"), ("FOO", "1")]);
        let config = EnvProbeConfig {
            inherit: false,
            timeout_ms: 1,
            shell: Some("/nonexistent".to_owned()),
        };

        let environment = collect(Platform::Windows, &config, &own);
        assert_eq!(environment.vars, own);
        assert_eq!(environment.report.source, EnvSource::Inherited);
        assert_eq!(environment.report.variable_count, 2);
        assert!(environment.report.warnings.is_empty());

        let environment = collect(Platform::MacOs, &config, &own);
        assert_eq!(environment.vars, own);
        assert_eq!(environment.report.source, EnvSource::Inherited);
    }

    #[test]
    fn config_conversion_blank_shell_means_account_default() {
        let config = EnvConfig {
            inherit: true,
            timeout_ms: 1234,
            shell: "   ".to_owned(),
        };
        let probe = EnvProbeConfig::from(&config);
        assert_eq!(
            probe,
            EnvProbeConfig {
                inherit: true,
                timeout_ms: 1234,
                shell: None
            }
        );

        let config = EnvConfig {
            inherit: false,
            timeout_ms: 10,
            shell: "/bin/dash".to_owned(),
        };
        assert_eq!(
            EnvProbeConfig::from(&config).shell.as_deref(),
            Some("/bin/dash")
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_probe_reads_the_login_shell_environment() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let own = pairs(&[("DSH_LITE_DSH_PATH", "/opt/dsh"), ("PWD", "/shell-cwd")]);
        let config = EnvProbeConfig {
            inherit: true,
            timeout_ms: 10_000,
            shell: Some("/bin/sh".to_owned()),
        };
        let environment = collect(Platform::Unix, &config, &own);

        assert_eq!(
            environment.report.source,
            EnvSource::LoginShell {
                shell: "/bin/sh".to_owned()
            }
        );
        assert!(environment.report.variable_count > 1);
        let vars = values(&environment.vars);
        assert!(vars.contains(&("DSH_LITE_DSH_PATH".to_owned(), "/opt/dsh".to_owned())));
        assert!(vars.contains(&("PWD".to_owned(), "/shell-cwd".to_owned())));
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_shell_falls_back_and_warns() {
        let own = pairs(&[("PATH", "/usr/bin")]);
        let config = EnvProbeConfig {
            inherit: true,
            timeout_ms: 5_000,
            shell: Some("/nonexistent/shell".to_owned()),
        };
        let environment = collect(Platform::Unix, &config, &own);

        assert!(
            environment
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("/nonexistent/shell")),
            "warnings: {:?}",
            environment.report.warnings
        );
        assert!(environment.report.variable_count >= 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_login_shell_is_killed_at_the_deadline() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp dir");
        let shell = directory.path().join("hung-shell");
        fs::write(&shell, "#!/bin/sh\nsleep 30\n").expect("write shell");
        let mut permissions = fs::metadata(&shell).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&shell, permissions).expect("chmod");

        let started = std::time::Instant::now();
        let result = run_probe(
            shell.to_str().expect("utf-8 path"),
            Duration::from_millis(300),
        );
        assert!(result.is_err(), "expected a timeout");
        assert!(started.elapsed() < Duration::from_secs(20));
        let message = result.expect_err("timeout");
        assert!(message.contains("timed out"), "message: {message}");
    }
}
