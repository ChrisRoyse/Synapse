//! Reproduce-time lens lookup and deterministic slot re-measurement.

mod fusion;

use std::collections::BTreeMap;
use std::str::FromStr;

use calyx_core::{CalyxError, CxId, Input, LensId, PanelSlotId, Result, SlotId, SlotVector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::append::LedgerCfStore;
use crate::codec::decode;
use crate::entry::{HASH_BYTES, LedgerEntry, SubjectId};
use crate::kind::EntryKind;

pub use fusion::{
    FusionMode, FusionWeights, HitRef, REPRODUCE_PAYLOAD_TAG, REPRODUCE_TOLERANCE, ReproduceResult,
    SlotWeight, append_reproduce_entry, assert_reproduced, assert_within_tolerance, reproduce,
    reproduce_payload_bytes, reproduce_verdict, reproduce_verdict_with_input_resolver,
    reproduce_with_input_resolver, rerun_fusion,
};

/// Stable answer identifier used by Lodestar answer entries.
pub type QueryId = Vec<u8>;

/// Minimal registry surface needed by ledger reproduce.
pub trait ReproduceLensRegistry {
    /// Returns the frozen weights hash for the content-addressed lens snapshot.
    fn frozen_weights_sha256(&self, lens_id: LensId) -> Result<[u8; HASH_BYTES]>;

    /// Measures an input with the frozen lens snapshot.
    fn measure_frozen(&self, lens_id: LensId, input: &Input) -> Result<SlotVector>;
}

/// Minimal Forge determinism surface needed before re-measurement.
pub trait ForgeBackend {
    /// Activates deterministic execution with the recorded seed.
    fn activate_determinism(&mut self, seed: u64) -> Result<()>;
}

/// Resolves content-addressed input bytes outside the ledger payload.
pub trait ReproduceInputResolver {
    /// Returns the raw input corresponding to a recorded slot.
    fn resolve_input(&self, slot: &RecordedSlot) -> Result<Input>;
}

/// Ledger-derived reproduce context for one answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReproduceContext {
    pub answer_id: QueryId,
    pub ledger_entries: Vec<LedgerEntry>,
    pub recorded_slots: Vec<RecordedSlot>,
}

/// Slot measurement metadata recorded in Measure/Answer provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedSlot {
    pub cx_id: CxId,
    pub panel_slot: PanelSlotId,
    pub lens_id: LensId,
    pub weights_sha256: [u8; HASH_BYTES],
    pub input_hash: [u8; HASH_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_shard_hash: Option<[u8; HASH_BYTES]>,
    pub forge_seed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Input>,
}

/// Re-measured slot vector produced by the reproduce path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemeasuredSlot {
    pub cx_id: CxId,
    pub panel_slot: PanelSlotId,
    pub lens_id: LensId,
    pub input_hash: [u8; HASH_BYTES],
    pub forge_seed: u64,
    pub vector: SlotVector,
}

/// Input resolver for contexts that intentionally carry inline test inputs.
pub struct InlineInputResolver;

impl ReproduceInputResolver for InlineInputResolver {
    fn resolve_input(&self, slot: &RecordedSlot) -> Result<Input> {
        slot.input.clone().ok_or_else(|| {
            CalyxError::ledger_corrupt(format!(
                "recorded slot {}:{} has no resolved input",
                slot.cx_id, slot.panel_slot
            ))
        })
    }
}

/// A reproduce request named an answer whose slot evidence does not exist.
///
/// This is not drift and not corruption: the ledger row is intact, and nothing
/// about the answer has been shown to differ. There is simply nothing recorded
/// to re-measure, so no verdict can be reached (#2094).
pub const CALYX_REPRODUCE_EVIDENCE_UNAVAILABLE: &str = "CALYX_REPRODUCE_EVIDENCE_UNAVAILABLE";

const EVIDENCE_UNAVAILABLE_REMEDIATION: &str = "the answer entry records no per-slot measurement evidence, so its result cannot be \
     re-measured and no reproduce verdict is possible. Nothing is corrupt and nothing has \
     drifted: wire the writer that records `recorded_slots` (inline) or `measure_refs` (pointing \
     at EntryKind::Measure entries) at the time the answer is produced, then reproduce answers \
     minted after that. Do NOT read the absence of evidence as a passing verdict";

