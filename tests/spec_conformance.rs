//! `PLAN.md` §11: loads the vendored `openapi/immich-openapi.json` at *runtime* (via
//! `std::fs`, resolved from `CARGO_MANIFEST_DIR` — the spec is a dev-time reference, not a
//! build input, per `PLAN.md` §3/§9) and asserts that:
//!
//! * every field `src/immich/dto.rs` deserializes as non-`Option` is in fact `required` in
//!   the spec's matching schema;
//! * every enum literal this crate's DTOs accept matches the spec's own `enum` array, in
//!   both directions (every spec literal deserializes to a *named* variant of ours, not the
//!   `Unrecognized` catch-all; every literal our types accept is one the spec actually
//!   defines);
//! * the three permission strings in `dto::REQUIRED_PERMISSIONS`, plus the `all` wildcard and
//!   `dto::TAG_PERMISSIONS`, are real members of the spec's `Permission` enum.
//!
//! Every JSON path below was confirmed with `jq` against the vendored spec before being
//! written here (see `NOTES.md`'s "Task 12" section for the exact queries), per this task's
//! brief. Where `src/immich/dto.rs` deliberately diverges from what the spec's `required`
//! array alone would suggest — a field that's spec-required-but-nullable, modelled as
//! `Option` here for that reason rather than because it can be *absent* — that divergence is
//! called out as an explicit, commented case below (already documented in `NOTES.md`, task
//! 3/4), never silently skipped.

use std::collections::HashSet;
use std::sync::LazyLock;

use immich_federation_at_home::immich::dto;

static SPEC: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi/immich-openapi.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read vendored spec at {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("vendored spec must be valid JSON")
});

