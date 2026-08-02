//! Manual FSV for #1963: the active operator panel now carries a graded dense
//! lens, a constant cosine lane can never be a content slot again, and a panel
//! with no graded view is refused at admission.
//!
//! ## The defect
//!
//! All five dense lenses on `syn-timeline-v1 @ 1900001` returned nearest-
//! neighbour cosine **exactly 1.0** for all 924 records. Four saturated a finite
//! image (one-hot over ~12 kinds and 2 actors, cyclic over 24 hours and 7 days);
//! the fifth, `event_time_rank`, is a dense `dim = 1` lane whose values live in
//! `[0, 1]`, so its cosine is identically `+1` and could never have been
//! anything else. The panel had no lens whose "these two records are alike" was
//! a measurement, so every neighbourhood surface on it was resolving ties.
//!
//! ## What this harness proves, and where it reads the truth from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | the per-slot grading table says what first principles say | the built panel contract's persisted `LensSpec`s |
//! | 2 | the two fail-closed gates actually refuse | the returned `StorageError`, driven directly |
//! | 3 | ingest measures slot 104, unit-norm, dim 32 | `build_timeline_constellation` on synthetic records with hand-computed vectors |
//! | 4 | edge cases do not silently degrade | empty / maximal / agent-actor records, printed before and after |
//! | 5 | the lens grades the **real** corpus | every `CF_TIMELINE` row of a frozen vault copy, re-measured through the real ingest path |
//! | 6 | the bytes land on disk | slot 104 read back out of the vault after a real backfill write |
//!
//! Parts 5 and 6 need a **copy** of a Synapse data directory: part 6 writes.
//!
//! ```text
//! cargo run -p synapse-storage --example panel_graded_lens_fsv -- <vault-parent-dir>
//! ```

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_slot_vector;
use calyx_core::{Constellation, SlotId, SlotVector};
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::Db;
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_VERSION, SYN_TIMELINE_PANEL_VERSION_PRE_1963,
    build_timeline_constellation, syn_active_panel_contract, syn_panel_cosine_grading,
};

const SCHEMA_VERSION: u32 = 1;
const SLOT_RECORD_VECTOR: u16 = 104;
const SLOT_RECENCY_RANK: u16 = 7;
const RECORD_VECTOR_DIM: usize = 32;
/// `SIMILARITY_DISTINCT_TOLERANCE` from `calyx_loom`: two cosines closer than
/// this are the same value for discrimination purposes.
const DISTINCT_TOLERANCE: f32 = 1e-4;

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

