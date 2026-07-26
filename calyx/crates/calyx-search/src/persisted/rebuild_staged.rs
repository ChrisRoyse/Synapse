//! Staged per-slot completion records (issue #1089). Each slot build publishes
//! its `SearchIndexEntry` to a `slot_*_seq_*.staged.json` sidecar as soon as
//! the slot's artifacts are on disk, so a rebuild killed mid-flight can be
//! resumed: a rerun at the SAME pinned base seq revalidates each staged
//! artifact (content hash + embedded base seq) and reuses it instead of
//! rebuilding, then finishes the atomic manifest publish. Staged records are
//! scratch state — the pre-publish `validate_staged_manifest_artifacts` gate
//! stays authoritative — and prune removes them after the manifest is written.

use super::super::rebuild_plan::SlotBuildPlan;
use super::super::*;

const STAGED_ARTIFACT_SCHEMA: &str = "calyx-search-staged-artifact-v1";

#[derive(Serialize, Deserialize)]
struct StagedSlotArtifact {
    schema: String,
    base_seq: u64,
    slot: u16,
    /// `None` records an absent-only slot scan (no index entry to build).
    entry: Option<SearchIndexEntry>,
    /// DiskANN manifest entries carry no content hash, so the staged record
    /// pins the exact graph/id-map bytes for resume validation.
    graph_sha256: Option<String>,
    id_map_sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StagedFilterArtifact {
    schema: String,
    base_seq: u64,
    entry: FilterIndexEntry,
}

pub(super) struct BuiltSlot {
    pub(super) entry: OptionalSearchIndexEntry,
    pub(super) row_count: usize,
}

impl BuiltSlot {
    pub(super) fn ok_phase(&self) -> &'static str {
        match self.entry.kind() {
            Some("diskann" | "flat_dense") => "dense_slot_ok",
            Some("sparse_inverted" | "sparse_bm25" | "sparse_dot") => "sparse_slot_ok",
            Some("multi_maxsim" | "multi_maxsim_segments") => "multi_slot_ok",
            _ => "slot_build_ok",
        }
    }
}

pub(super) enum OptionalSearchIndexEntry {
    Some(SearchIndexEntry),
    None { slot: u16 },
}

impl OptionalSearchIndexEntry {
    pub(super) fn slot(&self) -> u16 {
        match self {
            Self::Some(entry) => entry.slot,
            Self::None { slot } => *slot,
        }
    }

    pub(super) fn kind(&self) -> Option<&str> {
        match self {
            Self::Some(entry) => Some(&entry.kind),
            Self::None { .. } => None,
        }
    }

    pub(super) fn into_entry(self) -> Option<SearchIndexEntry> {
        match self {
            Self::Some(entry) => Some(entry),
            Self::None { .. } => None,
        }
    }
}

fn staged_slot_path(root: &Path, slot: SlotId, base_seq: u64) -> PathBuf {
    root.join(format!(
        "slot_{:05}_seq_{:020}.staged.json",
        slot.get(),
        base_seq
    ))
}

fn staged_filter_path(root: &Path, base_seq: u64) -> PathBuf {
    root.join(format!("filter_seq_{:020}.staged.json", base_seq))
}

