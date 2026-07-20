//! Durable vault-level "search index rebuild required" marker (issue #1089).
//!
//! Base CF commits and the derived search-index rebuild are separate steps, so
//! an external kill (CLI timeout, power loss) between them leaves durable rows
//! with a stale `idx/search/manifest.json` and nothing on disk that says so.
//! This marker is the write-ahead intent record for that gap:
//!
//! * every mutation path writes the marker BEFORE it advances the Base CF, and
//! * the rebuild removes it only AFTER the new manifest is durably published,
//!
//! so at any crash point the vault either has a fresh manifest or a marker
//! naming the exact commit that made the derived state stale. Searches keep
//! failing closed via the existing manifest seq check; the marker exists so
//! operators and FSV automation get a first-class, structured record instead
//! of having to infer staleness by recomputing seq comparisons.

use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

pub const REBUILD_REQUIRED_SCHEMA: &str = "calyx-search-rebuild-required-v2";
const REBUILD_REQUIRED_NAME: &str = "rebuild-required.json";
pub const REBUILD_REQUIRED_REMEDIATION: &str = "run `calyx rebuild-search-index <vault>`; the rebuild reuses staged slot artifacts from an interrupted run and clears this marker after the manifest is durably republished";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebuildRequiredMarker {
    pub schema: String,
    /// Exact panel generation whose derived indexes are stale.
    pub panel_version: u32,
    /// Which mutation path staked the intent (`batch_ingest`, `text_ingest`,
    /// `anchor_command`, `search_index_rebuild`, ...).
    pub source: String,
    pub detail: String,
    /// Durable vault seq the derived indexes must reach. `None` while the
    /// mutation is still in flight (final seq unknown yet); a `None` marker
    /// means "assume stale until a rebuild completes".
    pub required_base_seq: Option<u64>,
    /// `manifest.json` base seq observed when the marker was written, if a
    /// manifest existed. Purely diagnostic.
    pub manifest_base_seq_at_write: Option<u64>,
    pub session_id: Option<String>,
    pub batch_path: Option<String>,
    pub process_id: u32,
    pub written_at_unix_ms: u64,
    pub remediation: String,
}

