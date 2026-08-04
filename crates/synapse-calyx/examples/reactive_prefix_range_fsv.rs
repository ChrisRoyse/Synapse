//! Manual FSV for #2004: unrelated Reactive rows cannot consume a typed relay budget.

use std::{error::Error, path::PathBuf};

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxPersistedNoveltyFinding, SynapseCalyxPersistedRegionFinding,
    SynapseCalyxVault,
};

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: reactive_prefix_range_fsv <empty-vault-dir>")?;
    std::fs::create_dir_all(&root)?;
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(root))?;
    for index in 1..=300_u64 {
        vault.persist_novelty_finding(&SynapseCalyxPersistedNoveltyFinding {
            panel_version: 1,
            query_cx_id: format!("{index:032x}"),
            guard_id: "fsv-prefix-range".to_owned(),
            action: "new_region".to_owned(),
            failing_slots: vec![1],
            ledger_seq: index,
            ledger_hash: format!("{index:064x}"),
        })?;
    }
    let expected = SynapseCalyxPersistedRegionFinding {
        region_kind: "application".to_owned(),
        region_id: "target-after-300-unrelated".to_owned(),
        subject_cx_id: "0000000000000000000000000000012d".to_owned(),
        trigger_cx_id: "0000000000000000000000000000012d".to_owned(),
        occurrence_id: 0,
        frequency: 1,
        observed_seq: 301,
    };
    vault.persist_region_finding(&expected)?;
    let readback = vault.persisted_region_findings(0, 1)?;
    let reactive_rows = vault.scan_cf_latest(ColumnFamily::Reactive)?.len();
    println!(
        "PREFIX_RANGE unrelated=300 total_reactive_rows={reactive_rows} requested=1 returned={} exact_match={}",
        readback.len(),
        readback == vec![expected.clone()]
    );
    if readback != vec![expected] || reactive_rows != 301 {
        return Err(
            "typed prefix range did not return the target independently of unrelated rows".into(),
        );
    }
    Ok(())
}