/// Validates every staged recovery record physically present in `root` before
/// a rebuild is allowed to write or prune anything. Records from older pins
/// are still evidence of an interrupted recovery and must be internally
/// complete; only a successful manifest publication may retire them.
pub(super) fn validate_present_staged_artifacts(
    vault_dir: &Path,
    root: &Path,
    plans: &[SlotBuildPlan],
) -> CliResult<usize> {
    let mut entries = fs::read_dir(root)?
        .filter_map(|entry| match entry {
            Ok(entry) => entry
                .file_name()
                .to_string_lossy()
                .ends_with(".staged.json")
                .then_some(Ok(entry)),
            Err(error) => Some(Err(error)),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in &entries {
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            return Err(stale(format!(
                "staged recovery artifact {} is not a regular file",
                path.display()
            )));
        }
        let name = entry.file_name().to_string_lossy().to_string();
        match parse_staged_name(&path, &name)? {
            StagedArtifactName::Slot { slot, base_seq } => {
                let plan = plans
                    .iter()
                    .find(|plan| plan.slot == slot)
                    .ok_or_else(|| {
                        stale(format!(
                            "staged slot artifact {} names slot {slot}, which is absent from the active panel rebuild plan",
                            path.display()
                        ))
                    })?;
                if reuse_staged_slot_entry(vault_dir, root, plan, base_seq)?.is_none() {
                    return Err(stale(format!(
                        "staged slot artifact {} disappeared during recovery preflight",
                        path.display()
                    )));
                }
            }
            StagedArtifactName::Filter { base_seq } => {
                if reuse_staged_filter_entry(vault_dir, root, base_seq)?.is_none() {
                    return Err(stale(format!(
                        "staged filter artifact {} disappeared during recovery preflight",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(entries.len())
}

enum StagedArtifactName {
    Slot { slot: SlotId, base_seq: u64 },
    Filter { base_seq: u64 },
}

fn parse_staged_name(path: &Path, name: &str) -> CliResult<StagedArtifactName> {
    let stem = name.strip_suffix(".staged.json").ok_or_else(|| {
        stale(format!(
            "staged recovery artifact {} lacks the .staged.json suffix",
            path.display()
        ))
    })?;
    if let Some(rest) = stem.strip_prefix("slot_") {
        let (slot, base_seq) = rest.split_once("_seq_").ok_or_else(|| {
            staged_name_error(path, "expected slot_<5 digits>_seq_<20 digits>.staged.json")
        })?;
        require_decimal_width(path, "slot", slot, 5)?;
        require_decimal_width(path, "base sequence", base_seq, 20)?;
        let slot = slot.parse::<u16>().map_err(|error| {
            staged_name_error(path, &format!("slot number does not fit u16: {error}"))
        })?;
        let base_seq = base_seq.parse::<u64>().map_err(|error| {
            staged_name_error(path, &format!("base sequence does not fit u64: {error}"))
        })?;
        let canonical = format!("slot_{slot:05}_seq_{base_seq:020}.staged.json");
        if name != canonical {
            return Err(staged_name_error(
                path,
                &format!("non-canonical staged slot name; expected {canonical}"),
            ));
        }
        return Ok(StagedArtifactName::Slot {
            slot: SlotId::new(slot),
            base_seq,
        });
    }
    if let Some(base_seq) = stem.strip_prefix("filter_seq_") {
        require_decimal_width(path, "base sequence", base_seq, 20)?;
        let base_seq = base_seq.parse::<u64>().map_err(|error| {
            staged_name_error(path, &format!("base sequence does not fit u64: {error}"))
        })?;
        let canonical = format!("filter_seq_{base_seq:020}.staged.json");
        if name != canonical {
            return Err(staged_name_error(
                path,
                &format!("non-canonical staged filter name; expected {canonical}"),
            ));
        }
        return Ok(StagedArtifactName::Filter { base_seq });
    }
    Err(staged_name_error(
        path,
        "expected slot_<5 digits>_seq_<20 digits>.staged.json or filter_seq_<20 digits>.staged.json",
    ))
}

fn require_decimal_width(path: &Path, field: &str, value: &str, width: usize) -> CliResult {
    if value.len() != width || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(staged_name_error(
            path,
            &format!("{field} must contain exactly {width} ASCII decimal digits"),
        ));
    }
    Ok(())
}

fn staged_name_error(path: &Path, detail: &str) -> CliError {
    stale(format!(
        "staged recovery artifact {} has an invalid file name: {detail}",
        path.display()
    ))
}

pub(super) fn write_staged_slot_artifact(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    base_seq: u64,
    entry: &OptionalSearchIndexEntry,
) -> CliResult {
    let staged = match entry {
        OptionalSearchIndexEntry::None { slot } => StagedSlotArtifact {
            schema: STAGED_ARTIFACT_SCHEMA.to_string(),
            base_seq,
            slot: *slot,
            entry: None,
            graph_sha256: None,
            id_map_sha256: None,
        },
        OptionalSearchIndexEntry::Some(entry) => {
            let (graph_sha256, id_map_sha256) = if entry.kind == "diskann" {
                (
                    Some(sha256_of_rel(vault_dir, entry.require_graph_rel(slot)?)?),
                    Some(sha256_of_rel(vault_dir, entry.require_id_map_rel(slot)?)?),
                )
            } else {
                (None, None)
            };
            StagedSlotArtifact {
                schema: STAGED_ARTIFACT_SCHEMA.to_string(),
                base_seq,
                slot: entry.slot,
                entry: Some(entry.clone()),
                graph_sha256,
                id_map_sha256,
            }
        }
    };
    write_json_atomic(&staged_slot_path(root, slot, base_seq), &staged)
}

pub(super) fn write_staged_filter_artifact(
    root: &Path,
    base_seq: u64,
    entry: &FilterIndexEntry,
) -> CliResult {
    write_json_atomic(
        &staged_filter_path(root, base_seq),
        &StagedFilterArtifact {
            schema: STAGED_ARTIFACT_SCHEMA.to_string(),
            base_seq,
            entry: entry.clone(),
        },
    )
}

/// `Ok(None)` means the staged record is physically absent. A present record
/// must parse and revalidate completely; corrupt resume state is evidence that
/// must remain visible, never a signal to silently regenerate different bytes.
pub(super) fn reuse_staged_slot_entry(
    vault_dir: &Path,
    root: &Path,
    plan: &SlotBuildPlan,
    base_seq: u64,
) -> CliResult<Option<BuiltSlot>> {
    let path = staged_slot_path(root, plan.slot, base_seq);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(stale(format!(
                "read staged slot artifact {} failed: {error}",
                path.display()
            )));
        }
    };
    let staged = serde_json::from_slice::<StagedSlotArtifact>(&bytes).map_err(|error| {
        stale(format!(
            "staged slot artifact {} is not valid JSON: {error}",
            path.display()
        ))
    })?;
    if staged.schema != STAGED_ARTIFACT_SCHEMA {
        return Err(stale(format!(
            "staged slot artifact {} schema {} != {STAGED_ARTIFACT_SCHEMA}",
            path.display(),
            staged.schema
        )));
    }
    if staged.base_seq != base_seq {
        return Err(stale(format!(
            "staged slot artifact {} base_seq {} != pinned {base_seq}",
            path.display(),
            staged.base_seq
        )));
    }
    if staged.slot != plan.slot.get() {
        return Err(stale(format!(
            "staged slot artifact {} slot {} != planned {}",
            path.display(),
            staged.slot,
            plan.slot
        )));
    }
    let Some(entry) = staged.entry else {
        if staged.graph_sha256.is_some() || staged.id_map_sha256.is_some() {
            return Err(stale(format!(
                "staged absent-only slot artifact {} unexpectedly contains index hashes",
                path.display()
            )));
        }
        return Ok(Some(BuiltSlot {
            entry: OptionalSearchIndexEntry::None {
                slot: plan.slot.get(),
            },
            row_count: 0,
        }));
    };
    if entry.slot != plan.slot.get() {
        return Err(stale(format!(
            "staged slot artifact {} entry slot {} != planned {}",
            path.display(),
            entry.slot,
            plan.slot
        )));
    }
    if entry.built_at_seq != base_seq {
        return Err(stale(format!(
            "staged slot artifact {} entry seq {} != pinned {base_seq}",
            path.display(),
            entry.built_at_seq
        )));
    }
    validate_staged_scoring_contract(&path, plan, &entry)?;
    let validated = match entry.kind.as_str() {
        "diskann" => validate_staged_diskann(
            vault_dir,
            &entry,
            plan.panel_version,
            plan.slot,
            staged.graph_sha256.as_deref(),
            staged.id_map_sha256.as_deref(),
        ),
        "flat_dense" => dense::validate_entry(vault_dir, &entry, plan.panel_version, plan.slot),
        "sparse_inverted" | "sparse_bm25" | "sparse_dot" => {
            sparse::validate_entry(vault_dir, &entry, base_seq, plan.slot)
        }
        "multi_maxsim" | "multi_maxsim_segments" => {
            multi::validate_entry(vault_dir, &entry, base_seq, plan.slot)
        }
        other => Err(stale(format!(
            "staged slot artifact {} uses unsupported index kind {other}",
            path.display()
        ))),
    };
    validated.map_err(|error| {
        stale(format!(
            "staged slot artifact {} failed referenced-artifact validation [{}] {}",
            path.display(),
            error.code(),
            error.message()
        ))
    })?;
    let row_count = entry.len;
    Ok(Some(BuiltSlot {
        entry: OptionalSearchIndexEntry::Some(entry),
        row_count,
    }))
}

fn validate_staged_diskann(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    graph_sha256: Option<&str>,
    id_map_sha256: Option<&str>,
) -> CliResult {
    let (Some(expected_graph), Some(expected_id_map)) = (graph_sha256, id_map_sha256) else {
        return Err(stale(format!(
            "staged diskann slot {slot} record is missing artifact hashes"
        )));
    };
    dense::validate_entry(vault_dir, entry, panel_version, slot)?;
    let actual_graph = sha256_of_rel(vault_dir, entry.require_graph_rel(slot)?)?;
    if actual_graph != expected_graph {
        return Err(stale(format!(
            "staged diskann slot {slot} graph sha256 {actual_graph} != staged {expected_graph}"
        )));
    }
    let actual_id_map = sha256_of_rel(vault_dir, entry.require_id_map_rel(slot)?)?;
    if actual_id_map != expected_id_map {
        return Err(stale(format!(
            "staged diskann slot {slot} id map sha256 {actual_id_map} != staged {expected_id_map}"
        )));
    }
    Ok(())
}

pub(super) fn reuse_staged_filter_entry(
    vault_dir: &Path,
    root: &Path,
    base_seq: u64,
) -> CliResult<Option<FilterIndexEntry>> {
    let path = staged_filter_path(root, base_seq);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(stale(format!(
                "read staged filter artifact {} failed: {error}",
                path.display()
            )));
        }
    };
    let staged = serde_json::from_slice::<StagedFilterArtifact>(&bytes).map_err(|error| {
        stale(format!(
            "staged filter artifact {} is not valid JSON: {error}",
            path.display()
        ))
    })?;
    if staged.schema != STAGED_ARTIFACT_SCHEMA {
        return Err(stale(format!(
            "staged filter artifact {} schema {} != {STAGED_ARTIFACT_SCHEMA}",
            path.display(),
            staged.schema
        )));
    }
    if staged.base_seq != base_seq {
        return Err(stale(format!(
            "staged filter artifact {} base_seq {} != pinned {base_seq}",
            path.display(),
            staged.base_seq
        )));
    }
    filter::validate_entry(vault_dir, &staged.entry, base_seq).map_err(|error| {
        stale(format!(
            "staged filter artifact {} failed referenced-artifact validation [{}] {}",
            path.display(),
            error.code(),
            error.message()
        ))
    })?;
    Ok(Some(staged.entry))
}

fn validate_staged_scoring_contract(
    path: &Path,
    plan: &SlotBuildPlan,
    entry: &SearchIndexEntry,
) -> CliResult {
    let actual_sparse = match entry.kind.as_str() {
        "sparse_inverted" => Some(sparse::SparseScoring::Bm25),
        "sparse_bm25" => Some(sparse::SparseScoring::Bm25),
        "sparse_dot" => Some(sparse::SparseScoring::DotProduct),
        _ => None,
    };
    match (plan.sparse_scoring, actual_sparse) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (Some(expected), Some(actual)) => Err(stale(format!(
            "staged slot artifact {} sparse scoring {actual:?} != active panel/lens contract {expected:?}",
            path.display()
        ))),
        (Some(expected), None) => Err(stale(format!(
            "staged slot artifact {} kind {} is not sparse but the active panel/lens contract requires {expected:?}",
            path.display(),
            entry.kind
        ))),
        (None, Some(actual)) => Err(stale(format!(
            "staged slot artifact {} declares sparse scoring {actual:?} but the active panel/lens contract is not sparse",
            path.display()
        ))),
        (None, None) => Ok(()),
    }
}

fn sha256_of_rel(vault_dir: &Path, rel: &str) -> CliResult<String> {
    let path = vault_dir.join(rel);
    let bytes = fs::read(&path).map_err(|error| {
        stale(format!(
            "read index artifact {} for staged hash failed: {error}",
            path.display()
        ))
    })?;
    Ok(sha256_hex(&bytes))
}
