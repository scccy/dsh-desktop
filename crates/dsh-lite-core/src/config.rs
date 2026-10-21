//! User configuration: defaults, TOML parsing, validation and file location.
//!
//! The configuration file is created on first launch and never rewritten, so a
//! user's edits survive shell upgrades. Every field is validated before the
//! shell starts anything: a misspelled key or an out-of-range timeout fails
//! loudly at startup instead of silently falling back to a default.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directory name created under the platform configuration root.
pub const APP_DIR_NAME: &str = "dsh desktop";

/// How the shell obtains a `dsh web` URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectMode {
    /// The shell starts `dsh web` itself and owns the child process.
    Spawn,
    /// The user supplies a complete tokenized URL; the shell starts nothing.
    Url,
    /// The shell probes a port the user started `dsh web` on.
    Port,
}

/// What closing the main window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloseAction {
    /// Stop the shell, terminating a sponsored `dsh` first.
    Quit,
    /// Keep the application and its `dsh` running in the tray or Dock.
    Hide,
}

/// Log verbosity written to the rolling log files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Failures only.
    Error,
    /// Failures and recoverable problems.
    Warn,
    /// Default: lifecycle events plus child output.
    Info,
    /// Adds environment-probe details and forwarded child output.
    Debug,
}

impl LogLevel {
    /// Single-word form written into log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// `[connect]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ConnectConfig {
    /// Connection mode; `--url` and `--port` on the command line override it.
    pub mode: ConnectMode,
    /// Tokenized URL used when `mode = "url"`.
    pub url: String,
    /// Requested port; `0` lets the operating system pick one.
    pub port: u16,
}

impl Default for ConnectConfig {
    fn default() -> Self {
        Self {
            mode: ConnectMode::Spawn,
            url: String::new(),
            port: 0,
        }
    }
}

/// `[dsh]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct DshConfig {
    /// Explicit `dsh` executable; empty means the discovery order in [`crate::dsh_locate`].
    pub path: String,
    /// Profile passed to `dsh --profile`; the built-in `web` template by default.
    pub profile: String,
    /// Readiness deadline for the `dsh web:` line.
    pub start_timeout_ms: u64,
    /// Extra arguments appended to the `dsh` command line.
    pub extra_args: Vec<String>,
}

impl Default for DshConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            profile: "web".to_owned(),
            start_timeout_ms: 120_000,
            extra_args: Vec::new(),
        }
    }
}

/// `[env]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct EnvConfig {
    /// Whether the child receives the user's login-shell environment.
    pub inherit: bool,
    /// Deadline for the login-shell probe.
    pub timeout_ms: u64,
    /// Shell used for the probe; empty means the account's login shell.
    pub shell: String,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            inherit: true,
            timeout_ms: 10_000,
            shell: String::new(),
        }
    }
}

/// `[window]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct WindowConfig {
    /// What closing the window does.
    pub close_action: CloseAction,
    /// Restore the previous size and position.
    pub remember_bounds: bool,
    /// Enable the DevTools shortcut in release builds.
    pub devtools: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            close_action: CloseAction::Quit,
            remember_bounds: true,
            devtools: false,
        }
    }
}

/// `[log]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct LogConfig {
    /// Verbosity of the shell's own records.
    pub level: LogLevel,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
        }
    }
}

/// Complete shell configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    /// `[connect]`.
    pub connect: ConnectConfig,
    /// `[dsh]`.
    pub dsh: DshConfig,
    /// `[env]`.
    pub env: EnvConfig,
    /// `[window]`.
    pub window: WindowConfig,
    /// `[log]`.
    pub log: LogConfig,
}

/// Configuration file failures.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read { path: PathBuf, source: io::Error },
    /// The file could not be created with its default content.
    Create { path: PathBuf, source: io::Error },
    /// The file is not valid TOML for this schema.
    Parse {
        path: Option<PathBuf>,
        source: Box<toml::de::Error>,
    },
    /// A value parsed but is not usable.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "could not read {}: {source}", path.display()),
            Self::Create { path, source } => {
                write!(f, "could not create {}: {source}", path.display())
            }
            Self::Parse { path, source } => match path {
                Some(path) => write!(f, "could not parse {}: {source}", path.display()),
                None => write!(f, "could not parse configuration: {source}"),
            },
            Self::Invalid(detail) => write!(f, "invalid configuration: {detail}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Create { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(&**source),
            Self::Invalid(_) => None,
        }
    }
}