impl RebuildRequiredMarker {
    pub fn new(panel_version: u32, source: &str, detail: impl Into<String>) -> CliResult<Self> {
        Ok(Self {
            schema: REBUILD_REQUIRED_SCHEMA.to_string(),
            panel_version,
            source: source.to_string(),
            detail: detail.into(),
            required_base_seq: None,
            manifest_base_seq_at_write: None,
            session_id: None,
            batch_path: None,
            process_id: std::process::id(),
            written_at_unix_ms: unix_ms()?,
            remediation: REBUILD_REQUIRED_REMEDIATION.to_string(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerClearOutcome {
    Cleared,
    Absent,
}

pub fn rebuild_required_marker_path(vault_dir: &Path, panel_version: u32) -> PathBuf {
    panel_index_root(vault_dir, panel_version).join(REBUILD_REQUIRED_NAME)
}

/// Durably publish the marker (atomic rename + parent dir fsync), then read it
/// back and verify the bytes decode to the same marker — the quarantine-marker
/// round-trip pattern, so a silently failing filesystem cannot fake intent.
pub fn write_rebuild_required_marker(
    vault_dir: &Path,
    marker: &RebuildRequiredMarker,
) -> CliResult<PathBuf> {
    if marker.schema != REBUILD_REQUIRED_SCHEMA {
        return Err(stale(format!(
            "refusing to write rebuild-required marker with schema {}; expected {REBUILD_REQUIRED_SCHEMA}",
            marker.schema
        )));
    }
    let path = rebuild_required_marker_path(vault_dir, marker.panel_version);
    fs_io::write_json_atomic_durable(&path, marker)?;
    let readback =
        read_rebuild_required_marker(vault_dir, marker.panel_version)?.ok_or_else(|| {
            stale(format!(
                "rebuild-required marker {} missing immediately after durable write",
                path.display()
            ))
        })?;
    if &readback != marker {
        return Err(stale(format!(
            "rebuild-required marker {} readback does not match written intent (source={} vs {})",
            path.display(),
            readback.source,
            marker.source
        )));
    }
    Ok(path)
}

/// `Ok(None)` when no marker exists. An unreadable or wrong-schema marker is a
/// hard error: the file's presence means a mutation staked intent, so guessing
/// its content would hide exactly the state it exists to expose.
pub fn read_rebuild_required_marker(
    vault_dir: &Path,
    panel_version: u32,
) -> CliResult<Option<RebuildRequiredMarker>> {
    let path = rebuild_required_marker_path(vault_dir, panel_version);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(stale(format!(
                "read rebuild-required marker {} failed: {error}",
                path.display()
            )));
        }
    };
    let marker: RebuildRequiredMarker = serde_json::from_slice(&bytes).map_err(|error| {
        stale(format!(
            "rebuild-required marker {} is not valid JSON: {error}; treat derived search state as stale and run `calyx rebuild-search-index <vault>`",
            path.display()
        ))
    })?;
    if marker.schema != REBUILD_REQUIRED_SCHEMA {
        return Err(stale(format!(
            "rebuild-required marker {} has schema {}; expected {REBUILD_REQUIRED_SCHEMA}",
            path.display(),
            marker.schema
        )));
    }
    if marker.panel_version != panel_version {
        return Err(stale(format!(
            "rebuild-required marker {} declares panel {} but panel {panel_version} was requested",
            path.display(),
            marker.panel_version
        )));
    }
    Ok(Some(marker))
}

/// Clear the marker after a completed rebuild published a manifest at
/// `manifest_base_seq`. Refuses (loudly) to clear a marker demanding a newer
/// seq than the manifest provides — that would mask real staleness.
pub fn clear_rebuild_required_marker(
    vault_dir: &Path,
    panel_version: u32,
    manifest_base_seq: u64,
) -> CliResult<MarkerClearOutcome> {
    let Some(marker) = read_rebuild_required_marker(vault_dir, panel_version)? else {
        return Ok(MarkerClearOutcome::Absent);
    };
    if let Some(required) = marker.required_base_seq
        && manifest_base_seq < required
    {
        return Err(stale(format!(
            "refusing to clear rebuild-required marker {}: it requires base seq {required} but the manifest was rebuilt at {manifest_base_seq}; a newer commit still lacks derived indexes",
            rebuild_required_marker_path(vault_dir, panel_version).display()
        )));
    }
    fs_io::remove_file_durable(&rebuild_required_marker_path(vault_dir, panel_version))?;
    Ok(MarkerClearOutcome::Cleared)
}

/// Clear a marker only if this process wrote it. Used by the ingest skip path
/// (no new constellations, rebuild not needed) so an interrupted earlier run's
/// marker is never silently discarded by a later replay-only batch.
pub fn clear_rebuild_required_marker_if_owned(
    vault_dir: &Path,
    panel_version: u32,
) -> CliResult<MarkerClearOutcome> {
    let Some(marker) = read_rebuild_required_marker(vault_dir, panel_version)? else {
        return Ok(MarkerClearOutcome::Absent);
    };
    if marker.process_id != std::process::id() {
        return Ok(MarkerClearOutcome::Absent);
    }
    fs_io::remove_file_durable(&rebuild_required_marker_path(vault_dir, panel_version))?;
    Ok(MarkerClearOutcome::Cleared)
}

/// One-line diagnostic used to enrich stale-derived errors so the operator sees
/// the recorded commit context, not just a seq mismatch.
pub(super) fn marker_error_context(vault_dir: &Path, panel_version: u32) -> String {
    match read_rebuild_required_marker(vault_dir, panel_version) {
        Ok(Some(marker)) => format!(
            "; rebuild-required marker present at {} (source={}, required_base_seq={}, session_id={}, batch_path={}, process_id={}, written_at_unix_ms={}): {}",
            rebuild_required_marker_path(vault_dir, panel_version).display(),
            marker.source,
            marker
                .required_base_seq
                .map(|seq| seq.to_string())
                .unwrap_or_else(|| "in-flight".to_string()),
            marker.session_id.as_deref().unwrap_or("<none>"),
            marker.batch_path.as_deref().unwrap_or("<none>"),
            marker.process_id,
            marker.written_at_unix_ms,
            marker.remediation
        ),
        Ok(None) => String::new(),
        Err(error) => format!(
            "; additionally the rebuild-required marker could not be read: {}",
            error.message()
        ),
    }
}

fn unix_ms() -> CliResult<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            stale(format!(
                "system clock before UNIX epoch while writing rebuild-required marker: {error}"
            ))
        })?
        .as_millis();
    u64::try_from(millis).map_err(|_| {
        stale("system clock overflow while writing rebuild-required marker".to_string())
    })
}