#[allow(
    clippy::too_many_lines,
    reason = "one linear audit; splitting separates a measurement from the check that admits it"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());

    // ------------------------------------------------------------------
    // Part 1 — the per-slot grading table (#1963 ask 3, the readback half)
    // ------------------------------------------------------------------
    println!("== Part 1: syn_panel_cosine_grading({SYN_TIMELINE_PANEL_VERSION}) ==");
    let rows = syn_panel_cosine_grading(SYN_TIMELINE_PANEL_VERSION)?
        .ok_or("the active timeline panel has no built-in contract")?;
    for row in &rows {
        println!(
            "  slot {:>3}  {:<36} grading={:<9} finite_directions={:?} retrieval_only={}",
            row.slot_id, row.slot_key, row.grading, row.finite_directions, row.retrieval_only
        );
    }
    // Hand-derived expectations, one per slot:
    //   1  kind_onehot(32)     -> 32 orthogonal directions          => finite(32)
    //   2  app_hash(1024)      -> sparse lane                       => not_dense
    //   3  title_sparse(2048)  -> sparse lane                       => not_dense
    //   4  hour_cyclic(24)     -> 24 positions on a cycle           => finite(24)
    //   5  dow_cyclic(7)       -> 7 positions on a cycle            => finite(7)
    //   6  actor_onehot(8)     -> 8 orthogonal directions           => finite(8)
    //   7  event_time_rank     -> dim 1 on [0,1], cosine == +1      => constant, retrieval-only
    // 103  title_bm25(2048)    -> sparse lane                       => not_dense
    // 104  record_vector(32)   -> continuum                         => graded
    let expected: &[(u16, &str, Option<u32>, bool)] = &[
        (1, "finite", Some(32), false),
        (2, "not_dense", None, false),
        (3, "not_dense", None, false),
        (4, "finite", Some(24), false),
        (5, "finite", Some(7), false),
        (6, "finite", Some(8), false),
        (7, "constant", None, true),
        (103, "not_dense", None, false),
        (104, "graded", None, false),
    ];
    let observed: Vec<(u16, &str, Option<u32>, bool)> = rows
        .iter()
        .map(|r| (r.slot_id, r.grading, r.finite_directions, r.retrieval_only))
        .collect();
    f.check("grading table", observed, expected.to_vec());
    f.check(
        "the panel carries exactly one graded dense content slot",
        rows.iter()
            .filter(|r| r.grading == "graded" && !r.retrieval_only)
            .map(|r| r.slot_id)
            .collect::<Vec<_>>(),
        vec![SLOT_RECORD_VECTOR],
    );
    f.check(
        "the superseded generation has no contract",
        syn_panel_cosine_grading(SYN_TIMELINE_PANEL_VERSION_PRE_1963)?.is_none(),
        true,
    );

    // ------------------------------------------------------------------
    // Part 2 — the two fail-closed gates, driven directly
    // ------------------------------------------------------------------
    println!("\n== Part 2: the gates refuse ==");
    // 2a. A panel whose content slots are exactly the pre-#1963 set — the same
    // contract minus slot 104 — must be refused. This is the physical proof the
    // gate would have caught the defect the issue reports.
    let contract = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, 0)?
        .ok_or("no active timeline panel contract")?;
    let pre_1963: Vec<_> = contract
        .panel
        .slots
        .iter()
        .filter(|s| s.slot_id.get() != SLOT_RECORD_VECTOR)
        .cloned()
        .collect();
    let refusal = synapse_storage::constellations::assert_panel_carries_graded_dense_lens(
        SYN_TIMELINE_PANEL_VERSION_PRE_1963,
        &pre_1963,
        &contract.registry,
    );
    match &refusal {
        Ok(()) => println!("  (no refusal)"),
        Err(error) => println!("  refusal: {error}"),
    }
    f.check(
        "the pre-#1963 slot set is refused",
        refusal
            .as_ref()
            .err()
            .map(|e| e.to_string().contains("CALYX_PANEL_NO_GRADED_DENSE_LENS")),
        Some(true),
    );
    // 2b. The full set passes, or the gate is simply broken.
    f.check(
        "the #1963 slot set passes",
        synapse_storage::constellations::assert_panel_carries_graded_dense_lens(
            SYN_TIMELINE_PANEL_VERSION,
            &contract.panel.slots,
            &contract.registry,
        )
        .is_ok(),
        true,
    );
    // 2c. A constant-cosine encoder cannot be registered as a content slot.
    let constant_as_content = synapse_storage::constellations::syn_content_slot(
        SlotId::new(SLOT_RECENCY_RANK),
        "syn.timeline.event_time_rank.v1",
        calyx_registry::AlgorithmicLens::syn_scalar_rank(
            "syn.timeline.event_time_rank.v1",
            calyx_core::Modality::Structured,
            0,
            4_102_444_800_000 * 1_000_000,
        ),
        SYN_TIMELINE_PANEL_VERSION,
        &mut calyx_registry::Registry::new(),
    );
    match &constant_as_content {
        Ok(_) => println!("  (no refusal)"),
        Err(error) => println!("  refusal: {error}"),
    }
    f.check(
        "a constant cosine lane is refused as a content slot",
        constant_as_content
            .as_ref()
            .err()
            .map(|e| e.to_string().contains("CALYX_PANEL_SLOT_COSINE_CONSTANT")),
        Some(true),
    );

    // ------------------------------------------------------------------
    // Part 3 — ingest measures slot 104 (synthetic, hand-computed)
    // ------------------------------------------------------------------
    println!("\n== Part 3: build_timeline_constellation writes slot 104 ==");
    let cx = measure(&record(
        TimelineKind::FocusChange,
        1_760_000_000_000_000_000,
        Some("notepad.exe"),
        Some("untitled - Notepad"),
    ))?;
    let vector = dense(&cx, SLOT_RECORD_VECTOR).ok_or("slot 104 is absent after ingest")?;
    println!("  slot 104 = {vector:?}");
    f.check("slot 104 dimension", vector.len(), RECORD_VECTOR_DIM);
    f.check(
        "slot 104 is unit norm (within the L2 tolerance 1e-3)",
        (norm(&vector) - 1.0).abs() <= 1.0e-3,
        true,
    );
    f.check(
        "slot 104 is finite",
        vector.iter().all(|v| v.is_finite()),
        true,
    );
    f.check(
        "slot 7 is still measured (retrieval-only, not removed)",
        dense(&cx, SLOT_RECENCY_RANK).map(|v| v.len()),
        Some(1usize),
    );
    // Determinism: the same record must measure to byte-identical output.
    let again = measure(&record(
        TimelineKind::FocusChange,
        1_760_000_000_000_000_000,
        Some("notepad.exe"),
        Some("untitled - Notepad"),
    ))?;
    f.check(
        "slot 104 is deterministic",
        dense(&again, SLOT_RECORD_VECTOR).as_ref() == Some(&vector),
        true,
    );

    // ------------------------------------------------------------------
    // Part 4 — edge cases, state printed before and after
    // ------------------------------------------------------------------
    println!("\n== Part 4: edge cases ==");
    // Edge A — the emptiest possible record: no app, no title, no payload.
    let bare = record(TimelineKind::IdleStart, 0, None, None);
    println!("  A input : kind=idle_start ts_ns=0 app=None title=None payload=null");
    let bare_cx = measure(&bare)?;
    let bare_v = dense(&bare_cx, SLOT_RECORD_VECTOR).ok_or("slot 104 absent on the bare record")?;
    println!(
        "  A output: slot104 norm={:.6} nonzero={}",
        norm(&bare_v),
        bare_v.iter().filter(|v| **v != 0.0).count()
    );
    f.check("A.dimension", bare_v.len(), RECORD_VECTOR_DIM);
    f.check("A.unit norm", (norm(&bare_v) - 1.0).abs() <= 1.0e-3, true);
    // The bare record still has non-zero components: `kind_ordinal`,
    // `raw_len_norm` and the `ln(1+0)/ln(1+scale) = 0` lengths mean at least the
    // kind and the row length carry it. A record whose every field were zero
    // would be un-normalizable and the encoder would fail closed, which is why
    // this check exists.
    f.check(
        "A.has at least one non-zero component (no un-normalizable all-zero vector)",
        bare_v.iter().any(|v| *v != 0.0),
        true,
    );

    // Edge B — maximal: a very long title and app, at the top of the day.
    let long_title = "x".repeat(4_000);
    let long_app = "a".repeat(300);
    let maximal = record(
        TimelineKind::Purge,
        86_399 * 1_000_000_000,
        Some(&long_app),
        Some(&long_title),
    );
    println!(
        "  B input : kind=purge ts_ns={} app_len={} title_len={}",
        86_399u64 * 1_000_000_000,
        long_app.len(),
        long_title.len()
    );
    let max_cx = measure(&maximal)?;
    let max_v =
        dense(&max_cx, SLOT_RECORD_VECTOR).ok_or("slot 104 absent on the maximal record")?;
    println!("  B output: slot104 norm={:.6}", norm(&max_v));
    f.check("B.unit norm", (norm(&max_v) - 1.0).abs() <= 1.0e-3, true);
    f.check(
        "B.differs from the bare record (the lens discriminates)",
        cosine(&bare_v, &max_v) < 0.999,
        true,
    );

    // Edge C — an agent actor and a browser_nav payload with a url.
    let mut agent = record(
        TimelineKind::BrowserNav,
        1_760_000_123_000_000_000,
        Some("chrome.exe"),
        Some("Example Domain"),
    );
    agent.actor = TimelineActor::Agent {
        session_id: "sess-1".to_owned(),
    };
    agent.payload = serde_json::json!({"title": "Example Domain", "url": "https://example.com/a"});
    println!("  C input : kind=browser_nav actor=agent url=https://example.com/a");
    let agent_cx = measure(&agent)?;
    let agent_v =
        dense(&agent_cx, SLOT_RECORD_VECTOR).ok_or("slot 104 absent on the agent record")?;
    println!("  C output: slot104 norm={:.6}", norm(&agent_v));
    f.check("C.unit norm", (norm(&agent_v) - 1.0).abs() <= 1.0e-3, true);
    // Same record with the human actor must measure differently: `actor_is_agent`
    // is a real component, so flipping it must move the direction.
    let mut human = agent.clone();
    human.actor = TimelineActor::Human;
    let human_v =
        dense(&measure(&human)?, SLOT_RECORD_VECTOR).ok_or("slot 104 absent on the human twin")?;
    let flip = cosine(&agent_v, &human_v);
    println!("  C flip  : cosine(agent, human twin) = {flip:.6}");
    f.check(
        "C.the actor flip changes the measurement",
        flip < 1.0 - DISTINCT_TOLERANCE,
        true,
    );

    // ------------------------------------------------------------------
    // Parts 5 and 6 — the real corpus
    // ------------------------------------------------------------------
    let Some(parent) = std::env::args().nth(1).map(PathBuf::from) else {
        println!("\n== Parts 5-6 skipped: no <vault-parent-dir> argument ==");
        return finish(&f);
    };
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    println!("\n== Part 5: every CF_TIMELINE row, re-measured through the real ingest path ==");
    println!("  vault_dir = {}", vault_dir.display());
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    let raw_rows = db.scan_cf(synapse_storage::cf::CF_TIMELINE)?;
    println!("  CF_TIMELINE rows = {}", raw_rows.len());
    let mut vectors = Vec::with_capacity(raw_rows.len());
    let mut decode_failures = 0usize;
    for (key, raw) in &raw_rows {
        let Ok(record) = serde_json::from_slice::<TimelineRecord>(raw) else {
            decode_failures += 1;
            continue;
        };
        let cx = build_timeline_constellation(context()?, key, raw, &record)?;
        let v = dense(&cx, SLOT_RECORD_VECTOR)
            .ok_or_else(|| format!("slot 104 absent for key {}", hex(key)))?;
        vectors.push(v);
    }
    println!(
        "  measured={} decode_failures={decode_failures}",
        vectors.len()
    );
    f.check("every source row decoded", decode_failures, 0usize);

    if vectors.len() >= 2 {
        let sims = nearest_neighbour_sims(&vectors);
        let (distinct, modal_share, min, max) = discrimination(&sims);
        println!(
            "  slot 104 nearest-neighbour cosine: n={} distinct={distinct} modal_share={modal_share:.6} range=[{min:.6},{max:.6}]",
            sims.len()
        );
        // The binding claim of #1963: the lens must not be definitional.
        // `MIN_DISCRIMINATIVE_DISTINCT = 3`, `MAX_DISCRIMINATIVE_MODAL_SHARE = 0.98`.
        f.check("distinct >= 3", distinct >= 3, true);
        f.check("modal_share < 0.98", modal_share < 0.98, true);
        f.check(
            "the lens reaches below 1.0",
            min < 1.0 - DISTINCT_TOLERANCE,
            true,
        );
    } else {
        f.0.push(format!(
            "only {} timeline rows measured; the corpus check needs at least 2",
            vectors.len()
        ));
    }

    // ------------------------------------------------------------------
    // Part 6 — the bytes on disk
    // ------------------------------------------------------------------
    println!(
        "
== Part 6: slot 104 written to, and read back out of, the vault =="
    );
    let slot_dir = vault_dir.join("cf").join("slot_104");
    let slot_dir_existed_before = slot_dir.is_dir();
    let files_before = count_files(&slot_dir);
    let coverage_before = active_records(&db, SYN_TIMELINE_PANEL_VERSION)?;
    println!(
        "  BEFORE: cf/slot_104 exists={} files={files_before}          panel {SYN_TIMELINE_PANEL_VERSION} active_version_records={coverage_before}",
        slot_dir.is_dir()
    );

    // Backfill a bounded page from the authoritative source CF, which is the
    // same code path the maintenance pass drives.
    let report =
        db.backfill_temporal_metadata(synapse_storage::cf::CF_TIMELINE, None, None, BACKFILL_ROWS)?;
    println!(
        "  backfill: examined={} inserted={} backfilled={} already_current={}",
        report.examined_rows,
        report.inserted_rows,
        report.backfilled_rows,
        report.already_current_rows
    );
    let written = report.inserted_rows + report.backfilled_rows;
    f.check("the backfill wrote rows", written > 0, true);

    let coverage_after = active_records(&db, SYN_TIMELINE_PANEL_VERSION)?;
    // Close the writer before counting files. `flush()` only syncs the WAL —
    // an SST for a brand-new column family is materialized by the checkpoint
    // the close path runs, so counting before the close would be racing the
    // flush scheduler and reporting a 0 that means nothing.
    db.flush()?;
    drop(db);
    let files_after = count_files(&slot_dir);
    println!(
        "  AFTER : cf/slot_104 exists={} files={files_after}          panel {SYN_TIMELINE_PANEL_VERSION} active_version_records={coverage_after}",
        slot_dir.is_dir()
    );
    f.check(
        "cf/slot_104 now exists on disk with at least one file",
        slot_dir.is_dir() && files_after > 0,
        true,
    );
    f.check(
        "cf/slot_104 did not exist before the write",
        (slot_dir_existed_before, files_before),
        (false, 0usize),
    );
    f.check(
        "panel coverage grew by exactly the rows written",
        coverage_after.saturating_sub(coverage_before) as u64,
        written,
    );

    // The independent read: with the writer closed, reopen read-only and pull
    // slot 104's stored bytes out of its own column family. This is the source
    // of truth — not the value the writer returned.
    let readonly = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        synapse_calyx::SynapseCalyxConfig {
            vault_dir: vault_dir.clone(),
            machine_salt_path: parent.join("machine-salt.bin"),
            tuning: synapse_calyx::SynapseCalyxTuningConfig::default().validate()?,
        },
        Some(vec![
            ColumnFamily::Kv,
            ColumnFamily::Base,
            ColumnFamily::slot(SlotId::new(SLOT_RECORD_VECTOR)),
        ]),
    )?;
    let stored = readonly.scan_cf_latest(ColumnFamily::slot(SlotId::new(SLOT_RECORD_VECTOR)))?;
    println!("  cf/slot_104 rows on disk = {}", stored.len());
    f.check(
        "at least the backfilled rows are physically present in cf/slot_104",
        stored.len() as u64 >= written,
        true,
    );

    let mut decoded_dense = 0usize;
    let mut bad_norm = Vec::new();
    let mut first_shown = false;
    for (key, value) in &stored {
        match decode_slot_vector(value)? {
            SlotVector::Dense { dim, data } => {
                decoded_dense += 1;
                if !first_shown {
                    println!("  first stored row key={} dim={dim}", hex(key));
                    println!("  first stored row data={data:?}");
                    first_shown = true;
                }
                if dim as usize != RECORD_VECTOR_DIM || (norm(&data) - 1.0).abs() > 1.0e-3 {
                    bad_norm.push(format!("{}: dim={dim} norm={}", hex(key), norm(&data)));
                }
            }
            other => bad_norm.push(format!("{}: not dense: {other:?}", hex(key))),
        }
    }
    f.check(
        "every stored slot-104 row is a unit-norm dense 32-vector",
        (decoded_dense, bad_norm.as_slice()),
        (stored.len(), &[] as &[String]),
    );

    // And the exact bytes: re-measure one source row and compare against what
    // the vault actually holds for that record's cx_id.
    let (key, raw) = raw_rows
        .first()
        .cloned()
        .ok_or("no CF_TIMELINE row available for the byte comparison")?;
    let record: TimelineRecord = serde_json::from_slice(&raw)?;
    let expected_vector = dense(
        &build_timeline_constellation(context()?, &key, &raw, &record)?,
        SLOT_RECORD_VECTOR,
    )
    .ok_or("slot 104 absent from the re-measured row")?;
    let stored_by_key: BTreeMap<Vec<u8>, Vec<u8>> = stored.into_iter().collect();
    let matches = stored_by_key.values().any(|value| {
        matches!(decode_slot_vector(value), Ok(SlotVector::Dense { data, .. }) if data == expected_vector)
    });
    println!("  re-measured first row's slot 104 = {expected_vector:?}");
    f.check(
        "the re-measured vector is byte-identical to one stored on disk",
        matches,
        true,
    );

    finish(&f)
}

