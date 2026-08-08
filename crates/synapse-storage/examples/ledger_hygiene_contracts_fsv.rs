//! Manual FSV for #2094 / #2095 / #2096: three ledger-hygiene contracts that
//! were declared nowhere and enforced nowhere.
//!
//! Each section drives PRODUCTION writers and PRODUCTION readers against scratch
//! vaults / scratch ledgers, and each carries a discrimination control — a
//! statement of what the pre-fix code did — so a run that passes cannot pass
//! equally well on the unfixed tree.
//!
//! ## A — #2096: is batch membership answerable from the ledger?
//!
//! The multi-constellation grounding anchor batch stamps ONE ledger entry onto
//! every constellation it grounds, under
//! `SubjectId::Query(b"synapse.grounding_anchor.multi.v1")` — a fixed literal
//! naming no constellation — and its payload named members only by
//! `source_key_sha256`. So "which constellations does seq N cover" had no answer
//! from the ledger: a live entry covering 559 transcript constellations could
//! only be resolved by re-deriving cx ids from source keys against the vault.
//!
//! A1 mints a real batch through `anchors_for_many_with_ledger_entry` and then
//! answers the question from **the Ledger CF row alone** — decode the entry, read
//! its `batch_members` declaration — with the vault's Base rows used only
//! afterwards to check that the answer is right. A2 proves the caller's own
//! payload still names no constellation, so the answerability comes from the
//! declaration and not from something the caller happened to include. A3 drives
//! the size bound and requires that truncation is stated, counted, pointed at an
//! authority, and never read as a negative.
//!
//! ## B — #2095: is an undeclared `(kind, subject)` refused at WRITE time?
//!
//! `write_cf_batch_with_ledger_entry` rewrote Base-row provenance with any
//! caller-supplied pair, with nothing declaring which pairs are legal. The first
//! symptom of a wrong one was a refused query weeks later, on a vault an operator
//! then suspected of corruption. B1/B2 supply undeclared pairs to both
//! Base-stamping writers and require the refusal *before* anything is written;
//! B3 proves the refusal is not a blanket one (a declared pair still commits);
//! B4 proves it is scoped to Base stamping (a batch with no Base rows is
//! untouched by the gate, which is every in-tree caller today).
//!
//! ## C — #2094: does reproduce's gate sit on evidence that can exist?
//!
//! `build_reproduce_context` gated a `measure_refs` target on
//! `EntryKind::Measure`, which no writer in the workspace mints — so the gate
//! could only ever be skipped. Worse, an answer entry with no evidence at all
//! returned an EMPTY context, and `assert_within_tolerance(&[], &[])` scores
//! empty-against-empty as `reproduced = true`: a reproduce verdict no measurement
//! backed. C1 drives the real path with a real `Measure` entry through it, C2
//! aims `measure_refs` at a non-measurement entry and requires the refusal, and
//! C3 proves the empty context is now refused rather than scored — printing the
//! vacuous verdict the old path would have returned.
//!
//! Usage:
//! `cargo run -p synapse-storage --example ledger_hygiene_contracts_fsv -- <empty-scratch-dir>`

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

use calyx_aster::cf::{ColumnFamily, base_key, ledger_key};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{
    Anchor, AnchorKind, AnchorValue, CalyxError, CxId, Input, LensId, PanelSlotId, Result, SlotId,
    SlotVector, SystemClock, VaultId, VaultStore as _,
};
use calyx_ledger::{
    ActorId, BatchMembers, EntryKind, LedgerAppender, MAX_ENUMERATED_BATCH_MEMBERS, MemberVerdict,
    MemoryLedgerStore, SubjectId, WriterStatus, assert_within_tolerance, build_reproduce_context,
    declare_batch_members, read_batch_members,
};
use serde_json::{Value, json};
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_VERSION, build_timeline_constellation,
};

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const CREATED_AT_MS: u64 = 1_785_000_000_000;
const ROWS: u8 = 6;

/// The exact subject literal `synapse-calyx put_grounding_anchors_for_many`
/// mints. Written out rather than imported so this instrument states the shape
/// independently of the code under test.
const GROUNDING_MULTI_SUBJECT: &[u8] = b"synapse.grounding_anchor.multi.v1";

