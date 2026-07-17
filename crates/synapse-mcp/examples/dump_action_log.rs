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

use std::path::Path;

use synapse_storage::{StorageBackendKind, cf, scan_cf_read_only};

const USAGE: &str = "usage: dump_action_log <db-path>";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let db_path = args.next().ok_or(USAGE)?;
    if let Some(extra) = args.next() {
        return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
    }
    let rows = scan_cf_read_only(
        Path::new(&db_path),
        synapse_core::SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        cf::CF_ACTION_LOG,
    )?;
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
