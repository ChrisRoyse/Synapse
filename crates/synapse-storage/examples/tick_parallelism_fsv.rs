//! Manual FSV for the parallelised derived-state maintenance tick (#2116) and
//! for the in-flight verdict window that made health latch `error` (#2145).
//!
//! # What was wrong, and therefore what has to be proven
//!
//! * **#2116.** `run_derived_state_maintenance` was 100% serial on one blocking
//!   thread: 26.8% duty cycle, 1.4 of 32 cores busy, 73 s average against a 60 s
//!   budget with a 110 s peak. Three graph lanes scanned three different column
//!   families one after another; three weave panels each took a 20 s budget one
//!   after another. None of them shared anything.
//! * **#2145.** The tick opened by clearing its own published verdict
//!   (`last_tick_subpass_failures.clear()`, `last_tick_failed = None`) and only
//!   republished it at the end. `health` maps a null verdict after a run to
//!   `error`, so for the 73-110 s the tick ran, health reported
//!   `calyx_derived_state.status=error` with an empty failure list beside it.
//!
//! # Where the truth is read from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | the parallel tick is faster | wall clock of one real tick, same seed, same vault bytes |
//! | 2 | it computes the same thing | SHA-256 over the physical `XTerm` + `Graph` CF row sets, read back through a read-only vault handle |
//! | 3 | the graph lanes derive the same edges | the derived-snapshot **fingerprints** the panel lifecycle records, which are a pure function of the edge set |
//! | 4 | the accounting invariant survives | `success + failure + skipped == attempts`, with one sub-pass deliberately broken |
//! | 5 | a failed tick recovers | the next clean tick republishes `last_tick_failed=Some(false)` |
//! | 6 | no guard is held wide or starved | `calyx_row_guard_sites` census after a parallel tick |
//! | 7 | the verdict is never null mid-tick | a reader thread polling the readback *while* a tick runs |
//!
//! # Why it is driven as separate processes
//!
//! Every counter this proves is process-global (`DERIVED_STATE_ATTEMPTS`, the
//! weave watermarks, the CF-count memo). Running the serial arm and the parallel
//! arm in one process would let the first arm's state decide the second arm's
//! work, which is exactly the contamination that makes an A/B meaningless. Each
//! subcommand is therefore one process against one vault directory, and the
//! comparison is done by the operator (or `tick_parallelism_fsv.sh`) over the
//! printed `key=value` lines.
//!
//! ```text
//! cargo run -p synapse-storage --example tick_parallelism_fsv -- seed    <dir> [panel_rows]
//! cargo run -p synapse-storage --example tick_parallelism_fsv -- tick    <dir>   # honours SYNAPSE_DERIVED_STATE_SUBPASS_WORKERS
//! cargo run -p synapse-storage --example tick_parallelism_fsv -- racetick <dir>  # tick + concurrent readback poller (#2145)
//! cargo run -p synapse-storage --example tick_parallelism_fsv -- corrupt <dir>   # inject one failing sub-pass
//! cargo run -p synapse-storage --example tick_parallelism_fsv -- repair  <dir>   # remove it again
//! cargo run -p synapse-storage --example tick_parallelism_fsv -- digest  <dir>
//! ```
//!
//! Writes. Point it at a scratch directory, never a vault anyone else is using.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use calyx_aster::cf::ColumnFamily;
use serde_json::json;
use sha2::{Digest, Sha256};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::{
    AgentEventKind, AgentEventRecord, EpisodeBoundary, EpisodeRecord, GenAiAttributes,
    TimelineActor, TimelineKind, TimelineRecord,
};
use synapse_storage::constellations::{
    SYN_AGENT_EVENT_PANEL_VERSION, SYN_EPISODE_PANEL_VERSION, SYN_GRAPHPOS_APP_PANEL_VERSION,
    SYN_GRAPHPOS_PROCESS_PANEL_VERSION, SYN_PATH_HIERARCHY_PANEL_VERSION,
    SYN_TIMELINE_PANEL_VERSION,
};
use synapse_storage::derived_state::{
    DERIVED_STATE_SUBPASS_WORKERS, NoveltyDeliveryReadback, ReactiveDeliveryReadback,
    derived_state_readback, register_derived_state_source, register_novelty_delivery_sink,
    register_reactive_delivery_sink, register_region_delivery_sink,
    run_derived_state_maintenance_once,
};
use synapse_storage::{Db, cf};

