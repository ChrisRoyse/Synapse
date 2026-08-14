use super::level::SstLevel;
use super::{MAX_INTERSECTING_SST_PAGE_SOURCES, SstEntry, SstPageReader, materialized_entry_bytes};
use crate::mvcc::is_tombstone_value;
use calyx_core::{CalyxError, Result};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One newest-wins row together with the physical source that supplied it.
///
/// Router memtables and MVCC overlays are plaintext while immutable SST
/// values may be sealed.  Keeping this bit beside the winner lets the router
/// open only immutable values instead of trying to decrypt a plaintext
/// overlay winner.
pub(crate) struct SstPageWinner {
    pub(crate) entry: SstEntry,
    pub(crate) from_overlay: bool,
}

/// Hard ceiling on immutable SST sources participating in one candidate page.
///
/// A flat newest-wins level must inspect one lower-bound key per intersecting
/// file. Failing above this ceiling keeps cursor initialization bounded and
/// makes compaction debt explicit instead of silently expanding read latency.
pub(super) fn range_page(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    after_key: Option<&[u8]>,
    limit: usize,
    overlay: Vec<SstEntry>,
) -> Result<Vec<SstEntry>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut cursor = open_page_cursor(level, start, end, after_key, overlay, None)?;
    next_page(&mut cursor, limit)
}

/// Returns at most `limit` newest raw key states, retaining tombstones.
///
/// This is the bounded building block for higher-layer latest-view merges.
/// Counting candidates rather than only live rows prevents a tombstone-dense
/// range from turning one nominal page into an unbounded logical-key
/// traversal. Immutable source fan-in has a separate hard ceiling.
pub(super) fn range_candidate_page(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    after_key: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<SstEntry>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut cursor = open_page_cursor(
        level,
        start,
        end,
        after_key,
        Vec::new(),
        Some(MAX_INTERSECTING_SST_PAGE_SOURCES),
    )?;
    let mut rows = Vec::with_capacity(limit);
    while rows.len() < limit {
        let Some(winner) = next_latest_entry(&mut cursor)? else {
            break;
        };
        rows.push(winner.entry);
    }
    Ok(rows)
}

/// Streams newest-wins pages while preserving whether each row came from the
/// caller's overlay or an immutable SST.
///
/// Pages are candidate-bounded and retain tombstones. The router must open an
/// immutable value before it can identify an encrypted tombstone, then filters
/// it. Invoking the router once per candidate page also gives the MVCC layer a
/// bounded place to re-check its reader lease even through a tombstone-only
/// range. The origin is intentionally not a public storage concept; it exists
/// only at the encryption boundary in the CF router.
pub(super) fn open_page_stream_with_overlay_origins(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    after_key: Option<&[u8]>,
    limit: usize,
    overlay: Vec<SstEntry>,
) -> Result<SstPageStream> {
    Ok(SstPageStream {
        cursor: open_page_cursor(level, start, end, after_key, overlay, None)?,
        limit,
    })
}

/// Opens a newest-wins stream that retains only key plus tombstone state.
///
/// Unlike the value page stream, this cursor validates every physical record
/// as it enters the merge heap, including older duplicate versions that cannot
/// win. That makes each SST data section a forward-only integrity-checked read
/// and prevents sparse winning-value reads from multiplying physical I/O.
pub(super) fn open_key_state_page_stream_with_overlay(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    limit: usize,
    overlay: Vec<SstEntry>,
) -> Result<SstKeyStatePageStream> {
    Ok(SstKeyStatePageStream {
        cursor: open_key_state_cursor(level, start, end, overlay)?,
        limit,
    })
}

/// Opens a snapshot value stream that reads every source forward only.
///
/// The ordinary page cursor delays value reads until it knows the winner,
/// which is ideal for a short random range but pathological for a whole-CF
/// merge with many overlapping SSTs: winning offsets become sparse point
/// reads. This stream validates and retains one current raw value per source,
/// so every source data section advances sequentially while newest-wins still
/// decides which values cross the router's decryption boundary.
pub(super) fn open_sequential_page_stream_with_overlay_origins(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    limit: usize,
    overlay: Vec<SstEntry>,
) -> Result<SstSequentialPageStream> {
    Ok(SstSequentialPageStream {
        cursor: open_sequential_page_cursor(level, start, end, overlay)?,
        limit,
    })
}

