//! Discovery of the `dsh` executable plus the profile preflight classification.
//!
//! The search order follows FR-1: command line, configuration file,
//! `DSH_LITE_DSH_PATH`, `PATH`, then well-known install locations. A path named
//! by the user (command line, configuration, environment) must resolve: a
//! missing or non-executable override is a hard error instead of a silent
//! fallback, so a stale `dsh.path` never degrades into another installation
//! without the user seeing it. Only the `PATH` and well-known steps may miss.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Lowest `dsh` version the shell accepts, mirroring the upstream `engines`
/// lower bound.
pub const MIN_VERSION: (u64, u64, u64) = (0, 2, 0);

/// Exclusive upper bound of the accepted `dsh` version range.
pub const MAX_VERSION_EXCLUSIVE: (u64, u64, u64) = (1, 0, 0);

/// Extensions tried for `dsh` on Windows, in the order used when `PATHEXT`
/// does not mention them.
#[cfg(windows)]
const WINDOWS_EXTENSIONS: [&str; 4] = ["exe", "cmd", "bat", "ps1"];

/// A `dsh` command line: the executable plus any wrapper arguments a script
/// suffix requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshCommand {
    pub(crate) program: PathBuf,
    pub(crate) prefix_args: Vec<String>,
}

impl DshCommand {
    /// Builds a command from an already resolved program and wrapper arguments.
    pub fn new(program: impl Into<PathBuf>, prefix_args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            prefix_args,
        }
    }

    /// Executable to start; a wrapper host for `.cmd`/`.bat`/`.ps1` scripts.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Arguments that precede the user's `dsh` arguments.
    pub fn prefix_args(&self) -> &[String] {
        &self.prefix_args
    }

    /// Single-line form for logs and error pages.
    pub fn display(&self) -> String {
        let mut text = self.program.display().to_string();
        for argument in &self.prefix_args {
            text.push(' ');
            text.push_str(argument);
        }
        text
    }
}

/// Host environment the discovery order reads, all optional so tests and the
/// assembler can supply a synthetic environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocateInputs {
    /// `--dsh-path` from the command line.
    pub cli_path: Option<PathBuf>,
    /// `dsh.path` from the configuration file.
    pub config_path: Option<PathBuf>,
    /// `DSH_LITE_DSH_PATH`.
    pub env_path: Option<PathBuf>,
    /// `PATH`.
    pub path_var: Option<OsString>,
    /// `PATHEXT` on Windows; ignored elsewhere.
    pub path_ext: Option<OsString>,
    /// `DSH_HOME`, the root holding `bin/dsh`.
    pub dsh_home: Option<PathBuf>,
    /// Home directory used for `~/.bun/bin` and `~/.local/bin`.
    pub home: Option<PathBuf>,
    /// `%LOCALAPPDATA%`, holding the pnpm global bin on Windows.
    pub local_app_data: Option<PathBuf>,
    /// `%APPDATA%`, holding the npm global prefix on Windows.
    pub app_data: Option<PathBuf>,
    /// npm global prefix when it is configured explicitly.
    pub npm_prefix: Option<PathBuf>,
}

impl LocateInputs {
    /// Collects the ambient process values, leaving the two user overrides
    /// (`cli_path`, `config_path`) to the caller.
    pub fn from_process_environment() -> Self {
        let path = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Self {
            path_var: std::env::var_os("PATH"),
            path_ext: std::env::var_os("PATHEXT"),
            env_path: path("DSH_LITE_DSH_PATH"),
            dsh_home: path("DSH_HOME"),
            home: path("HOME").or_else(|| path("USERPROFILE")),
            local_app_data: path("LOCALAPPDATA"),
            app_data: path("APPDATA"),
            npm_prefix: path("NPM_CONFIG_PREFIX"),
            ..Self::default()
        }
    }
}

/// Discovery failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// No candidate resolved; `tried` lists every path that was attempted.
    NotFound { tried: Vec<String> },
    /// A named path exists in the search order but cannot be started.
    NotExecutable(PathBuf),
    /// A source was selected but its value is empty.
    MissingValue(&'static str),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { tried } => {
                write!(f, "could not find dsh; tried")?;
                for entry in tried {
                    write!(f, "\n  - {entry}")?;
                }
                Ok(())
            }
            Self::NotExecutable(path) => {
                write!(f, "dsh path {} is not executable", path.display())
            }
            Self::MissingValue(source) => write!(f, "{source} is empty"),
        }
    }
}

impl std::error::Error for LocateError {}

