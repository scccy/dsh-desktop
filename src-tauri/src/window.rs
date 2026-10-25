//! Window geometry, navigation policy and the handoff to the system browser.
//!
//! The shell owns exactly one window. Its geometry is remembered in
//! `window.json` and corrected against the connected monitors, so a display
//! that disappeared between two launches cannot leave the window off-screen.
//!
//! Navigation is decided in one place: pages served by the shell itself stay in
//! the window, the `dsh web` target the shell connected to stays in the window,
//! and every other URL goes to the system browser instead of replacing the
//! application UI.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

use dsh_lite_core::config::Platform;
use dsh_lite_core::url_policy::{is_loopback_http, redact_url, shares_target, token_of};

/// Label of the single main window.
pub const MAIN_WINDOW: &str = "main";

/// Label of the child webview that draws the shell's own title bar.
pub const HEADER_WEBVIEW: &str = "header";

/// Label of the child webview that hosts `dsh web` and the shell's pages.
pub const CONTENT_WEBVIEW: &str = "content";

/// Height of the shell's title bar, in logical pixels.
///
/// The WinUI shell used 40 DIP for the same row; keeping the number means a
/// window restored by one shell lines up in the other.
pub const HEADER_HEIGHT: f64 = 40.0;

/// Height of the title bar in physical pixels for a given scale factor.
pub fn header_height(scale: f64) -> f64 {
    (HEADER_HEIGHT * scale).round()
}

/// Whether this platform gets the shell's own title bar instead of the system one.
///
/// Windows and Linux draw the 40 DIP row. macOS keeps the system window: the
/// traffic lights, the rounded corners, the zoom button and
/// double-click-to-zoom are window-manager behaviour that a drawn row cannot
/// reproduce, and the application menu already carries every action the header
/// would offer — drawing one there would put the same entries on screen twice.
pub const fn uses_custom_title_bar() -> bool {
    !cfg!(target_os = "macos")
}

/// Smallest window the shell allows, matching the packaged minimum size.
pub const MIN_WIDTH: u32 = 640;

/// Smallest window height the shell allows.
pub const MIN_HEIGHT: u32 = 480;

/// Where the macOS traffic lights sit, in logical pixels from the window's
/// top-left corner.
///
/// The official shell passes `trafficLightPosition: { x: 16, y: 18 }` to
/// `BrowserWindow` (`apps/desktop/src/main.ts`, macOS branch of `createWindow`).
/// Tauri v2 only exposes the equivalent on `WebviewWindowBuilder`, which a bare
/// window with a child content webview cannot use, so `platform::apply_window_chrome`
/// applies this natively through AppKit (and moves the buttons through
/// `convertRect:toView:` so the flipped title-bar space never leaks into the maths).
pub const MACOS_TRAFFIC_LIGHT_POSITION: (f64, f64) = (16.0, 18.0);

/// macOS sidebar material, the official `vibrancy: 'sidebar'`.
pub const MACOS_VIBRANCY_MATERIAL: &str = "sidebar";

/// Material state, the official `visualEffectState: 'active'`: the material stays
/// put when the window loses focus, while the default washes the sidebar out.
pub const MACOS_VISUAL_EFFECT_STATE: &str = "active";

/// Whether the window background is opaque; the material needs it transparent
/// (official `backgroundColor: '#00000000'`).
pub const MACOS_OPAQUE_BACKGROUND: bool = false;

/// One-line description of the window chrome, written into the startup log.
///
/// The acceptance run compares the real window against these numbers, and a log
/// line is the only place the shell can state what it asked the window manager
/// for — neither Tauri nor AppKit can be queried for "which traffic-light inset
/// did you apply".
pub fn chrome_summary(platform: Platform) -> String {
    match platform {
        Platform::MacOs => format!(
            "hiddenInset, traffic lights at x{} y{}, vibrancy {} (state {}, opaque background {})",
            MACOS_TRAFFIC_LIGHT_POSITION.0,
            MACOS_TRAFFIC_LIGHT_POSITION.1,
            MACOS_VIBRANCY_MATERIAL,
            MACOS_VISUAL_EFFECT_STATE,
            MACOS_OPAQUE_BACKGROUND,
        ),
        _ => format!(
            "custom {} DIP header webview above the content, system window buttons",
            HEADER_HEIGHT
        ),
    }
}

