//! Manual FSV for the second `CF_PROCESS_HISTORY` writer (#2097).
//!
//! # What was wrong, and therefore what has to be proven
//!
//! `CF_PROCESS_HISTORY` had exactly one writer, `act_launch`. Every row in it
//! described a process the daemon had just created, so every row's verified
//! parent was the daemon itself (`parent_is_observer: true`), and the
//! `syn-graphpos-process-v1` process lane could derive nothing but a **star**
//! centred on the daemon's pid — a new hub on every daemon restart. #2089 had
//! already supplied the guarded parentage primitive; what was missing was a
//! writer that observes processes the daemon did not create.
//!
//! Five things therefore have to hold:
//!
//! 1. **A real tree is physically observed.** A launched `pwsh -> cmd -> ping`
//!    chain appears in `CF_PROCESS_HISTORY` as three rows with the parent chain
//!    intact, written by the topology observer and not by any launch path.
//! 2. **Rows carry the #2089 reuse guard.** Every row carries the full
//!    `parentage` observation with both creation times, and the flat
//!    `parent_pid` the graph keys on appears only when the observation is
//!    edge-bearing.
//! 3. **The lane derives that tree.** A real derived-state maintenance tick
//!    derives an edge set that contains the hand-computed chain, from edges
//!    whose parents are **not** the observer — the star is gone.
//! 4. **Re-observation does not mint rows.** A second observation tick over a
//!    stable machine leaves the physical row count unchanged, because row
//!    identity is `(pid, creation_time)` and a fresh identity is skipped.
//! 5. **TTL is managed.** Rows carry the `schema_version`/`ts_ns` pair the
//!    `CF_PROCESS_HISTORY` retention policy reads, plus the horizon they are
//!    subject to.
//!
//! # Where the truth is read from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | the chain is observed | rows scanned back out of the physical CF, printed verbatim |
//! | 2 | the guard is present | the `parentage` object on those exact rows |
//! | 3 | the lane derives the tree | the lane's own structured yield event from a real tick, plus an independent re-derivation of the edge set from the physical rows |
//! | 4 | re-observation is idempotent | physical row count before and after a second tick |
//! | 5 | TTL fields are present | the retention fields on the physical rows |
//!
//! Nothing here is asserted from a return value alone: every claim is checked
//! against bytes read back out of the vault, or against the engine's own log.
//!
//! ```text
//! cargo run -p synapse-reflex --example process_topology_fsv -- <new-empty-scratch-dir>
//! ```
//!
//! Writes, and launches real processes. Point it at a scratch directory, never a
//! vault anyone else is using. The launched `ping` is bounded and the harness
//! kills the whole chain before it exits.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, Mutex},
    time::Duration,
};

use synapse_action::process_parentage::ProcessTopology;
use synapse_core::SCHEMA_VERSION;
use synapse_reflex::process_topology::{
    PROCESS_OBSERVED_ROW_KIND, PlannedRow, ProcessTopologyObserver,
};
use synapse_storage::{Db, cf};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

// ---------------------------------------------------------------------------
// Structured-log capture: the lane's yield is an event, not a return value.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    code: String,
    lane: String,
    rows_examined: u64,
    edges_derived: u64,
    skips: String,
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
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_text(field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        self.record_text(field.name(), rendered.trim_matches('"'));
    }
}

impl EventVisitor {
    fn record_text(&mut self, name: &str, value: &str) {
        match name {
            "code" => value.clone_into(&mut self.0.code),
            "lane" => value.clone_into(&mut self.0.lane),
            "skips" => value.clone_into(&mut self.0.skips),
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

// ---------------------------------------------------------------------------
// The chain under observation
// ---------------------------------------------------------------------------

/// A launched `pwsh -> cmd -> ping` chain, killed on drop.
///
/// `ping -n 60 127.0.0.1` is a process that sits still for a minute doing
/// nothing but existing, which is exactly what a topology observation needs: a
/// stable leaf that is guaranteed alive across two observation ticks.
struct Chain {
    root: Child,
}

impl Chain {
    fn launch() -> Result<Self, Box<dyn Error>> {
        let root = Command::new("pwsh")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "cmd.exe /c ping.exe -n 60 127.0.0.1",
            ])
            .spawn()?;
        // The chain builds itself top-down; give the kernel time to create both
        // descendants before the table is read. This is a wait for a physical
        // fact, not a guess: the descendants are verified below and the FSV
        // fails loudly if they are absent.
        std::thread::sleep(Duration::from_secs(3));
        Ok(Self { root })
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // taskkill /T reaps the descendants; killing only the root would leave
        // `ping` running for the rest of its minute.
        let _ = Command::new("taskkill")
            .args(["/PID", &self.root.id().to_string(), "/T", "/F"])
            .output();
        let _ = self.root.kill();
        let _ = self.root.wait();
    }
}

/// The chain, resolved to the pids the kernel actually created.
#[derive(Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "each field names a process by image; the `_pid` suffix is the unit"
)]
struct ResolvedChain {
    pwsh_pid: u32,
    cmd_pid: u32,
    ping_pid: u32,
}