/// Configuration written on first launch; field order follows the tables.
pub const DEFAULT_CONFIG_TOML: &str = r#"[connect]
mode = "spawn"          # spawn | url | port
url = ""                # mode = "url" 时使用
port = 0                # 0 = 由 OS 分配；>0 时透传 --port

[dsh]
path = ""               # 空 = 按 FR-1 顺序探测
profile = "web"         # 内置模板名，或已存在的自定义 profile
startTimeoutMs = 120000
extraArgs = []          # 追加到 dsh 命令行（高级）

[env]
inherit = true
timeoutMs = 10000
shell = ""              # 空 = 取用户数据库中的登录 shell

[window]
closeAction = "quit"    # quit | hide；macOS 默认 hide
rememberBounds = true
devtools = false

[log]
level = "info"          # error | warn | info | debug
"#;

impl Config {
    /// Parses configuration text; `path` only enriches the error report.
    pub fn parse(text: &str, path: Option<&Path>) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.map(Path::to_path_buf),
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Rejects values that parse but cannot run.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.dsh.profile.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "[dsh] profile must name a profile".to_owned(),
            ));
        }
        if self.dsh.start_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "[dsh] startTimeoutMs must be greater than 0".to_owned(),
            ));
        }
        if self.env.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "[env] timeoutMs must be greater than 0".to_owned(),
            ));
        }
        if self.connect.mode == ConnectMode::Url && self.connect.url.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "[connect] url must be set when mode is \"url\"; pass --url or switch to \"spawn\""
                    .to_owned(),
            ));
        }
        if !self
            .dsh
            .extra_args
            .iter()
            .all(|argument| !argument.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "[dsh] extraArgs must not contain empty entries".to_owned(),
            ));
        }
        Ok(())
    }

    /// Reads the file at `path`, writing [`DEFAULT_CONFIG_TOML`] first when absent.
    ///
    /// The returned flag reports whether this call created the file, so the
    /// shell can log the resolved location once instead of on every start.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool), ConfigError> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| ConfigError::Create {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::write(path, DEFAULT_CONFIG_TOML).map_err(|source| ConfigError::Create {
                path: path.to_path_buf(),
                source,
            })?;
            let config = Self::parse(DEFAULT_CONFIG_TOML, Some(path))?;
            return Ok((config, true));
        }
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Ok((Self::parse(&text, Some(path))?, false))
    }
}

/// Host operating system family for configuration placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows: configuration lives under `%APPDATA%`.
    Windows,
    /// macOS: configuration lives under `~/Library/Application Support`.
    MacOs,
    /// Linux and other Unix systems: `$XDG_CONFIG_HOME` or `~/.config`.
    Unix,
}

impl Platform {
    /// Platform of the running binary.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

/// Reads an environment variable for configuration placement.
pub type EnvLookup<'a> = dyn Fn(&str) -> Option<OsString> + 'a;

/// Resolves the configuration directory for `platform` from `lookup`.
///
/// Returns `None` when the platform's root directory cannot be determined,
/// which leaves the shell without a writable configuration file.
pub fn config_dir(platform: Platform, lookup: &EnvLookup<'_>) -> Option<PathBuf> {
    let base = match platform {
        Platform::Windows => PathBuf::from(lookup("APPDATA")?),
        Platform::MacOs => PathBuf::from(lookup("HOME")?).join("Library/Application Support"),
        Platform::Unix => match lookup("XDG_CONFIG_HOME") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => PathBuf::from(lookup("HOME")?).join(".config"),
        },
    };
    Some(base.join(APP_DIR_NAME))
}

/// Configuration directory of the running process.
pub fn default_config_dir() -> Option<PathBuf> {
    config_dir(Platform::current(), &|name| std::env::var_os(name))
}

