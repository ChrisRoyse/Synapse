//! Manual FSV for the process lane's edge-derivation input contract and its
//! zero-yield signal (#2089).
//!
//! # What was wrong, and therefore what has to be proven
//!
//! `syn-graphpos-process-v1` fuses two lanes into one published edge set: agent
//! spawn edges and process parent/child edges. On the live vault the process
//! lane contributed **zero** edges over 1085 rows, and nothing said so: the only
//! emptiness check (`AGENT_GRAPH_INELIGIBLE`) fires on the *fused* set, which
//! the spawn lane kept non-empty. A lane measuring nothing and a lane measuring
//! normally produced byte-identical logs.
//!
//! Two things therefore have to hold now:
//!
//! 1. **The contract.** A parent pid is admitted as an edge only with its
//!    pid-reuse guard attached — the parent's and child's creation times, with
//!    the parent created first. A bare parent pid, a pid the writer marked
//!    recycled, and a "verified" record whose own timestamps contradict it are
//!    all refused, each with its own named reason. Windows recycles pids
//!    (`devblogs.microsoft.com/oldnewthing/20150403-00`), so an unguarded pid is
//!    indistinguishable from a recycled one and a fabricated edge is worse than
//!    a missing one.
//! 2. **The signal.** A lane that examines rows and derives no edges from any of
//!    them logs `STORAGE_DERIVED_STATE_GRAPH_LANE_CONTRIBUTED_NO_EDGES`, with
//!    the row count, the edge count and the exact refusal census.
//!
//! # Where the truth is read from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | the contract admits exactly the guarded rows | the lane's own yield event, emitted by the real maintenance tick |
//! | 2 | every refusal is attributed to a named reason | the `skips` census on that event |
//! | 3 | a lane of rows that yields nothing is loud | the warn-level event, from a vault seeded to reproduce the live one |
//! | 4 | the published snapshot attributes its edges per lane | the publish event's `process_lane_*` fields |
//!
//! The evidence is the daemon's own structured log, captured through a
//! subscriber layer, because a return value cannot distinguish "this lane
//! contributed nothing" from "this lane was not consulted".
//!
//! ```text
//! cargo run -p synapse-storage --example process_lane_yield_fsv -- <new-empty-scratch-dir>
//! ```
//!
//! Writes. Point it at a scratch directory, never a vault anyone else is using.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;
use synapse_core::SCHEMA_VERSION;
use synapse_storage::derived_state::{
    register_derived_state_source, run_derived_state_maintenance_once,
};
use synapse_storage::{Db, cf};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

/// One captured structured event, flattened to its fields.
#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    code: String,
    lane: String,
    rows_examined: u64,
    edges_derived: u64,
    skips: String,
    process_lane_rows_examined: u64,
    process_lane_edges_derived: u64,
    process_lane_skips: String,
    level: String,
}

static EVENTS: std::sync::LazyLock<Arc<Mutex<Vec<CapturedEvent>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

#[derive(Default)]
struct EventVisitor(CapturedEvent);

impl Visit for EventVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "rows_examined" => self.0.rows_examined = value,
            "edges_derived" => self.0.edges_derived = value,
            "process_lane_rows_examined" => self.0.process_lane_rows_examined = value,
            "process_lane_edges_derived" => self.0.process_lane_edges_derived = value,
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "code" => self.0.code = value.to_owned(),
            "lane" => self.0.lane = value.to_owned(),
            "skips" => self.0.skips = value.to_owned(),
            "process_lane_skips" => self.0.process_lane_skips = value.to_owned(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        let rendered = rendered.trim_matches('"').to_owned();
        match field.name() {
            "code" => self.0.code = rendered,
            "lane" => self.0.lane = rendered,
            "skips" => self.0.skips = rendered,
            "process_lane_skips" => self.0.process_lane_skips = rendered,
            _ => {}
        }
    }
}

struct CaptureLayer;

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        if visitor.0.code.is_empty() {
            return;
        }
        visitor.0.level = event.metadata().level().to_string();
        let mut guard = match EVENTS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(visitor.0);
    }
}

fn captured() -> Vec<CapturedEvent> {
    let guard = match EVENTS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

fn reset_captured() {
    let mut guard = match EVENTS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clear();
}

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

/// Creation FILETIMEs, spaced so "parent first" and "parent second" are both
/// expressible exactly.
const EARLY_100NS: u64 = 134_300_000_000_000_000;
const LATE_100NS: u64 = 134_300_000_000_010_000;

fn row(key: &str, value: serde_json::Value) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    Ok((key.as_bytes().to_vec(), serde_json::to_vec(&value)?))
}