/// Discovers `dsh` following the FR-1 order, stopping at the first hit.
pub fn locate(inputs: &LocateInputs) -> Result<DshCommand, LocateError> {
    if let Some(path) = &inputs.cli_path {
        return resolve_explicit(path, "--dsh-path").map(|path| wrap_executable(path, inputs));
    }
    if let Some(path) = &inputs.config_path {
        return resolve_explicit(path, "dsh.path").map(|path| wrap_executable(path, inputs));
    }
    if let Some(path) = &inputs.env_path {
        return resolve_explicit(path, "DSH_LITE_DSH_PATH")
            .map(|path| wrap_executable(path, inputs));
    }

    let mut tried = Vec::new();

    if let Some(path_var) = &inputs.path_var {
        for directory in std::env::split_paths(path_var) {
            // An empty `PATH` entry means the working directory; searching it
            // would let a file in the current folder shadow the real install.
            if directory.as_os_str().is_empty() {
                continue;
            }
            for candidate in directory_candidates(&directory, inputs) {
                if is_executable_file(&candidate) {
                    return Ok(wrap_executable(candidate, inputs));
                }
                tried.push(candidate.display().to_string());
            }
        }
    }

    for candidate in well_known_candidates(inputs) {
        if is_executable_file(&candidate) {
            return Ok(wrap_executable(candidate, inputs));
        }
        tried.push(candidate.display().to_string());
    }

    Err(LocateError::NotFound { tried })
}

/// Resolves a user-supplied path, reporting empties and unusable files.
fn resolve_explicit(path: &Path, source: &'static str) -> Result<PathBuf, LocateError> {
    if path.as_os_str().is_empty() {
        return Err(LocateError::MissingValue(source));
    }
    if !is_executable_file(path) {
        return Err(LocateError::NotExecutable(path.to_path_buf()));
    }
    Ok(path.to_path_buf())
}

/// Maps an executable file to the command that actually starts it.
///
/// `.ps1` runs through `pwsh` when it is on `PATH` and through
/// `powershell.exe` otherwise; `.cmd`/`.bat` run through `cmd.exe /c`. Other
/// suffixes and every non-Windows platform start the file directly.
pub fn wrap_executable(path: impl Into<PathBuf>, inputs: &LocateInputs) -> DshCommand {
    let path = path.into();
    #[cfg(windows)]
    if let Some(command) = wrap_script(&path, inputs) {
        return command;
    }
    let _ = inputs;
    DshCommand {
        program: path,
        prefix_args: Vec::new(),
    }
}

/// Wraps a Windows script in the host process able to run it.
#[cfg(windows)]
fn wrap_script(path: &Path, inputs: &LocateInputs) -> Option<DshCommand> {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("ps1") => Some(DshCommand {
            program: powershell_host(inputs),
            prefix_args: vec![
                "-NoProfile".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-File".to_owned(),
                path.display().to_string(),
            ],
        }),
        Some("cmd") | Some("bat") => Some(DshCommand {
            program: PathBuf::from("cmd.exe"),
            prefix_args: vec!["/c".to_owned(), path.display().to_string()],
        }),
        _ => None,
    }
}

/// PowerShell host for `.ps1` scripts: `pwsh` when available, else Windows
/// PowerShell.
#[cfg(windows)]
fn powershell_host(inputs: &LocateInputs) -> PathBuf {
    if let Some(path_var) = &inputs.path_var {
        for directory in std::env::split_paths(path_var) {
            let candidate = directory.join("pwsh.exe");
            if is_executable_file(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from("powershell.exe")
}

/// Candidate file names inside one `PATH` directory.
fn directory_candidates(directory: &Path, inputs: &LocateInputs) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut extensions: Vec<String> = Vec::new();
        if let Some(path_ext) = &inputs.path_ext {
            for entry in path_ext.to_string_lossy().split(';') {
                let extension = entry.trim().trim_start_matches('.').to_ascii_lowercase();
                let known = WINDOWS_EXTENSIONS.contains(&extension.as_str());
                if known && !extensions.contains(&extension) {
                    extensions.push(extension);
                }
            }
        }
        for extension in WINDOWS_EXTENSIONS {
            let extension = extension.to_owned();
            if !extensions.contains(&extension) {
                extensions.push(extension);
            }
        }
        extensions
            .into_iter()
            .map(|extension| directory.join(format!("dsh.{extension}")))
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = inputs;
        vec![directory.join("dsh")]
    }
}

/// Install locations outside `PATH`, in the order the requirements list them.
fn well_known_candidates(inputs: &LocateInputs) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut push = |directory: &Path| {
        for candidate in directory_candidates(directory, inputs) {
            candidates.push(candidate);
        }
    };

    if let Some(dsh_home) = &inputs.dsh_home {
        push(&dsh_home.join("bin"));
    }
    if let Some(npm_prefix) = &inputs.npm_prefix {
        push(&npm_prefix_dir(npm_prefix));
    }
    if let Some(local_app_data) = &inputs.local_app_data {
        push(&local_app_data.join("pnpm"));
    }
    if let Some(directory) = inputs.app_data.as_deref().and_then(app_data_npm_dir) {
        push(&directory);
    }
    if let Some(home) = &inputs.home {
        push(&home.join(".bun/bin"));
        push(&home.join(".local/bin"));
    }

    candidates
}

