//! The path-hash → content-hash cache (see `scratch/CACHE-DESIGN.md`).
//!
//! Immich external libraries hash by *path*, not by content, so the checksum the export
//! instance reports for such an asset can never be checked against the bytes we download,
//! and can never match anything on the import instance (which always content-hashes on
//! upload). We learn the real content hash the first time we download such an asset and
//! remember it here, keyed by the path hash, so later runs can skip the download entirely
//! when nothing has changed.
//!
//! The cache is a fact about the *export* instance — "the file whose path hash is X
//! currently hashes to Y" — not a record of anything this program did. Losing it can only
//! ever cost work (a re-download), never correctness, which is what allows every failure
//! mode below (missing file, corrupt file, unknown format) to be a warning rather than
//! fatal, and is what makes an unconfigured `CACHE_DIR` a supported, silent no-op rather
//! than a degraded mode callers need to branch on.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// File name inside `CACHE_DIR`; see `scratch/CACHE-DESIGN.md`'s "File format" section for
/// the exact JSON shape this (de)serializes.
const FILE_NAME: &str = "content-hashes.json";

/// Bumped only if the on-disk shape ever changes incompatibly. An older or newer reader
/// disagreeing with this number is treated exactly like a corrupt file (see [`load`]) —
/// there is deliberately no migration path, since the cache is disposable.
const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    modified: DateTime<Utc>,
    content: String,
}

/// The on-disk shape, one file's worth. Kept as its own type (rather than serializing
/// `ContentHashCache` directly) because the live cache also tracks `dir`, which has no
/// business in the file.
#[derive(Debug, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    entries: HashMap<String, Entry>,
}

/// An in-memory map of path hash → (the `modified` timestamp it was learned at, the
/// content hash), optionally backed by a JSON file. `get`/`insert`/`persist` all take
/// `&self`: `sync.rs` shares one instance across concurrent transfer futures behind an
/// `Arc`, so the map lives behind a plain [`std::sync::Mutex`] rather than requiring
/// exclusive access. The lock is only ever held for a map operation or a `clone`, never
/// across an await point — this type has no async methods at all, which is what makes that
/// guarantee free instead of something callers have to be careful about.
pub struct ContentHashCache {
    dir: Option<PathBuf>,
    entries: Mutex<HashMap<String, Entry>>,
}

impl ContentHashCache {
    /// Disabled — every lookup misses, every insert is kept in memory for this process
    /// only, and [`persist`](Self::persist) does nothing. This is what an unset
    /// `CACHE_DIR` produces: a fresh `cargo run` must never fail because of an unwritable
    /// default, so "no cache configured" has to be representable as a value rather than an
    /// error.
    pub fn disabled() -> Self {
        Self {
            dir: None,
            entries: Mutex::new(HashMap::new()),
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
        })
    }

    /// The cached content hash for this path hash, if one was recorded at exactly this
    /// `modified` timestamp. A path hash never changes when the underlying file changes
    /// (that's the whole problem this cache exists to work around), so `modified` is the
    /// only signal that the file has moved on since we learned this entry; a differing
    /// timestamp is treated as a miss rather than a stale hit.
    pub fn get(&self, path_checksum: &str, modified: DateTime<Utc>) -> Option<String> {
        let entries = self.entries.lock().unwrap();
        entries
            .get(path_checksum)
            .filter(|entry| entry.modified == modified)
            .map(|entry| entry.content.clone())
    }

    /// Records a content hash learned by downloading, overwriting any existing entry for
    /// this path hash.
    pub fn insert(&self, path_checksum: String, modified: DateTime<Utc>, content_checksum: String) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            path_checksum,
            Entry {
                modified,
                content: content_checksum,
            },
        );
    }

    /// Writes the cache to disk, pruned to only the keys in `keep` — the path hashes seen
    /// this run. Pruning here (rather than never removing anything) is safe precisely
    /// because the cache is disposable: an asset that leaves the source album just stops
    /// being looked up, so keeping its entry around would only be dead weight, never a
    /// correctness gain. A no-op when disabled. Errors are returned, not logged, so the
    /// caller decides how loud a failed save should be.
    ///
    /// The write itself is temp-file-then-`rename` for atomicity: a crash or a concurrent
    /// reader never observes a half-written file.
    pub fn persist(&self, keep: &HashSet<String>) -> Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };

        let mut entries = self.entries.lock().unwrap();
        entries.retain(|path_checksum, _| keep.contains(path_checksum));
        let file_format = FileFormat {
            version: CURRENT_VERSION,
            entries: entries.clone(),
        };
        drop(entries);

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

    /// Whether a cache directory is configured, for the startup log line.
    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// Number of entries currently held, for the startup log line. Named `entry_count`
    /// rather than `len` so that clippy's `len_without_is_empty` doesn't demand an
    /// `is_empty` companion that would have no caller — this is a count for a log line, not
    /// a collection API.
    pub fn entry_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

/// Loads `<dir>/content-hashes.json` if present and well-formed; anything else (missing
/// file, unreadable, unparseable, unknown `version`) yields an empty map after a single
/// warning, per [`ContentHashCache::open`]'s doc comment.
fn load(dir: &Path) -> HashMap<String, Entry> {
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

    parsed.entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_modified() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 7, 30, 14, 4, 3).unwrap()
    }

    fn keep_set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|k| (*k).to_string()).collect()
    }

    #[test]
    fn round_trips_through_open_insert_persist_open() {
        let dir = tempfile::tempdir().unwrap();
        let modified = sample_modified();

        let cache = ContentHashCache::open(dir.path()).unwrap();
        cache.insert("hash-a".to_string(), modified, "content-a".to_string());
        cache.persist(&keep_set(&["hash-a"])).unwrap();

        let reopened = ContentHashCache::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get("hash-a", modified),
            Some("content-a".to_string())
        );
        assert_eq!(reopened.entry_count(), 1);
    }

    #[test]
    fn a_stale_modified_timestamp_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentHashCache::open(dir.path()).unwrap();
        let modified = sample_modified();
        cache.insert("hash-a".to_string(), modified, "content-a".to_string());

        let different = modified + chrono::Duration::seconds(1);
        assert_eq!(cache.get("hash-a", different), None);
        assert_eq!(cache.get("hash-a", modified), Some("content-a".to_string()));
    }

    #[test]
    fn persist_prunes_keys_not_in_the_keep_set() {
        let dir = tempfile::tempdir().unwrap();
        let modified = sample_modified();

        let cache = ContentHashCache::open(dir.path()).unwrap();
        cache.insert("keep-me".to_string(), modified, "c1".to_string());
        cache.insert("drop-me".to_string(), modified, "c2".to_string());
        cache.persist(&keep_set(&["keep-me"])).unwrap();
        assert_eq!(cache.entry_count(), 1);

        let reopened = ContentHashCache::open(dir.path()).unwrap();
        assert_eq!(reopened.get("keep-me", modified), Some("c1".to_string()));
        assert_eq!(reopened.get("drop-me", modified), None);
        assert_eq!(reopened.entry_count(), 1);
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
            br#"{"version":9999,"entries":{}}"#,
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
        cache.insert("hash-a".to_string(), modified, "content-a".to_string());
        // In-memory lookups still work for the lifetime of this process...
        assert_eq!(cache.get("hash-a", modified), Some("content-a".to_string()));
        assert_eq!(cache.entry_count(), 1);
        // ...but persisting is a no-op, since there is nowhere configured to write to.
        assert!(cache.persist(&keep_set(&["hash-a"])).is_ok());
    }
}
