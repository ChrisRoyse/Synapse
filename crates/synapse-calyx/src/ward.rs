//! Ward guard calibration and verification against the vault (#1677).
//!
//! # Why this module exists
//!
//! `calyx-search` already consumes a calibrated Ward [`GuardProfile`]: profile
//! backed `--guard in-region` loads the Guard CF row `profile\0default` and
//! fails closed with `CALYX_GUARD_PROVISIONAL` when it is absent. Until now no
//! `synapse-*` crate depended on `calyx-ward` at all, so nothing ever wrote that
//! row and the consumer could only ever fail. This module is the writer.
//!
//! # The calibration corpus contract, and why it is narrow on purpose
//!
//! Ward's tau is a **conformal / Clopper-Pearson** threshold: it is chosen as
//! the smallest cosine at which the *observed* false-accept count over a corpus
//! of KNOWN-BAD cases is low enough that the true false-accept rate is at most
//! `target_far` with confidence `1 - alpha` (Vovk et al.'s training-conditional
//! validity; the bound is the exact binomial inversion of Clopper-Pearson 1934).
//! That guarantee is a statement about a genuine bad-case distribution. Feed it
//! synthetic badness and the number it prints is not a weaker guarantee, it is
//! **no guarantee at all** — and it prints `ok` forever. So this module never
//! manufactures a bad case.
//!
//! The corpus is therefore drawn from what the vault can state about itself
//! without *inference*. Two sources qualify, and the difference between them is
//! the whole point:
//!
//! 1. An [`Anchor`] whose value is [`AnchorValue::Bool`] with `confidence > 0`.
//!    `Bool(true)` is an adjudicated good outcome, `Bool(false)` an adjudicated
//!    bad one. No interpretation is involved at all.
//! 2. An [`AnchorValue::Enum`] whose **kind** appears in
//!    [`SYNAPSE_DECLARED_ENUM_ADJUDICATIONS`] — a closed, hand-written table in
//!    which a specific anchor kind declares which of its values means "good",
//!    citing the production code that already makes that same split.
//!
//! `Number`, `Text`, `OneHot`, `Vector`, and any enum kind **not** in that
//! table, have no polarity here and are counted as *unadjudicated* and
//! reported, never scored.
//!
//! Source 2 is not a weakening of the honesty rule, and it is worth being
//! precise about why. The rule that matters is *never invent a bad case*. A
//! declared enum adjudication invents nothing: the outcomes are real, observed,
//! already-recorded failures, and the mapping from value to polarity is not
//! guessed by Ward but copied from the subsystem that writes the anchor and
//! already partitions the same field the same way. What Ward must never do is
//! decide on its own that some unfamiliar enum value looks like a failure —
//! and it still cannot, because the table is exhaustive and fails closed on
//! anything absent from it.
//!
//! The score function is the one the guard actually enforces with —
//! `dense_cosine(produced, matched)` — so calibration and deployment measure the
//! same quantity:
//!
//! - `bad_scores[j]`  = max cosine of bad record `j` against the good set.
//! - `good_scores[i]` = max cosine of good record `i` against the good set with
//!   itself removed (leave-one-out; including itself would score a constant 1.0
//!   and report a fictitious FRR of 0).
//!
//! # Where the corpus has to come from
//!
//! This section used to say Synapse had no surface writing `Bool(false)`
//! outcome anchors. That was wrong, and measuring the live vault is what showed
//! it. Three surfaces write adjudicated Bool outcomes today:
//!
//! | writer | anchor kind | polarity |
//! |---|---|---|
//! | `agent_events.rs` | `synapse:agent_tool_call_success` | `Bool(!error_present)` — a failed tool call IS a `Bool(false)` |
//! | `verification.rs` | `synapse:verification_outcome` | `Bool(code_count > 0)` |
//! | `mcp_usage.rs` | `synapse:mcp_steering_enabled` | `Bool(policy.enabled)` |
//!
//! The real blocker was narrower and structural, and naming it wrongly sent the
//! operator to build a path that already exists. Calibration used to be pinned
//! to the **single durable active panel**, and on this vault that is
//! `syn-timeline-v1`, which receives none of the anchors above. Measured on
//! 2026-07-30:
//!
//! ```text
//! panel 1900001 (active, timeline)  548 scanned  0 good  0 bad  548 unadjudicated
//! panel 1665001 (agent-event)       carries synapse:agent_tool_call_success,
//!                                   but calibration refuses it: not the active panel
//! ```
//!
//! # Why that pin existed, and why it is gone (#1919)
//!
//! It read as a deadlock between two reasonable rules. It was not. Both halves
//! traced to one thing that had nothing to do with evidence:
//!
//! 1. **The panel lookup.** Calibration needs a [`Panel`] for exactly one
//!    purpose — proving each named slot is dense and `Active`. A vault manifest
//!    publishes exactly *one* `Panel` snapshot, so that lookup could only ever
//!    succeed for the active panel. The pin was standing in for a missing
//!    definition lookup, and it was enforced with an error that told the
//!    operator the active panel was the only legal target.
//! 2. **The profile key.** The Guard CF wrote every profile to one constant key,
//!    `profile\0default`. With a single-key namespace a second panel's profile
//!    could only overwrite the first, so allowing a second panel would have been
//!    genuinely unsafe — which is what made the pin look load-bearing.
//!
//! Both are fixed by construction rather than by relaxing a gate. The caller
//! supplies the panel definition (Synapse's panels are code-declared and
//! content-addressed, so any generation reconstructs deterministically and its
//! lens ids hash to the same frozen contracts ingest measured with), and
//! profiles are keyed by panel version, with `profile\0default` kept as a mirror
//! written **only** for the durable active panel so `calyx-search`'s guarded
//! reader can never pick up a profile for a panel it is not serving.
//!
//! Nothing about the evidence requirement moved: the corpus scan, the
//! both-polarities requirement, the Clopper-Pearson certification, and the
//! refusal to manufacture a bad case are all unchanged. A guard that will not
//! calibrate is safe; a guard calibrated on invented badness is a lie with a
//! confidence interval printed next to it.
//!
//! The corpus this unlocks, measured on the live vault 2026-07-31:
//!
//! ```text
//! panel 1776006 (syn-mcp-usage-v1)  1,100 records  grounded_fraction 1.0000  provisional=false
//!                                   anchor synapse:mcp_tool_call_outcome, a
//!                                   SYNAPSE_DECLARED_ENUM_ADJUDICATIONS kind
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::mvcc::CfRead;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{
    AnchorKind, AnchorValue, CalyxError, Clock, CxId, DENSE_COSINE_SCORING_ENGINE, Panel, SlotId,
    SlotVector, Ts, dense_cosine,
};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use calyx_registry::load_vault_panel_state;
use calyx_ward::{
    CalibrationInput, GuardId, GuardPolicy, GuardProfile, JOINT_POLICY_ESTIMATOR, MIN_BAD_SCORES,
    NoveltyAction, SlotKind, calibrate, guard, validate_calibration_slots,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::{
    SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxPersistedNoveltyFinding, SynapseCalyxVault,
    drift::reactive_novelty_key,
};

/// Guard CF key of the default calibrated profile.
///
/// This MUST stay byte-identical to
/// `calyx_search::engine::guard::DEFAULT_GUARD_PROFILE_KEY` (private to that
/// crate) — it is the single rendezvous point between this writer and the
/// profile-backed guarded-search reader.
pub const SYNAPSE_GUARD_DEFAULT_PROFILE_KEY: &[u8] = b"profile\0default";

/// Default conformal miscoverage budget: the calibrated tau bounds the true FAR
/// at `target_far` with confidence `1 - alpha`.
pub const SYNAPSE_GUARD_DEFAULT_ALPHA: f32 = 0.05;

/// Minimum good (in-region) exemplars needed per slot. Two is the arithmetic
/// floor for a leave-one-out nearest-neighbour score; it is not a statistical
/// sufficiency claim and is not reported as one.
pub const SYNAPSE_GUARD_MIN_GOOD_SCORES: usize = 2;

/// Binary serving-artifact contract. Calibration publishes this immutable
/// generation beside the Ward profile; verification never reconstructs it by
/// scanning the calibration corpus.
const SYNAPSE_GUARD_SERVING_SCHEMA: &str = "synapse.calyx.ward.serving.v1";

/// A corrupt or pathologically large Guard CF row must not make the
/// authorization path allocate without bound. This is a hard format ceiling,
/// not a truncation target: calibration and readback fail closed above it.
const SYNAPSE_GUARD_SERVING_MAX_BYTES: usize = 512 * 1024 * 1024;

/// One slot to calibrate, with the aspect the operator asserts it carries.
///
/// The aspect is REQUIRED and never inferred: it sets the maximum permitted
/// `target_far` (`identity` 0.01, `content` 0.03, `stylistic` 0.05) and is
/// persisted into the profile's per-slot calibration metadata. Guessing it from
/// a slot id would silently relax or tighten the guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardSlotSpec {
    pub slot: u16,
    pub aspect: SynapseCalyxGuardAspect,
}

/// Operator-asserted slot aspect (mirrors `calyx_ward::SlotKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxGuardAspect {
    Identity,
    Stylistic,
    Content,
}

impl SynapseCalyxGuardAspect {
    #[must_use]
    pub const fn slot_kind(self) -> SlotKind {
        match self {
            Self::Identity => SlotKind::Identity,
            Self::Stylistic => SlotKind::Stylistic,
            Self::Content => SlotKind::Content,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        self.slot_kind().label()
    }
}

/// Bounded request for one guard calibration pass.
#[derive(Clone, Debug)]
pub struct SynapseCalyxGuardCalibrateParams {
    pub panel_version: u32,
    pub slots: Vec<SynapseCalyxGuardSlotSpec>,
    pub domain: String,
    /// Optional exact grounded anchor axis used for good/bad adjudication.
    /// Omitting it preserves the legacy all-adjudicable-anchor behavior.
    pub anchor_kind: Option<String>,
    pub alpha: f32,
    /// Per-slot target false-accept rate. `None` uses the aspect's maximum.
    pub target_far: Option<f32>,
    pub max_records: usize,
    /// When false the calibration is computed and reported but the Guard CF is
    /// not written (a dry run for operators sizing a corpus).
    pub persist: bool,
    /// Explicit disposition for an otherwise valid calibrated OOD verdict.
    /// Security-sensitive callers keep `RejectClosed`; learning panels may
    /// select `NewRegion`, and identity boundaries may select `Quarantine`.
    pub novelty_action: NoveltyAction,
    /// The panel definition to validate the named slots against, when the
    /// requested panel is **not** the durable active one (#1919).
    ///
    /// Calibration needs a `Panel` for exactly one purpose:
    /// `validate_calibration_slots` must prove every named slot is dense and
    /// `Active`. A vault manifest publishes exactly one `Panel` snapshot, so
    /// that lookup could only ever succeed for the active panel — which is why
    /// calibration was pinned to it, and why the guard was uncertifiable on a
    /// vault whose active panel carries no adjudicated outcome while another
    /// panel carries 1,100 fully-adjudicated ones.
    ///
    /// The pin was never protecting the *evidence*; it was standing in for a
    /// missing definition lookup. Supplying the definition removes the pin
    /// without loosening anything: the corpus scan, the bad-case requirement,
    /// the Clopper-Pearson certification and the refusal to manufacture badness
    /// are all unchanged, and the version is still checked to match.
    ///
    /// `None` keeps the original behaviour exactly — resolve the durable active
    /// panel and refuse anything else.
    pub calibration_panel: Option<Panel>,
}

impl SynapseCalyxGuardCalibrateParams {
    #[must_use]
    pub fn new(panel_version: u32, slots: Vec<SynapseCalyxGuardSlotSpec>) -> Self {
        Self {
            panel_version,
            slots,
            domain: "default".to_owned(),
            anchor_kind: None,
            alpha: SYNAPSE_GUARD_DEFAULT_ALPHA,
            target_far: None,
            max_records: crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            persist: true,
            novelty_action: NoveltyAction::RejectClosed,
            calibration_panel: None,
        }
    }
}

/// One slot's calibration evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardSlotCalibration {
    pub slot: u16,
    pub aspect: String,
    pub target_far: f32,
    pub tau: f32,
    /// Observed false-accept count at `tau` over the bad corpus.
    pub bad_accepts: usize,
    pub achieved_far: f64,
    pub achieved_frr: f64,
    pub good_scores: usize,
    pub bad_scores: usize,
    /// Per-slot diagnostic Clopper-Pearson tail. For a multi-slot joint-policy
    /// profile, `policy_clopper_pearson_tail` is the authoritative certificate;
    /// an individual slot is allowed to accept a heterogeneous bad case that
    /// another required slot rejects.
    pub clopper_pearson_tail: f64,
    /// Smallest bad-corpus size at which `target_far` is certifiable at `alpha`.
    pub certifiable_min_bad_scores: usize,
}

