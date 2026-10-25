//! Minimum `dsh` version gate for the Desktop Host.
//!
//! The shell ships no runtime, so the Host runs on whatever global `dsh` the
//! machine has. Desktop-profile support is a moving target: `0.2.0-rc.2` starts
//! but skips the marketplace bundle and leaves `webServer.protocol` undefined,
//! while `0.2.1-rc.2` boots the profile this shell was written against. The gate
//! turns that difference into one explicit decision instead of a mystery boot
//! failure:
//!
//! - **below the minimum** → refuse, naming the upgrade command;
//! - **newer than the tested line** → accept with a warning: the launcher's
//!   protocol is additive, so a future `dsh` may well work, but nobody tested it;
//! - **inside the supported line** → accept silently.
//!
//! Comparison follows semver §11: a prerelease sorts *below* the release it
//! leads to (`0.2.1-rc.2 < 0.2.1`), numeric prerelease identifiers compare
//! numerically (`rc.2 < rc.10`), and build metadata is ignored.
//!
//! This module also owns the `dsh --version` probe itself, so the shell can gate
//! the version *before* starting a Host, and the launcher can repeat the same
//! check inside the Host process (see `src-tauri/host/desktop-host.mjs`).

use std::cmp::Ordering;
use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
use crate::launcher::CREATE_NO_WINDOW;
use crate::launcher::{DEFAULT_STOP_GRACE, terminate, waits_within};
use crate::url_policy;

/// Lowest `dsh` version whose desktop profile this shell supports.
///
/// `0.2.0-rc.2` boots, but silently skips the marketplace bundle and reports no
/// `webServer.protocol`; `0.2.1-rc.2` is the first release that matches the
/// launcher's contract, so everything older is refused.
pub const MINIMUM_VERSION: &str = "0.2.1-rc.2";

/// Newest release line the desktop profile was verified against.
///
/// Anything above it is allowed with a warning rather than refused: refusing a
/// newer `dsh` would break the shell on every upstream release.
pub const TESTED_VERSION: &str = "0.2.1";

/// Command shown to the user when the installed `dsh` is too old.
pub const UPGRADE_COMMAND: &str = "npm i -g @deepseek-ai/dsh";

/// One semver prerelease identifier.
///
/// Derived ordering is exactly semver §11.4: numeric identifiers sort below
/// alphanumeric ones and compare numerically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Numeric(u64),
    Alpha(String),
}

/// A parsed semantic version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
    /// Prerelease identifiers; empty for a release version.
    prerelease: Vec<Identifier>,
}

impl SemVer {
    /// Parses `major.minor.patch[-prerelease][+build]`.
    ///
    /// Build metadata is accepted and dropped, as semver §10 requires. A
    /// version embedded in a longer line (`dsh 1.4.12`) is found the same way
    /// `dsh --version` output is: by locating the semver-shaped token.
    pub fn parse(text: &str) -> Option<Self> {
        let token = crate::dsh_locate::parse_version(text)?;
        // Build metadata never participates in comparisons, and it may itself
        // contain a hyphen, so it is stripped before the prerelease is split.
        let without_build = token.split('+').next().unwrap_or(token.as_str());
        let (core, prerelease) = match without_build.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (without_build, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        let prerelease = match prerelease {
            Some(text) => parse_prerelease(text)?,
            None => Vec::new(),
        };
        Some(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    /// The configured minimum version.
    pub fn minimum() -> Self {
        Self::parse(MINIMUM_VERSION).expect("MINIMUM_VERSION is valid semver")
    }

    /// The newest version the desktop profile was verified against.
    pub fn tested() -> Self {
        Self::parse(TESTED_VERSION).expect("TESTED_VERSION is valid semver")
    }

    /// Whether this is a release version (no prerelease identifiers).
    pub fn is_release(&self) -> bool {
        self.prerelease.is_empty()
    }
}

/// Splits `-` separated prerelease text into identifiers.
fn parse_prerelease(text: &str) -> Option<Vec<Identifier>> {
    let mut identifiers = Vec::new();
    for part in text.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return None;
        }
        if part.bytes().all(|byte| byte.is_ascii_digit()) {
            identifiers.push(Identifier::Numeric(part.parse().ok()?));
        } else {
            identifiers.push(Identifier::Alpha(part.to_owned()));
        }
    }
    (!identifiers.is_empty()).then_some(identifiers)
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (self.is_release(), other.is_release()) {
                // A release outranks every prerelease of the same triple.
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_prerelease(&self.prerelease, &other.prerelease),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compares prerelease identifier lists, where a shorter list sorts first when
/// every shared identifier is equal (`rc < rc.1`).
fn compare_prerelease(left: &[Identifier], right: &[Identifier]) -> Ordering {
    for (a, b) in left.iter().zip(right) {
        let ordering = a.cmp(b);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.prerelease.is_empty() {
            return Ok(());
        }
        f.write_str("-")?;
        for (index, identifier) in self.prerelease.iter().enumerate() {
            if index > 0 {
                f.write_str(".")?;
            }
            match identifier {
                Identifier::Numeric(value) => write!(f, "{value}")?,
                Identifier::Alpha(value) => f.write_str(value)?,
            }
        }
        Ok(())
    }
}

/// What the gate decided about one installed `dsh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionGate {
    /// At or above the minimum and within the verified line.
    Supported {
        /// Version found on this machine.
        found: SemVer,
        /// Configured minimum.
        minimum: SemVer,
    },
    /// Above the verified line; accepted with a warning.
    Untested {
        /// Version found on this machine.
        found: SemVer,
        /// Configured minimum.
        minimum: SemVer,
        /// Newest verified version.
        tested: SemVer,
    },
    /// Older than the minimum; the Host must not be started.
    BelowMinimum {
        /// Version found on this machine.
        found: SemVer,
        /// Configured minimum.
        minimum: SemVer,
    },
}

impl VersionGate {
    /// The version the decision was made about.
    pub fn found(&self) -> &SemVer {
        match self {
            Self::Supported { found, .. }
            | Self::Untested { found, .. }
            | Self::BelowMinimum { found, .. } => found,
        }
    }

    /// The configured minimum.
    pub fn minimum(&self) -> &SemVer {
        match self {
            Self::Supported { minimum, .. }
            | Self::Untested { minimum, .. }
            | Self::BelowMinimum { minimum, .. } => minimum,
        }
    }

    /// Whether the Host may be started.
    pub fn is_supported(&self) -> bool {
        !self.is_refusal()
    }

    /// Whether this decision must stop the Host from starting.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::BelowMinimum { .. })
    }

    /// Warning text for the FR-9 log, when the decision is a "continue anyway".
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Untested {
                found,
                minimum,
                tested,
            } => Some(format!(
                "dsh {found} is newer than the tested {tested} (minimum {minimum}); continuing, but the desktop profile may have changed"
            )),
            Self::Supported { .. } | Self::BelowMinimum { .. } => None,
        }
    }

    /// Refusal text for the error page, when the Host must not start.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::BelowMinimum { found, minimum } => Some(format!(
                "dsh {found} is older than the required {minimum}; upgrade with `{UPGRADE_COMMAND}` and try again"
            )),
            Self::Supported { .. } | Self::Untested { .. } => None,
        }
    }
}

