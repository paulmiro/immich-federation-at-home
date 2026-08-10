//! The whole logging implementation: a level, one global threshold, and five macros.
//!
//! `PLAN.md` §8 asks for level-filtered, single-line, human-readable log lines on stderr and
//! nothing else — no spans, no per-target filtering, no JSON, no subscriber registry. That
//! is a small enough job to own outright, so this module owns it rather than pulling in
//! `tracing` + `tracing-subscriber`: everything the program's logging does is the ~40 lines
//! below, readable end to end by anyone who knows basic Rust.
//!
//! Usage is `println!`-shaped rather than structured — `info!("transferred asset
//! filename={filename} bytes={bytes}")` — so a log line's source reads exactly like the line
//! it produces:
//!
//! ```text
//! 2026-08-10T14:22:07.318Z  INFO transferred asset filename=IMG_4312.HEIC bytes=4184233
//! ```
//!
//! [`set_level`] is called once from `main.rs` after config parsing. Until then the threshold
//! is [`Level::Info`], which is also what tests get — they never call `set_level`, so a test
//! binary logs at `info` to the captured stderr of whichever test emitted it.

use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

use clap::ValueEnum;

/// Log verbosity, ordered least to most verbose. The derived [`Ord`] is what
/// [`enabled`] filters on: an event is emitted when its level is `<=` the threshold.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    /// The fixed-width tag used in the output, padded so messages line up in a terminal.
    const fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => " WARN",
            Level::Info => " INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

impl fmt::Display for Level {
    /// Lowercase, matching `LOG_LEVEL`'s accepted spellings — this is what `clap` prints as
    /// the default in `--help`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        })
    }
}

/// The process-wide verbosity threshold, as a [`Level`] cast to `u8`.
static THRESHOLD: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Sets the process-wide verbosity threshold. Called once from `main.rs`; safe to call at any
/// time from any thread, and a concurrent [`enabled`] check simply sees the old or the new
/// value.
pub fn set_level(level: Level) {
    THRESHOLD.store(level as u8, Ordering::Relaxed);
}

/// Whether `level` is currently being logged. The macros call this themselves, so a call site
/// only needs it directly when *building* an argument is expensive enough to want skipping —
/// see `immich/mod.rs`'s body redaction at `trace`.
pub fn enabled(level: Level) -> bool {
    (level as u8) <= THRESHOLD.load(Ordering::Relaxed)
}

/// Writes one log line to stderr. Prefer the macros; this is their implementation, public
/// only because they expand to a call to it.
///
/// A failed write is ignored on purpose: there is nowhere useful to report "I could not
/// write a log line" to, and a closed stderr must not take the program down mid-sync.
pub fn emit(level: Level, args: fmt::Arguments<'_>) {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{timestamp} {} {args}", level.tag());
}

/// Shared expansion for the five level macros below: check the threshold first so a
/// suppressed line costs one atomic load and never evaluates its arguments.
#[macro_export]
macro_rules! log_at {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            $crate::log::emit($level, format_args!($($arg)*));
        }
    };
}

/// Logs at [`Level::Error`] — something went wrong that the operator has to act on.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, $($arg)*) };
}

/// Logs at [`Level::Warn`] — something unexpected that the program handled by itself.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, $($arg)*) };
}

/// Logs at [`Level::Info`] — the default level; the per-run narrative from `PLAN.md` §8.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, $($arg)*) };
}

/// Logs at [`Level::Debug`] — per-request detail: method, URL, status, duration.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, $($arg)*) };
}

/// Logs at [`Level::Trace`] — response bodies, with secrets redacted.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Trace, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_from_least_to_most_verbose() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
    }

    /// `set_level` is process-wide, so this is the one test that touches it; it restores the
    /// default afterwards to keep the rest of the test binary logging at `info`.
    #[test]
    fn enabled_admits_everything_at_or_below_the_threshold() {
        set_level(Level::Warn);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Warn));
        assert!(!enabled(Level::Info));
        assert!(!enabled(Level::Trace));
        set_level(Level::Info);
    }
}
