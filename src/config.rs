//! Configuration for `immich-federation-at-home` — see `scratch/JOBS-DESIGN.md`'s
//! "Configuration" section, which this module implements field for field.
//!
//! There are two layers. [`Cli`] is the raw `clap` surface: every flag/environment variable
//! the program accepts, parsed but not yet interpreted — `clap` only checks *types*. [`load`]
//! turns a [`Cli`] plus an optional TOML source into the resolved [`Settings`] the rest of the
//! program actually runs on: precedence applied, secrets read, every job's required keys
//! present, everything validated. `main.rs` is the only caller that uses the real process
//! argv/environment/filesystem; every rule below is otherwise tested against literal argv and
//! an injected environment lookup, never `std::env::set_var` (unsound to call across threads
//! under edition 2024, and this crate `forbid`s `unsafe_code` outright regardless).
//!
//! Two configuration sources, never both:
//!
//! * **No `--config` / `CONFIG_FILE` / `CONFIG`** — today's behaviour, unchanged: flags and
//!   environment variables form exactly one job, internally named `default`.
//! * **A config file** (`--config`/`CONFIG_FILE`, a path; or `CONFIG`, literal TOML) — the
//!   file becomes the complete job list. Global keys not set in the file fall back to their
//!   environment variable, then to their built-in default. Job keys do not read the
//!   environment at all in this mode — only `[jobs.<name>]` tables and top-level defaults.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::parser::ValueSource;
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use url::Url;
use uuid::Uuid;

use crate::log::Level;
use crate::warn;

/// Target album resolved from `import_album`: either a UUID or an exact album name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumRef {
    /// `import_album` parsed as a UUID; resolve via `GET /albums/{id}`.
    Id(Uuid),
    /// `import_album` did not parse as a UUID; resolve via `GET /albums?name=…` (exact
    /// match, filtered client-side).
    Name(String),
}

/// A secret value (API key or share-link password) that must never be printed.
///
/// `Debug` always prints `"[redacted]"`, regardless of the wrapped value. That is what keeps
/// `#[derive(Debug)]` on [`Job`]/[`Settings`] — and therefore any `{:?}` logging of them —
/// safe. Use [`Secret::expose`] to get at the real value, and only at the point it is
/// actually needed (an HTTP header or query parameter), never in a log line.
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

/// `clap` value parser (and the TOML duration fields' parser) for `humantime` durations
/// (`30s`, `30m`, `1h30m`, `6h`, …).
fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(raw).map_err(|e| {
        format!(
            "invalid duration '{raw}': {e} (expected something like '30s', '30m', '1h30m', or '6h')"
        )
    })
}

// =============================================================================================
// Resolved configuration: what the rest of the program runs on.
// =============================================================================================

/// One export-album -> import-album mirror. Every existing `Config` field lives here now,
/// plus `name`, which only ever appears in logs (`scratch/JOBS-DESIGN.md`: "Job names are
/// TOML keys, so duplicates are already a parse error. No name validation beyond that").
#[derive(Debug, Clone)]
pub struct Job {
    pub name: String,
    pub export_album_url: String,
    pub export_album_password: Option<Secret>,
    pub import_server_url: String,
    pub import_api_key: Secret,
    pub import_album: String,
    pub interval: Duration,
    pub request_timeout: Duration,
    pub transfer_timeout: Duration,
}

impl Job {
    /// Validates the zero/empty rules `scratch/JOBS-DESIGN.md`'s Validation list asks for,
    /// applied per job: a non-empty/non-whitespace API key, and non-zero interval/timeouts.
    /// Named with the job so a failure in job 5 of 20 doesn't require guessing which one.
    pub fn validate(&self) -> Result<()> {
        if self.import_api_key.expose().trim().is_empty() {
            bail!("job '{}': import_api_key must not be empty", self.name);
        }
        if self.interval.is_zero() {
            bail!("job '{}': interval must be greater than zero", self.name);
        }
        if self.request_timeout.is_zero() {
            bail!(
                "job '{}': request_timeout must be greater than zero",
                self.name
            );
        }
        if self.transfer_timeout.is_zero() {
            bail!(
                "job '{}': transfer_timeout must be greater than zero",
                self.name
            );
        }
        Ok(())
    }

    /// Classifies `import_album` as a UUID or an exact album name.
    pub fn import_album_ref(&self) -> AlbumRef {
        match Uuid::parse_str(self.import_album.trim()) {
            Ok(id) => AlbumRef::Id(id),
            Err(_) => AlbumRef::Name(self.import_album.clone()),
        }
    }

    /// Normalises `import_server_url` into the Immich API base URL. See
    /// [`normalize_server_url`] for the (separately unit-tested) pure logic.
    pub fn import_api_base(&self) -> Result<Url> {
        normalize_server_url(&self.import_server_url)
    }
}

/// Process-global settings — not any one job's concern. See `scratch/JOBS-DESIGN.md`'s Keys
/// table for exactly what each one defaults to and which environment variable feeds it.
#[derive(Debug, Clone)]
pub struct Globals {
    pub log_level: Level,
    pub cache_dir: Option<PathBuf>,
    /// Where each in-flight asset is staged, handed to `SyncContext` and used by its
    /// temp-file builder directly rather than by exporting `TMPDIR` to the process
    /// (`std::env::set_var` is `unsafe` under edition 2024 and this crate `forbid`s
    /// `unsafe_code`). Unset means the platform's default temp directory.
    pub tmp_dir: Option<PathBuf>,
    pub transfer_concurrency: u32,
}

impl Globals {
    pub fn validate(&self) -> Result<()> {
        if self.transfer_concurrency == 0 {
            bail!("transfer_concurrency must be greater than zero");
        }
        Ok(())
    }
}

/// The whole resolved configuration: process-global settings, plus every job to run.
///
/// `jobs` is built from a `BTreeMap<String, _>` keyed by job name and then drained in order,
/// so iteration order is always sorted-by-name — deterministic, which matters because job
/// names appear in interleaved log output (`job=<name>`, `src/log.rs`).
#[derive(Debug, Clone)]
pub struct Settings {
    pub globals: Globals,
    pub jobs: Vec<Job>,
    /// `--once` / `RUN_ONCE`: an invocation mode, never a file key — see `scratch/JOBS-DESIGN.md`.
    pub run_once: bool,
}

// =============================================================================================
// The `clap` surface.
// =============================================================================================

