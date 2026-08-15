//! Durable, revision-guarded panel lifecycle state.
//!
//! The active-panel manifest is a serving default, not a catalog. Runtime lens
//! mutations therefore live in Registry CF under one row per logical panel so
//! changing a non-default panel cannot move the serving pointer (#1668).

use std::collections::BTreeMap;

use calyx_aster::cf::ColumnFamily;
use calyx_core::{
    AbsentReason, Constellation, CxId, Input, LensId, Panel, SlotId, SlotState, SlotVector, Ts,
};
use calyx_registry::{
    AlgorithmicLens, BackfillState, BackfillTask, BackfillTaskId, FrozenLensContract, LensRuntime,
    LensSpec, Registry, SlotSpec, SwapController, algorithmic_encoder, canonical_json_bytes,
    derive_runtime_contract_from_spec, measure_registry_batch_with_runtime_limit,
};
use serde::{Deserialize, Serialize};

use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

const SCHEMA_VERSION: u16 = 2;
const KEY_PREFIX: &[u8] = b"panel-lifecycle\0v1\0";
const MAX_CAS_ATTEMPTS: usize = 64;
const MAX_PANEL_NAME_BYTES: usize = 128;
const OPERATION_ID_HEX_BYTES: usize = 64;
const PANEL_LIFECYCLE_MEASURE_BATCH_LIMIT: usize = 256;

pub const SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID: &str = "SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID";
pub const SYNAPSE_CALYX_PANEL_LIFECYCLE_CONFLICT: &str = "SYNAPSE_CALYX_PANEL_LIFECYCLE_CONFLICT";
pub const SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED: &str =
    "SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED";

/// Durable source material required to reconstruct every added frozen lens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxAddedLens {
    pub operation_id: String,
    pub lens_spec: LensSpec,
    pub source_projection: SynapseCalyxSourceProjection,
}

/// Frozen mapping from an authoritative source row to the bytes a lens sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum SynapseCalyxSourceProjection {
    /// Measure the exact bytes stored by the authoritative source CF.
    RawSourceBytes,
    /// Decode the source JSON and re-encode the complete value canonically.
    WholeRecordCanonicalJson,
    /// Select one RFC 6901 JSON pointer from the decoded source record.
    JsonPointer {
        pointer: String,
        encoding: SynapseCalyxProjectedValueEncoding,
    },
    /// Whole-corpus measurement derived from one pinned source sequence.
    /// These rows are published atomically and cannot use source-row backfill.
    DerivedSnapshot { source_seq: u64, snapshot: u64 },
}

/// Stable byte encoding for a JSON-pointer projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxProjectedValueEncoding {
    /// String bytes, or the exact JSON lexical form of a number/bool/null.
    ScalarText,
    /// Canonical JSON bytes, including arrays and objects.
    CanonicalJson,
}

/// Authoritative lifecycle row for one logical panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxPanelLifecycleState {
    pub schema_version: u16,
    pub panel_name: String,
    pub controller: SwapController,
    pub added_lenses: BTreeMap<LensId, SynapseCalyxAddedLens>,
}

/// One graph row staged into the atomic derived-snapshot publication.
pub struct SynapseCalyxDerivedGraphRow {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// Exact immutable state for one whole-corpus derived snapshot.
pub struct SynapseCalyxDerivedSnapshotRequest<'a> {
    pub panel_name: &'a str,
    pub operation_id: &'a str,
    pub panel: Panel,
    pub registry: Registry,
    pub constellations: Vec<Constellation>,
    pub graph_rows: Vec<SynapseCalyxDerivedGraphRow>,
    pub source_seq: u64,
    pub snapshot: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxDerivedSnapshotReadback {
    pub panel_name: String,
    pub panel_version: u32,
    pub source_seq: u64,
    pub snapshot: u64,
    pub constellation_count: u64,
    pub graph_row_count: u64,
    pub committed_seq: u64,
    pub lifecycle_sha256: String,
}

/// One idempotent add-lens mutation request.
pub struct SynapseCalyxAddLensRequest<'a> {
    pub panel_name: &'a str,
    pub base_panel: Panel,
    pub operation_id: &'a str,
    pub slot_key: &'a str,
    pub lens_spec: LensSpec,
    pub source_projection: SynapseCalyxSourceProjection,
    pub candidates: &'a [CxId],
    pub now: Ts,
}

/// Committed mutation plus exact physical readback identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxAddLensReadback {
    pub state: SynapseCalyxPanelLifecycleState,
    pub slot_id: u16,
    pub lens_id: LensId,
    pub queued: u64,
    pub committed_seq: u64,
    pub value_sha256: String,
    pub existing_identical: bool,
}

/// One idempotent lifecycle-state transition.
pub struct SynapseCalyxSetLensStateRequest<'a> {
    pub panel_name: &'a str,
    pub operation_id: &'a str,
    pub slot_id: SlotId,
    pub state: SlotState,
    pub now: Ts,
}

/// Physical readback for a park, unpark, or retire transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxSetLensStateReadback {
    pub state: SynapseCalyxPanelLifecycleState,
    pub slot_id: u16,
    pub lens_id: LensId,
    pub lifecycle_state: SlotState,
    pub committed_seq: u64,
    pub value_sha256: String,
    pub existing_identical: bool,
}

