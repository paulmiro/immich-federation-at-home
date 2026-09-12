//! Shared HTTP plumbing used by both the export client (share-link auth, a later step)
//! and the import client (API-key auth, a later step): client construction, [`ApiError`],
//! the status→result helper, request instrumentation + retry wiring, and the [`Version`]
//! newtype used by `PLAN.md` §5's startup version gates.
//!
//! Everything here is generic over "a JSON HTTP call" — [`send_json`] is what both future
//! clients will call for every endpoint except the two that stream large bodies (the
//! original-file download and the multipart upload), which will instead compose
//! [`execute_once`] directly so the body never has to be buffered in memory.

pub mod dto;
pub mod export;
pub mod import;

use std::fmt;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;
use url::Url;

use crate::retry::{self, RetryPolicy, Retryable};
use crate::{debug, trace};

/// This tool's `User-Agent`, e.g. `immich-federation-at-home/0.2.0`.
pub const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Builds a `reqwest::Client` with `timeout` applied to every request it makes and this
/// tool's name/version as the `User-Agent`. `timeout` is a parameter (rather than a single
/// hard-coded constant) because the export and import clients need different values —
/// `REQUEST_TIMEOUT` for metadata calls, `TRANSFER_TIMEOUT` for the download+upload of one
/// asset (`PLAN.md` §4) — and different `Client`s entirely for that reason: both
/// `export::ExportClient` and `import::ImportClient` build one of each.
///
/// `cookie_store` should be `true` only for the export client: the shared-link password
/// login (`PLAN.md` §5 step 5, E2) sets an `immich_shared_link_token` cookie that every
/// subsequent export-side request must carry. The import client (API-key auth) never
/// needs one. TLS is always `rustls` — the only backend compiled in, see `Cargo.toml`.
///
/// `default_headers` is applied to every request the returned client ever makes. This is
/// what lets `import::ImportClient` bake its `x-api-key` header in once at construction time
/// (`PLAN.md` §2: "the API key goes in an `x-api-key` header") rather than every call site
/// having to remember to attach it — pass `HeaderMap::new()` for a client that needs none
/// (the export client authenticates via `?key=`/`?slug=` query parameters instead, see
/// `share_url::ShareRef::apply`).
pub fn build_client(
    timeout: Duration,
    cookie_store: bool,
    default_headers: HeaderMap,
) -> reqwest::Result<Client> {
    Client::builder()
        .timeout(timeout)
        .user_agent(USER_AGENT)
        .cookie_store(cookie_store)
        .default_headers(default_headers)
        .build()
}

/// `Duration::as_millis()` returns `u128`; every duration this module logs (an HTTP call's
/// elapsed time) fits comfortably in a `u64` millisecond count, but clippy's pedantic
/// `cast_possible_truncation` lint doesn't know that — `unwrap_or(u64::MAX)` is a
/// saturating fallback for durations that can't occur here in practice, not a panic.
fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------------------
// URL / body redaction — PLAN.md §8: never let a share key, slug, password, or API key
// reach a log line or an error message.
// ---------------------------------------------------------------------------------------

/// Query-parameter names whose *value* must never reach a log line or an error message.
/// `key`/`slug` are the share link's bearer credential and ride on essentially every
/// export-side request (`PLAN.md` §2); the rest are defensive, in case a future endpoint
/// carries a secret in the query string instead of a header.
const SENSITIVE_QUERY_PARAMS: &[&str] = &["key", "slug", "password", "token", "apikey", "api_key"];

