//! macOS behaviour: menu bar and Dock, `open` handoff, no console concept.
//!
//! The `dsh://` scheme is declared by the bundle, not by the running process:
//! LaunchServices reads `CFBundleURLTypes` from `Contents/Info.plist` when the
//! app is registered, so the only thing the shell can do at run time is check
//! that the declaration is there and say so in the log. That check is what makes
//! a missing `Info.plist` entry visible in the acceptance walk-through instead of
//! looking like "deep links silently do nothing".

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tauri::{AppHandle, RunEvent, Window};
use url::Url;

use super::Registration;

pub fn install_shell_entry(app: &AppHandle) -> tauri::Result<()> {
    super::menu::install(app)
}

/// The application menu holds every action, so nothing is disabled per state.
pub fn set_connection_ready(app: &AppHandle, ready: bool) {
    let _ = (app, ready);
}

pub fn handle_other_event(app: &AppHandle, event: &RunEvent) {
    match event {
        // A click on the Dock icon re-opens the window instead of quitting. Like
        // the official `activate` handler, only a launch with no visible window
        // needs the shell to act; AppKit already raises a visible one.
        RunEvent::Reopen {
            has_visible_windows,
            ..
        } => {
            if !has_visible_windows {
                crate::lifecycle::show_main_window(app);
            }
        }
        // Electron's `open-url`: the system hands a `dsh://` URL to the running
        // app — a second launch, a link in another application, or the URL that
        // started this very process.
        RunEvent::Opened { urls } => {
            for url in urls {
                crate::lifecycle::handle_deep_link(app, url.as_str());
            }
        }
        _ => {}
    }
}

pub fn shell_base() -> Url {
    Url::parse("tauri://localhost/loading.html").expect("the built-in shell origin is valid")
}

