//! The one place in the shell where operating-system differences live.
//!
//! Everything outside this module calls the functions below and never branches
//! on the target itself. A platform that eventually needs a different native
//! shell only replaces its own file here; `dsh-lite-core` stays reusable as it
//! is, because it never reached into the windowing layer.

use std::path::Path;
use std::process::Command;

use tauri::{AppHandle, RunEvent};
use url::Url;

#[cfg(target_os = "macos")]
#[path = "../menu.rs"]
pub mod menu;

#[cfg(not(target_os = "macos"))]
#[path = "../menubar.rs"]
pub mod menubar;

#[cfg(not(target_os = "macos"))]
#[path = "../tray.rs"]
pub mod tray;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(unix, not(target_os = "macos")))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(all(unix, not(target_os = "macos")))]
use unix as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// Installs the platform's application entry point.
///
/// macOS uses the application menu and the Dock, the other platforms use a
/// menu bar plus a tray icon; every platform exposes the same actions.
///
/// The lifecycle hooks that belong to every platform (panic diagnostics, the
/// `dsh://` handler, a deep link that arrived on the command line) are installed
/// from here so `main.rs` keeps a single call site.
pub fn install_shell_entry(app: &AppHandle) -> tauri::Result<()> {
    crate::lifecycle::install(app);
    imp::install_shell_entry(app)
}

/// Enables the menu entries that only make sense once `dsh web` is showing.
pub fn set_connection_ready(app: &AppHandle, ready: bool) {
    imp::set_connection_ready(app, ready);
}

/// Handles the run-loop events this platform adds to the portable ones.
pub fn handle_run_event(app: &AppHandle, event: &RunEvent) {
    match event {
        // The official `before-quit` hook: a quit that did not come from the
        // shell's own surfaces (Dock menu, `osascript`, session end) still has to
        // pass the confirmation, and a confirmed one must not ask twice.
        RunEvent::ExitRequested { api, .. } => {
            if crate::lifecycle::exit_approved() {
                crate::lifecycle::shutdown(app);
            } else {
                api.prevent_exit();
                crate::lifecycle::quit(app);
            }
        }
        // Every platform stops a sponsored child and remembers the window.
        RunEvent::Exit => crate::lifecycle::shutdown(app),
        other => imp::handle_other_event(app, other),
    }
}

/// Origin of the pages shipped with the shell.
pub fn shell_base() -> Url {
    imp::shell_base()
}

/// Opens a URL, a directory or a file with the user's own handler.
pub fn open_with_system(target: &str) -> std::io::Result<()> {
    imp::open_with_system(target)
}

/// Keeps a helper process from flashing a console window.
pub fn hide_console(command: &mut Command) {
    imp::hide_console(command);
}

/// Command line that starts a tool installed as a script shim.
///
/// Windows installs `npm` as `npm.cmd`, which `CreateProcess` cannot start
/// directly, so the shim runs through `cmd.exe /c`; every other platform starts
/// the name as it is.
pub fn shell_program(name: &str) -> (String, Vec<String>) {
    imp::shell_program(name)
}

/// Applies the platform's window chrome and returns a one-line report.
///
/// macOS draws the sidebar material and moves the traffic lights; every other
/// platform draws its own header webview and has nothing to do, so the call is a
/// no-op there. The result is a `String` rather than `()` on purpose: the
/// platform layer is a leaf and does not reach into the shell's logger, so the
/// caller decides how to record what was applied.
///
/// Idempotent — safe to call again after a resize or a full-screen round trip.
pub fn apply_window_chrome(window: &tauri::Window) -> Result<String, String> {
    imp::apply_window_chrome(window)
}

/// Re-applies only the scroll behaviour, for calls made after the child webviews
/// exist (page load, resize).
///
/// The window chrome is applied while the webviews are still being inserted, so
/// the first pass can miss them; this is the idempotent second chance.
pub fn tame_scrolling(window: &tauri::Window) {
    imp::tame_scrolling(window)
}

