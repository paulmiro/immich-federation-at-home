//! Per-job runtime state: lazy remote checks plus one sync run, driven repeatedly by
//! [`crate::scheduler::run`] (`scratch/JOBS-DESIGN.md`'s "One task per job" and "Startup
//! moves into the job loop").
//!
//! [`JobRunner::tick`] is the `perform_run` factory `scheduler::run` calls once per pass: on
//! a job's first tick — or any tick after its remote checks last failed — it runs
//! [`startup::run_startup`] and only keeps the resulting [`SyncContext`] on success; once
//! that has happened, later ticks skip straight to [`SyncContext::run_once`]. The state
//! lives behind a `tokio::sync::Mutex` rather than a `std::sync::Mutex`, because holding a
//! `std::sync::MutexGuard` across an `.await` doesn't compile, and behind a `Mutex` rather
//! than a `RefCell`, because a `RefCell` isn't `Send` and this type is spawned into a
//! `tokio::task::JoinSet` (`main.rs`), which requires it.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};

use crate::cache::ContentHashCache;
use crate::config::Job;
use crate::startup;
use crate::sync::SyncContext;

/// One job's own state across every tick of its scheduler loop: the job's config, the
/// process-wide resources every job shares (`cache`, `transfers`), and the lazily-built
/// [`SyncContext`] that only a passing remote-checks pass produces.
pub struct JobRunner {
    job: Job,
    cache: Arc<ContentHashCache>,
    transfers: Arc<Semaphore>,
    transfer_concurrency: u32,
    tmp_dir: Option<PathBuf>,
    sync: Mutex<Option<SyncContext>>,
}

