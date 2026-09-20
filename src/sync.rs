//! The per-run sync algorithm (`PLAN.md` §6): list the export album, ask the import
//! instance which of those assets it already has, transfer whatever's missing, and make
//! sure every import-side asset (freshly uploaded or already present) ends up in the
//! target album. One [`SyncContext`] is built once per job, by that job's own remote-checks
//! pass (`startup::run_startup`, called lazily from `job::JobRunner::tick` — see those
//! modules' doc comments), and [`SyncContext::run_once`] is called on every tick after that.
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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::stream::{self, StreamExt};
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempPath};
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;
use uuid::Uuid;

use crate::cache::ContentHashCache;
use crate::immich::dto;
use crate::immich::export::{DownloadOutcome, ExportClient, ExportError, SourceAsset};
use crate::immich::import::{BulkUploadCheckOutcome, ImportClient, UploadRequest};
use crate::retry::{self, RetryPolicy};
use crate::{debug, error, format_error_chain, format_error_chain_dyn, info, warn};

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
    /// Path-hashed assets (`scratch/CACHE-DESIGN.md`) served entirely from the content-hash
    /// cache this run — no download, deduplicated via the up-front bulk-upload-check exactly
    /// like a content-hashed asset. Tracked separately from `already_present`/`transferred`
    /// so an operator can tell the cache is actually doing something; a cache hit that turned
    /// out to be a duplicate on the import side is counted in both this and
    /// `already_present`.
    pub cache_hits: usize,
    /// Assets the import instance reported as tagged this run (step 4b) — every asset that
    /// needed a step-4 album-add, freshly uploaded or already present alike, when the job
    /// configures at least one tag. Zero (not tracked separately from a real zero) both when
    /// the job configures no tags and when the tag-assets call itself failed — see
    /// [`SyncContext::tag_assets`]'s doc comment for why that failure is isolated rather than
    /// propagated.
    pub tagged: u64,
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
    /// The export API base URL (`share_url::parse_share_url`'s first return value), used
    /// verbatim as the namespace key for every [`ContentHashCache`] call this context makes
    /// (`scratch/JOBS-DESIGN.md`'s "Cache, file format v2"). Opaque to this module — never
    /// reparsed or validated, just threaded through.
    export_instance: String,
    /// `transfer_concurrency`, clamped to at least 1 (`Config::validate` already rejects 0,
    /// but a stray 0 here would make `buffer_unordered` never poll anything — cheap
    /// insurance against that footgun). This bounds only *this job's* `buffer_unordered`
    /// polling window in [`Self::run_once`] — how many of its own transfer futures are
    /// being driven at once — which is a separate concern from `transfers` below, the
    /// process-wide semaphore that enforces the real cap on assets in flight across every
    /// job. Both are sized from the same global `transfer_concurrency` value, but this field
    /// only saves a job with few assets from polling pointlessly wide; `transfers` is what
    /// the disk/tmpfs sizing advice in `README.md` is actually written against.
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
    /// The path-hash → content-hash cache (`scratch/CACHE-DESIGN.md`, file format v2 per
    /// `scratch/JOBS-DESIGN.md`). An `Arc`: the cache is opened once per process (`main.rs`)
    /// and shared by every job's `SyncContext`, since a cache entry keyed by export instance
    /// is exactly the thing two jobs mirroring the same friend's server should share.
    cache: Arc<ContentHashCache>,
    /// The process-wide transfer cap (`scratch/JOBS-DESIGN.md`'s "Global transfer cap"),
    /// acquired around each asset's whole step-3 span in [`Self::transfer_one`]. Unlike
    /// `concurrency` above, this is shared across every job in the process — it is the
    /// thing that actually bounds how many assets are staged in `TMPDIR` at once, which is
    /// what the README's tmpfs sizing advice is written against.
    transfers: Arc<Semaphore>,
    /// `Globals::tmp_dir` (`scratch/JOBS-DESIGN.md`'s Keys table) — where to stage each
    /// asset's bytes while it's in flight. `None` means "let `tempfile` pick the platform
    /// default", exactly as before this field existed. A plain `Option<PathBuf>`, not an
    /// `Arc`: it's one small, cheaply-cloned value read in exactly one place
    /// ([`Self::transfer_one_inner`]), with nothing to share across jobs the way `cache` and
    /// `transfers` above are shared.
    tmp_dir: Option<PathBuf>,
    /// The job's configured tags, already resolved to import-side ids by
    /// `startup::resolve_tags` (creating whatever didn't already exist). Empty means "don't
    /// tag anything" — [`Self::tag_assets`] short-circuits without a call in that case.
    tag_ids: Vec<Uuid>,
}

