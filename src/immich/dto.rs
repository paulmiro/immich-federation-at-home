//! Hand-written `serde` types for the Immich API surface used by `PLAN.md` §2 (calls
//! E1–E5, I1–I6). Every field name, type, and required/nullable-ness below was checked
//! against `openapi/immich-openapi-3.1.0.json` with `jq` — see the doc comment on each
//! type for the exact query used and what it returned. Where `PLAN.md`/`scratch/RESEARCH.md`
//! said something different from the spec, the spec wins; discrepancies are called out
//! below and recorded in `NOTES.md`.
//!
//! Two rules apply to every type here:
//! * `#[serde(rename_all = "...")]`, never `deny_unknown_fields` — the server sends far
//!   more fields than we model, and a spec field we don't care about must never break
//!   deserialization.
//! * Any enum whose value space the server might extend in a future release (asset type,
//!   upload status, bulk-check action/reason, …) gets a `#[serde(other)]` catch-all
//!   variant named `Unrecognized`, so an unknown value deserializes instead of erroring
//!   the whole response out from under an otherwise-healthy sync run.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------------------
// E1 / I1 — GET /server/version
// ---------------------------------------------------------------------------------------

/// `GET /server/version` response. `jq '.components.schemas.ServerVersionResponseDto'`:
/// all four fields (`major`, `minor`, `patch`, `prerelease`) are `required`; `prerelease`
/// is additionally `nullable` (an integer or `null`, key always present). Unauthenticated
/// endpoint on both the export and import sides (`PLAN.md` E1/I1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerVersionResponseDto {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<u64>,
}

// ---------------------------------------------------------------------------------------
// E3 — GET /shared-links/me (and E2's POST /shared-links/login, same response shape)
// ---------------------------------------------------------------------------------------

/// `SharedLinkType`. `jq '.components.schemas.SharedLinkType'` → `enum: ["ALBUM",
/// "INDIVIDUAL"]`. `PLAN.md` §5 step 6 only ever compares against `"ALBUM"`, but the
/// server could plausibly grow a third link type, hence `Unrecognized`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SharedLinkType {
    Album,
    Individual,
    #[serde(other)]
    Unrecognized,
}

/// The subset of `SharedLinkResponseDto` that `PLAN.md` §5 step 6 asserts on:
/// `jq '.components.schemas.SharedLinkResponseDto'` → `required: [allowDownload,
/// allowUpload, assets, createdAt, description, expiresAt, id, key, password,
/// showMetadata, slug, type, userId]`.
///
/// Notably **`album` is *not* in that `required` list** (only `type`/`id`/etc. are) —
/// makes sense, an `INDIVIDUAL`-type link has no album, so it's `Option<AlbumResponseDto>`
/// here rather than the plan's implicit "it's just there" framing. `expiresAt` *is*
/// required but also `nullable` (an ISO-8601 string or `null`, key always present) —
/// `Option<DateTime<Utc>>`. We deliberately don't model `assets`, `key`, `password`,
/// `slug`, `userId`, `createdAt`, `description` here: nothing downstream needs them, and
/// `key`/`password` in particular must never be logged (`PLAN.md` §8) — not carrying them
/// into a struct that might end up in a `{:?}` somewhere is one less way to leak them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedLinkResponseDto {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub r#type: SharedLinkType,
    pub album: Option<AlbumResponseDto>,
    pub allow_download: bool,
    pub allow_upload: bool,
    pub show_metadata: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

/// `SharedLinkLoginDto` — E2's request body, `POST /shared-links/login {"password":"…"}`.
/// `jq '.components.schemas.SharedLinkLoginDto'` → `{password: string}`, required.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedLinkLoginDto {
    pub password: String,
}

// ---------------------------------------------------------------------------------------
// I3 — GET /albums/{id}, GET /albums?name=… (and embedded in SharedLinkResponseDto.album)
// ---------------------------------------------------------------------------------------

/// The subset of `AlbumResponseDto` we need: `jq '.components.schemas.AlbumResponseDto'`
/// → `required` includes `albumName`, `assetCount`, and `id` (among others we don't
/// model, e.g. `albumUsers`, `shared`, timestamps).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumResponseDto {
    pub id: Uuid,
    pub album_name: String,
    pub asset_count: u64,
}

// ---------------------------------------------------------------------------------------
// E4 — POST /search/metadata
// ---------------------------------------------------------------------------------------

