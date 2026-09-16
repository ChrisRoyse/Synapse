//! Developer readback utility: dump `CF_ACTION_LOG` rows as JSON lines.
//!
//! Its output is supporting storage evidence only; manual FSV remains separate.
//! Run it against a *stopped* daemon's `--db` directory and diff the physical
//! action-audit log (including the #1006 foreground-tier policy block) against
//! what the live tools reported.
//!
//! ```text
//! cargo run -p synapse-mcp --example dump_action_log -- <db-path>
//! ```
//!
//! Errors out (non-zero exit) when the DB cannot be opened or a row fails to
//! decode — a corrupt audit row is a finding, not something to skip.

use synapse_storage::{Db, cf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::args()
        .nth(1)
        .ok_or("usage: dump_action_log <db-path>")?;
    let db = Db::open(std::path::Path::new(&db_path), synapse_core::SCHEMA_VERSION)?;
    let rows = db.scan_cf(cf::CF_ACTION_LOG)?;
    let mut invalid = 0usize;
    for (_key, value) in &rows {
        match serde_json::from_slice::<serde_json::Value>(value) {
            Ok(record) => println!("{record}"),
            Err(error) => {
                invalid += 1;
                eprintln!("INVALID ROW: {error}");
            }
        }
    }
    eprintln!("rows={} invalid={invalid}", rows.len());
    if invalid > 0 {
        return Err(format!("{invalid} action-log rows failed to decode").into());
    }
    Ok(())
}