// Every flag this program accepts, parsed but not yet resolved into a `Settings` — that's
// `load`'s job, which is also where every environment variable named in the doc comments
// below is actually read (through an injected lookup, not `clap`'s own `env` attribute —
// see the note below) and where "required" is actually enforced, with a message that names
// the job.
//
// None of these fields carry `clap`'s `env = "…"` attribute, even though every one of them
// has an equivalent environment variable — a deliberate departure from a straightforward
// `clap` setup, and the one place this module doesn't follow `scratch/JOBS-DESIGN.md`'s
// literal suggestion of leaning on `ArgMatches::value_source` against `clap`'s own env
// merge. The reason: `clap` resolves its `env` attribute by reading the *real* process
// environment directly, with no seam for a test to substitute anything else — and this
// crate's tests may never call `std::env::set_var` (unsound across threads under edition
// 2024, and `forbid`s `unsafe_code` outright regardless). Every rule this format needs to
// enforce (flag > file > env > default; two spellings disagreeing) has to be exercisable
// against a plain `HashMap`-backed lookup, so `load` reads every environment variable
// itself via its injected `env: &dyn Fn(&str) -> Option<String>` parameter, and this struct
// exists purely to define the flags, their types, and their `--help` text. `value_source`
// is still used, but only to tell "the flag was given" apart from "it wasn't" — never to
// distinguish an env-sourced value from a default, since `clap` no longer supplies either.
//
// This is a plain comment, not a doc comment, on purpose: `clap`'s derive concatenates
// *every* doc comment attached to this item into `--help`'s about text regardless of where
// among the attributes it sits, so an explanation aimed at the next maintainer has to stay
// out of the doc-comment form entirely or it leaks into user-facing output ahead of the
// actual description below.
#[derive(Parser, Debug)]
#[command(name = "immich-federation-at-home", version)]
/// Mirror one or more shared Immich albums from foreign ("export") instances into albums on
/// your own ("import") instance, using a public share link on one side and an API key on the
/// other.
///
/// With no `--config`/`CONFIG_FILE`/`CONFIG`, the flags below (each with an equivalent
/// environment variable, named in its own `--help` text — a flag wins if both are given)
/// describe a single job, exactly as before. For more than one job, or to keep secrets out
/// of the process environment, point `--config`/`CONFIG_FILE` at a TOML file, or set
/// `CONFIG` to the TOML text directly (for platforms that can only inject environment
/// variables) — see the README's "Configuration" section for the file format. Setting both
/// `CONFIG_FILE`/`--config` and `CONFIG` is a startup error.
pub struct Cli {
    /// Path to a TOML config file describing one or more jobs. Also settable as
    /// `CONFIG_FILE`. Mutually exclusive with `CONFIG` (which holds the TOML text directly,
    /// for platforms that can only inject environment variables, and has no flag of its
    /// own) — setting both is a startup error.
    #[arg(long = "config")]
    pub config_file: Option<PathBuf>,

    // ---- globals: flag > file > env > default -----------------------------------------
    /// Log verbosity: everything at this level and above is written to stderr. Also
    /// settable as `LOG_LEVEL`. Default: `info`.
    #[arg(long, value_enum)]
    pub log_level: Option<Level>,

    /// Directory for the content-hash cache, shared by every job. Also settable as
    /// `CACHE_DIR`. Leave unset to disable the cache entirely.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Directory used to stage each asset's bytes transiently while it is in flight. Also
    /// settable as `TMPDIR`. Leave unset to use the platform's default temp directory.
    #[arg(long)]
    pub tmp_dir: Option<PathBuf>,

    /// How many assets to transfer in parallel, across every job in the process. Also
    /// settable as `TRANSFER_CONCURRENCY`, or (compatibility only) `IMPORT_CONCURRENCY` —
    /// setting both is a startup error. Default: `4`.
    #[arg(long = "transfer-concurrency")]
    pub transfer_concurrency: Option<u32>,

    /// Old spelling of `--transfer-concurrency`/`TRANSFER_CONCURRENCY`, kept only so
    /// existing deployments keep working.
    #[arg(long = "import-concurrency", hide = true)]
    pub import_concurrency: Option<u32>,

    // ---- job keys: only form the implicit `default` job when no config file is given ---
    /// Share link for the album to mirror, e.g. `https://photos.friend.example/share/AbC123`
    /// or `https://photos.friend.example/s/holiday-2026`. Also settable as
    /// `EXPORT_ALBUM_URL`. Ignored if a config file is in use — set it there instead, per
    /// job or as a top-level default.
    #[arg(long)]
    pub export_album_url: Option<String>,

    /// Password for the share link above. Also settable as `EXPORT_ALBUM_PASSWORD`. Leave
    /// unset if the link has no password. Ignored if a config file is in use.
    #[arg(long)]
    pub export_album_password: Option<Secret>,

    /// Base URL of your own ("import") Immich instance, e.g. `https://immich.example.com`.
    /// Also settable as `IMPORT_SERVER_URL`. Ignored if a config file is in use.
    #[arg(long)]
    pub import_server_url: Option<String>,

    /// API key for the import instance. Needs the `asset.upload`, `album.read`, and
    /// `albumAsset.create` permissions (or the `all` wildcard). Also settable as
    /// `IMPORT_API_KEY`. Ignored if a config file is in use.
    #[arg(long)]
    pub import_api_key: Option<Secret>,

    /// Target album on the import instance: a UUID, or an exact album name. It must already
    /// exist. Also settable as `IMPORT_ALBUM`. Ignored if a config file is in use.
    #[arg(long)]
    pub import_album: Option<String>,

    /// How often to check the export album for new assets, as a `humantime` duration (e.g.
    /// `30m`, `1h30m`, `6h`). Also settable as `INTERVAL`, or (compatibility only)
    /// `IMPORT_INTERVAL` — setting both is a startup error. Ignored if a config file is in
    /// use. Default: `1h`.
    #[arg(long = "interval", value_parser = parse_duration)]
    pub interval: Option<Duration>,

    /// Old spelling of `--interval`/`INTERVAL`, kept only so existing deployments keep
    /// working.
    #[arg(long = "import-interval", hide = true, value_parser = parse_duration)]
    pub import_interval: Option<Duration>,

    /// Timeout for metadata calls (album listing, search pagination, permission checks, …),
    /// as a `humantime` duration. Also settable as `REQUEST_TIMEOUT`. Ignored if a config
    /// file is in use. Default: `30s`.
    #[arg(long, value_parser = parse_duration)]
    pub request_timeout: Option<Duration>,

    /// Timeout for downloading and re-uploading a single asset, as a `humantime` duration.
    /// Also settable as `TRANSFER_TIMEOUT`. Ignored if a config file is in use. Default:
    /// `30m`.
    #[arg(long, value_parser = parse_duration)]
    pub transfer_timeout: Option<Duration>,

    /// Do one sync pass per job and exit, instead of looping forever. Also settable as
    /// `RUN_ONCE=true` / `RUN_ONCE=false`. An invocation mode, never a config file key.
    #[arg(long = "once", action = clap::ArgAction::SetTrue)]
    pub run_once: bool,
}

