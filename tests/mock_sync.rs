//! `PLAN.md` §11: a small `axum` server implementing the whole E1-E5/I1-I6 API surface,
//! driving the **real** client code (`ExportClient`, `ImportClient`, `SyncContext::run_once`,
//! and — for one scenario — `startup::run_startup`) through eight scenarios, from *outside*
//! the crate (`tests/`, not `src/sync.rs`'s own `#[cfg(test)]` module).
//!
//! `src/sync.rs`'s own unit tests already stand up small in-process fake export/import
//! servers; this file is deliberately a separate, black-box exercise of the same flow
//! through the crate's public API only (`immich_federation_at_home::...`), so some
//! duplication of "a fake Immich server" is expected. What's different here: **one**
//! [`MockServer`] fixture — a single `axum` app hosting both the export surface (nested
//! under `/export/api`) and the import surface (nested under `/import/api`) behind one
//! `Arc<Mutex<MockState>>` — configured per scenario via its builder-style setters, rather than
//! eight ad-hoc servers.
//!
//! Every assertion is on a returned value ([`RunSummary`], [`StartupOutcome`]) or on the
//! mock server's own recorded state (upload attempts, album membership, request headers) —
//! never on log or error *text* (`PLAN.md` §11); matching on an error *variant* is fine and
//! used below (the download-401 scenario).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::CommandFactory;
use serde_json::json;
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use url::Url;
use uuid::Uuid;

use immich_federation_at_home::cache::ContentHashCache;
use immich_federation_at_home::config::{self, Cli, Secret};
use immich_federation_at_home::immich::dto;
use immich_federation_at_home::immich::export::{ExportClient, ExportError};
use immich_federation_at_home::immich::import::ImportClient;
use immich_federation_at_home::retry::RetryPolicy;
use immich_federation_at_home::share_url::ShareRef;
use immich_federation_at_home::startup::run_startup;
use immich_federation_at_home::sync::SyncContext;

// -----------------------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------------------

/// One asset as it exists on the mock export server.
#[derive(Clone)]
struct AssetFixture {
    id: Uuid,
    filename: String,
    bytes: Vec<u8>,
    checksum: String,
    /// If set, `GET /assets/{id}/original` serves different bytes than `checksum` implies —
    /// simulating corruption in transit (scenario 6).
    corrupt: bool,
}

fn sha1_base64(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    BASE64.encode(hasher.finalize())
}

fn fixture(n: u128, filename: &str, bytes: &[u8]) -> AssetFixture {
    AssetFixture {
        id: Uuid::from_u128(n),
        filename: filename.to_owned(),
        checksum: sha1_base64(bytes),
        bytes: bytes.to_vec(),
        corrupt: false,
    }
}