/// Result of one guard calibration pass with the physical Guard CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each Boolean is an independently read-back physical certification claim exposed to the MCP caller"
)]
pub struct SynapseCalyxGuardCalibrateReport {
    pub panel_version: u32,
    pub domain: String,
    pub anchor_kind: Option<String>,
    pub guard_id: String,
    pub alpha: f32,
    pub novelty_action: String,
    pub records_scanned: usize,
    pub adjudicated_good: usize,
    pub adjudicated_bad: usize,
    pub unadjudicated: usize,
    pub conflicting: usize,
    /// Adjudicated records dropped because they carry none of the requested
    /// guard slots. A large value here against a healthy `adjudicated_*` count
    /// means the named slots are wrong for this panel, not that the corpus is.
    pub adjudicated_without_guarded_slots: usize,
    /// Adjudicated rows carrying only a strict subset of a multi-slot joint
    /// Guard roster. They are excluded as a named physical population rather
    /// than letting each slot silently calibrate on different identities.
    pub adjudicated_incomplete_guarded_slots: usize,
    pub adjudicated_incomplete_guarded_slots_sha256: String,
    pub adjudicated_incomplete_guarded_slots_sample: Vec<String>,
    pub estimator: String,
    /// Backend executing the canonical calibration/serving scorer.
    pub scoring_backend: String,
    /// The score path used to turn dense corpus rows into cosine-equivalent
    /// nearest-neighbour scores.
    pub scoring_engine: String,
    /// Maximum calibration-to-serving cosine deviation conservatively folded
    /// into every bad/good score before tau selection.
    pub scoring_tolerance: f32,
    /// Frozen serving combination policy whose actual accept/reject decision
    /// was conformally certified over aligned physical bad rows.
    pub policy: String,
    pub policy_bad_accepts: usize,
    pub policy_achieved_far: f64,
    pub policy_achieved_frr: f64,
    pub policy_clopper_pearson_tail: f64,
    pub policy_certified: bool,
    pub slots: Vec<SynapseCalyxGuardSlotCalibration>,
    pub persisted: bool,
    /// Byte length of the Guard CF row read back after the write.
    pub guard_cf_profile_bytes: usize,
    /// SHA-256 of the exact persisted profile bytes that bind the serving
    /// generation.
    pub guard_cf_profile_sha256: String,
    /// Byte length of the immutable trusted-exemplar serving artifact.
    pub guard_cf_serving_bytes: usize,
    /// SHA-256 of the exact serving-artifact row read back from Guard CF.
    pub guard_cf_serving_sha256: String,
    pub guard_cf_rows_after: usize,
    /// Proof the read-back row decodes as a calibrated profile.
    pub readback_calibrated: bool,
    /// Proof the separately read serving row is schema-valid and bound to the
    /// exact profile generation above.
    pub readback_serving_bound: bool,
}

/// Request for one guard verification. Verification performs only point reads:
/// the corpus bound belongs to calibration, never the serving path.
#[derive(Clone, Debug)]
pub struct SynapseCalyxGuardVerifyParams {
    pub panel_version: u32,
    pub query_cx_id: String,
    pub high_stakes: bool,
}

/// One slot's verdict inside a guard verification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardSlotVerdict {
    pub slot: u16,
    pub cos: f32,
    pub tau: f32,
    pub pass: bool,
    pub matched_cx_id: String,
}

/// A `calyx_ward::GuardVerdict` produced against the persisted profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardVerifyReport {
    pub panel_version: u32,
    pub query_cx_id: String,
    pub guard_id: String,
    pub domain: String,
    pub calibration_anchor_kind: Option<String>,
    pub high_stakes: bool,
    pub overall_pass: bool,
    pub provisional: bool,
    pub policy: String,
    pub required_slots: Vec<u16>,
    pub per_slot: Vec<SynapseCalyxGuardSlotVerdict>,
    pub failing_slots: Vec<u16>,
    pub action: Option<String>,
    pub calibration_far: Option<f32>,
    pub calibration_frr: Option<f32>,
    pub calibration_confidence: Option<f32>,
    pub trusted_exemplars: usize,
    /// Exact immutable generation used for this decision.
    pub guard_cf_profile_sha256: String,
    pub guard_cf_serving_sha256: String,
    /// Physical append-only Ledger row sealing this exact verdict.
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

/// One enum anchor kind whose value set carries a **declared** good/bad
/// polarity, and the production code that declares it.
///
/// Ward accepts a polarity here only when some other part of the system already
/// partitions the identical field the identical way. `declared_by` is not a
/// comment — it is the evidence that this entry copies an existing decision
/// rather than inventing one, and an entry that cannot cite such a site does
/// not belong in the table.
#[derive(Clone, Copy, Debug)]
pub struct DeclaredEnumAdjudication {
    /// The anchor kind label, matching [`AnchorKind::Label`].
    pub kind_label: &'static str,
    /// The single value that means "good". Every other value of this kind is
    /// adjudicated bad.
    pub good_value: &'static str,
    /// Where in the codebase this same partition is already made.
    pub declared_by: &'static str,
}

/// The closed set of enum anchor kinds Ward may adjudicate.
///
/// Adding an entry is a deliberate act with a burden of proof: name the code
/// that already splits the field this way. Removing one only ever narrows what
/// the guard will calibrate on, which is the safe direction.
///
/// Measured on the live vault 2026-07-30: `synapse:mcp_tool_call_outcome`
/// covers 1,002 records at 1.0000 grounded coverage on panel 1776006, split
/// 791 good / 211 bad. The 211 is confirmed twice over and independently — the
/// `syn.mcp_usage.error_onehot.v1` lane is present on exactly 211 of those
/// records (every other full-coverage lane reports 1,002), and the assay's
/// measured anchor entropy of 0.7421873 bits solves to p = 0.2107, i.e.
/// 211/1002. That is the only corpus on this vault with both polarities in
/// quantity.
pub const SYNAPSE_DECLARED_ENUM_ADJUDICATIONS: &[DeclaredEnumAdjudication] =
    &[DeclaredEnumAdjudication {
        kind_label: "synapse:mcp_tool_call_outcome",
        good_value: "ok",
        declared_by: "crates/synapse-mcp/src/server/mcp_usage.rs — usage_evidence() counts \
                      `status == \"ok\"` as success_rows and `status != \"ok\"` as error_rows \
                      over these exact CF_KV rows",
    }];

/// The declared verdict for one enum anchor, or `None` when this kind has no
/// declared polarity and must stay unadjudicated.
fn declared_enum_verdict(kind: &AnchorKind, value: &str) -> Option<bool> {
    let AnchorKind::Label(label) = kind else {
        // Only `Label` kinds are addressable by name. The structural kinds
        // (TestPass, Thumbs, Reward, ...) carry their own semantics and are not
        // routed through this table.
        return None;
    };
    SYNAPSE_DECLARED_ENUM_ADJUDICATIONS
        .iter()
        .find(|declared| declared.kind_label == label)
        .map(|declared| value == declared.good_value)
}

/// One record's adjudicated slot vectors.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AdjudicatedRecord {
    cx_id: CxId,
    slots: BTreeMap<u16, Vec<f32>>,
    /// Source-row pointer used only while building calibration diagnostics.
    /// Trusted serving artifacts deliberately omit it: online scoring needs
    /// vectors and constellation identity, not a source-system address.
    #[serde(skip)]
    source_pointer: Option<String>,
}

/// Frozen trusted-region vectors used by the online guard. The profile hash is
/// a consistency token: thresholds and exemplars cannot be mixed across
/// calibration generations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GuardServingArtifact {
    schema: String,
    panel_version: u32,
    guard_id: String,
    profile_sha256: String,
    required_slots: Vec<u16>,
    trusted_exemplars: Vec<AdjudicatedRecord>,
}

/// Process-local decoded view of one physically persisted serving generation.
/// It is reusable only while the Guard CF's exact per-family change signal is
/// unchanged; any Guard mutation forces a fresh point read and hash check.
#[derive(Clone, Debug)]
pub(crate) struct GuardServingMemo {
    pub(crate) profile_sha256: String,
    pub(crate) artifact_sha256: String,
    pub(crate) guard_cf_signal: (u64, u64),
    pub(crate) artifact: Arc<GuardServingArtifact>,
}

/// The adjudicated corpus split, with every excluded record counted.
struct AdjudicatedCorpus {
    good: Vec<AdjudicatedRecord>,
    bad: Vec<AdjudicatedRecord>,
    records_scanned: usize,
    unadjudicated: usize,
    conflicting: usize,
    /// Adjudicated records carrying none of the requested guard slots.
    ///
    /// Previously an uncounted `continue`, which is how the #1894 hydration
    /// defect stayed invisible: every record landed here and nothing said so.
    adjudicated_without_guarded_slots: usize,
    adjudicated_incomplete_guarded_slots: Vec<CxId>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::hex_bytes(&Sha256::digest(bytes))
}

fn guard_identity_population_sha256(ids: &[CxId]) -> String {
    let mut identities = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
    identities.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-guard-identity-population-v1\0");
    for identity in identities {
        hasher.update((identity.len() as u64).to_be_bytes());
        hasher.update(identity.as_bytes());
    }
    crate::hex_bytes(&hasher.finalize())
}

fn encode_guard_serving_artifact(
    artifact: &GuardServingArtifact,
) -> Result<Vec<u8>, SynapseCalyxError> {
    let bytes = bincode::serde::encode_to_vec(
        artifact,
        bincode::config::standard().with_limit::<{ SYNAPSE_GUARD_SERVING_MAX_BYTES }>(),
    )
    .map_err(|error| {
        guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_ENCODE_FAILED",
            format!("encode the immutable Ward serving artifact: {error}"),
            "inspect the trusted exemplar ids, slot dimensions, and finite-value invariants before retrying calibration",
        )
    })?;
    if bytes.len() > SYNAPSE_GUARD_SERVING_MAX_BYTES {
        return Err(guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_TOO_LARGE",
            format!(
                "encoded Ward serving artifact is {} bytes, above the hard {}-byte format ceiling",
                bytes.len(),
                SYNAPSE_GUARD_SERVING_MAX_BYTES
            ),
            "reduce the explicitly requested calibration corpus or guard-slot dimensionality, then recalibrate; the artifact is never truncated",
        ));
    }
    Ok(bytes)
}

