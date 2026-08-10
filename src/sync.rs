//! The per-run sync algorithm (`PLAN.md` §6): list the export album, ask the import
//! instance which of those assets it already has, transfer whatever's missing, and make
//! sure every import-side asset (freshly uploaded or already present) ends up in the
//! target album. One [`SyncContext`] is built once at startup (`main.rs`, a later step)
//! and [`SyncContext::run_once`] is called on every tick.
//!
//! Properties this preserves, straight from `PLAN.md` §6:
//! * **Idempotent** — a second run right after the first transfers nothing and re-adds
//!   nothing (I4's dedup-by-checksum and I6's idempotent album membership do the work; this
//!   module just has to call them every time rather than caching anything itself).
//! * **Additive only** — nothing is ever deleted.
//! * **Memory-bounded** — every asset's bytes stream through a `TMPDIR` temp file; nothing
//!   here ever holds a whole file in a `Vec<u8>`.
//! * **Failure-isolated** — a single asset failing steps 3a–3c (download/checksum/upload)
//!   is logged, counted, and does not stop the run; step 1 or 2 failing aborts the run (an
//!   `Err` from [`SyncContext::run_once`]) but not the process — the next tick retries.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::stream::{self, StreamExt};
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempPath};
use tokio::task::spawn_blocking;
use uuid::Uuid;

use crate::format_error_chain_dyn;
use crate::immich::dto;
use crate::immich::export::{DownloadOutcome, ExportClient, ExportError, SourceAsset};
use crate::immich::import::{BulkUploadCheckOutcome, ImportClient, UploadRequest};
use crate::retry::{self, RetryPolicy};
use crate::{debug, error, info, warn};

/// The counters from one [`SyncContext::run_once`] call — also what backs the §8 summary
/// log line, returned as data (not just logged) so `main.rs` and tests can assert on it.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    /// Total assets enumerated in the export album this run.
    pub source: usize,
    /// Assets the import instance already had by checksum (I4 duplicate, with or without
    /// `isTrashed`) — not re-uploaded, but still sent through step 4's album-add.
    pub already_present: usize,
    /// Assets newly uploaded this run (I5 succeeded, checksum verified).
    pub transferred: usize,
    /// Assets that failed steps 3a–3c (download, checksum mismatch, or upload) or an
    /// individual step-4 album-add — isolated, logged, and not fatal to the run.
    pub failed: usize,
    /// Assets newly added to the target album this run (I6 `success: true`). Does **not**
    /// include assets I6 reported as already members — that's the idempotent case.
    pub added_to_album: usize,
    /// Assets permanently unusable this run: I4 `unsupported-format`, or an unclassifiable
    /// bulk-upload-check result. Never retried by a later tick unless the source changes.
    pub skipped: usize,
    /// Wall-clock time for the whole run.
    pub took: Duration,
}

/// Everything one sync run needs, built once at startup and reused for every tick
/// (`PLAN.md` §9's scheduler, a later step, holds one of these and calls
/// [`SyncContext::run_once`] repeatedly).
pub struct SyncContext {
    export: ExportClient,
    import: ImportClient,
    /// The export album's own id (from `shared_link_me().album.id` at startup) — what E4
    /// pages through every run.
    export_album_id: Uuid,
    /// The resolved `IMPORT_ALBUM` target — what I6 adds to every run.
    import_album_id: Uuid,
    /// `IMPORT_CONCURRENCY`, clamped to at least 1 (`Config::validate` already rejects 0,
    /// but a stray 0 here would make `buffer_unordered` never poll anything — cheap
    /// insurance against that footgun).
    concurrency: usize,
    /// `TRANSFER_TIMEOUT` (`PLAN.md` §4): bounds one asset's *whole* step-3 span (temp file
    /// creation, download, checksum, and upload, including upload's own internal retries).
    /// Applied explicitly by [`SyncContext::transfer_one`] — see that function's doc
    /// comment for why the clients' own per-call timeouts don't already cover this.
    transfer_timeout: Duration,
    /// Retry policy for step 3a's download. [`ExportClient::download_original`] is
    /// deliberately not retried internally (it cannot undo bytes already written to a
    /// writer); this is what lets [`SyncContext::download_with_retry`] retry it safely from
    /// here, opening a clean temp file per attempt.
    download_retry_policy: RetryPolicy,
}

