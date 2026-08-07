//! Manual Full State Verification for #2093: the panel catalog's declared
//! lineage, checked against the generation allocator that actually owns it.
//!
//! ## What #2093 reported, and what is actually true
//!
//! The deployed census read:
//!
//! ```text
//! anchors_stranded_panels=["syn-graphpos-app-v1@1685001 (59 anchored record(s) stranded;
//!   active holds 0, superseded generation(s) 1665001(166) hold 166)"]
//! syn-graphpos-app-v1@1685001:records=0 cov=n/a grounded=0/0 superseded=16650
//! ```
//!
//! and the issue read it as "a version bump minted an empty active generation
//! `1685001` that the publish's fingerprint short-circuit will never populate".
//! **Both halves of that reading are wrong, and Part 1 measures why.**
//!
//! * `1_685_001` is the derived family's *reserved base* generation. A derived
//!   snapshot publishes under a generation the vault-global allocator mints, and
//!   #2062 floored dynamic allocation at `3_000_000_000`. No publish can put a
//!   row at `1_685_001` — so `records=0` there is permanent and correct, and a
//!   skip predicate changed to "publish when the active generation is empty"
//!   would mint one permanent owner row **per tick, forever**, and still never
//!   change that reading. That is #2081's leak, reintroduced.
//! * `1_665_001` is not a graph-panel generation at all. It is
//!   `syn-agent-event-v1`'s, retired by #1983 — whose commit appended it to the
//!   `syn-graphpos-app-v1` catalog entry immediately above the agent-event entry
//!   in the same file. The census believed the catalog, attributed 16,650
//!   agent-event rows to a panel declared `backfill_source_cf: None`, and
//!   reported 59 replayable anchors as permanently unrepairable.
//!
//! ## The authority
//!
//! The allocator's owners map. Every generation was reserved
//! `builtin:<panel_name>` while it was that panel's active one, and that row is
//! durable and never rewritten — so it remembers which panel wrote a generation
//! long after the catalog constant moved on. This harness recreates that exact
//! reservation, the way the daemon wrote it, and reads the census against it.
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | a real tick publishes at a dynamic generation, never at `1_685_001` | the allocator owners map + the `Base` census |
//! | 2 | an unchanged fingerprint mints nothing across further ticks | the allocator's `owner_count` and watermark |
//! | 3 | the corrected catalog agrees with the allocator about `1_665_001` | `catalog_lineage_misattributed` on the aligned vault |
//! | 4 | the graph panel no longer carries agent-event history | its census row + `anchor_debt_unbackfillable_panels` |
//! | 5 | an orphan below the next publish is retired by it (#2081 ask 3) | the allocator's retirement ledger |
//! | 6 | the guard is live, and an unsuperseded orphan is named (#2081 ask 4) | a second vault reserved the way #1983's catalog claimed |
//!
//! ```text
//! cargo run -p synapse-storage --example catalog_lineage_authority_fsv -- <new-empty-scratch-dir>
//! ```
//!
//! Writes two vaults under the supplied directory. Never point it at a vault
//! anyone else is using.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxMathBackend, SynapseCalyxVault};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    SYN_AGENT_EVENT_PANEL_NAME, SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983,
    SYN_GRAPHPOS_APP_PANEL_NAME, SYN_GRAPHPOS_APP_PANEL_VERSION, builtin_panel_catalog,
};
use synapse_storage::derived_state::{
    NoveltyDeliveryReadback, ReactiveDeliveryReadback, register_derived_state_source,
    register_novelty_delivery_sink, register_reactive_delivery_sink, register_region_delivery_sink,
    run_derived_state_maintenance,
};
use synapse_storage::panel_coverage::PanelCoverageReport;
use synapse_storage::{Db, StorageBackendKind, cf};

/// #2062's floor. Everything at or above it is runtime-minted; everything below
/// is code-declared. Restated here rather than imported so the claim is checked
/// against the number the issue argues about.
const DYNAMIC_GENERATION_FLOOR: u32 = 3_000_000_000;
/// Distinct apps in the focus seed. A small cycle the structural pass converges
/// on immediately.
const SEED_APPS: u64 = 5;
const SEED_ROWS: u64 = 40;
/// Ticks driven after the first publish, to prove the fingerprint short-circuit
/// mints nothing while the graph is unchanged.
const IDLE_TICKS: usize = 2;

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

