//! Independent physical Reactive CF readback for #1680.

use std::{error::Error, path::PathBuf};

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: reactive_drift_readback_fsv <existing-vault-dir>")?;
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(dir),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let rows = vault.scan_cf_at(snapshot, ColumnFamily::Reactive)?;
    println!(
        "REACTIVE_READBACK snapshot={} rows={}",
        snapshot,
        rows.len()
    );
    for (key, value) in rows {
        let json = serde_json::from_slice::<serde_json::Value>(&value)?;
        println!(
            "ROW key_hex={} value_len={} value_json={}",
            key.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            value.len(),
            serde_json::to_string(&json)?,
        );
    }
    Ok(())
}
