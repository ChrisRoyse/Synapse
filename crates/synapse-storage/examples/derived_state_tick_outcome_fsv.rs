//! Manual Full State Verification for #2088: the derived-state maintenance task
//! returns the tick's real outcome.
//!
//! ## The defect
//!
//! ```text
//! impl gc::GcRunner for CalyxDerivedStateRunner {
//!     fn run_once(&self) -> StorageResult<gc::GcReport> {
//!         crate::derived_state::run_derived_state_maintenance();   // -> ()
//!         Ok(gc::GcReport::default())
//!     }
//! }
//! ```
//!
//! `run_once` could not fail, so `STORAGE_MAINTENANCE_COMPLETED
//! operation="storage_derived_state" is_ok=…` was `true` on every tick including
//! the ones that failed, `retryable_gc_failure_kind` was handed a value that was
//! never `Err`, and the task's published report was a zeroed `GcReport` that
//! described nothing that happened. #2080 had already given the tick a real
//! verdict — a sub-pass failure ledger — and it stopped at the module boundary.
//!
//! ## How the failure is injected: a real one, not a stub
//!
//! `drive_app_transition_graph` folds consecutive distinct `FocusChange` apps
//! into a transition graph and publishes its spectral signatures. A path graph
//! `P_n` has adjacency eigenvalues `2*cos(k*pi/(n+1))`, so its two largest are
//! separated by `O(1/n^2)`; at `n = 400` the shifted power iteration's frozen
//! 256-iteration budget cannot reach `tol = 1e-6` and the publish is correctly
//! refused. A user who focuses 400 distinct applications in order produces
//! exactly that graph, so this is an input the sub-pass can genuinely receive.
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | a clean tick returns `Ok` | the pass's own return value, beside its ledger |
//! | 2 | a tick with a failed sub-pass returns `Err` naming the sub-pass codes | the returned `StorageError` |
//! | 3 | the error is derived, never double-counted | the sub-pass and tick counters across both ticks |
//! | 4 | the retry decision is where the tick loop can see it | `maintenance_failure_classification` on the real error |
//! | 5 | the loop still retries what it should | the same classifier on a real Calyx backpressure error |
//!
//! ```text
//! cargo run -p synapse-storage --example derived_state_tick_outcome_fsv -- <new-empty-vault-dir>
//! ```
//!
//! Writes. Point it at a scratch directory, never a vault anyone else is using.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::derived_state::{
    NoveltyDeliveryReadback, ReactiveDeliveryReadback, derived_state_readback,
    register_derived_state_source, register_novelty_delivery_sink, register_reactive_delivery_sink,
    register_region_delivery_sink, run_derived_state_maintenance,
};
use synapse_storage::{Db, StorageError, cf, maintenance_failure_classification};

/// Distinct apps in the converging graph. A 4-cycle: dense enough that the
/// shifted power iteration closes in a handful of steps.
const CLEAN_APPS: u64 = 4;
/// Focus rows in the clean seed.
const CLEAN_ROWS: u64 = 32;
/// Path length whose `O(1/n^2)` spectral gap the frozen 256-iteration budget
/// cannot close. Same constant, and same reasoning, as `graph_publish_leak_fsv`.
const CHAIN_APPS: u64 = 400;

struct Failures(Vec<String>);

impl Failures {
    fn check(&mut self, label: &str, held: bool, evidence: &str) {
        let verdict = if held { "PASS" } else { "FAIL" };
        println!("  [{verdict}] {label}: {evidence}");
        if !held {
            self.0.push(format!("{label}: {evidence}"));
        }
    }
}

fn focus_row(index: u64, app: String) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: 1_785_955_000_000_000_000 + index * 1_000_000_000,
        kind: TimelineKind::FocusChange,
        actor: TimelineActor::Human,
        app: Some(app),
        payload: json!({ "seq": index }),
    };
    Ok((
        format!("t{index:08}").into_bytes(),
        serde_json::to_vec(&record)?,
    ))
}

/// The three counters a tick moves, read after it completes.
fn counters() -> (u64, u64, u64, u64, u64, Option<bool>, Vec<String>) {
    let readback = derived_state_readback();
    (
        readback.attempts_total,
        readback.success_total,
        readback.failure_total,
        readback.skipped_total,
        readback.subpass_failures_total,
        readback.last_tick_failed,
        readback.last_tick_subpass_failures,
    )
}