pub fn open_with_system(target: &str) -> io::Result<()> {
    Command::new("open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

pub fn hide_console(_command: &mut Command) {}

pub fn shell_program(name: &str) -> (String, Vec<String>) {
    (name.to_owned(), Vec::new())
}

/// Rectangle of one AppKit standard window button: `(x, y, width, height)`.
///
/// Always **window base coordinates** — origin bottom-left of the window content,
/// y pointing up, which is what `-[NSView convertRect:toView:]` with a `nil` view
/// returns. Never a raw `-[NSView frame]`: the traffic lights live in the title
/// bar's container view, and in a `hidden_title` + transparent-title-bar window
/// that container is **flipped** (y grows downwards), so its frames read as
/// "distance from the top" and mixing the two spaces pushed the buttons out of
/// the window on the first real-device run.
pub type ButtonRect = (f64, f64, f64, f64);

/// How far a button may sit from its target and still count as "already there".
///
/// A flipped↔window conversion can land a fraction of a point off; without a
/// tolerance the "in place" check would never settle and every resize would
/// rewrite the frames.
const IN_PLACE_TOLERANCE: f64 = 0.5;

/// Whether `rect` sits fully inside `bounds` (both in the same coordinate space).
///
/// The last line of defence: a button moved outside its superview is clipped and
/// disappears from the window even though the arithmetic "succeeded". Refusing
/// such a rect turns a silent visual bug into "left alone".
pub fn rect_within(rect: ButtonRect, bounds: ButtonRect) -> bool {
    let (x, y, width, height) = rect;
    let (left, top, bound_width, bound_height) = bounds;
    [x, y, width, height, left, top, bound_width, bound_height]
        .iter()
        .all(|value| value.is_finite())
        && x >= left
        && y >= top
        && x + width <= left + bound_width
        && y + height <= top + bound_height
}

/// What to do with the traffic-light group.
#[derive(Debug, Clone, PartialEq)]
pub enum TrafficLightPlan {
    /// Already at the official position (within [`IN_PLACE_TOLERANCE`]).
    InPlace,
    /// Move every button by `shift`, to the given window-base rectangles.
    Move {
        /// `(dx, dy)` in window base coordinates.
        shift: (f64, f64),
        /// Target rectangle per button, in window base coordinates.
        targets: Vec<ButtonRect>,
    },
}

/// Plans the move that puts the traffic-light group's top-left corner on
/// `target`.
///
/// `target` is measured from the window's **top-left**, y pointing down — how the
/// official shell states it (`trafficLightPosition: { x: 16, y: 18 }`). The
/// arithmetic happens in window base coordinates (y up), so only the vertical
/// part is mirrored, through `window.1`.
///
/// Returns `None` — meaning "do not move anything" — when the group has no
/// buttons, the window has no usable size, or the requested position would put a
/// button **outside the window**. That last check is deliberate: a wrong target is
/// worse than no target, and it is exactly the failure the first implementation
/// shipped (buttons moved by 759 points and disappeared).
///
/// @param current - current rectangles, already converted to window base coordinates.
/// @param target - wanted top-left corner of the group, from the window's top-left.
/// @param window - window size in points, `(width, height)`.
/// @returns a plan, or `None` when the move cannot be made safely.
pub fn traffic_light_targets(
    current: &[ButtonRect],
    target: (f64, f64),
    window: (f64, f64),
) -> Option<TrafficLightPlan> {
    let (window_width, window_height) = window;
    if current.is_empty()
        || !window_width.is_finite()
        || !window_height.is_finite()
        || window_width <= 0.0
        || window_height <= 0.0
        || !target.0.is_finite()
        || !target.1.is_finite()
    {
        return None;
    }

    let left = current
        .iter()
        .map(|(x, ..)| *x)
        .fold(f64::INFINITY, f64::min);
    let top = current
        .iter()
        .map(|(_, y, _, height)| y + height)
        .fold(f64::NEG_INFINITY, f64::max);
    if !left.is_finite() || !top.is_finite() {
        return None;
    }

    // The group's wanted corner in window base coordinates: x as given, y
    // mirrored through the window height because AppKit counts from the bottom.
    let wanted_left = target.0;
    let wanted_top = window_height - target.1;
    let shift = (wanted_left - left, wanted_top - top);

    let mut targets = Vec::with_capacity(current.len());
    for (x, y, width, height) in current {
        let moved = (x + shift.0, y + shift.1, *width, *height);
        // Every button must stay fully inside the window; refuse the whole plan
        // rather than move some of them out of sight.
        if moved.0 < 0.0
            || moved.1 < 0.0
            || moved.0 + moved.2 > window_width
            || moved.1 + moved.3 > window_height
        {
            return None;
        }
        targets.push(moved);
    }

    if shift.0.abs() < IN_PLACE_TOLERANCE && shift.1.abs() < IN_PLACE_TOLERANCE {
        return Some(TrafficLightPlan::InPlace);
    }
    Some(TrafficLightPlan::Move { shift, targets })
}

/// Applies the official macOS window chrome: the sidebar material and the
/// traffic-light position (`apps/desktop/src/main.ts`, `createWindow`).
///
/// Both steps are best effort: the returned `String` is the one-line report the
/// caller logs on success, and an `Err` carries whichever step failed — window
/// appearance must never keep the shell from starting.
///
/// Nothing here borrows the shell state, so this stays a leaf of the platform
/// layer (the caller owns logging).
pub fn apply_window_chrome(window: &Window) -> Result<String, String> {
    // Scrolling is applied later (page load / resize), once the child webviews
    // actually exist; at this point the view tree has none yet.
    // In full screen AppKit owns the title bar and re-lays the buttons out; the
    // caller re-applies this after leaving full screen instead.
    if window.is_fullscreen().unwrap_or(false) {
        return Ok("fullscreen: window chrome is left to AppKit".to_owned());
    }

    let mut report: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    match window_vibrancy::apply_vibrancy(
        window,
        window_vibrancy::NSVisualEffectMaterial::Sidebar,
        Some(window_vibrancy::NSVisualEffectState::Active),
        Some(0.0),
    ) {
        Ok(()) => report.push("vibrancy sidebar (state active)".to_owned()),
        Err(error) => failures.push(format!("vibrancy: {error}")),
    }

    match move_traffic_lights(window) {
        // Report the achieved corner first: a `(6.0, -9.0)` shift means nothing
        // when reading a screenshot, "moved to x16 y18" does.
        Ok(TrafficLightShift::Moved((dx, dy))) => report.push(format!(
            "traffic lights moved to x{:.0} y{:.0} (shift {dx:.1}, {dy:.1})",
            crate::window::MACOS_TRAFFIC_LIGHT_POSITION.0,
            crate::window::MACOS_TRAFFIC_LIGHT_POSITION.1,
        )),
        Ok(TrafficLightShift::InPlace) => report.push("traffic lights already in place".to_owned()),
        Ok(TrafficLightShift::Unavailable(why)) => {
            report.push(format!("traffic lights left alone ({why})"))
        }
        Err(error) => failures.push(format!("traffic lights: {error}")),
    }

    if failures.is_empty() {
        Ok(report.join("; "))
    } else {
        // Keep what did work in the message: a half-applied look is still useful
        // to know when reading the log after a bug report.
        if !report.is_empty() {
            failures.insert(0, report.join("; "));
        }
        Err(failures.join("; "))
    }
}

/// Shifts the close / miniaturize / zoom buttons to the configured position.
///
/// The whole conversion is delegated to AppKit, because the buttons do not live
/// in the coordinate space the maths needs:
///
/// 1. `frame()` is expressed in the button's **superview** — the title bar's
///    container view, which is *flipped* when the window hides its title. Reading
///    it as if it were a bottom-left space is what pushed the buttons off-window
///    on the first real-device run (a `(7, 759)` shift instead of `(7, 8)`).
/// 2. `frame()` is in the parent's space, so `-[parent convertRect:toView:nil]`
///    asks AppKit for the **window base** rectangle — flipped and unflipped title
///    bars both arrive in the same space, and no assumption about the parent is
///    left in the code.
/// 3. The planned target is turned back with `convertRect:fromView:nil` on the
///    button's parent, so writing the origin needs no assumption either.
///
/// Everything is validated before the first button moves: a half-applied group
/// would be worse than an untouched one.
fn move_traffic_lights(window: &Window) -> Result<TrafficLightShift, String> {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSButton, NSWindow, NSWindowButton};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    /// The three buttons of the traffic-light group, in display order.
    const KINDS: [NSWindowButton; 3] = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ];

    let raw = window
        .ns_window()
        .map_err(|error| format!("no native window handle: {error}"))?;
    if raw.is_null() {
        return Ok(TrafficLightShift::Unavailable("the native window is null"));
    }
    // SAFETY: `ns_window` returns the live `NSWindow` AppKit owns for this
    // window; it is only read and its title-bar buttons moved, and every call site
    // runs on the main thread (setup / window callbacks).
    let native: &NSWindow = unsafe { &*raw.cast::<NSWindow>() };

    // Buttons plus their parents, collected first so a failure leaves them alone.
    let mut views: Vec<Retained<NSButton>> = Vec::with_capacity(KINDS.len());
    let mut parents: Vec<Retained<objc2_app_kit::NSView>> = Vec::with_capacity(KINDS.len());
    let mut current: Vec<ButtonRect> = Vec::with_capacity(KINDS.len());
    for kind in KINDS {
        let Some(button) = native.standardWindowButton(kind) else {
            return Ok(TrafficLightShift::Unavailable(
                "the window draws no standard buttons",
            ));
        };
        // SAFETY: AppKit owns the superview for as long as the button is a
        // subview of it, which it is here (the button is still attached).
        let Some(parent) = (unsafe { button.superview() }) else {
            return Ok(TrafficLightShift::Unavailable(
                "a standard button has no superview",
            ));
        };
        // `frame()` is expressed in the **parent's** coordinate system, so the
        // conversion has to be asked of the parent. Asking the button instead
        // (`button.convertRect(frame)`) would reinterpret that rect in the
        // button's own space and offset it by its own position — the same class of
        // wrong-but-"successful" move that put the buttons off-window.
        let in_window = parent.convertRect_toView(button.frame(), None);
        current.push((
            in_window.origin.x,
            in_window.origin.y,
            in_window.size.width,
            in_window.size.height,
        ));
        views.push(button);
        parents.push(parent);
    }

    let size = native.frame().size;
    let plan = match traffic_light_targets(
        &current,
        crate::window::MACOS_TRAFFIC_LIGHT_POSITION,
        (size.width, size.height),
    ) {
        Some(plan) => plan,
        // No plan means "leave it alone": either nothing to do or the position
        // would not fit the window.
        None => {
            return Ok(TrafficLightShift::Unavailable(
                "the requested position does not fit the window",
            ));
        }
    };
    let TrafficLightPlan::Move { shift, targets } = plan else {
        return Ok(TrafficLightShift::InPlace);
    };

    // Convert every target back into its parent's space and validate it **before**
    // touching any of them: a half-moved group, or one pushed out of the title
    // bar (where it would be clipped and vanish), is worse than no move at all.
    let mut origins: Vec<NSPoint> = Vec::with_capacity(targets.len());
    for (parent, (x, y, width, height)) in parents.iter().zip(targets) {
        let wanted = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
        let in_parent = parent.convertRect_fromView(wanted, None);
        let bounds = parent.bounds();
        let parent_bounds = (
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        );
        let moved = (
            in_parent.origin.x,
            in_parent.origin.y,
            in_parent.size.width,
            in_parent.size.height,
        );
        if !rect_within(moved, parent_bounds) {
            return Ok(TrafficLightShift::Unavailable(
                "the moved button would leave the title bar",
            ));
        }
        origins.push(in_parent.origin);
    }

    for (button, origin) in views.iter().zip(origins) {
        button.setFrameOrigin(origin);
    }
    Ok(TrafficLightShift::Moved(shift))
}