/// Reproducible measurement of one authoritative row through one added lens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxAddedLensMeasurement {
    pub panel_name: String,
    pub panel_version: u32,
    pub lens_id: LensId,
    pub input_sha256: String,
    pub vector: SlotVector,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBackfillClaimReadback {
    pub state: SynapseCalyxPanelLifecycleState,
    pub tasks: Vec<BackfillTask>,
    pub recovered_in_flight: u64,
    pub committed_seq: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBackfillCompleteReadback {
    pub state: SynapseCalyxPanelLifecycleState,
    pub task_id: u64,
    pub committed_seq: u64,
    pub value_sha256: String,
}

impl SynapseCalyxVault {
    /// Atomically publishes a fingerprinted graph snapshot, its exact frozen
    /// panel contract, all derived constellations, and one ledger entry.
    ///
    /// # Errors
    ///
    /// Returns a structured error when validation, generation allocation, the
    /// atomic publication, or its independent physical readback fails.
    #[expect(
        clippy::too_many_lines,
        reason = "validation, generation allocation, atomic publication, and readback are one ordered snapshot transaction"
    )]
    pub fn publish_derived_snapshot(
        &self,
        request: &SynapseCalyxDerivedSnapshotRequest<'_>,
    ) -> Result<SynapseCalyxDerivedSnapshotReadback, SynapseCalyxError> {
        lifecycle_key(request.panel_name)?;
        validate_operation_id(request.operation_id)?;
        if request.constellations.is_empty() || request.graph_rows.is_empty() {
            return Err(invalid(
                "derived snapshot requires at least one graph row and one constellation",
            ));
        }
        let allocation =
            self.allocate_panel_generation(request.panel_name, request.operation_id)?;
        if allocation.panel_generation != request.panel.version {
            return Err(invalid(format!(
                "derived snapshot generation {} differs from operation allocation {}",
                request.panel.version, allocation.panel_generation
            )));
        }
        let key = lifecycle_key(request.panel_name)?;
        let mut added_lenses = BTreeMap::new();
        for snapshot_row in request.registry.lens_snapshots() {
            let spec = snapshot_row.spec.ok_or_else(|| {
                invalid(format!(
                    "derived panel lens {} has no durable LensSpec",
                    snapshot_row.lens_id
                ))
            })?;
            added_lenses.insert(
                snapshot_row.lens_id,
                SynapseCalyxAddedLens {
                    operation_id: request.operation_id.to_owned(),
                    lens_spec: spec,
                    source_projection: SynapseCalyxSourceProjection::DerivedSnapshot {
                        source_seq: request.source_seq,
                        snapshot: request.snapshot,
                    },
                },
            );
        }
        for slot in &request.panel.slots {
            if !added_lenses.contains_key(&slot.lens_id) {
                return Err(invalid(format!(
                    "derived panel slot {} references unregistered lens {}",
                    slot.slot_id, slot.lens_id
                )));
            }
        }
        let state = SynapseCalyxPanelLifecycleState {
            schema_version: SCHEMA_VERSION,
            panel_name: request.panel_name.to_owned(),
            controller: SwapController::new(request.panel.clone()),
            added_lenses,
        };
        let lifecycle_value = serde_json::to_vec(&state).map_err(|error| {
            invalid(format!(
                "encode derived lifecycle state for {}: {error}",
                request.panel_name
            ))
        })?;
        let existing = self.read_cf_latest(ColumnFamily::Registry, &key)?;
        let expected_revision = existing.as_deref().map(sha256_array);
        if let Some(existing) = existing.as_ref() {
            if existing.as_slice() == lifecycle_value {
                return derived_snapshot_readback(self, request, &lifecycle_value);
            }
            let prior: SynapseCalyxPanelLifecycleState =
                serde_json::from_slice(existing).map_err(|error| {
                    conflict(format!(
                        "panel {} has an undecodable lifecycle state: {error}",
                        request.panel_name
                    ))
                })?;
            let prior_is_derived = prior.added_lenses.values().all(|lens| {
                matches!(
                    lens.source_projection,
                    SynapseCalyxSourceProjection::DerivedSnapshot { .. }
                )
            });
            if prior.panel_name != request.panel_name
                || !prior_is_derived
                || prior.controller.panel().version >= request.panel.version
            {
                return Err(conflict(format!(
                    "panel {} lifecycle cannot advance from generation {} to {} (derived={prior_is_derived})",
                    request.panel_name,
                    prior.controller.panel().version,
                    request.panel.version
                )));
            }
        }
        let mut rows = request
            .graph_rows
            .iter()
            .map(|row| (ColumnFamily::Graph, row.key.clone(), row.value.clone()))
            .collect::<Vec<_>>();
        rows.push((ColumnFamily::Registry, key.clone(), lifecycle_value.clone()));
        let payload = canonical_json_bytes(&serde_json::json!({
            "operation": "publish_derived_snapshot",
            "operation_id": request.operation_id,
            "panel_name": request.panel_name,
            "panel_version": request.panel.version,
            "source_seq": request.source_seq,
            "snapshot": request.snapshot,
            "constellation_count": request.constellations.len(),
            "graph_row_count": request.graph_rows.len(),
        }))
        .map_err(|error| SynapseCalyxError::from_calyx("encode derived snapshot ledger", &error))?;
        self.vault
            .put_batch_with_ingest_ledger_and_derived_rows(
                request.constellations.clone(),
                calyx_ledger::SubjectId::Query(request.operation_id.as_bytes().to_vec()),
                payload,
                calyx_ledger::ActorId::Service("synapse-derived-state".to_owned()),
                rows,
                Some(calyx_aster::vault::DerivedRegistryRevisionGuard {
                    key,
                    expected_revision,
                }),
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx("publish atomic derived graph snapshot", &error)
            })?;
        derived_snapshot_readback(self, request, &lifecycle_value)
    }
    /// Lists every physical Base id in one panel, bounded before allocation.
    ///
    /// # Errors
    ///
    /// Fails when `max_records` is zero, a Base row cannot decode, or the panel
    /// contains more records than the caller's declared durable-queue budget.
    pub fn panel_constellation_ids(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<Vec<CxId>, SynapseCalyxError> {
        if max_records == 0 {
            return Err(invalid("panel constellation id limit must be positive"));
        }
        let mut ids = Vec::new();
        self.with_panel_read_snapshot(
            panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| {
                self.walk_panel_base_snapshot(snapshot, panel_version, |_snapshot, _key, value| {
                let base = calyx_aster::vault::encode::decode_constellation_base_projection(value)
                    .map_err(|error| {
                        SynapseCalyxError::from_calyx(
                            "decode Base row projection for lifecycle candidate census",
                            &error,
                        )
                    })?;
                if base.panel_version == panel_version {
                    if ids.len() >= max_records {
                        return Err(invalid(format!(
                            "panel {panel_version} exceeds lifecycle candidate limit {max_records}"
                        )));
                    }
                    ids.push(base.cx_id);
                }
                Ok(crate::SynapseCalyxWalkStep::Continue)
            })
            },
        )?;
        Ok(ids)
    }

    /// Reads one panel's lifecycle row from the physical Registry CF.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Registry row cannot be read or decoded.
    pub fn read_panel_lifecycle(
        &self,
        panel_name: &str,
    ) -> Result<Option<SynapseCalyxPanelLifecycleState>, SynapseCalyxError> {
        let key = lifecycle_key(panel_name)?;
        self.read_cf_latest(ColumnFamily::Registry, &key)?
            .map(|bytes| decode_state(&bytes, panel_name))
            .transpose()
    }

    /// Reconstructs every dynamically added frozen lens into a supplied base
    /// registry and returns the durable lifecycle panel definition.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the lifecycle row is corrupt or any
    /// frozen lens cannot be reconstructed with its exact stored identity.
    pub fn reconstruct_panel_lifecycle_contract(
        &self,
        panel_name: &str,
        mut registry: Registry,
    ) -> Result<Option<calyx_registry::VaultPanelState>, SynapseCalyxError> {
        let Some(state) = self.read_panel_lifecycle(panel_name)? else {
            return Ok(None);
        };
        for (expected_id, added) in &state.added_lenses {
            let (lens, contract) = runtime_lens(&added.lens_spec)?;
            let observed = registry
                .register_frozen_with_spec(lens, contract, added.lens_spec.clone())
                .map_err(|error| {
                    SynapseCalyxError::from_calyx(
                        "reconstruct durable panel lifecycle registry",
                        &error,
                    )
                })?;
            if observed != *expected_id {
                return Err(invalid(format!(
                    "reconstructed lifecycle lens {observed} differs from stored id {expected_id}"
                )));
            }
        }
        Ok(Some(calyx_registry::VaultPanelState {
            panel: state.controller.panel().clone(),
            registry,
            registry_snapshot: None,
        }))
    }

    /// Adds one deterministic lens and durably enqueues historical records.
    ///
    /// The generation allocation and operation id are idempotent. The panel
    /// row itself is CAS guarded, so concurrent mutations are retried from the
    /// winning physical state and never overwrite one another.
    ///
    /// # Errors
    ///
    /// Returns a structured error when validation, allocation, registry
    /// reconstruction, CAS publication, or physical readback fails.
    #[expect(
        clippy::too_many_lines,
        reason = "generation allocation, CAS mutation, backfill enqueue, and physical readback are one ordered lifecycle transaction"
    )]
    pub fn add_panel_lens(
        &self,
        request: &SynapseCalyxAddLensRequest<'_>,
    ) -> Result<SynapseCalyxAddLensReadback, SynapseCalyxError> {
        validate_request(request)?;
        let panel_name = request.panel_name.to_owned();
        let key = lifecycle_key(&panel_name)?;
        let (lens, frozen_contract) = runtime_lens(&request.lens_spec)?;
        self.reserve_panel_generations(&[(panel_name.clone(), request.base_panel.version)])?;
        let allocation = self.allocate_panel_generation(&panel_name, request.operation_id)?;

        for _attempt in 0..MAX_CAS_ATTEMPTS {
            let revisioned = self.read_cf_latest_revisioned(ColumnFamily::Registry, &key)?;
            let (mut state, expected_revision) = match revisioned {
                Some(row) => (
                    decode_state(&row.value, &panel_name)?,
                    Some(row.revision_sha256),
                ),
                None => (
                    SynapseCalyxPanelLifecycleState {
                        schema_version: SCHEMA_VERSION,
                        panel_name: panel_name.clone(),
                        controller: SwapController::new(request.base_panel.clone()),
                        added_lenses: BTreeMap::new(),
                    },
                    None,
                ),
            };
            ensure_base_compatible(&state, &request.base_panel)?;
            let mut registry = Registry::new();
            let lens_id = registry
                .register_frozen_with_spec(
                    lens.clone(),
                    frozen_contract.clone(),
                    request.lens_spec.clone(),
                )
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("register added panel lens", &error)
                })?;
            let outcome = state
                .controller
                .add_lens(
                    &registry,
                    SlotSpec {
                        key: request.slot_key.to_owned(),
                        lens_id,
                        shape: request.lens_spec.output,
                        modality: request.lens_spec.modality,
                        asymmetry: request.lens_spec.asymmetry,
                        quant: request.lens_spec.quant_default,
                        axis: request.lens_spec.axis.clone(),
                        retrieval_only: request.lens_spec.retrieval_only,
                        excluded_from_dedup: request.lens_spec.excluded_from_dedup,
                    },
                    request
                        .candidates
                        .iter()
                        .copied()
                        .map(|cx_id| calyx_registry::BackfillCandidate { cx_id, priority: 0 }),
                    request.now,
                    allocation.panel_generation,
                )
                .map_err(|error| SynapseCalyxError::from_calyx("add lens to panel", &error))?;
            state.added_lenses.insert(
                lens_id,
                SynapseCalyxAddedLens {
                    operation_id: request.operation_id.to_owned(),
                    lens_spec: request.lens_spec.clone(),
                    source_projection: request.source_projection.clone(),
                },
            );
            validate_state(&state)?;
            if outcome.index.ready && allocation.existing_identical {
                let stored = self
                    .read_cf_latest(ColumnFamily::Registry, &key)?
                    .ok_or_else(|| {
                        conflict(format!(
                            "idempotent lifecycle row for {panel_name} is absent"
                        ))
                    })?;
                let readback = decode_state(&stored, &panel_name)?;
                if readback != state {
                    return Err(conflict(format!(
                        "operation {} is allocated but its lifecycle state differs",
                        request.operation_id
                    )));
                }
                return Ok(SynapseCalyxAddLensReadback {
                    state: readback,
                    slot_id: outcome.slot.slot_id.get(),
                    lens_id,
                    queued: 0,
                    committed_seq: allocation.committed_seq,
                    value_sha256: hex_sha256(&stored),
                    existing_identical: true,
                });
            }
            let encoded = serde_json::to_vec(&state).map_err(|error| {
                invalid(format!("encode lifecycle row for {panel_name}: {error}"))
            })?;
            let write = self.write_cf_batch_if_revision(
                ColumnFamily::Registry,
                &key,
                expected_revision,
                vec![SynapseCalyxCfWrite {
                    cf: ColumnFamily::Registry,
                    key: key.clone(),
                    value: encoded,
                }],
            )?;
            if !write.applied {
                continue;
            }
            let stored = self
                .read_cf_latest(ColumnFamily::Registry, &key)?
                .ok_or_else(|| {
                    conflict(format!(
                        "committed lifecycle row for {panel_name} is absent"
                    ))
                })?;
            let readback = decode_state(&stored, &panel_name)?;
            if readback != state {
                return Err(conflict(format!(
                    "lifecycle row for {panel_name} differs at committed sequence {}",
                    write.committed_seq
                )));
            }
            return Ok(SynapseCalyxAddLensReadback {
                state: readback,
                slot_id: outcome.slot.slot_id.get(),
                lens_id,
                queued: outcome.queued as u64,
                committed_seq: write.committed_seq,
                value_sha256: hex_sha256(&stored),
                existing_identical: allocation.existing_identical && outcome.index.ready,
            });
        }
        Err(conflict(format!(
            "panel {panel_name} lifecycle mutation exceeded {MAX_CAS_ATTEMPTS} atomic retries"
        )))
    }

    /// Parks, unparks, or irreversibly retires one durable panel slot.
    ///
    /// # Errors
    ///
    /// Returns a structured error when validation, CAS mutation, ledger
    /// publication, or physical readback fails.
    #[expect(
        clippy::too_many_lines,
        reason = "slot transition validation, CAS mutation, ledger publication, and readback are one ordered lifecycle transaction"
    )]
    pub fn set_panel_lens_state(
        &self,
        request: &SynapseCalyxSetLensStateRequest<'_>,
    ) -> Result<SynapseCalyxSetLensStateReadback, SynapseCalyxError> {
        lifecycle_key(request.panel_name)?;
        validate_operation_id(request.operation_id)?;
        let key = lifecycle_key(request.panel_name)?;
        for _attempt in 0..MAX_CAS_ATTEMPTS {
            let row = self
                .read_cf_latest_revisioned(ColumnFamily::Registry, &key)?
                .ok_or_else(|| {
                    conflict(format!(
                        "panel {:?} has no durable lifecycle row",
                        request.panel_name
                    ))
                })?;
            let mut state = decode_state(&row.value, request.panel_name)?;
            let (existing_slot_id, existing_lens_id, existing_state) = state
                .controller
                .panel()
                .slots
                .iter()
                .find(|slot| slot.slot_id == request.slot_id)
                .ok_or_else(|| {
                    invalid(format!(
                        "slot {} is not present in panel {:?}",
                        request.slot_id, request.panel_name
                    ))
                })
                .map(|slot| (slot.slot_id, slot.lens_id, slot.state))?;
            if existing_state == request.state {
                return Ok(SynapseCalyxSetLensStateReadback {
                    state,
                    slot_id: existing_slot_id.get(),
                    lens_id: existing_lens_id,
                    lifecycle_state: existing_state,
                    committed_seq: self.latest_seq(),
                    value_sha256: hex_sha256(&row.value),
                    existing_identical: true,
                });
            }
            if existing_state == SlotState::Retired {
                return Err(SynapseCalyxError::from_calyx(
                    "change durable panel lens state",
                    &calyx_core::CalyxError::lens_frozen_violation(format!(
                        "slot {} is retired and cannot transition to {:?}",
                        request.slot_id, request.state
                    )),
                ));
            }
            let allocation =
                self.allocate_panel_generation(request.panel_name, request.operation_id)?;
            let outcome = match request.state {
                SlotState::Active => state.controller.unpark_lens(
                    request.slot_id,
                    request.now,
                    allocation.panel_generation,
                ),
                SlotState::Parked => state.controller.park_lens(
                    request.slot_id,
                    request.now,
                    allocation.panel_generation,
                ),
                SlotState::Retired => state.controller.retire_lens(
                    request.slot_id,
                    request.now,
                    allocation.panel_generation,
                ),
            }
            .map_err(|error| {
                SynapseCalyxError::from_calyx("change durable panel lens state", &error)
            })?;
            validate_state(&state)?;
            let encoded = serde_json::to_vec(&state).map_err(|error| {
                invalid(format!(
                    "encode lifecycle row for {}: {error}",
                    request.panel_name
                ))
            })?;
            let write = self.write_cf_batch_if_revision(
                ColumnFamily::Registry,
                &key,
                Some(row.revision_sha256),
                vec![SynapseCalyxCfWrite {
                    cf: ColumnFamily::Registry,
                    key: key.clone(),
                    value: encoded,
                }],
            )?;
            if !write.applied {
                continue;
            }
            let stored = self
                .read_cf_latest(ColumnFamily::Registry, &key)?
                .ok_or_else(|| conflict("committed lifecycle transition row is absent"))?;
            let readback = decode_state(&stored, request.panel_name)?;
            if readback != state {
                return Err(conflict(format!(
                    "lifecycle transition differs at committed sequence {}",
                    write.committed_seq
                )));
            }
            return Ok(SynapseCalyxSetLensStateReadback {
                state: readback,
                slot_id: outcome.slot_id.get(),
                lens_id: outcome.lens_id,
                lifecycle_state: outcome.state,
                committed_seq: write.committed_seq,
                value_sha256: hex_sha256(&stored),
                existing_identical: false,
            });
        }
        Err(conflict(format!(
            "panel {} lifecycle transition exceeded {MAX_CAS_ATTEMPTS} atomic retries",
            request.panel_name
        )))
    }

    /// Reconstructs one added frozen lens and measures authoritative source bytes.
    ///
    /// # Errors
    ///
    /// Fails closed when the lifecycle row or lens is absent/retired, its frozen
    /// runtime no longer reproduces, source JSON/projection is invalid, or the
    /// measured vector violates the declared shape/numerical contract.
    pub fn measure_added_panel_lens(
        &self,
        panel_name: &str,
        lens_id: LensId,
        source_bytes: &[u8],
    ) -> Result<SynapseCalyxAddedLensMeasurement, SynapseCalyxError> {
        let mut measured =
            self.measure_added_panel_lens_batch(panel_name, lens_id, &[source_bytes])?;
        if measured.len() != 1 {
            return Err(invalid(format!(
                "single added-lens measurement returned {} rows instead of 1",
                measured.len()
            )));
        }
        Ok(measured.remove(0))
    }

    /// Reconstructs one added frozen lens once and measures an ordered source
    /// page through the registry's runtime-limited native batch path.
    ///
    /// # Errors
    ///
    /// Fails closed for an empty batch, absent/retired lens state, projection or
    /// runtime errors, reconstructed-id drift, or output cardinality mismatch.
    pub fn measure_added_panel_lens_batch(
        &self,
        panel_name: &str,
        lens_id: LensId,
        source_rows: &[&[u8]],
    ) -> Result<Vec<SynapseCalyxAddedLensMeasurement>, SynapseCalyxError> {
        if source_rows.is_empty() {
            return Err(invalid(
                "added panel lens batch must contain at least one source row",
            ));
        }
        let state = self.read_panel_lifecycle(panel_name)?.ok_or_else(|| {
            conflict(format!("panel {panel_name:?} has no durable lifecycle row"))
        })?;
        let added = state.added_lenses.get(&lens_id).ok_or_else(|| {
            invalid(format!(
                "lens {lens_id} is not an added lens on panel {panel_name:?}"
            ))
        })?;
        let slot = state
            .controller
            .panel()
            .slots
            .iter()
            .find(|slot| slot.lens_id == lens_id)
            .ok_or_else(|| invalid(format!("lens {lens_id} has no durable panel slot")))?;
        if slot.state == SlotState::Retired {
            return Err(SynapseCalyxError::from_calyx(
                "measure added panel lens",
                &calyx_core::CalyxError::lens_frozen_violation(format!(
                    "lens {lens_id} is retired and cannot measure new source rows"
                )),
            ));
        }
        let (lens, contract) = runtime_lens(&added.lens_spec)?;
        let mut registry = Registry::new();
        let registered_id = registry
            .register_frozen_with_spec(lens, contract, added.lens_spec.clone())
            .map_err(|error| {
                SynapseCalyxError::from_calyx("reconstruct added panel lens", &error)
            })?;
        if registered_id != lens_id {
            return Err(invalid(format!(
                "reconstructed lens id {registered_id} differs from lifecycle id {lens_id}"
            )));
        }
        let projected = source_rows
            .iter()
            .map(|source_bytes| project_source_bytes(&added.source_projection, source_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        let inputs = projected
            .iter()
            .map(|bytes| Input::new(added.lens_spec.modality, bytes.clone()))
            .collect::<Vec<_>>();
        let vectors = measure_registry_batch_with_runtime_limit(
            &registry,
            lens_id,
            &inputs,
            Some(PANEL_LIFECYCLE_MEASURE_BATCH_LIMIT),
        )
        .map_err(|error| SynapseCalyxError::from_calyx("measure added panel lens", &error))?;
        if vectors.len() != projected.len() {
            return Err(invalid(format!(
                "added panel lens batch returned {} vectors for {} projected inputs",
                vectors.len(),
                projected.len()
            )));
        }
        Ok(projected
            .into_iter()
            .zip(vectors)
            .map(|(projected, vector)| SynapseCalyxAddedLensMeasurement {
                panel_name: panel_name.to_owned(),
                panel_version: state.controller.panel().version,
                lens_id,
                input_sha256: hex_sha256(&projected),
                vector,
            })
            .collect())
    }

    /// Re-measures one authoritative input into the latest durable lifecycle
    /// generation while preserving the supplied base generation as history.
    ///
    /// Returns `None` when the panel has never been mutated. Active added
    /// lenses are measured immediately; parked and retired slots are carried as
    /// explicit inactive absences so a zero vector can never be inferred.
    ///
    /// # Errors
    ///
    /// Returns a structured error when lifecycle state cannot be read, an active
    /// added lens cannot be measured, or the materialized schema is invalid.
    pub fn materialize_panel_lifecycle_generation(
        &self,
        panel_name: &str,
        identity_input: &[u8],
        source_bytes: &[u8],
        base: &Constellation,
    ) -> Result<Option<Constellation>, SynapseCalyxError> {
        let mut materialized = self.materialize_panel_lifecycle_generation_batch(
            panel_name,
            &[(identity_input, source_bytes, base)],
        )?;
        if materialized.len() != 1 {
            return Err(invalid(format!(
                "single lifecycle materialization returned {} rows instead of 1",
                materialized.len()
            )));
        }
        Ok(materialized.remove(0))
    }

    /// Re-measures an ordered source page into the latest durable lifecycle
    /// generation, grouping every active added lens through one registry batch.
    ///
    /// # Errors
    ///
    /// Returns a structured error for an empty page, generation inversion,
    /// added-lens batch failure/cardinality drift, or invalid materialized schema.
    pub fn materialize_panel_lifecycle_generation_batch(
        &self,
        panel_name: &str,
        rows: &[(&[u8], &[u8], &Constellation)],
    ) -> Result<Vec<Option<Constellation>>, SynapseCalyxError> {
        if rows.is_empty() {
            return Err(invalid(
                "panel lifecycle materialization batch must contain at least one source row",
            ));
        }
        let Some(state) = self.read_panel_lifecycle(panel_name)? else {
            return Ok(vec![None; rows.len()]);
        };
        let panel = state.controller.panel();
        let mut materialized = Vec::with_capacity(rows.len());
        for (row_index, (identity_input, _source_bytes, base)) in rows.iter().enumerate() {
            if base.panel_version > panel.version {
                return Err(conflict(format!(
                    "base constellation generation {} is newer than lifecycle generation {} for {panel_name:?} at row_index={row_index}",
                    base.panel_version, panel.version
                )));
            }
            let mut row = (*base).clone();
            if row.panel_version < panel.version {
                row.cx_id = self.cx_id_for_input(identity_input, panel.version);
                row.panel_version = panel.version;
            }
            materialized.push(row);
        }
        for slot in &panel.slots {
            let missing = materialized
                .iter()
                .enumerate()
                .filter_map(|(row_index, row)| {
                    (!row.slots.contains_key(&slot.slot_id)).then_some(row_index)
                })
                .collect::<Vec<_>>();
            if missing.is_empty() {
                continue;
            }
            match slot.state {
                SlotState::Active => {
                    let source_rows = missing
                        .iter()
                        .map(|row_index| rows[*row_index].1)
                        .collect::<Vec<_>>();
                    let measured = self.measure_added_panel_lens_batch(
                        panel_name,
                        slot.lens_id,
                        &source_rows,
                    )?;
                    if measured.len() != missing.len() {
                        return Err(invalid(format!(
                            "lifecycle lens {} returned {} measurements for {} missing rows",
                            slot.lens_id,
                            measured.len(),
                            missing.len()
                        )));
                    }
                    for (row_index, measurement) in missing.into_iter().zip(measured) {
                        materialized[row_index]
                            .slots
                            .insert(slot.slot_id, measurement.vector);
                    }
                }
                SlotState::Parked | SlotState::Retired => {
                    let vector = SlotVector::Absent {
                        reason: AbsentReason::LensInactive,
                    };
                    for row_index in missing {
                        materialized[row_index]
                            .slots
                            .insert(slot.slot_id, vector.clone());
                    }
                }
            }
        }
        let state_after = self.read_panel_lifecycle(panel_name)?;
        if state_after.as_ref() != Some(&state) {
            return Err(conflict(format!(
                "panel {panel_name:?} lifecycle state changed while its {}-row measurement batch was in flight; refusing to publish vectors against a mixed contract",
                rows.len()
            )));
        }
        for (row_index, row) in materialized.iter().enumerate() {
            row.validate_schema().map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!(
                        "validate materialized panel lifecycle generation row_index={row_index}"
                    ),
                    &error,
                )
            })?;
        }
        Ok(materialized.into_iter().map(Some).collect())
    }

    /// Atomically recovers abandoned claims and claims a bounded next batch.
    ///
    /// # Errors
    ///
    /// Returns a structured error for an invalid limit, abandoned work without
    /// explicit recovery, CAS exhaustion, or a failed physical readback.
    pub fn claim_panel_backfill(
        &self,
        panel_name: &str,
        limit: usize,
        recover_in_flight: bool,
    ) -> Result<SynapseCalyxBackfillClaimReadback, SynapseCalyxError> {
        if !(1..=256).contains(&limit) {
            return Err(invalid("backfill claim limit must be within 1..=256"));
        }
        let key = lifecycle_key(panel_name)?;
        for _attempt in 0..MAX_CAS_ATTEMPTS {
            let row = self
                .read_cf_latest_revisioned(ColumnFamily::Registry, &key)?
                .ok_or_else(|| {
                    conflict(format!("panel {panel_name:?} has no durable lifecycle row"))
                })?;
            let mut state = decode_state(&row.value, panel_name)?;
            let abandoned = state
                .controller
                .queue()
                .tasks()
                .filter(|task| task.state == BackfillState::InFlight)
                .map(|task| task.id)
                .collect::<Vec<_>>();
            if !abandoned.is_empty() && !recover_in_flight {
                return Err(conflict(format!(
                    "panel {panel_name:?} has {} in-flight backfill tasks; restart recovery must be explicitly requested",
                    abandoned.len()
                )));
            }
            for id in &abandoned {
                state.controller.queue_mut().retry(*id).map_err(|error| {
                    SynapseCalyxError::from_calyx("recover abandoned panel backfill task", &error)
                })?;
            }
            let tasks = state.controller.queue_mut().claim_batch(limit);
            if tasks.is_empty() && abandoned.is_empty() {
                return Ok(SynapseCalyxBackfillClaimReadback {
                    state,
                    tasks,
                    recovered_in_flight: 0,
                    committed_seq: self.latest_seq(),
                    value_sha256: hex_sha256(&row.value),
                });
            }
            let encoded = serde_json::to_vec(&state).map_err(|error| {
                invalid(format!(
                    "encode backfill claim state for {panel_name}: {error}"
                ))
            })?;
            let write = self.write_cf_batch_if_revision(
                ColumnFamily::Registry,
                &key,
                Some(row.revision_sha256),
                vec![SynapseCalyxCfWrite {
                    cf: ColumnFamily::Registry,
                    key: key.clone(),
                    value: encoded,
                }],
            )?;
            if !write.applied {
                continue;
            }
            let stored = self
                .read_cf_latest(ColumnFamily::Registry, &key)?
                .ok_or_else(|| conflict("committed backfill claim row is absent"))?;
            let readback = decode_state(&stored, panel_name)?;
            if readback != state {
                return Err(conflict(
                    "backfill claim physical readback differs from committed state",
                ));
            }
            return Ok(SynapseCalyxBackfillClaimReadback {
                state: readback,
                tasks,
                recovered_in_flight: abandoned.len() as u64,
                committed_seq: write.committed_seq,
                value_sha256: hex_sha256(&stored),
            });
        }
        Err(conflict(format!(
            "panel {panel_name:?} backfill claim exceeded {MAX_CAS_ATTEMPTS} atomic retries"
        )))
    }

    /// Marks one physically verified task complete in the durable queue.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the task is absent or unverified, or when
    /// CAS publication and physical readback fail.
    pub fn complete_panel_backfill_task(
        &self,
        panel_name: &str,
        task_id: BackfillTaskId,
    ) -> Result<SynapseCalyxBackfillCompleteReadback, SynapseCalyxError> {
        let key = lifecycle_key(panel_name)?;
        for _attempt in 0..MAX_CAS_ATTEMPTS {
            let row = self
                .read_cf_latest_revisioned(ColumnFamily::Registry, &key)?
                .ok_or_else(|| {
                    conflict(format!("panel {panel_name:?} has no durable lifecycle row"))
                })?;
            let mut state = decode_state(&row.value, panel_name)?;
            let task = state
                .controller
                .queue()
                .tasks()
                .find(|task| task.id == task_id)
                .cloned()
                .ok_or_else(|| invalid(format!("backfill task {} is absent", task_id.get())))?;
            if task.state == BackfillState::Complete {
                return Ok(SynapseCalyxBackfillCompleteReadback {
                    state,
                    task_id: task_id.get(),
                    committed_seq: self.latest_seq(),
                    value_sha256: hex_sha256(&row.value),
                });
            }
            if task.state != BackfillState::InFlight {
                return Err(conflict(format!(
                    "backfill task {} is {:?}, not in_flight",
                    task_id.get(),
                    task.state
                )));
            }
            state
                .controller
                .queue_mut()
                .complete(task_id)
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("complete durable panel backfill task", &error)
                })?;
            let encoded = serde_json::to_vec(&state).map_err(|error| {
                invalid(format!(
                    "encode backfill completion for {panel_name}: {error}"
                ))
            })?;
            let write = self.write_cf_batch_if_revision(
                ColumnFamily::Registry,
                &key,
                Some(row.revision_sha256),
                vec![SynapseCalyxCfWrite {
                    cf: ColumnFamily::Registry,
                    key: key.clone(),
                    value: encoded,
                }],
            )?;
            if !write.applied {
                continue;
            }
            let stored = self
                .read_cf_latest(ColumnFamily::Registry, &key)?
                .ok_or_else(|| conflict("committed backfill completion row is absent"))?;
            let readback = decode_state(&stored, panel_name)?;
            if readback != state {
                return Err(conflict(
                    "backfill completion physical readback differs from committed state",
                ));
            }
            return Ok(SynapseCalyxBackfillCompleteReadback {
                state: readback,
                task_id: task_id.get(),
                committed_seq: write.committed_seq,
                value_sha256: hex_sha256(&stored),
            });
        }
        Err(conflict(format!(
            "panel {panel_name:?} backfill completion exceeded {MAX_CAS_ATTEMPTS} atomic retries"
        )))
    }
}

