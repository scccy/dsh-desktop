//! Preload bridge: the Tauri commands and events behind `window.dshDesktop`.
//!
//! The JavaScript half lives in `src-tauri/bridge/preload.js` and is injected as
//! an initialization script on the content webview; this module answers it.
//! Every command checks the calling webview and its document origin before it
//! answers, mirroring the Electron `assertDesktopSender` guard: only the
//! application's own document (`dsh-app://app` or a loopback origin) may reach
//! these operations.
//!
//! ## Registration checklist for `src-tauri/src/main.rs`
//!
//! ```ignore
//! mod bridge;
//!
//! tauri::Builder::default()
//!     // 1. State. `data_dir` is the shell's own directory (see state.rs).
//!     .manage(bridge::BridgeState::new(data_dir.clone()))
//!     // 2. Commands. The macro expands to `tauri::generate_handler![...]`.
//!     .invoke_handler(bridge_handler!())
//! ```
//!
//! and on the content webview builder (`window::build_window`):
//!
//! ```ignore
//! .initialization_script(bridge::bridge_script(Some("dsh-app://app")))
//! ```
//!
//! After the Desktop Host reports `ready`, hand the payload over (this is the
//! only wiring the bridge cannot do itself):
//!
//! ```ignore
//! let bridge = app.state::<bridge::BridgeState>();
//! bridge.set_boot(ready.injections.clone(), ready.origin.clone());
//! bridge.set_app_base(app_server_origin); // http://127.0.0.1:<port>
//! ```
//!
//! ## Events (emitted by the shell, subscribed by the preload)
//!
//! | event                                  | payload                                   | emitted by |
//! |----------------------------------------|-------------------------------------------|------------|
//! | `dsh-desktop://shortcuts-input`        | `DesktopShortcutInput`                    | menu/keyboard owner (`bridge::notify_shortcut_input`) |
//! | `dsh-desktop://shortcuts-changed`      | `ShortcutConfigSnapshot`                  | `desktop_shortcuts_edit` |
//! | `dsh-desktop://updates-presentation`   | `DesktopUpdatePresentation`               | `desktop_updates_open` |
//! | `dsh-desktop://window-fullscreen`      | `bool`                                    | window owner (`bridge::notify_fullscreen`) |
//! | `dsh-desktop://enter-workspace`        | `null`                                    | workspace handoff (`bridge::notify_workspace_entered`) |
//! | `dsh-desktop://browser-open-requested` | `{ lease, url }` (P3)                     | embedded browser owner |
//! | `dsh-desktop://boot-failed`            | the reported message                      | `desktop_boot_failed` |

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State, Webview};
use tauri_plugin_dialog::DialogExt;

use crate::connection::{self, ErrorKind, ShellError};
use crate::state::AppState;
use crate::window::{CONTENT_WEBVIEW, MAIN_WINDOW, MIN_HEIGHT, MIN_WIDTH};

/// The preload script, byte for byte (`node --check` covers its syntax).
pub const BRIDGE_JS: &str = include_str!("../bridge/preload.js");

/// Scheme of Desktop-owned application documents.
pub const APP_SCHEME: &str = "dsh-app";

/// Host of Desktop-owned application documents.
pub const APP_HOST: &str = "app";

/// Origin the preload script pins itself to (`dsh-app://app`).
pub const APP_SCHEME_ORIGIN: &str = "dsh-app://app";

/// Event carrying one native key press or menu activation to the Web UI.
#[allow(dead_code)] // native key relay lands with the sidebar browser (P3)
pub const EVENT_SHORTCUTS_INPUT: &str = "dsh-desktop://shortcuts-input";
/// Event carrying a newly accepted shortcut configuration.
pub const EVENT_SHORTCUTS_CHANGED: &str = "dsh-desktop://shortcuts-changed";
/// Event carrying one update presentation state.
pub const EVENT_UPDATES_PRESENTATION: &str = "dsh-desktop://updates-presentation";
/// Event carrying the window's fullscreen state.
pub const EVENT_WINDOW_FULLSCREEN: &str = "dsh-desktop://window-fullscreen";
/// Event asking the page to focus its document once the workspace took over.
pub const EVENT_ENTER_WORKSPACE: &str = "dsh-desktop://enter-workspace";
/// Event reporting a failed Web UI boot.
pub const EVENT_BOOT_FAILED: &str = "dsh-desktop://boot-failed";
/// Event carrying an embedded-browser navigation request (P3).
#[allow(dead_code)] // sidebar browser (P3)
pub const EVENT_BROWSER_OPEN_REQUESTED: &str = "dsh-desktop://browser-open-requested";

/// File inside the data directory holding the shortcut preferences.
pub const SHORTCUTS_FILE: &str = "shortcuts.json";
/// File inside the data directory holding the stored language preference.
pub const LOCALE_FILE: &str = "locale-preference.json";
/// Deadline for the short helper processes used by the device description.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for the `npm view` release probe.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Boot payload
// ---------------------------------------------------------------------------

/// Answer of `dshDesktopBoot.ready()`.
///
/// `injections` are applied verbatim by the Web UI boot script; `streamBaseUrl`
/// becomes `__DSH_TRANSPORT__.streamBaseUrl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootPayload {
    /// Host-provided injection rows (`global` / `script` / `script-src` / `style` / `html`).
    pub injections: Vec<Value>,
    /// Origin the Web UI opens its event stream against.
    pub stream_base_url: String,
}

// ---------------------------------------------------------------------------
// Shortcuts protocol (`@deepseek-ai/dsh-client-shortcuts/protocol`)
// ---------------------------------------------------------------------------

/// One declared binding, exactly as the trusted product frame sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutBinding {
    /// Physical key code (`KeyK`, `Digit1`, `F5`, …).
    pub code: String,
    /// Second key of a chord.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_code: Option<String>,
    /// Declared modifiers, including the logical `primary`.
    pub modifiers: Vec<String>,
}

/// One revision-checked preference edit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type")]
pub enum ShortcutEdit {
    /// Bind or unbind one command (`null` unbinds it explicitly).
    #[serde(rename = "set")]
    Set {
        /// Command identity.
        id: String,
        /// New binding, or `null` for an explicit unbind.
        #[serde(default)]
        binding: Option<ShortcutBinding>,
    },
    /// Drop the override of one command so it inherits its default again.
    #[serde(rename = "reset")]
    Reset {
        /// Command identity.
        id: String,
    },
    /// Drop every override of the current profile.
    #[serde(rename = "reset-all")]
    ResetAll,
}

/// Accepted shortcut preferences plus their identity, as the Web UI expects them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutConfigSnapshot {
    /// Opaque identity, invalidated by configuration and catalog changes.
    pub revision: String,
    /// Monotonic within one storage-owner lifetime.
    pub sequence: u64,
    /// Stored preference document, preserving dormant overrides.
    pub document: Value,
    /// `ready` or `unreadable`.
    pub status: &'static str,
    /// `read`, `invalid`, `future`, or `null`.
    pub error: Option<&'static str>,
    /// Whether no override applies to the current profile.
    pub using_defaults: bool,
}