fn evidence_unavailable(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_REPRODUCE_EVIDENCE_UNAVAILABLE,
        message: message.into(),
        remediation: EVIDENCE_UNAVAILABLE_REMEDIATION,
    }
}

/// Reads the answer entry and its referenced Measure entries from the ledger CF.
///
/// # The evidence contract, and why this refuses more than it used to (#2094)
///
/// An answer entry claims its slot measurements either inline (`recorded_slots`)
/// or by reference (`measure_refs`, seqs pointing at entries whose payload is
/// one [`RecordedSlot`]). The reference form is gated on the referenced entry
/// actually being a measurement record — without that gate a `measure_refs` seq
/// could name any entry at all and `recorded_slot_from_value` would either fail
/// confusingly or succeed against a payload that coincidentally carried the
/// right field names.
///
/// A workspace-wide sweep for #2094 found that gate to be unsatisfiable: nothing
/// mints [`EntryKind::Measure`] (it is
/// [`crate::kind::WriterStatus::ReservedUnwritten`]), and nothing mints
/// `measure_refs` or `recorded_slots` either. The whole contract is
/// producer-less, and the gate was dead code that looked like protection.
///
/// The gate is kept — it is the correct check, and `Measure` is the correct kind
/// for the evidence — but it is now expressed against the declared
/// [`EntryKind::carries_recorded_slot`] predicate, and the unreachability is
/// surfaced at the front door instead of being hidden behind it: a context that
/// resolves NO recorded slots is refused with
/// [`CALYX_REPRODUCE_EVIDENCE_UNAVAILABLE`]. It used to be returned empty, and
/// `assert_within_tolerance` scores empty-against-empty as `reproduced = true` —
/// so an answer with no evidence at all could report as successfully reproduced.
/// A reproduce verdict that no measurement backed is worse than no verdict.
pub fn build_reproduce_context(
    cf_reader: &impl LedgerCfStore,
    answer_id: &QueryId,
) -> Result<ReproduceContext> {
    let entries = read_entries(cf_reader)?;
    let answer = entries
        .iter()
        .find(|entry| entry.kind == EntryKind::Answer && answer_matches(entry, answer_id))
        .ok_or_else(|| CalyxError::ledger_corrupt("answer ledger entry not found"))?;
    let payload = payload_value(answer)?;

    let by_seq: BTreeMap<_, _> = entries.iter().map(|entry| (entry.seq, entry)).collect();
    let mut ledger_entries = vec![answer.clone()];
    let mut recorded_slots = recorded_slots_from_value(&payload)?;
    let inline_slots = recorded_slots.len();

    let refs = measure_refs(&payload)?;
    let referenced = refs.len();
    for seq in refs {
        let measure = by_seq.get(&seq).ok_or_else(|| {
            // The referenced seq is genuinely absent from an append-only chain,
            // which is a ledger-content defect.
            CalyxError::ledger_corrupt(format!(
                "answer entry at seq {} references measurement evidence at seq {seq}, which is \
                 not present in the ledger",
                answer.seq
            ))
        })?;
        if !measure.kind.carries_recorded_slot() {
            return Err(evidence_unavailable(format!(
                "answer entry at seq {} references measurement evidence at seq {seq}, but that \
                 entry is a {} entry and only {} entries are declared to carry a recorded slot",
                answer.seq,
                measure.kind,
                EntryKind::Measure
            )));
        }
        recorded_slots.push(recorded_slot_from_value(&payload_value(measure)?)?);
        ledger_entries.push((*measure).clone());
    }

    if recorded_slots.is_empty() {
        return Err(evidence_unavailable(format!(
            "answer entry at seq {} records no per-slot measurement evidence: it carries \
             {inline_slots} inline `recorded_slots` and {referenced} `measure_refs`. No writer in \
             this workspace mints either field, nor any {} entry to point one at (#2094), so \
             re-measurement has nothing to run and an empty replay must NOT be scored as a \
             successful reproduction",
            answer.seq,
            EntryKind::Measure
        )));
    }

    Ok(ReproduceContext {
        answer_id: answer_id.clone(),
        ledger_entries,
        recorded_slots,
    })
}

