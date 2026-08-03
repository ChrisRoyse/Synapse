//! FSV for issue #1969: a read-only vault must fail closed on a column family
//! it never opened, instead of answering zero rows.
//!
//! ## What this proves, and against what
//!
//! The source of truth is a **copy of the live Calyx vault**, not a synthesized
//! one. The whole defect is that a partially-opened handle returns `0` where a
//! populated CF really holds tens of thousands of rows, so a fixture with an
//! empty `Base` CF would prove nothing: `0` would be the right answer. The
//! copy's real `Base` row count is established first, through a handle that
//! *did* open `Base`, and every refusal below is then measured against that
//! number.
//!
//! Run with the vault directory as argv[1]:
//!
//! ```text
//! cargo run --release -p synapse-calyx --example cf_selection_fail_closed_fsv -- <vault_dir>
//! ```
//!
//! Exits non-zero on the first failed check.

use std::path::PathBuf;
use std::process::ExitCode;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_core::SlotId;
use synapse_calyx::{
    SynapseCalyxCfRangePage, SynapseCalyxCfRows, SynapseCalyxConfig, SynapseCalyxError,
    SynapseCalyxReadOnlyVault,
};

const NOT_SELECTED: &str = "CALYX_ASTER_CF_NOT_SELECTED";

struct Checks {
    passed: usize,
    failed: usize,
}

impl Checks {
    fn check(&mut self, name: &str, ok: bool, detail: &str) {
        if ok {
            self.passed += 1;
            println!("  PASS  {name}\n          {detail}");
        } else {
            self.failed += 1;
            println!("  FAIL  {name}\n          {detail}");
        }
    }
}

/// Runs `op` and classifies the outcome as refused-with-the-right-code,
/// refused-with-a-different-code, or — the defect — answered successfully.
fn expect_refusal<T>(
    checks: &mut Checks,
    name: &str,
    answered: impl Fn(&T) -> String,
    op: impl FnOnce() -> Result<T, SynapseCalyxError>,
) {
    match op() {
        Ok(value) => checks.check(
            name,
            false,
            &format!(
                "the unselected-CF read SUCCEEDED and answered {}; this is exactly the defect \
                 (#1969) — a caller cannot tell this from the CF being empty",
                answered(&value)
            ),
        ),
        Err(error) if error.code == NOT_SELECTED => checks.check(
            name,
            true,
            &format!("refused with {}: {}", error.code, error.message),
        ),
        Err(error) => checks.check(
            name,
            false,
            &format!(
                "refused, but with {} rather than {NOT_SELECTED}: {}",
                error.code, error.message
            ),
        ),
    }
}

