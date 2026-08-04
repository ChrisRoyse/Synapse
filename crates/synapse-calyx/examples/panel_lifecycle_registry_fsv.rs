//! Manual physical Registry-CF verification for #1668's lifecycle owner.

use std::path::PathBuf;
use std::process::ExitCode;

use calyx_aster::cf::ColumnFamily;
use calyx_core::{Asymmetry, CxId, Modality, Panel, QuantPolicy, SlotId, SlotShape, SlotState};
use calyx_registry::{
    LensRuntime, LensSpec, NormPolicy, derive_runtime_contract_from_spec,
    lens_spec_with_frozen_contract,
};
use synapse_calyx::panel_lifecycle::{
    SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID, SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED,
    SynapseCalyxAddLensRequest, SynapseCalyxSetLensStateRequest, SynapseCalyxSourceProjection,
};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxVault};

const ADD_OPERATION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EXTERNAL_OPERATION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PARK_OPERATION: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const RETIRE_OPERATION: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const UNRETIRE_OPERATION: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FSV FAILED: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: panel_lifecycle_registry_fsv <fresh-vault-dir>")?;
    if vault_dir.exists() {
        return Err(format!(
            "vault path must be absent before FSV: {}",
            vault_dir.display()
        )
        .into());
    }
    println!("SOURCE OF TRUTH: Registry CF in {}", vault_dir.display());
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(vault_dir))?;
    let base = Panel {
        version: 10,
        slots: Vec::new(),
        created_at: 1,
        kernel_ref: None,
        guard_ref: None,
    };
    let candidates = [
        CxId::from_bytes([1; 16]),
        CxId::from_bytes([2; 16]),
        CxId::from_bytes([3; 16]),
    ];

    let before = vault.scan_cf_latest(ColumnFamily::Registry)?;
    println!("BEFORE happy: Registry rows={}", before.len());
    let created = vault.add_panel_lens(SynapseCalyxAddLensRequest {
        panel_name: "syn-fsv-panel-v1",
        base_panel: base.clone(),
        operation_id: ADD_OPERATION,
        slot_key: "fsv.byte_features",
        lens_spec: byte_spec(),
        source_projection: SynapseCalyxSourceProjection::RawSourceBytes,
        candidates: &candidates,
        now: 2,
    })?;
    let after = vault.scan_cf_latest(ColumnFamily::Registry)?;
    let physical = after
        .iter()
        .find(|(key, _)| key == b"panel-lifecycle\0v1\0syn-fsv-panel-v1")
        .ok_or("physical lifecycle Registry row absent")?;
    let physical_hash = sha256_hex(&physical.1);
    ensure(
        created.queued == 3,
        "three historical records were not queued",
    )?;
    ensure(
        created.state.controller.panel().slots.len() == 1,
        "added slot absent",
    )?;
    ensure(
        created.value_sha256 == physical_hash,
        "returned hash differs from physical bytes",
    )?;
    println!(
        "AFTER happy: Registry rows={} generation={} slot={} queued={} seq={} sha256={} physical_bytes={}",
        after.len(),
        created.state.controller.panel().version,
        created.slot_id,
        created.queued,
        created.committed_seq,
        physical_hash,
        physical.1.len()
    );
    let measured = vault.measure_added_panel_lens("syn-fsv-panel-v1", created.lens_id, b"Aa 1")?;
    let calyx_core::SlotVector::Dense { dim, data } = measured.vector else {
        return Err("byte-feature lifecycle lens did not return a dense vector".into());
    };
    ensure(
        dim == 16 && data.len() == 16,
        "byte-feature vector shape drifted",
    )?;
    ensure(
        data[..8] == [0.0625, 1.0, 0.25, 0.5, 0.25, 0.0, 0.25, 0.25],
        format!("known byte-feature prefix differs: {:?}", &data[..8]),
    )?;
    ensure(
        measured.input_sha256 == sha256_hex(b"Aa 1"),
        "measured input hash differs from authoritative bytes",
    )?;
    println!(
        "MEASURE known input: input='Aa 1' dim=16 prefix={:?} input_sha256={}",
        &data[..8],
        measured.input_sha256
    );

    let replay_before = vault.scan_cf_latest(ColumnFamily::Registry)?;
    let replay = vault.add_panel_lens(SynapseCalyxAddLensRequest {
        panel_name: "syn-fsv-panel-v1",
        base_panel: base.clone(),
        operation_id: ADD_OPERATION,
        slot_key: "fsv.byte_features",
        lens_spec: byte_spec(),
        source_projection: SynapseCalyxSourceProjection::RawSourceBytes,
        candidates: &candidates,
        now: 3,
    })?;
    let replay_after = vault.scan_cf_latest(ColumnFamily::Registry)?;
    ensure(
        replay.existing_identical,
        "idempotent replay was not classified identical",
    )?;
    ensure(
        replay_before == replay_after,
        "idempotent replay mutated Registry CF",
    )?;
    println!(
        "EDGE idempotent replay: before_rows={} after_rows={} generation={} existing_identical=true state_unchanged=true",
        replay_before.len(),
        replay_after.len(),
        replay.state.controller.panel().version
    );

    let parked = vault.set_panel_lens_state(SynapseCalyxSetLensStateRequest {
        panel_name: "syn-fsv-panel-v1",
        operation_id: PARK_OPERATION,
        slot_id: SlotId::new(created.slot_id),
        state: SlotState::Parked,
        now: 4,
    })?;
    let parked_physical = lifecycle_bytes(&vault)?;
    ensure(
        parked.value_sha256 == sha256_hex(&parked_physical),
        "park readback hash differs from physical Registry bytes",
    )?;
    ensure(
        parked.state.controller.panel().slots[0].state == SlotState::Parked,
        "parked slot was removed or remained active",
    )?;
    println!(
        "AFTER park: generation={} slot_count={} lifecycle=parked sha256={}",
        parked.state.controller.panel().version,
        parked.state.controller.panel().slots.len(),
        parked.value_sha256
    );
    let retired = vault.set_panel_lens_state(SynapseCalyxSetLensStateRequest {
        panel_name: "syn-fsv-panel-v1",
        operation_id: RETIRE_OPERATION,
        slot_id: SlotId::new(created.slot_id),
        state: SlotState::Retired,
        now: 5,
    })?;
    let retired_physical = lifecycle_bytes(&vault)?;
    ensure(
        retired.value_sha256 == sha256_hex(&retired_physical),
        "retire readback hash differs from physical Registry bytes",
    )?;
    ensure(
        retired.state.controller.panel().slots.len() == 1
            && retired.state.controller.panel().slots[0].state == SlotState::Retired,
        "retirement destructively removed the historical slot",
    )?;
    println!(
        "AFTER retire: generation={} slot_count=1 lifecycle=retired historical_slot_present=true sha256={}",
        retired.state.controller.panel().version,
        retired.value_sha256
    );

    edge(
        &vault,
        "invalid panel name",
        SynapseCalyxAddLensRequest {
            panel_name: "bad\0panel",
            base_panel: base.clone(),
            operation_id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            slot_key: "fsv.invalid",
            lens_spec: byte_spec(),
            source_projection: SynapseCalyxSourceProjection::RawSourceBytes,
            candidates: &[],
            now: 6,
        },
        SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID,
    )?;
    edge(
        &vault,
        "unsupported external runtime",
        SynapseCalyxAddLensRequest {
            panel_name: "syn-fsv-panel-v1",
            base_panel: base.clone(),
            operation_id: EXTERNAL_OPERATION,
            slot_key: "fsv.external",
            lens_spec: external_spec(),
            source_projection: SynapseCalyxSourceProjection::RawSourceBytes,
            candidates: &[],
            now: 7,
        },
        SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED,
    )?;

    let before_unretire = vault.scan_cf_latest(ColumnFamily::Registry)?;
    let unretire = match vault.set_panel_lens_state(SynapseCalyxSetLensStateRequest {
        panel_name: "syn-fsv-panel-v1",
        operation_id: UNRETIRE_OPERATION,
        slot_id: SlotId::new(created.slot_id),
        state: SlotState::Active,
        now: 8,
    }) {
        Ok(_) => return Err("retired slot unexpectedly reactivated".into()),
        Err(error) => error,
    };
    let after_unretire = vault.scan_cf_latest(ColumnFamily::Registry)?;
    ensure(
        before_unretire == after_unretire,
        "retired-slot reactivation mutated Registry CF",
    )?;
    println!(
        "EDGE retired reactivation: code={} before_rows={} after_rows={} historical_slot_present=true state_unchanged=true",
        unretire.code,
        before_unretire.len(),
        after_unretire.len()
    );
    println!("FSV PASS: physical lifecycle row and all boundaries verified");
    Ok(())
}

