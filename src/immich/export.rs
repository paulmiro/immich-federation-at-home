//! `ExportClient` — the share-link-authenticated side of the API surface (`PLAN.md` §2,
//! calls E1–E5): probing the export server's version, the password-login cookie dance,
//! reading the shared link's own metadata, paginating through an album's assets, and
//! streaming an asset's original bytes to disk while hashing them.
//!
//! Every authenticated request carries the share link's credential as a query parameter —
//! `?key=…` or `?slug=…`, applied via [`ShareRef::apply`] — never a header, never logged
//! (see [`crate::immich::redact_url`]). `E1` (`/server/version`) is the one exception: it is
//! unauthenticated by design, so no [`ShareRef`] is applied to it.

use std::fmt;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::{Client, Method, StatusCode};
use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use url::Url;
use uuid::Uuid;

use crate::config::Secret;
use crate::immich::{
    self, ApiError, Version, build_client, dto, execute_once, parse_json_response, send_json,
};
use crate::retry::{RetryPolicy, Retryable};
use crate::share_url::ShareRef;
use crate::{debug, error, warn};

/// How many assets `POST /search/metadata` (E4) is asked for per page. `PLAN.md` §6 step 1
/// fixes this at 250.
pub const SEARCH_PAGE_SIZE: u32 = 250;

/// A hard cap on how many pages [`ExportClient::list_album_assets`] will follow before
/// giving up. At [`SEARCH_PAGE_SIZE`] assets per page this is 5,000,000 assets — an album
/// will never legitimately be this large; the cap exists purely so a server bug (an
/// `nextPage` that never goes `null`) turns into a bounded, loud error instead of an
/// unbounded hang. See also the "did `nextPage` actually change" check in the same function,
/// which usually catches a stuck server far sooner than this cap would.
const MAX_SEARCH_PAGES: u32 = 20_000;

/// One asset as enumerated from the export album (`PLAN.md` §6 step 1: "id, checksum,
/// filename, mime, created, modified, type, duration"). A thin, purpose-built projection of
/// [`dto::AssetResponseDto`] — kept separate from the wire DTO so `sync.rs` (a later step)
/// depends on a small stable shape rather than the full response type, and so the
/// empty-checksum rejection below has somewhere to live as a type-level guarantee: once you
/// have a `SourceAsset`, its `checksum` is known non-empty.
#[derive(Debug, Clone)]
pub struct SourceAsset {
    pub id: Uuid,
    pub checksum: String,
    pub filename: String,
    pub mime: Option<String>,
    pub created: DateTime<Utc>,
    pub modified: DateTime<Utc>,
    pub r#type: dto::AssetTypeEnum,
    pub duration: Option<i64>,
    /// `dto::AssetResponseDto::original_path`, carried through unchanged. `None` either
    /// because the server omitted it (see that field's doc comment) or, in principle,
    /// because it really is empty — either way [`Self::checksum_is_path_hash`] treats
    /// `None` as "assume content-hashed", the safe fallback.
    pub original_path: Option<String>,
}

impl fmt::Display for SourceAsset {
    /// The three fields `PLAN.md` §8 wants on every per-asset log line. `sync.rs` interpolates
    /// a `SourceAsset` directly (`info!("transferred asset {asset} ...")`) rather than
    /// spelling the same three out at each of its dozen call sites.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} checksum={} export_id={}",
            self.filename, self.checksum, self.id
        )
    }
}

/// [`dto::AssetResponseDto::checksum`] is spec-required but **not** spec-guaranteed
/// non-empty (see `NOTES.md`, task 3/4). An asset with an empty checksum can never be
/// deduplicated via `POST /assets/bulk-upload-check` (`PLAN.md` R6), so it is unusable to
/// this tool; [`ExportClient::list_album_assets`] logs one of these at `error` per offending
/// asset and excludes it from the returned list, rather than failing the whole page over one
/// asset's bad metadata.
#[derive(Debug, Error)]
#[error("asset {id} ({filename:?}) has an empty checksum and cannot be deduplicated; skipping it")]
pub struct EmptyChecksumError {
    id: Uuid,
    filename: String,
}

impl TryFrom<dto::AssetResponseDto> for SourceAsset {
    type Error = EmptyChecksumError;