/// Returns `url` with every [`SENSITIVE_QUERY_PARAMS`] value replaced by `"REDACTED"`,
/// preserving scheme/host/path and any other query parameters (so the result is still
/// useful for correlating log lines, e.g. `page=2` survives). Used everywhere a URL is
/// logged or embedded in an [`ApiError`] — see [`execute_once`] and [`parse_json_response`].
pub fn redact_url(url: &Url) -> String {
    if url.query().is_none() {
        return url.as_str().to_owned();
    }
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            if SENSITIVE_QUERY_PARAMS.contains(&k.to_ascii_lowercase().as_str()) {
                (k.into_owned(), "REDACTED".to_owned())
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();
    let mut redacted = url.clone();
    redacted.query_pairs_mut().clear();
    for (k, v) in &pairs {
        redacted.query_pairs_mut().append_pair(k, v);
    }
    redacted.as_str().to_owned()
}

/// JSON object keys whose value must never reach a `trace` log line. Mirrors
/// [`SENSITIVE_QUERY_PARAMS`], but for response *bodies* rather than URLs — notably
/// `SharedLinkResponseDto.key`, the share link's own credential, which the server happily
/// echoes back in `GET /shared-links/me`'s response body (we don't even model that field
/// in `dto::SharedLinkResponseDto` for this exact reason, but a `trace`-level raw-body dump
/// bypasses our own DTOs entirely, so it needs its own redaction pass).
const SENSITIVE_JSON_FIELDS: &[&str] = &["key", "slug", "password", "token", "apikey"];

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if !v.is_null() && SENSITIVE_JSON_FIELDS.contains(&k.to_ascii_lowercase().as_str())
                {
                    *v = serde_json::Value::String("REDACTED".to_owned());
                } else {
                    redact_json_value(v);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json_value),
        _ => {}
    }
}

/// Redacts sensitive fields from a JSON response body for `trace` logging. Parses into a
/// generic [`serde_json::Value`] (already-buffered bytes only — see [`parse_json_response`]'s
/// doc comment for why that's safe here and never done for the streaming original-file
/// download), walks it recursively, and re-serialises. Unparseable bytes (never expected
/// for a body we're about to also `serde_json::from_slice` as a typed DTO, but this
/// function is defensive on its own) become a placeholder rather than panicking.
fn redact_json_body(bytes: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(mut value) => {
            redact_json_value(&mut value);
            value.to_string()
        }
        Err(_) => "<unparseable body>".to_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// ApiError
// ---------------------------------------------------------------------------------------

/// Every error this crate's HTTP layer can produce. Carries the HTTP status (where there
/// is one) so [`retry`] can classify it and so callers can distinguish 401/403/404
/// (`PLAN.md` §5's startup checks all need to). `url` on every variant is already
/// [`redact_url`]-ed — never the raw request URL.
#[derive(Debug, Error)]
pub enum ApiError {
    /// The request never got a response at all: DNS/connect failure, timeout, TLS error,
    /// or (rarely) a request-builder error (e.g. an invalid header value).
    #[error("{method} {url} failed")]
    Transport {
        method: Method,
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// A non-2xx response, with Immich's `{message, error, statusCode}` body parsed when
    /// present (see [`dto::ImmichErrorBody`]) or a fallback built from the raw body.
    #[error("{method} {url} -> {status}: {message}")]
    Status {
        method: Method,
        url: String,
        status: StatusCode,
        message: String,
        error: Option<String>,
        /// Present only for a `429` whose `Retry-After` header parsed as a sane number of
        /// seconds — see [`parse_retry_after`].
        retry_after: Option<Duration>,
    },

    /// A 2xx response whose body didn't deserialize as the type the caller asked for — a
    /// real bug (our DTO vs. the server's actual shape), never worth retrying.
    #[error("{method} {url} -> {status}: failed to decode JSON response body")]
    Decode {
        method: Method,
        url: String,
        status: StatusCode,
        #[source]
        source: serde_json::Error,
    },
}

impl ApiError {
    /// The HTTP status this error carries, if any (a pure transport failure has none).
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            ApiError::Transport { source, .. } => source.status(),
            ApiError::Status { status, .. } | ApiError::Decode { status, .. } => Some(*status),
        }
    }
}