/// Width and height of the window that must remain on a monitor.
const MIN_VISIBLE: u32 = 96;

/// Remembered geometry of the main window, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowLayout {
    /// Left edge in physical pixels.
    pub x: i32,
    /// Top edge in physical pixels.
    pub y: i32,
    /// Outer width in physical pixels.
    pub width: u32,
    /// Outer height in physical pixels.
    pub height: u32,
    /// Whether the window was maximized when it was remembered.
    pub maximized: bool,
}

/// Rectangle of one monitor, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl MonitorRect {
    fn overlap_width(&self, layout: &WindowLayout) -> u32 {
        let left = self.x.max(layout.x);
        let right = (self.x + self.width as i32).min(layout.x + layout.width as i32);
        (right - left).max(0) as u32
    }

    fn overlap_height(&self, layout: &WindowLayout) -> u32 {
        let top = self.y.max(layout.y);
        let bottom = (self.y + self.height as i32).min(layout.y + layout.height as i32);
        (bottom - top).max(0) as u32
    }
}

/// Corrects a remembered geometry so the window stays reachable.
///
/// The size is raised to the minimum and capped to the largest monitor, and a
/// position that no monitor shows is replaced by a centered position on the
/// primary monitor.
pub fn clamp_layout(layout: WindowLayout, monitors: &[MonitorRect]) -> WindowLayout {
    let mut clamped = layout;
    clamped.width = clamped.width.max(MIN_WIDTH);
    clamped.height = clamped.height.max(MIN_HEIGHT);
    let Some(primary) = monitors.first() else {
        return clamped;
    };

    let widest = monitors
        .iter()
        .map(|monitor| monitor.width)
        .max()
        .unwrap_or(clamped.width);
    let tallest = monitors
        .iter()
        .map(|monitor| monitor.height)
        .max()
        .unwrap_or(clamped.height);
    clamped.width = clamped.width.min(widest);
    clamped.height = clamped.height.min(tallest);

    let visible = monitors.iter().any(|monitor| {
        monitor.overlap_width(&clamped) >= MIN_VISIBLE
            && monitor.overlap_height(&clamped) >= MIN_VISIBLE
    });
    if visible {
        return clamped;
    }

    clamped.x = primary.x + (primary.width as i32 - clamped.width as i32) / 2;
    clamped.y = primary.y + (primary.height as i32 - clamped.height as i32) / 2;
    clamped
}

/// Reads the remembered geometry, treating a damaged file as no file.
pub fn load_layout(path: &Path) -> Option<WindowLayout> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persists the geometry; a failure is reported to the caller for the log.
pub fn save_layout(path: &Path, layout: &WindowLayout) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(layout).map_err(io::Error::other)?;
    fs::write(path, format!("{text}\n"))
}

/// Whether `url` belongs to the shell's own pages.
///
/// `shell_origin` is the loopback http origin (`http://127.0.0.1:<port>/`) that
/// serves the shell's pages; a URL on that exact origin is a shell page too.
pub fn is_shell_page(shell_origin: Option<&Url>, url: &Url) -> bool {
    match url.scheme() {
        "tauri" | "about" | "data" => true,
        "http" | "https" => {
            url.host_str()
                .map(|host| host.eq_ignore_ascii_case("tauri.localhost"))
                .unwrap_or(false)
                || shell_origin.is_some_and(|origin| shares_target(origin, url))
        }
        _ => false,
    }
}

/// Where a navigation should happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Navigation {
    /// A page shipped with the shell; keep it in the window.
    Shell,
    /// The `dsh web` instance the shell connected to; keep it in the window.
    App,
    /// Anything else; open it in the system browser.
    External,
}

/// Classifies a navigation request against the current connection target.
pub fn classify_navigation(
    app_target: Option<&Url>,
    shell_origin: Option<&Url>,
    candidate: &Url,
) -> Navigation {
    // The renderer document (`dsh-app://app/`) and the shell's own documents
    // (`dsh-app://shell/…`) belong to this window; nothing else on that scheme
    // exists.
    if candidate.scheme() == "dsh-app" {
        return Navigation::App;
    }
    if is_shell_page(shell_origin, candidate) {
        return Navigation::Shell;
    }
    match app_target {
        Some(target) if is_loopback_http(candidate) && shares_target(target, candidate) => {
            Navigation::App
        }
        _ => Navigation::External,
    }
}

