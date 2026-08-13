use super::{
    COMPACTION_ADOPTION_FIRST_INDEX, COMPACTION_ADOPTION_LAST_INDEX, CompactionDebt,
    CompactionReport, CompactionResult, CompactionThrottle, DEFAULT_COMPACTION_TARGET_BYTES,
    SstShard, WRITE_AMP_SCALE,
};
use crate::cf::ColumnFamily;
use crate::sst::{SstStreamingReader, SstSummary, write_sst};
use crate::storage_names::{SstName, classify_sst};
use calyx_core::{CalyxError, Result};
use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::fs;
use std::path::{Path, PathBuf};

const SST_HEADER_LEN: u64 = 32;
const SST_RECORD_HEADER_LEN: u64 = 12;
const SST_INDEX_ENTRY_FIXED_LEN: u64 = 12;
const SST_BLOOM_HEADER_LEN: u64 = 16;

pub(super) fn compact_shards_with_target(
    cf: ColumnFamily,
    inputs: &[SstShard],
    output_path: impl AsRef<Path>,
    throttle: CompactionThrottle,
    output_target_bytes: u64,
) -> Result<CompactionResult> {
    let debt_before = CompactionDebt::measure(inputs, DEFAULT_COMPACTION_TARGET_BYTES);
    if inputs.is_empty() || (inputs.len() < 2 && throttle.max_input_bytes.is_some()) {
        return Ok(CompactionResult::Skipped { debt: debt_before });
    }
    if let Some(max) = throttle.max_input_bytes
        && debt_before.pending_bytes > max
    {
        return Ok(CompactionResult::Skipped { debt: debt_before });
    }

    let output_path = output_path.as_ref().to_path_buf();
    let parent = output_path
        .parent()
        .ok_or_else(|| CalyxError::disk_pressure("compaction output has no parent"))?
        .to_path_buf();
    fs::create_dir_all(&parent).map_err(|error| {
        CalyxError::disk_pressure(format!("create compaction output dir: {error}"))
    })?;

    let mut writer = RollingSstWriter::new(&output_path, output_target_bytes)?;
    let mut logical_bytes = 0_u64;
    let mut emitted_keys = 0_u64;
    // Compaction is an external merge, including the byte-admitted live lane.
    // Retain one validated cursor per immutable input and only one current key
    // in the heap. The old live-lane branch decoded every selected SST in
    // parallel and accumulated the complete newest-wins result in a BTreeMap;
    // its heap therefore scaled with the logical corpus and overlapped both the
    // GC reference census and the rolling output buffer. `SstStreamingReader`
    // owns one persistent file handle, so this is one open per input for the
    // complete pass rather than the historical two opens per row.
    let mut cursors = Vec::with_capacity(inputs.len());
    let mut heap = BinaryHeap::new();
    for (precedence, shard) in inputs.iter().enumerate() {
        let cursor_index = cursors.len();
        cursors.push(SstCursor::open(shard, precedence)?);
        push_cursor_front(&mut heap, &cursors, cursor_index);
    }

    while let Some(item) = heap.pop() {
        let key = item.key;
        let mut winner_precedence = item.precedence;
        let mut winner = cursors[item.cursor_index].entry_at_current()?;
        advance_cursor(&mut heap, &mut cursors, item.cursor_index);

        while heap.peek().is_some_and(|next| next.key == key) {
            let duplicate = heap.pop().expect("heap peeked duplicate");
            let candidate = cursors[duplicate.cursor_index].entry_at_current()?;
            if duplicate.precedence >= winner_precedence {
                winner_precedence = duplicate.precedence;
                winner = candidate;
            }
            advance_cursor(&mut heap, &mut cursors, duplicate.cursor_index);
        }

        logical_bytes = logical_bytes.saturating_add(winner.value.len() as u64);
        emitted_keys = emitted_keys.saturating_add(1);
        writer.push(winner.key, winner.value)?;
    }
    let summaries = writer.finish(emitted_keys == 0)?;
    let output_shards = summaries
        .iter()
        .map(|summary| SstShard {
            cf,
            path: summary.path.clone(),
            level: inputs.iter().map(|shard| shard.level).max().unwrap_or(0) + 1,
            bytes: summary.bytes,
        })
        .collect::<Vec<_>>();
    let debt_after = CompactionDebt::measure(&output_shards, DEFAULT_COMPACTION_TARGET_BYTES);
    let input_bytes = debt_before.pending_bytes;
    let output_bytes = summaries
        .iter()
        .map(|summary| summary.bytes)
        .fold(0_u64, u64::saturating_add);
    let output_paths = summaries
        .iter()
        .map(|summary| summary.path.clone())
        .collect::<Vec<_>>();
    let output_path = output_paths
        .first()
        .cloned()
        .ok_or_else(|| CalyxError::disk_pressure("compaction produced no output SST"))?;
    let write_amp_milli = output_bytes.saturating_mul(WRITE_AMP_SCALE) / logical_bytes.max(1);

    Ok(CompactionResult::Compacted(Box::new(CompactionReport {
        cf,
        input_files: inputs.len(),
        input_paths: inputs.iter().map(|shard| shard.path.clone()).collect(),
        input_bytes,
        output_bytes,
        logical_bytes,
        write_amp_milli,
        reclaimed_input_files: 0,
        debt_before,
        debt_after,
        output_path,
        output_paths,
        staging_parent: parent,
    })))
}