fn decode_guard_serving_artifact(
    bytes: &[u8],
    profile: &GuardProfile,
    profile_sha256: &str,
) -> Result<GuardServingArtifact, SynapseCalyxError> {
    if bytes.is_empty() || bytes.len() > SYNAPSE_GUARD_SERVING_MAX_BYTES {
        return Err(guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_SIZE_INVALID",
            format!(
                "Ward serving artifact length {} is outside 1..={SYNAPSE_GUARD_SERVING_MAX_BYTES}",
                bytes.len()
            ),
            "recalibrate the guard to atomically republish a bounded serving artifact",
        ));
    }
    let (artifact, consumed) = bincode::serde::decode_from_slice::<GuardServingArtifact, _>(
        bytes,
        bincode::config::standard().with_limit::<{ SYNAPSE_GUARD_SERVING_MAX_BYTES }>(),
    )
    .map_err(|error| {
        guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_DECODE_FAILED",
            format!("decode the persisted Ward serving artifact: {error}"),
            "the Guard CF serving row is corrupt or uses an unsupported schema; recalibrate before verification",
        )
    })?;
    if consumed != bytes.len() {
        return Err(guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_TRAILING_BYTES",
            format!(
                "Ward serving decoder consumed {consumed} of {} persisted bytes",
                bytes.len()
            ),
            "recalibrate the guard; a serving row with unaccounted bytes is never trusted",
        ));
    }
    validate_guard_serving_artifact(&artifact, profile, profile_sha256)?;
    Ok(artifact)
}

#[allow(clippy::too_many_lines)]
fn validate_guard_serving_artifact(
    artifact: &GuardServingArtifact,
    profile: &GuardProfile,
    profile_sha256: &str,
) -> Result<(), SynapseCalyxError> {
    let expected_slots = profile
        .required_slots
        .iter()
        .map(|slot| slot.get())
        .collect::<Vec<_>>();
    if artifact.schema != SYNAPSE_GUARD_SERVING_SCHEMA
        || artifact.panel_version != profile.panel_version
        || artifact.guard_id != profile.guard_id.to_string()
        || artifact.profile_sha256 != profile_sha256
        || artifact.required_slots != expected_slots
    {
        return Err(guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_GENERATION_MISMATCH",
            format!(
                "Ward serving artifact is not bound to profile generation: schema={:?}, panel={}, guard_id={}, profile_sha256={}, required_slots={:?}; expected schema={SYNAPSE_GUARD_SERVING_SCHEMA:?}, panel={}, guard_id={}, profile_sha256={profile_sha256}, required_slots={expected_slots:?}",
                artifact.schema,
                artifact.panel_version,
                artifact.guard_id,
                artifact.profile_sha256,
                artifact.required_slots,
                profile.panel_version,
                profile.guard_id,
            ),
            "recalibrate the guard; thresholds and trusted vectors from different generations are never combined",
        ));
    }
    if artifact.trusted_exemplars.is_empty() {
        return Err(guard_error(
            "SYNAPSE_CALYX_GUARD_SERVING_EMPTY",
            "Ward serving artifact contains no trusted exemplar".to_owned(),
            "attach grounded good outcomes and recalibrate the guard",
        ));
    }

    let required = expected_slots.iter().copied().collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();
    let mut dimensions = BTreeMap::<u16, usize>::new();
    let mut coverage = BTreeMap::<u16, usize>::new();
    for record in &artifact.trusted_exemplars {
        if !ids.insert(record.cx_id) {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_SERVING_DUPLICATE_EXEMPLAR",
                format!("trusted exemplar {} appears more than once", record.cx_id),
                "recalibrate from the canonical panel membership generation; duplicate identities are never scored twice",
            ));
        }
        if record.slots.is_empty() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_SERVING_EXEMPLAR_EMPTY",
                format!("trusted exemplar {} carries no guarded slots", record.cx_id),
                "recalibrate from hydrated constellations that carry at least one required dense slot",
            ));
        }
        for (slot, vector) in &record.slots {
            if !required.contains(slot) {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_SERVING_SLOT_UNDECLARED",
                    format!(
                        "trusted exemplar {} carries slot {slot}, absent from profile required_slots {expected_slots:?}",
                        record.cx_id
                    ),
                    "recalibrate the exact profile and serving artifact together",
                ));
            }
            if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_SERVING_VECTOR_INVALID",
                    format!(
                        "trusted exemplar {} slot {slot} has dimension {} or a non-finite component",
                        record.cx_id,
                        vector.len()
                    ),
                    "repair the source lens/vector invariant and recalibrate; invalid vectors are never treated as absence or zero",
                ));
            }
            match dimensions.get(slot) {
                Some(dimension) if *dimension != vector.len() => {
                    return Err(guard_error(
                        "SYNAPSE_CALYX_GUARD_SERVING_DIMENSION_MISMATCH",
                        format!(
                            "trusted exemplar {} slot {slot} dimension {} differs from generation dimension {dimension}",
                            record.cx_id,
                            vector.len()
                        ),
                        "repair the frozen lens generation and recalibrate; mixed dimensions fail closed",
                    ));
                }
                None => {
                    dimensions.insert(*slot, vector.len());
                }
                Some(_) => {}
            }
            *coverage.entry(*slot).or_default() += 1;
        }
    }
    for slot in expected_slots {
        let count = coverage.get(&slot).copied().unwrap_or_default();
        if count < SYNAPSE_GUARD_MIN_GOOD_SCORES {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_SERVING_SLOT_UNDERSUPPORTED",
                format!(
                    "required slot {slot} has {count} trusted exemplar(s), below the serving minimum {SYNAPSE_GUARD_MIN_GOOD_SCORES}"
                ),
                "attach more grounded good records carrying this slot and recalibrate",
            ));
        }
    }
    Ok(())
}

