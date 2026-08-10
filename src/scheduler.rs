//! The sync loop (`PLAN.md` §9/§12 task 9): an immediate first run, then `IMPORT_INTERVAL`
//! **between** runs (never a fixed-rate tick, so a slow run can't make the next one start
//! early or overlap it), `RUN_ONCE` support, and cooperative graceful shutdown.
//!
//! **Honesty about shutdown granularity.** `PLAN.md` §12 task 9's brief describes graceful
//! shutdown as finishing "the in-flight asset". This module cannot deliver that: the unit of
//! work it can see is one whole [`SyncContext::run_once`](crate::sync::SyncContext::run_once)
//! call, which has no cancellation hook into its own per-asset loop (adding one would mean
//! touching `sync.rs`, out of this task's scope — see `NOTES.md`). What [`run`] actually
//! guarantees is: an in-flight *run* is never cancelled or interrupted, but no *new* run is
//! started, and a pending sleep between runs is cut short immediately, once a shutdown has
//! been requested. That's "finishes the current run", not "finishes the current asset" — a
//! real distinction the task brief itself explicitly allows substituting when the finer
//! granularity isn't achievable, as long as it's stated plainly, which this doc comment
//! (and `main.rs`'s corresponding one) is doing.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

use crate::{error, info};

use crate::format_error_chain;

/// What [`run`] returned, so the caller (`main.rs`) can pick a process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `RUN_ONCE`: the single pass succeeded.
    RanOnceOk,
    /// `RUN_ONCE`: the single pass failed (already logged by [`run`] itself).
    RanOnceFailed,
    /// Interval mode: a shutdown was requested and honoured — either right after a run
    /// finished, or while sleeping between runs.
    ShutdownRequested,
}

/// Shared shutdown state between [`run`]'s loop and whatever's watching for SIGINT/SIGTERM
/// (`main.rs`, not tested here — see this module's own top-level doc comment for why real
/// signal handling stays out of the library).
///
/// Two pieces of state serve two different needs: [`Self::is_triggered`] is a synchronous,
/// non-blocking check (used right after a run finishes, before deciding whether to sleep or
/// exit), while [`Self::wait`] is an async wait woken immediately by [`Self::trigger`] (used
/// to cut the between-runs sleep short instead of polling). Checking the flag first inside
/// [`Self::wait`] — rather than relying solely on [`Notify`]'s own "buffered wakeup" — means
/// it's safe to call `wait` even after `trigger` already happened.
pub struct ShutdownSignal {
    triggered: AtomicBool,
    notify: Notify,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            triggered: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// Marks the signal triggered and wakes anyone currently in [`Self::wait`]. Idempotent:
    /// a second call is a harmless no-op as far as this type's own state is concerned (it's
    /// `main.rs`'s job to treat a *second* incoming OS signal specially — an immediate
    /// process exit — which has nothing to do with this type).
    pub fn trigger(&self) {
        self.triggered.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }

