//! Append-only diagnostic log used by the IDE and debugger to trace
//! lifecycle transitions (file open/close, debug start/stop, etc.).
//!
//! Disabled by default. Set `BRUTO_IDE_LOG=1` (or any non-empty value) to
//! enable; the log is written to `/tmp/bruto-ide-trace.log` (override with
//! `BRUTO_IDE_LOG_PATH=/some/path`). Each call to `init_for_session()`
//! truncates the file so a single launch produces a clean trace.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_PATH: &str = "/tmp/bruto-ide-trace.log";

struct LogState {
    enabled: bool,
    path: String,
    started_at: SystemTime,
}

static STATE: OnceLock<LogState> = OnceLock::new();

fn state() -> &'static LogState {
    STATE.get_or_init(|| {
        let enabled = std::env::var("BRUTO_IDE_LOG")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        let path = std::env::var("BRUTO_IDE_LOG_PATH").unwrap_or_else(|_| DEFAULT_PATH.into());
        LogState {
            enabled,
            path,
            started_at: SystemTime::now(),
        }
    })
}

/// Truncate the log file and write a banner. Called once on IDE
/// startup so the trace covers exactly one session.
pub fn init_for_session() {
    let s = state();
    if !s.enabled {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&s.path)
    {
        let _ = writeln!(
            f,
            "=== bruto-ide trace start (pid {}) ===",
            std::process::id()
        );
    }
}

pub fn is_enabled() -> bool {
    state().enabled
}

pub fn write_event(args: std::fmt::Arguments<'_>) {
    let s = state();
    if !s.enabled {
        return;
    }
    let elapsed = SystemTime::now()
        .duration_since(s.started_at)
        .unwrap_or_default()
        .as_secs_f64();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.path)
    {
        let _ = writeln!(f, "[{elapsed:8.3}] {args}");
    }
}

/// Write a timestamped line to the trace log. No-op unless `BRUTO_IDE_LOG`
/// is set. Use like `trace!("opened file {}", path.display())`.
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        $crate::trace_log::write_event(format_args!($($arg)*));
    };
}

/// Convenience used by `_ = UNIX_EPOCH;` to silence unused warnings when
/// the module is compiled but not yet referenced from every call site.
#[allow(dead_code)]
fn _force_link() {
    let _ = UNIX_EPOCH;
}
