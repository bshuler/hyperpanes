//! Process-wide logging: `tracing` with a size-rotated log file per process role.
//!
//! Every process this binary can be (the GUI, the session daemon, a `ctl`/`attach`/`worker`
//! command line, the crash reporter) calls [`init`] once at entry. Levels resolve, highest
//! precedence first:
//!
//! 1. `HYPERPANES_LOG` — a `tracing_subscriber::EnvFilter` directive string (`debug`,
//!    `hyperpanes_core::session=trace,info`, …). A session-scoped override.
//! 2. `HYPERPANES_DEBUG` — the historical debug switch; any value means `debug`.
//! 3. The persisted `logLevel` setting, handed in by the caller as `default_level`.
//! 4. `info`.
//!
//! At `debug`, every instrumented function (`#[tracing::instrument(level = "debug", ret)]`)
//! logs its entry with its parameters and its exit with its return value: the subscriber
//! emits span `new`/`close` events, and `ret` records the value on the way out. At `info`
//! the spans cost one disabled-callsite check each and emit nothing.
//!
//! The file lives under [`crate::persistence::paths::logs_dir`] as `hyperpanes-<role>.log`
//! and rolls at [`MAX_LOG_BYTES`] into `.1` … `.N` ([`KEEP_ROTATED`]). Warnings and errors are
//! mirrored to stderr so a terminal launch still shows what went wrong.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// A log file rolls over once it would exceed this many bytes.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
/// How many rolled files (`.1` newest … `.N` oldest) are kept beside the live one.
pub const KEEP_ROTATED: usize = 5;

/// Environment variable carrying an `EnvFilter` directive string.
pub const ENV_LOG: &str = "HYPERPANES_LOG";
/// The historical debug switch: set (to anything) means `debug`.
pub const ENV_DEBUG: &str = "HYPERPANES_DEBUG";

/// The level names a `logLevel` setting may hold, in ascending verbosity.
pub const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
/// The level used when nothing else says otherwise.
pub const DEFAULT_LEVEL: &str = "info";

/// Whether `level` is one of [`LEVELS`] (case-insensitive).
#[tracing::instrument(level = "debug", ret)]
pub fn valid_level(level: &str) -> bool {
    LEVELS.iter().any(|l| l.eq_ignore_ascii_case(level))
}

/// The directive string [`init`] will use, given the persisted level. Exposed so the
/// resolution order is testable without installing a subscriber.
#[tracing::instrument(level = "debug", ret)]
pub fn resolve_directives(default_level: &str) -> String {
    resolve_from(
        std::env::var(ENV_LOG).ok().as_deref(),
        std::env::var_os(ENV_DEBUG).is_some(),
        default_level,
    )
}

#[tracing::instrument(level = "debug", ret)]
fn resolve_from(env_log: Option<&str>, env_debug: bool, default_level: &str) -> String {
    if let Some(d) = env_log.map(str::trim).filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    if env_debug {
        return "debug".to_string();
    }
    let lvl = default_level.trim().to_ascii_lowercase();
    if valid_level(&lvl) {
        lvl
    } else {
        DEFAULT_LEVEL.to_string()
    }
}

/// The log file for a process role (`app`, `daemon`, `cli`, `worker`, `crash`, `headless`).
#[tracing::instrument(level = "debug", ret)]
pub fn log_path(role: &str) -> PathBuf {
    crate::persistence::paths::logs_dir().join(format!("hyperpanes-{role}.log"))
}

/// Install the global subscriber for this process. `role` names the log file;
/// `default_level` is the persisted `logLevel` setting (any unknown value means `info`).
/// A second call is a no-op (tests and embedded callers may race), and any failure to open
/// the file degrades to stderr-only rather than aborting the process.
#[tracing::instrument(level = "debug", ret)]
pub fn init(role: &str, default_level: &str) {
    let directives = resolve_directives(default_level);
    let filter = EnvFilter::try_new(&directives)
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LEVEL));
    let file_writer = RotatingWriter::new(log_path(role), MAX_LOG_BYTES, KEEP_ROTATED);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_writer(file_writer);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .without_time()
        .with_writer(io::stderr)
        .with_filter(tracing_subscriber::filter::LevelFilter::WARN);
    use tracing_subscriber::Layer;
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();
    if installed {
        tracing::info!(
            role,
            level = %directives,
            version = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
            "logging started"
        );
    }
}

/// A `Write` that appends to one file and rolls it over by size.
///
/// Rolling renames `file` → `file.1` (after shifting `.1` → `.2` … and dropping the
/// oldest), then reopens `file` empty. The size is tracked in-process from the opened
/// length, so a file another process is also appending to may overshoot by that
/// process's share — a bounded, accepted imprecision for per-role files.
#[derive(Clone)]
pub struct RotatingWriter {
    inner: Arc<Mutex<Rotating>>,
}

struct Rotating {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    written: u64,
    /// Set after the first open failure so a missing directory doesn't spam stderr per line.
    failed: bool,
}

