//! `immich-federation-at-home` — mirror one or more shared Immich albums into local albums.
//!
//! A thin wiring layer over the `immich_federation_at_home` library crate (`src/lib.rs`):
//! parse config, validate it, set the log level, open the process-wide cache and transfer
//! semaphore, then spawn one `tokio` task per job (`scratch/JOBS-DESIGN.md`'s "One task per
//! job") into a `JoinSet`. Each task wraps a [`job::JobRunner`] in [`log::with_job`] and
//! hands it to the scheduler (`scheduler::run`), which lazily runs that job's own remote
//! checks (`startup::run_startup`, via [`job::JobRunner::tick`]) before its first sync run
//! and repeats them only after a failure. Every piece with real logic to test lives in the
//! library (`src/job.rs`, `src/startup.rs`, `src/scheduler.rs`) — this file itself is
//! deliberately not unit tested, since it's mostly process-global side effects (real
//! argv/env, the process-wide log threshold, real OS signals, the real process exit code)
//! that don't have a meaningful in-process test.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::CommandFactory;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use anyhow::Context;
use immich_federation_at_home::cache::ContentHashCache;
use immich_federation_at_home::config::{self, Cli, ConfigSource, ConfigText, Settings};
use immich_federation_at_home::scheduler::{self, ShutdownSignal};
use immich_federation_at_home::{error, format_error_chain, info, job, log, startup, warn};

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

