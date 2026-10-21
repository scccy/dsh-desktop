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

use dsh_lite_core::url_policy::{is_loopback_http, redact_url, shares_target, token_of};

/// Label of the single main window.
pub const MAIN_WINDOW: &str = "main";

/// Smallest window the shell allows, matching the packaged minimum size.
pub const MIN_WIDTH: u32 = 640;

/// Smallest window height the shell allows.
pub const MIN_HEIGHT: u32 = 480;

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
pub fn is_shell_page(url: &Url) -> bool {
    match url.scheme() {
        "tauri" | "about" | "data" => true,
        "http" | "https" => url
            .host_str()
            .map(|host| host.eq_ignore_ascii_case("tauri.localhost"))
            .unwrap_or(false),
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
pub fn classify_navigation(app_target: Option<&Url>, candidate: &Url) -> Navigation {
    if is_shell_page(candidate) {
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
pub fn classify_page_load(pending: Option<&Url>, loaded: &Url) -> PageOutcome {
    if is_shell_page(loaded) {
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
        assert!(is_shell_page(&url("tauri://localhost/loading.html")));
        assert!(is_shell_page(&url("http://tauri.localhost/error.html")));
        assert!(is_shell_page(&url("about:blank")));
        assert!(!is_shell_page(&url("http://127.0.0.1:3080/")));
        assert!(!is_shell_page(&url("https://example.com/")));
    }

    #[test]
    fn only_the_connected_target_and_shell_pages_stay_in_the_window() {
        let target = url("http://127.0.0.1:3080/");
        assert_eq!(
            classify_navigation(Some(&target), &url("http://127.0.0.1:3080/workspace")),
            Navigation::App
        );
        assert_eq!(
            classify_navigation(Some(&target), &url("http://tauri.localhost/settings.html")),
            Navigation::Shell
        );
        assert_eq!(
            classify_navigation(Some(&target), &url("https://example.com/docs")),
            Navigation::External
        );
        assert_eq!(
            classify_navigation(Some(&target), &url("http://127.0.0.1:9999/")),
            Navigation::External,
            "another local port is another server"
        );
        assert_eq!(
            classify_navigation(None, &url("https://example.com")),
            Navigation::External
        );
    }

    #[test]
    fn a_page_load_with_a_consumed_token_authenticates_the_window() {
        let pending = url("http://127.0.0.1:3080/?token=secret");
        let loaded = url("http://127.0.0.1:3080/");
        let outcome = classify_page_load(Some(&pending), &loaded);
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
        let PageOutcome::AuthFailed { detail } = classify_page_load(Some(&pending), &loaded) else {
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
            classify_page_load(Some(&pending), &loaded),
            PageOutcome::AuthFailed { .. }
        ));
    }

    #[test]
    fn a_shell_page_during_the_handshake_is_not_a_failure() {
        let pending = url("http://127.0.0.1:3080/?token=secret");
        assert_eq!(
            classify_page_load(Some(&pending), &url("tauri://localhost/loading.html")),
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