struct SstCursor {
    reader: SstStreamingReader,
    position: usize,
    precedence: usize,
}

impl SstCursor {
    fn open(shard: &SstShard, precedence: usize) -> Result<Self> {
        Ok(Self {
            reader: SstStreamingReader::open(&shard.path)?,
            position: 0,
            precedence,
        })
    }

    fn current_key(&self) -> Option<&[u8]> {
        self.reader.key_at(self.position)
    }

    fn entry_at_current(&mut self) -> Result<crate::sst::SstEntry> {
        self.reader.entry_at(self.position)
    }

    fn advance(&mut self) {
        self.position = self.position.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeapItem {
    key: Vec<u8>,
    cursor_index: usize,
    precedence: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.precedence.cmp(&self.precedence))
            .then_with(|| other.cursor_index.cmp(&self.cursor_index))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

fn push_cursor_front(heap: &mut BinaryHeap<HeapItem>, cursors: &[SstCursor], cursor_index: usize) {
    if let Some(key) = cursors[cursor_index].current_key() {
        heap.push(HeapItem {
            key: key.to_vec(),
            cursor_index,
            precedence: cursors[cursor_index].precedence,
        });
    }
}

fn advance_cursor(heap: &mut BinaryHeap<HeapItem>, cursors: &mut [SstCursor], cursor_index: usize) {
    cursors[cursor_index].advance();
    push_cursor_front(heap, cursors, cursor_index);
}

pub(crate) struct RollingSstWriter {
    base_path: PathBuf,
    target_bytes: u64,
    summaries: Vec<SstSummary>,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    row_storage_bytes: u64,
    committed: bool,
}

impl RollingSstWriter {
    pub(crate) fn new(base_path: impl AsRef<Path>, target_bytes: u64) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        let parent = base_path
            .parent()
            .ok_or_else(|| CalyxError::disk_pressure("rolled SST output has no parent"))?;
        fs::create_dir_all(parent).map_err(|error| {
            CalyxError::disk_pressure(format!("create rolled SST output dir: {error}"))
        })?;
        Ok(Self {
            base_path,
            target_bytes: target_bytes.max(minimum_empty_sst_bytes()),
            summaries: Vec::new(),
            entries: Vec::new(),
            row_storage_bytes: 0,
            committed: false,
        })
    }

    pub(crate) fn push(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        let row_bytes = sst_row_storage_bytes(&key, &value)?;
        let projected = sst_estimated_bytes(
            self.entries.len().saturating_add(1),
            self.row_storage_bytes.saturating_add(row_bytes),
        )?;
        if projected > self.target_bytes {
            if self.entries.is_empty() {
                return Err(oversized_row_error(
                    &key,
                    &value,
                    projected,
                    self.target_bytes,
                ));
            }
            self.flush_current()?;
            let row_only = sst_estimated_bytes(1, row_bytes)?;
            if row_only > self.target_bytes {
                return Err(oversized_row_error(
                    &key,
                    &value,
                    row_only,
                    self.target_bytes,
                ));
            }
        }

        self.row_storage_bytes = self.row_storage_bytes.saturating_add(row_bytes);
        self.entries.push((key, value));
        Ok(())
    }

    pub(crate) fn finish(mut self, write_empty: bool) -> Result<Vec<SstSummary>> {
        if !self.entries.is_empty() || (write_empty && self.summaries.is_empty()) {
            self.flush_current()?;
        }
        self.committed = true;
        Ok(std::mem::take(&mut self.summaries))
    }

    fn flush_current(&mut self) -> Result<()> {
        let ordinal = self.summaries.len();
        let path = rolled_compaction_output_path(&self.base_path, ordinal)?;
        if ordinal > 0 && path.exists() {
            return Err(CalyxError {
                code: "CALYX_ASTER_COMPACTION_SLOTS_EXHAUSTED",
                message: format!(
                    "rolled compaction output already exists: {}",
                    path.display()
                ),
                remediation: "advance the vault durable seq or remove superseded compaction \
                              outputs via a verified compaction before retrying",
            });
        }
        let entries = self
            .entries
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()));
        let summary = write_sst(&path, entries)?;
        if summary.bytes > self.target_bytes {
            let _ = fs::remove_file(&summary.path);
            return Err(CalyxError {
                code: "CALYX_ASTER_COMPACTION_OUTPUT_EXCEEDS_TARGET",
                message: format!(
                    "compaction output {} wrote {} bytes, exceeding target {} bytes",
                    summary.path.display(),
                    summary.bytes,
                    self.target_bytes
                ),
                remediation: "report the SST size estimator drift; inputs were preserved",
            });
        }
        self.summaries.push(summary);
        self.entries.clear();
        self.row_storage_bytes = 0;
        Ok(())
    }
}