/// Spawns one task per job into a `JoinSet` (`scratch/JOBS-DESIGN.md`'s "One task per job"),
/// each running its own [`scheduler::run`] loop over a [`job::JobRunner`], wrapped in
/// [`log::with_job`] so every line it logs — including its lazy remote checks — carries
/// `job=<name>`. Pulled out of `main` just to keep that function's line count down (mirrors
/// why `resolve_settings` above is its own function).
///
/// Returns the `JoinSet` (still running) plus a receiver of each job's first-attempt result
/// (`true`/`false` for its first tick's `Ok`/`Err`) — `main`'s "except at startup" check
/// consumes that channel right after this returns; see its own comment for why a channel
/// rather than, say, inspecting the `JoinSet` directly (jobs run forever in interval mode, so
/// the `JoinSet` alone can't answer "did the first tick succeed" without also waiting for the
/// job to *finish*, which it never does before shutdown).
fn spawn_jobs(
    jobs: Vec<config::Job>,
    cache: &Arc<ContentHashCache>,
    transfers: &Arc<Semaphore>,
    transfer_concurrency: u32,
    tmp_dir: Option<&PathBuf>,
    run_once: bool,
    shutdown: &Arc<ShutdownSignal>,
) -> (JoinSet<scheduler::Outcome>, mpsc::Receiver<bool>) {
    // Bounded to exactly `jobs.len()`: each job sends at most one message, ever, so the
    // channel can never fill up and a send can never block.
    let (first_attempt_tx, first_attempt_rx) = mpsc::channel::<bool>(jobs.len().max(1));

    let mut join_set = JoinSet::new();
    for job in jobs {
        let name = job.name.clone();
        let interval = job.interval;
        let runner = job::JobRunner::new(
            job,
            Arc::clone(cache),
            Arc::clone(transfers),
            transfer_concurrency,
            tmp_dir.cloned(),
        );
        let shutdown = Arc::clone(shutdown);
        let tx = first_attempt_tx.clone();

        join_set.spawn(log::with_job(name, async move {
            let mut first_attempt = true;
            let perform_run = || {
                let report_first_attempt = std::mem::replace(&mut first_attempt, false);
                let runner = &runner;
                let tx = &tx;
                async move {
                    let result = runner.tick().await;
                    if report_first_attempt {
                        // A send error only means `main` already decided the startup
                        // question (see its own comment) and dropped its receiver — nothing
                        // left to report to at that point.
                        let _ = tx.send(result.is_ok()).await;
                    }
                    result
                }
            };
            scheduler::run(interval, run_once, shutdown.as_ref(), perform_run).await
        }));
    }
    // `spawn_jobs`'s own clone must be dropped for the channel to ever close on its own —
    // every per-job clone above lives for that job's whole task, which in interval mode
    // never ends until shutdown, so without this, closing the channel that way could never
    // happen at all.
    drop(first_attempt_tx);

    (join_set, first_attempt_rx)
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
    // straight to stderr, no log formatting. `format_error_chain` (not bare `{err}`) is what
    // actually surfaces the failure: several of `load`'s own error paths (an unknown TOML
    // key, for one) carry the actionable detail — which key, which job — only in the
    // wrapped-`Context`'s source, not in the top-level message `{err}`'s `Display` alone
    // would print.
    let settings = match resolve_settings(&matches) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("Error: {}", format_error_chain(&err));
            return ExitCode::FAILURE;
        }
    };
    log::set_level(settings.globals.log_level);

    // The content-hash cache (`scratch/CACHE-DESIGN.md`) and the process-wide transfer
    // semaphore (`scratch/JOBS-DESIGN.md`'s "Global transfer cap") are both process-level
    // resources, opened once here — before any job is spawned — and shared as `Arc`s across
    // every job's `SyncContext` (via `job::JobRunner`) rather than built per job. Built
    // before any job starts so a misconfigured `CACHE_DIR` is a loud, immediate startup
    // failure rather than a warning discovered only once the first run tries to save the
    // cache: this is always user error (typically a root-owned bind mount) and must be
    // caught here.
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
    // The cache's own status is process-wide, not any one job's concern (`startup.rs`'s
    // `StartupSummary` no longer carries it) — logged once, here, rather than once per job.
    info!(
        "{}",
        startup::cache_summary(&cache, settings.globals.cache_dir.as_deref())
    );
    // `Globals::validate` (inside `load`, above) already rejects 0; `.max(1)` is cheap
    // insurance against a `Semaphore::new(0)` that would never let any transfer through.
    let transfers = Arc::new(Semaphore::new(
        usize::try_from(settings.globals.transfer_concurrency)
            .unwrap_or(usize::MAX)
            .max(1),
    ));
    let tmp_dir = settings.globals.tmp_dir.clone();

    // `ShutdownSignal` is shared between every job's scheduler loop below and
    // `watch_for_shutdown_signals`, spawned as its own task so it keeps listening for OS
    // signals no matter what any job's loop is doing at the time.
    let shutdown = Arc::new(ShutdownSignal::new());
    tokio::spawn(watch_for_shutdown_signals(shutdown.clone()));

    // `scratch/JOBS-DESIGN.md`'s "One task per job": each job gets its own `JobRunner` (its
    // own `SyncContext`, built lazily on its first tick) and its own scheduler loop
    // honouring its own `interval`, all sharing the one `cache` and `transfers` `Arc` built
    // above — see `spawn_jobs`.
    let job_count = settings.jobs.len();
    let (mut jobs, mut first_attempt_rx) = spawn_jobs(
        settings.jobs,
        &cache,
        &transfers,
        settings.globals.transfer_concurrency,
        tmp_dir.as_ref(),
        settings.run_once,
        &shutdown,
    );

    // "Except at startup" (`scratch/JOBS-DESIGN.md`'s Runtime section): in interval mode, a
    // job failing never takes the process down on its own — *unless* every job's very first
    // attempt failed, in which case running on with zero working jobs isn't useful and this
    // is treated like any other fatal startup failure. `RUN_ONCE` doesn't need this check
    // (its own stricter "any job failed" rule below already covers it), but running it
    // anyway is harmless: it simply waits for every job's one-and-only tick to finish
    // reporting, no differently than the `join_next` loop after it would.
    //
    // The loop can't deadlock: `scheduler::run` always calls `perform_run` at least once,
    // regardless of `shutdown`'s state, so every job reports exactly one message unless its
    // task panics first, in which case the channel simply closes early (`None`) — treated
    // the same as "no success seen yet", not as a hang.
    let mut any_first_attempt_ok = false;
    let mut first_attempts_seen = 0usize;
    while first_attempts_seen < job_count {
        match first_attempt_rx.recv().await {
            Some(true) => {
                any_first_attempt_ok = true;
                break;
            }
            Some(false) => first_attempts_seen += 1,
            None => break,
        }
    }
    if !settings.run_once && !any_first_attempt_ok {
        error!("every job failed its first attempt at startup; exiting");
        return ExitCode::FAILURE;
    }

    // From here on, per-job failures are each job's own problem (logged with its name,
    // retried on its next tick) and never decide the process exit code directly — except
    // that `RUN_ONCE` still exits non-zero if *any* job's single pass failed, which is
    // exactly what every task's returned `Outcome` tells us below.
    let mut any_run_failed = false;
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok(scheduler::Outcome::RanOnceFailed) => any_run_failed = true,
            Ok(scheduler::Outcome::RanOnceOk | scheduler::Outcome::ShutdownRequested) => {}
            Err(join_err) => {
                error!("a job task did not complete cleanly: {join_err}");
                any_run_failed = true;
            }
        }
    }

    if settings.run_once && any_run_failed {
        ExitCode::FAILURE
    } else {
        // Interval mode always ends this way once every job's loop has returned: shutdown
        // was requested and honoured, per `scratch/JOBS-DESIGN.md`'s "shutdown in interval
        // mode is still a success exit" — ongoing per-job failures after startup already
        // didn't change that, above.
        ExitCode::SUCCESS
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
