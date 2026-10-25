//! Close behaviour, deep links, quit confirmation, fatal recovery and the exit
//! sequence.
//!
//! Every path that ends the process runs through this module, so the invariants
//! live in one place:
//!
//! - closing the window either hides it (`window.closeAction = "hide"`, the
//!   macOS default) or asks before quitting, and a hide on a tray platform shows
//!   the official one-time "still running" confirmation first;
//! - quitting asks the Desktop Host what would be interrupted, exactly like the
//!   official `quit-confirmation.ts`: no Host means nothing to interrupt and no
//!   dialog, an unreadable Host answer means "assume running work" and still ask,
//!   and a repeated request joins the decision instead of stacking dialogs;
//! - a sponsored `dsh` child, the Host and the loopback app server are recycled
//!   on **every** exit path — the tree is bound to this process (Job Object /
//!   process group), so even a hard exit cannot leave an orphan behind;
//! - `dsh://open` is the only deep link the shell understands, and its only
//!   effect is to focus the existing window (official `open-url` handler).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, Window};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

use dsh_lite_core::config::{CloseAction, Platform};
use dsh_lite_core::desktop_host::QUIT_INSPECTION_DEADLINE;
use dsh_lite_core::logging::{self, CrashReport, CrashSource};

use crate::connection;
use crate::platform::deeplink::{self, Classification};
use crate::state::AppState;
use crate::window::{MAIN_WINDOW, WindowLayout};

/// Copy of the official `backgroundNoticeBody`.
#[allow(dead_code)]
// main.rs 接线点：窗口 CloseRequested 的 Hide 分支改用 hide_to_background()
const BACKGROUND_NOTICE: &str = "正在运行的任务不会中断，可在系统托盘中重新打开窗口。\nRunning tasks will continue. You can reopen the window from the system tray.";

/// Button acknowledging the background notice.
#[allow(dead_code)]
const BACKGROUND_CONFIRM: &str = "确认";

/// Title of the quit confirmation (official `quitTitle`).
const QUIT_TITLE: &str = "退出 DSH Desktop？";

/// Explanation shown when running work would be interrupted.
const QUIT_ACTIVE_TASKS: &str = "当前正在运行的任务将会中断。\nRunning tasks will be interrupted.";

/// Explanation shown when only scheduled work is affected.
const QUIT_SCHEDULED_TASKS: &str =
    "应用关闭期间，定时任务不会运行。\nScheduled tasks will not run while the app is closed.";

/// Explanation shown when both kinds of work are affected.
const QUIT_ACTIVE_AND_SCHEDULED_TASKS: &str = "当前正在运行的任务将会中断，且应用关闭期间，定时任务不会运行。\n\
     Running tasks will be interrupted, and scheduled tasks will not run while the app is closed.";

/// Button that accepts the quit.
const QUIT_CONFIRM: &str = "退出";

/// Button that cancels the quit.
const QUIT_CANCEL: &str = "取消";

/// Tail lines of the error kept in the recovery dialog; the report holds the rest.
const RECOVERY_DETAIL_LINES: usize = 8;

/// Character budget of the recovery dialog body.
const RECOVERY_DETAIL_BUDGET: usize = 1200;

/// Whether a quit decision is already open; a second request joins it.
static QUIT_DECISION_OPEN: AtomicBool = AtomicBool::new(false);

/// Whether the user already agreed to exit.
///
/// The run loop asks this before letting an exit through: `app.exit()` must not
/// be turned into a second confirmation, while a quit request that did not come
/// from this module (Dock menu, `osascript`, session end) must be.
static EXIT_APPROVED: AtomicBool = AtomicBool::new(false);

/// Whether the panic hook was installed by this process.
static PANIC_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Close behaviour for `platform` given the configured value.
///
/// macOS keeps the application (and its `dsh`) alive when the window closes, so
/// a first launch on macOS treats the untouched shared default — `quit` — as
/// `hide`. An explicit `quit` in an existing file is always honoured.
///
/// This is a recorded difference from the official shell, which always hides on
/// close; `项目要求.md` FR-5 keeps the value configurable (see
/// `docs/desktop-parity.md` §9).
pub fn effective_close_action(
    platform: Platform,
    configured: CloseAction,
    created_config: bool,
) -> CloseAction {
    match (platform, configured, created_config) {
        (Platform::MacOs, CloseAction::Quit, true) => CloseAction::Hide,
        _ => configured,
    }
}

/// What closing the window should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CloseDecision {
    /// Hide the window and keep the Host, the app server and the tray icon
    /// running. `notice` asks for the official one-time confirmation first.
    Hide {
        /// The "still running in the background" notice has never been shown.
        notice: bool,
    },
    /// Quit the whole application; the caller goes through the quit confirmation.
    Quit,
}