impl Drop for RollingSstWriter {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for summary in &self.summaries {
            let _ = fs::remove_file(&summary.path);
        }
    }
}

fn oversized_row_error(key: &[u8], value: &[u8], bytes: u64, target: u64) -> CalyxError {
    CalyxError {
        code: "CALYX_ASTER_COMPACTION_ROW_EXCEEDS_TARGET",
        message: format!(
            "single compacted SST row with key length {} and value length {} would write \
             {bytes} bytes, exceeding target {target} bytes",
            key.len(),
            value.len()
        ),
        remediation: "raise the compaction output target or split oversized values before \
                      retrying compaction",
    }
}

fn minimum_empty_sst_bytes() -> u64 {
    sst_estimated_bytes(0, 0).expect("empty SST estimate")
}

fn sst_row_storage_bytes(key: &[u8], value: &[u8]) -> Result<u64> {
    let key_len = u64::try_from(key.len())
        .map_err(|_| CalyxError::disk_pressure("SST key length exceeds u64"))?;
    let value_len = u64::try_from(value.len())
        .map_err(|_| CalyxError::disk_pressure("SST value length exceeds u64"))?;
    Ok(SST_RECORD_HEADER_LEN
        .saturating_add(key_len)
        .saturating_add(value_len)
        .saturating_add(SST_INDEX_ENTRY_FIXED_LEN)
        .saturating_add(key_len))
}

fn sst_estimated_bytes(row_count: usize, row_storage_bytes: u64) -> Result<u64> {
    let bloom_key_count = row_count.max(1);
    let bloom_bits = bloom_key_count
        .checked_mul(16)
        .and_then(|count| count.checked_next_power_of_two())
        .ok_or_else(|| CalyxError::disk_pressure("SST bloom bit count exceeds usize"))?
        .max(64);
    let bloom_bytes = u64::try_from(bloom_bits.div_ceil(8))
        .map_err(|_| CalyxError::disk_pressure("SST bloom byte count exceeds u64"))?;
    Ok(SST_HEADER_LEN
        .saturating_add(row_storage_bytes)
        .saturating_add(SST_BLOOM_HEADER_LEN)
        .saturating_add(bloom_bytes))
}

fn rolled_compaction_output_path(base_path: &Path, ordinal: usize) -> Result<PathBuf> {
    if ordinal == 0 {
        return Ok(base_path.to_path_buf());
    }
    let parent = base_path
        .parent()
        .ok_or_else(|| CalyxError::disk_pressure("rolled SST output has no parent"))?;
    let name = classify_sst(base_path)?.ok_or_else(|| {
        CalyxError::aster_corrupt_shard(format!(
            "rolled compaction requires canonical output name: {}",
            base_path.display()
        ))
    })?;
    let (seq, upper_index) = match name {
        SstName::DurableBatch { seq, index } => {
            let upper_index = index.checked_sub(1).ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "rolled compaction adoption slots exhausted below {}",
                    base_path.display()
                ))
            })?;
            (seq, upper_index)
        }
        SstName::Compacted { seq } => (seq, COMPACTION_ADOPTION_LAST_INDEX),
        SstName::RouterLegacy { .. } | SstName::Flush { .. } => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "rolled compaction output must be commit-domain named: {}",
                base_path.display()
            )));
        }
    };
    let capped_upper = upper_index.min(COMPACTION_ADOPTION_LAST_INDEX);
    if capped_upper >= COMPACTION_ADOPTION_FIRST_INDEX {
        for index in (COMPACTION_ADOPTION_FIRST_INDEX..=capped_upper).rev() {
            let path = parent.join(format!("{seq:020}-{index:04}.sst"));
            if !path.exists() {
                return Ok(path);
            }
        }
    }
    Err(CalyxError {
        code: "CALYX_ASTER_COMPACTION_SLOTS_EXHAUSTED",
        message: format!(
            "no free rolled compaction adoption slot remains below {} for commit seq {seq} in {}",
            base_path.display(),
            parent.display()
        ),
        remediation: "advance the vault durable seq or remove superseded compaction outputs via a \
                      verified compaction before retrying",
    })
}