/// Outcome of one preference edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutSaveResult {
    /// `saved`, `stale`, `unreadable`, `write-failed`, `not-ready` or `conflict`.
    pub status: &'static str,
    /// Snapshot the caller should adopt, accepted or unchanged.
    pub snapshot: ShortcutConfigSnapshot,
    /// Validation issue of the requested binding, when it cannot be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// Commands the requested binding collides with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
}

/// Persisted shortcut state shared by every read and write.
#[derive(Debug)]
struct ShortcutStore {
    /// Last accepted document (or the empty default).
    document: Value,
    /// Last catalog the revision was computed against.
    catalog: Vec<Value>,
    /// `ready` or `unreadable`.
    status: &'static str,
    /// Read failure classification.
    error: Option<&'static str>,
    /// Identity of the last accepted state.
    revision: String,
    /// Monotonic reply counter.
    sequence: u64,
}

// ---------------------------------------------------------------------------
// Updates
// ---------------------------------------------------------------------------

/// Phase of the Desktop-owned update presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // download/verify/install phases arrive with the updater (P3)
pub enum UpdatePhase {
    /// Nothing is happening.
    Idle,
    /// A release probe is running.
    Checking,
    /// A newer release exists.
    Available,
    /// The package is downloading.
    Downloading,
    /// The download is being verified.
    Verifying,
    /// The update is being installed.
    Installing,
    /// The update is ready to install.
    Ready,
    /// The last operation failed.
    Error,
}

/// Semantic update content rendered by the Web UI (no raw diagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopUpdatePresentation {
    /// Current phase.
    pub phase: UpdatePhase,
    /// Release version, when one was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Download progress in percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u32>,
    /// Classified failure copy selected by the Web locale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl DesktopUpdatePresentation {
    /// The presentation for "nothing is happening".
    pub fn idle() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            version: None,
            percent: None,
            failure: None,
        }
    }

    /// The presentation for a running release probe.
    pub fn checking() -> Self {
        Self {
            phase: UpdatePhase::Checking,
            version: None,
            percent: None,
            failure: None,
        }
    }

    /// The presentation for an available release.
    pub fn available(version: &str) -> Self {
        Self {
            phase: UpdatePhase::Available,
            version: Some(version.to_owned()),
            percent: None,
            failure: None,
        }
    }

    /// The presentation for a failed operation.
    pub fn failed(failure: &str) -> Self {
        Self {
            phase: UpdatePhase::Error,
            version: None,
            percent: None,
            failure: Some(failure.to_owned()),
        }
    }
}

impl Default for DesktopUpdatePresentation {
    fn default() -> Self {
        Self::idle()
    }
}

// ---------------------------------------------------------------------------
// Locale
// ---------------------------------------------------------------------------

/// Answer of `__DSH_LOCALE__.read()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocaleBootstrap {
    /// Operating-system languages, in preference order.
    pub languages: Vec<String>,
    /// Explicitly stored preference, or `null`.
    pub preference: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything the bridge commands share.
///
/// Managed by the shell with `app.manage(BridgeState::new(data_dir))`; the
/// setters are called from the startup path, never from the page.
pub struct BridgeState {
    data_dir: PathBuf,
    /// `darwin`, `win32` or `linux`.
    platform: &'static str,
    /// Additional accepted document origins (`scheme://host[:port]`).
    origins: Mutex<Vec<String>>,
    /// Loopback app-server base used by the request rewrite.
    app_base: Mutex<Option<String>>,
    /// Host payload handed to `dshDesktopBoot.ready()`.
    boot: Mutex<Option<BootPayload>>,
    /// Shortcut preferences.
    shortcuts: Mutex<ShortcutStore>,
    /// Latest update presentation.
    updates: Mutex<DesktopUpdatePresentation>,
    /// Stored language preference.
    locale: Mutex<Option<String>>,
    /// Whether the shortcut editor currently owns the key presses.
    recording: AtomicBool,
}

impl BridgeState {
    /// Builds the state for one shell instance.
    ///
    /// `data_dir` is the shell's own directory: `shortcuts.json` and
    /// `locale-preference.json` live there.
    pub fn new(data_dir: PathBuf) -> Self {
        let locale = read_locale_preference_file(&data_dir);
        Self {
            data_dir,
            platform: platform_name(),
            origins: Mutex::new(Vec::new()),
            app_base: Mutex::new(None),
            boot: Mutex::new(None),
            shortcuts: Mutex::new(ShortcutStore {
                document: empty_document(),
                catalog: Vec::new(),
                status: "ready",
                error: None,
                revision: String::new(),
                sequence: 0,
            }),
            updates: Mutex::new(DesktopUpdatePresentation::idle()),
            locale: Mutex::new(locale),
            recording: AtomicBool::new(false),
        }
    }

    /// Records the Host payload the Web UI boots from.
    pub fn set_boot(&self, injections: Vec<Value>, stream_base_url: impl Into<String>) {
        *lock(&self.boot) = Some(BootPayload {
            injections,
            stream_base_url: stream_base_url.into(),
        });
    }

    /// Records the loopback app-server base used by the request rewrite.
    pub fn set_app_base(&self, base: impl Into<String>) {
        *lock(&self.app_base) = Some(base.into());
    }

    /// Accepts one more document origin (`scheme://host[:port]`).
    #[allow(dead_code)] // used by the sidebar browser's guest origins (P3)
    pub fn allow_origin(&self, origin: impl Into<String>) {
        let origin = origin.into();
        if origin.is_empty() {
            return;
        }
        let mut origins = lock(&self.origins);
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }

    /// The recorded boot payload, when the Host is ready.
    pub fn boot(&self) -> Option<BootPayload> {
        lock(&self.boot).clone()
    }

    /// The recorded app-server base.
    pub fn app_base(&self) -> Option<String> {
        lock(&self.app_base).clone()
    }

    /// The accepted document origins.
    pub fn extra_origins(&self) -> Vec<String> {
        lock(&self.origins).clone()
    }

    /// The current update presentation.
    pub fn updates(&self) -> DesktopUpdatePresentation {
        lock(&self.updates).clone()
    }

    /// Stores and publishes one update presentation.
    pub fn publish_updates(&self, app: &AppHandle, presentation: DesktopUpdatePresentation) {
        *lock(&self.updates) = presentation.clone();
        if let Err(error) = app.emit(EVENT_UPDATES_PRESENTATION, &presentation) {
            log_warn(app, &format!("could not publish the update state: {error}"));
        }
    }

    /// Whether an edit may still be applied against `revision`.
    fn shortcut_revision_is(&self, revision: &str) -> bool {
        lock(&self.shortcuts).revision == revision
    }

    /// Whether the shortcut editor owns the key presses.
    fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    /// Reads the stored preferences against one command catalog.
    fn read_shortcuts(&self, definitions: Vec<Value>) -> ShortcutConfigSnapshot {
        let parsed = read_document(&self.data_dir);
        let profile = self.profile();
        let mut store = lock(&self.shortcuts);
        store.sequence += 1;
        store.catalog = definitions;
        store.document = parsed.document;
        store.status = parsed.status;
        store.error = parsed.error;
        store.revision = revision_for(&store.catalog, &store.document, &profile);
        snapshot_of(&store, &profile)
    }

