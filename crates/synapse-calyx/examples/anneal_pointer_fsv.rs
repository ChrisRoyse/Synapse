use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use serde_json::json;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxVault};

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: anneal_pointer_fsv <new-or-existing-vault-dir>")?;
    let config = SynapseCalyxConfig::from_vault_dir(vault_dir.clone());
    let before_exists = vault_dir.exists();
    let before_files = if before_exists {
        std::fs::read_dir(&vault_dir)?.count()
    } else {
        0
    };
    println!(
        "{}",
        json!({"state":"before","vault_exists":before_exists,"top_level_entries":before_files})
    );

    let vault = SynapseCalyxVault::open(config.clone())?;
    let status = vault.anneal_status()?;
    println!("{}", json!({"trigger":"open","status":status}));
    vault.close("anneal_pointer_fsv")?;

    let inspector = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![ColumnFamily::Kv, ColumnFamily::AnnealRollback]),
    )?;
    let artifacts = inspector.scan_anneal_tuning_artifacts_latest()?;
    let rollback = inspector.scan_anneal_rollback_latest()?;
    println!(
        "{}",
        json!({
            "state":"after_independent_read",
            "latest_seq":inspector.latest_seq(),
            "artifact_rows":artifacts.iter().map(|(key, value)| json!({"key_hex":hex(key),"value_utf8":String::from_utf8_lossy(value)})).collect::<Vec<_>>(),
            "rollback_rows":rollback.iter().map(|(key, value)| json!({"key_hex":hex(key),"value_hex":hex(value)})).collect::<Vec<_>>()
        })
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