impl SynapseCalyxVault {
    /// Reads the serving-policy false-accept rate from the persisted calibrated
    /// guard for a panel. Joint-policy profiles use their independently
    /// certified combined FAR; legacy one-slot profiles retain the conservative
    /// worst-per-slot interpretation. This is a read-only readiness input.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the guard row is absent, corrupt, or has
    /// no finite per-slot calibration evidence.
    pub fn guard_calibration_far(&self, panel_version: u32) -> Result<f32, SynapseCalyxError> {
        let panel_key = Self::guard_profile_key(panel_version);
        let row = self
            .read_cf_latest(ColumnFamily::Guard, &panel_key)?
            .ok_or_else(|| {
                guard_error(
                    calyx_ward::CALYX_GUARD_PROVISIONAL,
                    format!("no calibrated Ward profile is persisted for panel {panel_version}"),
                    "run hygiene operation=guard_calibrate for this panel",
                )
            })?;
        let profile: GuardProfile = serde_json::from_slice(&row).map_err(|error| {
            guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!("decode persisted guard profile for readiness: {error}"),
                "repair or recalibrate the corrupt guard profile",
            )
        })?;
        if profile.panel_version != panel_version || !profile.is_calibrated() {
            return Err(guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!("guard profile is not calibrated for panel {panel_version}"),
                "run hygiene operation=guard_calibrate for this panel",
            ));
        }
        calyx_ward::validate_high_stakes_profile(&profile, &profile.required_slots).map_err(
            |error| {
                guard_error(
                    error.code(),
                    format!(
                        "guard profile for panel {panel_version} has no current high-stakes scoring contract: {error}"
                    ),
                    "recalibrate this panel; legacy thresholds without a serving-score parity envelope remain readable history but cannot establish readiness",
                )
            },
        )?;
        let calibration = profile.calibration.as_ref().ok_or_else(|| {
            guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                "calibrated guard profile has no calibration measurement".to_owned(),
                "recalibrate the guard from held-out good and bad cases",
            )
        })?;
        if calibration.estimator == JOINT_POLICY_ESTIMATOR {
            // Multi-slot calibration certifies the actual combined serving
            // policy. Per-slot FARs remain diagnostics: a heterogeneous bad
            // case may legitimately collide on an unrelated slot while the
            // all-required policy still rejects it on another physical atom.
            Ok(calibration.far)
        } else {
            Ok(calibration
                .per_slot
                .values()
                .map(|slot| slot.far)
                .fold(calibration.far, f32::max))
        }
    }

    /// Calibrates a Ward [`GuardProfile`] from the vault's adjudicated corpus and
    /// persists it to the native `Guard` CF under the key the guarded-search
    /// consumer reads, then reads that row back and re-decodes it.
    ///
    /// # Errors
    ///
    /// Fails closed — never with a fabricated corpus — when: the panel is not
    /// published; a requested slot is unknown/inactive/non-dense; there are no
    /// adjudicated bad cases (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_ABSENT`); there are
    /// fewer than [`MIN_BAD_SCORES`] of them
    /// (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_INSUFFICIENT`); the bad corpus is too
    /// small for `target_far` to be certifiable at `alpha`
    /// (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_UNCERTIFIABLE`); ward's own conformal
    /// gate did not actually certify the returned tau
    /// (`SYNAPSE_CALYX_GUARD_TAU_UNCERTIFIED`); or the CF write/readback fails.
    #[allow(clippy::too_many_lines)]
    pub fn guard_calibrate(
        &self,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> Result<SynapseCalyxGuardCalibrateReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("guard_calibrate");
        if params.slots.is_empty() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_NO_SLOTS",
                "guard calibration named no slots; a profile with no required slot is inert and would accept everything".to_owned(),
                "name at least one dense active panel slot with its aspect (identity/stylistic/content)",
            ));
        }
        if params
            .anchor_kind
            .as_deref()
            .is_some_and(|kind| kind.trim().is_empty() || kind.len() > 128 || kind.trim() != kind)
        {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_ANCHOR_KIND_INVALID",
                format!(
                    "guard calibration anchor_kind {:?} must be a non-blank, trimmed identifier of at most 128 bytes",
                    params.anchor_kind
                ),
                "name the exact grounded anchor kind, for example reward or action_guard_region",
            ));
        }
        if !params.alpha.is_finite() || !(0.0..1.0).contains(&params.alpha) {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_INVALID_ALPHA",
                format!(
                    "guard calibration alpha {} is not a finite miscoverage budget in [0,1)",
                    params.alpha
                ),
                "supply a conformal miscoverage budget such as 0.05 (95% confidence)",
            ));
        }
        let panel = self
            .panel_for_guard_calibration(params.panel_version, params.calibration_panel.as_ref())?;
        let corpus = self.collect_adjudicated_corpus(params)?;

        // The honesty gate. Refusing is the correct outcome when reality has not
        // supplied a bad-case distribution; there is no synthetic branch.
        if corpus.bad.is_empty() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_BAD_CORPUS_ABSENT",
                format!(
                    "panel {} has no adjudicated bad case: {} record(s) scanned, {} adjudicated good (Bool(true), or a declared Enum good value, with confidence > 0), {} adjudicated bad (Bool(false), or any other value of a declared Enum kind), {} unadjudicated (anchors carrying Number/Text/OneHot/Vector values, or an Enum whose kind is not in SYNAPSE_DECLARED_ENUM_ADJUDICATIONS — none of which has a polarity Ward may infer), {} adjudicated but carrying none of the requested guard slots, {} adjudicated but carrying only a strict subset of the joint Guard roster",
                    params.panel_version,
                    corpus.records_scanned,
                    corpus.good.len(),
                    corpus.bad.len(),
                    corpus.unadjudicated,
                    corpus.adjudicated_without_guarded_slots,
                    corpus.adjudicated_incomplete_guarded_slots.len(),
                ),
                "a conformal FAR bound is only meaningful over a real known-bad distribution, so calibrating on manufactured badness would report `ok` forever and is refused here, not worked around. This panel genuinely carries no adjudicated bad case — but another panel may: calibration is no longer pinned to the durable active panel (#1919), so name the panel that actually receives outcomes. On this vault that is syn-mcp-usage-v1 @ 1776006 (synapse:mcp_tool_call_outcome, a declared enum adjudication) and syn-agent-event-v1 @ 1665001 (synapse:agent_tool_call_success, Bool(!error_present) — every failed tool call is a Bool(false)). Read `hygiene operation=grounding_gap` per panel to see which is adjudicated before concluding that no adjudication path exists",
            ));
        }

        let clock = FixedGuardClock(self.clock_now_ms()?);
        // Calibration and serving deliberately use the same canonical CPU
        // scorer. A Forge/GPU normalized-dot ranking is mathematically cosine,
        // but its parallel reduction can differ by an f32 ulp at tau and turn a
        // calibrated rejection into a serving acceptance.
        let scoring_backend = "cpu".to_owned();
        let mut inputs = Vec::with_capacity(params.slots.len());
        let mut evidence = Vec::with_capacity(params.slots.len());
        let mut collision_diagnostics = Vec::new();
        for spec in &params.slots {
            let target_far = match params.target_far {
                Some(target_far) => target_far,
                None => self.configured_guard_target_far(spec.aspect)?,
            };
            let score_report = calibration_slot_scores(&corpus.good, &corpus.bad, spec.slot)?;
            if let Some(diagnostic) =
                calibration_collision_diagnostic(spec.slot, &score_report.nearest_good)
            {
                collision_diagnostics.push(diagnostic);
            }
            let good_scores = score_report.good_scores;
            let bad_scores = score_report.bad_scores;
            let good_case_ids = score_report.good_case_ids;
            let bad_case_ids = score_report.bad_case_ids;
            if good_scores.len() < SYNAPSE_GUARD_MIN_GOOD_SCORES {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_GOOD_CORPUS_INSUFFICIENT",
                    format!(
                        "slot {} has {} adjudicated good exemplar(s) with a dense vector; a leave-one-out in-region score needs at least {SYNAPSE_GUARD_MIN_GOOD_SCORES}",
                        spec.slot,
                        good_scores.len()
                    ),
                    "embed this slot on more Bool(true)-anchored records, or drop the slot from the calibration request",
                ));
            }
            if bad_scores.len() < MIN_BAD_SCORES {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_BAD_CORPUS_INSUFFICIENT",
                    format!(
                        "slot {} has {} adjudicated bad score(s) with a dense vector; ward's conformal calibration requires at least {MIN_BAD_SCORES}",
                        spec.slot,
                        bad_scores.len()
                    ),
                    "collect more adjudicated bad cases for this panel; do not lower the minimum, and do not synthesize cases to reach it",
                ));
            }
            let certifiable_min = min_certifiable_bad_scores(target_far, params.alpha);
            if bad_scores.len() < certifiable_min {
                // Without this gate ward's conformal search finds no certifiable
                // candidate and falls back to `next_above(max bad score)`, which
                // rejects every possible cosine while still reporting far=0.0 at
                // the requested confidence. That silent conservative fallback
                // reads as a calibrated guard. Refuse instead, and name n.
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_BAD_CORPUS_UNCERTIFIABLE",
                    format!(
                        "slot {} has {} adjudicated bad score(s); target_far {target_far} cannot be certified at alpha {} with fewer than {certifiable_min} (the exact one-sided Clopper-Pearson bound needs (1-target_far)^n <= alpha even with zero false accepts)",
                        spec.slot,
                        bad_scores.len(),
                        params.alpha
                    ),
                    "collect at least the named number of adjudicated bad cases, or raise target_far to what this corpus size can actually certify",
                ));
            }

            inputs.push(CalibrationInput {
                slot: SlotId::new(spec.slot),
                good_scores,
                bad_scores,
                good_case_ids,
                bad_case_ids,
                slot_kind: spec.aspect.slot_kind(),
                target_far,
                score_tolerance: 0.0,
                scoring_engine: DENSE_COSINE_SCORING_ENGINE.to_owned(),
            });
            evidence.push((*spec, target_far, certifiable_min));
        }

        validate_calibration_slots(&inputs, &panel).map_err(|error| {
            guard_error(
                error.code(),
                format!("guard calibration slot validation failed: {error}"),
                "name only dense, Active slots of the published panel; a profile guarding a sparse/multi/parked slot fails every query at query time",
            )
        })?;

        let template = GuardProfile {
            guard_id: GuardId::new(Uuid::new_v4()),
            panel_version: params.panel_version,
            domain: params.domain.clone(),
            calibration_anchor_kind: params.anchor_kind.clone(),
            tau: BTreeMap::new(),
            required_slots: Vec::new(),
            policy: GuardPolicy::AllRequired,
            calibration: None,
            novelty_action: params.novelty_action.clone(),
        };
        let profile =
            calibrate(template, inputs.clone(), params.alpha, &clock).map_err(|error| {
                let collision_suffix = if collision_diagnostics.is_empty() {
                    String::new()
                } else {
                    format!("; {}", collision_diagnostics.join("; "))
                };
                guard_error(
                    error.code(),
                    format!("ward conformal calibration failed: {error}{collision_suffix}"),
                    "inspect the named physical good/bad collision rows and repair the immutable pre-trigger lens or its missing-state semantics; never add outcome/error/after-state leakage, relax calibration minimums, or synthesize cases to force a profile",
                )
            })?;

        let calibration = profile.calibration.as_ref().ok_or_else(|| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_PROFILE_UNCALIBRATED",
                "ward returned a profile without calibration metadata".to_owned(),
                "repair calyx-ward calibration; a profile without an estimator and corpus hash is never persisted",
            )
        })?;
        let joint_policy = calibration.estimator == JOINT_POLICY_ESTIMATOR;
        let policy_target_far = evidence
            .iter()
            .map(|(_, target_far, _)| *target_far)
            .reduce(f32::min)
            .ok_or_else(|| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_POLICY_EVIDENCE_EMPTY",
                    "guard calibration produced no policy FAR target".to_owned(),
                    "name at least one dense active guard slot",
                )
            })?;
        let (policy_bad_accepts, policy_bad_count) =
            calibration_policy_accepts(&profile, &inputs, true)?;
        let (policy_good_accepts, policy_good_count) =
            calibration_policy_accepts(&profile, &inputs, false)?;
        let policy_tail =
            clopper_pearson_tail(policy_bad_accepts, policy_bad_count, policy_target_far);
        let policy_certified = policy_tail <= f64::from(params.alpha) + f64::EPSILON;
        if !policy_certified {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_POLICY_TAU_UNCERTIFIED",
                format!(
                    "guard policy {:?} tau profile accepts {policy_bad_accepts}/{policy_bad_count} aligned bad case(s); exact one-sided Clopper-Pearson tail {policy_tail} exceeds alpha {} at target_far {policy_target_far}",
                    profile.policy, params.alpha
                ),
                "collect more physically adjudicated bad cases or strengthen the frozen causal views; a policy whose combined serving verdict is uncertified is never persisted",
            ));
        }
        let policy_far = fraction(policy_bad_accepts, policy_bad_count);
        let policy_false_reject_rate = fraction(
            policy_good_count.saturating_sub(policy_good_accepts),
            policy_good_count,
        );

        let mut slots = Vec::with_capacity(inputs.len());
        for (input, (spec, target_far, certifiable_min)) in inputs.iter().zip(evidence) {
            let tau = profile.tau_for(&input.slot).ok_or_else(|| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_TAU_MISSING",
                    format!("ward returned no tau for calibrated slot {}", spec.slot),
                    "inspect calyx-ward calibrate(); a calibrated profile must carry a tau per input slot",
                )
            })?;
            let bad_accepts = input
                .bad_scores
                .iter()
                .filter(|score| conservative_bad_guard_score(**score, input.score_tolerance) >= tau)
                .count();
            let tail = clopper_pearson_tail(bad_accepts, input.bad_scores.len(), target_far);
            // Independent readback of ward's own gate: if the returned tau is
            // not certified at alpha, ward took its conservative fallback and
            // the profile's reported FAR is not a bound. Refuse to persist it.
            if !joint_policy && tail > f64::from(params.alpha) + f64::EPSILON {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_TAU_UNCERTIFIED",
                    format!(
                        "slot {} tau {tau} accepts {bad_accepts}/{} bad case(s); the exact one-sided Clopper-Pearson tail P(X <= {bad_accepts} | n, {target_far}) = {tail} exceeds alpha {}, so this tau is NOT certified at the requested confidence",
                        spec.slot,
                        input.bad_scores.len(),
                        params.alpha
                    ),
                    "the calibration corpus does not support the requested target_far/alpha; collect more adjudicated bad cases or relax target_far — a profile whose tau is uncertified is never persisted",
                ));
            }
            let far_at_tau = fraction(bad_accepts, input.bad_scores.len());
            let good_reject_rate = fraction(
                input
                    .good_scores
                    .iter()
                    .filter(|score| {
                        conservative_good_guard_score(**score, input.score_tolerance) < tau
                    })
                    .count(),
                input.good_scores.len(),
            );
            slots.push(SynapseCalyxGuardSlotCalibration {
                slot: spec.slot,
                aspect: spec.aspect.label().to_owned(),
                target_far,
                tau,
                bad_accepts,
                achieved_far: far_at_tau,
                achieved_frr: good_reject_rate,
                good_scores: input.good_scores.len(),
                bad_scores: input.bad_scores.len(),
                clopper_pearson_tail: tail,
                certifiable_min_bad_scores: certifiable_min,
            });
        }

        let (
            guard_cf_profile_bytes,
            guard_cf_profile_sha256,
            guard_cf_serving_bytes,
            guard_cf_serving_sha256,
            readback_calibrated,
            readback_serving_bound,
        ) = if params.persist {
            let encoded = serde_json::to_vec(&profile).map_err(|error| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_ENCODE_FAILED",
                    format!("encode the calibrated guard profile: {error}"),
                    "inspect the profile fields before retrying the calibration",
                )
            })?;
            let profile_sha256 = sha256_hex(&encoded);
            let serving = GuardServingArtifact {
                schema: SYNAPSE_GUARD_SERVING_SCHEMA.to_owned(),
                panel_version: profile.panel_version,
                guard_id: profile.guard_id.to_string(),
                profile_sha256: profile_sha256.clone(),
                required_slots: profile
                    .required_slots
                    .iter()
                    .map(|slot| slot.get())
                    .collect(),
                trusted_exemplars: corpus.good.clone(),
            };
            validate_guard_serving_artifact(&serving, &profile, &profile_sha256)?;
            let serving_encoded = encode_guard_serving_artifact(&serving)?;
            let serving_sha256 = sha256_hex(&serving_encoded);
            // The panel-keyed row is the source of truth. The constant
            // `profile\0default` key is a *mirror*, written only when this panel
            // is the one `calyx-search` is actually serving, so that reader can
            // never load a profile calibrated for a different panel (#1919).
            let panel_key = Self::guard_profile_key(params.panel_version);
            let serving_key = Self::guard_serving_key(params.panel_version);
            let mut writes = vec![
                SynapseCalyxCfWrite {
                    cf: ColumnFamily::Guard,
                    key: panel_key.clone(),
                    value: encoded.clone(),
                },
                SynapseCalyxCfWrite {
                    cf: ColumnFamily::Guard,
                    key: serving_key.clone(),
                    value: serving_encoded.clone(),
                },
            ];
            if self.is_durable_active_panel(params.panel_version) {
                writes.push(SynapseCalyxCfWrite {
                    cf: ColumnFamily::Guard,
                    key: SYNAPSE_GUARD_DEFAULT_PROFILE_KEY.to_vec(),
                    value: encoded.clone(),
                });
            }
            // One Aster group commit publishes profile, exact serving
            // generation, and optional active-panel mirror together. There is
            // no interval in which a new profile can name absent exemplars.
            self.write_cf_batch(writes)?;
            self.flush()?;
            let mut readback = self.read_cf_batch_latest(&[
                CfRead::new(ColumnFamily::Guard, panel_key),
                CfRead::new(ColumnFamily::Guard, serving_key),
            ])?;
            if readback.len() != 2 {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_GENERATION_READBACK_COUNT_MISMATCH",
                    format!(
                        "atomic Guard generation readback returned {} rows for 2 requested keys",
                        readback.len()
                    ),
                    "inspect the Aster atomic batch point-read path; calibration is not reported persisted",
                ));
            }
            let serving_row = readback.pop().flatten().ok_or_else(|| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_SERVING_READBACK_MISSING",
                    format!(
                        "the trusted-exemplar serving row for guard {} is absent immediately after its atomic publication",
                        profile.guard_id
                    ),
                    "inspect the Aster Guard CF group commit; verification cannot serve this profile",
                )
            })?;
            let Some(row) = readback.pop().flatten() else {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_READBACK_MISSING",
                    format!(
                        "the calibrated guard profile for panel {} is absent from the Guard CF immediately after a flushed write",
                        params.panel_version
                    ),
                    "inspect the vault Guard CF and the commit path; a write that cannot be read back is never reported as persisted",
                ));
            };
            let decoded: GuardProfile = serde_json::from_slice(&row).map_err(|error| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_READBACK_DECODE_FAILED",
                    format!("decode the Guard CF profile that was just written: {error}"),
                    "the persisted row is not a GuardProfile the guarded-search consumer can load; inspect the codec",
                )
            })?;
            let readback_profile_sha256 = sha256_hex(&row);
            if row != encoded || readback_profile_sha256 != profile_sha256 {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_READBACK_MISMATCH",
                    format!(
                        "flushed Guard profile readback hash {readback_profile_sha256} differs from published hash {profile_sha256}"
                    ),
                    "inspect the Aster group-commit and Guard CF point-read paths; calibration is not reported persisted",
                ));
            }
            let readback_serving_sha256 = sha256_hex(&serving_row);
            if serving_row != serving_encoded || readback_serving_sha256 != serving_sha256 {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_SERVING_READBACK_MISMATCH",
                    format!(
                        "flushed serving-artifact readback hash {readback_serving_sha256} differs from published hash {serving_sha256}"
                    ),
                    "inspect the Aster group-commit and Guard CF point-read paths; the generation is never served",
                ));
            }
            let decoded_serving = Arc::new(decode_guard_serving_artifact(
                &serving_row,
                &decoded,
                &readback_profile_sha256,
            )?);
            let readback_calibrated =
                decoded.is_calibrated() && decoded.guard_id == profile.guard_id;
            let readback_serving_bound = decoded_serving.guard_id == decoded.guard_id.to_string()
                && decoded_serving.profile_sha256 == readback_profile_sha256;
            let memo = GuardServingMemo {
                profile_sha256: readback_profile_sha256.clone(),
                artifact_sha256: readback_serving_sha256.clone(),
                guard_cf_signal: self.cf_change_signal(ColumnFamily::Guard),
                artifact: decoded_serving,
            };
            let mut serving_memo = match self.guard_serving_memo.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *serving_memo = Some(memo);
            drop(serving_memo);
            (
                row.len(),
                readback_profile_sha256,
                serving_row.len(),
                readback_serving_sha256,
                readback_calibrated,
                readback_serving_bound,
            )
        } else {
            (0, String::new(), 0, String::new(), false, false)
        };
        let guard_cf_rows_after = self
            .count_cf_latest_bounded(ColumnFamily::Guard)?
            .rows_visited;
        let incomplete_guarded_slots_sha256 =
            guard_identity_population_sha256(&corpus.adjudicated_incomplete_guarded_slots);
        let incomplete_guarded_slots_sample = corpus
            .adjudicated_incomplete_guarded_slots
            .iter()
            .take(16)
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        Ok(SynapseCalyxGuardCalibrateReport {
            panel_version: params.panel_version,
            domain: params.domain.clone(),
            anchor_kind: params.anchor_kind.clone(),
            guard_id: profile.guard_id.to_string(),
            alpha: params.alpha,
            novelty_action: match params.novelty_action {
                NoveltyAction::NewRegion => "new_region".to_owned(),
                NoveltyAction::Quarantine => "quarantine".to_owned(),
                NoveltyAction::RejectClosed => "reject_closed".to_owned(),
            },
            records_scanned: corpus.records_scanned,
            adjudicated_good: corpus.good.len(),
            adjudicated_bad: corpus.bad.len(),
            unadjudicated: corpus.unadjudicated,
            conflicting: corpus.conflicting,
            adjudicated_without_guarded_slots: corpus.adjudicated_without_guarded_slots,
            adjudicated_incomplete_guarded_slots: corpus.adjudicated_incomplete_guarded_slots.len(),
            adjudicated_incomplete_guarded_slots_sha256: incomplete_guarded_slots_sha256,
            adjudicated_incomplete_guarded_slots_sample: incomplete_guarded_slots_sample,
            estimator: calibration.estimator.clone(),
            scoring_backend,
            scoring_engine: DENSE_COSINE_SCORING_ENGINE.to_owned(),
            scoring_tolerance: 0.0,
            policy: match profile.policy {
                GuardPolicy::AllRequired => "all_required".to_owned(),
                GuardPolicy::KofN { k } => format!("k_of_n:{k}"),
            },
            policy_bad_accepts,
            policy_achieved_far: policy_far,
            policy_achieved_frr: policy_false_reject_rate,
            policy_clopper_pearson_tail: policy_tail,
            policy_certified,
            slots,
            persisted: params.persist,
            guard_cf_profile_bytes,
            guard_cf_profile_sha256,
            guard_cf_serving_bytes,
            guard_cf_serving_sha256,
            guard_cf_rows_after,
            readback_calibrated,
            readback_serving_bound,
        })
    }

    /// Verifies one record against the persisted Ward guard profile and returns
    /// the full [`calyx_ward::GuardVerdict`] decomposition.
    ///
    /// # Errors
    ///
    /// Fails closed when no calibrated profile is persisted, when the profile
    /// was calibrated for a different panel, when the query record or its
    /// required slot vectors are missing, or when no trusted exemplar exists to
    /// match against.
    #[allow(clippy::too_many_lines)]
    pub fn guard_verify(
        &self,
        params: &SynapseCalyxGuardVerifyParams,
    ) -> Result<SynapseCalyxGuardVerifyReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("guard_verify");
        let query_cx = crate::parse_cx_id(&params.query_cx_id)?;
        // Read the profile calibrated for *this* panel. Legacy profiles without
        // an exact panel key and a generation-bound serving artifact fail
        // closed and must be recalibrated; verification never falls back to a
        // default row or reconstructs trusted state from a corpus scan.
        let panel_key = Self::guard_profile_key(params.panel_version);
        let serving_key = Self::guard_serving_key(params.panel_version);
        let mut generation = self.read_cf_batch_latest(&[
            CfRead::new(ColumnFamily::Guard, panel_key),
            CfRead::new(ColumnFamily::Guard, serving_key),
        ])?;
        if generation.len() != 2 {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_GENERATION_READ_COUNT_MISMATCH",
                format!(
                    "atomic Guard generation point-read returned {} rows for 2 requested keys",
                    generation.len()
                ),
                "inspect the Aster atomic batch point-read path; no verdict is released",
            ));
        }
        let serving_row = generation.pop().flatten().ok_or_else(|| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_SERVING_MISSING",
                format!(
                    "calibrated Ward panel {} has no immutable trusted-exemplar serving row",
                    params.panel_version
                ),
                "recalibrate this panel with the current runtime; verification never reconstructs or falls back to a corpus scan",
            )
        })?;
        let row = generation.pop().flatten().ok_or_else(|| {
            guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!(
                    "no calibrated Ward guard profile is persisted at the exact panel-keyed Guard CF row for panel {}",
                    params.panel_version
                ),
                "run hygiene operation=guard_calibrate for this panel; verification never consults a legacy/default profile",
            )
        })?;
        let profile_sha256 = sha256_hex(&row);
        let profile: GuardProfile = serde_json::from_slice(&row).map_err(|error| {
            guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!("decode the persisted default guard profile: {error}"),
                "the Guard CF row is not a decodable GuardProfile; recalibrate the guard",
            )
        })?;
        if profile.panel_version != params.panel_version {
            return Err(guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!(
                    "guard profile panel_version {} does not match the requested panel {}",
                    profile.panel_version, params.panel_version
                ),
                "recalibrate the guard for the active panel before verifying against it",
            ));
        }
        if !profile.is_calibrated() {
            return Err(guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                "the persisted default guard profile carries no calibration provenance".to_owned(),
                "run guard calibrate; an uncalibrated profile is never used to admit output",
            ));
        }

        let serving_sha256 = sha256_hex(&serving_row);
        let (serving, serving_sha256) = self.load_guard_serving_generation(
            &profile,
            &profile_sha256,
            &serving_row,
            &serving_sha256,
        )?;
        let query =
            self.load_record_slots(params.panel_version, query_cx, &profile.required_slots)?;

        let mut produced = BTreeMap::new();
        let mut matched = BTreeMap::new();
        let mut matched_ids = BTreeMap::new();
        for slot in &profile.required_slots {
            let raw = slot.get();
            let Some(query_vec) = query.get(&raw) else {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_QUERY_SLOT_MISSING",
                    format!(
                        "record {query_cx} carries no dense vector for required guard slot {raw} in panel {}",
                        params.panel_version
                    ),
                    "supply a record that carries every slot the profile requires; a missing required slot is never treated as a pass",
                ));
            };
            let Some((exemplar, _)) = best_match(query_vec, &serving.trusted_exemplars, raw) else {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_NO_TRUSTED_EXEMPLAR",
                    format!(
                        "no adjudicated good exemplar carries slot {raw} in panel {}; there is nothing trusted to match the record against",
                        params.panel_version
                    ),
                    "anchor in-region records with AnchorValue::Bool(true) and embed the guarded slots before verifying",
                ));
            };
            let exemplar_vec = serving.trusted_exemplars[exemplar]
                .slots
                .get(&raw)
                .cloned()
                .unwrap_or_default();
            matched_ids.insert(raw, serving.trusted_exemplars[exemplar].cx_id.to_string());
            produced.insert(*slot, query_vec.clone());
            matched.insert(*slot, exemplar_vec);
        }

        let verdict =
            guard(&profile, &produced, &matched, params.high_stakes).map_err(|error| {
                guard_error(
                    error.code(),
                    format!("ward guard evaluation failed: {error}"),
                    "inspect the profile's required slots and the record's slot coverage",
                )
            })?;

        // A Ward verdict is a security decision, not an observational return
        // value. Commit its complete decomposition before exposing it so every
        // accepted/refused decision has a physical, hash-chained source of
        // truth. Failure to append fails the verification itself closed.
        let verdict_payload = serde_json::to_vec(&serde_json::json!({
            "ward_provenance": "ward_guard_verdict_v1",
            "cx_id": query_cx.to_string(),
            "guard_id": verdict.guard_id.to_string(),
            "overall_pass": verdict.overall_pass,
            "provisional": verdict.provisional,
            "action": verdict.action,
            "per_slot": verdict.per_slot,
        }))
        .map_err(|error| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_VERDICT_ENCODE_FAILED",
                format!("encode Ward verdict ledger payload: {error}"),
                "inspect the GuardVerdict serialization contract; no verdict is returned without durable provenance",
            )
        })?;
        let per_slot: Vec<SynapseCalyxGuardSlotVerdict> = verdict
            .per_slot
            .iter()
            .map(|slot| SynapseCalyxGuardSlotVerdict {
                slot: slot.slot.get(),
                cos: slot.cos,
                tau: slot.tau,
                pass: slot.pass,
                matched_cx_id: matched_ids
                    .get(&slot.slot.get())
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        let novelty_action = verdict.action.as_ref().and_then(|action| match action {
            NoveltyAction::NewRegion => Some("new_region"),
            NoveltyAction::Quarantine => Some("quarantine"),
            NoveltyAction::RejectClosed => None,
        });
        let failing_slots = per_slot
            .iter()
            .filter(|slot| !slot.pass)
            .map(|slot| slot.slot)
            .collect::<Vec<_>>();
        let query_cx_string = query_cx.to_string();
        let guard_id_string = verdict.guard_id.to_string();
        let ledger_ref = self
            .vault
            .append_ledger_entry_with_rows(
                EntryKind::Guard,
                SubjectId::Cx(query_cx),
                verdict_payload,
                ActorId::Service("calyx-ward".to_owned()),
                |ledger_ref| {
                    let Some(action) = novelty_action else {
                        return Ok(Vec::new());
                    };
                    let finding = SynapseCalyxPersistedNoveltyFinding {
                        panel_version: params.panel_version,
                        query_cx_id: query_cx_string.clone(),
                        guard_id: guard_id_string.clone(),
                        action: action.to_owned(),
                        failing_slots: failing_slots.clone(),
                        ledger_seq: ledger_ref.seq,
                        ledger_hash: crate::hex_bytes(&ledger_ref.hash),
                    };
                    let key = reactive_novelty_key(&finding).map_err(|error| {
                        CalyxError::ledger_group_commit_failed(format!(
                            "build Ward novelty outbox key: {error}"
                        ))
                    })?;
                    let value = serde_json::to_vec(&finding).map_err(|error| {
                        CalyxError::ledger_group_commit_failed(format!(
                            "encode Ward novelty outbox row: {error}"
                        ))
                    })?;
                    Ok(vec![(ColumnFamily::Reactive, key, value)])
                },
            )
            .map_err(|error| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_VERDICT_LEDGER_FAILED",
                    format!("append Ward verdict to the physical Ledger CF: {error}"),
                    "repair the vault Ledger append path and verify its hash chain before retrying; the verdict was not released",
                )
            })?;

        Ok(SynapseCalyxGuardVerifyReport {
            panel_version: params.panel_version,
            query_cx_id: query_cx.to_string(),
            guard_id: verdict.guard_id.to_string(),
            domain: profile.domain.clone(),
            calibration_anchor_kind: profile.calibration_anchor_kind.clone(),
            high_stakes: params.high_stakes,
            overall_pass: verdict.overall_pass,
            provisional: verdict.provisional,
            policy: match profile.policy {
                GuardPolicy::AllRequired => "all_required".to_owned(),
                GuardPolicy::KofN { k } => format!("k_of_n(k={k})"),
            },
            required_slots: profile
                .required_slots
                .iter()
                .copied()
                .map(SlotId::get)
                .collect(),
            failing_slots: per_slot
                .iter()
                .filter(|slot| !slot.pass)
                .map(|slot| slot.slot)
                .collect(),
            per_slot,
            action: verdict.action.map(|action| match action {
                NoveltyAction::NewRegion => "new_region".to_owned(),
                NoveltyAction::Quarantine => "quarantine".to_owned(),
                NoveltyAction::RejectClosed => "reject_closed".to_owned(),
            }),
            calibration_far: profile.calibration.as_ref().map(|meta| meta.far),
            calibration_frr: profile.calibration.as_ref().map(|meta| meta.frr),
            calibration_confidence: profile.calibration.as_ref().map(|meta| meta.confidence),
            trusted_exemplars: serving.trusted_exemplars.len(),
            guard_cf_profile_sha256: profile_sha256,
            guard_cf_serving_sha256: serving_sha256,
            ledger_seq: ledger_ref.seq,
            ledger_hash: crate::hex_bytes(&ledger_ref.hash),
        })
    }

    /// The operator-configured default target FAR for one guard aspect (#1883).
    ///
    /// `guard_far_identity` / `guard_far_content` / `guard_far_stylistic` were
    /// validated at startup, frozen into the lowered artifact and echoed to
    /// `health` while calibration silently used `calyx_ward::SlotKind::
    /// default_target_far()` instead — so tuning them changed nothing. This is
    /// the single place the default now comes from, which makes those three
    /// knobs load bearing.
    ///
    /// Precedence is unchanged and explicit: a per-request `target_far` still
    /// wins. The Ward constant survives only in its other role, as
    /// `calibrate.rs`'s per-aspect **ceiling** — so configuring a FAR tighter
    /// than policy takes effect, and configuring one looser than policy is
    /// refused loudly (`target_far exceeds slot_kind maximum`) rather than
    /// quietly ignored.
    fn configured_guard_target_far(
        &self,
        aspect: SynapseCalyxGuardAspect,
    ) -> Result<f32, SynapseCalyxError> {
        let tuning = self.effective_tuning()?;
        Ok(match aspect {
            SynapseCalyxGuardAspect::Identity => tuning.guard_far_identity,
            SynapseCalyxGuardAspect::Stylistic => tuning.guard_far_stylistic,
            SynapseCalyxGuardAspect::Content => tuning.guard_far_content,
        })
    }

    /// Resolves the [`Panel`] definition the guard validates its slots against.
    ///
    /// Two sources, in this order, and never a third:
    ///
    /// 1. a `calibration_panel` supplied by the caller — the caller owns the
    ///    catalogue of code-declared panels and can reconstruct any of them
    ///    deterministically. Its version must match the requested one.
    /// 2. the durable active panel published in the vault manifest.
    ///
    /// A request for a panel that is neither still fails closed, and the error
    /// now names which of the two lookups came up empty instead of asserting
    /// that the active panel is the only legal target (#1919).
    fn panel_for_guard_calibration(
        &self,
        panel_version: u32,
        supplied: Option<&Panel>,
    ) -> Result<Panel, SynapseCalyxError> {
        if let Some(panel) = supplied {
            if panel.version != panel_version {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_PANEL_MISMATCH",
                    format!(
                        "guard calibration requested panel {panel_version}, but the supplied panel definition is version {}",
                        panel.version
                    ),
                    "supply the panel definition for the exact version being calibrated; a profile validated against another panel's slot map guards the wrong vectors",
                ));
            }
            return Ok(panel.clone());
        }
        let state = load_vault_panel_state(&self.config.vault_dir).map_err(|error| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_NO_ACTIVE_PANEL",
                format!(
                    "no panel definition is available for guard calibration: no definition was supplied for panel {panel_version}, and the durable active panel could not be loaded: {error}"
                ),
                "supply the panel definition for this version, or publish the active panel, before calibrating a guard profile",
            )
        })?;
        if state.panel.version != panel_version {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_PANEL_MISMATCH",
                format!(
                    "guard calibration requested panel {panel_version}, but no definition was supplied for it and the durable active panel is {}",
                    state.panel.version
                ),
                "supply the panel definition for the requested version (a code-declared panel can be reconstructed), or calibrate against the active panel; a profile whose slots were never validated against a real slot map guards nothing",
            ));
        }
        Ok(state.panel)
    }

    /// Guard CF key of the calibrated profile for one panel (#1919).
    ///
    /// The Guard CF used one constant key for the whole vault, so a second
    /// panel's profile could only ever overwrite the first. That single-key
    /// namespace — not any rule about evidence — is what forced calibration to
    /// be pinned to one panel. Keying by panel makes coexistence structural, and
    /// makes "a profile bound to another panel fails every guarded query" an
    /// invariant enforced by the key rather than by refusing to calibrate.
    #[must_use]
    fn guard_profile_key(panel_version: u32) -> Vec<u8> {
        let mut key = Vec::with_capacity(15 + std::mem::size_of::<u32>());
        key.extend_from_slice(b"profile\0panel\0");
        key.extend_from_slice(&panel_version.to_be_bytes());
        key
    }

    /// Guard CF key of the immutable trusted-exemplar generation currently
    /// served for one panel. Profile and artifact share one atomic commit/read
    /// snapshot, so a stable key avoids retaining an unbounded large row per
    /// recalibration while the embedded profile hash still binds the contents.
    #[must_use]
    fn guard_serving_key(panel_version: u32) -> Vec<u8> {
        let mut key = Vec::with_capacity(15 + std::mem::size_of::<u32>());
        key.extend_from_slice(b"serving\0panel\0");
        key.extend_from_slice(&panel_version.to_be_bytes());
        key
    }

    /// Point-reads and validates the exact serving generation named by the
    /// profile, or reuses the decoded generation while the Guard CF's exact
    /// change signal proves no physical mutation occurred.
    fn load_guard_serving_generation(
        &self,
        profile: &GuardProfile,
        profile_sha256: &str,
        bytes: &[u8],
        artifact_sha256: &str,
    ) -> Result<(Arc<GuardServingArtifact>, String), SynapseCalyxError> {
        let signal = self.cf_change_signal(ColumnFamily::Guard);
        let memo = match self.guard_serving_memo.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(memo) = memo
            && memo.profile_sha256 == profile_sha256
            && memo.artifact_sha256 == artifact_sha256
            && memo.guard_cf_signal == signal
        {
            return Ok((memo.artifact, memo.artifact_sha256));
        }
        let artifact = Arc::new(decode_guard_serving_artifact(
            bytes,
            profile,
            profile_sha256,
        )?);
        let readback_signal = self.cf_change_signal(ColumnFamily::Guard);
        let replacement = GuardServingMemo {
            profile_sha256: profile_sha256.to_owned(),
            artifact_sha256: artifact_sha256.to_owned(),
            guard_cf_signal: readback_signal,
            artifact: Arc::clone(&artifact),
        };
        let mut memo = match self.guard_serving_memo.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *memo = Some(replacement);
        drop(memo);
        Ok((artifact, artifact_sha256.to_owned()))
    }

    /// True when `panel_version` is the vault's durable active panel.
    ///
    /// Used only to decide whether to *also* mirror the profile onto
    /// [`SYNAPSE_GUARD_DEFAULT_PROFILE_KEY`], which `calyx-search`'s guarded
    /// reader looks up by that exact constant. Mirroring only the active panel's
    /// profile is what keeps that reader correct by construction: it can never
    /// pick up a profile calibrated for a panel it is not serving.
    fn is_durable_active_panel(&self, panel_version: u32) -> bool {
        load_vault_panel_state(&self.config.vault_dir)
            .is_ok_and(|state| state.panel.version == panel_version)
    }

    /// Splits the panel into adjudicated good/bad records, counting everything
    /// it refuses to interpret.
    #[expect(
        clippy::too_many_lines,
        reason = "one snapshot-scoped pass classifies every grounded outcome and hydrates its requested guard slots; splitting would obscure that all accepted rows share one coherent pin"
    )]
    fn collect_adjudicated_corpus(
        &self,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> Result<AdjudicatedCorpus, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let wanted: Vec<u16> = params.slots.iter().map(|spec| spec.slot).collect();
        let mut corpus = AdjudicatedCorpus {
            good: Vec::new(),
            bad: Vec::new(),
            records_scanned: 0,
            unadjudicated: 0,
            conflicting: 0,
            adjudicated_without_guarded_slots: 0,
            adjudicated_incomplete_guarded_slots: Vec::new(),
        };
        // The panel membership sidecar prevents a cross-panel Base scan. This
        // fold selects adjudicated exemplars and stops at `max_records`, so its
        // Base point reads are bounded to the requested panel corpus.
        self.with_panel_read_snapshot(
            params.panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| {
                self.walk_panel_base_snapshot(
                    snapshot,
                    params.panel_version,
                    |snapshot, _key, value| {
                        let constellation = decode_constellation_base(value).map_err(|error| {
                            SynapseCalyxError::from_calyx("decode Base constellation", &error)
                        })?;
                        if constellation.panel_version != params.panel_version {
                            return Ok(crate::SynapseCalyxWalkStep::Continue);
                        }
                        corpus.records_scanned += 1;
                        let mut good = false;
                        let mut bad = false;
                        let mut adjudicated = false;
                        for anchor in &constellation.anchors {
                            if anchor.confidence <= 0.0 {
                                continue;
                            }
                            if params.anchor_kind.as_deref().is_some_and(|wanted| {
                                crate::grounding::anchor_kind_label(&anchor.kind) != wanted
                            }) {
                                continue;
                            }
                            match anchor.value {
                                AnchorValue::Bool(true) => {
                                    good = true;
                                    adjudicated = true;
                                }
                                AnchorValue::Bool(false) => {
                                    bad = true;
                                    adjudicated = true;
                                }
                                // An enum is adjudicable only when its kind appears in the
                                // closed declaration table, which copies a partition the
                                // writing subsystem already makes. An undeclared kind falls
                                // through to unadjudicated, so this stays fail-closed.
                                AnchorValue::Enum(ref value) => {
                                    match declared_enum_verdict(&anchor.kind, value) {
                                        Some(true) => {
                                            good = true;
                                            adjudicated = true;
                                        }
                                        Some(false) => {
                                            bad = true;
                                            adjudicated = true;
                                        }
                                        None => {}
                                    }
                                }
                                // No repo-wide polarity convention exists for these values.
                                // Interpreting one would fabricate the very labels the
                                // conformal bound is a statement about.
                                _ => {}
                            }
                        }
                        if !adjudicated {
                            corpus.unadjudicated += 1;
                            return Ok(crate::SynapseCalyxWalkStep::Continue);
                        }
                        if good && bad {
                            corpus.conflicting += 1;
                            return Ok(crate::SynapseCalyxWalkStep::Continue);
                        }
                        // A Base row carries only `(slot_id, slot_hash)` pairs: every slot
                        // it decodes to is `SlotVector::Absent`, by design, because the
                        // vectors live in the per-slot CFs (#1894). Reading `slots`
                        // straight off the decoded row therefore found NOTHING on every
                        // record of every panel, so `slots.is_empty()` was always true and
                        // every adjudicated record was dropped by the `continue` below —
                        // silently, without a counter.
                        //
                        // That made `guard_calibrate` incapable of scoring a single record
                        // on any vault, including one with a perfect Bool corpus. It went
                        // unnoticed because the guard has always refused earlier, at
                        // PANEL_MISMATCH or BAD_CORPUS_ABSENT, and never reached this line
                        // on real data. `load_panel_dense_corpus` hydrates for exactly this
                        // reason; the guard has to as well.
                        let hydrated =
                            self.hydrated_constellation_at_snapshot(constellation.cx_id, snapshot)?;
                        let mut slots = BTreeMap::new();
                        for slot in &wanted {
                            if let Some(vector) = hydrated
                                .slots
                                .get(&SlotId::new(*slot))
                                .and_then(guard_dense_vector)
                            {
                                slots.insert(*slot, vector);
                            }
                        }
                        if slots.is_empty() {
                            // Counted, not swallowed. An adjudicated record that carries
                            // none of the requested slots is a real finding — it usually
                            // means the caller named a slot this panel does not measure, or
                            // measures only on a subset — and the previous silent `continue`
                            // is what let the hydration defect above hide.
                            corpus.adjudicated_without_guarded_slots += 1;
                            return Ok(crate::SynapseCalyxWalkStep::Continue);
                        }
                        if slots.len() != wanted.len() {
                            corpus
                                .adjudicated_incomplete_guarded_slots
                                .push(constellation.cx_id);
                            return Ok(crate::SynapseCalyxWalkStep::Continue);
                        }
                        let record = AdjudicatedRecord {
                            cx_id: constellation.cx_id,
                            slots,
                            source_pointer: constellation.input_ref.pointer,
                        };
                        if good {
                            corpus.good.push(record);
                        } else {
                            corpus.bad.push(record);
                        }
                        if corpus.records_scanned >= max_records {
                            return Ok(crate::SynapseCalyxWalkStep::Stop);
                        }
                        Ok(crate::SynapseCalyxWalkStep::Continue)
                    },
                )
            },
        )?;
        Ok(corpus)
    }

    /// Loads one record's dense vectors for the requested slots.
    fn load_record_slots(
        &self,
        panel_version: u32,
        cx_id: CxId,
        required_slots: &[SlotId],
    ) -> Result<BTreeMap<u16, Vec<f32>>, SynapseCalyxError> {
        self.with_panel_read_snapshot(
            panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| {
                let constellation = self.hydrated_constellation_at_snapshot(cx_id, snapshot)?;
                if constellation.panel_version != panel_version {
                    return Err(guard_error(
                        "SYNAPSE_CALYX_GUARD_QUERY_RECORD_PANEL_MISMATCH",
                        format!(
                            "record {cx_id} belongs to panel {}, not requested panel {panel_version}",
                            constellation.panel_version
                        ),
                        "supply a cx_id that exists in this exact panel",
                    ));
                }
                let mut slots = BTreeMap::new();
                for slot in required_slots {
                    let raw = slot.get();
                    if let Some(vector) = constellation
                        .slots
                        .get(slot)
                        .and_then(guard_dense_vector)
                    {
                        slots.insert(raw, vector);
                    }
                }
                Ok(slots)
            },
        )
    }
}