impl SyncContext {
    pub fn new(
        export: ExportClient,
        import: ImportClient,
        export_album_id: Uuid,
        import_album_id: Uuid,
        concurrency: u32,
        transfer_timeout: Duration,
        download_retry_policy: RetryPolicy,
    ) -> Self {
        Self {
            export,
            import,
            export_album_id,
            import_album_id,
            concurrency: usize::try_from(concurrency).unwrap_or(usize::MAX).max(1),
            transfer_timeout,
            download_retry_policy,
        }
    }

    /// Runs one full sync pass (`PLAN.md` §6). `Err` means step 1 or step 2 failed outright
    /// (listing the export album, or the bulk-upload-check call) — the run is abandoned,
    /// but the caller decides what happens next (the scheduler just waits for the next
    /// tick). Every other failure mode (a single asset's download/upload, an individual
    /// album-add) is isolated: logged, counted in the returned [`RunSummary`], and does not
    /// prevent the rest of the run from completing.
    pub async fn run_once(&self) -> anyhow::Result<RunSummary> {
        let start = Instant::now();
        info!(
            "sync run starting export_album_id={} import_album_id={} concurrency={}",
            self.export_album_id, self.import_album_id, self.concurrency
        );

        // ---- 1. list ---------------------------------------------------------------
        let source_assets = self
            .export
            .list_album_assets(self.export_album_id)
            .await
            .context("failed to list the export album's assets")?;
        let source_count = source_assets.len();
        debug!("source assets listed count={source_count}");

        // ---- 2. check ---------------------------------------------------------------
        let check_items: Vec<dto::AssetBulkUploadCheckItem> = source_assets
            .iter()
            .map(|asset| dto::AssetBulkUploadCheckItem {
                id: asset.id.to_string(),
                checksum: asset.checksum.clone(),
            })
            .collect();
        let outcomes = self
            .import
            .check_bulk_upload(&check_items)
            .await
            .context("failed to check which assets the import instance already has")?;

        // import-side UUID -> filename, for every asset that needs an album-add call this
        // run (pre-existing duplicates and freshly uploaded assets alike) — I6 only ever
        // returns bare UUIDs, but the "added to album" log line (§8) needs a filename too.
        let mut album_targets: HashMap<Uuid, String> = HashMap::new();
        let classified = Self::classify(source_assets, &outcomes, &mut album_targets);
        debug!(
            "bulk-upload-check complete to_transfer={} already_present={} skipped={}",
            classified.to_transfer.len(),
            classified.already_present_count,
            classified.skipped_count
        );

        // ---- 3. transfer --------------------------------------------------------------
        let transfer_results: Vec<Option<TransferSuccess>> = stream::iter(classified.to_transfer)
            .map(|asset| self.transfer_one(asset))
            .buffer_unordered(self.concurrency)
            .collect()
            .await;

        let mut transferred_count: usize = 0;
        let mut failed_count: usize = 0;
        for result in transfer_results {
            match result {
                Some(success) => {
                    album_targets.insert(success.import_id, success.filename);
                    transferred_count += 1;
                }
                None => failed_count += 1,
            }
        }

        // ---- 4. album -----------------------------------------------------------------
        let (added_to_album_count, album_failed_count) = self.add_to_album(&album_targets).await?;
        failed_count += album_failed_count;

        // ---- 5. summary -----------------------------------------------------------------
        let took = start.elapsed();
        let summary = RunSummary {
            source: source_count,
            already_present: classified.already_present_count,
            transferred: transferred_count,
            failed: failed_count,
            added_to_album: added_to_album_count,
            skipped: classified.skipped_count,
            took,
        };
        info!(
            "sync run complete source={} already_present={} transferred={} failed={} \
             added_to_album={} skipped={} took={:.1?}",
            summary.source,
            summary.already_present,
            summary.transferred,
            summary.failed,
            summary.added_to_album,
            summary.skipped,
            summary.took
        );
        Ok(summary)
    }

