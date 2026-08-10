//! The ten-step startup sequence (`PLAN.md` §5): version-gate both servers, validate the
//! shared link's own settings, check the import API key's permissions, resolve the target
//! album, and log a summary — everything `main.rs` needs before it can build a
//! [`crate::sync::SyncContext`] and start the scheduler.
//!
//! Steps 1 ("parse config; validate") and 2 ("set the log level") are deliberately *not*
//! here: they're one-shot, process-global side effects (`clap::Parser::parse` reads real
//! argv/env, and the log threshold is process-wide) that can't be meaningfully unit tested,
//! so `main.rs` does them directly. Everything from step
//! 3 onward — parsing `EXPORT_ALBUM_URL`, both version gates, the shared-link assertions,
//! the permission check, and album resolution — is pure enough or HTTP-driven-but-testable
//! enough to live here, per this task's brief. [`run_startup`] is the orchestrating async
//! function `main.rs` actually calls; everything else in this module is a smaller piece it
//! composes, each independently unit tested below.

use anyhow::Context;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::config::Config;
use crate::immich::export::{ExportClient, ExportError, LoginOutcome};
use crate::immich::import::ImportClient;
use crate::immich::{Version, dto};
use crate::retry::RetryPolicy;
use crate::sync::SyncContext;
use crate::{info, warn};

// ---------------------------------------------------------------------------------------
// Version gates (§5 steps 4 and 7)
// ---------------------------------------------------------------------------------------

/// The export instance's sharing API this tool depends on (bulk asset metadata via
/// `POST /search/metadata`, per `PLAN.md` §1's "Decisions" table) changed in v3.0.3; older
/// servers can't enumerate album checksums in bulk at all.
pub const EXPORT_MIN_VERSION: Version = Version {
    major: 3,
    minor: 0,
    patch: 3,
};

/// The import-side calls this tool makes need at least v3.0.0 (`PLAN.md` §5 step 7).
pub const IMPORT_MIN_VERSION: Version = Version {
    major: 3,
    minor: 0,
    patch: 0,
};

/// §5 step 4 (E1): require the export instance to be at least [`EXPORT_MIN_VERSION`],
/// naming the *detected* version in the failure message (not just the requirement) so the
/// operator doesn't have to go look it up themselves.
pub fn check_export_version(version: Version) -> anyhow::Result<()> {
    if version < EXPORT_MIN_VERSION {
        anyhow::bail!(
            "the export instance (EXPORT_ALBUM_URL) is running Immich v{version}, but this tool \
             requires v{EXPORT_MIN_VERSION} or newer: the shared-link API this tool depends on to \
             enumerate an album's assets in bulk changed in v3.0.3. Ask the export instance's \
             owner to upgrade it, or point EXPORT_ALBUM_URL at a newer instance."
        );
    }
    Ok(())
}