/// One compact, validated raw-vector matrix for a single guard slot.
///
/// `row_ids` is not incidental bookkeeping: it is the physical-record
/// provenance for every row handed to the canonical scorer, and therefore the diagnostic
/// bridge from a numerical failure back to the vault record that must be
/// repaired.
struct WardSlotMatrix {
    flat: Vec<f32>,
    row_ids: Vec<String>,
    dim: usize,
}

/// One bad record's exact nearest trusted exemplar. Keeping this provenance
/// beside the score makes an inseparable calibration corpus actionable: Ward
/// can name the physical rows whose pre-trigger measurements collide instead
/// of returning only an unreachable scalar threshold.
#[derive(Clone, Debug)]
struct NearestGoodMatch {
    bad_cx_id: String,
    good_cx_id: String,
    score: f32,
}

struct CalibrationSlotScores {
    good_scores: Vec<f32>,
    bad_scores: Vec<f32>,
    good_case_ids: Vec<String>,
    bad_case_ids: Vec<String>,
    nearest_good: Vec<NearestGoodMatch>,
}

/// Bounded physical-row evidence for the exact failure mode behind an
/// unreachable cosine threshold. A score of `1.0` means the selected lens gave
/// a known-bad record the same direction as a trusted record; no legal tau can
/// reject the former while accepting the latter. Record ids are safe audit
/// identifiers and the example list is deliberately capped.
fn calibration_collision_diagnostic(slot: u16, matches: &[NearestGoodMatch]) -> Option<String> {
    const MAX_EXAMPLES: usize = 4;
    let exact = matches
        .iter()
        // Scores come from the exact shared dense-cosine scorer, which clamps
        // only reduction-order excess to the legal maximum.
        .filter(|nearest| nearest.score >= 1.0)
        .collect::<Vec<_>>();
    if exact.is_empty() {
        return None;
    }
    let examples = exact
        .iter()
        .take(MAX_EXAMPLES)
        .map(|nearest| format!("{}->{}", nearest.bad_cx_id, nearest.good_cx_id))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!(
        "slot {slot} has {} exact bad-to-good cosine collision(s) at score 1.0; physical cx_id examples=[{examples}]",
        exact.len()
    ))
}