/// Verifies that the registry snapshot matches the recorded frozen weights hash.
pub fn lookup_frozen_lens(
    registry: &dyn ReproduceLensRegistry,
    lens_id: LensId,
    weights_sha256: &[u8; HASH_BYTES],
) -> Result<()> {
    let actual = registry.frozen_weights_sha256(lens_id)?;
    if &actual == weights_sha256 {
        Ok(())
    } else {
        Err(CalyxError::lens_frozen_violation(format!(
            "lens {lens_id} weights hash does not match recorded Measure entry"
        )))
    }
}

/// Activates deterministic Forge execution for a recorded seed.
pub fn activate_forge_determinism(forge: &mut dyn ForgeBackend, seed: u64) -> Result<()> {
    forge.activate_determinism(seed)
}

/// Re-measures slots using inline inputs carried by the context.
pub fn remeasure_slots(
    ctx: &ReproduceContext,
    registry: &dyn ReproduceLensRegistry,
    forge: &mut dyn ForgeBackend,
) -> Result<Vec<RemeasuredSlot>> {
    remeasure_slots_with_input_resolver(ctx, registry, forge, &InlineInputResolver)
}

/// Re-measures slots using an external content-addressed input resolver.
pub fn remeasure_slots_with_input_resolver(
    ctx: &ReproduceContext,
    registry: &dyn ReproduceLensRegistry,
    forge: &mut dyn ForgeBackend,
    resolver: &dyn ReproduceInputResolver,
) -> Result<Vec<RemeasuredSlot>> {
    let mut out = Vec::with_capacity(ctx.recorded_slots.len());
    for slot in &ctx.recorded_slots {
        lookup_frozen_lens(registry, slot.lens_id, &slot.weights_sha256)?;
        activate_forge_determinism(forge, slot.forge_seed)?;
        let input = resolver.resolve_input(slot)?;
        verify_input_hash(slot, &input)?;
        let vector = registry.measure_frozen(slot.lens_id, &input)?;
        out.push(RemeasuredSlot {
            cx_id: slot.cx_id,
            panel_slot: slot.panel_slot,
            lens_id: slot.lens_id,
            input_hash: slot.input_hash,
            forge_seed: slot.forge_seed,
            vector,
        });
    }
    Ok(out)
}

fn read_entries(cf_reader: &impl LedgerCfStore) -> Result<Vec<LedgerEntry>> {
    cf_reader
        .scan()?
        .into_iter()
        .map(|row| decode(&row.bytes))
        .collect()
}

fn answer_matches(entry: &LedgerEntry, answer_id: &QueryId) -> bool {
    matches!(&entry.subject, SubjectId::Query(bytes) if bytes == answer_id)
}

fn payload_value(entry: &LedgerEntry) -> Result<Value> {
    serde_json::from_slice(&entry.payload).map_err(|error| {
        CalyxError::ledger_corrupt(format!("decode {} payload json: {error}", entry.kind))
    })
}

fn recorded_slots_from_value(value: &Value) -> Result<Vec<RecordedSlot>> {
    let Some(slots) = value.get("recorded_slots") else {
        return Ok(Vec::new());
    };
    let array = slots
        .as_array()
        .ok_or_else(|| CalyxError::ledger_corrupt("recorded_slots must be an array"))?;
    array.iter().map(recorded_slot_from_value).collect()
}

fn measure_refs(value: &Value) -> Result<Vec<u64>> {
    let Some(refs) = value.get("measure_refs") else {
        return Ok(Vec::new());
    };
    let array = refs
        .as_array()
        .ok_or_else(|| CalyxError::ledger_corrupt("measure_refs must be an array"))?;
    array
        .iter()
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| CalyxError::ledger_corrupt("measure_refs entries must be u64"))
        })
        .collect()
}

fn recorded_slot_from_value(value: &Value) -> Result<RecordedSlot> {
    Ok(RecordedSlot {
        cx_id: parse_id(value, "cx_id")?,
        panel_slot: parse_panel_slot(value)?,
        lens_id: parse_id(value, "lens_id")?,
        weights_sha256: parse_hash_field(value, "weights_sha256")?,
        input_hash: parse_hash_field(value, "input_hash")?,
        corpus_shard_hash: parse_optional_hash_field(value, "corpus_shard_hash")?,
        forge_seed: parse_forge_seed(value)?,
        input: parse_optional_input(value)?,
    })
}