/// §5 step 7 (I1): require the import instance to be at least [`IMPORT_MIN_VERSION`],
/// naming the detected version.
pub fn check_import_version(version: Version) -> anyhow::Result<()> {
    if version < IMPORT_MIN_VERSION {
        anyhow::bail!(
            "the import instance (IMPORT_SERVER_URL) is running Immich v{version}, but this tool \
             requires v{IMPORT_MIN_VERSION} or newer. Upgrade the import instance."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Shared-link assertions (§5 step 6, E3)
// ---------------------------------------------------------------------------------------

/// What [`assert_shared_link`] captures from a passing shared link — everything the rest of
/// startup (and the final summary) needs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportAlbumInfo {
    pub album_id: Uuid,
    pub album_name: String,
    pub asset_count: u64,
    /// `true` when `expiresAt` is set and within [`EXPIRY_WARNING_DAYS`] of `now` — already
    /// logged at `warn` by this function; carried on the result too so a caller (and this
    /// module's own tests) can observe it without scraping log output.
    pub expiring_soon: bool,
}

/// How many days out from expiry `PLAN.md` §5 step 6 wants a warning.
const EXPIRY_WARNING_DAYS: i64 = 7;

/// `SharedLinkType`'s wire spelling, for actionable error/log text (the derived `Debug`
/// would print `Individual`/`Unrecognized`, not the `INDIVIDUAL` an operator would recognise
/// from the Immich UI or API docs). Mirrors `sync.rs`'s `status_str` helper.
fn shared_link_type_str(t: dto::SharedLinkType) -> &'static str {
    match t {
        dto::SharedLinkType::Album => "ALBUM",
        dto::SharedLinkType::Individual => "INDIVIDUAL",
        dto::SharedLinkType::Unrecognized => "UNRECOGNIZED",
    }
}

/// §5 step 6 (E3): asserts every property `PLAN.md` requires of the shared link, each with
/// its own actionable message, then captures the album fields the rest of startup needs.
/// Order matters here — `type == ALBUM` is checked first since none of the later assertions
/// (which all assume an album exists) make sense otherwise.
pub fn assert_shared_link(
    link: &dto::SharedLinkResponseDto,
    now: DateTime<Utc>,
) -> anyhow::Result<ExportAlbumInfo> {
    if link.r#type != dto::SharedLinkType::Album {
        anyhow::bail!(
            "EXPORT_ALBUM_URL points at a {} share link, not an album link; an individual-asset \
             share link has no album to mirror. On the export instance, create an album share \
             link instead and update EXPORT_ALBUM_URL to point at it.",
            shared_link_type_str(link.r#type)
        );
    }

    let album = link.album.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "the shared link reports type ALBUM but its response carries no album; this looks \
             like a server-side inconsistency on the export instance, not something fixable by \
             changing configuration here"
        )
    })?;

    if !link.allow_download {
        anyhow::bail!(
            "the shared link has \"Allow download\" turned off, so every asset download will \
             fail. On the export instance, edit the share link and enable \"Allow download\"."
        );
    }

    let mut expiring_soon = false;
    if let Some(expires_at) = link.expires_at {
        if expires_at <= now {
            anyhow::bail!(
                "the shared link expired at {expires_at} (now is {now}). On the export instance, \
                 create a new share link (or extend this one's expiry), then update \
                 EXPORT_ALBUM_URL."
            );
        }
        let remaining = expires_at - now;
        if remaining <= chrono::Duration::days(EXPIRY_WARNING_DAYS) {
            warn!(
                "the export shared link expires soon; renew it on the export instance before it \
                 does, or syncing will silently stop working expires_at={expires_at} \
                 remaining_days={}",
                remaining.num_days()
            );
            expiring_soon = true;
        }
    }

    Ok(ExportAlbumInfo {
        album_id: album.id,
        album_name: album.album_name.clone(),
        asset_count: album.asset_count,
        expiring_soon,
    })
}

// ---------------------------------------------------------------------------------------
// API key permission check (§5 step 8, I2)
// ---------------------------------------------------------------------------------------

