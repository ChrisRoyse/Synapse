//! Manual FSV for #1980: a panel-version bump strands every anchor the corpus
//! had, and the backfill now carries them across.
//!
//! ## The defect
//!
//! `cx_id = hash(input_bytes, panel_version, vault_salt)` and an anchor is keyed
//! by `(cx_id, kind)`. So bumping a panel version re-keys every record and
//! orphans every anchor written against the previous generation: the re-measured
//! corpus is born ungrounded, while coverage reads 1.0 and nothing complains.
//!
//! On this vault `syn-episode-v1` bumped 1_904_002 -> 1_964_001 and the `Base`
//! census reads **171 of 171 grounded** at the superseded generation and **0 of
//! the same 171** at the active one, off the identical 171 `CF_EPISODES` source
//! rows. Grounding is the axis every bits / sufficiency / kernel result is
//! defined against, so that panel silently became unmeasurable.
//!
//! ## Why the episode panel is the decisive arm
//!
//! **The backfill has no derivation path for episode anchors.** Only
//! `CF_AGENT_TRANSCRIPTS` derives an outcome during a backfill
//! (`put_agent_transcript_outcome_anchor_row`); `CF_EPISODES` has none. So an
//! episode anchor appearing on the active generation after a backfill can only
//! have been *carried*, never re-derived. The harness asserts
//! `outcome_anchored_rows == 0` on the same run to nail that down.
//!
//! ## The control that makes this a test rather than an assertion
//!
//! `CF_TIMELINE` has two superseded generations and **zero** grounded records on
//! any of them. Its arm must report `generations_read = 2` and
//! `anchors_carried_forward = 0`. Without it, a carry that never ran would look
//! identical to a carry that correctly found nothing, and every "0" below would
//! be vacuous.
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | the stranding is real and named | `Base` census per generation, before any write |
//! | 2 | one named row is physically ungrounded | `Anchors` CF scan for that row's active `cx_id` |
//! | 3 | the carry runs and grounds the active generation | `Base` census after a real backfill |
//! | 4 | the anchor is physically on disk after a REOPEN | `Anchors` CF scan on a fresh read-only handle |
//! | 5 | control: a carry with nothing to carry still runs | `CF_TIMELINE`, generations_read > 0, carried 0 |
//! | 6 | idempotent: a second pass carries nothing new | second backfill's counters + unchanged census |
//! | 7 | edge cases fail closed | the returned `StorageError`, driven directly |
//!
//! ```text
//! cargo run -p synapse-storage --example anchor_carry_forward_fsv -- <vault-parent-dir>
//! ```
//!
//! Needs a **copy**: this writes.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use synapse_storage::Db;
use synapse_storage::cf;
use synapse_storage::panel_coverage::PanelCoverageReport;

const SCHEMA_VERSION: u32 = 1;
const EPISODE_PANEL: &str = "syn-episode-v1";
const TIMELINE_PANEL: &str = "syn-timeline-v1";
/// Whole-panel pages. 171 episode rows is the entire population, so the claim
/// "every stranded anchor is recovered" is proven over all of it rather than a
/// sample -- and 171 is small enough that the whole run is seconds.
const PAGE_ROWS: usize = 256;

struct Failures(Vec<String>);

impl Failures {
    fn check(
        &mut self,
        label: &str,
        observed: impl std::fmt::Debug,
        expected: impl std::fmt::Debug,
    ) {
        let observed = format!("{observed:?}");
        let expected = format!("{expected:?}");
        let verdict = if observed == expected { "PASS" } else { "FAIL" };
        println!("  [{verdict}] {label}: observed={observed} expected={expected}");
        if verdict == "FAIL" {
            self.0
                .push(format!("{label}: observed={observed} expected={expected}"));
        }
    }
}

/// The three numbers this issue turns on, for one panel, read off the physical
/// `Base` census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanelGrounding {
    active_version: u32,
    active_records: usize,
    active_grounded: usize,
    superseded_grounded: usize,
    stranded: usize,
}