/// Measured constellations seeded per weave panel, by default.
///
/// Sized against `WEAVE_INTERVAL_MAX_RECORDS = 2_000`: the weave's cost is
/// quadratic in the records that share a slot, so a panel just under the cap is
/// the most expensive interval part the maintainer can be handed, which is the
/// case the 20 s per-panel budget exists for. Three panels of it is what a
/// serial tick spends up to 60 s of one core on.
const DEFAULT_PANEL_ROWS: u64 = 1_500;

/// Raw (unmeasured) rows seeded per graph-lane source CF.
///
/// The graph lanes scan their whole column family, so this is what makes the
/// scan halves cost anything at all. Deliberately larger than the weave seed:
/// the app-focus lane pages `CF_TIMELINE` 1,000 rows at a time and the point is
/// to make three such scans overlap rather than queue.
const GRAPH_LANE_ROWS: u64 = 60_000;

/// Graph-lane rows seeded by the equivalence arm.
///
/// Small deliberately: the equivalence claim is proven by the derived bytes
/// being identical, and a corpus that takes ten times as long to seed makes them
/// no more identical.
const EQUIVALENCE_GRAPH_LANE_ROWS: u64 = 3_000;

/// Base for every seeded timestamp. Fixed, so a re-seed of the same size
/// produces the same source bytes.
const TS_BASE_NS: u64 = 1_785_955_000_000_000_000;

/// Registers accepting no-op delivery sinks for the three reactive relays.
///
/// The daemon owns these boundaries; without them every tick outside the daemon
/// fails on `no daemon region sink is registered`, which would make a *clean*
/// tick unreachable and the #2145 recovery claim unprovable. They accept and
/// drop nothing, so a genuinely dropped delivery is still reachable if a relay
/// ever produces one.
fn register_accepting_sinks() {
    register_reactive_delivery_sink(|_finding| {
        Ok(ReactiveDeliveryReadback {
            matched: 1,
            queued: 1,
            dropped: 0,
        })
    });
    register_region_delivery_sink(|_finding| {
        Ok(ReactiveDeliveryReadback {
            matched: 1,
            queued: 1,
            dropped: 0,
        })
    });
    register_novelty_delivery_sink(|_finding| {
        Ok(NoveltyDeliveryReadback {
            notification: ReactiveDeliveryReadback {
                matched: 1,
                queued: 1,
                dropped: 0,
            },
            quarantine_escalated: false,
        })
    });
}

fn timeline_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: TS_BASE_NS + index * 1_000_000,
        kind: if index.is_multiple_of(3) {
            TimelineKind::BrowserNav
        } else {
            TimelineKind::FocusChange
        },
        actor: TimelineActor::Human,
        app: Some(format!("app-{}", index % 17)),
        payload: json!({
            "title": format!("window {index}"),
            "url": format!("https://example.invalid/a/{}/b/{}", index % 23, index % 7),
        }),
    };
    Ok((
        format!("t{index:010}").into_bytes(),
        serde_json::to_vec(&record)?,
    ))
}

fn agent_event_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let record = AgentEventRecord {
        record_version: 1,
        ts_ns: TS_BASE_NS + index * 1_000_000,
        kind: if index.is_multiple_of(5) {
            AgentEventKind::SpawnRequested
        } else {
            AgentEventKind::TurnStarted
        },
        session_id: Some(format!("session-{}", index % 97)),
        spawn_id: Some(format!("spawn-{index}")),
        reason_code: Some("session_initialized".to_owned()),
        end_state: None,
        state_from: None,
        state_to: None,
        attributes: GenAiAttributes::default(),
        payload: json!({ "seq": index, "started_by_session_id": format!("session-{}", index % 97) }),
    };
    Ok((
        format!("a{index:010}").into_bytes(),
        serde_json::to_vec(&record)?,
    ))
}

/// A process row carrying the full `#2089` parentage guard, so the process lane
/// derives real edges rather than counting refusals.
fn process_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let pid = 1_000 + index;
    let parent_pid = 1_000 + (index / 8);
    Ok((
        format!("p{index:010}").into_bytes(),
        serde_json::to_vec(&json!({
            "pid": pid,
            "parent_pid": parent_pid,
            "ts_ns": TS_BASE_NS + index * 1_000_000,
            "parentage": {
                "state": "parent_verified",
                "parent_pid": parent_pid,
                "parent_created_at_unix_ms": 1_785_955_000_000_u64,
                "child_created_at_unix_ms": 1_785_955_000_001_u64 + index,
            },
        }))?,
    ))
}

