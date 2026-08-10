//! Integration test for `EXPORT_ALBUM_URL` parsing (`PLAN.md` §11 bullet 1), driven through
//! the public `immich_federation_at_home::share_url` API rather than internals — this is
//! what makes it an integration test instead of the unit tests already living next to the
//! implementation in `src/share_url.rs`.

use immich_federation_at_home::share_url::{ShareRef, parse_share_url};
use url::Url;

#[test]
fn share_key_form() {
    let (base, share_ref) = parse_share_url("https://photos.friend.example/share/AbC123").unwrap();
    assert_eq!(base.as_str(), "https://photos.friend.example/api");
    assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
}

#[test]
fn share_slug_form() {
    let (base, share_ref) =
        parse_share_url("https://photos.friend.example/s/holiday-2026").unwrap();
    assert_eq!(base.as_str(), "https://photos.friend.example/api");
    assert_eq!(share_ref, ShareRef::Slug("holiday-2026".to_owned()));
}

#[test]
fn sub_path_deployment() {
    let (base, share_ref) = parse_share_url("https://host/immich/share/AbC123").unwrap();
    assert_eq!(base.as_str(), "https://host/immich/api");
    assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
}

#[test]
fn sub_path_deployment_slug_form() {
    let (base, share_ref) = parse_share_url("https://host/immich/s/holiday-2026").unwrap();
    assert_eq!(base.as_str(), "https://host/immich/api");
    assert_eq!(share_ref, ShareRef::Slug("holiday-2026".to_owned()));
}

#[test]
fn trailing_slash_on_key() {
    let (base, share_ref) = parse_share_url("https://host/share/AbC123/").unwrap();
    assert_eq!(base.as_str(), "https://host/api");
    assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
}

#[test]
fn trailing_slash_on_sub_path_slug() {
    let (base, share_ref) = parse_share_url("https://host/immich/s/holiday-2026/").unwrap();
    assert_eq!(base.as_str(), "https://host/immich/api");
    assert_eq!(share_ref, ShareRef::Slug("holiday-2026".to_owned()));
}

#[test]
fn http_scheme() {
    let (base, share_ref) = parse_share_url("http://host/share/AbC123").unwrap();
    assert_eq!(base.as_str(), "http://host/api");
    assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
}

#[test]
fn https_scheme() {
    let (base, _) = parse_share_url("https://host/share/AbC123").unwrap();
    assert_eq!(base.scheme(), "https");
}

#[test]
fn non_default_port_is_preserved() {
    let (base, _) = parse_share_url("https://host:8443/share/AbC123").unwrap();
    assert_eq!(base.as_str(), "https://host:8443/api");
    assert_eq!(base.port(), Some(8443));
}

#[test]
fn query_string_on_input_is_ignored() {
    let (base, share_ref) =
        parse_share_url("https://host/share/AbC123?utm_source=friend&foo=bar").unwrap();
    assert_eq!(base.as_str(), "https://host/api");
    assert_eq!(base.query(), None);
    assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
}

#[test]
fn fragment_on_input_is_ignored() {
    let (base, share_ref) = parse_share_url("https://host/share/AbC123#some-section").unwrap();
    assert_eq!(base.as_str(), "https://host/api");
    assert_eq!(base.fragment(), None);
    assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
}

#[test]
fn query_and_fragment_together_are_ignored() {
    let (base, share_ref) = parse_share_url("https://host/s/holiday-2026?x=1#y").unwrap();
    assert_eq!(base.as_str(), "https://host/api");
    assert_eq!(share_ref, ShareRef::Slug("holiday-2026".to_owned()));
}

// ---- garbage rejection ------------------------------------------------------------------

#[test]
fn rejects_completely_invalid_url() {
    assert!(parse_share_url("this is not a url").is_err());
}

#[test]
fn rejects_bare_origin() {
    assert!(parse_share_url("https://host").is_err());
}

#[test]
fn rejects_wrong_path_marker() {
    assert!(parse_share_url("https://host/albums/AbC123").is_err());
}

#[test]
fn rejects_share_marker_with_no_key() {
    assert!(parse_share_url("https://host/share").is_err());
}

#[test]
fn rejects_share_marker_with_empty_key() {
    assert!(parse_share_url("https://host/share/").is_err());
}

#[test]
fn rejects_s_marker_with_no_slug() {
    assert!(parse_share_url("https://host/s").is_err());
}

#[test]
fn rejects_ftp_style_garbage() {
    assert!(parse_share_url("not-even-a-scheme://nope").is_err());
}

// ---- the key=/slug= query-parameter helper ---------------------------------------------

#[test]
fn share_ref_apply_rides_along_on_a_request_url() {
    let (base, share_ref) = parse_share_url("https://photos.friend.example/share/AbC123").unwrap();
    // `base` (".../api", per PLAN.md §2's examples) deliberately has no trailing slash, so
    // plain string formatting — not `Url::join` — is the safe way to append a fixed endpoint
    // path: `Url::join`'s RFC 3986 "merge" replaces everything after the base's last `/`,
    // which would silently drop the `api` segment here (`base.join("search/metadata")`
    // yields `.../search/metadata`, not `.../api/search/metadata`). See NOTES.md.
    let request_url = Url::parse(&format!("{base}/search/metadata")).unwrap();
    let final_url = share_ref.apply(request_url);
    assert_eq!(
        final_url.as_str(),
        "https://photos.friend.example/api/search/metadata?key=AbC123"
    );
}

#[test]
fn joining_without_a_trailing_slash_on_the_base_drops_the_api_segment() {
    // Documents the `Url::join` gotcha from the test above as its own executable example,
    // so it can't silently bit-rot into a stale comment.
    let (base, _) = parse_share_url("https://photos.friend.example/share/AbC123").unwrap();
    assert_eq!(base.as_str(), "https://photos.friend.example/api");
    let joined = base.join("search/metadata").unwrap();
    assert_eq!(
        joined.as_str(),
        "https://photos.friend.example/search/metadata",
        "Url::join on a trailing-slash-less base does not do what it looks like it does"
    );
}

#[test]
fn share_ref_apply_uses_slug_param_name_for_slug_links() {
    let share_ref = ShareRef::Slug("holiday-2026".to_owned());
    let url = Url::parse("https://host/api/shared-links/me").unwrap();
    let final_url = share_ref.apply(url);
    assert_eq!(
        final_url.as_str(),
        "https://host/api/shared-links/me?slug=holiday-2026"
    );
}