fn project_source_bytes(
    projection: &SynapseCalyxSourceProjection,
    source_bytes: &[u8],
) -> Result<Vec<u8>, SynapseCalyxError> {
    match projection {
        SynapseCalyxSourceProjection::RawSourceBytes => Ok(source_bytes.to_vec()),
        SynapseCalyxSourceProjection::WholeRecordCanonicalJson => {
            canonical_json_bytes(&decode_source_json(source_bytes)?).map_err(|error| {
                SynapseCalyxError::from_calyx("canonicalize whole source record", &error)
            })
        }
        SynapseCalyxSourceProjection::JsonPointer { pointer, encoding } => {
            let value = decode_source_json(source_bytes)?;
            let selected = value.pointer(pointer).ok_or_else(|| {
                invalid(format!("source JSON has no value at pointer {pointer:?}"))
            })?;
            match encoding {
                SynapseCalyxProjectedValueEncoding::CanonicalJson => canonical_json_bytes(selected)
                    .map_err(|error| {
                        SynapseCalyxError::from_calyx(
                            "canonicalize JSON-pointer source value",
                            &error,
                        )
                    }),
                SynapseCalyxProjectedValueEncoding::ScalarText => match selected {
                    serde_json::Value::String(value) => Ok(value.as_bytes().to_vec()),
                    serde_json::Value::Number(_)
                    | serde_json::Value::Bool(_)
                    | serde_json::Value::Null => canonical_json_bytes(selected).map_err(|error| {
                        SynapseCalyxError::from_calyx(
                            "encode JSON-pointer scalar source value",
                            &error,
                        )
                    }),
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                        Err(invalid(format!(
                            "source JSON pointer {pointer:?} selected a composite value for scalar_text encoding"
                        )))
                    }
                },
            }
        }
        SynapseCalyxSourceProjection::DerivedSnapshot {
            source_seq,
            snapshot,
        } => Err(SynapseCalyxError::new(
            SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID,
            format!(
                "lens is derived from whole snapshot {snapshot} at source sequence {source_seq}, not one authoritative source row"
            ),
            "rebuild and atomically publish a new derived snapshot generation; source-row backfill is not valid for whole-graph measurements",
        )),
    }
}