impl WardSlotMatrix {
    const fn rows(&self) -> usize {
        self.row_ids.len()
    }

    fn row(&self, index: usize) -> &[f32] {
        let start = index * self.dim;
        &self.flat[start..start + self.dim]
    }
}

/// Builds validated per-slot matrices and scores them with the exact canonical
/// function Ward invokes at serving time.
///
/// This is intentionally not dispatched through Forge. A GPU normalized-dot
/// reduction is mathematically equivalent, but its valid floating-point order
/// is not byte-identical to `dense_cosine`; a one-ulp difference at tau is a
/// different security decision. Calibration is off the hot path, so numeric
/// identity with serving owns this boundary.
fn calibration_slot_scores(
    good: &[AdjudicatedRecord],
    bad: &[AdjudicatedRecord],
    slot: u16,
) -> Result<CalibrationSlotScores, SynapseCalyxError> {
    let good_matrix = collect_slot_matrix(good, slot, "good", None)?;
    let good_dim = good_matrix.as_ref().map(|matrix| matrix.dim);
    let bad_matrix = collect_slot_matrix(bad, slot, "bad", good_dim)?;

    let Some(good_matrix) = good_matrix.as_ref() else {
        // Without one trusted vector there is no candidate space. The caller's
        // existing GOOD_CORPUS_INSUFFICIENT gate names that evidence deficit.
        return Ok(CalibrationSlotScores {
            good_scores: Vec::new(),
            bad_scores: Vec::new(),
            good_case_ids: Vec::new(),
            bad_case_ids: Vec::new(),
            nearest_good: Vec::new(),
        });
    };
    let good_scores = leave_one_out_scores(good_matrix, slot)?;
    let (bad_scores, nearest_good) = match bad_matrix.as_ref() {
        Some(matrix) => nearest_good_scores(matrix, good_matrix, slot)?,
        None => (Vec::new(), Vec::new()),
    };
    Ok(CalibrationSlotScores {
        good_scores,
        bad_scores,
        good_case_ids: good_matrix.row_ids.clone(),
        bad_case_ids: bad_matrix
            .as_ref()
            .map_or_else(Vec::new, |matrix| matrix.row_ids.clone()),
        nearest_good,
    })
}

