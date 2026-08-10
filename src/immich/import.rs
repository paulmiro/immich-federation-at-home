//! `ImportClient` — the API-key-authenticated side of the API surface (`PLAN.md` §2, calls
//! I1–I6): version/permission probes, target-album resolution, the bulk-upload-check dedup
//! oracle, the streaming multipart upload, and idempotent album membership.
//!
//! Every request carries the API key as an `x-api-key` **default header**, baked into both
//! `reqwest::Client`s at construction time in [`ImportClient::new`] — never a query
//! parameter, so it can neither be forgotten on a new call site nor leak into a logged URL
//! (`PLAN.md` §8, [`crate::immich::redact_url`] only ever needed to cover *query* params).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::multipart::{Form, Part};
use reqwest::{Body, Client, Method, StatusCode};
use thiserror::Error;
use tokio_util::io::ReaderStream;
use url::Url;
use uuid::Uuid;

use crate::config::{AlbumRef, Secret};
use crate::debug;
use crate::immich::{
    ApiError, Version, build_client, dto, execute_once, parse_json_response, send_json,
};
use crate::retry::{self, RetryPolicy, Retryable};

/// `PLAN.md` §6 step 2: `POST /assets/bulk-upload-check` is chunked into groups of this size.
pub const BULK_CHECK_CHUNK_SIZE: usize = 500;

/// `PLAN.md` §6 step 4: `PUT /albums/{id}/assets` is chunked into groups of this size.
pub const ALBUM_ADD_CHUNK_SIZE: usize = 500;

/// Everything that can go wrong on the import side, beyond a plain [`ApiError`]. Kept
/// distinct so callers (`main.rs`, `sync.rs` — later steps) can match on import-specific,
/// actionable cases (album resolution failures, a local file-open failure) without parsing
/// status codes or `std::io::Error` kinds themselves.
#[derive(Debug, Error)]
pub enum ImportError {
    #[error(transparent)]
    Api(#[from] ApiError),

    /// I3, UUID form: `GET /albums/{id}` came back `400` or `404`. Both are folded into the
    /// same "does not exist" message — `PLAN.md` §5 step 9 doesn't distinguish them, and
    /// there is no other plausible reason `GET /albums/{id}` would 400 on a syntactically
    /// valid UUID.
    #[error("import album {id} does not exist")]
    AlbumIdNotFound { id: Uuid },

    /// I3, name form, zero matches: `PLAN.md` §5 step 9 wants "an error listing nearby
    /// names" — `available` is the full `GET /albums` listing (a second call, deliberately;
    /// see [`ImportClient::resolve_album`]'s doc comment) joined for display.
    #[error("no album named {name:?} exists on the import instance; albums found: {available}")]
    AlbumNameNotFound { name: String, available: String },

    /// I3, name form, more than one exact match: `matches` lists `"name (id)"` for each hit
    /// so the operator can pick a UUID and switch `IMPORT_ALBUM` to it.
    #[error("album name {name:?} is ambiguous on the import instance; matches: {matches}")]
    AlbumNameAmbiguous { name: String, matches: String },

    /// I5: the asset file couldn't be (re)opened for upload — a local problem (the temp file
    /// was already cleaned up, a permissions issue, …), not anything the import server did.
    /// Deliberately **not** retryable (see [`Retryable`] below): retrying an open that just
    /// failed against the same path can't succeed differently.
    #[error("failed to open {path} for upload")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Lets [`ImportError`] be driven through [`crate::retry::retry`] directly (used by
/// [`ImportClient::upload_asset`]): an [`ImportError::Api`] defers to the wrapped
/// [`ApiError`]'s own classification (connection errors/timeouts/`429`/`5xx`), while
/// [`ImportError::Io`] and the album-resolution variants are never retryable — none of them
/// can succeed differently on an identical retry.
impl Retryable for ImportError {
    fn is_retryable(&self) -> bool {
        matches!(self, ImportError::Api(err) if err.is_retryable())
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            ImportError::Api(err) => err.retry_after(),
            _ => None,
        }
    }
}

/// The classified outcome of one item from [`ImportClient::check_bulk_upload`] (I4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkUploadCheckOutcome {
    /// Not present on the import instance by checksum — safe to upload.
    Accept,
    /// Already present (or unsupported) on the import instance; `asset_id` is the
    /// *existing* import-side asset's UUID when the reason is a duplicate.
    Reject {
        reason: Option<dto::AssetRejectReason>,
        asset_id: Option<Uuid>,
        is_trashed: bool,
    },
}

