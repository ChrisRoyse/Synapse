//! Builds a deterministic real-vault corpus for #1680 MCP delivery FSV.

use std::{collections::BTreeMap, error::Error, path::PathBuf};

use calyx_core::{
    Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, SlotId, SlotVector, VaultId,
};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxVault};

const PANEL: u32 = 1_680_001;
const ROWS: u16 = 100;
const SLOT: u16 = 1;

fn row(vault_id: VaultId, index: u16) -> Constellation {
    let mut id = [0_u8; 16];
    id[..2].copy_from_slice(&index.to_be_bytes());
    let value = if index < ROWS / 2 {
        f32::from(index % 5) * 0.01
    } else {
        10.0 + f32::from(index % 5) * 0.01
    };
    let mut input_hash = [0_u8; 32];
    input_hash[..2].copy_from_slice(&index.to_be_bytes());
    Constellation {
        cx_id: CxId::from_bytes(id),
        vault_id,
        panel_version: PANEL,
        created_at: 1_785_800_000_000 + u64::from(index),
        input_ref: InputRef {
            hash: input_hash,
            pointer: Some(format!("fsv-1680/{index}")),
            redacted: false,
        },
        modality: Modality::Structured,
        slots: BTreeMap::from([(
            SlotId::new(SLOT),
            SlotVector::Dense {
                dim: 1,
                data: vec![value],
            },
        )]),
        scalars: BTreeMap::new(),
        metadata: BTreeMap::from([("fixture_index".to_owned(), index.to_string())]),
        anchors: Vec::new(),
        provenance: LedgerRef {
            seq: u64::from(index) + 1,
            hash: [0; 32],
        },
        flags: CxFlags::default(),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: reactive_drift_fixture_fsv <new-scratch-dir>")?;
    std::fs::create_dir(&dir).map_err(|error| {
        format!(
            "SYNAPSE_FSV_SCRATCH_CREATE_FAILED: cannot create {}: {error}; remediation=pass a new child path",
            dir.display()
        )
    })?;
    println!("BEFORE path={} base_rows=0", dir.display());
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    let vault_id = vault.vault_id_value();
    for index in 0..ROWS {
        vault.put_observation_constellation(row(vault_id, index))?;
    }
    vault.checkpoint()?;
    println!(
        "AFTER path={} vault_id={} panel={} base_rows={} reference_center=0.02 recent_center=10.02 latest_seq={}",
        dir.display(),
        vault_id,
        PANEL,
        ROWS,
        vault.latest_seq(),
    );
    Ok(())
}
