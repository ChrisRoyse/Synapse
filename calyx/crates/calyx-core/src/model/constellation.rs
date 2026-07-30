//! Atomic Calyx constellation record.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{CxId, Modality, Result, SlotId, VaultId};

use super::{
    Anchor, CxFlags, InputRef, LedgerRef, SlotVector, Ts, validation::record_schema_error,
};

/// Leapable Vault contract key for a source chunk identifier.
pub const METADATA_CHUNK_ID: &str = "chunk_id";
/// Leapable Vault contract key for the owning database identifier.
pub const METADATA_DATABASE_NAME: &str = "database_name";
/// Source-system event timestamp in Unix seconds, when the source provided one.
pub const METADATA_SOURCE_EVENT_TIME_SECS: &str = "source_event_time_secs";
/// Verbatim source timestamp text or integer used to derive event seconds.
pub const METADATA_SOURCE_EVENT_TIME_RAW: &str = "source_event_time_raw";
/// Temporal lane activation state for this constellation.
pub const METADATA_TEMPORAL_LANE_STATE: &str = "temporal_lane_state";
/// Stable reason code when temporal lanes are inactive for this constellation.
pub const METADATA_TEMPORAL_INACTIVE_REASON: &str = "temporal_inactive_reason";
/// Documented ordering source used by sequence/positional temporal slots.
pub const METADATA_SOURCE_SEQUENCE: &str = "source_sequence";
/// Temporal lanes have a real event-time source and may be scored.
pub const TEMPORAL_LANE_ACTIVE: &str = "active";
/// Temporal lanes are suppressed because no real event-time source exists.
pub const TEMPORAL_LANE_INACTIVE: &str = "inactive";
/// Stable reason for timeless source rows.
pub const TEMPORAL_MISSING_CREATED_AT: &str = "source_missing_created_at";

/// One input measured by one panel of frozen lenses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Constellation {
    /// Content-addressed constellation id.
    pub cx_id: CxId,
    /// Owning vault.
    pub vault_id: VaultId,
    /// Panel version used for this measurement.
    pub panel_version: u32,
    /// Server-stamped creation timestamp.
    pub created_at: Ts,
    /// Hash and optional raw-input pointer.
    pub input_ref: InputRef,
    /// Input modality.
    pub modality: Modality,
    /// Per-slot vectors; absent slots are explicit values.
    pub slots: BTreeMap<SlotId, SlotVector>,
    /// Scalar measurements derived at ingest.
    pub scalars: BTreeMap<String, f64>,
    /// Verbatim string identifiers and source-system metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Grounded outcomes observed for this input.
    pub anchors: Vec<Anchor>,
    /// Ledger entry proving input -> lens -> constellation lineage.
    pub provenance: LedgerRef,
    /// Trust and degradation flags for this constellation.
    pub flags: CxFlags,
}

impl Constellation {
    /// Returns a string metadata value without allocating.
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(String::as_str)
    }

    /// Returns the preserved Leapable chunk identifier, when this row came from a Vault chunk.
    pub fn chunk_id(&self) -> Option<&str> {
        self.metadata_value(METADATA_CHUNK_ID)
    }

    /// Returns the preserved Leapable database identifier, when this row came from a Vault chunk.
    pub fn database_name(&self) -> Option<&str> {
        self.metadata_value(METADATA_DATABASE_NAME)
    }

    /// Returns the preserved source event timestamp, suppressing explicitly
    /// inactive temporal lanes instead of falling back to storage time.
    pub fn source_event_time_secs(&self) -> Option<i64> {
        if self.metadata_value(METADATA_TEMPORAL_LANE_STATE) == Some(TEMPORAL_LANE_INACTIVE) {
            return None;
        }
        self.metadata_value(METADATA_SOURCE_EVENT_TIME_SECS)
            .and_then(|value| value.parse::<i64>().ok())
    }

    /// Returns the preserved source event timestamp at full **nanosecond**
    /// precision, when the writer recorded one as an integer nanosecond count.
    ///
    /// [`METADATA_SOURCE_EVENT_TIME_RAW`] is documented as verbatim source text
    /// *or* an integer, so a non-integer value yields `None` rather than an
    /// error: it means this writer did not record nanoseconds, which callers
    /// must distinguish from a corrupt one. Callers that require nanoseconds
    /// should cross-check against [`Self::source_event_time_secs`] and fail
    /// loudly on disagreement — the two stamps are written together
    /// (`activate_temporal_lane`) and must agree by construction (issue #1893).
    ///
    /// Like [`Self::source_event_time_secs`], an explicitly inactive temporal
    /// lane is suppressed rather than substituted with storage time.
    pub fn source_event_time_nanos(&self) -> Option<u64> {
        if self.metadata_value(METADATA_TEMPORAL_LANE_STATE) == Some(TEMPORAL_LANE_INACTIVE) {
            return None;
        }
        self.metadata_value(METADATA_SOURCE_EVENT_TIME_RAW)
            .and_then(|value| value.parse::<u64>().ok())
    }

    /// Validates this record at storage/API boundaries.
    pub fn validate_schema(&self) -> Result<()> {
        if self.panel_version == 0 {
            return Err(record_schema_error(
                "constellation panel_version must be greater than zero",
            ));
        }
        for (slot, vector) in &self.slots {
            vector
                .validate_schema()
                .map_err(|err| record_schema_error(format!("slot {slot}: {}", err.message)))?;
        }
        for (key, value) in &self.scalars {
            if key.is_empty() {
                return Err(record_schema_error("scalar key must not be empty"));
            }
            if !value.is_finite() {
                return Err(record_schema_error(format!("scalar {key:?} is NaN or Inf")));
            }
        }
        for key in self.metadata.keys() {
            if key.is_empty() {
                return Err(record_schema_error("metadata key must not be empty"));
            }
        }
        for anchor in &self.anchors {
            anchor.validate_schema()?;
        }
        Ok(())
    }
}
