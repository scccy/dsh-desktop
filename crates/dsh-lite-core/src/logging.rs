//! Rolling log files and crash reports.
//!
//! The shell keeps one active log file and rotates it by size, so a long-running
//! session cannot fill the disk and the freshest output is always readable. Every
//! record and every crash report passes through [`redact_text`] before it reaches
//! the disk: the process token in the `dsh web` URL must never be written.
//!
//! Timestamps are UTC in ISO 8601 form; the shell never formats local time
//! because log files travel between machines in bug reports.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::LogLevel;
use crate::url_policy::redact_text;

/// Name of the active log file inside the log directory.
const ACTIVE_LOG: &str = "dsh-desktop.log";

/// Stem shared by the active file and its rotated siblings.
const LOG_STEM: &str = "dsh-desktop";

/// Size at which the active file rotates, in bytes.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Number of rotated files kept next to the active one (`dsh-desktop.1.log`…).
const ROTATED_FILES: u32 = 4;

/// Filename prefix of crash reports.
const CRASH_PREFIX: &str = "crash-";

/// Crash reports kept in the log directory; older ones are deleted.
const CRASH_KEPT: usize = 10;

/// Upper bound on the millisecond offsets tried for same-tick report names.
const CRASH_NAME_ATTEMPTS: u64 = 1_000;

/// Rolling log writer shared by every thread of the shell.
///
/// A record is written in one call under an internal mutex, so concurrent
/// writers cannot interleave partial lines. `verbose` promotes the effective
/// level to [`LogLevel::Debug`] regardless of the configured level, matching
/// `--verbose` on the command line.
pub struct Logger {
    dir: PathBuf,
    level: LogLevel,
    verbose: bool,
    state: Mutex<LogState>,
}

/// Mutable state guarded by [`Logger::state`].
struct LogState {
    file: File,
    size: u64,
}

impl Logger {
    /// Opens the log directory and appends to the active file, creating both.
    pub fn open(dir: &Path, level: LogLevel, verbose: bool) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(ACTIVE_LOG))?;
        let size = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_path_buf(),
            level,
            verbose,
            state: Mutex::new(LogState { file, size }),
        })
    }

    /// Directory holding the active file and its rotated siblings.
    pub fn log_dir(&self) -> &Path {
        &self.dir
    }

    /// Writes one record unless `level` is more verbose than the active level.
    ///
    /// Line breaks inside `message` are escaped, so a child process cannot forge
    /// a log line by printing a timestamp and level of its own.
    pub fn record(&self, level: LogLevel, message: &str) {
        if level > self.effective_level() {
            return;
        }
        let line = format!(
            "{} {} {}\n",
            utc_timestamp(SystemTime::now()),
            level_tag(level),
            sanitize(message)
        );
        self.append(&line);
    }

    /// Records a failure that stops a user-visible operation.
    pub fn error(&self, message: &str) {
        self.record(LogLevel::Error, message);
    }

    /// Records a recoverable problem, such as a failed fallback probe.
    pub fn warn(&self, message: &str) {
        self.record(LogLevel::Warn, message);
    }

    /// Records a lifecycle event; the default level.
    pub fn info(&self, message: &str) {
        self.record(LogLevel::Info, message);
    }

    /// Records diagnostics that `--verbose` adds.
    pub fn debug(&self, message: &str) {
        self.record(LogLevel::Debug, message);
    }

    fn effective_level(&self) -> LogLevel {
        if self.verbose {
            LogLevel::Debug
        } else {
            self.level
        }
    }

    fn append(&self, line: &str) {
        if let Err(error) = self.try_append(line) {
            // Logging must never take the shell down; stderr is the last resort.
            eprintln!("DSH Desktop: could not write log line: {error}");
        }
    }

    fn try_append(&self, line: &str) -> io::Result<()> {
        let mut state = self.lock();
        if state.size + line.len() as u64 > MAX_LOG_BYTES {
            self.rotate(&mut state)?;
        }
        state.file.write_all(line.as_bytes())?;
        state.file.flush()?;
        state.size += line.len() as u64;
        Ok(())
    }

    /// Recovers the state even after a panic left the mutex poisoned; the file
    /// handle itself stays usable, so dropping the log would lose more than it
    /// protects.
    fn lock(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drops the oldest rotated file, shifts the rest up by one and starts a new
    /// active file.
    fn rotate(&self, state: &mut LogState) -> io::Result<()> {
        let oldest = self.rotated_path(ROTATED_FILES);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for index in (1..ROTATED_FILES).rev() {
            let from = self.rotated_path(index);
            if from.exists() {
                fs::rename(&from, self.rotated_path(index + 1))?;
            }
        }
        let active = self.dir.join(ACTIVE_LOG);
        if active.exists() {
            fs::rename(&active, self.rotated_path(1))?;
        }
        state.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&active)?;
        state.size = 0;
        Ok(())
    }

    fn rotated_path(&self, index: u32) -> PathBuf {
        self.dir.join(format!("{LOG_STEM}.{index}.log"))
    }
}