impl Retryable for ApiError {
    /// `PLAN.md` §7: retry on connection errors, timeouts, `429`, and `5xx`; never any
    /// other `4xx`. `is_connect()`/`is_timeout()` are deliberately narrower than
    /// `reqwest::Error::is_request()` (which also covers e.g. a client-builder error from
    /// a malformed header — retrying an identical malformed request can't ever succeed).
    /// A [`ApiError::Decode`] is never retryable — it means our DTO disagrees with the
    /// server's actual response shape, which no amount of retrying fixes.
    fn is_retryable(&self) -> bool {
        match self {
            ApiError::Transport { source, .. } => source.is_connect() || source.is_timeout(),
            ApiError::Status { status, .. } => status.as_u16() == 429 || status.is_server_error(),
            ApiError::Decode { .. } => false,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::Status {
                status,
                retry_after,
                ..
            } if status.as_u16() == 429 => *retry_after,
            _ => None,
        }
    }
}

/// Caps how long a `Retry-After` on a `429` is honoured for. `retry.rs`'s own schedule
/// tops out at 16s; 60s is generous headroom without letting a misconfigured (or hostile)
/// server stall an entire sync run.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Parses a `Retry-After` response header as a plain number of seconds — the form
/// `NestJS`'s throttler guard (and virtually every rate limiter) sends, as opposed to the
/// HTTP-date alternative `Retry-After` also legally allows. Anything else (absent header,
/// non-numeric value, `0`, or a value this parses fine but is nonsensically large) is
/// treated as "no override": `0` isn't a meaningful backoff and a giant value fails the
/// "sane" bar from `PLAN.md` §7, so both fall through to the caller's own schedule instead
/// of trusting the server blindly.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = raw.trim().parse().ok()?;
    if seconds == 0 {
        return None;
    }
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

/// An HTML error page from a misconfigured reverse proxy, say, shouldn't blow up a log
/// line.
const MAX_ERROR_BODY_CHARS: usize = 500;

fn parse_error_body(bytes: &[u8], status: StatusCode) -> (String, Option<String>) {
    if let Ok(body) = serde_json::from_slice::<dto::ImmichErrorBody>(bytes) {
        return (
            body.message.unwrap_or_else(|| status.to_string()),
            body.error,
        );
    }

    let raw = String::from_utf8_lossy(bytes);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (status.to_string(), None);
    }

    let truncated: String = trimmed.chars().take(MAX_ERROR_BODY_CHARS).collect();
    let message = if trimmed.chars().count() > MAX_ERROR_BODY_CHARS {
        format!("{truncated}…")
    } else {
        truncated
    };
    (message, None)
}

// ---------------------------------------------------------------------------------------
// Sending requests: instrumentation + retry
// ---------------------------------------------------------------------------------------

/// A single (non-retried) request attempt: sends `request` and logs at `debug` — method,
/// **redacted** URL, status (or the transport failure), and elapsed time — regardless of
/// outcome. This is where `PLAN.md` §8's "debug logs every request's method, redacted URL,
/// status and duration" lives, so both the export and import clients get it for free by
/// routing every call through here (directly, or via [`send_json`]).
///
/// `method`/`url` are supplied by the caller rather than extracted from `request` after
/// the fact: a `reqwest::RequestBuilder`'s internals aren't inspectable before `.send()`
/// consumes it, but the caller already has both in hand from building the request in the
/// first place (`client.get(url.clone())`, …) — no real burden, and it sidesteps that
/// limitation entirely. There is no separate `client: &Client` parameter — `RequestBuilder`
/// already carries its originating client internally, and `.send()` uses it.
///
/// Returns the raw [`Response`] on success, **not** buffered — 2xx or not, this layer
/// doesn't decide. That is deliberate: it is what lets a streaming caller (the
/// original-file download, a later step) consume the body as a stream without this
/// function ever pulling the bytes into memory. [`parse_json_response`] is the layer that
/// turns a non-2xx status into an [`ApiError`], for callers that do want a JSON body.
pub async fn execute_once(
    method: Method,
    url: &Url,
    request: RequestBuilder,
) -> Result<Response, ApiError> {
    let redacted = redact_url(url);
    let start = Instant::now();
    let result = request.send().await;
    let took_ms = millis_u64(start.elapsed());
    match result {
        Ok(response) => {
            debug!(
                "http request {method} {redacted} status={} took_ms={took_ms}",
                response.status()
            );
            Ok(response)
        }
        Err(source) => {
            debug!("http request failed {method} {redacted} took_ms={took_ms} error={source}");
            Err(ApiError::Transport {
                method,
                url: redacted,
                // `reqwest::Error`'s own `Display`/`Debug` embeds the *unredacted* URL
                // when one is attached (`" for url (...)"`), which would smuggle
                // `?key=`/`?slug=` straight through `{source}` in `ApiError`'s own
                // `#[error(...)]` message. `without_url()` is reqwest's own documented
                // tool for exactly this ("If the URL contains sensitive information …
                // be sure to remove it") — our own already-redacted `url` field carries
                // the URL for the message instead.
                source: source.without_url(),
            })
        }
    }
}

