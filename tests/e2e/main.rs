//! `PLAN.md` §11's last test tier: a real end-to-end run against two real Immich instances,
//! started by `tests/e2e/compose.yaml` (two independent stacks — server + postgres + redis
//! each, ML container omitted — on host ports 2283 and 2284).
//!
//! # Status: executed, and passing
//!
//! This suite was written before it could be run (`PLAN.md` §11 asked for exactly that), but
//! it has since been executed against two real `immich-server:v3.1.0` stacks and passes: all
//! four fixtures transferred with matching checksums, and a second run transferred nothing.
//! Verified twice — once against the stacks as they came up, then again from a completely
//! clean slate (`down -v` and back up) after the one defect below was fixed. Everything it
//! asserts is now observed behaviour of a real Immich server, not a transcription of the
//! vendored `OpenAPI` document.
//!
//! **The one thing the first real run found**, since it is the kind of bug that hides:
//! `compose.yaml`'s `depends_on` listed the database and redis without
//! `condition: service_healthy` (matching upstream's own compose file, which also omits it).
//! Both `immich-server` containers therefore raced postgres, died with
//! `Error: connect ECONNREFUSED …:5432`, and were resurrected only by `restart:
//! unless-stopped` — so `up -d --wait` printed `container … is unhealthy` and the stack
//! limped to health by accident. Fixed by requiring `service_healthy` on both dependencies
//! and setting `restart: 'no'`, so a genuinely broken fixture now fails loudly instead of
//! restarting itself into a green state.
//!
//! Four things the writing-phase header called out as unverified are now confirmed against
//! the real server: `POST /auth/admin-sign-up` works on each freshly-initialized stack with
//! no onboarding step in between; `POST /search/metadata` with `albumIds` really is the way
//! to enumerate an album's assets (`AlbumResponseDto` has no `assets` field); two stacks run
//! side by side on one Docker host with separate networks, volumes and ports; and the
//! shared-link password-cookie dance (`E2`/`E3`, `PLAN.md` §2) behaves against a real
//! `immich-server` the way `tests/mock_sync.rs`'s mock always claimed.
//!
//! Still worth knowing: `wait_for_ready`'s 3-minute timeout and 2-second poll are still
//! guesses, they have simply never been hit — a clean `up -d --wait` took about 65 seconds
//! here, so there is headroom but it has not been probed on a slow or loaded machine.
//!
//! Run for real with:
//! ```sh
//! docker compose -f tests/e2e/compose.yaml up -d --wait
//! cargo test --test e2e -- --ignored --nocapture
//! docker compose -f tests/e2e/compose.yaml down -v
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::CommandFactory;
use reqwest::multipart;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;
use url::Url;
use uuid::Uuid;

use immich_federation_at_home::cache::ContentHashCache;
use immich_federation_at_home::config::{self, Cli};
use immich_federation_at_home::startup::run_startup;

/// Host port the export stack's `immich-server` publishes (`tests/e2e/compose.yaml`).
const EXPORT_PORT: u16 = 2283;
/// Host port the import stack's `immich-server` publishes.
const IMPORT_PORT: u16 = 2284;

/// How long to wait for each stack's `GET /api/server/ping` to start answering `200` before
/// giving up. Unverified guess — see this file's top-level doc comment, point 1.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const READY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Password set on the export album's shared link (step 4).
const SHARE_PASSWORD: &str = "correct-horse-battery-staple";

// ---------------------------------------------------------------------------------------------
// A thin, hand-written Immich admin/user API client for test setup only.
//
// This is deliberately *not* `ExportClient`/`ImportClient` from `src/immich/` — those model
// exactly the nine endpoints (E1-E5/I1-I6) the shipped program calls, authenticated either by
// share-link key or by API key. Everything here (admin sign-up, session login, album/
// shared-link/API-key creation) is one-time *test fixture setup* that a real operator would do
// once by hand in the Immich web UI, so it stays in the test file rather than growing the
// library's own API surface.
// ---------------------------------------------------------------------------------------------

struct ImmichAdmin {
    api_base: Url,
    client: reqwest::Client,
    /// Session bearer token, set once [`Self::login`] succeeds.
    token: Option<String>,
}

impl ImmichAdmin {
    fn new(host_port: u16) -> Self {
        let api_base = Url::parse(&format!("http://127.0.0.1:{host_port}/api/")).unwrap();
        Self {
            api_base,
            client: reqwest::Client::new(),
            token: None,
        }
    }