impl JobRunner {
    pub fn new(
        job: Job,
        cache: Arc<ContentHashCache>,
        transfers: Arc<Semaphore>,
        transfer_concurrency: u32,
        tmp_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            job,
            cache,
            transfers,
            transfer_concurrency,
            tmp_dir,
            sync: Mutex::new(None),
        }
    }

    /// The job's own name — only ever used for `main.rs`'s `log::with_job` scope, since
    /// every other log line this runner produces already runs inside that scope.
    pub fn name(&self) -> &str {
        &self.job.name
    }

    /// One `scheduler::run` pass for this job: run the remote checks
    /// ([`startup::run_startup`]) if they haven't passed yet, then one
    /// [`SyncContext::run_once`]. A `SyncContext` that already exists is reused as-is even if
    /// the run it's about to do fails — only a failed *checks* attempt leaves it unset, so
    /// only that is retried on the next tick (`scratch/JOBS-DESIGN.md`'s "Runtime" section:
    /// "a job that has passed its checks ... repeats them only after a failure").
    pub async fn tick(&self) -> anyhow::Result<()> {
        let mut guard = self.sync.lock().await;
        if guard.is_none() {
            let outcome = startup::run_startup(
                &self.job,
                Arc::clone(&self.cache),
                Arc::clone(&self.transfers),
                self.transfer_concurrency,
                self.tmp_dir.clone(),
            )
            .await?;
            *guard = Some(outcome.sync);
        }
        guard
            .as_ref()
            .expect("just populated above when empty")
            .run_once()
            .await
            .map(|_summary| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Json;
    use axum::Router;
    use axum::extract::Path;
    use axum::routing::{get, post};
    use serde_json::json;

    use crate::config::Secret;
    use crate::immich::test_support::spawn_test_server;

    fn test_job(export_album_url: String, import_server_url: String, import_album: String) -> Job {
        Job {
            name: "test".to_owned(),
            export_album_url,
            export_album_password: None,
            import_server_url,
            import_api_key: Secret::from_str("test-api-key").unwrap(),
            import_album,
            interval: std::time::Duration::from_secs(3600),
            request_timeout: std::time::Duration::from_secs(5),
            transfer_timeout: std::time::Duration::from_secs(5),
            tags: Vec::new(),
        }
    }

    /// Export app whose `/server/version` fails the version gate for the first
    /// `fail_attempts` calls, then reports a passing version forever after. Counts every
    /// call, so tests can assert exactly how many times `run_startup` actually reached this
    /// endpoint — the one HTTP call every `run_startup` attempt makes first, which makes it
    /// a reliable proxy for "how many times were the checks attempted".
    fn export_app(version_calls: Arc<AtomicU32>, fail_attempts: u32) -> Router {
        let search_calls = Arc::new(AtomicU32::new(0));
        Router::new()
            .route(
                "/server/version",
                get(move || {
                    let version_calls = version_calls.clone();
                    async move {
                        let call = version_calls.fetch_add(1, Ordering::SeqCst);
                        if call < fail_attempts {
                            Json(json!({"major": 2, "minor": 0, "patch": 0, "prerelease": null}))
                        } else {
                            Json(json!({"major": 3, "minor": 1, "patch": 0, "prerelease": null}))
                        }
                    }
                }),
            )
            .route(
                "/shared-links/me",
                get(|| async {
                    Json(json!({
                        "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
                        "type": "ALBUM",
                        "album": {
                            "id": "9c858901-8a57-4791-81fe-4c455b099bc9",
                            "albumName": "Holiday 2026",
                            "assetCount": 0
                        },
                        "allowDownload": true,
                        "allowUpload": false,
                        "showMetadata": true,
                        "expiresAt": null
                    }))
                }),
            )
            .route(
                "/search/metadata",
                post(move || {
                    let search_calls = search_calls.clone();
                    async move {
                        search_calls.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "albums": {"total": 0, "items": []},
                            "assets": {"items": [], "nextPage": null, "total": 0, "count": 0}
                        }))
                    }
                }),
            )
    }

    fn import_app() -> Router {
        Router::new()
            .route(
                "/server/version",
                get(|| async {
                    Json(json!({"major": 3, "minor": 1, "patch": 0, "prerelease": null}))
                }),
            )
            .route(
                "/api-keys/me",
                get(|| async {
                    Json(json!({
                        "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
                        "name": "sync-key",
                        "permissions": crate::immich::dto::REQUIRED_PERMISSIONS,
                        "createdAt": "2024-01-01T00:00:00.000Z",
                        "updatedAt": "2024-01-01T00:00:00.000Z"
                    }))
                }),
            )
            .route(
                "/albums/{id}",
                get(|Path(id): Path<String>| async move {
                    Json(json!({"id": id, "albumName": "My Family Photos", "assetCount": 0}))
                }),
            )
    }

    /// Regression test for the design's exact wording: "a job whose remote checks fail once
    /// and succeed on the next tick performs its checks exactly twice and its sync run
    /// once."
    #[tokio::test]
    async fn checks_retried_after_a_failure_then_run_once_on_success() {
        let version_calls = Arc::new(AtomicU32::new(0));
        let (export_base, _export_server) =
            spawn_test_server(Router::new().nest("/api", export_app(version_calls.clone(), 1)))
                .await;
        let (import_base, _import_server) =
            spawn_test_server(Router::new().nest("/api", import_app())).await;

        let job = test_job(
            format!("{export_base}share/testkey"),
            import_base.to_string(),
            "8a5e1e2b-2222-4444-8888-aaaaaaaaaaaa".to_owned(),
        );
        let runner = JobRunner::new(
            job,
            Arc::new(ContentHashCache::disabled()),
            Arc::new(Semaphore::new(4)),
            4,
            None,
        );

        let first = runner.tick().await;
        assert!(
            first.is_err(),
            "the export version gate must fail the first attempt"
        );
        assert_eq!(version_calls.load(Ordering::SeqCst), 1);

        let second = runner.tick().await;
        assert!(
            second.is_ok(),
            "the second attempt must succeed once the version gate passes"
        );
        assert_eq!(
            version_calls.load(Ordering::SeqCst),
            2,
            "checks must have been attempted exactly twice"
        );
    }

    /// Regression test for: "a job that has passed its checks does not re-run them on later
    /// ticks."
    #[tokio::test]
    async fn passed_checks_are_not_repeated_on_later_ticks() {
        let version_calls = Arc::new(AtomicU32::new(0));
        let (export_base, _export_server) =
            spawn_test_server(Router::new().nest("/api", export_app(version_calls.clone(), 0)))
                .await;
        let (import_base, _import_server) =
            spawn_test_server(Router::new().nest("/api", import_app())).await;

        let job = test_job(
            format!("{export_base}share/testkey"),
            import_base.to_string(),
            "8a5e1e2b-2222-4444-8888-aaaaaaaaaaaa".to_owned(),
        );
        let runner = JobRunner::new(
            job,
            Arc::new(ContentHashCache::disabled()),
            Arc::new(Semaphore::new(4)),
            4,
            None,
        );

        assert!(runner.tick().await.is_ok());
        assert!(runner.tick().await.is_ok());
        assert!(runner.tick().await.is_ok());

        assert_eq!(
            version_calls.load(Ordering::SeqCst),
            1,
            "checks must run exactly once across three ticks once they've passed"
        );
    }

    #[test]
    fn name_returns_the_jobs_own_name() {
        let job = test_job(
            "https://example.com/share/key".to_owned(),
            "https://example.com".to_owned(),
            "album".to_owned(),
        );
        let runner = JobRunner::new(
            job,
            Arc::new(ContentHashCache::disabled()),
            Arc::new(Semaphore::new(1)),
            1,
            None,
        );
        assert_eq!(runner.name(), "test");
    }
}
