//! Full-state verification for #1968: prove the paged `Base` walk measures the
//! **same census** as the whole-CF materialization it replaced, and that its
//! row-table read-guard hold is bounded where the old one was not.
//!
//! ## The defect
//!
//! `scan_cf_latest(cf)` holds the CF's row-table shard for its entire body: the
//! router's full-CF materialization, the tombstone merge, the table overlay and
//! the barrier check all run inside one guard whose duration grows with the
//! column family. The live daemon's own census (pid 504, 22,718 s of uptime)
//! read:
//!
//! ```text
//! site              holds   mean_us     max_us  over_budget
//! scan_cf_latest      380   280,946  2,651,363    380 / 380
//! overlay_table_rows  125   208,360  2,004,190     75 / 125
//! ```
//!
//! Over budget on *every* call, against a budget of
//! `ROW_READ_GUARD_WARN_US = 25,000`. `Base` is the family every constellation
//! write lands in, so a publishing MCP call could queue behind a maintenance
//! scan for up to 2.65 s.
//!
//! Five maintenance folds issued it, and **none of them needed the
//! materialization**: every one was a streaming aggregate written as a
//! collect-then-iterate.
//!
//! ## What this proves, in order
//!
//! * **A — equivalence.** The shipped `panel_census()` (now paged) against a
//!   reference census folded from an unpaged `scan_cf_latest`, over the same
//!   frozen vault at the same sequence. Every per-generation count must agree
//!   exactly. A faster scan that measures something different is not a fix.
//! * **B — the hold.** Both arms bracketed by `row_guard_census()`, which
//!   counts every hold and starts timing on *acquisition*, so a run that waited
//!   behind a writer is not charged for the wait. The number that matters is
//!   the **worst single hold**, because that is the stall one committer sees —
//!   not the total, which the paged arm necessarily spreads over many holds.
//! * **C — edge cases**, each printing system state before and after.
//! * **D — bits on disk.** A fresh read-only reopen after close, counting the
//!   physical `Base` rows, against what the walk reported visiting.
//!
//! ## Why a frozen copy is required
//!
//! Releasing the guard between pages means a walk sees a *moving* window where
//! `scan_cf_latest` saw one instant. Comparing paged against unpaged on a live
//! vault compares two different windows and would disagree for a reason that is
//! not a defect. On a frozen copy nothing commits, so `walk.atomic()` holds and
//! the equivalence check is well-posed — and this run **asserts** `atomic()`
//! rather than assuming it, so a contaminated copy fails loudly instead of
//! producing a meaningless comparison.
//!
//! ```text
//! robocopy <live>\db-daemon <copy>\db-daemon /E
//! copy <live>\machine-salt.bin <copy>\machine-salt.bin
//! cargo run --release -p synapse-calyx --example base_walk_hold_fsv -- <copy>
//! ```

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::{ColumnFamily, SlotFamilyKind};
use calyx_aster::mvcc::ROW_READ_GUARD_WARN_US;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::SlotId;
use synapse_calyx::{
    SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, SynapseCalyxConfig, SynapseCalyxReadOnlyVault,
    SynapseCalyxTuningConfig, SynapseCalyxVault, SynapseCalyxWalkStep, anchor_kind_label,
};

/// One generation's counts, folded identically by both arms.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Generation {
    records: usize,
    grounded_records: usize,
    anchor_kind_records: BTreeMap<String, usize>,
    earliest_created_at_ms: Option<u64>,
    latest_created_at_ms: Option<u64>,
    unattributed_records: usize,
}

/// The full census both arms produce, as a value that can be compared with `==`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Census {
    by_version: BTreeMap<u32, Generation>,
    rows: usize,
    decode_failures: usize,
}

