//! GUI-free logic for the DSH Desktop shell.
//!
//! Every module here is independent of Tauri and of the host windowing system so
//! that discovery, probing, URL handling and log retention are covered by plain
//! `cargo test`. The `src-tauri` crate only assembles these pieces around a
//! system WebView.

pub mod cli;
pub mod config;
pub mod console_text;
pub mod dsh_locate;
pub mod env_probe;
#[cfg(windows)]
pub mod job;
pub mod launcher;
pub mod logging;
pub mod update;
pub mod url_policy;

/// Shell name used in diagnostics, log lines and the window title.
pub const PRODUCT_NAME: &str = "DSH Desktop";

/// Shell version, reported by `--version` and settings.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
