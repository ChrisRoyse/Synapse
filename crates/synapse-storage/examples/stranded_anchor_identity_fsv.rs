//! Read-only Full State Verification for #1982.
//!
//! Opens an explicitly named physical vault, folds the real Base CF census, and
//! prints the exact source-identity set difference between grounded superseded
//! generations and the active generation. No synthetic census and no mutation.

use std::error::Error;
use std::path::PathBuf;

use synapse_storage::Db;

const SCHEMA_VERSION: u32 = 1;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let vault_dir = PathBuf::from(
        args.next()
            .ok_or("usage: stranded_anchor_identity_fsv <exact-vault-directory>")?,
    );
    if args.next().is_some() {
        return Err("exactly one vault directory is required".into());
    }
    if !vault_dir.is_dir() {
        return Err(format!("vault directory does not exist: {}", vault_dir.display()).into());
    }

    println!("SOURCE OF TRUTH: {}", vault_dir.display());
    println!("BEFORE: read-only; no action has run");
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let report = db.measure_panel_coverage()?;
    println!(
        "BASE CF: rows={} decoded={} failures={} atomic={}",
        report.base_cf_rows,
        report.records_total,
        report.decode_failures,
        report.accounting_holds()
    );
    if report.decode_failures != 0 || report.base_cf_rows != report.records_total {
        return Err("Base CF census is incomplete; refusing a set-difference verdict".into());
    }

    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>10}",
        "panel", "active", "old-ground", "stranded", "unknown"
    );
    for panel in &report.panels {
        println!(
            "{:<28} {:>10} {:>10} {:>10} {:>10}",
            panel.panel_name,
            panel.grounded_records,
            panel.superseded_grounded_records,
            panel.anchors_stranded_on_superseded,
            panel.anchors_stranding_identity_unknown,
        );
    }
    println!("AFTER: read-only; physical state intentionally unchanged");
    println!("STRANDED PANELS: {:?}", report.anchors_stranded_panels);
    println!("PASS: exact source-identity comparison completed over every decoded Base row");
    Ok(())
}