/// Resolves the close policy in one place so call sites only follow the result.
///
/// `notice_shown` is whether the one-time marker file already exists. Only the
/// platforms whose only way back is the tray get the notice: on macOS the Dock
/// icon is the way back and the official shell hides silently, while on Windows
/// and Linux a window that "disappears" behind a tray icon is surprising enough
/// to deserve one explanation — the official `background-notice.ts` behaviour.
#[allow(dead_code)]
pub fn close_decision(
    platform: Platform,
    configured: CloseAction,
    created_config: bool,
    notice_shown: bool,
) -> CloseDecision {
    match effective_close_action(platform, configured, created_config) {
        CloseAction::Hide => CloseDecision::Hide {
            notice: has_tray(platform) && !notice_shown,
        },
        CloseAction::Quit => CloseDecision::Quit,
    }
}

/// Whether this platform reaches a hidden window through a tray icon.
#[allow(dead_code)]
pub const fn has_tray(platform: Platform) -> bool {
    matches!(platform, Platform::Windows | Platform::Unix)
}

/// What the Host says a quit would interrupt, plus the "could not ask" state.
///
/// The three-way split is the official one: a missing Host means nothing can be
/// running, an unreadable answer must be treated as running work, and only a
/// readable answer with no work lets the shell quit silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitInspection {
    /// No Host and no legacy `dsh` child: nothing can be interrupted.
    NothingToInterrupt,
    /// The Host answered.
    Known {
        /// Running work would be interrupted.
        active_tasks: bool,
        /// Armed scheduled reminders would not run while the app is closed.
        scheduled_tasks: bool,
    },
    /// The inspection failed, timed out, or no channel exists: assume work.
    Unknown,
}

/// The sentence the quit confirmation shows, if it has to be shown at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitPrompt {
    /// Nothing to interrupt: quit without asking.
    None,
    /// Running work would be interrupted.
    ActiveTasks,
    /// Scheduled work would not run while the app is closed.
    ScheduledTasks,
    /// Both of the above.
    ActiveAndScheduledTasks,
}

impl QuitPrompt {
    /// Copy shown in the dialog body.
    pub const fn text(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::ActiveTasks => Some(QUIT_ACTIVE_TASKS),
            Self::ScheduledTasks => Some(QUIT_SCHEDULED_TASKS),
            Self::ActiveAndScheduledTasks => Some(QUIT_ACTIVE_AND_SCHEDULED_TASKS),
        }
    }
}

/// Chooses the confirmation copy for one inspection result.
///
/// Direct port of the official `resolveDesktopQuitPrompt`: an unknown state
/// warns about running tasks rather than quitting silently.
pub fn resolve_quit_prompt(inspection: QuitInspection) -> QuitPrompt {
    match inspection {
        QuitInspection::NothingToInterrupt => QuitPrompt::None,
        QuitInspection::Unknown => QuitPrompt::ActiveTasks,
        QuitInspection::Known {
            active_tasks: false,
            scheduled_tasks: false,
        } => QuitPrompt::None,
        QuitInspection::Known {
            active_tasks: true,
            scheduled_tasks: true,
        } => QuitPrompt::ActiveAndScheduledTasks,
        QuitInspection::Known {
            active_tasks: true,
            scheduled_tasks: false,
        } => QuitPrompt::ActiveTasks,
        QuitInspection::Known {
            active_tasks: false,
            scheduled_tasks: true,
        } => QuitPrompt::ScheduledTasks,
    }
}

/// Records the current geometry in memory; it is written when the shell exits.
pub fn remember_layout(app: &AppHandle, window: &Window) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    if let Some(layout) = window_layout(window) {
        state.inner().remember_layout(layout);
    }
}

/// Reads the window geometry straight from the window manager.
fn window_layout(window: &Window) -> Option<WindowLayout> {
    let (Ok(position), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return None;
    };
    Some(WindowLayout {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
        maximized: window.is_maximized().unwrap_or(false),
    })
}

/// Writes the geometry to `window.json` when the configuration asks for it.
///
/// The in-memory value only exists after a move or a resize, so a window that
/// was never touched falls back to its current geometry.
pub fn persist_layout(app: &AppHandle, state: &AppState) {
    if !state.config().window.remember_bounds {
        return;
    }
    let layout = state
        .layout()
        .or_else(|| app.get_window(MAIN_WINDOW).as_ref().and_then(window_layout));
    let Some(layout) = layout else {
        return;
    };
    match crate::window::save_layout(&state.window_state_path(), &layout) {
        Ok(()) => state.logger.debug(&format!(
            "remembered the window geometry {},{} {}x{}",
            layout.x, layout.y, layout.width, layout.height
        )),
        Err(error) => state
            .logger
            .warn(&format!("could not remember the window geometry: {error}")),
    }
}