/// §5 step 8 (I2): requires [`dto::REQUIRED_PERMISSIONS`] ⊆ the key's own permissions (or
/// the `all` wildcard — see [`dto::ApiKeyResponseDto::missing_permissions`]), failing with
/// *exactly* the missing permissions named, never a generic "insufficient permissions".
pub fn check_api_key_permissions(key: &dto::ApiKeyResponseDto) -> anyhow::Result<()> {
    let missing = key.missing_permissions(&dto::REQUIRED_PERMISSIONS);
    if !missing.is_empty() {
        anyhow::bail!(
            "IMPORT_API_KEY is missing the following permission(s): {}. On the import instance, \
             edit the API key and grant them (or use a key with the \"all\" wildcard).",
            missing.join(", ")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Startup summary (§5 step 10)
// ---------------------------------------------------------------------------------------

/// Everything `PLAN.md` §5 step 10 wants logged once startup succeeds. Returned (not just
/// logged) so `main.rs` and this module's own integration test can inspect it directly
/// rather than scraping log output.
#[derive(Debug, Clone)]
pub struct StartupSummary {
    pub export_version: Version,
    pub import_version: Version,
    pub share_link_id: Uuid,
    pub share_link_type: &'static str,
    pub share_link_expires_at: Option<DateTime<Utc>>,
    pub source_album_name: String,
    pub source_asset_count: u64,
    pub target_album_name: String,
    pub target_album_id: Uuid,
    pub interval: std::time::Duration,
    pub concurrency: u32,
}

impl StartupSummary {
    /// Emits the single `info`-level "startup complete" line `PLAN.md` §5 step 10 asks for.
    pub fn log(&self) {
        let expires_at = self
            .share_link_expires_at
            .map_or_else(|| "never".to_owned(), |t| t.to_rfc3339());
        info!(
            // Field labels use the same export/import vocabulary as the environment
            // variables an operator configured, so a log line and the config it came from
            // name the same thing the same way. (The struct's own fields keep the
            // source/target role names — they describe the data flow, not the config.)
            "startup complete export_version={} import_version={} share_link_id={} \
             share_link_type={} share_link_expires_at={expires_at} export_album={:?} \
             export_asset_count={} import_album={:?} import_album_id={} interval={} \
             concurrency={}",
            self.export_version,
            self.import_version,
            self.share_link_id,
            self.share_link_type,
            self.source_album_name,
            self.source_asset_count,
            self.target_album_name,
            self.target_album_id,
            humantime::format_duration(self.interval),
            self.concurrency
        );
    }
}

// ---------------------------------------------------------------------------------------
// Orchestration (§5 steps 3-10)
// ---------------------------------------------------------------------------------------

/// Everything [`run_startup`] hands back to `main.rs`: a fully wired [`SyncContext`] ready
/// for the scheduler to call [`SyncContext::run_once`] on, plus the summary that was just
/// logged (exposed for tests and any future caller that wants it, not because `main.rs`
/// needs to do anything further with it).
pub struct StartupOutcome {
    pub sync: SyncContext,
    pub summary: StartupSummary,
}

/// Runs `PLAN.md` §5 steps 3 through 10 in order, each with its own actionable failure
/// message (naming the env var or the remote-side setting to change, never a bare
/// propagated HTTP error) — see the individual step functions above for the exact wording.
/// `Err` here is always fatal: `main.rs` logs it and exits non-zero, per §5's "the process
/// exits non-zero on any failure".
pub async fn run_startup(config: &Config) -> anyhow::Result<StartupOutcome> {
    // Step 3 — parse EXPORT_ALBUM_URL.
    let (export_api_base, share_ref) = crate::share_url::parse_share_url(&config.export_album_url)
        .context("failed to parse EXPORT_ALBUM_URL")?;

    let retry_policy = RetryPolicy::default();
    let export = ExportClient::new(
        export_api_base,
        share_ref,
        config.request_timeout,
        config.transfer_timeout,
        retry_policy.clone(),
    )
    .context("failed to build the export-side HTTP client")?;

    // Step 4 — E1: export server version gate.
    let export_version = export
        .server_version()
        .await
        .context("failed to reach the export instance's GET /server/version")?;
    info!("export server version {export_version}");
    check_export_version(export_version)?;

    // Step 5 — E2: shared-link password login, only if one is configured.
    if let Some(password) = &config.export_album_password {
        match export.login(password).await {
            Ok(LoginOutcome::LoggedIn) => info!("logged in to the export shared link"),
            // `NotPasswordProtected` already warns inside `ExportClient::login` itself.
            Ok(LoginOutcome::NotPasswordProtected) => {}
            Err(ExportError::WrongPassword) => {
                anyhow::bail!(
                    "the export instance rejected EXPORT_ALBUM_PASSWORD with 401 Unauthorized: \
                     either the password is wrong, or EXPORT_ALBUM_URL's key/slug itself is \
                     invalid. Double check both."
                );
            }
            Err(err) => return Err(err).context("shared-link login failed"),
        }
    }

    // Step 6 — E3: the shared link's own metadata, then every §5-step-6 assertion.
    let link = export
        .shared_link_me()
        .await
        .context("failed to fetch the shared link's own metadata (GET /shared-links/me)")?;
    let album_info = assert_shared_link(&link, Utc::now())?;

    // Import client, built now so steps 7-9 can use it.
    let import = ImportClient::new(
        config.import_api_base()?,
        &config.import_api_key,
        config.request_timeout,
        config.transfer_timeout,
        retry_policy.clone(),
    )
    .context("failed to build the import-side HTTP client")?;

    // Step 7 — I1: import server version gate.
    let import_version = import
        .server_version()
        .await
        .context("failed to reach the import instance's GET /server/version")?;
    info!("import server version {import_version}");
    check_import_version(import_version)?;

    // Step 8 — I2: API key permission check.
    let api_key = import
        .get_api_key()
        .await
        .context("failed to fetch the import API key's own permissions (GET /api-keys/me)")?;
    check_api_key_permissions(&api_key)?;

    // Step 9 — I3: resolve IMPORT_ALBUM. `ImportError`'s own messages (album-not-found by
    // id, by name with the available albums listed, or ambiguous-name with the matching
    // ids) are already exactly the actionable text §5 step 9 asks for — nothing to add.
    let target_album = import.resolve_album(&config.import_album_ref()).await?;

    // Step 10 — summary.
    let summary = StartupSummary {
        export_version,
        import_version,
        share_link_id: link.id,
        share_link_type: shared_link_type_str(link.r#type),
        share_link_expires_at: link.expires_at,
        source_album_name: album_info.album_name,
        source_asset_count: album_info.asset_count,
        target_album_name: target_album.album_name,
        target_album_id: target_album.id,
        interval: config.import_interval,
        concurrency: config.import_concurrency,
    };
    summary.log();

    let sync = SyncContext::new(
        export,
        import,
        album_info.album_id,
        target_album.id,
        config.import_concurrency,
        config.transfer_timeout,
        retry_policy,
    );

    Ok(StartupOutcome { sync, summary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // ---- version gates -------------------------------------------------------------------

    fn version(major: u64, minor: u64, patch: u64) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn export_version_accepts_the_minimum_and_newer() {
        assert!(check_export_version(version(3, 0, 3)).is_ok());
        assert!(check_export_version(version(3, 1, 0)).is_ok());
    }

    #[test]
    fn export_version_rejects_older() {
        assert!(check_export_version(version(3, 0, 2)).is_err());
        assert!(check_export_version(version(2, 7, 5)).is_err());
    }

    #[test]
    fn import_version_accepts_the_minimum_and_newer() {
        assert!(check_import_version(version(3, 0, 0)).is_ok());
        assert!(check_import_version(version(3, 1, 0)).is_ok());
    }

    #[test]
    fn import_version_rejects_older() {
        assert!(check_import_version(version(2, 9, 9)).is_err());
    }

    // ---- shared-link assertions ------------------------------------------------------------

    fn base_link(now: DateTime<Utc>) -> dto::SharedLinkResponseDto {
        let _ = now;
        dto::SharedLinkResponseDto {
            id: Uuid::from_u128(1),
            r#type: dto::SharedLinkType::Album,
            album: Some(dto::AlbumResponseDto {
                id: Uuid::from_u128(2),
                album_name: "Holiday 2026".to_owned(),
                asset_count: 5,
            }),
            allow_download: true,
            allow_upload: false,
            show_metadata: true,
            expires_at: None,
        }
    }

    #[test]
    fn wrong_link_type_is_rejected() {
        let now = Utc::now();
        let mut link = base_link(now);
        link.r#type = dto::SharedLinkType::Individual;

        assert!(assert_shared_link(&link, now).is_err());
    }

    #[test]
    fn album_type_with_no_album_is_rejected() {
        let now = Utc::now();
        let mut link = base_link(now);
        link.album = None;

        assert!(assert_shared_link(&link, now).is_err());
    }

    #[test]
    fn allow_download_false_is_rejected() {
        let now = Utc::now();
        let mut link = base_link(now);
        link.allow_download = false;

        assert!(assert_shared_link(&link, now).is_err());
    }

    #[test]
    fn expired_link_is_rejected() {
        let now = Utc::now();
        let mut link = base_link(now);
        link.expires_at = Some(now - chrono::Duration::days(1));

        assert!(assert_shared_link(&link, now).is_err());
    }

    #[test]
    fn link_expiring_in_three_days_warns_but_does_not_fail() {
        let now = Utc::now();
        let mut link = base_link(now);
        link.expires_at = Some(now + chrono::Duration::days(3));

        let info = assert_shared_link(&link, now).expect("must succeed, only a warning");
        assert!(info.expiring_soon);
    }

    #[test]
    fn link_expiring_in_thirty_days_is_not_flagged() {
        let now = Utc::now();
        let mut link = base_link(now);
        link.expires_at = Some(now + chrono::Duration::days(30));

        let info = assert_shared_link(&link, now).unwrap();
        assert!(!info.expiring_soon);
    }

    #[test]
    fn link_with_no_expiry_is_never_flagged() {
        let now = Utc::now();
        let link = base_link(now);

        let info = assert_shared_link(&link, now).unwrap();
        assert!(!info.expiring_soon);
    }

    #[test]
    fn passing_link_captures_album_fields() {
        let now = Utc::now();
        let link = base_link(now);

        let info = assert_shared_link(&link, now).unwrap();
        assert_eq!(info.album_id, Uuid::from_u128(2));
        assert_eq!(info.album_name, "Holiday 2026");
        assert_eq!(info.asset_count, 5);
    }

    // ---- permission check -------------------------------------------------------------------

    fn api_key(permissions: Vec<String>) -> dto::ApiKeyResponseDto {
        dto::ApiKeyResponseDto {
            id: Uuid::from_u128(3),
            name: "sync-key".to_owned(),
            permissions,
        }
    }

    #[test]
    fn permission_check_passes_with_the_exact_required_set() {
        let key = api_key(
            dto::REQUIRED_PERMISSIONS
                .iter()
                .map(ToString::to_string)
                .collect(),
        );
        assert!(check_api_key_permissions(&key).is_ok());
    }

    #[test]
    fn permission_check_passes_with_the_all_wildcard() {
        let key = api_key(vec![dto::PERMISSION_ALL.to_owned()]);
        assert!(check_api_key_permissions(&key).is_ok());
    }

    /// Which permissions come back as missing is `ApiKeyResponseDto::missing_permissions`'s
    /// job and is tested exhaustively in `dto.rs`; all this function adds is turning a
    /// non-empty result into an error.
    #[test]
    fn permission_check_fails_when_a_permission_is_missing() {
        let key = api_key(vec![dto::PERMISSION_ASSET_UPLOAD.to_owned()]);
        assert!(check_api_key_permissions(&key).is_err());
    }

    // ---- run_startup end-to-end (real in-process HTTP, not a mock) --------------------------

    use axum::Json;
    use axum::Router;
    use axum::extract::Path;
    use axum::routing::get;
    use serde_json::json;

    use crate::immich::test_support::spawn_test_server;

    fn export_app() -> Router {
        Router::new()
            .route(
                "/server/version",
                get(|| async {
                    Json(json!({"major": 3, "minor": 1, "patch": 0, "prerelease": null}))
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
                            "assetCount": 5
                        },
                        "allowDownload": true,
                        "allowUpload": false,
                        "showMetadata": true,
                        "expiresAt": null
                    }))
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
                        "permissions": dto::REQUIRED_PERMISSIONS,
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

    #[tokio::test]
    async fn run_startup_happy_path_builds_a_ready_sync_context() {
        let (export_base, _export_server) =
            spawn_test_server(Router::new().nest("/api", export_app())).await;
        let (import_base, _import_server) =
            spawn_test_server(Router::new().nest("/api", import_app())).await;

        let target_album_id = "8a5e1e2b-2222-4444-8888-aaaaaaaaaaaa";
        let config = Config::try_parse_from([
            "immich-federation-at-home",
            "--export-album-url",
            &format!("{export_base}share/testkey"),
            "--import-server-url",
            import_base.as_str(),
            "--import-api-key",
            "test-api-key",
            "--import-album",
            target_album_id,
        ])
        .unwrap();

        let outcome = run_startup(&config).await.expect("startup should succeed");

        assert_eq!(outcome.summary.export_version, version(3, 1, 0));
        assert_eq!(outcome.summary.import_version, version(3, 1, 0));
        assert_eq!(outcome.summary.source_album_name, "Holiday 2026");
        assert_eq!(outcome.summary.source_asset_count, 5);
        assert_eq!(outcome.summary.target_album_name, "My Family Photos");
        assert_eq!(outcome.summary.target_album_id.to_string(), target_album_id);
    }

    #[tokio::test]
    async fn run_startup_fails_on_missing_permissions() {
        let (export_base, _export_server) =
            spawn_test_server(Router::new().nest("/api", export_app())).await;
        let restricted_import_app = Router::new()
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
                        "name": "read-only-key",
                        "permissions": [dto::PERMISSION_ASSET_UPLOAD],
                        "createdAt": "2024-01-01T00:00:00.000Z",
                        "updatedAt": "2024-01-01T00:00:00.000Z"
                    }))
                }),
            );
        let (import_base, _import_server) =
            spawn_test_server(Router::new().nest("/api", restricted_import_app)).await;

        let config = Config::try_parse_from([
            "immich-federation-at-home",
            "--export-album-url",
            &format!("{export_base}share/testkey"),
            "--import-server-url",
            import_base.as_str(),
            "--import-api-key",
            "test-api-key",
            "--import-album",
            "8a5e1e2b-2222-4444-8888-aaaaaaaaaaaa",
        ])
        .unwrap();

        assert!(
            run_startup(&config).await.is_err(),
            "startup should have failed: the key only has asset.upload"
        );
    }
}