/// Folds one decoded `Base` row into the census.
///
/// **Both arms call this identical function.** That is deliberate: the claim
/// under test is that the *scan mechanism* is equivalent, so any difference in
/// the fold itself would confound the comparison.
fn fold_row(census: &mut Census, value: &[u8]) {
    let Ok(base) = decode_constellation_base(value) else {
        census.decode_failures += 1;
        return;
    };
    let entry = census.by_version.entry(base.panel_version).or_default();
    entry.records += 1;
    if !base.metadata.contains_key("synapse_source_cf")
        || !base.metadata.contains_key("synapse_source_key_hex")
    {
        entry.unattributed_records += 1;
    }
    entry.earliest_created_at_ms = Some(
        entry
            .earliest_created_at_ms
            .map_or(base.created_at, |seen| seen.min(base.created_at)),
    );
    entry.latest_created_at_ms = Some(
        entry
            .latest_created_at_ms
            .map_or(base.created_at, |seen| seen.max(base.created_at)),
    );
    let kinds: std::collections::BTreeSet<String> = base
        .anchors
        .iter()
        .filter(|anchor| anchor.confidence > 0.0)
        .map(|anchor| anchor_kind_label(&anchor.kind))
        .collect();
    if !kinds.is_empty() {
        entry.grounded_records += 1;
    }
    for kind in kinds {
        *entry.anchor_kind_records.entry(kind).or_default() += 1;
    }
}

/// One site's `(holds, total_held_us, max_held_us)` at this instant.
fn site(vault: &SynapseCalyxVault, name: &str) -> Result<(u64, u64, u64), Box<dyn Error>> {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == name)
        .map(|entry| (entry.holds, entry.total_held_us, entry.max_held_us))
        .ok_or_else(|| format!("the census has no site named '{name}'").into())
}