/// Logs — once — that hiding the window keeps `dsh` running.
pub fn note_background(state: &AppState) {
    let marker = state.background_notice_path();
    if marker.exists() {
        state
            .logger
            .debug("the window was hidden; the background notice was already shown");
        return;
    }
    if let Err(error) = std::fs::write(&marker, "shown\n") {
        state
            .logger
            .warn(&format!("could not record the background notice: {error}"));
    }
    state.logger.info(
        "the window was hidden; the shell and dsh keep running in the background, quit from the tray or the menu",
    );
}

/// Hides the window according to `window.closeAction`.
///
/// The window's close handler calls this instead of hiding directly, so the
/// one-time background notice and the quit confirmation both live on the same
/// path as the caption button and the tray.
#[allow(dead_code)]
pub fn hide_to_background(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        hide_main_window(app);
        return;
    };
    let state = Arc::clone(state.inner());
    let decision = close_decision(
        state.platform(),
        state.config().window.close_action,
        state.created_config,
        state.background_notice_path().exists(),
    );
    match decision {
        CloseDecision::Quit => quit(app),
        CloseDecision::Hide { notice: false } => {
            hide_main_window(app);
            note_background(&state);
        }
        CloseDecision::Hide { notice: true } => {
            // Official `background-notice.ts`: an explicit acknowledgement on the
            // first hide; the marker file means it is never asked twice.
            state
                .logger
                .info("showing the one-time background notice before hiding the window");
            let handle = app.clone();
            let state = Arc::clone(&state);
            app.dialog()
                .message(BACKGROUND_NOTICE)
                .title(dsh_lite_core::PRODUCT_NAME)
                .buttons(MessageDialogButtons::OkCustom(
                    BACKGROUND_CONFIRM.to_owned(),
                ))
                .show(move |_| {
                    hide_main_window(&handle);
                    note_background(&state);
                });
        }
    }
}

/// Stops everything the shell owns without exiting the process.
///
/// The run loop reaches this from the close request and again from the exit
/// events, so only the first call does the work. This is the one place that
/// guarantees the Host and the app server are gone before the process ends.
pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner();
        if !state.begin_shutdown() {
            return;
        }
        // Browser guests are child webviews of the main window: closing the
        // shell must take them with it (official `releaseAll`), or they outlive
        // the document that created them.
        let guests = crate::browser::shutdown(app);
        if guests > 0 {
            state
                .logger
                .debug(&format!("closed {guests} browser guest webviews"));
        }
        connection::stop_child(state);
        crate::app_runtime::stop(state);
        persist_layout(app, state);
        state.logger.info("shell shut down");
    }
}

/// Asks the user before stopping the shell.
///
/// Every quit entry point (window close, tray, application menu, ⌘Q, the
/// settings page) calls this, so the confirmation cannot be bypassed by one
/// surface forgetting it. The decision runs off the UI thread because the native
/// dialog blocks the calling thread.
pub fn quit(app: &AppHandle) {
    if QUIT_DECISION_OPEN.swap(true, Ordering::SeqCst) {
        // Official behaviour for a repeated request: bring the existing decision
        // (and the window behind it) forward instead of stacking dialogs.
        show_main_window(app);
        return;
    }
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("quit-decision".into())
        .spawn(move || {
            let approved = decided_quit(&handle);
            QUIT_DECISION_OPEN.store(false, Ordering::SeqCst);
            if approved {
                exit_now(&handle);
            }
        });
    if spawned.is_err() {
        // Without a worker the confirmation cannot be shown; refuse the quit
        // rather than silently killing running work.
        QUIT_DECISION_OPEN.store(false, Ordering::SeqCst);
        if let Some(state) = app.try_state::<Arc<AppState>>() {
            state
                .logger
                .warn("could not start the quit decision thread; the quit was refused");
        }
    }
}

/// Stops the sponsored processes and ends the process; the caller has decided.
pub fn exit_now(app: &AppHandle) {
    approve_exit();
    shutdown(app);
    app.exit(0);
}

/// Records that the user agreed to exit, so the run loop lets the exit through.
pub fn approve_exit() {
    EXIT_APPROVED.store(true, Ordering::SeqCst);
}

/// Whether this exit was already confirmed.
///
/// Official `before-quit`: every quit path passes the same confirmation, which
/// in Tauri means asking on `RunEvent::ExitRequested` unless this module is the
/// one that decided.
pub fn exit_approved() -> bool {
    EXIT_APPROVED.load(Ordering::SeqCst)
}