impl SyncContext {
    /// 12 constructor arguments rather than a builder or params struct: every field is a
    /// distinct, already-well-named piece of startup state (see each field's own doc comment
    /// above) built exactly once per job (`startup.rs::run_startup`) and once more in this
    /// module's own tests — there is no repeated or optional-subset call site that a builder
    /// would pay for itself against.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        export: ExportClient,
        import: ImportClient,
        export_album_id: Uuid,
        import_album_id: Uuid,
        export_instance: String,
        concurrency: u32,
        transfer_timeout: Duration,
        download_retry_policy: RetryPolicy,
        cache: Arc<ContentHashCache>,
        transfers: Arc<Semaphore>,
        tmp_dir: Option<PathBuf>,
        tag_ids: Vec<Uuid>,
    ) -> Self {
        Self {
            export,
            import,
            export_album_id,
            import_album_id,
            export_instance,
            concurrency: usize::try_from(concurrency).unwrap_or(usize::MAX).max(1),
            transfer_timeout,
            download_retry_policy,
            cache,
            transfers,
            tmp_dir,
            tag_ids,
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

        // ---- partition into the three cohorts (`scratch/CACHE-DESIGN.md`) --------------
        let cohorts = self.partition_cohorts(source_assets);
        let cache_hits = cohorts.cache_hits;

        // ---- 2. check (cohorts 1 and 2 only; the miss cohort is checked per asset in
        // step 3, after its own download — see `scratch/CACHE-DESIGN.md`) ----------------
        let check_items: Vec<dto::AssetBulkUploadCheckItem> = cohorts
            .check_candidates
            .iter()
            .map(|candidate| dto::AssetBulkUploadCheckItem {
                id: candidate.asset.id.to_string(),
                checksum: candidate.checksum.clone(),
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
        let mut classified =
            Self::classify(cohorts.check_candidates, &outcomes, &mut album_targets);
        classified
            .to_transfer
            .extend(cohorts.misses.into_iter().map(|asset| PlannedTransfer {
                asset,
                expected_checksum: None,
            }));
        debug!(
            "bulk-upload-check complete to_transfer={} already_present={} skipped={} \
             cache_hits={cache_hits}",
            classified.to_transfer.len(),
            classified.already_present_count,
            classified.skipped_count
        );

        // ---- 3. transfer --------------------------------------------------------------
        let transfer_results: Vec<TransferOutcome> = stream::iter(classified.to_transfer)
            .map(|item| self.transfer_one(item))
            .buffer_unordered(self.concurrency)
            .collect()
            .await;
        let counts = Self::fold_transfer_results(
            transfer_results,
            &mut album_targets,
            classified.already_present_count,
            classified.skipped_count,
        );

        // ---- 4. album -----------------------------------------------------------------
        let (added_to_album_count, album_failed_count) = self.add_to_album(&album_targets).await?;
        let failed_count = counts.failed + album_failed_count;

        // ---- 4b. tags -------------------------------------------------------------------
        // Every asset that needed a step-4 album-add this run (freshly uploaded or an
        // already-present duplicate alike) gets tagged too — the same idempotent
        // "re-assert every run" treatment as album membership, and the same target set.
        // Unlike step 4, a failure here is isolated rather than propagated: uploading and
        // albuming already succeeded, tagging is naturally retried next tick against the
        // same recomputed asset set, and there is nothing per-asset to roll back.
        let tagged_count = self.tag_assets(&album_targets).await;

        // ---- persist the content-hash cache --------------------------------------------
        // TTL-based pruning (`scratch/JOBS-DESIGN.md`) means this no longer needs to
        // coincide with anything about this run in particular: unlike the old keep-set
        // prune, a lost race against another job's own persist costs nothing, since every
        // persist writes the whole in-memory map regardless of which job triggered it. A
        // failure here is logged, not propagated: the run's real work is already done, and
        // the cache is disposable (losing it only costs a future re-download, never
        // correctness).
        if let Err(err) = self.cache.persist() {
            error!(
                "failed to persist the content-hash cache: {}",
                format_error_chain(&err)
            );
        }

        // ---- 5. summary -----------------------------------------------------------------
        let took = start.elapsed();
        let summary = RunSummary {
            source: source_count,
            already_present: counts.already_present,
            transferred: counts.transferred,
            failed: failed_count,
            added_to_album: added_to_album_count,
            skipped: counts.skipped,
            cache_hits,
            tagged: tagged_count,
            took,
        };
        info!(
            "sync run complete source={} already_present={} transferred={} failed={} \
             added_to_album={} skipped={} cache_hits={} tagged={} took={:.1?}",
            summary.source,
            summary.already_present,
            summary.transferred,
            summary.failed,
            summary.added_to_album,
            summary.skipped,
            summary.cache_hits,
            summary.tagged,
            summary.took
        );
        Ok(summary)
    }

    /// Folds step 3's [`TransferOutcome`]s into the running already-present/skipped counts
    /// step 2 already produced (a cache-miss's own post-download check can add to either),
    /// inserting every id that needs a step-4 album-add into `album_targets` along the way.
    fn fold_transfer_results(
        transfer_results: Vec<TransferOutcome>,
        album_targets: &mut HashMap<Uuid, String>,
        already_present_count: usize,
        skipped_count: usize,
    ) -> TransferCounts {
        let mut counts = TransferCounts {
            already_present: already_present_count,
            skipped: skipped_count,
            ..TransferCounts::default()
        };
        for outcome in transfer_results {
            match outcome {
                TransferOutcome::Transferred {
                    import_id,
                    filename,
                } => {
                    album_targets.insert(import_id, filename);
                    counts.transferred += 1;
                }
                TransferOutcome::AlreadyPresent {
                    import_id,
                    filename,
                } => {
                    album_targets.insert(import_id, filename);
                    counts.already_present += 1;
                }
                TransferOutcome::Skipped => counts.skipped += 1,
                TransferOutcome::Failed => counts.failed += 1,
            }
        }
        counts
    }

    /// Partitions `source_assets` into the three cohorts `scratch/CACHE-DESIGN.md` describes:
    /// content-hashed assets and path-hashed cache hits (both destined for the up-front
    /// `bulk-upload-check` in step 2, as [`CheckCandidate`]s carrying the checksum that check
    /// should use), and path-hashed cache misses (checked individually in step 3, after their
    /// own download, since their real content hash isn't known yet).
    fn partition_cohorts(&self, source_assets: Vec<SourceAsset>) -> Cohorts {
        let mut check_candidates: Vec<CheckCandidate> = Vec::new();
        let mut misses: Vec<SourceAsset> = Vec::new();
        let mut cache_hits: usize = 0;

        for asset in source_assets {
            if !asset.checksum_is_path_hash() {
                let checksum = asset.checksum.clone();
                check_candidates.push(CheckCandidate { asset, checksum });
                continue;
            }

            if let Some(content_checksum) =
                self.cache
                    .get(&self.export_instance, &asset.checksum, asset.modified)
            {
                debug!(
                    "content-hash cache hit filename={} checksum={} export_id={}",
                    asset.filename, asset.checksum, asset.id
                );
                cache_hits += 1;
                check_candidates.push(CheckCandidate {
                    asset,
                    checksum: content_checksum,
                });
            } else {
                debug!(
                    "content-hash cache miss filename={} checksum={} export_id={}",
                    asset.filename, asset.checksum, asset.id
                );
                misses.push(asset);
            }
        }

        Cohorts {
            check_candidates,
            misses,
            cache_hits,
        }
    }

    /// Step 2 (`PLAN.md` §6): partitions `candidates` by their I4 outcome, logging every
    /// classification (the "already present" §8 line, plus the `isTrashed` warning and the
    /// two error-and-skip cases). Already-present assets are inserted into `album_targets`
    /// right away, since step 4 needs them regardless of what step 3 does. `candidate.checksum`
    /// (rather than `candidate.asset.checksum`) is what was actually sent to
    /// `bulk-upload-check` and what an accepted candidate is expected to hash to after
    /// download — for a path-hashed cache hit these differ, the latter being the path hash
    /// (see [`CheckCandidate`]).
    fn classify(
        candidates: Vec<CheckCandidate>,
        outcomes: &HashMap<String, BulkUploadCheckOutcome>,
        album_targets: &mut HashMap<Uuid, String>,
    ) -> ClassifiedAssets {
        let mut to_transfer = Vec::new();
        let mut already_present_count: usize = 0;
        let mut skipped_count: usize = 0;

        for candidate in candidates {
            let CheckCandidate { asset, checksum } = candidate;
            match outcomes.get(asset.id.to_string().as_str()) {
                Some(BulkUploadCheckOutcome::Accept) => to_transfer.push(PlannedTransfer {
                    asset,
                    expected_checksum: Some(checksum),
                }),
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
                "added to album filename={filename} import_id={id} import_album_id={}",
                self.import_album_id
            );
        }

        let mut failed_count: usize = 0;
        for (id, reason) in &outcome.failed {
            let filename = album_targets.get(id).map_or("(unknown)", String::as_str);
            error!(
                "failed to add asset to the import album filename={filename} import_id={id} \
                 import_album_id={} reason={}",
                self.import_album_id,
                album_add_error_str(*reason)
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

    /// Step 4b: `PUT /tags/assets` with every id in `album_targets` and every tag id this
    /// job resolved at startup — idempotent on the server, same as step 4's album-add.
    /// Skips the call entirely when there's nothing to tag (no assets this run, or the job
    /// configures no tags at all), returning `0`. Deliberately **not** propagated as an
    /// `Err` on failure either (also `0` in that case): see the call site's comment for why
    /// a tagging failure is isolated rather than fatal to the run.
    async fn tag_assets(&self, album_targets: &HashMap<Uuid, String>) -> u64 {
        if self.tag_ids.is_empty() || album_targets.is_empty() {
            return 0;
        }

        let ids: Vec<Uuid> = album_targets.keys().copied().collect();
        match self.import.tag_assets(&self.tag_ids, &ids).await {
            Ok(count) => {
                debug!(
                    "tag-assets complete tag_ids={:?} assets={} count={count}",
                    self.tag_ids,
                    ids.len()
                );
                count
            }
            Err(err) => {
                error!(
                    "failed to tag assets on the import instance tag_ids={:?} assets={}: {}",
                    self.tag_ids,
                    ids.len(),
                    format_error_chain_dyn(&err)
                );
                0
            }
        }
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
    async fn transfer_one(&self, item: PlannedTransfer) -> TransferOutcome {
        // The process-wide transfer permit is acquired *before* starting the
        // `TRANSFER_TIMEOUT` clock, not inside it. `transfers` exists to cap how many assets
        // are staged in `TMPDIR` at once across every job, not to cap how long one asset is
        // allowed to queue behind others — if the wait counted against the budget, an asset
        // could time out purely for having been scheduled behind other jobs' assets, before
        // any of its own step-3 work even started. Moving the `acquire().await` inside the
        // `timeout` below would look like a harmless simplification but would reintroduce
        // exactly that bug.
        let _permit = match self.transfers.acquire().await {
            Ok(permit) => permit,
            Err(_closed) => {
                // Only possible if every `Arc<Semaphore>` clone were dropped, which never
                // happens while a `SyncContext` (and therefore this method) is callable.
                error!(
                    "transfer semaphore closed unexpectedly; skipping {}",
                    item.asset
                );
                return TransferOutcome::Failed;
            }
        };

        match tokio::time::timeout(self.transfer_timeout, self.transfer_one_inner(&item)).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                error!(
                    "asset transfer timed out; skipping {} transfer_timeout={}",
                    item.asset,
                    humantime::format_duration(self.transfer_timeout)
                );
                TransferOutcome::Failed
            }
        }
    }

    async fn transfer_one_inner(&self, item: &PlannedTransfer) -> TransferOutcome {
        let asset = &item.asset;

        // `NamedTempFile`/`Builder::tempfile()` is a blocking API (it calls `mkstemp`
        // under the hood) — run it on the blocking pool rather than an async worker
        // thread. Only the *path* is kept afterwards (`into_temp_path`); all actual
        // reading/writing goes through `tokio::fs` over that path, which dispatches its
        // own I/O to the blocking pool per call already.
        let tmp_dir = self.tmp_dir.clone();
        let temp_path = match spawn_blocking(move || {
            let mut builder = TempFileBuilder::new();
            builder.prefix("immich-federation-");
            match &tmp_dir {
                Some(dir) => builder.tempfile_in(dir),
                None => builder.tempfile(),
            }
            .map(NamedTempFile::into_temp_path)
        })
        .await
        {
            Ok(Ok(path)) => path,
            Ok(Err(source)) => {
                error!("failed to create a temporary file for the download {asset}: {source}");
                return TransferOutcome::Failed;
            }
            Err(join_err) => {
                error!("temp file creation task did not complete {asset}: {join_err}");
                return TransferOutcome::Failed;
            }
        };

        let outcome = self.download_and_upload(item, &temp_path).await;

        // Cleanup also goes through the blocking pool: `TempPath`'s own `Drop` impl would
        // otherwise do a synchronous `remove_file` right here on whatever thread is
        // running this future.
        if let Err(join_err) = spawn_blocking(move || drop(temp_path)).await {
            warn!("temp file cleanup task did not complete cleanly: {join_err}");
        }

        outcome
    }

    /// Steps 3a–3c: download to `temp_path` (with its own retry loop, see
    /// [`Self::download_with_retry`]), verify the checksum when one is expected, and upload
    /// (or, for a path-hashed cache miss with no expected checksum, run the post-download
    /// single-item `bulk-upload-check` `scratch/CACHE-DESIGN.md` describes before deciding
    /// whether to upload at all). Every failure is logged here (with the asset's identifying
    /// fields) and turned into [`TransferOutcome::Failed`] rather than propagated — this is
    /// where §6's per-asset failure isolation actually happens.
    async fn download_and_upload(
        &self,
        item: &PlannedTransfer,
        temp_path: &TempPath,
    ) -> TransferOutcome {
        let start = Instant::now();
        let asset = &item.asset;

        // 3a
        let download_outcome = match self.download_with_retry(asset, temp_path).await {
            Ok(outcome) => outcome,
            Err(err) => {
                error!(
                    "failed to download the original asset {asset}: {}",
                    format_error_chain_dyn(&err)
                );
                return TransferOutcome::Failed;
            }
        };

        // Every path-hashed asset that gets downloaded — hit or miss alike — teaches the
        // cache its real content hash, keyed by the path hash we already know. A hit that
        // reaches here (its cached hash didn't dedupe it against the import instance) just
        // reconfirms what the cache already had; a miss is what actually populates the
        // cache. A no-op when the cache is disabled.
        if asset.checksum_is_path_hash() {
            self.cache.insert(
                &self.export_instance,
                asset.checksum.clone(),
                asset.modified,
                download_outcome.checksum_sha1_base64.clone(),
            );
        }

        match &item.expected_checksum {
            // 3b — a known expected checksum (a content-hashed asset, or a path-hashed cache
            // hit verified against the cached content hash): never upload a corrupted body.
            Some(expected) => {
                if download_outcome.checksum_sha1_base64 != *expected {
                    error!(
                        "downloaded bytes do not match the source checksum; refusing to \
                         upload a corrupted body {asset} actual_checksum={}",
                        download_outcome.checksum_sha1_base64
                    );
                    return TransferOutcome::Failed;
                }
                self.upload(asset, &download_outcome, temp_path, start)
                    .await
            }
            // No expected checksum: a path-hashed cache miss. Nothing to verify bytes
            // against (`scratch/CACHE-DESIGN.md` accepts that corruption detection is lost
            // here), but the real content hash is now known, so run the single-item
            // bulk-upload-check the up-front step 2 skipped for this asset.
            None => {
                self.check_and_upload_miss(asset, &download_outcome, temp_path, start)
                    .await
            }
        }
    }

    /// 3c: uploads `asset`'s already-downloaded, already-verified bytes.
    async fn upload(
        &self,
        asset: &SourceAsset,
        download_outcome: &DownloadOutcome,
        temp_path: &TempPath,
        start: Instant,
    ) -> TransferOutcome {
        let upload_request = UploadRequest {
            file_path: temp_path.as_ref(),
            filename: &asset.filename,
            file_created_at: asset.created,
            file_modified_at: asset.modified,
            duration_ms: asset.duration,
            checksum_sha1_base64: &download_outcome.checksum_sha1_base64,
        };
        match self.import.upload_asset(&upload_request).await {
            Ok(media) => {
                info!(
                    "transferred asset {asset} import_id={} bytes={} status={} took={:.1?}",
                    media.id,
                    download_outcome.bytes_written,
                    status_str(media.status),
                    start.elapsed()
                );
                TransferOutcome::Transferred {
                    import_id: media.id,
                    filename: asset.filename.clone(),
                }
            }
            Err(err) => {
                error!(
                    "failed to upload asset to the import instance {asset}: {}",
                    format_error_chain_dyn(&err)
                );
                TransferOutcome::Failed
            }
        }
    }

    /// The post-download check for a path-hashed cache miss (`scratch/CACHE-DESIGN.md`): a
    /// single-item `bulk-upload-check` carrying the content hash just learned by downloading,
    /// interpreted the same way [`Self::classify`] interprets the up-front batched call, but
    /// kept as its own small match (rather than sharing `classify`'s loop body) since the two
    /// call sites log different things — `classify` is reporting on an asset that hasn't been
    /// downloaded yet, this one is reporting on bytes already in hand.
    async fn check_and_upload_miss(
        &self,
        asset: &SourceAsset,
        download_outcome: &DownloadOutcome,
        temp_path: &TempPath,
        start: Instant,
    ) -> TransferOutcome {
        let check_item = dto::AssetBulkUploadCheckItem {
            id: asset.id.to_string(),
            checksum: download_outcome.checksum_sha1_base64.clone(),
        };
        let outcomes = match self
            .import
            .check_bulk_upload(std::slice::from_ref(&check_item))
            .await
        {
            Ok(outcomes) => outcomes,
            Err(err) => {
                error!(
                    "failed to check whether the import instance already has this \
                     freshly-downloaded asset {asset}: {}",
                    format_error_chain_dyn(&err)
                );
                return TransferOutcome::Failed;
            }
        };

        match outcomes.get(asset.id.to_string().as_str()) {
            Some(BulkUploadCheckOutcome::Accept) => {
                self.upload(asset, download_outcome, temp_path, start).await
            }
            Some(BulkUploadCheckOutcome::Reject {
                reason: Some(dto::AssetRejectReason::Duplicate),
                asset_id: Some(import_id),
                is_trashed,
            }) => {
                if *is_trashed {
                    warn!(
                        "asset already exists on the import instance but sits in its trash \
                         filename={} checksum={} export_id={} import_id={import_id}",
                        asset.filename, download_outcome.checksum_sha1_base64, asset.id
                    );
                }
                info!(
                    "already present after download filename={} checksum={} export_id={} \
                     import_id={import_id}",
                    asset.filename, download_outcome.checksum_sha1_base64, asset.id
                );
                TransferOutcome::AlreadyPresent {
                    import_id: *import_id,
                    filename: asset.filename.clone(),
                }
            }
            Some(BulkUploadCheckOutcome::Reject {
                reason: Some(dto::AssetRejectReason::UnsupportedFormat),
                ..
            }) => {
                error!(
                    "the import instance rejected this freshly-downloaded asset as an \
                     unsupported format; skipping permanently filename={} checksum={} \
                     export_id={}",
                    asset.filename, download_outcome.checksum_sha1_base64, asset.id
                );
                TransferOutcome::Skipped
            }
            other => {
                error!(
                    "bulk-upload-check returned an unusable result for this \
                     freshly-downloaded asset; skipping filename={} checksum={} export_id={} \
                     outcome={other:?}",
                    asset.filename, download_outcome.checksum_sha1_base64, asset.id
                );
                TransferOutcome::Skipped
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

/// One item [`SyncContext::classify`] sent to `bulk-upload-check`: an asset paired with the
/// content checksum actually used for that check. Equal to `asset.checksum` for a
/// content-hashed asset; the cached content hash (not `asset.checksum`, the path hash) for a
/// path-hashed cache hit — see `scratch/CACHE-DESIGN.md`.
struct CheckCandidate {
    asset: SourceAsset,
    checksum: String,
}

/// [`SyncContext::partition_cohorts`]'s result.
struct Cohorts {
    /// Content-hashed assets and path-hashed cache hits — what step 2's up-front
    /// `bulk-upload-check` covers.
    check_candidates: Vec<CheckCandidate>,
    /// Path-hashed cache misses — checked individually in step 3, after their own download.
    misses: Vec<SourceAsset>,
    /// How many of `check_candidates` were path-hashed cache hits.
    cache_hits: usize,
}

/// [`SyncContext::fold_transfer_results`]'s result: the run-wide counters
/// [`SyncContext::run_once`] needs after step 3, before step 4's album-add failures are added
/// to `failed`.
#[derive(Default)]
struct TransferCounts {
    transferred: usize,
    already_present: usize,
    skipped: usize,
    failed: usize,
}

/// One asset step 3 is about to transfer, carrying the checksum step 3b should verify the
/// downloaded bytes against — `Some` for a content-hashed asset or a path-hashed cache hit,
/// `None` for a path-hashed cache miss, whose real content hash isn't known until the bytes
/// have arrived (`scratch/CACHE-DESIGN.md`).
struct PlannedTransfer {
    asset: SourceAsset,
    expected_checksum: Option<String>,
}

/// What [`SyncContext::transfer_one`] hands back to [`SyncContext::run_once`]. A cache miss
/// can resolve to any of the four variants (its own post-download check can find it already
/// present or permanently unsupported), where a content-hashed asset or a cache hit can only
/// ever end up `Transferred` or `Failed`.
enum TransferOutcome {
    /// Freshly uploaded. `import_id`/`filename` are what step 4 needs to add it to the album.
    Transferred { import_id: Uuid, filename: String },
    /// The post-download check (path-hashed cache miss only) found the import instance
    /// already had these bytes — not uploaded, but still added to the album.
    AlreadyPresent { import_id: Uuid, filename: String },
    /// The post-download check rejected it permanently (unsupported format), or returned
    /// something unclassifiable. Never retried by a later tick unless the source changes.
    Skipped,
    /// An isolated failure (download, checksum mismatch, upload, or the post-download
    /// check itself), already logged at the point it happened.
    Failed,
}

/// [`SyncContext::classify`]'s result: which assets step 3 needs to transfer, plus the
/// counters [`SyncContext::run_once`] folds into the final [`RunSummary`].
struct ClassifiedAssets {
    to_transfer: Vec<PlannedTransfer>,
    already_present_count: usize,
    skipped_count: usize,
}

/// `dto::AssetMediaStatus`'s wire spelling, for the `status` field on the "transferred
/// asset" log line (`PLAN.md` §8's example shows `status=created`, lowercase — the derived
/// `Debug` on the enum would print `Created`). Kept local to this module rather than adding
/// a `Display` impl to the DTO type, since nothing else needs one.
/// The wire spelling of an I6 per-item failure reason, for the log line — same reasoning as
/// [`status_str`]: `{:?}` on the `Option` would print Rust syntax (`Some(NoPermission)`) at an
/// operator who has only ever seen the JSON spelling.
fn album_add_error_str(reason: Option<dto::BulkIdErrorReason>) -> &'static str {
    match reason {
        Some(dto::BulkIdErrorReason::Duplicate) => "duplicate",
        Some(dto::BulkIdErrorReason::NoPermission) => "no_permission",
        Some(dto::BulkIdErrorReason::NotFound) => "not_found",
        Some(dto::BulkIdErrorReason::Unknown) => "unknown",
        Some(dto::BulkIdErrorReason::Validation) => "validation",
        Some(dto::BulkIdErrorReason::Unrecognized) => "unrecognized",
        None => "(none given)",
    }
}

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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post, put};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use chrono::{DateTime, Utc};
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
        /// `Some` for a path-hashed (external-library) asset: `checksum` above is then
        /// `sha1("path:" + this)`, not a content hash — see [`path_hashed_fixture`]. `None`
        /// for an ordinary content-hashed asset, `fixture`'s case.
        original_path: Option<String>,
        /// `fileModifiedAt`, as an RFC 3339 string. Varied by the cache-staleness test; every
        /// other fixture uses [`DEFAULT_MODIFIED`].
        modified: String,
        /// If set, `GET /assets/{id}/original` serves *different* bytes than `checksum`
        /// implies — simulating corruption in transit (step 3b must catch this).
        corrupt: bool,
        /// If set, `GET /assets/{id}/original` always 500s — simulating a download
        /// failure isolated to this one asset.
        fail: bool,
    }

    /// `fileModifiedAt`/`fileCreatedAt` every fixture uses unless a test overrides it via
    /// [`ExportFixture::with_modified`].
    const DEFAULT_MODIFIED: &str = "2026-05-01T12:00:01.000Z";

    fn fixture(n: u128, filename: &str, contents: &[u8]) -> ExportFixture {
        let mut hasher = Sha1::new();
        hasher.update(contents);
        ExportFixture {
            id: Uuid::from_u128(n),
            filename: filename.to_owned(),
            bytes: contents.to_vec(),
            checksum: BASE64.encode(hasher.finalize()),
            original_path: None,
            modified: DEFAULT_MODIFIED.to_owned(),
            corrupt: false,
            fail: false,
        }
    }

    /// A path-hashed (external-library) fixture: `checksum` is `sha1("path:" + path)`, per
    /// `SourceAsset::checksum_is_path_hash`'s detection rule — not a hash of `contents` at
    /// all, which is the whole point of the cache this module exists to test.
    fn path_hashed_fixture(n: u128, filename: &str, path: &str, contents: &[u8]) -> ExportFixture {
        let mut hasher = Sha1::new();
        hasher.update(b"path:");
        hasher.update(path.as_bytes());
        ExportFixture {
            id: Uuid::from_u128(n),
            filename: filename.to_owned(),
            bytes: contents.to_vec(),
            checksum: BASE64.encode(hasher.finalize()),
            original_path: Some(path.to_owned()),
            modified: DEFAULT_MODIFIED.to_owned(),
            corrupt: false,
            fail: false,
        }
    }

    /// The content hash `contents` would actually hash to — what the cache is expected to
    /// learn for a [`path_hashed_fixture`] once it's downloaded.
    fn content_checksum(contents: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(contents);
        BASE64.encode(hasher.finalize())
    }

    impl ExportFixture {
        fn with_modified(mut self, modified: &str) -> Self {
            self.modified = modified.to_owned();
            self
        }
    }

    fn asset_json(f: &ExportFixture) -> serde_json::Value {
        json!({
            "id": f.id.to_string(),
            "checksum": f.checksum,
            "originalFileName": f.filename,
            "type": "IMAGE",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": f.modified,
            "originalMimeType": "image/jpeg",
            "duration": null,
            "originalPath": f.original_path,
        })
    }

    /// Spawns a fake export server (E4 search + E5 download) serving exactly `fixtures`.
    /// Also hands back a counter of `GET /assets/{id}/original` calls. Skipping the download
    /// is the entire point of the content-hash cache, and an upload counter can't prove it:
    /// a warm-cache run that downloaded and *then* deduplicated would leave the upload count
    /// untouched while doing exactly the work the cache exists to avoid.
    async fn spawn_export_server(
        fixtures: Vec<ExportFixture>,
    ) -> (Url, Arc<AtomicUsize>, JoinHandle<()>) {
        let fixtures = Arc::new(fixtures);
        let search_fixtures = fixtures.clone();
        let downloads = Arc::new(AtomicUsize::new(0));
        let download_counter = downloads.clone();
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
                    let download_counter = download_counter.clone();
                    async move {
                        download_counter.fetch_add(1, Ordering::Relaxed);
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
        let (url, handle) = spawn_test_server(Router::new().nest("/api", app)).await;
        (url, downloads, handle)
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
        /// tag id -> tagged asset ids, simulating `PUT /tags/assets`'s idempotent tagging.
        tag_assignments: HashMap<Uuid, HashSet<Uuid>>,
        /// When set, `PUT /tags/assets` always fails — simulating a tagging outage isolated
        /// from an otherwise-healthy run (`tag_assets_failure_does_not_fail_the_run`).
        fail_tag_assets: bool,
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
            .route("/tags/assets", put(tag_assets_handler))
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

    async fn tag_assets_handler(
        State(state): State<Arc<Mutex<ImportServerState>>>,
        Json(body): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let mut state = state.lock().unwrap();
        if state.fail_tag_assets {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"statusCode": 500, "message": "boom"})),
            );
        }
        let parse_ids = |key: &str| -> Vec<Uuid> {
            body[key]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|v| v.as_str().unwrap().parse().unwrap())
                .collect()
        };
        let tag_ids = parse_ids("tagIds");
        let asset_ids = parse_ids("assetIds");
        for tag_id in &tag_ids {
            let tagged = state.tag_assignments.entry(*tag_id).or_default();
            tagged.extend(asset_ids.iter().copied());
        }
        (StatusCode::OK, Json(json!({"count": asset_ids.len()})))
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
        context_with_cache(
            export_base,
            import_base,
            import_album_id,
            ContentHashCache::disabled(),
        )
    }

    fn context_with_cache(
        export_base: &Url,
        import_base: &Url,
        import_album_id: Uuid,
        cache: ContentHashCache,
    ) -> SyncContext {
        context_with_cache_and_tags(export_base, import_base, import_album_id, cache, Vec::new())
    }

    /// Like [`context`], but with `tag_ids` already resolved — as `startup::resolve_tags`
    /// would have left them — for the tagging-specific tests below.
    fn context_with_tags(
        export_base: &Url,
        import_base: &Url,
        import_album_id: Uuid,
        tag_ids: Vec<Uuid>,
    ) -> SyncContext {
        context_with_cache_and_tags(
            export_base,
            import_base,
            import_album_id,
            ContentHashCache::disabled(),
            tag_ids,
        )
    }

    fn context_with_cache_and_tags(
        export_base: &Url,
        import_base: &Url,
        import_album_id: Uuid,
        cache: ContentHashCache,
        tag_ids: Vec<Uuid>,
    ) -> SyncContext {
        SyncContext::new(
            export_client(export_base),
            import_client(import_base),
            Uuid::from_u128(0x5111_0000_0000_0000_0000_0000_0000_0000),
            import_album_id,
            TEST_EXPORT_INSTANCE.to_owned(),
            4,
            Duration::from_secs(10),
            RetryPolicy::zero_delay(),
            Arc::new(cache),
            Arc::new(Semaphore::new(4)),
            None,
            tag_ids,
        )
    }

    /// The export instance key every test context is built with — arbitrary, but fixed, so
    /// tests that pre-seed a cache entry directly (rather than via a prior `run_once`) know
    /// exactly which namespace to write into.
    const TEST_EXPORT_INSTANCE: &str = "https://export.example.com/api";

    const ALBUM_ID: Uuid = Uuid::from_u128(0x9999_0000_0000_0000_0000_0000_0000_0000);

    fn default_modified() -> DateTime<Utc> {
        DEFAULT_MODIFIED.parse().unwrap()
    }

    // ---- clean first run --------------------------------------------------------------

    #[tokio::test]
    async fn clean_first_run_transfers_and_adds_everything() {
        let fixtures = vec![
            fixture(1, "a.jpg", b"asset one bytes"),
            fixture(2, "b.jpg", b"asset two bytes, a bit longer"),
            fixture(3, "c.jpg", b"asset three"),
        ];
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures.clone()).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures.clone()).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures.clone()).await;
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
        let (export_base, _downloads, _e) = spawn_export_server(fixtures.clone()).await;
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

    // ---- the content-hash cache (`scratch/CACHE-DESIGN.md`) --------------------------------

    /// A cold cache: the path-hashed asset must be downloaded, pass its post-download
    /// single-item check (accepted), get uploaded, and have its real content hash land in the
    /// cache — on disk, not just in memory, since [`ContentHashCache::persist`] runs inside
    /// `run_once` itself.
    #[tokio::test]
    async fn path_hashed_cold_cache_downloads_checks_uploads_and_populates_cache() {
        let path = "/library/photo.jpg";
        let contents = b"external library bytes";
        let fx = path_hashed_fixture(1, "photo.jpg", path, contents);
        let (export_base, _downloads, _e) = spawn_export_server(vec![fx.clone()]).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(cache_dir.path()).unwrap();
        let ctx = context_with_cache(&export_base, &import_base, ALBUM_ID, cache);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 1);
        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.cache_hits, 0);
        assert_eq!(summary.already_present, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.added_to_album, 1);
        assert_eq!(state.lock().unwrap().upload_calls, 1);

        // The content hash the download actually produced (not the path-hash `checksum` the
        // fixture advertises) must be what got cached.
        let reopened = ContentHashCache::open(cache_dir.path()).unwrap();
        assert_eq!(
            reopened.get(TEST_EXPORT_INSTANCE, &fx.checksum, default_modified()),
            Some(content_checksum(contents))
        );
    }

    /// The same asset, second run, warm cache: the up-front bulk-upload-check alone
    /// deduplicates it (the cache substitutes the real content hash into that check), so no
    /// download and no second upload ever happen.
    #[tokio::test]
    async fn path_hashed_warm_cache_second_run_dedupes_with_no_download() {
        let path = "/library/photo.jpg";
        let contents = b"external library bytes, round two";
        let fx = path_hashed_fixture(1, "photo.jpg", path, contents);
        let (export_base, downloads, _e) = spawn_export_server(vec![fx.clone()]).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(cache_dir.path()).unwrap();
        let ctx = context_with_cache(&export_base, &import_base, ALBUM_ID, cache);

        let first = ctx.run_once().await.unwrap();
        assert_eq!(first.transferred, 1);
        assert_eq!(first.cache_hits, 0);
        assert_eq!(
            downloads.load(Ordering::Relaxed),
            1,
            "the cold run downloads"
        );

        let second = ctx.run_once().await.unwrap();
        assert_eq!(second.source, 1);
        assert_eq!(second.transferred, 0);
        assert_eq!(second.cache_hits, 1);
        assert_eq!(second.already_present, 1);
        assert_eq!(
            second.added_to_album, 0,
            "already a member from the first run"
        );
        assert_eq!(second.failed, 0);

        // Only one upload ever happened, across both runs — and, the point of the whole
        // exercise, only one download: the warm run never touched the export instance's
        // bytes at all.
        assert_eq!(state.lock().unwrap().upload_calls, 1);
        assert_eq!(downloads.load(Ordering::Relaxed), 1);
    }

    /// A path-hashed cache miss whose post-download check finds the import instance already
    /// has these bytes: not uploaded, counted as `already_present`, still added to the album,
    /// and still learned into the cache (the content hash is known regardless of what the
    /// import side already has).
    #[tokio::test]
    async fn path_hashed_miss_duplicate_after_download_is_not_uploaded() {
        let path = "/library/photo.jpg";
        let contents = b"bytes the import side already has";
        let fx = path_hashed_fixture(1, "photo.jpg", path, contents);
        let (export_base, _downloads, _e) = spawn_export_server(vec![fx.clone()]).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let existing_id = Uuid::from_u128(0xE000_0000_0000_0000_0000_0000_0000_0001);
        {
            let mut state = state.lock().unwrap();
            state
                .by_checksum
                .insert(content_checksum(contents), existing_id);
        }
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(cache_dir.path()).unwrap();
        let ctx = context_with_cache(&export_base, &import_base, ALBUM_ID, cache);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 1);
        assert_eq!(summary.transferred, 0);
        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.added_to_album, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            state.lock().unwrap().upload_calls,
            0,
            "a post-download duplicate must never be uploaded"
        );

        let reopened = ContentHashCache::open(cache_dir.path()).unwrap();
        assert_eq!(
            reopened.get(TEST_EXPORT_INSTANCE, &fx.checksum, default_modified()),
            Some(content_checksum(contents)),
            "the content hash must be learned even though the asset wasn't uploaded"
        );
    }

    /// A cache entry whose `modified` no longer matches the source asset's current
    /// `fileModifiedAt` is a miss, not a stale hit: the asset is re-downloaded.
    #[tokio::test]
    async fn path_hashed_stale_modified_timestamp_is_a_miss() {
        let path = "/library/photo.jpg";
        let contents = b"the file changed since we last looked";
        let new_modified = "2026-06-01T09:00:00.000Z";
        let fx = path_hashed_fixture(1, "photo.jpg", path, contents).with_modified(new_modified);
        let (export_base, _downloads, _e) = spawn_export_server(vec![fx.clone()]).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(cache_dir.path()).unwrap();
        // The cache already has an entry for this path hash, but at the *old* modified
        // timestamp — stale now that the fixture's fileModifiedAt has moved on.
        cache.insert(
            TEST_EXPORT_INSTANCE,
            fx.checksum.clone(),
            default_modified(),
            content_checksum(b"the old content, before it changed"),
        );
        let ctx = context_with_cache(&export_base, &import_base, ALBUM_ID, cache);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.source, 1);
        assert_eq!(
            summary.transferred, 1,
            "a stale entry must be treated as a miss"
        );
        assert_eq!(summary.cache_hits, 0);
        assert_eq!(state.lock().unwrap().upload_calls, 1);

        let reopened = ContentHashCache::open(cache_dir.path()).unwrap();
        let new_modified_parsed: DateTime<Utc> = new_modified.parse().unwrap();
        assert_eq!(
            reopened.get(TEST_EXPORT_INSTANCE, &fx.checksum, new_modified_parsed),
            Some(content_checksum(contents)),
            "the cache must be updated with the fresh content hash and timestamp"
        );
    }

    /// A content-hashed asset (no `originalPath`) behaves exactly as before the cache existed:
    /// the cache never enters the picture, and a genuine checksum mismatch on download is
    /// still refused rather than uploaded.
    #[tokio::test]
    async fn content_hashed_asset_checksum_mismatch_still_refused_with_cache_enabled() {
        let mut corrupt = fixture(1, "corrupt.jpg", b"looks fine on paper");
        corrupt.corrupt = true;
        let (export_base, _downloads, _e) = spawn_export_server(vec![corrupt]).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(cache_dir.path()).unwrap();
        let ctx = context_with_cache(&export_base, &import_base, ALBUM_ID, cache);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.transferred, 0);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.cache_hits, 0);
        assert_eq!(state.lock().unwrap().upload_calls, 0);
    }

    // ---- the cache never decides whether work happens (`scratch/JOBS-DESIGN.md`) -----------

    /// Regression test for the invariant `scratch/JOBS-DESIGN.md` calls out by name: a cache
    /// hit must never short-circuit step 4. Here the cache is warmed directly (as if some
    /// *other* job against the same export instance had already downloaded this asset), and
    /// the import instance already has the resulting content hash too (as if that other job
    /// had already uploaded it). The asset must still flow through the up-front
    /// bulk-upload-check, classify as an ordinary duplicate, and land in `album_targets` —
    /// exactly as any other already-present asset would, not be treated as "the cache says
    /// this is handled" and skipped.
    #[tokio::test]
    async fn path_hashed_cache_hit_that_is_already_present_still_gets_added_to_album() {
        let path = "/library/photo.jpg";
        let contents = b"learned by a different job against the same export instance";
        let fx = path_hashed_fixture(1, "photo.jpg", path, contents);
        let (export_base, downloads, _e) = spawn_export_server(vec![fx.clone()]).await;
        let (import_base, state, _i) = spawn_import_server().await;

        let existing_id = Uuid::from_u128(0xF000_0000_0000_0000_0000_0000_0000_0001);
        {
            let mut state = state.lock().unwrap();
            state
                .by_checksum
                .insert(content_checksum(contents), existing_id);
        }

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(cache_dir.path()).unwrap();
        cache.insert(
            TEST_EXPORT_INSTANCE,
            fx.checksum.clone(),
            default_modified(),
            content_checksum(contents),
        );
        let ctx = context_with_cache(&export_base, &import_base, ALBUM_ID, cache);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.cache_hits, 1, "must be served from the warm cache");
        assert_eq!(summary.transferred, 0);
        assert_eq!(summary.already_present, 1);
        assert_eq!(
            summary.added_to_album, 1,
            "a cache hit that turns out to be a duplicate must still reach step 4"
        );
        assert_eq!(summary.failed, 0);
        assert_eq!(
            downloads.load(Ordering::Relaxed),
            0,
            "a cache hit must never download"
        );
        assert!(
            state
                .lock()
                .unwrap()
                .album_members
                .get(&ALBUM_ID)
                .is_some_and(|members| members.contains(&existing_id)),
            "the existing import-side asset must be added to the album, not silently dropped"
        );
    }

    // ---- tags -----------------------------------------------------------------------------

    const TAG_ID: Uuid = Uuid::from_u128(0x7A6_0000_0000_0000_0000_0000_0000_0000);

    #[tokio::test]
    async fn no_tags_configured_means_no_tag_assets_call() {
        let fixtures = vec![fixture(1, "a.jpg", b"asset one bytes")];
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let ctx = context(&export_base, &import_base, ALBUM_ID);

        let summary = ctx.run_once().await.unwrap();

        assert_eq!(summary.tagged, 0);
        assert!(
            state.lock().unwrap().tag_assignments.is_empty(),
            "a job with no configured tags must never call PUT /tags/assets"
        );
    }

    #[tokio::test]
    async fn freshly_transferred_assets_are_tagged() {
        let fixtures = vec![fixture(1, "a.jpg", b"asset one bytes")];
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let ctx = context_with_tags(&export_base, &import_base, ALBUM_ID, vec![TAG_ID]);

        let summary = ctx.run_once().await.unwrap();
        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.tagged, 1);

        let state = state.lock().unwrap();
        assert_eq!(
            state.tag_assignments.get(&TAG_ID).map(HashSet::len),
            Some(1)
        );
    }

    #[tokio::test]
    async fn already_present_assets_are_tagged_too() {
        let fixtures = vec![fixture(1, "already.jpg", b"already on the import side")];
        let (export_base, _downloads, _e) = spawn_export_server(fixtures.clone()).await;
        let (import_base, state, _i) = spawn_import_server().await;
        let existing_id = Uuid::from_u128(0xB000_0000_0000_0000_0000_0000_0000_0001);
        {
            let mut state = state.lock().unwrap();
            state
                .by_checksum
                .insert(fixtures[0].checksum.clone(), existing_id);
        }
        let ctx = context_with_tags(&export_base, &import_base, ALBUM_ID, vec![TAG_ID]);

        let summary = ctx.run_once().await.unwrap();
        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.transferred, 0);
        assert_eq!(summary.tagged, 1);

        let state = state.lock().unwrap();
        assert!(
            state
                .tag_assignments
                .get(&TAG_ID)
                .is_some_and(|tagged| tagged.contains(&existing_id)),
            "an asset the import instance already had must still be tagged, matching how it's \
             still added to the album"
        );
    }

    #[tokio::test]
    async fn tag_assets_failure_does_not_fail_the_run() {
        let fixtures = vec![fixture(1, "a.jpg", b"asset one bytes")];
        let (export_base, _downloads, _e) = spawn_export_server(fixtures).await;
        let (import_base, state, _i) = spawn_import_server().await;
        state.lock().unwrap().fail_tag_assets = true;
        let ctx = context_with_tags(&export_base, &import_base, ALBUM_ID, vec![TAG_ID]);

        let summary = ctx
            .run_once()
            .await
            .expect("a tag-assets failure must not fail the run");

        assert_eq!(summary.transferred, 1);
        assert_eq!(
            summary.added_to_album, 1,
            "the album-add step must still succeed independently of tagging"
        );
        assert_eq!(summary.tagged, 0);
        assert!(state.lock().unwrap().tag_assignments.is_empty());
    }
}