fn episode_record(index: u64) -> EpisodeRecord {
    let start_ts_ns = TS_BASE_NS + index * 1_000_000;
    EpisodeRecord {
        record_version: 1,
        ts_ns: start_ts_ns,
        episode_id: format!("ep1-{index:016x}"),
        start_ts_ns,
        end_ts_ns: start_ts_ns + 500_000,
        actor: TimelineActor::Human,
        app: Some(format!("app-{}", index % 17)),
        document: Some(format!("document-{}", index % 31)),
        url: Some(format!(
            "https://example.invalid/a/{}/b/{}",
            index % 23,
            index % 7
        )),
        title_first: Some(format!("episode {index} about topic {}", index % 31)),
        title_last: Some(format!(
            "a summary of episode {index} covering topic {} and topic {} with enough words in it              that the lexical lens has something to hash",
            index % 31,
            index % 13
        )),
        distinct_title_count: u32::try_from(index % 9).unwrap_or(1) + 1,
        row_count: index % 40 + 1,
        keystroke_count: index % 300,
        click_count: index % 50,
        interruption_count: u32::try_from(index % 5).unwrap_or(0),
        interrupted_ms: index % 900,
        started_because: EpisodeBoundary::AppSwitch,
        ended_because: EpisodeBoundary::IdleGap,
    }
}

