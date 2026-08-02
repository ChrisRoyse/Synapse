//! Manual FSV for #1964 on the episode panel: a record vector fed a raw
//! magnitude is a re-encoding of that magnitude, the new encoder refuses one,
//! and the rebuilt lens grades the real corpus the old one tied.
//!
//! ## The defect
//!
//! `syn_record_vector` places each numeric field at `hash(path) % dim`,
//! multiplies by the field's **raw value**, and unit-normalizes.
//! `syn.episode.record_vector.v1` (slot 22) fed `start_unix_ms` and
//! `end_unix_ms` — ~1.7e12 — beside counts in `0..1e4`. The timestamp owned the
//! direction; every other field sat ~1e-9 of the norm, nine orders of magnitude
//! below `f32::EPSILON`, so it could not move a cosine at all.
//!
//! ## What this harness proves, and where it reads the truth from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | the old lens ties every real record | all stored `cf/slot_22` vectors on a frozen vault copy |
//! | 2 | the new encoder refuses a raw magnitude, and accepts a scaled record | the returned `CalyxError`, driven directly |
//! | 3 | three edge cases do not silently degrade | hand-built records, state printed before and after |
//! | 4 | the rebuilt lens grades the **same** real corpus | every `CF_EPISODES` row, re-measured through the real ingest path |
//! | 5 | the admission gate refuses a legacy record vector on a new panel | the returned `StorageError`, driven directly |
//! | 6 | the bytes land on disk | `cf/slot_113` read back after a real backfill write |
//!
//! Parts 1 and 4-6 need a **copy** of a Synapse data directory: part 6 writes.
//!
//! ```text
//! cargo run -p synapse-storage --example episode_record_vector_fsv -- <vault-parent-dir>
//! ```

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_slot_vector;
use calyx_core::{Constellation, Input, Lens, Modality, SlotId, SlotVector};
use calyx_registry::{AlgorithmicLens as RegistryAlgorithmicLens, Registry};
use synapse_core::types::EpisodeRecord;
use synapse_storage::Db;
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_EPISODE_PANEL_VERSION, SYN_EPISODE_PANEL_VERSION_PRE_1964,
    build_episode_constellation, syn_content_slot,
};