/// Normalises a `import_server_url` value into the Immich API base URL: strips a trailing
/// `/`, then a trailing `/api` (so a trailing `/api/` also works), then appends `/api`.
/// Sub-path deployments, non-default ports, and both `http`/`https` are preserved, since only
/// the path is touched. Any query string or fragment on the input is dropped.
pub fn normalize_server_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim())
        .with_context(|| format!("import_server_url '{raw}' is not a valid URL"))?;
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/api").unwrap_or(path);
    url.set_path(&format!("{path}/api"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

// =============================================================================================
// Config-file discovery: `--config`/`CONFIG_FILE` (a path) vs `CONFIG` (literal TOML).
// =============================================================================================

/// Where the TOML config, if any, comes from. Resolved without touching the filesystem, so
/// it's plain data a caller (`main.rs`, or a test) can act on however it likes — `main.rs`
/// reads a [`ConfigSource::Path`] from disk itself; [`ConfigSource::Inline`] already has its
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    Path(PathBuf),
    Inline(String),
}

/// Resolves `--config`/`CONFIG_FILE`/`CONFIG` into at most one [`ConfigSource`], erroring if
/// both a path and inline TOML are given — never a precedence puzzle, per
/// `scratch/JOBS-DESIGN.md`.
pub fn resolve_config_source(
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ConfigSource>> {
    // `--config` has no `clap` `env` attribute (see `Cli`'s doc comment for why), so
    // `CONFIG_FILE` is consulted here directly rather than relying on `clap` to have merged
    // it in already.
    let path = if matches.value_source("config_file") == Some(ValueSource::CommandLine) {
        matches.get_one::<PathBuf>("config_file").cloned()
    } else {
        env("CONFIG_FILE").map(PathBuf::from)
    };
    let inline = env("CONFIG");
    match (path, inline) {
        (Some(_), Some(_)) => bail!(
            "both CONFIG_FILE (or --config) and CONFIG are set; set only one — CONFIG_FILE/\
             --config name a TOML file on disk, CONFIG holds the TOML text directly"
        ),
        (Some(p), None) => Ok(Some(ConfigSource::Path(p))),
        (None, Some(s)) => Ok(Some(ConfigSource::Inline(s))),
        (None, None) => Ok(None),
    }
}

/// The TOML text for [`load`], plus (when it came from a file) the path it came from — the
/// only reason [`load`] would ever need the path is
/// [`warn_if_world_or_group_readable`]'s permission check, which makes no sense for `CONFIG`
/// (an environment variable has no Unix permission bits).
pub struct ConfigText<'a> {
    pub toml: &'a str,
    pub path: Option<&'a Path>,
}

// =============================================================================================
// The per-job/top-level-default TOML shape.
// =============================================================================================

/// One `[jobs.<name>]` table, or the top-level defaults for every job — the same shape
/// either way, since every job key is also a top-level default (`scratch/JOBS-DESIGN.md`).
///
/// This is deserialized straight from a `toml::Table` with all four *global* keys and
/// `jobs` itself already removed (see [`parse_config_file`]), so
/// `deny_unknown_fields` here is exactly the unknown-key check the design asks for: a
/// typo'd key anywhere in a job table, or at the top level, fails loudly instead of being
/// silently ignored or silently inherited.
///
/// Why this isn't one `#[serde(flatten)]`ed struct covering globals + defaults + jobs in a
/// single pass: serde's `deny_unknown_fields` does not compose with `#[serde(flatten)]` (a
/// long-standing serde limitation, not an oversight here) — a flattened field swallows
/// anything it doesn't recognise instead of erroring, which is exactly the "typo'd
/// `import_album` silently inherits the top-level one" bug this format has to rule out. Pull
/// the globals out by hand first, and this struct can `deny_unknown_fields` safely.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobToml {
    export_album_url: Option<String>,
    export_album_password: Option<String>,
    export_album_password_file: Option<String>,
    export_album_password_env: Option<String>,
    import_server_url: Option<String>,
    import_api_key: Option<String>,
    import_api_key_file: Option<String>,
    import_api_key_env: Option<String>,
    import_album: Option<String>,
    /// `humantime` text, parsed by [`resolve_duration`] — kept as a raw string here so a bad
    /// duration can be reported with the job and key that produced it, rather than a bare
    /// serde error.
    interval: Option<String>,
    request_timeout: Option<String>,
    transfer_timeout: Option<String>,
}

/// Which table a secret-resolution or required-key error happened in, so the message names
/// it — "the top level, or one job" per `scratch/JOBS-DESIGN.md`.
enum SecretScope<'a> {
    TopLevel,
    Job(&'a str),
}

impl fmt::Display for SecretScope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretScope::TopLevel => write!(f, "top-level default"),
            SecretScope::Job(name) => write!(f, "job '{name}'"),
        }
    }
}

/// Resolves one secret key's three spellings (`key`, `key_file`, `key_env`) in a single
/// table. At most one may be set — two is a startup error naming the table and the key.
/// Returns whether the *inline* spelling was the one used, since that's what
/// [`warn_if_world_or_group_readable`] cares about.
///
/// `_file` is read, and `_env` looked up through the injected `env`, right here — "read at
/// startup" per `scratch/JOBS-DESIGN.md` — so a caller never needs to know which spelling
/// won.
fn resolve_secret_field(
    scope: &SecretScope<'_>,
    key: &str,
    inline: Option<String>,
    file: Option<String>,
    env_var: Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Option<Secret>, bool)> {
    let spellings_set = [inline.is_some(), file.is_some(), env_var.is_some()]
        .into_iter()
        .filter(|set| *set)
        .count();
    if spellings_set > 1 {
        bail!(
            "{scope}: {key} is set more than once — use only one of {key}, {key}_file, or \
             {key}_env"
        );
    }

    if let Some(value) = inline {
        return Ok((
            Some(Secret::from_str(&value).unwrap_or_else(|e| match e {})),
            true,
        ));
    }
    if let Some(path) = file {
        let path = PathBuf::from(path);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("{scope}: could not read {key}_file at {}", path.display()))?;
        let value = trim_trailing_newline(&content);
        return Ok((
            Some(Secret::from_str(value).unwrap_or_else(|e| match e {})),
            false,
        ));
    }
    if let Some(name) = env_var {
        let value = env(&name).ok_or_else(|| {
            anyhow!("{scope}: {key}_env names environment variable {name}, which is not set")
        })?;
        return Ok((
            Some(Secret::from_str(&value).unwrap_or_else(|e| match e {})),
            false,
        ));
    }
    Ok((None, false))
}

/// `_file` secrets are "read at startup, trailing newline trimmed" per
/// `scratch/JOBS-DESIGN.md` — a single trailing `\n` or `\r\n`, the way editors and `echo`
/// leave a secret file, not every trailing whitespace character.
fn trim_trailing_newline(s: &str) -> &str {
    s.strip_suffix('\n')
        .map_or(s, |s| s.strip_suffix('\r').unwrap_or(s))
}

/// Resolves one duration key: the job's own value, else the top-level default, else
/// `builtin_default` — all as raw `humantime` text until here, so a parse failure can name
/// the job and the key.
fn resolve_duration(
    job_name: &str,
    key: &str,
    job_value: Option<String>,
    default_value: Option<String>,
    builtin_default: &str,
) -> Result<Duration> {
    let raw = job_value
        .or(default_value)
        .unwrap_or_else(|| builtin_default.to_owned());
    parse_duration(&raw).map_err(|e| anyhow!("job '{job_name}': {key}: {e}"))
}

/// Warns (never fails) if `path` is group- or world-readable and the config it came from set
/// at least one secret inline, per `scratch/JOBS-DESIGN.md`: "silent is wrong; fatal is
/// wrong too — it is the operator's call". `_file`/`_env` secrets never end up in the file
/// itself, so they don't trigger this.
fn warn_if_world_or_group_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o044 != 0 {
            warn!(
                "config file {} contains an inline secret and is group- or world-readable \
                 (mode {mode:o}); consider an _file or _env spelling instead, or restricting \
                 the file's permissions to the owner only",
                path.display()
            );
        }
    }
}

/// Reads and parses one environment variable through the injected lookup, naming the
/// variable in the error if `parse` rejects it.
fn parse_env<T>(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    parse: impl Fn(&str) -> std::result::Result<T, String>,
) -> Result<Option<T>> {
    match env(name) {
        Some(raw) => parse(&raw).map(Some).map_err(|e| anyhow!("{name}: {e}")),
        None => Ok(None),
    }
}