/// Clears the approval; only a test needs this.
#[cfg(test)]
fn reset_exit_approval() {
    EXIT_APPROVED.store(false, Ordering::SeqCst);
}

/// Runs the inspection, asks when necessary, and reports whether to quit.
fn decided_quit(app: &AppHandle) -> bool {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return true;
    };
    let state = Arc::clone(state.inner());
    let prompt = resolve_quit_prompt(inspect_quit(&state));
    let Some(text) = prompt.text() else {
        state
            .logger
            .info("quitting without a confirmation: nothing would be interrupted");
        return true;
    };
    state
        .logger
        .info(&format!("quit confirmation shown ({prompt:?})"));
    let approved = app
        .dialog()
        .message(text)
        .title(QUIT_TITLE)
        .buttons(MessageDialogButtons::OkCancelCustom(
            QUIT_CONFIRM.to_owned(),
            QUIT_CANCEL.to_owned(),
        ))
        .blocking_show();
    state.logger.info(if approved {
        "the user approved the quit"
    } else {
        "the user cancelled the quit; the shell keeps running"
    });
    approved
}

/// Asks the Desktop Host what a quit would interrupt.
fn inspect_quit(state: &AppState) -> QuitInspection {
    let answer = {
        let mut runtime = state.runtime();
        runtime
            .host
            .as_mut()
            .map(|host| host.inspect_quit(QUIT_INSPECTION_DEADLINE))
    };
    match answer {
        // No Host at all: nothing of ours can be running in it.
        None => {
            if state.runtime().child.is_some() {
                // The legacy attach path owns a `dsh` child but has no control
                // channel, so its work cannot be inspected: assume work.
                state
                    .logger
                    .debug("the legacy dsh child has no quit inspection channel");
                QuitInspection::Unknown
            } else {
                QuitInspection::NothingToInterrupt
            }
        }
        Some(Ok(answer)) => {
            state.logger.debug(&format!(
                "quit inspection: active tasks {}, scheduled tasks {}",
                answer.active_tasks, answer.scheduled_tasks
            ));
            QuitInspection::Known {
                active_tasks: answer.active_tasks,
                scheduled_tasks: answer.scheduled_tasks,
            }
        }
        Some(Err(error)) => {
            state.logger.warn(&format!(
                "quit inspection unavailable ({error}); assuming running tasks"
            ));
            QuitInspection::Unknown
        }
    }
}

/// Shows, restores and focuses the single main window.
pub fn show_main_window(app: &AppHandle) {
    let visible = main_window_visible(app);
    if let Some(window) = app.get_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state
            .logger
            .debug(&format!("focused the main window (was visible: {visible})"));
    }
}

/// Hides the main window, keeping the process alive.
#[allow(dead_code)]
pub fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_window(MAIN_WINDOW) {
        let _ = window.hide();
    }
}

/// Whether the main window is currently visible.
pub fn main_window_visible(app: &AppHandle) -> bool {
    app.get_window(MAIN_WINDOW)
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

/// Acts on one `dsh://` URL the system handed to the shell.
///
/// The official handler is deliberately this small: `dsh://open` focuses the
/// existing window and nothing else; every other `dsh://` URL is ignored. There
/// is no navigation, no argument parsing and no new window — the workspace lives
/// inside the one window the shell already shows.
pub fn handle_deep_link(app: &AppHandle, raw: &str) {
    match deeplink::classify(raw) {
        Classification::Open => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                state
                    .logger
                    .info("deep link dsh://open: focusing the main window");
            }
            show_main_window(app);
        }
        Classification::Unsupported(url) => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                state.logger.info(&format!(
                    "ignoring the deep link {url}: only dsh://open is supported"
                ));
            }
        }
        Classification::NotDeepLink => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                state
                    .logger
                    .debug(&format!("ignoring {raw}: not a dsh deep link"));
            }
        }
    }
}

/// Handles the arguments of a second launch (or of this one) before anything else.
///
/// Returns `true` when a deep link was consumed, so the caller knows the request
/// was not an ordinary `--url` / `--port` attach.
pub fn handle_startup_arguments(app: &AppHandle, arguments: &[String]) -> bool {
    let Some((index, _)) = deeplink::first_open(arguments) else {
        return false;
    };
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.logger.info(&format!(
            "deep link in argument {index} of the command line"
        ));
    }
    handle_deep_link(app, &arguments[index]);
    true
}

