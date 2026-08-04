//! Durable, revision-guarded panel lifecycle state.
//!
//! The active-panel manifest is a serving default, not a catalog. Runtime lens
//! mutations therefore live in Registry CF under one row per logical panel so
//! changing a non-default panel cannot move the serving pointer (#1668).

use std::collections::BTreeMap;

use calyx_aster::cf::ColumnFamily;
use calyx_core::{CxId, LensId, Panel, SlotId, SlotState, Ts};
use calyx_registry::{
    AlgorithmicLens, FrozenLensContract, LensRuntime, LensSpec, Registry, SlotSpec, SwapController,
    algorithmic_encoder, derive_runtime_contract_from_spec,
};
use serde::{Deserialize, Serialize};

use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

const SCHEMA_VERSION: u16 = 1;
const KEY_PREFIX: &[u8] = b"panel-lifecycle\0v1\0";
const MAX_CAS_ATTEMPTS: usize = 64;
const MAX_PANEL_NAME_BYTES: usize = 128;
const OPERATION_ID_HEX_BYTES: usize = 64;

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

/// One idempotent add-lens mutation request.
pub struct SynapseCalyxAddLensRequest<'a> {
    pub panel_name: &'a str,
    pub base_panel: Panel,
    pub operation_id: &'a str,
    pub slot_key: &'a str,
    pub lens_spec: LensSpec,
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

impl SynapseCalyxVault {
    /// Reads one panel's lifecycle row from the physical Registry CF.
    pub fn read_panel_lifecycle(
        &self,
        panel_name: &str,
    ) -> Result<Option<SynapseCalyxPanelLifecycleState>, SynapseCalyxError> {
        let key = lifecycle_key(panel_name)?;
        self.read_cf_latest(ColumnFamily::Registry, &key)?
            .map(|bytes| decode_state(&bytes, panel_name))
            .transpose()
    }

    /// Adds one deterministic lens and durably enqueues historical records.
    ///
    /// The generation allocation and operation id are idempotent. The panel
    /// row itself is CAS guarded, so concurrent mutations are retried from the
    /// winning physical state and never overwrite one another.
    pub fn add_panel_lens(
        &self,
        request: SynapseCalyxAddLensRequest<'_>,
    ) -> Result<SynapseCalyxAddLensReadback, SynapseCalyxError> {
        validate_request(&request)?;
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
    pub fn set_panel_lens_state(
        &self,
        request: SynapseCalyxSetLensStateRequest<'_>,
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
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
