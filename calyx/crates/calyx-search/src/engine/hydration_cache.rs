//! MVCC-snapshot-keyed reuse of hydrated hit documents.
//!
//! A hit document read is fully determined by (vault, cx_id, pinned snapshot
//! seq, hydrated slot selection): MVCC guarantees the same snapshot seq reads
//! identical bytes. Search pins one reader lease and proves index/delta
//! freshness before consulting this cache; only the redundant page readback
//! is skipped. Any vault advance produces a new pinned seq and therefore a
//! fresh read, so no staleness can hide behind this cache.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use calyx_core::{Constellation, CxId};

use crate::error::CliResult;
use crate::persisted::canonical_pin_vault_dir;

const MAX_CACHED_DOCS: usize = 512;

type DocKey = (String, CxId, u64, bool, String);

struct DocCache {
    docs: BTreeMap<DocKey, Arc<Constellation>>,
    order: VecDeque<DocKey>,
}

fn cache() -> &'static Mutex<DocCache> {
    static CACHE: OnceLock<Mutex<DocCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(DocCache {
            docs: BTreeMap::new(),
            order: VecDeque::new(),
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
    Ok(cache.docs.get(&key).cloned())
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
    let mut cache = cache().lock().expect("hydration doc cache poisoned");
    if cache.docs.insert(key.clone(), doc).is_none() {
        cache.order.push_back(key);
    }
    while cache.order.len() > MAX_CACHED_DOCS {
        if let Some(evicted) = cache.order.pop_front() {
            cache.docs.remove(&evicted);
        }
    }
    Ok(())
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
