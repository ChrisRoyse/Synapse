//! Process-wide bounded shared `SstReader` cache — the LSM table cache.
//!
//! `SstReader::open` verifies the whole SST body checksum. Repeating that work
//! for every point/range read becomes dominant once compaction produces a
//! large immutable SST. Cached readers are keyed by canonical path and
//! revalidated by length and modification time before reuse.
//!
//! On Windows an active memory mapping prevents deletion. Every physical SST
//! reclaim path must therefore call [`invalidate_reader`] immediately before
//! `remove_file`; deletion remains fail-closed while an in-flight read owns a
//! clone.

use super::SstReader;
use calyx_core::{CalyxError, Result};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

/// Bounds open mappings, file descriptors, decoded indexes, and bloom filters.
const MAX_CACHED_READERS: usize = 256;

struct Entry {
    reader: Arc<SstReader>,
    len: u64,
    modified: Option<SystemTime>,
    last_used: u64,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_tick() -> u64 {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

fn lock() -> MutexGuard<'static, HashMap<PathBuf, Entry>> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn canonical_key(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| metadata_error("canonicalize SST", path, error))
}

fn metadata_error(context: &str, path: &Path, error: io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!("{context} {}: {error}", path.display()))
}

/// Returns a checksum-verified shared reader for the current file identity.
pub fn shared_reader(path: &Path) -> Result<Arc<SstReader>> {
    let key = canonical_key(path)?;
    let metadata = fs::metadata(&key).map_err(|error| metadata_error("stat SST", &key, error))?;
    let len = metadata.len();
    let modified = metadata.modified().ok();

    {
        let mut cache = lock();
        if let Some(entry) = cache.get_mut(&key) {
            if entry.len == len && entry.modified == modified {
                entry.last_used = next_tick();
                return Ok(Arc::clone(&entry.reader));
            }
            cache.remove(&key);
        }
    }

    // The checksum pass stays outside the cache lock so unrelated files never
    // serialize behind a cold open. Concurrent cold opens are both valid
    // because SST files are immutable.
    let reader = Arc::new(SstReader::open(&key)?);
    let mut cache = lock();
    cache.insert(
        key,
        Entry {
            reader: Arc::clone(&reader),
            len,
            modified,
            last_used: next_tick(),
        },
    );
    evict_over_cap(&mut cache);
    Ok(reader)
}

/// Drops the cache-owned mapping for an already-canonical key.
///
/// [`invalidate_reader`] canonicalizes, which opens the file. Reclaim paths
/// that must drop mappings while holding the exclusive router lock resolve
/// their canonical keys *before* taking that lock and call this instead, so
/// no filesystem syscall runs inside the lock (issue #1806).
pub fn invalidate_reader_canonical(canonical: &Path) {
    let mut cache = lock();
    cache.remove(canonical);
}

/// Drops the cache-owned mapping before a caller reclaims an SST file.
pub fn invalidate_reader(path: &Path) {
    let mut cache = lock();
    match fs::canonicalize(path) {
        Ok(key) => {
            cache.remove(&key);
        }
        Err(_) => {
            cache.remove(path);
        }
    }
}

fn evict_over_cap(cache: &mut HashMap<PathBuf, Entry>) {
    while cache.len() > MAX_CACHED_READERS {
        let victim = cache
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone());
        let Some(victim) = victim else {
            break;
        };
        cache.remove(&victim);
    }
}