/// The `required` array of `components.schemas.<name>`, as a set — empty if the schema has
/// no `required` array at all (several of the spec's request DTOs, e.g. `MetadataSearchDto`,
/// have none: they're "a bag of optional filters", per `NOTES.md`).
fn required_fields(schema: &str) -> HashSet<String> {
    SPEC["components"]["schemas"][schema]["required"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().expect("required entries are strings").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The `enum` array of `components.schemas.<name>`, as a set.
fn spec_enum_values(schema: &str) -> HashSet<String> {
    SPEC["components"]["schemas"][schema]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{schema} has no `enum` array in the vendored spec"))
        .iter()
        .map(|v| v.as_str().expect("enum entries are strings").to_owned())
        .collect()
}

/// Every field named here must be spec-`required` for `schema`. This is a subset check on
/// purpose: fields our DTO models as `Option` (because the spec says they're optional, or
/// because they're required-but-nullable, see the divergence notes below) are simply not
/// listed, and fields the spec requires that we don't model at all (e.g. `AssetResponseDto`'s
/// `width`/`height`/`ownerId`/…) are none of this test's business.
fn assert_required(schema: &str, our_non_option_fields: &[&str]) {
    let required = required_fields(schema);
    for field in our_non_option_fields {
        assert!(
            required.contains(*field),
            "dto.rs deserializes {schema}.{field} as non-Option, but the vendored spec's \
             `required` array for {schema} is {required:?} — either the spec dropped this \
             field's required-ness (make it Option in dto.rs) or this test's field list drifted"
        );
    }
}

// -----------------------------------------------------------------------------------------
// Required-field conformance, one function per schema `dto.rs` maps to.
// -----------------------------------------------------------------------------------------

#[test]
fn server_version_response_dto_required_fields() {
    // major/minor/patch: non-Option in dto::ServerVersionResponseDto. prerelease is
    // Option<u64> — spec-required-but-nullable (an integer or null, key always present),
    // not because it can be missing; not asserted here as "required" for that reason.
    assert_required("ServerVersionResponseDto", &["major", "minor", "patch"]);
}

#[test]
fn shared_link_response_dto_required_fields() {
    // dto::SharedLinkResponseDto's non-Option fields.
    assert_required(
        "SharedLinkResponseDto",
        &["id", "type", "allowDownload", "allowUpload", "showMetadata"],
    );
    // Deliberate divergence #1: `album` is Option<AlbumResponseDto> in dto.rs even though
    // an ALBUM-type link always has one in practice — the spec's own `required` array does
    // NOT list `album` (an INDIVIDUAL-type link has none), so Option is the spec-accurate
    // choice, not a divergence from it. Documented here anyway since PLAN.md §5 step 6's
    // prose reads as if it's always present.
    assert!(!required_fields("SharedLinkResponseDto").contains("album"));
    // Deliberate divergence #2: `expiresAt` IS in the spec's `required` array, but dto.rs
    // models it as `Option<DateTime<Utc>>` because the spec also marks it `nullable: true`
    // (an ISO-8601 string or `null`, key always present) — Option captures exactly that,
    // not "sometimes absent". Confirmed nullable via `jq
    // '.components.schemas.SharedLinkResponseDto.properties.expiresAt'`.
    assert!(required_fields("SharedLinkResponseDto").contains("expiresAt"));
    assert_eq!(
        SPEC["components"]["schemas"]["SharedLinkResponseDto"]["properties"]["expiresAt"]["nullable"],
        true,
        "expiresAt's required-but-Option modelling in dto.rs relies on it staying nullable"
    );
}

#[test]
fn shared_link_login_dto_required_fields() {
    assert_required("SharedLinkLoginDto", &["password"]);
}

#[test]
fn album_response_dto_required_fields() {
    assert_required("AlbumResponseDto", &["id", "albumName", "assetCount"]);
}

#[test]
fn asset_response_dto_required_fields() {
    // dto::AssetResponseDto's non-Option fields.
    assert_required(
        "AssetResponseDto",
        &[
            "id",
            "checksum",
            "originalFileName",
            "type",
            "fileCreatedAt",
            "fileModifiedAt",
        ],
    );
    // Deliberate divergence #1: `originalMimeType` is Option<String> in dto.rs, and — unlike
    // the other fields above — this one is NOT because of nullability: the spec's own
    // `required` array simply does not list `originalMimeType` at all (confirmed by its
    // absence below), which is exactly what dto.rs's doc comment says, correcting PLAN.md's
    // task-4 bullet list, which implied it was always present.
    assert!(!required_fields("AssetResponseDto").contains("originalMimeType"));
    // Deliberate divergence #2: `duration` IS spec-required, but dto.rs models it as
    // `Option<i64>` because it's also `nullable: true` (integer-or-null, key always
    // present, "null for static images") — the same required-but-nullable shape as
    // `expiresAt` above.
    assert!(required_fields("AssetResponseDto").contains("duration"));
    assert_eq!(
        SPEC["components"]["schemas"]["AssetResponseDto"]["properties"]["duration"]["nullable"],
        true,
        "duration's required-but-Option modelling in dto.rs relies on it staying nullable"
    );
    // `checksum` is spec-required AND non-nullable (a plain string) — stronger than
    // PLAN.md's framing that "checksum being absent is a real failure mode"; confirmed no
    // `nullable` key at all on this property.
    assert!(
        SPEC["components"]["schemas"]["AssetResponseDto"]["properties"]["checksum"]["nullable"]
            .is_null(),
        "checksum was expected to be non-nullable per NOTES.md; the spec now marks it nullable"
    );
}

#[test]
fn search_response_dto_required_fields() {
    // dto::SearchResponseDto only models `assets`, non-Option. `albums` is not modelled at
    // all (nothing downstream needs it), so it's simply absent from this check.
    assert_required("SearchResponseDto", &["assets"]);
}

#[test]
fn search_asset_response_dto_required_fields() {
    // dto::SearchAssetResponseDto's non-Option fields: items, total, count.
    assert_required("SearchAssetResponseDto", &["items", "total", "count"]);
    // Deliberate divergence: `nextPage` IS spec-required, but dto.rs models it as
    // `Option<String>` for the same required-but-nullable reason as `duration`/`expiresAt`
    // above (an opaque continuation token, `null` on the last page).
    assert!(required_fields("SearchAssetResponseDto").contains("nextPage"));
    assert_eq!(
        SPEC["components"]["schemas"]["SearchAssetResponseDto"]["properties"]["nextPage"]["nullable"],
        true,
        "nextPage's required-but-Option modelling in dto.rs relies on it staying nullable"
    );
    // Also pin down NOTES.md's finding that nextPage is a *string* token, not a number —
    // a silent spec type change here would break the "page until None" pagination loop in
    // export.rs in a much uglier way than a failed required-field check.
    assert_eq!(
        SPEC["components"]["schemas"]["SearchAssetResponseDto"]["properties"]["nextPage"]["type"],
        "string"
    );
}

#[test]
fn asset_bulk_upload_check_item_required_fields() {
    // Both fields of dto::AssetBulkUploadCheckItem (our own request DTO) are non-Option.
    assert_required("AssetBulkUploadCheckItem", &["id", "checksum"]);
    // Deliberate divergence: the spec types `id` as a plain `string` (its own description:
    // "client-side identifier echoed in the response"), not `format: uuid` — dto.rs
    // therefore models it as `String`, not `Uuid`, even though every caller happens to put
    // an export-asset UUID's string form in it.
    assert_eq!(
        SPEC["components"]["schemas"]["AssetBulkUploadCheckItem"]["properties"]["id"]["format"],
        serde_json::Value::Null,
        "AssetBulkUploadCheckItem.id was expected to have no `format: uuid` constraint"
    );
}

#[test]
fn asset_bulk_upload_check_dto_required_fields() {
    assert_required("AssetBulkUploadCheckDto", &["assets"]);
}

#[test]
fn asset_bulk_upload_check_result_required_fields() {
    // dto::AssetBulkUploadCheckResult's non-Option fields: id, action. assetId/isTrashed/
    // reason are all Option — and per the spec they're all genuinely optional (not just
    // nullable), confirmed below.
    assert_required("AssetBulkUploadCheckResult", &["id", "action"]);
    let required = required_fields("AssetBulkUploadCheckResult");
    for optional_field in ["assetId", "isTrashed", "reason"] {
        assert!(
            !required.contains(optional_field),
            "{optional_field} was expected to stay optional in the spec"
        );
    }
    // assetId *is* format:uuid, unlike the request-side id above (dto.rs models it
    // Option<Uuid> for exactly this reason).
    assert_eq!(
        SPEC["components"]["schemas"]["AssetBulkUploadCheckResult"]["properties"]["assetId"]["format"],
        "uuid"
    );
}

#[test]
fn asset_bulk_upload_check_response_dto_required_fields() {
    assert_required("AssetBulkUploadCheckResponseDto", &["results"]);
}

#[test]
fn asset_media_response_dto_required_fields() {
    assert_required("AssetMediaResponseDto", &["id", "status"]);
}

#[test]
fn bulk_ids_dto_required_fields() {
    assert_required("BulkIdsDto", &["ids"]);
}

#[test]
fn bulk_id_response_dto_required_fields() {
    // id/success non-Option; `error` is Option and genuinely optional per the spec.
    assert_required("BulkIdResponseDto", &["id", "success"]);
    assert!(!required_fields("BulkIdResponseDto").contains("error"));
}

#[test]
fn api_key_response_dto_required_fields() {
    // dto::ApiKeyResponseDto only models id/name/permissions, all non-Option. createdAt/
    // updatedAt are spec-required too but simply not modelled here (nothing downstream
    // needs them).
    assert_required("ApiKeyResponseDto", &["id", "name", "permissions"]);
}

#[test]
fn tag_upsert_dto_required_fields() {
    assert_required("TagUpsertDto", &["tags"]);
}

#[test]
fn tag_response_dto_required_fields() {
    // dto::TagResponseDto only models id/value, both non-Option; name/createdAt/updatedAt
    // are spec-required too but simply not modelled here (nothing downstream needs them).
    assert_required("TagResponseDto", &["id", "value"]);
}

#[test]
fn tag_bulk_assets_dto_required_fields() {
    assert_required("TagBulkAssetsDto", &["tagIds", "assetIds"]);
}

#[test]
fn tag_bulk_assets_response_dto_required_fields() {
    assert_required("TagBulkAssetsResponseDto", &["count"]);
}

// -----------------------------------------------------------------------------------------
// Enum literal conformance — every wire string dto.rs's enums accept, checked against the
// spec's `enum` array in both directions: every spec literal must deserialize to a *named*
// variant (not the `Unrecognized` catch-all), and the set of literals we claim to know about
// must equal the spec's set exactly (catches us silently drifting to accept/reject a literal
// the spec doesn't/does define).
// -----------------------------------------------------------------------------------------

/// Deserializes `wire` into `T` and asserts the result equals `expected` — i.e. `wire` maps
/// to a *specific named* variant, not e.g. an `Unrecognized`/`Other` catch-all that would
/// also happily "succeed" here without actually proving the literal spelling is right.
fn assert_wire_maps_to<T>(wire: &str, expected: &T)
where
    T: serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = format!("{wire:?}");
    let parsed: T = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!(
            "{json} failed to deserialize as {}: {e}",
            std::any::type_name::<T>()
        )
    });
    assert_eq!(
        &parsed, expected,
        "wire literal {wire:?} did not map to the expected variant"
    );
}

