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

/// Resident decoded-index budget for the shared immutable-reader cache.
///
/// A count-only limit retained 256 readers regardless of whether each decoded
/// index was 4 KiB or 40 MiB. The production Ledger walk in #2239 therefore
/// retained over a gigabyte while still appearing "within cap". This is an
/// internal cache working-set budget, not a process memory limit: a caller can
/// open any valid SST, but an oversized reader is not kept after that call.
const MAX_CACHED_READER_HEAP_BYTES: usize = 32 * 1024 * 1024;

/// File-backed mmap span retained by the cache.
///
/// This is separate from decoded heap because mapped files do not appear in
/// allocator accounting yet their faulted pages are charged to process working
/// set. The production cache in #2239 reported only 66 MiB of heap while 50
/// retained 64 MiB mappings drove physical working set above 4 GiB. A cache is
/// useful only when it accounts for every resource it retains.
const MAX_CACHED_READER_MAPPED_BYTES: usize = 32 * 1024 * 1024;

struct Entry {
    reader: Arc<SstReader>,
    len: u64,
    modified: Option<SystemTime>,
    last_used: u64,
    heap_bytes: usize,
    mapped_bytes: usize,
}

#[derive(Default)]
struct ReaderCache {
    entries: HashMap<PathBuf, Entry>,
    retained_heap_bytes: usize,
    retained_mapped_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SstReaderCacheStatus {
    pub entries: usize,
    pub estimated_heap_bytes: usize,
    pub mapped_bytes: usize,
    pub max_entries: usize,
    pub max_estimated_heap_bytes: usize,
    pub max_mapped_bytes: usize,
}

fn cache() -> &'static Mutex<ReaderCache> {
    static CACHE: OnceLock<Mutex<ReaderCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ReaderCache::default()))
}

fn next_tick() -> u64 {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

fn lock() -> MutexGuard<'static, ReaderCache> {
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
        if let Some(entry) = cache.entries.get_mut(&key) {
            if entry.len == len && entry.modified == modified {
                entry.last_used = next_tick();
                return Ok(Arc::clone(&entry.reader));
            }
            remove_entry(&mut cache, &key);
        }
    }

    // The checksum pass stays outside the cache lock so unrelated files never
    // serialize behind a cold open. Concurrent cold opens are both valid
    // because SST files are immutable.
    let reader = Arc::new(SstReader::open(&key)?);
    let heap_bytes = reader.estimated_heap_bytes();
    let mapped_bytes = reader.mapped_bytes();
    let mut cache = lock();
    if heap_bytes <= MAX_CACHED_READER_HEAP_BYTES && mapped_bytes <= MAX_CACHED_READER_MAPPED_BYTES
    {
        let prior = cache.entries.insert(
            key,
            Entry {
                reader: Arc::clone(&reader),
                len,
                modified,
                last_used: next_tick(),
                heap_bytes,
                mapped_bytes,
            },
        );
        if let Some(prior) = prior {
            subtract_retained_bytes(&mut cache, prior.heap_bytes, prior.mapped_bytes);
        }
        cache.retained_heap_bytes = cache.retained_heap_bytes.saturating_add(heap_bytes);
        cache.retained_mapped_bytes = cache.retained_mapped_bytes.saturating_add(mapped_bytes);
    }
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
    remove_entry(&mut cache, canonical);
}

/// Drops the cache-owned mapping before a caller reclaims an SST file.
pub fn invalidate_reader(path: &Path) {
    let mut cache = lock();
    match fs::canonicalize(path) {
        Ok(key) => {
            remove_entry(&mut cache, &key);
        }
        Err(_) => {
            remove_entry(&mut cache, path);
        }
    }
}

pub fn reader_cache_status() -> SstReaderCacheStatus {
    let cache = lock();
    SstReaderCacheStatus {
        entries: cache.entries.len(),
        estimated_heap_bytes: cache.retained_heap_bytes,
        mapped_bytes: cache.retained_mapped_bytes,
        max_entries: MAX_CACHED_READERS,
        max_estimated_heap_bytes: MAX_CACHED_READER_HEAP_BYTES,
        max_mapped_bytes: MAX_CACHED_READER_MAPPED_BYTES,
    }
}

fn remove_entry(cache: &mut ReaderCache, key: &Path) {
    if let Some(entry) = cache.entries.remove(key) {
        subtract_retained_bytes(cache, entry.heap_bytes, entry.mapped_bytes);
    }
}

fn subtract_retained_bytes(cache: &mut ReaderCache, heap_bytes: usize, mapped_bytes: usize) {
    assert!(
        cache.retained_heap_bytes >= heap_bytes && cache.retained_mapped_bytes >= mapped_bytes,
        "CALYX_ASTER_SST_READER_CACHE_ACCOUNTING_UNDERFLOW retained_heap_bytes={} removing_heap_bytes={heap_bytes} retained_mapped_bytes={} removing_mapped_bytes={mapped_bytes}",
        cache.retained_heap_bytes,
        cache.retained_mapped_bytes
    );
    cache.retained_heap_bytes -= heap_bytes;
    cache.retained_mapped_bytes -= mapped_bytes;
}

fn evict_over_cap(cache: &mut ReaderCache) {
    while cache.entries.len() > MAX_CACHED_READERS
        || cache.retained_heap_bytes > MAX_CACHED_READER_HEAP_BYTES
        || cache.retained_mapped_bytes > MAX_CACHED_READER_MAPPED_BYTES
    {
        let victim = cache
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone());
        let Some(victim) = victim else {
            break;
        };
        remove_entry(cache, &victim);
    }
}