fn grounding_of(report: &PanelCoverageReport, panel_name: &str) -> PanelGrounding {
    let panel = report
        .panels
        .iter()
        .find(|panel| panel.panel_name == panel_name)
        .unwrap_or_else(|| panic!("{panel_name} absent from the panel coverage report"));
    PanelGrounding {
        active_version: panel.panel_version,
        active_records: panel.active_version_records,
        active_grounded: panel.grounded_records,
        superseded_grounded: panel.superseded_grounded_records,
        stranded: panel.anchors_stranded_on_superseded,
    }
}

fn print_grounding(when: &str, panel_name: &str, g: &PanelGrounding) {
    println!(
        "  {when:<7} {panel_name:<24} v{} records={} grounded={} superseded_grounded={} STRANDED={}",
        g.active_version, g.active_records, g.active_grounded, g.superseded_grounded, g.stranded
    );
}

/// Drives a whole CF through the backfill, returning the summed counters.
fn drive_backfill(db: &Db, source_cf: &str) -> Result<BTreeMap<String, u64>, Box<dyn Error>> {
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    let mut cursor: Option<Vec<u8>> = None;
    let mut pages = 0_u64;
    loop {
        let report =
            db.backfill_temporal_metadata(source_cf, None, cursor.as_deref(), PAGE_ROWS)?;
        pages += 1;
        *totals.entry("examined_rows".into()).or_default() += report.examined_rows;
        *totals.entry("inserted_rows".into()).or_default() += report.inserted_rows;
        *totals.entry("backfilled_rows".into()).or_default() += report.backfilled_rows;
        *totals.entry("already_current_rows".into()).or_default() += report.already_current_rows;
        *totals.entry("outcome_anchored_rows".into()).or_default() += report.outcome_anchored_rows;
        *totals.entry("anchors_carried_forward".into()).or_default() +=
            report.anchors_carried_forward;
        *totals.entry("rows_anchor_carried".into()).or_default() += report.rows_anchor_carried;
        *totals
            .entry("anchor_carry_source_generations_read".into())
            .or_default() += report.anchor_carry_source_generations_read;
        if !report.more {
            break;
        }
        cursor = report.resume_after_physical;
        if cursor.is_none() {
            break;
        }
    }
    totals.insert("pages".into(), pages);
    Ok(totals)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let Some(parent) = std::env::args().nth(1).map(PathBuf::from) else {
        return Err("usage: anchor_carry_forward_fsv <vault-parent-dir>".into());
    };
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    // ---------------------------------------------------------------
    println!("== Part 1: the stranding, read off the physical Base census BEFORE any write ==");
    let before = db.measure_panel_coverage()?;
    let ep_before = grounding_of(&before, EPISODE_PANEL);
    let tl_before = grounding_of(&before, TIMELINE_PANEL);
    print_grounding("BEFORE", EPISODE_PANEL, &ep_before);
    print_grounding("BEFORE", TIMELINE_PANEL, &tl_before);
    println!(
        "  anchors_stranded_panels = {:?}",
        before.anchors_stranded_panels
    );

    // The whole issue in three numbers. If any of these is not what #1980
    // recorded, the vault moved and every later arm is measuring something else.
    f.check(
        "episode panel is fully re-measured at the active generation",
        ep_before.active_records > 0 && ep_before.active_records == ep_before.superseded_grounded,
        true,
    );
    f.check(
        "...and yet grounds NOTHING there",
        ep_before.active_grounded,
        0_usize,
    );
    f.check(
        "...while the superseded generation holds every one of its anchors",
        ep_before.superseded_grounded,
        ep_before.active_records,
    );
    f.check(
        "the stranded count names it",
        ep_before.stranded,
        ep_before.active_records,
    );
    f.check(
        "and the report lists the panel as stranded",
        before
            .anchors_stranded_panels
            .iter()
            .any(|name| name.contains(EPISODE_PANEL)),
        true,
    );

    // ---------------------------------------------------------------
    println!("\n== Part 2: one named source row, physically ungrounded at the active cx_id ==");
    let episode_rows = db.scan_cf_physical_page(cf::CF_EPISODES, None, 1)?;
    let (probe_key, probe_value) = episode_rows
        .rows
        .first()
        .cloned()
        .ok_or("CF_EPISODES holds no rows on this vault; nothing to prove")?;
    let scan_before = db.calyx_anchor_scan_for_source(cf::CF_EPISODES, &probe_key, &probe_value)?;
    println!(
        "  probe row key_hex={} -> panel {} v{} cx_id={} anchors={}",
        scan_before.source_key_hex,
        scan_before.panel_name,
        scan_before.panel_version,
        scan_before.cx_id,
        scan_before.anchors.len()
    );
    f.check(
        "the probe row's ACTIVE generation carries no anchor",
        scan_before.anchors.len(),
        0_usize,
    );
    let probe_cx_before = scan_before.cx_id.clone();

    // ---------------------------------------------------------------
    println!("\n== Part 3: drive the real backfill over the whole episode panel ==");
    let ep_totals = drive_backfill(&db, cf::CF_EPISODES)?;
    for (k, v) in &ep_totals {
        println!("  {k:<40} {v}");
    }
    f.check(
        "every episode row was examined",
        ep_totals.get("examined_rows").copied().unwrap_or(0),
        ep_before.active_records as u64,
    );
    f.check(
        "an anchor was carried for every one of them",
        ep_totals
            .get("anchors_carried_forward")
            .copied()
            .unwrap_or(0),
        ep_before.active_records as u64,
    );
    f.check(
        "...on that many rows",
        ep_totals.get("rows_anchor_carried").copied().unwrap_or(0),
        ep_before.active_records as u64,
    );
    // The discriminator: no derivation path exists for episode anchors, so a
    // non-zero here would mean the carry was not what grounded the panel.
    f.check(
        "and NOTHING was derived -- the episode panel has no derivation path",
        ep_totals
            .get("outcome_anchored_rows")
            .copied()
            .unwrap_or(999),
        0_u64,
    );

    println!("\n  -- Base census AFTER the carry --");
    let after = db.measure_panel_coverage()?;
    let ep_after = grounding_of(&after, EPISODE_PANEL);
    print_grounding("AFTER", EPISODE_PANEL, &ep_after);
    println!(
        "  anchors_stranded_panels = {:?}",
        after.anchors_stranded_panels
    );
    f.check(
        "the active generation is now fully grounded",
        ep_after.active_grounded,
        ep_before.active_records,
    );
    f.check("nothing is stranded any more", ep_after.stranded, 0_usize);
    f.check(
        "the panel is no longer reported as stranded",
        after
            .anchors_stranded_panels
            .iter()
            .any(|name| name.contains(EPISODE_PANEL)),
        false,
    );
    f.check(
        "the record population did not change -- this grounded, it did not duplicate",
        ep_after.active_records,
        ep_before.active_records,
    );

    // ---------------------------------------------------------------
    println!("\n== Part 4: the bytes on disk, after CLOSING and reopening read-only ==");
    drop(db);
    let reopened = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let scan_after =
        reopened.calyx_anchor_scan_for_source(cf::CF_EPISODES, &probe_key, &probe_value)?;
    println!(
        "  probe row -> cx_id={} anchors={}",
        scan_after.cx_id,
        scan_after.anchors.len()
    );
    for row in &scan_after.anchors {
        println!(
            "    kind={} value={:?} source={} observed_at_ms={} confidence={}",
            row.kind, row.value.text_value, row.source, row.observed_at_ms, row.confidence
        );
    }
    f.check(
        "the same probe row resolves to the same active cx_id after reopen",
        scan_after.cx_id,
        probe_cx_before,
    );
    f.check(
        "and now carries exactly one anchor, read from the Anchors CF",
        scan_after.anchors.len(),
        1_usize,
    );
    let carried = scan_after
        .anchors
        .first()
        .ok_or("no anchor survived the reopen")?;
    f.check(
        "the carried anchor kept the ORIGINAL writer's source, not a re-derivation's",
        carried.source.as_str(),
        "synapse-episode-segment",
    );
    // `calyx_anchor_scan_for_source` renders `AnchorKind::Label(name)` with its
    // `label:` discriminant, which is NOT the bare name `anchor_kind_label`
    // returns. Asserting the rendered form on purpose: this arm is reading the
    // physical Anchors CF through the operator-facing surface, so it must expect
    // what that surface actually emits.
    f.check(
        "the carried anchor is the episode segmentation outcome",
        carried.kind.as_str(),
        "label:synapse:episode_segmentation_outcome",
    );
    f.check(
        "the carried anchor grounds (finite, positive confidence)",
        carried.confidence.is_finite() && carried.confidence > 0.0,
        true,
    );

    // ---------------------------------------------------------------
    println!("\n== Part 5: CONTROL -- a panel with superseded generations and no anchors ==");
    println!("  syn-timeline-v1 has 2 superseded generations, 0 grounded on any of them.");
    println!("  The carry must RUN over it and carry nothing. If generations_read were 0,");
    println!("  every zero in this harness would be the carry not running at all.");
    let tl_totals = drive_backfill(&reopened, cf::CF_TIMELINE)?;
    for (k, v) in &tl_totals {
        println!("  {k:<40} {v}");
    }
    f.check(
        "the carry probed the superseded generations",
        tl_totals
            .get("anchor_carry_source_generations_read")
            .copied()
            .unwrap_or(0)
            > 0,
        true,
    );
    f.check(
        "and correctly carried nothing, because there was nothing to carry",
        tl_totals
            .get("anchors_carried_forward")
            .copied()
            .unwrap_or(999),
        0_u64,
    );
    let tl_after = grounding_of(&reopened.measure_panel_coverage()?, TIMELINE_PANEL);
    f.check(
        "the timeline panel's grounding is unchanged",
        tl_after.active_grounded,
        tl_before.active_grounded,
    );

    // ---------------------------------------------------------------
    println!("\n== Part 6: idempotence -- a second pass must carry NOTHING new ==");
    let ep_second = drive_backfill(&reopened, cf::CF_EPISODES)?;
    for (k, v) in &ep_second {
        println!("  {k:<40} {v}");
    }
    f.check(
        "a second pass carries no anchor, because the active generation already has them",
        ep_second
            .get("anchors_carried_forward")
            .copied()
            .unwrap_or(999),
        0_u64,
    );
    f.check(
        "but it still probed the superseded generations",
        ep_second
            .get("anchor_carry_source_generations_read")
            .copied()
            .unwrap_or(0)
            > 0,
        true,
    );
    let ep_twice = grounding_of(&reopened.measure_panel_coverage()?, EPISODE_PANEL);
    print_grounding("AFTER2", EPISODE_PANEL, &ep_twice);
    f.check(
        "and grounding is unchanged -- no double-count, no duplicate anchor",
        ep_twice.active_grounded,
        ep_after.active_grounded,
    );
    let scan_twice =
        reopened.calyx_anchor_scan_for_source(cf::CF_EPISODES, &probe_key, &probe_value)?;
    f.check(
        "the probe row still holds exactly one anchor",
        scan_twice.anchors.len(),
        1_usize,
    );

    // ---------------------------------------------------------------
    println!("\n== Part 7: edge cases, each driven and printed ==");
    let unknown =
        synapse_storage::constellations::superseded_panel_versions_for_source_cf("CF_NOT_A_CF");
    println!("  unknown source CF -> {unknown:?}");
    f.check(
        "an unknown source CF is refused, not treated as having no history",
        unknown.is_err(),
        true,
    );

    let episodes_history =
        synapse_storage::constellations::superseded_panel_versions_for_source_cf(cf::CF_EPISODES)?;
    println!("  CF_EPISODES declared generation history -> {episodes_history:?}");
    f.check(
        "the episode panel declares its superseded generations",
        episodes_history.is_empty(),
        false,
    );

    let backfill_unknown = reopened.backfill_temporal_metadata("CF_NOT_A_CF", None, None, 1);
    println!(
        "  backfill over an unknown CF -> {:?}",
        backfill_unknown.as_ref().err().map(ToString::to_string)
    );
    f.check(
        "the backfill itself refuses an unknown CF",
        backfill_unknown.is_err(),
        true,
    );

    // ---------------------------------------------------------------
    println!("\n================================================");
    if f.0.is_empty() {
        println!("PASS: every stranded anchor was carried to the active generation and read back");
        println!(
            "      from the Anchors CF after a reopen; the control proves the carry ran on a\n      \
             panel with nothing to carry, and a second pass changed nothing."
        );
        Ok(())
    } else {
        println!("FAIL ({} check(s)):", f.0.len());
        for failure in &f.0 {
            println!("  - {failure}");
        }
        Err("anchor carry-forward FSV failed".into())
    }
}