/// What [`move_traffic_lights`] did, including the reasons it did nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TrafficLightShift {
    /// Buttons were shifted by this many points (window base coordinates).
    Moved((f64, f64)),
    /// Already where the official shell puts them.
    InPlace,
    /// Standard buttons are not available, or moving them would be unsafe.
    Unavailable(&'static str),
}

/// Applies the scroll behaviour only; used after page load and on resize.
pub fn tame_scrolling(window: &Window) {
    let _ = tame_webview_scrolling(window);
}

/// Turns off the two WebView behaviours that make a native window feel like a
/// browser page.
///
/// Electron's `BrowserWindow` defaults to `scrollBounce: false` on macOS, so the
/// official shell never rubber-bands; WKWebView enables it, and a trackpad drag
/// then moves the whole UI. The back/forward swipe gesture has no place in a
/// single-window shell either. Both live on the `WKWebView`'s scroll view, which
/// Tauri does not expose, so the view tree is walked by class name.
///
/// Best effort: an unexpected hierarchy leaves the page exactly as it was.
fn tame_webview_scrolling(window: &Window) -> String {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(raw) = window.ns_window() else {
        return "scrolling: no native window".to_owned();
    };
    if raw.is_null() {
        return "scrolling: no native window".to_owned();
    }
    // SAFETY: `ns_window` hands back the live NSWindow; this only reads the view
    // tree and writes two scroll-view flags on the main thread.
    let native: &AnyObject = unsafe { &*raw.cast::<AnyObject>() };
    let content: *mut AnyObject = unsafe { msg_send![native, contentView] };
    if content.is_null() {
        return "scrolling: no content view".to_owned();
    }

    let mut webviews = 0_usize;
    // Depth-first over the subview tree; the WKWebView sits a few levels down.
    let mut stack: Vec<*mut AnyObject> = vec![content];
    while let Some(view) = stack.pop() {
        if view.is_null() {
            continue;
        }
        let class: &objc2::runtime::AnyClass = unsafe { msg_send![view, class] };
        let name = class.name().to_string_lossy().into_owned();
        if name.contains("WKWebView") {
            // `scrollView` is a WKWebView property; `bounces` and the two
            // `alwaysBounce*` flags are NSScrollView's.
            let scroll: *mut AnyObject = unsafe { msg_send![view, scrollView] };
            if !scroll.is_null() {
                // `Bool` (not Rust `bool`) because `BOOL` is a signed char on
                // some architectures.
                let no = objc2::runtime::Bool::NO;
                let _: () = unsafe { msg_send![scroll, setBounces: no] };
                let _: () = unsafe { msg_send![scroll, setAlwaysBounceVertical: no] };
                let _: () = unsafe { msg_send![scroll, setAlwaysBounceHorizontal: no] };
            }
            // Two-finger swipe navigation would leave the app for a previous
            // document; the shell has no history to go back to.
            let _: () = unsafe {
                msg_send![view, setAllowsBackForwardNavigationGestures: objc2::runtime::Bool::NO]
            };
            webviews += 1;
        }
        let subviews: *mut AnyObject = unsafe { msg_send![view, subviews] };
        if !subviews.is_null() {
            let count: usize = unsafe { msg_send![subviews, count] };
            for index in 0..count {
                let child: *mut AnyObject = unsafe { msg_send![subviews, objectAtIndex: index] };
                stack.push(child);
            }
        }
    }
    if webviews == 0 {
        "scrolling: no webview found".to_owned()
    } else {
        format!("scrolling: bounce and swipe-back off on {webviews} webview(s)")
    }
}