#[test]
fn shared_link_type_matches_the_spec_enum() {
    let ours: HashSet<&str> = ["ALBUM", "INDIVIDUAL"].into_iter().collect();
    assert_eq!(
        ours,
        spec_enum_values("SharedLinkType")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_wire_maps_to("ALBUM", &dto::SharedLinkType::Album);
    assert_wire_maps_to("INDIVIDUAL", &dto::SharedLinkType::Individual);
}

#[test]
fn asset_order_matches_the_spec_enum() {
    // Serialize-only (request-only enum, never appears in a response we parse) — checked by
    // serialization instead of deserialization.
    let ours: HashSet<&str> = ["asc", "desc"].into_iter().collect();
    assert_eq!(
        ours,
        spec_enum_values("AssetOrder")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_eq!(serde_json::to_value(dto::AssetOrder::Asc).unwrap(), "asc");
    assert_eq!(serde_json::to_value(dto::AssetOrder::Desc).unwrap(), "desc");
}

#[test]
fn asset_type_enum_matches_the_spec_enum() {
    let ours: HashSet<&str> = ["IMAGE", "VIDEO", "AUDIO", "OTHER"].into_iter().collect();
    assert_eq!(
        ours,
        spec_enum_values("AssetTypeEnum")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_wire_maps_to("IMAGE", &dto::AssetTypeEnum::Image);
    assert_wire_maps_to("VIDEO", &dto::AssetTypeEnum::Video);
    assert_wire_maps_to("AUDIO", &dto::AssetTypeEnum::Audio);
    assert_wire_maps_to("OTHER", &dto::AssetTypeEnum::Other);
}

#[test]
fn asset_upload_action_matches_the_spec_enum() {
    let ours: HashSet<&str> = ["accept", "reject"].into_iter().collect();
    assert_eq!(
        ours,
        spec_enum_values("AssetUploadAction")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_wire_maps_to("accept", &dto::AssetUploadAction::Accept);
    assert_wire_maps_to("reject", &dto::AssetUploadAction::Reject);
}

#[test]
fn asset_reject_reason_matches_the_spec_enum() {
    let ours: HashSet<&str> = ["duplicate", "unsupported-format"].into_iter().collect();
    assert_eq!(
        ours,
        spec_enum_values("AssetRejectReason")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_wire_maps_to("duplicate", &dto::AssetRejectReason::Duplicate);
    assert_wire_maps_to(
        "unsupported-format",
        &dto::AssetRejectReason::UnsupportedFormat,
    );
}

#[test]
fn asset_media_status_matches_the_spec_enum() {
    // NOTES.md: this spec version has only `created`/`duplicate` — no `replaced`, contrary
    // to earlier speculative research notes.
    let ours: HashSet<&str> = ["created", "duplicate"].into_iter().collect();
    assert_eq!(
        ours,
        spec_enum_values("AssetMediaStatus")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_wire_maps_to("created", &dto::AssetMediaStatus::Created);
    assert_wire_maps_to("duplicate", &dto::AssetMediaStatus::Duplicate);
}

#[test]
fn bulk_id_error_reason_matches_the_spec_enum() {
    let ours: HashSet<&str> = [
        "duplicate",
        "no_permission",
        "not_found",
        "unknown",
        "validation",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        ours,
        spec_enum_values("BulkIdErrorReason")
            .iter()
            .map(String::as_str)
            .collect()
    );
    assert_wire_maps_to("duplicate", &dto::BulkIdErrorReason::Duplicate);
    assert_wire_maps_to("no_permission", &dto::BulkIdErrorReason::NoPermission);
    assert_wire_maps_to("not_found", &dto::BulkIdErrorReason::NotFound);
    assert_wire_maps_to("unknown", &dto::BulkIdErrorReason::Unknown);
    assert_wire_maps_to("validation", &dto::BulkIdErrorReason::Validation);
}

// -----------------------------------------------------------------------------------------
// API-key permission literals (`dto::REQUIRED_PERMISSIONS`, `dto::PERMISSION_ALL`) — a
// subset check against the spec's `Permission` enum (155 literals at last count and growing;
// `dto::ApiKeyResponseDto.permissions` is deliberately `Vec<String>`, not a closed enum, for
// exactly that reason — see `NOTES.md`).
// -----------------------------------------------------------------------------------------

#[test]
fn required_permissions_are_real_spec_permissions() {
    let spec_permissions = spec_enum_values("Permission");
    for permission in dto::REQUIRED_PERMISSIONS {
        assert!(
            spec_permissions.contains(permission),
            "{permission:?} is not a member of the spec's Permission enum any more"
        );
    }
    assert!(
        spec_permissions.contains(dto::PERMISSION_ALL),
        "the \"all\" wildcard is not a member of the spec's Permission enum any more"
    );
}

#[test]
fn tag_permissions_are_real_spec_permissions() {
    let spec_permissions = spec_enum_values("Permission");
    for permission in dto::TAG_PERMISSIONS {
        assert!(
            spec_permissions.contains(permission),
            "{permission:?} is not a member of the spec's Permission enum any more"
        );
    }
}