/// What failed when the shell wrote a crash report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashSource {
    /// The shell could not reach a ready `dsh web` endpoint.
    Startup,
    /// The WebView reported a renderer or navigation failure.
    Renderer,
    /// The shell itself failed after startup.
    Shell,
}

impl CrashSource {
    /// Single-word form written into the report filename and body.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Renderer => "renderer",
            Self::Shell => "shell",
        }
    }
}

/// A fatal failure written next to the logs for the user to attach to a bug
/// report.
///
/// The report deliberately carries only versions and the error chain: no
/// environment values, no configuration values and no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport {
    /// Component that failed.
    pub source: CrashSource,
    /// Most specific error first, followed by its causes.
    pub error_chain: Vec<String>,
    /// Version of the shell that wrote the report.
    pub shell_version: String,
    /// Version reported by the `dsh` executable, when it could be read.
    pub dsh_version: Option<String>,
    /// Operating system and architecture, as `os arch`.
    pub platform: String,
}

/// Writes `report` into `dir` and prunes old reports.
///
/// The filename is `crash-<UTC compact>-<source>.log`. Two reports written in the
/// same millisecond would collide, so a busy millisecond takes the next free one;
/// filenames therefore stay unique and sorted by age. Only the newest
/// [`CRASH_KEPT`] reports survive.
pub fn write_crash_report(dir: &Path, report: &CrashReport) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let now = SystemTime::now();
    let path = crash_path(dir, now, report.source.as_str());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(crash_body(now, report).as_bytes())?;
    file.flush()?;
    prune_crash_reports(dir)?;
    Ok(path)
}