    /// Applies one preference edit and persists the result.
    fn edit_shortcuts(&self, edit: &ShortcutEdit, revision: &str) -> ShortcutSaveResult {
        let profile = self.profile();
        let mut store = lock(&self.shortcuts);
        if store.status != "ready" {
            return ShortcutSaveResult {
                status: "not-ready",
                snapshot: snapshot_of(&store, &profile),
                issue: None,
                conflicts: None,
            };
        }
        if store.revision != revision {
            return ShortcutSaveResult {
                status: "stale",
                snapshot: snapshot_of(&store, &profile),
                issue: None,
                conflicts: None,
            };
        }
        let candidate = apply_edit(&store.document, edit, &profile);
        if let Err(error) = write_document(&self.data_dir.join(SHORTCUTS_FILE), &candidate) {
            if let Some(state) = error.raw_os_error() {
                let _ = state;
            }
            return ShortcutSaveResult {
                status: "write-failed",
                snapshot: snapshot_of(&store, &profile),
                issue: None,
                conflicts: None,
            };
        }
        store.document = candidate;
        store.sequence += 1;
        store.revision = revision_for(&store.catalog, &store.document, &profile);
        ShortcutSaveResult {
            status: "saved",
            snapshot: snapshot_of(&store, &profile),
            issue: None,
            conflicts: None,
        }
    }

    /// The current `runtime:platform` shortcut profile.
    fn profile(&self) -> String {
        format!("desktop:{}", shortcut_platform(self.platform))
    }
}

/// Locks a mutex, recovering the value after a panic instead of dropping it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Origin guard
// ---------------------------------------------------------------------------

/// `scheme://host[:port]` of a URL, or an empty string.
pub fn origin_of(url: &url::Url) -> String {
    match (url.scheme(), url.host_str()) {
        (scheme, Some(host)) => match url.port() {
            Some(port) => format!("{scheme}://{host}:{port}"),
            None => format!("{scheme}://{host}"),
        },
        _ => String::new(),
    }
}

/// Whether `url` belongs to a Desktop-owned document.
///
/// Application documents are the `dsh-app://app` scheme, any loopback http URL
/// (the shell pages and the app server) and every origin the shell allowed.
pub fn url_is_app_document(url: &url::Url, extra_origins: &[String]) -> bool {
    if url.scheme() == APP_SCHEME && url.host_str() == Some(APP_HOST) {
        return true;
    }
    if dsh_lite_core::url_policy::is_loopback_http(url) {
        return true;
    }
    let origin = origin_of(url);
    !origin.is_empty() && extra_origins.contains(&origin)
}

/// Rejects commands from any webview the shell does not own, or from a
/// document that is not a Desktop-owned application document.
fn assert_sender(webview: &Webview, bridge: &BridgeState) -> Result<(), String> {
    if webview.label() != CONTENT_WEBVIEW {
        return Err("dsh desktop: rejected IPC from an unowned webview".to_owned());
    }
    let url = webview
        .url()
        .map_err(|error| format!("dsh desktop: the caller URL is unavailable: {error}"))?;
    if !url_is_app_document(&url, &bridge.extra_origins()) {
        return Err("dsh desktop: rejected IPC from an unowned renderer".to_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

/// `process.platform`: `darwin`, `win32` or `linux`.
pub const fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "linux"
    }
}

/// The shortcut profile platform for a `process.platform` name.
pub const fn shortcut_platform(platform: &str) -> &'static str {
    match platform.as_bytes() {
        b"darwin" => "macos",
        b"win32" => "windows",
        _ => "linux",
    }
}

/// `process.arch` in Node's spelling (`arm64`, `x64`, …).
pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Preference document
// ---------------------------------------------------------------------------

/// Result of reading the preference file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedDocument {
    document: Value,
    status: &'static str,
    error: Option<&'static str>,
}

/// The empty preference document.
fn empty_document() -> Value {
    json!({ "schemaVersion": 2, "profiles": {} })
}

/// Reads and validates the preference document.
fn read_document(data_dir: &Path) -> ParsedDocument {
    let path = data_dir.join(SHORTCUTS_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ParsedDocument {
                document: empty_document(),
                status: "ready",
                error: None,
            };
        }
        Err(_) => {
            return ParsedDocument {
                document: empty_document(),
                status: "unreadable",
                error: Some("read"),
            };
        }
    };
    match parse_document(&raw) {
        Ok(document) => ParsedDocument {
            document,
            status: "ready",
            error: None,
        },
        Err(error) => ParsedDocument {
            document: empty_document(),
            status: "unreadable",
            error: Some(error),
        },
    }
}

/// Validates a stored preference document.
fn parse_document(raw: &str) -> Result<Value, &'static str> {
    let document: Value = serde_json::from_str(raw).map_err(|_| "invalid")?;
    let Some(object) = document.as_object() else {
        return Err("invalid");
    };
    let Some(schema_version) = object.get("schemaVersion").and_then(Value::as_u64) else {
        return Err("invalid");
    };
    if schema_version > 2 {
        return Err("future");
    }
    if object
        .get("profiles")
        .is_some_and(|profiles| !profiles.is_object())
    {
        return Err("invalid");
    }
    Ok(document)
}