/// Reports whether the bundle declares the `dsh` URL scheme.
///
/// Nothing is written: on macOS the declaration belongs to the signed bundle, so
/// a shell that could change it at run time would break its own signature.
pub fn register_deep_link(executable: &Path) -> Registration {
    let Some(plist) = info_plist(executable) else {
        return Registration::Failed(
            "could not locate Contents/Info.plist next to the executable".to_owned(),
        );
    };
    let Ok(text) = std::fs::read_to_string(&plist) else {
        return Registration::Failed(format!("could not read {}", plist.display()));
    };
    if super::deeplink::plist_declares_scheme(&text, super::deeplink::SCHEME) {
        Registration::DeclaredInBundle
    } else {
        Registration::NotDeclaredInBundle
    }
}

/// `Contents/Info.plist` for an executable inside `Contents/MacOS/`.
///
/// A development build has no bundle at all, which is reported as a missing
/// declaration rather than as an error: deep links simply are not available
/// outside a packaged app.
fn info_plist(executable: &Path) -> Option<PathBuf> {
    let contents = executable.parent()?.parent()?;
    let plist = contents.join("Info.plist");
    plist.is_file().then_some(plist)
}

#[cfg(test)]
mod tests {
    use super::{TrafficLightPlan, traffic_light_targets};

