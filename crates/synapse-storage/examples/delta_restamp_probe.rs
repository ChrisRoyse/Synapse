//! Probe: is a slot CF's "changed key" count its *changes*, or its whole row
//! count? (issue #1935)
//!
//! `measure_panel_delta` treats a changed slot key with an unchanged `Base` row
//! as proof that a writer bypassed the same-batch contract. That inference is
//! only valid if the changed-key history records **writes**. This probe
//! compares, for each named CF, the number of keys reported changed after a
//! given sequence against the number of rows the CF holds at all.
//!
//! If `changed_after == total_rows` for a CF nothing has bulk-written, the
//! history is recording something other than writes — and every conclusion
//! drawn from it, including the corruption verdict, is drawn from the wrong
//! quantity.
//!
//! # Usage
//!
//! ```text
//! cargo run -p synapse-storage --example delta_restamp_probe -- <vault-parent-dir> <after_seq> <cf> [<cf>...]
//! ```
//!
//! Read-only. Point it at a copy.

use std::error::Error;
use std::path::PathBuf;

use synapse_storage::Db;

const SCHEMA_VERSION: u32 = 1;

#[expect(
    clippy::cast_precision_loss,
    reason = "the inspector reports an approximate ratio of two physical u64 row counters"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let parent = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: delta_restamp_probe <dir-containing-db-daemon> <after_seq> <cf>...")?;
    let after_seq: u64 = args
        .next()
        .ok_or("missing <after_seq>")?
        .parse()
        .map_err(|error| format!("parse after_seq: {error}"))?;
    let cfs: Vec<String> = args.collect();
    if cfs.is_empty() {
        return Err("name at least one column family".into());
    }
    let vault_dir = parent.join("db-daemon");
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let latest = db.calyx_vault_status()?.latest_seq.unwrap_or_default();

    println!("delta_restamp_probe");
    println!("  vault_dir  = {}", vault_dir.display());
    println!("  latest_seq = {latest}");
    println!("  after_seq  = {after_seq}");
    println!();
    println!(
        "  {:<14} {:>10} {:>14} {:>10}  verdict",
        "cf", "rows", "changed_after", "ratio"
    );
    for cf in &cfs {
        let rows = db.calyx_cf_row_count(cf)?;
        let changed = db.calyx_changed_key_count_after(cf, after_seq)?;
        // A ratio at or near 1.0 means the whole CF is reported changed. That
        // cannot be a write history for a CF whose rows are appended, not
        // rewritten.
        let ratio = if rows == 0 {
            0.0
        } else {
            changed as f64 / rows as f64
        };
        let verdict = if rows > 0 && changed >= rows {
            "WHOLE CF REPORTED CHANGED"
        } else if ratio > 0.5 {
            "majority reported changed"
        } else {
            "plausible as a write history"
        };
        println!("  {cf:<14} {rows:>10} {changed:>14} {ratio:>10.4}  {verdict}");
    }
    Ok(())
}