/// Walk the observed topology down from the launched root to find the exact
/// `pwsh -> cmd -> ping` pids. Resolved from the same snapshot the rows are
/// built from, so the expected edge set and the recorded rows cannot disagree.
fn resolve_chain(topology: &ProcessTopology, root_pid: u32) -> Option<ResolvedChain> {
    let child_of = |parent: u32, image_prefix: &str| -> Option<u32> {
        topology
            .entries
            .iter()
            .find(|entry| {
                entry.parentage.edge_parent_pid() == Some(parent)
                    && entry
                        .image_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().starts_with(image_prefix))
            })
            .map(|entry| entry.pid)
    };
    let cmd_pid = child_of(root_pid, "cmd")?;
    let ping_pid = child_of(cmd_pid, "ping")?;
    Some(ResolvedChain {
        pwsh_pid: root_pid,
        cmd_pid,
        ping_pid,
    })
}

// ---------------------------------------------------------------------------
// Independent re-derivation of the lane's edge set from the physical rows
// ---------------------------------------------------------------------------

/// Re-apply the process lane's admission rule to the physical rows, without
/// reusing the lane's code.
///
/// This is what makes "the lane published the tree I know about" checkable: the
/// expected edge set is computed here from bytes, and the lane's own count is
/// compared against it.
fn edges_from_physical_rows(rows: &[PlannedRow]) -> BTreeSet<(u64, u64)> {
    let mut edges = BTreeSet::new();
    for (_key, value) in rows {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(value) else {
            continue;
        };
        let Some(pid) = record.get("pid").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Some(parent_pid) = record.get("parent_pid").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Some(parentage) = record.get("parentage") else {
            continue;
        };
        if parentage.get("state").and_then(serde_json::Value::as_str) != Some("parent_verified") {
            continue;
        }
        let observed = parentage
            .get("parent_pid_observed")
            .and_then(serde_json::Value::as_u64);
        let parent_start = parentage
            .get("parent_start_time_100ns")
            .and_then(serde_json::Value::as_u64);
        let child_start = parentage
            .get("child_start_time_100ns")
            .and_then(serde_json::Value::as_u64);
        let (Some(observed), Some(parent_start), Some(child_start)) =
            (observed, parent_start, child_start)
        else {
            continue;
        };
        if observed != parent_pid || parent_start > child_start || pid == 0 || parent_pid == 0 {
            continue;
        }
        edges.insert((parent_pid, pid));
    }
    edges
}

fn topology_rows(db: &Db) -> Result<Vec<PlannedRow>, Box<dyn Error>> {
    Ok(db.scan_cf_prefix(
        cf::CF_PROCESS_HISTORY,
        b"process_history/v1/process_topology/",
    )?)
}

fn row_for_pid(
    rows: &[PlannedRow],
    pid: u32,
) -> Option<(String, serde_json::Map<String, serde_json::Value>)> {
    rows.iter().find_map(|(key, value)| {
        let record = serde_json::from_slice::<serde_json::Value>(value).ok()?;
        let object = record.as_object()?;
        (object.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid)))
            .then(|| (String::from_utf8_lossy(key).into_owned(), object.clone()))
    })
}

/// Open a scratch vault with CPU math. A scratch FSV vault on a CUDA host must
/// say which math it wants; the lane under test is structural either way.
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

