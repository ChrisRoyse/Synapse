use super::level::SstLevel;
use super::{MAX_INTERSECTING_SST_PAGE_SOURCES, SstEntry, SstPageReader};
use crate::mvcc::is_tombstone_value;
use calyx_core::{CalyxError, Result};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

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
        let Some(entry) = next_latest_entry(&mut cursor)? else {
            break;
        };
        rows.push(entry);
    }
    Ok(rows)
}

pub(super) fn range_pages<F, E>(
    level: &SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    after_key: Option<&[u8]>,
    limit: usize,
    overlay: Vec<SstEntry>,
    mut on_page: F,
) -> std::result::Result<(), E>
where
    F: FnMut(Vec<SstEntry>) -> std::result::Result<(), E>,
    E: From<calyx_core::CalyxError>,
{
    if limit == 0 {
        return Ok(());
    }
    let mut cursor =
        open_page_cursor(level, start, end, after_key, overlay, None).map_err(E::from)?;
    loop {
        let page = next_page(&mut cursor, limit).map_err(E::from)?;
        if page.is_empty() {
            break;
        }
        on_page(page)?;
    }
    Ok(())
}

struct PageCursor<'a> {
    sources: Vec<PageSource<'a>>,
    heap: BinaryHeap<HeapItem>,
    end: Option<Vec<u8>>,
}

enum PageSource<'a> {
    Overlay {
        rows: Vec<SstEntry>,
        pos: usize,
    },
    Sst {
        reader: SstPageReader<'a>,
        pos: usize,
    },
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

impl PageSource<'_> {
    fn current_key(&self, end: Option<&[u8]>) -> Option<&[u8]> {
        let key = match self {
            Self::Overlay { rows, pos } => rows.get(*pos).map(|row| row.key.as_slice()),
            Self::Sst { reader, pos } => reader.key_at(*pos),
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
            Self::Sst { reader, pos } => reader.entry_at(*pos),
        }
    }

    fn advance_past(&mut self, key: &[u8]) {
        match self {
            Self::Overlay { rows, pos } => {
                while rows.get(*pos).is_some_and(|row| row.key.as_slice() == key) {
                    *pos += 1;
                }
            }
            Self::Sst { reader, pos } => {
                while reader
                    .key_at(*pos)
                    .is_some_and(|candidate| candidate == key)
                {
                    *pos += 1;
                }
            }
        }
    }
}

fn open_page_cursor<'a>(
    level: &'a SstLevel,
    start: &[u8],
    end: Option<&[u8]>,
    after_key: Option<&[u8]>,
    overlay: Vec<SstEntry>,
    max_intersecting_sst_sources: Option<usize>,
) -> Result<PageCursor<'a>> {
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
            let Some(reader) = file.open_page_reader()? else {
                return Ok(None);
            };
            let mut pos = reader.lower_bound(lower, exclusive);
            while reader.key_at(pos).is_some_and(|key| key < start) {
                pos += 1;
            }
            if reader
                .key_at(pos)
                .is_some_and(|key| end.is_none_or(|end| key < end))
            {
                Ok(Some(PageSource::Sst { reader, pos }))
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

impl PageCursor<'_> {
    fn push_current(&mut self, source: usize) {
        if let Some(key) = self.sources[source].current_key(self.end.as_deref()) {
            self.heap.push(HeapItem {
                key: key.to_vec(),
                source,
            });
        }
    }
}

fn next_page(cursor: &mut PageCursor<'_>, limit: usize) -> Result<Vec<SstEntry>> {
    let mut out = Vec::with_capacity(limit);
    while out.len() < limit {
        let Some(entry) = next_latest_entry(cursor)? else {
            break;
        };
        if !is_tombstone_value(&entry.value) {
            out.push(entry);
        }
    }
    Ok(out)
}

fn next_latest_entry(cursor: &mut PageCursor<'_>) -> Result<Option<SstEntry>> {
    let Some(first) = cursor.heap.pop() else {
        return Ok(None);
    };
    let next_key = first.key;
    let winner_source = first.source;
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
        cursor.sources[source].advance_past(&next_key);
        cursor.push_current(source);
    }
    Ok(Some(entry))
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
