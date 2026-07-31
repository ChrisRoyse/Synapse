//! Manual FSV instrument: does grounding coverage tell the truth about a text
//! lane on records that have no text?
//!
//! ## Why this exists
//!
//! #1904 added `EP_SLOT_TITLE_BM25` (slot 108) to the episode panel. Verifying
//! it on the live vault produced this:
//!
//! ```text
//! episode segment -> 171 constellations inserted
//! hygiene grounding_gap panel_version=1904002
//!   slot 108  records_present=171  coverage_fraction=1.0
//! ```
//!
//! A *title* lane, reported as measured on 100% of episodes. But the same
//! episode listing shows many episodes with `distinct_title_count: 0` and no
//! `title_first` field at all — a silent window focus that never resolved a
//! title. There is nothing for a title lane to measure on those rows.
//!
//! ## The defect
//!
//! `episode_title_text` returns `""` when both title fields are absent, and the
//! sparse encoder measured over `""` produces `Sparse { entries: [] }`. That is
//! not `SlotVector::Absent`, so `grounding_gap`'s `is_absent` skip did not fire
//! and the row counted as covered.
//!
//! The rest of the system already reads that vector the *other* way.
//! `calyx-search`'s `field_doc_count` says so outright:
//!
//! > A row whose vector is empty is a document that does not have this field at
//! > all. It is retained in `rows` (the lane must know the row exists so a delta
//! > can mask it) but it is not a document the field's statistics describe.
//!
//! So BM25's `N` and `avgdl` were always correct — this is not a ranking bug.
//! It is a **readback** bug, and the same family as #1915: two different states
//! ("measured something" and "measured nothing") were indistinguishable in the
//! payload, and the operator-facing number was the optimistic one.
//!
//! That is also why the encoder is left alone. Emitting `Absent` instead would
//! contradict the search layer, which needs the empty row present so a delta can
//! mask it. The fix belongs where the lie was told.
//!
//! ## What this proves
//!
//! A synthetic episode corpus with a **known** titled/untitled split, put
//! through the production `build_episode_constellation`, then read back through
//! the production `grounding_gap_report`. The split is chosen by this file, so
//! the expected answer is known before the vault is opened.
//!
//! ```text
//! cargo run --release -p synapse-storage --example episode_title_coverage_fsv -- <empty-dir>
//! ```

use std::error::Error;
use std::path::PathBuf;

use calyx_core::{Anchor, AnchorKind, AnchorValue, CxId, SlotVector, VaultId};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxVault};
use synapse_core::types::{EpisodeBoundary, EpisodeRecord, TimelineActor};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_EPISODE_PANEL_VERSION, build_episode_constellation,
};

/// Episodes that DO carry a foreground title.
const TITLED: u32 = 12;
/// Episodes that do NOT — a silent focus that never resolved a title. This is
/// the real shape on the live vault, not an invented edge case.
const UNTITLED: u32 = 8;
const TOTAL: u32 = TITLED + UNTITLED;

/// `EP_SLOT_TITLE_BM25`. Private to `constellations.rs`, so it is restated here;
/// claim 1 fails loudly if the panel ever stops emitting it.
const SLOT_TITLE_BM25: u16 = 108;
/// `EP_SLOT_TITLE_SPARSE` — the normalized sibling. It reads the same text, so
/// it must show the same split. Two independent lanes agreeing is what makes
/// the number evidence rather than a single lane's quirk.
const SLOT_TITLE_SPARSE: u16 = 11;
/// `EP_SLOT_START_HOUR_CYCLIC` — a dense lane derived from the timestamp, which
/// every episode has. It is the control: it must stay at full coverage, proving
/// the fix narrowed the right thing instead of deflating every slot.
const SLOT_START_HOUR_CYCLIC: u16 = 15;

const ANCHOR_KIND: &str = "synapse:episode_segmentation_outcome";

