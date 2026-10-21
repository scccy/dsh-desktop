//! Command-line surface of the shell.
//!
//! Flags override the configuration file; every flag that changes the
//! connection is also settable there so that a desktop shortcut without
//! arguments still behaves as the user configured.

use std::fmt;
use std::path::PathBuf;

/// What the process should do after parsing its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// Start or attach, then show the window.
    Run(RunOptions),
    /// Print [`crate::VERSION`] and exit.
    Version,
    /// Print [`HELP`] and exit.
    Help,
}

/// Connection overrides collected from the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Tokenized URL to attach to; the shell then starts no `dsh`.
    pub url: Option<String>,
    /// Port to probe for an already running `dsh web`.
    pub port: Option<u16>,
    /// Explicit `dsh` executable.
    pub dsh_path: Option<PathBuf>,
    /// Profile passed to `dsh --profile`.
    pub profile: Option<String>,
    /// Skip the login-shell environment probe.
    pub no_env_probe: bool,
    /// Forward child output and log at debug level.
    pub verbose: bool,
    /// Forget the remembered window size and position.
    pub reset_window: bool,
}

/// Argument parsing failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// A flag that requires a value did not receive one.
    MissingValue(&'static str),
    /// A flag's value could not be interpreted.
    InvalidValue {
        flag: &'static str,
        value: String,
        expected: &'static str,
    },
    /// An argument is not part of the interface.
    UnknownArgument(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(f, "{flag} requires a value"),
            Self::InvalidValue {
                flag,
                value,
                expected,
            } => {
                write!(f, "{flag} expects {expected}, got {value:?}")
            }
            Self::UnknownArgument(argument) => write!(f, "unknown argument {argument:?}"),
        }
    }
}

impl std::error::Error for CliError {}

/// Usage text printed by `--help` and after a parse error.
pub const HELP: &str = "\
dsh desktop - a lightweight native shell for a locally installed dsh

Usage:
  dsh-desktop [--url <url>] [--port <n>] [--dsh-path <path>] [--profile <name>]
              [--no-env-probe] [--verbose] [--reset-window] [--version] [--help]

Connection:
  --url <url>        attach to a running dsh web using its tokenized URL
  --port <n>         probe http://127.0.0.1:<n> for a running dsh web

dsh:
  --dsh-path <path>  dsh executable to start; default: discover from PATH
  --profile <name>   profile passed to dsh --profile; default: web

Environment:
  --no-env-probe     inherit this process' environment instead of reading the login shell

Other:
  --verbose          forward dsh output and log at debug level
  --reset-window     ignore the remembered window size and position
  --version          print the shell version
  --help             print this help
";

/// Parses arguments, excluding the program name.
pub fn parse<I, S>(args: I) -> Result<CliAction, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut options = RunOptions::default();
    let mut arguments = args.into_iter().map(Into::into);

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--version" | "-V" => return Ok(CliAction::Version),
            "--help" | "-h" => return Ok(CliAction::Help),
            "--no-env-probe" => options.no_env_probe = true,
            "--verbose" => options.verbose = true,
            "--reset-window" => options.reset_window = true,
            "--url" => options.url = Some(value(&mut arguments, "--url")?),
            "--dsh-path" => {
                options.dsh_path = Some(PathBuf::from(value(&mut arguments, "--dsh-path")?))
            }
            "--profile" => options.profile = Some(value(&mut arguments, "--profile")?),
            "--port" => {
                let raw = value(&mut arguments, "--port")?;
                let port = raw.parse::<u16>().map_err(|_| CliError::InvalidValue {
                    flag: "--port",
                    value: raw.clone(),
                    expected: "a port from 0 to 65535",
                })?;
                options.port = Some(port);
            }
            other => return Err(CliError::UnknownArgument(other.to_owned())),
        }
    }

    Ok(CliAction::Run(options))
}

fn value(
    arguments: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, CliError> {
    arguments.next().ok_or(CliError::MissingValue(flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(arguments: &[&str]) -> RunOptions {
        match parse(arguments.iter().copied()).expect("parses") {
            CliAction::Run(options) => options,
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn no_arguments_means_a_default_run() {
        assert_eq!(run(&[]), RunOptions::default());
    }

    #[test]
    fn flags_populate_run_options() {
        let options = run(&[
            "--url",
            "http://127.0.0.1:3080/?token=secret",
            "--port",
            "3080",
            "--dsh-path",
            "/opt/dsh/bin/dsh",
            "--profile",
            "dsh-desktop",
            "--no-env-probe",
            "--verbose",
            "--reset-window",
        ]);
        assert_eq!(
            options.url.as_deref(),
            Some("http://127.0.0.1:3080/?token=secret")
        );
        assert_eq!(options.port, Some(3080));
        assert_eq!(options.dsh_path, Some(PathBuf::from("/opt/dsh/bin/dsh")));
        assert_eq!(options.profile.as_deref(), Some("dsh-desktop"));
        assert!(options.no_env_probe);
        assert!(options.verbose);
        assert!(options.reset_window);
    }

    #[test]
    fn version_and_help_short_circuit_other_flags() {
        assert_eq!(parse(["--verbose", "--version"]), Ok(CliAction::Version));
        assert_eq!(parse(["--help"]), Ok(CliAction::Help));
        assert_eq!(parse(["-h"]), Ok(CliAction::Help));
        assert_eq!(parse(["-V"]), Ok(CliAction::Version));
    }

    #[test]
    fn port_zero_is_accepted_and_garbage_is_rejected() {
        assert_eq!(run(&["--port", "0"]).port, Some(0));
        assert_eq!(
            parse(["--port", "70000"]),
            Err(CliError::InvalidValue {
                flag: "--port",
                value: "70000".to_owned(),
                expected: "a port from 0 to 65535"
            })
        );
        assert_eq!(
            parse(["--port", "abc"]),
            Err(CliError::InvalidValue {
                flag: "--port",
                value: "abc".to_owned(),
                expected: "a port from 0 to 65535"
            })
        );
    }

    #[test]
    fn missing_values_and_unknown_arguments_report_the_flag() {
        assert_eq!(parse(["--url"]), Err(CliError::MissingValue("--url")));
        assert_eq!(
            parse(["--profile"]),
            Err(CliError::MissingValue("--profile"))
        );
        assert_eq!(
            parse(["--nope"]),
            Err(CliError::UnknownArgument("--nope".to_owned()))
        );
        assert!(
            CliError::MissingValue("--url")
                .to_string()
                .contains("--url"),
            "message names the flag"
        );
    }

    #[test]
    fn help_lists_every_supported_flag() {
        for flag in [
            "--url",
            "--port",
            "--dsh-path",
            "--profile",
            "--no-env-probe",
            "--verbose",
            "--reset-window",
            "--version",
            "--help",
        ] {
            assert!(HELP.contains(flag), "help omits {flag}");
        }
    }
}