/// The "status → result" step: given a `Response` already received for `method`/`url`,
/// returns the parsed JSON body on 2xx, or an [`ApiError::Status`] otherwise (parsing
/// Immich's `{message, error, statusCode}` body when present, falling back to the raw
/// body). A JSON-decode failure on an otherwise-2xx response becomes [`ApiError::Decode`]
/// — never a panic, never silently swallowed.
///
/// Always buffers the body (`response.bytes()`) — every caller of this function wants a
/// typed JSON value out of it, and none of our JSON endpoints return anything remotely
/// close to the size of an original asset file, so this is the one place buffering the
/// full body is the right call. The original-file download (a later step) must and does
/// route around this function entirely, working directly off [`execute_once`]'s streaming
/// `Response`.
pub async fn parse_json_response<T: DeserializeOwned>(
    method: Method,
    url: &Url,
    response: Response,
) -> Result<T, ApiError> {
    let status = response.status();
    let redacted = redact_url(url);
    let retry_after = if status.as_u16() == 429 {
        parse_retry_after(response.headers())
    } else {
        None
    };
    let bytes = response
        .bytes()
        .await
        .map_err(|source| ApiError::Transport {
            method: method.clone(),
            url: redacted.clone(),
            source: source.without_url(),
        })?;

    trace!(
        "http response body {method} {redacted} status={status} body={}",
        redact_json_body(&bytes)
    );

    if status.is_success() {
        serde_json::from_slice(&bytes).map_err(|source| ApiError::Decode {
            method,
            url: redacted,
            status,
            source,
        })
    } else {
        let (message, error) = parse_error_body(&bytes, status);
        Err(ApiError::Status {
            method,
            url: redacted,
            status,
            message,
            error,
            retry_after,
        })
    }
}