/// Opens the allocation-reusing form of the sequential newest-wins stream.
///
/// [`SstSequentialPageStream`] must transfer owned values into output pages.
/// This stream instead lends one winning row to a synchronous callback, then
/// returns both key and value allocations to that row's physical source before
/// advancing. Multiple instances can be held concurrently to merge two CFs
/// without materializing either corpus.
pub(super) fn open_sequential_row_stream_with_overlay_origins(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    overlay: Vec<SstEntry>,
) -> Result<SstSequentialRowStream> {
    Ok(SstSequentialRowStream {
        cursor: open_sequential_page_cursor(level, start, end, overlay)?,
    })
}

/// Visits a newest-wins snapshot while reusing one value buffer per source.
///
/// Returning pages transfers every winning `Vec` to the caller, forcing the
/// next row from that source to allocate a replacement before merge ordering
/// can continue. A whole-CF verifier consumes each winner synchronously, so it
/// can return that same allocation to its source immediately. The callback may
/// mutate the value in place (for authenticated decryption) but must not mutate
/// the key. Returning `false` stops the scan after the accepted row.
pub(super) fn visit_sequential_with_overlay_origins(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    overlay: Vec<SstEntry>,
    mut visit: impl FnMut(&[u8], &mut Vec<u8>, bool) -> Result<bool>,
) -> Result<()> {
    let mut stream = open_sequential_row_stream_with_overlay_origins(level, start, end, overlay)?;
    let mut keep_scanning = true;
    while keep_scanning
        && stream.next_with(|key, value, from_overlay| {
            keep_scanning = visit(key, value, from_overlay)?;
            Ok(())
        })?
    {}
    Ok(())
}

/// Owning, allocation-reusing sequential row cursor.
pub(crate) struct SstSequentialRowStream {
    cursor: SequentialPageCursor,
}

impl SstSequentialRowStream {
    /// Lends the next newest-wins row to `visit` and returns `false` at EOF.
    /// The callback must not retain the slices beyond the call.
    pub(crate) fn next_with(
        &mut self,
        visit: impl FnOnce(&[u8], &mut Vec<u8>, bool) -> Result<()>,
    ) -> Result<bool> {
        let Some(first) = self.cursor.pop() else {
            return Ok(false);
        };
        let winner_source = first.source;
        let from_overlay = matches!(
            &self.cursor.sources[winner_source],
            PageSource::Overlay { .. }
        );
        let mut entry = first.entry;
        visit(&entry.key, &mut entry.value, from_overlay)?;
        let next_key = entry.key.as_slice();
        while self
            .cursor
            .heap
            .peek()
            .is_some_and(|item| item.entry.key.as_slice() == next_key)
        {
            let mut duplicate = self
                .cursor
                .pop()
                .expect("peek confirmed duplicate sequential heap item");
            self.cursor.sources[duplicate.source].advance_past(next_key)?;
            self.cursor
                .push_current_reusing(duplicate.source, &mut duplicate.entry)?;
        }
        self.cursor.sources[winner_source].advance_past(next_key)?;
        self.cursor
            .push_current_reusing(winner_source, &mut entry)?;
        Ok(true)
    }
}

/// An owning set of already-open immutable file handles plus its bounded
/// newest-wins merge state.
///
/// Creation happens while the router shard is pinned. Once constructed, every
/// file handle and the Arc-backed lookup/bounds metadata outlive a concurrent
/// level retirement, so the router lock can be released before callbacks run.
pub(crate) struct SstPageStream {
    cursor: PageCursor,
    limit: usize,
}

impl SstPageStream {
    pub(crate) fn next_page(&mut self) -> Result<Option<Vec<SstPageWinner>>> {
        let mut page = Vec::with_capacity(self.limit);
        while page.len() < self.limit {
            let Some(winner) = next_latest_entry(&mut self.cursor)? else {
                break;
            };
            page.push(winner);
        }
        Ok((!page.is_empty()).then_some(page))
    }
}