/// Registers (or verifies) the `dsh://` handler for this executable.
///
/// macOS reads the scheme from the bundle's `Info.plist`, so there is nothing to
/// write at run time; Windows writes the per-user `HKCU\Software\Classes\…`
/// keys and Linux drops a `.desktop` file. Registration never fails the shell:
/// the caller logs the outcome and carries on.
pub fn register_deep_link(executable: &Path) -> Registration {
    imp::register_deep_link(executable)
}

/// Result of asking the shell to own the `dsh://` URL scheme.
///
/// One vocabulary for three platforms: `Registered` is produced by the Windows
/// and Linux writers, the two bundle variants only by macOS, so on any single
/// platform one half of the enum is legitimately unconstructed.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    /// The platform now routes `dsh://…` to this executable.
    Registered,
    /// macOS: the bundle declares the scheme; nothing to write at run time.
    DeclaredInBundle,
    /// macOS: the bundle's `Info.plist` does not declare the scheme, so macOS
    /// will never hand the shell a `dsh://` URL.
    NotDeclaredInBundle,
    /// The write failed; the message is safe to log.
    Failed(String),
}

impl Registration {
    /// Short form for the log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::DeclaredInBundle => "declared in the bundle",
            Self::NotDeclaredInBundle => "missing from the bundle Info.plist",
            Self::Failed(_) => "failed",
        }
    }
}

/// Why the scheme could not be registered, for the log.
pub fn registration_failure(result: &Registration) -> Option<&str> {
    match result {
        Registration::Failed(reason) => Some(reason.as_str()),
        _ => None,
    }
}

/// The `dsh://` URL scheme: what the shell opens for, and the per-platform
/// payloads that make the operating system hand it those URLs.
///
/// Deliberately free of Tauri and of the operating system: argument
/// classification, the Windows registry writes, the Linux `.desktop` entry and
/// the `Info.plist` check are plain string work, so `cargo test` covers them on
/// every platform (no display, no registry access).
///
/// Scope is the official one (`apps/desktop/src/main.ts`): a single URL,
/// `dsh://open`, whose only effect is to focus the existing main window. Any
/// other `dsh://` URL is logged and ignored — there is no routing table here.
pub mod deeplink {
    use url::Url;

    /// URL scheme the shell claims, matching `setAsDefaultProtocolClient('dsh')`.
    pub const SCHEME: &str = "dsh";

    /// Host of the one URL the shell acts on.
    pub const OPEN_HOST: &str = "open";

    /// Arguments the shell needs to own the scheme on Windows, ready for `reg.exe`.
    ///
    /// `%1` is expanded by the shell to the URL itself, so the handler receives
    /// the deep link as its first argument.
    #[allow(dead_code)]
    // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
    pub const ARGUMENT_PLACEHOLDER: &str = "%1";

