//! Manual FSV for #2084: does the search provenance verifier recognise every
//! ledger entry shape a Base row can legitimately point at — and does it still
//! shout when the row is genuinely unreadable?
//!
//! ## The defect
//!
//! `calyx-search`'s `entry_covers_cx` recognised exactly two shapes: a ledger
//! entry whose `SubjectId::Cx` names the hit, or an `EntryKind::Ingest` entry
//! whose payload carries a `cx_id` array containing it. Anything else returned a
//! bare `false`, which the caller reported as
//!
//! ```text
//! CALYX_LEDGER_CORRUPT: search hit <cx> ledger seq <n> subject mismatch
//! remediation: ledger CF integrity violation — run verify_chain to identify range
//! ```
//!
//! — *after* the immediately preceding check had confirmed the entry hashes
//! exactly to the value Base recorded. A byte-identical, hash-verified ledger row
//! is by definition not a corrupt one, and the remediation would have an operator
//! restore a healthy vault from restic.
//!
//! The shape that tripped it in production is the multi-constellation grounding
//! anchor batch: `synapse-calyx put_grounding_anchors_for_many` mints ONE
//! `EntryKind::Grounding` / `SubjectId::Query(b"synapse.grounding_anchor.multi.v1")`
//! entry and `calyx-aster multi_cx_anchor_rows_with_ledger_ref` stamps its
//! `LedgerRef` onto every constellation in the batch. That is `Grounding`, not
//! `Ingest`, and its subject names no constellation at all, so every transcript
//! carrying a terminal agent outcome became unsearchable.
//!
//! ## What this instrument does
//!
//! Seven independent scratch vaults, one per entry shape, each built through the
//! **production** writers and queried through the **production** engine
//! (`calyx_search::search_outcome_with_query_vectors_freshness`, the same entry
//! point the `find` facade calls). Every corpus is four timeline rows of which
//! exactly one carries a coined marker term, so each query has a single correct
//! answer that cannot be satisfied by a decoy.
//!
//! | # | shape stamped onto the Base row | expectation |
//! |---|---|---|
//! | 1 | `Ingest` / `Cx(self)` — plain `put` | 1 grounded hit (control) |
//! | 2 | `Grounding` / `Query(multi marker)` — **the #2084 shape** | 1 grounded hit, trace names it batch-**verified** |
//! | 3 | `Ingest` / `Cx(first of batch)`, payload enumerates all | 1 grounded hit via the payload member list |
//! | 4 | `Ingest` / `Query(operation id)` — derived snapshot publish | 1 grounded hit, trace names it batch-verified |
//! | 5 | `Migrate` / `Cx(a different constellation)` | refused `CALYX_SEXTANT_PROVENANCE_SUBJECT_UNRESOLVED` |
//! | 6 | `Anneal` / `Kernel(..)` — a shape no writer stamps, **historical row** | refused `CALYX_SEXTANT_PROVENANCE_SHAPE_UNREGISTERED` naming `anneal/kernel` |
//! | 7 | `Ingest` / `Cx(other)` with a payload that enumerates nothing, **historical row** | refused `CALYX_LEDGER_CORRUPT` — a genuinely malformed entry keeps the loud verdict |
//! | 8 | `Grounding` / `Query(multi marker)` with **no member declaration** — a pre-#2096 row | 1 grounded hit, trace names it batch-**scoped** |
//!
//! Cases 5–7 are what stop this being a rubber stamp: a verifier that simply
//! accepted everything would pass 1–4 and fail all of 5–7. Case 7 in particular
//! proves the corruption channel is still wired, so widening recognition did not
//! disarm the check it was hiding behind. Case 6 proves an unrecognised shape
//! surfaces as its own evidence-carrying refusal that names the shape, rather
//! than as either false corruption or a silent drop.
//!
//! Every refusal is also checked for the string `restic` and for the ledger
//! corruption remediation, so an intact-ledger refusal can never again tell an
//! operator to restore the vault.
//!
//! ## What #2094/#2095/#2096 changed here
//!
//! Cases 6 and 7 used to be stamped through the live
//! `write_cf_batch_with_ledger_entry` path. #2095 gave that path a declared
//! legality contract, so it now REFUSES to mint either shape onto a Base row —
//! the refusal itself is proved by `ledger_hygiene_contracts_fsv`. Both shapes
//! are nevertheless still reachable by the READER, because rows committed before
//! that gate existed carry them and an append-only ledger cannot be rewritten.
//! So they are constructed here the only way that is now honest: the ledger entry
//! is appended on its own and the Base row is written with its `provenance`
//! pointing at it, reproducing byte-for-byte the state a pre-#2095 writer left
//! behind. A reader that stopped handling those rows would strand every one of
//! them, so this instrument keeps proving it does.
//!
//! Cases 2 and 4 tightened in the other direction: their batch entries now carry
//! a `batch_members` declaration (#2096), so the engine positively VERIFIES
//! membership from the entry instead of accepting it on the strength of the
//! entry-hash binding alone. Case 8 is the pre-#2096 form of case 2 and pins the
//! trusting path for historical rows, so the tightening cannot quietly become a
//! refusal of everything already committed.
//!
//! Usage:
//! `cargo run -p synapse-storage --example provenance_coverage_shapes_fsv -- <empty-scratch-dir>`

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