/// Outcome of a finished page load inside the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageOutcome {
    /// The shell's own page is showing.
    Shell,
    /// The token was consumed: the page is authenticated.
    Authenticated {
        /// Final URL, guaranteed token-free and safe to log.
        url: Url,
    },
    /// The token survived the load, so authentication never happened.
    AuthFailed {
        /// Redacted reason, safe for logs and the error page.
        detail: String,
    },
    /// Another page of the connected `dsh web` instance is showing.
    InApp,
    /// A page outside the shell and outside the connection.
    External,
}

/// Decides what a finished page load means for the connection state.
///
/// The token check is the FR-3 closure: the shell only treats the page as
/// authenticated when the loaded URL no longer carries a token parameter. A URL
/// that still does means the server never redirected the first hit, so the
/// session cookie was not seeded.
pub fn classify_page_load(
    pending: Option<&Url>,
    shell_origin: Option<&Url>,
    loaded: &Url,
) -> PageOutcome {
    if is_shell_page(shell_origin, loaded) {
        return PageOutcome::Shell;
    }
    if let Some(pending) = pending {
        if token_of(loaded).is_some() {
            return PageOutcome::AuthFailed {
                detail: format!(
                    "the token was still in the address after loading {}",
                    redact_url(loaded.as_str())
                ),
            };
        }
        if !shares_target(pending, loaded) {
            return PageOutcome::AuthFailed {
                detail: "the page left the dsh web origin before the token was consumed".to_owned(),
            };
        }
        return PageOutcome::Authenticated {
            url: dsh_lite_core::url_policy::clean_url(loaded),
        };
    }
    if is_loopback_http(loaded) {
        return PageOutcome::InApp;
    }
    PageOutcome::External
}

/// Origin of the local shell pages, used until the first page load reports it.
pub fn default_shell_base() -> Url {
    crate::platform::shell_base()
}

/// Base for the shell's pre-reconnect loading page.
///
/// Prefers the loopback http origin of the shell server so the following
/// navigation to `dsh web`'s token URL stays same-site (same host; SameSite
/// ignores the port). Falling back to `default_shell_base()` — the Tauri origin
/// on macOS — would make that navigation cross-site and WebKit would withhold
/// the `SameSite=Strict` auth cookie, landing on the 401 page.
pub fn loading_page_base(shell_origin: Option<&Url>) -> Url {
    shell_origin.cloned().unwrap_or_else(default_shell_base)
}

/// URL of a shell page relative to the origin learned from `base`.
pub fn shell_page_url(base: &Url, page: &str) -> Url {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    let directory = base
        .path()
        .rsplit_once('/')
        .map(|(directory, _)| directory.to_owned())
        .unwrap_or_default();
    let path = if directory.is_empty() {
        format!("/{page}")
    } else {
        format!("{directory}/{page}")
    };
    url.set_path(&path);
    url
}

/// Appends query parameters to a shell page URL.
pub fn shell_page_url_with_query(base: &Url, page: &str, query: &[(&str, &str)]) -> Url {
    let mut url = shell_page_url(base, page);
    if !query.is_empty() {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in query {
            serializer.append_pair(name, value);
        }
        url.set_query(Some(&serializer.finish()));
    }
    url
}

/// Opens `url` with the user's default browser.
pub fn open_external(url: &str) -> io::Result<()> {
    crate::platform::open_with_system(url)
}