    /// What a deep link asks the shell to do.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DeepLink {
        /// Show, restore and focus the single main window — the official
        /// `focusPrimaryWindow()`. It never navigates anywhere.
        Open,
    }

    /// How one command-line argument or received URL relates to this shell.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Classification {
        /// `dsh://open` or `dsh://open/`: act on it.
        Open,
        /// A `dsh://` URL the shell does not act on. Carries the URL back so the
        /// caller can log it (already a string the user's system produced).
        Unsupported(String),
        /// Not a `dsh://` URL at all: a flag, a path, another scheme.
        NotDeepLink,
    }

    /// Classifies one argument or received URL.
    ///
    /// The official shell compares the whole string against `dsh://open` and
    /// `dsh://open/`; this parses instead, so a trailing slash, a different
    /// letter case or a Windows-quoted argument behaves the same way, and a
    /// query string (`dsh://open?…`) is *not* silently treated as the bare form.
    pub fn classify(raw: &str) -> Classification {
        let raw = raw.trim();
        if raw.is_empty() {
            return Classification::NotDeepLink;
        }
        let Ok(url) = Url::parse(raw) else {
            return Classification::NotDeepLink;
        };
        if !url.scheme().eq_ignore_ascii_case(SCHEME) {
            return Classification::NotDeepLink;
        }
        let open = url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(OPEN_HOST))
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none();
        if open {
            Classification::Open
        } else {
            Classification::Unsupported(raw.to_owned())
        }
    }

    /// First argument that asks for an action, with its position.
    ///
    /// Works on a whole `argv` (program name included): the shell scans it both
    /// for its own launch and for the arguments of a second instance, which is
    /// how Windows and Linux deliver a deep link to an already running shell.
    pub fn first_open<I, S>(arguments: I) -> Option<(usize, DeepLink)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        arguments
            .into_iter()
            .enumerate()
            .find_map(|(index, argument)| match classify(argument.as_ref()) {
                Classification::Open => Some((index, DeepLink::Open)),
                _ => None,
            })
    }

    /// One registry write: a key, an optional value name (`None` = the key's
    /// default value) and the value itself.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[allow(dead_code)]
    // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
    pub struct RegistryValue {
        /// Full key path, for example `HKCU\Software\Classes\dsh`.
        pub key: String,
        /// Value name; `None` writes the key's default value.
        pub name: Option<String>,
        /// Data to write.
        pub value: String,
    }

    impl RegistryValue {
        /// `reg.exe` arguments for this write.
        ///
        /// `/f` overwrites without asking, which makes registration idempotent:
        /// the shell may run it on every packaged launch.
        #[allow(dead_code)]
        // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
        pub fn add_arguments(&self) -> Vec<String> {
            let mut arguments = vec!["add".to_owned(), self.key.clone()];
            if let Some(name) = &self.name {
                arguments.push("/v".to_owned());
                arguments.push(name.clone());
            }
            arguments.push("/d".to_owned());
            arguments.push(self.value.clone());
            arguments.push("/f".to_owned());
            arguments
        }
    }

    /// Registry root of the scheme handler, per user (no elevation needed).
    #[allow(dead_code)]
    // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
    pub fn windows_key() -> String {
        format!("HKCU\\Software\\Classes\\{SCHEME}")
    }

    /// Values that make Windows route `dsh://…` to `executable`.
    ///
    /// Mirrors what Electron's `setAsDefaultProtocolClient` writes: the class
    /// itself, its `URL Protocol` marker, a default icon and the open command.
    #[allow(dead_code)]
    // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
    pub fn windows_registration(executable: &str) -> Vec<RegistryValue> {
        let key = windows_key();
        vec![
            RegistryValue {
                key: key.clone(),
                name: None,
                value: "URL:DSH Desktop protocol".to_owned(),
            },
            RegistryValue {
                key: key.clone(),
                name: Some("URL Protocol".to_owned()),
                value: String::new(),
            },
            RegistryValue {
                key: format!("{key}\\DefaultIcon"),
                name: None,
                value: format!("{executable},0"),
            },
            RegistryValue {
                key: format!("{key}\\shell\\open\\command"),
                name: None,
                value: format!("\"{executable}\" \"{ARGUMENT_PLACEHOLDER}\""),
            },
        ]
    }

    /// `.desktop` entry that registers the scheme with the desktop environment.
    ///
    /// `NoDisplay=true`: the handler is not an application the user starts, just
    /// the target of `x-scheme-handler/dsh`.
    #[allow(dead_code)]
    // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
    pub fn unix_desktop_entry(executable: &str) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=DSH Desktop\n\
             Comment=Open dsh:// links with DSH Desktop\n\
             Exec={} %u\n\
             TryExec={}\n\
             Terminal=false\n\
             NoDisplay=true\n\
             MimeType=x-scheme-handler/{SCHEME};\n",
            quoted_exec(executable),
            quoted_exec(executable),
        )
    }

    /// Quotes one path for the `Exec=` line of a `.desktop` entry.
    ///
    /// The spec allows a double-quoted argument; the characters it reserves must
    /// be backslash-escaped, and a literal `%` becomes `%%`.
    #[allow(dead_code)]
    // 只由对应平台的 impl 构造；本模块的测试在三个平台上都覆盖它。
    fn quoted_exec(executable: &str) -> String {
        let mut quoted = String::with_capacity(executable.len() + 2);
        quoted.push('"');
        for character in executable.chars() {
            match character {
                '"' | '\\' | '`' | '$' => {
                    quoted.push('\\');
                    quoted.push(character);
                }
                '%' => quoted.push_str("%%"),
                other => quoted.push(other),
            }
        }
        quoted.push('"');
        quoted
    }

    /// Whether a bundle `Info.plist` declares `scheme` in `CFBundleURLSchemes`.
    ///
    /// A text scan rather than a plist parse: the only thing needed is "does the
    /// bundle list this scheme", and the shell must not grow a parser for it.
    pub fn plist_declares_scheme(plist: &str, scheme: &str) -> bool {
        let mut rest = plist;
        while let Some(position) = rest.find("CFBundleURLSchemes") {
            rest = &rest[position + "CFBundleURLSchemes".len()..];
            // The schemes live in the `<array>` that follows the key.
            let Some(start) = rest.find('<') else {
                return false;
            };
            rest = &rest[start..];
            let end = rest.find("</array>").unwrap_or(rest.len());
            let block = &rest[..end];
            if plist_strings(block).any(|value| value.eq_ignore_ascii_case(scheme)) {
                return true;
            }
        }
        false
    }

    /// Contents of every `<string>…</string>` in `block`, trimmed.
    fn plist_strings(block: &str) -> impl Iterator<Item = &str> {
        block
            .split("<string>")
            .skip(1)
            .filter_map(|chunk| chunk.split("</string>").next())
            .map(str::trim)
    }
}