    fn try_from(asset: dto::AssetResponseDto) -> Result<Self, Self::Error> {
        if asset.checksum.trim().is_empty() {
            return Err(EmptyChecksumError {
                id: asset.id,
                filename: asset.original_file_name,
            });
        }
        Ok(Self {
            id: asset.id,
            checksum: asset.checksum,
            filename: asset.original_file_name,
            mime: asset.original_mime_type,
            created: asset.file_created_at,
            modified: asset.file_modified_at,
            r#type: asset.r#type,
            duration: asset.duration,
            original_path: asset.original_path,
        })
    }
}

impl SourceAsset {
    /// Whether `self.checksum` is one of Immich's **path hashes** rather than a content
    /// hash of the file's bytes.
    ///
    /// Immich has two checksum algorithms for an asset (`server/src/enum.ts`, v3.1.0):
    /// `sha1File` (sha1 of the whole file's contents — what everything in this program has
    /// assumed so far) and `sha1Path` (sha1 of the literal string `"path:"` concatenated
    /// with `originalPath` — the server never opens the file). External-library scans use
    /// the latter: `library.service.ts:421` builds it as `hashSha1(path:${assetPath})`,
    /// and that's the only value ever stored for such an asset — there is no second,
    /// content-based hash sitting alongside it to fall back on.
    ///
    /// Which algorithm produced a given `checksum` is not something the API tells us.
    /// `checksumAlgorithm` exists as a field on the server's asset entity, but it is
    /// deliberately not mapped into `AssetResponseDto` — it does not appear anywhere in
    /// `openapi/immich-openapi-3.1.0.json`. So there is no discriminator to read; the only
    /// way to tell the two apart is to recompute one of them and compare. That's what this
    /// method does, and it is why the answer it gives is a **positive identification, not
    /// an inference**: `sha1("path:" + originalPath)` either equals `checksum` — in which
    /// case, short of an astronomically unlikely SHA-1 collision, this asset's checksum
    /// really is its path hash — or it doesn't, in which case it is presumptively a content
    /// hash (the safe default; see [`dto::AssetResponseDto::original_path`]'s doc comment
    /// for why `None` also falls here). There is no ambiguous middle case.
    ///
    /// Deliberately **not** based on `libraryId`. An asset belonging to an external library
    /// is not the same thing as an asset whose checksum is a path hash: the motion-photo
    /// video that Immich extracts from a Pixel-style `.MP`/`.MV` still image inherits that
    /// image's `libraryId` (`metadata.service.ts`) but is content-hashed with an explicit
    /// `sha1File` call on the extracted video bytes, not a path hash — the extracted asset
    /// has no meaningful "path" of its own to hash. Keying off `libraryId` would
    /// misclassify that asset as path-hashed and wrongly discard its checksum.
    pub fn checksum_is_path_hash(&self) -> bool {
        let Some(path) = &self.original_path else {
            return false;
        };
        let mut hasher = Sha1::new();
        hasher.update(b"path:");
        hasher.update(path.as_bytes());
        BASE64.encode(hasher.finalize()) == self.checksum
    }
}

/// The outcome of [`ExportClient::download_original`]: how many bytes were streamed and the
/// SHA-1 of exactly those bytes, base64-encoded (the same encoding
/// [`dto::AssetResponseDto::checksum`] uses, per `RESEARCH.md`) so the caller can compare it
/// directly against the source asset's checksum without a re-encoding step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub bytes_written: u64,
    pub checksum_sha1_base64: String,
}

