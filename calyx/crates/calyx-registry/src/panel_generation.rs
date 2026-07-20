use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Result};
use serde::{Deserialize, Serialize};

pub const CALYX_PANEL_GENERATION_INVALID: &str = "CALYX_PANEL_GENERATION_INVALID";
pub const CALYX_PANEL_GENERATION_CONFLICT: &str = "CALYX_PANEL_GENERATION_CONFLICT";
pub const CALYX_PANEL_GENERATION_EXHAUSTED: &str = "CALYX_PANEL_GENERATION_EXHAUSTED";

const SCHEMA_VERSION: u16 = 1;
const ALLOCATOR_KEY: &[u8] = b"panel-generation-allocator\0v1";
const MAX_OWNERS: usize = 10_000;
const MAX_CAS_ATTEMPTS: usize = 64;
const MAX_PANEL_NAME_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PanelGenerationAllocatorState {
    schema_version: u16,
    next_generation: u32,
    owners: BTreeMap<u32, String>,
    operations: BTreeMap<String, u32>,
}

impl Default for PanelGenerationAllocatorState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_generation: 1,
            owners: BTreeMap::new(),
            operations: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelGenerationAllocation {
    pub panel_name: String,
    pub operation_id: String,
    pub panel_generation: u32,
    pub committed_seq: u64,
    pub existing_identical: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelGenerationAllocatorReadback {
    pub schema_version: u16,
    pub next_generation: u32,
    pub owner_count: u64,
    pub operation_count: u64,
    pub owners: BTreeMap<u32, String>,
    pub latest_seq: u64,
}

pub fn reserve_vault_panel_generations<C: Clock>(
    vault: &AsterVault<C>,
    reservations: &[(String, u32)],
) -> Result<PanelGenerationAllocatorReadback> {
    validate_reservations(reservations)?;
    for _attempt in 0..MAX_CAS_ATTEMPTS {
        let (mut state, revision) = read_state_revisioned(vault)?;
        let mut changed = false;
        for (panel_name, generation) in reservations {
            let owner = format!("builtin:{panel_name}");
            match state.owners.get(generation) {
                Some(existing) if existing != &owner => {
                    return Err(conflict(format!(
                        "panel generation {generation} is owned by {existing:?}, not {owner:?}"
                    )));
                }
                Some(_) => {}
                None => {
                    ensure_owner_capacity(&state)?;
                    state.owners.insert(*generation, owner);
                    changed = true;
                }
            }
            let next = generation.checked_add(1).ok_or_else(exhausted)?;
            if state.next_generation < next {
                state.next_generation = next;
                changed = true;
            }
        }
        if !changed {
            return read_vault_panel_generation_allocator(vault);
        }
        if let Some(seq) = compare_and_write(vault, revision, &state)? {
            return readback_at(vault, seq, &state);
        }
    }
    Err(conflict(format!(
        "panel generation reservation exceeded {MAX_CAS_ATTEMPTS} atomic retry attempts"
    )))
}

pub fn allocate_vault_panel_generation<C: Clock>(
    vault: &AsterVault<C>,
    panel_name: &str,
    operation_id: &str,
) -> Result<PanelGenerationAllocation> {
    validate_panel_name(panel_name)?;
    validate_operation_id(operation_id)?;
    let operation_key = format!("{panel_name}:{operation_id}");
    for _attempt in 0..MAX_CAS_ATTEMPTS {
        let (mut state, revision) = read_state_revisioned(vault)?;
        if let Some(generation) = state.operations.get(&operation_key).copied() {
            let expected_owner = format!("dynamic:{operation_key}");
            if state.owners.get(&generation) != Some(&expected_owner) {
                return Err(conflict(format!(
                    "operation {operation_key:?} maps to generation {generation}, but its owner row is absent or different"
                )));
            }
            return Ok(PanelGenerationAllocation {
                panel_name: panel_name.to_owned(),
                operation_id: operation_id.to_owned(),
                panel_generation: generation,
                committed_seq: vault.latest_seq(),
                existing_identical: true,
            });
        }
        ensure_owner_capacity(&state)?;
        let generation = next_free_generation(&state)?;
        let next_generation = generation.checked_add(1).ok_or_else(exhausted)?;
        state.next_generation = next_generation;
        state
            .owners
            .insert(generation, format!("dynamic:{operation_key}"));
        state.operations.insert(operation_key.clone(), generation);
        if let Some(seq) = compare_and_write(vault, revision, &state)? {
            let readback = read_state_at(vault, seq)?;
            if readback.operations.get(&operation_key) != Some(&generation)
                || readback.owners.get(&generation) != Some(&format!("dynamic:{operation_key}"))
            {
                return Err(conflict(format!(
                    "allocated panel generation {generation} lacks exact committed readback for operation {operation_key:?}"
                )));
            }
            return Ok(PanelGenerationAllocation {
                panel_name: panel_name.to_owned(),
                operation_id: operation_id.to_owned(),
                panel_generation: generation,
                committed_seq: seq,
                existing_identical: false,
            });
        }
    }
    Err(conflict(format!(
        "panel generation allocation exceeded {MAX_CAS_ATTEMPTS} atomic retry attempts"
    )))
}

pub fn read_vault_panel_generation_allocator<C: Clock>(
    vault: &AsterVault<C>,
) -> Result<PanelGenerationAllocatorReadback> {
    let state = vault
        .read_cf_latest(ColumnFamily::Registry, ALLOCATOR_KEY)?
        .map(|bytes| decode_state(&bytes))
        .transpose()?
        .unwrap_or_default();
    state.validate()?;
    Ok(readback(vault.latest_seq(), state))
}

fn read_state_revisioned<C: Clock>(
    vault: &AsterVault<C>,
) -> Result<(PanelGenerationAllocatorState, Option<[u8; 32]>)> {
    let Some((bytes, revision)) =
        vault.read_cf_latest_revisioned(ColumnFamily::Registry, ALLOCATOR_KEY)?
    else {
        return Ok((PanelGenerationAllocatorState::default(), None));
    };
    Ok((decode_state(&bytes)?, Some(revision)))
}

fn read_state_at<C: Clock>(
    vault: &AsterVault<C>,
    seq: u64,
) -> Result<PanelGenerationAllocatorState> {
    let bytes = vault
        .read_cf_at(seq, ColumnFamily::Registry, ALLOCATOR_KEY)?
        .ok_or_else(|| {
            conflict(format!(
                "panel generation allocator missing at sequence {seq}"
            ))
        })?;
    decode_state(&bytes)
}

fn compare_and_write<C: Clock>(
    vault: &AsterVault<C>,
    revision: Option<[u8; 32]>,
    state: &PanelGenerationAllocatorState,
) -> Result<Option<u64>> {
    state.validate()?;
    let bytes = serde_json::to_vec(state)
        .map_err(|error| invalid(format!("encode panel generation allocator: {error}")))?;
    let outcome = vault.write_cf_batch_if_revision(
        ColumnFamily::Registry,
        ALLOCATOR_KEY,
        revision,
        [(ColumnFamily::Registry, ALLOCATOR_KEY.to_vec(), bytes)],
    )?;
    Ok(outcome.applied.then_some(outcome.seq))
}

fn readback_at<C: Clock>(
    vault: &AsterVault<C>,
    seq: u64,
    expected: &PanelGenerationAllocatorState,
) -> Result<PanelGenerationAllocatorReadback> {
    let actual = read_state_at(vault, seq)?;
    if &actual != expected {
        return Err(conflict(format!(
            "panel generation allocator readback differs at committed sequence {seq}"
        )));
    }
    Ok(readback(seq, actual))
}

fn readback(
    latest_seq: u64,
    state: PanelGenerationAllocatorState,
) -> PanelGenerationAllocatorReadback {
    PanelGenerationAllocatorReadback {
        schema_version: state.schema_version,
        next_generation: state.next_generation,
        owner_count: state.owners.len() as u64,
        operation_count: state.operations.len() as u64,
        owners: state.owners,
        latest_seq,
    }
}

impl PanelGenerationAllocatorState {
    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION || self.next_generation == 0 {
            return Err(invalid(format!(
                "panel generation allocator schema={} next_generation={} is invalid",
                self.schema_version, self.next_generation
            )));
        }
        if self.owners.len() > MAX_OWNERS || self.operations.len() > MAX_OWNERS {
            return Err(exhausted());
        }
        for (generation, owner) in &self.owners {
            if *generation == 0 || owner.is_empty() {
                return Err(invalid(format!(
                    "panel generation allocator contains invalid owner generation={generation} owner={owner:?}"
                )));
            }
        }
        let mut seen = BTreeSet::new();
        for (operation, generation) in &self.operations {
            if operation.is_empty() || !seen.insert(*generation) {
                return Err(invalid(format!(
                    "panel generation allocator contains invalid or duplicate operation {operation:?} generation={generation}"
                )));
            }
            let Some(owner) = self.owners.get(generation) else {
                return Err(invalid(format!(
                    "panel generation operation {operation:?} points to unowned generation {generation}"
                )));
            };
            if owner != &format!("dynamic:{operation}") {
                return Err(invalid(format!(
                    "panel generation operation {operation:?} owner mismatch: {owner:?}"
                )));
            }
        }
        Ok(())
    }
}