fn cpu_config(dir: &Path) -> SynapseCalyxConfig {
    // A scratch FSV vault on a CUDA host must say which math it wants: `Auto`
    // refuses rather than silently running CPU kernels on a GPU box.
    let mut config = SynapseCalyxConfig::from_vault_dir(dir.to_path_buf());
    config.tuning.math_backend = SynapseCalyxMathBackend::Cpu;
    config
}

/// Writes the `builtin:<panel>` owner claim the daemon wrote when `generation`
/// was that panel's active version, and optionally the `dynamic:` claim a
/// pre-#2081 publish left behind when its spectral pass failed *after*
/// allocating. Closes the vault so `Db` can open it.
///
/// The dynamic claim is minted through the allocator itself rather than
/// fabricated, so it is byte-identical to the residue on the deployed vault:
/// an owner row, an operation mapping, and no `Base` row anywhere.
fn seed_allocator_history(
    dir: &Path,
    reservation: (&str, u32),
    orphan_publisher: Option<&str>,
) -> Result<Option<u32>, Box<dyn Error>> {
    std::fs::create_dir_all(dir)?;
    let vault = SynapseCalyxVault::open(cpu_config(dir))?;
    let (panel_name, generation) = reservation;
    let readback = vault.reserve_panel_generations(&[(panel_name.to_owned(), generation)])?;
    println!(
        "  RESERVED generation={generation} owner={:?} owner_count={}",
        readback.owners.get(&generation),
        readback.owner_count
    );
    let orphan = match orphan_publisher {
        Some(panel) => {
            let allocation = vault.allocate_panel_generation(panel, &"2093".repeat(16))?;
            println!(
                "  ORPHANED generation={} owner=dynamic:{panel} rows=0 (the residue of a publish \
                 that allocated and never committed)",
                allocation.panel_generation
            );
            Some(allocation.panel_generation)
        }
        None => None,
    };
    drop(vault);
    Ok(orphan)
}

fn focus_row(index: u64) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: 1_785_955_000_000_000_000 + index * 1_000_000_000,
        kind: TimelineKind::FocusChange,
        actor: TimelineActor::Human,
        app: Some(format!("app-{}", index % SEED_APPS)),
        payload: json!({ "seq": index }),
    };
    Ok((
        format!("t{index:08}").into_bytes(),
        serde_json::to_vec(&record)?,
    ))
}

fn panel_row<'a>(
    report: &'a PanelCoverageReport,
    name: &str,
) -> &'a synapse_storage::panel_coverage::PanelCoverageRow {
    report
        .panels
        .iter()
        .find(|panel| panel.panel_name == name)
        .unwrap_or_else(|| panic!("{name} absent from the panel coverage report"))
}

/// `(owner_count, next_generation, generations owned by the graph app panel)`.
fn allocator_state(db: &Db) -> Result<(u64, u32, Vec<u32>), Box<dyn Error>> {
    let readback = db.panel_generation_allocator()?;
    let mut owned: Vec<u32> = readback
        .owners
        .iter()
        .filter(|(_, owner)| {
            owner.as_str() == format!("builtin:{SYN_GRAPHPOS_APP_PANEL_NAME}")
                || owner.starts_with(&format!("dynamic:{SYN_GRAPHPOS_APP_PANEL_NAME}:"))
        })
        .map(|(generation, _)| *generation)
        .collect();
    owned.sort_unstable();
    Ok((readback.owner_count, readback.next_generation, owned))
}