fn asset_json(f: &AssetFixture) -> serde_json::Value {
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

/// Everything the mock server needs, shared across every handler and preserved across
/// repeated `run_once` calls within a test (the idempotency scenario relies on this — a real
/// Immich instance would retain exactly this state between runs too).
struct MockState {
    // ---- export side ------------------------------------------------------------------
    password: Option<String>,
    allow_download: bool,
    export_album_id: Uuid,
    export_album_name: String,
    assets: Vec<AssetFixture>,
    /// `None` ⇒ serve every asset on page 1 with `nextPage: null` (the common case). `Some`
    /// ⇒ groups of asset *indices* served one group per page, in order (scenario 8).
    page_chunks: Option<Vec<Vec<usize>>>,
    search_calls: u32,
    /// Per-asset override for `GET /assets/{id}/original`'s response status (scenario 4).
    download_overrides: HashMap<Uuid, StatusCode>,

    // ---- import side ------------------------------------------------------------------
    permissions: Vec<String>,
    import_album_id: Uuid,
    import_album_name: String,
    by_checksum: HashMap<String, Uuid>,
    trashed: HashSet<Uuid>,
    unsupported: HashSet<String>,
    album_members: HashMap<Uuid, HashSet<Uuid>>,
    next_id: u128,
    /// Every `POST /assets` call, successful or not.
    upload_attempts: u32,
    /// How many *more* upload attempts should fail with 500 before succeeding (scenario 5).
    upload_fail_remaining: u32,
}

impl MockState {
    fn fresh_import_id(&mut self) -> Uuid {
        self.next_id += 1;
        Uuid::from_u128(0xE000_0000_0000_0000_0000_0000_0000_0000 + self.next_id)
    }
}

const EXPORT_ALBUM_ID: Uuid = Uuid::from_u128(0x5111_0000_0000_0000_0000_0000_0000_0000);
const IMPORT_ALBUM_ID: Uuid = Uuid::from_u128(0x9999_0000_0000_0000_0000_0000_0000_0000);

/// The reusable fixture: one `axum` server (E1-E5 nested under `/export/api`, I1-I6 nested
/// under `/import/api`) plus handles for driving real `ExportClient`/`ImportClient`/
/// `SyncContext` against it and inspecting what happened afterward.
struct MockServer {
    export_base: Url,
    import_base: Url,
    state: Arc<Mutex<MockState>>,
    _handle: JoinHandle<()>,
}

impl MockServer {
    async fn spawn(assets: Vec<AssetFixture>) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            password: None,
            allow_download: true,
            export_album_id: EXPORT_ALBUM_ID,
            export_album_name: "Holiday 2026".to_owned(),
            assets,
            page_chunks: None,
            search_calls: 0,
            download_overrides: HashMap::new(),
            permissions: dto::REQUIRED_PERMISSIONS
                .iter()
                .map(ToString::to_string)
                .collect(),
            import_album_id: IMPORT_ALBUM_ID,
            import_album_name: "My Family Photos".to_owned(),
            by_checksum: HashMap::new(),
            trashed: HashSet::new(),
            unsupported: HashSet::new(),
            album_members: HashMap::new(),
            next_id: 0,
            upload_attempts: 0,
            upload_fail_remaining: 0,
        }));

        let app = Router::new()
            .nest("/export/api", export_router(state.clone()))
            .nest("/import/api", import_router(state.clone()));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let root = Url::parse(&format!("http://{addr}/")).unwrap();
        Self {
            export_base: Url::parse(&format!("{root}export/api")).unwrap(),
            import_base: Url::parse(&format!("{root}import/api")).unwrap(),
            state,
            _handle: handle,
        }
    }

    // ---- configuration (called before the run(s) under test) --------------------------

    fn seed_existing(&self, checksum: &str, existing_id: Uuid) {
        self.state
            .lock()
            .unwrap()
            .by_checksum
            .insert(checksum.to_owned(), existing_id);
    }

    fn mark_trashed(&self, existing_id: Uuid) {
        self.state.lock().unwrap().trashed.insert(existing_id);
    }

    fn mark_unsupported(&self, checksum: &str) {
        self.state
            .lock()
            .unwrap()
            .unsupported
            .insert(checksum.to_owned());
    }

    fn override_download_status(&self, asset_id: Uuid, status: StatusCode) {
        self.state
            .lock()
            .unwrap()
            .download_overrides
            .insert(asset_id, status);
    }

    fn set_upload_fail_count(&self, n: u32) {
        self.state.lock().unwrap().upload_fail_remaining = n;
    }

    fn set_page_chunks(&self, chunks: Vec<Vec<usize>>) {
        self.state.lock().unwrap().page_chunks = Some(chunks);
    }

    // ---- inspection (called after the run(s) under test) -------------------------------

    fn upload_attempts(&self) -> u32 {
        self.state.lock().unwrap().upload_attempts
    }

    fn search_calls(&self) -> u32 {
        self.state.lock().unwrap().search_calls
    }

    fn album_member_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .album_members
            .get(&IMPORT_ALBUM_ID)
            .map_or(0, HashSet::len)
    }

    // ---- real client construction -------------------------------------------------------

    fn export_client(&self) -> ExportClient {
        ExportClient::new(
            self.export_base.clone(),
            ShareRef::Key("test-key".to_owned()),
            Duration::from_secs(5),
            Duration::from_secs(5),
            RetryPolicy::zero_delay(),
        )
        .unwrap()
    }

    fn import_client(&self) -> ImportClient {
        ImportClient::new(
            self.import_base.clone(),
            &secret("test-api-key"),
            Duration::from_secs(5),
            Duration::from_secs(5),
            RetryPolicy::zero_delay(),
        )
        .unwrap()
    }

    fn sync_context(&self) -> SyncContext {
        SyncContext::new(
            self.export_client(),
            self.import_client(),
            EXPORT_ALBUM_ID,
            IMPORT_ALBUM_ID,
            self.export_base.to_string(),
            4,
            Duration::from_secs(10),
            RetryPolicy::zero_delay(),
            Arc::new(ContentHashCache::disabled()),
            Arc::new(Semaphore::new(4)),
        )
    }
}

fn secret(value: &str) -> Secret {
    value.parse().unwrap()
}

// -----------------------------------------------------------------------------------------
// Export-side routes (E1-E5)
// -----------------------------------------------------------------------------------------

