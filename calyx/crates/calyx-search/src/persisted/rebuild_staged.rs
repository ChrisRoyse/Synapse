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
            Some("sparse_inverted") => "sparse_slot_ok",
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

/// `Ok(None)` = nothing staged (or the staged record failed validation and the
/// slot must be rebuilt — the expected residue of a killed run, surfaced via
/// the caller's progress phases, never trusted). `Ok(Some)` = every byte the
/// staged entry references revalidated at this exact base seq.
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
    let Ok(staged) = serde_json::from_slice::<StagedSlotArtifact>(&bytes) else {
        return Ok(None);
    };
    if staged.schema != STAGED_ARTIFACT_SCHEMA
        || staged.base_seq != base_seq
        || staged.slot != plan.slot.get()
    {
        return Ok(None);
    }
    let Some(entry) = staged.entry else {
        return Ok(Some(BuiltSlot {
            entry: OptionalSearchIndexEntry::None {
                slot: plan.slot.get(),
            },
            row_count: 0,
        }));
    };
    if entry.slot != plan.slot.get() || entry.built_at_seq != base_seq {
        return Ok(None);
    }
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
        "sparse_inverted" => sparse::validate_entry(vault_dir, &entry, base_seq, plan.slot),
        "multi_maxsim" | "multi_maxsim_segments" => {
            multi::validate_entry(vault_dir, &entry, base_seq, plan.slot)
        }
        _ => return Ok(None),
    };
    if validated.is_err() {
        return Ok(None);
    }
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
    let Ok(staged) = serde_json::from_slice::<StagedFilterArtifact>(&bytes) else {
        return Ok(None);
    };
    if staged.schema != STAGED_ARTIFACT_SCHEMA || staged.base_seq != base_seq {
        return Ok(None);
    }
    if filter::validate_entry(vault_dir, &staged.entry, base_seq).is_err() {
        return Ok(None);
    }
    Ok(Some(staged.entry))
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
