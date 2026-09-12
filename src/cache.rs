//! The path-hash → content-hash cache (see `scratch/CACHE-DESIGN.md`, and
//! `scratch/JOBS-DESIGN.md`'s "Cache, file format v2" for why it's namespaced and shared).
//!
//! Immich external libraries hash by *path*, not by content, so the checksum the export
//! instance reports for such an asset can never be checked against the bytes we download,
//! and can never match anything on the import instance (which always content-hashes on
//! upload). We learn the real content hash the first time we download such an asset and
//! remember it here, keyed by the path hash, so later runs can skip the download entirely
//! when nothing has changed.
//!
//! The cache is a fact about an *export* instance — "the file whose path hash is X currently
//! hashes to Y on this server" — not a record of anything this program did. Losing it can
//! only ever cost work (a re-download), never correctness, which is what allows every
//! failure mode below (missing file, corrupt file, unknown format) to be a warning rather
//! than fatal, and is what makes an unconfigured `CACHE_DIR` a supported, silent no-op
//! rather than a degraded mode callers need to branch on.
//!
//! One process can run several jobs, possibly against several different export instances,
//! possibly two jobs against the *same* one. A path hash alone is not a safe cache key
//! across instances (two different servers can have a file at the same path), so every entry
//! is namespaced under the export API base URL it was learned from — an opaque string as far
//! as this module is concerned, never parsed or validated here. Two jobs sharing an export
//! instance therefore share entries too, which is the main practical win of running them in
//! one process: only the first of them pays the cold download.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// File name inside `CACHE_DIR`; see `scratch/JOBS-DESIGN.md`'s "Format" section for the
/// exact JSON shape this (de)serializes.
const FILE_NAME: &str = "content-hashes.json";

/// Bumped only if the on-disk shape ever changes incompatibly. An older or newer reader
/// disagreeing with this number is treated exactly like a corrupt file (see [`load`]) —
/// there is deliberately no migration path, since the cache is disposable. v1 (a flat
/// `entries` map, no instance namespacing, no `last_seen`) reads as this and simply starts
/// empty; there is no code anywhere that understands the v1 shape any more.
const CURRENT_VERSION: u32 = 2;

/// How long an entry may go unread before [`ContentHashCache::persist`] drops it. Not
/// configurable — see `scratch/JOBS-DESIGN.md`. There is no "keep-set" retain rule any more
/// (that only ever made sense when exactly one job, on one schedule, used the cache), so this
/// TTL is the only thing standing between the file and unbounded growth as export albums
/// change over time.
const TTL_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    modified: DateTime<Utc>,
    content: String,
    /// Refreshed to now on every [`ContentHashCache::get`] hit (not just on insert), so an
    /// entry a job keeps using every tick never expires, and one nothing looks up any more
    /// ages out within [`TTL_DAYS`].
    last_seen: DateTime<Utc>,
}

/// The on-disk shape, one file's worth. Kept as its own type (rather than serializing
/// `ContentHashCache` directly) because the live cache also tracks `dir`, which has no
/// business in the file.
#[derive(Debug, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    instances: HashMap<String, HashMap<String, Entry>>,
}

/// An in-memory map of export instance → path hash → (the `modified` timestamp it was
/// learned at, the content hash, when it was last looked up), optionally backed by a JSON
/// file. `get`/`insert`/`persist` all take `&self`: the cache is opened once per process
/// (`main.rs`) and shared across every job's concurrent transfer futures behind an `Arc`, so
/// the map lives behind a plain [`std::sync::Mutex`] rather than requiring exclusive access.
///
/// Two locks, deliberately not one: `entries` guards the map itself and is only ever held
/// for a map operation or a `clone`, and `persist_lock` serializes [`Self::persist`]
/// end-to-end (snapshot, serialize, write, rename) so that two jobs persisting at the same
/// moment can't have the older snapshot land on disk last — see that method's doc comment.
/// `persist_lock` is always the *outer* lock: `persist` takes it for the whole call and only
/// ever acquires `entries` (briefly, released before the file I/O) from inside it, never the
/// other way around, so there is no ordering inversion to deadlock on. No method on this type
/// is async, so neither lock is ever held across an await point — a guarantee callers get for
/// free rather than one they have to be careful about.
pub struct ContentHashCache {
    dir: Option<PathBuf>,
    entries: Mutex<HashMap<String, HashMap<String, Entry>>>,
    persist_lock: Mutex<()>,
}