/// `AssetOrder` — request-only (never appears in a response we parse), so `Serialize`
/// only. `jq '.components.schemas.AssetOrder'` → `enum: ["asc", "desc"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetOrder {
    Asc,
    Desc,
}

/// The subset of `MetadataSearchDto` E4 needs: `{"albumIds":[…],"page":N,"size":250,
/// "order":"asc"}`. `jq '.components.schemas.MetadataSearchDto'` has **no top-level
/// `required` array at all** (it's a giant bag of optional filters) — we still send all
/// four fields on every request, so they're plain (non-`Option`) here; that's a choice
/// made for *our* request shape, not something the spec forces.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataSearchDto {
    pub album_ids: Vec<Uuid>,
    pub page: u32,
    pub size: u32,
    pub order: AssetOrder,
}

/// `AssetTypeEnum`. `jq '.components.schemas.AssetTypeEnum'` → `enum: ["IMAGE", "VIDEO",
/// "AUDIO", "OTHER"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AssetTypeEnum {
    Image,
    Video,
    Audio,
    Other,
    #[serde(other)]
    Unrecognized,
}

/// `AssetVisibility`. `jq '.components.schemas.AssetVisibility'` → `enum: ["archive",
/// "timeline", "hidden", "locked"]`.
///
/// Only `Hidden` is acted on (see
/// [`ExportClient::list_album_assets`](crate::immich::export::ExportClient::list_album_assets)):
/// it is the flag Immich puts on the motion-video half of a live photo, so those are
/// skipped rather than uploaded as standalone video clips. The other variants exist so the
/// value round-trips through `serde` — an archived asset is transferred like any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetVisibility {
    Archive,
    Timeline,
    Hidden,
    Locked,
    #[serde(other)]
    Unrecognized,
}

/// The subset of `AssetResponseDto` §6 step 1 needs. `jq
/// '.components.schemas.AssetResponseDto'` → `required` includes `checksum`, `duration`
/// (nullable — see below), `fileCreatedAt`, `fileModifiedAt`, `id`, `originalFileName`,
/// `originalPath`, `type`, among many fields we don't model. **`originalMimeType` is *not*
/// in the `required` list** — `PLAN.md`'s bullet list implies it's always there, but the
/// spec disagrees, so it's `Option<String>` here (recorded in `NOTES.md`).
///
/// `checksum` *is* spec-required and non-nullable (a plain `String`, not `Option`), which
/// is stronger than `PLAN.md`'s framing ("checksum being absent is a real failure mode we
/// must handle explicitly, not `unwrap()`"). Modelling it as `String` per the spec still
/// satisfies that: if the key is genuinely missing from a response, deserializing the
/// enclosing page fails with a structured `serde_json::Error` — the caller gets a `Result`
/// to handle, never a panic. What the spec's "required" *doesn't* guarantee is a
/// *non-empty* value; the next step (export.rs, task 5) must still treat an empty
/// `checksum` string as unusable for dedup rather than trusting it blindly (see
/// `NOTES.md`).
///
/// `duration` is required **and** nullable (`integer` or `null`, key always present,
/// "Video/gif duration in milliseconds \[or\] null for static images") — `Option<i64>`.
///
/// `originalPath` — `jq '.components.schemas.AssetResponseDto.required'` lists it, and
/// `jq '.components.schemas.AssetResponseDto.properties.originalPath'` is
/// `{"description": "Original file path", "type": "string"}` with no `"nullable": true`,
/// i.e. it is spec-required **and** non-nullable, the same strong guarantee `checksum`
/// gets. It is still modelled as `Option<String>` with `#[serde(default)]` here anyway,
/// deliberately weaker than the spec — because the cost of being wrong is asymmetric. If a
/// future server version ever stops sending this field for some response shape we haven't
/// seen yet (a shared-link projection, say), a plain `String` field would fail
/// deserialization of the *entire page* that asset appears on, taking down every other
/// asset on that page along with it — assets that have nothing to do with path-hash
/// detection. `#[serde(default)]` makes a missing key deserialize to `None` instead of an
/// error. `None` is then treated as "assume content-hashed" (see
/// [`crate::immich::export::SourceAsset::checksum_is_path_hash`]) — today's behaviour, and
/// the safe fallback: it can only cause a path-hashed asset to be misclassified as
/// content-hashed (which just means step 3b's checksum verification does nothing useful
/// for that one asset, as it already does today), never the reverse.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetResponseDto {
    pub id: Uuid,
    pub checksum: String,
    pub original_file_name: String,
    #[serde(rename = "type")]
    pub r#type: AssetTypeEnum,
    pub file_created_at: DateTime<Utc>,
    pub file_modified_at: DateTime<Utc>,
    /// Not required by the spec — see the struct doc comment.
    pub original_mime_type: Option<String>,
    /// Milliseconds; `None` for static images. Required-but-nullable — see the struct doc
    /// comment.
    pub duration: Option<i64>,
    /// Spec-required and non-nullable, modelled as `Option` anyway for forward
    /// compatibility — see the struct doc comment.
    #[serde(default)]
    pub original_path: Option<String>,
    /// Spec-required and non-nullable, modelled as `Option` anyway for the same reason
    /// `original_path` is — see the struct doc comment. `None` (a server that stopped
    /// sending the field) means "not hidden", i.e. transfer it, which is what this tool did
    /// before the field was modelled at all.
    #[serde(default)]
    pub visibility: Option<AssetVisibility>,
}

