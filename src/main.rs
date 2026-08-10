//! `immich-federation-at-home` — mirror a shared Immich album into a local album.
//!
//! A thin wiring layer over the `immich_federation_at_home` library crate (`src/lib.rs`):
//! parse config, validate it, set the log level, run the `PLAN.md` §5 startup sequence
//! (`startup::run_startup`), then hand the resulting [`sync::SyncContext`] to the scheduler
//! (`scheduler::run`) until it's time to exit. Every piece with real logic to test lives in
//! the library (`src/startup.rs`, `src/scheduler.rs`) — this file itself is deliberately not
//! unit tested, since it's mostly process-global side effects (real argv/env, the
//! process-wide log threshold, real OS signals, the real process exit code) that don't have
//! a meaningful in-process test.

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};

use immich_federation_at_home::config::Config;
use immich_federation_at_home::scheduler::{self, ShutdownSignal};
use immich_federation_at_home::{error, format_error_chain, info, log, startup, warn};

#[tokio::main]
async fn main() -> ExitCode {
    // PLAN.md §5 step 1: parse config, then its semantic validation (clap's derive already
    // enforces types; `Config::validate` covers the rest — a non-empty API key, non-zero
    // interval/timeouts/concurrency). The configured log level isn't in effect yet at this
    // point, so a failure here is reported the same way clap's own parse errors already are:
    // straight to stderr, no log formatting.
    let config = Config::parse();
    if let Err(err) = config.validate() {
        eprintln!("Error: {err}");
        return ExitCode::FAILURE;
    }

    // PLAN.md §5 step 2.
    log::set_level(config.log_level);

    // PLAN.md §5 steps 3-10. Sample output (both success and failure) is in this task's
    // report; every failure here is actionable prose naming the env var or the remote-side
    // setting to fix, never a bare propagated HTTP error — see `startup.rs`.
    let outcome = match startup::run_startup(&config).await {
        Ok(outcome) => outcome,
        Err(err) => {
            error!("{}", format_error_chain(&err));
            return ExitCode::FAILURE;
        }
    };

    // PLAN.md §9: the scheduler, plus graceful shutdown wiring. `ShutdownSignal` is shared
    // between the loop below and `watch_for_shutdown_signals`, spawned as its own task so it
    // keeps listening for OS signals no matter what the scheduler loop is doing at the time.
    let shutdown = Arc::new(ShutdownSignal::new());
    tokio::spawn(watch_for_shutdown_signals(shutdown.clone()));

    let sync = outcome.sync;
    let perform_run = || async { sync.run_once().await.map(|_summary| ()) };

    match scheduler::run(
        config.import_interval,
        config.run_once,
        shutdown.as_ref(),
        perform_run,
    )
    .await
    {
        scheduler::Outcome::RanOnceOk | scheduler::Outcome::ShutdownRequested => ExitCode::SUCCESS,
        scheduler::Outcome::RanOnceFailed => ExitCode::FAILURE,
    }
}

/// Watches for SIGINT/SIGTERM and turns the first one into a graceful
/// [`ShutdownSignal::trigger`] — see `scheduler.rs`'s own top-level doc comment for exactly
/// what "graceful" means in this codebase: the current sync run (if any) is always allowed
/// to finish — never interrupted mid-asset — but no new run starts afterward, and a pending
/// sleep between runs is cut short immediately rather than waiting out the rest of
/// `IMPORT_INTERVAL`.
///
/// A **second** signal is deliberately *not* routed through that graceful path at all: it
/// calls [`std::process::exit`] directly, immediately, from wherever this task happens to be
/// running — the entire point of sending a second signal is "I don't want to wait for the
/// graceful path any longer". `130` is the conventional shell exit code for "killed by
/// SIGINT" (`128 + 2`); used here for either signal rather than tracking which of the two
/// was actually received a second time, since the distinction doesn't matter once the
/// decision is "exit right now".
///
/// Unix-only (`tokio::signal::unix`) with no `cfg` split for other platforms: every target
/// this project actually ships for is Unix — `flake.nix`'s `systems` list is Linux/macOS
/// only, and the Docker image is Linux-only — so there is no Windows target to support here.
async fn watch_for_shutdown_signals(shutdown: Arc<ShutdownSignal>) {
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install a SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install a SIGINT handler");

    loop {
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }

        if shutdown.is_triggered() {
            warn!(
                "second shutdown signal received; forcing an immediate exit without waiting \
                 for the in-flight sync run to finish"
            );
            std::process::exit(130);
        }

        info!(
            "shutdown signal received; finishing the in-flight sync run, then exiting (send \
             another signal to force an immediate exit instead)"
        );
        shutdown.trigger();
    }
}