/// Deterministic synthetic episode. `index < TITLED` gets a distinct real title;
/// the rest get none at all, exactly like a silent focus row.
fn episode(index: u32) -> EpisodeRecord {
    let titled = index < TITLED;
    let start_ts_ns = 1_785_000_000_000_000_000_u64 + u64::from(index) * 60_000_000_000;
    // Distinct titles so the lane cannot pass by measuring one repeated string,
    // and a shared word ("synapse") so the corpus has real term overlap.
    let title = titled.then(|| format!("synapse episode {index} - Visual Studio Code"));
    EpisodeRecord {
        record_version: 1,
        ts_ns: start_ts_ns,
        episode_id: format!("ep1-fsv{index:013x}"),
        start_ts_ns,
        end_ts_ns: start_ts_ns + 30_000_000_000,
        actor: TimelineActor::Human,
        app: Some("code.exe".to_owned()),
        document: title.clone(),
        url: None,
        title_first: title.clone(),
        title_last: title,
        distinct_title_count: u32::from(titled),
        row_count: 3,
        keystroke_count: u64::from(index),
        click_count: 1,
        interruption_count: 0,
        interrupted_ms: 0,
        started_because: EpisodeBoundary::AppSwitch,
        ended_because: EpisodeBoundary::IdleGap,
    }
}