fn validate_reservations(reservations: &[(String, u32)]) -> Result<()> {
    let mut generations = BTreeMap::new();
    for (panel_name, generation) in reservations {
        validate_panel_name(panel_name)?;
        if *generation == 0 {
            return Err(invalid(format!(
                "panel {panel_name:?} reserves zero generation"
            )));
        }
        if let Some(existing) = generations.insert(*generation, panel_name.as_str())
            && existing != panel_name
        {
            return Err(conflict(format!(
                "generation {generation} is requested by both {existing:?} and {panel_name:?}"
            )));
        }
    }
    Ok(())
}

fn validate_panel_name(panel_name: &str) -> Result<()> {
    if panel_name.is_empty()
        || panel_name.len() > MAX_PANEL_NAME_BYTES
        || !panel_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid(format!(
            "panel name {panel_name:?} must be 1..={MAX_PANEL_NAME_BYTES} lowercase ASCII letters, digits, or hyphens"
        )));
    }
    Ok(())
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.len() != 64
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "operation_id {operation_id:?} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn ensure_owner_capacity(state: &PanelGenerationAllocatorState) -> Result<()> {
    if state.owners.len() >= MAX_OWNERS || state.operations.len() >= MAX_OWNERS {
        return Err(exhausted());
    }
    Ok(())
}

fn next_free_generation(state: &PanelGenerationAllocatorState) -> Result<u32> {
    let mut candidate = state.next_generation;
    while state.owners.contains_key(&candidate) {
        candidate = candidate.checked_add(1).ok_or_else(exhausted)?;
    }
    Ok(candidate)
}

fn decode_state(bytes: &[u8]) -> Result<PanelGenerationAllocatorState> {
    let state: PanelGenerationAllocatorState = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("decode panel generation allocator: {error}")))?;
    state.validate()?;
    Ok(state)
}

fn invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_GENERATION_INVALID,
        message: message.into(),
        remediation: "repair the exact Registry CF allocator row before changing any panel",
    }
}

fn conflict(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_GENERATION_CONFLICT,
        message: message.into(),
        remediation: "retry with the same operation identity or inspect conflicting Registry CF ownership",
    }
}

fn exhausted() -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_GENERATION_EXHAUSTED,
        message: "panel generation allocator is exhausted".to_owned(),
        remediation: "archive retired generations into a new explicitly versioned allocator format",
    }
}