/// Configuration file of the running process.
pub fn default_config_path() -> Option<PathBuf> {
    default_config_dir().map(|dir| dir.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| map.get(key).map(OsString::from)
    }

    #[test]
    fn defaults_match_the_documented_schema() {
        let config = Config::parse(DEFAULT_CONFIG_TOML, None).expect("default file parses");
        assert_eq!(config, Config::default());
        assert_eq!(config.connect.mode, ConnectMode::Spawn);
        assert_eq!(config.dsh.profile, "web");
        assert_eq!(config.dsh.start_timeout_ms, 120_000);
        assert_eq!(config.env.timeout_ms, 10_000);
        assert!(config.env.inherit);
        assert_eq!(config.window.close_action, CloseAction::Quit);
        assert!(config.window.remember_bounds);
        assert_eq!(config.log.level, LogLevel::Info);
    }

    #[test]
    fn partial_tables_keep_the_remaining_defaults() {
        let config = Config::parse("[dsh]\nprofile = \"dsh-desktop\"\n", None).expect("parses");
        assert_eq!(config.dsh.profile, "dsh-desktop");
        assert_eq!(config.dsh.start_timeout_ms, 120_000);
        assert_eq!(config.log.level, LogLevel::Info);
    }

    #[test]
    fn unknown_keys_are_rejected_instead_of_ignored() {
        let error = Config::parse("[dsh]\nstartTimoutMs = 5\n", None).expect_err("typo rejected");
        assert!(
            matches!(error, ConfigError::Parse { .. }),
            "unexpected: {error}"
        );
    }

    #[test]
    fn close_action_and_level_use_lowercase_values() {
        let config = Config::parse(
            "[window]\ncloseAction = \"hide\"\n[log]\nlevel = \"debug\"\n",
            None,
        )
        .expect("parses");
        assert_eq!(config.window.close_action, CloseAction::Hide);
        assert_eq!(config.log.level, LogLevel::Debug);
        let error = Config::parse("[window]\ncloseAction = \"minimize\"\n", None)
            .expect_err("rejects values");
        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    #[test]
    fn url_mode_requires_a_url() {
        let error =
            Config::parse("[connect]\nmode = \"url\"\n", None).expect_err("rejects empty url");
        assert!(matches!(error, ConfigError::Invalid(message) if message.contains("url")));
    }

    #[test]
    fn zero_timeouts_and_empty_profile_are_rejected() {
        assert!(matches!(
            Config::parse("[dsh]\nstartTimeoutMs = 0\n", None),
            Err(ConfigError::Invalid(message)) if message.contains("startTimeoutMs")
        ));
        assert!(matches!(
            Config::parse("[env]\ntimeoutMs = 0\n", None),
            Err(ConfigError::Invalid(message)) if message.contains("timeoutMs")
        ));
        assert!(matches!(
            Config::parse("[dsh]\nprofile = \"  \"\n", None),
            Err(ConfigError::Invalid(message)) if message.contains("profile")
        ));
    }

    #[test]
    fn extra_args_must_not_carry_empty_entries() {
        assert!(matches!(
            Config::parse("[dsh]\nextraArgs = [\"--verbose\", \"\"]\n", None),
            Err(ConfigError::Invalid(message)) if message.contains("extraArgs")
        ));
    }

    #[test]
    fn first_load_creates_the_default_file_and_keeps_existing_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");

        let (config, created) = Config::load_or_create(&path).expect("creates");
        assert!(created);
        assert_eq!(config, Config::default());
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            DEFAULT_CONFIG_TOML
        );

        fs::write(&path, "[dsh]\nprofile = \"custom\"\n").expect("rewrite");
        let (config, created) = Config::load_or_create(&path).expect("loads");
        assert!(!created);
        assert_eq!(config.dsh.profile, "custom");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "[dsh]\nprofile = \"custom\"\n"
        );
    }

    #[test]
    fn broken_file_reports_its_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[dsh\nprofile = 1\n").expect("write");
        let error = Config::load_or_create(&path).expect_err("rejects");
        let message = error.to_string();
        assert!(message.contains("config.toml"), "unexpected: {message}");
    }

    #[test]
    fn config_dir_follows_each_platform_convention() {
        let windows = lookup(&[("APPDATA", "C:\\Users\\me\\AppData\\Roaming")]);
        assert_eq!(
            config_dir(Platform::Windows, &windows),
            Some(PathBuf::from("C:\\Users\\me\\AppData\\Roaming").join(APP_DIR_NAME))
        );

        let macos = lookup(&[("HOME", "/Users/me")]);
        assert_eq!(
            config_dir(Platform::MacOs, &macos),
            Some(PathBuf::from("/Users/me/Library/Application Support").join(APP_DIR_NAME))
        );

        let xdg = lookup(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/home/me/.xdg")]);
        assert_eq!(
            config_dir(Platform::Unix, &xdg),
            Some(PathBuf::from("/home/me/.xdg").join(APP_DIR_NAME))
        );

        let fallback = lookup(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "")]);
        assert_eq!(
            config_dir(Platform::Unix, &fallback),
            Some(PathBuf::from("/home/me/.config").join(APP_DIR_NAME))
        );

        let missing = lookup(&[]);
        assert_eq!(config_dir(Platform::Unix, &missing), None);
        assert_eq!(config_dir(Platform::Windows, &missing), None);
    }
}
