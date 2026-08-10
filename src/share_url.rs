//! Parsing for `EXPORT_ALBUM_URL`, the export-side share link.
//!
//! Per `PLAN.md` §2 and §5 step 3, the share URL has the shape `<origin><base>/share/<key>`
//! or `<origin><base>/s/<slug>`; the API root for every other export-side request is
//! `<origin><base>/api`, and the key/slug rides along as a `?key=…`/`?slug=…` query
//! parameter on every one of those requests.

use anyhow::{Context, Result, anyhow};
use url::Url;

/// How a request authenticates against the export instance: the opaque `key` from a
/// `/share/<key>` link, or the human-readable `slug` from a `/s/<slug>` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareRef {
    /// `<origin><base>/share/<key>` — the long opaque key form.
    Key(String),
    /// `<origin><base>/s/<slug>` — the short human-readable slug form.
    Slug(String),
}

impl ShareRef {
    /// Returns `url` with this share reference's query parameter (`key=…` or `slug=…`)
    /// appended, preserving whatever query parameters `url` already has. Every export-side
    /// request needs this riding along (`PLAN.md` §2).
    pub fn apply(&self, mut url: Url) -> Url {
        let (name, value) = match self {
            ShareRef::Key(key) => ("key", key.as_str()),
            ShareRef::Slug(slug) => ("slug", slug.as_str()),
        };
        url.query_pairs_mut().append_pair(name, value);
        url
    }
}

/// Parses `EXPORT_ALBUM_URL` into the export API base URL and the share reference to
/// authenticate with.
///
/// Accepts exactly two shapes — anything else is rejected with an error naming both:
/// * `<origin><base>/share/<key>` — API root is `<origin><base>/api`
/// * `<origin><base>/s/<slug>`    — API root is `<origin><base>/api`
///
/// Sub-path deployments (any number of leading path segments before the `share`/`s`
/// marker), trailing slashes, `http` vs `https`, and non-default ports are all handled,
/// since only the *last two* path segments are inspected. A query string or fragment on the
/// input is accepted and ignored — it carries no meaning for a share link.
pub fn parse_share_url(raw: &str) -> Result<(Url, ShareRef)> {
    // Every rejection path — an unparseable URL as much as a wrong marker — surfaces the
    // same "expected .../share/<key> or .../s/<slug>" message; the underlying parse error
    // (if any) is preserved as the anyhow source chain rather than the top-level Display.
    let url = Url::parse(raw.trim()).with_context(|| invalid_share_url_message(raw))?;

    // Owned, rather than borrowed from `url`, so `url` is free to be moved below once we're
    // done inspecting its path.
    let segments: Vec<String> = url
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();

    let (value, rest) = segments
        .split_last()
        .ok_or_else(|| invalid_share_url(raw))?;
    let (marker, base_segments) = rest.split_last().ok_or_else(|| invalid_share_url(raw))?;

    let share_ref = match marker.as_str() {
        "share" => ShareRef::Key(value.clone()),
        "s" => ShareRef::Slug(value.clone()),
        _ => return Err(invalid_share_url(raw)),
    };

    let mut api_base = url;
    api_base.set_query(None);
    api_base.set_fragment(None);
    let mut api_path_segments: Vec<&str> = base_segments.iter().map(String::as_str).collect();
    api_path_segments.push("api");
    api_base.set_path(&format!("/{}", api_path_segments.join("/")));

    Ok((api_base, share_ref))
}

fn invalid_share_url_message(raw: &str) -> String {
    format!(
        "EXPORT_ALBUM_URL '{raw}' is not a recognised share link; expected \
         '<origin><base>/share/<key>' or '<origin><base>/s/<slug>'"
    )
}

fn invalid_share_url(raw: &str) -> anyhow::Error {
    anyhow!(invalid_share_url_message(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_form() {
        let (base, share_ref) =
            parse_share_url("https://photos.friend.example/share/AbC123").unwrap();
        assert_eq!(base.as_str(), "https://photos.friend.example/api");
        assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
    }

    #[test]
    fn slug_form() {
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
    fn sub_path_deployment_slug() {
        let (base, share_ref) = parse_share_url("https://host/immich/s/holiday-2026").unwrap();
        assert_eq!(base.as_str(), "https://host/immich/api");
        assert_eq!(share_ref, ShareRef::Slug("holiday-2026".to_owned()));
    }

    #[test]
    fn multi_segment_sub_path() {
        let (base, _) = parse_share_url("https://host/a/b/c/share/AbC123").unwrap();
        assert_eq!(base.as_str(), "https://host/a/b/c/api");
    }

    #[test]
    fn trailing_slash() {
        let (base, share_ref) = parse_share_url("https://host/share/AbC123/").unwrap();
        assert_eq!(base.as_str(), "https://host/api");
        assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
    }

    #[test]
    fn http_scheme() {
        let (base, _) = parse_share_url("http://host/share/AbC123").unwrap();
        assert_eq!(base.as_str(), "http://host/api");
    }

    #[test]
    fn non_default_port() {
        let (base, _) = parse_share_url("https://host:8443/share/AbC123").unwrap();
        assert_eq!(base.as_str(), "https://host:8443/api");
    }

    #[test]
    fn query_string_is_ignored() {
        let (base, share_ref) =
            parse_share_url("https://host/share/AbC123?foo=bar&baz=qux").unwrap();
        assert_eq!(base.as_str(), "https://host/api");
        assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
    }

    #[test]
    fn fragment_is_ignored() {
        let (base, share_ref) = parse_share_url("https://host/share/AbC123#section").unwrap();
        assert_eq!(base.as_str(), "https://host/api");
        assert_eq!(share_ref, ShareRef::Key("AbC123".to_owned()));
    }

    #[test]
    fn rejects_wrong_marker() {
        assert!(parse_share_url("https://host/album/AbC123").is_err());
    }

    #[test]
    fn rejects_bare_origin() {
        assert!(parse_share_url("https://host").is_err());
    }

    #[test]
    fn rejects_bare_origin_with_trailing_slash() {
        assert!(parse_share_url("https://host/").is_err());
    }

    #[test]
    fn rejects_single_segment_path() {
        // "share" with nothing after it: no key/slug value to extract.
        assert!(parse_share_url("https://host/share").is_err());
    }

    #[test]
    fn rejects_share_with_missing_key() {
        assert!(parse_share_url("https://host/share/").is_err());
    }

    #[test]
    fn rejects_garbage_url() {
        assert!(parse_share_url("not a url at all").is_err());
    }

    #[test]
    fn apply_appends_key_query_param() {
        let base = Url::parse("https://host/api/search/metadata").unwrap();
        let full = ShareRef::Key("AbC123".to_owned()).apply(base);
        assert_eq!(full.as_str(), "https://host/api/search/metadata?key=AbC123");
    }

    #[test]
    fn apply_appends_slug_query_param() {
        let base = Url::parse("https://host/api/shared-links/me").unwrap();
        let full = ShareRef::Slug("holiday-2026".to_owned()).apply(base);
        assert_eq!(
            full.as_str(),
            "https://host/api/shared-links/me?slug=holiday-2026"
        );
    }

    #[test]
    fn apply_preserves_existing_query_params() {
        let base = Url::parse("https://host/api/search/metadata?page=2").unwrap();
        let full = ShareRef::Key("AbC123".to_owned()).apply(base);
        assert_eq!(
            full.as_str(),
            "https://host/api/search/metadata?page=2&key=AbC123"
        );
    }
}
