use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use calyx_core::{Anchor, AnchorValue, Constellation, CxId};
use calyx_sextant::{AnchorPredicate, MetadataPredicate, QueryFilters, ScalarOp, ScalarPredicate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{FilterIndexEntry, rel, stale, write_json_atomic_hashed};
use crate::error::CliResult;

const FILTER_FORMAT: &str = "calyx-search-filter-index-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilterIndex {
    format: String,
    base_seq: u64,
    rows: Vec<FilterRow>,
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

pub(super) fn write(
    vault_dir: &Path,
    root: &Path,
    docs: &BTreeMap<CxId, Constellation>,
    base_seq: u64,
) -> CliResult<FilterIndexEntry> {
    let path = root.join(format!(
        "filters_seq_{base_seq:020}_n_{:010}.json",
        docs.len()
    ));
    let index = FilterIndex {
        format: FILTER_FORMAT.to_string(),
        base_seq,
        rows: docs.values().map(FilterRow::from).collect(),
    };
    let sha256 = write_json_atomic_hashed(&path, &index)?;
    Ok(FilterIndexEntry {
        built_at_seq: base_seq,
        len: docs.len(),
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
    let index = read(vault_dir, entry, manifest_base_seq)?;
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

fn read(
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
    let _ = read(vault_dir, entry, manifest_base_seq)?;
    Ok(())
}

fn validate(index: &FilterIndex, entry: &FilterIndexEntry, manifest_base_seq: u64) -> CliResult {
    if index.format != FILTER_FORMAT {
        return Err(stale(format!(
            "persistent search filter sidecar has format {}; expected {FILTER_FORMAT}",
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