    /// Step 2 (`PLAN.md` §6): partitions `source_assets` by their I4 outcome, logging every
    /// classification (the "already present" §8 line, plus the `isTrashed` warning and the
    /// two error-and-skip cases). Already-present assets are inserted into `album_targets`
    /// right away, since step 4 needs them regardless of what step 3 does.
    fn classify(
        source_assets: Vec<SourceAsset>,
        outcomes: &HashMap<String, BulkUploadCheckOutcome>,
        album_targets: &mut HashMap<Uuid, String>,
    ) -> ClassifiedAssets {
        let mut to_transfer = Vec::new();
        let mut already_present_count: usize = 0;
        let mut skipped_count: usize = 0;

        for asset in source_assets {
            match outcomes.get(asset.id.to_string().as_str()) {
                Some(BulkUploadCheckOutcome::Accept) => to_transfer.push(asset),
                Some(BulkUploadCheckOutcome::Reject {
                    reason: Some(dto::AssetRejectReason::Duplicate),
                    asset_id: Some(import_id),
                    is_trashed,
                }) => {
                    if *is_trashed {
                        warn!(
                            "asset already exists on the import instance but sits in its trash \
                             filename={} checksum={} export_id={} import_id={import_id}",
                            asset.filename, asset.checksum, asset.id
                        );
                    }
                    info!(
                        "already present filename={} checksum={} export_id={} \
                         import_id={import_id}",
                        asset.filename, asset.checksum, asset.id
                    );
                    album_targets.insert(*import_id, asset.filename.clone());
                    already_present_count += 1;
                }
                Some(BulkUploadCheckOutcome::Reject {
                    reason: Some(dto::AssetRejectReason::UnsupportedFormat),
                    ..
                }) => {
                    error!(
                        "the import instance rejected this asset as an unsupported format; \
                         skipping permanently filename={} checksum={} export_id={}",
                        asset.filename, asset.checksum, asset.id
                    );
                    skipped_count += 1;
                }
                other => {
                    // A `reject` with no usable `assetId`, an unrecognised reason, or a
                    // missing entry entirely — none of these are actionable, so treat them
                    // the same as `unsupported-format`: log loudly and move on, rather than
                    // letting one odd item fail the whole run.
                    error!(
                        "bulk-upload-check returned an unusable result for this asset; \
                         skipping filename={} checksum={} export_id={} outcome={other:?}",
                        asset.filename, asset.checksum, asset.id
                    );
                    skipped_count += 1;
                }
            }
        }

        ClassifiedAssets {
            to_transfer,
            already_present_count,
            skipped_count,
        }
    }

    /// Step 4 (`PLAN.md` §6): `PUT /albums/{id}/assets` with every id in `album_targets` —
    /// idempotent on the server (an already-member asset comes back as a duplicate, not an
    /// error). Returns `(newly_added, per_item_failures)`; `per_item_failures` folds into
    /// the run's overall `failed` counter in [`Self::run_once`]. Skips the call entirely
    /// when there is nothing to add (an empty export album, or everything was skipped in
    /// step 2).
    async fn add_to_album(
        &self,
        album_targets: &HashMap<Uuid, String>,
    ) -> anyhow::Result<(usize, usize)> {
        if album_targets.is_empty() {
            return Ok((0, 0));
        }

        let ids: Vec<Uuid> = album_targets.keys().copied().collect();
        let outcome = self
            .import
            .add_assets_to_album(self.import_album_id, &ids)
            .await
            .context("failed to add assets to the import album")?;

        for id in &outcome.added {
            let filename = album_targets.get(id).map_or("(unknown)", String::as_str);
            info!(
                "added to album filename={filename} import_id={id} album_id={}",
                self.import_album_id
            );
        }

        let mut failed_count: usize = 0;
        for (id, reason) in &outcome.failed {
            let filename = album_targets.get(id).map_or("(unknown)", String::as_str);
            error!(
                "failed to add asset to the import album filename={filename} import_id={id} \
                 album_id={} reason={reason:?}",
                self.import_album_id
            );
            failed_count += 1;
        }

        debug!(
            "album-add complete added={} already_in_album={} failed={}",
            outcome.added.len(),
            outcome.already_present.len(),
            outcome.failed.len()
        );

        Ok((outcome.added.len(), failed_count))
    }