const BASE_STAMP_UNDECLARED: &str = "CALYX_LEDGER_BASE_STAMP_UNDECLARED";
const REPRODUCE_EVIDENCE_UNAVAILABLE: &str = "CALYX_REPRODUCE_EVIDENCE_UNAVAILABLE";

fn main() -> std::result::Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: ledger_hygiene_contracts_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&root)?;
    println!("ledger_hygiene_contracts_fsv  (#2094 / #2095 / #2096)");
    println!("root={}\n", root.display());

    let mut failures: Vec<String> = Vec::new();
    section_a_membership(&root, &mut failures)?;
    section_b_write_legality(&root, &mut failures)?;
    section_c_reproduce_evidence(&mut failures)?;

    println!("\n================= VERDICT =================");
    if failures.is_empty() {
        println!(
            "PASS: batch membership is answerable from the ledger alone and states its own\n\
             truncation; an undeclared (kind, subject) pair is refused at write time with\n\
             {BASE_STAMP_UNDECLARED} before anything is committed; and reproduce's gate now sits\n\
             on evidence that a writer can actually produce, refusing an empty context instead of\n\
             scoring it as a successful reproduction."
        );
        Ok(())
    } else {
        for failure in &failures {
            println!("  FAIL {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}

// ===========================================================================
// A — #2096: batch membership answerable from the ledger
// ===========================================================================

#[allow(clippy::too_many_lines)]
fn section_a_membership(
    root: &Path,
    failures: &mut Vec<String>,
) -> std::result::Result<(), Box<dyn Error>> {
    println!("=== A. #2096 — which constellations does seq N cover? ===");
    let scratch = open_scratch(root, "a_membership")?;

    // The caller's payload, byte-identical in shape to
    // `transcript_end_state_anchor_batch_payload`: hashed source keys, a count,
    // and nothing that names a constellation.
    let caller_payload = json!({
        "schema": "synapse.grounding_anchor_batch.v1",
        "source_cf": "agent_transcripts",
        "source_count": ROWS,
        "sources": scratch.ids.iter().map(|_| json!({
            "source_key_sha256": "b".repeat(64),
            "source_value_sha256": "c".repeat(64),
        })).collect::<Vec<_>>(),
    });

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
    let outcome = scratch.vault.anchors_for_many_with_ledger_entry(
        anchors,
        EntryKind::Grounding,
        SubjectId::Query(GROUNDING_MULTI_SUBJECT.to_vec()),
        serde_json::to_vec(&caller_payload)?,
        ActorId::Service("synapse-outcome-anchors".to_owned()),
    )?;
    let seq = outcome
        .ledger_ref
        .as_ref()
        .ok_or("the grounding batch wrote no ledger entry; section A would be vacuous")?
        .seq;
    println!(
        "  minted a real multi-constellation grounding batch: seq={seq} \
         written_anchors={} constellations={}",
        outcome.written_anchor_count,
        scratch.ids.len()
    );

    // ---- A1: answer the question from the LEDGER ALONE --------------------
    let entry = read_ledger_entry(&scratch.vault, seq)?;
    let declared = read_batch_members(&entry.payload)?;
    let answer = match &declared {
        BatchMembers::Complete { members, total, .. } => {
            println!("  A1 ledger seq {seq} declares {total} member(s), complete:");
            for member in members {
                println!("       {member}");
            }
            members.clone()
        }
        other => {
            failures.push(format!(
                "A1: ledger seq {seq} carries no complete member declaration ({other:?}); \
                 'which constellations does this batch cover' is still unanswerable"
            ));
            BTreeSet::new()
        }
    };
    let truth = scratch
        .ids
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    if !answer.is_empty() && answer != truth {
        failures.push(format!(
            "A1: the ledger's answer does not match the constellations actually stamped: \
             ledger={answer:?} vault={truth:?}"
        ));
    }
    // The vault is consulted only to CHECK the ledger's answer, never to produce
    // it. Each stamped Base row must carry this exact seq.
    for cx_id in &scratch.ids {
        let stored = scratch.vault.get(*cx_id, scratch.vault.latest_seq())?;
        if stored.provenance.seq != seq {
            failures.push(format!(
                "A1: {cx_id} was declared a member of seq {seq} but its Base row points at seq {}",
                stored.provenance.seq
            ));
        }
    }
    if answer == truth && !truth.is_empty() {
        println!("  A1 OK: the ledger's answer is exactly the set of stamped Base rows");
    }

    // The operator-facing audit surface must answer the same question. `audit`
    // and `get_provenance` both reach a row through `entry_cx_mentions`, which
    // walks the payload for `cx_id` fields — so the declaration makes the batch
    // discoverable from an operator's "what touched this constellation" query,
    // not only from a reader that knows to look for `batch_members`.
    let mentioned = calyx_ledger::entry_cx_mentions(&entry)
        .into_iter()
        .map(|cx_id| cx_id.to_string())
        .collect::<BTreeSet<_>>();
    println!(
        "  A1 audit surface: entry_cx_mentions(seq {seq}) returns {} constellation(s)",
        mentioned.len()
    );
    if mentioned != truth {
        failures.push(format!(
            "A1: the ledger audit surface reports {mentioned:?} for seq {seq}, but the batch \
             stamped {truth:?}; 'what touched this constellation' would miss the batch"
        ));
    }

    // ---- A2: discrimination — the caller's payload names nobody -----------
    let caller_text = serde_json::to_string(&caller_payload)?;
    let leaked = truth
        .iter()
        .filter(|member| caller_text.contains(member.as_str()))
        .count();
    println!(
        "  A2 discrimination: the writer's own payload mentions {leaked} of {} member ids \
         (pre-#2096 that is the whole payload, so the question had no answer)",
        truth.len()
    );
    if leaked != 0 {
        failures.push(format!(
            "A2: the caller payload already named {leaked} member(s), so section A would pass \
             without the #2096 declaration and proves nothing"
        ));
    }

    // ---- A3: the size bound must be stated, never silent ------------------
    let oversized = (0..=MAX_ENUMERATED_BATCH_MEMBERS)
        .map(synthetic_cx)
        .collect::<Vec<_>>();
    let bounded = declare_batch_members(b"{}", &oversized)?;
    let bounded_members = read_batch_members(&bounded)?;
    match &bounded_members {
        BatchMembers::Truncated {
            total,
            members,
            authority,
            ..
        } => {
            println!(
                "  A3 bound: {} requested -> declared total={total} listed={} truncated=true \
                 authority={authority}",
                oversized.len(),
                members.len()
            );
            if *total != oversized.len() || members.len() != MAX_ENUMERATED_BATCH_MEMBERS {
                failures.push(format!(
                    "A3: a truncated declaration must state the COMPLETE total ({}) and list \
                     exactly the bound ({MAX_ENUMERATED_BATCH_MEMBERS}); saw total={total} \
                     listed={}",
                    oversized.len(),
                    members.len()
                ));
            }
            if authority.is_empty() {
                failures.push(
                    "A3: a truncated declaration names no authority for the complete set"
                        .to_owned(),
                );
            }
        }
        other => failures.push(format!(
            "A3: a member list above the {MAX_ENUMERATED_BATCH_MEMBERS} bound must declare itself \
             truncated; saw {other:?}"
        )),
    }
    // The one thing truncation must never do: read as a decided negative.
    let unlisted = *oversized
        .last()
        .ok_or("the oversized batch is empty; A3 would be vacuous")?;
    let verdict = bounded_members.verdict(unlisted);
    println!("  A3 unlisted member verdict = {verdict:?} (must be Undecided, never NotListed)");
    if verdict != MemberVerdict::Undecided {
        failures.push(format!(
            "A3: a member outside a TRUNCATED list decided as {verdict:?}; a size bound must \
             never become a false negative on a covered row"
        ));
    }
    let listed = *oversized
        .first()
        .ok_or("the oversized batch is empty; A3 would be vacuous")?;
    if bounded_members.verdict(listed) != MemberVerdict::Listed {
        failures
            .push("A3: a member inside the truncated prefix was not decided as Listed".to_owned());
    }
    Ok(())
}

// ===========================================================================
// B — #2095: write-time legality of the (kind, subject) pair
// ===========================================================================

#[allow(clippy::too_many_lines)]
fn section_b_write_legality(
    root: &Path,
    failures: &mut Vec<String>,
) -> std::result::Result<(), Box<dyn Error>> {
    println!("\n=== B. #2095 — an undeclared (kind, subject) must be refused at WRITE time ===");
    let scratch = open_scratch(root, "b_write_legality")?;
    let target = *scratch.ids.first().ok_or("empty corpus")?;
    let base_bytes = scratch
        .vault
        .read_cf_at(
            scratch.vault.latest_seq(),
            ColumnFamily::Base,
            &base_key(target),
        )?
        .ok_or("Base row disappeared")?;
    let before_seq = scratch.vault.latest_seq();
    let before_provenance = scratch.vault.get(target, before_seq)?.provenance.seq;

    // ---- B1: the layer commit path ----------------------------------------
    let refusal = scratch
        .vault
        .write_cf_batch_with_ledger_entry(
            [(ColumnFamily::Base, base_key(target), base_bytes.clone())],
            EntryKind::Anneal,
            SubjectId::Kernel(b"fsv-2095-anneal-change".to_vec()),
            serde_json::to_vec(&json!({ "kind": "Anneal", "change_id": 1 }))?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )
        .map(|_| ());
    report_refusal(
        "B1 write_cf_batch_with_ledger_entry Anneal/Kernel",
        &refusal,
        BASE_STAMP_UNDECLARED,
        &["anneal/kernel"],
        failures,
    );

    // ---- B2: the multi-constellation anchor path --------------------------
    let anchors = vec![(
        target,
        vec![Anchor {
            kind: AnchorKind::Label("synapse:agent_end_state".to_owned()),
            value: AnchorValue::Enum("completed".to_owned()),
            source: "synapse-outcome-anchors".to_owned(),
            observed_at: CREATED_AT_MS,
            confidence: 1.0,
        }],
    )];
    let refusal = scratch
        .vault
        .anchors_for_many_with_ledger_entry(
            anchors,
            EntryKind::Grounding,
            SubjectId::Lens(LensId::from_parts(
                "fsv-2095-not-a-grounding-subject",
                b"w",
                b"c",
                b"s",
            )),
            serde_json::to_vec(&json!({ "schema": "fsv" }))?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )
        .map(|_| ());
    report_refusal(
        "B2 anchors_for_many_with_ledger_entry Grounding/Lens",
        &refusal,
        BASE_STAMP_UNDECLARED,
        &["grounding/lens"],
        failures,
    );

    // Neither refusal may have written anything.
    let after_seq = scratch.vault.latest_seq();
    let after_provenance = scratch.vault.get(target, after_seq)?.provenance.seq;
    println!(
        "  refused writes left the vault at seq {before_seq} -> {after_seq}, \
         Base provenance {before_provenance} -> {after_provenance}"
    );
    if after_seq != before_seq || after_provenance != before_provenance {
        failures.push(format!(
            "B1/B2: a refused write still moved durable state (seq {before_seq}->{after_seq}, \
             provenance {before_provenance}->{after_provenance}); the refusal must happen before \
             anything is staged"
        ));
    }

    // ---- B3: non-vacuity — a DECLARED pair still commits -------------------
    let declared = scratch.vault.write_cf_batch_with_ledger_entry(
        [(ColumnFamily::Base, base_key(target), base_bytes)],
        EntryKind::Ingest,
        SubjectId::Query(b"fsv-2095-declared-publish".to_vec()),
        serde_json::to_vec(&json!({ "operation": "publish_derived_snapshot" }))?,
        ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
    );
    match declared {
        Ok(_) => {
            let stamped = scratch
                .vault
                .get(target, scratch.vault.latest_seq())?
                .provenance
                .seq;
            let entry = read_ledger_entry(&scratch.vault, stamped)?;
            let members = read_batch_members(&entry.payload)?;
            println!(
                "  B3 a DECLARED ingest/query pair committed and stamped seq {stamped}, \
                 declaring {} member(s)",
                members
                    .total()
                    .map_or_else(|| "no".to_owned(), |total| total.to_string())
            );
            if members.verdict(target) != MemberVerdict::Listed {
                failures.push(
                    "B3: a declared Base-stamping write did not enumerate the row it stamped"
                        .to_owned(),
                );
            }
        }
        Err(error) => failures.push(format!(
            "B3: the gate refused a DECLARED (ingest, query) pair, so it is a blanket refusal \
             rather than a contract: {} {}",
            error.code, error.message
        )),
    }

    // ---- B4: the gate is scoped to Base stamping --------------------------
    // Every in-tree caller of this API writes non-Base rows (the KV/document/
    // relational/timeseries/blob layers and the cross-model transaction). Those
    // stamp no provenance, so the contract must not touch them — including with
    // a pair that would be refused if a Base row were present.
    let graph_write = scratch.vault.write_cf_batch_with_ledger_entry(
        [(
            ColumnFamily::Graph,
            b"fsv-2095-graph-row".to_vec(),
            b"opaque".to_vec(),
        )],
        EntryKind::Anneal,
        SubjectId::Kernel(b"fsv-2095-anneal-change".to_vec()),
        // Deliberately NOT a JSON object: a non-Base batch must not even be
        // required to carry a declarable payload.
        b"not json at all".to_vec(),
        ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
    );
    match graph_write {
        Ok(seq) => println!("  B4 a batch with no Base rows is unaffected by the gate (seq {seq})"),
        Err(error) => failures.push(format!(
            "B4: the Base-stamping contract refused a batch that stamps NO Base row: {} {}",
            error.code, error.message
        )),
    }
    Ok(())
}

fn report_refusal(
    case: &str,
    outcome: &Result<()>,
    code: &str,
    must_name: &[&str],
    failures: &mut Vec<String>,
) {
    match outcome {
        Ok(()) => failures.push(format!(
            "{case}: the write SUCCEEDED; an undeclared shape reached a Base row's provenance"
        )),
        Err(error) => {
            println!("  {case}\n     REFUSED {}: {}", error.code, error.message);
            if error.code != code {
                failures.push(format!(
                    "{case}: expected {code}, observed {}: {}",
                    error.code, error.message
                ));
                return;
            }
            for needle in must_name {
                if !error.message.contains(needle) {
                    failures.push(format!(
                        "{case}: refusal does not name {needle:?}; message={:?}",
                        error.message
                    ));
                }
            }
            if error.remediation.to_lowercase().contains("restic") {
                failures.push(format!(
                    "{case}: a pre-write refusal routes the operator to a restore"
                ));
            }
        }
    }
}

// ===========================================================================
// C — #2094: reproduce's gate must sit on evidence a writer can produce
// ===========================================================================

/// A deterministic frozen-lens stand-in.
///
/// The reproduce path needs a `ReproduceLensRegistry` and a `ForgeBackend` to
/// re-measure through; this instrument supplies the smallest honest pair. The
/// measurement is a pure function of the input bytes, so re-measuring the same
/// recorded input twice necessarily agrees — which is what makes a drift verdict
/// here mean "the ledger evidence resolved and replayed", not "the lens is
/// stable".
struct FsvRegistry {
    lens_id: LensId,
    weights: [u8; 32],
}

impl calyx_ledger::ReproduceLensRegistry for FsvRegistry {
    fn frozen_weights_sha256(&self, lens_id: LensId) -> Result<[u8; 32]> {
        if lens_id == self.lens_id {
            Ok(self.weights)
        } else {
            Err(CalyxError::registry_unavailable(format!(
                "fsv registry holds no lens {lens_id}"
            )))
        }
    }

    fn measure_frozen(&self, _lens_id: LensId, input: &Input) -> Result<SlotVector> {
        let digest = blake3::hash(&input.bytes);
        let data = digest.as_bytes()[..2]
            .iter()
            .map(|byte| f32::from(*byte) / 255.0)
            .collect::<Vec<_>>();
        Ok(SlotVector::Dense { dim: 2, data })
    }
}

struct FsvForge {
    activations: Vec<u64>,
}

impl calyx_ledger::ForgeBackend for FsvForge {
    fn activate_determinism(&mut self, seed: u64) -> Result<()> {
        self.activations.push(seed);
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
fn section_c_reproduce_evidence(
    failures: &mut Vec<String>,
) -> std::result::Result<(), Box<dyn Error>> {
    println!("\n=== C. #2094 — reproduce's gate must sit on evidence that can exist ===");
    println!(
        "  declared writer status: EntryKind::Measure = {:?}, Score = {:?}, \
         Admission = {:?}, AgentForecast = {:?}",
        EntryKind::Measure.writer_status(),
        EntryKind::Score.writer_status(),
        EntryKind::Admission.writer_status(),
        EntryKind::AgentForecast.writer_status(),
    );
    for kind in [
        EntryKind::Measure,
        EntryKind::Score,
        EntryKind::Admission,
        EntryKind::AgentForecast,
    ] {
        if kind.writer_status() != WriterStatus::ReservedUnwritten {
            failures.push(format!(
                "C0: {kind} is declared {:?} but #2094 found no writer for it",
                kind.writer_status()
            ));
        }
    }

    let lens_id = LensId::from_parts("fsv-2094", b"weights", b"corpus", b"2x f32");
    let weights = *blake3::hash(b"fsv-2094-frozen-weights").as_bytes();
    let registry = FsvRegistry { lens_id, weights };
    let input_bytes = b"the recorded input behind one slot measurement".to_vec();
    let input_hash = *blake3::hash(&input_bytes).as_bytes();
    let cx_id = synthetic_cx(7);
    let answer_id = b"fsv-2094-answer".to_vec();

    let recorded_slot = json!({
        "cx_id": cx_id.to_string(),
        "panel_version": SYN_TIMELINE_PANEL_VERSION,
        "slot_id": 103,
        "lens_id": lens_id.to_string(),
        "weights_sha256": hex(&weights),
        "input_hash": hex(&input_hash),
        "forge_seed": 424_242_u64,
        "input": { "modality": "text", "bytes": input_bytes, "pointer": null },
    });

    // ---- C1: the gate is reachable and PASSES on a real Measure entry -----
    {
        let mut appender = LedgerAppender::open(MemoryLedgerStore::default(), SystemClock)?;
        // The production writer surface for a Measure entry. Nothing in the tree
        // calls it today (#2094) — driving it here is what makes the gate
        // reachable at all.
        let measure = appender.append(
            EntryKind::Measure,
            SubjectId::Lens(lens_id),
            serde_json::to_vec(&recorded_slot)?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )?;
        appender.append(
            EntryKind::Answer,
            SubjectId::Query(answer_id.clone()),
            serde_json::to_vec(&answer_payload(cx_id, &[measure.seq]))?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )?;
        let store = appender.into_store();
        let context = build_reproduce_context(&store, &answer_id)?;
        println!(
            "  C1 answer -> measure_refs [{}] resolved {} recorded slot(s) through the real gate",
            measure.seq,
            context.recorded_slots.len()
        );
        if context.recorded_slots.len() != 1 {
            failures.push(format!(
                "C1: the reproduce context resolved {} slots from one measure_ref",
                context.recorded_slots.len()
            ));
        }
        let mut forge = FsvForge {
            activations: Vec::new(),
        };
        let remeasured = calyx_ledger::remeasure_slots(&context, &registry, &mut forge)?;
        println!(
            "  C1 re-measured {} slot(s); forge determinism activated with seed(s) {:?}",
            remeasured.len(),
            forge.activations
        );
        if forge.activations != vec![424_242_u64] {
            failures.push(format!(
                "C1: re-measurement did not activate forge determinism with the recorded seed; \
                 saw {:?}",
                forge.activations
            ));
        }
        if remeasured.len() != 1 {
            failures.push("C1: re-measurement produced no slot from resolved evidence".to_owned());
        }
    }

    // ---- C2: the gate REFUSES a measure_ref aimed elsewhere ---------------
    {
        let mut appender = LedgerAppender::open(MemoryLedgerStore::default(), SystemClock)?;
        let decoy = appender.append(
            EntryKind::Grounding,
            SubjectId::Query(b"not-a-measurement".to_vec()),
            serde_json::to_vec(&recorded_slot)?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )?;
        appender.append(
            EntryKind::Answer,
            SubjectId::Query(answer_id.clone()),
            serde_json::to_vec(&answer_payload(cx_id, &[decoy.seq]))?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )?;
        let store = appender.into_store();
        // The decoy payload IS a valid recorded slot, so without the kind gate
        // the context would resolve happily off an entry that attests nothing
        // about a measurement.
        let outcome = build_reproduce_context(&store, &answer_id).map(|_| ());
        report_refusal(
            "C2 measure_ref -> Grounding entry",
            &outcome,
            REPRODUCE_EVIDENCE_UNAVAILABLE,
            &["grounding", "measure"],
            failures,
        );
    }

    // ---- C3: no evidence at all is refused, not scored --------------------
    {
        let mut appender = LedgerAppender::open(MemoryLedgerStore::default(), SystemClock)?;
        appender.append(
            EntryKind::Answer,
            SubjectId::Query(answer_id.clone()),
            serde_json::to_vec(&answer_payload(cx_id, &[]))?,
            ActorId::Service("ledger-hygiene-contracts-fsv".to_owned()),
        )?;
        let store = appender.into_store();
        let outcome = build_reproduce_context(&store, &answer_id).map(|_| ());
        report_refusal(
            "C3 answer with no recorded_slots and no measure_refs",
            &outcome,
            REPRODUCE_EVIDENCE_UNAVAILABLE,
            &["#2094"],
            failures,
        );
    }

    // ---- C3 discrimination: what the empty context used to score ----------
    let (reproduced, drift) = assert_within_tolerance(&[], &[], 1.0e-3);
    println!(
        "  C3 discrimination: the production scorer still reports empty-vs-empty as \
         reproduced={reproduced} max_drift={drift} — which is exactly the verdict an evidence-free \
         answer used to receive"
    );
    if !reproduced {
        failures.push(
            "C3: empty-vs-empty no longer scores as reproduced, so the hazard this refusal \
             exists to close cannot be demonstrated and the check may be redundant"
                .to_owned(),
        );
    }
    Ok(())
}

/// An answer payload of the shape `reproduce_result_from_remeasured` consumes.
fn answer_payload(cx_id: CxId, measure_refs: &[u64]) -> Value {
    json!({
        "original_hits": [{ "cx_id": cx_id.to_string(), "score": 0.5 }],
        "fusion_weights": {
            "mode": "single_lens",
            "k": 1,
            "candidates": [cx_id.to_string()],
            "single_slot": PanelSlotId::new(SYN_TIMELINE_PANEL_VERSION, SlotId::new(103)),
        },
        "measure_refs": measure_refs,
    })
}

// ===========================================================================
// shared scaffolding
// ===========================================================================

struct Scratch {
    vault: AsterVault,
    ids: Vec<CxId>,
}

fn open_scratch(root: &Path, name: &str) -> std::result::Result<Scratch, Box<dyn Error>> {
    let dir = root.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let vault_id: VaultId = VAULT_ID.parse()?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"ledger-hygiene-contracts-fsv".to_vec(),
        VaultOptions::default(),
    )?;
    let mut ids = Vec::new();
    for tag in 1..=ROWS {
        let constellation = row(vault_id, tag)?;
        ids.push(constellation.cx_id);
        vault.put(constellation)?;
    }
    Ok(Scratch { vault, ids })
}

fn row(
    vault_id: VaultId,
    tag: u8,
) -> std::result::Result<calyx_core::Constellation, Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: 1_785_000_000_000_000_000 + u64::from(tag) * 1_000_000_000,
        kind: TimelineKind::TitleChange,
        actor: TimelineActor::Human,
        app: Some("Notepad.exe".to_owned()),
        payload: json!({ "title": format!("ledger hygiene row {tag} - Notepad") }),
    };
    let raw = serde_json::to_vec(&record)?;
    let context = NativeConstellationContext {
        vault_id,
        cx_id: CxId::from_bytes([0x20, 0x96, tag, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        created_at_ms: CREATED_AT_MS + u64::from(tag),
        next_ledger_seq: u64::from(tag) + 1,
    };
    let key = format!("fsv-ledger-hygiene/timeline-{tag}");
    Ok(build_timeline_constellation(
        context,
        key.as_bytes(),
        &raw,
        &record,
    )?)
}

fn read_ledger_entry(
    vault: &AsterVault,
    seq: u64,
) -> std::result::Result<calyx_ledger::LedgerEntry, Box<dyn Error>> {
    let bytes = vault
        .read_cf_at(vault.latest_seq(), ColumnFamily::Ledger, &ledger_key(seq))?
        .ok_or_else(|| format!("ledger row {seq} is missing"))?;
    Ok(calyx_ledger::decode(&bytes)?)
}

fn synthetic_cx(index: usize) -> CxId {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&(index as u64).to_be_bytes());
    bytes[8] = 0x20;
    bytes[9] = 0x94;
    CxId::from_bytes(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