/// Sends a JSON request with retries and instrumentation: the main entry point both
/// future clients use for every endpoint except the two that stream large bodies. `request`
/// is a *factory* (`FnMut() -> RequestBuilder`), called once per attempt — see
/// [`retry::retry`]'s doc comment for why a `RequestBuilder` can't simply be reused across
/// retries.
pub async fn send_json<T, F>(
    policy: &RetryPolicy,
    op_name: &str,
    method: Method,
    url: &Url,
    mut request: F,
) -> Result<T, ApiError>
where
    T: DeserializeOwned,
    F: FnMut() -> RequestBuilder,
{
    retry::retry(policy, op_name, || {
        let builder = request();
        let method = method.clone();
        async move {
            let response = execute_once(method.clone(), url, builder).await?;
            parse_json_response(method, url, response).await
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------------------

/// `major.minor.patch` from `dto::ServerVersionResponseDto`, deliberately dropping
/// `prerelease` — `PLAN.md` §5 steps 4 and 7 only ever compare against release cutoffs
/// (`>= 3.0.3`, `>= 3.0.0`), and correctly ordering prereleases against their release
/// (is `3.1.0-rc.1 < 3.1.0`?) is out of scope for what this tool needs.
///
/// `PartialOrd`/`Ord` are derived: Rust compares a tuple-like struct's fields in
/// declaration order, so `major` dominates `minor` dominates `patch` — exactly semver
/// precedence for release versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl From<dto::ServerVersionResponseDto> for Version {
    fn from(dto: dto::ServerVersionResponseDto) -> Self {
        Self {
            major: dto.major,
            minor: dto.minor,
            patch: dto.patch,
        }
    }
}

/// Small shared test helper for the export/import client test suites (`export.rs`,
/// `import.rs`) and this module's own tests below: spins up a real `axum` server on an
/// OS-assigned loopback port. Kept in one place — `pub(crate)` rather than duplicated three
/// times — since every HTTP client test in this crate needs the exact same "give me a
/// running server and its base URL" primitive; nothing here is specific to `send_json`'s own
/// tests.
#[cfg(test)]
pub(crate) mod test_support {
    use tokio::task::JoinHandle;
    use url::Url;

    /// Binds `app` to `127.0.0.1:0` (an OS-assigned free port) and returns its base URL
    /// (**with** a trailing slash — `Url::join`-friendly for callers that want it, though
    /// `export.rs`/`import.rs` build their own request URLs by `format!`, per this module's
    /// own `Url::join` gotcha, and so normalise the trailing slash away themselves) plus the
    /// `JoinHandle` of the task serving it. Callers just hold the handle for the test's
    /// duration; dropping it aborts the server, which is fine — tests don't need graceful
    /// shutdown.
    pub(crate) async fn spawn_test_server(app: axum::Router) -> (Url, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{addr}/")).unwrap(), handle)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::spawn_test_server;
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ---- redact_url -----------------------------------------------------------------

    #[test]
    fn redact_url_masks_key_and_slug_but_keeps_other_params() {
        let url = Url::parse("https://host/api/search/metadata?key=SuperSecret&page=2").unwrap();
        assert_eq!(
            redact_url(&url),
            "https://host/api/search/metadata?key=REDACTED&page=2"
        );
    }

    #[test]
    fn redact_url_masks_slug() {
        let url = Url::parse("https://host/api/shared-links/me?slug=holiday-2026").unwrap();
        assert_eq!(
            redact_url(&url),
            "https://host/api/shared-links/me?slug=REDACTED"
        );
    }

    #[test]
    fn redact_url_leaves_url_without_query_untouched() {
        let url = Url::parse("https://host/api/server/version").unwrap();
        assert_eq!(redact_url(&url), "https://host/api/server/version");
    }

    #[test]
    fn redact_url_leaves_non_sensitive_params_untouched() {
        let url = Url::parse("https://host/api/albums?name=Holiday").unwrap();
        assert_eq!(redact_url(&url), "https://host/api/albums?name=Holiday");
    }

    // ---- redact_json_body -------------------------------------------------------------

    #[test]
    fn redact_json_body_masks_key_field_recursively() {
        let body = br#"{"id":"abc","key":"secret-share-key","album":{"slug":"nested-secret"}}"#;
        // Key order is alphabetical: `serde_json`'s default `Value::Object` is a `BTreeMap`.
        assert_eq!(
            redact_json_body(body),
            r#"{"album":{"slug":"REDACTED"},"id":"abc","key":"REDACTED"}"#
        );
    }

    #[test]
    fn redact_json_body_leaves_null_secret_fields_as_null() {
        let body = br#"{"key":null}"#;
        assert_eq!(redact_json_body(body), r#"{"key":null}"#);
    }

    // ---- parse_retry_after --------------------------------------------------------------

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, value.parse().unwrap());
        headers
    }

    #[test]
    fn parse_retry_after_reads_plain_seconds() {
        let headers = headers_with_retry_after("5");
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_retry_after_caps_at_max() {
        let headers = headers_with_retry_after("3600");
        assert_eq!(parse_retry_after(&headers), Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn parse_retry_after_missing_header_is_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_retry_after_non_numeric_is_none() {
        // An HTTP-date form, which we deliberately don't parse — see the doc comment.
        let headers = headers_with_retry_after("Wed, 21 Oct 2026 07:28:00 GMT");
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_zero_is_none() {
        let headers = headers_with_retry_after("0");
        assert_eq!(parse_retry_after(&headers), None);
    }

    // ---- parse_error_body ----------------------------------------------------------------

    #[test]
    fn parse_error_body_reads_immich_json_shape() {
        let body = br#"{"statusCode":404,"message":"Album not found","error":"Not Found"}"#;
        let (message, error) = parse_error_body(body, StatusCode::NOT_FOUND);
        assert_eq!(message, "Album not found");
        assert_eq!(error.as_deref(), Some("Not Found"));
    }

    #[test]
    fn parse_error_body_falls_back_to_raw_text_for_non_json_body() {
        let body = b"<html>502 Bad Gateway</html>";
        let (message, error) = parse_error_body(body, StatusCode::BAD_GATEWAY);
        assert_eq!(message, "<html>502 Bad Gateway</html>");
        assert_eq!(error, None);
    }

    #[test]
    fn parse_error_body_falls_back_to_status_text_for_empty_body() {
        let (message, _) = parse_error_body(b"", StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(message, StatusCode::INTERNAL_SERVER_ERROR.to_string());
    }

    #[test]
    fn parse_error_body_truncates_very_long_non_json_bodies() {
        let body = "x".repeat(10_000);
        let (message, _) = parse_error_body(body.as_bytes(), StatusCode::BAD_REQUEST);
        assert!(message.chars().count() <= 501);
        assert!(message.ends_with('…'));
    }

    // ---- ApiError classification (Status/Decode variants) --------------------------------

    fn status_error(status: StatusCode) -> ApiError {
        ApiError::Status {
            method: Method::GET,
            url: "https://host/api/x".to_owned(),
            status,
            message: "boom".to_owned(),
            error: None,
            retry_after: None,
        }
    }

    #[test]
    fn status_429_is_retryable() {
        assert!(status_error(StatusCode::TOO_MANY_REQUESTS).is_retryable());
    }

    #[test]
    fn status_5xx_is_retryable() {
        assert!(status_error(StatusCode::INTERNAL_SERVER_ERROR).is_retryable());
        assert!(status_error(StatusCode::BAD_GATEWAY).is_retryable());
        assert!(status_error(StatusCode::SERVICE_UNAVAILABLE).is_retryable());
    }

    #[test]
    fn other_4xx_is_never_retryable() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
        ] {
            assert!(!status_error(status).is_retryable(), "status was {status}");
        }
    }

    #[test]
    fn decode_error_is_never_retryable() {
        let err = ApiError::Decode {
            method: Method::GET,
            url: "https://host/api/x".to_owned(),
            status: StatusCode::OK,
            source: serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn retry_after_is_only_surfaced_for_429() {
        let mut err = status_error(StatusCode::TOO_MANY_REQUESTS);
        if let ApiError::Status { retry_after, .. } = &mut err {
            *retry_after = Some(Duration::from_secs(7));
        }
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));

        let mut not_429 = status_error(StatusCode::SERVICE_UNAVAILABLE);
        if let ApiError::Status { retry_after, .. } = &mut not_429 {
            *retry_after = Some(Duration::from_secs(7));
        }
        assert_eq!(not_429.retry_after(), None);
    }

    #[test]
    fn api_error_status_accessor() {
        assert_eq!(
            status_error(StatusCode::NOT_FOUND).status(),
            Some(StatusCode::NOT_FOUND)
        );
    }

    // ---- ApiError classification (Transport variant) — real reqwest::Error values -------

    #[tokio::test]
    async fn connection_refused_is_classified_retryable_and_url_stays_redacted() {
        // Port 1 is reserved/unlisted; connecting to loopback on it fails fast and
        // deterministically with ECONNREFUSED, with no real network required.
        let client = Client::new();
        let url = Url::parse("http://127.0.0.1:1/?key=SuperSecret").unwrap();
        let request = client.get(url.clone());
        let err = execute_once(Method::GET, &url, request)
            .await
            .expect_err("connection to a closed loopback port must fail");
        assert!(err.is_retryable());
        // Not an assertion about the message's wording but about a leak: the whole point of
        // putting `redact_url(&url)` (not the raw `url`) into the `ApiError` is that the
        // share key must not survive into the error's own Display and from there into a log
        // line. `reqwest::Error` embeds the unredacted URL unless `without_url()` is called,
        // so this has regressed before.
        assert!(!err.to_string().contains("SuperSecret"));
    }

    #[tokio::test]
    async fn builder_error_is_not_retryable() {
        let client = Client::new();
        // An invalid header value forces a request-builder error surfaced at `.send()`.
        let request = client
            .get("http://127.0.0.1:1/")
            .header("x-test", "bad\nvalue");
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let err = execute_once(Method::GET, &url, request)
            .await
            .expect_err("an invalid header value must fail to build");
        assert!(!err.is_retryable());
    }

    // ---- Version ---------------------------------------------------------------------

    #[test]
    fn version_equal_is_greater_or_equal() {
        let a = Version {
            major: 3,
            minor: 0,
            patch: 3,
        };
        let b = Version {
            major: 3,
            minor: 0,
            patch: 3,
        };
        assert!(a >= b);
        assert_eq!(a, b);
    }

    #[test]
    fn version_minor_bump_outranks_patch() {
        let newer = Version {
            major: 3,
            minor: 1,
            patch: 0,
        };
        let older = Version {
            major: 3,
            minor: 0,
            patch: 3,
        };
        assert!(newer > older);
    }

    #[test]
    fn version_major_bump_outranks_everything() {
        let newer = Version {
            major: 3,
            minor: 0,
            patch: 3,
        };
        let older = Version {
            major: 2,
            minor: 7,
            patch: 5,
        };
        assert!(older < newer);
    }

    #[test]
    fn version_patch_comparison_is_numeric_not_lexicographic() {
        // A naive string comparison would put "3.10.0" before "3.9.9".
        let v3_10_0 = Version {
            major: 3,
            minor: 10,
            patch: 0,
        };
        let v3_9_9 = Version {
            major: 3,
            minor: 9,
            patch: 9,
        };
        assert!(v3_10_0 > v3_9_9);
    }

    #[test]
    fn version_display() {
        let v = Version {
            major: 3,
            minor: 1,
            patch: 0,
        };
        assert_eq!(v.to_string(), "3.1.0");
    }

    #[test]
    fn version_from_server_version_response_dto_drops_prerelease() {
        let dto = dto::ServerVersionResponseDto {
            major: 3,
            minor: 1,
            patch: 0,
            prerelease: Some(2),
        };
        let v: Version = dto.into();
        assert_eq!(
            v,
            Version {
                major: 3,
                minor: 1,
                patch: 0
            }
        );
    }

    // ---- send_json end-to-end (a tiny real HTTP server, not a mock) ---------------------

    #[tokio::test]
    async fn send_json_retries_a_500_and_succeeds_on_the_next_attempt() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_handler = calls.clone();
        let app = axum::Router::new().route(
            "/version",
            axum::routing::get(move || {
                let calls = calls_for_handler.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"statusCode":500,"message":"boom"})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            axum::Json(
                                serde_json::json!({"major":3,"minor":1,"patch":0,"prerelease":null}),
                            ),
                        )
                    }
                }
            }),
        );
        let (base, _server) = spawn_test_server(app).await;
        let url = base.join("version").unwrap();
        let client = build_client(Duration::from_secs(5), false, HeaderMap::new()).unwrap();
        let policy = RetryPolicy::zero_delay();

        let dto: dto::ServerVersionResponseDto =
            send_json(&policy, "get_server_version", Method::GET, &url, || {
                client.get(url.clone())
            })
            .await
            .expect("second attempt should succeed");

        assert_eq!(dto.major, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn send_json_does_not_retry_a_404() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_handler = calls.clone();
        let app = axum::Router::new().route(
            "/albums/missing",
            axum::routing::get(move || {
                let calls = calls_for_handler.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::NOT_FOUND,
                        axum::Json(
                            serde_json::json!({"statusCode":404,"message":"Album not found"}),
                        ),
                    )
                }
            }),
        );
        let (base, _server) = spawn_test_server(app).await;
        let url = base.join("albums/missing").unwrap();
        let client = build_client(Duration::from_secs(5), false, HeaderMap::new()).unwrap();
        let policy = RetryPolicy::zero_delay();

        let result: Result<dto::AlbumResponseDto, ApiError> =
            send_json(&policy, "get_album", Method::GET, &url, || {
                client.get(url.clone())
            })
            .await;

        let err = result.expect_err("404 must not be retried into success");
        assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