    /// One asset's whole step-3 span, bounded by `TRANSFER_TIMEOUT`.
    ///
    /// Neither client's own per-call timeout covers this on its own:
    /// [`ExportClient::download_original`] and [`ImportClient::upload_asset`] each apply
    /// their client's timeout (`TRANSFER_TIMEOUT`) to *their own* request only, and
    /// `upload_asset` additionally retries internally up to 3 times, each attempt getting
    /// its own fresh `TRANSFER_TIMEOUT` budget. Left alone, one asset's download + verify +
    /// upload(+retries) could take a multiple of `TRANSFER_TIMEOUT`, not `TRANSFER_TIMEOUT`
    /// itself. Wrapping the whole thing here is what actually enforces the PLAN.md §4
    /// contract ("Timeout for downloading and re-uploading a single asset").
    async fn transfer_one(&self, asset: SourceAsset) -> Option<TransferSuccess> {
        match tokio::time::timeout(self.transfer_timeout, self.transfer_one_inner(&asset)).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                error!(
                    "asset transfer timed out; skipping {asset} transfer_timeout={}",
                    humantime::format_duration(self.transfer_timeout)
                );
                None
            }
        }
    }

    async fn transfer_one_inner(&self, asset: &SourceAsset) -> Option<TransferSuccess> {
        let start = Instant::now();

        // `NamedTempFile`/`Builder::tempfile()` is a blocking API (it calls `mkstemp`
        // under the hood) — run it on the blocking pool rather than an async worker
        // thread. Only the *path* is kept afterwards (`into_temp_path`); all actual
        // reading/writing goes through `tokio::fs` over that path, which dispatches its
        // own I/O to the blocking pool per call already.
        let temp_path = match spawn_blocking(|| {
            TempFileBuilder::new()
                .prefix("immich-federation-")
                .tempfile()
                .map(NamedTempFile::into_temp_path)
        })
        .await
        {
            Ok(Ok(path)) => path,
            Ok(Err(source)) => {
                error!("failed to create a temporary file for the download {asset}: {source}");
                return None;
            }
            Err(join_err) => {
                error!("temp file creation task did not complete {asset}: {join_err}");
                return None;
            }
        };

        let result = self.download_and_upload(asset, &temp_path).await;

        // Cleanup also goes through the blocking pool: `TempPath`'s own `Drop` impl would
        // otherwise do a synchronous `remove_file` right here on whatever thread is
        // running this future.
        if let Err(join_err) = spawn_blocking(move || drop(temp_path)).await {
            warn!("temp file cleanup task did not complete cleanly: {join_err}");
        }

        let (media, download_outcome) = result?;
        info!(
            "transferred asset {asset} import_id={} bytes={} status={} took={:.1?}",
            media.id,
            download_outcome.bytes_written,
            status_str(media.status),
            start.elapsed()
        );
        Some(TransferSuccess {
            import_id: media.id,
            filename: asset.filename.clone(),
        })
    }

    /// Steps 3a–3c: download to `temp_path` (with its own retry loop, see
    /// [`Self::download_with_retry`]), verify the checksum, and upload. Every failure is
    /// logged here (with the asset's identifying fields) and turned into `None` rather than
    /// propagated — this is where §6's per-asset failure isolation actually happens.
    async fn download_and_upload(
        &self,
        asset: &SourceAsset,
        temp_path: &TempPath,
    ) -> Option<(dto::AssetMediaResponseDto, DownloadOutcome)> {
        // 3a
        let download_outcome = match self.download_with_retry(asset, temp_path).await {
            Ok(outcome) => outcome,
            Err(err) => {
                error!(
                    "failed to download the original asset {asset}: {}",
                    format_error_chain_dyn(&err)
                );
                return None;
            }
        };

        // 3b — never upload a corrupted body.
        if download_outcome.checksum_sha1_base64 != asset.checksum {
            error!(
                "downloaded bytes do not match the source checksum; refusing to upload a \
                 corrupted body {asset} actual_checksum={}",
                download_outcome.checksum_sha1_base64
            );
            return None;
        }

        // 3c
        let upload_request = UploadRequest {
            file_path: temp_path.as_ref(),
            filename: &asset.filename,
            file_created_at: asset.created,
            file_modified_at: asset.modified,
            duration_ms: asset.duration,
            checksum_sha1_base64: &download_outcome.checksum_sha1_base64,
        };
        match self.import.upload_asset(&upload_request).await {
            Ok(media) => Some((media, download_outcome)),
            Err(err) => {
                error!(
                    "failed to upload asset to the import instance {asset}: {}",
                    format_error_chain_dyn(&err)
                );
                None
            }
        }
    }

    /// Drives step 3a's download through [`crate::retry::retry`], reopening (truncating)
    /// `temp_path` fresh on every attempt. [`ExportClient::download_original`] cannot
    /// safely retry itself — see its doc comment — so this is the caller-side retry loop
    /// its own documentation expects; `ExportError` implementing [`crate::retry::Retryable`]
    /// (added alongside this file, see `NOTES.md`) is what lets it plug into the same
    /// generic helper every other retried call in this crate uses, instead of a bespoke
    /// backoff loop.
    async fn download_with_retry(
        &self,
        asset: &SourceAsset,
        temp_path: &TempPath,
    ) -> Result<DownloadOutcome, ExportError> {
        retry::retry(&self.download_retry_policy, "download_asset", || async {
            let mut file =
                tokio::fs::File::create(temp_path)
                    .await
                    .map_err(|source| ExportError::Io {
                        asset_id: asset.id,
                        source,
                    })?;
            self.export.download_original(asset.id, &mut file).await
        })
        .await
    }
}

