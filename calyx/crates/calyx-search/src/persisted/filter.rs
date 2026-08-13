use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::mvcc::Snapshot;
use calyx_aster::vault::AsterVault;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{Anchor, AnchorValue, Clock, Constellation, CxId};
use calyx_sextant::{AnchorPredicate, MetadataPredicate, QueryFilters, ScalarOp, ScalarPredicate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::fs_io::HashingReader;
use super::{FilterIndexEntry, rel, stale, write_atomic_hashed};
use crate::error::CliResult;

const FILTER_FORMAT_V1: &str = "calyx-search-filter-index-v1";
const FILTER_FORMAT_V2: &str = "calyx-search-filter-index-v2-jsonl";
const MAX_FILTER_LINE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilterIndex {
    format: String,
    base_seq: u64,
    rows: Vec<FilterRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StreamingFilterHeader {
    format: String,
    base_seq: u64,
    len: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilterRow {
    cx_id: CxId,
    scalars: BTreeMap<String, f64>,
    anchors: Vec<Anchor>,
    metadata: FilterMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilterMetadata {
    vault_id: calyx_core::VaultId,
    modality: calyx_core::Modality,
    panel_version: u32,
    created_at: u64,
    input_redacted: bool,
    input_pointer: Option<String>,
}

pub(super) fn write_from_vault_snapshot<C: Clock>(
    vault_dir: &Path,
    root: &Path,
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    panel_version: u32,
    page_rows: usize,
    expected_len: usize,
) -> CliResult<FilterIndexEntry> {
    let base_seq = snapshot.seq();
    let path = root.join(format!(
        "filters_seq_{base_seq:020}_n_{expected_len:010}.jsonl"
    ));
    let header = StreamingFilterHeader {
        format: FILTER_FORMAT_V2.to_owned(),
        base_seq,
        len: expected_len,
    };
    let sha256 = write_atomic_hashed(&path, |writer| {
        serde_json::to_writer(&mut *writer, &header)?;
        writer.write_all(b"\n")?;
        let mut written = 0usize;
        let mut last_cx_id = None;
        vault.scan_cf_range_pages_snapshot(
            snapshot,
            ColumnFamily::Base,
            &KeyRange {
                start: Vec::new(),
                end: None,
            },
            page_rows,
            |page| {
                for (key, bytes) in page {
                    let key_bytes: [u8; 16] = key.as_slice().try_into().map_err(|_| {
                        stale(format!(
                            "filter rebuild Base key has {} bytes; expected 16",
                            key.len()
                        ))
                    })?;
                    let cx_id = CxId::from_bytes(key_bytes);
                    let cx = decode_constellation_base(&bytes)?;
                    if cx.cx_id != cx_id {
                        return Err(stale(format!(
                            "filter rebuild Base key {cx_id} contains constellation {}",
                            cx.cx_id
                        )));
                    }
                    if cx.panel_version != panel_version {
                        continue;
                    }
                    if last_cx_id.is_some_and(|previous| previous >= cx_id) {
                        return Err(stale(format!(
                            "filter rebuild Base rows are not strictly ordered: prior {last_cx_id:?}, current {cx_id}"
                        )));
                    }
                    last_cx_id = Some(cx_id);
                    let row = FilterRow::from(&cx);
                    validate_row(&row)?;
                    serde_json::to_writer(&mut *writer, &row)?;
                    writer.write_all(b"\n")?;
                    written = written.checked_add(1).ok_or_else(|| {
                        stale("filter rebuild row count overflow while streaming sidecar")
                    })?;
                }
                Ok(())
            },
        )?;
        if written != expected_len {
            return Err(stale(format!(
                "streamed filter sidecar wrote {written} panel {panel_version} rows, but the independently planned Base scan counted {expected_len}"
            )));
        }
        Ok(())
    })?;
    Ok(FilterIndexEntry {
        built_at_seq: base_seq,
        len: expected_len,
        index_rel: rel(vault_dir, &path)?,
        sha256,
    })
}

pub(super) fn candidates(
    vault_dir: &Path,
    entry: Option<&FilterIndexEntry>,
    manifest_base_seq: u64,
    filters: &QueryFilters,
) -> CliResult<Option<BTreeSet<CxId>>> {
    if filters.is_empty() {
        return Ok(None);
    }
    let entry = entry.ok_or_else(|| {
        stale("persistent search filter sidecar is absent from manifest; rebuild the vault search indexes before filtered search")
    })?;
    if entry.index_rel.ends_with(".jsonl") {
        let mut matches = BTreeSet::new();
        visit_stream(vault_dir, entry, manifest_base_seq, |row| {
            if row.matches(filters) {
                matches.insert(row.cx_id);
            }
            Ok(())
        })?;
        return Ok(Some(matches));
    }
    let index = read_legacy(vault_dir, entry, manifest_base_seq)?;
    Ok(Some(
        index
            .rows
            .iter()
            .filter(|row| row.matches(filters))
            .map(|row| row.cx_id)
            .collect(),
    ))
}

pub(super) fn constellation_matches(cx: &Constellation, filters: &QueryFilters) -> bool {
    FilterRow::from(cx).matches(filters)
}

fn read_legacy(
    vault_dir: &Path,
    entry: &FilterIndexEntry,
    manifest_base_seq: u64,
) -> CliResult<FilterIndex> {
    let path = vault_dir.join(&entry.index_rel);
    if !path.is_file() {
        return Err(stale(format!(
            "persistent search filter sidecar missing at {}; rebuild the vault search indexes",
            path.display()
        )));
    }
    let bytes = fs::read(&path)?;
    let actual = sha256_hex(&bytes);
    if actual != entry.sha256 {
        return Err(stale(format!(
            "persistent search filter sidecar sha256 {actual} != manifest {}; rebuild the vault search indexes",
            entry.sha256
        )));
    }
    let index: FilterIndex = serde_json::from_slice(&bytes).map_err(|err| {
        stale(format!(
            "persistent search filter sidecar {} is not valid JSON: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    validate(&index, entry, manifest_base_seq)?;
    Ok(index)
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &FilterIndexEntry,
    manifest_base_seq: u64,
) -> CliResult {
    if entry.index_rel.ends_with(".jsonl") {
        visit_stream(vault_dir, entry, manifest_base_seq, |_| Ok(()))?;
    } else {
        let _ = read_legacy(vault_dir, entry, manifest_base_seq)?;
    }
    Ok(())
}

fn validate(index: &FilterIndex, entry: &FilterIndexEntry, manifest_base_seq: u64) -> CliResult {
    if index.format != FILTER_FORMAT_V1 {
        return Err(stale(format!(
            "persistent search filter sidecar has format {}; expected {FILTER_FORMAT_V1}",
            index.format
        )));
    }
    if index.base_seq != manifest_base_seq || entry.built_at_seq != manifest_base_seq {
        return Err(stale(format!(
            "persistent search filter sidecar seq {} / entry seq {} != manifest seq {}; rebuild the vault search indexes",
            index.base_seq, entry.built_at_seq, manifest_base_seq
        )));
    }
    if index.rows.len() != entry.len {
        return Err(stale(format!(
            "persistent search filter sidecar row len {} != manifest len {}; rebuild the vault search indexes",
            index.rows.len(),
            entry.len
        )));
    }
    let mut seen = BTreeSet::new();
    for row in &index.rows {
        if !seen.insert(row.cx_id) {
            return Err(stale(format!(
                "persistent search filter sidecar repeats {}; rebuild the vault search indexes",
                row.cx_id
            )));
        }
        validate_row(row)?;
    }
    Ok(())
}

fn visit_stream<F>(
    vault_dir: &Path,
    entry: &FilterIndexEntry,
    manifest_base_seq: u64,
    mut visit: F,
) -> CliResult
where
    F: FnMut(&FilterRow) -> CliResult,
{
    let path = vault_dir.join(&entry.index_rel);
    if !path.is_file() {
        return Err(stale(format!(
            "persistent streaming search filter sidecar missing at {}; rebuild the vault search indexes",
            path.display()
        )));
    }
    let mut hashing_reader = HashingReader::new(File::open(&path)?);
    let mut reader = BufReader::new(&mut hashing_reader);
    let header_line = read_bounded_line(&mut reader, &path, "header")?.ok_or_else(|| {
        stale(format!(
            "persistent streaming search filter sidecar {} is empty",
            path.display()
        ))
    })?;
    let header: StreamingFilterHeader = serde_json::from_str(&header_line).map_err(|error| {
        stale(format!(
            "persistent streaming search filter header {} is invalid JSON: {error}",
            path.display()
        ))
    })?;
    if header.format != FILTER_FORMAT_V2 {
        return Err(stale(format!(
            "persistent streaming search filter sidecar has format {}; expected {FILTER_FORMAT_V2}",
            header.format
        )));
    }
    if header.base_seq != manifest_base_seq
        || entry.built_at_seq != manifest_base_seq
        || header.len != entry.len
    {
        return Err(stale(format!(
            "persistent streaming search filter header seq {} / entry seq {} / len {} does not match manifest seq {manifest_base_seq} / len {}",
            header.base_seq, entry.built_at_seq, header.len, entry.len
        )));
    }
    let mut count = 0usize;
    let mut last_cx_id = None;
    while let Some(line) = read_bounded_line(&mut reader, &path, "row")? {
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            return Err(stale(format!(
                "persistent streaming search filter sidecar {} contains an empty row line",
                path.display()
            )));
        }
        let row: FilterRow = serde_json::from_str(&line).map_err(|error| {
            stale(format!(
                "persistent streaming search filter row {} is invalid JSON: {error}",
                path.display()
            ))
        })?;
        if last_cx_id.is_some_and(|previous| previous >= row.cx_id) {
            return Err(stale(format!(
                "persistent streaming search filter rows are not strictly ordered: prior {last_cx_id:?}, current {}",
                row.cx_id
            )));
        }
        last_cx_id = Some(row.cx_id);
        validate_row(&row)?;
        visit(&row)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| stale("persistent streaming search filter row count overflow"))?;
    }
    if count != entry.len {
        return Err(stale(format!(
            "persistent streaming search filter row len {count} != manifest len {}; rebuild the vault search indexes",
            entry.len
        )));
    }
    drop(reader);
    let actual = hashing_reader.into_sha256();
    if actual != entry.sha256 {
        return Err(stale(format!(
            "persistent streaming search filter sidecar sha256 {actual} != manifest {}; rebuild the vault search indexes",
            entry.sha256
        )));
    }
    Ok(())
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    path: &Path,
    kind: &str,
) -> CliResult<Option<String>> {
    let mut line = String::new();
    let read = reader
        .take(MAX_FILTER_LINE_BYTES + 1)
        .read_line(&mut line)
        .map_err(|error| {
            stale(format!(
                "persistent streaming search filter {kind} at {} could not be decoded as UTF-8: {error}",
                path.display()
            ))
        })?;
    if read == 0 {
        return Ok(None);
    }
    if read as u64 > MAX_FILTER_LINE_BYTES {
        return Err(stale(format!(
            "persistent streaming search filter {kind} at {} exceeds the {MAX_FILTER_LINE_BYTES}-byte structural bound",
            path.display()
        )));
    }
    if !line.ends_with('\n') {
        return Err(stale(format!(
            "persistent streaming search filter {kind} at {} is not newline-terminated",
            path.display()
        )));
    }
    Ok(Some(line))
}

fn validate_row(row: &FilterRow) -> CliResult {
    for (name, value) in &row.scalars {
        if name.is_empty() || !value.is_finite() {
            return Err(stale(format!(
                "persistent search filter sidecar row {} has invalid scalar {name:?}",
                row.cx_id
            )));
        }
    }
    for anchor in &row.anchors {
        anchor.validate_schema().map_err(|err| {
            stale(format!(
                "persistent search filter sidecar row {} has invalid anchor: {}",
                row.cx_id, err.message
            ))
        })?;
    }
    Ok(())
}

impl From<&Constellation> for FilterRow {
    fn from(cx: &Constellation) -> Self {
        Self {
            cx_id: cx.cx_id,
            scalars: cx.scalars.clone(),
            anchors: cx.anchors.clone(),
            metadata: FilterMetadata {
                vault_id: cx.vault_id,
                modality: cx.modality,
                panel_version: cx.panel_version,
                created_at: cx.created_at,
                input_redacted: cx.input_ref.redacted,
                input_pointer: cx.input_ref.pointer.clone(),
            },
        }
    }
}

impl FilterRow {
    fn matches(&self, filters: &QueryFilters) -> bool {
        filters
            .scalars
            .iter()
            .all(|filter| self.scalar_matches(filter))
            && filters
                .anchors
                .iter()
                .all(|filter| self.anchor_matches(filter))
            && filters
                .metadata
                .iter()
                .all(|filter| self.metadata.matches(filter))
    }

    fn scalar_matches(&self, filter: &ScalarPredicate) -> bool {
        self.scalars
            .get(&filter.name)
            .is_some_and(|actual| compare_scalar(*actual, filter.op, filter.value))
    }

    fn anchor_matches(&self, filter: &AnchorPredicate) -> bool {
        self.anchors.iter().any(|anchor| {
            anchor.kind == filter.kind
                && filter
                    .value
                    .as_ref()
                    .is_none_or(|value| anchor_value_matches(&anchor.value, value))
                && filter
                    .min_confidence
                    .is_none_or(|minimum| anchor.confidence >= minimum)
                && filter
                    .source
                    .as_ref()
                    .is_none_or(|source| &anchor.source == source)
        })
    }
}

impl FilterMetadata {
    fn matches(&self, filter: &MetadataPredicate) -> bool {
        match filter {
            MetadataPredicate::Vault(vault) => self.vault_id == *vault,
            MetadataPredicate::Modality(modality) => self.modality == *modality,
            MetadataPredicate::PanelVersion(version) => self.panel_version == *version,
            MetadataPredicate::CreatedAt { min, max } => {
                min.is_none_or(|value| self.created_at >= value)
                    && max.is_none_or(|value| self.created_at <= value)
            }
            MetadataPredicate::InputRedacted(expected) => self.input_redacted == *expected,
            MetadataPredicate::InputPointerContains(fragment) => self
                .input_pointer
                .as_deref()
                .is_some_and(|pointer| pointer.contains(fragment)),
        }
    }
}

fn compare_scalar(actual: f64, op: ScalarOp, expected: f64) -> bool {
    if !actual.is_finite() || !expected.is_finite() {
        return false;
    }
    match op {
        ScalarOp::Eq => actual == expected,
        ScalarOp::Gt => actual > expected,
        ScalarOp::Gte => actual >= expected,
        ScalarOp::Lt => actual < expected,
        ScalarOp::Lte => actual <= expected,
    }
}

fn anchor_value_matches(actual: &AnchorValue, expected: &AnchorValue) -> bool {
    actual == expected
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