fn export_router(state: Arc<Mutex<MockState>>) -> Router {
    Router::new()
        .route(
            "/server/version",
            get(|| async { Json(json!({"major": 3, "minor": 1, "patch": 0, "prerelease": null})) }),
        )
        .route("/shared-links/login", post(login_handler))
        .route("/shared-links/me", get(shared_link_me_handler))
        .route("/search/metadata", post(search_metadata_handler))
        .route("/assets/{id}/original", get(download_handler))
        .with_state(state)
}

async fn login_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let state = state.lock().unwrap();
    let Some(expected) = &state.password else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"statusCode": 400, "message": "Shared link is not password protected"})),
        );
    };
    if body["password"].as_str() == Some(expected.as_str()) {
        (StatusCode::CREATED, Json(shared_link_json(&state)))
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"statusCode": 401, "message": "Invalid password"})),
        )
    }
}

async fn shared_link_me_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    let state = state.lock().unwrap();
    if state.password.is_some() {
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !cookie.contains("immich_shared_link_token=") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"statusCode": 401, "message": "Password required"})),
            );
        }
    }
    (StatusCode::OK, Json(shared_link_json(&state)))
}

fn shared_link_json(state: &MockState) -> serde_json::Value {
    json!({
        "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
        "type": "ALBUM",
        "album": {
            "id": state.export_album_id.to_string(),
            "albumName": state.export_album_name,
            "assetCount": state.assets.len(),
        },
        "allowDownload": state.allow_download,
        "allowUpload": false,
        "showMetadata": true,
        "expiresAt": null
    })
}

async fn search_metadata_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut state = state.lock().unwrap();
    state.search_calls += 1;
    let page = usize::try_from(body["page"].as_u64().unwrap_or(1)).unwrap();

    let (items, next_page): (Vec<serde_json::Value>, Option<String>) =
        if let Some(chunks) = state.page_chunks.clone() {
            let items = chunks
                .get(page - 1)
                .map(|indices| {
                    indices
                        .iter()
                        .map(|&i| asset_json(&state.assets[i]))
                        .collect()
                })
                .unwrap_or_default();
            let next_page = (page < chunks.len()).then(|| (page + 1).to_string());
            (items, next_page)
        } else {
            (state.assets.iter().map(asset_json).collect(), None)
        };

    let total = state.assets.len();
    Json(json!({
        "albums": {"total": 0, "items": []},
        "assets": {"items": items, "nextPage": next_page, "total": total, "count": items.len()}
    }))
}

async fn download_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    Path(id): Path<String>,
) -> (StatusCode, Vec<u8>) {
    let state = state.lock().unwrap();
    let Ok(id) = id.parse::<Uuid>() else {
        return (StatusCode::NOT_FOUND, Vec::new());
    };
    if let Some(status) = state.download_overrides.get(&id) {
        return (
            *status,
            format!(
                r#"{{"statusCode":{},"message":"mocked failure"}}"#,
                status.as_u16()
            )
            .into_bytes(),
        );
    }
    let Some(fixture) = state.assets.iter().find(|f| f.id == id) else {
        return (StatusCode::NOT_FOUND, Vec::new());
    };
    if fixture.corrupt {
        let mut bad = fixture.bytes.clone();
        bad.push(0xFF);
        return (StatusCode::OK, bad);
    }
    (StatusCode::OK, fixture.bytes.clone())
}

// -----------------------------------------------------------------------------------------
// Import-side routes (I1-I6)
// -----------------------------------------------------------------------------------------

fn import_router(state: Arc<Mutex<MockState>>) -> Router {
    Router::new()
        .route(
            "/server/version",
            get(|| async { Json(json!({"major": 3, "minor": 1, "patch": 0, "prerelease": null})) }),
        )
        .route("/api-keys/me", get(api_keys_me_handler))
        .route("/albums/{id}", get(get_album_handler))
        .route("/albums", get(list_albums_handler))
        .route("/assets/bulk-upload-check", post(bulk_upload_check_handler))
        .route("/assets", post(upload_handler))
        .route("/albums/{id}/assets", put(album_add_handler))
        .with_state(state)
}

async fn api_keys_me_handler(
    State(state): State<Arc<Mutex<MockState>>>,
) -> Json<serde_json::Value> {
    let state = state.lock().unwrap();
    Json(json!({
        "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
        "name": "sync-key",
        "permissions": state.permissions,
        "createdAt": "2024-01-01T00:00:00.000Z",
        "updatedAt": "2024-01-01T00:00:00.000Z"
    }))
}

async fn get_album_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let state = state.lock().unwrap();
    Json(json!({"id": id, "albumName": state.import_album_name, "assetCount": 0}))
}