/// Filename for a report, skipping stamps already taken.
fn crash_path(dir: &Path, now: SystemTime, source: &str) -> PathBuf {
    for offset in 0..CRASH_NAME_ATTEMPTS {
        let stamp = utc_compact(now + Duration::from_millis(offset));
        let candidate = dir.join(format!("{CRASH_PREFIX}{stamp}-{source}.log"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Exhausting the offsets needs a clock stopped for a second; `create_new`
    // then reports the collision as an ordinary I/O error.
    dir.join(format!("{CRASH_PREFIX}{}-{source}.log", utc_compact(now)))
}

/// Report text, redacted as a whole.
fn crash_body(now: SystemTime, report: &CrashReport) -> String {
    let mut body = String::new();
    body.push_str(&redact_text("DSH Desktop crash report"));
    body.push('\n');
    body.push_str(&format!("time: {}\n", utc_timestamp(now)));
    body.push_str(&format!("source: {}\n", report.source.as_str()));
    body.push_str(&format!(
        "shell version: {}\n",
        redact_text(&report.shell_version)
    ));
    body.push_str(&format!(
        "dsh version: {}\n",
        redact_text(report.dsh_version.as_deref().unwrap_or("unknown"))
    ));
    body.push_str(&format!("platform: {}\n", redact_text(&report.platform)));
    body.push_str("\nerror chain:\n");
    if report.error_chain.is_empty() {
        body.push_str("  1. (no error reported)\n");
    }
    for (index, entry) in report.error_chain.iter().enumerate() {
        body.push_str(&format!("  {}. {}\n", index + 1, redact_text(entry)));
    }
    body
}

/// Deletes all but the newest [`CRASH_KEPT`] reports in `dir`.
fn prune_crash_reports(dir: &Path) -> io::Result<()> {
    let mut reports: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(CRASH_PREFIX) || !name.ends_with(".log") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        reports.push((modified, entry.path()));
    }
    if reports.len() <= CRASH_KEPT {
        return Ok(());
    }
    // Oldest first; the filename breaks ties for reports written in the same tick.
    reports.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let surplus = reports.len() - CRASH_KEPT;
    for (_, path) in reports.into_iter().take(surplus) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Level tag written into a log line.
fn level_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::Info => "INFO",
        LogLevel::Debug => "DEBUG",
    }
}

/// Redacts `message` and escapes line breaks so one call produces one line.
fn sanitize(message: &str) -> String {
    redact_text(message)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for `now`.
fn utc_timestamp(now: SystemTime) -> String {
    let (year, month, day, hour, minute, second, millis) = utc_parts(now);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// `YYYYMMDDTHHMMSSmmmZ` for `now`, used in crash report filenames.
fn utc_compact(now: SystemTime) -> String {
    let (year, month, day, hour, minute, second, millis) = utc_parts(now);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}{millis:03}Z")
}

/// Calendar parts of `now` in UTC. Times before the Unix epoch clamp to it.
fn utc_parts(now: SystemTime) -> (i64, u32, u32, u32, u32, u32, u32) {
    let elapsed = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let millis = elapsed.subsec_millis();
    let seconds = elapsed.as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (day_seconds / 3_600) as u32,
        ((day_seconds % 3_600) / 60) as u32,
        (day_seconds % 60) as u32,
        millis,
    )
}

/// Gregorian date for a day count since 1970-01-01, after Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    if month <= 2 {
        (year + 1, month, day)
    } else {
        (year, month, day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    fn logger_at(level: LogLevel) -> (TempDir, Logger) {
        let dir = TempDir::new().expect("temp dir");
        let logger = Logger::open(dir.path(), level, false).expect("logger");
        (dir, logger)
    }

    fn read_log(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    fn record_count(content: &str) -> usize {
        content.lines().count()
    }

    #[test]
    fn log_dir_is_the_opened_directory() {
        let (dir, logger) = logger_at(LogLevel::Info);
        assert_eq!(logger.log_dir(), dir.path());
    }

    #[test]
    fn open_creates_the_log_directory() {
        let root = TempDir::new().expect("temp dir");
        let dir = root.path().join("nested").join("logs");
        let logger = Logger::open(&dir, LogLevel::Info, false).expect("logger");
        assert!(dir.is_dir());
        logger.info("ready");
        assert!(read_log(&dir, ACTIVE_LOG).contains("ready"));
    }

    #[test]
    fn line_format_is_utc_timestamp_level_and_message() {
        let (dir, logger) = logger_at(LogLevel::Info);
        logger.info("shell started");
        let content = read_log(dir.path(), ACTIVE_LOG);
        let line = content.lines().next().expect("one record");
        let mut parts = line.splitn(3, ' ');
        let timestamp = parts.next().expect("timestamp");
        let level = parts.next().expect("level");
        let message = parts.next().expect("message");
        assert_eq!(level, "INFO");
        assert_eq!(message, "shell started");
        assert_eq!(timestamp.len(), 24, "unexpected timestamp {timestamp}");
        assert!(timestamp.ends_with('Z'));
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[19..20], ".");
        assert!(timestamp[..4].chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn levels_are_filtered_by_the_configured_level() {
        let (dir, logger) = logger_at(LogLevel::Warn);
        logger.error("fatal detail");
        logger.warn("recoverable detail");
        logger.info("lifecycle detail");
        logger.debug("probe detail");
        let content = read_log(dir.path(), ACTIVE_LOG);
        assert_eq!(record_count(&content), 2);
        assert!(content.contains("ERROR fatal detail"));
        assert!(content.contains("WARN recoverable detail"));
        assert!(!content.contains("lifecycle detail"));
        assert!(!content.contains("probe detail"));
    }

    #[test]
    fn debug_level_records_everything() {
        let (dir, logger) = logger_at(LogLevel::Debug);
        logger.error("e");
        logger.warn("w");
        logger.info("i");
        logger.debug("d");
        assert_eq!(record_count(&read_log(dir.path(), ACTIVE_LOG)), 4);
    }

    #[test]
    fn verbose_promotes_the_active_level() {
        let dir = TempDir::new().expect("temp dir");
        let logger = Logger::open(dir.path(), LogLevel::Error, true).expect("logger");
        logger.debug("forwarded child output");
        assert!(read_log(dir.path(), ACTIVE_LOG).contains("DEBUG forwarded child output"));
    }

    #[test]
    fn tokens_in_messages_are_redacted() {
        let (dir, logger) = logger_at(LogLevel::Info);
        logger.info("spawned dsh web at http://127.0.0.1:4100/?token=supersecret");
        logger.error("navigation failed for http://127.0.0.1:4100/?a=1&token=supersecret");
        let content = read_log(dir.path(), ACTIVE_LOG);
        assert!(!content.contains("supersecret"), "token leaked: {content}");
        assert!(content.contains("token=<redacted>"));
        assert!(content.contains("a=1&token=<redacted>"));
    }

    #[test]
    fn line_breaks_in_messages_cannot_forge_records() {
        let (dir, logger) = logger_at(LogLevel::Info);
        logger.info("child output\n2026-01-01T00:00:00.000Z INFO forged line");
        let content = read_log(dir.path(), ACTIVE_LOG);
        assert_eq!(record_count(&content), 1);
        assert!(content.contains("\\n2026-01-01T00:00:00.000Z INFO forged line"));
    }

    #[test]
    fn appends_to_an_existing_file() {
        let dir = TempDir::new().expect("temp dir");
        {
            let logger = Logger::open(dir.path(), LogLevel::Info, false).expect("logger");
            logger.info("first run");
        }
        let logger = Logger::open(dir.path(), LogLevel::Info, false).expect("logger");
        logger.info("second run");
        let content = read_log(dir.path(), ACTIVE_LOG);
        assert_eq!(record_count(&content), 2);
    }

    #[test]
    fn active_file_rotates_at_the_size_threshold() {
        let (dir, logger) = logger_at(LogLevel::Info);
        let chunk = "x".repeat(256 * 1024);
        let records = (MAX_LOG_BYTES / chunk.len() as u64) as usize + 4;
        for _ in 0..records {
            logger.info(&chunk);
        }
        assert!(dir.path().join(format!("{LOG_STEM}.1.log")).exists());
        let active = fs::metadata(dir.path().join(ACTIVE_LOG)).expect("active file");
        assert!(active.len() < MAX_LOG_BYTES);
        assert!(active.len() > 0);
    }

    #[test]
    fn rotation_keeps_five_files_and_drops_the_oldest() {
        let (dir, logger) = logger_at(LogLevel::Info);
        for index in 1..=ROTATED_FILES {
            fs::write(
                dir.path().join(format!("{LOG_STEM}.{index}.log")),
                format!("old record {index}\n"),
            )
            .expect("seed rotated file");
        }
        let chunk = "x".repeat(256 * 1024);
        for _ in 0..((MAX_LOG_BYTES / chunk.len() as u64) as usize + 1) {
            logger.info(&chunk);
        }
        for index in 1..=ROTATED_FILES {
            assert!(
                dir.path().join(format!("{LOG_STEM}.{index}.log")).exists(),
                "rotated file {index} missing"
            );
        }
        assert!(
            !dir.path()
                .join(format!("{LOG_STEM}.{}.log", ROTATED_FILES + 1))
                .exists()
        );
        let oldest = read_log(dir.path(), &format!("{LOG_STEM}.{}.log", ROTATED_FILES));
        assert!(
            oldest.contains("old record 3"),
            "unexpected oldest file: {oldest}"
        );
        let rotated_active = fs::metadata(dir.path().join(format!("{LOG_STEM}.1.log")));
        assert!(rotated_active.expect("rotated active file").len() > 0);
    }

    #[test]
    fn log_writes_are_thread_safe() {
        let dir = TempDir::new().expect("temp dir");
        let logger = Logger::open(dir.path(), LogLevel::Info, false).expect("logger");
        thread::scope(|scope| {
            for worker in 0..4 {
                let logger = &logger;
                scope.spawn(move || {
                    for record in 0..25 {
                        logger.info(&format!("worker {worker} record {record}"));
                    }
                });
            }
        });
        let content = read_log(dir.path(), ACTIVE_LOG);
        assert_eq!(record_count(&content), 100);
        assert!(content.lines().all(|line| line.contains(" INFO worker ")));
    }

    #[test]
    fn crash_report_name_encodes_source_and_utc_stamp() {
        let dir = TempDir::new().expect("temp dir");
        let report = sample_report(CrashSource::Renderer, vec!["renderer crashed".to_owned()]);
        let path = write_crash_report(dir.path(), &report).expect("crash report");
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("crash-"), "unexpected name {name}");
        assert!(name.ends_with("-renderer.log"), "unexpected name {name}");
        assert!(name.contains('T'));
        assert!(path.is_file());
    }

    #[test]
    fn crash_report_contains_versions_and_error_chain() {
        let dir = TempDir::new().expect("temp dir");
        let report = sample_report(
            CrashSource::Startup,
            vec!["no dsh web line".to_owned(), "deadline elapsed".to_owned()],
        );
        let path = write_crash_report(dir.path(), &report).expect("crash report");
        let content = fs::read_to_string(&path).expect("report text");
        assert!(content.contains("source: startup"));
        assert!(content.contains("shell version: 0.1.0"));
        assert!(content.contains("dsh version: 1.2.3"));
        assert!(content.contains("platform: windows x86_64"));
        assert!(content.contains("  1. no dsh web line"));
        assert!(content.contains("  2. deadline elapsed"));
        assert!(content.contains("time: "));
    }

    #[test]
    fn crash_report_handles_missing_dsh_version_and_empty_chain() {
        let dir = TempDir::new().expect("temp dir");
        let report = CrashReport {
            source: CrashSource::Shell,
            error_chain: Vec::new(),
            shell_version: "0.1.0".to_owned(),
            dsh_version: None,
            platform: "windows x86_64".to_owned(),
        };
        let path = write_crash_report(dir.path(), &report).expect("crash report");
        let content = fs::read_to_string(&path).expect("report text");
        assert!(content.contains("dsh version: unknown"));
        assert!(content.contains("source: shell"));
        assert!(content.contains("(no error reported)"));
    }

    #[test]
    fn crash_report_redacts_tokens() {
        let dir = TempDir::new().expect("temp dir");
        let report = CrashReport {
            source: CrashSource::Renderer,
            error_chain: vec![
                "navigation to http://127.0.0.1:4100/?token=supersecret failed".to_owned(),
                "child printed token=supersecret".to_owned(),
            ],
            shell_version: "0.1.0".to_owned(),
            dsh_version: Some("1.2.3".to_owned()),
            platform: "windows x86_64".to_owned(),
        };
        let path = write_crash_report(dir.path(), &report).expect("crash report");
        let content = fs::read_to_string(&path).expect("report text");
        assert!(!content.contains("supersecret"), "token leaked: {content}");
        assert!(content.contains("token=<redacted>"));
    }

    #[test]
    fn crash_reports_are_capped_at_ten() {
        let dir = TempDir::new().expect("temp dir");
        let mut written = Vec::new();
        for index in 0..12 {
            let report = sample_report(CrashSource::Shell, vec![format!("failure {index}")]);
            written.push(write_crash_report(dir.path(), &report).expect("crash report"));
        }
        let remaining: Vec<PathBuf> = fs::read_dir(dir.path())
            .expect("log dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().starts_with(CRASH_PREFIX))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            remaining.len(),
            CRASH_KEPT,
            "remaining reports: {remaining:?}"
        );
        let newest = written.last().expect("newest report");
        assert!(newest.exists(), "newest report was pruned");
        assert!(!written[0].exists(), "oldest report survived pruning");
    }

    #[test]
    fn crash_reports_do_not_disturb_log_files() {
        let dir = TempDir::new().expect("temp dir");
        let logger = Logger::open(dir.path(), LogLevel::Info, false).expect("logger");
        logger.info("before crash");
        let report = sample_report(CrashSource::Shell, vec!["boom".to_owned()]);
        write_crash_report(dir.path(), &report).expect("crash report");
        let content = read_log(dir.path(), ACTIVE_LOG);
        assert_eq!(record_count(&content), 1);
        assert!(content.contains("before crash"));
    }

    #[test]
    fn timestamps_are_utc_and_advance() {
        let earlier = UNIX_EPOCH + Duration::from_millis(0);
        assert_eq!(utc_timestamp(earlier), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_compact(earlier), "19700101T000000000Z");
        let sample = UNIX_EPOCH + Duration::from_millis(1_767_227_523_456);
        assert_eq!(utc_timestamp(sample), "2026-01-01T00:32:03.456Z");
        assert_eq!(utc_compact(sample), "20260101T003203456Z");
        let leap = UNIX_EPOCH + Duration::from_millis(951_782_400_000);
        assert_eq!(utc_timestamp(leap), "2000-02-29T00:00:00.000Z");
    }

    fn sample_report(source: CrashSource, error_chain: Vec<String>) -> CrashReport {
        CrashReport {
            source,
            error_chain,
            shell_version: "0.1.0".to_owned(),
            dsh_version: Some("1.2.3".to_owned()),
            platform: "windows x86_64".to_owned(),
        }
    }
}