impl ContentHashCache {
    /// Disabled — [`get`](Self::get)/[`insert`](Self::insert) still work as an ordinary
    /// in-memory map for this process's own lifetime (so a single run's own downloads still
    /// dedupe against each other), but nothing is ever loaded from or written to disk:
    /// [`persist`](Self::persist) does nothing, and nothing survives past this process. This
    /// is what an unset `CACHE_DIR` produces: a fresh `cargo run` must never fail because of
    /// an unwritable default, so "no cache configured" has to be representable as a value
    /// rather than an error.
    pub fn disabled() -> Self {
        Self {
            dir: None,
            entries: Mutex::new(HashMap::new()),
            persist_lock: Mutex::new(()),
        }
    }

    /// Opens (creating if needed) `dir` and loads any existing cache file.
    ///
    /// Writability is verified right now by actually creating a file in `dir`, renaming it,
    /// and removing it — not merely by `create_dir_all` succeeding, which a root-owned bind
    /// mount can do while every subsequent write fails. Catching that here makes a
    /// misconfigured `CACHE_DIR` a loud, immediate startup error instead of a silent
    /// failure discovered only when the first run tries to save.
    ///
    /// A file that exists but is unreadable, unparseable, or has an unknown `version` is
    /// *not* an error here, per the module doc: the cache is disposable, so the correct
    /// response is to warn once and start empty, not to take the program down.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create cache directory {}", dir.display()))?;

        let probe = tempfile::Builder::new()
            .prefix(".write-check-")
            .tempfile_in(dir)
            .with_context(|| format!("cache directory {} is not writable", dir.display()))?;
        let probe_path = dir.join(".write-check");
        probe
            .persist(&probe_path)
            .map_err(|e| e.error)
            .with_context(|| format!("cache directory {} is not writable", dir.display()))?;
        fs::remove_file(&probe_path)
            .with_context(|| format!("failed to remove write-check file in {}", dir.display()))?;

        Ok(Self {
            dir: Some(dir.to_path_buf()),
            entries: Mutex::new(load(dir)),
            persist_lock: Mutex::new(()),
        })
    }

    /// The cached content hash for this `instance`'s `path_checksum`, if one was recorded at
    /// exactly this `modified` timestamp. A path hash never changes when the underlying file
    /// changes (that's the whole problem this cache exists to work around), so `modified` is
    /// the only signal that the file has moved on since we learned this entry; a differing
    /// timestamp is treated as a miss rather than a stale hit. A hit refreshes the entry's
    /// `last_seen` to now, which is what keeps an entry a job still uses every tick from
    /// ageing out under [`Self::persist`]'s TTL prune.
    pub fn get(
        &self,
        instance: &str,
        path_checksum: &str,
        modified: DateTime<Utc>,
    ) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(instance)?.get_mut(path_checksum)?;
        if entry.modified != modified {
            return None;
        }
        entry.last_seen = Utc::now();
        Some(entry.content.clone())
    }

    /// Records a content hash learned by downloading, overwriting any existing entry for
    /// this instance's path hash, and stamping `last_seen` as now.
    pub fn insert(
        &self,
        instance: &str,
        path_checksum: String,
        modified: DateTime<Utc>,
        content_checksum: String,
    ) {
        let mut entries = self.entries.lock().unwrap();
        entries.entry(instance.to_owned()).or_default().insert(
            path_checksum,
            Entry {
                modified,
                content: content_checksum,
                last_seen: Utc::now(),
            },
        );
    }

    /// Writes the cache to disk, first pruning entries whose `last_seen` is older than
    /// [`TTL_DAYS`] (dropping an instance's map entirely once it's empty, so a retired export
    /// instance doesn't leave a dead key behind forever). A no-op when disabled. Errors are
    /// returned, not logged, so the caller decides how loud a failed save should be.
    ///
    /// Serialised end-to-end — prune, snapshot, serialise, write, rename — under
    /// `persist_lock`, held for the whole call. Without that, two jobs finishing at the same
    /// moment could both read the map, both serialize their own snapshot, and then write in
    /// either order — the newer snapshot could lose the race and an older one would land on
    /// disk last, silently reverting whichever job wrote first. `entries`' own lock is only
    /// taken twice, briefly, inside this: once by [`Self::prune`], once to clone the map for
    /// serialization — never held across the file I/O below.
    ///
    /// The write itself is temp-file-then-`rename` for atomicity: a crash or a concurrent
    /// reader never observes a half-written file.
    pub fn persist(&self) -> Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        let _persist_guard = self.persist_lock.lock().unwrap();

        self.prune(Utc::now());
        let file_format = {
            let entries = self.entries.lock().unwrap();
            FileFormat {
                version: CURRENT_VERSION,
                instances: entries.clone(),
            }
        };

        let json = serde_json::to_vec_pretty(&file_format).context("failed to serialize cache")?;

        let mut temp = tempfile::Builder::new()
            .prefix(".content-hashes-")
            .tempfile_in(dir)
            .context("failed to create a temp file for the cache")?;
        temp.write_all(&json)
            .context("failed to write the cache temp file")?;
        temp.persist(dir.join(FILE_NAME))
            .map_err(|e| e.error)
            .context("failed to move the cache temp file into place")?;
        Ok(())
    }

    /// The pure TTL-prune logic [`Self::persist`] applies, parameterized on `now` so it can
    /// be tested without sleeping — [`Self::persist`] is the only real caller, and always
    /// passes [`Utc::now`].
    fn prune(&self, now: DateTime<Utc>) {
        let cutoff = now - chrono::Duration::days(TTL_DAYS);
        let mut entries = self.entries.lock().unwrap();
        for instance_entries in entries.values_mut() {
            instance_entries.retain(|_, entry| entry.last_seen > cutoff);
        }
        entries.retain(|_, instance_entries| !instance_entries.is_empty());
    }

    /// Whether a cache directory is configured, for the startup log line.
    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// Total entries currently held across every instance, for the startup log line. Named
    /// `entry_count` rather than `len` so that clippy's `len_without_is_empty` doesn't demand
    /// an `is_empty` companion that would have no caller — this is a count for a log line,
    /// not a collection API.
    pub fn entry_count(&self) -> usize {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(HashMap::len)
            .sum()
    }
}

