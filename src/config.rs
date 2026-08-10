//! Environment-variable configuration for `immich-federation-at-home`, parsed with `clap`'s
//! derive API so that `--help` doubles as documentation (every flag below also has an `env`
//! attribute, so `--help` shows the environment variable it reads).
//!
//! Every variable mirrors `PLAN.md` §4's table field-for-field. `clap` only checks *types*;
//! call [`Config::validate`] afterwards to enforce the semantic rules from `PLAN.md` §5 step
//! 1 (non-empty API key, non-zero interval/timeouts/concurrency).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use url::Url;
use uuid::Uuid;

use crate::log::Level;

/// Target album resolved from `IMPORT_ALBUM`: either a UUID or an exact album name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumRef {
    /// `IMPORT_ALBUM` parsed as a UUID; resolve via `GET /albums/{id}`.
    Id(Uuid),
    /// `IMPORT_ALBUM` did not parse as a UUID; resolve via `GET /albums?name=…` (exact
    /// match, filtered client-side).
    Name(String),
}

/// A secret value (API key or share-link password) that must never be printed.
///
/// `Debug` always prints `"[redacted]"`, regardless of the wrapped value. That is what keeps
/// `#[derive(Debug)]` on [`Config`] — and therefore any `{:?}` logging of it — safe per
/// `PLAN.md` §8 ("never log the API key, the password, or the share key"). Use
/// [`Secret::expose`] to get at the real value, and only at the point it is actually needed
/// (an HTTP header or query parameter), never in a log line.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// The wrapped value. Named loudly (rather than e.g. `AsRef`/`Deref`) so call sites that
    /// reach for the real secret are easy to grep for and to eyeball in review.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl FromStr for Secret {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// `clap` value parser for `humantime` durations (`30s`, `30m`, `1h30m`, `6h`, …), used for
/// `IMPORT_INTERVAL`, `REQUEST_TIMEOUT`, and `TRANSFER_TIMEOUT`.
fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(raw).map_err(|e| {
        format!(
            "invalid duration '{raw}': {e} (expected something like '30s', '30m', '1h30m', or '6h')"
        )
    })
}

/// Mirror a shared Immich album from a foreign ("export") instance into an album on your own
/// ("import") instance, using a public share link on one side and an API key on the other.
///
/// All configuration is environment variables (the flags below are equivalent — a flag wins
/// over its environment variable if both are given). Repeated runs are idempotent and the
/// tool keeps no state of its own: `TMPDIR` (or the platform's default temp directory) is
/// used to stage each asset's bytes transiently while it is in flight, and nothing is kept
/// afterwards.
#[derive(Parser, Debug)]
#[command(name = "immich-federation-at-home", version)]
pub struct Config {
    /// Share link for the album to mirror, e.g. `https://photos.friend.example/share/AbC123`
    /// or `https://photos.friend.example/s/holiday-2026`. Sub-path deployments and trailing
    /// slashes are tolerated.
    #[arg(long, env = "EXPORT_ALBUM_URL")]
    pub export_album_url: String,

    /// Password for the share link above. Leave unset if the link has no password.
    #[arg(long, env = "EXPORT_ALBUM_PASSWORD")]
    pub export_album_password: Option<Secret>,

    /// Base URL of your own ("import") Immich instance, e.g. `https://immich.example.com`. A
    /// trailing `/` and a trailing `/api` are both tolerated and normalised away.
    #[arg(long, env = "IMPORT_SERVER_URL")]
    pub import_server_url: String,

    /// API key for the import instance. Needs the `asset.upload`, `album.read`, and
    /// `albumAsset.create` permissions (or the `all` wildcard).
    #[arg(long, env = "IMPORT_API_KEY")]
    pub import_api_key: Secret,

    /// Target album on the import instance: a UUID, or an exact album name. It must already
    /// exist — this program errors out rather than creating it.
    #[arg(long, env = "IMPORT_ALBUM")]
    pub import_album: String,

    /// How often to check the export album for new assets, as a `humantime` duration (e.g.
    /// `30m`, `1h30m`, `6h`).
    #[arg(long, env = "IMPORT_INTERVAL", default_value = "1h", value_parser = parse_duration)]
    pub import_interval: Duration,