    /// Resolves as soon as [`Self::trigger`] has been (or is concurrently being) called.
    async fn wait(&self) {
        if self.is_triggered() {
            return;
        }
        self.notify.notified().await;
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Drives the sync loop. `perform_run` is a *factory* (`FnMut() -> Fut`, called once per pass)
/// rather than a plain `Future`, mirroring every other retried/repeated operation in this
/// crate (`retry::retry`, `immich::send_json`) — the same call can't be replayed, so each
/// tick needs its own `Future`.
///
/// * `RUN_ONCE` (`run_once: true`): runs exactly once and returns immediately, regardless of
///   `shutdown` — there's no second tick to skip.
/// * Interval mode: after each run, a failure is logged at `error` (`PLAN.md` §6: a failing
///   run must not kill the process) and the loop continues — sleeping `interval` before the
///   next run, *between* runs rather than at a fixed rate, so a slow run can never overlap
///   the next. `shutdown` is checked synchronously right after a run completes (so a signal
///   received *during* a run doesn't get followed by a pointless full sleep-then-run) and
///   raced against the sleep itself (so a signal received *while sleeping* ends the process
///   immediately rather than waiting out the rest of `interval`). It is never raced against
///   `perform_run` itself — see this module's top-level doc comment for why.
pub async fn run<R, RFut>(
    interval: Duration,
    run_once: bool,
    shutdown: &ShutdownSignal,
    mut perform_run: R,
) -> Outcome
where
    R: FnMut() -> RFut,
    RFut: Future<Output = anyhow::Result<()>>,
{
    loop {
        let result = perform_run().await;
        if let Err(err) = &result {
            let cause = format_error_chain(err);
            if run_once {
                error!("sync run failed: {cause}");
            } else {
                error!("sync run failed; will try again at the next tick: {cause}");
            }
        }

        if run_once {
            return if result.is_ok() {
                Outcome::RanOnceOk
            } else {
                Outcome::RanOnceFailed
            };
        }

        if shutdown.is_triggered() {
            info!("shutdown requested; not starting another run");
            return Outcome::ShutdownRequested;
        }

        info!(
            "sleeping until the next run interval={}",
            humantime::format_duration(interval)
        );
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = shutdown.wait() => {
                info!("shutdown requested while waiting for the next run; exiting now");
                return Outcome::ShutdownRequested;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    // ---- RUN_ONCE ---------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn run_once_executes_exactly_once_and_reports_success() {
        let calls = AtomicU32::new(0);
        let shutdown = ShutdownSignal::new();

        let outcome = run(Duration::from_secs(3600), true, &shutdown, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await;

        assert_eq!(outcome, Outcome::RanOnceOk);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn run_once_reports_failure_without_retrying() {
        let calls = AtomicU32::new(0);
        let shutdown = ShutdownSignal::new();

        let outcome = run(Duration::from_secs(3600), true, &shutdown, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(anyhow::anyhow!("boom")) }
        })
        .await;

        assert_eq!(outcome, Outcome::RanOnceFailed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // ---- interval mode: sleep between runs, not a fixed-rate tick -------------------------

    #[tokio::test(start_paused = true)]
    async fn interval_mode_sleeps_between_runs_before_the_second_one() {
        let calls = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(ShutdownSignal::new());
        let calls_task = calls.clone();
        let shutdown_task = shutdown.clone();

        let handle = tokio::spawn(async move {
            run(Duration::from_secs(60), false, &shutdown_task, || {
                calls_task.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .await
        });

        // The first run happens immediately, no sleep needed to see it.
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Not yet a second run before the interval elapses. `sleep` (not `advance`) is
        // deliberate: tokio's own docs warn that `advance` only jumps the clock and yields
        // once, without guaranteeing every timer past the new deadline has actually been
        // polled — `sleep` relies on paused-time auto-advance instead, which *does*
        // guarantee every timer up to and including this one's deadline has fired, in order.
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The second run fires once the interval has fully elapsed.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        shutdown.trigger();
        let outcome = handle.await.unwrap();
        assert_eq!(outcome, Outcome::ShutdownRequested);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_the_sleep_exits_immediately_not_after_the_full_interval() {
        let calls = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(ShutdownSignal::new());
        let calls_task = calls.clone();
        let shutdown_task = shutdown.clone();

        let handle = tokio::spawn(async move {
            run(Duration::from_secs(3600), false, &shutdown_task, || {
                calls_task.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first run must have started"
        );

        // Trigger shutdown while the loop is sleeping, nowhere near the hour-long interval.
        shutdown.trigger();

        // Bounded by *real* wall-clock time (not virtual/paused time): if this hangs, the
        // scheduler is incorrectly waiting out the full paused interval instead of reacting
        // to the shutdown signal immediately.
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("must exit almost immediately, not after the full interval")
            .unwrap();

        assert_eq!(outcome, Outcome::ShutdownRequested);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must not start a second run"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_requested_right_after_a_run_skips_the_sleep_entirely() {
        let calls = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(ShutdownSignal::new());
        let calls_task = calls.clone();
        let shutdown_task = shutdown.clone();

        // Trigger before the loop even starts: the very first run still happens (RUN_ONCE
        // semantics don't apply here — interval mode always runs at least once), but the
        // loop must exit right after it instead of sleeping.
        shutdown.trigger();

        let handle = tokio::spawn(async move {
            run(Duration::from_secs(3600), false, &shutdown_task, || {
                calls_task.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .await
        });

        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("must not sleep at all once already shut down")
            .unwrap();

        assert_eq!(outcome, Outcome::ShutdownRequested);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // ---- a failing run does not stop the loop ------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn failing_run_does_not_stop_the_loop() {
        let calls = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(ShutdownSignal::new());
        let calls_task = calls.clone();
        let shutdown_task = shutdown.clone();

        let handle = tokio::spawn(async move {
            run(Duration::from_secs(10), false, &shutdown_task, || {
                let attempt = calls_task.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(anyhow::anyhow!("transient failure"))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // `sleep`, not `advance` — see the sibling test above for why.
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a failed run must still be followed by another tick, not kill the loop"
        );

        shutdown.trigger();
        let outcome = handle.await.unwrap();
        assert_eq!(outcome, Outcome::ShutdownRequested);
    }

    // ---- ShutdownSignal in isolation -----------------------------------------------------

    #[tokio::test]
    async fn wait_returns_immediately_if_already_triggered() {
        let shutdown = ShutdownSignal::new();
        shutdown.trigger();
        tokio::time::timeout(Duration::from_secs(1), shutdown.wait())
            .await
            .expect("wait() must not block once already triggered");
    }

    #[tokio::test]
    async fn wait_wakes_up_once_triggered_concurrently() {
        let shutdown = Arc::new(ShutdownSignal::new());
        let shutdown2 = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            shutdown2.trigger();
        });
        tokio::time::timeout(Duration::from_secs(5), shutdown.wait())
            .await
            .expect("wait() must wake up once trigger() is called");
    }
}
