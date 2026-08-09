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

use synapse_storage::{
    StorageBackendKind,
    action_log::{diagnostic_for_invalid_row, validate_action_log_row},
    cf, scan_cf_read_only,
};

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
    for (key, value) in &rows {
        match validate_action_log_row(key, value) {
            Ok(record) => println!("{}", record.value),
            Err(error) => {
                invalid += 1;
                let diagnostic = diagnostic_for_invalid_row(key, value, &error);
                eprintln!(
                    "INVALID ROW failure_code={} detail={} key_len_bytes={} key_sha256={} value_len_bytes={} value_sha256={}",
                    diagnostic.failure_code,
                    diagnostic.failure_detail,
                    diagnostic.key_len_bytes,
                    diagnostic.key_sha256,
                    diagnostic.value_len_bytes,
                    diagnostic.value_sha256,
                );
            }
        }
    }
    eprintln!("rows={} invalid={invalid}", rows.len());
    if invalid > 0 {
        return Err(format!("{invalid} action-log rows failed to decode").into());
    }
    Ok(())
}