/// `SearchResponseDto` — the top-level `POST /search/metadata` response. `jq
/// '.components.schemas.SearchResponseDto'` → `required: [albums, assets]`. We only care
/// about `assets` (a shared-link search has no `albums` facet worth reading); `albums` is
/// simply not modelled and ignored by serde since there's no `deny_unknown_fields`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponseDto {
    pub assets: SearchAssetResponseDto,
}

/// The paginated `assets` object inside `SearchResponseDto`. `jq
/// '.components.schemas.SearchAssetResponseDto'` → `required: [count, facets, items,
/// nextPage, total]`.
///
/// **`nextPage` is a `string` (nullable), not a number** — verified directly against the
/// schema (`"nextPage": {"type": "string", "nullable": true, ...}`), contradicting the
/// easy assumption that a "next page" field would be numeric. In practice Immich's server
/// emits a stringified page number (e.g. `"2"`), but nothing in the spec promises that
/// format, so treat it as an opaque continuation token: page until it's `None`, don't
/// parse it as an integer. See `NOTES.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAssetResponseDto {
    pub items: Vec<AssetResponseDto>,
    pub next_page: Option<String>,
    /// Deprecated by upstream as of v3.0.0 per `x-immich-history`, but still present on
    /// the wire; kept since the task list asked for it explicitly.
    pub total: u64,
    pub count: u64,
}

// ---------------------------------------------------------------------------------------
// I4 — POST /assets/bulk-upload-check
// ---------------------------------------------------------------------------------------

/// `jq '.components.schemas.AssetBulkUploadCheckItem'` → `required: [checksum, id]`.
/// `id` here is *not* the export-side asset UUID typed as such — the spec's own
/// description is "Client-side identifier echoed in the response to match results to
/// inputs (e.g. filename)" and its schema type is a plain `string`, not `format: uuid`.
/// `PLAN.md`'s I4 uses the export asset's UUID *string* for it, which is a valid (if
/// spec-looser-than-that) choice, not a requirement — hence `String` here, matching the
/// spec exactly rather than over-typing it as `Uuid`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBulkUploadCheckItem {
    pub id: String,
    pub checksum: String,
}

/// `jq '.components.schemas.AssetBulkUploadCheckDto'` → `{assets: [...]}`, required.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBulkUploadCheckDto {
    pub assets: Vec<AssetBulkUploadCheckItem>,
}

/// `jq '.components.schemas.AssetUploadAction'` → `enum: ["accept", "reject"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetUploadAction {
    Accept,
    Reject,
    #[serde(other)]
    Unrecognized,
}

/// `jq '.components.schemas.AssetRejectReason'` → `enum: ["duplicate",
/// "unsupported-format"]`. `kebab-case` maps `UnsupportedFormat` → `unsupported-format`
/// and `Duplicate` → `duplicate` in one shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetRejectReason {
    Duplicate,
    UnsupportedFormat,
    #[serde(other)]
    Unrecognized,
}

/// `jq '.components.schemas.AssetBulkUploadCheckResult'` → `required: [action, id]`;
/// `assetId`, `isTrashed`, `reason` are all optional. "Existing asset ID if duplicate" per
/// the spec description, hence `Option<Uuid>` (this one *is* `format: uuid`, unlike the
/// request-side `id`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBulkUploadCheckResult {
    pub id: String,
    pub action: AssetUploadAction,
    pub asset_id: Option<Uuid>,
    pub is_trashed: Option<bool>,
    pub reason: Option<AssetRejectReason>,
}

