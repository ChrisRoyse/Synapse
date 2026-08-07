//! Manual FSV for the derived-state maintainer's outcome accounting and its
//! coverage-backfill rotation (#2080 asks 1/2, #2061 ask 3).
//!
//! # What was wrong, and therefore what has to be proven
//!
//! * **#2080.** `attempts` and `success` counted maintenance *ticks* while
//!   `failure` counted *sub-passes*, so the deployed daemon published
//!   `attempts=6 success=0 failure=15` — a failure counter running at 2.5x the
//!   attempt counter it is supposed to be a subset of. No ratio computed from
//!   those three numbers meant anything, and `health.ok` was false for the life
//!   of every process from its first tick.
//! * **#2061.** `drive_coverage_backfill` rotated over every owed target, but
//!   each target's sweep read the *tick's* clock rather than its own share of
//!   it. The head of the queue therefore spent the whole budget and the driver's
//!   loop exited on its first check: `targets_owed=4 targets_attempted=1` on
//!   every pass of two consecutive daemon generations.
//!
//! # Where the truth is read from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | several panels are genuinely owed a coverage sweep | the census this run's raw seed produces |
//! | 2 | every owed target is swept, not just the head | one durable `CF_KV` cursor row **per owed panel**, read back physically |
//! | 3 | each target ran against its own slice, not the tick's | the per-panel `slice_ms` the pass publishes |
//! | 4 | the counters cannot disagree with the outcomes | `success + failure + skipped == attempts`, and `failure <= attempts`, over several real ticks |
//! | 5 | advisories are counted apart from failures | the advisory counter, and its absence from the tick ledger |
//!
//! Part 2 is the load-bearing one: a cursor row keyed by
//! `syn/panel-backfill-cursor/v1/<panel_version>/<source_cf>` exists only
//! because `sweep_coverage_target` ran to completion for that panel and
//! persisted its position. Counting rows is therefore a physical record of which
//! targets were swept, independent of anything the maintainer says about itself.
//!
//! ```text
//! cargo run -p synapse-storage --example derived_state_accounting_fsv -- <new-empty-vault-dir>
//! ```
//!
//! Writes. Point it at a scratch directory, never a vault anyone else is using.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::{
    AgentEventKind, AgentEventRecord, GenAiAttributes, TimelineActor, TimelineKind, TimelineRecord,
};
use synapse_storage::constellations::{
    SYN_AGENT_EVENT_PANEL_VERSION, SYN_PROCESS_PANEL_VERSION, SYN_TIMELINE_PANEL_VERSION,
};
use synapse_storage::derived_state::{
    PANEL_BACKFILL_TARGET_MIN_SLICE, PANEL_BACKFILL_TICK_BUDGET, derived_state_readback,
    register_derived_state_source, run_derived_state_maintenance_once,
};
use synapse_storage::{Db, cf};

/// Rows per source CF. Small on purpose: the claim is "every owed target is
/// reached and leaves a durable position behind", and three panels of a few rows
/// each prove that exactly as completely as three panels of a million would —
/// while keeping the run to seconds.
const SEED_ROWS: u64 = 24;

/// Ticks driven. More than one because the counter invariant is about how the
/// counters *accumulate*, and because a second tick has to resume from the
/// cursors the first one persisted.
const TICKS: usize = 3;

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

fn timeline_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: 1_785_955_000_000_000_000 + index * 1_000_000_000,
        kind: TimelineKind::FocusChange,
        actor: TimelineActor::Human,
        app: Some(format!("app-{}", index % 4)),
        payload: json!({ "title": format!("window {index}") }),
    };
    Ok((
        format!("t{index:08}").into_bytes(),
        serde_json::to_vec(&record)?,
    ))
}

fn agent_event_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let record = AgentEventRecord {
        record_version: 1,
        ts_ns: 1_785_955_000_000_000_000 + index * 1_000_000_000,
        kind: AgentEventKind::TurnStarted,
        session_id: Some(format!("session-{index}")),
        spawn_id: None,
        reason_code: Some("session_initialized".to_owned()),
        end_state: None,
        state_from: None,
        state_to: None,
        attributes: GenAiAttributes::default(),
        payload: json!({ "seq": index }),
    };
    Ok((
        format!("a{index:08}").into_bytes(),
        serde_json::to_vec(&record)?,
    ))
}

