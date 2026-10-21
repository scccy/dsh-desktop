//! Checking the published `dsh` release without installing anything.
//!
//! The shell never upgrades `dsh` itself: `dsh` is installed and upgraded with
//! the user's own package manager (see 项目要求.md). All this module does is turn
//! the output of `npm view <package> version` into "there is a newer release" or
//! "you are current", so both shells say the same thing about the same input.
//!
//! Only the interpretation lives here; actually spawning `npm` is the caller's
//! job, because the two shells already own a process runner with the console
//! hiding and deadline behaviour their platform needs.

use crate::dsh_locate::parse_version;

/// npm package whose published version is compared with the installed one.
pub const PACKAGE: &str = "@deepseek-ai/dsh";

/// Command shown to the user when an upgrade is available.
///
/// The shell only displays it; copying or running it is the user's choice.
pub const UPGRADE_COMMAND: &str = "npm install -g @deepseek-ai/dsh@latest";

/// Meaning of a finished `npm view` probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// Version published on the registry.
    pub latest: String,
    /// Version installed on this machine, when it was known.
    pub installed: Option<String>,
    /// Whether the published version is strictly newer than the installed one.
    /// Unknown or unreadable installed versions are never "newer".
    pub newer: bool,
}

/// Why a probe could not produce a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// `npm view` exited with a non-zero status; carries the status.
    Failed(i32),
    /// `npm view` succeeded but its output held no version.
    NoVersion,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(code) => write!(f, "npm view exited with code {code}"),
            Self::NoVersion => write!(f, "npm view returned no version"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Whether `latest` is a newer release than `installed`.
///
/// Both are version strings; an unreadable pair is never "newer", so a prompt is
/// only shown when the comparison is certain.
pub fn update_is_newer(latest: &str, installed: &str) -> bool {
    let (Some(latest), Some(installed)) = (version_triple(latest), version_triple(installed))
    else {
        return false;
    };
    latest > installed
}

/// Interprets the output of `npm view <package> version`.
///
/// A non-zero exit status fails the probe even when stdout happens to carry
/// text, because npm prints deprecation warnings there.
pub fn interpret(
    stdout: &str,
    exit_code: i32,
    installed: Option<&str>,
) -> Result<UpdateOutcome, UpdateError> {
    if exit_code != 0 {
        return Err(UpdateError::Failed(exit_code));
    }
    let Some(latest) = parse_version(stdout) else {
        return Err(UpdateError::NoVersion);
    };
    let newer = installed.is_some_and(|installed| update_is_newer(&latest, installed));
    Ok(UpdateOutcome {
        latest,
        installed: installed.map(str::to_owned),
        newer,
    })
}

/// Release triple of a version string, ignoring pre-release and build suffixes.
fn version_triple(raw: &str) -> Option<(u64, u64, u64)> {
    let parsed = parse_version(raw)?;
    let core = parsed.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_successful_probe_with_a_newer_release_reports_an_update() {
        let outcome = interpret("0.3.0\n", 0, Some("0.2.1-rc.2")).expect("outcome");
        assert_eq!(outcome.latest, "0.3.0");
        assert_eq!(outcome.installed.as_deref(), Some("0.2.1-rc.2"));
        assert!(outcome.newer);
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        let outcome = interpret("0.2.1", 0, Some("0.2.1")).expect("outcome");
        assert!(!outcome.newer);
    }

    #[test]
    fn an_unknown_installed_version_is_never_an_update() {
        let outcome = interpret("0.3.0", 0, None).expect("outcome");
        assert!(!outcome.newer);
        assert!(outcome.installed.is_none());
    }

    #[test]
    fn a_failing_npm_is_reported_with_its_exit_code() {
        assert_eq!(
            interpret("0.3.0", 1, Some("0.2.1")),
            Err(UpdateError::Failed(1))
        );
    }

    #[test]
    fn output_without_a_version_is_reported() {
        assert_eq!(
            interpret("npm ERR! not found", 0, Some("0.2.1")),
            Err(UpdateError::NoVersion)
        );
    }

    #[test]
    fn the_display_strings_are_usable_in_a_dialog() {
        assert_eq!(
            UpdateError::Failed(2).to_string(),
            "npm view exited with code 2"
        );
        assert!(!UPGRADE_COMMAND.is_empty());
        assert_eq!(PACKAGE, "@deepseek-ai/dsh");
    }
}