async fn list_albums_handler(
    State(state): State<Arc<Mutex<MockState>>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let state = state.lock().unwrap();
    Json(json!([
        {"id": state.import_album_id.to_string(), "albumName": state.import_album_name, "assetCount": 0}
    ]))
}

async fn bulk_upload_check_handler(
    State(state): State<Arc<Mutex<MockState>>>,
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
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    // No real multipart parse (import.rs's own tests document why: no `multipart` feature on
    // the dev-dep `axum`) — the checksum header, computed by the real client from whatever it
    // streamed, is what proves the body actually carries the file bytes.
    let _ = body;
    let checksum = headers
        .get("x-immich-checksum")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    let mut state = state.lock().unwrap();
    state.upload_attempts += 1;
    if state.upload_fail_remaining > 0 {
        state.upload_fail_remaining -= 1;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"statusCode": 500, "message": "mocked transient failure"})),
        );
    }

    let id = state.fresh_import_id();
    state.by_checksum.insert(checksum, id);
    (
        StatusCode::CREATED,
        Json(json!({"id": id.to_string(), "status": "created"})),
    )
}

async fn album_add_handler(
    State(state): State<Arc<Mutex<MockState>>>,
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

// -----------------------------------------------------------------------------------------
// Scenario 1 — a clean first run
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn clean_first_run_transfers_and_adds_everything() {
    let server = MockServer::spawn(vec![
        fixture(1, "a.jpg", b"asset one bytes"),
        fixture(2, "b.jpg", b"asset two bytes, a bit longer"),
        fixture(3, "c.jpg", b"asset three"),
    ])
    .await;
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.source, 3);
    assert_eq!(summary.already_present, 0);
    assert_eq!(summary.transferred, 3);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.added_to_album, 3);
    assert_eq!(summary.skipped, 0);
    assert_eq!(server.upload_attempts(), 3);
    assert_eq!(server.album_member_count(), 3);
}

// -----------------------------------------------------------------------------------------
// Scenario 2 — an idempotent second run
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn idempotent_second_run_transfers_nothing_and_readds_nothing() {
    let server = MockServer::spawn(vec![
        fixture(1, "a.jpg", b"asset one bytes"),
        fixture(2, "b.jpg", b"asset two bytes, a bit longer"),
    ])
    .await;
    let ctx = server.sync_context();

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
    assert_eq!(
        server.upload_attempts(),
        2,
        "only one upload per asset, across both runs"
    );
    assert_eq!(server.album_member_count(), 2);
}

// -----------------------------------------------------------------------------------------
// Scenario 3 — a partial-overlap run
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn partial_overlap_run_transfers_only_the_missing_assets() {
    let fixtures = vec![
        fixture(1, "already.jpg", b"already on the import side"),
        fixture(2, "new-one.jpg", b"brand new asset one"),
        fixture(3, "new-two.jpg", b"brand new asset two"),
    ];
    let server = MockServer::spawn(fixtures.clone()).await;
    // Pre-seed the import server: asset 1's checksum already exists there (e.g. from a prior,
    // unrelated upload), but was never added to this album — and sits in the import
    // instance's trash, exercising PLAN.md §6 step 2's `isTrashed` case (a warning, not a
    // failure: it must still be album-added).
    let existing_id = Uuid::from_u128(0xB000_0000_0000_0000_0000_0000_0000_0001);
    server.seed_existing(&fixtures[0].checksum, existing_id);
    server.mark_trashed(existing_id);
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.source, 3);
    assert_eq!(summary.already_present, 1);
    assert_eq!(summary.transferred, 2);
    assert_eq!(summary.failed, 0);
    assert_eq!(
        summary.added_to_album, 3,
        "all three must land in the album this run, including the trashed duplicate"
    );
    assert_eq!(server.upload_attempts(), 2);
}

// -----------------------------------------------------------------------------------------
// Scenario 4 — a download 401
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn download_401_fails_only_the_one_asset() {
    let fixtures = vec![
        fixture(1, "forbidden.jpg", b"cannot be downloaded"),
        fixture(2, "fine.jpg", b"downloads just fine"),
    ];
    let server = MockServer::spawn(fixtures.clone()).await;
    server.override_download_status(fixtures[0].id, StatusCode::UNAUTHORIZED);
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.source, 2);
    assert_eq!(summary.transferred, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.added_to_album, 1);
    assert_eq!(
        server.upload_attempts(),
        1,
        "the forbidden asset must never reach upload"
    );

    // The client-level error variant, exercised directly against ExportClient (not just
    // observed indirectly through the run summary): a download 401 surfaces as
    // ExportError::DownloadForbidden, matching src/immich/export.rs's own documented
    // behaviour for allowDownload: false.
    let export = server.export_client();
    let mut buf: Vec<u8> = Vec::new();
    let err = export
        .download_original(fixtures[0].id, &mut buf)
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