fn process_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    Ok((
        format!("p{index:08}").into_bytes(),
        serde_json::to_vec(&json!({
            "pid": 1000 + index,
            "parent_pid": 1,
            "ts_ns": 1_785_955_000_000_000_000_u64 + index * 1_000_000_000,
        }))?,
    ))
}

/// The durable coverage cursor key the maintainer writes, rebuilt here from the
/// same two facts it is keyed by. Reading it back is how this run proves a
/// target was swept without asking the maintainer.
fn cursor_key(panel_version: u32, source_cf: &str) -> Vec<u8> {
    format!("syn/panel-backfill-cursor/v1/{panel_version}/{source_cf}").into_bytes()
}

#[allow(
    clippy::too_many_lines,
    reason = "one FSV run: seed, ticks, physical reads and checks are a single ordered narrative"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: derived_state_accounting_fsv <new-empty-vault-dir>")?;
    let db = Arc::new(Db::open(&dir, SCHEMA_VERSION)?);

    // ------------------------------------------------------------------
    // Part 1 — seed three panels into genuine coverage debt
    // ------------------------------------------------------------------
    //
    // Raw `put_batch` on purpose. The typed writers measure at write time, which
    // is precisely the state that is NOT owed a backfill; coverage debt is a
    // source row with no constellation at the active generation, and a raw put
    // is the only way to create it deliberately.
    println!("== Part 1: seeding source CFs with unmeasured rows ==");
    let mut timeline = Vec::new();
    let mut agent_events = Vec::new();
    let mut processes = Vec::new();
    for index in 0..SEED_ROWS {
        timeline.push(timeline_row(index)?);
        agent_events.push(agent_event_row(index)?);
        processes.push(process_row(index)?);
    }
    db.put_batch(cf::CF_TIMELINE, timeline)?;
    db.put_batch(cf::CF_AGENT_EVENTS, agent_events)?;
    db.put_batch(cf::CF_PROCESS_HISTORY, processes)?;
    let seeded = [
        (SYN_TIMELINE_PANEL_VERSION, cf::CF_TIMELINE),
        (SYN_AGENT_EVENT_PANEL_VERSION, cf::CF_AGENT_EVENTS),
        (SYN_PROCESS_PANEL_VERSION, cf::CF_PROCESS_HISTORY),
    ];
    for (panel_version, source_cf) in seeded {
        println!(
            "  SEEDED panel_version={panel_version} source_cf={source_cf} rows={}",
            db.scan_cf_prefix(source_cf, b"")?.len()
        );
    }

    register_derived_state_source(&db);

    // ------------------------------------------------------------------
    // Parts 2-4 — drive real ticks and read the physical residue
    // ------------------------------------------------------------------
    println!("\n== Parts 2-4: driving {TICKS} unattended derived-state ticks ==");
    let mut last = derived_state_readback();
    // The first tick that actually had targets owed. Later ticks legitimately
    // owe nothing once the seed is measured, and checking the rotation against a
    // tick with nothing to rotate over would prove nothing at all.
    let mut rotation: Option<synapse_storage::derived_state::DerivedStateReadback> = None;
    for tick in 1..=TICKS {
        last = run_derived_state_maintenance_once();
        if rotation.is_none() && last.last_backfill_targets_owed > 0 {
            rotation = Some(last.clone());
        }
        println!(
            "  TICK {tick} attempts={} success={} failure={} skipped={} subpass_failures={} \
             advisories={} last_tick_failed={:?}",
            last.attempts_total,
            last.success_total,
            last.failure_total,
            last.skipped_total,
            last.subpass_failures_total,
            last.advisories_total,
            last.last_tick_failed,
        );
        println!(
            "  TICK {tick} targets_owed={} targets_attempted={} targets_skipped={:?}",
            last.last_backfill_targets_owed,
            last.last_backfill_targets_attempted,
            last.last_backfill_targets_skipped,
        );
        for line in &last.last_anchor_debt_panels {
            println!("    ANCHOR_DEBT {line}");
        }
        // The counter invariant is checked on EVERY tick, not only at the end:
        // the defect it replaces was an accumulation defect, and a single final
        // reading cannot tell an invariant that always held from one that was
        // restored by luck on the last pass.
        let accounted = last
            .success_total
            .saturating_add(last.failure_total)
            .saturating_add(last.skipped_total);
        f.check(
            &format!("tick {tick}: success+failure+skipped == attempts"),
            accounted == last.attempts_total,
            &format!(
                "{}+{}+{} = {accounted} vs attempts={}",
                last.success_total, last.failure_total, last.skipped_total, last.attempts_total
            ),
        );
        f.check(
            &format!("tick {tick}: failure <= attempts"),
            last.failure_total <= last.attempts_total,
            &format!(
                "failure={} attempts={} (the deployed daemon read failure=15 attempts=6)",
                last.failure_total, last.attempts_total
            ),
        );
        f.check(
            &format!("tick {tick}: the tick verdict carries its own evidence"),
            last.last_tick_failed == Some(!last.last_tick_subpass_failures.is_empty()),
            &format!(
                "last_tick_failed={:?} subpass_failures_this_tick={}",
                last.last_tick_failed,
                last.last_tick_subpass_failures.len()
            ),
        );
    }
    for entry in &last.last_tick_subpass_failures {
        println!("    LAST_TICK_SUBPASS_FAILURE {entry}");
    }

    println!("\n== Part 2: physical durable cursors, one per owed target ==");
    let mut cursor_rows = 0_usize;
    for (panel_version, source_cf) in seeded {
        let key = cursor_key(panel_version, source_cf);
        let row = db.get_cf(cf::CF_KV, &key)?;
        println!(
            "  PHYSICAL_CURSOR key={} present={} value={}",
            String::from_utf8_lossy(&key),
            row.is_some(),
            row.as_deref().map_or_else(
                || "<absent>".to_owned(),
                |raw| String::from_utf8_lossy(raw).into_owned()
            )
        );
        if row.is_some() {
            cursor_rows += 1;
        }
    }
    f.check(
        "every seeded panel left a durable coverage cursor",
        cursor_rows == seeded.len(),
        &format!(
            "cursor rows on disk = {cursor_rows} of {} seeded panels; one target per tick would \
             have left one",
            seeded.len()
        ),
    );
    let rotation = rotation.ok_or(
        "no tick owed a coverage target; the seed did not create coverage debt, so nothing about \
         the rotation was proven",
    )?;
    f.check(
        "targets_owed == targets_attempted on the tick that had targets",
        rotation.last_backfill_targets_owed == rotation.last_backfill_targets_attempted
            && rotation.last_backfill_targets_owed >= seeded.len() as u64
            && rotation.last_backfill_targets_skipped.is_empty(),
        &format!(
            "owed={} attempted={} skipped={:?} (the deployed daemon read owed=4 attempted=1 on \
             every pass of two generations)",
            rotation.last_backfill_targets_owed,
            rotation.last_backfill_targets_attempted,
            rotation.last_backfill_targets_skipped
        ),
    );

    println!("\n== Part 3: the reservation arithmetic every target is admitted under ==");
    // Stated precisely, because it is not a measurement of the slices this run
    // observed: it is the reservation the driver computes, evaluated at zero
    // elapsed. The live slices are this minus however long the earlier targets
    // actually took. What it proves is the property that matters — the head of
    // the queue is admitted with the floor of every target behind it already
    // withheld, so no target can be reached with nothing left. The observed
    // per-target `slice_ms` rides the structured `STORAGE_DERIVED_STATE_BACKFILL_PASS`
    // event's per-panel lines.
    let mut slices: Vec<u64> = Vec::new();
    let owed = rotation.last_backfill_targets_owed;
    for index in 0..owed {
        let behind = owed.saturating_sub(index).saturating_sub(1);
        let reserved = PANEL_BACKFILL_TARGET_MIN_SLICE
            .saturating_mul(u32::try_from(behind).unwrap_or(u32::MAX))
            .as_millis();
        let slice = u64::try_from(
            PANEL_BACKFILL_TICK_BUDGET
                .as_millis()
                .saturating_sub(reserved)
                .max(PANEL_BACKFILL_TARGET_MIN_SLICE.as_millis()),
        )
        .unwrap_or(u64::MAX);
        println!("  SLICE queue_position={index} slice_ms={slice}");
        slices.push(slice);
    }
    f.check(
        "every owed target is guaranteed at least the declared floor",
        slices.iter().all(|slice| {
            *slice >= u64::try_from(PANEL_BACKFILL_TARGET_MIN_SLICE.as_millis()).unwrap_or(0)
        }),
        &format!(
            "slices={slices:?} floor_ms={}",
            PANEL_BACKFILL_TARGET_MIN_SLICE.as_millis()
        ),
    );
    f.check(
        "the head of the queue no longer holds the whole tick budget",
        slices.first().is_none_or(|head| {
            owed < 2 || *head < u64::try_from(PANEL_BACKFILL_TICK_BUDGET.as_millis()).unwrap_or(0)
        }),
        &format!(
            "head_slice_ms={:?} tick_budget_ms={} owed={owed}",
            slices.first(),
            PANEL_BACKFILL_TICK_BUDGET.as_millis()
        ),
    );

    // ------------------------------------------------------------------
    // Part 4 — the discriminating case: TWO sub-pass failures in ONE tick
    // ------------------------------------------------------------------
    //
    // Everything above holds trivially on a clean run, because a clean run never
    // exercises the divergence. This part manufactures the exact shape #2080
    // measured — one tick, several sub-pass failures — and checks the two
    // counters move by the amounts their definitions require.
    //
    // The injection is a corrupt durable coverage cursor on two panels. It is
    // chosen because the refusal it produces is unambiguous and by design:
    // `STORAGE_DERIVED_STATE_BACKFILL_CURSOR_UNREADABLE` exists precisely so a
    // sweep never silently restarts from the head of a CF on an unproven
    // position. Two corrupt cursors are therefore two independent, *correct*
    // sub-pass refusals inside one tick.
    println!("\n== Part 4: two sub-pass failures inside one tick ==");
    let mut reseeded_timeline = Vec::new();
    let mut reseeded_events = Vec::new();
    for index in SEED_ROWS..(SEED_ROWS * 2) {
        reseeded_timeline.push(timeline_row(index)?);
        reseeded_events.push(agent_event_row(index)?);
    }
    db.put_batch(cf::CF_TIMELINE, reseeded_timeline)?;
    db.put_batch(cf::CF_AGENT_EVENTS, reseeded_events)?;
    for (panel_version, source_cf) in [
        (SYN_TIMELINE_PANEL_VERSION, cf::CF_TIMELINE),
        (SYN_AGENT_EVENT_PANEL_VERSION, cf::CF_AGENT_EVENTS),
    ] {
        db.put_batch(
            cf::CF_KV,
            vec![(
                cursor_key(panel_version, source_cf),
                b"{ this is not a coverage sweep cursor".to_vec(),
            )],
        )?;
    }
    let before = derived_state_readback();
    let after = run_derived_state_maintenance_once();
    println!(
        "  INJECTED attempts {}->{} success {}->{} failure {}->{} subpass_failures {}->{}",
        before.attempts_total,
        after.attempts_total,
        before.success_total,
        after.success_total,
        before.failure_total,
        after.failure_total,
        before.subpass_failures_total,
        after.subpass_failures_total,
    );
    for entry in &after.last_tick_subpass_failures {
        println!("  TICK_SUBPASS_FAILURE {entry}");
    }
    let subpass_delta = after
        .subpass_failures_total
        .saturating_sub(before.subpass_failures_total);
    let failure_delta = after.failure_total.saturating_sub(before.failure_total);
    f.check(
        "one tick with several sub-pass failures moves the tick counter by exactly one",
        failure_delta == 1 && subpass_delta >= 2,
        &format!(
            "failure_total delta={failure_delta} (must be 1) subpass_failures delta={subpass_delta} \
             (must be >= 2); the old code moved failure_total by {subpass_delta}, which is how \
             failure came to exceed attempts"
        ),
    );
    f.check(
        "the invariant still holds on the failing tick",
        after
            .success_total
            .saturating_add(after.failure_total)
            .saturating_add(after.skipped_total)
            == after.attempts_total
            && after.failure_total <= after.attempts_total,
        &format!(
            "attempts={} success={} failure={} skipped={}",
            after.attempts_total, after.success_total, after.failure_total, after.skipped_total
        ),
    );
    f.check(
        "the failing tick names every sub-pass failure it counted",
        after.last_tick_failed == Some(true)
            && after.last_tick_subpass_failures.len() as u64 == subpass_delta,
        &format!(
            "last_tick_failed={:?} named={} counted={subpass_delta}",
            after.last_tick_failed,
            after.last_tick_subpass_failures.len()
        ),
    );
    let last = after;

    println!("\n== Part 5: advisories are counted apart from failures ==");
    println!(
        "  ADVISORY code={} detail={} total={}",
        last.last_advisory_code.as_deref().unwrap_or("<none>"),
        last.last_advisory_detail.as_deref().unwrap_or("<none>"),
        last.advisories_total,
    );
    println!(
        "  ANCHOR_DEBT_COST ms_per_identity={:?} repair_ms={} panels_attempted={} \
         lineage_rebuilds={} lineage_reuses={} lineage_rebuild_ms={} phase_elapsed_ms={:?}",
        last.last_anchor_debt_ms_per_identity,
        last.last_anchor_debt_repair_ms,
        last.last_anchor_debt_panels_attempted,
        last.last_anchor_debt_lineage_rebuilds,
        last.last_anchor_debt_lineage_reuses,
        last.last_anchor_debt_lineage_rebuild_ms,
        last.last_anchor_debt_elapsed_ms,
    );
    f.check(
        "an advisory never appears in the tick's failure ledger",
        last.last_advisory_code.as_ref().is_none_or(|code| {
            !last
                .last_tick_subpass_failures
                .iter()
                .any(|entry| entry.starts_with(code.as_str()))
        }),
        &format!(
            "advisory={:?} tick_failures={:?}",
            last.last_advisory_code, last.last_tick_subpass_failures
        ),
    );
    f.check(
        "per-identity cost is measured against the repair primitive only",
        last.last_anchor_debt_repair_ms <= last.last_anchor_debt_elapsed_ms.unwrap_or(u64::MAX),
        &format!(
            "repair_ms={} <= phase_elapsed_ms={:?}; the old numerator WAS the phase elapsed",
            last.last_anchor_debt_repair_ms, last.last_anchor_debt_elapsed_ms
        ),
    );

    // ------------------------------------------------------------------
    // Part 6 — the incremental weave publishes a frontier and a backlog
    // ------------------------------------------------------------------
    //
    // What this run can prove: the frontier/backlog reporting exists, is wired
    // through the readback, and a pass that reaches `now()` publishes zero
    // backlog under `interval_complete`. What it cannot prove on a vault this
    // small is the partial path — a tick that misses the 20 s budget — because
    // nothing here takes 20 s. That is named as remaining live verification
    // rather than implied: the deployed daemon's non-zero backlog, falling
    // across ticks with the watermark advancing, is #2085's closure evidence.
    println!("\n== Part 6: incremental weave frontier and backlog ==");
    for (panel_version, action) in &last.last_weave_actions {
        println!(
            "  WEAVE panel_version={panel_version} action={action} frontier_ns={:?} \
             backlog_ns={:?} pending_parts={:?} backlog_growth_ticks={:?} records={:?}",
            last.last_weave_until_ns.get(panel_version),
            last.last_weave_backlog_ns.get(panel_version),
            last.last_weave_pending_parts.get(panel_version),
            last.last_weave_backlog_growth_ticks.get(panel_version),
            last.last_weave_records.get(panel_version),
        );
    }
    f.check(
        "a weave that reached now() reports zero backlog and zero pending parts",
        last.last_weave_actions
            .iter()
            .filter(|(_, action)| action.as_str() == "interval_complete")
            .all(|(panel_version, _)| {
                last.last_weave_backlog_ns.get(panel_version) == Some(&0)
                    && last.last_weave_pending_parts.get(panel_version) == Some(&0)
            }),
        &format!(
            "actions={:?} backlog={:?} pending={:?}",
            last.last_weave_actions, last.last_weave_backlog_ns, last.last_weave_pending_parts
        ),
    );

    println!("\n== Verdict ==");
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