/// One visible raw key state after newest-wins merging.
pub(crate) struct SstKeyState {
    pub(crate) key: Vec<u8>,
    pub(crate) is_tombstone: bool,
}

/// Allocation-bounded owning stream for exact key cardinality reads.
pub(crate) struct SstKeyStatePageStream {
    cursor: KeyStateCursor,
    limit: usize,
}

impl SstKeyStatePageStream {
    pub(crate) fn next_page(&mut self) -> Result<Option<Vec<SstKeyState>>> {
        let mut page = Vec::with_capacity(self.limit);
        while page.len() < self.limit {
            let Some(state) = next_latest_key_state(&mut self.cursor)? else {
                break;
            };
            page.push(state);
        }
        Ok((!page.is_empty()).then_some(page))
    }
}

/// Memory ceiling for the one-current-value-per-source merge and each output
/// page. Exceeding it is explicit compaction/data-shape debt, never permission
/// to materialize until the daemon exhausts host memory.
const SEQUENTIAL_SNAPSHOT_PAGE_MAX_BYTES: usize = 64 << 20;

pub(crate) struct SstSequentialPageStream {
    cursor: SequentialPageCursor,
    limit: usize,
}

impl SstSequentialPageStream {
    pub(crate) fn next_page(&mut self) -> Result<Option<Vec<SstPageWinner>>> {
        let mut page = Vec::with_capacity(self.limit);
        let mut page_bytes = 0_usize;
        while page.len() < self.limit {
            let Some(winner) = next_latest_sequential_entry(&mut self.cursor)? else {
                break;
            };
            let row_bytes =
                materialized_entry_bytes::<SstEntry>(&winner.entry.key, &winner.entry.value);
            page_bytes = page_bytes.saturating_add(row_bytes);
            if page_bytes > SEQUENTIAL_SNAPSHOT_PAGE_MAX_BYTES {
                return Err(sequential_snapshot_memory_budget(
                    "output page",
                    page_bytes,
                    row_bytes,
                ));
            }
            page.push(winner);
        }
        Ok((!page.is_empty()).then_some(page))
    }
}

struct PageCursor {
    sources: Vec<PageSource>,
    heap: BinaryHeap<HeapItem>,
    end: Option<Vec<u8>>,
}

enum PageSource {
    Overlay { rows: Vec<SstEntry>, pos: usize },
    Sst { reader: Box<SstPageReader> },
}

struct KeyStateCursor {
    sources: Vec<KeyStateSource>,
    heap: BinaryHeap<KeyStateHeapItem>,
    end: Option<Vec<u8>>,
}

struct SequentialPageCursor {
    sources: Vec<PageSource>,
    heap: BinaryHeap<SequentialHeapItem>,
    heap_bytes: usize,
    end: Option<Vec<u8>>,
}

enum KeyStateSource {
    Overlay { rows: Vec<SstEntry>, pos: usize },
    Sst { reader: Box<SstPageReader> },
}

#[derive(Debug)]
struct KeyStateHeapItem {
    key: Vec<u8>,
    source: usize,
    is_tombstone: bool,
}

#[derive(Debug)]
struct SequentialHeapItem {
    entry: SstEntry,
    source: usize,
}

impl PartialEq for SequentialHeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.entry.key == other.entry.key && self.source == other.source
    }
}

impl Eq for SequentialHeapItem {}

impl Ord for SequentialHeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .entry
            .key
            .cmp(&self.entry.key)
            .then_with(|| other.source.cmp(&self.source))
    }
}

impl PartialOrd for SequentialHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for KeyStateHeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.source == other.source
    }
}

impl Eq for KeyStateHeapItem {}

impl Ord for KeyStateHeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.source.cmp(&self.source))
    }
}