/// The synthetic measurement context.
///
/// The vault id is not an input to any timeline lens, so a fixed value is
/// correct and keeps every synthetic measurement reproducible. It is parsed
/// rather than asserted so a malformed constant is a structured failure, not a
/// panic.
fn context() -> Result<NativeConstellationContext, Box<dyn Error>> {
    Ok(NativeConstellationContext {
        vault_id: "00000000000000000000000003"
            .parse()
            .map_err(|error| format!("fixed synthetic vault id does not parse: {error:?}"))?,
        cx_id: calyx_core::CxId::from_bytes([4u8; 16]),
        created_at_ms: 1_760_000_000_000,
        next_ledger_seq: 1,
    })
}

fn record(
    kind: TimelineKind,
    ts_ns: u64,
    app: Option<&str>,
    title: Option<&str>,
) -> TimelineRecord {
    let mut r = TimelineRecord::new(ts_ns, kind, TimelineActor::Human);
    r.app = app.map(str::to_owned);
    if let Some(title) = title {
        r.payload = serde_json::json!({ "title": title });
    }
    r
}

fn measure(record: &TimelineRecord) -> Result<Constellation, Box<dyn Error>> {
    let raw = serde_json::to_vec(record)?;
    Ok(build_timeline_constellation(
        context()?,
        b"k",
        &raw,
        record,
    )?)
}