const SCHEMA_VERSION: u32 = 1;
/// The superseded magnitude-weighted lane.
const SLOT_OLD: u16 = 22;
/// The #1964 lane.
const SLOT_NEW: u16 = 113;
const RECORD_VECTOR_DIM: usize = 64;
/// `SIMILARITY_DISTINCT_TOLERANCE` from `calyx_loom`: two cosines closer than
/// this are the same value for discrimination purposes.
const DISTINCT_TOLERANCE: f32 = 1e-4;
/// Rows backfilled in part 6. Small on purpose: the claim is that the write
/// path lands slot 113 on disk with the right bytes, and a bounded page proves
/// that as completely as the whole CF would.
const BACKFILL_ROWS: usize = 64;

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
    let Some(parent) = std::env::args().nth(1).map(PathBuf::from) else {
        return Err("usage: episode_record_vector_fsv <vault-parent-dir>".into());
    };
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    // ------------------------------------------------------------------
    // Part 1 — the old lane, read off disk
    // ------------------------------------------------------------------
    println!("== Part 1: stored cf/slot_{SLOT_OLD} vectors (the superseded lane) ==");
    let stored_old = read_slot_vectors(&parent, SLOT_OLD)?;
    println!("  rows on disk = {}", stored_old.len());
    if stored_old.len() >= 2 {
        let sims = nearest_neighbour_sims(&stored_old);
        let (distinct, share, min, max) = discrimination(&sims);
        println!(
            "  nearest-neighbour cosine: distinct={distinct} modal_share={share:.6} range=[{min:.6},{max:.6}]"
        );
        f.check(
            "the superseded lane is tied at exactly one cosine for every record",
            (distinct, share == 1.0, min == 1.0, max == 1.0),
            (1usize, true, true, true),
        );
    } else {
        f.0.push(format!(
            "only {} stored slot-{SLOT_OLD} rows; part 1 needs at least 2",
            stored_old.len()
        ));
    }

    // ------------------------------------------------------------------
    // Part 2 — the encoder's fail-closed precondition
    // ------------------------------------------------------------------
    println!("\n== Part 2: syn_record_vector_unit_fields refuses a raw magnitude ==");
    let lens = RegistryAlgorithmicLens::syn_record_vector_unit_fields(
        "fsv.probe.record_vector.v1",
        Modality::Structured,
        u32::try_from(RECORD_VECTOR_DIM)?,
    );
    // Exactly the shape the superseded episode record had.
    let raw_ms = serde_json::json!({
        "row_count": 12,
        "start_unix_ms": 1_770_000_000_000_u64,
    });
    let refusal = measure_err(&lens, &raw_ms);
    println!("  input  = {raw_ms}");
    println!("  result = {refusal:?}");
    // The refusal must name the *worst* offender: `row_count` (12) is also out
    // of scale, but the timestamp is eleven orders of magnitude worse and is
    // the field an operator has to fix.
    f.check(
        "the refusal names the largest offender, and counts the rest",
        (
            refusal
                .as_ref()
                .is_some_and(|e| e.contains("largest offender is field `start_unix_ms`")),
            refusal
                .as_ref()
                .is_some_and(|e| e.contains("further out-of-scale field(s): row_count=12")),
        ),
        (true, true),
    );
    // The same record on comparable scales is accepted.
    let scaled = serde_json::json!({
        "row_count_norm": 0.36,
        "start_day_fraction": 0.5138,
    });
    let accepted = measure_dense(&lens, &scaled)?;
    println!("  input  = {scaled}");
    println!(
        "  vector norm = {:.6} dim = {}",
        norm(&accepted),
        accepted.len()
    );
    f.check(
        "a comparably-scaled record is accepted as a unit-norm vector",
        (accepted.len(), (norm(&accepted) - 1.0).abs() < 1.0e-5),
        (RECORD_VECTOR_DIM, true),
    );

    // ------------------------------------------------------------------
    // Part 3 — edge cases, state printed before and after
    // ------------------------------------------------------------------
    println!("\n== Part 3: boundary and edge cases ==");
    // (a) exactly on the bound: 1.0 and -1.0 are inside, the smallest step
    //     beyond is not.
    let on_bound = serde_json::json!({ "a": 1.0, "b": -1.0 });
    let past_bound = serde_json::json!({ "a": 1.000_000_1, "b": -1.0 });
    println!("  (a) BEFORE: on_bound={on_bound}  past_bound={past_bound}");
    let on_ok = measure_dense(&lens, &on_bound).is_ok();
    let past_err = measure_err(&lens, &past_bound);
    println!("      AFTER : on_bound accepted={on_ok}  past_bound refused={past_err:?}");
    f.check(
        "the bound is closed at +-1.0 and the next representable step is refused",
        (on_ok, past_err.is_some()),
        (true, true),
    );

    // (b) an all-zero record: every field is legal and in range, but the sum is
    //     the zero vector, which has no direction to normalize.
    let all_zero = serde_json::json!({ "a": 0.0, "b": 0.0 });
    println!("  (b) BEFORE: all_zero={all_zero}");
    let zero_err = measure_err(&lens, &all_zero);
    println!("      AFTER : refused={zero_err:?}");
    f.check(
        "an all-zero record is refused rather than returning a direction it does not have",
        zero_err.is_some(),
        true,
    );

    // (c) a record with no numeric fields at all.
    let no_numbers = serde_json::json!({ "app": "notepad", "ok": true });
    println!("  (c) BEFORE: no_numbers={no_numbers}");
    let none_err = measure_err(&lens, &no_numbers);
    println!("      AFTER : refused={none_err:?}");
    f.check(
        "a record with no numeric field is refused",
        none_err
            .as_ref()
            .is_some_and(|e| e.contains("no numeric fields")),
        true,
    );

    // ------------------------------------------------------------------
    // Part 4 — the same real corpus, through the rebuilt lens
    // ------------------------------------------------------------------
    println!("\n== Part 4: every CF_EPISODES row re-measured through the real ingest path ==");
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let raw_rows = db.scan_cf(synapse_storage::cf::CF_EPISODES)?;
    println!("  CF_EPISODES rows = {}", raw_rows.len());
    let mut new_vectors = Vec::with_capacity(raw_rows.len());
    let mut decode_failures = 0usize;
    for (key, raw) in &raw_rows {
        let Ok(record) = serde_json::from_slice::<EpisodeRecord>(raw) else {
            decode_failures += 1;
            continue;
        };
        let cx = build_episode_constellation(context()?, key, raw, &record)?;
        match dense_slot(&cx, SLOT_NEW) {
            Some(vector) => new_vectors.push(vector),
            None => decode_failures += 1,
        }
    }
    println!(
        "  measured = {} unreadable = {decode_failures}",
        new_vectors.len()
    );
    f.check(
        "every readable episode row produced a slot-113 vector",
        decode_failures,
        0usize,
    );
    if new_vectors.len() >= 2 {
        let sims = nearest_neighbour_sims(&new_vectors);
        let (distinct, share, min, max) = discrimination(&sims);
        println!(
            "  nearest-neighbour cosine: distinct={distinct} modal_share={share:.6} range=[{min:.6},{max:.6}]"
        );
        f.check(
            "the rebuilt lens grades the corpus the superseded one tied",
            (distinct > 1, share < 1.0),
            (true, true),
        );
    } else {
        f.0.push("fewer than 2 episode rows; part 4 proves nothing".to_owned());
    }

    // ------------------------------------------------------------------
    // Part 5 — the admission gate
    // ------------------------------------------------------------------
    println!(
        "\n== Part 5: a legacy record vector is refused on a panel that is not grandfathered =="
    );
    let mut registry = Registry::new();
    let legacy = RegistryAlgorithmicLens::syn_record_vector(
        "fsv.probe.legacy_record_vector.v1",
        Modality::Structured,
        u32::try_from(RECORD_VECTOR_DIM)?,
    );
    let gate = syn_content_slot(
        SlotId::new(SLOT_NEW),
        "fsv.probe.legacy_record_vector.v1",
        legacy,
        SYN_EPISODE_PANEL_VERSION,
        &mut registry,
    );
    println!("  panel {SYN_EPISODE_PANEL_VERSION} + syn_record_vector -> {gate:?}");
    f.check(
        "the gate refuses with CALYX_PANEL_RECORD_VECTOR_MAGNITUDE_WEIGHTED",
        gate.as_ref()
            .err()
            .map(|e| format!("{e}").contains("CALYX_PANEL_RECORD_VECTOR_MAGNITUDE_WEIGHTED")),
        Some(true),
    );

    // ------------------------------------------------------------------
    // Part 6 — the bytes on disk
    // ------------------------------------------------------------------
    println!("\n== Part 6: cf/slot_{SLOT_NEW} written to, and read back out of, the vault ==");
    let slot_dir = vault_dir.join("cf").join(format!("slot_{SLOT_NEW}"));
    let existed_before = slot_dir.is_dir();
    let files_before = count_files(&slot_dir);
    let active_before = active_records(&db, SYN_EPISODE_PANEL_VERSION)?;
    let superseded_before = active_records(&db, SYN_EPISODE_PANEL_VERSION_PRE_1964)?;
    println!(
        "  BEFORE: cf/slot_{SLOT_NEW} exists={existed_before} files={files_before}  \
         panel {SYN_EPISODE_PANEL_VERSION} records={active_before}  \
         superseded {SYN_EPISODE_PANEL_VERSION_PRE_1964} records={superseded_before}"
    );

    let report =
        db.backfill_temporal_metadata(synapse_storage::cf::CF_EPISODES, None, None, BACKFILL_ROWS)?;
    println!(
        "  backfill: examined={} inserted={} backfilled={} already_current={}",
        report.examined_rows,
        report.inserted_rows,
        report.backfilled_rows,
        report.already_current_rows
    );
    let written = report.inserted_rows + report.backfilled_rows;
    f.check("the backfill wrote rows", written > 0, true);

    let active_after = active_records(&db, SYN_EPISODE_PANEL_VERSION)?;
    // `flush()` only syncs the WAL; an SST for a brand-new column family is
    // materialized by the checkpoint the close path runs, so counting files
    // before the close would race the flush scheduler.
    db.flush()?;
    drop(db);
    let files_after = count_files(&slot_dir);
    println!(
        "  AFTER : cf/slot_{SLOT_NEW} exists={} files={files_after}  \
         panel {SYN_EPISODE_PANEL_VERSION} records={active_after}",
        slot_dir.is_dir()
    );
    f.check(
        "cf/slot_113 did not exist before the write",
        (existed_before, files_before),
        (false, 0usize),
    );
    f.check(
        "cf/slot_113 now exists on disk with at least one file",
        slot_dir.is_dir() && files_after > 0,
        true,
    );
    f.check(
        "panel coverage grew by exactly the rows written",
        active_after.saturating_sub(active_before) as u64,
        written,
    );

    // The independent read: writer closed, reopen read-only, pull slot 113's
    // stored bytes out of its own column family.
    let stored_new_raw = read_slot_rows(&parent, SLOT_NEW)?;
    println!(
        "  cf/slot_{SLOT_NEW} rows on disk = {}",
        stored_new_raw.len()
    );
    f.check(
        "at least the backfilled rows are physically present",
        stored_new_raw.len() as u64 >= written,
        true,
    );

    let mut dense_rows = 0usize;
    let mut bad = Vec::new();
    let mut shown = false;
    for (key, value) in &stored_new_raw {
        match decode_slot_vector(value)? {
            SlotVector::Dense { dim, data } => {
                dense_rows += 1;
                if !shown {
                    println!("  first stored row key={} dim={dim}", hex(key));
                    println!("  first stored row data={data:?}");
                    shown = true;
                }
                if dim as usize != RECORD_VECTOR_DIM || (norm(&data) - 1.0).abs() > 1.0e-3 {
                    bad.push(format!("{}: dim={dim} norm={}", hex(key), norm(&data)));
                }
            }
            other => bad.push(format!("{}: not dense: {other:?}", hex(key))),
        }
    }
    f.check(
        "every stored slot-113 row is a unit-norm dense 64-vector",
        (dense_rows, bad.as_slice()),
        (stored_new_raw.len(), &[] as &[String]),
    );

    // And the exact bytes: re-measure one source row and compare against what
    // the vault actually holds.
    let (key, raw) = raw_rows
        .first()
        .cloned()
        .ok_or("no CF_EPISODES row available for the byte comparison")?;
    let record: EpisodeRecord = serde_json::from_slice(&raw)?;
    let expected = dense_slot(
        &build_episode_constellation(context()?, &key, &raw, &record)?,
        SLOT_NEW,
    )
    .ok_or("slot 113 absent from the re-measured row")?;
    println!("  re-measured first row's slot 113 = {expected:?}");
    let matched = stored_new_raw.iter().any(|(_, value)| {
        matches!(decode_slot_vector(value), Ok(SlotVector::Dense { data, .. }) if data == expected)
    });
    f.check(
        "the re-measured vector is byte-identical to one stored on disk",
        matched,
        true,
    );

    finish(&f)
}