fn derived_snapshot_readback(
    vault: &SynapseCalyxVault,
    request: &SynapseCalyxDerivedSnapshotRequest<'_>,
    lifecycle_value: &[u8],
) -> Result<SynapseCalyxDerivedSnapshotReadback, SynapseCalyxError> {
    let stored = vault
        .read_cf_latest(ColumnFamily::Registry, &lifecycle_key(request.panel_name)?)?
        .ok_or_else(|| conflict("derived lifecycle row is absent after publication"))?;
    if stored != lifecycle_value {
        return Err(conflict(
            "derived lifecycle physical readback differs from published bytes",
        ));
    }
    for row in &request.graph_rows {
        let observed = vault
            .read_cf_latest(ColumnFamily::Graph, &row.key)?
            .ok_or_else(|| conflict("derived Graph row is absent after publication"))?;
        if observed != row.value {
            return Err(conflict(
                "derived Graph row physical readback differs from published bytes",
            ));
        }
    }
    for constellation in &request.constellations {
        let hydrated = vault
            .hydrate_constellation_latest(constellation.cx_id)
            .map_err(|error| {
                SynapseCalyxError::new(
                    error.code,
                    format!(
                        "read back derived constellation {}: {}",
                        constellation.cx_id, error.message
                    ),
                    error.remediation,
                )
            })?;
        // Aster assigns the batch's authoritative ledger reference at commit;
        // every other byte must match the caller's measured constellation.
        let mut expected = constellation.clone();
        expected.provenance = hydrated.provenance.clone();
        if hydrated != expected {
            return Err(conflict(
                "derived constellation physical Base/Slot readback differs from published value",
            ));
        }
    }
    Ok(SynapseCalyxDerivedSnapshotReadback {
        panel_name: request.panel_name.to_owned(),
        panel_version: request.panel.version,
        source_seq: request.source_seq,
        snapshot: request.snapshot,
        constellation_count: request.constellations.len() as u64,
        graph_row_count: request.graph_rows.len() as u64,
        committed_seq: vault.latest_seq(),
        lifecycle_sha256: hex_sha256(&stored),
    })
}