impl PartialOrd for KeyStateHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeapItem {
    key: Vec<u8>,
    source: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.source.cmp(&self.source))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PageSource {
    fn current_key(&self, end: Option<&[u8]>) -> Option<&[u8]> {
        let key = match self {
            Self::Overlay { rows, pos } => rows.get(*pos).map(|row| row.key.as_slice()),
            Self::Sst { reader } => reader.current_key(),
        }?;
        if end.is_some_and(|end| key >= end) {
            None
        } else {
            Some(key)
        }
    }

    fn read_current(&mut self) -> Result<SstEntry> {
        match self {
            Self::Overlay { rows, pos } => Ok(rows[*pos].clone()),
            Self::Sst { reader } => reader.read_current(),
        }
    }

    fn read_current_into(&mut self, entry: &mut SstEntry) -> Result<()> {
        match self {
            Self::Overlay { rows, pos } => {
                let row = rows.get(*pos).ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(
                        "sequential overlay cursor is exhausted".to_owned(),
                    )
                })?;
                entry.key.clone_from(&row.key);
                entry.value.clone_from(&row.value);
                Ok(())
            }
            Self::Sst { reader } => reader.read_current_into(entry),
        }
    }

    fn advance_past(&mut self, key: &[u8]) -> Result<()> {
        match self {
            Self::Overlay { rows, pos } => {
                while rows.get(*pos).is_some_and(|row| row.key.as_slice() == key) {
                    *pos += 1;
                }
                Ok(())
            }
            Self::Sst { reader } => reader.advance_past(key),
        }
    }
}

impl KeyStateSource {
    fn current_key(&self, end: Option<&[u8]>) -> Option<&[u8]> {
        let key = match self {
            Self::Overlay { rows, pos } => rows.get(*pos).map(|row| row.key.as_slice()),
            Self::Sst { reader } => reader.current_key(),
        }?;
        if end.is_some_and(|end| key >= end) {
            None
        } else {
            Some(key)
        }
    }

    fn read_current_tombstone_state(&mut self) -> Result<bool> {
        match self {
            Self::Overlay { rows, pos } => rows.get(*pos).map_or_else(
                || {
                    Err(CalyxError::aster_corrupt_shard(
                        "key-state overlay cursor is exhausted".to_owned(),
                    ))
                },
                |row| Ok(is_tombstone_value(&row.value)),
            ),
            Self::Sst { reader } => reader.read_current_tombstone_state(),
        }
    }

    fn advance_past(&mut self, key: &[u8]) -> Result<()> {
        match self {
            Self::Overlay { rows, pos } => {
                while rows.get(*pos).is_some_and(|row| row.key.as_slice() == key) {
                    *pos += 1;
                }
                Ok(())
            }
            Self::Sst { reader } => reader.advance_past(key),
        }
    }
}