/// Second launch: focus the existing window and consume its deep link.
///
/// Windows and Linux deliver a `dsh://` URL to an already running shell as a
/// command-line argument of the new process, which is why the deep link is read
/// from `arguments` here rather than only from the run loop. Returns `true` when
/// the arguments were a deep link (nothing else to do).
#[allow(dead_code)]
// main.rs 接线点：on_second_instance 改调本函数
pub fn handle_second_instance(app: &AppHandle, arguments: &[String]) -> bool {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.logger.info(&format!(
            "a second instance asked for the window with {} arguments",
            arguments.len()
        ));
    }
    let deep_link = handle_startup_arguments(app, arguments);
    if !deep_link {
        // No deep link: the request is just "show me the window again".
        show_main_window(app);
    }
    deep_link
}

/// Installs the process-wide lifecycle hooks; called once from the setup path.
///
/// Three things belong here and nowhere else: the panic hook (so a crash leaves
/// a report behind even when nobody sees stderr), the `dsh://` handler
/// registration, and a deep link that arrived on this process's own command line.
pub fn install(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = Arc::clone(state.inner());
        install_panic_hook(state.log_dir.clone());
        state.logger.debug(&format!(
            "window chrome: {}",
            crate::window::chrome_summary(state.platform())
        ));
        verify_shell_documents(app, &state);
        install_window_chrome(app);
        register_deep_link(&state);
    }
    let arguments: Vec<String> = std::env::args().collect();
    handle_startup_arguments(app, &arguments);
}

/// Applies the platform window chrome and keeps it applied across full-screen
/// round trips.
///
/// Runs right after the window exists, so `restore_geometry` has already given it
/// its final size (the traffic-light maths depends on the content height). The
/// material and the traffic-light position are the official macOS look (see
/// `window::chrome_summary`); on Windows and Linux the platform call is a no-op
/// because the shell draws its own 40 DIP header row there.
fn install_window_chrome(app: &AppHandle) {
    let Some(window) = app.get_window(MAIN_WINDOW) else {
        return;
    };
    log_window_chrome(app, crate::platform::apply_window_chrome(&window));

    // AppKit re-lays the title bar out on its own when a window enters or leaves
    // full screen. Re-aligning after the transition (and never during it) keeps
    // the two from fighting over the same button frames.
    let handle = app.clone();
    let watched = window.clone();
    // `on_window_event` takes an `Fn`, so the last observed state lives in an
    // atomic rather than in a captured `mut`.
    let fullscreen = Arc::new(AtomicBool::new(window.is_fullscreen().unwrap_or(false)));
    let mirror = Arc::clone(&fullscreen);
    window.on_window_event(move |event| {
        if !matches!(
            event,
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::ScaleFactorChanged { .. }
        ) {
            return;
        }
        let now = watched.is_fullscreen().unwrap_or(false);
        if now == mirror.load(Ordering::Relaxed) {
            return;
        }
        mirror.store(now, Ordering::Relaxed);
        if now {
            return;
        }
        log_window_chrome(&handle, crate::platform::apply_window_chrome(&watched));
    });
}

/// Records one window-chrome attempt; the platform layer has no logger of its own.
fn log_window_chrome(app: &AppHandle, result: Result<String, String>) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    match result {
        // Platforms without chrome report an empty line rather than a fake one.
        Ok(report) if report.is_empty() => {}
        Ok(report) => state
            .logger
            .info(&format!("window chrome applied: {report}")),
        Err(error) => state
            .logger
            .warn(&format!("window chrome could not be applied: {error}")),
    }
}

/// Documents the shell itself navigates to; every one of them must be bundled.
///
/// Checked once, before the window can show anything: a missing document is a
/// damaged installation, and the only two outcomes without this check are a
/// white window or a shell page that never arrives. Both are silent, so the
/// check turns them into a crash report plus the recovery dialog.
const SHELL_DOCUMENTS: [&str; 4] = ["loading.html", "error.html", "settings.html", "index.html"];

fn verify_shell_documents(app: &AppHandle, state: &AppState) {
    let resolver = app.asset_resolver();
    let missing: Vec<&str> = SHELL_DOCUMENTS
        .iter()
        .copied()
        .filter(|name| resolver.get((*name).to_owned()).is_none())
        .collect();
    if missing.is_empty() {
        state
            .logger
            .debug("every bundled shell document is present");
        return;
    }
    let detail = format!(
        "内置文档缺失 / bundled shell documents are missing: {}（安装不完整 / the installation is incomplete）",
        missing.join(", ")
    );
    report_fatal(app, FatalSource::Shell, &detail);
}

