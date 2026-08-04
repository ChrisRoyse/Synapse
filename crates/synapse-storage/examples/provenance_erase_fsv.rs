//! Reproduce + lawful erase + independent physical readback FSV (#1679).

use std::{error::Error, path::PathBuf};

use calyx_aster::cf::{ColumnFamily, SlotFamilyKind};
use calyx_core::{CxId, SlotId};
use serde_json::json;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, encode_json};

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: provenance_erase_fsv <empty-vault-dir>")?;
    let db = Db::open(&root, SCHEMA_VERSION)?;
    let ts_ns = 1_785_870_000_000_000_000_u64;
    let key = ts_ns.to_be_bytes();
    let record = json!({
        "schema_version": 1,
        "audit_id": "erase-fsv",
        "ts_ns": ts_ns,
        "seq": 1,
        "tool": "erase_fsv_action",
        "status": "ok",
        "error_code": null,
        "details": {"secret_marker": "must-be-unrecoverable"}
    });
    let raw = encode_json(&record)?;
    let context = br#"{"action_id":"erase_fsv_action","outcome_anchor":{"value":{"bool":true}},"ground_truth_anchor":{"value":{"bool":true}}}"#;

    println!("BEFORE seq={} source_present=false", seq(&db)?);
    let publication =
        db.put_action_oracle_publication(&key, &raw, &record, ts_ns, &key, context)?;
    let reproduced = db.reproduce_calyx_record(&publication.constellation_cx_id)?;
    println!(
        "REPRODUCE seq={} cx={} reproduced={} entry_present={} entry_self_verifies={} drift={}",
        seq(&db)?,
        publication.constellation_cx_id,
        reproduced.reproduced,
        reproduced.entry_present,
        reproduced.entry_self_verifies,
        reproduced.drift
    );
    if !reproduced.reproduced || reproduced.drift != "none" {
        return Err("fresh derived record did not reproduce exactly".into());
    }

    let before_erase = seq(&db)?;
    let erased = db.erase_calyx_record(&publication.constellation_cx_id)?;
    println!(
        "ERASE before_seq={before_erase} after_seq={} records_deleted={} tombstone_present={} tombstone_seq={:?} chain={:?}",
        seq(&db)?,
        erased.records_deleted,
        erased.tombstone_present,
        erased.tombstone_seq,
        erased.chain_verify
    );

    let second_before = seq(&db)?;
    let second = db.erase_calyx_record(&publication.constellation_cx_id);
    println!(
        "EDGE_ALREADY_ABSENT before_seq={second_before} after_seq={} code={}",
        seq(&db)?,
        second.as_ref().err().map_or("none", |error| error.code())
    );
    let invalid_before = seq(&db)?;
    let invalid = db.erase_calyx_record("not-a-cx-id");
    println!(
        "EDGE_INVALID_ID before_seq={invalid_before} after_seq={} code={}",
        seq(&db)?,
        invalid.as_ref().err().map_or("none", |error| error.code())
    );

    let snapshot = seq(&db)?;
    let tombstone_seq = erased
        .tombstone_seq
        .ok_or("erase tombstone sequence absent")?;
    drop(db);
    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(root),
        None,
    )?;
    let cx_id = publication.constellation_cx_id.parse::<CxId>()?;
    let mut content_rows = 0usize;
    for family in [
        ColumnFamily::Base,
        ColumnFamily::Scalars,
        ColumnFamily::Anchors,
        ColumnFamily::Recurrence,
    ] {
        content_rows += readback
            .scan_cf_at(snapshot, family)?
            .iter()
            .filter(|(key, _)| key.windows(16).any(|window| window == cx_id.as_bytes()))
            .count();
    }
    for slot in [48_u16, 49, 50, 51, 52] {
        for kind in [SlotFamilyKind::Quantized, SlotFamilyKind::Raw] {
            content_rows += readback
                .scan_cf_at(
                    snapshot,
                    ColumnFamily::Slot {
                        slot: SlotId::new(slot),
                        kind,
                    },
                )?
                .iter()
                .filter(|(key, _)| key.windows(16).any(|window| window == cx_id.as_bytes()))
                .count();
        }
    }
    let ledger = readback.scan_cf_at(snapshot, ColumnFamily::Ledger)?;
    let ledger_entry_present = ledger
        .iter()
        .any(|(key, _)| key.as_slice() == tombstone_seq.to_be_bytes());
    let ledger_rows = ledger.len();
    println!(
        "PHYSICAL_SOT snapshot={snapshot} erased_cx_rows={content_rows} tombstone_ledger_seq={tombstone_seq} ledger_entry_present={} ledger_rows={ledger_rows}",
        ledger_entry_present
    );
    if content_rows != 0 || !ledger_entry_present || !erased.chain_verify.intact {
        return Err("erase physical source-of-truth conjunction failed".into());
    }
    Ok(())
}

fn seq(db: &Db) -> Result<u64, Box<dyn Error>> {
    Ok(db.calyx_vault_inspect()?.ok_or("vault absent")?.latest_seq)
}