    /// Log verbosity: everything at this level and above is written to stderr.
    #[arg(long, env = "LOG_LEVEL", value_enum, default_value_t = Level::Info)]
    pub log_level: Level,

    /// How many assets to transfer in parallel.
    #[arg(long, env = "IMPORT_CONCURRENCY", default_value_t = 4)]
    pub import_concurrency: u32,

    /// Timeout for metadata calls (album listing, search pagination, permission checks, …),
    /// as a `humantime` duration.
    #[arg(long, env = "REQUEST_TIMEOUT", default_value = "30s", value_parser = parse_duration)]
    pub request_timeout: Duration,

    /// Timeout for downloading and re-uploading a single asset, as a `humantime` duration.
    #[arg(long, env = "TRANSFER_TIMEOUT", default_value = "30m", value_parser = parse_duration)]
    pub transfer_timeout: Duration,

    /// Do one sync pass and exit, instead of looping forever with `IMPORT_INTERVAL` between
    /// runs. Also settable as `RUN_ONCE=true` / `RUN_ONCE=false`.
    #[arg(long = "once", env = "RUN_ONCE", action = clap::ArgAction::SetTrue)]
    pub run_once: bool,
}

impl Config {
    /// Validates the semantic rules from `PLAN.md` §5 step 1 that `clap`'s type-level
    /// parsing cannot express on its own: a non-empty/non-whitespace API key, and non-zero
    /// interval, concurrency, and timeouts. Returns an actionable error naming the offending
    /// environment variable.
    pub fn validate(&self) -> Result<()> {
        if self.import_api_key.expose().trim().is_empty() {
            bail!("IMPORT_API_KEY must not be empty");
        }
        if self.import_interval.is_zero() {
            bail!("IMPORT_INTERVAL must be greater than zero");
        }
        if self.import_concurrency == 0 {
            bail!("IMPORT_CONCURRENCY must be greater than zero");
        }
        if self.request_timeout.is_zero() {
            bail!("REQUEST_TIMEOUT must be greater than zero");
        }
        if self.transfer_timeout.is_zero() {
            bail!("TRANSFER_TIMEOUT must be greater than zero");
        }
        Ok(())
    }

    /// Classifies `IMPORT_ALBUM` as a UUID or an exact album name.
    pub fn import_album_ref(&self) -> AlbumRef {
        match Uuid::parse_str(self.import_album.trim()) {
            Ok(id) => AlbumRef::Id(id),
            Err(_) => AlbumRef::Name(self.import_album.clone()),
        }
    }

    /// Normalises `IMPORT_SERVER_URL` into the Immich API base URL. See
    /// [`normalize_server_url`] for the (separately unit-tested) pure logic.
    pub fn import_api_base(&self) -> Result<Url> {
        normalize_server_url(&self.import_server_url)
    }
}