/// Registers the `dsh://` handler when this build may own one.
///
/// The official shell registers only from a packaged build (or with
/// `DSH_DESKTOP_DEV_APP=1`); a development binary must not take the scheme away
/// from the installed application.
fn register_deep_link(state: &AppState) {
    if cfg!(debug_assertions) && std::env::var_os("DSH_DESKTOP_DEV_APP").is_none() {
        state
            .logger
            .debug("development build: leaving the dsh:// handler to the installed app");
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        state
            .logger
            .warn("could not resolve the executable path; the dsh:// handler is unchanged");
        return;
    };
    let registration = crate::platform::register_deep_link(&executable);
    match crate::platform::registration_failure(&registration) {
        Some(reason) => state.logger.warn(&format!(
            "dsh:// handler {}: {reason}",
            registration.as_str()
        )),
        None => state
            .logger
            .info(&format!("dsh:// handler {}", registration.as_str())),
    }
    // A missing declaration cannot be fixed from inside the process; saying so
    // once is what turns "deep links do nothing" into a visible cause.
    if registration == crate::platform::Registration::NotDeclaredInBundle {
        state.logger.warn(
            "the bundle does not declare CFBundleURLTypes; macOS will not deliver dsh:// URLs to this app",
        );
    }
}

/// Where a fatal failure surfaced, in the shell's own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalSource {
    /// The three-layer boot never reached a ready Host.
    Startup,
    /// The renderer document could not boot. Wired by the bridge's
    /// `desktop_boot_failed` path; kept here so the vocabulary has one home.
    #[allow(dead_code)]
    Renderer,
    /// The shell itself failed after startup.
    Shell,
}

impl FatalSource {
    /// Maps onto the crash-report source vocabulary.
    pub const fn crash_source(self) -> CrashSource {
        match self {
            Self::Startup => CrashSource::Startup,
            Self::Renderer => CrashSource::Renderer,
            Self::Shell => CrashSource::Shell,
        }
    }
}

/// Splits a rendered failure into the report's error chain.
///
/// The shell's failures are already formatted strings (`"outer\n\ncause"`), so a
/// blank line is the closest thing to the official nested-cause list.
pub fn error_chain(detail: &str) -> Vec<String> {
    let mut chain: Vec<String> = detail
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if chain.is_empty() {
        chain.push("(no detail was reported)".to_owned());
    }
    chain
}

/// Body of the recovery dialog: the error tail, the report path, then advice.
///
/// Official semantics: the dialog stays readable (a bounded tail of the error)
/// and names the file that holds everything else.
pub fn recovery_detail(detail: &str, report: Option<&Path>) -> String {
    let (advice, report_line) = match report {
        Some(path) => (
            "\n\n重新安装 DSH Desktop 后重试；完整诊断已写入下面的报告文件。",
            format!("\n报告文件：{}", path.display()),
        ),
        // No report was written: promising a file would send the user looking
        // for one that does not exist.
        None => ("\n\n重新安装 DSH Desktop 后重试。", String::new()),
    };
    let lines: Vec<&str> = detail
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines
        .get(lines.len().saturating_sub(RECOVERY_DETAIL_LINES)..)
        .unwrap_or(&[])
        .join("\n");
    let shortened = tail.len() != detail.trim().len();
    let budget =
        RECOVERY_DETAIL_BUDGET.saturating_sub(advice.chars().count() + report_line.chars().count());
    let kept: Vec<char> = tail.chars().collect();
    let start = kept.len().saturating_sub(budget);
    let mut body = String::new();
    if shortened {
        body.push_str("…（只显示错误尾部，完整内容见报告文件）\n");
    }
    body.extend(&kept[start..]);
    body.push_str(&report_line);
    body.push_str(advice);
    body
}

/// Writes a crash report, then shows the native recovery dialog.
///
/// The report is written **first** so the dialog can name the file; a failed
/// write still opens the dialog (a diagnostic aid must not block recovery).
pub fn report_fatal(app: &AppHandle, source: FatalSource, detail: &str) {
    let state = app
        .try_state::<Arc<AppState>>()
        .map(|state| Arc::clone(state.inner()));
    let report = CrashReport {
        source: source.crash_source(),
        error_chain: error_chain(detail),
        shell_version: dsh_lite_core::VERSION.to_owned(),
        dsh_version: state
            .as_ref()
            .and_then(|state| state.runtime().dsh.version.clone()),
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    };
    let written = match state.as_ref() {
        Some(state) => match logging::write_crash_report(&state.log_dir, &report) {
            Ok(path) => Some(path),
            Err(error) => {
                state
                    .logger
                    .warn(&format!("could not write the crash report: {error}"));
                None
            }
        },
        None => None,
    };
    if let Some(state) = state.as_ref() {
        state.logger.error(&format!(
            "fatal failure ({}): {detail}",
            source.crash_source().as_str()
        ));
        match written.as_deref() {
            Some(path) => state
                .logger
                .error(&format!("crash report: {}", path.display())),
            None => state.logger.warn("no crash report could be written"),
        }
    }

    let handle = app.clone();
    app.dialog()
        .message(recovery_detail(detail, written.as_deref()))
        .title("DSH Desktop 启动失败 / Startup failed")
        .buttons(MessageDialogButtons::OkCancelCustom(
            QUIT_CONFIRM.to_owned(),
            "重新启动".to_owned(),
        ))
        .show(move |exit| {
            shutdown(&handle);
            if exit {
                // The recovery decision *is* the confirmation; without this the
                // run loop would ask the quit question straight after it.
                approve_exit();
                handle.exit(1);
            } else {
                // A restart ignores `prevent_exit`, so the run loop needs no flag.
                handle.restart();
            }
        });
}