fn print_tick(label: &str, outcome: &Result<(), StorageError>) {
    let (attempts, success, failure, skipped, subpass, last_failed, evidence) = counters();
    println!(
        "  {label} returned={} attempts={attempts} success={success} failure={failure} \
         skipped={skipped} subpass_failures={subpass} last_tick_failed={last_failed:?}",
        match outcome {
            Ok(()) => "Ok(())".to_owned(),
            Err(error) => format!("Err({})", error.code()),
        }
    );
    for line in &evidence {
        println!("    LEDGER {line}");
    }
    if let Err(error) = outcome {
        println!("    ERROR  {error}");
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one FSV run: seed, two real ticks, physical reads and checks are a single ordered narrative"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: derived_state_tick_outcome_fsv <new-empty-vault-dir>")?;
    // A scratch FSV vault on a CUDA host must say which math it wants: `Auto`
    // refuses rather than silently running CPU kernels on a GPU box. The
    // structural pass under test is CPU math either way.
    let mut config = synapse_calyx::SynapseCalyxConfig::from_vault_dir(dir.clone());
    config.tuning.math_backend = synapse_calyx::SynapseCalyxMathBackend::Cpu;
    let db = Arc::new(Db::open_with_resolved_calyx_config(
        &dir,
        SCHEMA_VERSION,
        synapse_storage::StorageBackendKind::default(),
        config,
    )?);
    println!("SOURCE_OF_TRUTH vault_dir={}", dir.display());
    register_derived_state_source(&db);
    // The daemon registers these three at startup; without them the Reactive,
    // region and novelty relays correctly fail closed on every tick and no tick
    // could ever be clean. Supplying the registration the daemon supplies is
    // setup, not a stub of the code under test — the sub-passes still run for
    // real and their findings still cross a real boundary.
    register_reactive_delivery_sink(|_finding| Ok(ReactiveDeliveryReadback::default()));
    register_region_delivery_sink(|_finding| Ok(ReactiveDeliveryReadback::default()));
    register_novelty_delivery_sink(|_finding| Ok(NoveltyDeliveryReadback::default()));

    // ------------------------------------------------------------------
    // Part 1 — a tick whose sub-passes all complete returns Ok
    // ------------------------------------------------------------------
    println!("\n== Part 1: a clean tick ==");
    let mut clean = Vec::new();
    for index in 0..CLEAN_ROWS {
        clean.push(focus_row(index, format!("app-{}", index % CLEAN_APPS))?);
    }
    db.put_batch(cf::CF_TIMELINE, clean)?;
    println!(
        "  SEEDED cf={} rows={} distinct_apps={CLEAN_APPS}",
        cf::CF_TIMELINE,
        db.scan_cf_prefix(cf::CF_TIMELINE, b"")?.len()
    );
    let clean_outcome = run_derived_state_maintenance();
    print_tick("TICK_1", &clean_outcome);
    let (_, success_1, failure_1, _, subpass_1, last_failed_1, _) = counters();
    f.check(
        "clean tick returns Ok",
        clean_outcome.is_ok(),
        &format!(
            "returned_ok={} last_tick_failed={last_failed_1:?}",
            clean_outcome.is_ok()
        ),
    );
    f.check(
        "the return value agrees with the tick ledger",
        clean_outcome.is_ok() == (last_failed_1 == Some(false)),
        &format!(
            "ok={} last_tick_failed={last_failed_1:?}",
            clean_outcome.is_ok()
        ),
    );

    // ------------------------------------------------------------------
    // Part 2 — a tick with a genuinely refused sub-pass returns Err
    // ------------------------------------------------------------------
    println!("\n== Part 2: a tick whose app-graph sub-pass is correctly refused ==");
    // The converging seed is removed first so the fold sees exactly `P_400` and
    // nothing else. Leaving the 4-cycle attached would make the graph a cycle
    // with a long tail, whose leading eigenvector localizes on the dense part
    // and whose gap the iteration closes — measured, on the first run of this
    // harness, as a tick that published normally.
    db.delete_batch(
        cf::CF_TIMELINE,
        (0..CLEAN_ROWS).map(|index| format!("t{index:08}").into_bytes()),
    )?;
    let mut chain = Vec::new();
    for index in 0..CHAIN_APPS {
        chain.push(focus_row(
            CLEAN_ROWS + index,
            format!("chain-app-{index:04}"),
        )?);
    }
    db.put_batch(cf::CF_TIMELINE, chain)?;
    println!(
        "  SEEDED cf={} rows={} path_graph_nodes={CHAIN_APPS}",
        cf::CF_TIMELINE,
        db.scan_cf_prefix(cf::CF_TIMELINE, b"")?.len()
    );
    let failed_outcome = run_derived_state_maintenance();
    print_tick("TICK_2", &failed_outcome);
    let (attempts_2, success_2, failure_2, skipped_2, subpass_2, last_failed_2, evidence_2) =
        counters();
    f.check(
        "a tick with a failed sub-pass returns Err",
        failed_outcome.is_err(),
        &format!(
            "returned_err={} last_tick_failed={last_failed_2:?}",
            failed_outcome.is_err()
        ),
    );
    let error_text = failed_outcome
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    f.check(
        "the error names the sub-pass code",
        error_text.contains("STORAGE_DERIVED_STATE_APP_GRAPH_FAILED"),
        &format!(
            "error_contains_subpass_code={} ledger={evidence_2:?}",
            error_text.contains("STORAGE_DERIVED_STATE_APP_GRAPH_FAILED")
        ),
    );
    f.check(
        "the error names the refusal that produced it",
        error_text.contains("CALYX_SPECTRAL_NOT_CONVERGED"),
        &format!(
            "error_carries_root_refusal={}",
            error_text.contains("CALYX_SPECTRAL_NOT_CONVERGED")
        ),
    );

    // ------------------------------------------------------------------
    // Part 3 — the error is derived from the ledger, not counted beside it
    // ------------------------------------------------------------------
    println!("\n== Part 3: no double counting ==");
    f.check(
        "the failing tick moved the tick failure counter exactly once",
        failure_2 == failure_1 + 1,
        &format!("failure_total {failure_1} -> {failure_2}"),
    );
    f.check(
        "the failing tick did not move the tick success counter",
        success_2 == success_1,
        &format!("success_total {success_1} -> {success_2}"),
    );
    f.check(
        "sub-pass failures moved by the number the ledger names, not by one more",
        subpass_2 - subpass_1 == evidence_2.len() as u64,
        &format!(
            "subpass_failures_total {subpass_1} -> {subpass_2} (delta {}) ledger_entries={}",
            subpass_2 - subpass_1,
            evidence_2.len()
        ),
    );
    f.check(
        "the tick counters still partition attempts",
        success_2 + failure_2 + skipped_2 == attempts_2,
        &format!("{success_2} + {failure_2} + {skipped_2} == {attempts_2}"),
    );

    // ------------------------------------------------------------------
    // Part 4/5 — the retry decision, where the tick loop reads it
    // ------------------------------------------------------------------
    println!("\n== Parts 4-5: the maintenance retry classification ==");
    let Err(real_error) = failed_outcome else {
        return Err("part 2 established this tick failed, so it must carry an error here".into());
    };
    let (classification, retryable, max_attempts) = maintenance_failure_classification(&real_error);
    println!(
        "  DERIVED_STATE_ERROR code={} classification={classification} retryable={retryable} \
         max_attempts={max_attempts}",
        real_error.code()
    );
    f.check(
        "a derived-state sub-pass failure is terminal, so the tick is not retried in place",
        !retryable && max_attempts == 1,
        &format!(
            "classification={classification} retryable={retryable} max_attempts={max_attempts}"
        ),
    );

    // The control. Without it, "not retryable" would be indistinguishable from a
    // classifier that calls everything terminal.
    let backpressure = StorageError::CalyxWriteFailed {
        cf_name: "calyx_constellation".to_owned(),
        code: synapse_calyx::SYNAPSE_CALYX_BACKPRESSURE,
        detail: "vault asked the caller to slow down".to_owned(),
        remediation: "retry after the write buffer drains",
        committed_seq: None,
    };
    let (bp_classification, bp_retryable, bp_max) =
        maintenance_failure_classification(&backpressure);
    println!(
        "  CONTROL_BACKPRESSURE code={} classification={bp_classification} \
         retryable={bp_retryable} max_attempts={bp_max}",
        backpressure.code()
    );
    f.check(
        "the same classifier still retries genuine contention",
        bp_retryable && bp_max > 1,
        &format!(
            "classification={bp_classification} retryable={bp_retryable} max_attempts={bp_max}"
        ),
    );

    println!();
    if f.0.is_empty() {
        println!(
            "VERDICT PASS: run_derived_state_maintenance returns the tick's real outcome, the error names its sub-pass failures, nothing is counted twice, and the retry decision is visible to the tick loop"
        );
        return Ok(());
    }
    for failure in &f.0 {
        println!("FAILED {failure}");
    }
    Err(format!("{} check(s) failed", f.0.len()).into())
}