    /// Window geometry used by the cases below: 800 points tall.
    const WINDOW: (f64, f64) = (1200.0, 800.0);

    /// The official `trafficLightPosition: { x: 16, y: 18 }`.
    const TARGET: (f64, f64) = (16.0, 18.0);

    /// Three 14-point buttons laid out side by side, described in **window base
    /// coordinates** (origin bottom-left, y up) — what `convertRect:toView:nil`
    /// returns. Their top edge sits 9 points below the window top.
    fn buttons_from_top(offset_from_top: f64) -> Vec<(f64, f64, f64, f64)> {
        let top = WINDOW.1 - offset_from_top;
        vec![
            (10.0, top - 14.0, 14.0, 14.0),
            (24.0, top - 14.0, 14.0, 14.0),
            (38.0, top - 14.0, 14.0, 14.0),
        ]
    }

    /// Applies a plan's shift the way `move_traffic_lights` does.
    fn applied(
        buttons: &[(f64, f64, f64, f64)],
        (dx, dy): (f64, f64),
    ) -> Vec<(f64, f64, f64, f64)> {
        buttons
            .iter()
            .map(|(x, y, w, h)| (x + dx, y + dy, *w, *h))
            .collect()
    }

    /// Distance of the group's top edge from the window top, in points.
    fn distance_from_top(buttons: &[(f64, f64, f64, f64)]) -> f64 {
        let top = buttons
            .iter()
            .map(|(_, y, _, h)| y + h)
            .fold(f64::NEG_INFINITY, f64::max);
        WINDOW.1 - top
    }

    #[test]
    fn the_group_lands_on_the_official_position() {
        let current = buttons_from_top(9.0);
        let Some(TrafficLightPlan::Move { shift, targets }) =
            traffic_light_targets(&current, TARGET, WINDOW)
        else {
            panic!("a misplaced group must produce a move");
        };
        // x moves from 10 to 16; the top edge moves from 9 points below the window
        // top to 18, i.e. 9 points *down*, which is -9 in the y-up space. The bug
        // this test guards against reported +759 here.
        assert_eq!(shift, (6.0, -9.0));
        assert_eq!(distance_from_top(&applied(&current, shift)), 18.0);
        assert_eq!(targets[0].0, 16.0, "the close button starts at x 16");
    }

    #[test]
    fn a_second_plan_after_applying_the_first_is_in_place() {
        // The caller re-runs this after every full-screen round trip. If the maths
        // did not converge, the buttons would walk off the window one move at a
        // time — the failure that reached the device.
        let mut current = buttons_from_top(9.0);
        for round in 0..3 {
            let plan = traffic_light_targets(&current, TARGET, WINDOW).expect("a plan");
            match plan {
                TrafficLightPlan::Move { shift, .. } => {
                    assert_eq!(round, 0, "only the first round may move anything");
                    current = applied(&current, shift);
                }
                TrafficLightPlan::InPlace => {
                    assert!(round > 0, "the first round must still move");
                }
            }
        }
        assert_eq!(distance_from_top(&current), 18.0);
    }

    #[test]
    fn a_sub_pixel_offset_counts_as_in_place() {
        // Flipped↔window conversions can be a fraction of a point off; treating
        // that as "move again" would rewrite frames on every resize. The group is
        // already at x 16 here, so only the 0.2-point vertical offset is left.
        let nearly: Vec<_> = buttons_from_top(17.8)
            .into_iter()
            .map(|(_, y, w, h)| (16.0, y, w, h))
            .collect();
        assert_eq!(
            traffic_light_targets(&nearly, TARGET, WINDOW),
            Some(TrafficLightPlan::InPlace)
        );
    }

