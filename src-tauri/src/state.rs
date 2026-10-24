//! Shared state of the running shell.
//!
//! The Tauri callbacks, the connection worker and the commands all reach the
//! same values: the resolved configuration, the sponsored `dsh` child, the
//! connection the window shows and the origin of the local shell pages. Every
//! field is guarded by its own mutex so a slow operation (navigation, stopping a
//! child) never runs while the state is locked.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dsh_lite_core::cli::RunOptions;
use dsh_lite_core::config::{Config, ConnectMode, Platform};
use dsh_lite_core::launcher::DshChild;
use dsh_lite_core::logging::Logger;
use url::Url;

use crate::connection::ShellError;
use crate::window::WindowLayout;

/// File inside the data directory that remembers the window geometry.
pub const WINDOW_STATE_FILE: &str = "window.json";

/// File marking that the one-time "still running" notice was shown.
pub const BACKGROUND_NOTICE_FILE: &str = "background-notice-shown";

/// Connection the window is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// How the shell obtained the URL.
    pub mode: ConnectMode,
    /// Target without the token, safe for logs and the settings page.
    pub web_url: Url,
    /// Port of the target, when it has one.
    pub port: Option<u16>,
}

/// Facts about the `dsh` installation, shown by the settings page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DshSummary {
    /// Resolved command line, with the executable and any wrapper arguments.
    pub command: Option<String>,
    /// Version reported by `dsh --version`.
    pub version: Option<String>,
    /// Version reported by `node --version`.
    pub node_version: Option<String>,
}

/// What the shell is doing, polled by the loading page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    /// No connection started yet.
    #[default]
    Idle,
    /// A connection attempt is running.
    Connecting,
    /// The window shows the authenticated `dsh web` UI.
    Ready,
    /// The last attempt failed; the window shows the error page.
    Failed,
}

impl Status {
    /// Single word written into the settings payload.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// Mutable values mutated by the worker and the window callbacks.
#[derive(Debug, Default)]
pub struct Runtime {
    /// Sponsored `dsh` process, absent in attach mode.
    pub child: Option<DshChild>,
    /// Target the window shows, once a connection exists.
    pub connection: Option<ConnectionInfo>,
    /// Discovery and version facts of the current `dsh`.
    pub dsh: DshSummary,
    /// Progress marker for the loading page.
    pub status: Status,
    /// Origin of the local shell pages, learned from the first page load.
    pub local_base: Option<Url>,
    /// Tokenized URL whose token the shell is still waiting to see consumed.
    pub pending_auth: Option<Url>,
    /// Origin (`http://127.0.0.1:<port>/`) of the read-only server that hosts
    /// the shell's own pages.
    ///
    /// The pages are served over loopback http instead of `tauri://localhost` so
    /// they are same-site with the `dsh web` server (same host; SameSite ignores
    /// the port). That lets the `SameSite=Strict` auth cookie survive the token
    /// redirect in a single hop, and keeps later shell↔app navigation same-site.
    pub shell_origin: Option<Url>,
    /// Last failure, kept for the settings page.
    pub last_error: Option<ShellError>,
}

/// Everything the shell shares between threads.
pub struct AppState {
    /// Directory holding the configuration, the window state and the logs.
    pub data_dir: PathBuf,
    /// Directory of the rolling log files.
    pub log_dir: PathBuf,
    /// Configuration file, when the platform exposes one.
    pub config_path: Option<PathBuf>,
    /// Rolling logger, shared with every thread.
    pub logger: Arc<Logger>,
    /// Command-line overrides of this process.
    pub cli: RunOptions,
    /// Whether this launch created the configuration file.
    pub created_config: bool,
    config: Mutex<Config>,
    runtime: Mutex<Runtime>,
    layout: Mutex<Option<WindowLayout>>,
    shutdown_done: AtomicBool,
}

impl AppState {
    /// Builds the state; the caller owns logging and configuration loading.
    pub fn new(
        data_dir: PathBuf,
        config_path: Option<PathBuf>,
        logger: Arc<Logger>,
        config: Config,
        cli: RunOptions,
        created_config: bool,
    ) -> Self {
        let log_dir = logger.log_dir().to_path_buf();
        Self {
            data_dir,
            log_dir,
            config_path,
            logger,
            cli,
            created_config,
            config: Mutex::new(config),
            runtime: Mutex::new(Runtime::default()),
            layout: Mutex::new(None),
            shutdown_done: AtomicBool::new(false),
        }
    }

    /// Copy of the active configuration.
    pub fn config(&self) -> Config {
        lock(&self.config).clone()
    }

    /// Replaces the active configuration after a successful save.
    pub fn replace_config(&self, config: Config) {
        *lock(&self.config) = config;
    }

    /// Locks the mutable runtime values.
    pub fn runtime(&self) -> MutexGuard<'_, Runtime> {
        lock(&self.runtime)
    }

    /// Removes the sponsored child so it can be stopped outside the lock.
    pub fn take_child(&self) -> Option<DshChild> {
        self.runtime().child.take()
    }

    /// Keeps the latest window geometry for the exit path to persist.
    pub fn remember_layout(&self, layout: WindowLayout) {
        *lock(&self.layout) = Some(layout);
    }

    /// Claims the one shutdown: `true` for the caller that must run it.
    ///
    /// The exit sequence starts from the window's close request and is repeated
    /// by the run loop's exit events, so stopping the child and remembering the
    /// window must happen exactly once.
    pub fn begin_shutdown(&self) -> bool {
        !self.shutdown_done.swap(true, Ordering::SeqCst)
    }

    /// Latest known window geometry.
    pub fn layout(&self) -> Option<WindowLayout> {
        *lock(&self.layout)
    }

    /// Path of the remembered window geometry.
    pub fn window_state_path(&self) -> PathBuf {
        self.data_dir.join(WINDOW_STATE_FILE)
    }

    /// Path of the one-time background notice marker.
    pub fn background_notice_path(&self) -> PathBuf {
        self.data_dir.join(BACKGROUND_NOTICE_FILE)
    }

    /// Platform of the running process.
    pub fn platform(&self) -> Platform {
        Platform::current()
    }
}

/// Locks a mutex, recovering the value after a panic instead of dropping it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