/// Directory holding `dsh` inside an npm global prefix.
#[cfg(windows)]
fn npm_prefix_dir(prefix: &Path) -> PathBuf {
    prefix.to_path_buf()
}

/// Directory holding `dsh` inside an npm global prefix.
#[cfg(not(windows))]
fn npm_prefix_dir(prefix: &Path) -> PathBuf {
    prefix.join("bin")
}

/// npm's global prefix inside `%APPDATA%`; not a Windows-only convention
/// elsewhere, so other platforms rely on `npm_prefix` alone.
#[cfg(windows)]
fn app_data_npm_dir(app_data: &Path) -> Option<PathBuf> {
    Some(app_data.join("npm"))
}

/// npm's global prefix inside `%APPDATA%`; not a Windows-only convention
/// elsewhere, so other platforms rely on `npm_prefix` alone.
#[cfg(not(windows))]
fn app_data_npm_dir(_app_data: &Path) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// Extracts the first semver-shaped version from `text`.
///
/// `dsh --version` prints a bare version, but tolerating a prefix keeps the
/// check usable when the value is copied out of a log line or a package banner.
pub fn parse_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let starts_token =
            index == 0 || !matches!(bytes[index - 1], b'0'..=b'9' | b'.' | b'-' | b'+');
        if bytes[index].is_ascii_digit()
            && starts_token
            && let Some(version) = version_at(text, index)
        {
            return Some(version);
        }
        index += 1;
    }
    None
}

/// Reads `major.minor.patch[-pre][+build]` starting at `start`.
fn version_at(text: &str, start: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut cursor = start;
    for part in 0..3 {
        let digits_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start {
            return None;
        }
        if part < 2 {
            if cursor >= bytes.len() || bytes[cursor] != b'.' {
                return None;
            }
            cursor += 1;
        }
    }
    for marker in b"-+" {
        if cursor < bytes.len() && bytes[cursor] == *marker {
            cursor += 1;
            let suffix_start = cursor;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric()
                    || matches!(bytes[cursor], b'.' | b'-' | b'+'))
            {
                cursor += 1;
            }
            if cursor == suffix_start {
                return None;
            }
        }
    }
    text.get(start..cursor).map(str::to_owned)
}

/// Reports whether a `dsh` version satisfies the accepted range.
///
/// The range is `>= 0.2.0` and `< 1.0.0` compared on the release triple, so
/// prereleases such as `0.2.0-rc.1` count as `0.2.0` and are accepted.
pub fn version_satisfies_engines(version: &str) -> bool {
    let Some(parsed) = parse_version(version) else {
        return false;
    };
    let core = parsed.split(['-', '+']).next().unwrap_or_default();
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let (Some(major), Some(minor), Some(patch)) = (
        parts.next().flatten(),
        parts.next().flatten(),
        parts.next().flatten(),
    ) else {
        return false;
    };
    let triple = (major, minor, patch);
    triple >= MIN_VERSION && triple < MAX_VERSION_EXCLUSIVE
}

/// Outcome of `dsh --profile <name> --dump-config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// The profile exists and can be started.
    Ready,
    /// dsh reported a missing profile; `hint` is its message verbatim.
    MissingProfile { profile: String, hint: String },
    /// Anything else failed.
    Failed { summary: String },
}

/// Classifies a preflight run from its exit code and captured stderr.
pub fn classify_preflight(exit_code: i32, stderr: &str) -> PreflightOutcome {
    if exit_code == 0 {
        return PreflightOutcome::Ready;
    }
    if let Some(profile) = missing_profile_name(stderr) {
        let hint = stderr
            .lines()
            .map(str::trim)
            .find(|line| line.contains("does not exist"))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("profile \"{profile}\" does not exist"));
        return PreflightOutcome::MissingProfile { profile, hint };
    }
    PreflightOutcome::Failed {
        summary: summarize(stderr, exit_code),
    }
}