    #[test]
    fn a_position_that_would_leave_the_window_is_refused() {
        let current = buttons_from_top(9.0);
        // 14-point buttons cannot sit 18 points from the top of a 20-point window:
        // the move would put them below the bottom edge.
        assert!(
            traffic_light_targets(&current, TARGET, (1200.0, 20.0)).is_none(),
            "a button must never be moved out of the window"
        );
        // The group would end past the right edge.
        assert!(traffic_light_targets(&current, (1195.0, 18.0), WINDOW).is_none());
        // Negative targets are refused outright.
        assert!(traffic_light_targets(&current, (16.0, -1.0), WINDOW).is_none());
        assert!(traffic_light_targets(&current, (-1.0, 18.0), WINDOW).is_none());
    }

    #[test]
    fn a_button_pushed_out_of_its_superview_is_refused() {
        use super::rect_within;
        // Inside: the target of a correct conversion (flipped title bar, 18 from
        // the top of a 52-point container).
        assert!(rect_within(
            (16.0, 18.0, 14.0, 14.0),
            (0.0, 0.0, 400.0, 52.0)
        ));
        // The shipped bug, expressed as a rect: the flipped-space value was
        // written into a flipped parent, so the button ended up hundreds of points
        // below the container and was clipped away.
        assert!(!rect_within(
            (10.0, 768.0, 14.0, 14.0),
            (0.0, 0.0, 400.0, 52.0)
        ));
        // Half outside on the right is refused as well.
        assert!(!rect_within(
            (395.0, 18.0, 14.0, 14.0),
            (0.0, 0.0, 400.0, 52.0)
        ));
        // Non-finite values can never be trusted.
        assert!(!rect_within(
            (f64::NAN, 18.0, 14.0, 14.0),
            (0.0, 0.0, 400.0, 52.0)
        ));
    }

    #[test]
    fn nothing_to_measure_produces_no_plan() {
        assert_eq!(traffic_light_targets(&[], TARGET, WINDOW), None);
        assert_eq!(
            traffic_light_targets(&buttons_from_top(9.0), TARGET, (0.0, 800.0)),
            None
        );
        assert_eq!(
            traffic_light_targets(&buttons_from_top(9.0), TARGET, (1200.0, 0.0)),
            None
        );
        assert_eq!(
            traffic_light_targets(&buttons_from_top(9.0), (f64::NAN, 18.0), WINDOW),
            None
        );
    }

    #[test]
    fn a_group_already_on_target_is_in_place() {
        let on_target = buttons_from_top(18.0);
        // x still needs fixing (10 → 16), so this is a horizontal-only move.
        expect_horizontal(&on_target);
    }

    fn expect_horizontal(on_target: &[(f64, f64, f64, f64)]) {
        let Some(TrafficLightPlan::Move { shift, .. }) =
            traffic_light_targets(on_target, TARGET, WINDOW)
        else {
            panic!("the group still needs to move sideways");
        };
        assert_eq!(shift.1, 0.0, "already at the right height");
        let settled = applied(on_target, shift);
        assert_eq!(
            traffic_light_targets(&settled, TARGET, WINDOW),
            Some(TrafficLightPlan::InPlace)
        );
    }

    #[test]
    fn the_shift_is_relative_to_the_leftmost_and_topmost_button() {
        // Some macOS versions lay the zoom button out taller; the group's corner
        // is still defined by the smallest x and the highest top edge.
        let top = WINDOW.1 - 9.0;
        let uneven = vec![
            (10.0, top - 14.0, 14.0, 14.0),
            (24.0, top - 16.0, 14.0, 16.0),
            (38.0, top - 14.0, 14.0, 14.0),
        ];
        let Some(TrafficLightPlan::Move { shift, targets }) =
            traffic_light_targets(&uneven, TARGET, WINDOW)
        else {
            panic!("expected a move");
        };
        assert_eq!(shift.0, 6.0, "the leftmost button defines x");
        assert_eq!(shift.1, -9.0, "the tallest button defines y");
        assert_eq!(targets[1].3, 16.0, "each button keeps its own height");
        assert_eq!(distance_from_top(&applied(&uneven, shift)), 18.0);
    }
}