/// Runs the startup work on a worker thread, turning a panic into a recovery
/// dialog instead of a window stuck on the loading page.
///
/// `main.rs` wraps the three-layer boot in this; a panic inside the boot would
/// otherwise be printed to a stderr nobody sees and leave the loading page up
/// forever.
#[allow(dead_code)] // main.rs 接线点：见回报里的接线清单（P4）
pub fn guarded_startup<F>(app: AppHandle, work: F)
where
    F: FnOnce() + Send + 'static,
{
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("desktop-runtime".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            if let Err(payload) = outcome {
                let detail = panic_payload(&*payload);
                report_fatal(&handle, FatalSource::Startup, &detail);
            }
        });
    if spawned.is_err() {
        report_fatal(
            &app,
            FatalSource::Startup,
            "could not start the desktop runtime thread",
        );
    }
}

/// Installs a panic hook that leaves a crash report behind.
///
/// A GUI shell has no visible stderr, so a panic that is not the startup boot's
/// would otherwise vanish. The hook only writes a file: showing UI from inside a
/// panic can deadlock the thread that panicked.
pub fn install_panic_hook(log_dir: PathBuf) {
    if PANIC_HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let detail = panic_payload_message(
            info.payload(),
            info.location()
                .map(|location| format!("{}:{}", location.file(), location.line())),
        );
        let report = CrashReport {
            source: CrashSource::Shell,
            error_chain: error_chain(&format!("panic: {detail}")),
            shell_version: dsh_lite_core::VERSION.to_owned(),
            dsh_version: None,
            platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        };
        match logging::write_crash_report(&log_dir, &report) {
            Ok(path) => eprintln!(
                "{}: panic report at {}",
                dsh_lite_core::PRODUCT_NAME,
                path.display()
            ),
            Err(error) => eprintln!(
                "{}: panic report could not be written: {error}",
                dsh_lite_core::PRODUCT_NAME
            ),
        }
        previous(info);
    }));
}

/// Renders a caught panic payload.
fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    panic_payload_message(payload, None)
}