#[cfg(test)]
mod tests {
    use super::Registration;
    use super::deeplink::{self, Classification, DeepLink};

    #[test]
    fn the_bare_open_url_is_the_only_handled_deep_link() {
        assert_eq!(deeplink::classify("dsh://open"), Classification::Open);
        assert_eq!(deeplink::classify("dsh://open/"), Classification::Open);
        assert_eq!(
            deeplink::classify("  DSH://OPEN  "),
            Classification::Open,
            "scheme and host are case-insensitive and arguments may be padded"
        );
    }

    #[test]
    fn other_dsh_urls_are_recognised_but_not_acted_on() {
        for raw in [
            "dsh://open?focus=1",
            "dsh://open#section",
            "dsh://",
            "dsh:open",
        ] {
            match deeplink::classify(raw) {
                Classification::Unsupported(url) => assert_eq!(url, raw),
                other => panic!("{raw} classified as {other:?}"),
            }
        }
    }

    #[test]
    fn command_line_noise_is_not_a_deep_link() {
        for raw in [
            "",
            "   ",
            "--url",
            "http://127.0.0.1:3080/",
            "/tmp/dsh",
            "dsh",
        ] {
            assert_eq!(
                deeplink::classify(raw),
                Classification::NotDeepLink,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn the_first_open_argument_wins_and_keeps_its_position() {
        let argv = [
            "/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop",
            "--verbose",
            "dsh://open",
        ];
        assert_eq!(deeplink::first_open(argv), Some((2, DeepLink::Open)));

        let none = ["dsh-desktop", "--url", "http://127.0.0.1:3080/"];
        assert_eq!(deeplink::first_open(none), None);
    }

    #[test]
    fn the_windows_registration_writes_every_value_electron_writes() {
        let values =
            deeplink::windows_registration(r"C:\Program Files\DSH Desktop\dsh-desktop.exe");
        let keys: Vec<String> = values
            .iter()
            .map(|value| match &value.name {
                Some(name) => format!("{}\\{name}", value.key),
                None => value.key.clone(),
            })
            .collect();
        assert_eq!(
            keys,
            [
                "HKCU\\Software\\Classes\\dsh",
                "HKCU\\Software\\Classes\\dsh\\URL Protocol",
                "HKCU\\Software\\Classes\\dsh\\DefaultIcon",
                "HKCU\\Software\\Classes\\dsh\\shell\\open\\command",
            ]
        );
        assert_eq!(
            values[1].value, "",
            "`URL Protocol` must exist and be empty"
        );
        assert_eq!(
            values[3].value, "\"C:\\Program Files\\DSH Desktop\\dsh-desktop.exe\" \"%1\"",
            "quoted so a path with spaces still starts the shell"
        );
        assert_eq!(
            values[3].add_arguments(),
            [
                "add",
                "HKCU\\Software\\Classes\\dsh\\shell\\open\\command",
                "/d",
                "\"C:\\Program Files\\DSH Desktop\\dsh-desktop.exe\" \"%1\"",
                "/f",
            ]
        );
    }

    #[test]
    fn the_desktop_entry_registers_the_scheme_without_polluting_the_menu() {
        let entry = deeplink::unix_desktop_entry("/opt/dsh desktop/dsh-desktop");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("MimeType=x-scheme-handler/dsh;\n"));
        assert!(entry.contains("NoDisplay=true\n"));
        assert!(
            entry.contains("Exec=\"/opt/dsh desktop/dsh-desktop\" %u\n"),
            "a path with spaces is quoted: {entry}"
        );
    }

    #[test]
    fn the_exec_line_escapes_what_the_desktop_spec_reserves() {
        let entry = deeplink::unix_desktop_entry(r#"/opt/a"b$c%d/dsh"#);
        assert!(
            entry.contains(r#"Exec="/opt/a\"b\$c%%d/dsh" %u"#),
            "entry: {entry}"
        );
    }

    #[test]
    fn a_bundle_plist_is_read_for_the_scheme_only() {
        let declared = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>dsh</string>
  <key>CFBundleURLTypes</key>
  <array><dict>
    <key>CFBundleURLName</key><string>dev.dsh.desktop</string>
    <key>CFBundleURLSchemes</key><array><string>dsh</string><string>deepseek</string></array>
  </dict></array>
</dict></plist>"#;
        assert!(deeplink::plist_declares_scheme(declared, "dsh"));
        assert!(
            deeplink::plist_declares_scheme(declared, "DSH"),
            "the comparison is case-insensitive"
        );
        assert!(
            !deeplink::plist_declares_scheme(declared, "other"),
            "an unrelated scheme is not declared"
        );

        let missing = r#"<plist version="1.0"><dict>
  <key>CFBundleURLTypes</key><array><dict>
    <key>CFBundleURLSchemes</key><array><string>other</string></array>
  </dict></array></dict></plist>"#;
        assert!(!deeplink::plist_declares_scheme(missing, "dsh"));

        let bare_name = r#"<plist version="1.0"><dict>
  <key>CFBundleName</key><string>dsh</string>
</dict></plist>"#;
        assert!(
            !deeplink::plist_declares_scheme(bare_name, "dsh"),
            "the product name alone does not register a scheme"
        );
    }

    #[test]
    fn registration_outcomes_are_loggable() {
        assert_eq!(Registration::Registered.as_str(), "registered");
        assert_eq!(
            Registration::NotDeclaredInBundle.as_str(),
            "missing from the bundle Info.plist"
        );
        assert_eq!(
            super::registration_failure(&Registration::Failed("reg.exe".to_owned())),
            Some("reg.exe")
        );
        assert_eq!(super::registration_failure(&Registration::Registered), None);
    }
}