/// Everything that can go wrong on the export side, beyond a plain [`ApiError`]. Kept
/// distinct from `ApiError` so callers (`main.rs`, `sync.rs` — later steps) can match on
/// export-specific, actionable cases (wrong password, download forbidden, …) without parsing
/// status codes themselves.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error(transparent)]
    Api(#[from] ApiError),

    /// `POST /shared-links/login` returned `401`. Per `PLAN.md` §5 step 5 this means a wrong
    /// password — but, verified against the server's own `auth.service.ts`, a `401` here can
    /// *also* mean the share key/slug itself doesn't resolve to a link at all (the auth
    /// guard's own `validateSharedLinkKey`/`validateSharedLinkSlug` reject before the login
    /// handler ever runs), since this is typically the first authenticated call made against
    /// the export instance. The message names both possibilities rather than guessing which.
    #[error(
        "shared-link login failed with 401 Unauthorized: either EXPORT_ALBUM_PASSWORD is \
         wrong, or EXPORT_ALBUM_URL's key/slug itself is invalid"
    )]
    WrongPassword,

    /// Downloading an asset's original bytes failed with a status that, on a genuine Immich
    /// server, only ever means one thing: the shared link has `allowDownload: false`.
    /// Verified against `access.ts`'s `checkSharedLinkAccess` (the `AssetDownload` case
    /// returns an empty access set when `!allowDownload`, which `requireAccess` turns into a
    /// `400 Bad Request` — **not** the `401`/`403` a first guess might expect; see
    /// `NOTES.md`). `401`/`403` are included in the match anyway since nothing in the spec
    /// promises a future server version won't tighten this to a "proper" auth status.
    #[error(
        "download of asset {asset_id} failed ({status}). This most likely means the share \
         link does not have \"Allow download\" enabled."
    )]
    DownloadForbidden {
        asset_id: Uuid,
        status: StatusCode,
        #[source]
        source: ApiError,
    },

    /// Writing the streamed bytes to the caller-supplied writer failed — a local disk
    /// problem (out of space, a bad `TMPDIR`, …), not anything the export server did.
    #[error("failed to write downloaded asset {asset_id} to disk")]
    Io {
        asset_id: Uuid,
        #[source]
        source: std::io::Error,
    },

    /// [`ExportClient::list_album_assets`]'s own infinite-loop guard: the server returned the
    /// exact same `nextPage` token on two consecutive pages, which — since we always send a
    /// freshly incremented `page` number ourselves rather than trusting the token's content
    /// (see `NOTES.md`: `nextPage` is spec-typed as an opaque nullable string, not a number)
    /// — means the server is not making progress. Aborting loudly here is strictly better
    /// than an unbounded hang.
    #[error(
        "pagination stalled while listing album {album_id}'s assets: the server returned the \
         same nextPage token twice in a row; aborting rather than looping forever"
    )]
    PaginationStalled { album_id: Uuid },

    /// The hard cap in [`MAX_SEARCH_PAGES`] was hit. See its doc comment.
    #[error(
        "listing album {album_id}'s assets did not finish within {MAX_SEARCH_PAGES} pages; \
         aborting rather than looping forever"
    )]
    TooManyPages { album_id: Uuid },
}

/// Lets [`ExportError`] be driven through [`crate::retry::retry`] directly. Added for
/// `sync.rs` (task 8): [`ExportClient::download_original`] (E5) is deliberately **not**
/// retried internally (see its doc comment — it cannot safely retry without corrupting a
/// partially-written output), which pushes the retry loop to the caller. That caller wants
/// to drive the retry through the same generic [`crate::retry::retry`] helper everything
/// else uses (rather than hand-rolling a second backoff/jitter implementation), which
/// requires `E: Retryable` — mirrors [`crate::immich::import::ImportError`]'s identical
/// impl exactly: defers to the wrapped [`ApiError`]'s own classification, `false` for every
/// other variant (a stalled/looping pagination or a local disk error can't succeed
/// differently on an identical retry).
impl Retryable for ExportError {
    fn is_retryable(&self) -> bool {
        matches!(self, ExportError::Api(err) if err.is_retryable())
    }

    fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            ExportError::Api(err) => err.retry_after(),
            _ => None,
        }
    }
}

/// The result of [`ExportClient::login`] (E2). A `400` from that endpoint means "this link
/// has no password" — per `PLAN.md` §5 step 5, a warning to surface to the operator, not a
/// failure: the sync can proceed unauthenticated exactly as if no password had been
/// configured. `LoggedIn` and `NotPasswordProtected` are otherwise equivalent for every
/// subsequent call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    LoggedIn,
    NotPasswordProtected,
}

/// The share-link-authenticated Immich client (`PLAN.md` §2's export side, E1–E5).
///
/// Holds two `reqwest::Client`s rather than one: metadata calls (E1–E4) use
/// `REQUEST_TIMEOUT`, the original-file download (E5) uses the much larger
/// `TRANSFER_TIMEOUT` (`PLAN.md` §4) — a single client can only have one default timeout, and
/// [`crate::immich::build_client`]'s own doc comment already anticipates exactly this split.
/// Both clients keep a cookie store (`PLAN.md` §5 step 5's login cookie): in practice only
/// `GET /shared-links/me` enforces the password (per `RESEARCH.md`/`shared-link.service.ts`,
/// search and download both work with just `?key=`/`?slug=`), so only the metadata client
/// strictly needs it, but enabling it on both is free and one less thing to get wrong if that
/// ever changes upstream.
pub struct ExportClient {
    api_base: Url,
    share_ref: ShareRef,
    metadata_client: Client,
    transfer_client: Client,
    retry_policy: RetryPolicy,
}

