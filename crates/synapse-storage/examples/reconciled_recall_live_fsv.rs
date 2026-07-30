//! Manual FSV instrument for #1907 against a **copy of the live vault**.
//!
//! `reconciled_recall_fsv` proves the defect and the fix on a corpus this
//! instrument builds itself. That is the right shape for a known answer, but it
//! is a corpus of ten rows with a coined term, and the live symptom appeared on
//! a real timeline panel with a real vocabulary and a real IDF distribution.
//! This driver closes that gap: it runs the same experiment on the actual live
//! rows, taken through the daemon's own consistent `storage operation=backup`
//! so the copy is a real snapshot rather than a hot file copy.
//!
//! ## The known answer is measured, not assumed
//!
//! On real data no term can be declared "the one correct hit" up front. So the
//! ground truth is established by measurement, in this order:
//!
//! 1. rebuild the generation, so the index is exactly current;
//! 2. run the probe against it — a **static, un-reconciled** search over a
//!    current index. Whatever it returns *is* the correct answer, by
//!    construction: there is no delta and nothing to reconcile;
//! 3. make the generation stale by re-writing every indexed row's lexical slot,
//!    the same qualified slot write the #1900 backfill performed;
//! 4. run the identical probe again, now through the delta-reconciliation path.
//!
//! Step 4 must reproduce step 2 **exactly** — same `cx_id`s, same order, and the
//! same scores, because the corpus content did not change, only its sequence
//! numbers. A score that matches to the bit is a much stronger claim than "some
//! hits came back": it proves the reconciled scorer rebuilt BM25's `N`, `avgdl`
//! and per-term `df` from the delta to the same values the index holds.
//!
//! Source of Truth is the copied vault's own bytes: the sidecar the generation
//! names is parsed for its posting counts, and every hit is compared against the
//! static readback of the same query.
//!
//! Usage:
//! `cargo run -p synapse-storage --example reconciled_recall_live_fsv -- <backup-vault-dir> <salt-file> <vault-id> [probe-text]`

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{CxId, PanelSlotId, SlotId, VaultId};
use calyx_registry::{VaultPanelState, load_vault_panel_state};
use calyx_search::{
    FusionChoice, FusionTuning, GuardChoice, SearchBudget, SearchFreshness, SearchTraceEvent,
};

/// One probe's ranked result: the hit ids in rank order with their scores.
type ProbeHits = Vec<(CxId, f32)>;

const TEXT_SLOT: u16 = 103;
const DEFAULT_PROBE: &str = "Notepad";

/// One query through the production engine.
fn run_query(
    vault: &AsterVault,
    vault_dir: &Path,
    state: &VaultPanelState,
    label: &str,
    text: &str,
) -> Result<ProbeHits, Box<dyn Error>> {
    let allowed = BTreeSet::from([SlotId::new(TEXT_SLOT)]);
    let query_vectors =
        calyx_search::measure_query_vectors_with_slots(state, text, Some(&allowed))?;
    let mut trace = Vec::new();
    let mut sink = |event: SearchTraceEvent| trace.push(event);
    let outcome = calyx_search::search_outcome_with_query_vectors_freshness(
        vault,
        vault_dir,
        &state.panel,
        &query_vectors,
        5,
        FusionChoice::SingleLensSlot(SlotId::new(TEXT_SLOT)),
        GuardChoice::Off,
        None,
        true,
        SearchFreshness::Fresh,
        SearchBudget::disabled(),
        FusionTuning::default(),
        Some(&mut sink),
    )?;
    let hits: ProbeHits = outcome
        .hits
        .iter()
        .map(|hit| (hit.cx_id, hit.score))
        .collect();
    println!("\n[{label}] by_text \"{text}\" -> hits={}", hits.len());
    for (rank, (cx_id, score)) in hits.iter().enumerate() {
        println!("  rank {} cx_id={cx_id} score={score}", rank + 1);
    }
    for event in trace
        .iter()
        .filter(|event| event.phase.contains("delta") || event.phase.contains("search_slot"))
    {
        println!("  TRACE {} {:?}", event.phase, event.detail);
    }
    Ok(hits)
}