fn open_page_cursor(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    after_key: Option<&[u8]>,
    overlay: Vec<SstEntry>,
    max_intersecting_sst_sources: Option<usize>,
) -> Result<PageCursor> {
    let lower = after_key.unwrap_or(start);
    let exclusive = after_key.is_some();
    if let Some(max_sources) = max_intersecting_sst_sources {
        let intersecting_sources = level
            .files
            .iter()
            .filter(|file| file.may_intersect(lower, end))
            .count();
        if intersecting_sources > max_sources {
            tracing::error!(
                code = "CALYX_ASTER_SST_PAGE_SOURCE_LIMIT_EXCEEDED",
                intersecting_sources,
                max_sources,
                "candidate-bounded SST page rejected excessive immutable source fan-in"
            );
            return Err(CalyxError {
                code: "CALYX_ASTER_SST_PAGE_SOURCE_LIMIT_EXCEEDED",
                message: format!(
                    "candidate-bounded SST page intersects {intersecting_sources} immutable sources, above the hard maximum {max_sources}"
                ),
                remediation: "compact the affected column family below the candidate-page source ceiling, verify the retained lookup state, and retry; do not fall back to a whole-file scan",
            });
        }
    }
    let mut sources = Vec::new();
    let overlay = overlay_page_rows(overlay, start, end, lower, exclusive);
    if !overlay.is_empty() {
        sources.push(PageSource::Overlay {
            rows: overlay,
            pos: 0,
        });
    }
    let file_sources = level
        .files
        .par_iter()
        .filter(|file| file.may_intersect(lower, end))
        .map(|file| {
            let Some(mut reader) = file.open_page_reader()? else {
                return Ok(None);
            };
            reader.seek_lower_bound(lower, exclusive)?;
            while reader.current_key().is_some_and(|key| key < start) {
                reader.advance_one()?;
            }
            if reader
                .current_key()
                .is_some_and(|key| end.is_none_or(|end| key < end))
            {
                Ok(Some(PageSource::Sst {
                    reader: Box::new(reader),
                }))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    sources.extend(file_sources.into_iter().flatten());
    let mut cursor = PageCursor {
        sources,
        heap: BinaryHeap::new(),
        end: end.map(|end| end.to_vec()),
    };
    for source in 0..cursor.sources.len() {
        cursor.push_current(source);
    }
    Ok(cursor)
}

fn open_key_state_cursor(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    overlay: Vec<SstEntry>,
) -> Result<KeyStateCursor> {
    let intersecting_sources = level
        .files
        .iter()
        .filter(|file| file.may_intersect(start, end))
        .count();
    if intersecting_sources > MAX_INTERSECTING_SST_PAGE_SOURCES {
        tracing::error!(
            code = "CALYX_ASTER_SST_KEY_STATE_SOURCE_LIMIT_EXCEEDED",
            intersecting_sources,
            max_sources = MAX_INTERSECTING_SST_PAGE_SOURCES,
            "key-state stream rejected excessive immutable source fan-in"
        );
        return Err(CalyxError {
            code: "CALYX_ASTER_SST_KEY_STATE_SOURCE_LIMIT_EXCEEDED",
            message: format!(
                "key-state stream intersects {intersecting_sources} immutable sources, above the hard maximum {MAX_INTERSECTING_SST_PAGE_SOURCES}"
            ),
            remediation: "compact the affected column family below the key-state source ceiling, verify the retained lookup state, and retry; exact counts fail closed rather than opening unbounded cursor state",
        });
    }

    let mut sources = Vec::new();
    let overlay = overlay_page_rows(overlay, start, end, start, false);
    if !overlay.is_empty() {
        sources.push(KeyStateSource::Overlay {
            rows: overlay,
            pos: 0,
        });
    }
    let file_sources = level
        .files
        .par_iter()
        .filter(|file| file.may_intersect(start, end))
        .map(|file| {
            let Some(mut reader) = file.open_key_state_reader()? else {
                return Ok(None);
            };
            reader.seek_lower_bound(start, false)?;
            if reader
                .current_key()
                .is_some_and(|key| end.is_none_or(|end| key < end))
            {
                Ok(Some(KeyStateSource::Sst {
                    reader: Box::new(reader),
                }))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    sources.extend(file_sources.into_iter().flatten());

    let mut cursor = KeyStateCursor {
        sources,
        heap: BinaryHeap::new(),
        end: end.map(<[u8]>::to_vec),
    };
    for source in 0..cursor.sources.len() {
        cursor.push_current(source)?;
    }
    Ok(cursor)
}

fn open_sequential_page_cursor(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    overlay: Vec<SstEntry>,
) -> Result<SequentialPageCursor> {
    let intersecting_sources = level
        .files
        .iter()
        .filter(|file| file.may_intersect(start, end))
        .count();
    if intersecting_sources > MAX_INTERSECTING_SST_PAGE_SOURCES {
        tracing::error!(
            code = "CALYX_ASTER_SST_SEQUENTIAL_SOURCE_LIMIT_EXCEEDED",
            intersecting_sources,
            max_sources = MAX_INTERSECTING_SST_PAGE_SOURCES,
            "sequential snapshot stream rejected excessive immutable source fan-in"
        );
        return Err(CalyxError {
            code: "CALYX_ASTER_SST_SEQUENTIAL_SOURCE_LIMIT_EXCEEDED",
            message: format!(
                "sequential snapshot stream intersects {intersecting_sources} immutable sources, above the hard maximum {MAX_INTERSECTING_SST_PAGE_SOURCES}"
            ),
            remediation: "compact the affected column family below the snapshot source ceiling and retry; the scan fails closed instead of opening unbounded cursor state",
        });
    }

    let mut sources = Vec::new();
    let overlay = overlay_page_rows(overlay, start, end, start, false);
    if !overlay.is_empty() {
        sources.push(PageSource::Overlay {
            rows: overlay,
            pos: 0,
        });
    }
    let file_sources = level
        .files
        .par_iter()
        .filter(|file| file.may_intersect(start, end))
        .map(|file| {
            let Some(mut reader) = file.open_key_state_reader()? else {
                return Ok(None);
            };
            reader.seek_lower_bound(start, false)?;
            if reader
                .current_key()
                .is_some_and(|key| end.is_none_or(|end| key < end))
            {
                Ok(Some(PageSource::Sst {
                    reader: Box::new(reader),
                }))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    sources.extend(file_sources.into_iter().flatten());

    let mut cursor = SequentialPageCursor {
        sources,
        heap: BinaryHeap::new(),
        heap_bytes: 0,
        end: end.map(<[u8]>::to_vec),
    };
    for source in 0..cursor.sources.len() {
        cursor.push_current(source)?;
    }
    Ok(cursor)
}

impl PageCursor {
    fn push_current(&mut self, source: usize) {
        if let Some(key) = self.sources[source].current_key(self.end.as_deref()) {
            self.heap.push(HeapItem {
                key: key.to_vec(),
                source,
            });
        }
    }
}

impl KeyStateCursor {
    fn push_current(&mut self, source: usize) -> Result<()> {
        let Some(key) = self.sources[source]
            .current_key(self.end.as_deref())
            .map(<[u8]>::to_vec)
        else {
            return Ok(());
        };
        let is_tombstone = self.sources[source].read_current_tombstone_state()?;
        self.heap.push(KeyStateHeapItem {
            key,
            source,
            is_tombstone,
        });
        Ok(())
    }
}

impl SequentialPageCursor {
    fn push_current(&mut self, source: usize) -> Result<()> {
        if self.sources[source]
            .current_key(self.end.as_deref())
            .is_none()
        {
            return Ok(());
        }
        let entry = self.sources[source].read_current()?;
        let row_bytes = materialized_entry_bytes::<SstEntry>(&entry.key, &entry.value);
        let next_bytes = self.heap_bytes.saturating_add(row_bytes);
        if next_bytes > SEQUENTIAL_SNAPSHOT_PAGE_MAX_BYTES {
            return Err(sequential_snapshot_memory_budget(
                "merge frontier",
                next_bytes,
                row_bytes,
            ));
        }
        self.heap_bytes = next_bytes;
        self.heap.push(SequentialHeapItem { entry, source });
        Ok(())
    }

    fn push_current_reusing(&mut self, source: usize, entry: &mut SstEntry) -> Result<()> {
        if self.sources[source]
            .current_key(self.end.as_deref())
            .is_none()
        {
            return Ok(());
        }
        self.sources[source].read_current_into(entry)?;
        let row_bytes = materialized_entry_bytes::<SstEntry>(&entry.key, &entry.value);
        let next_bytes = self.heap_bytes.saturating_add(row_bytes);
        if next_bytes > SEQUENTIAL_SNAPSHOT_PAGE_MAX_BYTES {
            return Err(sequential_snapshot_memory_budget(
                "merge frontier",
                next_bytes,
                row_bytes,
            ));
        }
        self.heap_bytes = next_bytes;
        self.heap.push(SequentialHeapItem {
            entry: SstEntry {
                key: std::mem::take(&mut entry.key),
                value: std::mem::take(&mut entry.value),
            },
            source,
        });
        Ok(())
    }

    fn pop(&mut self) -> Option<SequentialHeapItem> {
        let item = self.heap.pop()?;
        let row_bytes = materialized_entry_bytes::<SstEntry>(&item.entry.key, &item.entry.value);
        self.heap_bytes = self.heap_bytes.saturating_sub(row_bytes);
        Some(item)
    }
}

fn next_page(cursor: &mut PageCursor, limit: usize) -> Result<Vec<SstEntry>> {
    let mut out = Vec::with_capacity(limit);
    while out.len() < limit {
        let Some(winner) = next_latest_entry(cursor)? else {
            break;
        };
        if !is_tombstone_value(&winner.entry.value) {
            out.push(winner.entry);
        }
    }
    Ok(out)
}

fn next_latest_entry(cursor: &mut PageCursor) -> Result<Option<SstPageWinner>> {
    let Some(first) = cursor.heap.pop() else {
        return Ok(None);
    };
    let next_key = first.key;
    let winner_source = first.source;
    let from_overlay = matches!(&cursor.sources[winner_source], PageSource::Overlay { .. });
    let entry = cursor.sources[winner_source].read_current()?;
    let mut duplicate_sources = vec![winner_source];
    while cursor
        .heap
        .peek()
        .is_some_and(|item| item.key.as_slice() == next_key.as_slice())
    {
        duplicate_sources.push(
            cursor
                .heap
                .pop()
                .expect("peek confirmed duplicate heap item")
                .source,
        );
    }
    for source in duplicate_sources {
        cursor.sources[source].advance_past(&next_key)?;
        cursor.push_current(source);
    }
    Ok(Some(SstPageWinner {
        entry,
        from_overlay,
    }))
}

fn next_latest_key_state(cursor: &mut KeyStateCursor) -> Result<Option<SstKeyState>> {
    let Some(first) = cursor.heap.pop() else {
        return Ok(None);
    };
    let next_key = first.key;
    let is_tombstone = first.is_tombstone;
    let mut duplicate_sources = vec![first.source];
    while cursor
        .heap
        .peek()
        .is_some_and(|item| item.key.as_slice() == next_key.as_slice())
    {
        duplicate_sources.push(
            cursor
                .heap
                .pop()
                .expect("peek confirmed duplicate key-state heap item")
                .source,
        );
    }
    for source in duplicate_sources {
        cursor.sources[source].advance_past(&next_key)?;
        cursor.push_current(source)?;
    }
    Ok(Some(SstKeyState {
        key: next_key,
        is_tombstone,
    }))
}

fn next_latest_sequential_entry(
    cursor: &mut SequentialPageCursor,
) -> Result<Option<SstPageWinner>> {
    let Some(first) = cursor.pop() else {
        return Ok(None);
    };
    let winner_source = first.source;
    let from_overlay = matches!(&cursor.sources[winner_source], PageSource::Overlay { .. });
    let entry = first.entry;
    let next_key = entry.key.as_slice();
    cursor.sources[winner_source].advance_past(next_key)?;
    cursor.push_current(winner_source)?;
    while cursor
        .heap
        .peek()
        .is_some_and(|item| item.entry.key.as_slice() == next_key)
    {
        let source = cursor
            .pop()
            .expect("peek confirmed duplicate sequential heap item")
            .source;
        cursor.sources[source].advance_past(next_key)?;
        cursor.push_current(source)?;
    }
    Ok(Some(SstPageWinner {
        entry,
        from_overlay,
    }))
}

fn sequential_snapshot_memory_budget(
    scope: &'static str,
    retained: usize,
    next_record: usize,
) -> CalyxError {
    CalyxError {
        code: "CALYX_ASTER_SEQUENTIAL_SNAPSHOT_MEMORY_BUDGET",
        message: format!(
            "sequential snapshot {scope} requires {retained} bytes after a {next_record}-byte record, above the {SEQUENTIAL_SNAPSHOT_PAGE_MAX_BYTES}-byte transient budget"
        ),
        remediation: "repair or split oversized immutable rows and compact excessive overlapping SSTs; snapshot paging fails closed instead of exhausting daemon memory",
    }
}

fn overlay_page_rows(
    rows: Vec<SstEntry>,
    start: &[u8],
    end: Option<&[u8]>,
    lower: &[u8],
    exclusive: bool,
) -> Vec<SstEntry> {
    let mut rows = rows
        .into_iter()
        .filter(|row| row.key.as_slice() >= start)
        .filter(|row| end.is_none_or(|end| row.key.as_slice() < end))
        .filter(|row| {
            if exclusive {
                row.key.as_slice() > lower
            } else {
                row.key.as_slice() >= lower
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.key.cmp(&right.key));
    rows
}
