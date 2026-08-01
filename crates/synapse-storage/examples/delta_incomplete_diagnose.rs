//! Diagnostic for `CALYX_SEARCH_DELTA_INCOMPLETE` (issue #1935).
//!
//! Prints, for one constellation, the MVCC sequence of its `Base` row and of
//! every slot row it declares. That is the fact set that says **which** writer
//! broke the "a live slot write is staged in the same atomic batch as its own
//! `Base` row" invariant:
//!
//! * every slot at one sequence and `Base` older  → a whole-constellation write
//!   that staged slots and skipped `Base`;
//! * one slot newer and the rest with `Base`      → a single-slot writer;
//! * `Base` at or after every slot                 → not this defect at all.
//!
//! # Usage
//!
//! ```text
//! cargo run -p synapse-storage --example delta_incomplete_diagnose -- <vault-parent-dir> <cx_id_hex>
//! ```
//!
//! Read-only. Point it at a copy so the live daemon keeps its writer lock.

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_core::CxId;
use synapse_storage::Db;

const SCHEMA_VERSION: u32 = 1;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let parent = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: delta_incomplete_diagnose <dir-containing-db-daemon> <cx_id_hex>")?;
    let cx_hex = args
        .next()
        .ok_or("usage: delta_incomplete_diagnose <dir-containing-db-daemon> <cx_id_hex>")?;
    let cx_id = CxId::from_str(&cx_hex).map_err(|error| format!("parse cx id: {error}"))?;
    let vault_dir = parent.join("db-daemon");

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let report = db.diagnose_constellation_row_sequences(cx_id)?;

    println!("delta_incomplete_diagnose");
    println!("  vault_dir     = {}", vault_dir.display());
    println!("  cx_id         = {}", report.cx_id);
    println!("  latest_seq    = {}", report.latest_seq);
    println!("  panel_version = {:?}", report.panel_version);
    println!("  base_present  = {}", report.base_row_present);
    println!("  base_row_seq  = {:?}", report.base_row_seq);
    println!("  declared slots and their row sequences:");
    for (slot, seq) in &report.slot_row_seqs {
        let marker = match (*seq, report.base_row_seq) {
            (Some(slot_seq), Some(base)) if slot_seq > base => "  <-- NEWER THAN Base",
            (None, _) => "  <-- declared but no visible slot row",
            _ => "",
        };
        println!("    slot_{slot:<5} seq={seq:?}{marker}");
    }
    let newer: Vec<u16> = report
        .slot_row_seqs
        .iter()
        .filter(|(_, seq)| match (seq, report.base_row_seq) {
            (Some(slot_seq), Some(base)) => *slot_seq > base,
            _ => false,
        })
        .map(|(slot, _)| *slot)
        .collect();
    println!();
    println!(
        "  slots newer than Base: {}/{} {newer:?}",
        newer.len(),
        report.slot_row_seqs.len()
    );
    Ok(())
}
