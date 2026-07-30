mod repair_all;
mod runtime;
mod static_contract;
pub use static_contract::{
    derive_runtime_contract_from_spec, spec_exact_value_queryable, spec_text_queryable,
};

use std::path::Path;

use calyx_aster::manifest::ImmutableRef;
use calyx_core::{CalyxError, LensId, Modality, Result, SlotId, SlotShape};
use serde::{Deserialize, Serialize};

pub use repair_all::{
    VaultRegistryContractRepairAllWrite, repair_vault_registry_contracts_from_specs,
};
pub(crate) use runtime::load_runtime_lens_from_spec;

use crate::frozen::FrozenLensContract;
use crate::persistence::{
    VaultRegistrySnapshot, load_manifest_panel_registry_snapshot, persist_vault_panel_state,
    rebuild_registry,
};
use crate::{LensSpec, RegistryLensSnapshot};

const REGISTRY_CONTRACT_DRIFT: &str = "CALYX_REGISTRY_CONTRACT_DRIFT";
const REGISTRY_CONTRACT_REPAIR_INVALID: &str = "CALYX_REGISTRY_CONTRACT_REPAIR_INVALID";
const REGISTRY_CONTRACT_REMEDIATION: &str = "run `calyx panel registry-repair --vault <vault> --slot <slot>` after inspecting the emitted registry diff";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryContractAudit {
    pub checked_count: usize,
    pub valid: bool,
    pub diffs: Vec<RegistryContractDiff>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryContractDiff {
    pub lens_id: LensId,
    pub name: Option<String>,
    pub spec_lens_id: Option<LensId>,
    pub persisted_contract_lens_id: LensId,
    pub runtime_contract_lens_id: Option<LensId>,
    pub fields: Vec<RegistryContractFieldDiff>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryContractFieldDiff {
    pub field: String,
    pub persisted: String,
    pub reconstructed: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryContractRepairChange {
    pub slot_id: SlotId,
    pub slot_key: String,
    pub old_lens_id: LensId,
    pub new_lens_id: LensId,
    pub old_shape: SlotShape,
    pub new_shape: SlotShape,
    pub old_modality: Modality,
    pub new_modality: Modality,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultRegistryContractRepairWrite {
    pub manifest_seq: u64,
    pub durable_seq: u64,
    pub panel_ref: ImmutableRef,
    pub registry_ref: ImmutableRef,
    pub wrote_manifest: bool,
    pub old_lens_id: LensId,
    pub new_lens_id: LensId,
    pub changes: Vec<RegistryContractRepairChange>,
}

pub fn audit_vault_registry_contracts(
    vault_dir: impl AsRef<Path>,
) -> Result<RegistryContractAudit> {
    let vault_dir = vault_dir.as_ref();
    let (_, _, snapshot) = load_manifest_panel_registry_snapshot(vault_dir)?;
    Ok(audit_registry_snapshot_contracts(&snapshot))
}

pub fn require_vault_registry_contracts(
    vault_dir: impl AsRef<Path>,
) -> Result<RegistryContractAudit> {
    let audit = audit_vault_registry_contracts(vault_dir)?;
    if audit.valid {
        return Ok(audit);
    }
    Err(registry_contract_drift_error(&audit))
}

pub fn audit_registry_snapshot_contracts(
    snapshot: &VaultRegistrySnapshot,
) -> RegistryContractAudit {
    let diffs = snapshot
        .lenses
        .iter()
        .filter_map(audit_registry_lens_contract)
        .collect::<Vec<_>>();
    RegistryContractAudit {
        checked_count: snapshot.lenses.len(),
        valid: diffs.is_empty(),
        diffs,
    }
}

pub fn repair_vault_registry_slot_from_spec(
    vault_dir: impl AsRef<Path>,
    slot_id: SlotId,
) -> Result<VaultRegistryContractRepairWrite> {
    let vault_dir = vault_dir.as_ref();
    let (manifest, mut panel, mut snapshot) = load_manifest_panel_registry_snapshot(vault_dir)?;
    let old_lens_id = panel
        .slots
        .iter()
        .find(|slot| slot.slot_id == slot_id)
        .map(|slot| slot.lens_id)
        .ok_or_else(|| {
            registry_contract_repair_invalid(format!("panel slot {slot_id} not found"))
        })?;
    let lens_index = snapshot
        .lenses
        .iter()
        .position(|lens| lens.lens_id == old_lens_id)
        .ok_or_else(|| {
            registry_contract_repair_invalid(format!(
                "panel slot {slot_id} references lens {old_lens_id}, but the persisted registry has no matching lens"
            ))
        })?;
    let original_lens = snapshot.lenses[lens_index].clone();
    if audit_registry_lens_contract(&original_lens).is_none() {
        let registry_ref = manifest.registry_ref.clone().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "vault manifest has no registry_ref after loading registry snapshot",
            )
        })?;
        return Ok(VaultRegistryContractRepairWrite {
            manifest_seq: manifest.manifest_seq,
            durable_seq: manifest.durable_seq,
            panel_ref: manifest.panel_ref,
            registry_ref,
            wrote_manifest: false,
            old_lens_id,
            new_lens_id: old_lens_id,
            changes: Vec::new(),
        });
    }
    let spec = original_lens.spec.as_ref().ok_or_else(|| {
        registry_contract_repair_invalid(format!(
            "lens {old_lens_id} has no LensSpec; cannot reconstruct a replacement contract"
        ))
    })?;
    let runtime_contract = derive_runtime_contract_from_spec(spec)?;
    let repaired_spec = spec_from_runtime_contract(spec.clone(), &runtime_contract);
    let new_lens_id = runtime_contract.lens_id();
    if snapshot
        .lenses
        .iter()
        .enumerate()
        .any(|(idx, lens)| idx != lens_index && lens.lens_id == new_lens_id)
    {
        return Err(registry_contract_repair_invalid(format!(
            "reconstructed lens {new_lens_id} already exists in the persisted registry; refusing to merge registry entries automatically"
        )));
    }

    snapshot.lenses[lens_index] = RegistryLensSnapshot {
        lens_id: new_lens_id,
        contract: runtime_contract.clone(),
        spec: Some(repaired_spec),
        determinism: original_lens.determinism,
        runtime_golden: if new_lens_id == old_lens_id {
            original_lens.runtime_golden.clone()
        } else {
            None
        },
    };
    let mut changes = Vec::new();
    for slot in &mut panel.slots {
        if slot.lens_id != old_lens_id {
            continue;
        }
        let change = RegistryContractRepairChange {
            slot_id: slot.slot_id,
            slot_key: slot.slot_key.key().to_string(),
            old_lens_id,
            new_lens_id,
            old_shape: slot.shape,
            new_shape: runtime_contract.shape(),
            old_modality: slot.modality,
            new_modality: runtime_contract.modality(),
        };
        slot.lens_id = new_lens_id;
        slot.shape = runtime_contract.shape();
        slot.modality = runtime_contract.modality();
        changes.push(change);
    }
    if !changes.iter().any(|change| change.slot_id == slot_id) {
        return Err(registry_contract_repair_invalid(format!(
            "panel slot {slot_id} was not updated during registry repair"
        )));
    }
    let audit = audit_registry_snapshot_contracts(&snapshot);
    if !audit.valid {
        return Err(registry_contract_drift_error(&audit));
    }
    let registry = rebuild_registry(&snapshot)?;
    let write = persist_vault_panel_state(vault_dir, &panel, &registry)?;
    Ok(VaultRegistryContractRepairWrite {
        manifest_seq: write.manifest_seq,
        durable_seq: write.durable_seq,
        panel_ref: write.panel_ref,
        registry_ref: write.registry_ref,
        wrote_manifest: true,
        old_lens_id,
        new_lens_id,
        changes,
    })
}

pub fn lens_spec_with_frozen_contract(spec: LensSpec, contract: &FrozenLensContract) -> LensSpec {
    spec_from_runtime_contract(spec, contract)
}

fn audit_registry_lens_contract(lens: &RegistryLensSnapshot) -> Option<RegistryContractDiff> {
    let mut fields = Vec::new();
    let mut runtime_contract_lens_id = None;
    let mut error_code = None;
    let mut error_message = None;
    let name = lens.spec.as_ref().map(|spec| spec.name.clone());
    let spec_lens_id = lens.spec.as_ref().map(LensSpec::lens_id);
    if lens.lens_id != lens.contract.lens_id() {
        fields.push(RegistryContractFieldDiff {
            field: "snapshot.lens_id".to_string(),
            persisted: lens.lens_id.to_string(),
            reconstructed: lens.contract.lens_id().to_string(),
        });
    }
    let Some(spec) = &lens.spec else {
        error_code = Some("CALYX_REGISTRY_CONTRACT_MISSING_SPEC".to_string());
        error_message = Some(format!(
            "lens {} is persisted without LensSpec metadata",
            lens.lens_id
        ));
        return Some(RegistryContractDiff {
            lens_id: lens.lens_id,
            name,
            spec_lens_id,
            persisted_contract_lens_id: lens.contract.lens_id(),
            runtime_contract_lens_id,
            fields,
            error_code,
            error_message,
        });
    };

    let declared = spec.declared_contract();
    fields.extend(contract_field_diffs(
        "spec_declared",
        &lens.contract,
        &declared,
    ));
    match derive_runtime_contract_from_spec(spec) {
        Ok(runtime_contract) => {
            runtime_contract_lens_id = Some(runtime_contract.lens_id());
            fields.extend(contract_field_diffs(
                "runtime",
                &lens.contract,
                &runtime_contract,
            ));
        }
        Err(error) => {
            error_code = Some(error.code.to_string());
            error_message = Some(error.message);
        }
    }

    if fields.is_empty() && error_code.is_none() {
        None
    } else {
        Some(RegistryContractDiff {
            lens_id: lens.lens_id,
            name,
            spec_lens_id,
            persisted_contract_lens_id: lens.contract.lens_id(),
            runtime_contract_lens_id,
            fields,
            error_code,
            error_message,
        })
    }
}

pub(crate) fn contract_field_diffs(
    prefix: &str,
    persisted: &FrozenLensContract,
    reconstructed: &FrozenLensContract,
) -> Vec<RegistryContractFieldDiff> {
    let mut fields = Vec::new();
    push_field_diff(
        &mut fields,
        prefix,
        "lens_id",
        persisted.lens_id().to_string(),
        reconstructed.lens_id().to_string(),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "name",
        persisted.name().to_string(),
        reconstructed.name().to_string(),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "weights_sha256",
        hex32(&persisted.weights_sha256()),
        hex32(&reconstructed.weights_sha256()),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "corpus_hash",
        hex32(&persisted.corpus_hash()),
        hex32(&reconstructed.corpus_hash()),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "shape",
        format!("{:?}", persisted.shape()),
        format!("{:?}", reconstructed.shape()),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "modality",
        format!("{:?}", persisted.modality()),
        format!("{:?}", reconstructed.modality()),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "dtype",
        format!("{:?}", persisted.dtype()),
        format!("{:?}", reconstructed.dtype()),
    );
    push_field_diff(
        &mut fields,
        prefix,
        "norm_policy",
        format!("{:?}", persisted.norm_policy()),
        format!("{:?}", reconstructed.norm_policy()),
    );
    fields
}

fn push_field_diff(
    fields: &mut Vec<RegistryContractFieldDiff>,
    prefix: &str,
    field: &str,
    persisted: String,
    reconstructed: String,
) {
    if persisted != reconstructed {
        fields.push(RegistryContractFieldDiff {
            field: format!("{prefix}.{field}"),
            persisted,
            reconstructed,
        });
    }
}

fn spec_from_runtime_contract(mut spec: LensSpec, contract: &FrozenLensContract) -> LensSpec {
    spec.name = contract.name().to_string();
    spec.output = contract.shape();
    spec.modality = contract.modality();
    spec.weights_sha256 = contract.weights_sha256();
    spec.corpus_hash = contract.corpus_hash();
    spec.norm_policy = contract.norm_policy();
    spec
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn registry_contract_drift_error(audit: &RegistryContractAudit) -> CalyxError {
    CalyxError {
        code: REGISTRY_CONTRACT_DRIFT,
        message: format!(
            "persisted registry contract audit failed for {} of {} lens(es): {}",
            audit.diffs.len(),
            audit.checked_count,
            audit
                .diffs
                .iter()
                .map(format_contract_diff)
                .collect::<Vec<_>>()
                .join("; ")
        ),
        remediation: REGISTRY_CONTRACT_REMEDIATION,
    }
}

fn format_contract_diff(diff: &RegistryContractDiff) -> String {
    let fields = diff
        .fields
        .iter()
        .map(|field| {
            format!(
                "{} persisted={} reconstructed={}",
                field.field, field.persisted, field.reconstructed
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "lens={} name={} spec_lens_id={} persisted_contract={} runtime_contract={} fields=[{}] error_code={} error_message={}",
        diff.lens_id,
        diff.name.as_deref().unwrap_or("<missing>"),
        diff.spec_lens_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<missing>".to_string()),
        diff.persisted_contract_lens_id,
        diff.runtime_contract_lens_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<unavailable>".to_string()),
        fields,
        diff.error_code.as_deref().unwrap_or("<none>"),
        diff.error_message.as_deref().unwrap_or("<none>")
    )
}

fn registry_contract_repair_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: REGISTRY_CONTRACT_REPAIR_INVALID,
        message: message.into(),
        remediation: "choose a panel slot backed by a persisted LensSpec whose runtime can be reconstructed without colliding with another registry lens",
    }
}