/// `jq '.components.schemas.AssetBulkUploadCheckResponseDto'` → `{results: [...]}`,
/// required.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBulkUploadCheckResponseDto {
    pub results: Vec<AssetBulkUploadCheckResult>,
}

// ---------------------------------------------------------------------------------------
// I5 — POST /assets (multipart upload)
// ---------------------------------------------------------------------------------------

/// `jq '.components.schemas.AssetMediaStatus'` → `enum: ["created", "duplicate"]` **in
/// this spec version** — no `"replaced"`, contrary to `RESEARCH.md`'s speculative
/// three-way framing (`created/replaced/duplicate`) elsewhere in the codebase's notes.
/// `PLAN.md`'s own I5 detail agrees with the spec (only `created`/`duplicate`). Recorded
/// in `NOTES.md`. `Unrecognized` covers a future third status regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetMediaStatus {
    Created,
    Duplicate,
    #[serde(other)]
    Unrecognized,
}

/// `jq '.components.schemas.AssetMediaResponseDto'` → `required: [id, status]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetMediaResponseDto {
    pub id: Uuid,
    pub status: AssetMediaStatus,
}

// ---------------------------------------------------------------------------------------
// I6 — PUT /albums/{id}/assets
// ---------------------------------------------------------------------------------------

/// `BulkIdsDto` — I6's request body, `{"ids": [...]}`.
/// `jq '.components.schemas.BulkIdsDto'` → `{ids: [uuid, ...]}`, required.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkIdsDto {
    pub ids: Vec<Uuid>,
}

/// `jq '.components.schemas.BulkIdErrorReason'` → `enum: ["duplicate", "no_permission",
/// "not_found", "unknown", "validation"]`. Note the spec's own literal `"unknown"` variant
/// (mapped to `Unknown` below) is distinct from this crate's `#[serde(other)]`
/// catch-all (`Unrecognized`) for values outside that set entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkIdErrorReason {
    Duplicate,
    NoPermission,
    NotFound,
    Unknown,
    Validation,
    #[serde(other)]
    Unrecognized,
}

/// `jq '.components.schemas.BulkIdResponseDto'` → `required: [id, success]`; `error` (and
/// `errorMessage`, not modelled here) are optional. `PLAN.md` I6: "already-present assets
/// come back `{success:false, error:\"duplicate\"}`".
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkIdResponseDto {
    pub id: Uuid,
    pub success: bool,
    pub error: Option<BulkIdErrorReason>,
}

// ---------------------------------------------------------------------------------------
// I2 — GET /api-keys/me
// ---------------------------------------------------------------------------------------

/// `jq '.components.schemas.ApiKeyResponseDto'` → `required: [createdAt, id, name,
/// permissions, updatedAt]`. `permissions` is `items: {$ref: Permission}`, and `jq
/// '.components.schemas.Permission'` lists **over 140** literal variants (and growing —
/// the server adds new fine-grained permissions across releases). Modelling that as a
/// closed Rust enum would mean this binary breaks (deserialize error → the whole
/// `GET /api-keys/me` call fails) every time upstream adds a permission we don't otherwise
/// care about, which is exactly backwards for a startup check whose only job is "does the
/// key have *these three* strings". `Vec<String>` is the deliberately loose,
/// future-proof choice; see [`ApiKeyResponseDto::missing_permissions`] for how we still
/// get precise, exact-match checking out of it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyResponseDto {
    pub id: Uuid,
    pub name: String,
    pub permissions: Vec<String>,
}

/// The `all` wildcard permission (`PLAN.md` §2: "`isGranted` is exact set membership plus
/// an `all` wildcard — no prefix globbing").
pub const PERMISSION_ALL: &str = "all";

/// Needed for I5/I4 — `POST /assets`, `POST /assets/bulk-upload-check`.
pub const PERMISSION_ASSET_UPLOAD: &str = "asset.upload";
/// Needed for I3 — `GET /albums`, `GET /albums/{id}`.
pub const PERMISSION_ALBUM_READ: &str = "album.read";
/// Needed for I6 — `PUT /albums/{id}/assets`.
pub const PERMISSION_ALBUM_ASSET_CREATE: &str = "albumAsset.create";