fn main() -> ExitCode {
    let Some(vault_dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cf_selection_fail_closed_fsv <vault_dir>");
        return ExitCode::FAILURE;
    };
    let mut checks = Checks {
        passed: 0,
        failed: 0,
    };
    println!(
        "cf_selection_fail_closed_fsv (#1969)\nvault_dir = {}\n",
        vault_dir.display()
    );

    // ---------------------------------------------------------------- phase A
    // Establish the source of truth: what does Base ACTUALLY hold? Through a
    // handle that opened it, so this number is not in question.
    println!("== Phase A: ground truth from a handle that opened Base ==");
    let config = SynapseCalyxConfig::from_vault_dir(vault_dir.clone());
    let full = match SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![ColumnFamily::Base, ColumnFamily::Kv]),
    ) {
        Ok(vault) => vault,
        Err(error) => {
            eprintln!("FATAL: could not open the vault with Base+Kv selected: {error}");
            return ExitCode::FAILURE;
        }
    };
    let base_rows = match full.scan_cf_latest(ColumnFamily::Base) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("FATAL: scanning Base on a handle that selected it failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let base_row_count = base_rows.len();
    let kv_row_count = full
        .scan_cf_latest(ColumnFamily::Kv)
        .map(|rows| rows.len())
        .unwrap_or(0);
    // A real key that really exists, for the point-read edge case below. A
    // refusal on a key that is absent anyway would prove nothing.
    let live_base_key = base_rows.first().map(|(key, _)| key.clone());
    println!(
        "  Base rows = {base_row_count}, Kv rows = {kv_row_count}, sample live Base key = {}",
        live_base_key
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |key| hex_prefix(key))
    );
    checks.check(
        "ground truth: Base is populated",
        base_row_count > 0,
        &format!(
            "{base_row_count} rows. If this were 0 the whole FSV would be vacuous, because 0 is \
             then the correct answer for every handle"
        ),
    );
    drop(full);

    // ---------------------------------------------------------------- phase B
    // The defect itself, on a handle that opened only Kv.
    println!("\n== Phase B: the same CF through a Kv-only handle ==");
    let config = SynapseCalyxConfig::from_vault_dir(vault_dir.clone());
    let kv_only = match SynapseCalyxReadOnlyVault::open_existing_kv_only(config) {
        Ok(vault) => vault,
        Err(error) => {
            eprintln!("FATAL: open_existing_kv_only failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "  BEFORE: this handle selected [Kv]; Base physically holds {base_row_count} rows on disk"
    );
    expect_refusal(
        &mut checks,
        "scan_cf_latest(Base) on a Kv-only handle",
        |rows: &SynapseCalyxCfRows| format!("{} rows (disk holds {base_row_count})", rows.len()),
        || kv_only.scan_cf_latest(ColumnFamily::Base),
    );

    // ---------------------------------------------------------------- phase C
    // The guard must not over-refuse. A Kv-only handle still has to serve Kv,
    // or this "fix" would just be a different wrong answer.
    println!("\n== Phase C: the selected CF must still be readable (no false refusal) ==");
    match kv_only.scan_cf_latest(ColumnFamily::Kv) {
        Ok(rows) => checks.check(
            "scan_cf_latest(Kv) on a Kv-only handle",
            rows.len() == kv_row_count,
            &format!(
                "answered {} rows against the {kv_row_count} the Base+Kv handle saw",
                rows.len()
            ),
        ),
        Err(error) => checks.check(
            "scan_cf_latest(Kv) on a Kv-only handle",
            false,
            &format!(
                "the selected CF was refused: [{}] {}",
                error.code, error.message
            ),
        ),
    }

    // ---------------------------------------------------------------- phase D
    // Edge cases. Every read shape must refuse, not just the scan the issue
    // happened to be found through.
    println!("\n== Phase D: edge cases — every read shape, not just the scan ==");

    // D1: a bounded range scan. Distinct from the whole-CF scan above because it
    // resolves through a different serving path, and it is the shape a readback
    // verifying a purge would most naturally use.
    expect_refusal(
        &mut checks,
        "edge 1/4: scan_cf_range_latest(Base, all) on a Kv-only handle",
        |rows: &SynapseCalyxCfRows| format!("{} rows (disk holds {base_row_count})", rows.len()),
        || kv_only.scan_cf_range_latest(ColumnFamily::Base, &KeyRange::all()),
    );

    // D2: a point read for a key that PROVABLY exists. This is the sharpest
    // case: `Ok(None)` here is a wrong answer about a row that is really there.
    if let Some(key) = &live_base_key {
        println!(
            "  BEFORE: key {} was read back from Base by the Base+Kv handle, so it exists",
            hex_prefix(key)
        );
        expect_refusal(
            &mut checks,
            "edge 2/4: read_cf_latest(Base, <a key that exists>) on a Kv-only handle",
            |value: &Option<Vec<u8>>| match value {
                Some(bytes) => format!("Some({} bytes)", bytes.len()),
                None => "None — a wrong answer about a row that provably exists".to_owned(),
            },
            || kv_only.read_cf_latest(ColumnFamily::Base, key),
        );
    } else {
        checks.check(
            "edge 2/4: read_cf_latest(Base, <a key that exists>)",
            false,
            "no live Base key was available to test with",
        );
    }

    // D3: the paged path. It bounds its hold correctly and would still have
    // answered an empty page.
    expect_refusal(
        &mut checks,
        "edge 3/4: scan_cf_range_page_latest(Base) on a Kv-only handle",
        |page: &SynapseCalyxCfRangePage| {
            format!("a page of {} rows, more={}", page.rows.len(), page.more)
        },
        || kv_only.scan_cf_range_page_latest(ColumnFamily::Base, &KeyRange::all(), None, 256),
    );

    // D4: a dynamic slot CF. The selected set is not just about static CFs, and
    // a slot CF is where the per-lens vectors live.
    expect_refusal(
        &mut checks,
        "edge 4/4: scan_cf_latest(slot 1) on a Kv-only handle",
        |rows: &SynapseCalyxCfRows| format!("{} rows", rows.len()),
        || kv_only.scan_cf_latest(ColumnFamily::slot(SlotId::new(1))),
    );

    println!(
        "\n== Result ==\n  passed = {}, failed = {}",
        checks.passed, checks.failed
    );
    if checks.failed == 0 {
        println!("  cf_selection_fail_closed_fsv: ALL CHECKS PASSED");
        ExitCode::SUCCESS
    } else {
        println!("  cf_selection_fail_closed_fsv: FAILED");
        ExitCode::FAILURE
    }
}

fn hex_prefix(key: &[u8]) -> String {
    key.iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}
