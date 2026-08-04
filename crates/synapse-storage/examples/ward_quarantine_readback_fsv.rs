//! Independent physical readback for Ward quarantine relay FSV (#1677/#1680).

use std::{error::Error, path::PathBuf};

use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf};

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: ward_quarantine_readback_fsv <vault-dir>")?;
    let db = Db::open(&root, SCHEMA_VERSION)?;
    let cursor = db.novelty_delivery_cursor()?;
    let findings = db.persisted_novelty_findings(0, 256)?;
    println!("NOVELTY cursor={cursor} rows={}", findings.len());
    for finding in &findings {
        println!("NOVELTY_ROW {}", serde_json::to_string(finding)?);
    }
    for prefix in [
        b"escalation/v1/item/".as_slice(),
        b"escalation/v1/audit/".as_slice(),
        b"escalation/v1/open/".as_slice(),
        b"approval/v1/item/".as_slice(),
        b"approval/v1/audit/".as_slice(),
    ] {
        let rows = db.scan_cf_prefix(cf::CF_KV, prefix)?;
        println!(
            "CF_KV_PREFIX {} rows={}",
            String::from_utf8_lossy(prefix),
            rows.len()
        );
        for (key, value) in rows {
            println!(
                "KV key={} value={}",
                String::from_utf8_lossy(&key),
                String::from_utf8_lossy(&value)
            );
        }
    }
    Ok(())
}