/// Parses the sidecar the generation names and reports its physical shape.
fn report_sidecar(vault_dir: &Path, panel_version: u32) -> Result<Vec<CxId>, Box<dyn Error>> {
    let dir = vault_dir
        .join("idx/search")
        .join(format!("panel_{panel_version:010}"));
    let mut sidecar = None;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with(&format!("slot_{TEXT_SLOT:05}_")) && name.ends_with(".sparse.json") {
            sidecar = Some(path);
        }
    }
    let path = sidecar.ok_or("no persisted sparse sidecar for the lexical slot")?;
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let rows = value["rows"].as_array().cloned().unwrap_or_default();
    let cells = value["postings"]
        .as_object()
        .map_or(0, serde_json::Map::len);
    println!(
        "SIDECAR {} rows={} distinct_posted_cells={cells} scoring={} dim={}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
        rows.len(),
        value["scoring"],
        value["dim"]
    );
    let mut ids = Vec::new();
    for row in &rows {
        if let Some(text) = row["cx_id"].as_str() {
            ids.push(text.parse::<CxId>()?);
        }
    }
    Ok(ids)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let vault_dir = PathBuf::from(
        args.next()
            .ok_or("usage: reconciled_recall_live_fsv <vault-dir> <salt-file> <vault-id> [text]")?,
    );
    let salt_path = PathBuf::from(args.next().ok_or("missing <salt-file>")?);
    let vault_id: VaultId = args.next().ok_or("missing <vault-id>")?.parse()?;
    let probe_text = args.next().unwrap_or_else(|| DEFAULT_PROBE.to_owned());

    println!(
        "reconciled_recall_live_fsv: vault_dir={}",
        vault_dir.display()
    );
    let salt = std::fs::read(&salt_path)?;
    println!(
        "salt={} ({} bytes) vault_id={vault_id}",
        salt_path.display(),
        salt.len()
    );

    let vault = AsterVault::open(&vault_dir, vault_id, salt, VaultOptions::default())?;
    let state = load_vault_panel_state(&vault_dir)?;
    let panel_version = state.panel.version;
    println!(
        "opened live copy: panel_version={panel_version} latest_seq={}",
        vault.latest_seq()
    );

    // --- step 1+2: current index, static search = the ground truth ----------
    calyx_search::rebuild_for_vault_with_panel_state(&vault_dir, &vault, &state)?;
    println!(
        "\n=== step 1: generation rebuilt at seq={} ===",
        vault.latest_seq()
    );
    let indexed = report_sidecar(&vault_dir, panel_version)?;
    println!("indexed rows in the lexical lane: {}", indexed.len());

    let truth = run_query(
        &vault,
        &vault_dir,
        &state,
        "step 2 GROUND TRUTH (static, current index)",
        &probe_text,
    )?;
    if truth.is_empty() {
        return Err(format!(
            "probe \"{probe_text}\" matches nothing even on a current index; pick a term that occurs in this corpus"
        )
        .into());
    }

    // --- step 3: make it stale exactly the way the backfill did -------------
    println!(
        "\n=== step 3: qualified slot write over all {} indexed rows ===",
        indexed.len()
    );
    let panel_slot = PanelSlotId::new(panel_version, SlotId::new(TEXT_SLOT));
    let before_seq = vault.latest_seq();
    let mut rewritten = 0_usize;
    let mut skipped = 0_usize;
    for cx_id in &indexed {
        match vault.read_slot_vector_at(vault.latest_seq(), *cx_id, SlotId::new(TEXT_SLOT))? {
            // Re-write the identical measured vector: the row must change, its
            // content must not, so the two searches remain comparable.
            Some(vector) => {
                vault.put_slot_vector(*cx_id, panel_slot, &vector)?;
                rewritten += 1;
            }
            None => skipped += 1,
        }
    }
    println!(
        "rewrote {rewritten} rows (skipped {skipped} with no stored vector); seq {before_seq} -> {}",
        vault.latest_seq()
    );

    let snapshot = vault.pin_reader(calyx_aster::mvcc::Freshness::FreshDerived, 60_000);
    let composition = calyx_search::measure_panel_delta(
        &vault,
        snapshot,
        panel_version,
        before_seq,
        [SlotId::new(TEXT_SLOT)],
    )?;
    let _ = vault.release_reader(snapshot.lease().id());
    println!("delta now: {}", composition.composition());

    // --- step 4: the identical probe, now reconciled ------------------------
    let reconciled = run_query(
        &vault,
        &vault_dir,
        &state,
        "step 4 RECONCILED (stale generation)",
        &probe_text,
    )?;

    // A genuine miss must still be an empty result on the same stale generation.
    let miss_term = "synfsv1907termthatcannotoccur";
    let miss = run_query(
        &vault,
        &vault_dir,
        &state,
        "edge: genuine miss on the stale generation",
        miss_term,
    )?;

    println!("\n================= VERDICT =================");
    let truth_ids: Vec<String> = truth.iter().map(|(id, _)| id.to_string()).collect();
    let recon_ids: Vec<String> = reconciled.iter().map(|(id, _)| id.to_string()).collect();
    println!("  ground truth hits : {}", truth_ids.join(","));
    println!("  reconciled hits   : {}", recon_ids.join(","));
    let same_order = truth_ids == recon_ids;
    let same_scores = truth.len() == reconciled.len()
        && truth
            .iter()
            .zip(&reconciled)
            .all(|((_, left), (_, right))| left.to_bits() == right.to_bits());
    let mut score_delta = BTreeMap::new();
    for ((id, left), (_, right)) in truth.iter().zip(&reconciled) {
        if left.to_bits() != right.to_bits() {
            score_delta.insert(id.to_string(), (*left, *right));
        }
    }
    println!("  identical cx_id order    = {same_order}");
    println!("  bit-identical scores     = {same_scores}");
    if !score_delta.is_empty() {
        println!("  score differences        = {score_delta:?}");
    }
    println!(
        "  genuine miss still empty = {} (hits={})",
        miss.is_empty(),
        miss.len()
    );
    if same_order && same_scores && miss.is_empty() {
        println!(
            "  PASS: reconciled recall over the live corpus reproduces the current index exactly"
        );
    } else {
        println!("  FAIL: reconciled recall diverges from the current index on real data");
    }
    Ok(())
}