/// Loads `<dir>/content-hashes.json` if present and well-formed; anything else (missing
/// file, unreadable, unparseable, unknown `version`) yields an empty map after a single
/// warning, per [`ContentHashCache::open`]'s doc comment. A v1 file (`{"version":1,"entries":
/// {...}}`) parses as neither a valid `FileFormat` (wrong field name) nor `CURRENT_VERSION`,
/// so it always ends up here as a warned, empty start — free migration, no code for it.
fn load(dir: &Path) -> HashMap<String, HashMap<String, Entry>> {
    let path = dir.join(FILE_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            crate::warn!(
                "cache file {} could not be read, starting empty: {e}",
                path.display()
            );
            return HashMap::new();
        }
    };

    let parsed: FileFormat = match serde_json::from_slice(&bytes) {
        Ok(parsed) => parsed,
        Err(e) => {
            crate::warn!(
                "cache file {} could not be parsed, starting empty: {e}",
                path.display()
            );
            return HashMap::new();
        }
    };

    if parsed.version != CURRENT_VERSION {
        crate::warn!(
            "cache file {} has unsupported version {}, starting empty",
            path.display(),
            parsed.version
        );
        return HashMap::new();
    }

    parsed.instances
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_modified() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 7, 30, 14, 4, 3).unwrap()
    }

    const INSTANCE_A: &str = "https://export-a.example.com/api";
    const INSTANCE_B: &str = "https://export-b.example.com/api";

    #[test]
    fn round_trips_through_open_insert_persist_open() {
        let dir = tempfile::tempdir().unwrap();
        let modified = sample_modified();

        let cache = ContentHashCache::open(dir.path()).unwrap();
        cache.insert(
            INSTANCE_A,
            "hash-a".to_string(),
            modified,
            "content-a".to_string(),
        );
        cache.persist().unwrap();

        let reopened = ContentHashCache::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get(INSTANCE_A, "hash-a", modified),
            Some("content-a".to_string())
        );
        assert_eq!(reopened.entry_count(), 1);
    }

    #[test]
    fn a_stale_modified_timestamp_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();
        cache.insert(
            INSTANCE_A,
            "hash-a".to_string(),
            modified,
            "content-a".to_string(),
        );

        let different = modified + chrono::Duration::seconds(1);
        assert_eq!(cache.get(INSTANCE_A, "hash-a", different), None);
        assert_eq!(
            cache.get(INSTANCE_A, "hash-a", modified),
            Some("content-a".to_string())
        );
    }

    #[test]
    fn two_instances_with_the_same_path_hash_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();

        cache.insert(
            INSTANCE_A,
            "same-hash".to_string(),
            modified,
            "content-a".to_string(),
        );
        cache.insert(
            INSTANCE_B,
            "same-hash".to_string(),
            modified,
            "content-b".to_string(),
        );

        assert_eq!(
            cache.get(INSTANCE_A, "same-hash", modified),
            Some("content-a".to_string())
        );
        assert_eq!(
            cache.get(INSTANCE_B, "same-hash", modified),
            Some("content-b".to_string())
        );
    }

    #[test]
    fn two_jobs_against_the_same_instance_share_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();

        // Job "family" downloads and learns the content hash first...
        cache.insert(
            INSTANCE_A,
            "hash-a".to_string(),
            modified,
            "content-a".to_string(),
        );

        // ...job "hiking", targeting a different album on the same export instance, gets it
        // for free.
        assert_eq!(
            cache.get(INSTANCE_A, "hash-a", modified),
            Some("content-a".to_string())
        );
    }

    #[test]
    fn a_get_hit_refreshes_last_seen_and_survives_pruning() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();
        let now = Utc::now();

        // Plant an entry whose last_seen is already well past the TTL.
        {
            let mut entries = cache.entries.lock().unwrap();
            entries.entry(INSTANCE_A.to_string()).or_default().insert(
                "hash-a".to_string(),
                Entry {
                    modified,
                    content: "content-a".to_string(),
                    last_seen: now - chrono::Duration::days(TTL_DAYS + 10),
                },
            );
        }

        // A hit refreshes last_seen to (real) now.
        assert_eq!(
            cache.get(INSTANCE_A, "hash-a", modified),
            Some("content-a".to_string())
        );

        // Pruning at `now` would have dropped the original last_seen, but not the refreshed
        // one — the refresh happened after `now`, so it's always inside the TTL window.
        cache.prune(now);
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn prune_drops_entries_older_than_the_ttl_and_keeps_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();
        let now = Utc::now();

        cache.insert(INSTANCE_A, "old".to_string(), modified, "c-old".to_string());
        cache.insert(
            INSTANCE_A,
            "fresh".to_string(),
            modified,
            "c-fresh".to_string(),
        );
        {
            let mut entries = cache.entries.lock().unwrap();
            entries
                .get_mut(INSTANCE_A)
                .unwrap()
                .get_mut("old")
                .unwrap()
                .last_seen = now - chrono::Duration::days(TTL_DAYS + 1);
        }

        cache.prune(now);

        assert_eq!(cache.entry_count(), 1);
        assert_eq!(
            cache.get(INSTANCE_A, "fresh", modified),
            Some("c-fresh".to_string())
        );
        assert_eq!(cache.get(INSTANCE_A, "old", modified), None);
    }

    #[test]
    fn pruning_an_instance_to_empty_drops_its_key_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();
        let now = Utc::now();

        cache.insert(
            INSTANCE_A,
            "hash-a".to_string(),
            modified,
            "content-a".to_string(),
        );
        {
            let mut entries = cache.entries.lock().unwrap();
            entries
                .get_mut(INSTANCE_A)
                .unwrap()
                .get_mut("hash-a")
                .unwrap()
                .last_seen = now - chrono::Duration::days(TTL_DAYS + 1);
        }

        cache.prune(now);

        assert_eq!(cache.entry_count(), 0);
        assert!(!cache.entries.lock().unwrap().contains_key(INSTANCE_A));
    }

    #[test]
    fn a_corrupt_cache_file_yields_an_empty_cache_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), b"not json at all").unwrap();

        let cache = ContentHashCache::open(dir.path()).unwrap();
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn an_unknown_version_yields_an_empty_cache_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE_NAME),
            br#"{"version":9999,"instances":{}}"#,
        )
        .unwrap();

        let cache = ContentHashCache::open(dir.path()).unwrap();
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn a_v1_file_loads_as_an_empty_cache_with_no_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE_NAME),
            br#"{"version":1,"entries":{"97NskcQtqUhXp7Pqxa80qkeo0TA=":{"modified":"2025-07-30T14:04:03Z","content":"BZm8Ilo+YBCs4cEz7mB5nV2hyQI="}}}"#,
        )
        .unwrap();

        let cache = ContentHashCache::open(dir.path()).unwrap();
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn disabled_cache_never_persists_anything() {
        let cache = ContentHashCache::disabled();
        assert!(!cache.is_enabled());

        let modified = sample_modified();
        cache.insert(
            INSTANCE_A,
            "hash-a".to_string(),
            modified,
            "content-a".to_string(),
        );
        // In-memory lookups still work for the lifetime of this process...
        assert_eq!(
            cache.get(INSTANCE_A, "hash-a", modified),
            Some("content-a".to_string())
        );
        assert_eq!(cache.entry_count(), 1);
        // ...but persisting is a no-op, since there is nowhere configured to write to.
        assert!(cache.persist().is_ok());
    }
}