impl ExportClient {
    pub fn new(
        api_base: Url,
        share_ref: ShareRef,
        request_timeout: Duration,
        transfer_timeout: Duration,
        retry_policy: RetryPolicy,
    ) -> anyhow::Result<Self> {
        let metadata_client = build_client(request_timeout, true, HeaderMap::new())
            .context("failed to build the export-side metadata HTTP client")?;
        let transfer_client = build_client(transfer_timeout, true, HeaderMap::new())
            .context("failed to build the export-side transfer HTTP client")?;
        Ok(Self {
            api_base,
            share_ref,
            metadata_client,
            transfer_client,
            retry_policy,
        })
    }

    /// Builds `<api_base><path>` — deliberately `format!`, not `Url::join`, per this crate's
    /// own documented gotcha (`NOTES.md`, `share_url.rs`'s doc comment): `api_base` never has
    /// a trailing slash, and `Url::join` on such a base silently drops its last path segment
    /// instead of appending. `path` must start with `/`. Infallible in practice: `api_base`
    /// is already a valid parsed `Url` and every caller passes a static (or UUID-interpolated,
    /// itself always URL-safe) literal path.
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("{}{path}", self.api_base))
            .expect("api_base + a static path suffix is always a valid URL")
    }

    /// E1 — `GET /server/version`. Unauthenticated: no [`ShareRef`] applied.
    pub async fn server_version(&self) -> Result<Version, ExportError> {
        let url = self.url("/server/version");
        let dto: dto::ServerVersionResponseDto = send_json(
            &self.retry_policy,
            "export_server_version",
            Method::GET,
            &url,
            || self.metadata_client.get(url.clone()),
        )
        .await?;
        Ok(dto.into())
    }

    /// E2 — `POST /shared-links/login`. Only meaningful to call when a password is
    /// configured (`PLAN.md` §5 step 5). Never logs `password` — it is only ever placed in
    /// the JSON request body, which nothing in [`crate::immich`]'s logging touches (`debug`
    /// logs method/URL/status only; `trace` logs *response* bodies, not requests). On
    /// success, the server's `Set-Cookie: immich_shared_link_token=…` is retained by
    /// `metadata_client`'s cookie store (`cookie_store(true)`, set in [`Self::new`]) and
    /// rides along on every subsequent request that client makes, including
    /// [`Self::shared_link_me`] — verified end to end by this module's own tests.
    pub async fn login(&self, password: &Secret) -> Result<LoginOutcome, ExportError> {
        let url = self.share_ref.apply(self.url("/shared-links/login"));
        let body = dto::SharedLinkLoginDto {
            password: password.expose().to_owned(),
        };
        let result: Result<dto::SharedLinkResponseDto, ApiError> = send_json(
            &self.retry_policy,
            "shared_link_login",
            Method::POST,
            &url,
            || self.metadata_client.post(url.clone()).json(&body),
        )
        .await;

        match result {
            Ok(_) => Ok(LoginOutcome::LoggedIn),
            Err(err) if err.status() == Some(StatusCode::BAD_REQUEST) => {
                warn!(
                    "shared-link login returned 400 Bad Request: this link is not \
                     password-protected; continuing without a password"
                );
                Ok(LoginOutcome::NotPasswordProtected)
            }
            Err(err) if err.status() == Some(StatusCode::UNAUTHORIZED) => {
                Err(ExportError::WrongPassword)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// E3 — `GET /shared-links/me`. The one endpoint that actually enforces a share link's
    /// password (`RESEARCH.md`), so calling this after [`Self::login`] is what proves the
    /// login cookie took effect.
    pub async fn shared_link_me(&self) -> Result<dto::SharedLinkResponseDto, ExportError> {
        let url = self.share_ref.apply(self.url("/shared-links/me"));
        let dto = send_json(
            &self.retry_policy,
            "shared_link_me",
            Method::GET,
            &url,
            || self.metadata_client.get(url.clone()),
        )
        .await?;
        Ok(dto)
    }

    /// E4 — `POST /search/metadata`, paginated internally (`PLAN.md` §6 step 1: size 250,
    /// `order: asc`, looping until `nextPage` is `null`) until every asset in `album_id` has
    /// been collected. Assets with an empty checksum are logged at `error` and excluded (see
    /// [`EmptyChecksumError`]) rather than failing the whole run.
    ///
    /// Two independent guards against a misbehaving server turning this into an infinite
    /// loop: [`MAX_SEARCH_PAGES`] caps the total number of pages followed, and — since
    /// `nextPage` is spec-typed as an opaque nullable string rather than a number
    /// (`NOTES.md`) and this method always sends its own incrementing `page` counter rather
    /// than trusting the token's content — two consecutive pages returning the exact same
    /// `nextPage` value is treated as "the server isn't making progress" and aborts
    /// immediately via [`ExportError::PaginationStalled`], well before the page cap would
    /// ever be hit in practice.
    pub async fn list_album_assets(&self, album_id: Uuid) -> Result<Vec<SourceAsset>, ExportError> {
        let url = self.share_ref.apply(self.url("/search/metadata"));
        let mut assets = Vec::new();
        let mut page: u32 = 1;
        let mut previous_next_page: Option<String> = None;

        loop {
            if page > MAX_SEARCH_PAGES {
                return Err(ExportError::TooManyPages { album_id });
            }

            let body = dto::MetadataSearchDto {
                album_ids: vec![album_id],
                page,
                size: SEARCH_PAGE_SIZE,
                order: dto::AssetOrder::Asc,
            };
            let response: dto::SearchResponseDto = send_json(
                &self.retry_policy,
                "search_metadata",
                Method::POST,
                &url,
                || self.metadata_client.post(url.clone()).json(&body),
            )
            .await?;

            debug!(
                "search/metadata page fetched album_id={album_id} page={page} items={} \
                 total={} next_page={}",
                response.assets.items.len(),
                response.assets.total,
                response.assets.next_page.as_deref().unwrap_or("(none)"),
            );

            for item in response.assets.items {
                match SourceAsset::try_from(item) {
                    Ok(asset) => assets.push(asset),
                    Err(err) => error!(
                        "skipping asset album_id={album_id}: {}",
                        crate::format_error_chain_dyn(&err)
                    ),
                }
            }

            match response.assets.next_page {
                None => break,
                Some(next) => {
                    if previous_next_page.as_deref() == Some(next.as_str()) {
                        return Err(ExportError::PaginationStalled { album_id });
                    }
                    previous_next_page = Some(next);
                    page += 1;
                }
            }
        }

        Ok(assets)
    }

    /// E5 — `GET /assets/{id}/original`, streamed straight into `writer` while a SHA-1
    /// hasher is fed the same chunks, so `PLAN.md` §6 step 3a's "download and hash" can
    /// happen in one pass without ever buffering the whole (potentially multi-gigabyte)
    /// asset in memory. The returned [`DownloadOutcome::checksum_sha1_base64`] is for the
    /// *caller* to compare against the source asset's own checksum (`PLAN.md` §6 step 3b) —
    /// this function only downloads and hashes, it does not know what checksum to expect.
    ///
    /// **Not retried internally.** The generic [`crate::retry::retry`] helper retries by
    /// calling a factory closure again, but it has no way to undo bytes this function has
    /// already written to `writer` — a retry that resumed mid-stream, or appended without
    /// truncating, would silently corrupt the output. Safe retrying requires a *clean*
    /// writer for each attempt (a fresh `NamedTempFile`, or the same file
    /// truncated-and-seeked-to-0), which only the caller can arrange — `sync.rs` (a later
    /// step) is expected to be the retry loop here, opening a fresh temp file per attempt.
    /// A transport error or a `5xx` still surfaces as a normal (retryable, per
    /// [`crate::retry::Retryable`]) [`ApiError`] for that caller to act on.
    pub async fn download_original(
        &self,
        asset_id: Uuid,
        writer: &mut (impl AsyncWrite + Unpin),
    ) -> Result<DownloadOutcome, ExportError> {
        let url = self
            .share_ref
            .apply(self.url(&format!("/assets/{asset_id}/original")));
        let request = self.transfer_client.get(url.clone());
        let response = execute_once(Method::GET, &url, request).await?;
        let status = response.status();

        if !status.is_success() {
            // Reuses `parse_json_response`'s error-body parsing (Immich's `{message,error,
            // statusCode}` shape, or a raw-text fallback) without duplicating it. `T` is
            // irrelevant here since a non-2xx response never reaches the decode step; the
            // `expect_err` is safe precisely because `status` was just checked above.
            let err = parse_json_response::<serde_json::Value>(Method::GET, &url, response)
                .await
                .expect_err("status already checked non-success");
            return Err(match status {
                StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    ExportError::DownloadForbidden {
                        asset_id,
                        status,
                        source: err,
                    }
                }
                _ => err.into(),
            });
        }

        let mut hasher = Sha1::new();
        let mut bytes_written: u64 = 0;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| ApiError::Transport {
                method: Method::GET,
                url: immich::redact_url(&url),
                source: source.without_url(),
            })?;
            hasher.update(&chunk);
            writer
                .write_all(&chunk)
                .await
                .map_err(|source| ExportError::Io { asset_id, source })?;
            bytes_written =
                bytes_written.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        }
        writer
            .flush()
            .await
            .map_err(|source| ExportError::Io { asset_id, source })?;

        Ok(DownloadOutcome {
            bytes_written,
            checksum_sha1_base64: BASE64.encode(hasher.finalize()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Json;
    use axum::Router;
    use axum::extract::{Path, Query};
    use axum::http::header;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::collections::HashMap;

    use crate::immich::test_support::spawn_test_server;
    use crate::retry::RetryPolicy;

    use super::*;

    /// Turns [`spawn_test_server`]'s base URL (`http://host:port/`) into something shaped
    /// like a real `api_base` (`http://host:port/api`, no trailing slash) — mirroring
    /// production, where `share_url::parse_share_url` always appends a real `/api` path
    /// segment. This matters for more than cosmetics: a **bare-origin** `Url` (no path
    /// segments at all) always round-trips through `Url::as_str()` with a trailing `/`
    /// (`url` normalises an empty path to `/`), which would silently double every slash this
    /// module's own `ExportClient::url` builds via `format!("{api_base}{path}")`. Test
    /// routers are nested under `/api` (see every `spawn_test_server(Router::new().nest(...))`
    /// call below) to match.
    fn api_base(server_base: &Url) -> Url {
        Url::parse(&format!("{server_base}api")).unwrap()
    }

    fn client(server_base: &Url, share_ref: ShareRef) -> ExportClient {
        ExportClient::new(
            api_base(server_base),
            share_ref,
            Duration::from_secs(5),
            Duration::from_secs(5),
            RetryPolicy::zero_delay(),
        )
        .unwrap()
    }

    fn key_ref() -> ShareRef {
        ShareRef::Key("test-key".to_owned())
    }

    fn secret(value: &str) -> Secret {
        value.parse().unwrap()
    }

    // ---- E1: server_version ------------------------------------------------------------

    #[tokio::test]
    async fn server_version_parses() {
        let app = Router::new().route(
            "/server/version",
            get(|| async { Json(json!({"major": 3, "minor": 1, "patch": 0, "prerelease": null})) }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let version = export.server_version().await.unwrap();
        assert_eq!(
            version,
            Version {
                major: 3,
                minor: 1,
                patch: 0
            }
        );
    }

    // ---- E2: login -----------------------------------------------------------------------

    fn shared_link_json(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "type": "ALBUM",
            "album": {"id": "9c858901-8a57-4791-81fe-4c455b099bc9", "albumName": "Holiday", "assetCount": 3},
            "allowDownload": true,
            "allowUpload": false,
            "showMetadata": true,
            "expiresAt": null
        })
    }

    #[tokio::test]
    async fn login_success_sets_a_cookie_that_survives_to_the_next_request() {
        let app = Router::new()
            .route(
                "/shared-links/login",
                post(|| async {
                    (
                        StatusCode::CREATED,
                        [(
                            header::SET_COOKIE,
                            "immich_shared_link_token=abc123; Path=/",
                        )],
                        Json(shared_link_json("3fa85f64-5717-4562-b3fc-2c963f66afa6")),
                    )
                }),
            )
            .route(
                "/shared-links/me",
                get(|headers: axum::http::HeaderMap| async move {
                    let cookie = headers
                        .get(header::COOKIE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    if cookie.contains("immich_shared_link_token=abc123") {
                        (
                            StatusCode::OK,
                            Json(shared_link_json("3fa85f64-5717-4562-b3fc-2c963f66afa6")),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"statusCode": 401, "message": "Password required"})),
                        )
                    }
                }),
            );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let outcome = export
            .login(&secret("hunter2"))
            .await
            .expect("login should succeed");
        assert_eq!(outcome, LoginOutcome::LoggedIn);

        // The cookie from `login` must have been retained and sent along automatically.
        let me = export
            .shared_link_me()
            .await
            .expect("cookie must have been carried over");
        assert_eq!(me.r#type, dto::SharedLinkType::Album);
    }

    #[tokio::test]
    async fn login_wrong_password_is_401_and_a_hard_error() {
        let app = Router::new().route(
            "/shared-links/login",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"statusCode": 401, "message": "Invalid password"})),
                )
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let err = export.login(&secret("wrong")).await.unwrap_err();
        assert!(matches!(err, ExportError::WrongPassword));
    }

    #[tokio::test]
    async fn login_not_password_protected_is_400_and_a_warning_not_an_error() {
        let app = Router::new().route(
            "/shared-links/login",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"statusCode": 400, "message": "Shared link is not password protected"})),
                )
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let outcome = export
            .login(&secret("whatever"))
            .await
            .expect("400 must be Ok(NotPasswordProtected), not an error");
        assert_eq!(outcome, LoginOutcome::NotPasswordProtected);
    }

    // ---- E3: shared_link_me ----------------------------------------------------------------

    #[tokio::test]
    async fn shared_link_me_parses_album_and_key_query_param() {
        let seen_key = Arc::new(std::sync::Mutex::new(None));
        let seen_key_for_handler = seen_key.clone();
        let app = Router::new().route(
            "/shared-links/me",
            get(move |Query(params): Query<HashMap<String, String>>| {
                let seen_key = seen_key_for_handler.clone();
                async move {
                    *seen_key.lock().unwrap() = params.get("key").cloned();
                    Json(shared_link_json("3fa85f64-5717-4562-b3fc-2c963f66afa6"))
                }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let me = export.shared_link_me().await.unwrap();
        assert_eq!(me.r#type, dto::SharedLinkType::Album);
        let album = me.album.unwrap();
        assert_eq!(album.album_name, "Holiday");
        assert_eq!(seen_key.lock().unwrap().as_deref(), Some("test-key"));
    }

    // ---- E4: list_album_assets (paginated) --------------------------------------------------

    fn asset_json(n: u32) -> serde_json::Value {
        json!({
            "id": format!("00000000-0000-0000-0000-{:012}", n),
            "checksum": format!("checksum-{n}="),
            "originalFileName": format!("IMG_{n}.jpg"),
            "type": "IMAGE",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "originalMimeType": "image/jpeg",
            "duration": null
        })
    }

    #[tokio::test]
    async fn list_album_assets_follows_three_pages() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new().route(
            "/search/metadata",
            post(move |Json(body): Json<serde_json::Value>| {
                let calls = calls_for_handler.clone();
                async move {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    let page = body["page"].as_u64().unwrap();
                    assert_eq!(page, u64::from(call) + 1, "must send its own incrementing page counter");
                    let (items, next_page) = match page {
                        1 => (vec![asset_json(1), asset_json(2)], Some("2")),
                        2 => (vec![asset_json(3)], Some("3")),
                        3 => (vec![asset_json(4)], None),
                        _ => panic!("unexpected page {page}"),
                    };
                    Json(json!({
                        "albums": {"total": 0, "items": []},
                        "assets": {"items": items, "nextPage": next_page, "total": 4, "count": items.len()}
                    }))
                }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let assets = export
            .list_album_assets(Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap())
            .await
            .unwrap();

        assert_eq!(assets.len(), 4);
        assert_eq!(assets[0].filename, "IMG_1.jpg");
        assert_eq!(assets[3].filename, "IMG_4.jpg");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn list_album_assets_skips_empty_checksum_assets_without_failing_the_page() {
        let app = Router::new().route(
            "/search/metadata",
            post(|| async {
                let mut bad = asset_json(1);
                bad["checksum"] = json!("");
                Json(json!({
                    "albums": {"total": 0, "items": []},
                    "assets": {"items": [bad, asset_json(2)], "nextPage": null, "total": 2, "count": 2}
                }))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let assets = export
            .list_album_assets(Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap())
            .await
            .unwrap();

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].filename, "IMG_2.jpg");
    }

    #[tokio::test]
    async fn list_album_assets_aborts_when_next_page_stalls() {
        let app = Router::new().route(
            "/search/metadata",
            post(|| async {
                Json(json!({
                    "albums": {"total": 0, "items": []},
                    "assets": {"items": [asset_json(1)], "nextPage": "stuck", "total": 99, "count": 1}
                }))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let err = export
            .list_album_assets(Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, ExportError::PaginationStalled { .. }));
    }

    // ---- E5: download_original --------------------------------------------------------------

    #[tokio::test]
    async fn download_original_streams_correct_bytes_and_checksum() {
        let body = b"hello immich world".to_vec();
        let app = Router::new().route(
            "/assets/{id}/original",
            get(move |Path(_id): Path<String>| {
                let body = body.clone();
                async move { (StatusCode::OK, body) }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let mut buf: Vec<u8> = Vec::new();
        let outcome = export
            .download_original(
                Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap(),
                &mut buf,
            )
            .await
            .unwrap();

        assert_eq!(buf, b"hello immich world");
        assert_eq!(outcome.bytes_written, 18);
        let mut hasher = Sha1::new();
        hasher.update(b"hello immich world");
        assert_eq!(
            outcome.checksum_sha1_base64,
            BASE64.encode(hasher.finalize())
        );
    }

    #[tokio::test]
    async fn download_original_401_names_allow_download_as_the_likely_cause() {
        let app = Router::new().route(
            "/assets/{id}/original",
            get(|Path(_id): Path<String>| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"statusCode": 401, "message": "Unauthorized"})),
                )
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let export = client(&base, key_ref());

        let mut buf: Vec<u8> = Vec::new();
        let err = export
            .download_original(
                Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap(),
                &mut buf,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ExportError::DownloadForbidden {
                status: StatusCode::UNAUTHORIZED,
                ..
            }
        ));
    }

    // ---- SourceAsset::checksum_is_path_hash ---------------------------------------------

    /// Builds a bare-bones `SourceAsset` for exercising `checksum_is_path_hash` in
    /// isolation — the fields other than `checksum`/`original_path` don't matter to it.
    fn source_asset(checksum: &str, original_path: Option<&str>) -> SourceAsset {
        SourceAsset {
            id: Uuid::nil(),
            checksum: checksum.to_owned(),
            filename: "irrelevant.jpg".to_owned(),
            mime: None,
            created: Utc::now(),
            modified: Utc::now(),
            r#type: dto::AssetTypeEnum::Image,
            duration: None,
            original_path: original_path.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn checksum_is_path_hash_true_for_a_real_verified_vector() {
        // Verified against a live Immich 3.1.0 instance: this exact (originalPath,
        // checksum) pair is a real external-library asset's path hash.
        let asset = source_asset(
            "97NskcQtqUhXp7Pqxa80qkeo0TA=",
            Some(
                "/data/fotos/PROJECTS/2025-07-18-SchwedenUrlaub/20250718_200917.324_Pixel 9 Pro.MP.jpg",
            ),
        );
        assert!(asset.checksum_is_path_hash());
    }

    #[test]
    fn checksum_is_path_hash_false_for_a_content_hash() {
        // Same path as the verified vector above, but a checksum that is not its path
        // hash — e.g. a genuine content hash of the file's bytes.
        let asset = source_asset(
            "BZm8Ilo+YBCs4cEz7mB5nV2hyQI=",
            Some(
                "/data/fotos/PROJECTS/2025-07-18-SchwedenUrlaub/20250718_200917.324_Pixel 9 Pro.MP.jpg",
            ),
        );
        assert!(!asset.checksum_is_path_hash());
    }

    #[test]
    fn checksum_is_path_hash_false_when_original_path_is_none() {
        // No originalPath to recompute against at all — must not panic, must not guess.
        let asset = source_asset("97NskcQtqUhXp7Pqxa80qkeo0TA=", None);
        assert!(!asset.checksum_is_path_hash());
    }
}