// -----------------------------------------------------------------------------------------
// Scenario 5 — an upload 500 with a successful retry
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn upload_500_is_retried_and_succeeds() {
    let server = MockServer::spawn(vec![fixture(1, "flaky.jpg", b"retries through a 500")]).await;
    server.set_upload_fail_count(1);
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.transferred, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.added_to_album, 1);
    assert_eq!(
        server.upload_attempts(),
        2,
        "one failed attempt, one retried success"
    );
}

// -----------------------------------------------------------------------------------------
// Scenario 6 — a checksum mismatch (the corrupted body must NOT be uploaded)
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn checksum_mismatch_never_uploads_the_corrupted_body() {
    let mut corrupt = fixture(1, "corrupt.jpg", b"looks fine on paper");
    corrupt.corrupt = true;
    let fixtures = vec![corrupt, fixture(2, "fine.jpg", b"downloads cleanly")];
    let server = MockServer::spawn(fixtures).await;
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.source, 2);
    assert_eq!(summary.transferred, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(
        summary.added_to_album, 1,
        "the corrupted asset must never reach the album"
    );
    assert_eq!(
        server.upload_attempts(),
        1,
        "a corrupted body must never be uploaded"
    );
}

// -----------------------------------------------------------------------------------------
// Scenario 7 — an unsupported-format rejection
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn unsupported_format_is_skipped_and_never_uploaded() {
    let fixtures = vec![
        fixture(1, "weird.bmp", b"an unsupported format"),
        fixture(2, "normal.jpg", b"a perfectly normal jpeg"),
    ];
    let server = MockServer::spawn(fixtures.clone()).await;
    server.mark_unsupported(&fixtures[0].checksum);
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.source, 2);
    assert_eq!(summary.transferred, 1);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.added_to_album, 1);
    assert_eq!(server.upload_attempts(), 1);
}

// -----------------------------------------------------------------------------------------
// Scenario 8 — a paginated album (3 pages)
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn paginated_album_follows_all_three_pages() {
    let fixtures: Vec<AssetFixture> = (1..=5)
        .map(|n| {
            fixture(
                n,
                &format!("img{n}.jpg"),
                format!("bytes for asset {n}").as_bytes(),
            )
        })
        .collect();
    let server = MockServer::spawn(fixtures).await;
    server.set_page_chunks(vec![vec![0, 1], vec![2, 3], vec![4]]);
    let ctx = server.sync_context();

    let summary = ctx.run_once().await.unwrap();

    assert_eq!(summary.source, 5);
    assert_eq!(summary.transferred, 5);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.added_to_album, 5);
    assert_eq!(server.search_calls(), 3, "must follow all three pages");
}

// -----------------------------------------------------------------------------------------
// Bonus — the full startup chain (run_startup) into one run_once, exercised end to end.
// Not one of the eight required scenarios, but PLAN.md's brief asks for run_startup
// "where practical"; this is the one scenario where a full Config -> SyncContext chain
// (version gates, shared-link assertions, permission check, album resolution) is worth the
// extra setup on top of what the eight scenarios above already cover directly.
// -----------------------------------------------------------------------------------------

#[tokio::test]
async fn run_startup_builds_a_working_sync_context() {
    let server = MockServer::spawn(vec![fixture(1, "a.jpg", b"asset one bytes")]).await;

    let matches = Cli::command()
        .try_get_matches_from([
            "immich-federation-at-home",
            "--export-album-url",
            &format!(
                "{}share/test-key",
                server.export_base.as_str().trim_end_matches("api")
            ),
            "--import-server-url",
            server.import_base.as_str().trim_end_matches("/api"),
            "--import-api-key",
            "test-api-key",
            "--import-album",
            &IMPORT_ALBUM_ID.to_string(),
        ])
        .unwrap();
    let settings = config::load(&matches, &|_: &str| None, None).unwrap();
    let job = &settings.jobs[0];

    let outcome = run_startup(
        job,
        Arc::new(ContentHashCache::disabled()),
        Arc::new(Semaphore::new(4)),
        settings.globals.transfer_concurrency,
        settings.globals.cache_dir.as_deref(),
    )
    .await
    .expect("startup should succeed against the mock");
    assert_eq!(outcome.summary.source_asset_count, 1);
    assert_eq!(outcome.summary.target_album_id, IMPORT_ALBUM_ID);

    let summary = outcome.sync.run_once().await.unwrap();
    assert_eq!(summary.transferred, 1);
    assert_eq!(summary.added_to_album, 1);
}
