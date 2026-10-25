//! Checking the published `dsh` release without installing anything.
//!
//! The shell never upgrades `dsh` itself: `dsh` is installed and upgraded with
//! the user's own package manager (see 项目要求.md). All this module does is turn
//! the output of `npm view <package> dist-tags --json` into "there is a newer
//! release" or "you are current", so both shells say the same thing about the
//! same input.
//!
//! The registry publishes several channels at once (`latest`, `next`, `alpha`),
//! and a user who installed a pre-release is *on* a pre-release channel: asking
//! only for `latest` would tell them "you are current" while `next` already
//! carries a newer build (and vice versa would nag stable users with alphas).
//! The rule here mirrors that: a pre-release install is compared against the
//! newest published version across channels, a stable install only against
//! `latest`.
//!
//! Only the interpretation lives here; actually spawning `npm` is the caller's
//! job, because the two shells already own a process runner with the console
//! hiding and deadline behaviour their platform needs.

use crate::dsh_locate::parse_version;

/// npm package whose published version is compared with the installed one.
pub const PACKAGE: &str = "@deepseek-ai/dsh";

/// Command shown when the newer release is the one `latest` points at.
///
/// The shell only displays it; copying or running it is the user's choice.
pub const UPGRADE_COMMAND: &str = "npm install -g @deepseek-ai/dsh@latest";

/// Command that installs the given dist-tag (`next`, `alpha`, …).
pub fn upgrade_command(channel: &str) -> String {
    format!("npm install -g {PACKAGE}@{channel}")
}

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
    /// dist-tag the published version came from (`latest`, `next`, …).
    pub channel: Option<String>,
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
    use crate::version_gate::SemVer;
    let (Some(latest), Some(installed)) = (SemVer::parse(latest), SemVer::parse(installed)) else {
        return false;
    };
    // Full semver precedence, not the release triple: `0.2.1-rc.3` is newer than
    // `0.2.1-rc.2`, and `0.2.1` is newer than `0.2.1-rc.3`.
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
        channel: None,
    })
}

