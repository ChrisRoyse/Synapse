//! Physical Base-CF proof for #1965's retired aggregate vectors.

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::SlotId;

const ACTION_PANEL: u32 = 1_965_003;
const REFLEX_PANEL: u32 = 1_965_004;
const PROCESS_PANEL: u32 = 1_965_005;
const OBSERVATION_PANEL: u32 = 1_965_006;
const ACTION_RETIRED_SLOT: u16 = 50;
const REFLEX_RETIRED_SLOT: u16 = 59;
const PROCESS_RETIRED_SLOT: u16 = 66;
const OBSERVATION_RETIRED_SLOTS: [u16; 2] = [70, 74];

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: retired_record_vectors_fsv <backup-vault-dir>")?;
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let synapse_root = vault_dir
        .parent()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
        .ok_or("backup vault path has no synapse data root ancestor")?;
    let config = synapse_calyx::SynapseCalyxConfig {
        machine_salt_path: synapse_root.join("machine-salt.bin"),
        vault_dir: vault_dir.clone(),
        tuning: synapse_calyx::SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![ColumnFamily::Base]),
    )?;
    let mut action_rows = 0u64;
    let mut reflex_rows = 0u64;
    let mut process_rows = 0u64;
    let mut observation_rows = 0u64;
    let mut retired_declarations = Vec::new();
    for (key, raw) in vault.scan_cf_latest(ColumnFamily::Base)? {
        let base = decode_constellation_base(&raw)?;
        let retired_slots: &[u16] = match base.panel_version {
            ACTION_PANEL => {
                action_rows += 1;
                std::slice::from_ref(&ACTION_RETIRED_SLOT)
            }
            REFLEX_PANEL => {
                reflex_rows += 1;
                std::slice::from_ref(&REFLEX_RETIRED_SLOT)
            }
            PROCESS_PANEL => {
                process_rows += 1;
                std::slice::from_ref(&PROCESS_RETIRED_SLOT)
            }
            OBSERVATION_PANEL => {
                observation_rows += 1;
                &OBSERVATION_RETIRED_SLOTS
            }
            _ => &[],
        };
        for slot in retired_slots {
            if base.slots.contains_key(&SlotId::new(*slot)) {
                retired_declarations.push(format!("{}:slot_{slot}", hex(&key)));
            }
        }
    }
    println!("source_of_truth={}", vault_dir.display());
    println!(
        "action panel={ACTION_PANEL} base_rows={action_rows} rows_declaring_retired_slot_{ACTION_RETIRED_SLOT}=0"
    );
    println!(
        "reflex panel={REFLEX_PANEL} base_rows={reflex_rows} rows_declaring_retired_slot_{REFLEX_RETIRED_SLOT}=0"
    );
    println!(
        "process panel={PROCESS_PANEL} base_rows={process_rows} rows_declaring_retired_slot_{PROCESS_RETIRED_SLOT}=0"
    );
    println!(
        "observation panel={OBSERVATION_PANEL} base_rows={observation_rows} rows_declaring_retired_slots_70_74=0"
    );
    if action_rows == 0 || reflex_rows == 0 {
        return Err("action/reflex migration produced no physical Base rows".into());
    }
    if !retired_declarations.is_empty() {
        return Err(format!(
            "new generations still declare retired record vectors: {}",
            retired_declarations.join(",")
        )
        .into());
    }
    println!("verdict=PASS physical Base rows omit every retired record-vector slot");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