/// Decides whether `found` is acceptable.
///
/// Returns `None` when the text carries no parsable version: the caller is then
/// unable to gate at all and should report that instead of guessing.
pub fn check(found: &str) -> Option<VersionGate> {
    SemVer::parse(found).map(gate)
}

/// Applies the policy to an already parsed version.
pub fn gate(found: SemVer) -> VersionGate {
    let minimum = SemVer::minimum();
    let tested = SemVer::tested();
    if found < minimum {
        VersionGate::BelowMinimum { found, minimum }
    } else if found > tested {
        VersionGate::Untested {
            found,
            minimum,
            tested,
        }
    } else {
        VersionGate::Supported { found, minimum }
    }
}

/// One `dsh --version` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionProbe {
    /// Program to run, usually `node`.
    pub program: PathBuf,
    /// Arguments, usually the dsh CLI entry followed by `--version`.
    pub args: Vec<OsString>,
    /// Child environment; empty inherits this process' environment.
    pub env: Vec<(OsString, OsString)>,
    /// Deadline for the whole probe.
    pub timeout: Duration,
}

/// Probe failures. None of these mean "too old"; they mean "could not ask".
#[derive(Debug)]
pub enum ProbeError {
    /// The probe could not be started.
    Spawn {
        /// Program that failed to start.
        program: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The probe did not finish before its deadline.
    Timeout {
        /// How long the shell waited.
        after: Duration,
    },
    /// The probe failed and printed nothing that parses as a version.
    Exited {
        /// Exit code, when the platform reported one.
        code: Option<i32>,
        /// Redacted stderr tail.
        stderr: String,
    },
    /// The probe succeeded but its output carries no version.
    Unparsable {
        /// Redacted output tail.
        output: String,
    },
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(f, "could not run ({}): {source}", program.display())
            }
            Self::Timeout { after } => write!(
                f,
                "`dsh --version` did not finish within {} ms",
                after.as_millis()
            ),
            Self::Exited { code, stderr } => {
                match code {
                    Some(code) => write!(f, "`dsh --version` exited with code {code}")?,
                    None => write!(f, "`dsh --version` was terminated")?,
                }
                if !stderr.trim().is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Unparsable { output } => {
                write!(f, "`dsh --version` printed no version")?;
                if !output.trim().is_empty() {
                    write!(f, ": {output}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ProbeError {}

/// Runs `dsh --version` and parses the version it prints.
///
/// `dsh --version` prints one short line, so its output is read after the
/// process exits instead of through a concurrent reader.
pub fn probe(request: &VersionProbe) -> Result<SemVer, ProbeError> {
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
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

        command.process_group(0);
    }

    let program = request.program.clone();
    let mut child = command
        .spawn()
        .map_err(|source| ProbeError::Spawn { program, source })?;

    if !waits_within(&mut child, request.timeout) {
        terminate(&mut child, DEFAULT_STOP_GRACE);
        return Err(ProbeError::Timeout {
            after: request.timeout,
        });
    }

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = child.wait().ok();
    let code = status.and_then(|status| status.code());

    if let Some(version) = SemVer::parse(&stdout) {
        return Ok(version);
    }
    if code.is_some_and(|code| code != 0) {
        return Err(ProbeError::Exited {
            code,
            stderr: url_policy::redact_text(stderr.trim()),
        });
    }
    Err(ProbeError::Unparsable {
        output: url_policy::redact_text(stdout.trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn version(text: &str) -> SemVer {
        SemVer::parse(text).unwrap_or_else(|| panic!("expected a version in {text:?}"))
    }

    #[test]
    fn prereleases_sort_below_their_release_and_by_identifier() {
        assert!(version("0.2.1-rc.1") < version("0.2.1-rc.2"));
        assert!(version("0.2.1-rc.2") < version("0.2.1"));
        assert!(version("0.2.0") < version("0.2.1-rc.1"));
        assert!(version("0.2.1-rc.2") < version("0.2.1-rc.10"));
        assert_eq!(version("0.2.1-rc.2"), version("0.2.1-rc.2+build.7"));
        assert_eq!(version("0.2.1+build.7"), version("0.2.1"));
    }

    #[test]
    fn prerelease_identifiers_follow_semver_precedence() {
        // The canonical semver §11 chain.
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in chain.windows(2) {
            assert!(
                version(pair[0]) < version(pair[1]),
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn only_three_component_versions_parse() {
        assert!(SemVer::parse("0.2").is_none());
        assert!(SemVer::parse("not a version").is_none());
        assert!(SemVer::parse("").is_none());
        assert!(SemVer::parse("0.2.1-").is_none());
        assert_eq!(
            SemVer::parse("  0.2.1\n").map(|v| v.to_string()),
            Some("0.2.1".to_owned())
        );
        assert_eq!(
            SemVer::parse("dsh 1.4.12").map(|v| v.to_string()),
            Some("1.4.12".to_owned())
        );
    }

    #[test]
    fn the_global_0_2_0_line_is_refused() {
        let gate = check("0.2.0-rc.2").expect("parsable");
        assert!(gate.is_refusal());
        let refusal = gate.refusal().expect("refusal text");
        assert!(refusal.contains("0.2.0-rc.2"), "refusal: {refusal}");
        assert!(refusal.contains(MINIMUM_VERSION), "refusal: {refusal}");
        assert!(refusal.contains(UPGRADE_COMMAND), "refusal: {refusal}");
    }

    #[test]
    fn the_supported_line_starts_at_0_2_1_rc_2() {
        let gate = check("0.2.1-rc.2").expect("parsable");
        assert!(!gate.is_refusal());
        assert_eq!(gate.found().to_string(), "0.2.1-rc.2");
        assert!(gate.warning().is_none());

        let gate = check("0.2.1").expect("parsable");
        assert!(!gate.is_refusal());
        assert!(gate.warning().is_none());

        let gate = check("0.2.1-rc.1").expect("parsable");
        assert!(gate.is_refusal());
    }

    #[test]
    fn versions_past_the_tested_line_are_accepted_with_a_warning() {
        for text in ["0.2.2", "0.3.0", "1.0.0"] {
            let gate = check(text).expect("parsable");
            assert!(gate.is_supported(), "{text} should not be refused");
            let warning = gate.warning().unwrap_or_default();
            assert!(warning.contains(text), "warning: {warning}");
            assert!(warning.contains(TESTED_VERSION), "warning: {warning}");
        }
    }

    #[test]
    fn unparsable_versions_are_not_guessed() {
        assert!(check("").is_none());
        assert!(check("0.2").is_none());
        assert!(check("unknown").is_none());
    }

    #[test]
    fn the_minimum_is_below_the_tested_line() {
        assert!(SemVer::minimum() < SemVer::tested());
    }

    #[cfg(unix)]
    fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write fixture");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_reads_the_version_a_cli_prints() {
        let directory = tempfile::tempdir().expect("temp dir");
        let program = script(directory.path(), "dsh", "printf '0.2.1-rc.2\\n'\n");
        let found = probe(&VersionProbe {
            program,
            args: vec![OsString::from("--version")],
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        })
        .expect("probe");
        assert_eq!(found, SemVer::minimum());
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_probe_is_reported_instead_of_guessed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let program = script(
            directory.path(),
            "dsh",
            "printf 'no dsh here\\n' >&2\nexit 3\n",
        );
        let error = probe(&VersionProbe {
            program,
            args: vec![OsString::from("--version")],
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        })
        .expect_err("must not invent a version");
        let message = error.to_string();
        assert!(message.contains("code 3"), "message: {message}");
        assert!(!message.contains("0.2.1"), "message: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_probe_output_is_unparsable() {
        let directory = tempfile::tempdir().expect("temp dir");
        let program = script(directory.path(), "dsh", "exit 0\n");
        let error = probe(&VersionProbe {
            program,
            args: vec![OsString::from("--version")],
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        })
        .expect_err("no version in output");
        assert!(matches!(error, ProbeError::Unparsable { .. }), "{error:?}");
    }
}