/// Interprets the output of `npm view <package> dist-tags --json`.
///
/// A stable install is only told about `latest`; a pre-release install is told
/// about the newest version across the published channels, which is the one its
/// own channel would actually deliver.
pub fn interpret_dist_tags(
    stdout: &str,
    exit_code: i32,
    installed: Option<&str>,
) -> Result<UpdateOutcome, UpdateError> {
    use crate::version_gate::SemVer;

    if exit_code != 0 {
        return Err(UpdateError::Failed(exit_code));
    }
    let tags: std::collections::BTreeMap<String, String> =
        serde_json::from_str(stdout).map_err(|_| UpdateError::NoVersion)?;
    if tags.is_empty() {
        return Err(UpdateError::NoVersion);
    }

    let installed_version = installed.and_then(SemVer::parse);
    let installed_is_prerelease = installed_version.as_ref().is_some_and(|v| !v.is_release());

    // Candidate pool: a stable install only follows `latest`; a pre-release
    // install also follows the pre-release channels.
    let candidate = tags
        .iter()
        .filter(|(tag, _)| installed_is_prerelease || *tag == "latest")
        .filter_map(|(tag, version)| SemVer::parse(version).map(|parsed| (tag, version, parsed)))
        .max_by(|left, right| left.2.cmp(&right.2));

    let Some((channel, version, parsed)) = candidate else {
        return Err(UpdateError::NoVersion);
    };
    let newer = installed_version.is_some_and(|installed| parsed > installed);
    Ok(UpdateOutcome {
        latest: version.clone(),
        installed: installed.map(str::to_owned),
        newer,
        channel: Some(channel.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_higher_version_counts_as_an_update() {
        assert!(update_is_newer("0.2.2", "0.2.1-rc.2"));
        assert!(update_is_newer("0.3.0", "0.2.9"));
        assert!(update_is_newer("1.0.0", "0.9.9"));
        assert!(!update_is_newer("0.1.9", "0.2.0"));
        assert!(!update_is_newer("not a version", "0.2.0"));
        assert!(!update_is_newer("0.2.2", "unknown"));
    }

    #[test]
    fn prereleases_are_ordered_by_semver_precedence() {
        // The regression this file shipped with: a triple comparison called
        // both of these "not newer".
        assert!(update_is_newer("0.2.1-rc.3", "0.2.1-rc.2"));
        assert!(update_is_newer("0.2.1", "0.2.1-rc.3"));
        assert!(update_is_newer("0.2.1-rc.1", "0.2.1-alpha.5"));
        assert!(!update_is_newer("0.2.1-rc.2", "0.2.1-rc.2"));
        assert!(!update_is_newer("0.2.1-rc.2", "0.2.1"));
        assert!(!update_is_newer("0.2.1-alpha.5", "0.2.1-rc.1"));
    }

    const TAGS: &str = r#"{"alpha":"0.2.1-alpha.5","latest":"0.2.1-rc.2","next":"0.2.1-rc.3"}"#;

    #[test]
    fn a_prerelease_install_follows_the_prerelease_channel() {
        let outcome = interpret_dist_tags(TAGS, 0, Some("0.2.1-rc.2")).expect("tags");
        assert_eq!(outcome.latest, "0.2.1-rc.3");
        assert_eq!(outcome.channel.as_deref(), Some("next"));
        assert!(outcome.newer, "rc.3 must be offered to an rc.2 install");
        assert_eq!(
            upgrade_command(outcome.channel.as_deref().unwrap_or("latest")),
            "npm install -g @deepseek-ai/dsh@next"
        );
    }

    #[test]
    fn a_stable_install_only_follows_latest() {
        // `next` is newer, but a stable install is not nagged about pre-releases.
        let outcome = interpret_dist_tags(TAGS, 0, Some("0.2.0")).expect("tags");
        assert_eq!(outcome.latest, "0.2.1-rc.2");
        assert_eq!(outcome.channel.as_deref(), Some("latest"));
        assert!(outcome.newer);
        // Already on the latest tag: nothing to offer even though `next` exists.
        let current = interpret_dist_tags(TAGS, 0, Some("0.2.1-rc.2")).expect("tags");
        assert_eq!(current.latest, "0.2.1-rc.3");
    }

    #[test]
    fn a_stable_install_on_latest_is_current() {
        let tags = r#"{"latest":"0.2.1","next":"0.3.0-rc.1"}"#;
        let outcome = interpret_dist_tags(tags, 0, Some("0.2.1")).expect("tags");
        assert_eq!(outcome.latest, "0.2.1");
        assert!(!outcome.newer);
    }

    #[test]
    fn the_real_npm_dist_tags_output_is_understood() {
        // Captured from `npm view @deepseek-ai/dsh dist-tags --json`; the
        // registry pretty-prints with newlines, which the parser must accept.
        let raw = r#"{
  "alpha": "0.2.1-alpha.5",
  "next": "0.2.1-rc.3",
  "latest": "0.2.1-rc.2"
}"#;
        let outcome = interpret_dist_tags(raw, 0, Some("0.2.1-rc.2")).expect("real output");
        assert!(outcome.newer);
        assert_eq!(outcome.latest, "0.2.1-rc.3");
        assert_eq!(outcome.channel.as_deref(), Some("next"));
    }

    #[test]
    fn dist_tag_output_must_be_usable() {
        assert_eq!(
            interpret_dist_tags("", 0, Some("0.2.1-rc.2")),
            Err(UpdateError::NoVersion)
        );
        assert_eq!(
            interpret_dist_tags("not json", 0, Some("0.2.1-rc.2")),
            Err(UpdateError::NoVersion)
        );
        assert_eq!(
            interpret_dist_tags("{}", 0, Some("0.2.1-rc.2")),
            Err(UpdateError::NoVersion)
        );
        assert_eq!(
            interpret_dist_tags(TAGS, 2, Some("0.2.1-rc.2")),
            Err(UpdateError::Failed(2))
        );
        // An unreadable installed version never claims an update.
        let outcome = interpret_dist_tags(TAGS, 0, Some("unknown")).expect("tags");
        assert!(!outcome.newer);
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