/// The exact permission set `PLAN.md` §5 step 8 requires the import API key to have (or
/// `all`).
pub const REQUIRED_PERMISSIONS: [&str; 3] = [
    PERMISSION_ASSET_UPLOAD,
    PERMISSION_ALBUM_READ,
    PERMISSION_ALBUM_ASSET_CREATE,
];

impl ApiKeyResponseDto {
    /// Returns the subset of `required` this key does *not* have, honouring the `all`
    /// wildcard (any of `self.permissions` being exactly `"all"` clears every
    /// requirement). This is exact set membership, deliberately **not** prefix matching —
    /// `PLAN.md` §2 is explicit that Immich's own `isGranted` check works this way, so e.g.
    /// having `"asset.read"` must not be mistaken for having `"asset.upload"`. An empty
    /// return value means the key is sufficient.
    pub fn missing_permissions(&self, required: &[&'static str]) -> Vec<&'static str> {
        if self.permissions.iter().any(|p| p == PERMISSION_ALL) {
            return Vec::new();
        }
        required
            .iter()
            .copied()
            .filter(|need| !self.permissions.iter().any(|have| have == need))
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Error body (all endpoints) — NOT part of the OpenAPI document
// ---------------------------------------------------------------------------------------

/// Immich's default `NestJS` exception-filter error body: `{statusCode, message, error}`.
/// **Not documented anywhere in the vendored spec** — `jq
/// '[.. | objects | select(has("statusCode"))] | length'` over the whole document returns
/// `0`. This shape is `NestJS`'s well-known default `HttpExceptionFilter` output (also
/// described, consistently, in `PLAN.md`'s I5 request detail), not something verified
/// against `openapi/immich-openapi-3.1.0.json` — recorded in `NOTES.md` since the task
/// brief for this file otherwise insists everything be checked against the spec.
///
/// `message` is sometimes a bare string, sometimes `string[]` (`NestJS`'s built-in
/// `ValidationPipe` reports multiple `class-validator` violations as an array) —
/// [`deserialize_message`] flattens either shape into one `String` by joining with `"; "`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImmichErrorBody {
    #[serde(default, deserialize_with = "deserialize_message")]
    pub message: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub status_code: Option<u16>,
}

/// Accepts either a bare JSON string or an array of strings for `message`, flattening the
/// array case by joining with `"; "`. `#[serde(default)]` on the field is still required
/// alongside this — using `deserialize_with` opts the field out of serde's usual "missing
/// key on an `Option<T>` field means `None`" built-in behaviour, so without `default` a
/// response body that omits `message` entirely would fail to deserialize instead of
/// yielding `None`.
fn deserialize_message<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Message {
        One(String),
        Many(Vec<String>),
    }

    Ok(
        Option::<Message>::deserialize(deserializer)?.map(|m| match m {
            Message::One(s) => s,
            Message::Many(items) => items.join("; "),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ServerVersionResponseDto --------------------------------------------------

    #[test]
    fn server_version_deserializes() {
        let dto: ServerVersionResponseDto =
            serde_json::from_str(r#"{"major":3,"minor":1,"patch":0,"prerelease":null}"#).unwrap();
        assert_eq!(dto.major, 3);
        assert_eq!(dto.minor, 1);
        assert_eq!(dto.patch, 0);
        assert_eq!(dto.prerelease, None);
    }

    #[test]
    fn server_version_prerelease_present() {
        let dto: ServerVersionResponseDto =
            serde_json::from_str(r#"{"major":3,"minor":1,"patch":0,"prerelease":1}"#).unwrap();
        assert_eq!(dto.prerelease, Some(1));
    }

    // ---- SharedLinkResponseDto / SharedLinkType --------------------------------------

    #[test]
    fn shared_link_album_type_with_album_deserializes() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "type": "ALBUM",
            "album": {
                "id": "9c858901-8a57-4791-81fe-4c455b099bc9",
                "albumName": "Holiday 2026",
                "assetCount": 42
            },
            "allowDownload": true,
            "allowUpload": false,
            "showMetadata": true,
            "expiresAt": "2027-01-01T00:00:00.000Z",
            "assets": [],
            "createdAt": "2024-01-01T00:00:00.000Z",
            "description": null,
            "key": "should-never-be-read-by-us",
            "password": null,
            "slug": null,
            "userId": "3fa85f64-5717-4562-b3fc-2c963f66afa6"
        }"#;
        let dto: SharedLinkResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.r#type, SharedLinkType::Album);
        let album = dto.album.expect("album present for ALBUM-type link");
        assert_eq!(album.album_name, "Holiday 2026");
        assert_eq!(album.asset_count, 42);
        assert!(dto.allow_download);
        assert!(!dto.allow_upload);
        assert!(dto.show_metadata);
        assert!(dto.expires_at.is_some());
    }

    #[test]
    fn shared_link_individual_type_has_no_album() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "type": "INDIVIDUAL",
            "allowDownload": true,
            "allowUpload": false,
            "showMetadata": true,
            "expiresAt": null,
            "assets": [],
            "createdAt": "2024-01-01T00:00:00.000Z",
            "description": null,
            "key": "x",
            "password": null,
            "slug": null,
            "userId": "3fa85f64-5717-4562-b3fc-2c963f66afa6"
        }"#;
        let dto: SharedLinkResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.r#type, SharedLinkType::Individual);
        assert!(dto.album.is_none());
        assert!(dto.expires_at.is_none());
    }

    #[test]
    fn shared_link_type_unknown_variant_is_forward_compatible() {
        let dto: SharedLinkType = serde_json::from_str(r#""SOMETHING_NEW""#).unwrap();
        assert_eq!(dto, SharedLinkType::Unrecognized);
    }

    #[test]
    fn shared_link_login_dto_serializes_password_only() {
        let dto = SharedLinkLoginDto {
            password: "hunter2".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&dto).unwrap(),
            r#"{"password":"hunter2"}"#
        );
    }

    // ---- AssetResponseDto -------------------------------------------------------------

    #[test]
    fn asset_response_full_fields_deserializes() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "checksum": "kR3lXyZ9abcdefghijklmnopqrs=",
            "originalFileName": "IMG_4312.HEIC",
            "type": "IMAGE",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "originalMimeType": "image/heic",
            "duration": null
        }"#;
        let asset: AssetResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(asset.checksum, "kR3lXyZ9abcdefghijklmnopqrs=");
        assert_eq!(asset.original_file_name, "IMG_4312.HEIC");
        assert_eq!(asset.r#type, AssetTypeEnum::Image);
        assert_eq!(asset.original_mime_type.as_deref(), Some("image/heic"));
        assert_eq!(asset.duration, None);
    }

    #[test]
    fn asset_response_missing_original_mime_type_is_none() {
        // originalMimeType is NOT in the spec's `required` list — must not error out.
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "checksum": "abc=",
            "originalFileName": "video.mp4",
            "type": "VIDEO",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "duration": 15230
        }"#;
        let asset: AssetResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(asset.original_mime_type, None);
        assert_eq!(asset.duration, Some(15230));
        assert_eq!(asset.r#type, AssetTypeEnum::Video);
    }

    #[test]
    fn asset_response_original_path_present_is_some() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "checksum": "abc=",
            "originalFileName": "video.mp4",
            "originalPath": "/data/library/video.mp4",
            "type": "VIDEO",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "duration": null
        }"#;
        let asset: AssetResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(
            asset.original_path.as_deref(),
            Some("/data/library/video.mp4")
        );
    }