fn lifecycle_bytes(vault: &SynapseCalyxVault) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    vault
        .scan_cf_latest(ColumnFamily::Registry)?
        .into_iter()
        .find(|(key, _)| key == b"panel-lifecycle\0v1\0syn-fsv-panel-v1")
        .map(|(_, value)| value)
        .ok_or_else(|| "physical lifecycle Registry row absent".into())
}

fn edge(
    vault: &SynapseCalyxVault,
    name: &str,
    request: SynapseCalyxAddLensRequest<'_>,
    expected_code: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let before = vault.scan_cf_latest(ColumnFamily::Registry)?;
    let error = match vault.add_panel_lens(request) {
        Ok(_) => return Err(format!("{name}: edge unexpectedly succeeded").into()),
        Err(error) => error,
    };
    let after = vault.scan_cf_latest(ColumnFamily::Registry)?;
    ensure(
        error.code == expected_code,
        format!("{name}: wrong code {}", error.code),
    )?;
    ensure(before == after, format!("{name}: Registry CF mutated"))?;
    println!(
        "EDGE {name}: code={} before_rows={} after_rows={} state_unchanged=true",
        error.code,
        before.len(),
        after.len()
    );
    Ok(())
}

fn byte_spec() -> LensSpec {
    let spec = LensSpec {
        name: "syn.fsv.byte_features.v1".to_owned(),
        runtime: LensRuntime::Algorithmic {
            kind: "byte_features".to_owned(),
        },
        output: SlotShape::Dense(16),
        modality: Modality::Structured,
        weights_sha256: [0; 32],
        corpus_hash: [7; 32],
        norm_policy: NormPolicy::finite_only(),
        max_batch: None,
        axis: Some("fsv".to_owned()),
        asymmetry: Asymmetry::None,
        quant_default: QuantPolicy::None,
        truncate_dim: None,
        recall_delta: 0.0,
        retrieval_only: false,
        excluded_from_dedup: false,
    };
    let Ok(contract) = derive_runtime_contract_from_spec(&spec) else {
        panic!("static FSV algorithmic spec is invalid");
    };
    lens_spec_with_frozen_contract(spec, &contract)
}

fn external_spec() -> LensSpec {
    LensSpec {
        runtime: LensRuntime::ExternalCmd {
            cmd: "never-run".to_owned(),
            args: Vec::new(),
        },
        name: "syn.fsv.external.v1".to_owned(),
        ..byte_spec()
    }
}

fn ensure(ok: bool, message: impl Into<String>) -> Result<(), Box<dyn std::error::Error>> {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