/// Reads the profile name out of `profile "<name>" does not exist`.
fn missing_profile_name(stderr: &str) -> Option<String> {
    let marker = "profile \"";
    let start = stderr.find(marker)? + marker.len();
    let rest = stderr.get(start..)?;
    let end = rest.find('"')?;
    let profile = rest.get(..end)?.trim();
    (!profile.is_empty()).then(|| profile.to_owned())
}

/// Joins stderr into one readable line for an error page.
fn summarize(stderr: &str, exit_code: i32) -> String {
    let joined: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if joined.is_empty() {
        return format!("dsh exited with code {exit_code}");
    }
    let text = joined.join(" ");
    const LIMIT: usize = 512;
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("chmod");
        }
        path
    }

    fn dsh_name() -> &'static str {
        if cfg!(windows) { "dsh.exe" } else { "dsh" }
    }

    #[cfg(windows)]
    fn arguments(command: &DshCommand) -> Vec<&str> {
        command.prefix_args().iter().map(String::as_str).collect()
    }

    #[test]
    fn explicit_overrides_win_in_documented_order() {
        let directory = tempfile::tempdir().expect("temp dir");
        let cli = fixture(directory.path(), "cli-dsh");
        let configured = fixture(directory.path(), "config-dsh");
        let from_env = fixture(directory.path(), "env-dsh");

        let inputs = LocateInputs {
            cli_path: Some(cli.clone()),
            config_path: Some(configured.clone()),
            env_path: Some(from_env.clone()),
            ..LocateInputs::default()
        };
        assert_eq!(locate(&inputs).expect("cli wins").program(), cli.as_path());

        let inputs = LocateInputs {
            cli_path: None,
            config_path: Some(configured.clone()),
            env_path: Some(from_env.clone()),
            ..LocateInputs::default()
        };
        assert_eq!(
            locate(&inputs).expect("config wins").program(),
            configured.as_path()
        );

        let inputs = LocateInputs {
            cli_path: None,
            config_path: None,
            env_path: Some(from_env.clone()),
            ..LocateInputs::default()
        };
        assert_eq!(
            locate(&inputs).expect("env wins").program(),
            from_env.as_path()
        );
    }

    #[test]
    fn a_named_path_that_cannot_run_fails_loudly() {
        let directory = tempfile::tempdir().expect("temp dir");
        let missing = directory.path().join("nope");
        let inputs = LocateInputs {
            cli_path: Some(missing.clone()),
            ..LocateInputs::default()
        };
        assert_eq!(locate(&inputs), Err(LocateError::NotExecutable(missing)));
    }

    #[test]
    fn empty_override_names_its_source() {
        let inputs = LocateInputs {
            cli_path: Some(PathBuf::new()),
            ..LocateInputs::default()
        };
        assert_eq!(
            locate(&inputs),
            Err(LocateError::MissingValue("--dsh-path"))
        );

        let inputs = LocateInputs {
            env_path: Some(PathBuf::new()),
            ..LocateInputs::default()
        };
        assert_eq!(
            locate(&inputs),
            Err(LocateError::MissingValue("DSH_LITE_DSH_PATH"))
        );
    }

    #[test]
    fn path_search_finds_the_platform_executable() {
        let directory = tempfile::tempdir().expect("temp dir");
        let expected = fixture(directory.path(), dsh_name());
        let inputs = LocateInputs {
            path_var: Some(std::env::join_paths([directory.path()]).expect("join paths")),
            ..LocateInputs::default()
        };
        let command = locate(&inputs).expect("found on PATH");
        assert_eq!(command.program(), expected.as_path());
        assert!(command.prefix_args().is_empty());
    }

    #[test]
    fn well_known_locations_are_searched_after_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let bin = directory.path().join("bin");
        fs::create_dir_all(&bin).expect("create dir");
        let expected = fixture(&bin, dsh_name());

        let inputs = LocateInputs {
            dsh_home: Some(directory.path().to_path_buf()),
            ..LocateInputs::default()
        };
        assert_eq!(
            locate(&inputs).expect("found under DSH_HOME").program(),
            expected.as_path()
        );
    }

    #[test]
    fn missing_everywhere_lists_what_was_tried() {
        let directory = tempfile::tempdir().expect("temp dir");
        let inputs = LocateInputs {
            path_var: Some(std::env::join_paths([directory.path()]).expect("join paths")),
            home: Some(directory.path().to_path_buf()),
            ..LocateInputs::default()
        };
        let error = locate(&inputs).expect_err("nothing to find");
        let message = error.to_string();
        let LocateError::NotFound { tried } = &error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(
            tried.iter().any(|entry| entry.ends_with(dsh_name())),
            "tried: {tried:?}"
        );
        assert!(
            tried.iter().any(|entry| entry.contains(".bun")),
            "tried: {tried:?}"
        );
        assert!(message.contains("could not find dsh"), "message: {message}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_script_suffixes_run_through_their_host() {
        let inputs = LocateInputs::default();

        let batch = wrap_executable(PathBuf::from(r"C:\npm\dsh.cmd"), &inputs);
        assert_eq!(batch.program(), Path::new("cmd.exe"));
        assert_eq!(arguments(&batch), vec!["/c", r"C:\npm\dsh.cmd"]);

        let script = wrap_executable(PathBuf::from(r"C:\npm\dsh.ps1"), &inputs);
        assert_eq!(script.program(), Path::new("powershell.exe"));
        assert_eq!(
            arguments(&script),
            vec![
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\npm\dsh.ps1"
            ]
        );

        let native = wrap_executable(PathBuf::from(r"C:\npm\dsh.exe"), &inputs);
        assert_eq!(native.program(), Path::new(r"C:\npm\dsh.exe"));
        assert!(native.prefix_args().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_powershell_prefers_pwsh_from_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let pwsh = fixture(directory.path(), "pwsh.exe");
        let inputs = LocateInputs {
            path_var: Some(std::env::join_paths([directory.path()]).expect("join paths")),
            ..LocateInputs::default()
        };
        let command = wrap_executable(PathBuf::from(r"C:\npm\dsh.ps1"), &inputs);
        assert_eq!(command.program(), pwsh.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn unix_scripts_start_directly() {
        let inputs = LocateInputs::default();
        let command = wrap_executable(PathBuf::from("/usr/local/bin/dsh.cmd"), &inputs);
        assert_eq!(command.program(), Path::new("/usr/local/bin/dsh.cmd"));
        assert!(command.prefix_args().is_empty());
    }

    #[test]
    fn display_names_program_and_wrapper_arguments() {
        let command = DshCommand::new("cmd.exe", vec!["/c".to_owned(), "dsh.cmd".to_owned()]);
        assert_eq!(command.display(), "cmd.exe /c dsh.cmd");
        assert_eq!(
            DshCommand::new("/usr/bin/dsh", Vec::new()).display(),
            "/usr/bin/dsh"
        );
    }

    #[test]
    fn parse_version_reads_bare_and_prefixed_values() {
        assert_eq!(parse_version("0.2.1-rc.2").as_deref(), Some("0.2.1-rc.2"));
        assert_eq!(parse_version("  0.2.1\n").as_deref(), Some("0.2.1"));
        assert_eq!(parse_version("dsh 1.4.12").as_deref(), Some("1.4.12"));
        assert_eq!(
            parse_version("0.2.1+build.5").as_deref(),
            Some("0.2.1+build.5")
        );
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn engines_range_covers_minor_bumps_and_prereleases() {
        assert!(version_satisfies_engines("0.2.0"));
        assert!(version_satisfies_engines("0.2.1-rc.2"));
        assert!(version_satisfies_engines("0.10.0"));
        assert!(!version_satisfies_engines("0.1.9"));
        assert!(!version_satisfies_engines("1.0.0"));
        assert!(!version_satisfies_engines("not a version"));
    }

    #[test]
    fn preflight_ready_needs_a_zero_exit() {
        assert_eq!(classify_preflight(0, ""), PreflightOutcome::Ready);
        assert_eq!(
            classify_preflight(0, "warning: noise"),
            PreflightOutcome::Ready
        );
    }

    #[test]
    fn missing_profile_keeps_the_upstream_message_as_hint() {
        let stderr = "profile \"custom\" does not exist; create it with 'dsh plugin --profile custom add <package>'\n";
        let PreflightOutcome::MissingProfile { profile, hint } = classify_preflight(1, stderr)
        else {
            panic!("expected a missing profile");
        };
        assert_eq!(profile, "custom");
        assert!(hint.contains("does not exist"), "hint: {hint}");
        assert!(
            hint.contains("dsh plugin --profile custom add"),
            "hint: {hint}"
        );
    }

    #[test]
    fn other_failures_summarize_stderr() {
        let PreflightOutcome::Failed { summary } = classify_preflight(2, "boom\n  details here \n")
        else {
            panic!("expected a failure");
        };
        assert_eq!(summary, "boom details here");

        let PreflightOutcome::Failed { summary } = classify_preflight(7, "   \n") else {
            panic!("expected a failure");
        };
        assert_eq!(summary, "dsh exited with code 7");
    }
}
