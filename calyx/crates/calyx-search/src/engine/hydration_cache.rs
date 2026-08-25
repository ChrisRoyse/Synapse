//! MVCC-snapshot-keyed reuse of hydrated hit documents.
//!
//! A hit document read is fully determined by (vault, cx_id, pinned snapshot
//! seq, hydrated slot selection): MVCC guarantees the same snapshot seq reads
//! identical bytes. Search pins one reader lease and proves index/delta
//! freshness before consulting this cache; only the redundant page readback
//! is skipped. Any vault advance produces a new pinned seq and therefore a
//! fresh read, so no staleness can hide behind this cache.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use calyx_core::{Constellation, CxId};

use crate::error::CliResult;
use crate::persisted::canonical_pin_vault_dir;

const MAX_CACHED_DOCS: usize = 128;
const MAX_CACHED_DOC_SERIALIZED_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHED_DOCS_SERIALIZED_BYTES: usize = 16 * 1024 * 1024;

type DocKey = (String, CxId, u64, bool, String);

struct DocCache {
    docs: BTreeMap<DocKey, CachedDoc>,
    order: VecDeque<DocKey>,
    retained_serialized_bytes: usize,
}

struct CachedDoc {
    doc: Arc<Constellation>,
    serialized_bytes: usize,
}

fn cache() -> &'static Mutex<DocCache> {
    static CACHE: OnceLock<Mutex<DocCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(DocCache {
            docs: BTreeMap::new(),
            order: VecDeque::new(),
            retained_serialized_bytes: 0,
        })
    })
}

pub(super) fn cached_doc(
    vault_dir: &Path,
    cx_id: CxId,
    snapshot_seq: u64,
    hydrate_slots: bool,
    slots_key: &str,
) -> CliResult<Option<Arc<Constellation>>> {
    let key = doc_key(vault_dir, cx_id, snapshot_seq, hydrate_slots, slots_key)?;
    let cache = cache().lock().expect("hydration doc cache poisoned");
    Ok(cache.docs.get(&key).map(|entry| Arc::clone(&entry.doc)))
}

pub(super) fn store_doc(
    vault_dir: &Path,
    cx_id: CxId,
    snapshot_seq: u64,
    hydrate_slots: bool,
    slots_key: &str,
    doc: Arc<Constellation>,
) -> CliResult {
    let key = doc_key(vault_dir, cx_id, snapshot_seq, hydrate_slots, slots_key)?;
    let serialized_bytes = serialized_size(doc.as_ref())?;
    if serialized_bytes > MAX_CACHED_DOC_SERIALIZED_BYTES {
        return Ok(());
    }
    let mut cache = cache().lock().expect("hydration doc cache poisoned");
    let prior = cache.docs.insert(
        key.clone(),
        CachedDoc {
            doc,
            serialized_bytes,
        },
    );
    if let Some(prior) = prior {
        cache.retained_serialized_bytes = cache
            .retained_serialized_bytes
            .saturating_sub(prior.serialized_bytes);
    } else {
        cache.order.push_back(key);
    }
    cache.retained_serialized_bytes = cache
        .retained_serialized_bytes
        .saturating_add(serialized_bytes);
    while cache.order.len() > MAX_CACHED_DOCS
        || cache.retained_serialized_bytes > MAX_CACHED_DOCS_SERIALIZED_BYTES
    {
        if let Some(evicted) = cache.order.pop_front()
            && let Some(evicted) = cache.docs.remove(&evicted)
        {
            cache.retained_serialized_bytes = cache
                .retained_serialized_bytes
                .saturating_sub(evicted.serialized_bytes);
        }
    }
    Ok(())
}

fn serialized_size(doc: &Constellation) -> CliResult<usize> {
    let mut counter = SerializedSizeCounter::default();
    serde_json::to_writer(&mut counter, doc).map_err(|error| {
        crate::error::SearchError::io(format!(
            "CALYX_SEARCH_HYDRATION_CACHE_SIZE_FAILED: could not measure constellation {0} before cache admission: {error}; remediation=inspect the constellation serializer and preserve uncached reads until it is repaired",
            doc.cx_id
        ))
    })?;
    Ok(counter.bytes)
}

#[derive(Default)]
struct SerializedSizeCounter {
    bytes: usize,
}

impl Write for SerializedSizeCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::other("serialized constellation size overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn doc_key(
    vault_dir: &Path,
    cx_id: CxId,
    snapshot_seq: u64,
    hydrate_slots: bool,
    slots_key: &str,
) -> CliResult<DocKey> {
    Ok((
        canonical_pin_vault_dir(vault_dir)?,
        cx_id,
        snapshot_seq,
        hydrate_slots,
        slots_key.to_string(),
    ))
}
