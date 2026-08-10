//! Retry helper (`PLAN.md` §7): at most `max_attempts` tries with a backoff schedule that
//! defaults to `1s → 4s → 16s` (jittered), applied to every HTTP call the export/import
//! clients make.
//!
//! Deliberately has **no dependency on `crate::immich`** — it only needs `E: Retryable`,
//! so `immich::ApiError` (task 3, `src/immich/mod.rs`) implements the trait rather than
//! this module knowing anything about HTTP, status codes, or reqwest. That keeps the
//! module testable with a trivial local error type (see the tests below) instead of
//! standing up real HTTP machinery, and avoids `retry.rs` importing from `immich` while
//! `immich::mod`'s `send_json` wrapper imports `retry` — both directions are fine within
//! one crate, but keeping `retry.rs` generic is simply better factoring.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use crate::warn;

/// Whether — and how — an error should be retried. Implemented by [`crate::immich::ApiError`]
/// (transport errors, HTTP status, a sanitised `Retry-After`); anything satisfying this
/// trait can be driven through [`retry`].
pub trait Retryable {
    /// `true` for reqwest connection errors, timeouts, HTTP `429`, and `5xx`. `false` for
    /// every other `4xx` (permanent for that request — retrying an identical request
    /// against, say, a 404 or a malformed-body 400 can't ever succeed) and for anything
    /// else (a JSON decode failure, a client builder error, …).
    fn is_retryable(&self) -> bool;

    /// A server-provided delay override for the *next* attempt — e.g. a `Retry-After` on
    /// a `429`, already parsed and capped to something sane by the implementor. `None`
    /// (the default) falls back to the policy's own backoff schedule.
    fn retry_after(&self) -> Option<Duration> {
        None
    }
}

/// The retry schedule. `max_attempts` counts the *first* try plus every retry (so
/// `max_attempts = 3` means "try, maybe retry, maybe retry once more" — three calls to
/// the operation at most). `backoff[i]` (jittered by [`jittered`]) is the delay before the
/// `(i + 2)`th attempt — i.e. `backoff[0]` is the wait after the 1st attempt fails,
/// `backoff[1]` after the 2nd, and so on; if there are more attempts than schedule
/// entries, the schedule's last entry repeats.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: Vec<Duration>,
}

impl Default for RetryPolicy {
    /// `PLAN.md` §7: at most 3 attempts, backoff 1s → 4s → 16s (jittered). The third
    /// backoff entry (16s) is part of the documented schedule even though, at the default
    /// `max_attempts = 3`, only the first two entries are ever consulted (there is no 4th
    /// attempt to delay before) — kept so a caller that bumps `max_attempts` up still gets
    /// the plan's exact progression instead of the schedule running out early.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: vec![
                Duration::from_secs(1),
                Duration::from_secs(4),
                Duration::from_secs(16),
            ],
        }
    }
}

impl RetryPolicy {
    /// Same attempt count as [`RetryPolicy::default`] but every backoff entry is zero, so
    /// [`retry`] never actually sleeps. For tests that want to exercise the retry
    /// *decision* logic (attempt counting, retryable-vs-not, exhaustion) without paying
    /// real wall-clock time.
    pub fn zero_delay() -> Self {
        Self {
            max_attempts: 3,
            backoff: vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
        }
    }

    fn delay_before_attempt(&self, attempt_index: usize) -> Duration {
        self.backoff
            .get(attempt_index)
            .or_else(|| self.backoff.last())
            .copied()
            .unwrap_or(Duration::ZERO)
    }
}

/// Applies "equal jitter" to `base`: half the delay is fixed, half is a random amount in
/// `[0, base/2]`. A zero `base` always yields exactly zero (so [`RetryPolicy::zero_delay`]
/// truly never sleeps, without `retry` needing a separate "jitter off" switch), while a
/// non-zero `base` still guarantees at least half of the intended wait — avoiding both a
/// thundering herd (full jitter down to zero would let many clients retry
/// simultaneously) and a too-aggressive minimum wait.
fn jittered(base: Duration) -> Duration {
    if base.is_zero() {
        return base;
    }
    let half_ms = millis_u64(base) / 2;
    let extra_ms = rand::random_range(0..=half_ms);
    Duration::from_millis(half_ms + extra_ms)
}