    fn url(&self, path: &str) -> Url {
        self.api_base.join(path).unwrap()
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// Polls `GET /server/ping` until it succeeds or [`READY_TIMEOUT`] elapses. See this
    /// file's top-level doc comment, point 1, for why this is the least-trusted part of setup.
    async fn wait_for_ready(&self) {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Ok(resp) = self.client.get(self.url("server/ping")).send().await
                && resp.status().is_success()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} never became ready within {READY_TIMEOUT:?}",
                self.api_base
            );
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }

    /// `POST /auth/admin-sign-up` then `POST /auth/login`, leaving `self.token` set. Immich
    /// only allows this once per fresh instance, which is exactly the "freshly booted
    /// container" state this test always starts from.
    async fn sign_up_and_log_in(&mut self, email: &str, password: &str, name: &str) {
        let resp = self
            .client
            .post(self.url("auth/admin-sign-up"))
            .json(&json!({"email": email, "password": password, "name": name}))
            .send()
            .await
            .expect("admin-sign-up request failed");
        assert!(
            resp.status().is_success(),
            "admin-sign-up failed: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );

        let resp = self
            .client
            .post(self.url("auth/login"))
            .json(&json!({"email": email, "password": password}))
            .send()
            .await
            .expect("login request failed");
        assert!(
            resp.status().is_success(),
            "login failed: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        let body: Value = resp.json().await.expect("login response was not JSON");
        self.token = Some(
            body["accessToken"]
                .as_str()
                .expect("login response had no accessToken")
                .to_owned(),
        );
    }

    /// `POST /assets` (multipart), returns the new asset's UUID.
    async fn upload_asset(&self, path: &Path) -> Uuid {
        let bytes = tokio::fs::read(path).await.expect("failed to read fixture");
        let filename = path.file_name().unwrap().to_string_lossy().into_owned();
        let now = chrono::Utc::now().to_rfc3339();

        let part = multipart::Part::bytes(bytes)
            .file_name(filename.clone())
            .mime_str("image/jpeg")
            .unwrap();
        let form = multipart::Form::new()
            .part("assetData", part)
            .text("filename", filename.clone())
            .text("fileCreatedAt", now.clone())
            .text("fileModifiedAt", now);

        let resp = self
            .authed(self.client.post(self.url("assets")))
            .multipart(form)
            .send()
            .await
            .expect("upload request failed");
        assert!(
            resp.status().is_success(),
            "upload of {filename} failed: {}",
            resp.status()
        );
        let body: Value = resp.json().await.expect("upload response was not JSON");
        body["id"]
            .as_str()
            .expect("upload response had no id")
            .parse()
            .expect("upload response id was not a UUID")
    }

    /// `POST /albums`, returns the new album's UUID.
    async fn create_album(&self, name: &str) -> Uuid {
        let resp = self
            .authed(self.client.post(self.url("albums")))
            .json(&json!({"albumName": name}))
            .send()
            .await
            .expect("create-album request failed");
        assert!(
            resp.status().is_success(),
            "create-album failed: {}",
            resp.status()
        );
        let body: Value = resp
            .json()
            .await
            .expect("create-album response was not JSON");
        body["id"]
            .as_str()
            .expect("create-album response had no id")
            .parse()
            .expect("create-album response id was not a UUID")
    }

    /// `PUT /albums/{id}/assets`.
    async fn add_assets_to_album(&self, album_id: Uuid, asset_ids: &[Uuid]) {
        let ids: Vec<String> = asset_ids.iter().map(ToString::to_string).collect();
        let resp = self
            .authed(
                self.client
                    .put(self.url(&format!("albums/{album_id}/assets"))),
            )
            .json(&json!({"ids": ids}))
            .send()
            .await
            .expect("add-to-album request failed");
        assert!(
            resp.status().is_success(),
            "add-to-album failed: {}",
            resp.status()
        );
    }

    /// `POST /shared-links` for an album, with `allowDownload: true` and a password. Returns
    /// the link's `key`, which is what `EXPORT_ALBUM_URL` embeds as `/share/<key>`.
    async fn create_password_protected_album_share_link(
        &self,
        album_id: Uuid,
        password: &str,
    ) -> String {
        let resp = self
            .authed(self.client.post(self.url("shared-links")))
            .json(&json!({
                "type": "ALBUM",
                "albumId": album_id.to_string(),
                "allowDownload": true,
                "showMetadata": true,
                "password": password,
            }))
            .send()
            .await
            .expect("create-shared-link request failed");
        assert!(
            resp.status().is_success(),
            "create-shared-link failed: {}",
            resp.status()
        );
        let body: Value = resp
            .json()
            .await
            .expect("create-shared-link response was not JSON");
        // `SharedLinkResponseDto.key` isn't modelled in `dto.rs` (the shipped client is never
        // handed the key directly — it discovers it from the URL the operator pastes in), but
        // the wire response does carry it (confirmed against the vendored spec).
        body["key"]
            .as_str()
            .expect("create-shared-link response had no key")
            .to_owned()
    }

    /// `POST /api-keys` scoped to exactly the permissions `PLAN.md` §5 step 8 requires.
    /// Returns the key's secret (shown only once, per the spec's own description).
    async fn create_scoped_api_key(&self, name: &str) -> String {
        let resp = self
            .authed(self.client.post(self.url("api-keys")))
            .json(&json!({
                "name": name,
                "permissions": immich_federation_at_home::immich::dto::REQUIRED_PERMISSIONS,
            }))
            .send()
            .await
            .expect("create-api-key request failed");
        assert!(
            resp.status().is_success(),
            "create-api-key failed: {}",
            resp.status()
        );
        let body: Value = resp
            .json()
            .await
            .expect("create-api-key response was not JSON");
        body["secret"]
            .as_str()
            .expect("create-api-key response had no secret")
            .to_owned()
    }

    /// `POST /search/metadata` with `albumIds: [album_id]`, single page (fixture counts here
    /// are always well under the 250-item page size `ExportClient` itself uses). Returns
    /// `(checksum, originalFileName)` pairs — everything the test needs to assert exact
    /// membership. This is the same endpoint (E4) `ExportClient::list_album_assets` calls
    /// against the *export* side; here it's called against the *import* side with a bearer
    /// token instead of a share-link key, purely for post-run verification.
    async fn list_album_asset_checksums(&self, album_id: Uuid) -> Vec<(String, String)> {
        let resp = self
            .authed(self.client.post(self.url("search/metadata")))
            .json(&json!({"albumIds": [album_id.to_string()], "page": 1, "size": 250}))
            .send()
            .await
            .expect("search/metadata request failed");
        assert!(
            resp.status().is_success(),
            "search/metadata failed: {}",
            resp.status()
        );
        let body: Value = resp
            .json()
            .await
            .expect("search/metadata response was not JSON");
        body["assets"]["items"]
            .as_array()
            .expect("search/metadata response had no assets.items array")
            .iter()
            .map(|item| {
                (
                    item["checksum"].as_str().unwrap_or_default().to_owned(),
                    item["originalFileName"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect()
    }
}

/// SHA-1, base64-encoded — the same encoding `AssetResponseDto.checksum` and
/// `x-immich-checksum` use throughout the crate (`NOTES.md`'s "x-immich-checksum header
/// encoding" finding).
fn sha1_base64_of_file(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("failed to read fixture for checksumming");
    let mut hasher = Sha1::new();
    hasher.update(&bytes);
    BASE64.encode(hasher.finalize())
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/fixtures")
}

fn fixture_paths() -> Vec<PathBuf> {
    let dir = fixtures_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read fixtures dir {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jpg"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no .jpg fixtures found under {} — see tests/e2e/fixtures/",
        dir.display()
    );
    paths
}

// -----------------------------------------------------------------------------------------
// The end-to-end scenario (PLAN.md §11's last bullet, §12 task 12)
// -----------------------------------------------------------------------------------------

/// Full round trip against two real Immich instances started by `tests/e2e/compose.yaml`:
///
/// 1. wait for both stacks to be healthy,
/// 2. admin-sign-up on each,
/// 3. upload the fixture images to the export instance,
/// 4. create an album there and a password-protected shared link,
/// 5. create a scoped API key and a target album on the import instance,
/// 6. run the sync (via the library, not the binary — `startup::run_startup` +
///    `SyncContext::run_once`, matching the exact functions `main.rs` itself calls),
/// 7. assert the target album contains exactly the fixtures, with matching checksums,
/// 8. run it a second time and assert nothing was transferred.
///
/// Requires `docker compose -f tests/e2e/compose.yaml up -d --wait` to have already been run
/// — this test does not manage the containers' lifecycle itself. It also assumes both stacks
/// are *fresh*: step 2 signs up the admin, which Immich only permits once per instance, so a
/// re-run against stacks that were not torn down with `down -v` will fail there.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires `docker compose -f tests/e2e/compose.yaml up -d --wait`"]
// One long, linear, step-numbered scenario is more readable here than splitting it up would
// be — see this test's own doc comment. It crept a few lines past clippy's default
// threshold when the `Settings`-resolution step grew from one flag-parsing call into two.
#[allow(clippy::too_many_lines)]
async fn mirrors_an_album_end_to_end() {
    let fixtures = fixture_paths();

    let mut export = ImmichAdmin::new(EXPORT_PORT);
    let mut import = ImmichAdmin::new(IMPORT_PORT);

    // Step 1 — wait for both stacks to be healthy.
    export.wait_for_ready().await;
    import.wait_for_ready().await;

    // Step 2 — admin-sign-up (+ login) on each.
    export
        .sign_up_and_log_in(
            "export-admin@example.invalid",
            "export-admin-password",
            "Export Admin",
        )
        .await;
    import
        .sign_up_and_log_in(
            "import-admin@example.invalid",
            "import-admin-password",
            "Import Admin",
        )
        .await;

    // Step 3 — upload the fixtures to the export instance.
    let mut uploaded = Vec::new();
    for path in &fixtures {
        let id = export.upload_asset(path).await;
        uploaded.push(id);
    }

    // Step 4 — an album there, and a password-protected shared link.
    let export_album_id = export.create_album("e2e source album").await;
    export.add_assets_to_album(export_album_id, &uploaded).await;
    let share_key = export
        .create_password_protected_album_share_link(export_album_id, SHARE_PASSWORD)
        .await;

    // Step 5 — a scoped API key and a target album on the import instance.
    let import_api_key = import.create_scoped_api_key("e2e sync key").await;
    let import_album_id = import.create_album("e2e target album").await;

    // Step 6 — run the sync through the library, exactly as `main.rs` does: resolve
    // `Settings` (no config file, so this forms the implicit single `default` job), run the
    // full startup sequence (§5), then call `SyncContext::run_once` directly rather than
    // looping the scheduler — the scheduler's `RUN_ONCE` mode (`scheduler::run` with
    // `run_once: true`) does exactly one call to the same function, so calling it directly
    // here is equivalent and lets the test assert on each run's `RunSummary`.
    let matches = Cli::command()
        .try_get_matches_from([
            "immich-federation-at-home",
            "--export-album-url",
            &format!("http://127.0.0.1:{EXPORT_PORT}/share/{share_key}"),
            "--export-album-password",
            SHARE_PASSWORD,
            "--import-server-url",
            &format!("http://127.0.0.1:{IMPORT_PORT}"),
            "--import-api-key",
            &import_api_key,
            "--import-album",
            &import_album_id.to_string(),
            "--once",
        ])
        .expect("argv should parse");
    let settings = config::load(&matches, &|_: &str| None, None)
        .expect("Settings should resolve from well-formed CLI args");
    let job = &settings.jobs[0];

    let cache = Arc::new(ContentHashCache::disabled());
    let transfers = Arc::new(Semaphore::new(4));
    let outcome = run_startup(
        job,
        cache,
        transfers,
        settings.globals.transfer_concurrency,
        settings.globals.tmp_dir.clone(),
    )
    .await
    .expect("run_startup should succeed against two freshly-provisioned real instances");
    assert_eq!(outcome.summary.source_asset_count, fixtures.len() as u64);
    assert_eq!(outcome.summary.target_album_id, import_album_id);

    let first_run = outcome
        .sync
        .run_once()
        .await
        .expect("first run_once should succeed");

    // Step 7 — the target album contains exactly the fixtures, with matching checksums.
    assert_eq!(first_run.source, fixtures.len());
    assert_eq!(first_run.transferred, fixtures.len());
    assert_eq!(first_run.failed, 0);
    assert_eq!(first_run.added_to_album, fixtures.len());

    let expected_checksums: std::collections::HashSet<String> =
        fixtures.iter().map(|p| sha1_base64_of_file(p)).collect();
    let actual = import.list_album_asset_checksums(import_album_id).await;
    assert_eq!(
        actual.len(),
        fixtures.len(),
        "target album must contain exactly the fixtures, no more, no fewer"
    );
    let actual_checksums: std::collections::HashSet<String> = actual
        .iter()
        .map(|(checksum, _)| checksum.clone())
        .collect();
    assert_eq!(
        actual_checksums, expected_checksums,
        "every transferred asset's checksum must match its source fixture exactly"
    );

    // Step 8 — a second run transfers nothing and re-adds nothing.
    let second_run = outcome
        .sync
        .run_once()
        .await
        .expect("second run_once should succeed");
    assert_eq!(second_run.source, fixtures.len());
    assert_eq!(second_run.transferred, 0, "must not re-transfer anything");
    assert_eq!(second_run.already_present, fixtures.len());
    assert_eq!(
        second_run.added_to_album, 0,
        "must not re-add already-present assets to the album"
    );
    assert_eq!(second_run.failed, 0);

    let after_second_run = import.list_album_asset_checksums(import_album_id).await;
    assert_eq!(
        after_second_run.len(),
        fixtures.len(),
        "the second run must not change the target album's membership"
    );
}
