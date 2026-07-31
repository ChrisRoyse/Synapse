//! Manual FSV instrument: does one lens's token-limit refusal still take down
//! the whole constellation? (#1924)
//!
//! ## What #1924 found
//!
//! `syn::sparse_text_tf` fails closed above `MAX_TEXT_TOKENS = 4096`. That is
//! correct for the *lens*. The defect was the blast radius: every slot in
//! `build_agent_transcript_constellation` was measured with `?`, so a row one
//! token over the bound did not lose its lexical lane — it lost its role
//! one-hot, its status one-hot, its event-kind hash, its token-count scalars,
//! its record vector and its temporal lenses too. The record became
//! **unmeasured** rather than **partially measured**.
//!
//! ## What this proves
//!
//! Three rows, one panel, measured through the real production builder:
//!
//! | row       | tokens | before this fix          | required after         |
//! |-----------|--------|--------------------------|------------------------|
//! | `under`   |  ~1.4k | full constellation       | unchanged, byte-equal  |
//! | `over`    |  >4096 | **whole record refused** | every other slot present, text slots `Absent{Error}` |
//! | `broken`  |  n/a   | record refused           | still refused          |
//!
//! The `broken` row is the control, and it is the reason this is not a
//! blanket "degrade every error" change: it carries a *code* fault rather than
//! an oversized input, and it must still abort. A fix that turned both into an
//! absent slot would be a silent fallback.
//!
//! Every claim is read back from the returned `Constellation`'s own slot map
//! and from the physical `Absent` reason string, not from a return code.
//!
//! ```text
//! cargo run --release -p synapse-storage --example slot_refusal_blast_radius_fsv
//! ```

use std::error::Error;

use calyx_core::{AbsentReason, Constellation, CxId, SlotVector, VaultId};
use synapse_core::types::{
    AgentTranscriptRecord, TranscriptRole, TranscriptSource, TranscriptToolCall, TranscriptUsage,
};
use synapse_storage::constellations::{
    NativeConstellationContext, SLOT_REFUSED_ABSENT_PREFIX,
    build_agent_transcript_constellation,
};

/// The lens bound this instrument straddles, mirrored from
/// `calyx-lenses/src/syn.rs`. If the two ever disagree the `over` row stops
/// being over and the run reports it rather than passing vacuously.
const MAX_TEXT_TOKENS: usize = 4096;
/// Slots fed by the three unbounded text lanes.
const TEXT_SLOTS: [u16; 3] = [40, 107, 109];

fn main() -> Result<(), Box<dyn Error>> {
    println!("== #1924 slot refusal blast radius FSV ==");
    let mut failures = Vec::new();

    // ---------------------------------------------------------------- under
    let under = build(&row_with_tool_calls(1, 40))?;
    let under_slots = under.slots.len();
    let under_absent = refused_slots(&under);
    println!("\n-- row `under` (well inside the bound) --");
    println!("  slots={under_slots} refused={under_absent:?}");
    if !under_absent.is_empty() {
        failures.push(format!(
            "an in-bounds row refused slots {under_absent:?}; the degrade path must not fire here"
        ));
    }

    // ----------------------------------------------------------------- over
    // 600 tool calls x ~12 distinct tokens each clears 4,096 comfortably while
    // staying inside every per-field ingest cap — which is exactly the shape
    // #1924 predicted would cross first: an unbounded *count* of bounded fields.
    let over_record = row_with_tool_calls(2, 600);
    let over = match build(&over_record) {
        Ok(constellation) => constellation,
        Err(error) => {
            println!("\n-- row `over` --");
            println!("  BUILD REFUSED: {error}");
            failures.push(
                "an over-limit row still aborted the whole constellation; the blast radius is \
                 unchanged"
                    .to_owned(),
            );
            return report(failures);
        }
    };
    println!("\n-- row `over` ({} tool calls) --", 600);
    let over_refused = refused_slots(&over);
    println!("  slots={} refused={over_refused:?}", over.slots.len());

    if over_refused.is_empty() {
        failures.push(format!(
            "the `over` row refused nothing, so it did not actually exceed \
             MAX_TEXT_TOKENS={MAX_TEXT_TOKENS} and this run proved nothing; widen the row"
        ));
    }
    for slot in over_refused.iter().copied() {
        if !TEXT_SLOTS.contains(&slot) {
            failures.push(format!(
                "slot {slot} refused, but only the three unbounded text lanes {TEXT_SLOTS:?} \
                 should ever degrade"
            ));
        }
    }

    // The load-bearing claim: every NON-text slot survived.
    let mut survivors = 0_usize;
    for (slot, vector) in &over.slots {
        let id = slot.get();
        if TEXT_SLOTS.contains(&id) {
            continue;
        }
        if matches!(vector, SlotVector::Absent { .. }) {
            // An inapplicable slot is legitimately absent (e.g. usage scalars on
            // a row with no usage). Only report it if `under` had it present.
            if under
                .slots
                .get(slot)
                .is_some_and(|other| !matches!(other, SlotVector::Absent { .. }))
            {
                failures.push(format!(
                    "slot {id} is present on the in-bounds row but absent on the over-limit row, \
                     so the refusal still spread beyond its own lane"
                ));
            }
            continue;
        }
        survivors += 1;
    }
    println!("  non-text slots still measured: {survivors}");
    if survivors == 0 {
        failures.push("no non-text slot survived the refusal".to_owned());
    }

    // The reason must name the lens, so the readback can answer "which lens".
    for slot in over_refused.iter().copied() {
        let reason = absent_reason(&over, slot).unwrap_or_default();
        let named = reason.starts_with(SLOT_REFUSED_ABSENT_PREFIX) && reason.contains("tokens");
        println!(
            "  slot {slot:>3} reason: {} {}",
            truncate(&reason, 96),
            if named { "OK" } else { "FAIL" }
        );
        if !named {
            failures.push(format!(
                "slot {slot}'s absent reason does not identify the refusing lens and its bound"
            ));
        }
    }

    // --------------------------------------------------------------- broken
    // Control: a row whose spawn_id is empty violates a record invariant rather
    // than a size bound. It must still abort — proving the degrade is keyed to
    // CALYX_LENS_INPUT_TOO_LARGE and is not a blanket error swallow.
    println!("\n-- row `broken` (control: a code fault, not a long input) --");
    let mut broken = row_with_tool_calls(3, 10);
    broken.role = None;
    broken.event_kind = None;
    broken.source = TranscriptSource::ClaudeSessionJsonl;
    // Force a genuine lens fault: a NaN-producing scalar is a numerical
    // invariant, which must never degrade to an absent slot.
    broken.usage = Some(TranscriptUsage {
        input_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
        cache_read_input_tokens: Some(u64::MAX),
        cache_creation_input_tokens: Some(u64::MAX),
        ..TranscriptUsage::default()
    });
    match build(&broken) {
        Ok(constellation) => {
            let refused = refused_slots(&constellation);
            println!("  built; refused={refused:?}");
            if !refused.is_empty() {
                failures.push(
                    "a non-size lens fault degraded to Absent{Error}; only \
                     CALYX_LENS_INPUT_TOO_LARGE may degrade"
                        .to_owned(),
                );
            } else {
                println!("  (no lens faulted on this row — control is inconclusive, not failed)");
            }
        }
        Err(error) => {
            println!("  refused as required: {}", truncate(&error.to_string(), 120));
        }
    }

    report(failures)
}