#[allow(
    clippy::too_many_lines,
    reason = "one FSV run: two vaults, real ticks and physical reads are a single ordered narrative"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: catalog_lineage_authority_fsv <new-empty-scratch-dir>")?;
    let aligned_dir = root.join("aligned");
    let misfiled_dir = root.join("misfiled");
    println!("SOURCE_OF_TRUTH root={}", root.display());
    println!(
        "CATALOG syn-agent-event-v1 PRE_1983 generation={SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983} \
         graph_app_declared_version={SYN_GRAPHPOS_APP_PANEL_VERSION}"
    );

    // ------------------------------------------------------------------
    // Vault A — the deployed truth: 1_665_001 was syn-agent-event-v1's
    // ------------------------------------------------------------------
    println!(
        "\n== Setup: recreating the allocator claim the daemon wrote when 1665001 was active =="
    );
    let aligned_orphan = seed_allocator_history(
        &aligned_dir,
        (
            SYN_AGENT_EVENT_PANEL_NAME,
            SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983,
        ),
        Some(SYN_GRAPHPOS_APP_PANEL_NAME),
    )?
    .ok_or("the aligned vault must carry a seeded dynamic orphan")?;

    let db = Arc::new(Db::open_with_resolved_calyx_config(
        &aligned_dir,
        SCHEMA_VERSION,
        StorageBackendKind::default(),
        cpu_config(&aligned_dir),
    )?);
    register_derived_state_source(&db);
    register_reactive_delivery_sink(|_| Ok(ReactiveDeliveryReadback::default()));
    register_region_delivery_sink(|_| Ok(ReactiveDeliveryReadback::default()));
    register_novelty_delivery_sink(|_| Ok(NoveltyDeliveryReadback::default()));

    // ------------------------------------------------------------------
    // Part 1 — where a real publish actually puts its rows
    // ------------------------------------------------------------------
    println!("\n== Part 1: a real derived-state tick publishes the app-transition graph ==");
    let mut rows = Vec::new();
    for index in 0..SEED_ROWS {
        rows.push(focus_row(index)?);
    }
    db.put_batch(cf::CF_TIMELINE, rows)?;
    let (owners_before, next_before, owned_before) = allocator_state(&db)?;
    println!(
        "  ALLOCATOR_BEFORE owner_count={owners_before} next_generation={next_before} \
         graph_app_owned={owned_before:?}"
    );
    let tick = run_derived_state_maintenance();
    println!(
        "  TICK_1 returned={}",
        match &tick {
            Ok(()) => "Ok(())".to_owned(),
            Err(error) => format!("Err({error})"),
        }
    );
    let (owners_after, next_after, owned_after) = allocator_state(&db)?;
    println!(
        "  ALLOCATOR_AFTER owner_count={owners_after} next_generation={next_after} \
         graph_app_owned={owned_after:?}"
    );
    let report = db.measure_panel_coverage()?;
    let graph_panel = panel_row(&report, SYN_GRAPHPOS_APP_PANEL_NAME);
    let dynamic_rollup = report
        .owned_dynamic_generations
        .iter()
        .find(|rollup| {
            rollup.panel_name == SYN_GRAPHPOS_APP_PANEL_NAME && rollup.owner_kind == "dynamic"
        })
        .cloned();
    println!(
        "  CENSUS {}@{} records={} superseded_records={} stranded={} coverage={:?}",
        graph_panel.panel_name,
        graph_panel.panel_version,
        graph_panel.active_version_records,
        graph_panel.superseded_records,
        graph_panel.anchors_stranded_on_superseded,
        graph_panel.coverage_fraction,
    );
    match &dynamic_rollup {
        Some(rollup) => println!(
            "  DYNAMIC  {} live_generations={:?} live_records={} retired={}",
            rollup.panel_name,
            rollup.live_generations,
            rollup.live_records,
            rollup.retired_generations.len()
        ),
        None => println!("  DYNAMIC  <no dynamic rollup for the graph app panel>"),
    }
    let minted: Vec<u32> = owned_after
        .iter()
        .copied()
        .filter(|generation| !owned_before.contains(generation))
        .collect();
    f.check(
        "the publish minted exactly one generation",
        minted.len() == 1,
        &format!("minted={minted:?}"),
    );
    f.check(
        "the minted generation is above the dynamic floor, not the declared base",
        minted
            .first()
            .is_some_and(|generation| *generation >= DYNAMIC_GENERATION_FLOOR),
        &format!("minted={minted:?} floor={DYNAMIC_GENERATION_FLOOR} declared_base={SYN_GRAPHPOS_APP_PANEL_VERSION}"),
    );
    f.check(
        "the catalog's declared active generation holds zero rows, permanently and correctly",
        graph_panel.active_version_records == 0,
        &format!(
            "{}@{} active_version_records={}",
            graph_panel.panel_name, graph_panel.panel_version, graph_panel.active_version_records
        ),
    );
    f.check(
        "the rows the publish wrote are attributed through the dynamic rollup",
        dynamic_rollup
            .as_ref()
            .is_some_and(|rollup| rollup.live_records > 0),
        &format!(
            "dynamic_live_records={:?}",
            dynamic_rollup.as_ref().map(|rollup| rollup.live_records)
        ),
    );

    // ------------------------------------------------------------------
    // Part 2 — an unchanged fingerprint mints nothing
    // ------------------------------------------------------------------
    println!("\n== Part 2: further ticks over the same graph ==");
    for tick_no in 1..=IDLE_TICKS {
        let outcome = run_derived_state_maintenance();
        let (owners, next, owned) = allocator_state(&db)?;
        println!(
            "  IDLE_TICK {tick_no} returned={} owner_count={owners} next_generation={next} \
             graph_app_owned={owned:?}",
            if outcome.is_ok() { "Ok(())" } else { "Err" }
        );
    }
    let (owners_idle, next_idle, owned_idle) = allocator_state(&db)?;
    f.check(
        "the fingerprint short-circuit minted nothing across the idle ticks",
        (owners_idle, next_idle, owned_idle.clone())
            == (owners_after, next_after, owned_after.clone()),
        &format!(
            "before=({owners_after}, {next_after}, {owned_after:?}) \
             after_{IDLE_TICKS}_idle_ticks=({owners_idle}, {next_idle}, {owned_idle:?})"
        ),
    );
    let idle_report = db.measure_panel_coverage()?;
    f.check(
        "and the declared active generation is still empty, so 'publish when it is empty' would \
         mint one owner row per tick forever without ever changing that reading",
        panel_row(&idle_report, SYN_GRAPHPOS_APP_PANEL_NAME).active_version_records == 0,
        &format!(
            "active_version_records={}",
            panel_row(&idle_report, SYN_GRAPHPOS_APP_PANEL_NAME).active_version_records
        ),
    );

    // ------------------------------------------------------------------
    // Part 3 — the corrected catalog agrees with the allocator
    // ------------------------------------------------------------------
    println!("\n== Part 3: catalog lineage vs allocator ownership ==");
    println!(
        "  MISATTRIBUTED {:?}",
        idle_report.catalog_lineage_misattributed
    );
    f.check(
        "no catalog entry claims a generation the allocator gives to another panel",
        idle_report.catalog_lineage_misattributed.is_empty(),
        &format!("findings={:?}", idle_report.catalog_lineage_misattributed),
    );
    let agent_event_lineage: Vec<u32> = builtin_panel_catalog()
        .into_iter()
        .find(|entry| entry.panel_name == SYN_AGENT_EVENT_PANEL_NAME)
        .map(|entry| entry.superseded_versions.to_vec())
        .unwrap_or_default();
    let graph_lineage: Vec<u32> = builtin_panel_catalog()
        .into_iter()
        .find(|entry| entry.panel_name == SYN_GRAPHPOS_APP_PANEL_NAME)
        .map(|entry| entry.superseded_versions.to_vec())
        .unwrap_or_default();
    println!(
        "  LINEAGE syn-agent-event-v1={agent_event_lineage:?} syn-graphpos-app-v1={graph_lineage:?}"
    );
    f.check(
        "1665001 is declared by the panel that wrote it",
        agent_event_lineage.contains(&SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983),
        &format!("syn-agent-event-v1 lineage={agent_event_lineage:?}"),
    );
    f.check(
        "and by no other panel",
        !graph_lineage.contains(&SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983),
        &format!("syn-graphpos-app-v1 lineage={graph_lineage:?}"),
    );

    // ------------------------------------------------------------------
    // Part 4 — the graph panel no longer carries another panel's debt
    // ------------------------------------------------------------------
    println!("\n== Part 4: the anchor debt the misattribution manufactured ==");
    let graph_panel = panel_row(&idle_report, SYN_GRAPHPOS_APP_PANEL_NAME);
    println!(
        "  UNBACKFILLABLE {:?}",
        idle_report.anchor_debt_unbackfillable_panels
    );
    f.check(
        "the graph panel declares no superseded records",
        graph_panel.superseded_records == 0 && graph_panel.superseded_versions_present.is_empty(),
        &format!(
            "superseded_records={} present={:?}",
            graph_panel.superseded_records, graph_panel.superseded_versions_present
        ),
    );
    f.check(
        "the graph panel is named by no anchor-debt finding",
        !idle_report
            .anchor_debt_unbackfillable_panels
            .iter()
            .any(|line| line.contains(SYN_GRAPHPOS_APP_PANEL_NAME)),
        &format!(
            "findings={:?}",
            idle_report.anchor_debt_unbackfillable_panels
        ),
    );

    // ------------------------------------------------------------------
    // Part 5 — #2081 ask 4: live owner rows the census cannot see
    // ------------------------------------------------------------------
    println!("\n== Part 5: allocator-live vs census-live (#2081 asks 3/4) ==");
    println!(
        "  DIVERGENCE allocator_live={} census_live={} dynamic_without_records={:?}",
        idle_report.allocator_live_count,
        idle_report.census_live_count,
        idle_report.allocator_live_dynamic_without_records
    );
    let retired_orphan = db
        .panel_generation_allocator()?
        .retired
        .get(&aligned_orphan)
        .copied();
    println!("  SEEDED_ORPHAN generation={aligned_orphan} retired_by={retired_orphan:?}");
    f.check(
        "the two totals are published, so a live claim the census cannot see is expressible",
        idle_report.allocator_live_count > idle_report.census_live_count,
        &format!(
            "allocator_live={} census_live={}",
            idle_report.allocator_live_count, idle_report.census_live_count
        ),
    );
    // #2081 ask 3, measured rather than assumed: an orphan minted BELOW the next
    // successful publish is retired by that publish's own supersession, with no
    // operator action and no new mechanism. That is why the actionable list is
    // empty on this vault even though an orphan was deliberately seeded into it.
    f.check(
        "the next successful publish retired the seeded orphan behind itself",
        retired_orphan.is_some(),
        &format!("orphan={aligned_orphan} retired_by={retired_orphan:?}"),
    );
    f.check(
        "builtin reservations without rows are NOT reported as findings",
        idle_report
            .allocator_live_dynamic_without_records
            .iter()
            .all(|line| !line.contains("builtin:")),
        &format!(
            "dynamic_without_records={:?}",
            idle_report.allocator_live_dynamic_without_records
        ),
    );

    // ------------------------------------------------------------------
    // Part 6 — the guard is live, on a vault reserved the way #1983 claimed
    // ------------------------------------------------------------------
    println!("\n== Part 6: control — the same generation owned by the graph panel ==");
    drop(db);
    let control_orphan = seed_allocator_history(
        &misfiled_dir,
        (
            SYN_GRAPHPOS_APP_PANEL_NAME,
            SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983,
        ),
        Some(SYN_GRAPHPOS_APP_PANEL_NAME),
    )?
    .ok_or("the control vault must carry a seeded dynamic orphan")?;
    let control = Db::open_with_resolved_calyx_config(
        &misfiled_dir,
        SCHEMA_VERSION,
        StorageBackendKind::default(),
        cpu_config(&misfiled_dir),
    )?;
    let control_report = control.measure_panel_coverage()?;
    println!(
        "  MISATTRIBUTED {:?}",
        control_report.catalog_lineage_misattributed
    );
    f.check(
        "a catalog/allocator disagreement about 1665001 is detected and named",
        control_report
            .catalog_lineage_misattributed
            .iter()
            .any(|line| {
                line.contains(SYN_AGENT_EVENT_PANEL_NAME)
                    && line.contains(&SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983.to_string())
                    && line.contains(SYN_GRAPHPOS_APP_PANEL_NAME)
            }),
        &format!(
            "findings={:?}",
            control_report.catalog_lineage_misattributed
        ),
    );
    // No publish ever ran on this vault, so nothing superseded the orphan and it
    // is still live. This is the shape the ~19-20 owner rows on the deployed
    // vault are in: minted, never written to, and above the last successful
    // publish so no supersession can reach them.
    println!(
        "  DIVERGENCE allocator_live={} census_live={} dynamic_without_records={:?}",
        control_report.allocator_live_count,
        control_report.census_live_count,
        control_report.allocator_live_dynamic_without_records
    );
    f.check(
        "an unsuperseded dynamic orphan is named by the census (#2081 ask 4)",
        control_report
            .allocator_live_dynamic_without_records
            .iter()
            .any(|line| line.starts_with(&format!("{control_orphan} "))),
        &format!(
            "orphan={control_orphan} named={:?}",
            control_report.allocator_live_dynamic_without_records
        ),
    );

    println!();
    if f.0.is_empty() {
        println!(
            "VERDICT PASS: derived publishes mint dynamic generations and never populate 1685001, \
             the unchanged fingerprint leaks nothing, 1665001 is declared by the panel the \
             allocator says wrote it, the graph panel carries no manufactured anchor debt, and \
             the catalog/allocator cross-check detects the disagreement it exists to catch"
        );
        return Ok(());
    }
    for failure in &f.0 {
        println!("FAILED {failure}");
    }
    Err(format!("{} check(s) failed", f.0.len()).into())
}