    #[test]
    fn asset_response_missing_original_path_is_none_not_an_error() {
        // originalPath IS spec-required, but dto.rs deliberately weakens this to Option —
        // see the struct doc comment. A response that omits it entirely must still
        // deserialize the rest of the page rather than erroring out.
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "checksum": "abc=",
            "originalFileName": "video.mp4",
            "type": "VIDEO",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "duration": null
        }"#;
        let asset: AssetResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(asset.original_path, None);
    }

    #[test]
    fn asset_response_visibility_deserializes_and_defaults_to_none() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "checksum": "abc=",
            "originalFileName": "IMG_4312.MOV",
            "type": "VIDEO",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "visibility": "hidden",
            "duration": 1500
        }"#;
        let asset: AssetResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(asset.visibility, Some(AssetVisibility::Hidden));

        let without = json.replace(r#""visibility": "hidden","#, "");
        let asset: AssetResponseDto = serde_json::from_str(&without).unwrap();
        assert_eq!(asset.visibility, None);
    }

    #[test]
    fn asset_visibility_unknown_variant_is_forward_compatible() {
        let v: AssetVisibility = serde_json::from_str(r#""quarantined""#).unwrap();
        assert_eq!(v, AssetVisibility::Unrecognized);
    }

    #[test]
    fn asset_type_unknown_variant_is_forward_compatible() {
        let t: AssetTypeEnum = serde_json::from_str(r#""HOLOGRAM""#).unwrap();
        assert_eq!(t, AssetTypeEnum::Unrecognized);
    }

    #[test]
    fn asset_response_missing_checksum_is_a_structured_error_not_a_panic() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "originalFileName": "video.mp4",
            "type": "VIDEO",
            "fileCreatedAt": "2026-05-01T12:00:00.000Z",
            "fileModifiedAt": "2026-05-01T12:00:01.000Z",
            "duration": null
        }"#;
        let result: Result<AssetResponseDto, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing required checksum should error");
    }

    // ---- MetadataSearchDto / AssetOrder (request-only) -------------------------------

    #[test]
    fn metadata_search_dto_serializes_expected_shape() {
        let dto = MetadataSearchDto {
            album_ids: vec![Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap()],
            page: 1,
            size: 250,
            order: AssetOrder::Asc,
        };
        let value: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["page"], 1);
        assert_eq!(value["size"], 250);
        assert_eq!(value["order"], "asc");
        assert_eq!(value["albumIds"][0], "3fa85f64-5717-4562-b3fc-2c963f66afa6");
    }

    // ---- SearchResponseDto / SearchAssetResponseDto (pagination) ----------------------

    #[test]
    fn search_response_next_page_is_a_string_token_not_a_number() {
        let json = r#"{
            "albums": {"total": 0, "items": []},
            "assets": {
                "items": [],
                "nextPage": "2",
                "total": 500,
                "count": 250
            }
        }"#;
        let dto: SearchResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.assets.next_page.as_deref(), Some("2"));
        assert_eq!(dto.assets.total, 500);
        assert_eq!(dto.assets.count, 250);
    }

    #[test]
    fn search_response_next_page_null_means_last_page() {
        let json = r#"{
            "albums": {"total": 0, "items": []},
            "assets": {"items": [], "nextPage": null, "total": 3, "count": 3}
        }"#;
        let dto: SearchResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.assets.next_page, None);
    }

    // ---- AssetBulkUploadCheck* ---------------------------------------------------------

    #[test]
    fn bulk_upload_check_request_serializes() {
        let dto = AssetBulkUploadCheckDto {
            assets: vec![AssetBulkUploadCheckItem {
                id: "3fa85f64-5717-4562-b3fc-2c963f66afa6".to_owned(),
                checksum: "abc=".to_owned(),
            }],
        };
        let value: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(
            value["assets"][0]["id"],
            "3fa85f64-5717-4562-b3fc-2c963f66afa6"
        );
        assert_eq!(value["assets"][0]["checksum"], "abc=");
    }

    #[test]
    fn bulk_upload_check_response_accept_reject_variants() {
        let json = r#"{
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
        }"#;
        let dto: AssetBulkUploadCheckResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.results.len(), 3);
        assert_eq!(dto.results[0].action, AssetUploadAction::Accept);
        assert_eq!(dto.results[0].reason, None);
        assert_eq!(dto.results[1].action, AssetUploadAction::Reject);
        assert_eq!(dto.results[1].reason, Some(AssetRejectReason::Duplicate));
        assert_eq!(dto.results[1].is_trashed, Some(true));
        assert!(dto.results[1].asset_id.is_some());
        assert_eq!(
            dto.results[2].reason,
            Some(AssetRejectReason::UnsupportedFormat)
        );
    }

    // ---- AssetMediaResponseDto ---------------------------------------------------------

    #[test]
    fn asset_media_response_created_and_duplicate() {
        let created: AssetMediaResponseDto = serde_json::from_str(
            r#"{"id": "3fa85f64-5717-4562-b3fc-2c963f66afa6", "status": "created"}"#,
        )
        .unwrap();
        assert_eq!(created.status, AssetMediaStatus::Created);

        let duplicate: AssetMediaResponseDto = serde_json::from_str(
            r#"{"id": "3fa85f64-5717-4562-b3fc-2c963f66afa6", "status": "duplicate"}"#,
        )
        .unwrap();
        assert_eq!(duplicate.status, AssetMediaStatus::Duplicate);
    }

    #[test]
    fn asset_media_status_unknown_variant_is_forward_compatible() {
        let status: AssetMediaStatus = serde_json::from_str(r#""replaced""#).unwrap();
        assert_eq!(status, AssetMediaStatus::Unrecognized);
    }

    // ---- BulkIdResponseDto --------------------------------------------------------------

    #[test]
    fn bulk_id_response_success_and_duplicate_error() {
        let ok: BulkIdResponseDto = serde_json::from_str(
            r#"{"id": "3fa85f64-5717-4562-b3fc-2c963f66afa6", "success": true}"#,
        )
        .unwrap();
        assert!(ok.success);
        assert_eq!(ok.error, None);

        let dup: BulkIdResponseDto = serde_json::from_str(
            r#"{"id": "3fa85f64-5717-4562-b3fc-2c963f66afa6", "success": false, "error": "duplicate"}"#,
        )
        .unwrap();
        assert!(!dup.success);
        assert_eq!(dup.error, Some(BulkIdErrorReason::Duplicate));
    }

    #[test]
    fn bulk_ids_dto_serializes() {
        let dto = BulkIdsDto {
            ids: vec![Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap()],
        };
        assert_eq!(
            serde_json::to_string(&dto).unwrap(),
            r#"{"ids":["3fa85f64-5717-4562-b3fc-2c963f66afa6"]}"#
        );
    }

    // ---- ApiKeyResponseDto / permission subset-check -----------------------------------

    #[test]
    fn api_key_response_deserializes() {
        let json = r#"{
            "id": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
            "name": "sync-key",
            "permissions": ["asset.upload", "album.read"],
            "createdAt": "2024-01-01T00:00:00.000Z",
            "updatedAt": "2024-01-01T00:00:00.000Z"
        }"#;
        let dto: ApiKeyResponseDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.permissions, vec!["asset.upload", "album.read"]);
    }

    #[test]
    fn missing_permissions_reports_exact_gaps() {
        let dto = ApiKeyResponseDto {
            id: Uuid::nil(),
            name: "sync-key".to_owned(),
            permissions: vec!["asset.upload".to_owned()],
        };
        let missing = dto.missing_permissions(&REQUIRED_PERMISSIONS);
        assert_eq!(
            missing,
            vec![PERMISSION_ALBUM_READ, PERMISSION_ALBUM_ASSET_CREATE]
        );
    }

    #[test]
    fn missing_permissions_empty_when_all_present() {
        let dto = ApiKeyResponseDto {
            id: Uuid::nil(),
            name: "sync-key".to_owned(),
            permissions: REQUIRED_PERMISSIONS
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        assert!(dto.missing_permissions(&REQUIRED_PERMISSIONS).is_empty());
    }

    #[test]
    fn missing_permissions_all_wildcard_satisfies_everything() {
        let dto = ApiKeyResponseDto {
            id: Uuid::nil(),
            name: "admin-key".to_owned(),
            permissions: vec![PERMISSION_ALL.to_owned()],
        };
        assert!(dto.missing_permissions(&REQUIRED_PERMISSIONS).is_empty());
    }

    #[test]
    fn missing_permissions_is_exact_membership_not_prefix_matching() {
        // "asset.read" must NOT satisfy "asset.upload" — no prefix globbing (PLAN.md §2).
        let dto = ApiKeyResponseDto {
            id: Uuid::nil(),
            name: "read-only-key".to_owned(),
            permissions: vec!["asset.read".to_owned()],
        };
        let missing = dto.missing_permissions(&[PERMISSION_ASSET_UPLOAD]);
        assert_eq!(missing, vec![PERMISSION_ASSET_UPLOAD]);
    }

    // ---- ImmichErrorBody ------------------------------------------------------------------

    #[test]
    fn error_body_string_message() {
        let dto: ImmichErrorBody = serde_json::from_str(
            r#"{"statusCode":404,"message":"Album not found","error":"Not Found"}"#,
        )
        .unwrap();
        assert_eq!(dto.message.as_deref(), Some("Album not found"));
        assert_eq!(dto.error.as_deref(), Some("Not Found"));
        assert_eq!(dto.status_code, Some(404));
    }

    #[test]
    fn error_body_array_message_is_joined() {
        let dto: ImmichErrorBody = serde_json::from_str(
            r#"{"statusCode":400,"message":["field a is required","field b is invalid"],"error":"Bad Request"}"#,
        )
        .unwrap();
        assert_eq!(
            dto.message.as_deref(),
            Some("field a is required; field b is invalid")
        );
    }

    #[test]
    fn error_body_missing_message_is_none() {
        let dto: ImmichErrorBody = serde_json::from_str(r#"{"statusCode":500}"#).unwrap();
        assert_eq!(dto.message, None);
        assert_eq!(dto.error, None);
    }
}