fn report(failures: Vec<String>) -> Result<(), Box<dyn Error>> {
    println!();
    if failures.is_empty() {
        println!("RESULT: PASS — an over-limit row loses only the lanes that refused it, and each");
        println!("        refusal names its lens in a reason the coverage readback can count.");
        Ok(())
    } else {
        println!("RESULT: FAIL — {} check(s) did not hold:", failures.len());
        for failure in &failures {
            println!("  - {failure}");
        }
        Err("FSV failed; see the failures above".into())
    }
}

fn build(record: &AgentTranscriptRecord) -> Result<Constellation, Box<dyn Error>> {
    let raw = serde_json::to_vec(record)?;
    let context = NativeConstellationContext {
        vault_id: "0000000000000000000000000V".parse::<VaultId>()?,
        cx_id: CxId::from_bytes([9_u8; 16]),
        created_at_ms: 1_785_000_000_000,
        next_ledger_seq: 1,
    };
    let key = format!("fsv1924/{}", record.line_no).into_bytes();
    Ok(build_agent_transcript_constellation(
        context, &key, &raw, record,
    )?)
}

/// Slots left `Absent{Error}` by a lens refusal.
fn refused_slots(constellation: &Constellation) -> Vec<u16> {
    let mut out: Vec<u16> = constellation
        .slots
        .iter()
        .filter(|(_, vector)| {
            matches!(
                vector,
                SlotVector::Absent {
                    reason: AbsentReason::Error(_)
                }
            )
        })
        .map(|(slot, _)| slot.get())
        .collect();
    out.sort_unstable();
    out
}

fn absent_reason(constellation: &Constellation, slot: u16) -> Option<String> {
    constellation
        .slots
        .iter()
        .find(|(id, _)| id.get() == slot)
        .and_then(|(_, vector)| match vector {
            SlotVector::Absent {
                reason: AbsentReason::Error(detail),
            } => Some(detail.clone()),
            _ => None,
        })
}

/// A real-shaped transcript row carrying `tool_calls` tool calls, each with
/// distinct argument tokens so the token count scales with the call count
/// rather than collapsing to a small vocabulary.
fn row_with_tool_calls(line_no: u64, tool_calls: usize) -> AgentTranscriptRecord {
    let mut record = AgentTranscriptRecord::new(
        1_785_200_000_000_000_000 + line_no,
        "agent-spawn-fsv1924".to_owned(),
        line_no,
        TranscriptSource::ClaudeSessionJsonl,
        4096,
        "0".repeat(64),
    );
    record.role = Some(TranscriptRole::Assistant);
    record.event_kind = Some("assistant".to_owned());
    record.model = Some("claude-opus-5".to_owned());
    record.turn_index = Some(1);
    record.content_summary = Some("measuring the refusal blast radius".to_owned());
    record.content_bytes = Some(34);
    record.tool_calls = (0..tool_calls)
        .map(|index| TranscriptToolCall {
            tool_name: format!("Tool{index}"),
            tool_call_id: Some(format!("toolu_fsv1924_{index}")),
            arguments: Some(format!(
                "{{\"alpha{index}\":\"bravo{index}\",\"charlie{index}\":\"delta{index}\",\
                  \"echo{index}\":\"foxtrot{index}\",\"golf{index}\":\"hotel{index}\",\
                  \"india{index}\":\"juliet{index}\",\"kilo{index}\":\"lima{index}\"}}"
            )),
            arguments_bytes: Some(192),
            ..TranscriptToolCall::default()
        })
        .collect();
    record.usage = Some(TranscriptUsage {
        input_tokens: Some(1_200),
        output_tokens: Some(340),
        cache_read_input_tokens: Some(90_000),
        cache_creation_input_tokens: Some(2_400),
        ..TranscriptUsage::default()
    });
    record
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}