/// Normalises an `IMPORT_SERVER_URL` value into the Immich API base URL: strips a trailing
/// `/`, then a trailing `/api` (so a trailing `/api/` also works), then appends `/api`.
/// Sub-path deployments, non-default ports, and both `http`/`https` are preserved, since only
/// the path is touched. Any query string or fragment on the input is dropped.
pub fn normalize_server_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim())
        .with_context(|| format!("IMPORT_SERVER_URL '{raw}' is not a valid URL"))?;
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/api").unwrap_or(path);
    url.set_path(&format!("{path}/api"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal set of CLI args covering every *required* field, so tests only have to
    /// override what they care about. Deliberately goes through `Config::try_parse_from`
    /// (clap's own testing surface) rather than the real process environment: mutating
    /// `std::env` is unsound across threads under the Rust 2024 edition (`set_var` is
    /// `unsafe`), and this crate additionally `forbid`s `unsafe_code` outright, so real
    /// env-var mutation isn't an option here even for a single, serialized test. Every case
    /// below is therefore expressed as explicit `--flag value` arguments, which exercises
    /// exactly the same parsing/value-parser code paths as the environment-variable form.
    /// Flags that take no value (`ArgAction::SetTrue`) — anything else in `extra` is treated
    /// as a `--flag value` pair.
    const BOOL_FLAGS: &[&str] = &["--once"];

    /// Builds a full, valid argv from a minimal set of required flags plus whatever `extra`
    /// specifies, *replacing* rather than duplicating a flag that's already in the base set
    /// (clap's `try_parse_from` rejects the same non-multi flag appearing twice). This is
    /// what lets individual tests override e.g. `--import-album` or `--import-api-key`
    /// without hand-rolling the whole argv each time.
    fn parse(extra: &[&str]) -> Result<Config, clap::Error> {
        let mut fields: Vec<(&str, Option<String>)> = vec![
            (
                "--export-album-url",
                Some("https://photos.friend.example/share/AbC123".to_owned()),
            ),
            (
                "--import-server-url",
                Some("https://immich.example.com".to_owned()),
            ),
            ("--import-api-key", Some("test-api-key".to_owned())),
            ("--import-album", Some("My Album".to_owned())),
        ];

        let mut i = 0;
        while i < extra.len() {
            let flag = extra[i];
            let value = if BOOL_FLAGS.contains(&flag) {
                i += 1;
                None
            } else {
                let v = extra.get(i + 1).copied().unwrap_or_default();
                i += 2;
                Some(v.to_owned())
            };
            match fields.iter_mut().find(|(f, _)| *f == flag) {
                Some(existing) => existing.1 = value,
                None => fields.push((flag, value)),
            }
        }

        let mut args = vec!["immich-federation-at-home".to_owned()];
        for (flag, value) in fields {
            args.push(flag.to_owned());
            if let Some(v) = value {
                args.push(v);
            }
        }
        Config::try_parse_from(args)
    }

    // ---- defaults ----------------------------------------------------------------------

    #[test]
    fn defaults_match_plan_table() {
        let cfg = parse(&[]).expect("minimal config should parse");
        assert_eq!(cfg.import_interval, Duration::from_secs(3600));
        assert_eq!(cfg.log_level, Level::Info);
        assert_eq!(cfg.import_concurrency, 4);
        assert_eq!(cfg.request_timeout, Duration::from_secs(30));
        assert_eq!(cfg.transfer_timeout, Duration::from_secs(30 * 60));
        assert!(!cfg.run_once);
        assert!(cfg.export_album_password.is_none());
    }

    // ---- --once / RUN_ONCE flag --------------------------------------------------------

    #[test]
    fn once_flag_sets_run_once() {
        let cfg = parse(&["--once"]).unwrap();
        assert!(cfg.run_once);
    }

    // ---- humantime duration parsing ----------------------------------------------------

    #[test]
    fn duration_parser_accepts_plain_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(30 * 60));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn duration_parser_accepts_compound_units() {
        assert_eq!(
            parse_duration("1h30m").unwrap(),
            Duration::from_secs(3600 + 30 * 60)
        );
    }

    #[test]
    fn duration_parser_rejects_garbage() {
        assert!(parse_duration("banana").is_err());
    }

    #[test]
    fn import_interval_flag_uses_duration_parser() {
        let cfg = parse(&["--import-interval", "1h30m"]).unwrap();
        assert_eq!(cfg.import_interval, Duration::from_secs(3600 + 30 * 60));
    }

    #[test]
    fn import_interval_flag_rejects_garbage() {
        assert!(parse(&["--import-interval", "banana"]).is_err());
    }

    // ---- LOG_LEVEL ----------------------------------------------------------------------

    #[test]
    fn log_level_flag_parses_all_variants() {
        for (flag, expected) in [
            ("error", Level::Error),
            ("warn", Level::Warn),
            ("info", Level::Info),
            ("debug", Level::Debug),
            ("trace", Level::Trace),
        ] {
            let cfg = parse(&["--log-level", flag]).unwrap();
            assert_eq!(cfg.log_level, expected, "flag was {flag}");
        }
    }

    #[test]
    fn log_level_flag_rejects_unknown_variant() {
        assert!(parse(&["--log-level", "verbose"]).is_err());
    }

    // ---- AlbumRef classification ---------------------------------------------------------

    #[test]
    fn import_album_ref_classifies_uuid() {
        let cfg = parse(&["--import-album", "3fa85f64-5717-4562-b3fc-2c963f66afa6"]).unwrap();
        assert_eq!(
            cfg.import_album_ref(),
            AlbumRef::Id(Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap())
        );
    }

    #[test]
    fn import_album_ref_classifies_name() {
        let cfg = parse(&["--import-album", "Family Photos"]).unwrap();
        assert_eq!(
            cfg.import_album_ref(),
            AlbumRef::Name("Family Photos".to_owned())
        );
    }

    #[test]
    fn import_album_ref_classifies_uuid_like_but_invalid_string_as_name() {
        // One character short of a real UUID: must not be misclassified.
        let cfg = parse(&["--import-album", "3fa85f64-5717-4562-b3fc-2c963f66afa"]).unwrap();
        assert!(matches!(cfg.import_album_ref(), AlbumRef::Name(_)));
    }

    // ---- IMPORT_SERVER_URL normalisation --------------------------------------------------

    #[test]
    fn normalize_server_url_bare_origin() {
        let url = normalize_server_url("https://immich.example.com").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_trailing_slash() {
        let url = normalize_server_url("https://immich.example.com/").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_trailing_api() {
        let url = normalize_server_url("https://immich.example.com/api").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_trailing_api_and_slash() {
        let url = normalize_server_url("https://immich.example.com/api/").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_sub_path_deployment() {
        let url = normalize_server_url("https://host/immich").unwrap();
        assert_eq!(url.as_str(), "https://host/immich/api");
    }

    #[test]
    fn normalize_server_url_sub_path_with_trailing_slash() {
        let url = normalize_server_url("https://host/immich/").unwrap();
        assert_eq!(url.as_str(), "https://host/immich/api");
    }

    #[test]
    fn normalize_server_url_sub_path_already_has_api() {
        let url = normalize_server_url("https://host/immich/api").unwrap();
        assert_eq!(url.as_str(), "https://host/immich/api");
    }

    #[test]
    fn normalize_server_url_http_scheme_preserved() {
        let url = normalize_server_url("http://immich.example.com").unwrap();
        assert_eq!(url.as_str(), "http://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_non_default_port_preserved() {
        let url = normalize_server_url("https://immich.example.com:8443").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com:8443/api");
    }

    #[test]
    fn normalize_server_url_rejects_garbage() {
        assert!(normalize_server_url("not a url").is_err());
    }

    // ---- Secret redaction ------------------------------------------------------------------

    #[test]
    fn secret_debug_is_always_redacted() {
        let secret = Secret::from_str("hunter2").unwrap();
        assert_eq!(format!("{secret:?}"), "[redacted]");
    }

    #[test]
    fn secret_expose_returns_the_real_value() {
        let secret = Secret::from_str("hunter2").unwrap();
        assert_eq!(secret.expose(), "hunter2");
    }

    /// Not a format assertion but a security one: whatever `{:?}` on a `Config` produces, the
    /// password and the API key must not be anywhere in it.
    #[test]
    fn config_debug_never_prints_secrets() {
        let cfg = parse(&["--export-album-password", "hunter2"]).unwrap();
        let debug_output = format!("{cfg:?}");
        assert!(!debug_output.contains("hunter2"));
        assert!(!debug_output.contains("test-api-key"));
    }

    // ---- validation ---------------------------------------------------------------------

    #[test]
    fn validate_accepts_minimal_config() {
        let cfg = parse(&[]).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_api_key() {
        assert!(
            parse(&["--import-api-key", ""])
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_rejects_whitespace_only_api_key() {
        assert!(
            parse(&["--import-api-key", "   "])
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_rejects_zero_interval() {
        assert!(
            parse(&["--import-interval", "0s"])
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_rejects_zero_concurrency() {
        assert!(
            parse(&["--import-concurrency", "0"])
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_rejects_zero_request_timeout() {
        assert!(
            parse(&["--request-timeout", "0s"])
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_rejects_zero_transfer_timeout() {
        assert!(
            parse(&["--transfer-timeout", "0s"])
                .unwrap()
                .validate()
                .is_err()
        );
    }

    // ---- required fields ------------------------------------------------------------------

    #[test]
    fn missing_required_field_is_a_parse_error() {
        // No args at all: every required field is missing.
        assert!(Config::try_parse_from(["immich-federation-at-home"]).is_err());
    }
}