/// Compacts every physical record carrying `slot` into a uniform row-major
/// matrix. Absent slots remain an evidence-count issue, but a present malformed
/// vector is structural corruption and fails with its exact record identity.
fn collect_slot_matrix(
    records: &[AdjudicatedRecord],
    slot: u16,
    role: &'static str,
    expected_dim: Option<usize>,
) -> Result<Option<WardSlotMatrix>, SynapseCalyxError> {
    let mut dim = expected_dim;
    let mut flat = Vec::new();
    let mut row_ids = Vec::new();
    for record in records {
        let Some(vector) = record.slots.get(&slot) else {
            continue;
        };
        let cx_id = record.cx_id.to_string();
        let row_identity = record
            .source_pointer
            .as_deref()
            .map_or_else(|| cx_id.clone(), |pointer| format!("{cx_id}@{pointer}"));
        if vector.is_empty() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_VECTOR_EMPTY",
                format!(
                    "adjudicated {role} record {cx_id} carries an empty dense vector for guard slot {slot}"
                ),
                "repair or re-embed the named physical record; an empty vector has no cosine direction and is never omitted from calibration",
            ));
        }
        let matrix_dim = *dim.get_or_insert(vector.len());
        if vector.len() != matrix_dim {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_VECTOR_DIM_MISMATCH",
                format!(
                    "adjudicated {role} record {cx_id} carries guard slot {slot} with dim {}, but this slot's calibration matrix requires dim {matrix_dim}",
                    vector.len()
                ),
                "repair or re-embed the named physical record at the panel slot's declared dimension; calibration never drops a mismatched row",
            ));
        }
        let mut norm_sq = 0.0_f32;
        for (element, value) in vector.iter().copied().enumerate() {
            if !value.is_finite() {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_VECTOR_NON_FINITE",
                    format!(
                        "adjudicated {role} record {cx_id} guard slot {slot} element {element} is non-finite: {value}"
                    ),
                    "repair or re-embed the named physical record with finite f32 values; calibration never drops a poisoned row",
                ));
            }
            norm_sq = value.mul_add(value, norm_sq);
        }
        if norm_sq == 0.0 {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_VECTOR_ZERO_NORM",
                format!(
                    "adjudicated {role} record {cx_id} guard slot {slot} has zero f32 norm and therefore no cosine direction"
                ),
                "repair or re-embed the named physical record with a directional vector; calibration never treats an undefined cosine as absent evidence",
            ));
        }
        if !norm_sq.is_finite() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_VECTOR_NORM_OVERFLOW",
                format!(
                    "adjudicated {role} record {cx_id} guard slot {slot} has finite elements whose squared norm overflows f32"
                ),
                "rescale or re-embed the named physical record so its norm is representable in f32; calibration never drops an overflowed row",
            ));
        }
        flat.try_reserve(vector.len()).map_err(|error| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_MATRIX_ALLOCATION",
                format!(
                    "reserve the {role} calibration matrix for guard slot {slot}, record {cx_id}: {error}"
                ),
                "reduce the bounded calibration corpus size or restore host memory, then retry the identical request",
            )
        })?;
        flat.extend_from_slice(vector);
        row_ids.push(row_identity);
    }
    let Some(dim) = dim.filter(|_| !row_ids.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(WardSlotMatrix { flat, row_ids, dim }))
}