/// SHA-256 over one native CF's whole latest row set, in physical key order.
///
/// Key and value lengths are hashed alongside the bytes so no two different row
/// sets can collide by concatenation.
fn cf_digest(
    vault: &SynapseCalyxReadOnlyVault,
    cf: ColumnFamily,
) -> Result<(usize, String), Box<dyn Error>> {
    let mut rows = vault.scan_cf_latest(cf)?;
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (key, value) in &rows {
        hasher.update((key.len() as u64).to_le_bytes());
        hasher.update(key);
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((rows.len(), hex))
}

/// Every derived-snapshot fingerprint a graph panel's lifecycle records.
///
/// The fingerprint is `graph_snapshot_fingerprint(&transitions)` — a pure
/// function of the edge set the lanes derived — so equality of these sets across
/// a serial and a parallel run is equality of the derived edge sets, proven
/// without depending on any timestamp or generation number the two runs cannot
/// share.
fn snapshot_fingerprints(db: &Db, panel_version: u32) -> Result<Vec<u64>, Box<dyn Error>> {
    let Some(state) = db.read_panel_lifecycle(panel_version)? else {
        return Ok(Vec::new());
    };
    let mut fingerprints: Vec<u64> = state
        .added_lenses
        .values()
        .filter_map(|added| match added.source_projection {
            synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection::DerivedSnapshot {
                snapshot,
                ..
            } => Some(snapshot),
            _ => None,
        })
        .collect();
    fingerprints.sort_unstable();
    fingerprints.dedup();
    Ok(fingerprints)
}

/// Seeds raw (unmeasured) rows in every weave-panel source CF, at a size whose
/// coverage sweeps **complete** inside one tick.
///
/// This is the equivalence arm that actually exercises the weave. The problem it
/// solves: a fresh process opens its weave watermark at registration time, so no
/// row seeded by an *earlier* process is ever inside the first tick's weave
/// window. The only records that land in it are the ones the tick's own coverage
/// backfill inserts. Those are non-deterministic when the sweep is cut off by
/// its wall-clock budget — and deterministic when the sweep runs to
/// `sweep_complete`, which is what this row count is chosen for. The weave then
/// receives an identical corpus in every arm, and any digest difference between
/// a serial and a parallel run is the parallelism and nothing else.
fn seed_debt(dir: &Path, rows: u64) -> Result<(), Box<dyn Error>> {
    let db = Arc::new(Db::open(dir, SCHEMA_VERSION)?);
    let mut timeline = Vec::with_capacity(usize::try_from(rows).unwrap_or(0));
    let mut agent_events = Vec::with_capacity(usize::try_from(rows).unwrap_or(0));
    let mut processes = Vec::with_capacity(usize::try_from(rows).unwrap_or(0));
    let mut episodes = Vec::with_capacity(usize::try_from(rows).unwrap_or(0));
    for index in 0..rows {
        timeline.push(timeline_row(index)?);
        agent_events.push(agent_event_row(index)?);
        processes.push(process_row(index)?);
        let record = episode_record(index);
        episodes.push((
            format!("e{index:010}").into_bytes(),
            serde_json::to_vec(&record)?,
        ));
    }
    db.put_batch(cf::CF_TIMELINE, timeline)?;
    db.put_batch(cf::CF_AGENT_EVENTS, agent_events)?;
    db.put_batch(cf::CF_PROCESS_HISTORY, processes)?;
    db.put_batch(cf::CF_EPISODES, episodes)?;
    println!("seed_debt_rows_per_cf={rows}");
    drop(db);
    Ok(())
}

fn seed(
    dir: &Path,
    panel_rows: u64,
    graph_rows: u64,
    measure_graph_rows: bool,
) -> Result<(), Box<dyn Error>> {
    let db = Arc::new(Db::open(dir, SCHEMA_VERSION)?);

    // --- Graph-lane sources.
    //
    // `measure_graph_rows` is the difference between the two arms this harness
    // runs, and it is the difference between two questions:
    //
    // * **perf arm (`seed`, raw rows).** Raw rows are coverage debt, so the
    //   tick's backfill phase has real work and the measured wall clock has the
    //   production shape.
    // * **equivalence arm (`seedeq`, measured rows).** Coverage debt makes the
    //   weave's INPUT non-deterministic: the backfill inserts constellations
    //   stamped `now`, which land inside the same tick's weave window, and how
    //   many it inserts is decided by a wall-clock budget. Two *serial* runs
    //   therefore disagree with each other, which would make any
    //   serial-vs-parallel digest comparison meaningless. Measuring at write
    //   time removes the debt and pins the weave input to exactly what was
    //   seeded.
    println!("seed_graph_lane_rows={graph_rows}");
    println!("seed_measure_graph_rows={measure_graph_rows}");
    let mut processes = Vec::with_capacity(usize::try_from(graph_rows).unwrap_or(0));
    for index in 0..graph_rows {
        processes.push(process_row(index)?);
    }
    db.put_batch(cf::CF_PROCESS_HISTORY, processes)?;
    if measure_graph_rows {
        register_derived_state_source(&db);
    }
    for index in 0..graph_rows {
        let (key, raw) = timeline_row(index)?;
        db.put_batch(cf::CF_TIMELINE, vec![(key.clone(), raw.clone())])?;
        if measure_graph_rows {
            let record: TimelineRecord = serde_json::from_slice(&raw)?;
            db.put_timeline_constellation(&key, &raw, &record)?;
        }
        let (key, raw) = agent_event_row(index)?;
        db.put_batch(cf::CF_AGENT_EVENTS, vec![(key.clone(), raw.clone())])?;
        if measure_graph_rows {
            let record: AgentEventRecord = serde_json::from_slice(&raw)?;
            db.put_agent_event_constellation(&key, &raw, &record)?;
        }
    }

    // --- Weave corpus: MEASURED, because the weave reads constellations, not
    //     source rows. Registration first so the watermark opens before these
    //     records are created and the first tick's interval contains them.
    if !measure_graph_rows {
        register_derived_state_source(&db);
    }
    println!("seed_panel_rows={panel_rows}");
    for index in 0..panel_rows {
        let (key, raw) = timeline_row(1_000_000 + index)?;
        let record: TimelineRecord = serde_json::from_slice(&raw)?;
        db.put_batch(cf::CF_TIMELINE, vec![(key.clone(), raw.clone())])?;
        db.put_timeline_constellation(&key, &raw, &record)?;

        let (key, raw) = agent_event_row(1_000_000 + index)?;
        let record: AgentEventRecord = serde_json::from_slice(&raw)?;
        db.put_batch(cf::CF_AGENT_EVENTS, vec![(key.clone(), raw.clone())])?;
        db.put_agent_event_constellation(&key, &raw, &record)?;

        let record = episode_record(index);
        let key = format!("e{index:010}").into_bytes();
        let raw = serde_json::to_vec(&record)?;
        db.put_batch(cf::CF_EPISODES, vec![(key.clone(), raw.clone())])?;
        db.put_episode_constellation(&key, &raw, &record)?;
        if index.is_multiple_of(250) {
            println!("  seeded_panel_rows={index}");
        }
    }
    println!(
        "seed_timeline_rows={}",
        db.scan_cf_prefix(cf::CF_TIMELINE, b"")?.len()
    );
    println!(
        "seed_agent_event_rows={}",
        db.scan_cf_prefix(cf::CF_AGENT_EVENTS, b"")?.len()
    );
    println!(
        "seed_process_rows={}",
        db.scan_cf_prefix(cf::CF_PROCESS_HISTORY, b"")?.len()
    );
    println!(
        "seed_episode_rows={}",
        db.scan_cf_prefix(cf::CF_EPISODES, b"")?.len()
    );
    drop(db);
    Ok(())
}

/// Prints one tick's wall clock and every field the comparison reads.
fn print_tick(elapsed_ms: u128, readback: &synapse_storage::derived_state::DerivedStateReadback) {
    println!("tick_elapsed_ms={elapsed_ms}");
    println!("attempts_total={}", readback.attempts_total);
    println!("success_total={}", readback.success_total);
    println!("failure_total={}", readback.failure_total);
    println!("skipped_total={}", readback.skipped_total);
    println!("subpass_failures_total={}", readback.subpass_failures_total);
    println!("advisories_total={}", readback.advisories_total);
    println!("last_tick_failed={:?}", readback.last_tick_failed);
    println!(
        "last_tick_subpass_failures_count={}",
        readback.last_tick_subpass_failures.len()
    );
    for entry in &readback.last_tick_subpass_failures {
        println!("  subpass_failure={entry}");
    }
    println!(
        "accounting_holds={}",
        readback
            .success_total
            .saturating_add(readback.failure_total)
            .saturating_add(readback.skipped_total)
            == readback.attempts_total
    );
    for (panel, action) in &readback.last_weave_actions {
        println!(
            "weave panel={panel} action={action} records={:?} xterm_rows={:?} graph_rows={:?} \
             backlog_ns={:?} pending_parts={:?}",
            readback.last_weave_records.get(panel),
            readback.last_weave_xterm_rows.get(panel),
            readback.last_weave_graph_rows.get(panel),
            readback.last_weave_backlog_ns.get(panel),
            readback.last_weave_pending_parts.get(panel),
        );
    }
    println!(
        "backfill action={:?} targets_owed={} targets_attempted={} targets_skipped={:?} pages={:?} \
         examined={:?} inserted={:?}",
        readback.last_backfill_action,
        readback.last_backfill_targets_owed,
        readback.last_backfill_targets_attempted,
        readback.last_backfill_targets_skipped,
        readback.last_backfill_pages,
        readback.last_backfill_examined_rows,
        readback.last_backfill_inserted_rows,
    );
}

fn tick(dir: &Path) -> Result<(), Box<dyn Error>> {
    let workers = std::env::var("SYNAPSE_DERIVED_STATE_SUBPASS_WORKERS")
        .unwrap_or_else(|_| DERIVED_STATE_SUBPASS_WORKERS.to_string());
    println!("configured_subpass_workers={workers}");
    let db = Arc::new(Db::open(dir, SCHEMA_VERSION)?);
    register_derived_state_source(&db);
    register_accepting_sinks();
    let started = std::time::Instant::now();
    let readback = run_derived_state_maintenance_once();
    print_tick(started.elapsed().as_millis(), &readback);

    // Part 6 — the row-guard census, read after the tick that just ran. A
    // parallel lane that took a wide guard, or starved one, shows up here and
    // nowhere else.
    let status = db.calyx_vault_status()?;
    let mut over_budget = 0_u64;
    let mut starved_holds = 0_u64;
    for site in &status.row_guard_census {
        if site.holds == 0 {
            continue;
        }
        over_budget = over_budget.saturating_add(site.over_budget_holds);
        starved_holds = starved_holds.saturating_add(site.starved_holds);
        println!(
            "guard_site site={} holds={} max_held_us={} mean_held_us={:?} over_budget={} starved={}",
            site.site,
            site.holds,
            site.max_held_us,
            format!("{:?}", site.mean_held_us),
            site.over_budget_holds,
            site.starved_holds
        );
    }
    println!("guard_over_budget_holds_total={over_budget}");
    println!("guard_starved_holds_total={starved_holds}");
    drop(db);
    Ok(())
}

/// Drives one tick while a second thread polls the published readback (#2145).
///
/// The poller is the whole point: it is standing where `health` stands. Before
/// the fix it would have observed `last_tick_failed=None` for the entire tick —
/// the state `health` renders as `calyx_derived_state.status=error`.
fn racetick(dir: &Path) -> Result<(), Box<dyn Error>> {
    let db = Arc::new(Db::open(dir, SCHEMA_VERSION)?);
    register_derived_state_source(&db);
    register_accepting_sinks();
    // One completed tick first, so there IS a previous verdict for the in-flight
    // window to preserve. Without it, `None` during the second tick would be
    // indistinguishable from `None` because nothing has run.
    let first = run_derived_state_maintenance_once();
    println!("priming_tick_last_tick_failed={:?}", first.last_tick_failed);

    let running = Arc::new(AtomicBool::new(true));
    let poll_flag = Arc::clone(&running);
    let poller = std::thread::spawn(move || {
        let mut samples = 0_u64;
        let mut null_verdicts = 0_u64;
        let mut verdict_evidence_disagreements = 0_u64;
        while poll_flag.load(Ordering::Relaxed) {
            let readback = derived_state_readback();
            samples += 1;
            match readback.last_tick_failed {
                None => null_verdicts += 1,
                Some(failed) => {
                    if failed == readback.last_tick_subpass_failures.is_empty() {
                        verdict_evidence_disagreements += 1;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (samples, null_verdicts, verdict_evidence_disagreements)
    });

    let started = std::time::Instant::now();
    let readback = run_derived_state_maintenance_once();
    running.store(false, Ordering::Relaxed);
    let (samples, null_verdicts, disagreements) = poller.join().map_err(|_| "poller panicked")?;
    print_tick(started.elapsed().as_millis(), &readback);
    println!("inflight_readback_samples={samples}");
    println!("inflight_null_verdicts={null_verdicts}");
    println!("inflight_verdict_evidence_disagreements={disagreements}");
    println!(
        "inflight_health_would_report_error={}",
        null_verdicts > 0 || disagreements > 0
    );
    drop(db);
    Ok(())
}

/// Injects exactly one failing sub-pass: a `CF_PROCESS_HISTORY` row whose value
/// is valid JSON but not a JSON object.
///
/// Chosen because it fails inside a **parallel** unit — the process-parent scan
/// lane — and its failure must still be attributed to the `agent_graph`
/// sub-pass, which is the lane fusion's own contract. A failure that could only
/// be produced on the driver thread would prove nothing about parallelisation.
fn corrupt(dir: &Path) -> Result<(), Box<dyn Error>> {
    let db = Db::open(dir, SCHEMA_VERSION)?;
    db.put_batch(
        cf::CF_PROCESS_HISTORY,
        vec![(b"p9999999999".to_vec(), b"\"not-an-object\"".to_vec())],
    )?;
    println!("injected_failing_row_cf={}", cf::CF_PROCESS_HISTORY);
    println!("injected_failing_row_key=p9999999999");
    Ok(())
}

fn repair(dir: &Path) -> Result<(), Box<dyn Error>> {
    let db = Db::open(dir, SCHEMA_VERSION)?;
    let (key, value) = process_row(9_999_999)?;
    // Replace the malformed row with a well-formed one under the same key, so
    // the corpus size is unchanged and the recovery tick differs from the failed
    // one in exactly one row's bytes.
    let _ = key;
    db.put_batch(
        cf::CF_PROCESS_HISTORY,
        vec![(b"p9999999999".to_vec(), value)],
    )?;
    println!("repaired_failing_row_key=p9999999999");
    Ok(())
}

/// Drives the injected-failure edge case and its recovery in one process.
///
/// One process on purpose: the claim is that the readback *recovers*, and that
/// is a statement about two consecutive ticks sharing one set of process-global
/// counters. Split across two processes those counters reset and the claim
/// evaporates.
fn faildrive(dir: &Path) -> Result<(), Box<dyn Error>> {
    let db = Arc::new(Db::open(dir, SCHEMA_VERSION)?);
    register_derived_state_source(&db);
    register_accepting_sinks();

    println!("--- tick 1: clean baseline ---");
    let started = std::time::Instant::now();
    print_tick(
        started.elapsed().as_millis(),
        &run_derived_state_maintenance_once(),
    );

    println!("--- injecting one failing sub-pass into a PARALLEL unit ---");
    db.put_batch(
        cf::CF_PROCESS_HISTORY,
        vec![(b"p9999999999".to_vec(), b"\"not-an-object\"".to_vec())],
    )?;
    println!("injected_failing_row=CF_PROCESS_HISTORY/p9999999999");

    println!("--- tick 2: one parallel sub-pass fails ---");
    let started = std::time::Instant::now();
    let failed = run_derived_state_maintenance_once();
    print_tick(started.elapsed().as_millis(), &failed);

    println!("--- repairing the injected row ---");
    let (_key, value) = process_row(9_999_999)?;
    db.put_batch(
        cf::CF_PROCESS_HISTORY,
        vec![(b"p9999999999".to_vec(), value)],
    )?;

    println!("--- tick 3: recovery ---");
    let started = std::time::Instant::now();
    let recovered = run_derived_state_maintenance_once();
    print_tick(started.elapsed().as_millis(), &recovered);
    println!(
        "recovery_verdict_cleared={}",
        recovered.last_tick_failed == Some(false)
            && recovered.last_tick_subpass_failures.is_empty()
    );
    println!(
        "historical_failure_counters_retained={}",
        recovered.failure_total >= 1 && recovered.last_failure_code.is_some()
    );
    println!(
        "recovery_last_failure_code={:?}",
        recovered.last_failure_code
    );
    drop(db);
    Ok(())
}

fn digest(dir: &Path) -> Result<(), Box<dyn Error>> {
    // Fingerprints first, through the writable handle, because the panel
    // lifecycle lives behind the normal storage API.
    {
        let db = Db::open(dir, SCHEMA_VERSION)?;
        for (name, panel_version) in [
            ("graphpos_app", SYN_GRAPHPOS_APP_PANEL_VERSION),
            ("graphpos_process", SYN_GRAPHPOS_PROCESS_PANEL_VERSION),
            ("path_hierarchy", SYN_PATH_HIERARCHY_PANEL_VERSION),
        ] {
            println!(
                "snapshot_fingerprints panel={name} values={:?}",
                snapshot_fingerprints(&db, panel_version)?
            );
        }
        for (name, panel_version) in [
            ("syn_timeline", SYN_TIMELINE_PANEL_VERSION),
            ("syn_episode", SYN_EPISODE_PANEL_VERSION),
            ("syn_agent_event", SYN_AGENT_EVENT_PANEL_VERSION),
        ] {
            println!("weave_panel_version {name}={panel_version}");
        }
    }
    // Then the native derived CFs, through a read-only inspection handle. Safe
    // to scan whole here and only here: the vault is closed, nothing is
    // committing, and this is the inspector rather than a maintenance lane.
    let config = SynapseCalyxConfig::from_vault_dir(dir.to_path_buf());
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![ColumnFamily::XTerm, ColumnFamily::Graph]),
    )?;
    for (name, cf) in [
        ("xterm", ColumnFamily::XTerm),
        ("graph", ColumnFamily::Graph),
    ] {
        let (rows, sha) = cf_digest(&vault, cf)?;
        println!("cf_digest cf={name} rows={rows} sha256={sha}");
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: tick_parallelism_fsv <seed|seedeq|seeddebt|tick|racetick|faildrive|corrupt|repair|digest> <dir> [panel_rows]")?;
    let dir = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: tick_parallelism_fsv <mode> <dir> [panel_rows]")?;
    match mode.as_str() {
        "seed" => {
            let panel_rows = args
                .next()
                .map(|raw| raw.parse::<u64>())
                .transpose()?
                .unwrap_or(DEFAULT_PANEL_ROWS);
            seed(&dir, panel_rows, GRAPH_LANE_ROWS, false)
        }
        "seedeq" => {
            let panel_rows = args
                .next()
                .map(|raw| raw.parse::<u64>())
                .transpose()?
                .unwrap_or(DEFAULT_PANEL_ROWS);
            seed(&dir, panel_rows, EQUIVALENCE_GRAPH_LANE_ROWS, true)
        }
        "seeddebt" => {
            let rows = args
                .next()
                .map(|raw| raw.parse::<u64>())
                .transpose()?
                .unwrap_or(1_000);
            seed_debt(&dir, rows)
        }
        "faildrive" => faildrive(&dir),
        "tick" => tick(&dir),
        "racetick" => racetick(&dir),
        "corrupt" => corrupt(&dir),
        "repair" => repair(&dir),
        "digest" => digest(&dir),
        other => Err(format!("unknown mode {other}").into()),
    }
}