fn parentage(
    state: &str,
    child_pid: u32,
    parent_pid: Option<u32>,
    parent_start: Option<u64>,
    child_start: Option<u64>,
) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "state": state,
        "child_pid": child_pid,
        "child_start_time_100ns": child_start,
        "parent_pid_observed": parent_pid,
        "parent_start_time_100ns": parent_start,
        "parent_is_observer": false,
        "observer_pid": 4242,
        "start_time_source": "windows_ntquerysysteminformation_createtime_filetime_100ns",
        "source": "windows_ntquerysysteminformation_system_process_information",
        "observed_at_unix_ms": 1_786_000_000_000_u64,
    })
}

/// `(key, value)` byte pairs as they would sit physically in the CF.
type CfRowBytes = Vec<(Vec<u8>, Vec<u8>)>;

/// The rows the contract must refuse, one per refusal reason.
fn refused_rows() -> Result<CfRowBytes, Box<dyn Error>> {
    Ok(vec![
        // A root: the kernel named no parent at all.
        row(
            "process_history/v1/fsv/0002",
            json!({
                "row_kind": "process_start",
                "pid": 1000,
                "parentage": parentage("no_parent_recorded", 1000, None, None, Some(EARLY_100NS)),
            }),
        )?,
        // The writer itself detected pid reuse.
        row(
            "process_history/v1/fsv/0003",
            json!({
                "row_kind": "process_start",
                "pid": 1002,
                "parentage": parentage(
                    "parent_pid_recycled", 1002, Some(999), Some(LATE_100NS), Some(EARLY_100NS)),
            }),
        )?,
        // A legacy row: a bare parent pid with no guard evidence behind it.
        // Indistinguishable from a recycled pid, therefore refused.
        row(
            "process_history/v1/fsv/0004",
            json!({
                "row_kind": "process_start",
                "pid": 1003,
                "parent_pid": 1001,
            }),
        )?,
        // The exact shape of all 1085 rows on the live vault: a launch record
        // with a pid and no parentage of any kind.
        row(
            "process_history/v1/fsv/0005",
            json!({
                "row_kind": "process_start",
                "pid": 1004,
                "target": "pwsh.exe",
                "status": "started",
            }),
        )?,
        // A record claiming verification whose own timestamps contradict it.
        // The reader re-checks the guard rather than trusting the writer.
        row(
            "process_history/v1/fsv/0006",
            json!({
                "row_kind": "process_start",
                "pid": 1006,
                "parent_pid": 1001,
                "parentage": parentage(
                    "parent_verified", 1006, Some(1001), Some(LATE_100NS), Some(EARLY_100NS)),
            }),
        )?,
        // The flat pid the graph keys on disagrees with the observed evidence.
        row(
            "process_history/v1/fsv/0007",
            json!({
                "row_kind": "process_start",
                "pid": 1007,
                "parent_pid": 1001,
                "parentage": parentage(
                    "parent_verified", 1007, Some(2000), Some(EARLY_100NS), Some(LATE_100NS)),
            }),
        )?,
        // The observer could not see the process at all.
        row(
            "process_history/v1/fsv/0008",
            json!({
                "row_kind": "process_start",
                "pid": 1008,
                "parentage": parentage("child_absent", 1008, None, None, None),
            }),
        )?,
    ])
}

/// The one row the contract must admit.
fn admitted_row() -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    row(
        "process_history/v1/fsv/0001",
        json!({
            "row_kind": "process_start",
            "pid": 1001,
            "parent_pid": 1000,
            "parentage": parentage(
                "parent_verified", 1001, Some(1000), Some(EARLY_100NS), Some(LATE_100NS)),
        }),
    )
}

/// Open a scratch vault with CPU math.
///
/// A scratch FSV vault on a CUDA host must say which math it wants: `Auto`
/// refuses rather than silently running CPU kernels on a GPU box. The lane under
/// test is structural either way.
fn open_scratch_vault(dir: &Path) -> Result<Arc<Db>, Box<dyn Error>> {
    let mut config = synapse_calyx::SynapseCalyxConfig::from_vault_dir(dir.to_path_buf());
    config.tuning.math_backend = synapse_calyx::SynapseCalyxMathBackend::Cpu;
    Ok(Arc::new(Db::open_with_resolved_calyx_config(
        dir,
        SCHEMA_VERSION,
        synapse_storage::StorageBackendKind::default(),
        config,
    )?))
}

fn lane_event(events: &[CapturedEvent], code: &str) -> Option<CapturedEvent> {
    events
        .iter()
        .find(|event| event.code == code && event.lane == "process_parent_edges")
        .cloned()
}