/// One flag's value, straight from `clap`, *only* if it was actually given on the command
/// line — `clap`'s own default (if the arg has one) is deliberately not returned here, since
/// every caller of this function wants to fall through to an env lookup or a file value
/// first. See [`Cli`]'s doc comment for why this whole module reads environment variables
/// itself rather than leaning on `clap`'s `env` attribute.
fn flag_value<T: Clone + Send + Sync + 'static>(matches: &clap::ArgMatches, id: &str) -> Option<T> {
    if matches.value_source(id) == Some(ValueSource::CommandLine) {
        matches.get_one::<T>(id).cloned()
    } else {
        None
    }
}

/// Resolves one key's **flag > file > env** precedence, with no built-in default: the
/// result is `None` when none of the three said anything. This is the shared core of
/// [`resolve_global`] (which adds a default on top) and is also used standalone for
/// `export_album_password` (genuinely optional — "unset" is itself the default) and, with
/// `file_value` always `None`, for every job key of the implicit no-file job.
fn resolve_optional<T: Clone + Send + Sync + 'static>(
    matches: &clap::ArgMatches,
    id: &str,
    env: &dyn Fn(&str) -> Option<String>,
    env_name: &str,
    parse: impl Fn(&str) -> std::result::Result<T, String>,
    file_value: Option<T>,
) -> Result<Option<T>> {
    if let Some(v) = flag_value::<T>(matches, id) {
        return Ok(Some(v));
    }
    if file_value.is_some() {
        return Ok(file_value);
    }
    parse_env(env, env_name, parse)
}

/// Resolves one key's **flag > file > env > default** precedence — the full global-key rule
/// from `scratch/JOBS-DESIGN.md`. With `file_value` always `None`, this is also just "flag >
/// env > default", which is exactly what an implicit-job key with a built-in default
/// (`request_timeout`, `transfer_timeout`) needs.
fn resolve_global<T: Clone + Send + Sync + 'static>(
    matches: &clap::ArgMatches,
    id: &str,
    env: &dyn Fn(&str) -> Option<String>,
    env_name: &str,
    parse: impl Fn(&str) -> std::result::Result<T, String>,
    file_value: Option<T>,
    default: T,
) -> Result<T> {
    Ok(resolve_optional(matches, id, env, env_name, parse, file_value)?.unwrap_or(default))
}

/// A required key that's still missing after everything else has had a chance to supply it:
/// named with the flag/env pair an operator would recognise from `--help`.
fn require<T>(value: Option<T>, flag: &str, env_name: &str) -> Result<T> {
    value.ok_or_else(|| anyhow!("{flag} (or {env_name}) is required"))
}

/// [`resolve_global`], for the two aliased globals/job-keys (`transfer_concurrency` and
/// `interval`) that have an old spelling to stay compatible with. Erroring if both spellings
/// were explicitly given (flag or env, either counts) is `scratch/JOBS-DESIGN.md`'s "never a
/// silent precedence rule": the two spellings disagreeing is exactly the situation an
/// operator mid-migration cannot spot from a log line, so it's fatal rather than resolved
/// one way or the other. Runs — and can fail — before any other validation. `file_value` is
/// always `None` for `interval` (a job key: the file governs it entirely through
/// [`resolve_duration`], never through this alias mechanism) and the real top-level TOML
/// value for `transfer_concurrency` (a global).
#[allow(clippy::too_many_arguments)]
fn resolve_global_alias<T: Clone + Send + Sync + 'static>(
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    new_id: &str,
    new_env: &str,
    old_id: &str,
    old_env: &str,
    parse: impl Fn(&str) -> std::result::Result<T, String>,
    file_value: Option<T>,
    default: T,
) -> Result<T> {
    let new_flag = flag_value::<T>(matches, new_id);
    let old_flag = flag_value::<T>(matches, old_id);
    let new_env_value = parse_env(env, new_env, &parse)?;
    let old_env_value = parse_env(env, old_env, &parse)?;

    if (new_flag.is_some() || new_env_value.is_some())
        && (old_flag.is_some() || old_env_value.is_some())
    {
        bail!(
            "both {new_env} and {old_env} are set; they are two spellings of the same setting \
             — set only {new_env} ({old_env} is accepted for backward compatibility only)"
        );
    }

    if let Some(v) = new_flag.or(old_flag) {
        return Ok(v);
    }
    if let Some(v) = file_value {
        return Ok(v);
    }
    Ok(new_env_value.or(old_env_value).unwrap_or(default))
}

/// `RUN_ONCE=true`/`RUN_ONCE=false` — the one non-`humantime`, non-numeric environment
/// variable this format reads, so it gets its own tiny parser rather than reusing
/// `resolve_global`'s `impl Fn(&str) -> Result<T, String>` shape for a `bool`.
fn parse_bool(raw: &str) -> std::result::Result<bool, String> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "invalid value '{other}' (expected 'true' or 'false')"
        )),
    }
}