/// Writes the preference document atomically (temporary file + rename).
fn write_document(path: &Path, document: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(document)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

/// Applies one edit to a copy of the document.
fn apply_edit(document: &Value, edit: &ShortcutEdit, profile: &str) -> Value {
    let mut candidate = document.clone();
    if !candidate.is_object() {
        candidate = empty_document();
    }
    let Some(root) = candidate.as_object_mut() else {
        return empty_document();
    };
    if !root.contains_key("schemaVersion") {
        root.insert("schemaVersion".to_owned(), json!(2));
    }
    let profiles_value = root
        .entry("profiles".to_owned())
        .or_insert_with(|| json!({}));
    if !profiles_value.is_object() {
        *profiles_value = json!({});
    }
    let Some(profiles) = profiles_value.as_object_mut() else {
        return candidate;
    };
    match edit {
        ShortcutEdit::Set { id, binding } => {
            let entry = profiles
                .entry(profile.to_owned())
                .or_insert_with(|| json!({}));
            if !entry.is_object() {
                *entry = json!({});
            }
            if let Some(map) = entry.as_object_mut() {
                let value = match binding {
                    Some(binding) => serde_json::to_value(binding).unwrap_or(Value::Null),
                    None => Value::Null,
                };
                map.insert(id.clone(), value);
            }
        }
        ShortcutEdit::Reset { id } => {
            if let Some(map) = profiles.get_mut(profile).and_then(Value::as_object_mut) {
                map.remove(id);
            }
        }
        ShortcutEdit::ResetAll => {
            profiles.remove(profile);
        }
    }
    candidate
}

/// The opaque identity of one accepted state.
///
/// The revision changes whenever the catalog or the stored document changes,
/// which is exactly when a reviewed draft has to be discarded.
fn revision_for(definitions: &[Value], document: &Value, profile: &str) -> String {
    let mut hasher = DefaultHasher::new();
    profile.hash(&mut hasher);
    definitions
        .iter()
        .map(Value::to_string)
        .collect::<Vec<String>>()
        .hash(&mut hasher);
    document.to_string().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Whether no override applies to the current profile.
fn using_defaults(document: &Value, profile: &str) -> bool {
    document
        .get("profiles")
        .and_then(|profiles| profiles.get(profile))
        .and_then(Value::as_object)
        .is_none_or(|overrides| overrides.is_empty())
}

/// The snapshot for one stored state.
fn snapshot_of(store: &ShortcutStore, profile: &str) -> ShortcutConfigSnapshot {
    ShortcutConfigSnapshot {
        revision: store.revision.clone(),
        sequence: store.sequence,
        document: store.document.clone(),
        status: store.status,
        error: store.error,
        using_defaults: using_defaults(&store.document, profile),
    }
}

// ---------------------------------------------------------------------------
// Locale preference
// ---------------------------------------------------------------------------

/// The stored language preference, when one was written.
fn read_locale_preference_file(data_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(data_dir.join(LOCALE_FILE)).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed
        .get("preference")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Stores the language preference.
fn write_locale_preference(data_dir: &Path, preference: &str) -> std::io::Result<()> {
    write_document(
        &data_dir.join(LOCALE_FILE),
        &json!({ "preference": preference }),
    )
}

/// The operating-system languages, deduplicated and in preference order.
fn system_languages() -> Vec<String> {
    let mut languages: Vec<String> = Vec::new();
    for language in sys_locale::get_locales() {
        let trimmed = language.trim().to_owned();
        if trimmed.is_empty() {
            continue;
        }
        if !languages
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(&trimmed))
        {
            languages.push(trimmed);
        }
    }
    languages
}

/// Validates a locale tag coming from the page.
fn locale_is_acceptable(locale: &str) -> bool {
    !locale.is_empty()
        && locale.len() <= 35
        && locale.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

// ---------------------------------------------------------------------------
// Device description
// ---------------------------------------------------------------------------

/// `name=value` fields separated by `; `, in the official field order, with no
/// hostname, user name or serial number.
pub fn read_device_info() -> String {
    let mut fields = vec![format!("platform={}", platform_name())];
    if let Some(os) = os_version() {
        fields.push(format!("os={os}"));
    }
    fields.push(format!("app_arch={}", arch_name()));
    if let Some(cpu) = cpu_model() {
        fields.push(format!("cpu={cpu}"));
    }
    if let Some(memory) = total_memory_gib() {
        fields.push(format!("memory_gib={memory}"));
    }
    fields.join("; ")
}

/// Runs a short read-only helper and returns its trimmed first line.
fn run_probe(program: &str, args: &[&str]) -> Option<String> {
    let args: Vec<String> = args.iter().map(|argument| (*argument).to_owned()).collect();
    let (code, output) = connection::helper_output(program, &[], &args, PROBE_TIMEOUT).ok()?;
    if code != 0 {
        return None;
    }
    let trimmed = output.lines().next().unwrap_or("").trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// The operating-system product version, when it is cheap to read.
fn os_version() -> Option<String> {
    if cfg!(target_os = "macos") {
        return run_probe("sw_vers", &["-productVersion"]);
    }
    if cfg!(target_os = "linux") {
        let release = fs::read_to_string("/etc/os-release").ok()?;
        let pretty = release
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))?
            .trim_matches('"')
            .to_owned();
        return if pretty.is_empty() {
            None
        } else {
            Some(pretty)
        };
    }
    // Windows has no cheap equivalent; the field is optional (the official
    // implementation omits unavailable fields as well).
    None
}

/// The reported CPU model.
fn cpu_model() -> Option<String> {
    if cfg!(target_os = "macos") {
        return run_probe("sysctl", &["-n", "machdep.cpu.brand_string"]);
    }
    if cfg!(target_os = "linux") {
        let info = fs::read_to_string("/proc/cpuinfo").ok()?;
        return info.lines().find_map(|line| {
            line.strip_prefix("model name")
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        });
    }
    std::env::var("PROCESSOR_IDENTIFIER").ok()
}

/// Total physical memory in GiB with one decimal.
fn total_memory_gib() -> Option<String> {
    if cfg!(target_os = "macos") {
        let bytes: f64 = run_probe("sysctl", &["-n", "hw.memsize"])?.parse().ok()?;
        return Some(format!("{:.1}", bytes / 1024f64.powi(3)));
    }
    if cfg!(target_os = "linux") {
        let info = fs::read_to_string("/proc/meminfo").ok()?;
        let kib: f64 = info
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        return Some(format!("{:.1}", kib / 1024f64.powi(2)));
    }
    None
}

// ---------------------------------------------------------------------------
// Logging helpers
// ---------------------------------------------------------------------------

/// Logs one warning through the shell logger, or keeps quiet without one.
fn log_warn(app: &AppHandle, message: &str) {
    if let Some(state) = app.try_state::<std::sync::Arc<AppState>>() {
        state.logger.warn(message);
    }
}

/// Logs one informational line through the shell logger.
fn log_info(app: &AppHandle, message: &str) {
    if let Some(state) = app.try_state::<std::sync::Arc<AppState>>() {
        state.logger.info(message);
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `dshDesktopBoot.ready()`: the Host payload the Web UI boots from.
#[tauri::command]
pub fn desktop_boot(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<BootPayload, String> {
    assert_sender(&webview, &bridge)?;
    let payload = bridge
        .boot()
        .ok_or_else(|| "dsh desktop: the Desktop Host is not ready".to_owned())?;
    if let Some(state) = app.try_state::<std::sync::Arc<AppState>>() {
        state.logger.debug(&format!(
            "desktop boot: {} injections, stream base {}",
            payload.injections.len(),
            payload.stream_base_url
        ));
    }
    Ok(payload)
}

/// `console.warn` / `console.error` / uncaught failures from the page.
///
/// The release build has no inspector, so the bridge forwards page diagnostics
/// into the shell log instead. Messages are truncated and never echo a payload
/// from an authentication call.
#[tauri::command]
pub fn desktop_console(app: AppHandle, level: String, message: String) {
    let Some(state) = app.try_state::<std::sync::Arc<AppState>>() else {
        return;
    };
    let text: String = message.chars().take(4000).collect();
    let text = dsh_lite_core::url_policy::redact_text(&text);
    if level == "error" {
        state.logger.warn(&format!("page error: {text}"));
    } else {
        state.logger.debug(&format!("page {level}: {text}"));
    }
}

/// `dshDesktopBoot.failed(message)`: report a startup failure to the shell.
#[tauri::command]
pub fn desktop_boot_failed(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    message: String,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    let detail: String = message.chars().take(2000).collect();
    log_warn(&app, &format!("web boot failed: {detail}"));
    if let Err(error) = app.emit(EVENT_BOOT_FAILED, &detail) {
        log_warn(
            &app,
            &format!("could not publish the boot failure: {error}"),
        );
    }
    // Reuse the existing error page instead of leaving a blank window.
    if let Some(state) = app.try_state::<std::sync::Arc<AppState>>() {
        connection::fail(
            &app,
            state.inner(),
            ShellError::new(ErrorKind::Ready, format!("web boot: {detail}")),
        );
    }
    Ok(())
}

/// `dshDesktop.deviceInfo()`: local machine description for feedback reports.
#[tauri::command]
pub fn desktop_device_info(
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<String, String> {
    assert_sender(&webview, &bridge)?;
    Ok(read_device_info())
}

/// `__DSH_DIRECTORY_PICKER__.pick()`: native folder selection.
///
/// Async so `blocking_pick_folder` runs off the main thread.
#[tauri::command]
pub async fn desktop_directory_pick(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<Option<String>, String> {
    assert_sender(&webview, &bridge)?;
    Ok(app
        .dialog()
        .file()
        .blocking_pick_folder()
        .map(|path| path.to_string()))
}

/// `dshDesktop.shortcuts.get(definitions)`.
#[tauri::command]
pub fn desktop_shortcuts_get(
    webview: Webview,
    bridge: State<'_, BridgeState>,
    definitions: Vec<Value>,
) -> Result<ShortcutConfigSnapshot, String> {
    assert_sender(&webview, &bridge)?;
    Ok(bridge.read_shortcuts(definitions))
}

/// `dshDesktop.shortcuts.edit(edit, revision)`.
#[tauri::command]
pub fn desktop_shortcuts_edit(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    edit: ShortcutEdit,
    revision: String,
) -> Result<ShortcutSaveResult, String> {
    assert_sender(&webview, &bridge)?;
    let result = bridge.edit_shortcuts(&edit, &revision);
    if result.status == "saved" {
        if let Err(error) = app.emit(EVENT_SHORTCUTS_CHANGED, &result.snapshot) {
            log_warn(&app, &format!("could not publish the shortcuts: {error}"));
        }
    } else if result.status == "write-failed" {
        log_warn(&app, "the shortcut preference file could not be written");
    }
    Ok(result)
}

/// `dshDesktop.shortcuts.recording(active)`.
#[tauri::command]
pub fn desktop_shortcuts_recording(
    webview: Webview,
    bridge: State<'_, BridgeState>,
    active: bool,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    bridge.recording.store(active, Ordering::SeqCst);
    Ok(())
}

/// `dshDesktop.keyboard.closeWindow(revision)`.
#[tauri::command]
pub fn desktop_shortcuts_close_window(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    revision: String,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    if !bridge.shortcut_revision_is(&revision) || bridge.is_recording() {
        return Ok(());
    }
    let Some(window) = app.get_window(MAIN_WINDOW) else {
        return Ok(());
    };
    if window.is_focused().unwrap_or(false) {
        let _ = window.close();
    }
    Ok(())
}

/// `dshDesktop.updates.status()`.
#[tauri::command]
pub fn desktop_updates_status(
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<DesktopUpdatePresentation, String> {
    assert_sender(&webview, &bridge)?;
    Ok(bridge.updates())
}

/// `dshDesktop.updates.open()`: run a release probe and publish the outcome.
#[tauri::command]
pub fn desktop_updates_open(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    let handle = app.clone();
    std::thread::spawn(move || run_update_probe(&handle));
    Ok(())
}

/// Probes the registry for a newer `dsh` and publishes each transition.
fn run_update_probe(app: &AppHandle) {
    let Some(bridge) = app.try_state::<BridgeState>() else {
        return;
    };
    bridge.publish_updates(app, DesktopUpdatePresentation::checking());
    let installed = app
        .try_state::<std::sync::Arc<AppState>>()
        .and_then(|state| state.runtime().dsh.version.clone());
    let (program, prefix) = crate::platform::shell_program("npm");
    let args = vec![
        "view".to_owned(),
        dsh_lite_core::update::PACKAGE.to_owned(),
        "version".to_owned(),
    ];
    log_info(app, "checking the published dsh release for the Web UI");
    let presentation = match connection::helper_output(&program, &prefix, &args, UPDATE_TIMEOUT) {
        Ok((code, output)) => {
            match dsh_lite_core::update::interpret(&output, code, installed.as_deref()) {
                Ok(outcome) if outcome.newer => {
                    DesktopUpdatePresentation::available(&outcome.latest)
                }
                Ok(_) => DesktopUpdatePresentation::idle(),
                Err(error) => {
                    log_warn(app, &format!("update check failed: {error}"));
                    DesktopUpdatePresentation::failed("check")
                }
            }
        }
        Err(error) => {
            log_warn(app, &format!("update check failed: {error}"));
            DesktopUpdatePresentation::failed("check-network")
        }
    };
    bridge.publish_updates(app, presentation);
}

/// `__DSH_LOCALE__.read()`.
#[tauri::command]
pub fn desktop_locale_bootstrap(
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<LocaleBootstrap, String> {
    assert_sender(&webview, &bridge)?;
    Ok(LocaleBootstrap {
        languages: system_languages(),
        preference: lock(&bridge.locale).clone(),
    })
}

/// `__DSH_LOCALE__.onChange(locale)`.
#[tauri::command]
pub fn desktop_locale_changed(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    locale: String,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    if !locale_is_acceptable(&locale) {
        return Err("dsh desktop: invalid locale".to_owned());
    }
    match write_locale_preference(&bridge.data_dir, &locale) {
        Ok(()) => {
            *lock(&bridge.locale) = Some(locale);
            Ok(())
        }
        Err(error) => {
            log_warn(
                &app,
                &format!("could not store the locale preference: {error}"),
            );
            Err("dsh desktop: the locale preference could not be stored".to_owned())
        }
    }
}

/// `dshOnboarding.hasApiKey()`: P3 hands this to the Host account service.
#[tauri::command]
pub fn desktop_onboarding_api_key(
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<bool, String> {
    assert_sender(&webview, &bridge)?;
    // P3: the official shell asks the Host for the credential state. The shell
    // has no credential store yet, so the Web UI sees "no API key" and offers
    // the account sign-in instead of the API-key walkthrough.
    Ok(false)
}

/// `dshOnboarding.setActive(active)`: reserve the wider minimum window.
#[tauri::command]
pub fn desktop_onboarding_active(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    active: bool,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    if let Some(window) = app.get_window(MAIN_WINDOW) {
        let size = if active {
            tauri::LogicalSize::new(960.0, 600.0)
        } else {
            tauri::LogicalSize::new(f64::from(MIN_WIDTH), f64::from(MIN_HEIGHT))
        };
        if let Err(error) = window.set_min_size(Some(size)) {
            log_warn(
                &app,
                &format!("could not resize the minimum window: {error}"),
            );
        }
    }
    Ok(())
}

/// `desktop_platform`: this process's `process.platform` equivalent.
#[tauri::command]
pub fn desktop_platform(
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<String, String> {
    assert_sender(&webview, &bridge)?;
    Ok(platform_name().to_owned())
}

/// The loopback app-server base the preload rewrites page requests onto.
#[tauri::command]
pub fn desktop_app_base(
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<String, String> {
    assert_sender(&webview, &bridge)?;
    bridge
        .app_base()
        .ok_or_else(|| "dsh desktop: the app server is not running".to_owned())
}

/// Native theme source from the Web UI (`light`, `dark` or `system`).
#[tauri::command]
pub fn desktop_native_theme_set(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    source: String,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    let theme = match source.as_str() {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        "system" => None,
        _ => return Ok(()),
    };
    if let Some(window) = app.get_window(MAIN_WINDOW)
        && let Err(error) = window.set_theme(theme)
    {
        log_warn(&app, &format!("could not apply the native theme: {error}"));
    }
    Ok(())
}

/// `dshPlatform.open(page, bounds)`: P3 (native Platform view).
#[tauri::command]
pub fn desktop_platform_open(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    page: String,
    bounds: Value,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    log_info(
        &app,
        &format!(
            "platform page requested ({page}); the native view is P3 ({})",
            summarize_bounds(&bounds)
        ),
    );
    Ok(())
}

/// `dshPlatform.setBounds(bounds)`: P3.
#[tauri::command]
pub fn desktop_platform_bounds(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
    bounds: Value,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    log_info(
        &app,
        &format!(
            "platform bounds reported; the native view is P3 ({})",
            summarize_bounds(&bounds)
        ),
    );
    Ok(())
}

/// `dshPlatform.close()`: P3.
#[tauri::command]
pub fn desktop_platform_close(
    app: AppHandle,
    webview: Webview,
    bridge: State<'_, BridgeState>,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    log_info(&app, "platform page closed; the native view is P3");
    Ok(())
}

/// `dshDesktop.browser.acquire(workspace)`: P3 (embedded webview guests).
#[tauri::command]
pub fn desktop_browser_acquire(
    webview: Webview,
    bridge: State<'_, BridgeState>,
    workspace: Value,
) -> Result<Value, String> {
    assert_sender(&webview, &bridge)?;
    let _ = workspace;
    Err("dsh desktop: embedded browser guests are not implemented yet (P3)".to_owned())
}

/// `dshDesktop.browser.release(lease)`: P3.
#[tauri::command]
pub fn desktop_browser_release(
    webview: Webview,
    bridge: State<'_, BridgeState>,
    lease: String,
) -> Result<(), String> {
    assert_sender(&webview, &bridge)?;
    let _ = lease;
    Err("dsh desktop: embedded browser guests are not implemented yet (P3)".to_owned())
}

/// A short, credential-free description of a bounds payload.
fn summarize_bounds(bounds: &Value) -> String {
    let field = |name: &str| {
        bounds
            .get(name)
            .and_then(Value::as_f64)
            .map(|value| format!("{value}"))
            .unwrap_or_else(|| "?".to_owned())
    };
    format!(
        "x={} y={} width={} height={}",
        field("x"),
        field("y"),
        field("width"),
        field("height")
    )
}

// ---------------------------------------------------------------------------
// Shell-side notification helpers
// ---------------------------------------------------------------------------

/// Builds the initialization script, optionally pinning the expected origin.
pub fn bridge_script(origin: Option<&str>) -> String {
    match origin {
        Some(origin) => format!(
            "globalThis.__DSH_BRIDGE_ORIGIN__ = {};\n{}",
            serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".to_owned()),
            BRIDGE_JS
        ),
        None => BRIDGE_JS.to_owned(),
    }
}

/// Tells the page that the workspace took over so it can focus its document.
#[allow(dead_code)]
pub fn notify_workspace_entered(app: &AppHandle) {
    if let Err(error) = app.emit(EVENT_ENTER_WORKSPACE, Value::Null) {
        log_warn(app, &format!("could not announce the workspace: {error}"));
    }
}

/// Mirrors the window's fullscreen state onto `<html data-fullscreen>`.
#[allow(dead_code)]
pub fn notify_fullscreen(app: &AppHandle, fullscreen: bool) {
    if let Err(error) = app.emit(EVENT_WINDOW_FULLSCREEN, fullscreen) {
        log_warn(app, &format!("could not announce fullscreen: {error}"));
    }
}

/// Publishes one native shortcut input (menu activation or forwarded key).
#[allow(dead_code)] // native key relay (P3)
pub fn notify_shortcut_input(app: &AppHandle, input: Value) {
    if let Err(error) = app.emit(EVENT_SHORTCUTS_INPUT, input) {
        log_warn(app, &format!("could not publish a key press: {error}"));
    }
}

/// Registers every bridge command on a Tauri builder.
///
/// `src-tauri/src/main.rs`:
/// `.invoke_handler(bridge_handler!())`.
#[macro_export]
macro_rules! bridge_handler {
    () => {
        tauri::generate_handler![
            $crate::bridge::desktop_boot,
            $crate::bridge::desktop_boot_failed,
            $crate::bridge::desktop_device_info,
            $crate::bridge::desktop_directory_pick,
            $crate::bridge::desktop_shortcuts_get,
            $crate::bridge::desktop_shortcuts_edit,
            $crate::bridge::desktop_shortcuts_recording,
            $crate::bridge::desktop_shortcuts_close_window,
            $crate::bridge::desktop_updates_status,
            $crate::bridge::desktop_updates_open,
            $crate::bridge::desktop_locale_bootstrap,
            $crate::bridge::desktop_locale_changed,
            $crate::bridge::desktop_onboarding_api_key,
            $crate::bridge::desktop_onboarding_active,
            $crate::bridge::desktop_platform,
            $crate::bridge::desktop_app_base,
            $crate::bridge::desktop_native_theme_set,
            $crate::bridge::desktop_platform_open,
            $crate::bridge::desktop_platform_bounds,
            $crate::bridge::desktop_platform_close,
            $crate::bridge::desktop_browser_acquire,
            $crate::bridge::desktop_browser_release,
        ]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> url::Url {
        url::Url::parse(raw).expect("valid url")
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "dsh-desktop-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&directory).expect("temporary directory");
        directory
    }

    #[test]
    fn the_boot_payload_uses_the_web_ui_spelling() {
        let payload = BootPayload {
            injections: vec![json!({ "kind": "global", "name": "x", "value": 1 })],
            stream_base_url: "http://127.0.0.1:54570".to_owned(),
        };
        let encoded = serde_json::to_value(&payload).expect("payload");
        assert_eq!(encoded["streamBaseUrl"], "http://127.0.0.1:54570");
        assert!(encoded.get("stream_base_url").is_none());
        assert_eq!(encoded["injections"][0]["kind"], "global");
        // The Web UI destructures exactly these two keys.
        let decoded: BootPayload = serde_json::from_value(encoded).expect("round trip");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn the_shortcut_snapshot_uses_the_web_ui_spelling() {
        let store = ShortcutStore {
            document: json!({
                "schemaVersion": 2,
                "profiles": { "desktop:macos": { "page.close": null } }
            }),
            catalog: vec![json!({ "id": "page.close" })],
            status: "ready",
            error: None,
            revision: "abc".to_owned(),
            sequence: 7,
        };
        let snapshot = snapshot_of(&store, "desktop:macos");
        let encoded = serde_json::to_value(&snapshot).expect("snapshot");
        assert_eq!(encoded["revision"], "abc");
        assert_eq!(encoded["sequence"], 7);
        assert_eq!(encoded["status"], "ready");
        assert_eq!(encoded["usingDefaults"], false);
        assert!(encoded["error"].is_null());
        assert!(encoded.get("using_defaults").is_none());
    }

    #[test]
    fn the_empty_document_counts_as_using_defaults() {
        let store = ShortcutStore {
            document: empty_document(),
            catalog: Vec::new(),
            status: "ready",
            error: None,
            revision: "abc".to_owned(),
            sequence: 1,
        };
        assert!(snapshot_of(&store, "desktop:macos").using_defaults);
    }

    #[test]
    fn every_shortcut_edit_shape_deserializes() {
        let set: ShortcutEdit = serde_json::from_value(json!({
            "type": "set",
            "id": "page.close",
            "binding": { "code": "KeyW", "modifiers": ["primary"] }
        }))
        .expect("set");
        let ShortcutEdit::Set { id, binding } = set else {
            panic!("expected set");
        };
        assert_eq!(id, "page.close");
        let binding = binding.expect("binding");
        assert_eq!(binding.code, "KeyW");
        assert_eq!(binding.modifiers, vec!["primary".to_owned()]);

        let unbind: ShortcutEdit = serde_json::from_value(json!({
            "type": "set",
            "id": "page.close",
            "binding": null
        }))
        .expect("unbind");
        let ShortcutEdit::Set { binding, .. } = unbind else {
            panic!("expected set");
        };
        assert!(binding.is_none());

        let reset: ShortcutEdit =
            serde_json::from_value(json!({ "type": "reset", "id": "page.close" })).expect("reset");
        assert_eq!(
            reset,
            ShortcutEdit::Reset {
                id: "page.close".to_owned()
            }
        );

        let reset_all: ShortcutEdit =
            serde_json::from_value(json!({ "type": "reset-all" })).expect("reset-all");
        assert_eq!(reset_all, ShortcutEdit::ResetAll);
    }

    #[test]
    fn an_edit_only_touches_the_current_profile() {
        let document = json!({
            "schemaVersion": 2,
            "profiles": { "desktop:windows": { "page.close": null } }
        });
        let edited = apply_edit(
            &document,
            &ShortcutEdit::Set {
                id: "session.new".to_owned(),
                binding: Some(ShortcutBinding {
                    code: "KeyN".to_owned(),
                    second_code: None,
                    modifiers: vec!["primary".to_owned()],
                }),
            },
            "desktop:macos",
        );
        let profiles = &edited["profiles"];
        assert_eq!(profiles["desktop:macos"]["session.new"]["code"], "KeyN");
        assert!(profiles["desktop:windows"]["page.close"].is_null());
    }

    #[test]
    fn resetting_removes_only_the_selected_override() {
        let document = json!({
            "schemaVersion": 2,
            "profiles": { "desktop:macos": { "a": null, "b": null } }
        });
        let edited = apply_edit(
            &document,
            &ShortcutEdit::Reset { id: "a".to_owned() },
            "desktop:macos",
        );
        assert!(edited["profiles"]["desktop:macos"].get("a").is_none());
        assert!(edited["profiles"]["desktop:macos"].get("b").is_some());

        let cleared = apply_edit(&edited, &ShortcutEdit::ResetAll, "desktop:macos");
        assert!(cleared["profiles"].get("desktop:macos").is_none());
    }

    #[test]
    fn the_revision_follows_the_catalog_and_the_document() {
        let document = empty_document();
        let first = revision_for(&[json!({ "id": "a" })], &document, "desktop:macos");
        let same = revision_for(&[json!({ "id": "a" })], &document, "desktop:macos");
        let other_catalog = revision_for(&[json!({ "id": "b" })], &document, "desktop:macos");
        let other_document = revision_for(
            &[json!({ "id": "a" })],
            &json!({ "schemaVersion": 2, "profiles": { "desktop:macos": { "a": null } } }),
            "desktop:macos",
        );
        assert_eq!(first, same);
        assert_ne!(first, other_catalog);
        assert_ne!(first, other_document);
        assert_eq!(first.len(), 16);
    }

    #[test]
    fn only_desktop_documents_may_reach_the_bridge() {
        assert!(url_is_app_document(&url("dsh-app://app/index.html"), &[]));
        assert!(url_is_app_document(&url("http://127.0.0.1:54321/"), &[]));
        assert!(url_is_app_document(&url("http://[::1]:8080/"), &[]));
        assert!(!url_is_app_document(&url("https://example.com/"), &[]));
        assert!(!url_is_app_document(
            &url("dsh-app://shell/update.html"),
            &[]
        ));
        assert!(!url_is_app_document(&url("http://example.com/"), &[]));
        assert!(url_is_app_document(
            &url("https://host.example/app"),
            &["https://host.example".to_owned()]
        ));
    }

    #[test]
    fn the_origin_of_a_url_keeps_its_port() {
        assert_eq!(
            origin_of(&url("http://127.0.0.1:54321/x")),
            "http://127.0.0.1:54321"
        );
        assert_eq!(origin_of(&url("dsh-app://app/x")), "dsh-app://app");
        // A URL without a host has no origin to compare, so it can never match.
        assert_eq!(origin_of(&url("file:///tmp/x")), "");
    }

    #[test]
    fn stored_documents_are_classified_before_they_are_used() {
        assert!(parse_document(r#"{"schemaVersion":2}"#).is_ok());
        assert_eq!(parse_document("{}"), Err("invalid"));
        assert_eq!(parse_document("nope"), Err("invalid"));
        assert_eq!(
            parse_document(r#"{"schemaVersion":3,"profiles":{}}"#),
            Err("future")
        );
        assert_eq!(
            parse_document(r#"{"schemaVersion":1,"profiles":[]}"#),
            Err("invalid")
        );
    }

    #[test]
    fn the_update_presentation_omits_absent_fields() {
        let idle = serde_json::to_value(DesktopUpdatePresentation::idle()).expect("idle");
        assert_eq!(idle, json!({ "phase": "idle" }));
        let available =
            serde_json::to_value(DesktopUpdatePresentation::available("0.3.0")).expect("available");
        assert_eq!(
            available,
            json!({ "phase": "available", "version": "0.3.0" })
        );
        let failed = serde_json::to_value(DesktopUpdatePresentation::failed("check-network"))
            .expect("failed");
        assert_eq!(failed["phase"], "error");
        assert_eq!(failed["failure"], "check-network");
        assert_eq!(
            serde_json::to_value(DesktopUpdatePresentation::checking()).expect("checking"),
            json!({ "phase": "checking" })
        );
    }

    #[test]
    fn the_locale_bootstrap_matches_the_client_parser() {
        let bootstrap = LocaleBootstrap {
            languages: vec!["zh-CN".to_owned(), "en-US".to_owned()],
            preference: None,
        };
        let encoded = serde_json::to_value(&bootstrap).expect("locale");
        assert_eq!(encoded["languages"][0], "zh-CN");
        assert!(encoded["preference"].is_null());
        assert!(locale_is_acceptable("zh-CN"));
        assert!(locale_is_acceptable("en"));
        assert!(!locale_is_acceptable(""));
        assert!(!locale_is_acceptable("zh CN"));
        assert!(!locale_is_acceptable(&"x".repeat(40)));
    }

    #[test]
    fn the_device_description_keeps_the_official_field_order() {
        let info = read_device_info();
        let fields: Vec<&str> = info.split("; ").collect();
        assert!(fields[0].starts_with("platform="));
        assert_eq!(fields[0], format!("platform={}", platform_name()));
        assert!(fields.iter().any(|field| field.starts_with("app_arch=")));
        for field in &fields {
            let (name, _) = field.split_once('=').expect("name=value");
            assert!(
                ["platform", "os", "app_arch", "cpu", "memory_gib"].contains(&name),
                "unexpected field {name}"
            );
        }
    }

    #[test]
    fn the_bridge_script_can_pin_the_document_origin() {
        let plain = bridge_script(None);
        assert_eq!(plain, BRIDGE_JS);
        assert!(plain.contains("dshDesktopBoot"));
        let pinned = bridge_script(Some("dsh-app://app"));
        assert!(pinned.starts_with("globalThis.__DSH_BRIDGE_ORIGIN__ = \"dsh-app://app\";\n"));
        assert!(pinned.ends_with(BRIDGE_JS));
    }

    #[test]
    fn the_preload_script_defines_every_documented_global() {
        for global in [
            "dshDesktopBoot",
            "dshDesktop",
            "dshOnboarding",
            "__DSH_DIRECTORY_PICKER__",
            "__DSH_HOST_PATHS__",
            "dshPlatform",
            "__DSH_LOCALE__",
        ] {
            assert!(
                BRIDGE_JS.contains(&format!("expose('{global}'")),
                "missing {global}"
            );
        }
    }

    #[test]
    fn the_preload_script_rewrites_dynamic_traffic() {
        assert!(BRIDGE_JS.contains("desktop_app_base"));
        assert!(BRIDGE_JS.contains("__DSH_APP_BASE__"));
        assert!(BRIDGE_JS.contains("installFetchShim"));
        assert!(BRIDGE_JS.contains("installXhrShim"));
        // WebSocket traffic follows `streamBaseUrl`; the bridge must not touch it.
        assert!(!BRIDGE_JS.contains("globalThis.WebSocket ="));
    }

    #[test]
    fn the_shortcut_platform_mapping_matches_the_protocol() {
        assert_eq!(shortcut_platform("darwin"), "macos");
        assert_eq!(shortcut_platform("win32"), "windows");
        assert_eq!(shortcut_platform("linux"), "linux");
        assert_eq!(shortcut_platform("freebsd"), "linux");
    }

    #[test]
    fn a_bounds_payload_is_summarized_without_extra_fields() {
        let summary = summarize_bounds(&json!({ "x": 1, "y": 2.5, "width": 300, "height": 400 }));
        assert_eq!(summary, "x=1 y=2.5 width=300 height=400");
        assert_eq!(summarize_bounds(&json!({})), "x=? y=? width=? height=?");
    }

    #[test]
    fn override_detection_ignores_other_profiles() {
        let document = json!({
            "schemaVersion": 2,
            "profiles": { "desktop:windows": { "a": null } }
        });
        assert!(using_defaults(&document, "desktop:macos"));
        assert!(!using_defaults(&document, "desktop:windows"));
    }

    #[test]
    fn the_document_files_round_trip_through_the_data_directory() {
        let directory = temporary_directory("bridge");

        let missing = read_document(&directory);
        assert_eq!(missing.status, "ready");
        assert!(missing.document.get("profiles").is_some());

        let document = apply_edit(
            &empty_document(),
            &ShortcutEdit::Set {
                id: "page.close".to_owned(),
                binding: None,
            },
            "desktop:macos",
        );
        write_document(&directory.join(SHORTCUTS_FILE), &document).expect("write");
        let reread = read_document(&directory);
        assert_eq!(reread.status, "ready");
        assert_eq!(
            reread.document["profiles"]["desktop:macos"]["page.close"],
            Value::Null
        );

        fs::write(directory.join(SHORTCUTS_FILE), "{ not json").expect("corrupt");
        let corrupt = read_document(&directory);
        assert_eq!(corrupt.status, "unreadable");
        assert_eq!(corrupt.error, Some("invalid"));

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_locale_preference_round_trips() {
        let directory = temporary_directory("locale");
        assert!(read_locale_preference_file(&directory).is_none());
        write_locale_preference(&directory, "zh-CN").expect("write");
        assert_eq!(
            read_locale_preference_file(&directory).as_deref(),
            Some("zh-CN")
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_shortcut_store_starts_ready_and_advances_its_sequence() {
        let directory = temporary_directory("store");
        let state = BridgeState::new(directory.clone());
        let catalog = vec![json!({ "id": "a", "defaults": {} })];
        let snapshot = state.read_shortcuts(catalog.clone());
        assert_eq!(snapshot.status, "ready");
        assert!(snapshot.using_defaults);
        assert_eq!(snapshot.sequence, 1);
        assert!(!snapshot.revision.is_empty());

        // The same read twice is a different sequence but the same revision.
        let next = state.read_shortcuts(catalog);
        assert_eq!(next.sequence, 2);
        assert_eq!(next.revision, snapshot.revision);

        // A stale revision is refused and nothing is written.
        let stale = state.edit_shortcuts(&ShortcutEdit::ResetAll, "0000000000000000");
        assert_eq!(stale.status, "stale");
        assert!(!directory.join(SHORTCUTS_FILE).exists());

        // The current revision is accepted and persisted.
        let saved = state.edit_shortcuts(
            &ShortcutEdit::Set {
                id: "a".to_owned(),
                binding: Some(ShortcutBinding {
                    code: "KeyK".to_owned(),
                    second_code: None,
                    modifiers: vec!["primary".to_owned()],
                }),
            },
            &next.revision,
        );
        assert_eq!(saved.status, "saved");
        assert!(!saved.snapshot.using_defaults);
        assert_ne!(saved.snapshot.revision, next.revision);
        assert!(directory.join(SHORTCUTS_FILE).exists());

        // A read after the save reproduces the revision the editor was given, so
        // the next edit is not rejected as stale.
        let after = state.read_shortcuts(vec![json!({ "id": "a", "defaults": {} })]);
        assert_eq!(after.revision, saved.snapshot.revision);
        assert!(!after.using_defaults);

        let _ = fs::remove_dir_all(&directory);
    }
}