use calyx_aster::cf::{ColumnFamily, base_key};
use calyx_aster::vault::base_rewrite::BaseRowRewrite;
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{Anchor, AnchorKind, AnchorValue, CxId, SlotId, VaultId, VaultStore as _};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
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

/// A term coined for this instrument: it occurs in exactly one row of every
/// corpus, so a correct query has exactly one correct answer.
const MARKER: &str = "synfsv2084quokka";
const ROWS: u8 = 4;
const MARKED_ROW: u8 = 2;
const TEXT_SLOT: u16 = 103;
const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const CREATED_AT_MS: u64 = 1_785_000_000_000;

/// The exact subject literal `synapse-calyx put_grounding_anchors_for_many`
/// mints. Written out here rather than imported so this instrument states the
/// shape independently of the code under test.
const GROUNDING_MULTI_SUBJECT: &[u8] = b"synapse.grounding_anchor.multi.v1";

/// The refusal codes this instrument requires, spelled out rather than imported
/// for the same reason.
const SUBJECT_UNRESOLVED: &str = "CALYX_SEXTANT_PROVENANCE_SUBJECT_UNRESOLVED";
const SHAPE_UNREGISTERED: &str = "CALYX_SEXTANT_PROVENANCE_SHAPE_UNREGISTERED";
const LEDGER_CORRUPT: &str = "CALYX_LEDGER_CORRUPT";

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
        cx_id: CxId::from_bytes([0x20, 0x84, tag, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        created_at_ms: CREATED_AT_MS + u64::from(tag),
        next_ledger_seq: u64::from(tag) + 1,
    };
    let key = format!("fsv-2084/timeline-{tag}");
    Ok(build_timeline_constellation(
        context,
        key.as_bytes(),
        &raw,
        &record,
    )?)
}

/// A scratch vault holding the corpus, with no provenance mutation applied yet.
struct Scratch {
    dir: PathBuf,
    vault: AsterVault,
    state: VaultPanelState,
    ids: Vec<CxId>,
    marked: CxId,
}

fn open_scratch(root: &Path, name: &str) -> Result<Scratch, Box<dyn Error>> {
    let dir = root.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let vault_id: VaultId = VAULT_ID.parse()?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"provenance-coverage-shapes-fsv".to_vec(),
        VaultOptions::default(),
    )?;
    let contract = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, CREATED_AT_MS)?
        .ok_or("no built-in contract for the timeline panel version")?;
    let state = VaultPanelState {
        panel: contract.panel,
        registry: contract.registry,
        registry_snapshot: None,
    };
    let mut ids = Vec::new();
    let mut marked = None;
    for tag in 1..=ROWS {
        let constellation = row(vault_id, tag)?;
        let cx_id = constellation.cx_id;
        if tag == MARKED_ROW {
            marked = Some(cx_id);
        }
        ids.push(cx_id);
        vault.put(constellation)?;
    }
    let marked = marked.ok_or("marked row was never built")?;
    Ok(Scratch {
        dir,
        vault,
        state,
        ids,
        marked,
    })
}