fn decode_source_json(source_bytes: &[u8]) -> Result<serde_json::Value, SynapseCalyxError> {
    serde_json::from_slice(source_bytes).map_err(|error| {
        invalid(format!(
            "authoritative source row is not valid JSON: {error}"
        ))
    })
}

fn runtime_lens(
    spec: &LensSpec,
) -> Result<(AlgorithmicLens, FrozenLensContract), SynapseCalyxError> {
    let LensRuntime::Algorithmic { kind } = &spec.runtime else {
        return Err(SynapseCalyxError::new(
            SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED,
            format!(
                "runtime lens {} uses a non-algorithmic execution engine",
                spec.name
            ),
            "register a deterministic algorithmic encoder; Synapse's frozen encoders-only contract does not admit learned or external runtime lenses",
        ));
    };
    let contract = derive_runtime_contract_from_spec(spec)
        .map_err(|error| SynapseCalyxError::from_calyx("validate added lens contract", &error))?;
    let encoder = algorithmic_encoder(kind, spec.output).ok_or_else(|| {
        SynapseCalyxError::new(
            SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED,
            format!(
                "algorithmic kind {kind:?} does not match output {:?}",
                spec.output
            ),
            "use a supported algorithmic kind with its exact declared output shape",
        )
    })?;
    Ok((
        AlgorithmicLens::new(&spec.name, spec.modality, encoder),
        contract,
    ))
}

