//! Manual FSV instrument for #1907: does free-text recall over a *stale* search
//! generation silently return zero hits?
//!
//! ## What the live delta actually said
//!
//! On the live vault the timeline generation sat at `base_seq=84517` with
//! `distinct_changed=454` — and `454` is not "rows written since the base seq",
//! it is **every row the panel holds**. The per-slot composition proves it:
//!
//! ```text
//!   slot_1=90 slot_2=90 slot_3=90 slot_4=90 slot_103=454   base_panel=454
//! ```
//!
//! 90 rows were fully ingested after the base seq; the other 364 had *only*
//! slot 103 and their `Base` row rewritten. That is the signature of the #1900
//! slot-103 backfill: `put_slot_vector` writes the slot row **and** the row's
//! `Base` row in one batch, because the `Base` slot hash is the vector's
//! integrity record (#1888). A whole-panel backfill therefore marks the whole
//! panel changed, so **every posting in the index belongs to `changed`** and
//! the reconciled scorers mask all of them. Whether recall survives depends
//! entirely on the *replacements* half of the reconciliation.
//!
//! This driver builds that exact condition — a qualified slot write over every
//! indexed row — at a size where the answer is known by construction.
//!
//! ## The known answer
//!
//! Ten timeline rows are ingested through the real public builder. Nine are
//! decoys whose titles share no term with the probe; **one** row's title
//! contains the coined term `synfsv1907zebra`, which occurs in no other row.
//!
//! ```text
//!   probe: by_text "synfsv1907zebra"  ->  MUST return exactly that one row
//! ```
//!
//! asserted three times against the same vault:
//!
//! | phase | vault state | expected |
//! |---|---|---|
//! | A | generation freshly built, nothing written after it | 1 hit, the marked row |
//! | B | every indexed row re-written by a qualified slot write | 1 hit, the marked row |
//! | C | generation rebuilt on top of B | 1 hit, the marked row |
//!
//! A is the control: it proves the corpus, the lens and the probe all work, so
//! a zero in B cannot be blamed on "nothing matched". C is the counter-control:
//! it proves the rows are still there and still findable, so a zero in B cannot
//! be blamed on the rewrite having destroyed them. **B is the experiment.**
//!
//! The engine's own trace is printed for every phase, so the stage that drops
//! the hit is named rather than inferred: `delta.scan.scoped` gives
//! `distinct_changed`, `search_slot.delta.start` gives `changed=` and
//! `replacements=`, and `search_slot.delta.done` gives the surviving hit count.
//!
//! Source of Truth is the vault and the sidecar bytes: the marked row's `Base`
//! row and its `cf/slot_103` vector are read back directly, and the persisted
//! sparse sidecar is parsed to prove it physically holds a posting in the cell
//! the probe measures. No claim rests on a return value from a write call.
//!
//! Usage:
//! `cargo run -p synapse-storage --example reconciled_recall_fsv -- <empty-scratch-dir>`

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

use calyx_aster::cf::{ColumnFamily, base_key};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{CxId, PanelSlotId, SlotId, SlotVector, VaultId, VaultStore as _};
use calyx_registry::VaultPanelState;
use calyx_search::{
    FusionChoice, FusionTuning, GuardChoice, SearchBudget, SearchFreshness, SearchTraceEvent,
};
use serde_json::json;
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_VERSION, build_timeline_constellation,
    syn_active_panel_contract,
};

/// A term coined for this instrument. It must occur in exactly one row, so the
/// probe has a single correct answer that no decoy can satisfy.
const MARKER: &str = "synfsv1907zebra";
const ROWS: u8 = 10;
const MARKED_ROW: u8 = 4;
const TEXT_SLOT: u16 = 103;
const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const CREATED_AT_MS: u64 = 1_785_000_000_000;
const LEASE_MS: u64 = 30_000;