/// Reads the provenance `(seq, hash-prefix)` a Base row currently carries — the
/// Source of Truth for "which ledger entry this hit points at".
fn stored_provenance(scratch: &Scratch, cx_id: CxId) -> Result<(u64, String), Box<dyn Error>> {
    let cx = scratch.vault.get(cx_id, scratch.vault.latest_seq())?;
    let mut hash = String::new();
    for byte in cx.provenance.hash.iter().take(4) {
        use std::fmt::Write as _;
        write!(&mut hash, "{byte:02x}")?;
    }
    Ok((cx.provenance.seq, hash))
}

/// Re-commits a Base row verbatim under a chosen ledger entry shape, so the
/// row's `provenance` is restamped with that shape and nothing else about the
/// row changes.
///
/// This is the production path `calyx-aster`'s `write_cf_batch_with_ledger_entry`
/// offers to every layer caller: it rewrites `provenance` on every `Base` row in
/// the batch (`attach_ledger_ref_to_base_rows`). Using it means the shapes below
/// are stamped exactly the way a real writer would stamp them.
fn restamp_with_shape(
    scratch: &Scratch,
    cx_id: CxId,
    kind: EntryKind,
    subject: SubjectId,
    payload: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let bytes = scratch
        .vault
        .read_cf_at(
            scratch.vault.latest_seq(),
            ColumnFamily::Base,
            &base_key(cx_id),
        )?
        .ok_or("Base row disappeared before restamping")?;
    scratch.vault.write_cf_batch_with_ledger_entry(
        [(ColumnFamily::Base, base_key(cx_id), bytes)],
        kind,
        subject,
        serde_json::to_vec(payload)?,
        ActorId::Service("provenance-coverage-shapes-fsv".to_owned()),
    )?;
    Ok(())
}

/// Reproduces the on-disk state a **pre-#2095/#2096 writer** left behind: a
/// ledger entry of an arbitrary shape, and a Base row whose `provenance` points
/// at it with no membership declaration anywhere.
///
/// The two steps are separate on purpose. `write_cf_batch_with_ledger_entry` now
/// refuses undeclared shapes and injects a `batch_members` declaration into
/// everything it does accept, so it can no longer produce these rows — which is
/// exactly the #2095/#2096 fix working. Appending the entry alone and writing the
/// Base row through the raw commit path yields the identical bytes an older build
/// committed, which is the only state under which the reader's historical
/// branches are reachable. Nothing here fabricates slot hashes: `BaseRowRewrite`
/// carries the stored ones through (#1888).
fn restamp_historical(
    scratch: &Scratch,
    cx_id: CxId,
    kind: EntryKind,
    subject: SubjectId,
    payload: &serde_json::Value,
) -> Result<u64, Box<dyn Error>> {
    let ledger_ref = scratch.vault.append_ledger_entry(
        kind,
        subject,
        serde_json::to_vec(payload)?,
        ActorId::Service("provenance-coverage-shapes-fsv".to_owned()),
    )?;
    let bytes = scratch
        .vault
        .read_cf_at(
            scratch.vault.latest_seq(),
            ColumnFamily::Base,
            &base_key(cx_id),
        )?
        .ok_or("Base row disappeared before historical restamping")?;
    let mut rewrite = BaseRowRewrite::decode(&bytes)?;
    rewrite.constellation_mut().provenance = ledger_ref.clone();
    scratch
        .vault
        .write_cf_batch([(ColumnFamily::Base, base_key(cx_id), rewrite.encode()?)])?;
    Ok(ledger_ref.seq)
}