fn validate_request(request: &SynapseCalyxAddLensRequest<'_>) -> Result<(), SynapseCalyxError> {
    lifecycle_key(request.panel_name)?;
    validate_operation_id(request.operation_id)?;
    if request.slot_key.trim().is_empty() {
        return Err(invalid("slot_key must be non-blank"));
    }
    if request.lens_spec.name.trim().is_empty() {
        return Err(invalid("lens spec name must be non-blank"));
    }
    validate_source_projection(&request.source_projection)?;
    Ok(())
}

fn validate_source_projection(
    projection: &SynapseCalyxSourceProjection,
) -> Result<(), SynapseCalyxError> {
    if let SynapseCalyxSourceProjection::JsonPointer { pointer, .. } = projection
        && (pointer.is_empty() || !pointer.starts_with('/') || pointer.as_bytes().contains(&0))
    {
        return Err(invalid(
            "JSON-pointer source projection must start with '/' and contain no NUL byte",
        ));
    }
    Ok(())
}

fn validate_operation_id(operation_id: &str) -> Result<(), SynapseCalyxError> {
    if operation_id.len() != OPERATION_ID_HEX_BYTES
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "operation_id must be exactly {OPERATION_ID_HEX_BYTES} lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn ensure_base_compatible(
    state: &SynapseCalyxPanelLifecycleState,
    base: &Panel,
) -> Result<(), SynapseCalyxError> {
    if base.version > state.controller.panel().version {
        return Err(conflict(format!(
            "supplied base panel generation {} is newer than durable lifecycle generation {} for {:?}",
            base.version,
            state.controller.panel().version,
            state.panel_name
        )));
    }
    Ok(())
}