/// Message of a panic payload, with an optional source location.
fn panic_payload_message(payload: &(dyn std::any::Any + Send), location: Option<String>) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic with a non-string payload".to_owned());
    match location {
        Some(location) => format!("{message} (at {location})"),
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::deeplink::DeepLink;

    #[test]
    fn the_close_action_of_the_file_is_used_once_it_was_written_by_the_user() {
        assert_eq!(
            effective_close_action(Platform::Windows, CloseAction::Quit, false),
            CloseAction::Quit
        );
        assert_eq!(
            effective_close_action(Platform::Unix, CloseAction::Hide, false),
            CloseAction::Hide
        );
        assert_eq!(
            effective_close_action(Platform::MacOs, CloseAction::Quit, false),
            CloseAction::Quit,
            "an explicit quit survives"
        );
    }

    #[test]
    fn a_fresh_macos_configuration_hides_instead_of_quitting() {
        assert_eq!(
            effective_close_action(Platform::MacOs, CloseAction::Quit, true),
            CloseAction::Hide
        );
        assert_eq!(
            effective_close_action(Platform::MacOs, CloseAction::Hide, true),
            CloseAction::Hide
        );
        assert_eq!(
            effective_close_action(Platform::Windows, CloseAction::Quit, true),
            CloseAction::Quit
        );
    }

    #[test]
    fn a_fresh_macos_close_hides_silently() {
        assert_eq!(
            close_decision(Platform::MacOs, CloseAction::Quit, true, false),
            CloseDecision::Hide { notice: false },
            "the Dock icon is the way back, so macOS needs no notice"
        );
    }

    #[test]
    fn the_first_tray_hide_asks_once_and_then_stops_asking() {
        for platform in [Platform::Windows, Platform::Unix] {
            assert_eq!(
                close_decision(platform, CloseAction::Hide, false, false),
                CloseDecision::Hide { notice: true },
                "{platform:?}"
            );
            assert_eq!(
                close_decision(platform, CloseAction::Hide, false, true),
                CloseDecision::Hide { notice: false },
                "{platform:?}"
            );
        }
    }

    #[test]
    fn a_configured_quit_is_never_turned_into_a_silent_hide() {
        for platform in [Platform::MacOs, Platform::Windows, Platform::Unix] {
            assert_eq!(
                close_decision(platform, CloseAction::Quit, false, false),
                CloseDecision::Quit,
                "{platform:?}"
            );
        }
    }

    #[test]
    fn only_windows_and_linux_reach_a_hidden_window_through_a_tray() {
        assert!(has_tray(Platform::Windows));
        assert!(has_tray(Platform::Unix));
        assert!(!has_tray(Platform::MacOs));
    }

    #[test]
    fn nothing_running_quits_without_asking() {
        assert_eq!(
            resolve_quit_prompt(QuitInspection::NothingToInterrupt),
            QuitPrompt::None
        );
        assert_eq!(
            resolve_quit_prompt(QuitInspection::Known {
                active_tasks: false,
                scheduled_tasks: false
            }),
            QuitPrompt::None
        );
    }

    #[test]
    fn a_host_that_cannot_be_inspected_still_asks() {
        assert_eq!(
            resolve_quit_prompt(QuitInspection::Unknown),
            QuitPrompt::ActiveTasks,
            "an unknown state must not quit silently"
        );
    }

    #[test]
    fn each_kind_of_interrupted_work_gets_its_own_copy() {
        let active = QuitInspection::Known {
            active_tasks: true,
            scheduled_tasks: false,
        };
        let scheduled = QuitInspection::Known {
            active_tasks: false,
            scheduled_tasks: true,
        };
        let both = QuitInspection::Known {
            active_tasks: true,
            scheduled_tasks: true,
        };
        assert_eq!(resolve_quit_prompt(active), QuitPrompt::ActiveTasks);
        assert_eq!(resolve_quit_prompt(scheduled), QuitPrompt::ScheduledTasks);
        assert_eq!(
            resolve_quit_prompt(both),
            QuitPrompt::ActiveAndScheduledTasks
        );
        assert_eq!(QuitPrompt::None.text(), None);
        for prompt in [
            QuitPrompt::ActiveTasks,
            QuitPrompt::ScheduledTasks,
            QuitPrompt::ActiveAndScheduledTasks,
        ] {
            let text = prompt.text().expect("copy");
            assert!(text.contains('\n'), "copy is bilingual: {text}");
        }
    }

    #[test]
    fn the_deep_link_that_starts_the_shell_is_the_only_one_acted_on() {
        let arguments = [
            "/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop".to_owned(),
            "dsh://open".to_owned(),
        ];
        assert_eq!(deeplink::first_open(&arguments), Some((1, DeepLink::Open)));

        let attach = [
            "dsh-desktop".to_owned(),
            "--url".to_owned(),
            "http://127.0.0.1:3080/?token=secret".to_owned(),
        ];
        assert_eq!(deeplink::first_open(&attach), None);
    }

    #[test]
    fn only_a_decided_quit_lets_the_exit_through() {
        reset_exit_approval();
        assert!(
            !exit_approved(),
            "an exit the shell has not confirmed must be intercepted"
        );
        approve_exit();
        assert!(exit_approved(), "exit_now() marks its own exit");
        reset_exit_approval();
    }

    #[test]
    fn a_failure_chain_is_split_on_blank_lines_and_never_empty() {
        assert_eq!(
            error_chain("outer\n\ncause one\n\ncause two"),
            ["outer", "cause one", "cause two"]
        );
        assert_eq!(error_chain("   "), ["(no detail was reported)"]);
    }

    #[test]
    fn the_recovery_dialog_keeps_the_tail_and_names_the_report() {
        let detail = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = recovery_detail(&detail, Some(Path::new("/tmp/crash-x.log")));
        assert!(body.starts_with("…（只显示错误尾部"));
        assert!(body.contains("line 20"));
        assert!(!body.contains("line 1\n"), "the head is dropped: {body}");
        assert!(body.contains("/tmp/crash-x.log"));
        assert!(body.contains("重新安装"), "advice is always appended");
    }

    #[test]
    fn a_short_failure_is_shown_whole_and_a_missing_report_says_nothing() {
        let body = recovery_detail("dsh is missing", None);
        assert!(!body.contains("只显示错误尾部"));
        assert!(body.starts_with("dsh is missing"));
        assert!(!body.contains("报告文件"));
    }
}