fn entry_count(vector: &SlotVector) -> Option<usize> {
    match vector {
        SlotVector::Sparse { entries, .. } => Some(entries.len()),
        _ => None,
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: episode_title_coverage_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    println!("episode_title_coverage_fsv: vault_dir={}", dir.display());
    println!("SYNTHETIC CORPUS: {TOTAL} episodes = {TITLED} titled + {UNTITLED} untitled");
    println!(
        "  the split is chosen HERE, so the expected coverage is known before the vault opens"
    );

    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    let vault_id: VaultId = vault.vault_id_value();
    println!("  vault_id={vault_id}");

    // --- measure through the PRODUCTION builder ----------------------------
    let mut measured_titled_entries = 0usize;
    let mut measured_untitled_entries = 0usize;
    let mut slot_108_seen = 0usize;

    for index in 0..TOTAL {
        let record = episode(index);
        let raw_bytes = serde_json::to_vec(&record)?;
        let source_key = record.episode_id.as_bytes().to_vec();
        let context = NativeConstellationContext {
            vault_id,
            cx_id: CxId::from_input(
                &raw_bytes,
                SYN_EPISODE_PANEL_VERSION,
                &vault_id.as_ulid().to_bytes(),
            ),
            created_at_ms: record.start_ts_ns / 1_000_000,
            next_ledger_seq: u64::from(index) + 1,
        };
        let mut constellation =
            build_episode_constellation(context, &source_key, &raw_bytes, &record)?;

        // Every episode carries a grounded anchor, so `grounded_records` cannot
        // be what separates the two groups — only the measurement can.
        constellation.anchors.push(Anchor {
            kind: AnchorKind::Label(ANCHOR_KIND.to_owned()),
            value: AnchorValue::Bool(index % 2 == 0),
            source: "episode_title_coverage_fsv".to_owned(),
            observed_at: context.created_at_ms,
            confidence: 1.0,
        });

        if let Some(vector) = constellation
            .slots
            .get(&calyx_core::SlotId::new(SLOT_TITLE_BM25))
        {
            slot_108_seen += 1;
            let entries = entry_count(vector).unwrap_or(usize::MAX);
            if index < TITLED {
                measured_titled_entries += entries;
            } else {
                measured_untitled_entries += entries;
            }
            if index == 0 || index == TITLED {
                println!(
                    "  index={index:<3} titled={:<5} slot108 sparse entries={entries}",
                    index < TITLED
                );
            }
        }

        vault.put_observation_constellation(constellation)?;
    }

    // --- read back the SOURCE OF TRUTH -------------------------------------
    println!("\n=== grounding_gap_report over the stored vault ===");
    let report = vault.grounding_gap_report(SYN_EPISODE_PANEL_VERSION, 10_000)?;
    println!(
        "  records_scanned={} records_measured={} grounded_fraction={:.4}",
        report.records_scanned, report.records_measured, report.grounded_fraction
    );
    for coverage in &report.slot_coverage {
        println!(
            "  slot {:>3}  present={:<4} empty_measurement={:<4} grounded={:<4} coverage={:.4}",
            coverage.slot,
            coverage.records_present,
            coverage.records_empty_measurement,
            coverage.grounded_records,
            coverage.coverage_fraction
        );
    }

    let find = |slot: u16| report.slot_coverage.iter().find(|c| c.slot == slot);

    // --- claims -------------------------------------------------------------
    println!("\n=== claims ===");
    let mut all_ok = true;
    let mut claim = |id: usize, what: &str, expected: String, observed: String, ok: bool| {
        println!("{id:>2} {what:<58} {}", verdict(ok));
        println!("     expected: {expected}");
        println!("     observed: {observed}");
        all_ok &= ok;
    };

    // 1. The encoder really does produce an empty vector for a titleless
    //    episode, and a non-empty one otherwise. This is the input side of the
    //    defect; if it fails, the premise is wrong and nothing below matters.
    let encoder_ok = slot_108_seen as u32 == TOTAL
        && measured_titled_entries > 0
        && measured_untitled_entries == 0;
    claim(
        1,
        "encoder: titled episodes post terms, untitled post none",
        format!("slot108 on all {TOTAL} records; titled entries>0; untitled entries==0"),
        format!(
            "slot108 on {slot_108_seen}; titled total entries={measured_titled_entries}; untitled total entries={measured_untitled_entries}"
        ),
        encoder_ok,
    );

    // 2. THE DEFECT. Coverage must count the 12 that measured something, not
    //    the 20 that merely carry the slot.
    let bm25 = find(SLOT_TITLE_BM25);
    let bm25_ok = bm25.is_some_and(|c| {
        c.records_present as u32 == TITLED && c.records_empty_measurement as u32 == UNTITLED
    });
    claim(
        2,
        "coverage: title BM25 lane counts only real measurements",
        format!("slot 108 present={TITLED} empty_measurement={UNTITLED}"),
        bm25.map_or_else(
            || "slot 108 MISSING from slot_coverage".to_owned(),
            |c| {
                format!(
                    "slot 108 present={} empty_measurement={}",
                    c.records_present, c.records_empty_measurement
                )
            },
        ),
        bm25_ok,
    );

    // 3. The normalized sibling reads the same text, so it must show the same
    //    split. Two lanes agreeing independently is what rules out a fluke.
    let sparse = find(SLOT_TITLE_SPARSE);
    let sparse_ok = sparse.is_some_and(|c| {
        c.records_present as u32 == TITLED && c.records_empty_measurement as u32 == UNTITLED
    });
    claim(
        3,
        "coverage: the normalized title lane agrees exactly",
        format!("slot 11 present={TITLED} empty_measurement={UNTITLED}"),
        sparse.map_or_else(
            || "slot 11 MISSING from slot_coverage".to_owned(),
            |c| {
                format!(
                    "slot 11 present={} empty_measurement={}",
                    c.records_present, c.records_empty_measurement
                )
            },
        ),
        sparse_ok,
    );

    // 4. CONTROL. A dense lane derived from the timestamp must be unaffected.
    //    Without this, a fix that simply deflated every slot would pass 2 and 3.
    let cyclic = find(SLOT_START_HOUR_CYCLIC);
    let cyclic_ok = cyclic
        .is_some_and(|c| c.records_present as u32 == TOTAL && c.records_empty_measurement == 0);
    claim(
        4,
        "control: the dense timestamp lane stays at full coverage",
        format!("slot 15 present={TOTAL} empty_measurement=0"),
        cyclic.map_or_else(
            || "slot 15 MISSING from slot_coverage".to_owned(),
            |c| {
                format!(
                    "slot 15 present={} empty_measurement={}",
                    c.records_present, c.records_empty_measurement
                )
            },
        ),
        cyclic_ok,
    );

    // 5. A dense ZERO is still a measurement. `start_hour_cyclic` at hour 0
    //    encodes sin(0)=0, and treating a zero component as absence would be the
    //    opposite error. Covered by claim 4 holding at the full count, but
    //    stated separately so the intent is explicit: only sparse/multi
    //    emptiness is unambiguous, never dense zeros.
    let dense_not_deflated = cyclic.is_some_and(|c| c.records_empty_measurement == 0);
    claim(
        5,
        "dense zeros are values, not absences",
        "no dense slot reports an empty measurement".to_owned(),
        format!(
            "dense empty_measurement counts = {}",
            report
                .slot_coverage
                .iter()
                .filter(|c| c.records_empty_measurement > 0)
                .map(|c| c.slot.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        dense_not_deflated,
    );

    println!();
    if all_ok {
        println!(
            "PASS: coverage reports {TITLED} measured / {UNTITLED} empty for the title lanes, and the dense control is untouched"
        );
        Ok(())
    } else {
        Err("episode_title_coverage_fsv: a claim failed; see the FAIL rows above".into())
    }
}