/// Leave-one-out nearest-neighbour cosine per good exemplar: the in-region score
/// the guard would compute for that record matched to its nearest OTHER trusted
/// exemplar. Including itself would score a constant 1.0 and report FRR = 0.
fn leave_one_out_scores(good: &WardSlotMatrix, slot: u16) -> Result<Vec<f32>, SynapseCalyxError> {
    if good.rows() < 2 {
        return Ok(Vec::new());
    }
    let mut scores = Vec::with_capacity(good.rows());
    for query_index in 0..good.rows() {
        let (_, score) = matrix_best_match(good.row(query_index), good, Some(query_index), slot)?;
        scores.push(score);
    }
    Ok(scores)
}

/// Nearest-good cosine per bad case: the score the guard would compute if that
/// known-bad output were presented and matched to the trusted region.
fn nearest_good_scores(
    bad: &WardSlotMatrix,
    good: &WardSlotMatrix,
    slot: u16,
) -> Result<(Vec<f32>, Vec<NearestGoodMatch>), SynapseCalyxError> {
    if bad.rows() == 0 || good.rows() == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut scores = Vec::with_capacity(bad.rows());
    let mut nearest_good = Vec::with_capacity(bad.rows());
    for query_index in 0..bad.rows() {
        let (candidate_index, score) = matrix_best_match(bad.row(query_index), good, None, slot)?;
        scores.push(score);
        nearest_good.push(NearestGoodMatch {
            bad_cx_id: bad.row_ids[query_index].clone(),
            good_cx_id: good.row_ids[candidate_index].clone(),
            score,
        });
    }
    Ok((scores, nearest_good))
}

fn matrix_best_match(
    query: &[f32],
    candidates: &WardSlotMatrix,
    excluded_index: Option<usize>,
    slot: u16,
) -> Result<(usize, f32), SynapseCalyxError> {
    let mut best = None;
    for candidate_index in 0..candidates.rows() {
        if excluded_index == Some(candidate_index) {
            continue;
        }
        let score = dense_cosine(query, candidates.row(candidate_index)).ok_or_else(|| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_SCORE_INVALID",
                format!(
                    "canonical {DENSE_COSINE_SCORING_ENGINE} could not score guard slot {slot} candidate row {candidate_index} ({})",
                    candidates.row_ids[candidate_index]
                ),
                "inspect the named physical row; calibration and serving both refuse an undefined dense cosine",
            )
        })?;
        if best.is_none_or(|(_, current)| score > current) {
            best = Some((candidate_index, score));
        }
    }
    best.ok_or_else(|| {
        guard_error(
            "SYNAPSE_CALYX_GUARD_SCORE_CANDIDATE_MISSING",
            format!(
                "guard slot {slot} has no identity-distinct candidate after applying excluded_index={excluded_index:?} to {} row(s)",
                candidates.rows()
            ),
            "provide at least two adjudicated good records carrying the guarded slot; calibration never scores a record against itself",
        )
    })
}

fn conservative_bad_guard_score(score: f32, tolerance: f32) -> f32 {
    (score + tolerance).min(1.0)
}

fn conservative_good_guard_score(score: f32, tolerance: f32) -> f32 {
    (score - tolerance).max(-1.0)
}

/// Independently replays the persisted Guard combination policy over the exact
/// aligned calibration rows. This does not reuse Ward's joint-score reducer:
/// it evaluates the same per-slot comparisons the serving guard performs and
/// therefore catches row-order, tau, or policy drift before persistence.
fn calibration_policy_accepts(
    profile: &GuardProfile,
    inputs: &[CalibrationInput],
    bad: bool,
) -> Result<(usize, usize), SynapseCalyxError> {
    let first = inputs.first().ok_or_else(|| {
        guard_error(
            "SYNAPSE_CALYX_GUARD_POLICY_EVIDENCE_EMPTY",
            "guard policy readback has no calibration inputs".to_owned(),
            "name at least one dense active guard slot",
        )
    })?;
    let first_ids = if bad {
        &first.bad_case_ids
    } else {
        &first.good_case_ids
    };
    for input in inputs {
        let ids = if bad {
            &input.bad_case_ids
        } else {
            &input.good_case_ids
        };
        let scores = if bad {
            &input.bad_scores
        } else {
            &input.good_scores
        };
        if ids != first_ids || ids.len() != scores.len() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_POLICY_CORPUS_MISALIGNED",
                format!(
                    "guard slot {} has {} {} ids and {} scores, but the policy corpus has {} ordered ids",
                    input.slot,
                    ids.len(),
                    if bad { "bad" } else { "good" },
                    scores.len(),
                    first_ids.len(),
                ),
                "repair the physical calibration scan; a joint Guard policy is never certified over count-only or differently ordered slot corpora",
            ));
        }
    }
    let mut accepted = 0usize;
    for row_index in 0..first_ids.len() {
        let mut pass_count = 0usize;
        for input in inputs {
            let score = if bad {
                conservative_bad_guard_score(input.bad_scores[row_index], input.score_tolerance)
            } else {
                conservative_good_guard_score(input.good_scores[row_index], input.score_tolerance)
            };
            let tau = profile.tau_for(&input.slot).ok_or_else(|| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_TAU_MISSING",
                    format!("joint guard profile has no tau for slot {}", input.slot),
                    "repair calyx-ward calibration; every required slot must carry an explicit finite tau",
                )
            })?;
            pass_count += usize::from(score >= tau);
        }
        let passes = match profile.policy {
            GuardPolicy::AllRequired => pass_count == inputs.len(),
            GuardPolicy::KofN { k } if k > 0 && k <= inputs.len() => pass_count >= k,
            GuardPolicy::KofN { k } => {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_POLICY_INVALID",
                    format!(
                        "joint guard policy k={k} is invalid for {} slots",
                        inputs.len()
                    ),
                    "repair the Guard profile with 1 <= k <= required slot count",
                ));
            }
        };
        accepted += usize::from(passes);
    }
    Ok((accepted, first_ids.len()))
}

/// Index of and cosine to the best-matching good exemplar on `slot`.
fn best_match(vector: &[f32], good: &[AdjudicatedRecord], slot: u16) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (index, record) in good.iter().enumerate() {
        let Some(candidate) = record.slots.get(&slot) else {
            continue;
        };
        let Some(cos) = dense_cosine(vector, candidate) else {
            continue;
        };
        if best.is_none_or(|(_, current)| cos > current) {
            best = Some((index, cos));
        }
    }
    best
}

/// Upper bound on the search for a certifiable corpus size. Past this the
/// requested `target_far`/`alpha` pair is reported as uncertifiable rather than
/// searched forever.
const MAX_CERTIFIABLE_SEARCH: usize = 10_000_000;

/// Smallest bad-corpus size at which `target_far` is certifiable at `alpha`.
///
/// The exact one-sided Clopper-Pearson upper bound with zero observed false
/// accepts requires `(1 - target_far)^n <= alpha` — the "rule of three" made
/// exact. Solved here by repeated multiplication so no float is ever narrowed
/// to an integer. With `target_far = 0.01` and `alpha = 0.05` the answer is
/// 299: `0.99^298 = 0.050037 > 0.05` and `0.99^299 = 0.049536 <= 0.05`.
#[must_use]
pub fn min_certifiable_bad_scores(target_far: f32, alpha: f32) -> usize {
    let target = f64::from(target_far);
    let alpha = f64::from(alpha);
    if !(0.0..1.0).contains(&target) || target <= 0.0 || alpha <= 0.0 {
        return usize::MAX;
    }
    if alpha >= 1.0 {
        return 1;
    }
    let survival_step = 1.0 - target;
    let mut survival = survival_step;
    for n in 1..=MAX_CERTIFIABLE_SEARCH {
        if survival <= alpha {
            return n;
        }
        survival *= survival_step;
    }
    usize::MAX
}

/// Exact binomial tail `P(X <= successes | trials, probability)`.
///
/// This is the tail the one-sided Clopper-Pearson upper bound inverts, and it is
/// recomputed here as an INDEPENDENT readback of the gate `calyx_ward::calibrate`
/// applies internally — a returned tau whose tail exceeds `alpha` was ward's
/// conservative fallback, not a certified threshold.
#[must_use]
pub fn clopper_pearson_tail(successes: usize, trials: usize, probability: f32) -> f64 {
    let probability = f64::from(probability);
    if successes >= trials {
        return 1.0;
    }
    if probability <= 0.0 {
        return 1.0;
    }
    if probability >= 1.0 {
        return 0.0;
    }
    let complement = 1.0 - probability;
    let trials_f = count_as_f64(trials);
    let mut term = complement.powi(i32::try_from(trials).unwrap_or(i32::MAX));
    let mut sum = term;
    for index in 0..successes {
        let index = count_as_f64(index);
        term *= (trials_f - index) / (index + 1.0) * probability / complement;
        sum += term;
        if sum > 1.0 {
            return 1.0;
        }
    }
    sum
}

/// Widens a corpus count to `f64` without a lossy primitive cast. Counts are
/// bounded by the scan record cap, far below `u32::MAX`.
fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

fn guard_dense_vector(vector: &SlotVector) -> Option<Vec<f32>> {
    match vector {
        SlotVector::Dense { data, .. } => Some(data.clone()),
        SlotVector::Sparse { .. } | SlotVector::Multi { .. } | SlotVector::Absent { .. } => None,
    }
}

fn fraction(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        count_as_f64(count) / count_as_f64(total)
    }
}

fn guard_error(
    code: &'static str,
    message: String,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message, remediation)
}

/// The vault's own millisecond clock, sampled once so every calibration record
/// in one pass carries the same timestamp.
struct FixedGuardClock(Ts);

impl Clock for FixedGuardClock {
    fn now(&self) -> Ts {
        self.0
    }
}