/// The outcome of one probe: either the hits the engine returned, or the
/// structured refusal it produced.
enum Probe {
    Hits(Vec<CxId>),
    Refused {
        code: String,
        message: String,
        remediation: String,
    },
}

/// Rebuilds the generation and runs the marker probe through the production
/// engine, capturing the trace so a batch-scoped acceptance can be asserted
/// rather than assumed.
fn probe(scratch: &Scratch) -> Result<(Probe, Vec<SearchTraceEvent>), Box<dyn Error>> {
    calyx_search::rebuild_for_vault_with_panel_state(&scratch.dir, &scratch.vault, &scratch.state)?;
    let allowed = BTreeSet::from([SlotId::new(TEXT_SLOT)]);
    let query_vectors =
        calyx_search::measure_query_vectors_with_slots(&scratch.state, MARKER, Some(&allowed))?;
    if query_vectors.is_empty() {
        return Err("the marker measured to no query vector; the probe would be vacuous".into());
    }
    let mut trace = Vec::new();
    let mut sink = |event: SearchTraceEvent| trace.push(event);
    let outcome = calyx_search::search_outcome_with_query_vectors_freshness(
        &scratch.vault,
        &scratch.dir,
        &scratch.state.panel,
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
    );
    let probe = match outcome {
        Ok(outcome) => Probe::Hits(outcome.hits.iter().map(|hit| hit.cx_id).collect()),
        Err(error) => Probe::Refused {
            code: error.code().to_owned(),
            message: error.message().to_owned(),
            remediation: error.remediation().unwrap_or("<none>").to_owned(),
        },
    };
    Ok((probe, trace))
}

/// The details the engine's own trace emitted under one coverage phase — the
/// evidence that recognition happened through the declared rule and not by
/// accident.
///
/// Two phases matter here and must never be conflated:
/// `provenance.ledger_coverage.batch_verified` means membership was decided FROM
/// the entry's member declaration (#2096); `...batch_scoped` means the entry
/// declared no members and acceptance rested on the entry-hash and chain-link
/// binding alone, which is all a pre-#2096 row can offer.
fn traced_coverage(trace: &[SearchTraceEvent], phase: &str) -> Vec<String> {
    trace
        .iter()
        .filter(|event| event.phase == phase)
        .map(|event| event.detail.clone().unwrap_or_default())
        .collect()
}

const BATCH_VERIFIED: &str = "provenance.ledger_coverage.batch_verified";
const BATCH_SCOPED: &str = "provenance.ledger_coverage.batch_scoped";

/// A refusal on an intact ledger row must never route an operator to a restore.
fn assert_no_restore_advice(case: &str, message: &str, remediation: &str) -> Result<(), String> {
    let haystack = format!("{message} {remediation}").to_lowercase();
    for forbidden in ["restic", "snapshot restore", "verify_chain"] {
        if haystack.contains(forbidden) {
            return Err(format!(
                "{case}: refusal on a hash-verified ledger row mentions {forbidden:?}; \
                 remediation={remediation:?}"
            ));
        }
    }
    Ok(())
}

/// The pre-#2084 recognition rule, restated here from the issue rather than
/// called, so the run reports what the shipped code *used* to do beside what it
/// does now.
///
/// ```text
/// if entry.subject == SubjectId::Cx(cx_id) { return true }
/// if entry.kind != EntryKind::Ingest       { return false }   // -> LEDGER_CORRUPT
/// payload cx_id array contains cx_id
/// ```
///
/// Cases the old rule would have served are not discriminating: if every case
/// passed under both rules the instrument would prove nothing about the fix.
fn old_rule_would_serve(
    kind: EntryKind,
    subject_names_this_cx: bool,
    payload_lists_this_cx: bool,
) -> bool {
    if subject_names_this_cx {
        return true;
    }
    if kind != EntryKind::Ingest {
        return false;
    }
    payload_lists_this_cx
}