fn dense(cx: &Constellation, slot: u16) -> Option<Vec<f32>> {
    match cx.slots.get(&SlotId::new(slot)) {
        Some(SlotVector::Dense { data, .. }) => Some(data.clone()),
        _ => None,
    }
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

fn nearest_neighbour_sims(vectors: &[Vec<f32>]) -> Vec<f32> {
    let mut sims = Vec::with_capacity(vectors.len());
    for (i, a) in vectors.iter().enumerate() {
        let mut best = f32::NEG_INFINITY;
        for (j, b) in vectors.iter().enumerate() {
            if i != j {
                best = best.max(cosine(a, b));
            }
        }
        if best.is_finite() {
            sims.push(best);
        }
    }
    sims
}

/// `(distinct, modal_share, min, max)` under the same 1e-4 run tolerance
/// `calyx_loom::SimilarityDiscrimination` uses.
fn discrimination(values: &[f32]) -> (usize, f32, f32, f32) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mut distinct = 0usize;
    let mut modal = 0usize;
    let mut run = 0usize;
    let mut start = sorted[0];
    for value in &sorted {
        if run > 0 && (*value - start).abs() <= DISTINCT_TOLERANCE {
            run += 1;
            continue;
        }
        modal = modal.max(run);
        distinct += 1;
        start = *value;
        run = 1;
    }
    modal = modal.max(run);
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts far below f32 precision limits"
    )]
    let share = modal as f32 / sorted.len() as f32;
    (distinct, share, sorted[0], sorted[sorted.len() - 1])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn finish(f: &Failures) -> Result<(), Box<dyn Error>> {
    println!("\n================================================================");
    if f.0.is_empty() {
        println!("ALL CHECKS PASSED");
        Ok(())
    } else {
        println!("{} CHECK(S) FAILED:", f.0.len());
        for failure in &f.0 {
            println!("  - {failure}");
        }
        Err("panel_graded_lens_fsv failed".into())
    }
}

/// Rows backfilled in part 6. Small on purpose: the claim is that the write
/// path lands slot 104 on disk with the right bytes, and 64 rows prove that as
/// completely as 100,000 would while keeping the harness a few seconds long.
const BACKFILL_ROWS: usize = 64;

fn count_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn active_records(db: &Db, panel_version: u32) -> Result<usize, Box<dyn Error>> {
    Ok(db
        .measure_panel_coverage()?
        .panels
        .iter()
        .find(|p| p.panel_version == panel_version)
        .map_or(0, |p| p.active_version_records))
}