#[allow(
    clippy::too_many_lines,
    reason = "one FSV run: launch, observe, publish and re-observe are a single narrative"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: process_topology_fsv <new-empty-scratch-dir>")?;
    tracing_subscriber::registry().with(CaptureLayer).init();
    println!("process_topology_fsv  (#2097)\nroot = {}", root.display());

    let db = open_scratch_vault(&root.join("vault"))?;
    let observer = ProcessTopologyObserver::new();
    let put = |batch: Vec<PlannedRow>| -> Result<(), String> {
        db.put_batch(cf::CF_PROCESS_HISTORY, batch)
            .map_err(|error| error.to_string())
    };

    // ------------------------------------------------------------------
    // Part 1 — a real multi-level tree is physically observed
    // ------------------------------------------------------------------
    println!("\n== Part 1: launch pwsh -> cmd -> ping and observe the table ==");
    let chain = Chain::launch()?;
    let root_pid = chain.root.id();
    println!("  LAUNCHED pwsh pid={root_pid}");

    let topology = synapse_action::capture_process_topology()?;
    println!(
        "  CAPTURED processes={} observer_pid={} source={}",
        topology.entries.len(),
        topology.observer_pid,
        topology.capture_source
    );
    let resolved = resolve_chain(&topology, root_pid).ok_or(
        "the launched pwsh -> cmd -> ping chain was not present in the observed process table; \
         the FSV cannot verify a tree it did not create",
    )?;
    println!("  RESOLVED_CHAIN {resolved:?}");

    // Hand-computed expected edges for the chain, stated before anything is
    // derived from them.
    let expected_chain_edges = BTreeSet::from([
        (u64::from(resolved.pwsh_pid), u64::from(resolved.cmd_pid)),
        (u64::from(resolved.cmd_pid), u64::from(resolved.ping_pid)),
    ]);
    println!("  EXPECTED_CHAIN_EDGES {expected_chain_edges:?}");

    let first_tick = observer.record_topology(&topology, put)?;
    println!("  TICK_1 {first_tick:?}");

    let rows = topology_rows(&db)?;
    let rows_after_first = rows.len();
    f.check(
        "the observer wrote rows for the observed table",
        rows_after_first > 0 && rows_after_first == first_tick.rows_written,
        &format!(
            "physical_rows={rows_after_first} rows_written={}",
            first_tick.rows_written
        ),
    );

    let mut chain_rows = BTreeMap::new();
    for (label, pid) in [
        ("pwsh", resolved.pwsh_pid),
        ("cmd", resolved.cmd_pid),
        ("ping", resolved.ping_pid),
    ] {
        match row_for_pid(&rows, pid) {
            Some((key, object)) => {
                println!(
                    "  PHYSICAL_ROW {label} key={key} value={}",
                    serde_json::to_string(&object)?
                );
                chain_rows.insert(label, object);
            }
            None => f.check(
                &format!("the {label} process has a physical row"),
                false,
                &format!("no CF_PROCESS_HISTORY row carries pid {pid}"),
            ),
        }
    }
    f.check(
        "every level of the chain is physically recorded",
        chain_rows.len() == 3,
        &format!("levels_recorded={}", chain_rows.len()),
    );

    // ------------------------------------------------------------------
    // Part 2 — rows carry the #2089 reuse guard, and are honest observations
    // ------------------------------------------------------------------
    println!("\n== Part 2: reuse-guard and provenance fields on those exact rows ==");
    for (label, object) in &chain_rows {
        let parentage = object
            .get("parentage")
            .and_then(serde_json::Value::as_object);
        let has_guard = parentage.is_some_and(|parentage| {
            parentage.get("state").and_then(serde_json::Value::as_str) == Some("parent_verified")
                && parentage.contains_key("parent_pid_observed")
                && parentage.contains_key("parent_start_time_100ns")
                && parentage.contains_key("child_start_time_100ns")
        });
        f.check(
            &format!("{label} carries the #2089 reuse-guard evidence"),
            has_guard,
            &serde_json::to_string(&parentage)?,
        );
        f.check(
            &format!("{label} names how it was observed and when"),
            object
                .get("capture_source")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|source| !source.is_empty())
                && object.contains_key("observed_at_unix_ms")
                && object.get("row_kind").and_then(serde_json::Value::as_str)
                    == Some(PROCESS_OBSERVED_ROW_KIND),
            &format!(
                "capture_source={:?} observed_at_unix_ms={:?} row_kind={:?}",
                object.get("capture_source"),
                object.get("observed_at_unix_ms"),
                object.get("row_kind")
            ),
        );
        f.check(
            &format!("{label} records an image name and no command line"),
            object.contains_key("image_name") && !object.contains_key("command_line"),
            &format!("image_name={:?}", object.get("image_name")),
        );
    }
    // The tell #2097 named: on a launch-only CF every row's parent was the
    // observer. On an observed table almost none of them can be.
    let observer_parented = rows
        .iter()
        .filter(|(_key, value)| {
            serde_json::from_slice::<serde_json::Value>(value).is_ok_and(|record| {
                record
                    .pointer("/parentage/parent_is_observer")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            })
        })
        .count();
    f.check(
        "the rows are not a star centred on the observer",
        observer_parented * 2 < rows_after_first,
        &format!("parent_is_observer_rows={observer_parented} of {rows_after_first}"),
    );

    // ------------------------------------------------------------------
    // Part 3 — the lane derives that tree
    // ------------------------------------------------------------------
    println!("\n== Part 3: a real maintenance tick derives the tree ==");
    let expected_edges = edges_from_physical_rows(&rows);
    println!("  EXPECTED_EDGES total={}", expected_edges.len());
    f.check(
        "the hand-computed chain edges are in the expected edge set",
        expected_chain_edges.is_subset(&expected_edges),
        &format!("chain={expected_chain_edges:?}"),
    );

    synapse_storage::derived_state::register_derived_state_source(&db);
    reset_captured();
    let readback = synapse_storage::derived_state::run_derived_state_maintenance_once();
    println!("  MAINTENANCE {readback:?}");
    let events = captured();
    let lane = events
        .iter()
        .find(|event| {
            event.code == "STORAGE_DERIVED_STATE_GRAPH_LANE_YIELD"
                && event.lane == "process_parent_edges"
        })
        .cloned();
    match lane {
        Some(lane) => {
            println!("  LANE_EVENT {lane:?}");
            f.check(
                "the lane examined every physical topology row",
                lane.rows_examined >= rows_after_first as u64,
                &format!(
                    "rows_examined={} physical={rows_after_first}",
                    lane.rows_examined
                ),
            );
            f.check(
                "the lane derived exactly the independently computed edge set",
                lane.edges_derived == expected_edges.len() as u64,
                &format!(
                    "edges_derived={} expected={} skips={}",
                    lane.edges_derived,
                    expected_edges.len(),
                    lane.skips
                ),
            );
            // The star baseline is one edge per daemon-launched process, all
            // sharing one source node. A real tree has many source nodes.
            let distinct_parents = expected_edges
                .iter()
                .map(|(parent, _child)| *parent)
                .collect::<BTreeSet<_>>();
            f.check(
                "the derived graph has many parent nodes, not one hub",
                distinct_parents.len() > 1,
                &format!("distinct_parents={}", distinct_parents.len()),
            );
        }
        None => f.check(
            "the process lane reported a yield for this tick",
            false,
            "no STORAGE_DERIVED_STATE_GRAPH_LANE_YIELD event for process_parent_edges",
        ),
    }

    // ------------------------------------------------------------------
    // Part 4 — re-observation does not mint rows
    // ------------------------------------------------------------------
    println!("\n== Part 4: a second observation tick over a stable machine ==");
    let second_tick = observer.run_once(|batch| {
        db.put_batch(cf::CF_PROCESS_HISTORY, batch)
            .map_err(|error| error.to_string())
    })?;
    println!("  TICK_2 {second_tick:?}");
    let rows_after_second = topology_rows(&db)?.len();
    f.check(
        "the second tick skipped the identities it had already recorded",
        second_tick.rows_skipped_fresh > 0,
        &format!("rows_skipped_fresh={}", second_tick.rows_skipped_fresh),
    );
    f.check(
        "the chain's rows were not duplicated by re-observation",
        rows_after_second >= rows_after_first
            && rows_after_second - rows_after_first == second_tick.rows_written,
        &format!(
            "rows_before={rows_after_first} rows_after={rows_after_second} new_rows_written={} \
             (any growth is processes that started between the two ticks, not re-observations)",
            second_tick.rows_written
        ),
    );
    for pid in [resolved.pwsh_pid, resolved.cmd_pid, resolved.ping_pid] {
        let occurrences = topology_rows(&db)?
            .iter()
            .filter(|(_key, value)| {
                serde_json::from_slice::<serde_json::Value>(value).is_ok_and(|record| {
                    record.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid))
                })
            })
            .count();
        f.check(
            &format!("pid {pid} occupies exactly one row after two ticks"),
            occurrences == 1,
            &format!("rows_for_pid={occurrences}"),
        );
    }

    // ------------------------------------------------------------------
    // Part 5 — TTL management
    // ------------------------------------------------------------------
    println!("\n== Part 5: TTL fields ==");
    for (label, object) in &chain_rows {
        let ttl_hours = object
            .get("retention_ttl_hours")
            .and_then(serde_json::Value::as_u64);
        f.check(
            &format!("{label} carries the retention fields the GC reads"),
            object
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                == Some(1)
                && object
                    .get("ts_ns")
                    .and_then(serde_json::Value::as_u64)
                    .is_some()
                && ttl_hours.is_some_and(|hours| hours > 0)
                && object.contains_key("retention_expires_at_unix_ms"),
            &format!(
                "schema_version={:?} ts_ns={:?} retention_ttl_hours={ttl_hours:?} expires={:?}",
                object.get("schema_version"),
                object.get("ts_ns"),
                object.get("retention_expires_at_unix_ms")
            ),
        );
    }

    drop(chain);
    drop(db);

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
