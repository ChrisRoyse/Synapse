use crate::sst::{self, SstSummary};
use calyx_core::{CalyxError, Result, Seq};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Logical per-row overhead used by memtable admission accounting.
pub const ENTRY_OVERHEAD_BYTES: usize = 4;

/// Backward-compatible name used by existing Aster callers.
pub type Memtable = BoundedMemtable;

/// A successful bounded memtable write acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteAck {
    pub seq: Seq,
    pub accepted_bytes: usize,
    pub used_bytes: usize,
    pub cap_bytes: usize,
    pub high_water_bytes: usize,
    pub flush_triggered: bool,
}

/// Snapshot of one memtable's byte accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemtableUsage {
    pub used_bytes: usize,
    pub cap_bytes: usize,
    pub high_water_bytes: usize,
    pub flush_triggered: bool,
}

/// In-memory ordered table with a hard byte cap and high-water flush signal.
#[derive(Debug)]
pub struct BoundedMemtable {
    entries: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
    cap_bytes: usize,
    high_water_bytes: usize,
    used_bytes: AtomicUsize,
}

/// Immutable handoff created when a mutable memtable rotates.
#[derive(Debug, Clone)]
pub struct FrozenMemtable {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl FrozenMemtable {
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
    }

    pub fn flush_to_sst(&self, path: impl AsRef<Path>) -> Result<SstSummary> {
        sst::write_sst(path, self.iter())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl BoundedMemtable {
    /// Creates an empty memtable with the default 80% high-water threshold.
    pub fn new(cap_bytes: usize) -> Self {
        Self::with_high_water(cap_bytes, high_water_for(cap_bytes))
    }

    /// Creates an empty memtable with an explicit high-water threshold.
    pub fn with_high_water(cap_bytes: usize, high_water_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            cap_bytes,
            high_water_bytes: high_water_bytes.min(cap_bytes),
            used_bytes: AtomicUsize::new(0),
        }
    }

    /// Estimates the bytes charged for one key/value row.
    pub fn entry_size(key: &[u8], value: &[u8]) -> usize {
        key.len()
            .saturating_add(value.len())
            .saturating_add(ENTRY_OVERHEAD_BYTES)
    }

    /// Inserts or replaces one key/value pair, failing closed at the byte cap.
    pub fn write(&self, key: &[u8], value: &[u8], seq: Seq) -> Result<WriteAck> {
        let accepted_bytes = Self::entry_size(key, value);
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| CalyxError::backpressure("memtable lock poisoned"))?;
        let existing = entries
            .get(key)
            .map(|old| Self::entry_size(key, old))
            .unwrap_or(0);
        let current = self.used_bytes.load(Ordering::Acquire);
        let next_bytes = current
            .saturating_sub(existing)
            .saturating_add(accepted_bytes);
        if accepted_bytes > self.cap_bytes || next_bytes > self.cap_bytes {
            return Err(CalyxError::backpressure(format!(
                "memtable byte cap {} exceeded by projected {} bytes",
                self.cap_bytes, next_bytes
            )));
        }

        entries.insert(key.to_vec(), value.to_vec());
        self.used_bytes.store(next_bytes, Ordering::Release);
        Ok(WriteAck {
            seq,
            accepted_bytes,
            used_bytes: next_bytes,
            cap_bytes: self.cap_bytes,
            high_water_bytes: self.high_water_bytes,
            flush_triggered: self.flush_trigger_for(next_bytes),
        })
    }

    /// Compatibility wrapper for existing router call sites.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        self.write(key.as_ref(), value.as_ref(), 0).map(|_| ())
    }

    /// Returns true once the current byte use reaches the high-water threshold.
    pub fn flush_trigger(&self) -> bool {
        self.flush_trigger_for(self.used_bytes())
    }

    /// Decrements the usage counter after bytes are physically flushed.
    pub fn reset_after_flush(&self, flushed_bytes: usize) {
        let _ = self
            .used_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(flushed_bytes))
            });
    }

    /// Returns a value by key.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self.entries.lock() {
            Ok(entries) => entries.get(key).cloned(),
            Err(poisoned) => poisoned.into_inner().get(key).cloned(),
        }
    }

    /// Returns cloned entries in key order.
    pub fn iter(&self) -> std::vec::IntoIter<(Vec<u8>, Vec<u8>)> {
        self.snapshot_entries().into_iter()
    }

    /// Returns cloned entries in `[start, end)` key order without cloning the whole table.
    pub fn range(&self, start: &[u8], end: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.range_until(start, Some(end))
    }

    /// Returns cloned entries in `[start, end)` or `[start, +inf)` key order.
    pub fn range_until(&self, start: &[u8], end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match end {
            Some(end) => entries
                .range(start.to_vec()..end.to_vec())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            None => entries
                .range(start.to_vec()..)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }

    /// Returns at most `limit` raw entries in key order after `after_key`.
    ///
    /// Tombstones are intentionally retained: the caller must merge this
    /// newest mutable layer with older immutable rows before deciding whether
    /// a key is live.
    pub(crate) fn range_candidate_page_until(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        if limit == 0 {
            return Vec::new();
        }
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lower = after_key
            .map(|key| Bound::Excluded(key.to_vec()))
            .unwrap_or_else(|| Bound::Included(start.to_vec()));
        let upper = end
            .map(|key| Bound::Excluded(key.to_vec()))
            .unwrap_or(Bound::Unbounded);
        entries
            .range((lower, upper))
            .take(limit)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Returns the greatest entry in `[start, upper]` (or `[start, upper)`).
    pub(crate) fn predecessor(
        &self,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let upper = if inclusive {
            Bound::Included(upper.to_vec())
        } else {
            Bound::Excluded(upper.to_vec())
        };
        entries
            .range((Bound::Included(start.to_vec()), upper))
            .next_back()
            .map(|(key, value)| (key.clone(), value.clone()))
    }

    /// Flushes the current memtable snapshot into an immutable SSTable.
    pub fn flush_to_sst(&self, path: impl AsRef<Path>) -> Result<SstSummary> {
        let entries = self.snapshot_entries();
        sst::write_sst(
            path,
            entries
                .iter()
                .map(|(key, value)| (key.as_slice(), value.as_slice())),
        )
    }

    pub fn freeze(self) -> FrozenMemtable {
        let entries = self
            .entries
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        FrozenMemtable { entries }
    }

    pub fn needs_flush(&self) -> bool {
        self.flush_trigger()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Acquire)
    }

    pub fn estimated_bytes(&self) -> usize {
        self.used_bytes()
    }

    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    pub fn byte_cap(&self) -> usize {
        self.cap_bytes()
    }

    pub fn high_water_bytes(&self) -> usize {
        self.high_water_bytes
    }

    pub fn usage(&self) -> MemtableUsage {
        let used_bytes = self.used_bytes();
        MemtableUsage {
            used_bytes,
            cap_bytes: self.cap_bytes,
            high_water_bytes: self.high_water_bytes,
            flush_triggered: self.flush_trigger_for(used_bytes),
        }
    }

    fn snapshot_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn flush_trigger_for(&self, used_bytes: usize) -> bool {
        self.cap_bytes == 0 || used_bytes >= self.high_water_bytes
    }
}

fn high_water_for(cap_bytes: usize) -> usize {
    cap_bytes.saturating_mul(4) / 5
}