/// `--once` / `RUN_ONCE`: an invocation mode, so — unlike every job/global key — it is
/// resolved the same way regardless of whether a config file is in play.
fn resolve_run_once(
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<bool> {
    if matches.get_flag("run_once") {
        return Ok(true);
    }
    Ok(parse_env(env, "RUN_ONCE", parse_bool)?.unwrap_or(false))
}

// =============================================================================================
// The loader.
// =============================================================================================

/// Resolves a full [`Settings`] from parsed argv (`matches`), an injected environment lookup,
/// and — when a config file is in play — its already-read TOML text. This is the whole
/// precedence/inheritance/validation engine from `scratch/JOBS-DESIGN.md`'s "Configuration"
/// section, and the reason it takes `matches` (rather than a parsed [`Cli`]) plus its own
/// `env` lookup for every field: telling "the user typed `--transfer-concurrency`" apart
/// from "that's just the default" needs `ArgMatches::value_source`, and — per [`Cli`]'s own
/// doc comment — every environment variable has to be read through the injected `env`
/// closure rather than `clap`'s `env` attribute, so that a test can supply one without ever
/// touching the real process environment.
///
/// `main.rs` is the only caller that supplies real argv/env/filesystem; every test in this
/// module's `tests` submodule calls this with literal argv (via `Cli::command()` +
/// `try_get_matches_from`), a closure over a `HashMap`, and TOML fixtures as string literals.
pub fn load(
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    config: Option<ConfigText<'_>>,
) -> Result<Settings> {
    let file = config.map(|c| parse_config_file(env, &c)).transpose()?;

    // Globals are resolved the same way regardless of which source is active — `file` is
    // simply `None` when there's no config file, which degenerates flag > file > env >
    // default into flag > env > default, exactly today's behaviour.
    let log_level = resolve_global(
        matches,
        "log_level",
        env,
        "LOG_LEVEL",
        |s| Level::from_str(s, true),
        file.as_ref().and_then(|f| f.log_level),
        Level::Info,
    )?;
    let cache_dir = resolve_optional(
        matches,
        "cache_dir",
        env,
        "CACHE_DIR",
        |s| Ok(PathBuf::from(s)),
        file.as_ref().and_then(|f| f.cache_dir.clone()),
    )?;
    let tmp_dir = resolve_optional(
        matches,
        "tmp_dir",
        env,
        "TMPDIR",
        |s| Ok(PathBuf::from(s)),
        file.as_ref().and_then(|f| f.tmp_dir.clone()),
    )?;
    let transfer_concurrency = resolve_global_alias(
        matches,
        env,
        "transfer_concurrency",
        "TRANSFER_CONCURRENCY",
        "import_concurrency",
        "IMPORT_CONCURRENCY",
        |s| s.parse::<u32>().map_err(|e| e.to_string()),
        file.as_ref().and_then(|f| f.transfer_concurrency),
        4,
    )?;

    let jobs = if let Some(f) = file {
        f.jobs
    } else {
        vec![build_implicit_job(matches, env)?]
    };

    let settings = Settings {
        globals: Globals {
            log_level,
            cache_dir,
            tmp_dir,
            transfer_concurrency,
        },
        jobs,
        run_once: resolve_run_once(matches, env)?,
    };
    settings.globals.validate()?;
    Ok(settings)
}

/// Builds the implicit `default` job from flags/env alone — [`load`]'s no-config-file path,
/// unchanged in behaviour from today's single-job tool. Every key here follows **flag >
/// env > default** (`interval` additionally accepting its old spelling), since there is no
/// file to fall back to.
fn build_implicit_job(
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Job> {
    let string_env = |s: &str| Ok(s.to_owned());

    let interval = resolve_global_alias(
        matches,
        env,
        "interval",
        "INTERVAL",
        "import_interval",
        "IMPORT_INTERVAL",
        parse_duration,
        None,
        Duration::from_secs(3600),
    )?;
    let job = Job {
        name: "default".to_owned(),
        export_album_url: require(
            resolve_optional(
                matches,
                "export_album_url",
                env,
                "EXPORT_ALBUM_URL",
                string_env,
                None,
            )?,
            "--export-album-url",
            "EXPORT_ALBUM_URL",
        )?,
        export_album_password: resolve_optional::<Secret>(
            matches,
            "export_album_password",
            env,
            "EXPORT_ALBUM_PASSWORD",
            |s| Ok(Secret::from_str(s).unwrap_or_else(|e| match e {})),
            None,
        )?,
        import_server_url: require(
            resolve_optional(
                matches,
                "import_server_url",
                env,
                "IMPORT_SERVER_URL",
                string_env,
                None,
            )?,
            "--import-server-url",
            "IMPORT_SERVER_URL",
        )?,
        import_api_key: require(
            resolve_optional::<Secret>(
                matches,
                "import_api_key",
                env,
                "IMPORT_API_KEY",
                |s| Ok(Secret::from_str(s).unwrap_or_else(|e| match e {})),
                None,
            )?,
            "--import-api-key",
            "IMPORT_API_KEY",
        )?,
        import_album: require(
            resolve_optional(
                matches,
                "import_album",
                env,
                "IMPORT_ALBUM",
                string_env,
                None,
            )?,
            "--import-album",
            "IMPORT_ALBUM",
        )?,
        interval,
        request_timeout: resolve_global(
            matches,
            "request_timeout",
            env,
            "REQUEST_TIMEOUT",
            parse_duration,
            None,
            Duration::from_secs(30),
        )?,
        transfer_timeout: resolve_global(
            matches,
            "transfer_timeout",
            env,
            "TRANSFER_TIMEOUT",
            parse_duration,
            None,
            Duration::from_secs(30 * 60),
        )?,
    };
    job.validate()?;
    Ok(job)
}

/// Everything [`parse_config_file`] pulls out of a config file: the raw (not yet merged with
/// flags/env) global keys it set, and the fully resolved job list — job resolution never
/// touches `clap` at all, so it's finished here rather than deferred to [`load`].
struct FileConfig {
    jobs: Vec<Job>,
    log_level: Option<Level>,
    cache_dir: Option<PathBuf>,
    tmp_dir: Option<PathBuf>,
    transfer_concurrency: Option<u32>,
}

/// The four global keys, pulled out of the top-level TOML table by hand — see
/// [`JobToml`]'s doc comment for why they can't just be part of that struct.
struct FileGlobals {
    log_level: Option<Level>,
    cache_dir: Option<PathBuf>,
    tmp_dir: Option<PathBuf>,
    transfer_concurrency: Option<u32>,
}

/// Removes and parses the four global keys from `table`, leaving whatever's left (the
/// top-level job defaults, or a typo) for the caller to deserialize as a [`JobToml`].
fn extract_file_globals(table: &mut toml::Table) -> Result<FileGlobals> {
    let log_level = table
        .remove("log_level")
        .map(toml::Value::try_into::<Level>)
        .transpose()
        .context("log_level: invalid value")?;
    let cache_dir = table
        .remove("cache_dir")
        .map(toml::Value::try_into::<String>)
        .transpose()
        .context("cache_dir: invalid value")?
        .map(PathBuf::from);
    let tmp_dir = table
        .remove("tmp_dir")
        .map(toml::Value::try_into::<String>)
        .transpose()
        .context("tmp_dir: invalid value")?
        .map(PathBuf::from);
    let transfer_concurrency = table
        .remove("transfer_concurrency")
        .map(toml::Value::try_into::<u32>)
        .transpose()
        .context("transfer_concurrency: invalid value")?;
    Ok(FileGlobals {
        log_level,
        cache_dir,
        tmp_dir,
        transfer_concurrency,
    })
}

/// A required job key that's still missing after inheritance: job value, else top-level
/// default, else an error naming both the job and the key.
fn require_inherited(
    name: &str,
    key: &str,
    job_value: Option<String>,
    default_value: Option<String>,
) -> Result<String> {
    job_value.or(default_value).ok_or_else(|| {
        anyhow!("job '{name}': {key} is required (set it in this job or as a top-level default)")
    })
}

/// One secret key's inheritance: if the job sets *any* of its three spellings, resolve from
/// the job alone — the top-level value, however it's spelled, is ignored entirely and never
/// even read. Otherwise fall back to resolving the top-level default. This is "inheritance
/// is per key, not per spelling" (`scratch/JOBS-DESIGN.md`), the one rule
/// [`resolve_secret_field`] itself can't enforce since it only ever sees one table at a time.
#[allow(clippy::too_many_arguments)]
fn resolve_inherited_secret(
    name: &str,
    key: &str,
    job_inline: Option<String>,
    job_file: Option<String>,
    job_env: Option<String>,
    default_inline: Option<String>,
    default_file: Option<String>,
    default_env: Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Option<Secret>, bool)> {
    if job_inline.is_some() || job_file.is_some() || job_env.is_some() {
        resolve_secret_field(
            &SecretScope::Job(name),
            key,
            job_inline,
            job_file,
            job_env,
            env,
        )
    } else {
        resolve_secret_field(
            &SecretScope::TopLevel,
            key,
            default_inline,
            default_file,
            default_env,
            env,
        )
    }
}

/// Resolves one `[jobs.<name>]` table against the top-level `defaults`, per
/// `scratch/JOBS-DESIGN.md`'s inheritance and secret rules. Returns the built [`Job`] plus
/// whether resolving it used an *inline* secret spelling (job's own or, on fallback, the
/// top level's) — accumulated by the caller into the file-wide
/// [`warn_if_world_or_group_readable`] check.
fn build_job_from_toml(
    name: &str,
    job: &JobToml,
    defaults: &JobToml,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Job, bool)> {
    let export_album_url = require_inherited(
        name,
        "export_album_url",
        job.export_album_url.clone(),
        defaults.export_album_url.clone(),
    )?;
    let import_server_url = require_inherited(
        name,
        "import_server_url",
        job.import_server_url.clone(),
        defaults.import_server_url.clone(),
    )?;
    let import_album = require_inherited(
        name,
        "import_album",
        job.import_album.clone(),
        defaults.import_album.clone(),
    )?;

    let (import_api_key, api_key_inline) = resolve_inherited_secret(
        name,
        "import_api_key",
        job.import_api_key.clone(),
        job.import_api_key_file.clone(),
        job.import_api_key_env.clone(),
        defaults.import_api_key.clone(),
        defaults.import_api_key_file.clone(),
        defaults.import_api_key_env.clone(),
        env,
    )?;
    let import_api_key = import_api_key.ok_or_else(|| {
        anyhow!(
            "job '{name}': import_api_key is required (set import_api_key, \
             import_api_key_file, or import_api_key_env — in this job or as a top-level \
             default)"
        )
    })?;

    let (export_album_password, password_inline) = resolve_inherited_secret(
        name,
        "export_album_password",
        job.export_album_password.clone(),
        job.export_album_password_file.clone(),
        job.export_album_password_env.clone(),
        defaults.export_album_password.clone(),
        defaults.export_album_password_file.clone(),
        defaults.export_album_password_env.clone(),
        env,
    )?;

    let interval = resolve_duration(
        name,
        "interval",
        job.interval.clone(),
        defaults.interval.clone(),
        "1h",
    )?;
    let request_timeout = resolve_duration(
        name,
        "request_timeout",
        job.request_timeout.clone(),
        defaults.request_timeout.clone(),
        "30s",
    )?;
    let transfer_timeout = resolve_duration(
        name,
        "transfer_timeout",
        job.transfer_timeout.clone(),
        defaults.transfer_timeout.clone(),
        "30m",
    )?;

    let job = Job {
        name: name.to_owned(),
        export_album_url,
        export_album_password,
        import_server_url,
        import_api_key,
        import_album,
        interval,
        request_timeout,
        transfer_timeout,
    };
    job.validate()?;
    Ok((job, api_key_inline || password_inline))
}

/// Parses a config file's TOML and resolves it into a [`FileConfig`]: pulls the four global
/// keys and `jobs` out of the top-level table by hand (see [`JobToml`]'s doc comment for
/// why), then resolves every job against the top-level defaults.
fn parse_config_file(
    env: &dyn Fn(&str) -> Option<String>,
    config: &ConfigText<'_>,
) -> Result<FileConfig> {
    let describe_source = || match config.path {
        Some(path) => format!("config file {}", path.display()),
        None => "the CONFIG environment variable".to_owned(),
    };

    let mut table: toml::Table = toml::from_str(config.toml)
        .with_context(|| format!("failed to parse {}", describe_source()))?;

    let globals = extract_file_globals(&mut table)?;
    let jobs_value = table.remove("jobs");

    // Whatever's left is the top-level defaults table; `JobToml`'s `deny_unknown_fields`
    // is what catches a mistyped global key here (it was only removed above if spelled
    // exactly right).
    let defaults: JobToml = toml::Value::Table(table)
        .try_into()
        .context("top-level config: unknown or invalid key")?;

    let jobs_table = match jobs_value {
        Some(toml::Value::Table(t)) => t,
        Some(_) => bail!("`jobs` must be a table of job tables (`[jobs.<name>]`)"),
        None => toml::Table::new(),
    };
    if jobs_table.is_empty() {
        bail!(
            "the config file defines no jobs (`jobs` is absent or empty); if you only want a \
             single job, remove the config file and use the environment variables/flags \
             instead"
        );
    }

    // `BTreeMap`, keyed by job name, so the resulting `Vec<Job>` is always sorted by name —
    // deterministic order for `job=<name>` log output no matter what order the TOML wrote
    // the tables in.
    let jobs_toml: BTreeMap<String, JobToml> = jobs_table
        .into_iter()
        .map(|(name, value)| {
            let toml::Value::Table(table) = value else {
                bail!("job '{name}': must be a table");
            };
            let job: JobToml = toml::Value::Table(table)
                .try_into()
                .with_context(|| format!("job '{name}': unknown or invalid key"))?;
            Ok((name, job))
        })
        .collect::<Result<_>>()?;

    let mut jobs = Vec::with_capacity(jobs_toml.len());
    let mut seen_pairs: HashMap<(String, String), String> = HashMap::new();
    let mut used_inline_secret = false;

    for (name, job_toml) in &jobs_toml {
        let (job, inline) = build_job_from_toml(name, job_toml, &defaults, env)?;
        used_inline_secret |= inline;

        // Warning, not fatal (`scratch/JOBS-DESIGN.md`): two jobs sharing only an import
        // album, or only an export album, are both legitimate and must stay silent — only
        // the exact pair repeating is flagged.
        let pair = (job.export_album_url.clone(), job.import_album.clone());
        if let Some(first) = seen_pairs.get(&pair) {
            warn!(
                "jobs '{first}' and '{name}' share the same export_album_url and \
                 import_album — that duplicates work; if it's intentional, ignore this warning"
            );
        } else {
            seen_pairs.insert(pair, name.clone());
        }

        jobs.push(job);
    }

    if let (Some(path), true) = (config.path, used_inline_secret) {
        warn_if_world_or_group_readable(path);
    }

    Ok(FileConfig {
        jobs,
        log_level: globals.log_level,
        cache_dir: globals.cache_dir,
        tmp_dir: globals.tmp_dir,
        transfer_concurrency: globals.transfer_concurrency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// No injected variable ever set — the "no file" tests exercise flags only, matching
    /// today's actual deployments (which set environment variables, but from `clap`'s point
    /// of view a flag and its equivalent env var take the same code path once parsed).
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Bool flags take no value — anything else in `extra` is a `--flag value` pair.
    const BOOL_FLAGS: &[&str] = &["--once"];

    /// Builds `ArgMatches` for a minimal, valid, no-config-file argv: every required field
    /// for the implicit job, plus whatever `extra` specifies — *replacing* rather than
    /// duplicating a flag that's already in the base set (`clap` rejects the same non-multi
    /// flag appearing twice). Mirrors the old `Config`-module test helper.
    fn matches(extra: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
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
        Cli::command().try_get_matches_from(args)
    }

    fn load_no_file(extra: &[&str]) -> Result<Settings> {
        let m = matches(extra).expect("argv should parse");
        load(&m, &no_env, None)
    }

    fn load_with_env(extra: &[&str], env: &dyn Fn(&str) -> Option<String>) -> Result<Settings> {
        let m = matches(extra).expect("argv should parse");
        load(&m, env, None)
    }

    fn load_file(extra: &[&str], toml: &str) -> Result<Settings> {
        let m = matches(extra).expect("argv should parse");
        load(&m, &no_env, Some(ConfigText { toml, path: None }))
    }

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // ---- no config file: today's behaviour, unchanged ------------------------------------

    #[test]
    fn no_file_forms_one_implicit_job_named_default_with_every_default() {
        let settings = load_no_file(&[]).expect("minimal config should resolve");
        assert_eq!(settings.jobs.len(), 1);
        let job = &settings.jobs[0];
        assert_eq!(job.name, "default");
        assert_eq!(job.interval, Duration::from_secs(3600));
        assert_eq!(job.request_timeout, Duration::from_secs(30));
        assert_eq!(job.transfer_timeout, Duration::from_secs(30 * 60));
        assert!(job.export_album_password.is_none());
        assert!(!settings.run_once);
        assert_eq!(settings.globals.log_level, Level::Info);
        assert_eq!(settings.globals.transfer_concurrency, 4);
        assert!(settings.globals.cache_dir.is_none());
        assert!(settings.globals.tmp_dir.is_none());
    }

    #[test]
    fn no_file_once_flag_sets_run_once() {
        let settings = load_no_file(&["--once"]).unwrap();
        assert!(settings.run_once);
    }

    #[test]
    fn no_file_missing_required_field_is_a_load_error() {
        // No file, and clap no longer enforces "required" for job keys (whether they're
        // required depends on whether a config file is in play) — so this is now an error
        // from `load`, not a clap parse error.
        let m = Cli::command()
            .try_get_matches_from(["immich-federation-at-home"])
            .expect("argv itself must still parse: nothing here is required at the clap level");
        assert!(load(&m, &no_env, None).is_err());
    }

    #[test]
    fn no_file_import_album_ref_classifies_uuid_and_name() {
        let settings =
            load_no_file(&["--import-album", "3fa85f64-5717-4562-b3fc-2c963f66afa6"]).unwrap();
        assert_eq!(
            settings.jobs[0].import_album_ref(),
            AlbumRef::Id(Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap())
        );

        let settings = load_no_file(&["--import-album", "Family Photos"]).unwrap();
        assert_eq!(
            settings.jobs[0].import_album_ref(),
            AlbumRef::Name("Family Photos".to_owned())
        );
    }

    // ---- normalize_server_url (unchanged pure logic) --------------------------------------

    #[test]
    fn normalize_server_url_bare_origin() {
        let url = normalize_server_url("https://immich.example.com").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_trailing_api_and_slash() {
        let url = normalize_server_url("https://immich.example.com/api/").unwrap();
        assert_eq!(url.as_str(), "https://immich.example.com/api");
    }

    #[test]
    fn normalize_server_url_rejects_garbage() {
        assert!(normalize_server_url("not a url").is_err());
    }

    // ---- humantime duration parsing --------------------------------------------------------

    #[test]
    fn duration_parser_accepts_plain_and_compound_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(
            parse_duration("1h30m").unwrap(),
            Duration::from_secs(3600 + 30 * 60)
        );
    }

    #[test]
    fn duration_parser_rejects_garbage() {
        assert!(parse_duration("banana").is_err());
    }

    // ---- Secret redaction -------------------------------------------------------------------

    #[test]
    fn secret_debug_is_always_redacted() {
        let secret = Secret::from_str("hunter2").unwrap();
        assert_eq!(format!("{secret:?}"), "[redacted]");
    }

    /// Not a format assertion but a security one: whatever `{:?}` on a `Job`/`Settings`
    /// produces, the password and the API key must not be anywhere in it.
    #[test]
    fn debug_never_prints_secrets() {
        let settings =
            load_no_file(&["--export-album-password", "hunter2"]).expect("should resolve");
        let debug_output = format!("{settings:?}");
        assert!(!debug_output.contains("hunter2"));
        assert!(!debug_output.contains("test-api-key"));
    }

    // ---- per-job zero/empty validation ------------------------------------------------------

    #[test]
    fn validate_rejects_empty_api_key() {
        assert!(load_no_file(&["--import-api-key", ""]).is_err());
    }

    #[test]
    fn validate_rejects_whitespace_only_api_key() {
        assert!(load_no_file(&["--import-api-key", "   "]).is_err());
    }

    #[test]
    fn validate_rejects_zero_interval() {
        assert!(load_no_file(&["--interval", "0s"]).is_err());
    }

    #[test]
    fn validate_rejects_zero_request_timeout() {
        assert!(load_no_file(&["--request-timeout", "0s"]).is_err());
    }

    #[test]
    fn validate_rejects_zero_transfer_timeout() {
        assert!(load_no_file(&["--transfer-timeout", "0s"]).is_err());
    }

    #[test]
    fn validate_rejects_zero_transfer_concurrency() {
        assert!(load_no_file(&["--transfer-concurrency", "0"]).is_err());
    }

    // ---- aliased env vars: INTERVAL/IMPORT_INTERVAL, TRANSFER_CONCURRENCY/IMPORT_CONCURRENCY

    #[test]
    fn both_interval_spellings_set_is_an_error() {
        let env = env_map(&[("INTERVAL", "2h"), ("IMPORT_INTERVAL", "2h")]);
        let lookup = |k: &str| env.get(k).cloned();
        assert!(load_with_env(&[], &lookup).is_err());
    }

    #[test]
    fn interval_new_spelling_alone_works() {
        let env = env_map(&[("INTERVAL", "2h")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load_with_env(&[], &lookup).unwrap();
        assert_eq!(settings.jobs[0].interval, Duration::from_secs(2 * 3600));
    }

    #[test]
    fn interval_old_spelling_alone_works() {
        let env = env_map(&[("IMPORT_INTERVAL", "3h")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load_with_env(&[], &lookup).unwrap();
        assert_eq!(settings.jobs[0].interval, Duration::from_secs(3 * 3600));
    }

    #[test]
    fn both_concurrency_spellings_set_is_an_error() {
        let env = env_map(&[("TRANSFER_CONCURRENCY", "8"), ("IMPORT_CONCURRENCY", "8")]);
        let lookup = |k: &str| env.get(k).cloned();
        assert!(load_with_env(&[], &lookup).is_err());
    }

    #[test]
    fn concurrency_new_spelling_alone_works() {
        let env = env_map(&[("TRANSFER_CONCURRENCY", "8")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load_with_env(&[], &lookup).unwrap();
        assert_eq!(settings.globals.transfer_concurrency, 8);
    }

    #[test]
    fn concurrency_old_spelling_alone_works() {
        let env = env_map(&[("IMPORT_CONCURRENCY", "9")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load_with_env(&[], &lookup).unwrap();
        assert_eq!(settings.globals.transfer_concurrency, 9);
    }

    #[test]
    fn flag_wins_over_both_alias_env_vars_unset() {
        let settings = load_no_file(&["--transfer-concurrency", "16"]).unwrap();
        assert_eq!(settings.globals.transfer_concurrency, 16);
    }

    // ---- config file: precedence, inheritance, secrets, validation ------------------------

    const MINIMAL_JOB_TOML: &str = r#"
        [jobs.family]
        export_album_url = "https://export.example.com/s/abc"
        import_server_url = "https://import.example.com"
        import_api_key = "file-api-key"
        import_album = "Family Photos"
    "#;

    #[test]
    fn file_present_forms_jobs_from_the_file_not_the_implicit_default() {
        let settings = load_file(&[], MINIMAL_JOB_TOML).unwrap();
        assert_eq!(settings.jobs.len(), 1);
        assert_eq!(settings.jobs[0].name, "family");
        assert_eq!(settings.jobs[0].import_album, "Family Photos");
    }

    #[test]
    fn zero_jobs_in_file_is_an_error_naming_the_env_var_alternative() {
        let err = load_file(&[], "log_level = \"info\"").unwrap_err();
        assert!(err.to_string().contains("environment variables"));
    }

    #[test]
    fn jobs_present_but_empty_is_also_zero_jobs() {
        assert!(load_file(&[], "[jobs]").is_err());
    }

    #[test]
    fn job_key_in_job_table_beats_top_level_default() {
        let toml = r#"
            import_album = "Default Album"

            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Family Photos"
        "#;
        let settings = load_file(&[], toml).unwrap();
        assert_eq!(settings.jobs[0].import_album, "Family Photos");
    }

    #[test]
    fn job_inherits_required_key_from_top_level() {
        let toml = r#"
            import_server_url = "https://import.example.com"
            import_api_key = "k"

            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_album = "Family Photos"
        "#;
        let settings = load_file(&[], toml).unwrap();
        assert_eq!(
            settings.jobs[0].import_server_url,
            "https://import.example.com"
        );
    }

    #[test]
    fn missing_required_key_after_inheritance_names_the_job() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_api_key = "k"
            import_album = "Family Photos"
        "#;
        let err = load_file(&[], toml).unwrap_err();
        assert!(err.to_string().contains("family"));
        assert!(err.to_string().contains("import_server_url"));
    }

    #[test]
    fn unknown_key_at_top_level_is_an_error() {
        let toml = r#"
            not_a_real_key = "oops"

            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Family Photos"
        "#;
        assert!(load_file(&[], toml).is_err());
    }

    #[test]
    fn unknown_key_inside_a_job_is_an_error() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_albus = "typo'd key"
        "#;
        assert!(load_file(&[], toml).is_err());
    }

    #[test]
    fn config_file_and_config_inline_both_set_is_an_error() {
        let m = matches(&[]).unwrap();
        let env = env_map(&[("CONFIG_FILE", "/tmp/whatever.toml"), ("CONFIG", "x = 1")]);
        let lookup = |k: &str| env.get(k).cloned();
        assert!(resolve_config_source(&m, &lookup).is_err());
    }

    #[test]
    fn duplicate_pair_is_a_warning_not_a_fatal_error() {
        let toml = r#"
            [jobs.a]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Same Album"

            [jobs.b]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Same Album"
        "#;
        assert!(load_file(&[], toml).is_ok());
    }

    #[test]
    fn sharing_only_import_album_is_silent_and_legitimate() {
        let toml = r#"
            [jobs.a]
            export_album_url = "https://export.example.com/s/one"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Merged Album"

            [jobs.b]
            export_album_url = "https://export.example.com/s/two"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Merged Album"
        "#;
        assert!(load_file(&[], toml).is_ok());
    }

    #[test]
    fn jobs_are_sorted_by_name() {
        let toml = r#"
            [jobs.zebra]
            export_album_url = "https://export.example.com/s/z"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "Z"

            [jobs.alpha]
            export_album_url = "https://export.example.com/s/a"
            import_server_url = "https://import.example.com"
            import_api_key = "k"
            import_album = "A"
        "#;
        let settings = load_file(&[], toml).unwrap();
        let names: Vec<&str> = settings.jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    // ---- global precedence: flag > file > env > default ------------------------------------

    #[test]
    fn global_flag_beats_file_and_env() {
        let toml = "log_level = \"debug\"\n".to_owned() + MINIMAL_JOB_TOML;
        let m = matches(&["--log-level", "error"]).unwrap();
        let env = env_map(&[("LOG_LEVEL", "warn")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load(
            &m,
            &lookup,
            Some(ConfigText {
                toml: &toml,
                path: None,
            }),
        )
        .unwrap();
        assert_eq!(settings.globals.log_level, Level::Error);
    }

    #[test]
    fn global_file_beats_env_when_no_flag() {
        let toml = "log_level = \"debug\"\n".to_owned() + MINIMAL_JOB_TOML;
        let m = matches(&[]).unwrap();
        let env = env_map(&[("LOG_LEVEL", "warn")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load(
            &m,
            &lookup,
            Some(ConfigText {
                toml: &toml,
                path: None,
            }),
        )
        .unwrap();
        assert_eq!(settings.globals.log_level, Level::Debug);
    }

    #[test]
    fn global_env_used_when_file_does_not_set_it() {
        let m = matches(&[]).unwrap();
        let env = env_map(&[("LOG_LEVEL", "warn")]);
        let lookup = |k: &str| env.get(k).cloned();
        let settings = load(
            &m,
            &lookup,
            Some(ConfigText {
                toml: MINIMAL_JOB_TOML,
                path: None,
            }),
        )
        .unwrap();
        assert_eq!(settings.globals.log_level, Level::Warn);
    }

    #[test]
    fn global_default_used_when_nothing_else_set() {
        let settings = load_file(&[], MINIMAL_JOB_TOML).unwrap();
        assert_eq!(settings.globals.log_level, Level::Info);
        assert_eq!(settings.globals.transfer_concurrency, 4);
    }

    // ---- secrets: three spellings, per key per table --------------------------------------

    #[test]
    fn secret_inline_spelling_resolves() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "inline-key"
            import_album = "Family Photos"
        "#;
        let settings = load_file(&[], toml).unwrap();
        assert_eq!(settings.jobs[0].import_api_key.expose(), "inline-key");
    }

    #[test]
    fn secret_file_spelling_resolves_and_trims_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "file-key\n").unwrap();

        let toml = format!(
            r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key_file = "{}"
            import_album = "Family Photos"
        "#,
            path.display()
        );
        let settings = load_file(&[], &toml).unwrap();
        assert_eq!(settings.jobs[0].import_api_key.expose(), "file-key");
    }

    #[test]
    fn secret_file_spelling_missing_file_is_a_fatal_error_naming_the_path() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key_file = "/does/not/exist/at/all"
            import_album = "Family Photos"
        "#;
        let err = load_file(&[], toml).unwrap_err();
        assert!(err.to_string().contains("/does/not/exist/at/all"));
    }

    #[test]
    fn secret_env_spelling_resolves() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key_env = "FAMILY_API_KEY"
            import_album = "Family Photos"
        "#;
        let env = env_map(&[("FAMILY_API_KEY", "env-key")]);
        let lookup = |k: &str| env.get(k).cloned();
        let m = matches(&[]).unwrap();
        let settings = load(&m, &lookup, Some(ConfigText { toml, path: None })).unwrap();
        assert_eq!(settings.jobs[0].import_api_key.expose(), "env-key");
    }

    #[test]
    fn secret_env_spelling_missing_var_is_a_fatal_error_naming_the_variable() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key_env = "MISSING_VAR"
            import_album = "Family Photos"
        "#;
        let err = load_file(&[], toml).unwrap_err();
        assert!(err.to_string().contains("MISSING_VAR"));
    }

    #[test]
    fn two_secret_spellings_in_one_job_is_an_error() {
        let toml = r#"
            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "inline-key"
            import_api_key_env = "FAMILY_API_KEY"
            import_album = "Family Photos"
        "#;
        assert!(load_file(&[], toml).is_err());
    }

    #[test]
    fn two_secret_spellings_at_top_level_is_an_error() {
        let toml = r#"
            import_api_key = "inline-key"
            import_api_key_env = "FAMILY_API_KEY"

            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_album = "Family Photos"
        "#;
        assert!(load_file(&[], toml).is_err());
    }

    #[test]
    fn job_overriding_an_inherited_secret_spelling_uses_only_the_jobs_own() {
        let toml = r#"
            import_api_key_env = "TOP_LEVEL_KEY"

            [jobs.family]
            export_album_url = "https://export.example.com/s/abc"
            import_server_url = "https://import.example.com"
            import_api_key = "job-inline-key"
            import_album = "Family Photos"
        "#;
        // TOP_LEVEL_KEY deliberately left unset: if the job's own spelling didn't fully
        // override the inherited one, resolving the top-level default would fail here.
        let settings = load_file(&[], toml).unwrap();
        assert_eq!(settings.jobs[0].import_api_key.expose(), "job-inline-key");
    }
}