/// The classified outcome of [`ImportClient::add_assets_to_album`] (I6): `PLAN.md` says
/// already-present assets come back `{success:false, error:"duplicate"}` and that is **not**
/// a failure, so this splits the raw per-item results into the three cases a caller actually
/// cares about instead of making it re-derive them from [`dto::BulkIdResponseDto`] itself.
#[derive(Debug, Clone, Default)]
pub struct AlbumAddOutcome {
    pub added: Vec<Uuid>,
    pub already_present: Vec<Uuid>,
    pub failed: Vec<(Uuid, Option<dto::BulkIdErrorReason>)>,
}

/// Everything [`ImportClient::upload_asset`] (I5) needs about one asset, bundled into a
/// struct rather than six positional arguments (clippy's `too_many_arguments` territory, and
/// harder to misorder by accident). `file_path` is a **path**, not an already-open file
/// handle — deliberately, so a retried upload attempt can reopen it fresh; see
/// [`ImportClient::upload_asset`]'s doc comment.
pub struct UploadRequest<'a> {
    pub file_path: &'a Path,
    pub filename: &'a str,
    pub file_created_at: DateTime<Utc>,
    pub file_modified_at: DateTime<Utc>,
    /// Milliseconds; `None` for anything that isn't a video (`PLAN.md` I5 request detail:
    /// "only for videos"). Since [`crate::immich::export::SourceAsset::duration`] is already
    /// `None` for every non-video asset (verified against the spec — see `NOTES.md`),
    /// callers can pass that value straight through without an extra "is this a video" check.
    pub duration_ms: Option<i64>,
    /// SHA-1 of `file_path`'s contents, base64-encoded — sent as the `x-immich-checksum`
    /// header. See [`ImportClient::upload_asset`]'s doc comment for why base64 (not hex) was
    /// chosen, and `NOTES.md` for the full reasoning.
    pub checksum_sha1_base64: &'a str,
}

/// The API-key-authenticated Immich client (`PLAN.md` §2's import side, I1–I6).
///
/// Like [`crate::immich::export::ExportClient`], holds two `reqwest::Client`s — one for
/// metadata calls (`REQUEST_TIMEOUT`), one for the multipart upload (`TRANSFER_TIMEOUT`) —
/// both with the `x-api-key` header baked in via `default_headers` (see
/// [`crate::immich::build_client`]). Neither needs a cookie store: API-key auth carries no
/// session state.
pub struct ImportClient {
    api_base: Url,
    metadata_client: Client,
    transfer_client: Client,
    retry_policy: RetryPolicy,
}

impl ImportClient {
    pub fn new(
        api_base: Url,
        api_key: &Secret,
        request_timeout: Duration,
        transfer_timeout: Duration,
        retry_policy: RetryPolicy,
    ) -> anyhow::Result<Self> {
        let mut api_key_value = HeaderValue::from_str(api_key.expose()).context(
            "IMPORT_API_KEY contains characters that are not valid in an HTTP header value",
        )?;
        // Keeps the key out of reqwest/hyper's own `Debug` output for the header map, on top
        // of this crate never logging it directly — belt and suspenders for a secret that
        // rides on every single request.
        api_key_value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", api_key_value);

        let metadata_client = build_client(request_timeout, false, headers.clone())
            .context("failed to build the import-side metadata HTTP client")?;
        let transfer_client = build_client(transfer_timeout, false, headers)
            .context("failed to build the import-side transfer HTTP client")?;

        Ok(Self {
            api_base,
            metadata_client,
            transfer_client,
            retry_policy,
        })
    }