/// Title for row `tag`. Exactly one row carries [`MARKER`]; every other title is
/// built from terms that do not appear in the probe.
fn title(tag: u8) -> String {
    if tag == MARKED_ROW {
        format!("{MARKER} quarterly ledger review - Notepad")
    } else {
        format!("decoy window {tag} spreadsheet budget - Calc")
    }
}

fn row(vault_id: VaultId, tag: u8) -> Result<calyx_core::Constellation, Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: 1_785_000_000_000_000_000 + u64::from(tag) * 1_000_000_000,
        kind: TimelineKind::TitleChange,
        actor: TimelineActor::Human,
        app: Some(if tag == MARKED_ROW {
            "Notepad.exe".to_owned()
        } else {
            "Calc.exe".to_owned()
        }),
        payload: json!({ "title": title(tag) }),
    };
    let raw = serde_json::to_vec(&record)?;
    let context = NativeConstellationContext {
        vault_id,
        // Re-derived content-addressed by the builder; this seed only has to be
        // distinct per row.
        cx_id: CxId::from_bytes([0x19, 0x07, tag, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        created_at_ms: CREATED_AT_MS + u64::from(tag),
        next_ledger_seq: u64::from(tag) + 1,
    };
    let key = format!("fsv-1907/timeline-{tag}");
    Ok(build_timeline_constellation(
        context,
        key.as_bytes(),
        &raw,
        &record,
    )?)
}

/// Reads the persisted sparse sidecar the generation names and reports what it
/// physically holds for the probe's measured cells — the bytes-level Source of
/// Truth behind "the index does have a posting for this query".
fn report_sidecar(vault_dir: &Path, query_cells: &[u32]) -> Result<(), Box<dyn Error>> {
    let dir = vault_dir
        .join("idx/search")
        .join(format!("panel_{SYN_TIMELINE_PANEL_VERSION:010}"));
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
    let Some(path) = sidecar else {
        println!(
            "  SIDECAR none found for slot {TEXT_SLOT} under {}",
            dir.display()
        );
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let rows = value["rows"].as_array().map_or(0, Vec::len);
    let postings = &value["postings"];
    println!(
        "  SIDECAR {} rows={rows} scoring={} dim={}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
        value["scoring"],
        value["dim"]
    );
    for cell in query_cells {
        let posted = postings
            .get(cell.to_string())
            .and_then(|v| v.as_array())
            .map_or(0, Vec::len);
        println!("  SIDECAR cell {cell} rows_posted_here={posted}");
    }
    Ok(())
}

/// Runs the probe through the real production engine and prints every trace
/// event, so an empty result is attributed to a named stage rather than guessed.
fn probe(
    vault: &AsterVault,
    vault_dir: &Path,
    state: &VaultPanelState,
    phase: &str,
    expected: CxId,
) -> Result<bool, Box<dyn Error>> {
    let hits = run_query(vault, vault_dir, state, phase, MARKER, true)?;
    let matched = hits.len() == 1 && hits[0].0 == expected;
    println!(
        "  EXPECTED hits=1 cx_id={expected}\n  OBSERVED hits={} {}",
        hits.len(),
        if matched { "MATCH" } else { "MISMATCH" }
    );
    Ok(matched)
}

/// One query through the production engine, returning `(cx_id, score)` per hit.
fn run_query(
    vault: &AsterVault,
    vault_dir: &Path,
    state: &VaultPanelState,
    phase: &str,
    text: &str,
    show_trace: bool,
) -> Result<Vec<(CxId, f32)>, Box<dyn Error>> {
    println!("\n--- phase {phase}: by_text \"{text}\" ---");
    let allowed = BTreeSet::from([SlotId::new(TEXT_SLOT)]);
    let query_vectors =
        calyx_search::measure_query_vectors_with_slots(state, text, Some(&allowed))?;
    let cells = query_vectors
        .iter()
        .flat_map(|(_, vector)| match vector {
            SlotVector::Sparse { entries, .. } => entries.iter().map(|e| e.idx).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    println!(
        "  measured query slots={} cells={cells:?}",
        query_vectors.len()
    );
    report_sidecar(vault_dir, &cells)?;

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
    if show_trace {
        for event in &trace {
            println!("  TRACE {event:?}");
        }
    } else {
        for event in trace
            .iter()
            .filter(|event| event.phase.starts_with("delta.") || event.phase.contains("delta."))
        {
            println!("  TRACE {event:?}");
        }
    }
    for hit in &outcome.hits {
        println!("  hit cx_id={} score={}", hit.cx_id, hit.score);
    }
    Ok(outcome
        .hits
        .iter()
        .map(|hit| (hit.cx_id, hit.score))
        .collect())
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: reconciled_recall_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    println!("reconciled_recall_fsv: vault_dir={}", dir.display());
    println!("marker term = {MARKER} (occurs in exactly 1 of {ROWS} rows)");

    let vault_id: VaultId = VAULT_ID.parse()?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"reconciled-recall-fsv".to_vec(),
        VaultOptions::default(),
    )?;
    let contract = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, CREATED_AT_MS)?
        .ok_or("no built-in contract for the timeline panel version")?;
    let state = VaultPanelState {
        panel: contract.panel,
        registry: contract.registry,
        registry_snapshot: None,
    };

    // --- ingest the corpus -------------------------------------------------
    let mut marked = None;
    for tag in 1..=ROWS {
        let constellation = row(vault_id, tag)?;
        let cx_id = constellation.cx_id;
        if tag == MARKED_ROW {
            marked = Some(cx_id);
        }
        vault.put(constellation)?;
    }
    let marked = marked.ok_or("marked row was never built")?;
    println!(
        "ingested {ROWS} rows at seq={}; marked cx_id={marked} title=\"{}\"",
        vault.latest_seq(),
        title(MARKED_ROW)
    );

    // Source of Truth: the marked row and its lexical vector are physically in
    // the vault before any search claim is made about them.
    let base_present = vault
        .read_cf_at(vault.latest_seq(), ColumnFamily::Base, &base_key(marked))?
        .is_some();
    let stored_vector =
        vault.read_slot_vector_at(vault.latest_seq(), marked, SlotId::new(TEXT_SLOT))?;
    println!(
        "SoT base_present={base_present} slot_{TEXT_SLOT}_vector={}",
        match &stored_vector {
            Some(SlotVector::Sparse { dim, entries }) =>
                format!("Sparse(dim={dim}, nnz={})", entries.len()),
            Some(other) => format!("{other:?}"),
            None => "ABSENT".to_owned(),
        }
    );

    calyx_search::rebuild_for_vault_with_panel_state(&dir, &vault, &state)?;
    println!("built generation at seq={}", vault.latest_seq());

    let phase_a = probe(&vault, &dir, &state, "A (fresh generation)", marked)?;

    // --- phase B: the #1907 condition --------------------------------------
    // A qualified slot write over EVERY indexed row: the same shape the #1900
    // whole-panel backfill left behind. Each write rewrites the row's `Base`
    // row alongside its slot row (#1888), so every row lands in `changed`.
    println!("\n=== qualified slot write over all {ROWS} indexed rows (the backfill shape) ===");
    if !state
        .panel
        .slots
        .iter()
        .any(|slot| slot.slot_id.get() == TEXT_SLOT)
    {
        return Err("timeline panel declares no slot 103".into());
    }
    let panel_slot = PanelSlotId::new(SYN_TIMELINE_PANEL_VERSION, SlotId::new(TEXT_SLOT));
    for tag in 1..=ROWS {
        let cx_id = row(vault_id, tag)?.cx_id;
        // Re-write the identical measured vector: the point is that the row
        // changed, not that its content did.
        let vector = vault
            .read_slot_vector_at(vault.latest_seq(), cx_id, SlotId::new(TEXT_SLOT))?
            .ok_or("indexed row has no stored lexical vector to re-write")?;
        vault.put_slot_vector(cx_id, panel_slot, &vector)?;
    }
    println!("latest_seq={}", vault.latest_seq());

    let snapshot = vault.pin_reader(calyx_aster::mvcc::Freshness::FreshDerived, LEASE_MS);
    let composition = calyx_search::measure_panel_delta(
        &vault,
        snapshot,
        SYN_TIMELINE_PANEL_VERSION,
        // The generation's base seq is what the rebuild pinned: the seq before
        // the rewrites began.
        u64::from(ROWS),
        [SlotId::new(TEXT_SLOT)],
    )?;
    let _ = vault.release_reader(snapshot.lease().id());
    println!("delta: {}", composition.composition());

    let phase_b = probe(&vault, &dir, &state, "B (STALE generation)", marked)?;

    // --- phase C: the counter-control --------------------------------------
    calyx_search::rebuild_for_vault_with_panel_state(&dir, &vault, &state)?;
    println!("\nrebuilt generation at seq={}", vault.latest_seq());
    let phase_c = probe(&vault, &dir, &state, "C (rebuilt on top of B)", marked)?;

    let edges = edge_cases(&vault, &dir, &state, vault_id, marked)?;

    println!("\n================= VERDICT =================");
    println!("  A fresh generation      recall_correct={phase_a}");
    println!("  B stale generation      recall_correct={phase_b}   <-- the experiment");
    println!("  C rebuilt generation    recall_correct={phase_c}");
    for (name, ok) in &edges {
        println!("  {name:<38} correct={ok}");
    }
    let edges_ok = edges.iter().all(|(_, ok)| *ok);
    if phase_a && phase_c && !phase_b {
        println!("  #1907 REPRODUCED: reconciled recall drops the hit the index provably holds");
    } else if phase_a && phase_b && phase_c && edges_ok {
        println!(
            "  #1907 FIXED: reconciled recall is correct on the happy path and all edge cases"
        );
    } else {
        println!("  INCONCLUSIVE / REGRESSED: see the failing line above");
    }
    Ok(())
}

/// Boundary audit. Each case prints the vault state before and after the action
/// that creates it, then the recall the engine actually produced.
///
/// These exist because the #1907 fix installs a fail-closed conservation check
/// on the reconciliation path, and a guard on a hot recall path is only worth
/// having if it stays silent on every healthy state. D and F are the two states
/// a cruder "replacements are empty" test would have false-closed on; E is the
/// half of reconciliation that was equally broken and is easy to forget, because
/// its symptom is a *missing* row rather than an empty result.
fn edge_cases(
    vault: &AsterVault,
    vault_dir: &Path,
    state: &VaultPanelState,
    vault_id: VaultId,
    marked: CxId,
) -> Result<Vec<(String, bool)>, Box<dyn Error>> {
    let mut results = Vec::new();
    println!("\n\n================= EDGE CASES =================");

    // --- D: a genuine miss on a stale generation is still an empty result ---
    // The generation must be stale for the reconciliation path to run at all,
    // so one more qualified slot write puts it there.
    let panel_slot = PanelSlotId::new(SYN_TIMELINE_PANEL_VERSION, SlotId::new(TEXT_SLOT));
    let vector = vault
        .read_slot_vector_at(vault.latest_seq(), marked, SlotId::new(TEXT_SLOT))?
        .ok_or("marked row lost its lexical vector")?;
    println!("\n[D] BEFORE latest_seq={}", vault.latest_seq());
    vault.put_slot_vector(marked, panel_slot, &vector)?;
    println!(
        "[D] AFTER  latest_seq={} (generation is now stale)",
        vault.latest_seq()
    );
    let absent_term = "synfsv1907noSuchTermAnywhere";
    let hits = run_query(
        vault,
        vault_dir,
        state,
        "D (genuine miss, stale generation)",
        absent_term,
        false,
    )?;
    let d_ok = hits.is_empty();
    println!(
        "  EXPECTED hits=0 and NO error (a miss is a miss, #1896)\n  OBSERVED hits={} {}",
        hits.len(),
        if d_ok { "OK" } else { "WRONG" }
    );
    results.push(("D genuine miss returns empty, no error".to_owned(), d_ok));

    // --- E: a row ingested AFTER the generation must be findable ------------
    // This is the additive half of reconciliation. Under the #1907 defect it
    // failed silently in the opposite direction: the row was simply absent from
    // results, with the rest of the corpus still ranking normally.
    let new_marker = "synfsv1907quokka";
    let new_row = {
        let record = TimelineRecord {
            record_version: 1,
            ts_ns: 1_785_000_000_000_000_000 + 99 * 1_000_000_000,
            kind: TimelineKind::TitleChange,
            actor: TimelineActor::Human,
            app: Some("Notepad.exe".to_owned()),
            payload: json!({ "title": format!("{new_marker} arrived after the generation") }),
        };
        let raw = serde_json::to_vec(&record)?;
        build_timeline_constellation(
            NativeConstellationContext {
                vault_id,
                cx_id: CxId::from_bytes([0x19, 0x07, 99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                created_at_ms: CREATED_AT_MS + 99,
                next_ledger_seq: 99,
            },
            b"fsv-1907/timeline-99",
            &raw,
            &record,
        )?
    };
    let new_id = new_row.cx_id;
    let before = run_query(
        vault,
        vault_dir,
        state,
        "E-before (row not yet written)",
        new_marker,
        false,
    )?;
    println!(
        "[E] BEFORE hits={} latest_seq={}",
        before.len(),
        vault.latest_seq()
    );
    vault.put(new_row)?;
    println!(
        "[E] AFTER  wrote cx_id={new_id} latest_seq={}",
        vault.latest_seq()
    );
    let after = run_query(
        vault,
        vault_dir,
        state,
        "E (new row, stale generation)",
        new_marker,
        false,
    )?;
    let e_ok = before.is_empty() && after.len() == 1 && after[0].0 == new_id;
    println!(
        "  EXPECTED before=0 after=1 cx_id={new_id}\n  OBSERVED before={} after={} {}",
        before.len(),
        after.len(),
        if e_ok { "OK" } else { "WRONG" }
    );
    results.push(("E post-generation row is findable".to_owned(), e_ok));

    // --- F: an erased row disappears, masked with nothing to replace it -----
    // The reconciliation masks a tombstoned row and has no replacement for it by
    // design. `declared` never counts it (the collector skips a row with no
    // visible Base), so the conservation check must stay silent and the correct
    // answer here is an empty result, not an error.
    let before = run_query(
        vault,
        vault_dir,
        state,
        "F-before (row still live)",
        MARKER,
        false,
    )?;
    let base_before = vault
        .read_cf_at(vault.latest_seq(), ColumnFamily::Base, &base_key(marked))?
        .is_some();
    println!(
        "[F] BEFORE base_row_present={base_before} hits={}",
        before.len()
    );
    let erased = vault.erase_scope_ledger_stamped(
        calyx_aster::erase::EraseScope::Cx(marked),
        &calyx_aster::erase::EraseRegistry::new(),
    )?;
    let base_after = vault
        .read_cf_at(vault.latest_seq(), ColumnFamily::Base, &base_key(marked))?
        .is_some();
    println!(
        "[F] AFTER  erased records_deleted={} base_row_present={base_after} latest_seq={}",
        erased.records_deleted,
        vault.latest_seq()
    );
    let after = run_query(
        vault,
        vault_dir,
        state,
        "F (erased row, stale generation)",
        MARKER,
        false,
    )?;
    let f_ok = before.len() == 1 && !base_before.eq(&false) && !base_after && after.is_empty();
    println!(
        "  EXPECTED before=1 after=0, base row gone, NO error\n  OBSERVED before={} after={} base_after={base_after} {}",
        before.len(),
        after.len(),
        if f_ok { "OK" } else { "WRONG" }
    );
    results.push(("F erased row vanishes, no error".to_owned(), f_ok));

    Ok(results)
}
