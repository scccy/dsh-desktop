//! In-window keyboard shortcuts.
//!
//! The shell registers no system-wide hot key and never consumes a key press
//! that belongs to the page: the listener forwards the event and lets it
//! continue, and the decision about what a chord means is taken here in Rust so
//! one table documents every shortcut.
//!
//! The listener is only attached when DevTools are allowed (a debug build or
//! `window.devtools = true`); a release build otherwise injects no script into
//! the `dsh web` page at all.

use tauri::WebviewWindow;

use dsh_lite_core::config::Platform;

/// What a recognised chord asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    /// Open or close the WebView DevTools.
    ToggleDevTools,
    /// Reload the page from the current connection.
    Reload,
    /// Show the connection settings page.
    ConnectionPanel,
    /// Quit the shell.
    Quit,
}

impl ShortcutAction {
    /// Stable name written into the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToggleDevTools => "toggle-devtools",
            Self::Reload => "reload",
            Self::ConnectionPanel => "connection-panel",
            Self::Quit => "quit",
        }
    }
}

/// Modifier keys held during one key press.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// `Ctrl` on every platform.
    pub control: bool,
    /// `⌘` on macOS.
    pub meta: bool,
    /// `Shift`.
    pub shift: bool,
    /// `Alt` / `⌥`.
    pub alt: bool,
}

impl Modifiers {
    /// The platform's command modifier: `⌘` on macOS, `Ctrl` elsewhere.
    fn primary(self, platform: Platform) -> bool {
        match platform {
            Platform::MacOs => self.meta,
            Platform::Windows | Platform::Unix => self.control,
        }
    }
}

/// Maps one key press onto a shell action.
///
/// DevTools are only reachable when `devtools_allowed` is set, which mirrors
/// the build profile and `window.devtools`.
pub fn classify(
    platform: Platform,
    modifiers: Modifiers,
    key: &str,
    devtools_allowed: bool,
) -> Option<ShortcutAction> {
    let key = key.trim().to_ascii_lowercase();
    if key == "f12" {
        return devtools_allowed.then_some(ShortcutAction::ToggleDevTools);
    }
    if !modifiers.primary(platform) {
        return None;
    }
    match key.as_str() {
        // `Ctrl+Shift+I` on Windows and Linux, `⌘⌥I` on macOS: both add one
        // modifier on top of the command key, and neither is a page chord.
        "i" if devtools_allowed && (modifiers.shift || modifiers.alt) => {
            Some(ShortcutAction::ToggleDevTools)
        }
        "r" if !modifiers.shift => Some(ShortcutAction::Reload),
        "l" if modifiers.shift => Some(ShortcutAction::ConnectionPanel),
        "q" if !modifiers.shift => Some(ShortcutAction::Quit),
        _ => None,
    }
}

/// Script that forwards key presses to the shell without consuming them.
///
/// It returns early when the page cannot reach the shell's IPC, so a page that
/// the shell does not own simply keeps its keys.
pub const LISTENER_SCRIPT: &str = r#"(function () {
  var internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== "function") { return; }
  if (window.__dshShellShortcuts) { return; }
  window.__dshShellShortcuts = true;
  window.addEventListener("keydown", function (event) {
    var target = event.target;
    if (target && (target.isContentEditable || target.tagName === "INPUT" || target.tagName === "TEXTAREA")) {
      return;
    }
    internals.invoke("shell_shortcut", {
      key: event.key,
      control: event.ctrlKey,
      meta: event.metaKey,
      shift: event.shiftKey,
      alt: event.altKey
    }).catch(function () {});
  }, true);
})();"#;

/// Attaches the key listener to `window`.
pub fn install(window: &WebviewWindow) -> tauri::Result<()> {
    window.eval(LISTENER_SCRIPT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(control: bool, meta: bool, shift: bool) -> Modifiers {
        Modifiers {
            control,
            meta,
            shift,
            alt: false,
        }
    }

    fn macos_devtools() -> Modifiers {
        Modifiers {
            control: false,
            meta: true,
            shift: false,
            alt: true,
        }
    }

    #[test]
    fn devtools_are_reachable_through_f12_and_the_platform_chord() {
        assert_eq!(
            classify(Platform::Windows, Modifiers::default(), "F12", true),
            Some(ShortcutAction::ToggleDevTools)
        );
        assert_eq!(
            classify(Platform::Windows, modifiers(true, false, true), "I", true),
            Some(ShortcutAction::ToggleDevTools),
            "Ctrl+Shift+I"
        );
        assert_eq!(
            classify(Platform::MacOs, macos_devtools(), "i", true),
            Some(ShortcutAction::ToggleDevTools),
            "⌘⌥I"
        );
        assert_eq!(
            classify(Platform::Windows, Modifiers::default(), "F12", false),
            None,
            "release builds without devtools ignore F12"
        );
        assert_eq!(
            classify(Platform::Windows, modifiers(true, false, true), "i", false),
            None,
            "the chord is inert when devtools are disabled"
        );
    }

    #[test]
    fn reload_and_quit_use_the_platform_command_modifier() {
        assert_eq!(
            classify(Platform::Windows, modifiers(true, false, false), "r", false),
            Some(ShortcutAction::Reload)
        );
        assert_eq!(
            classify(Platform::MacOs, modifiers(false, true, false), "R", false),
            Some(ShortcutAction::Reload)
        );
        assert_eq!(
            classify(Platform::Windows, modifiers(true, false, false), "q", false),
            Some(ShortcutAction::Quit)
        );
        assert_eq!(
            classify(Platform::MacOs, modifiers(false, true, false), "q", false),
            Some(ShortcutAction::Quit)
        );
    }

    #[test]
    fn control_on_macos_and_command_on_windows_are_not_shell_chords() {
        assert_eq!(
            classify(Platform::MacOs, modifiers(true, false, false), "r", true),
            None,
            "⌃R belongs to the page on macOS"
        );
        assert_eq!(
            classify(Platform::Windows, modifiers(false, true, false), "r", true),
            None
        );
    }

    #[test]
    fn the_connection_panel_needs_the_shift_modifier() {
        assert_eq!(
            classify(Platform::Windows, modifiers(true, false, true), "L", false),
            Some(ShortcutAction::ConnectionPanel)
        );
        assert_eq!(
            classify(Platform::Windows, modifiers(true, false, false), "l", false),
            None
        );
    }

    #[test]
    fn unmodified_keys_and_modifier_only_presses_are_ignored() {
        assert_eq!(
            classify(Platform::Windows, Modifiers::default(), "r", true),
            None
        );
        assert_eq!(
            classify(Platform::Windows, Modifiers::default(), "", true),
            None
        );
        assert_eq!(
            classify(
                Platform::Windows,
                modifiers(true, false, false),
                "Tab",
                true
            ),
            None
        );
    }

    #[test]
    fn every_action_has_a_distinct_log_name() {
        let names: Vec<&str> = [
            ShortcutAction::ToggleDevTools,
            ShortcutAction::Reload,
            ShortcutAction::ConnectionPanel,
            ShortcutAction::Quit,
        ]
        .iter()
        .map(|action| action.as_str())
        .collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "names: {names:?}");
    }

    #[test]
    fn the_listener_neither_blocks_nor_prevents_default() {
        assert!(!LISTENER_SCRIPT.contains("preventDefault"));
        assert!(!LISTENER_SCRIPT.contains("stopPropagation"));
        assert!(!LISTENER_SCRIPT.contains("stopImmediatePropagation"));
        assert!(LISTENER_SCRIPT.contains("shell_shortcut"));
    }
}