fn parse_id<T>(value: &Value, field: &'static str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let text = required(value, field)?
        .as_str()
        .ok_or_else(|| CalyxError::ledger_corrupt(format!("{field} must be a stable string id")))?;
    text.parse::<T>()
        .map_err(|error| CalyxError::ledger_corrupt(format!("parse {field}: {error}")))
}

fn parse_slot_id(value: &Value) -> Result<SlotId> {
    let raw = required(value, "slot_id")?;
    if let Some(slot) = raw.as_u64() {
        return u16::try_from(slot)
            .map(SlotId::new)
            .map_err(|_| CalyxError::ledger_corrupt("slot_id exceeds u16"));
    }
    let text = raw
        .as_str()
        .ok_or_else(|| CalyxError::ledger_corrupt("slot_id must be u16 or string"))?;
    text.parse::<SlotId>()
        .map_err(|error| CalyxError::ledger_corrupt(format!("parse slot_id: {error}")))
}

fn parse_panel_slot(value: &Value) -> Result<PanelSlotId> {
    let panel_version = required(value, "panel_version")?
        .as_u64()
        .ok_or_else(|| CalyxError::ledger_corrupt("panel_version must be u32"))
        .and_then(|raw| {
            u32::try_from(raw).map_err(|_| CalyxError::ledger_corrupt("panel_version exceeds u32"))
        })?;
    Ok(PanelSlotId::new(panel_version, parse_slot_id(value)?))
}

fn parse_hash_field(value: &Value, field: &'static str) -> Result<[u8; HASH_BYTES]> {
    parse_hash_value(required(value, field)?, field)
}

fn parse_optional_hash_field(
    value: &Value,
    field: &'static str,
) -> Result<Option<[u8; HASH_BYTES]>> {
    value
        .get(field)
        .map(|value| parse_hash_value(value, field))
        .transpose()
}

fn parse_hash_value(value: &Value, field: &'static str) -> Result<[u8; HASH_BYTES]> {
    if let Some(text) = value.as_str() {
        return parse_hex_32(text, field);
    }
    let bytes: Vec<u8> = serde_json::from_value(value.clone())
        .map_err(|error| CalyxError::ledger_corrupt(format!("parse {field}: {error}")))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        CalyxError::ledger_corrupt(format!(
            "{field} has {} bytes, expected {HASH_BYTES}",
            bytes.len()
        ))
    })
}

fn parse_forge_seed(value: &Value) -> Result<u64> {
    match value.get("forge_seed").and_then(Value::as_u64) {
        Some(seed) => Ok(seed),
        None => Err(CalyxError::reproduce_nondeterministic(
            "Measure payload is missing forge_seed",
        )),
    }
}

fn parse_optional_input(value: &Value) -> Result<Option<Input>> {
    value
        .get("input")
        .map(|input| {
            serde_json::from_value(input.clone()).map_err(|error| {
                CalyxError::ledger_corrupt(format!("decode recorded input: {error}"))
            })
        })
        .transpose()
}

fn verify_input_hash(slot: &RecordedSlot, input: &Input) -> Result<()> {
    let actual = *blake3::hash(&input.bytes).as_bytes();
    if actual == slot.input_hash {
        Ok(())
    } else {
        Err(CalyxError::ledger_corrupt(format!(
            "resolved input hash mismatch for {}:{}",
            slot.cx_id, slot.panel_slot
        )))
    }
}

fn required<'a>(value: &'a Value, field: &'static str) -> Result<&'a Value> {
    value
        .get(field)
        .ok_or_else(|| CalyxError::ledger_corrupt(format!("missing {field}")))
}

fn parse_hex_32(value: &str, field: &'static str) -> Result<[u8; HASH_BYTES]> {
    if value.len() != HASH_BYTES * 2 {
        return Err(CalyxError::ledger_corrupt(format!(
            "{field} hex has {} chars, expected {}",
            value.len(),
            HASH_BYTES * 2
        )));
    }
    let mut out = [0_u8; HASH_BYTES];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_value(chunk[0])
            .ok_or_else(|| CalyxError::ledger_corrupt(format!("{field} contains non-hex digit")))?;
        let lo = hex_value(chunk[1])
            .ok_or_else(|| CalyxError::ledger_corrupt(format!("{field} contains non-hex digit")))?;
        out[index] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