/// `Duration::as_millis()` returns `u128`; every duration this module ever handles (a
/// backoff delay measured in seconds) fits comfortably in a `u64` millisecond count, but
/// clippy's pedantic `cast_possible_truncation` lint doesn't know that — `unwrap_or(u64::MAX)`
/// is a saturating fallback for the astronomically large durations that can't occur here in
/// practice, rather than a panic.
fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Runs `op` — a *factory* for the actual attempt, called once per try — up to
/// `policy.max_attempts` times, retrying only when the error's [`Retryable::is_retryable`]
/// says so. `op` is `FnMut() -> Fut` rather than a single `Future` because a failed HTTP
/// attempt generally can't be replayed from the same `Future` (the request may have a
/// streaming body, and the underlying connection is gone anyway) — callers reconstruct
/// the request inside the closure each time it's invoked.
///
/// Every retry logs at `warn` with the attempt number, `op_name`, and the cause
/// (`PLAN.md` §7). On exhaustion, returns the last error unchanged.
pub async fn retry<T, E, F, Fut>(policy: &RetryPolicy, op_name: &str, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Retryable + fmt::Display,
{
    let mut attempt: u32 = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt >= policy.max_attempts || !err.is_retryable() {
                    return Err(err);
                }
                let delay = err.retry_after().unwrap_or_else(|| {
                    jittered(policy.delay_before_attempt((attempt - 1) as usize))
                });
                warn!(
                    "retrying after failure operation={op_name} attempt={attempt}/{} \
                     delay_ms={} cause={err}",
                    policy.max_attempts,
                    millis_u64(delay),
                );
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug)]
    struct TestError {
        retryable: bool,
        message: &'static str,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl Retryable for TestError {
        fn is_retryable(&self) -> bool {
            self.retryable
        }
    }

    #[tokio::test]
    async fn succeeds_first_try() {
        let calls = AtomicU32::new(0);
        let policy = RetryPolicy::zero_delay();
        let result: Result<u32, TestError> = retry(&policy, "test-op", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(42) }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn succeeds_on_third_try() {
        let calls = AtomicU32::new(0);
        let policy = RetryPolicy::zero_delay();
        let result: Result<u32, TestError> = retry(&policy, "test-op", || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(TestError {
                        retryable: true,
                        message: "transient",
                    })
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn exhausts_attempts_and_returns_the_last_error() {
        let calls = AtomicU32::new(0);
        let policy = RetryPolicy::zero_delay();
        let result: Result<u32, TestError> = retry(&policy, "test-op", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(TestError {
                    retryable: true,
                    message: "still failing",
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().message, "still failing");
        // max_attempts = 3: the initial try plus two retries, never a fourth call.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_retryable_error_returns_immediately_without_retrying() {
        let calls = AtomicU32::new(0);
        let policy = RetryPolicy::zero_delay();
        let result: Result<u32, TestError> = retry(&policy, "test-op", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(TestError {
                    retryable: false,
                    message: "permanent",
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_single_max_attempt_never_retries_even_if_retryable() {
        let calls = AtomicU32::new(0);
        let policy = RetryPolicy {
            max_attempts: 1,
            backoff: vec![],
        };
        let result: Result<u32, TestError> = retry(&policy, "test-op", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(TestError {
                    retryable: true,
                    message: "would retry if allowed",
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn jittered_zero_base_is_always_zero() {
        for _ in 0..20 {
            assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
        }
    }

    #[test]
    fn jittered_stays_within_half_to_full_base() {
        let base = Duration::from_secs(4);
        for _ in 0..200 {
            let d = jittered(base);
            assert!(d >= base / 2, "delay {d:?} below half of base {base:?}");
            assert!(d <= base, "delay {d:?} above base {base:?}");
        }
    }

    #[test]
    fn delay_before_attempt_repeats_last_entry_past_schedule_length() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay_before_attempt(0), Duration::from_secs(1));
        assert_eq!(policy.delay_before_attempt(1), Duration::from_secs(4));
        assert_eq!(policy.delay_before_attempt(2), Duration::from_secs(16));
        // Past the schedule's length: repeats the last entry rather than panicking or
        // silently falling back to zero.
        assert_eq!(policy.delay_before_attempt(5), Duration::from_secs(16));
    }
}