fn describe(probe: &Probe) -> String {
    match probe {
        Probe::Hits(hits) => format!("{} hit(s) {hits:?}", hits.len()),
        Probe::Refused { code, message, .. } => format!("REFUSED {code}: {message}"),
    }
}

/// Requires exactly the marked row back, grounded.
fn expect_marked_hit(case: &str, scratch: &Scratch, probe: &Probe) -> Result<(), String> {
    match probe {
        Probe::Hits(hits) if hits.len() == 1 && hits[0] == scratch.marked => Ok(()),
        Probe::Hits(hits) => Err(format!(
            "{case}: expected exactly the marked row {} but got {hits:?}",
            scratch.marked
        )),
        Probe::Refused { code, message, .. } => Err(format!(
            "{case}: expected a grounded hit but the engine refused {code}: {message}"
        )),
    }
}

/// Requires a refusal carrying exactly `code`, with `must_name` present in the
/// message so the evidence actually identifies the shape.
fn expect_refusal(case: &str, probe: &Probe, code: &str, must_name: &[&str]) -> Result<(), String> {
    match probe {
        Probe::Hits(hits) => Err(format!(
            "{case}: expected refusal {code} but the engine returned {} hit(s) {hits:?}",
            hits.len()
        )),
        Probe::Refused {
            code: observed,
            message,
            ..
        } => {
            if observed != code {
                return Err(format!(
                    "{case}: expected {code} but observed {observed}: {message}"
                ));
            }
            for needle in must_name {
                if !message.contains(needle) {
                    return Err(format!(
                        "{case}: refusal {code} does not name {needle:?}; message={message:?}"
                    ));
                }
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: provenance_coverage_shapes_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&root)?;
    println!(
        "provenance_coverage_shapes_fsv  (#2084)   root={}",
        root.display()
    );
    println!("marker term = {MARKER} (occurs in exactly 1 of {ROWS} rows per corpus)\n");

    let mut failures: Vec<String> = Vec::new();

    // ---- 1. control: plain single ingest, Ingest / Cx(self) ----------------
    {
        let case = "1 Ingest/Cx(self)";
        let scratch = open_scratch(&root, "case1_ingest_cx_self")?;
        let (seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        let (probe, _trace) = probe(&scratch)?;
        println!("=== {case} — the control ===");
        println!("  marked row provenance seq={seq} hash={hash}..");
        println!("  {}", describe(&probe));
        if let Err(error) = expect_marked_hit(case, &scratch, &probe) {
            failures.push(error);
        }
    }

    // ---- 2. the #2084 shape: Grounding / Query(multi marker) ---------------
    {
        let case = "2 Grounding/Query(multi)";
        let scratch = open_scratch(&root, "case2_grounding_multi")?;
        let (before_seq, _) = stored_provenance(&scratch, scratch.marked)?;
        let anchors = scratch
            .ids
            .iter()
            .map(|cx_id| {
                (
                    *cx_id,
                    vec![Anchor {
                        kind: AnchorKind::Label("synapse:agent_end_state".to_owned()),
                        value: AnchorValue::Enum("completed".to_owned()),
                        source: "synapse-outcome-anchors".to_owned(),
                        observed_at: CREATED_AT_MS,
                        confidence: 1.0,
                    }],
                )
            })
            .collect::<Vec<_>>();
        // The production multi-constellation grounding writer, byte for byte:
        // one Grounding entry under an opaque Query subject, stamped onto every
        // constellation in the batch.
        let outcome = scratch.vault.anchors_for_many_with_ledger_entry(
            anchors,
            EntryKind::Grounding,
            SubjectId::Query(GROUNDING_MULTI_SUBJECT.to_vec()),
            serde_json::to_vec(&json!({
                "schema": "synapse.grounding_anchor_batch.v1",
                "source_count": ROWS,
            }))?,
            ActorId::Service("synapse-outcome-anchors".to_owned()),
        )?;
        let batch_seq = outcome
            .ledger_ref
            .as_ref()
            .ok_or("the grounding batch wrote no ledger entry; the case would be vacuous")?
            .seq;
        let (after_seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — the shape #2084 reported as vault corruption ===");
        println!(
            "  anchors written={} existing={} batch ledger seq={batch_seq}",
            outcome.written_anchor_count, outcome.existing_anchor_count
        );
        println!("  marked row provenance {before_seq} -> {after_seq} hash={hash}..");
        if after_seq != batch_seq {
            failures.push(format!(
                "{case}: the batch entry at seq {batch_seq} was not stamped onto the marked row \
                 (still seq {after_seq}); the case would prove nothing"
            ));
        }
        let (probe, trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        let verified = traced_coverage(&trace, BATCH_VERIFIED);
        for detail in &verified {
            println!("  TRACE {BATCH_VERIFIED} {detail}");
        }
        for detail in &traced_coverage(&trace, BATCH_SCOPED) {
            println!("  TRACE {BATCH_SCOPED} {detail}");
        }
        if let Err(error) = expect_marked_hit(case, &scratch, &probe) {
            failures.push(error);
        } else if !verified.iter().any(|detail| {
            detail.contains("kind=grounding")
                && detail.contains("subject=query")
                && detail.contains(&format!("members={ROWS}"))
        }) {
            failures.push(format!(
                "{case}: the hit was served but the engine never declared it batch-VERIFIED with \
                 {ROWS} declared members; post-#2096 this shape must be decided from the entry's \
                 own member list, not accepted on the entry-hash binding alone"
            ));
        }
    }

    // ---- 3. Ingest / Cx(first of batch), payload enumerates all ------------
    {
        let case = "3 Ingest/Cx(other)+payload list";
        let scratch = open_scratch(&root, "case3_batch_enumerated")?;
        // Re-ingest the same corpus as one batch under a ledger entry whose
        // subject names only the FIRST row and whose payload enumerates all of
        // them — the `calyx-aster batch_payload` shape.
        let vault_id: VaultId = VAULT_ID.parse()?;
        let batch = (1..=ROWS)
            .map(|tag| row(vault_id, tag))
            .collect::<Result<Vec<_>, _>>()?;
        let members = scratch
            .ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let first = *scratch.ids.first().ok_or("empty corpus")?;
        if first == scratch.marked {
            return Err("the marked row is the batch subject; case 3 would test the subject rule instead of the payload rule".into());
        }
        scratch.vault.put_batch_with_ingest_ledger(
            batch,
            SubjectId::Cx(first),
            serde_json::to_vec(&json!({
                "mode": "batch_ingest",
                "count": members.len(),
                "cx_id": members,
            }))?,
            ActorId::Service("provenance-coverage-shapes-fsv".to_owned()),
        )?;
        let (seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — the marked row is NOT the subject; the payload names it ===");
        println!("  batch subject cx={first}   marked cx={}", scratch.marked);
        println!("  marked row provenance seq={seq} hash={hash}..");
        let (probe, _trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        if let Err(error) = expect_marked_hit(case, &scratch, &probe) {
            failures.push(error);
        }
    }

    // ---- 4. Ingest / Query(operation id) — derived snapshot publish --------
    {
        let case = "4 Ingest/Query(operation)";
        let scratch = open_scratch(&root, "case4_ingest_query")?;
        restamp_with_shape(
            &scratch,
            scratch.marked,
            EntryKind::Ingest,
            SubjectId::Query(b"fsv-2084-publish-operation".to_vec()),
            &json!({
                "operation": "publish_derived_snapshot",
                "constellation_count": ROWS,
            }),
        )?;
        let (seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — the derived-snapshot publish shape (no cx_id anywhere) ===");
        println!("  marked row provenance seq={seq} hash={hash}..");
        let (probe, trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        let verified = traced_coverage(&trace, BATCH_VERIFIED);
        for detail in &verified {
            println!("  TRACE {BATCH_VERIFIED} {detail}");
        }
        for detail in &traced_coverage(&trace, BATCH_SCOPED) {
            println!("  TRACE {BATCH_SCOPED} {detail}");
        }
        if let Err(error) = expect_marked_hit(case, &scratch, &probe) {
            failures.push(error);
        } else if verified.is_empty() {
            failures.push(format!(
                "{case}: the derived-snapshot publish shape was served without a member \
                 declaration; #2096 requires the layer-commit writer to enumerate the Base rows \
                 it stamps"
            ));
        }
    }

    // ---- 5. Migrate / Cx(a DIFFERENT constellation) ------------------------
    {
        let case = "5 Migrate/Cx(other)";
        let scratch = open_scratch(&root, "case5_subject_unresolved")?;
        let other = *scratch
            .ids
            .iter()
            .find(|id| **id != scratch.marked)
            .ok_or("corpus has no second row")?;
        restamp_with_shape(
            &scratch,
            scratch.marked,
            EntryKind::Migrate,
            SubjectId::Cx(other),
            &json!({ "mode": "temporal-metadata-backfill", "cx_id": other.to_string() }),
        )?;
        let (seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — a single-subject shape naming somebody else ===");
        println!("  marked row provenance seq={seq} hash={hash}.. subject names {other}");
        let (probe, _trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        if let Err(error) = expect_refusal(case, &probe, SUBJECT_UNRESOLVED, &["migrate", "cx"]) {
            failures.push(error);
        }
        if let Probe::Refused {
            message,
            remediation,
            ..
        } = &probe
            && let Err(error) = assert_no_restore_advice(case, message, remediation)
        {
            failures.push(error);
        }
    }

    // ---- 6. Anneal / Kernel(..) — an undeclared shape, historical row ------
    {
        let case = "6 Anneal/Kernel (undeclared, historical)";
        let scratch = open_scratch(&root, "case6_shape_unregistered")?;
        let seq = restamp_historical(
            &scratch,
            scratch.marked,
            EntryKind::Anneal,
            SubjectId::Kernel(b"fsv-2084-anneal-change".to_vec()),
            &json!({ "kind": "Anneal", "change_id": 1 }),
        )?;
        let (stored_seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — a shape no declared writer stamps onto a Base row ===");
        println!(
            "  minted historically at seq={seq}; marked row provenance seq={stored_seq} hash={hash}.."
        );
        let (probe, _trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        if let Err(error) = expect_refusal(case, &probe, SHAPE_UNREGISTERED, &["anneal/kernel"]) {
            failures.push(error);
        }
        if let Probe::Refused {
            message,
            remediation,
            ..
        } = &probe
            && let Err(error) = assert_no_restore_advice(case, message, remediation)
        {
            failures.push(error);
        }
    }

    // ---- 7. malformed enumerating entry — corruption MUST still fire -------
    {
        let case = "7 Ingest/Cx(other) with no member list";
        let scratch = open_scratch(&root, "case7_malformed_enumeration")?;
        let other = *scratch
            .ids
            .iter()
            .find(|id| **id != scratch.marked)
            .ok_or("corpus has no second row")?;
        restamp_historical(
            &scratch,
            scratch.marked,
            EntryKind::Ingest,
            SubjectId::Cx(other),
            // Declared to enumerate (Ingest under a Cx subject that is not ours)
            // but carries no member declaration at all — neither the #2096
            // `batch_members` block nor the legacy top-level `cx_id` array. It is
            // unreadable as its own declared shape, which is a real
            // ledger-content defect.
            &json!({ "mode": "batch_ingest", "count": 4 }),
        )?;
        let (seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — the corruption channel must still be wired ===");
        println!("  marked row provenance seq={seq} hash={hash}..");
        let (probe, _trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        if let Err(error) = expect_refusal(case, &probe, LEDGER_CORRUPT, &["cx_id"]) {
            failures.push(error);
        }
    }

    // ---- 8. a PRE-#2096 batch entry that names no member at all ------------
    // The ledger is append-only, so every multi-constellation grounding batch
    // written before #2096 still carries no member declaration. Tightening the
    // reader must not strand them: this case pins the trusting path that those
    // rows — and only those rows — still take.
    {
        let case = "8 Grounding/Query(multi), no declaration";
        let scratch = open_scratch(&root, "case8_batch_scoped_legacy")?;
        let seq = restamp_historical(
            &scratch,
            scratch.marked,
            EntryKind::Grounding,
            SubjectId::Query(GROUNDING_MULTI_SUBJECT.to_vec()),
            // Exactly the pre-#2096 payload: hashed source keys, a count, and
            // nothing that names a constellation.
            &json!({
                "schema": "synapse.grounding_anchor_batch.v1",
                "source_cf": "agent_transcripts",
                "source_count": ROWS,
            }),
        )?;
        let (stored_seq, hash) = stored_provenance(&scratch, scratch.marked)?;
        println!("\n=== {case} — a row committed before members were declarable ===");
        println!(
            "  minted historically at seq={seq}; marked row provenance seq={stored_seq} hash={hash}.."
        );
        let (probe, trace) = probe(&scratch)?;
        println!("  {}", describe(&probe));
        let scoped = traced_coverage(&trace, BATCH_SCOPED);
        for detail in &scoped {
            println!("  TRACE {BATCH_SCOPED} {detail}");
        }
        if let Err(error) = expect_marked_hit(case, &scratch, &probe) {
            failures.push(error);
        } else if !scoped
            .iter()
            .any(|detail| detail.contains("kind=grounding") && detail.contains("subject=query"))
        {
            failures.push(format!(
                "{case}: a pre-#2096 batch row was served, but not through the batch-scoped \
                 trusting path; either it was silently upgraded to a verified verdict it cannot \
                 support, or coverage was decided by some other route"
            ));
        }
        if !traced_coverage(&trace, BATCH_VERIFIED).is_empty() {
            failures.push(format!(
                "{case}: an entry carrying NO member declaration was reported as batch-verified"
            ));
        }
    }

    // ---- the discrimination control ---------------------------------------
    // Cases 2 and 4 are the ones the old two-shape rule refused. If they were
    // servable under it too, this instrument would pass with or without the fix.
    println!("\n=== discrimination control: what the pre-#2084 rule would have done ===");
    let old_verdicts = [
        (
            "1 Ingest/Cx(self)",
            old_rule_would_serve(EntryKind::Ingest, true, false),
        ),
        (
            "2 Grounding/Query(multi)",
            old_rule_would_serve(EntryKind::Grounding, false, false),
        ),
        (
            "3 Ingest/Cx(other)+payload list",
            old_rule_would_serve(EntryKind::Ingest, false, true),
        ),
        (
            "4 Ingest/Query(operation)",
            old_rule_would_serve(EntryKind::Ingest, false, false),
        ),
    ];
    for (case, served) in &old_verdicts {
        println!(
            "  {case:<34} old rule: {}",
            if *served {
                "served"
            } else {
                "CALYX_LEDGER_CORRUPT (subject mismatch)"
            }
        );
    }
    if old_verdicts.iter().all(|(_, served)| *served) {
        failures.push(
            "every legitimate shape would also have been served by the pre-#2084 rule; this run \
             does not discriminate and would pass on the unfixed code"
                .to_owned(),
        );
    }

    println!("\n================= VERDICT =================");
    if failures.is_empty() {
        println!(
            "PASS: all four legitimate shapes serve grounded hits, the two recognised-but-\n\
             uncovered shapes refuse with their own codes naming the shape and never mention a\n\
             restore, and a malformed entry still reports {LEDGER_CORRUPT}."
        );
        Ok(())
    } else {
        for failure in &failures {
            println!("  FAIL {failure}");
        }
        Err(format!("{} case(s) failed", failures.len()).into())
    }
}