/// What [`SyncContext::transfer_one`] hands back to [`SyncContext::run_once`] on success:
/// just enough to add the asset to the album and log that addition later (step 4 only ever
/// gets bare UUIDs back from I6, hence carrying `filename` along here too).
struct TransferSuccess {
    import_id: Uuid,
    filename: String,
}

/// [`SyncContext::classify`]'s result: which assets step 3 needs to transfer, plus the
/// counters [`SyncContext::run_once`] folds into the final [`RunSummary`].
struct ClassifiedAssets {
    to_transfer: Vec<SourceAsset>,
    already_present_count: usize,
    skipped_count: usize,
}

/// `dto::AssetMediaStatus`'s wire spelling, for the `status` field on the "transferred
/// asset" log line (`PLAN.md` §8's example shows `status=created`, lowercase — the derived
/// `Debug` on the enum would print `Created`). Kept local to this module rather than adding
/// a `Display` impl to the DTO type, since nothing else needs one.
fn status_str(status: dto::AssetMediaStatus) -> &'static str {
    match status {
        dto::AssetMediaStatus::Created => "created",
        dto::AssetMediaStatus::Duplicate => "duplicate",
        dto::AssetMediaStatus::Unrecognized => "unrecognized",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post, put};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use serde_json::json;
    use sha1::{Digest, Sha1};
    use tokio::task::JoinHandle;
    use url::Url;

    use crate::config::Secret;
    use crate::immich::test_support::spawn_test_server;

    use super::*;

    // ---- fixtures -------------------------------------------------------------------

    /// One asset as it exists on the fake export server: real bytes (checksummed
    /// correctly) unless `corrupt`/`fail` say otherwise.
    #[derive(Clone)]
    struct ExportFixture {
        id: Uuid,
        filename: String,
        bytes: Vec<u8>,
        checksum: String,
        /// If set, `GET /assets/{id}/original` serves *different* bytes than `checksum`
        /// implies — simulating corruption in transit (step 3b must catch this).
        corrupt: bool,
        /// If set, `GET /assets/{id}/original` always 500s — simulating a download
        /// failure isolated to this one asset.
        fail: bool,
    }

    fn fixture(n: u128, filename: &str, contents: &[u8]) -> ExportFixture {
        let mut hasher = Sha1::new();
        hasher.update(contents);
        ExportFixture {
            id: Uuid::from_u128(n),
            filename: filename.to_owned(),
            bytes: contents.to_vec(),
            checksum: BASE64.encode(hasher.finalize()),
            corrupt: false,
            fail: false,
        }
    }

    fn asset_json(f: &ExportFixture) -> serde_json::Value {
        json!({
            "id": f.id.to_string(),
            "checksum": f.checksum,
            "originalFileName": f.filename,
            "type": "IMAGE",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "originalMimeType": "image/jpeg",
            "duration": null
        })
    }

    /// Spawns a fake export server (E4 search + E5 download) serving exactly `fixtures`.
    async fn spawn_export_server(fixtures: Vec<ExportFixture>) -> (Url, JoinHandle<()>) {
        let fixtures = Arc::new(fixtures);
        let search_fixtures = fixtures.clone();
        let app = Router::new()
            .route(
                "/search/metadata",
                post(move || {
                    let items: Vec<_> = search_fixtures.iter().map(asset_json).collect();
                    async move {
                        Json(json!({
                            "albums": {"total": 0, "items": []},
                            "assets": {"items": items, "nextPage": null, "total": items.len(), "count": items.len()}
                        }))
                    }
                }),
            )
            .route(
                "/assets/{id}/original",
                get(move |Path(id): Path<String>| {
                    let fixtures = fixtures.clone();
                    async move {
                        let found = fixtures.iter().find(|f| f.id.to_string() == id).cloned();
                        let Some(f) = found else {
                            return (StatusCode::NOT_FOUND, Vec::new());
                        };
                        if f.fail {
                            return (StatusCode::INTERNAL_SERVER_ERROR, b"boom".to_vec());
                        }
                        if f.corrupt {
                            let mut bad = f.bytes.clone();
                            bad.push(0xFF);
                            return (StatusCode::OK, bad);
                        }
                        (StatusCode::OK, f.bytes.clone())
                    }
                }),
            );
        spawn_test_server(Router::new().nest("/api", app)).await
    }

    /// In-memory state for the fake import server, shared across every request handler and
    /// (deliberately) across repeated `run_once` calls within one test, so a second run
    /// sees exactly what the first run left behind — the same way a real Immich instance
    /// would.
    #[derive(Default)]
    struct ImportServerState {
        /// checksum -> existing import-side asset id, simulating I4's dedup oracle.
        by_checksum: HashMap<String, Uuid>,
        /// import-side asset ids considered "in the trash" for I4's `isTrashed` flag.
        trashed: HashSet<Uuid>,
        /// checksums I4 always rejects as `unsupported-format`.
        unsupported: HashSet<String>,
        /// album id -> member asset ids, simulating I6's idempotent membership.
        album_members: HashMap<Uuid, HashSet<Uuid>>,
        next_id: u128,
        upload_calls: u32,
    }

    impl ImportServerState {
        fn fresh_id(&mut self) -> Uuid {
            self.next_id += 1;
            Uuid::from_u128(0xA000_0000_0000_0000_0000_0000_0000_0000 + self.next_id)
        }
    }

    async fn spawn_import_server() -> (Url, Arc<Mutex<ImportServerState>>, JoinHandle<()>) {
        let state = Arc::new(Mutex::new(ImportServerState::default()));
        let app = Router::new()
            .route("/assets/bulk-upload-check", post(bulk_upload_check_handler))
            .route("/assets", post(upload_handler))
            .route("/albums/{id}/assets", put(album_add_handler))
            .with_state(state.clone());
        let (base, handle) = spawn_test_server(Router::new().nest("/api", app)).await;
        (base, state, handle)
    }

    async fn bulk_upload_check_handler(
        State(state): State<Arc<Mutex<ImportServerState>>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let state = state.lock().unwrap();
        let assets = body["assets"].as_array().cloned().unwrap_or_default();
        let results: Vec<_> = assets
            .iter()
            .map(|a| {
                let id = a["id"].as_str().unwrap();
                let checksum = a["checksum"].as_str().unwrap();
                if state.unsupported.contains(checksum) {
                    return json!({"id": id, "action": "reject", "reason": "unsupported-format"});
                }
                match state.by_checksum.get(checksum) {
                    Some(existing) => json!({
                        "id": id,
                        "action": "reject",
                        "reason": "duplicate",
                        "assetId": existing.to_string(),
                        "isTrashed": state.trashed.contains(existing),
                    }),
                    None => json!({"id": id, "action": "accept"}),
                }
            })
            .collect();
        Json(json!({"results": results}))
    }

    async fn upload_handler(
        State(state): State<Arc<Mutex<ImportServerState>>>,
        headers: axum::http::HeaderMap,
        body: axum::body::Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let checksum = headers
            .get("x-immich-checksum")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        // Not a real multipart parse (see import.rs's own tests for why: no `multipart`
        // feature on the dev-dep `axum`) — we only need proof the body actually contains
        // the file bytes, which the checksum header (computed by the real client from what
        // it streamed) already gives us honestly.
        let _ = body;
        let mut state = state.lock().unwrap();
        state.upload_calls += 1;
        let id = state.fresh_id();
        state.by_checksum.insert(checksum, id);
        (
            StatusCode::CREATED,
            Json(json!({"id": id.to_string(), "status": "created"})),
        )
    }

    async fn album_add_handler(
        State(state): State<Arc<Mutex<ImportServerState>>>,
        Path(album_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let album_id: Uuid = album_id.parse().unwrap();
        let mut state = state.lock().unwrap();
        let ids = body["ids"].as_array().cloned().unwrap_or_default();
        let members = state.album_members.entry(album_id).or_default();
        let results: Vec<_> = ids
            .iter()
            .map(|raw| {
                let id: Uuid = raw.as_str().unwrap().parse().unwrap();
                if members.insert(id) {
                    json!({"id": id.to_string(), "success": true})
                } else {
                    json!({"id": id.to_string(), "success": false, "error": "duplicate"})
                }
            })
            .collect();
        Json(json!(results))
    }

    fn secret(value: &str) -> Secret {
        value.parse().unwrap()
    }

    fn api_base(server_base: &Url) -> Url {
        Url::parse(&format!("{server_base}api")).unwrap()
    }

    fn export_client(base: &Url) -> ExportClient {
        ExportClient::new(
            api_base(base),
            crate::share_url::ShareRef::Key("test-key".to_owned()),
            Duration::from_secs(5),
            Duration::from_secs(5),
            RetryPolicy::zero_delay(),
        )
        .unwrap()
    }

    fn import_client(base: &Url) -> ImportClient {
        ImportClient::new(
            api_base(base),
            &secret("test-api-key"),
            Duration::from_secs(5),
            Duration::from_secs(5),
            RetryPolicy::zero_delay(),
        )
        .unwrap()
    }

    fn context(export_base: &Url, import_base: &Url, import_album_id: Uuid) -> SyncContext {
        SyncContext::new(
            export_client(export_base),
            import_client(import_base),
            Uuid::from_u128(0x5111_0000_0000_0000_0000_0000_0000_0000),
            import_album_id,
            4,
            Duration::from_secs(10),
            RetryPolicy::zero_delay(),
        )
    }

    const ALBUM_ID: Uuid = Uuid::from_u128(0x9999_0000_0000_0000_0000_0000_0000_0000);

    // ---- clean first run --------------------------------------------------------------

    #[tokio::test]
    async fn clean_first_run_transfers_and_adds_everything() {
        let fixtures = vec![
            fixture(1, "a.jpg", b"asset one bytes"),
            fixture(2, "b.jpg", b"asset two bytes, a bit longer"),
            fixture(3, "c.jpg", b"asset three"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 3);
        assert_eq!(summary.already_present, 0);
        assert_eq!(summary.transferred, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.added_to_album, 3);
        assert_eq!(summary.skipped, 0);

        let state = state.lock().unwrap();
        assert_eq!(state.upload_calls, 3);
        assert_eq!(
            state.album_members.get(&ALBUM_ID).map(HashSet::len),
            Some(3)
        );
    }

    // ---- idempotent second run ---------------------------------------------------------

    #[tokio::test]
    async fn second_run_transfers_nothing_and_readds_nothing() {
        let fixtures = vec![
            fixture(1, "a.jpg", b"asset one bytes"),
            fixture(2, "b.jpg", b"asset two bytes, a bit longer"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let first = ctx.run_once().await.unwrap();
        assert_eq!(first.transferred, 2);

        let second = ctx.run_once().await.unwrap();
        assert_eq!(second.source, 2);
        assert_eq!(second.already_present, 2);
        assert_eq!(second.transferred, 0);
        assert_eq!(second.failed, 0);
        assert_eq!(
            second.added_to_album, 0,
            "already-member assets must not recount as added"
        );
        assert_eq!(second.skipped, 0);

        // Only one upload per asset ever happened, across both runs.
        assert_eq!(state.lock().unwrap().upload_calls, 2);
    }

    // ---- partial overlap ----------------------------------------------------------------

    #[tokio::test]
    async fn partial_overlap_transfers_only_the_missing_assets() {
        let fixtures = vec![
            fixture(1, "already.jpg", b"already on the import side"),
            fixture(2, "new-one.jpg", b"brand new asset one"),
            fixture(3, "new-two.jpg", b"brand new asset two"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures.clone()).await;
        let (import_base, state, _i) = spawn_import_server().await;
        // Pre-seed the import server: asset 1's checksum already exists there (as if a
        // previous, unrelated upload put it there), but it was never added to this album.
        {
            let mut state = state.lock().unwrap();
            let existing_id = Uuid::from_u128(0xB000_0000_0000_0000_0000_0000_0000_0001);
            state
                .by_checksum
                .insert(fixtures[0].checksum.clone(), existing_id);
        }
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 3);
        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.transferred, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            summary.added_to_album, 3,
            "all three must land in the album this run"
        );
        assert_eq!(summary.skipped, 0);
        assert_eq!(state.lock().unwrap().upload_calls, 2);
    }

    // ---- checksum mismatch on download ---------------------------------------------------

    #[tokio::test]
    async fn checksum_mismatch_is_isolated_to_the_one_asset() {
        let mut corrupt = fixture(1, "corrupt.jpg", b"looks fine on paper");
        corrupt.corrupt = true;
        let fixtures = vec![
            corrupt,
            fixture(2, "fine-one.jpg", b"this one downloads cleanly"),
            fixture(3, "fine-two.jpg", b"so does this one"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 3);
        assert_eq!(summary.transferred, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(
            summary.added_to_album, 2,
            "the corrupted asset must never reach the album"
        );
        assert_eq!(
            state.lock().unwrap().upload_calls,
            2,
            "a corrupted body must never be uploaded"
        );
    }

    // ---- unsupported-format rejection -----------------------------------------------------

    #[tokio::test]
    async fn unsupported_format_is_skipped_and_never_uploaded() {
        let fixtures = vec![
            fixture(1, "weird.bmp", b"an unsupported format"),
            fixture(2, "normal.jpg", b"a perfectly normal jpeg"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures.clone()).await;
        let (import_base, state, _i) = spawn_import_server().await;
        state
            .lock()
            .unwrap()
            .unsupported
            .insert(fixtures[0].checksum.clone());
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 2);
        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.added_to_album, 1);
        assert_eq!(state.lock().unwrap().upload_calls, 1);
    }

    // ---- one download failure, rest of the run completes -----------------------------------

    #[tokio::test]
    async fn one_download_failure_does_not_stop_the_run() {
        let mut broken = fixture(1, "broken.jpg", b"will never download");
        broken.fail = true;
        let fixtures = vec![
            broken,
            fixture(2, "ok-one.jpg", b"downloads just fine one"),
            fixture(3, "ok-two.jpg", b"downloads just fine two"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 3);
        assert_eq!(summary.transferred, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.added_to_album, 2);
        assert_eq!(state.lock().unwrap().upload_calls, 2);
    }

    // ---- isTrashed duplicate ------------------------------------------------------------

    #[tokio::test]
    async fn trashed_duplicate_is_still_album_added() {
        let fixtures = vec![fixture(1, "trashed.jpg", b"exists but in the trash")];
        let (export_base, _e) = spawn_export_server(fixtures.clone()).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let existing_id = Uuid::from_u128(0xC000_0000_0000_0000_0000_0000_0000_0001);
        {
            let mut state = state.lock().unwrap();
            state
                .by_checksum
                .insert(fixtures[0].checksum.clone(), existing_id);
            state.trashed.insert(existing_id);
        }
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.transferred, 0);
        assert_eq!(
            summary.added_to_album, 1,
            "a trashed duplicate must still be album-added"
        );
    }

    // ---- every outcome in one run --------------------------------------------------------

    /// One run covering all four per-asset outcomes at once — a fresh upload, a plain
    /// duplicate, a trashed duplicate, and an unsupported-format rejection — to pin down how
    /// they add up in the [`RunSummary`]. Also the scenario to run with `--nocapture` when
    /// eyeballing the `info` log output by hand.
    #[tokio::test]
    async fn mixed_run_counts_every_outcome() {
        let fixtures = vec![
            fixture(1, "new-asset.jpg", b"a brand new photo"),
            fixture(2, "duplicate.jpg", b"already on the import side"),
            fixture(3, "trashed-dupe.jpg", b"already there but trashed"),
            fixture(4, "unsupported.bmp", b"not a supported format"),
        ];
        let (export_base, _e) = spawn_export_server(fixtures.clone()).await;
        let (import_base, state, _i) = spawn_import_server().await;
        {
            let mut state = state.lock().unwrap();
            let dup_id = Uuid::from_u128(0xD000_0000_0000_0000_0000_0000_0000_0001);
            state
                .by_checksum
                .insert(fixtures[1].checksum.clone(), dup_id);
            let trashed_id = Uuid::from_u128(0xD000_0000_0000_0000_0000_0000_0000_0002);
            state
                .by_checksum
                .insert(fixtures[2].checksum.clone(), trashed_id);
            state.trashed.insert(trashed_id);
            state.unsupported.insert(fixtures[3].checksum.clone());
        }
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 4);
        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.already_present, 2);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.added_to_album, 3);
        assert_eq!(summary.failed, 0);
    }
}