    /// Builds `<api_base><path>` — see [`crate::immich::export::ExportClient::url`]'s doc
    /// comment for why this is `format!`, not `Url::join`. Infallible in practice for the
    /// same reason.
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("{}{path}", self.api_base))
            .expect("api_base + a static path suffix is always a valid URL")
    }

    /// I1 — `GET /server/version`.
    pub async fn server_version(&self) -> Result<Version, ApiError> {
        let url = self.url("/server/version");
        let dto: dto::ServerVersionResponseDto = send_json(
            &self.retry_policy,
            "import_server_version",
            Method::GET,
            &url,
            || self.metadata_client.get(url.clone()),
        )
        .await?;
        Ok(dto.into())
    }

    /// I2 — `GET /api-keys/me`. No permission is required to call this endpoint itself
    /// (`PLAN.md` §2); the actual subset check against `PLAN.md` §5 step 8's required
    /// permissions is [`dto::ApiKeyResponseDto::missing_permissions`], already written in
    /// task 3/4 — this method's only job is fetching the DTO for the caller to check.
    pub async fn get_api_key(&self) -> Result<dto::ApiKeyResponseDto, ApiError> {
        let url = self.url("/api-keys/me");
        send_json(&self.retry_policy, "get_api_key", Method::GET, &url, || {
            self.metadata_client.get(url.clone())
        })
        .await
    }

    /// I3 — resolves `IMPORT_ALBUM` per `PLAN.md` §5 step 9. Never creates the album.
    pub async fn resolve_album(
        &self,
        album_ref: &AlbumRef,
    ) -> Result<dto::AlbumResponseDto, ImportError> {
        match album_ref {
            AlbumRef::Id(id) => self.get_album_by_id(*id).await,
            AlbumRef::Name(name) => self.get_album_by_name(name).await,
        }
    }

    async fn get_album_by_id(&self, id: Uuid) -> Result<dto::AlbumResponseDto, ImportError> {
        let url = self.url(&format!("/albums/{id}"));
        let result: Result<dto::AlbumResponseDto, ApiError> = send_json(
            &self.retry_policy,
            "get_album_by_id",
            Method::GET,
            &url,
            || self.metadata_client.get(url.clone()),
        )
        .await;

        match result {
            Ok(album) => Ok(album),
            Err(err)
                if matches!(
                    err.status(),
                    Some(StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND)
                ) =>
            {
                Err(ImportError::AlbumIdNotFound { id })
            }
            Err(err) => Err(err.into()),
        }
    }

    /// `GET /albums?name=…` **plus a client-side exact-match filter**, since the server's own
    /// matching may be looser than an exact match (`NOTES.md`). On zero matches, fetches the
    /// *full* album list (a second, deliberate call) purely to build a useful "here's what
    /// does exist" error message — `PLAN.md` explicitly calls the extra round-trip worth it.
    async fn get_album_by_name(&self, name: &str) -> Result<dto::AlbumResponseDto, ImportError> {
        let mut url = self.url("/albums");
        url.query_pairs_mut().append_pair("name", name);
        let albums: Vec<dto::AlbumResponseDto> = send_json(
            &self.retry_policy,
            "get_albums_by_name",
            Method::GET,
            &url,
            || self.metadata_client.get(url.clone()),
        )
        .await?;

        let mut matches: Vec<dto::AlbumResponseDto> = albums
            .into_iter()
            .filter(|album| album.album_name == name)
            .collect();

        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => {
                let all = self.list_all_albums().await?;
                let available = if all.is_empty() {
                    "(none)".to_owned()
                } else {
                    all.iter()
                        .map(|a| a.album_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                Err(ImportError::AlbumNameNotFound {
                    name: name.to_owned(),
                    available,
                })
            }
            _ => {
                let listing = matches
                    .iter()
                    .map(|a| format!("{} ({})", a.album_name, a.id))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(ImportError::AlbumNameAmbiguous {
                    name: name.to_owned(),
                    matches: listing,
                })
            }
        }
    }

    async fn list_all_albums(&self) -> Result<Vec<dto::AlbumResponseDto>, ApiError> {
        let url = self.url("/albums");
        send_json(
            &self.retry_policy,
            "list_all_albums",
            Method::GET,
            &url,
            || self.metadata_client.get(url.clone()),
        )
        .await
    }

    /// I4 — `POST /assets/bulk-upload-check`, chunked internally into groups of
    /// [`BULK_CHECK_CHUNK_SIZE`] (`PLAN.md` §6 step 2). Returns every input item's outcome
    /// keyed by the `id` string that was sent for it (the spec's own correlation mechanism —
    /// see `dto::AssetBulkUploadCheckItem`'s doc comment), so the caller (`sync.rs`) can map
    /// results back to the `SourceAsset`s it built the request from.
    pub async fn check_bulk_upload(
        &self,
        items: &[dto::AssetBulkUploadCheckItem],
    ) -> Result<HashMap<String, BulkUploadCheckOutcome>, ApiError> {
        let url = self.url("/assets/bulk-upload-check");
        let mut outcomes = HashMap::with_capacity(items.len());

        for chunk in items.chunks(BULK_CHECK_CHUNK_SIZE) {
            let body = dto::AssetBulkUploadCheckDto {
                assets: chunk.to_vec(),
            };
            debug!("bulk-upload-check chunk chunk_size={}", chunk.len());
            let response: dto::AssetBulkUploadCheckResponseDto = send_json(
                &self.retry_policy,
                "bulk_upload_check",
                Method::POST,
                &url,
                || self.metadata_client.post(url.clone()).json(&body),
            )
            .await?;

            for result in response.results {
                let outcome = match result.action {
                    dto::AssetUploadAction::Accept => BulkUploadCheckOutcome::Accept,
                    dto::AssetUploadAction::Reject | dto::AssetUploadAction::Unrecognized => {
                        BulkUploadCheckOutcome::Reject {
                            reason: result.reason,
                            asset_id: result.asset_id,
                            is_trashed: result.is_trashed.unwrap_or(false),
                        }
                    }
                };
                outcomes.insert(result.id, outcome);
            }
        }

        Ok(outcomes)
    }

    /// I5 — `POST /assets`, a streaming multipart upload. Parts sent: `assetData` (the file,
    /// streamed — never buffered whole into memory — with `filename` and a guessed
    /// `Content-Type`), `filename`, `fileCreatedAt`/`fileModifiedAt` (RFC 3339 with
    /// millisecond precision and a `Z` suffix, matching `AssetMediaCreateDto`'s spec pattern
    /// exactly — verified with `jq` against `openapi/immich-openapi-3.1.0.json`), and
    /// `duration` (milliseconds, text, only when [`UploadRequest::duration_ms`] is `Some`).
    /// The `x-immich-checksum` header carries `checksum_sha1_base64` — **base64**, not hex:
    /// the spec only documents the header as "sha1 checksum" without naming an encoding, but
    /// every other checksum this tool touches (`AssetResponseDto.checksum`,
    /// `bulk-upload-check`'s request/response) is base64, `fromChecksum`-style parsing
    /// (`utils/request.ts`, vendored in `scratch/immich/`) auto-detects base64 vs. hex by
    /// length either way, and reusing the exact base64 string
    /// [`crate::immich::export::ExportClient::download_original`] already computed avoids a
    /// pointless re-encode. See `NOTES.md` for the full reasoning trail.
    ///
    /// Both `201 {status:"created"}` and `200 {status:"duplicate"}` are success — nothing
    /// special-cases them here, since [`crate::immich::parse_json_response`] already treats
    /// any 2xx as success and decodes the body the same way regardless of the exact code.
    ///
    /// **Retried, safely, by reopening the file from `file_path` on every attempt** — unlike
    /// [`crate::immich::export::ExportClient::download_original`] (which cannot safely retry
    /// because it cannot undo bytes already written to an output writer), a re-upload of the
    /// same file cannot corrupt anything: the file is read-only input here, so a fresh
    /// `tokio::fs::File::open` + a fresh [`reqwest::multipart::Form`] genuinely recreates the
    /// whole request body each attempt (verified by this module's own
    /// `upload_asset_retries_reopening_the_file_each_attempt` test, which fails the first
    /// attempt and asserts the retried request still carries the full body). It is also
    /// *idempotent* on the server: `asset-media.service.ts`'s `uploadAsset` catches a unique
    /// checksum-constraint violation and returns `{status:"duplicate"}` instead of erroring
    /// (`scratch/immich/asset-media.service.ts`), so even a retry after a successful-but-lost
    /// response lands as a harmless "duplicate" rather than a second copy of the asset.
    pub async fn upload_asset(
        &self,
        req: &UploadRequest<'_>,
    ) -> Result<dto::AssetMediaResponseDto, ImportError> {
        let url = self.url("/assets");
        retry::retry(&self.retry_policy, "upload_asset", || {
            self.upload_asset_once(req, &url)
        })
        .await
    }

    async fn upload_asset_once(
        &self,
        req: &UploadRequest<'_>,
        url: &Url,
    ) -> Result<dto::AssetMediaResponseDto, ImportError> {
        let file = tokio::fs::File::open(req.file_path)
            .await
            .map_err(|source| ImportError::Io {
                path: req.file_path.to_path_buf(),
                source,
            })?;
        let length = file
            .metadata()
            .await
            .map_err(|source| ImportError::Io {
                path: req.file_path.to_path_buf(),
                source,
            })?
            .len();

        let mime = mime_guess::from_path(req.file_path).first_or_octet_stream();
        let body = Body::wrap_stream(ReaderStream::new(file));
        let asset_part = Part::stream_with_length(body, length)
            .file_name(req.filename.to_owned())
            .mime_str(mime.essence_str())
            .expect("mime_guess always returns a syntactically valid MIME essence string");

        let mut form = Form::new()
            .part("assetData", asset_part)
            .text("filename", req.filename.to_owned())
            .text(
                "fileCreatedAt",
                req.file_created_at
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
            )
            .text(
                "fileModifiedAt",
                req.file_modified_at
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
            );
        if let Some(duration_ms) = req.duration_ms {
            form = form.text("duration", duration_ms.to_string());
        }

        let request = self
            .transfer_client
            .post(url.clone())
            .multipart(form)
            .header("x-immich-checksum", req.checksum_sha1_base64);

        let response = execute_once(Method::POST, url, request).await?;
        let media = parse_json_response(Method::POST, url, response).await?;
        Ok(media)
    }

    /// I6 — `PUT /albums/{id}/assets`, chunked internally into groups of
    /// [`ALBUM_ADD_CHUNK_SIZE`] (`PLAN.md` §6 step 4). Idempotent on the server: an
    /// already-present asset comes back `{success:false, error:"duplicate"}`, which this
    /// classifies into [`AlbumAddOutcome::already_present`] rather than
    /// [`AlbumAddOutcome::failed`] — not an error condition for the caller.
    pub async fn add_assets_to_album(
        &self,
        album_id: Uuid,
        asset_ids: &[Uuid],
    ) -> Result<AlbumAddOutcome, ApiError> {
        let url = self.url(&format!("/albums/{album_id}/assets"));
        let mut outcome = AlbumAddOutcome::default();

        for chunk in asset_ids.chunks(ALBUM_ADD_CHUNK_SIZE) {
            let body = dto::BulkIdsDto {
                ids: chunk.to_vec(),
            };
            let results: Vec<dto::BulkIdResponseDto> = send_json(
                &self.retry_policy,
                "add_assets_to_album",
                Method::PUT,
                &url,
                || self.metadata_client.put(url.clone()).json(&body),
            )
            .await?;

            for result in results {
                if result.success {
                    outcome.added.push(result.id);
                } else if result.error == Some(dto::BulkIdErrorReason::Duplicate) {
                    outcome.already_present.push(result.id);
                } else {
                    outcome.failed.push((result.id, result.error));
                }
            }
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Json;
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{Path, Query};
    use axum::http::HeaderMap as AxumHeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::{get, post, put};
    use serde_json::json;

    use crate::immich::test_support::spawn_test_server;

    use super::*;

    /// Turns [`spawn_test_server`]'s base URL (`http://host:port/`) into something shaped
    /// like a real `api_base` (`http://host:port/api`, no trailing slash) — mirroring
    /// production, where `config::normalize_server_url` always appends a real `/api` path
    /// segment. See `export.rs`'s identical helper for why a bare-origin `Url` can't just
    /// have its trailing slash trimmed away (it comes back on the next `.as_str()`). Test
    /// routers are nested under `/api` (see every `spawn_test_server(Router::new().nest(...))`
    /// call below) to match.
    fn api_base(server_base: &Url) -> Url {
        Url::parse(&format!("{server_base}api")).unwrap()
    }

    fn client(server_base: &Url) -> ImportClient {
        ImportClient::new(
            api_base(server_base),
            &secret("test-api-key"),
            Duration::from_secs(5),
            Duration::from_secs(5),
            RetryPolicy::zero_delay(),
        )
        .unwrap()
    }

    fn secret(value: &str) -> Secret {
        value.parse().unwrap()
    }

    fn rfc3339(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    // ---- I1: server_version ---------------------------------------------------------------

    #[tokio::test]
    async fn server_version_parses_and_sends_the_api_key_header() {
        let seen_key = Arc::new(Mutex::new(None));
        let seen_key_for_handler = seen_key.clone();
        let app = Router::new().route(
            "/server/version",
            get(move |headers: AxumHeaderMap| {
                let seen_key = seen_key_for_handler.clone();
                async move {
                    *seen_key.lock().unwrap() = headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    Json(json!({"major": 3, "minor": 0, "patch": 0, "prerelease": null}))
                }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let version = import.server_version().await.unwrap();
        assert_eq!(
            version,
            Version {
                major: 3,
                minor: 0,
                patch: 0
            }
        );
        assert_eq!(seen_key.lock().unwrap().as_deref(), Some("test-api-key"));
    }

    // ---- I2: get_api_key / permission check ------------------------------------------------

    fn api_key_json(permissions: &[&str]) -> serde_json::Value {
        json!({
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "name": "sync-key",
            "permissions": permissions,
            "createdAt": "2024-01-01T00:00:00.000Z",
            "updatedAt": "2024-01-01T00:00:00.000Z"
        })
    }

    #[tokio::test]
    async fn get_api_key_permission_check_passes_with_all_required_permissions() {
        let app = Router::new().route(
            "/api-keys/me",
            get(|| async {
                Json(api_key_json(&[
                    dto::PERMISSION_ASSET_UPLOAD,
                    dto::PERMISSION_ALBUM_READ,
                    dto::PERMISSION_ALBUM_ASSET_CREATE,
                ]))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let key = import.get_api_key().await.unwrap();
        assert!(
            key.missing_permissions(&dto::REQUIRED_PERMISSIONS)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn get_api_key_permission_check_reports_exact_gaps() {
        let app = Router::new().route(
            "/api-keys/me",
            get(|| async { Json(api_key_json(&[dto::PERMISSION_ASSET_UPLOAD])) }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let key = import.get_api_key().await.unwrap();
        let missing = key.missing_permissions(&dto::REQUIRED_PERMISSIONS);
        assert_eq!(
            missing,
            vec![
                dto::PERMISSION_ALBUM_READ,
                dto::PERMISSION_ALBUM_ASSET_CREATE
            ]
        );
    }

    // ---- I3: resolve_album ------------------------------------------------------------------

    #[tokio::test]
    async fn resolve_album_by_uuid() {
        let app = Router::new().route(
            "/albums/{id}",
            get(|Path(id): Path<String>| async move {
                Json(json!({"id": id, "albumName": "Holiday", "assetCount": 3}))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let id = Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap();
        let album = import.resolve_album(&AlbumRef::Id(id)).await.unwrap();
        assert_eq!(album.album_name, "Holiday");
        assert_eq!(album.id, id);
    }

    #[tokio::test]
    async fn resolve_album_by_uuid_not_found() {
        let app = Router::new().route(
            "/albums/{id}",
            get(|Path(_id): Path<String>| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"statusCode": 404, "message": "Album not found"})),
                )
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let id = Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap();
        let err = import.resolve_album(&AlbumRef::Id(id)).await.unwrap_err();
        assert!(matches!(err, ImportError::AlbumIdNotFound { id: found } if found == id));
    }

    #[tokio::test]
    async fn resolve_album_by_exact_name() {
        let app = Router::new().route(
            "/albums",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                let name = params.get("name").cloned().unwrap_or_default();
                let albums = if name == "Holiday 2026" {
                    vec![json!({"id": "9c858901-8a57-4791-81fe-4c455b099bc9", "albumName": "Holiday 2026", "assetCount": 1})]
                } else {
                    vec![]
                };
                Json(albums)
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let album = import
            .resolve_album(&AlbumRef::Name("Holiday 2026".to_owned()))
            .await
            .unwrap();
        assert_eq!(album.album_name, "Holiday 2026");
    }

    #[tokio::test]
    async fn resolve_album_by_name_not_found() {
        let app = Router::new().route(
            "/albums",
            get(|| async {
                Json(json!([
                    {"id": "11111111-1111-1111-1111-111111111111", "albumName": "Other Album", "assetCount": 2}
                ]))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let err = import
            .resolve_album(&AlbumRef::Name("Missing Album".to_owned()))
            .await
            .unwrap_err();
        match err {
            ImportError::AlbumNameNotFound { name, .. } => assert_eq!(name, "Missing Album"),
            other => panic!("expected AlbumNameNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_album_by_name_ambiguous() {
        let app = Router::new().route(
            "/albums",
            get(|| async {
                Json(json!([
                    {"id": "11111111-1111-1111-1111-111111111111", "albumName": "Holiday", "assetCount": 2},
                    {"id": "22222222-2222-2222-2222-222222222222", "albumName": "Holiday", "assetCount": 5}
                ]))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let err = import
            .resolve_album(&AlbumRef::Name("Holiday".to_owned()))
            .await
            .unwrap_err();
        match err {
            ImportError::AlbumNameAmbiguous { name, .. } => assert_eq!(name, "Holiday"),
            other => panic!("expected AlbumNameAmbiguous, got {other:?}"),
        }
    }

    // ---- I4: check_bulk_upload (chunking) ---------------------------------------------------

    #[tokio::test]
    async fn check_bulk_upload_chunks_into_groups_of_500() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new().route(
            "/assets/bulk-upload-check",
            post(move |Json(body): Json<serde_json::Value>| {
                let calls = calls_for_handler.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let assets = body["assets"].as_array().cloned().unwrap_or_default();
                    assert!(
                        assets.len() <= BULK_CHECK_CHUNK_SIZE,
                        "chunk must not exceed 500 items"
                    );
                    let results: Vec<_> = assets
                        .iter()
                        .map(|a| json!({"id": a["id"], "action": "accept"}))
                        .collect();
                    Json(json!({"results": results}))
                }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let items: Vec<dto::AssetBulkUploadCheckItem> = (0..750)
            .map(|i| dto::AssetBulkUploadCheckItem {
                id: format!("asset-{i}"),
                checksum: format!("checksum-{i}="),
            })
            .collect();

        let outcomes = import.check_bulk_upload(&items).await.unwrap();
        assert_eq!(outcomes.len(), 750);
        assert!(
            outcomes
                .values()
                .all(|o| *o == BulkUploadCheckOutcome::Accept)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "750 items at 500/chunk must be 2 requests"
        );
    }

    #[tokio::test]
    async fn check_bulk_upload_classifies_duplicate_and_unsupported_rejections() {
        let app = Router::new().route(
            "/assets/bulk-upload-check",
            post(|| async {
                Json(json!({
                    "results": [
                        {"id": "a", "action": "accept"},
                        {
                            "id": "b",
                            "action": "reject",
                            "reason": "duplicate",
                            "assetId": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
                            "isTrashed": true
                        },
                        {"id": "c", "action": "reject", "reason": "unsupported-format"}
                    ]
                }))
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let items = vec![
            dto::AssetBulkUploadCheckItem {
                id: "a".to_owned(),
                checksum: "x=".to_owned(),
            },
            dto::AssetBulkUploadCheckItem {
                id: "b".to_owned(),
                checksum: "y=".to_owned(),
            },
            dto::AssetBulkUploadCheckItem {
                id: "c".to_owned(),
                checksum: "z=".to_owned(),
            },
        ];
        let outcomes = import.check_bulk_upload(&items).await.unwrap();

        assert_eq!(outcomes["a"], BulkUploadCheckOutcome::Accept);
        match &outcomes["b"] {
            BulkUploadCheckOutcome::Reject {
                reason,
                asset_id,
                is_trashed,
            } => {
                assert_eq!(*reason, Some(dto::AssetRejectReason::Duplicate));
                assert!(asset_id.is_some());
                assert!(*is_trashed);
            }
            BulkUploadCheckOutcome::Accept => panic!("expected Reject, got Accept"),
        }
        match &outcomes["c"] {
            BulkUploadCheckOutcome::Reject {
                reason, is_trashed, ..
            } => {
                assert_eq!(*reason, Some(dto::AssetRejectReason::UnsupportedFormat));
                assert!(!*is_trashed);
            }
            BulkUploadCheckOutcome::Accept => panic!("expected Reject, got Accept"),
        }
    }

    // ---- I5: upload_asset (multipart) -------------------------------------------------------

    struct CapturedUpload {
        checksum_header: Option<String>,
        raw_body: String,
    }

    async fn spawn_capturing_upload_server() -> (
        Url,
        Arc<Mutex<Option<CapturedUpload>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let captured: Arc<Mutex<Option<CapturedUpload>>> = Arc::new(Mutex::new(None));
        let captured_for_handler = captured.clone();
        let app = Router::new().route(
            "/assets",
            post(move |headers: AxumHeaderMap, body: Bytes| {
                let captured = captured_for_handler.clone();
                async move {
                    let checksum_header = headers
                        .get("x-immich-checksum")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let raw_body = String::from_utf8_lossy(&body).into_owned();
                    *captured.lock().unwrap() = Some(CapturedUpload { checksum_header, raw_body });
                    (
                        StatusCode::CREATED,
                        Json(json!({"id": "3fa85f64-5717-4562-b3fc-2c963f66afa6", "status": "created"})),
                    )
                }
            }),
        );
        let (base, server) = spawn_test_server(Router::new().nest("/api", app)).await;
        (base, captured, server)
    }

    fn temp_jpeg(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        file.write_all(contents).unwrap();
        file
    }

    #[tokio::test]
    async fn upload_asset_sends_expected_parts_and_checksum_header() {
        let (base, captured, _server) = spawn_capturing_upload_server().await;
        let import = client(&base);
        let file = temp_jpeg(b"fake jpeg bytes");

        let req = UploadRequest {
            file_path: file.path(),
            filename: "IMG_1234.jpg",
            file_created_at: rfc3339("2026-05-01T12:00:00Z"),
            file_modified_at: rfc3339("2026-05-01T12:00:01Z"),
            duration_ms: None,
            checksum_sha1_base64: "abcDEF123=",
        };

        let response = import.upload_asset(&req).await.unwrap();
        assert_eq!(response.status, dto::AssetMediaStatus::Created);

        let captured = captured
            .lock()
            .unwrap()
            .take()
            .expect("server must have captured a request");
        assert_eq!(captured.checksum_header.as_deref(), Some("abcDEF123="));
        assert!(
            captured
                .raw_body
                .contains("name=\"assetData\"; filename=\"IMG_1234.jpg\""),
            "body was: {}",
            captured.raw_body
        );
        assert!(captured.raw_body.contains("Content-Type: image/jpeg"));
        assert!(captured.raw_body.contains("name=\"filename\""));
        assert!(captured.raw_body.contains("name=\"fileCreatedAt\""));
        assert!(captured.raw_body.contains("2026-05-01T12:00:00.000Z"));
        assert!(captured.raw_body.contains("name=\"fileModifiedAt\""));
        assert!(captured.raw_body.contains("2026-05-01T12:00:01.000Z"));
        assert!(
            !captured.raw_body.contains("name=\"duration\""),
            "no duration_ms means no duration part"
        );
        assert!(captured.raw_body.contains("fake jpeg bytes"));
    }

    #[tokio::test]
    async fn upload_asset_video_includes_duration_part() {
        let (base, captured, _server) = spawn_capturing_upload_server().await;
        let import = client(&base);
        let file = temp_jpeg(b"fake video bytes");

        let req = UploadRequest {
            file_path: file.path(),
            filename: "clip.mp4",
            file_created_at: rfc3339("2026-05-01T12:00:00Z"),
            file_modified_at: rfc3339("2026-05-01T12:00:01Z"),
            duration_ms: Some(15230),
            checksum_sha1_base64: "abc=",
        };

        import.upload_asset(&req).await.unwrap();

        let captured = captured.lock().unwrap().take().unwrap();
        assert!(captured.raw_body.contains("name=\"duration\""));
        assert!(captured.raw_body.contains("15230"));
    }

    #[tokio::test]
    async fn upload_asset_retries_reopening_the_file_each_attempt() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new().route(
            "/assets",
            post(move |body: Bytes| {
                let calls = calls_for_handler.clone();
                async move {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    let raw = String::from_utf8_lossy(&body).into_owned();
                    assert!(
                        raw.contains("retry me please"),
                        "attempt {attempt} must still carry the full recreated body"
                    );
                    if attempt == 0 {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"statusCode": 500, "message": "boom"})),
                        )
                            .into_response()
                    } else {
                        (
                            StatusCode::CREATED,
                            Json(json!({"id": "3fa85f64-5717-4562-b3fc-2c963f66afa6", "status": "created"})),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);
        let file = temp_jpeg(b"retry me please");

        let req = UploadRequest {
            file_path: file.path(),
            filename: "IMG_1234.jpg",
            file_created_at: rfc3339("2026-05-01T12:00:00Z"),
            file_modified_at: rfc3339("2026-05-01T12:00:01Z"),
            duration_ms: None,
            checksum_sha1_base64: "abc=",
        };

        let response = import
            .upload_asset(&req)
            .await
            .expect("second attempt should succeed");
        assert_eq!(response.status, dto::AssetMediaStatus::Created);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // ---- I6: add_assets_to_album -------------------------------------------------------------

    #[tokio::test]
    async fn add_assets_to_album_classifies_duplicates_as_not_an_error() {
        let app = Router::new().route(
            "/albums/{id}/assets",
            put(|Json(body): Json<serde_json::Value>| async move {
                let ids = body["ids"].as_array().cloned().unwrap_or_default();
                let results: Vec<_> = ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| {
                        if i == 0 {
                            json!({"id": id, "success": true})
                        } else {
                            json!({"id": id, "success": false, "error": "duplicate"})
                        }
                    })
                    .collect();
                Json(results)
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let ids = vec![
            Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
        ];
        let album_id = Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap();
        let outcome = import.add_assets_to_album(album_id, &ids).await.unwrap();

        assert_eq!(outcome.added, vec![ids[0]]);
        assert_eq!(outcome.already_present, vec![ids[1]]);
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn add_assets_to_album_chunks_into_groups_of_500() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new().route(
            "/albums/{id}/assets",
            put(move |Json(body): Json<serde_json::Value>| {
                let calls = calls_for_handler.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let ids = body["ids"].as_array().cloned().unwrap_or_default();
                    assert!(ids.len() <= ALBUM_ADD_CHUNK_SIZE);
                    let results: Vec<_> = ids
                        .iter()
                        .map(|id| json!({"id": id, "success": true}))
                        .collect();
                    Json(results)
                }
            }),
        );
        let (base, _server) = spawn_test_server(Router::new().nest("/api", app)).await;
        let import = client(&base);

        let ids: Vec<Uuid> = (0..750u32)
            .map(|i| Uuid::from_u128(u128::from(i) + 1))
            .collect();
        let album_id = Uuid::parse_str("9c858901-8a57-4791-81fe-4c455b099bc9").unwrap();
        let outcome = import.add_assets_to_album(album_id, &ids).await.unwrap();

        assert_eq!(outcome.added.len(), 750);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