/// Opens a directory or file with the platform's file manager.
pub fn open_path(path: &Path) -> io::Result<()> {
    crate::platform::open_with_system(&path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("valid URL")
    }

    #[test]
    fn the_loading_page_prefers_the_loopback_shell_origin() {
        let origin = url("http://127.0.0.1:4321/");
        assert_eq!(loading_page_base(Some(&origin)), origin);
    }

    #[test]
    fn the_macos_chrome_matches_the_official_window_options() {
        assert_eq!(MACOS_TRAFFIC_LIGHT_POSITION, (16.0, 18.0));
        assert_eq!(MACOS_VIBRANCY_MATERIAL, "sidebar");
        assert_eq!(
            MACOS_VISUAL_EFFECT_STATE, "active",
            "the material must not follow the window focus"
        );
        const { assert!(!MACOS_OPAQUE_BACKGROUND) };
        let summary = chrome_summary(Platform::MacOs);
        assert!(summary.contains("x16"), "{summary}");
        assert!(summary.contains("sidebar"), "{summary}");
        assert!(!summary.contains("header"), "{summary}");
        assert!(chrome_summary(Platform::Windows).contains("40 DIP"));
    }

    #[test]
    fn the_loading_page_falls_back_to_the_built_in_shell_origin() {
        assert_eq!(loading_page_base(None), default_shell_base());
    }

    fn layout(x: i32, y: i32, width: u32, height: u32) -> WindowLayout {
        WindowLayout {
            x,
            y,
            width,
            height,
            maximized: false,
        }
    }

    #[test]
    fn a_remembered_position_on_a_connected_monitor_is_kept() {
        let monitors = [MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        let remembered = layout(120, 80, 1200, 800);
        assert_eq!(clamp_layout(remembered, &monitors), remembered);
    }

    #[test]
    fn a_position_on_a_detached_monitor_is_centered_on_the_primary() {
        let monitors = [MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        // The second display was to the right and is gone.
        let remembered = layout(2600, 200, 1200, 800);
        let clamped = clamp_layout(remembered, &monitors);
        assert_eq!(clamped.x, (1920 - 1200) / 2);
        assert_eq!(clamped.y, (1080 - 800) / 2);
        assert_eq!(clamped.width, 1200);
    }

    #[test]
    fn a_window_is_never_smaller_than_the_minimum_size() {
        let monitors = [MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        let clamped = clamp_layout(layout(0, 0, 200, 120), &monitors);
        assert_eq!(clamped.width, MIN_WIDTH);
        assert_eq!(clamped.height, MIN_HEIGHT);
    }

    #[test]
    fn a_window_larger_than_every_monitor_is_shrunk() {
        let monitors = [MonitorRect {
            x: 0,
            y: 0,
            width: 1366,
            height: 768,
        }];
        let clamped = clamp_layout(layout(0, 0, 4000, 3000), &monitors);
        assert_eq!(clamped.width, 1366);
        assert_eq!(clamped.height, 768);
    }

    #[test]
    fn a_partly_visible_window_keeps_its_position() {
        let monitors = [MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        // 200 pixels of the window remain on the right edge of the monitor.
        let remembered = layout(1720, 100, 1200, 800);
        assert_eq!(clamp_layout(remembered, &monitors), remembered);
    }

    #[test]
    fn without_monitors_the_size_rules_still_apply() {
        let clamped = clamp_layout(layout(0, 0, 10, 10), &[]);
        assert_eq!(clamped.width, MIN_WIDTH);
        assert_eq!(clamped.height, MIN_HEIGHT);
    }

    #[test]
    fn shell_pages_are_recognised_on_every_platform() {
        assert!(is_shell_page(None, &url("tauri://localhost/loading.html")));
        assert!(is_shell_page(
            None,
            &url("http://tauri.localhost/error.html")
        ));
        assert!(is_shell_page(None, &url("about:blank")));
        assert!(!is_shell_page(None, &url("http://127.0.0.1:3080/")));
        assert!(!is_shell_page(None, &url("https://example.com/")));

        // The loopback http origin that hosts the shell's pages is a shell origin.
        let shell = url("http://127.0.0.1:41234/");
        assert!(is_shell_page(
            Some(&shell),
            &url("http://127.0.0.1:41234/loading.html")
        ));
        assert!(
            !is_shell_page(Some(&shell), &url("http://127.0.0.1:3080/")),
            "another port is the dsh server, not the shell"
        );
    }

    #[test]
    fn only_the_connected_target_and_shell_pages_stay_in_the_window() {
        let target = url("http://127.0.0.1:3080/");
        let shell = url("http://127.0.0.1:41234/");
        assert_eq!(
            classify_navigation(
                Some(&target),
                Some(&shell),
                &url("http://127.0.0.1:3080/workspace")
            ),
            Navigation::App
        );
        assert_eq!(
            classify_navigation(
                Some(&target),
                Some(&shell),
                &url("http://127.0.0.1:41234/settings.html")
            ),
            Navigation::Shell
        );
        assert_eq!(
            classify_navigation(
                Some(&target),
                None,
                &url("http://tauri.localhost/settings.html")
            ),
            Navigation::Shell
        );
        assert_eq!(
            classify_navigation(
                Some(&target),
                Some(&shell),
                &url("https://example.com/docs")
            ),
            Navigation::External
        );
        assert_eq!(
            classify_navigation(Some(&target), Some(&shell), &url("http://127.0.0.1:9999/")),
            Navigation::External,
            "another local port is another server"
        );
        assert_eq!(
            classify_navigation(None, None, &url("https://example.com")),
            Navigation::External
        );
    }

    #[test]
    fn a_page_load_with_a_consumed_token_authenticates_the_window() {
        let pending = url("http://127.0.0.1:3080/?token=secret");
        let loaded = url("http://127.0.0.1:3080/");
        let outcome = classify_page_load(Some(&pending), None, &loaded);
        assert_eq!(
            outcome,
            PageOutcome::Authenticated {
                url: url("http://127.0.0.1:3080/")
            }
        );
    }

    #[test]
    fn a_page_load_that_keeps_the_token_fails_authentication() {
        let pending = url("http://127.0.0.1:3080/?token=secret");
        let loaded = url("http://127.0.0.1:3080/?token=secret");
        let PageOutcome::AuthFailed { detail } = classify_page_load(Some(&pending), None, &loaded)
        else {
            panic!("expected an authentication failure");
        };
        assert!(
            !detail.contains("secret"),
            "detail leaked the token: {detail}"
        );
        assert!(detail.contains("<redacted>"), "detail: {detail}");
    }

    #[test]
    fn a_page_load_that_leaves_the_target_fails_authentication() {
        let pending = url("http://127.0.0.1:3080/?token=secret");
        let loaded = url("http://127.0.0.1:9000/");
        assert!(matches!(
            classify_page_load(Some(&pending), None, &loaded),
            PageOutcome::AuthFailed { .. }
        ));
    }

    #[test]
    fn a_shell_page_during_the_handshake_is_not_a_failure() {
        let pending = url("http://127.0.0.1:3080/?token=secret");
        assert_eq!(
            classify_page_load(Some(&pending), None, &url("tauri://localhost/loading.html")),
            PageOutcome::Shell
        );
        // The loopback shell origin is a shell page too.
        let shell = url("http://127.0.0.1:41234/");
        assert_eq!(
            classify_page_load(
                Some(&pending),
                Some(&shell),
                &url("http://127.0.0.1:41234/loading.html")
            ),
            PageOutcome::Shell
        );
    }

    #[test]
    fn page_urls_keep_the_origin_learned_from_the_first_load() {
        let base = url("http://tauri.localhost/loading.html");
        assert_eq!(
            shell_page_url(&base, "error.html").as_str(),
            "http://tauri.localhost/error.html"
        );
        assert_eq!(
            shell_page_url_with_query(&base, "error.html", &[("kind", "auth"), ("detail", "a&b")])
                .as_str(),
            "http://tauri.localhost/error.html?kind=auth&detail=a%26b"
        );

        let custom = url("tauri://localhost/loading.html");
        assert_eq!(
            shell_page_url(&custom, "settings.html").as_str(),
            "tauri://localhost/settings.html"
        );
        assert_eq!(
            shell_page_url(&url("tauri://localhost/"), "loading.html").as_str(),
            "tauri://localhost/loading.html"
        );
    }

    #[test]
    fn geometry_survives_a_round_trip_and_a_damaged_file_is_ignored() {
        let directory = std::env::temp_dir().join(format!("dsh-window-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("create directory");
        let path = directory.join("window.json");

        let remembered = layout(10, 20, 1000, 700);
        save_layout(&path, &remembered).expect("saved");
        assert_eq!(load_layout(&path), Some(remembered));

        std::fs::write(&path, "{not json").expect("write");
        assert_eq!(load_layout(&path), None);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