impl RotatingWriter {
    /// A writer for `path`, rolling at `max_bytes` and keeping `keep` rolled files.
    #[tracing::instrument(level = "debug")]
    pub fn new(path: PathBuf, max_bytes: u64, keep: usize) -> Self {
        RotatingWriter {
            inner: Arc::new(Mutex::new(Rotating {
                path,
                max_bytes,
                keep,
                file: None,
                written: 0,
                failed: false,
            })),
        }
    }

    /// The live file's path.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn path(&self) -> PathBuf {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).path.clone()
    }
}

impl Rotating {
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn open(&mut self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(f);
        Ok(())
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn rotate(&mut self) {
        self.file = None;
        rotate_files(&self.path, self.keep);
        self.written = 0;
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            if let Err(e) = self.open() {
                if !self.failed {
                    self.failed = true;
                    let _ = writeln!(
                        io::stderr(),
                        "hyperpanes: cannot open log file {}: {e}",
                        self.path.display()
                    );
                }
                return Err(e);
            }
        }
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate();
            self.open()?;
        }
        let f = self.file.as_mut().expect("opened above");
        f.write_all(buf)?;
        self.written += buf.len() as u64;
        Ok(())
    }
}

/// Shift `path.{keep-1}` → `path.{keep}` … `path` → `path.1`, dropping the oldest.
#[tracing::instrument(level = "debug", ret)]
fn rotate_files(path: &Path, keep: usize) {
    if keep == 0 {
        let _ = std::fs::remove_file(path);
        return;
    }
    let numbered = |n: usize| -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    };
    let _ = std::fs::remove_file(numbered(keep));
    for n in (1..keep).rev() {
        let _ = std::fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

/// One `Write` handle onto a [`RotatingWriter`]; each call locks for its duration.
pub struct RotatingHandle {
    inner: Arc<Mutex<Rotating>>,
}

impl Write for RotatingHandle {
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.write_all(buf)?;
        Ok(buf.len())
    }
    #[tracing::instrument(level = "debug", ret, skip_all)]
    fn flush(&mut self) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match g.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for RotatingWriter {
    type Writer = RotatingHandle;
    fn make_writer(&'a self) -> Self::Writer {
        RotatingHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp-logging-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("hyperpanes-test.log")
    }

    #[test]
    fn env_log_wins_over_debug_and_setting() {
        assert_eq!(resolve_from(Some("trace"), true, "warn"), "trace");
        assert_eq!(resolve_from(Some("  "), true, "warn"), "debug");
        assert_eq!(resolve_from(None, true, "warn"), "debug");
        assert_eq!(resolve_from(None, false, "WARN"), "warn");
        assert_eq!(resolve_from(None, false, "loud"), "info");
        assert_eq!(resolve_from(None, false, ""), "info");
    }

    #[test]
    fn valid_levels() {
        for l in LEVELS {
            assert!(valid_level(l));
        }
        assert!(valid_level("Info"));
        assert!(!valid_level("verbose"));
    }

    #[test]
    fn rolls_over_by_size_and_keeps_n() {
        let path = tmp("roll");
        let w = RotatingWriter::new(path.clone(), 100, 2);
        let mut h = w.make_writer();
        // 40-byte lines: 2 fit, the 3rd rolls, etc. 10 lines → several rolls.
        for i in 0..10 {
            let line = format!("{i:0>38}\n");
            h.write_all(line.as_bytes()).unwrap();
        }
        h.flush().unwrap();
        let live = std::fs::metadata(&path).unwrap().len();
        assert!(live <= 100, "live file {live} bytes exceeds the cap");
        let n1 = PathBuf::from(format!("{}.1", path.display()));
        let n2 = PathBuf::from(format!("{}.2", path.display()));
        let n3 = PathBuf::from(format!("{}.3", path.display()));
        assert!(n1.exists(), ".1 should exist");
        assert!(n2.exists(), ".2 should exist");
        assert!(!n3.exists(), ".3 must have been dropped (keep=2)");
        // Every rolled file is also under the cap.
        for p in [&n1, &n2] {
            assert!(std::fs::metadata(p).unwrap().len() <= 100);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn one_oversized_write_still_lands() {
        let path = tmp("big");
        let w = RotatingWriter::new(path.clone(), 10, 1);
        let mut h = w.make_writer();
        h.write_all(b"this line is longer than ten bytes\n").unwrap();
        h.write_all(b"second\n").unwrap();
        let live = std::fs::read_to_string(&path).unwrap();
        assert_eq!(live, "second\n");
        let n1 = std::fs::read_to_string(format!("{}.1", path.display())).unwrap();
        assert!(n1.starts_with("this line"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn resumes_size_from_an_existing_file() {
        let path = tmp("resume");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; 95]).unwrap();
        let w = RotatingWriter::new(path.clone(), 100, 1);
        let mut h = w.make_writer();
        h.write_all(b"0123456789\n").unwrap(); // 95 + 11 > 100 → rolls first
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0123456789\n");
        assert_eq!(std::fs::metadata(format!("{}.1", path.display())).unwrap().len(), 95);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn log_path_is_per_role_under_logs_dir() {
        let p = log_path("daemon");
        assert_eq!(p.file_name().unwrap(), "hyperpanes-daemon.log");
        assert_eq!(p.parent().unwrap(), crate::persistence::paths::logs_dir());
    }
}