fn check(label: &str, held: bool, detail: &str, failures: &mut Vec<String>) {
    println!("  [{}] {label}", if held { "PASS" } else { "FAIL" });
    if !detail.is_empty() {
        println!("         {detail}");
    }
    if !held {
        failures.push(label.to_owned());
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: base_walk_hold_fsv <vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxVault::open(config.clone())?;
    let mut failures: Vec<String> = Vec::new();

    println!("base_walk_hold_fsv  (#1968)");
    println!("frozen vault copy = {}", vault_dir.display());
    println!("latest_seq        = {}", vault.latest_seq());
    println!("guard budget      = {ROW_READ_GUARD_WARN_US} us");
    println!("walk page size    = {SYNAPSE_CALYX_CF_WALK_PAGE_ROWS} rows");

    // -----------------------------------------------------------------------
    // A. Equivalence: the paged fold against the unpaged fold it replaced.
    // -----------------------------------------------------------------------
    println!("\n=== A. equivalence — paged fold vs unpaged fold, same vault, same fold body");

    let (scan_holds_0, scan_us_0, _) = site(&vault, "scan_cf_latest")?;
    let mut unpaged = Census::default();
    for (_key, value) in vault.scan_cf_latest(ColumnFamily::Base)? {
        fold_row(&mut unpaged, &value);
    }
    let (scan_holds_1, scan_us_1, _) = site(&vault, "scan_cf_latest")?;
    let unpaged_hold_us = scan_us_1 - scan_us_0;
    unpaged.rows = unpaged
        .by_version
        .values()
        .map(|g| g.records)
        .sum::<usize>()
        + unpaged.decode_failures;

    let (page_holds_0, page_us_0, _) = site(&vault, "scan_cf_range_page_latest")?;
    let mut paged = Census::default();
    let walk = vault.walk_cf_latest(
        ColumnFamily::Base,
        SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
        |_key, value| {
            fold_row(&mut paged, value);
            Ok(SynapseCalyxWalkStep::Continue)
        },
    )?;
    let (page_holds_1, page_us_1, _) = site(&vault, "scan_cf_range_page_latest")?;
    paged.rows = walk.rows_visited;

    println!(
        "  unpaged: 1 hold,  {unpaged_hold_us} us total,   {} rows folded",
        unpaged.rows
    );
    println!(
        "  paged  : {} holds, {} us total, {} rows folded over {} pages (seq {} -> {})",
        page_holds_1 - page_holds_0,
        page_us_1 - page_us_0,
        paged.rows,
        walk.pages,
        walk.snapshot_seq_first,
        walk.snapshot_seq_last
    );

    check(
        "the frozen copy really is quiescent (walk.atomic())",
        walk.atomic(),
        &format!(
            "first_seq={} last_seq={} pages={} — a moving window would make the census \
             comparison below meaningless rather than merely wrong",
            walk.snapshot_seq_first, walk.snapshot_seq_last, walk.pages
        ),
        &mut failures,
    );
    check(
        "exactly one scan_cf_latest hold for the unpaged arm",
        scan_holds_1 - scan_holds_0 == 1,
        &format!("delta_holds={}", scan_holds_1 - scan_holds_0),
        &mut failures,
    );
    check(
        "paged row count == unpaged row count",
        paged.rows == unpaged.rows,
        &format!("paged={} unpaged={}", paged.rows, unpaged.rows),
        &mut failures,
    );
    check(
        "paged census == unpaged census, generation for generation",
        paged == unpaged,
        &format!(
            "{} generation(s) compared; decode_failures paged={} unpaged={}",
            paged.by_version.len(),
            paged.decode_failures,
            unpaged.decode_failures
        ),
        &mut failures,
    );

    println!(
        "\n  {:>12} {:>9} {:>9} {:>9} {:>16} {:>16}",
        "generation", "records", "grounded", "unattr", "earliest_ms", "latest_ms"
    );
    for (version, generation) in &paged.by_version {
        let agrees = unpaged.by_version.get(version) == Some(generation);
        println!(
            "  {:>12} {:>9} {:>9} {:>9} {:>16} {:>16}  {}",
            version,
            generation.records,
            generation.grounded_records,
            generation.unattributed_records,
            generation.earliest_created_at_ms.unwrap_or(0),
            generation.latest_created_at_ms.unwrap_or(0),
            if agrees { "==" } else { "DIFFERS" }
        );
    }

    // The shipped function, not a re-implementation: prove `panel_census()`
    // itself lands on the same numbers now that it pages.
    let shipped = vault.panel_census()?;
    let shipped_rows: usize = shipped.entries.iter().map(|entry| entry.records).sum();
    check(
        "the shipped panel_census() agrees with the unpaged reference fold",
        shipped_rows + shipped.decode_failures == unpaged.rows
            && shipped.entries.len() == unpaged.by_version.len()
            && shipped.entries.iter().all(|entry| {
                unpaged
                    .by_version
                    .get(&entry.panel_version)
                    .is_some_and(|reference| {
                        reference.records == entry.records
                            && reference.grounded_records == entry.grounded_records
                    })
            }),
        &format!(
            "shipped base_cf_rows={} entries={} decode_failures={} walk_pages={} atomic={}",
            shipped.base_cf_rows,
            shipped.entries.len(),
            shipped.decode_failures,
            shipped.walk.pages,
            shipped.walk.atomic()
        ),
        &mut failures,
    );

    // -----------------------------------------------------------------------
    // B. The hold. The worst SINGLE hold is what stalls one committer.
    // -----------------------------------------------------------------------
    println!("\n=== B. the guard hold — worst single hold is the stall a committer sees");

    // Re-measure per-page maxima cleanly: the running census `max_held_us` is a
    // high-water mark over the process, so it is read as a delta of maxima only
    // after a fresh walk, and the per-page worst is tracked by bracketing.
    // The page size is a real tradeoff and is chosen by measurement, not taste:
    // a bigger page holds the guard longer, a smaller page pays the per-page
    // merge cost more often. Sweeping it is what turns the constant into a
    // decision with evidence behind it.
    println!(
        "
  {:>10} {:>7} {:>14} {:>13} {:>13} {:>10}",
        "page_rows", "pages", "worst_hold_us", "mean_hold_us", "total_us", "over_budget"
    );
    let mut worst_page_us = 0u64;
    let mut pages_over_budget = 0usize;
    let mut page_count = 0u64;
    let mut page_total_us = 0u64;
    for candidate in [256usize, 512, 1_024, 2_048, 4_096, 8_192] {
        let mut worst = 0u64;
        let mut over = 0usize;
        let mut pages = 0u64;
        let mut total = 0u64;
        let range = calyx_aster::cf::KeyRange::all();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let (h0, u0, _) = site(&vault, "scan_cf_range_page_latest")?;
            let page = vault.scan_cf_range_page_latest(
                ColumnFamily::Base,
                &range,
                cursor.as_deref(),
                candidate,
            )?;
            let (h1, u1, _) = site(&vault, "scan_cf_range_page_latest")?;
            if h1 - h0 == 1 {
                let held = u1 - u0;
                worst = worst.max(held);
                total += held;
                pages += 1;
                if held >= ROW_READ_GUARD_WARN_US {
                    over += 1;
                }
            }
            if !page.more {
                break;
            }
            cursor = page.resume_after;
        }
        println!(
            "  {candidate:>10} {pages:>7} {worst:>14} {:>13.1} {total:>13} {over:>10}{}",
            total as f64 / pages.max(1) as f64,
            if candidate == SYNAPSE_CALYX_CF_WALK_PAGE_ROWS {
                "  <-- shipped"
            } else {
                ""
            }
        );
        if candidate == SYNAPSE_CALYX_CF_WALK_PAGE_ROWS {
            worst_page_us = worst;
            pages_over_budget = over;
            page_count = pages;
            page_total_us = total;
        }
    }

    println!(
        "
  unpaged  worst single hold : {unpaged_hold_us:>10} us   ({} over budget)",
        u8::from(unpaged_hold_us >= ROW_READ_GUARD_WARN_US)
    );
    println!(
        "  paged    worst single hold : {worst_page_us:>10} us   ({pages_over_budget} page(s) over budget)"
    );
    println!(
        "  paged    mean hold         : {:>10.1} us over {page_count} pages ({page_total_us} us total)",
        page_total_us as f64 / page_count.max(1) as f64
    );
    if worst_page_us > 0 {
        println!(
            "  worst-hold reduction       : {:.1}x",
            unpaged_hold_us as f64 / worst_page_us as f64
        );
    }

    check(
        "the unpaged arm is over the guard budget (the defect reproduces here)",
        unpaged_hold_us >= ROW_READ_GUARD_WARN_US,
        &format!("{unpaged_hold_us} us >= {ROW_READ_GUARD_WARN_US} us"),
        &mut failures,
    );
    check(
        "every paged hold is under the guard budget",
        pages_over_budget == 0,
        &format!(
            "{pages_over_budget} of {page_count} pages at or over {ROW_READ_GUARD_WARN_US} us;              worst page {worst_page_us} us"
        ),
        &mut failures,
    );
    check(
        "the shipped page size keeps the worst hold under half the guard budget",
        worst_page_us > 0 && worst_page_us * 2 <= ROW_READ_GUARD_WARN_US,
        &format!(
            "worst_page={worst_page_us} us against half-budget {} us — a page size whose worst              hold merely squeaks under the budget has no headroom for a busier machine",
            ROW_READ_GUARD_WARN_US / 2
        ),
        &mut failures,
    );

    // -----------------------------------------------------------------------
    // C. Edge cases, state printed before and after.
    // -----------------------------------------------------------------------
    println!("\n=== C. edge cases");

    // C1: a zero page size cannot make forward progress and is refused.
    println!("\n  C1: page_rows = 0");
    println!("      before: no walk has been attempted with a zero page size");
    let zero = vault.walk_cf_latest(ColumnFamily::Base, 0, |_k, _v| {
        Ok(SynapseCalyxWalkStep::Continue)
    });
    match &zero {
        Ok(walk) => println!("      after : Ok({walk:?})  <-- must not happen"),
        Err(error) => println!("      after : {} :: {}", error.code, error.message),
    }
    check(
        "a zero page size is refused rather than spinning",
        zero.as_ref()
            .is_err_and(|error| error.code == "SYNAPSE_CALYX_CF_WALK_PAGE_ROWS_ZERO"),
        "",
        &mut failures,
    );

    // C2: three boundaries that "a big CF" does not cover.
    //
    //  * a **non-pageable** CF. Candidate-bounded paging needs a retained,
    //    validated SST lookup index, and the open policy retains it only for
    //    `Kv`, `Base` and the slot CFs. A walk over anything else must FAIL
    //    CLOSED naming the missing index — never return a partial or empty
    //    result that reads like "this CF has no rows".
    //  * a pageable CF with **fewer rows than one page** — the single-page
    //    path, where `more` is false on the first read.
    //  * a pageable CF with **no rows** — which must still read exactly one
    //    page, because zero pages is indistinguishable from "the walk never
    //    ran".
    println!(
        "
  C2a: a column family whose SST lookup index is not retained"
    );
    let assay_rows = vault.count_cf_latest(ColumnFamily::Assay)?;
    println!("      before: Assay holds {assay_rows} row(s) and is not a pageable surface");
    let unpageable = vault.walk_cf_latest(
        ColumnFamily::Assay,
        SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
        |_k, _v| Ok(SynapseCalyxWalkStep::Continue),
    );
    match &unpageable {
        Ok(walk) => println!(
            "      after : Ok(pages={}, rows={})  <-- must not happen",
            walk.pages, walk.rows_visited
        ),
        Err(error) => println!("      after : {} :: {}", error.code, error.message),
    }
    check(
        "a walk over a non-pageable CF fails closed instead of reporting zero rows",
        unpageable
            .as_ref()
            .is_err_and(|error| error.source_code == Some("CALYX_ASTER_SST_PAGE_INDEX_MISSING")),
        &format!(
            "the CF holds {assay_rows} row(s); a silent empty result here would be a wrong              answer, which is the failure mode this whole issue is about"
        ),
        &mut failures,
    );

    println!(
        "
  C2b/C2c: pageable slot CFs below one page, and empty ones"
    );
    let mut small_cf = None;
    let mut empty_cf = None;
    for slot in [113u16, 22, 74, 66, 59, 104, 50, 81] {
        let cf = ColumnFamily::Slot {
            slot: SlotId::new(slot),
            kind: SlotFamilyKind::Quantized,
        };
        let rows = vault.count_cf_latest(cf)?;
        println!("      before: slot_{slot:02} holds {rows} row(s)");
        if rows == 0 {
            empty_cf.get_or_insert((cf, slot));
        } else if rows < SYNAPSE_CALYX_CF_WALK_PAGE_ROWS {
            small_cf.get_or_insert((cf, slot, rows));
        }
    }

    if let Some((cf, slot, rows)) = small_cf {
        let mut visited = 0usize;
        let walk = vault.walk_cf_latest(cf, SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, |_k, _v| {
            visited += 1;
            Ok(SynapseCalyxWalkStep::Continue)
        })?;
        println!(
            "      after : slot_{slot:02} ({rows} rows) pages={} rows_visited={} rows_examined={}",
            walk.pages, walk.rows_visited, walk.rows_examined
        );
        check(
            "a CF smaller than one page reads exactly one page and visits every row",
            walk.pages == 1 && walk.rows_visited == rows && visited == rows,
            &format!("pages={} visited={visited} expected={rows}", walk.pages),
            &mut failures,
        );
    } else {
        check(
            "a pageable CF smaller than one page was available to test",
            false,
            "no probed slot CF is non-empty and under one page; this case proved nothing",
            &mut failures,
        );
    }

    if let Some((cf, slot)) = empty_cf {
        let mut visited = 0usize;
        let walk = vault.walk_cf_latest(cf, SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, |_k, _v| {
            visited += 1;
            Ok(SynapseCalyxWalkStep::Continue)
        })?;
        println!(
            "      after : slot_{slot:02} (empty) pages={} rows_visited={} stopped_early={} atomic={}",
            walk.pages,
            walk.rows_visited,
            walk.stopped_early,
            walk.atomic()
        );
        check(
            "an empty CF reads exactly one page, visits nothing, and is still atomic",
            walk.pages == 1
                && walk.rows_visited == 0
                && visited == 0
                && !walk.stopped_early
                && walk.atomic(),
            &format!("pages={} visited={visited}", walk.pages),
            &mut failures,
        );
    } else {
        check(
            "an empty pageable CF was available to test",
            false,
            "no probed slot CF is empty; this case proved nothing",
            &mut failures,
        );
    }

    // C3: an early stop must actually stop. This is the property that keeps the
    // migrated `load_record_slots` lookup from paging the whole CF.
    println!("\n  C3: a visitor that stops on the first row");
    println!(
        "      before: Base holds {} row(s) over {} page(s)",
        walk.rows_visited, walk.pages
    );
    let mut seen = 0usize;
    let stopped = vault.walk_cf_latest(
        ColumnFamily::Base,
        SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
        |_k, _v| {
            seen += 1;
            Ok(SynapseCalyxWalkStep::Stop)
        },
    )?;
    println!(
        "      after : pages={} rows_visited={} stopped_early={} (visitor saw {seen} row)",
        stopped.pages, stopped.rows_visited, stopped.stopped_early
    );
    check(
        "an early stop reads one page, visits one row, and reports stopped_early",
        stopped.pages == 1 && stopped.rows_visited == 1 && seen == 1 && stopped.stopped_early,
        "a Stop that still paged the whole CF would defeat the migrated lookup callers",
        &mut failures,
    );

    // C4: the smallest legal page. One row per page exercises the resume cursor
    // on every single row — the path that would spin forever if it stalled.
    println!("\n  C4: page_rows = 1 over the first 2,000 Base rows (cursor stress)");
    let mut counted = 0usize;
    let small = vault.walk_cf_latest(ColumnFamily::Base, 1, |_k, _v| {
        counted += 1;
        if counted >= 2_000 {
            Ok(SynapseCalyxWalkStep::Stop)
        } else {
            Ok(SynapseCalyxWalkStep::Continue)
        }
    })?;
    println!(
        "      after : pages={} rows_visited={} rows_examined={} stopped_early={}",
        small.pages, small.rows_visited, small.rows_examined, small.stopped_early
    );
    check(
        "one row per page advances the cursor on every row without stalling",
        small.pages == 2_000 && small.rows_visited == 2_000 && small.stopped_early,
        &format!("pages={} visited={}", small.pages, small.rows_visited),
        &mut failures,
    );

    // -----------------------------------------------------------------------
    // D. Bits on disk: a fresh read-only reopen, after close.
    // -----------------------------------------------------------------------
    println!("\n=== D. bits on disk — fresh read-only reopen after close");
    let claimed_rows = walk.rows_visited;
    vault.close("base_walk_hold_fsv finished measuring")?;
    // `open_existing()` selects only `ColumnFamily::Kv`, and asking the handle
    // for an unselected CF answers `0` rather than erroring — so the CF under
    // test is named explicitly here. (Filed separately: a read-only handle that
    // answers an unselected CF with an empty result is a silent wrong answer.)
    let reopened =
        SynapseCalyxReadOnlyVault::open_existing_with_cfs(config, Some(vec![ColumnFamily::Base]))?;
    let on_disk = reopened.scan_cf_latest(ColumnFamily::Base)?.len();
    println!("  walk reported visiting : {claimed_rows} Base rows");
    println!("  physical rows on disk  : {on_disk} Base rows");
    check(
        "the walk visited exactly the rows the reopened vault holds",
        on_disk == claimed_rows,
        &format!("on_disk={on_disk} claimed={claimed_rows}"),
        &mut failures,
    );

    println!("\n----------------------------------------------------------------");
    if failures.is_empty() {
        println!("PASS: every check held.");
        Ok(())
    } else {
        println!("FAIL: {} check(s) failed:", failures.len());
        for failure in &failures {
            println!("  - {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}