#[allow(
    clippy::too_many_lines,
    reason = "one FSV run: two seeded vaults and their captured events are a single narrative"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: process_lane_yield_fsv <new-empty-scratch-dir>")?;
    tracing_subscriber::registry().with(CaptureLayer).init();
    println!("process_lane_yield_fsv  (#2089)\nroot = {}", root.display());

    // ------------------------------------------------------------------
    // Parts 1, 2 & 4 — the contract, on a vault holding one admissible row
    // ------------------------------------------------------------------
    println!("\n== Parts 1-2, 4: one guarded row among seven refusable ones ==");
    let mixed_dir = root.join("mixed");
    let mixed = open_scratch_vault(&mixed_dir)?;
    let mut rows = refused_rows()?;
    rows.push(admitted_row()?);
    let seeded_rows = rows.len();
    for (key, value) in &rows {
        println!(
            "  SEEDED {} {}",
            String::from_utf8_lossy(key),
            String::from_utf8_lossy(value)
        );
    }
    mixed.put_batch(cf::CF_PROCESS_HISTORY, rows)?;
    register_derived_state_source(&mixed);
    reset_captured();
    let _ = run_derived_state_maintenance_once();
    let events = captured();

    let yield_event = lane_event(&events, "STORAGE_DERIVED_STATE_GRAPH_LANE_YIELD")
        .ok_or("the process lane emitted no yield event on a vault that has rows")?;
    println!("  LANE_EVENT {yield_event:?}");
    f.check(
        "the lane examined every seeded row",
        yield_event.rows_examined == seeded_rows as u64,
        &format!(
            "rows_examined={} seeded={seeded_rows}",
            yield_event.rows_examined
        ),
    );
    f.check(
        "exactly the guarded row became an edge",
        yield_event.edges_derived == 1,
        &format!("edges_derived={}", yield_event.edges_derived),
    );
    for reason in [
        "parentage_no_parent_recorded",
        "parentage_parent_pid_recycled",
        "parent_pid_without_reuse_guard_evidence",
        "row_carries_no_parentage_observation",
        "parentage_start_time_guard_violated",
        "parent_pid_disagrees_with_parentage_evidence",
        "parentage_child_absent",
    ] {
        f.check(
            &format!("refusal `{reason}` is named in the census"),
            yield_event.skips.contains(reason),
            &yield_event.skips,
        );
    }

    let publish = events
        .iter()
        .find(|event| event.code == "STORAGE_DERIVED_STATE_AGENT_GRAPH_PUBLISHED")
        .cloned();
    match publish {
        Some(publish) => {
            println!("  PUBLISH_EVENT {publish:?}");
            f.check(
                "the published snapshot attributes its edges to the process lane",
                publish.process_lane_edges_derived == 1
                    && publish.process_lane_rows_examined == seeded_rows as u64,
                &format!(
                    "process_lane_rows_examined={} process_lane_edges_derived={} skips={}",
                    publish.process_lane_rows_examined,
                    publish.process_lane_edges_derived,
                    publish.process_lane_skips
                ),
            );
        }
        None => f.check(
            "the published snapshot attributes its edges to the process lane",
            false,
            "no publish event was emitted",
        ),
    }
    drop(mixed);

    // ------------------------------------------------------------------
    // Part 3 — the live vault's condition: rows, no edges, and now a warning
    // ------------------------------------------------------------------
    println!("\n== Part 3: a lane of rows that yields nothing ==");
    let silent_dir = root.join("unguarded");
    let silent = open_scratch_vault(&silent_dir)?;
    let refused = refused_rows()?;
    let refused_count = refused.len();
    silent.put_batch(cf::CF_PROCESS_HISTORY, refused)?;
    register_derived_state_source(&silent);
    reset_captured();
    let _ = run_derived_state_maintenance_once();
    let events = captured();

    let empty_event = lane_event(
        &events,
        "STORAGE_DERIVED_STATE_GRAPH_LANE_CONTRIBUTED_NO_EDGES",
    );
    match empty_event {
        Some(empty_event) => {
            println!("  LANE_EVENT {empty_event:?}");
            f.check(
                "a zero-yield lane is reported at warn level, not debug",
                empty_event.level == "WARN",
                &format!("level={}", empty_event.level),
            );
            f.check(
                "the zero-yield report carries the row population it refused",
                empty_event.rows_examined == refused_count as u64 && empty_event.edges_derived == 0,
                &format!(
                    "rows_examined={} edges_derived={} skips={}",
                    empty_event.rows_examined, empty_event.edges_derived, empty_event.skips
                ),
            );
        }
        None => f.check(
            "a lane with rows and no edges announces itself",
            false,
            "no CONTRIBUTED_NO_EDGES event was emitted",
        ),
    }
    f.check(
        "the yield event is not also emitted for the empty lane",
        lane_event(&events, "STORAGE_DERIVED_STATE_GRAPH_LANE_YIELD").is_none(),
        "a lane reports exactly one verdict per tick",
    );
    drop(silent);

    println!("\n== VERDICT ==");
    if f.0.is_empty() {
        println!("  ALL CHECKS PASSED");
        Ok(())
    } else {
        for failure in &f.0 {
            println!("  FAILED {failure}");
        }
        Err(format!("{} check(s) failed", f.0.len()).into())
    }
}
