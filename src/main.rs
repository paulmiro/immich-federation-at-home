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

use clap::CommandFactory;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Semaphore;

use anyhow::Context;
use immich_federation_at_home::cache::ContentHashCache;
use immich_federation_at_home::config::{self, Cli, ConfigSource, ConfigText, Settings};
use immich_federation_at_home::scheduler::{self, ShutdownSignal};
use immich_federation_at_home::{error, format_error_chain, info, log, startup, warn};

/// The real `std::env::var`, wrapped to fit [`config::load`]'s injected-lookup signature —
/// every test instead supplies a closure over a plain `HashMap`, since mutating the real
/// environment is unsound to do from a test (`std::env::set_var` is `unsafe` under edition
/// 2024, and this crate `forbid`s `unsafe_code` outright regardless).
fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `--config`/`CONFIG_FILE`/`CONFIG`: resolved without touching the filesystem, then (for a
/// path) read here — `config::load` itself never does disk I/O for the main config source,
/// only for `_file` secrets, so every other rule it applies is unit-testable against literal
/// TOML strings (see `config.rs`'s own tests). Pulled out of `main` just to keep that
/// function's line count down; the error-reporting shape (`Result` in, `eprintln!` +
/// `ExitCode::FAILURE` out) is identical to every other startup failure in `main`.
fn resolve_settings(matches: &clap::ArgMatches) -> anyhow::Result<Settings> {
    let env: &dyn Fn(&str) -> Option<String> = &real_env;
    let source = config::resolve_config_source(matches, env)?;
    let config_text = match &source {
        None => None,
        Some(ConfigSource::Inline(text)) => Some(text.clone()),
        Some(ConfigSource::Path(path)) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("could not read config file {}", path.display()))?,
        ),
    };
    let config = config_text.as_deref().map(|toml| ConfigText {
        toml,
        path: match &source {
            Some(ConfigSource::Path(path)) => Some(path.as_path()),
            _ => None,
        },
    });
    config::load(matches, env, config)
}

#[tokio::main]
async fn main() -> ExitCode {
    // Bad argv still gets clap's own formatted error and exit code, exactly as
    // `Cli::parse()` would have given — `try_get_matches_from` + `.exit()` is what lets us
    // keep the raw `ArgMatches` below (needed for `value_source`, see `config::load`)
    // instead of just a parsed `Cli`.
    let matches = match Cli::command().try_get_matches_from(std::env::args_os()) {
        Ok(matches) => matches,
        Err(err) => err.exit(),
    };

    // Config parsing, precedence, inheritance, secrets, and validation all happen inside
    // `load` — see `config.rs`. The configured log level isn't in effect yet at this point,
    // so a failure here is reported the same way clap's own parse errors already are:
    // straight to stderr, no log formatting.
    let settings = match resolve_settings(&matches) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    };
    log::set_level(settings.globals.log_level);

    // The content-hash cache (`scratch/CACHE-DESIGN.md`) and the process-wide transfer
    // semaphore (`scratch/JOBS-DESIGN.md`'s "Global transfer cap") are both process-level
    // resources, opened once here rather than inside `run_startup`, so a future multi-job
    // `main.rs` can share one of each across every job's `SyncContext` instead of building a
    // cache or a cap per job. Built before `run_startup` so a misconfigured `CACHE_DIR` is a
    // loud, immediate startup failure rather than a warning discovered only once the first
    // run tries to save the cache: this is always user error (typically a root-owned bind
    // mount) and must be caught here.
    let cache = match &settings.globals.cache_dir {
        Some(dir) => match ContentHashCache::open(dir) {
            Ok(cache) => cache,
            Err(err) => {
                let err = err.context(format!(
                    "CACHE_DIR={} could not be created or written. If you are running the \
                     container image, it runs as uid 65532, so a bind-mounted host directory \
                     must be owned by that uid (chown 65532:65532 on the host) — a named \
                     Docker volume avoids the problem entirely and is what the README \
                     recommends. Unset CACHE_DIR to run without a cache instead.",
                    dir.display()
                ));
                error!("{}", format_error_chain(&err));
                return ExitCode::FAILURE;
            }
        },
        None => ContentHashCache::disabled(),
    };
    let cache = Arc::new(cache);
    // `Globals::validate` (inside `load`, above) already rejects 0; `.max(1)` is cheap
    // insurance against a `Semaphore::new(0)` that would never let any transfer through.
    let transfers = Arc::new(Semaphore::new(
        usize::try_from(settings.globals.transfer_concurrency)
            .unwrap_or(usize::MAX)
            .max(1),
    ));

    // TODO(runtime-agent): this whole single-job branch is a deliberate, temporary bridge —
    // `scratch/JOBS-DESIGN.md`'s "one task per job" scheduling isn't wired up yet, so more
    // than one configured job can't run at all yet. Delete this check once `main.rs` spawns
    // one task per job into a `JoinSet` instead.
    let job = match settings.jobs.as_slice() {
        [job] => job,
        jobs => {
            error!(
                "{} jobs are configured, but this build only runs a single job; per-job \
                 scheduling is not wired up yet",
                jobs.len()
            );
            return ExitCode::FAILURE;
        }
    };

    // PLAN.md §5 steps 3-10. Sample output (both success and failure) is in this task's
    // report; every failure here is actionable prose naming the env var or the remote-side
    // setting to fix, never a bare propagated HTTP error — see `startup.rs`.
    let outcome = match startup::run_startup(
        job,
        cache,
        transfers,
        settings.globals.transfer_concurrency,
        settings.globals.cache_dir.as_deref(),
    )
    .await
    {
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
        job.interval,
        settings.run_once,
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
/// sleep between runs is cut short immediately rather than waiting out the rest of the job's
/// own `interval`.
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