// ----------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------

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

fn measure_dense(
    lens: &RegistryAlgorithmicLens,
    value: &serde_json::Value,
) -> Result<Vec<f32>, Box<dyn Error>> {
    let bytes = serde_json::to_vec(value)?;
    match lens.measure(&Input::new(Modality::Structured, bytes))? {
        SlotVector::Dense { data, .. } => Ok(data),
        other => Err(format!("expected a dense vector, got {other:?}").into()),
    }
}

fn measure_err(lens: &RegistryAlgorithmicLens, value: &serde_json::Value) -> Option<String> {
    let bytes = serde_json::to_vec(value).ok()?;
    lens.measure(&Input::new(Modality::Structured, bytes))
        .err()
        .map(|error| format!("{error}"))
}

/// One raw column-family row: the key and the encoded value, exactly as stored.
type CfRow = (Vec<u8>, Vec<u8>);

fn read_slot_rows(parent: &std::path::Path, slot: u16) -> Result<Vec<CfRow>, Box<dyn Error>> {
    let cf = ColumnFamily::slot(SlotId::new(slot));
    let vault = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        synapse_calyx::SynapseCalyxConfig {
            vault_dir: parent.join("db-daemon"),
            machine_salt_path: parent.join("machine-salt.bin"),
            tuning: synapse_calyx::SynapseCalyxTuningConfig::default().validate()?,
        },
        Some(vec![cf]),
    )?;
    Ok(vault.scan_cf_latest(cf)?)
}

fn read_slot_vectors(parent: &std::path::Path, slot: u16) -> Result<Vec<Vec<f32>>, Box<dyn Error>> {
    let mut out = Vec::new();
    for (_, value) in read_slot_rows(parent, slot)? {
        if let SlotVector::Dense { data, .. } = decode_slot_vector(&value)? {
            out.push(data);
        }
    }
    Ok(out)
}

fn dense_slot(cx: &Constellation, slot: u16) -> Option<Vec<f32>> {
    match cx.slots.get(&SlotId::new(slot)) {
        Some(SlotVector::Dense { data, .. }) => Some(data.clone()),
        _ => None,
    }
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::NAN;
    }
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
                let s = cosine(a, b);
                if s > best {
                    best = s;
                }
            }
        }
        if best.is_finite() {
            sims.push(best);
        }
    }
    sims
}

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
        Err("episode_record_vector_fsv failed".into())
    }
}