fn validate_state(state: &SynapseCalyxPanelLifecycleState) -> Result<(), SynapseCalyxError> {
    if state.schema_version != SCHEMA_VERSION {
        return Err(invalid(format!(
            "lifecycle row schema mismatch: schema={} panel={:?}",
            state.schema_version, state.panel_name
        )));
    }
    for slot in &state.controller.panel().slots {
        if slot.added_at_panel_version > state.controller.panel().version {
            return Err(invalid(format!(
                "slot {} was added at future generation {} beyond panel {}",
                slot.slot_id,
                slot.added_at_panel_version,
                state.controller.panel().version
            )));
        }
    }
    Ok(())
}

fn decode_state(
    bytes: &[u8],
    expected_panel_name: &str,
) -> Result<SynapseCalyxPanelLifecycleState, SynapseCalyxError> {
    let state: SynapseCalyxPanelLifecycleState =
        serde_json::from_slice(bytes).map_err(|error| {
            invalid(format!(
                "decode lifecycle row for {expected_panel_name}: {error}"
            ))
        })?;
    validate_state(&state)?;
    if state.panel_name != expected_panel_name {
        return Err(invalid(format!(
            "lifecycle key names {expected_panel_name:?}, but row owns {:?}",
            state.panel_name
        )));
    }
    Ok(state)
}

fn lifecycle_key(panel_name: &str) -> Result<Vec<u8>, SynapseCalyxError> {
    if panel_name.is_empty()
        || panel_name.len() > MAX_PANEL_NAME_BYTES
        || panel_name.as_bytes().contains(&0)
    {
        return Err(invalid(format!(
            "panel name must contain 1..={MAX_PANEL_NAME_BYTES} non-NUL bytes"
        )));
    }
    let mut key = KEY_PREFIX.to_vec();
    key.extend_from_slice(panel_name.as_bytes());
    Ok(key)
}

fn invalid(message: impl Into<String>) -> SynapseCalyxError {
    SynapseCalyxError::new(
        SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID,
        message,
        "inspect the exact Registry CF lifecycle row and repair the panel contract before retrying",
    )
}

fn conflict(message: impl Into<String>) -> SynapseCalyxError {
    SynapseCalyxError::new(
        SYNAPSE_CALYX_PANEL_LIFECYCLE_CONFLICT,
        message,
        "read the latest Registry CF lifecycle row, preserve its generation and operation ownership, then retry with a new idempotent operation id",
    )
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn sha256_array(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes).into()
}
