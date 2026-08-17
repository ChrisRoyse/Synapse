use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::time::Duration;

use calyx_core::{
    AbsentReason, Anchor, AnchorKind, AnchorValue, Asymmetry, CalyxError, CalyxErrorCode,
    Constellation, CxFlags, CxId, Input, InputRef, LedgerRef, Lens, METADATA_SOURCE_EVENT_TIME_RAW,
    METADATA_SOURCE_EVENT_TIME_SECS, METADATA_SOURCE_SEQUENCE, METADATA_TEMPORAL_INACTIVE_REASON,
    METADATA_TEMPORAL_LANE_STATE, Modality, Panel, QuantPolicy, Slot, SlotId, SlotKey,
    SlotResource, SlotState, SlotVector, TEMPORAL_LANE_ACTIVE, TEMPORAL_LANE_INACTIVE,
    TEMPORAL_MISSING_CREATED_AT, VaultId,
};
use calyx_mincut::{
    StructuralParams, TransitionEdge, build_transition_graph, structural_signatures,
};
use calyx_registry::measure::{absent, input_hash};
use calyx_registry::{
    AlgorithmicEncoder as RegistryAlgorithmicEncoder, AlgorithmicLens, LensRuntime, LensSpec,
    Registry, default_recall_delta, measure_registry_batch_with_runtime_limit,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_calyx::SynapseCalyxPutDisposition;
use synapse_calyx::SynapseCalyxVault;
use synapse_calyx::lens_provenance;
use synapse_calyx::panel_lifecycle::{
    SynapseCalyxDerivedGraphRow, SynapseCalyxDerivedSnapshotReadback,
    SynapseCalyxDerivedSnapshotRequest,
};
use synapse_core::types::{
    AgentEndState, AgentEventKind, AgentEventRecord, AgentTranscriptRecord, EpisodeBoundary,
    EpisodeRecord, GenAiOperationName, SensorStatus, StoredObservation, StoredReflexAudit,
    TimelineActor, TimelineKind, TimelineRecord, TranscriptParseStatus, TranscriptRole,
    TranscriptSource,
};
use synapse_telemetry::metrics::{
    CALYX_CONSTELLATION_MEASUREMENT_DURATION_US, CALYX_CONSTELLATION_MEASUREMENT_ERRORS_TOTAL,
    CALYX_CONSTELLATION_MEASUREMENTS_TOTAL, CALYX_SLOT_LENS_REFUSED_TOTAL,
};

// Ingest and active-panel publication must derive frozen lens identities through
// the same implementation. Two independent AlgorithmicLens types previously
// allowed their norm fingerprints to drift and made valid batched measurement
// fail because one declared slot acquired two content addresses.
type RegistryAlgorithmicLens = AlgorithmicLens;

use crate::{GroundingAnchor, GroundingAnchorValue, StorageError, StorageResult, cf};

pub const SYN_TIMELINE_PANEL_NAME: &str = "syn-timeline-v1";
/// Current timeline slot layout.
///
/// #1900 added `TL_SLOT_TITLE_BM25`, a raw term-frequency lexical lane, because
/// the normalized signed lane it sits beside cannot carry the term frequency or
/// the document length BM25 ranks on. A `panel_version` identifies a slot
/// layout, and the Base row's slot membership is immutable once written — a
/// qualified slot write into a row that never declared the slot is refused by
/// design — so a new lens on this panel is a new generation, re-measured from
/// the authoritative `CF_TIMELINE` rows, exactly as #1776 did.
/// #1963 added `TL_SLOT_RECORD_VECTOR`, the panel's first **graded** dense lens.
/// Before it, all five dense lenses on this panel returned nearest-neighbour
/// cosine exactly `1.0` for all 924 records: four saturated a finite image
/// (one-hot over ~12 kinds, cyclic over 24 hours / 7 days) and the fifth
/// (`event_time_rank`, dense `dim = 1` over `[0, 1]`) could never return
/// anything but `+1`. The panel therefore had no lens whose "these two records
/// are alike" was a measurement, so no neighbourhood analysis on it — blind
/// spots, find-similar ranking, the between-record graph — could resolve
/// anything. Measured manually on a frozen copy of the live vault, the new lens
/// returns 42 distinct
/// nearest-neighbour values over `[0.953, 1.000]` with a modal share of 0.578.
pub const SYN_TIMELINE_PANEL_VERSION: u32 = 1_963_001;
/// The timeline layout #1963 superseded, kept named so an audit can identify
/// rows written before the graded record-vector lens existed.
pub const SYN_TIMELINE_PANEL_VERSION_PRE_1963: u32 = 1_900_001;
/// The timeline layout #1900 superseded, kept named so an audit can identify
/// rows written before the BM25 lane existed rather than guess at a number.
pub const SYN_TIMELINE_PANEL_VERSION_PRE_1900: u32 = 1_664_001;
pub const SYN_EPISODE_PANEL_NAME: &str = "syn-episode-v1";
/// Current episode slot layout.
///
/// #1904 added `EP_SLOT_TITLE_BM25`. Same reasoning as the timeline bump above:
/// a `panel_version` identifies a slot layout and Base slot membership is
/// immutable once written, so a new lens is a new generation re-measured from
/// the authoritative `CF_EPISODES` rows. Adding the slot without bumping this
/// now fails closed with `CALYX_ASTER_PANEL_SLOT_SET_IMMUTABLE` (#1903) rather
/// than being dropped.
pub const SYN_EPISODE_PANEL_VERSION: u32 = 1_964_001;
/// The episode generation superseded by #1964.
///
/// `syn.episode.record_vector.v1` (slot 22) fed `start_unix_ms` and
/// `end_unix_ms` — ~1.7e12 — beside counts in `0..1e4`. `syn_record_vector`
/// weights each field by its raw magnitude and unit-normalizes, so the vector
/// was a re-encoding of the episode's clock and every other field sat ~1e-9
/// below it, far under `f32` resolution. Measured over all 171 stored slot-22
/// vectors on the live vault: nearest-neighbour cosine `1.000000` for every
/// record, `distinct = 1`. The generation is superseded rather than edited
/// because a frozen lens never changes meaning: slot 22's rows still decode as
/// what they were measured as.
pub const SYN_EPISODE_PANEL_VERSION_PRE_1964: u32 = 1_904_002;
/// The episode layout #1904 superseded.
pub const SYN_EPISODE_PANEL_VERSION_PRE_1904: u32 = 1_664_002;
pub const SYN_AGENT_EVENT_PANEL_NAME: &str = "syn-agent-event-v1";
pub const SYN_AGENT_EVENT_PANEL_VERSION: u32 = 1_965_001;
pub const SYN_AGENT_EVENT_PANEL_VERSION_PRE_1965: u32 = 1_983_001;
pub const SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983: u32 = 1_665_001;
pub const SYN_AGENT_TRANSCRIPT_PANEL_NAME: &str = "syn-agent-transcript-v1";
/// Current agent-transcript slot layout.
///
/// #1904 added `AT_SLOT_TEXT_BM25`, giving the largest text corpus on the vault
/// its first lexically-rankable lane. #1921 then measured that lane and found it
/// could see only 9.2% of the corpus, and added `AT_SLOT_TEXT_FULL_BM25` over
/// the prose the record already carried but no lens had ever read. See the
/// episode constant above for why a new lens is a new generation.
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION: u32 = 1_965_002;
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1965: u32 = 1_983_002;
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1983: u32 = 1_921_001;
/// The agent-transcript layout #1921 superseded — the one #1904 introduced.
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1921: u32 = 1_904_003;
/// The agent-transcript layout #1904 superseded.
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1904: u32 = 1_665_002;
// #1776 moved these panels off their old panel-local slot ids onto exclusive
// GLOBAL blocks. A panel_version identifies a slot layout, so the new layout
// gets a new version: reusing the old one would make a single panel_version
// mean two different slot maps, and every row written under it before the move
// would be reinterpreted under the new meaning. The `_1776` generation is the
// first that writes into the panel's own block.
pub const SYN_ACTION_PANEL_NAME: &str = "syn-action-v1";
/// #2050 adds [`ACT_SLOT_TARGET_VECTOR`], the panel's first **dense,
/// similarity-bearing** representation of the action's target identity.
///
/// A new lens is a new generation, for the reason the episode and
/// agent-transcript constants above already record: a `panel_version` names one
/// immutable slot layout, and every row written under it was measured with
/// exactly that set of frozen lens ids. Adding slot 117 to `2020001` would make
/// one version mean two different slot maps.
///
/// ## Why the generation was unavoidable here, and not a matter of taste
///
/// `2020001`'s only target lane is [`ACT_SLOT_TARGET_HASH`], a `syn_hash` — a
/// deliberately **sparse** one-cell whole-value hash. Ward scores exclusively
/// dense vectors (`calyx_ward::validate_calibration_slots` admits only dense
/// `Active` slots, `synapse_calyx::ward::guard_dense_vector` discards every
/// `SlotVector::Sparse`, and the enforced score is `dense_cosine`), so slot 49
/// reports zero usable exemplars on every calibration forever. Measured live on
/// 2026-08-07, excluding it did not help: dry calibrations of slots 48, 50, 51
/// and 52 each failed closed with `CALYX_GUARD_PROVISIONAL` at worst bad score
/// 1.0 and tau 1.0000001, because a kind one-hot, a record summary and two
/// clock cycles are identical between a success and a failure of the same
/// action. No slot of `2020001` can carry a target-identity guard.
///
/// A sparse guard metric in Ward was considered and rejected: even under a
/// Jaccard/Dice metric a one-cell whole-value hash has cosine exactly `{0, 1}`,
/// so it is not similarity-bearing under *any* metric, and changing Ward's
/// score function would silently redefine every already-certified conformal FAR
/// bound and the `calyx-search` guarded reader that consumes the same profile.
/// #2185 moves the exact #2050 layout to a fresh generation after an earlier
/// pre-merge build wrote a different slot map under `2_050_001`. A generation
/// is the durable identity of one slot contract; once two physical layouts have
/// used an id, that id cannot truthfully name either layout again.
///
/// Generation `2_185_002` adds [`ACT_SLOT_REQUEST_VECTOR`], the first lane that
/// measures the actual pre-execution request rather than only its tool, verb,
/// target and clock. The request is taken exclusively from point-in-time audit
/// fields that existed before the outcome: command `payload_bounded` plus its
/// redacted full-payload digest, or the explicit action-audit preflight request
/// snapshot. Terminal response/error details are excluded.
/// Status, error, response and after-state fields are never read. This is a new
/// immutable generation because adding the slot to `2_185_001` would silently
/// reinterpret every already-persisted constellation under a layout it never
/// carried.
///
/// Generation `2_185_003` keeps that exact, audit-grade lane and adds two
/// independently measurable bounded causes: payload-size class and structural-
/// shape class. The exact request lane took 217 values over 457 paired records
/// in the first physical assay. Repeated values correctly selected the discrete
/// estimator, but 225 occupied outcome cells required 1,125 samples for its
/// Miller-Madow bias bound, so the lane was honestly unmeasured. Flattening or
/// deleting exact identity would lose audit truth. These two coarser lenses are
/// separate slots instead: each is point-in-time, target-blind, finite-support,
/// and suitable for bits, Ward, and the exhaustive association maps.
pub const SYN_ACTION_PANEL_VERSION: u32 = 2_185_003;
/// The exact request-vector generation superseded by the bounded request-cause lanes.
///
/// It remains readable history and is always re-measured from its source
/// rows rather than having new slot meanings grafted onto it.
pub const SYN_ACTION_PANEL_VERSION_PRE_REQUEST_CLASSES: u32 = 2_185_002;
/// The target-vector-only generation superseded by the request-cause lane.
pub const SYN_ACTION_PANEL_VERSION_PRE_REQUEST: u32 = 2_185_001;
/// The contaminated generation #2185 superseded. It remains readable history,
/// but new rows must never join its mixed physical slot layouts.
pub const SYN_ACTION_PANEL_VERSION_PRE_2185: u32 = 2_050_001;
/// The generation #2050 superseded — the layout with no dense target lane.
pub const SYN_ACTION_PANEL_VERSION_PRE_2050: u32 = 2_020_001;
pub const SYN_ACTION_PANEL_VERSION_PRE_2020: u32 = 2_006_001;
pub const SYN_ACTION_PANEL_VERSION_PRE_2006: u32 = 1_965_003;
pub const SYN_ACTION_PANEL_VERSION_PRE_1965: u32 = 1_776_001;
pub const SYN_REFLEX_PANEL_NAME: &str = "syn-reflex-v1";
pub const SYN_REFLEX_PANEL_VERSION: u32 = 1_965_004;
pub const SYN_REFLEX_PANEL_VERSION_PRE_1965: u32 = 1_776_002;
pub const SYN_PROCESS_PANEL_NAME: &str = "syn-process-v1";
pub const SYN_PROCESS_PANEL_VERSION: u32 = 1_965_005;
pub const SYN_PROCESS_PANEL_VERSION_PRE_1965: u32 = 1_776_003;
pub const SYN_OBSERVATION_PANEL_NAME: &str = "syn-observation-v1";
pub const SYN_OBSERVATION_PANEL_VERSION: u32 = 1_965_006;
pub const SYN_OBSERVATION_PANEL_VERSION_PRE_1965: u32 = 1_776_004;
pub const SYN_OUTCOME_PANEL_NAME: &str = "syn-outcome-v1";
pub const SYN_OUTCOME_PANEL_VERSION: u32 = 1_965_008;
pub const SYN_OUTCOME_PANEL_VERSION_PRE_1965: u32 = 1_776_005;
pub const SYN_MCP_USAGE_PANEL_NAME: &str = "syn-mcp-usage-v1";
pub const SYN_MCP_USAGE_PANEL_VERSION: u32 = 1_965_007;
pub const SYN_MCP_USAGE_PANEL_VERSION_PRE_1965: u32 = 1_776_006;
pub const SYN_RECURRENCE_SUBJECT_PANEL_NAME: &str = "syn-recurrence-subject-v1";
pub const SYN_RECURRENCE_SUBJECT_PANEL_VERSION: u32 = 1_776_007;

/// Panel versions superseded by the #1776 global slot allocation.
///
/// Rows carrying these versions were written when slot ids were panel-local, so
/// their vectors sit in `cf/slot_01`..`cf/slot_08` alongside other panels'. They
/// are not readable as this panel's current layout and must never be compared
/// with rows written under the current version. Kept as named constants so the
/// migration and any audit can identify them exactly rather than by magic
/// number.
pub const SYN_PRE_1776_PANEL_VERSIONS: &[(&str, u32)] = &[
    (SYN_ACTION_PANEL_NAME, 1_666_001),
    (SYN_REFLEX_PANEL_NAME, 1_666_002),
    (SYN_PROCESS_PANEL_NAME, 1_666_003),
    (SYN_OBSERVATION_PANEL_NAME, 1_666_004),
    (SYN_OUTCOME_PANEL_NAME, 1_669_001),
    (SYN_MCP_USAGE_PANEL_NAME, 1_691_001),
    (SYN_RECURRENCE_SUBJECT_PANEL_NAME, 1_667_001),
];
// Graph-structural / hierarchy encoder panels (#1685). These are derived panels
// built off-line from a fingerprinted graph/hierarchy snapshot, not per source
// row. The version constant is the family's first (snapshot-0) generation; a new
// snapshot allocates a fresh vault-global generation via the #1668 lifecycle
// allocator, so `panel_version` is a builder parameter here.
pub const SYN_GRAPHPOS_APP_PANEL_NAME: &str = "syn-graphpos-app-v1";
pub const SYN_GRAPHPOS_APP_PANEL_VERSION: u32 = 1_685_001;
pub const SYN_GRAPHPOS_PROCESS_PANEL_NAME: &str = "syn-graphpos-process-v1";
pub const SYN_GRAPHPOS_PROCESS_PANEL_VERSION: u32 = 1_685_002;
pub const SYN_PATH_HIERARCHY_PANEL_NAME: &str = "syn-path-hierarchy-v1";
pub const SYN_PATH_HIERARCHY_PANEL_VERSION: u32 = 1_685_003;
pub const SYN_MCP_USAGE_KEY_PREFIX: &[u8] = b"mcp-usage/v1/";
pub const SYN_OUTCOME_KEY_PREFIX: &[u8] = b"escalation/v1/audit/";
pub const SYN_OUTCOME_BACKFILL_SOURCE: &str = "CF_KV:escalation/v1/audit/";

/// Every `CF_KV` row family a physical writer measures into `syn-outcome-v1`.
///
/// **#1984.** The declaration used to be the single prefix
/// [`SYN_OUTCOME_KEY_PREFIX`], and it was wrong — not a little wrong, but wrong
/// in a way that made the panel's own repair refuse its own work. The census
/// enumerated a stranded identity at
/// `approval/v1/audit/apr1-019f7466…/00000001784364334939-…`, the exact-key
/// branch of `Db::backfill_temporal_metadata` refused it as "outside
/// escalation/v1/audit/", and the panel's anchor debt could not move on any tick.
///
/// The physical writers settle which of the two was wrong, and it is the
/// declaration. [`anchor_panel_for_source_cf`] maps **all** of `CF_KV` (bar the
/// `mcp-usage/v1/` prefix) to this panel, and four call sites use that mapping to
/// write outcome constellations into it:
///
/// | writer | key family |
/// |---|---|
/// | `synapse-mcp/src/server/escalation/mod.rs` | `escalation/v1/audit/` |
/// | `synapse-mcp/src/m3/approvals.rs` | `approval/v1/audit/` |
/// | `synapse-mcp/src/server/verification.rs` | `verification/audit/v1/` |
/// | `synapse-mcp/src/m3/suggestions.rs` | `suggestion/v1/` |
///
/// (`CF_ROUTINE_STATE` also feeds this panel, from `m3/routines.rs`, but it is a
/// different column family and is reported as
/// `anchors_stranded_source_cf_unmeasured` rather than silently folded in here.)
///
/// Re-measuring any of these through the outcome path is byte-exact:
/// [`outcome_constellation_input_bytes`] frames `(cf::CF_KV, key, value)`
/// identically for the live write and for the backfill, so the re-measured
/// `cx_id` is the same content address the original writer produced. The prefix
/// is a *population* declaration, never an input to the identity.
///
/// Ordered and disjoint, which is what lets a paged sweep walk them as one
/// logical source with a physical cursor that identifies its own prefix.
pub const SYN_OUTCOME_KEY_PREFIXES: &[&[u8]] = &[
    b"approval/v1/audit/",
    b"escalation/v1/audit/",
    b"suggestion/v1/",
    b"verification/audit/v1/",
];

/// The declared outcome prefix one `CF_KV` key belongs to, or `None`.
///
/// Used by the exact-key repair to admit any declared family rather than one of
/// them. Never a `starts_with` on the panel's *whole* CF: `CF_KV` also holds the
/// MCP-usage panel's rows and a great deal that is no panel's population at all,
/// so admitting the family would reinterpret unrelated rows as outcomes.
#[must_use]
pub fn outcome_backfill_prefix_for_key(key: &[u8]) -> Option<&'static [u8]> {
    SYN_OUTCOME_KEY_PREFIXES
        .iter()
        .copied()
        .find(|prefix| key.starts_with(prefix))
}

/// The declared outcome prefixes rendered for an error or a log line.
#[must_use]
pub fn outcome_backfill_prefixes_display() -> String {
    SYN_OUTCOME_KEY_PREFIXES
        .iter()
        .map(|prefix| String::from_utf8_lossy(prefix).into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}
/// Logical backfill source for the MCP-usage subset of the shared KV family.
///
/// This is deliberately not `CF_KV`: using the whole family would reinterpret
/// unrelated outcome rows as MCP usage and make a bounded migration impossible.
pub const SYN_MCP_USAGE_BACKFILL_SOURCE: &str = "CF_KV:mcp-usage/v1/";
pub const SYN_OBSERVATION_SAMPLE_EVERY_N_ENV: &str = "SYNAPSE_CALYX_OBSERVATION_SAMPLE_EVERY_N";
pub const SYN_OBSERVATION_SAMPLE_EVERY_N_DEFAULT: u64 = 10;

pub const META_PANEL_NAME: &str = "synapse_panel_name";
// One definition, shared with the layer that reads these back for the #1940
// orphan probe. Re-exported rather than re-spelled, so the writer and the
// reader cannot drift into two different strings for one key.
pub use synapse_calyx::{
    METADATA_SOURCE_CF as META_SOURCE_CF, METADATA_SOURCE_KEY_HEX as META_SOURCE_KEY_HEX,
};
pub const META_RAW_SHA256: &str = "synapse_raw_sha256";
pub const META_RAW_LEN_BYTES: &str = "synapse_raw_len_bytes";
pub const META_TIME_BASIS: &str = "synapse_time_basis";
pub const META_EXACT_TS_NS: &str = "synapse_ts_ns";
pub const META_RECENCY_BASIS: &str = "synapse_recency_basis";

const POINTER_SCHEME: &str = "synapse";
const TIME_BASIS_UTC: &str = "utc";
const RECENCY_BASIS_EVENT_TIME_RANK: &str = "frozen_event_unix_ms_rank_1970_2100";
/// Highest global slot id any Synapse panel may durably claim.
///
/// This is a Synapse-side allocation ceiling, not a Calyx limit: `SlotId` is a
/// `u16` and `cf/slot_<id>` directories are named from it, so there is ample
/// headroom. Raise it when a new panel block needs room; the compile-time
/// assertion on `PANEL_SLOT_BLOCKS` keeps the two in agreement.
const CALYX_DURABLE_SLOT_ID_MAX: u16 = 124;
const MAX_EXACT_F64_INT: u64 = 9_007_199_254_740_991;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;
const SECS_PER_HOUR: u64 = 60 * 60;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;
const RANK_BOUND_MICROS_PER_UNIT: i64 = 1_000_000;
const RECENCY_RANK_MAX_UNIX_MS: i64 = 4_102_444_800_000;
const RECENCY_RANK_MAX_UNIX_MS_MICROS: i64 = RECENCY_RANK_MAX_UNIX_MS * RANK_BOUND_MICROS_PER_UNIT;
const MAX_DAY_DURATION_MS: i64 = 86_400_000;
const MAX_DAY_DURATION_MS_MICROS: i64 = MAX_DAY_DURATION_MS * RANK_BOUND_MICROS_PER_UNIT;
/// Saturation point for `syn.agent_transcript.line_rank.v1`, in transcript lines.
///
/// Unlike its sibling bounds this is NOT a quantity that cannot be exceeded:
/// year 2100 (`RECENCY_RANK_MAX_UNIX_MS`) and 24 hours (`MAX_DAY_DURATION_MS`)
/// are real ceilings, but an agent transcript has no maximum line count. This
/// is the point past which the line number stops discriminating for a
/// retrieval-only ordering ordinate, so inputs above it saturate here rather
/// than failing measurement — see [`saturating_rank_input`] (#2030).
///
/// The value is byte-identical to the literal it replaced (`10_000_000_000`
/// micros), because it is part of the lens id `syn_scalar_rank:0:10000000000`
/// and therefore of the frozen panel contract. Changing it is a panel version
/// bump and a full re-measure, never an edit here alone.
const AT_LINE_RANK_MAX_LINES: i64 = 10_000;
const AT_LINE_RANK_MAX_LINES_MICROS: i64 = AT_LINE_RANK_MAX_LINES * RANK_BOUND_MICROS_PER_UNIT;
const _: () = assert!(
    AT_LINE_RANK_MAX_LINES_MICROS == 10_000_000_000,
    "syn.agent_transcript.line_rank.v1 resolves to the lens id \
     syn_scalar_rank:0:10000000000, which is part of the frozen panel contract every stored \
     record was measured under. Naming the bound (#2030) must not move it: changing this value \
     silently re-measures nothing while making stored vectors mean something else. Bump the \
     panel version and re-measure instead"
);

const TL_SLOT_KIND_ONEHOT: SlotId = SlotId::new(1);
const TL_SLOT_APP_HASH: SlotId = SlotId::new(2);
const TL_SLOT_TITLE_SPARSE: SlotId = SlotId::new(3);
const TL_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(4);
const TL_SLOT_DOW_CYCLIC: SlotId = SlotId::new(5);
const TL_SLOT_ACTOR_ONEHOT: SlotId = SlotId::new(6);
const TL_SLOT_RECENCY_RANK: SlotId = SlotId::new(7);
/// Raw term-frequency lexical lane over the same title text `TL_SLOT_TITLE_SPARSE`
/// hashes (#1900).
///
/// Slot ids are allocated vault-globally in exclusive per-panel blocks (#1776),
/// and the timeline panel's original block `1..=7` is boxed in by the episode
/// panel at 8. A block is contiguous by construction, so a new timeline lens
/// cannot extend it — the timeline panel gets a **second** block, `103..=106`,
/// declared in `PANEL_SLOT_BLOCKS`. The write-time guard rejected this exact id
/// on a fresh vault before any row was written, which is what the guard is for.
const TL_SLOT_TITLE_BM25: SlotId = SlotId::new(103);
/// The panel's graded dense lens (#1963), in the same second block as 103.
///
/// See [`SYN_TIMELINE_PANEL_VERSION`] for why the panel needed one and
/// [`timeline_numeric_record`] for why every field it measures is placed on a
/// comparable scale first.
const TL_SLOT_RECORD_VECTOR: SlotId = SlotId::new(104);
/// Dimension of the timeline record vector.
///
/// `timeline_numeric_record` emits 11 fields, which `syn_record_vector` places
/// by a signed hash of the field path. 32 buckets keeps the expected collision
/// count under one while staying the smallest power of two that does so, and a
/// collision is a merge of two already-normalized components rather than a lost
/// field.
const TL_RECORD_VECTOR_DIM: u32 = 32;
/// Frozen scale for the log-normalized app-name length component.
const TL_APP_LEN_SCALE: f64 = 64.0;
/// Frozen scale for the log-normalized title length component.
const TL_TITLE_LEN_SCALE: f64 = 128.0;
/// Frozen scale for the log-normalized url-host length component.
const TL_URL_HOST_LEN_SCALE: f64 = 64.0;
/// Frozen scale for the log-normalized raw-row-length component.
const TL_RAW_LEN_SCALE: f64 = 4096.0;

const EP_SLOT_APP_HASH: SlotId = SlotId::new(8);
const EP_SLOT_DOCUMENT_HASH: SlotId = SlotId::new(9);
const EP_SLOT_URL_HOST_HASH: SlotId = SlotId::new(10);
const EP_SLOT_TITLE_SPARSE: SlotId = SlotId::new(11);
const EP_SLOT_START_HOUR_CYCLIC: SlotId = SlotId::new(12);
const EP_SLOT_START_DOW_CYCLIC: SlotId = SlotId::new(13);
const EP_SLOT_DURATION_LOG1P: SlotId = SlotId::new(14);
const EP_SLOT_DURATION_RANK: SlotId = SlotId::new(15);
const EP_SLOT_KEYSTROKES_ZSCORE: SlotId = SlotId::new(16);
const EP_SLOT_CLICKS_ZSCORE: SlotId = SlotId::new(17);
const EP_SLOT_ROW_COUNT_ZSCORE: SlotId = SlotId::new(18);
const EP_SLOT_STARTED_BOUNDARY_ONEHOT: SlotId = SlotId::new(19);
const EP_SLOT_ENDED_BOUNDARY_ONEHOT: SlotId = SlotId::new(20);
const EP_SLOT_INTERRUPTION_RATIO: SlotId = SlotId::new(21);
// Slot 22 was `syn.episode.record_vector.v1`, superseded by #1964 — see
// [`SYN_EPISODE_PANEL_VERSION_PRE_1964`]. Its id stays inside the episode
// block below so nothing else can claim `cf/slot_22`, and its rows keep
// decoding as what they were measured as.
/// The episode panel's graded dense lens (#1964), measured by
/// `syn_record_vector_unit_fields` over [`episode_numeric_record`], every field
/// of which is on a comparable scale in `[0, 1]`.
///
/// A new id rather than a re-use of 22: a slot id names one physical global
/// column family, and writing a differently-meaning vector into `cf/slot_22`
/// would leave one CF holding two incomparable populations that every search
/// rebuild, cross-term and assay scans as one.
const EP_SLOT_RECORD_VECTOR: SlotId = SlotId::new(113);
/// Raw term-frequency lexical lane over the same title text
/// `EP_SLOT_TITLE_SPARSE` hashes (#1904). The episode panel's `8..=22` is boxed
/// in by the agent-event panel at 23, so this is a second block.
const EP_SLOT_TITLE_BM25: SlotId = SlotId::new(108);

/// Sparse dimension for the episode lexical lane.
///
/// An episode title is a window/document title, the same shape of text the
/// timeline panel measures, so this is sized against the same kind of small
/// repetitive vocabulary rather than against transcript prose: the live
/// timeline BM25 sidecar occupies **51 distinct cells** across 113
/// title-bearing rows, so 4,096 leaves two orders of magnitude of headroom.
/// Recorded as its own constant so the two lanes can diverge without one
/// silently inheriting the other's sizing.
const EP_TITLE_BM25_DIM: u32 = 4_096;

const AE_SLOT_KIND_ONEHOT: SlotId = SlotId::new(23);
const AE_SLOT_OPERATION_ONEHOT: SlotId = SlotId::new(24);
const AE_SLOT_PROVIDER_HASH: SlotId = SlotId::new(25);
const AE_SLOT_REQUEST_MODEL_HASH: SlotId = SlotId::new(26);
const AE_SLOT_RESPONSE_MODEL_HASH: SlotId = SlotId::new(27);
const AE_SLOT_TOOL_HASH: SlotId = SlotId::new(28);
const AE_SLOT_ERROR_ONEHOT: SlotId = SlotId::new(29);
const AE_SLOT_END_STATE_ONEHOT: SlotId = SlotId::new(30);
const AE_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(31);
const AE_SLOT_DOW_CYCLIC: SlotId = SlotId::new(32);
const AE_SLOT_USAGE_TOTAL_LOG1P: SlotId = SlotId::new(33);
// Slot 34 remains reserved inside the agent-event block for the superseded
// magnitude-weighted record vector. New generations must not write it (#1965).
const AE_SLOT_HAS_END_STATE: SlotId = SlotId::new(114);

/// Query-admissible panels whose neighbourhood-derived Loom and Lodestar
/// artifacts are maintained unattended.
///
/// One table owns both schedules because both consumers require the same sealed
/// search-membership generation and graded dense geometry. A finite-only panel
/// must not appear here merely because its storage schema is reconstructable.
pub(crate) const SYN_ASSOCIATION_MAINTENANCE_TARGETS: &[(u32, u16)] = &[
    (SYN_TIMELINE_PANEL_VERSION, TL_SLOT_RECORD_VECTOR.get()),
    (SYN_EPISODE_PANEL_VERSION, EP_SLOT_RECORD_VECTOR.get()),
];

/// One complete, low-level temporal stream universe maintained as a typed
/// observational causal-evidence map.
///
/// Unlike [`SYN_ASSOCIATION_MAINTENANCE_TARGETS`], these targets do not require
/// dense geometry or grounded outcomes. They require an active source-event
/// lane and a metadata field that is present on every active row. The causal
/// map itself enforces complete source coverage and complete `C(n,2)` stream
/// enumeration; a field whose cardinality exceeds the estimator budget is
/// exceeds a measured pair/cell/lag/conditioning/artifact budget is reported as
/// explicitly unmaintainable and is never sampled.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SynCausalMapMaintenanceTarget {
    pub panel_name: &'static str,
    pub panel_version: u32,
    pub group_key: &'static str,
}

/// Built-in temporal populations whose native categorical event kinds are
/// refreshed into durable causal-map generations by the derived-state owner.
///
/// The chosen keys are record-native classifications, not inferred labels:
/// their writers above place them directly beside the exact source-event time.
/// This makes the schedule domain-neutral while still covering human activity,
/// episodes, agent activity, transcripts, actions, reflexes, processes,
/// perception, and MCP tool use.
pub(crate) const SYN_CAUSAL_MAP_MAINTENANCE_TARGETS: &[SynCausalMapMaintenanceTarget] = &[
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_TIMELINE_PANEL_NAME,
        panel_version: SYN_TIMELINE_PANEL_VERSION,
        group_key: "timeline_kind",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_EPISODE_PANEL_NAME,
        panel_version: SYN_EPISODE_PANEL_VERSION,
        group_key: "episode_started_because",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_AGENT_EVENT_PANEL_NAME,
        panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
        group_key: "agent_event_kind",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
        panel_version: SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        group_key: "agent_transcript_status",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_ACTION_PANEL_NAME,
        panel_version: SYN_ACTION_PANEL_VERSION,
        group_key: "action_kind",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_REFLEX_PANEL_NAME,
        panel_version: SYN_REFLEX_PANEL_VERSION,
        group_key: "reflex_status",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_PROCESS_PANEL_NAME,
        panel_version: SYN_PROCESS_PANEL_VERSION,
        group_key: "process_event_kind",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_OBSERVATION_PANEL_NAME,
        panel_version: SYN_OBSERVATION_PANEL_VERSION,
        group_key: "observation_mode",
    },
    SynCausalMapMaintenanceTarget {
        panel_name: SYN_MCP_USAGE_PANEL_NAME,
        panel_version: SYN_MCP_USAGE_PANEL_VERSION,
        group_key: "mcp_usage_tool",
    },
];

const AT_SLOT_ROLE_ONEHOT: SlotId = SlotId::new(35);
const AT_SLOT_STATUS_ONEHOT: SlotId = SlotId::new(36);
const AT_SLOT_SOURCE_ONEHOT: SlotId = SlotId::new(37);
const AT_SLOT_EVENT_KIND_HASH: SlotId = SlotId::new(38);
const AT_SLOT_MODEL_HASH: SlotId = SlotId::new(39);
const AT_SLOT_TEXT_SPARSE: SlotId = SlotId::new(40);
const AT_SLOT_TOOL_HASH: SlotId = SlotId::new(41);
const AT_SLOT_LINE_RANK: SlotId = SlotId::new(42);
const AT_SLOT_INPUT_TOKENS_LOG1P: SlotId = SlotId::new(43);
const AT_SLOT_OUTPUT_TOKENS_LOG1P: SlotId = SlotId::new(44);
const AT_SLOT_CACHE_READ_LOG1P: SlotId = SlotId::new(45);
const AT_SLOT_CACHE_CREATION_LOG1P: SlotId = SlotId::new(46);
// Slot 47 remains reserved inside the transcript block for the superseded
// magnitude-weighted record vector. Its replacement uses slot 110 (#1965).
/// Comparable-scale replacement for the magnitude-weighted slot 47 (#1965).
const AT_SLOT_RECORD_VECTOR_V2: SlotId = SlotId::new(110);
/// Raw term-frequency lexical lane over the same transcript text
/// `AT_SLOT_TEXT_SPARSE` hashes (#1904).
///
/// The agent-transcript panel's original block `35..=47` is boxed in by the
/// action panel at 48, and a block is contiguous by construction, so this panel
/// gets a **second** block. Note that `104..=106` — which #1904 originally
/// proposed — are *not* free: they are unused headroom inside the **timeline**
/// panel's `103..=106`, and taking them would put a transcript vector into a
/// column family another panel exclusively owns, which is the #1776 collision.
const AT_SLOT_TEXT_BM25: SlotId = SlotId::new(107);

/// Sparse dimension for the agent-transcript lexical lane.
///
/// Chosen from the corpus's measured vocabulary, not copied from the timeline
/// panel's 2048. `syn_sparse_text_tf` is a hashing-trick encoder, so the
/// quantity that matters is how many *distinct* terms compete for cells, not
/// how long a document is: document length is what BM25's `b` already corrects
/// for, while a hash collision is uncorrectable and makes a query for one term
/// score documents that only ever contained another.
///
/// Measured manually over the real provider session corpus these rows are
/// derived from (46 files,
/// 2,487 text-bearing documents, 117,972 tokens, **8,702 distinct terms**):
///
/// | dim | load factor | colliding terms | collision rate |
/// |---|---|---|---|
/// | 2,048 | 4.25 | 8,584 | **0.9864** |
/// | 4,096 | 2.12 | 7,657 | 0.8799 |
/// | 8,192 | 1.06 | 5,664 | 0.6509 |
/// | 16,384 | 0.53 | 3,573 | 0.4106 |
/// | 32,768 | 0.27 | 1,971 | 0.2265 |
/// | 65,536 | 0.13 | 1,063 | 0.1222 |
///
/// Copying the timeline's 2048 would have put **98.6%** of terms in a shared
/// cell. The measured rates track `1 - exp(-V/dim)` closely (at 65,536 that
/// predicts 0.124 against 0.1222 observed), so the model can be trusted to
/// project past the sample.
///
/// The sample is 2,487 documents; the live panel holds 14,030 rows, of which a
/// substantial share are non-text control lines. Projecting the text-bearing
/// vocabulary at 15k-20k distinct terms, 65,536 would give ~26% and 262,144
/// gives ~7%. A larger dimension is close to free here — a sparse vector stores
/// only occupied cells, and the persisted postings map is keyed by occupied
/// cells too, so the cost is O(tokens), not O(dim) — which makes headroom the
/// cheap side of this trade.
///
/// Re-run the instrument before changing this; do not adjust it by intuition.
const AT_TEXT_BM25_DIM: u32 = 262_144;

/// Raw term-frequency lexical lane over **every** piece of prose the transcript
/// record carries, not just `content_summary` (#1921).
///
/// `AT_SLOT_TEXT_BM25` (107) is fed by `transcript_text`, which reads only
/// `content_summary` / `source_error` / `parse_error`. Measured against the real
/// source corpus (53 session JSONL files, 33,016 lines) that is **9.2%** of
/// rows — and the vault agreed independently at 326/3,308 = 9.9% (#1921).
///
/// The other 90% is not missing. It is carried verbatim on the same record, in
/// `tool_calls[].tool_name` / `.arguments` / `.result_summary`, and reaches no
/// lens at all:
///
/// | | rows | fraction |
/// |---|---|---|
/// | carry `content_summary` (slot 107 sees these) | 3,024 | 0.0917 |
/// | carry prose only in `tool_calls[]` | 17,445 | 0.5290 |
/// | carry **any** prose (this slot sees these) | 20,469 | **0.6207** |
///
/// In measured tokens that is 140,391 against 2,372,186 — **16.9x** — and in
/// text-bearing documents 3,026 against 20,490 — **6.77x**.
///
/// This is a *new slot*, not a redefinition of 107. A lens is frozen: changing
/// what `syn.agent_transcript.text_bm25.v1` is handed would silently make two
/// different measurements share one identity, and every row already written
/// under it would be reinterpreted. 107 keeps measuring exactly what it always
/// measured, so the two lanes stay comparable and the gain is itself
/// measurable — which is the point.
const AT_SLOT_TEXT_FULL_BM25: SlotId = SlotId::new(109);

/// Sparse dimension for the full-prose agent-transcript lexical lane.
///
/// Sized the same way `AT_TEXT_BM25_DIM` was — against the corpus's measured
/// **distinct-term vocabulary**, because a hashing-trick encoder collides on
/// terms, not on length — but re-measured, because the full projection is a
/// different corpus:
///
/// ```text
/// distinct terms, content-only : 9,778
/// distinct terms, full prose   : 70,894   (7.25x)
///
/// dim         load   predicted collision rate
/// 262,144    0.270      0.2370   <- AT_TEXT_BM25_DIM, reused verbatim
/// 1,048,576  0.068      0.0654
/// 2,097,152  0.034      0.0332
/// ```
///
/// Copying `AT_TEXT_BM25_DIM` would have put **23.7%** of the full-prose
/// vocabulary in a shared cell — the same mistake in kind that copying the
/// timeline's 2,048 would have been for 107, just less extreme.
///
/// 2,097,152 is chosen over 1,048,576 because the cost of a sparse dimension is
/// O(occupied cells) = O(tokens), never O(dim): the vector stores only occupied
/// cells and the persisted postings map is keyed by occupied cells. Headroom is
/// therefore close to free, and at 2x vocabulary growth 2^21 still holds ~6.5%
/// while 2^20 would have decayed to ~12.6%.
///
/// The projection is also proven to fit the lens: `syn::sparse_text_tf` refuses
/// a document over `MAX_TEXT_TOKENS` (4,096), and measured over the real corpus
/// the full projection runs p50=38, p90=325, p99=974, p99.9=1,276, **p100=1,388
/// tokens, with zero documents over the limit**. The per-field caps already in
/// `AgentTranscriptRecord` (2,048 summary + 8,192 args + 8,192 result chars)
/// bound it below the lens ceiling, so this lane needs no truncation of its own
/// and cannot fail closed on length.
///
/// Re-measure manually against the physical corpus before changing this; do not
/// adjust it by intuition.
const AT_TEXT_FULL_BM25_DIM: u32 = 2_097_152;

/// Dimension of `syn.action.target_vector.v2` (#2050/#1690).
///
/// The lens is `syn_record_vector_unit_fields`, which is exactly the **signed
/// feature-hashing** construction of Weinberger et al., *Feature Hashing for
/// Large Scale Multitask Learning* (ICML 2009): each named feature is placed by
/// `hash(path) mod dim` and signed by a second hash of the same digest, which
/// makes the hashed inner product an unbiased estimate of the true one with
/// variance `O(1/dim)`. So `dim` is a noise budget, not a vocabulary size, and
/// the quantity that has to be sized against it is the number of features one
/// target emits — not the number of distinct targets the vault will ever see.
///
/// [`action_target_features`] emits at most `1` kind + `1` shape +
/// [`ACT_TARGET_MAX_FIELDS`] exact-value features + [`ACT_TARGET_MAX_COMPONENTS`]
/// component features = 50. At `dim = 256` the expected number of colliding
/// pairs is `50^2 / (2 * 256) ~= 4.9`, and a collision merges two already-signed
/// unit-scale components rather than losing a feature. Doubling to 512 would
/// halve that at twice the durable cost per row on a TTL-managed CF; halving to
/// 128 would roughly double it. 256 is the smallest power of two that keeps the
/// hashing noise (`O(1/sqrt(dim)) ~= 0.06` on a unit-normalized cosine) an order
/// of magnitude below the separation this lane is calibrated on.
///
/// Frozen: this value is part of the lens id `syn_record_vector_unit_fields:256`
/// and therefore of the panel contract every stored vector was measured under.
/// Changing it is a panel version bump and a full re-measure, never an edit.
const ACT_TARGET_VECTOR_DIM: u32 = 256;
/// Dimension of the bounded pre-action request projection.
///
/// A request emits at most 64 structural nodes plus a small bounded envelope. At
/// 512 dimensions the signed-hash collision expectation is below five pairs
/// and projection noise is about `1/sqrt(512) ~= 0.044`. The action source CF
/// is TTL-managed, so this doubles one short-lived dense lane rather than
/// creating an unbounded permanent corpus. Frozen with the lens id.
const ACT_REQUEST_VECTOR_DIM: u32 = 512;
/// Frozen byte-length scale for the pre-action request lane.
///
/// One authenticated Streamable-HTTP MCP request is capped at 1 MiB by
/// `synapse-mcp::http::session::MAX_MCP_REQUEST_BYTES`. Request snapshots are a
/// projection of that body, so a larger claimed payload is source-contract
/// drift and fails closed. The normalized feature is
/// `ln(1+n) / ln(1+ACT_REQUEST_MAX_PAYLOAD_BYTES)`, keeping it in `[0,1]` as
/// required by `syn_record_vector_unit_fields`.
const ACT_REQUEST_MAX_PAYLOAD_BYTES: u64 = 1024 * 1024;
/// Maximum payload nodes whose graded structure is materialized. Command audit
/// rows retain an exact digest of the complete redacted request, so an overflow
/// remains identity-complete and is explicitly marked in the vector. Legacy
/// rows have no such digest and therefore fail closed instead of truncating.
const ACT_REQUEST_MAX_NODES: usize = 64;
const ACT_REQUEST_EXACT_WEIGHT: f64 = 1.0;
const ACT_REQUEST_ENVELOPE_WEIGHT: f64 = 0.75;
const ACT_REQUEST_SHAPE_WEIGHT: f64 = 0.5;
/// Frozen finite support for the request byte-length lens. Eight domain
/// thresholds keep the worst-case binary-outcome contingency table to sixteen
/// occupied cells instead of one cell per exact byte count.
const ACT_REQUEST_SIZE_CLASS_LEVELS: u32 = 8;
/// Frozen finite support for the structural request lens. Thirty signed-hash
/// buckets cap its worst-case binary-outcome table at sixty occupied cells;
/// the exact structural summary is defined below and never includes values.
const ACT_REQUEST_SHAPE_CLASS_LEVELS: u32 = 30;
/// Weight of a whole-field exact-value feature. The identity carrier: two rows
/// naming the same value for the same field share this feature exactly.
const ACT_TARGET_EXACT_WEIGHT: f64 = 1.0;
/// Weight of the target-kind feature (`window` / `cdp` / `scalar`).
const ACT_TARGET_KIND_WEIGHT: f64 = 0.5;
/// Weight of the field-set shape feature, which separates a window target from
/// a CDP target even when neither shares a field value.
const ACT_TARGET_SHAPE_WEIGHT: f64 = 0.5;
/// Total L2 mass a single field's path/URL components may contribute.
///
/// Split as `mass / sqrt(k)` over `k` components, so the components of one field
/// carry exactly `mass` of squared magnitude regardless of how many there are —
/// a long path cannot outweigh a short one, which is the failure
/// `syn_record_vector_unit_fields` exists to refuse.
///
/// Component granularity is deliberate and is the one place this encoding
/// departs from the record-linkage literature. Bloom-filter PPRL (Schnell,
/// Bachteler & Reiher, *Privacy-preserving record linkage using Bloom filters*,
/// BMC MIDM 2009) hashes character q-grams so that near-miss spellings still
/// score. Character trigrams over this corpus would be actively harmful: a
/// window target is an opaque `HWND` integer and a CDP target id is a random
/// 32-hex string, whose trigrams overlap by chance, and the known-bad corpus is
/// built from unique nonexistent executables that all share `C:\`, `\Users\`
/// and `.exe`. That would put a similarity floor under every bad case, which is
/// exactly the inseparability that made `2020001` uncalibratable. Splitting on
/// path/URL separators instead keeps the field-level graded similarity the
/// literature is after at the granularity that actually carries identity here.
const ACT_TARGET_COMPONENT_MASS: f64 = 0.5;
/// Cap on whole-field exact-value features per target. A target object with
/// more fields than this is refused rather than silently truncated: dropping
/// fields would make two different targets measure identically.
const ACT_TARGET_MAX_FIELDS: usize = 16;
/// Cap on component features per target, over all fields.
const ACT_TARGET_MAX_COMPONENTS: usize = 32;
/// Characters that separate the identity components of a path, URL or
/// qualified name. Frozen: part of the lens's measured meaning.
const ACT_TARGET_COMPONENT_SEPARATORS: &[char] = &[
    '/', '\\', '.', ':', '?', '&', '=', '#', ' ', '\t', ',', ';', '|', '"', '\'',
];

// Slot ids are GLOBAL, not panel-local (#1776).
//
// Calyx persists every measured vector in a physical `cf/slot_<id>` column
// family keyed only by `CxId` (`calyx-aster::cf::slot_key`), and search/index
// rebuild scans an entire global `ColumnFamily::slot(slot)`. Two panels sharing
// a slot id therefore share one physical column family holding vectors of
// different shapes and different meanings, and every association, assay, kernel
// and search result computed over it compares incomparable things.
//
// This used to be treated as panel-local, which put seven panels into
// `slot_01`..`slot_08` on the production vault. Each panel now owns an
// exclusive contiguous block declared in `PANEL_SLOT_BLOCKS` below; the blocks
// are checked disjoint at compile time, and `validate_panel_slot_allocation`
// fails closed at write time if a constellation ever declares a slot outside
// its own panel's block. The literal ids stay literal on purpose: they are
// durable identities, so they must be greppable and must never be silently
// recomputed.
const ACT_SLOT_KIND_ONEHOT: SlotId = SlotId::new(48);
const ACT_SLOT_TARGET_HASH: SlotId = SlotId::new(49);
const ACT_SLOT_RECORD_VECTOR: SlotId = SlotId::new(50);
// Slot 50 is reserved for the magnitude-weighted vector retired by #1965.
const ACT_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(51);
const ACT_SLOT_DOW_CYCLIC: SlotId = SlotId::new(52);
/// The dense, similarity-bearing target-identity lane (#2050).
///
/// The action panel's first block `48..=52` is contiguous and full, and 53
/// belongs to the reflex panel, so this lens takes an id in the panel's
/// **second** block — the same move #1900/#1921/#1964 made for the timeline,
/// agent-transcript and episode panels.
const ACT_SLOT_TARGET_VECTOR: SlotId = SlotId::new(117);
/// Dense point-in-time request-cause lane. Slot 118 is in the action panel's
/// reserved second block and has never carried another meaning.
const ACT_SLOT_REQUEST_VECTOR: SlotId = SlotId::new(118);
/// Bounded log-scale payload-size category. Slot 119 has never carried another
/// meaning and is independently assayable from exact request identity.
const ACT_SLOT_REQUEST_SIZE_CLASS: SlotId = SlotId::new(119);
/// Bounded structural-shape category. Slot 120 has never carried another
/// meaning and is independently assayable from exact request identity.
const ACT_SLOT_REQUEST_SHAPE_CLASS: SlotId = SlotId::new(120);

const RF_SLOT_REFLEX_HASH: SlotId = SlotId::new(53);
const RF_SLOT_OUTCOME_ONEHOT: SlotId = SlotId::new(54);
const RF_SLOT_LATENCY_LOG1P: SlotId = SlotId::new(55);
const RF_SLOT_STEP_COUNT_LOG1P: SlotId = SlotId::new(56);
const RF_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(57);
const RF_SLOT_DOW_CYCLIC: SlotId = SlotId::new(58);
// Slot 59 is reserved for the magnitude-weighted vector retired by #1965.

const PR_SLOT_PROCESS_HASH: SlotId = SlotId::new(60);
const PR_SLOT_EVENT_ONEHOT: SlotId = SlotId::new(61);
const PR_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(62);
const PR_SLOT_DOW_CYCLIC: SlotId = SlotId::new(63);
const PR_SLOT_UPTIME_LOG1P: SlotId = SlotId::new(64);
const PR_SLOT_RECENCY_RANK: SlotId = SlotId::new(65);
// Slot 66 is reserved for the magnitude-weighted vector retired by #1965.

const OB_SLOT_APP_HASH: SlotId = SlotId::new(67);
const OB_SLOT_ROLE_HISTOGRAM: SlotId = SlotId::new(68);
const OB_SLOT_ENTITY_MULTI_HOT: SlotId = SlotId::new(69);
// Slot 70 is reserved for the uncalibrated magnitude-weighted HUD vector
// retired by #1965.
const OB_SLOT_FLAGS_MULTI_HOT: SlotId = SlotId::new(71);
const OB_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(72);
const OB_SLOT_DOW_CYCLIC: SlotId = SlotId::new(73);
// Slot 74 is reserved for the magnitude-weighted vector retired by #1965.

const OUT_SLOT_SOURCE_CF_ONEHOT: SlotId = SlotId::new(75);
const OUT_SLOT_EVENT_ONEHOT: SlotId = SlotId::new(76);
const OUT_SLOT_STATUS_ONEHOT: SlotId = SlotId::new(77);
const OUT_SLOT_TARGET_HASH: SlotId = SlotId::new(78);
const OUT_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(79);
const OUT_SLOT_DOW_CYCLIC: SlotId = SlotId::new(80);
// Slot 81 is reserved for the magnitude-weighted vector retired by #1965.
const OUT_SLOT_RECORD_VECTOR_V2: SlotId = SlotId::new(116);

const MU_SLOT_TOOL_ONEHOT: SlotId = SlotId::new(82);
const MU_SLOT_OPERATION_ONEHOT: SlotId = SlotId::new(83);
const MU_SLOT_ROUTE_HASH: SlotId = SlotId::new(84);
const MU_SLOT_PARAM_SHAPE_HASH: SlotId = SlotId::new(85);
const MU_SLOT_STATUS_ONEHOT: SlotId = SlotId::new(86);
const MU_SLOT_ERROR_ONEHOT: SlotId = SlotId::new(87);
const MU_SLOT_PROFILE_HASH: SlotId = SlotId::new(88);
const MU_SLOT_SURFACE_HASH: SlotId = SlotId::new(89);
const MU_SLOT_SESSION_SEQUENCE_RANK: SlotId = SlotId::new(90);
const MU_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(91);
const MU_SLOT_DOW_CYCLIC: SlotId = SlotId::new(92);
// Slot 93 is reserved for the uncalibrated unit-vector lens retired by #1965.
const MU_SLOT_RECORD_VECTOR_V2: SlotId = SlotId::new(115);

const RS_SLOT_KIND_ONEHOT: SlotId = SlotId::new(94);
const RS_SLOT_SUBJECT_HASH: SlotId = SlotId::new(95);

// Graph-position panels. App and process are two DISTINCT panels measuring
// different graphs with the same lens family, so they need distinct blocks:
// sharing ids would put an app-transition signature and a process-tree
// signature in one physical CF. Low slot is the frozen dense structural
// signature; high slot is the hashed neighbour-label histogram.
const GP_APP_SLOT_SIGNATURE: SlotId = SlotId::new(96);
const GP_APP_SLOT_NEIGHBORS: SlotId = SlotId::new(97);
const GP_PROCESS_SLOT_SIGNATURE: SlotId = SlotId::new(98);
const GP_PROCESS_SLOT_NEIGHBORS: SlotId = SlotId::new(99);

// Path-hierarchy panel: frozen dense path signature, ancestor-set multi-hot,
// leaf path hash.
const PH_SLOT_SIGNATURE: SlotId = SlotId::new(100);
const PH_SLOT_ANCESTORS: SlotId = SlotId::new(101);
const PH_SLOT_PATH_HASH: SlotId = SlotId::new(102);

/// One exclusive, contiguous block of global slot ids owned by one panel.
///
/// A panel may own **more than one** block. It has to: a block is contiguous, so
/// once a panel's neighbours are allocated its original block cannot grow, and a
/// panel that gains a lens later (#1900 added a BM25 lexical lane to the timeline
/// panel, whose `1..=7` is boxed in by the episode panel at 8) needs a second
/// range rather than an id belonging to someone else.
struct PanelSlotBlock {
    panel: &'static str,
    first: u16,
    last: u16,
}

/// The global slot allocation: every Synapse panel and the physical
/// `cf/slot_<id>` range it exclusively owns.
///
/// Adding a panel means adding a block here. The compile-time assertion below
/// rejects any overlap, so the #1776 collision cannot be reintroduced by
/// accident — a duplicate id fails the build, not the vault.
const PANEL_SLOT_BLOCKS: &[PanelSlotBlock] = &[
    PanelSlotBlock {
        panel: SYN_TIMELINE_PANEL_NAME,
        first: 1,
        last: 7,
    },
    // The timeline panel's second block (#1900). Holds `TL_SLOT_TITLE_BM25`
    // (103); 104..=106 are unallocated headroom in this panel's own range, so
    // the next timeline lens needs no third block. Unused ids inside a block
    // have no physical `cf/slot_<id>` and cost nothing.
    PanelSlotBlock {
        panel: SYN_TIMELINE_PANEL_NAME,
        first: 103,
        last: 106,
    },
    PanelSlotBlock {
        panel: SYN_EPISODE_PANEL_NAME,
        first: 8,
        last: 22,
    },
    // The episode panel's second block (#1904). Holds `EP_SLOT_TITLE_BM25`.
    PanelSlotBlock {
        panel: SYN_EPISODE_PANEL_NAME,
        first: 108,
        last: 108,
    },
    // The episode panel's third block (#1964). Holds
    // `EP_SLOT_RECORD_VECTOR` (113). 108 could not be extended because 109
    // belongs to the agent-transcript panel and a block is contiguous by
    // construction.
    PanelSlotBlock {
        panel: SYN_EPISODE_PANEL_NAME,
        first: 113,
        last: 113,
    },
    PanelSlotBlock {
        panel: SYN_AGENT_EVENT_PANEL_NAME,
        first: 23,
        last: 34,
    },
    PanelSlotBlock {
        panel: SYN_AGENT_EVENT_PANEL_NAME,
        first: 114,
        last: 114,
    },
    PanelSlotBlock {
        panel: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
        first: 35,
        last: 47,
    },
    // The agent-transcript panel's second block (#1904). Holds
    // `AT_SLOT_TEXT_BM25`.
    PanelSlotBlock {
        panel: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
        first: 107,
        last: 107,
    },
    // The agent-transcript panel's third block (#1921). Holds
    // `AT_SLOT_TEXT_FULL_BM25` (109); 110..=112 are unallocated headroom inside
    // this panel's own range, so the next transcript lens needs no fourth
    // block. 108 could not be folded in because it belongs to the episode
    // panel, and a block is contiguous by construction.
    PanelSlotBlock {
        panel: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
        first: 109,
        last: 112,
    },
    PanelSlotBlock {
        panel: SYN_ACTION_PANEL_NAME,
        first: 48,
        last: 52,
    },
    // The action panel's second block (#2050/#1690). Holds the dense target,
    // exact request, bounded request-size and bounded request-shape lanes
    // (117..=120). `48..=52` could not be
    // extended because 53 belongs to the reflex panel and a block is contiguous
    // by construction.
    PanelSlotBlock {
        panel: SYN_ACTION_PANEL_NAME,
        first: 117,
        last: 120,
    },
    PanelSlotBlock {
        panel: SYN_REFLEX_PANEL_NAME,
        first: 53,
        last: 59,
    },
    PanelSlotBlock {
        panel: SYN_PROCESS_PANEL_NAME,
        first: 60,
        last: 66,
    },
    PanelSlotBlock {
        panel: SYN_OBSERVATION_PANEL_NAME,
        first: 67,
        last: 74,
    },
    PanelSlotBlock {
        panel: SYN_OUTCOME_PANEL_NAME,
        first: 75,
        last: 81,
    },
    PanelSlotBlock {
        panel: SYN_OUTCOME_PANEL_NAME,
        first: 116,
        last: 116,
    },
    PanelSlotBlock {
        panel: SYN_MCP_USAGE_PANEL_NAME,
        first: 82,
        last: 93,
    },
    PanelSlotBlock {
        panel: SYN_MCP_USAGE_PANEL_NAME,
        first: 115,
        last: 115,
    },
    PanelSlotBlock {
        panel: SYN_RECURRENCE_SUBJECT_PANEL_NAME,
        first: 94,
        last: 95,
    },
    PanelSlotBlock {
        panel: SYN_GRAPHPOS_APP_PANEL_NAME,
        first: 96,
        last: 97,
    },
    PanelSlotBlock {
        panel: SYN_GRAPHPOS_PROCESS_PANEL_NAME,
        first: 98,
        last: 99,
    },
    PanelSlotBlock {
        panel: SYN_PATH_HIERARCHY_PANEL_NAME,
        first: 100,
        last: 102,
    },
];

/// Compile-time proof that no two panels claim the same physical slot CF, that
/// every block is well formed, and that every block stays inside the durable
/// slot-id budget.
const fn panel_slot_blocks_are_disjoint() -> bool {
    let mut outer = 0;
    while outer < PANEL_SLOT_BLOCKS.len() {
        let block = &PANEL_SLOT_BLOCKS[outer];
        if block.first == 0 || block.first > block.last || block.last > CALYX_DURABLE_SLOT_ID_MAX {
            return false;
        }
        let mut inner = outer + 1;
        while inner < PANEL_SLOT_BLOCKS.len() {
            let other = &PANEL_SLOT_BLOCKS[inner];
            if block.first <= other.last && other.first <= block.last {
                return false;
            }
            inner += 1;
        }
        outer += 1;
    }
    true
}

const _: () = assert!(
    panel_slot_blocks_are_disjoint(),
    "PANEL_SLOT_BLOCKS must be well formed, within CALYX_DURABLE_SLOT_ID_MAX, and mutually \
     disjoint: Calyx stores every slot in a global cf/slot_<id> column family, so two panels \
     sharing an id share one physical column family (issue #1776)"
);

const GP_NEIGHBOR_HISTOGRAM_DIM: u32 = 2048;
const PH_ANCESTOR_DIM: u32 = 2048;
const PH_PATH_HASH_DIM: u32 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecurrenceSubjectKind {
    Action,
    AppUsage,
    Routine,
}

impl RecurrenceSubjectKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::AppUsage => "app_usage",
            Self::Routine => "routine",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstellationPutReport {
    pub panel_name: &'static str,
    pub panel_version: u32,
    pub source_cf: &'static str,
    pub source_key_hex: String,
    pub raw_sha256: String,
    pub cx_id: String,
    pub disposition: SynapseCalyxPutDisposition,
    pub latest_seq: u64,
    pub slot_count: u64,
    pub scalar_count: u64,
    pub duration_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporalMetadataBackfillRowReport {
    pub source_key: Vec<u8>,
    pub inserted_rows: u64,
    pub backfilled_rows: u64,
    pub already_current_rows: u64,
    pub temporal_ineligible_rows: u64,
    pub outcome_anchored_rows: u64,
    pub outcome_absent_rows: u64,
    pub outcome_unadjudicable_rows: u64,
    pub anchors_carried_forward: u64,
    pub rows_anchor_carried: u64,
    pub anchor_carry_source_generations_read: u64,
    pub latest_seq: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporalMetadataBackfillRowFailure {
    pub source_key: Vec<u8>,
    pub error: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporalMetadataBackfillReport {
    pub source_cf: String,
    pub examined_rows: u64,
    pub inserted_rows: u64,
    pub backfilled_rows: u64,
    pub already_current_rows: u64,
    /// Rows whose authoritative source declares no event time and therefore no
    /// active temporal lane. They remain valid panel records but are not
    /// candidates for temporal metadata migration.
    pub temporal_ineligible_rows: u64,
    /// Rows this page grounded with a declared tool-call outcome anchor
    /// (#1926). Always 0 for CFs that carry no adjudicated outcome. A sweep
    /// that re-measures rows but grounds none of them is a visible fact here
    /// rather than something to infer from a later coverage report.
    pub outcome_anchored_rows: u64,
    /// Rows this page examined that carried no outcome to write at all.
    pub outcome_absent_rows: u64,
    /// Rows this page examined that were observed tool results the declared
    /// adjudication declined to decide (#1926). Non-zero here is a real signal:
    /// the corpus contains a result shape this code does not yet cover.
    pub outcome_unadjudicable_rows: u64,
    /// Anchors this page carried forward from a superseded generation (#1980).
    ///
    /// Deliberately separate from `outcome_anchored_rows`: that counts outcomes
    /// DERIVED from the source row on this pass, this counts outcomes the corpus
    /// had already observed and would otherwise have lost to the panel bump.
    /// Conflating them would hide a carry that silently stopped working behind a
    /// derivation that still runs.
    pub anchors_carried_forward: u64,
    /// Rows on this page that received at least one carried anchor.
    pub rows_anchor_carried: u64,
    /// Superseded generations actually probed across this page. Zero while a
    /// panel has never been bumped; a page with rows but zero generations read
    /// on a panel that HAS been bumped is a broken carry, not a quiet one.
    pub anchor_carry_source_generations_read: u64,
    /// Candidate rows this page walked before TTL filtering.
    ///
    /// On a TTL-managed source CF `examined_rows` can be 0 while the page did
    /// real work, because every candidate it walked had expired. Without this
    /// field those two states — "the page was all expired" and "the CF has no
    /// more rows" — produce the identical report, and an operator reading
    /// `examined=0 inserted=0` concludes the backfill is finished when it has
    /// not started (#1965).
    pub candidate_rows_examined: u64,
    /// Candidate rows this page skipped because their TTL had passed.
    pub expired_rows_skipped: u64,
    pub latest_seq: u64,
    pub resume_after_physical: Option<Vec<u8>>,
    pub more: bool,
    /// Ordered per-source dispositions from the same physical commit/readback.
    /// Exact-key maintenance batches use these to preserve pointwise quarantine
    /// semantics without returning to one durable transaction per identity.
    pub row_reports: Vec<TemporalMetadataBackfillRowReport>,
    /// Pointwise preflight failures from an exact-key maintenance batch. The
    /// public exact-batch wrapper joins these back to request order by physical
    /// source key. Paged sweeps and single-key calls fail the whole operation
    /// and therefore always leave this empty.
    pub row_failures: Vec<TemporalMetadataBackfillRowFailure>,
}

// ---------------------------------------------------------------------------
// Exact-match-by-hash lanes (#1899)
// ---------------------------------------------------------------------------

/// One whole-value hash lane and the authoritative source field it measures.
///
/// A `syn_hash` slot content-addresses an entire field value into one sparse
/// cell, which makes exact-match recall possible and makes a bucket collision
/// indistinguishable from a true match *inside the index*. The only thing that
/// can tell them apart is the source field itself, so a lane is only usable for
/// exact matching if the field it measured can be named and re-read. This table
/// is that naming, and a slot absent from it is refused rather than confirmed
/// against a guess.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SynExactMatchLane {
    pub panel_version: u32,
    pub slot: u16,
    /// Authoritative source column family the value is re-read from.
    pub source_cf: &'static str,
    /// Dot-separated JSON path into the source row, matching the measurement
    /// site's field exactly.
    pub field_path: &'static str,
}

/// Every declared exact-match lane, keyed by (`panel_version`, slot).
///
/// Mirrors the measurement sites: each entry names the same field the
/// corresponding `syn_hash` lens is handed at ingest. A field path that drifts
/// from its measurement site makes confirmation reject true matches, which is a
/// loud failure (`candidates_confirmed=0`) rather than a silent one.
pub const SYN_EXACT_MATCH_LANES: &[SynExactMatchLane] = &[
    SynExactMatchLane {
        panel_version: SYN_TIMELINE_PANEL_VERSION,
        slot: 2,
        source_cf: cf::CF_TIMELINE,
        field_path: "app",
    },
    SynExactMatchLane {
        panel_version: SYN_EPISODE_PANEL_VERSION,
        slot: 8,
        source_cf: cf::CF_EPISODES,
        field_path: "app",
    },
    SynExactMatchLane {
        panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
        slot: 25,
        source_cf: cf::CF_AGENT_EVENTS,
        field_path: "provider",
    },
    SynExactMatchLane {
        panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
        slot: 28,
        source_cf: cf::CF_AGENT_EVENTS,
        field_path: "tool_name",
    },
];

/// One exact-match candidate's confirmation against its authoritative source
/// field (#1899).
///
/// A hash-lane probe returns bucket candidates, and a collision is
/// byte-identical to a true match inside the index. This is the readback of the
/// only check that can separate them: re-read the source field and compare.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactMatchConfirmation {
    pub cx_id: String,
    /// True only when the source field was read and equals the queried value.
    pub confirmed: bool,
    pub source_cf: Option<String>,
    pub source_key_hex: Option<String>,
    /// The value actually found on the source row, when one was readable.
    pub observed_value: Option<String>,
    /// Machine-readable verdict: `confirmed`, `bucket_collision`,
    /// `source_field_absent`, `base_row_absent`, `source_row_absent`,
    /// `panel_mismatch`, or `source_cf_mismatch`.
    pub verdict: &'static str,
}

/// Verdict for a candidate whose source field equals the queried value.
pub const EXACT_MATCH_CONFIRMED: &str = "confirmed";
/// Verdict for a candidate that shares the bucket but not the value.
pub const EXACT_MATCH_BUCKET_COLLISION: &str = "bucket_collision";

/// Looks up the declared exact-match lane for one panel slot.
#[must_use]
pub fn syn_exact_match_lane(panel_version: u32, slot: u16) -> Option<&'static SynExactMatchLane> {
    SYN_EXACT_MATCH_LANES
        .iter()
        .find(|lane| lane.panel_version == panel_version && lane.slot == slot)
}

/// Reads one dot-separated field out of an authoritative source row as the exact
/// string the lens measured.
///
/// `Ok(None)` means the field is absent or null on this row, which is a real
/// answer: the lens measured `Absent` for it, so the row cannot be a true match.
/// A non-string field is an error rather than a stringified guess, because the
/// hash was taken over the bytes the measurement site passed and a coerced
/// number would compare against different bytes.
///
/// # Errors
///
/// Returns a read-scoped storage error when the row is not valid JSON, when a
/// path segment traverses a non-object, or when the addressed field is neither
/// null nor a string.
pub fn syn_exact_field_value(
    source_cf: &str,
    field_path: &str,
    raw: &[u8],
) -> StorageResult<Option<String>> {
    let row: Value = serde_json::from_slice(raw).map_err(|error| StorageError::ReadFailed {
        cf_name: source_cf.to_owned(),
        detail: format!("decode authoritative row for exact-match confirmation: {error}"),
    })?;
    let mut cursor = &row;
    for segment in field_path.split('.') {
        match cursor {
            Value::Object(map) => match map.get(segment) {
                Some(next) => cursor = next,
                None => return Ok(None),
            },
            Value::Null => return Ok(None),
            other => {
                return Err(StorageError::ReadFailed {
                    cf_name: source_cf.to_owned(),
                    detail: format!(
                        "exact-match field path {field_path} traverses a non-object at segment {segment}: found {}",
                        value_kind(other)
                    ),
                });
            }
        }
    }
    match cursor {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        other => Err(StorageError::ReadFailed {
            cf_name: source_cf.to_owned(),
            detail: format!(
                "exact-match field path {field_path} addresses a {}, but the hash lane measured string bytes",
                value_kind(other)
            ),
        }),
    }
}

const fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[must_use]
pub fn temporal_migration_metadata(
    constellation: &Constellation,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let identity = [
        META_PANEL_NAME,
        META_SOURCE_CF,
        META_SOURCE_KEY_HEX,
        META_RAW_SHA256,
        META_RAW_LEN_BYTES,
    ]
    .into_iter()
    .filter_map(|key| {
        constellation
            .metadata
            .get(key)
            .map(|value| (key.to_owned(), value.clone()))
    })
    .collect();
    let temporal = [
        METADATA_TEMPORAL_LANE_STATE,
        METADATA_TEMPORAL_INACTIVE_REASON,
        METADATA_SOURCE_EVENT_TIME_SECS,
        METADATA_SOURCE_EVENT_TIME_RAW,
        METADATA_SOURCE_SEQUENCE,
    ]
    .into_iter()
    .filter_map(|key| {
        constellation
            .metadata
            .get(key)
            .map(|value| (key.to_owned(), value.clone()))
    })
    .collect();
    (identity, temporal)
}

/// Classifies the authoritative temporal metadata contract for migration.
///
/// An explicitly inactive lane is valid only when it carries a non-empty
/// reason and no event-time coordinates. Every other non-active shape is a
/// contract error rather than an ineligible-row shortcut.
///
/// # Errors
///
/// Returns a structured error when the temporal lane state is missing,
/// contradictory, or carries invalid event-time coordinates.
pub fn temporal_migration_eligible(temporal: &BTreeMap<String, String>) -> StorageResult<bool> {
    match temporal
        .get(METADATA_TEMPORAL_LANE_STATE)
        .map(String::as_str)
    {
        Some(TEMPORAL_LANE_ACTIVE) => Ok(true),
        Some(TEMPORAL_LANE_INACTIVE) => {
            let reason_present = temporal
                .get(METADATA_TEMPORAL_INACTIVE_REASON)
                .is_some_and(|value| !value.is_empty());
            let coordinates_absent = [
                METADATA_SOURCE_EVENT_TIME_SECS,
                METADATA_SOURCE_EVENT_TIME_RAW,
                METADATA_SOURCE_SEQUENCE,
            ]
            .into_iter()
            .all(|key| !temporal.contains_key(key));
            if reason_present && coordinates_absent {
                Ok(false)
            } else {
                Err(measurement_error(
                    "inactive temporal lane requires a non-empty reason and no event-time coordinates",
                    format!("temporal_metadata={temporal:?}"),
                ))
            }
        }
        state => Err(measurement_error(
            "authoritative temporal metadata has no recognized lane state",
            format!("lane_state={state:?} temporal_metadata={temporal:?}"),
        )),
    }
}

impl ConstellationPutReport {
    #[must_use]
    pub const fn inserted(&self) -> bool {
        self.disposition.inserted()
    }

    #[must_use]
    pub const fn deduped(&self) -> bool {
        self.disposition.deduped()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NativeConstellationContext {
    pub vault_id: VaultId,
    pub cx_id: CxId,
    pub created_at_ms: u64,
    pub next_ledger_seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalyxConstellationInputMode {
    SourceValue,
    FramedSourceRow,
    McpUsageFramedSourceRow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalyxAnchorPanel {
    pub panel_name: &'static str,
    pub panel_version: u32,
    pub input_mode: CalyxConstellationInputMode,
}

/// Returns the native Calyx panel used to derive the source row's `CxId`.
///
/// # Errors
///
/// Returns a write-scoped storage error when the source column family has no
/// native constellation contract and therefore cannot receive grounded anchors.
pub fn anchor_panel_for_source_cf(cf_name: &str) -> StorageResult<CalyxAnchorPanel> {
    let panel = match cf_name {
        cf::CF_TIMELINE => CalyxAnchorPanel {
            panel_name: SYN_TIMELINE_PANEL_NAME,
            panel_version: SYN_TIMELINE_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_EPISODES => CalyxAnchorPanel {
            panel_name: SYN_EPISODE_PANEL_NAME,
            panel_version: SYN_EPISODE_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_AGENT_EVENTS => CalyxAnchorPanel {
            panel_name: SYN_AGENT_EVENT_PANEL_NAME,
            panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_AGENT_TRANSCRIPTS => CalyxAnchorPanel {
            panel_name: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            panel_version: SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_ACTION_LOG => CalyxAnchorPanel {
            panel_name: SYN_ACTION_PANEL_NAME,
            panel_version: SYN_ACTION_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_REFLEX_AUDIT => CalyxAnchorPanel {
            panel_name: SYN_REFLEX_PANEL_NAME,
            panel_version: SYN_REFLEX_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_PROCESS_HISTORY => CalyxAnchorPanel {
            panel_name: SYN_PROCESS_PANEL_NAME,
            panel_version: SYN_PROCESS_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        cf::CF_OBSERVATIONS => CalyxAnchorPanel {
            panel_name: SYN_OBSERVATION_PANEL_NAME,
            panel_version: SYN_OBSERVATION_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::SourceValue,
        },
        SYN_MCP_USAGE_BACKFILL_SOURCE => CalyxAnchorPanel {
            panel_name: SYN_MCP_USAGE_PANEL_NAME,
            panel_version: SYN_MCP_USAGE_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::McpUsageFramedSourceRow,
        },
        SYN_OUTCOME_BACKFILL_SOURCE | cf::CF_KV | cf::CF_ROUTINE_STATE => CalyxAnchorPanel {
            panel_name: SYN_OUTCOME_PANEL_NAME,
            panel_version: SYN_OUTCOME_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::FramedSourceRow,
        },
        other => {
            return Err(StorageError::WriteFailed {
                cf_name: other.to_owned(),
                detail: format!(
                    "source column family {other:?} has no Calyx constellation panel for grounded anchors"
                ),
            });
        }
    };
    Ok(panel)
}

/// Returns the native Calyx panel for one source row. `CF_KV` hosts several
/// row families, so row-key namespaces select the correct panel where needed.
///
/// # Errors
///
/// Returns a write-scoped storage error when the source row has no native
/// constellation contract.
pub fn anchor_panel_for_source_row(
    cf_name: &'static str,
    source_key: &[u8],
) -> StorageResult<CalyxAnchorPanel> {
    if cf_name == cf::CF_KV && source_key.starts_with(SYN_MCP_USAGE_KEY_PREFIX) {
        return Ok(CalyxAnchorPanel {
            panel_name: SYN_MCP_USAGE_PANEL_NAME,
            panel_version: SYN_MCP_USAGE_PANEL_VERSION,
            input_mode: CalyxConstellationInputMode::McpUsageFramedSourceRow,
        });
    }
    anchor_panel_for_source_cf(cf_name)
}

#[must_use]
pub fn source_constellation_input_bytes(
    mode: CalyxConstellationInputMode,
    source_cf: &str,
    source_key: &[u8],
    source_value: &[u8],
) -> Vec<u8> {
    match mode {
        CalyxConstellationInputMode::SourceValue => source_value.to_vec(),
        CalyxConstellationInputMode::FramedSourceRow => {
            outcome_constellation_input_bytes(source_cf, source_key, source_value)
        }
        CalyxConstellationInputMode::McpUsageFramedSourceRow => {
            mcp_usage_constellation_input_bytes(source_cf, source_key, source_value)
        }
    }
}

#[must_use]
pub fn outcome_constellation_input_bytes(
    source_cf: &str,
    source_key: &[u8],
    source_value: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        "synapse-outcome-input-v1".len()
            + source_cf.len()
            + source_key.len()
            + source_value.len()
            + 24,
    );
    append_framed(&mut out, b"synapse-outcome-input-v1");
    append_framed(&mut out, source_cf.as_bytes());
    append_framed(&mut out, source_key);
    append_framed(&mut out, source_value);
    out
}

#[must_use]
pub fn mcp_usage_constellation_input_bytes(
    source_cf: &str,
    source_key: &[u8],
    source_value: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        "synapse-mcp-usage-input-v1".len()
            + source_cf.len()
            + source_key.len()
            + source_value.len()
            + 24,
    );
    append_framed(&mut out, b"synapse-mcp-usage-input-v1");
    append_framed(&mut out, source_cf.as_bytes());
    append_framed(&mut out, source_key);
    append_framed(&mut out, source_value);
    out
}

#[must_use]
pub fn recurrence_subject_input_bytes(kind: RecurrenceSubjectKind, subject_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        "synapse-recurrence-subject-v1".len() + kind.as_str().len() + subject_id.len() + 24,
    );
    append_framed(&mut out, b"synapse-recurrence-subject-v1");
    append_framed(&mut out, kind.as_str().as_bytes());
    append_framed(&mut out, subject_id.as_bytes());
    out
}

/// Builds the stable Base row that owns a native recurrence series.
///
/// The subject identity deliberately excludes occurrence time: every event
/// for one app/routine must append below the same `CxId`.
///
/// # Errors
///
/// Returns a measurement error when the subject identity or generated Calyx
/// constellation violates its schema contract.
pub fn build_recurrence_subject_constellation(
    context: NativeConstellationContext,
    kind: RecurrenceSubjectKind,
    subject_id: &str,
    input_bytes: &[u8],
) -> StorageResult<Constellation> {
    let subject_id = non_empty(subject_id).ok_or_else(|| {
        measurement_error(
            "empty recurrence subject",
            format!("kind={}", kind.as_str()),
        )
    })?;
    let mut slots = BTreeMap::new();
    slots.insert(
        RS_SLOT_KIND_ONEHOT,
        measure_text(
            SYN_RECURRENCE_SUBJECT_PANEL_NAME,
            AlgorithmicLens::syn_one_hot(
                "syn.recurrence_subject.kind_onehot.v1",
                Modality::Structured,
                4,
            ),
            kind.as_str(),
        )?,
    );
    slots.insert(
        RS_SLOT_SUBJECT_HASH,
        measure_text(
            SYN_RECURRENCE_SUBJECT_PANEL_NAME,
            AlgorithmicLens::syn_hash(
                "syn.recurrence_subject.identity_hash.v1",
                Modality::Structured,
                4096,
            ),
            subject_id,
        )?,
    );
    let subject_hash = sha256_hex(input_bytes);
    let mut metadata = common_metadata(
        SYN_RECURRENCE_SUBJECT_PANEL_NAME,
        "CALYX_RECURRENCE_SUBJECT",
        subject_hash.as_bytes(),
        input_bytes,
    );
    metadata.insert(
        "recurrence_subject_kind".to_owned(),
        kind.as_str().to_owned(),
    );
    metadata.insert(
        "recurrence_subject_id".to_owned(),
        truncate_metadata(subject_id),
    );
    if kind == RecurrenceSubjectKind::Action {
        metadata.insert("oracle.domain".to_owned(), "synapse.action".to_owned());
        metadata.insert("oracle.action".to_owned(), truncate_metadata(subject_id));
    }
    constellation(
        context,
        SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
        format!(
            "synapse://recurrence-subject/{}/{subject_hash}",
            kind.as_str()
        ),
        input_bytes,
        slots,
        BTreeMap::new(),
        metadata,
    )
}

/// Which graph-position panel a node signature belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphPositionKind {
    /// App-transition graph (timeline focus changes).
    App,
    /// Process parent/child + spawn tree.
    Process,
}

impl GraphPositionKind {
    #[must_use]
    pub const fn panel_name(self) -> &'static str {
        match self {
            Self::App => SYN_GRAPHPOS_APP_PANEL_NAME,
            Self::Process => SYN_GRAPHPOS_PROCESS_PANEL_NAME,
        }
    }

    #[must_use]
    pub const fn base_panel_version(self) -> u32 {
        match self {
            Self::App => SYN_GRAPHPOS_APP_PANEL_VERSION,
            Self::Process => SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
        }
    }

    const fn identity_tag(self) -> &'static [u8] {
        match self {
            Self::App => b"synapse-graphpos-app-v1",
            Self::Process => b"synapse-graphpos-process-v1",
        }
    }

    /// App and process are distinct panels measuring distinct graphs, so their
    /// signatures must land in distinct physical slot column families (#1776).
    const fn signature_slot(self) -> SlotId {
        match self {
            Self::App => GP_APP_SLOT_SIGNATURE,
            Self::Process => GP_PROCESS_SLOT_SIGNATURE,
        }
    }

    const fn neighbors_slot(self) -> SlotId {
        match self {
            Self::App => GP_APP_SLOT_NEIGHBORS,
            Self::Process => GP_PROCESS_SLOT_NEIGHBORS,
        }
    }
}

/// One node's structural position in a fingerprinted graph snapshot.
///
/// Computed by the `calyx-mincut` substrate (`structural_signatures`). Degrees
/// are raw counts; the four centralities are normalized to `[0, 1]`.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphPositionSignature {
    pub in_degree: u64,
    pub out_degree: u64,
    pub total_degree: u64,
    pub betweenness: f64,
    pub eigenvector: f64,
    pub pagerank: f64,
    pub clustering: f64,
    /// Labels of the node's graph neighbours, for the neighbour-label histogram.
    pub neighbor_labels: Vec<String>,
}

/// One record's position in a fingerprinted document/URL/process/spawn hierarchy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathPositionSignature {
    pub depth: u64,
    pub sibling_rank: u64,
    pub sibling_count: u64,
    pub subtree_size: u64,
    pub ancestor_count: u64,
    pub path_len: u64,
    pub is_root: bool,
    pub is_leaf: bool,
    /// Ordered ancestor path components, for the ancestor-set multi-hot.
    pub ancestor_components: Vec<String>,
    /// Stable leaf path key, for the path hash slot.
    pub path_hash_key: String,
}

#[derive(Serialize)]
struct GraphSignatureLensInput {
    in_degree: u64,
    out_degree: u64,
    total_degree: u64,
    betweenness: f64,
    eigenvector: f64,
    pagerank: f64,
    clustering: f64,
}

#[derive(Serialize)]
struct PathSignatureLensInput {
    depth: u64,
    sibling_rank: u64,
    sibling_count: u64,
    subtree_size: u64,
    ancestor_count: u64,
    path_len: u64,
    is_root: bool,
    is_leaf: bool,
}

/// Deterministic 64-bit fingerprint of a graph/hierarchy snapshot.
///
/// Derived from the aggregated `(src, dst, count)` transitions. A different
/// snapshot yields a different fingerprint, which pins both the frozen lens id
/// and the derived panel generation so structural values can never silently
/// drift.
#[must_use]
pub fn graph_snapshot_fingerprint(transitions: &[(String, String, u64)]) -> u64 {
    let mut sorted: Vec<&(String, String, u64)> = transitions.iter().collect();
    sorted.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-graph-snapshot-v1");
    for (src, dst, count) in sorted {
        hasher.update((src.len() as u64).to_be_bytes());
        hasher.update(src.as_bytes());
        hasher.update((dst.len() as u64).to_be_bytes());
        hasher.update(dst.as_bytes());
        hasher.update(count.to_be_bytes());
    }
    let digest = hasher.finalize();
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(prefix)
}

/// Stable identity bytes for a graph-position node in one snapshot.
///
/// The snapshot is part of the identity so re-measuring a node under a new
/// snapshot lands in a new Base generation rather than mutating the old
/// constellation.
#[must_use]
pub fn graph_position_identity_bytes(
    kind: GraphPositionKind,
    snapshot: u64,
    node_id: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(kind.identity_tag().len() + node_id.len() + 24);
    append_framed(&mut out, kind.identity_tag());
    append_framed(&mut out, &snapshot.to_be_bytes());
    append_framed(&mut out, node_id.as_bytes());
    out
}

/// Stable identity bytes for a path-hierarchy record in one snapshot.
#[must_use]
pub fn path_position_identity_bytes(snapshot: u64, node_key: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(b"synapse-path-hierarchy-v1".len() + node_key.len() + 24);
    append_framed(&mut out, b"synapse-path-hierarchy-v1");
    append_framed(&mut out, &snapshot.to_be_bytes());
    append_framed(&mut out, node_key.as_bytes());
    out
}

/// Builds the derived graph-position constellation for one node in a snapshot.
///
/// `panel_version` is the vault-global generation allocated for this snapshot by
/// the #1668 lifecycle allocator (`kind.base_panel_version()` for the first
/// generation). The frozen [`AlgorithmicLens::syn_graph_signature`] is pinned to
/// `snapshot`, so a new snapshot produces a new lens id — never silent drift.
///
/// # Errors
///
/// Returns a measurement error when the node id is empty, a centrality is not
/// finite, or a Syn* lens rejects its input.
pub fn build_graph_position_constellation(
    context: NativeConstellationContext,
    kind: GraphPositionKind,
    panel_version: u32,
    snapshot: u64,
    node_id: &str,
    signature: &GraphPositionSignature,
) -> StorageResult<Constellation> {
    let node_id = non_empty(node_id)
        .ok_or_else(|| measurement_error("empty graph-position node id", kind.panel_name()))?;
    let panel_name = kind.panel_name();
    let lens_input = GraphSignatureLensInput {
        in_degree: signature.in_degree,
        out_degree: signature.out_degree,
        total_degree: signature.total_degree,
        betweenness: clamp_unit(signature.betweenness, "betweenness")?,
        eigenvector: clamp_unit(signature.eigenvector, "eigenvector")?,
        pagerank: clamp_unit(signature.pagerank, "pagerank")?,
        clustering: clamp_unit(signature.clustering, "clustering")?,
    };
    let mut slots = BTreeMap::new();
    slots.insert(
        kind.signature_slot(),
        measure_json(
            panel_name,
            AlgorithmicLens::syn_graph_signature(
                "syn.graphpos.signature.v1",
                Modality::Structured,
                snapshot,
            ),
            &lens_input,
        )?,
    );
    slots.insert(
        kind.neighbors_slot(),
        optional_json_slice_slot(
            panel_name,
            AlgorithmicLens::syn_multi_hot(
                "syn.graphpos.neighbor_histogram.v1",
                Modality::Structured,
                GP_NEIGHBOR_HISTOGRAM_DIM,
            ),
            &signature.neighbor_labels,
        )?,
    );

    let identity = graph_position_identity_bytes(kind, snapshot, node_id);
    let identity_hash = sha256_hex(&identity);
    let mut metadata = common_metadata(
        panel_name,
        "CALYX_GRAPHPOS",
        identity_hash.as_bytes(),
        &identity,
    );
    metadata.insert("graph_snapshot".to_owned(), snapshot.to_string());
    metadata.insert("graph_node_id".to_owned(), truncate_metadata(node_id));
    insert_graph_position_readback(&mut metadata, signature);
    constellation(
        context,
        panel_version,
        format!("synapse://graphpos/{panel_name}/{snapshot:016x}/{identity_hash}"),
        &identity,
        slots,
        BTreeMap::new(),
        metadata,
    )
}

/// Builds the derived path-hierarchy constellation for one record in a snapshot.
///
/// # Errors
///
/// Returns a measurement error when the node key is empty, a field is invalid,
/// or a Syn* lens rejects its input.
pub fn build_path_hierarchy_constellation(
    context: NativeConstellationContext,
    panel_version: u32,
    snapshot: u64,
    node_key: &str,
    signature: &PathPositionSignature,
) -> StorageResult<Constellation> {
    let node_key = non_empty(node_key).ok_or_else(|| {
        measurement_error(
            "empty path-hierarchy node key",
            SYN_PATH_HIERARCHY_PANEL_NAME,
        )
    })?;
    let panel_name = SYN_PATH_HIERARCHY_PANEL_NAME;
    let lens_input = PathSignatureLensInput {
        depth: signature.depth,
        sibling_rank: signature.sibling_rank,
        sibling_count: signature.sibling_count,
        subtree_size: signature.subtree_size,
        ancestor_count: signature.ancestor_count,
        path_len: signature.path_len,
        is_root: signature.is_root,
        is_leaf: signature.is_leaf,
    };
    let mut slots = BTreeMap::new();
    slots.insert(
        PH_SLOT_SIGNATURE,
        measure_json(
            panel_name,
            AlgorithmicLens::syn_path_signature(
                "syn.path_hierarchy.signature.v1",
                Modality::Structured,
                snapshot,
            ),
            &lens_input,
        )?,
    );
    slots.insert(
        PH_SLOT_ANCESTORS,
        optional_json_slice_slot(
            panel_name,
            AlgorithmicLens::syn_multi_hot(
                "syn.path_hierarchy.ancestor_set.v1",
                Modality::Structured,
                PH_ANCESTOR_DIM,
            ),
            &signature.ancestor_components,
        )?,
    );
    slots.insert(
        PH_SLOT_PATH_HASH,
        optional_hash_slot(
            panel_name,
            "syn.path_hierarchy.path_hash.v1",
            non_empty(&signature.path_hash_key),
            PH_PATH_HASH_DIM,
        )?,
    );

    let identity = path_position_identity_bytes(snapshot, node_key);
    let identity_hash = sha256_hex(&identity);
    let mut metadata = common_metadata(
        panel_name,
        "CALYX_PATH_HIERARCHY",
        identity_hash.as_bytes(),
        &identity,
    );
    metadata.insert("graph_snapshot".to_owned(), snapshot.to_string());
    metadata.insert("path_node_key".to_owned(), truncate_metadata(node_key));
    metadata.insert("path_depth".to_owned(), signature.depth.to_string());
    metadata.insert(
        "path_sibling_rank".to_owned(),
        signature.sibling_rank.to_string(),
    );
    metadata.insert(
        "path_subtree_size".to_owned(),
        signature.subtree_size.to_string(),
    );
    constellation(
        context,
        panel_version,
        format!("synapse://path-hierarchy/{snapshot:016x}/{identity_hash}"),
        &identity,
        slots,
        BTreeMap::new(),
        metadata,
    )
}

fn insert_graph_position_readback(
    metadata: &mut BTreeMap<String, String>,
    signature: &GraphPositionSignature,
) {
    metadata.insert(
        "graph_in_degree".to_owned(),
        signature.in_degree.to_string(),
    );
    metadata.insert(
        "graph_out_degree".to_owned(),
        signature.out_degree.to_string(),
    );
    metadata.insert(
        "graph_total_degree".to_owned(),
        signature.total_degree.to_string(),
    );
    metadata.insert(
        "graph_betweenness".to_owned(),
        signature.betweenness.to_string(),
    );
    metadata.insert(
        "graph_eigenvector".to_owned(),
        signature.eigenvector.to_string(),
    );
    metadata.insert("graph_pagerank".to_owned(), signature.pagerank.to_string());
    metadata.insert(
        "graph_clustering".to_owned(),
        signature.clustering.to_string(),
    );
}

/// Clamps a centrality into `[0, 1]`, tolerating floating-point overshoot from
/// the substrate while rejecting non-finite values fail-closed.
fn clamp_unit(value: f64, field: &str) -> StorageResult<f64> {
    if !value.is_finite() {
        return Err(measurement_error(
            "non-finite graph centrality",
            format!("{field}={value}"),
        ));
    }
    Ok(value.clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------
// Panel lifecycle: capability cards + admission gate (#1668)
//
// A newly added lens/panel is measured immediately (Parked) and may only be
// promoted to full (Admitted) status once a capability card meets the documented
// thresholds. The card is profiled from the #1672 assay reports (bits /
// sufficiency / redundancy); this gate consumes plain numeric profile fields so
// it stays decoupled from the evolving synapse-calyx report structs. Park/retire
// are non-destructive: retired slots remain readable for history.
// ---------------------------------------------------------------------------

/// Synthetic CF label used in structured panel-lifecycle errors.
const PANEL_LIFECYCLE_CF: &str = "calyx_panel_lifecycle";

/// Signal floor (bits) a lens must clear to be admitted.
///
/// Mirrors the Calyx assay bit floor (`SYNAPSE_ASSAY_BIT_FLOOR`, handbook §13).
pub const PANEL_ADMISSION_BIT_FLOOR: f32 = 0.05;
/// Pairwise redundancy (normalized MI) ceiling above which a lens is a duplicate.
pub const PANEL_ADMISSION_CORRELATION_CEILING: f32 = 0.6;
/// Minimum paired samples before a capability profile is trusted.
pub const PANEL_ADMISSION_MIN_SAMPLES: usize = 50;

/// Lifecycle state of a panel generation or one of its lens slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelLifecycleState {
    /// Added and measured on new records, but not yet admitted to full status.
    Parked,
    /// Admitted: measured on all records and searchable on its slot.
    Admitted,
    /// Retired: no longer measured on new records; slot stays readable for
    /// history (non-destructive).
    Retired,
}

impl PanelLifecycleState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parked => "parked",
            Self::Admitted => "admitted",
            Self::Retired => "retired",
        }
    }

    /// Every lifecycle state keeps its historical slot values readable.
    #[must_use]
    pub const fn is_readable(self) -> bool {
        true
    }

    /// Whether new records are measured on this slot in this state.
    #[must_use]
    pub const fn measures_new_records(self) -> bool {
        matches!(self, Self::Parked | Self::Admitted)
    }
}

/// The measured capability profile of one lens slot.
///
/// Field values come from the #1672 assay reports: `signal_bits` from the per
/// lens grounded-bits pass, `max_redundancy_nmi` from the redundancy/effective
/// rank pass, `n_samples` from the anchored records scanned. `spread`,
/// `separation`, and `coverage` are `[0, 1]` descriptors; `cost_micros` is the
/// measured encode cost.
#[derive(Clone, Debug, PartialEq)]
pub struct LensCapabilityProfile {
    pub panel_name: String,
    pub panel_version: u32,
    pub slot: u16,
    pub signal_bits: f32,
    pub spread: f32,
    pub separation: f32,
    pub cost_micros: u64,
    pub coverage: f32,
    pub max_redundancy_nmi: f32,
    pub n_samples: usize,
}

/// The gate's decision for one lens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PanelAdmissionOutcome {
    /// The profile clears every threshold; the lens may be admitted.
    Admit,
    /// The profile is valid but fails one or more thresholds; stay parked.
    Park { reasons: Vec<String> },
}

impl PanelAdmissionOutcome {
    #[must_use]
    pub const fn is_admit(&self) -> bool {
        matches!(self, Self::Admit)
    }
}

/// A capability card: the measured profile plus the gate's decision and the
/// exact threshold snapshot the decision was made against.
#[derive(Clone, Debug, PartialEq)]
pub struct CapabilityCard {
    pub panel_name: String,
    pub panel_version: u32,
    pub slot: u16,
    pub signal_bits: f32,
    pub spread: f32,
    pub separation: f32,
    pub cost_micros: u64,
    pub coverage: f32,
    pub max_redundancy_nmi: f32,
    pub n_samples: usize,
    pub bit_floor: f32,
    pub correlation_ceiling: f32,
    pub min_samples: usize,
    pub outcome: PanelAdmissionOutcome,
}

/// Profiles one lens into a [`CapabilityCard`] and decides admission fail-closed.
///
/// Malformed profiles (non-finite metrics, out-of-range `[0, 1]` descriptors,
/// negative bits) are refused with a structured error rather than being silently
/// admitted. A well-formed but under-signal profile yields a `Park` outcome, not
/// an error.
///
/// # Errors
///
/// Returns a structured `CALYX_PANEL_CAPABILITY_INVALID` error when a profile
/// field is non-finite or out of its valid range.
pub fn evaluate_capability(profile: &LensCapabilityProfile) -> StorageResult<CapabilityCard> {
    validate_unit_metric("signal_bits", profile.signal_bits, false)?;
    if profile.signal_bits < 0.0 {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_CAPABILITY_INVALID",
            &format!(
                "panel={} slot={} signal_bits={} must be >= 0",
                profile.panel_name, profile.slot, profile.signal_bits
            ),
            "recompute the assay bits pass; a negative bits estimate is a measurement fault",
        ));
    }
    validate_unit_metric("spread", profile.spread, true)?;
    validate_unit_metric("separation", profile.separation, true)?;
    validate_unit_metric("coverage", profile.coverage, true)?;
    validate_unit_metric("max_redundancy_nmi", profile.max_redundancy_nmi, true)?;

    let mut reasons = Vec::new();
    if profile.n_samples < PANEL_ADMISSION_MIN_SAMPLES {
        reasons.push(format!(
            "insufficient_samples: {} < {PANEL_ADMISSION_MIN_SAMPLES}",
            profile.n_samples
        ));
    }
    if profile.signal_bits < PANEL_ADMISSION_BIT_FLOOR {
        reasons.push(format!(
            "below_bit_floor: signal_bits {} < {PANEL_ADMISSION_BIT_FLOOR}",
            profile.signal_bits
        ));
    }
    if profile.max_redundancy_nmi > PANEL_ADMISSION_CORRELATION_CEILING {
        reasons.push(format!(
            "redundant: max_redundancy_nmi {} > {PANEL_ADMISSION_CORRELATION_CEILING}",
            profile.max_redundancy_nmi
        ));
    }
    if profile.coverage <= 0.0 {
        reasons.push("no_coverage: slot is absent on every anchored record".to_owned());
    }

    let outcome = if reasons.is_empty() {
        PanelAdmissionOutcome::Admit
    } else {
        PanelAdmissionOutcome::Park { reasons }
    };
    Ok(CapabilityCard {
        panel_name: profile.panel_name.clone(),
        panel_version: profile.panel_version,
        slot: profile.slot,
        signal_bits: profile.signal_bits,
        spread: profile.spread,
        separation: profile.separation,
        cost_micros: profile.cost_micros,
        coverage: profile.coverage,
        max_redundancy_nmi: profile.max_redundancy_nmi,
        n_samples: profile.n_samples,
        bit_floor: PANEL_ADMISSION_BIT_FLOOR,
        correlation_ceiling: PANEL_ADMISSION_CORRELATION_CEILING,
        min_samples: PANEL_ADMISSION_MIN_SAMPLES,
        outcome,
    })
}

/// Admits a lens to full status, fail-closed against its capability card.
///
/// A lens can only be admitted from `Parked` and only when its card's outcome is
/// `Admit`. A retired lens is terminal and can never be re-admitted.
///
/// # Errors
///
/// Returns a structured error when the transition is illegal or the card does
/// not meet the documented thresholds.
pub fn admit_lens(
    current: PanelLifecycleState,
    card: &CapabilityCard,
) -> StorageResult<PanelLifecycleState> {
    if current == PanelLifecycleState::Retired {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_ADMIT_RETIRED",
            &format!(
                "panel={} slot={} is retired and cannot be re-admitted",
                card.panel_name, card.slot
            ),
            "allocate a new panel generation for the lens instead of re-admitting a retired slot",
        ));
    }
    match &card.outcome {
        PanelAdmissionOutcome::Admit => Ok(PanelLifecycleState::Admitted),
        PanelAdmissionOutcome::Park { reasons } => Err(panel_lifecycle_error(
            "CALYX_PANEL_ADMISSION_REFUSED",
            &format!(
                "panel={} slot={} failed the admission gate: {}",
                card.panel_name,
                card.slot,
                reasons.join("; ")
            ),
            "keep the lens parked; raise grounded signal above the bit floor or reduce redundancy below the ceiling, then re-profile",
        )),
    }
}

/// Parks a lens (idempotent). Retired lenses are terminal.
///
/// # Errors
///
/// Returns a structured error when parking a retired lens.
pub fn park_lens(current: PanelLifecycleState) -> StorageResult<PanelLifecycleState> {
    if current == PanelLifecycleState::Retired {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_PARK_RETIRED",
            "a retired lens is terminal and cannot be parked",
            "allocate a new panel generation instead of reviving a retired slot",
        ));
    }
    Ok(PanelLifecycleState::Parked)
}

/// Retires a lens (idempotent, non-destructive). The slot stays readable.
#[must_use]
pub const fn retire_lens(_current: PanelLifecycleState) -> PanelLifecycleState {
    PanelLifecycleState::Retired
}

/// The declared lens name for every physical slot id, mirroring the exact
/// lens-name literal each ingest measurement site passes to its frozen
/// `AlgorithmicLens` (issue #1897).
///
/// Slot ids are globally unique across every `syn-*` panel, so one flat table
/// answers "which lens is slot N?" for any panel without the caller having to
/// know which panel it is asking about. That question had no answer before: the
/// intelligence surfaces reported bare slot numbers, so an operator told that
/// "a column is constant" could not tell which of a panel's lenses was the dead
/// one without reading code and re-deriving the corpus.
///
/// This table is a *mirror*, not the source: the measurement sites still own
/// the literal, because a lens name is content-addressed into its `LensId` and
/// editing one here would silently repoint a slot at a different lens. It was
/// generated by extracting the literals from those sites, and a slot whose name
/// is not listed is reported as unnamed rather than guessed at.
const SYN_SLOT_LENS_NAMES: &[(SlotId, &str)] = &[
    (TL_SLOT_KIND_ONEHOT, "syn.timeline.kind_onehot.v1"),
    (TL_SLOT_APP_HASH, "syn.timeline.app_hash.v1"),
    (TL_SLOT_TITLE_SPARSE, "syn.timeline.title_sparse.v1"),
    (TL_SLOT_HOUR_CYCLIC, "syn.timeline.hour_cyclic.v1"),
    (TL_SLOT_DOW_CYCLIC, "syn.timeline.dow_cyclic.v1"),
    (TL_SLOT_ACTOR_ONEHOT, "syn.timeline.actor_onehot.v1"),
    (TL_SLOT_RECENCY_RANK, "syn.timeline.event_time_rank.v1"),
    (TL_SLOT_TITLE_BM25, "syn.timeline.title_bm25.v1"),
    (TL_SLOT_RECORD_VECTOR, "syn.timeline.record_vector.v1"),
    (EP_SLOT_APP_HASH, "syn.episode.app_hash.v1"),
    (EP_SLOT_DOCUMENT_HASH, "syn.episode.document_hash.v1"),
    (EP_SLOT_URL_HOST_HASH, "syn.episode.url_host_hash.v1"),
    (EP_SLOT_TITLE_SPARSE, "syn.episode.title_sparse.v1"),
    (EP_SLOT_TITLE_BM25, "syn.episode.title_bm25.v1"),
    (
        EP_SLOT_START_HOUR_CYCLIC,
        "syn.episode.start_hour_cyclic.v1",
    ),
    (EP_SLOT_START_DOW_CYCLIC, "syn.episode.start_dow_cyclic.v1"),
    (EP_SLOT_DURATION_LOG1P, "syn.episode.duration_log1p.v1"),
    (EP_SLOT_DURATION_RANK, "syn.episode.duration_rank.v1"),
    (
        EP_SLOT_KEYSTROKES_ZSCORE,
        "syn.episode.keystrokes_zscore.v1",
    ),
    (EP_SLOT_CLICKS_ZSCORE, "syn.episode.clicks_zscore.v1"),
    (EP_SLOT_ROW_COUNT_ZSCORE, "syn.episode.row_count_zscore.v1"),
    (
        EP_SLOT_STARTED_BOUNDARY_ONEHOT,
        "syn.episode.started_boundary_onehot.v1",
    ),
    (
        EP_SLOT_ENDED_BOUNDARY_ONEHOT,
        "syn.episode.ended_boundary_onehot.v1",
    ),
    (
        EP_SLOT_INTERRUPTION_RATIO,
        "syn.episode.interruption_ratio_raw.v1",
    ),
    (EP_SLOT_RECORD_VECTOR, "syn.episode.record_vector.v2"),
    (AE_SLOT_KIND_ONEHOT, "syn.agent_event.kind_onehot.v1"),
    (
        AE_SLOT_OPERATION_ONEHOT,
        "syn.agent_event.operation_onehot.v1",
    ),
    (AE_SLOT_PROVIDER_HASH, "syn.agent_event.provider_hash.v1"),
    (
        AE_SLOT_REQUEST_MODEL_HASH,
        "syn.agent_event.request_model_hash.v1",
    ),
    (
        AE_SLOT_RESPONSE_MODEL_HASH,
        "syn.agent_event.response_model_hash.v1",
    ),
    (AE_SLOT_TOOL_HASH, "syn.agent_event.tool_hash.v1"),
    (AE_SLOT_ERROR_ONEHOT, "syn.agent_event.error_onehot.v1"),
    (
        AE_SLOT_END_STATE_ONEHOT,
        "syn.agent_event.end_state_onehot.v2",
    ),
    (AE_SLOT_HOUR_CYCLIC, "syn.agent_event.hour_cyclic.v1"),
    (AE_SLOT_DOW_CYCLIC, "syn.agent_event.dow_cyclic.v1"),
    (
        AE_SLOT_USAGE_TOTAL_LOG1P,
        "syn.agent_event.usage_total_log1p.v1",
    ),
    (AE_SLOT_HAS_END_STATE, "syn.agent_event.has_end_state.v1"),
    (AT_SLOT_ROLE_ONEHOT, "syn.agent_transcript.role_onehot.v1"),
    (
        AT_SLOT_STATUS_ONEHOT,
        "syn.agent_transcript.status_onehot.v2",
    ),
    (
        AT_SLOT_SOURCE_ONEHOT,
        "syn.agent_transcript.source_onehot.v1",
    ),
    (
        AT_SLOT_EVENT_KIND_HASH,
        "syn.agent_transcript.event_kind_hash.v1",
    ),
    (AT_SLOT_MODEL_HASH, "syn.agent_transcript.model_hash.v1"),
    (AT_SLOT_TEXT_SPARSE, "syn.agent_transcript.text_sparse.v1"),
    (AT_SLOT_TEXT_BM25, "syn.agent_transcript.text_bm25.v1"),
    (
        AT_SLOT_TEXT_FULL_BM25,
        "syn.agent_transcript.text_full_bm25.v1",
    ),
    (AT_SLOT_TOOL_HASH, "syn.agent_transcript.tool_hash.v1"),
    (AT_SLOT_LINE_RANK, "syn.agent_transcript.line_rank.v1"),
    (
        AT_SLOT_INPUT_TOKENS_LOG1P,
        "syn.agent_transcript.input_tokens_log1p.v1",
    ),
    (
        AT_SLOT_OUTPUT_TOKENS_LOG1P,
        "syn.agent_transcript.output_tokens_log1p.v1",
    ),
    (
        AT_SLOT_CACHE_READ_LOG1P,
        "syn.agent_transcript.cache_read_log1p.v1",
    ),
    (
        AT_SLOT_CACHE_CREATION_LOG1P,
        "syn.agent_transcript.cache_creation_log1p.v1",
    ),
    (
        AT_SLOT_RECORD_VECTOR_V2,
        "syn.agent_transcript.record_vector.v2",
    ),
    (ACT_SLOT_KIND_ONEHOT, "syn.action.kind_onehot.v2"),
    (ACT_SLOT_TARGET_HASH, "syn.action.target_hash.v2"),
    (ACT_SLOT_RECORD_VECTOR, "syn.action.record_vector.v2"),
    (ACT_SLOT_HOUR_CYCLIC, "syn.action.hour_cyclic.v1"),
    (ACT_SLOT_DOW_CYCLIC, "syn.action.dow_cyclic.v1"),
    (ACT_SLOT_TARGET_VECTOR, "syn.action.target_vector.v2"),
    (ACT_SLOT_REQUEST_VECTOR, "syn.action.request_vector.v1"),
    (
        ACT_SLOT_REQUEST_SIZE_CLASS,
        "syn.action.request_size_class.v1",
    ),
    (
        ACT_SLOT_REQUEST_SHAPE_CLASS,
        "syn.action.request_shape_class.v1",
    ),
    (RF_SLOT_REFLEX_HASH, "syn.reflex.reflex_hash.v1"),
    (RF_SLOT_OUTCOME_ONEHOT, "syn.reflex.outcome_onehot.v1"),
    (RF_SLOT_LATENCY_LOG1P, "syn.reflex.latency_ms_log1p.v1"),
    (RF_SLOT_STEP_COUNT_LOG1P, "syn.reflex.step_count_log1p.v1"),
    (RF_SLOT_HOUR_CYCLIC, "syn.reflex.hour_cyclic.v1"),
    (RF_SLOT_DOW_CYCLIC, "syn.reflex.dow_cyclic.v1"),
    (PR_SLOT_PROCESS_HASH, "syn.process.process_hash.v1"),
    (PR_SLOT_EVENT_ONEHOT, "syn.process.event_onehot.v1"),
    (PR_SLOT_HOUR_CYCLIC, "syn.process.hour_cyclic.v1"),
    (PR_SLOT_DOW_CYCLIC, "syn.process.dow_cyclic.v1"),
    (PR_SLOT_UPTIME_LOG1P, "syn.process.uptime_ms_log1p.v1"),
    (PR_SLOT_RECENCY_RANK, "syn.process.event_time_rank.v1"),
    (OB_SLOT_APP_HASH, "syn.observation.app_hash.v1"),
    (OB_SLOT_ROLE_HISTOGRAM, "syn.observation.role_histogram.v1"),
    (
        OB_SLOT_ENTITY_MULTI_HOT,
        "syn.observation.entity_multi_hot.v1",
    ),
    (
        OB_SLOT_FLAGS_MULTI_HOT,
        "syn.observation.flags_multi_hot.v1",
    ),
    (OB_SLOT_HOUR_CYCLIC, "syn.observation.hour_cyclic.v1"),
    (OB_SLOT_DOW_CYCLIC, "syn.observation.dow_cyclic.v1"),
    (OUT_SLOT_SOURCE_CF_ONEHOT, "syn.outcome.source_cf_onehot.v1"),
    (OUT_SLOT_EVENT_ONEHOT, "syn.outcome.event_onehot.v1"),
    (OUT_SLOT_STATUS_ONEHOT, "syn.outcome.status_onehot.v1"),
    (OUT_SLOT_TARGET_HASH, "syn.outcome.target_hash.v1"),
    (OUT_SLOT_HOUR_CYCLIC, "syn.outcome.hour_cyclic.v1"),
    (OUT_SLOT_DOW_CYCLIC, "syn.outcome.dow_cyclic.v1"),
    (OUT_SLOT_RECORD_VECTOR_V2, "syn.outcome.record_vector.v2"),
    (MU_SLOT_TOOL_ONEHOT, "syn.mcp_usage.tool_onehot.v1"),
    (
        MU_SLOT_OPERATION_ONEHOT,
        "syn.mcp_usage.operation_onehot.v1",
    ),
    (MU_SLOT_ROUTE_HASH, "syn.mcp_usage.route_hash.v1"),
    (
        MU_SLOT_PARAM_SHAPE_HASH,
        "syn.mcp_usage.param_shape_hash.v1",
    ),
    (MU_SLOT_STATUS_ONEHOT, "syn.mcp_usage.status_onehot.v1"),
    (MU_SLOT_ERROR_ONEHOT, "syn.mcp_usage.error_onehot.v1"),
    (MU_SLOT_PROFILE_HASH, "syn.mcp_usage.profile_hash.v1"),
    (MU_SLOT_SURFACE_HASH, "syn.mcp_usage.tool_surface_hash.v1"),
    (
        MU_SLOT_SESSION_SEQUENCE_RANK,
        "syn.mcp_usage.session_sequence_rank.v1",
    ),
    (MU_SLOT_HOUR_CYCLIC, "syn.mcp_usage.hour_cyclic.v1"),
    (MU_SLOT_DOW_CYCLIC, "syn.mcp_usage.dow_cyclic.v1"),
    (MU_SLOT_RECORD_VECTOR_V2, "syn.mcp_usage.record_vector.v2"),
    (RS_SLOT_KIND_ONEHOT, "syn.recurrence_subject.kind_onehot.v1"),
    (
        RS_SLOT_SUBJECT_HASH,
        "syn.recurrence_subject.identity_hash.v1",
    ),
    (GP_APP_SLOT_SIGNATURE, "syn.graphpos.signature.v1"),
    (GP_APP_SLOT_NEIGHBORS, "syn.graphpos.neighbor_histogram.v1"),
    (GP_PROCESS_SLOT_SIGNATURE, "syn.graphpos.signature.v1"),
    (
        GP_PROCESS_SLOT_NEIGHBORS,
        "syn.graphpos.neighbor_histogram.v1",
    ),
    (PH_SLOT_SIGNATURE, "syn.path_hierarchy.signature.v1"),
    (PH_SLOT_ANCESTORS, "syn.path_hierarchy.ancestor_set.v1"),
    (PH_SLOT_PATH_HASH, "syn.path_hierarchy.path_hash.v1"),
];

/// Fails closed when the measurement-provenance declaration has drifted from the
/// slot/lens catalog it must mirror (#1958 ask 4).
///
/// ## What it enforces
///
/// `synapse_calyx::lens_provenance::SYN_SLOT_SOURCE_FIELDS` declares, per slot,
/// the record fields that slot's construction site reads. That declaration is
/// what lets an assay refuse a circular measurement **structurally**, instead of
/// hoping a statistical detector notices — which it provably cannot for a lens
/// that merely *contains* the label (#1958).
///
/// A declaration nobody enforces is a declaration that drifts, and #1953's own
/// facade gap is what that looks like: `anchor_leakage` existed internally,
/// serialised correctly, and simply did not exist on the wire. So this checks
/// three things rather than one:
///
/// 1. every catalogued slot is declared — a new lens cannot be silently unaudited;
/// 2. no declaration names a slot the catalog does not have — a deleted or
///    renumbered slot cannot leave a stale entry behind that matches nothing;
/// 3. the lens **name** each declaration carries equals the catalogued name —
///    so a slot reassigned to a different lens invalidates its field list
///    instead of inheriting it.
///
/// It also checks that every panel version named by an anchor declaration is a
/// live panel version, so a version bump cannot silently detach the anchor's
/// determining-field set and turn a refusal into a clean pass.
///
/// Called from [`syn_reconstructable_panel_contract`], which is on the daemon's startup
/// path: a drifted declaration stops the process rather than publishing a panel
/// whose slots cannot be adjudicated.
///
/// # Errors
///
/// Returns an error naming every undeclared slot, every orphaned declaration,
/// every duplicate, every lens-name mismatch, and every unknown panel version.
pub fn assert_syn_lens_provenance_complete() -> StorageResult<()> {
    let mut declared: BTreeMap<u16, &'static str> = BTreeMap::new();
    let mut duplicates = Vec::new();
    for (slot, _, lens, _) in lens_provenance::SYN_SLOT_SOURCE_FIELDS {
        if declared.insert(*slot, lens).is_some() {
            duplicates.push(*slot);
        }
    }
    let catalog: BTreeMap<u16, &'static str> = SYN_SLOT_LENS_NAMES
        .iter()
        .map(|(slot, name)| (slot.get(), *name))
        .collect();
    let undeclared: Vec<u16> = catalog
        .keys()
        .filter(|slot| !declared.contains_key(*slot))
        .copied()
        .collect();
    let orphaned: Vec<u16> = declared
        .keys()
        .filter(|slot| !catalog.contains_key(*slot))
        .copied()
        .collect();
    let renamed: Vec<String> = catalog
        .iter()
        .filter_map(|(slot, name)| {
            let found = declared.get(slot)?;
            (found != name).then(|| format!("{slot}: catalog={name} declared={found}"))
        })
        .collect();
    let known_versions: BTreeSet<u32> = [
        SYN_TIMELINE_PANEL_VERSION,
        SYN_EPISODE_PANEL_VERSION,
        SYN_AGENT_EVENT_PANEL_VERSION,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        SYN_ACTION_PANEL_VERSION,
        SYN_REFLEX_PANEL_VERSION,
        SYN_PROCESS_PANEL_VERSION,
        SYN_OBSERVATION_PANEL_VERSION,
        SYN_OUTCOME_PANEL_VERSION,
        SYN_MCP_USAGE_PANEL_VERSION,
        SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
        SYN_GRAPHPOS_APP_PANEL_VERSION,
        SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
        SYN_PATH_HIERARCHY_PANEL_VERSION,
    ]
    .into_iter()
    .collect();
    let stale_anchor_versions: Vec<String> = lens_provenance::SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .filter(|(_, version, _)| !known_versions.contains(version))
        .map(|(kind, version, _)| format!("{kind}@{version}"))
        .collect();

    // #1962. An anchor declaration on a panel the catalog declares
    // observation-shaped is a contradiction between two declarations, and the
    // failure mode is silent in the worst direction: `outcome_bearing: false`
    // says 0.0 grounded coverage is *correct*, while the anchor declaration says
    // an outcome is expected here and simply is not being written. Nothing
    // reconciled the two, so `syn-timeline-v1 @ 1900001` sat with a declared
    // `synapse:mcp_tool_call_outcome` that no code path could ever produce — an
    // intent that read as a defect on every grounding readback.
    //
    // Refused at startup rather than reported, because the two declarations are
    // both compile-time constants: if they disagree, one of them is wrong now,
    // and no amount of runtime evidence will settle it.
    let observation_shaped: BTreeSet<u32> = builtin_panel_catalog()
        .into_iter()
        .filter(|entry| !entry.outcome_bearing)
        .flat_map(|entry| {
            std::iter::once(entry.panel_version).chain(entry.superseded_versions.iter().copied())
        })
        .collect();
    let anchors_on_observation_panels: Vec<String> = lens_provenance::SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .filter(|(_, version, _)| observation_shaped.contains(version))
        .map(|(kind, version, _)| format!("{kind}@{version}"))
        .collect();

    if undeclared.is_empty()
        && orphaned.is_empty()
        && duplicates.is_empty()
        && renamed.is_empty()
        && stale_anchor_versions.is_empty()
        && anchors_on_observation_panels.is_empty()
    {
        return Ok(());
    }
    Err(StorageError::WriteFailed {
        cf_name: cf::CF_KV.to_owned(),
        detail: format!(
            "CALYX_LENS_SOURCE_FIELDS_INCOMPLETE: every built-in panel slot must declare the \
             record fields it measures so anchor leakage can be refused structurally (#1958). \
             undeclared_slots={undeclared:?} orphaned_declarations={orphaned:?} \
             duplicate_declarations={duplicates:?} lens_name_mismatches={renamed:?} \
             anchor_declarations_on_unknown_panel_versions={stale_anchor_versions:?} \
             anchor_declarations_on_observation_shaped_panels={anchors_on_observation_panels:?}. \
             Fix synapse_calyx::lens_provenance, listing every record field the slot's \
             construction site in this file reads transitively. An anchor declared on a panel \
             this file's builtin_panel_catalog marks outcome_bearing=false contradicts that \
             catalog entry (#1962): either the panel does receive that outcome and the catalog \
             entry is wrong, or it does not and the anchor declaration must go — never both."
        ),
    })
}

/// Returns the declared lens name for one physical slot id, or `None` when the
/// slot is not one of the built-in `syn-*` panel slots.
#[must_use]
pub fn syn_slot_lens_name(slot: SlotId) -> Option<&'static str> {
    SYN_SLOT_LENS_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == slot)
        .map(|(_, name)| *name)
}

/// The whole declared slot-to-lens-name catalog, for callers that need to hand
/// a lens-naming map to a lower layer that cannot see these declarations.
#[must_use]
pub fn syn_slot_lens_names() -> BTreeMap<u16, String> {
    SYN_SLOT_LENS_NAMES
        .iter()
        .map(|(slot, name)| (slot.get(), (*name).to_owned()))
        .collect()
}

/// How a panel's record population relates to a physical source column family.
///
/// This is the declaration that makes a coverage *fraction* meaningful. #1927
/// asks for "`active_version_records` vs `source_cf_rows`", and that ratio is
/// only a coverage number when the panel is supposed to hold one constellation
/// per source row. For a panel fed by a filtered or sampled subset of a CF the
/// same ratio is a meaningless number that would read as a permanent 2% outage,
/// so those panels declare the weaker relationship instead of borrowing the
/// stronger one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelSource {
    /// One constellation per row of this CF. A coverage fraction below 1.0 means
    /// rows are stranded and a backfill is owed.
    FullCf(&'static str),
    /// Fed by a *subset* of this CF (a key prefix, a sampling rate, a row-kind
    /// filter). Record count and CF row count are both reported, and no coverage
    /// fraction is derived from them, because the denominator is wrong.
    SubsetOfCf(&'static str),
    /// Built from a derived snapshot rather than per source row (the #1685
    /// graph-position and hierarchy panels, and the recurrence-subject panel,
    /// whose records are subjects rather than occurrences). No source CF.
    Derived,
}

impl PanelSource {
    /// The CF this panel reads, when it reads one.
    #[must_use]
    pub const fn cf_name(self) -> Option<&'static str> {
        match self {
            Self::FullCf(cf) | Self::SubsetOfCf(cf) => Some(cf),
            Self::Derived => None,
        }
    }

    /// True only when `records / cf_rows` is a coverage fraction rather than an
    /// arbitrary ratio of two different populations.
    #[must_use]
    pub const fn is_full_cf(self) -> bool {
        matches!(self, Self::FullCf(_))
    }
}

/// One built-in panel, with the declarations coverage and grounding readbacks
/// need in order to interpret a number instead of guessing at it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelCatalogEntry {
    pub panel_name: &'static str,
    /// The generation new records are written at today.
    pub panel_version: u32,
    /// What this panel's population is drawn from.
    pub source: PanelSource,
    /// **#1920 ask 3.** Whether this panel is supposed to carry grounded outcome
    /// anchors at all, declared rather than inferred from its coverage.
    ///
    /// The census across all fourteen panels split them cleanly and the split is
    /// recorded here so it stops being re-derived: timeline, action, reflex,
    /// process, observation, recurrence-subject and the three graphpos/path
    /// panels are *observations* — a timeline row records that something was
    /// seen, not how it turned out — and their 0.0 grounded coverage is correct,
    /// not a gap. Episode, outcome, mcp-usage, agent-event and agent-transcript
    /// are outcome-bearing, and a 0.0 there IS a gap.
    ///
    /// Load-bearing, not cosmetic: a grounding readback must not report a
    /// deliberate 0.0 as a deficiency, and the intelligence surfaces must not
    /// wait for bits from a panel that will never have an anchor to measure them
    /// about.
    pub outcome_bearing: bool,
    /// **#1940.** Whether this panel's source CF carries an audit TTL, so its
    /// rows are expected to be evicted out from under the constellations
    /// measured from them. Declared here rather than inferred, for the same
    /// reason `outcome_bearing` is: it decides whether an observation is a
    /// finding.
    ///
    /// A constellation whose source row is gone is *sacred and permanent*
    /// either way — nothing can re-measure a row that no longer exists. But the
    /// two causes call for opposite responses. On a TTL-managed source the
    /// absence is the retention policy working as designed, and reporting it as
    /// an anomaly on every census trains readers to ignore the field. On a
    /// source with no TTL, the same absence means a constellation's provenance
    /// points at a row that was never written or was destroyed outside the
    /// retention path — a grounding violation, and a real integrity finding.
    ///
    /// Measured on the live vault 2026-08-01 by per-record source-key probe:
    /// 227 of 81,733 attributed constellations had lost their source row, and
    /// every one of them belonged to a panel declared TTL-managed here
    /// (`syn-action-v1` 224, `syn-process-v1` 2, `syn-observation-v1` 1). Zero
    /// belonged to a non-TTL source.
    pub source_ttl_managed: bool,
    /// Whether grounded anchors on superseded generations retain the same
    /// meaning and may be carried to the active generation.
    ///
    /// False when the generation bump deliberately changes outcome
    /// adjudication. In that case historical anchors remain sacred evidence,
    /// but are neither replay debt nor valid inputs to the new contract.
    pub carry_superseded_anchors: bool,
    /// Generations this panel has been through, newest-superseded first.
    ///
    /// Rows at these versions are still physically in the `Base` CF and are
    /// still counted by every whole-CF row count, but nothing reads them: a
    /// record is only found by a surface scoped to the *active* version. Naming
    /// them is what lets the coverage readback separate "stranded on a
    /// superseded generation" from "never measured at all" — two states with
    /// opposite remedies.
    pub superseded_versions: &'static [u32],
    /// The CF `Db::backfill_temporal_metadata` can re-measure this panel from,
    /// when there is one.
    ///
    /// **#1927 ask 2** needs exactly this: the maintainer cannot drive a
    /// backfill for a panel whose re-measure path does not exist, and silently
    /// treating "no path" as "already covered" is how a panel sits at 1.7%
    /// indefinitely with health reporting ok. A `None` here is reported as an
    /// un-backfillable shortfall, not as success.
    pub backfill_source_cf: Option<&'static str>,
}

/// One panel version's place in a live panel's declared lineage (#1972).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupersededPanelLineage {
    /// The panel whose lineage this version belongs to.
    pub panel_name: &'static str,
    /// The generation that superseded it and is live today.
    pub live_panel_version: u32,
}

/// Resolves a panel version that has **no** code-declared slot contract into
/// its place in a live panel's lineage, when it has one (#1972 ask 2).
///
/// The search-generation sweep collapsed two situations with opposite remedies
/// into one `unmaintainable_no_contract` bucket:
///
/// * *this is a closed superseded version of a panel that is still live* —
///   retiring its published search generation is safe and correct, because the
///   live generation of the same panel carries the corpus; and
/// * *this panel version is entirely unknown to the code* — nothing may be
///   deleted until someone establishes what it is.
///
/// Nothing new is declared to tell them apart: [`builtin_panel_catalog`]
/// already carries `superseded_versions` per panel, and this reads it. That
/// matters because a *second* declaration of the same fact could drift from the
/// first, and a lineage table that disagrees with the catalog would authorise
/// deleting an index for a panel the catalog still considers live.
#[must_use]
pub fn superseded_panel_lineage(panel_version: u32) -> Option<SupersededPanelLineage> {
    builtin_panel_catalog().into_iter().find_map(|entry| {
        entry
            .superseded_versions
            .contains(&panel_version)
            .then_some(SupersededPanelLineage {
                panel_name: entry.panel_name,
                live_panel_version: entry.panel_version,
            })
    })
}

/// The built-in panel catalog: every `syn-*` panel, its active generation, and
/// the declarations #1920 ask 3 and #1927 asks 1/3 require.
///
/// `builtin_panel_catalog` previously existed with a doc comment saying it was
/// "for the `panel list` action" and **had no caller anywhere in the workspace**
/// (#1920's census comment found this). It is now the single source of truth the
/// panel-coverage readback joins the physical census against.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "the catalog is one exhaustive static declaration whose source, anchor, retrieval, and lifecycle contracts must remain co-located"
)]
pub fn builtin_panel_catalog() -> Vec<PanelCatalogEntry> {
    vec![
        // --- observation-shaped: correctly unanchored (#1920 ask 3) ---
        PanelCatalogEntry {
            panel_name: SYN_TIMELINE_PANEL_NAME,
            panel_version: SYN_TIMELINE_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_TIMELINE),
            outcome_bearing: false,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[
                SYN_TIMELINE_PANEL_VERSION_PRE_1963,
                SYN_TIMELINE_PANEL_VERSION_PRE_1900,
            ],
            backfill_source_cf: Some(cf::CF_TIMELINE),
        },
        PanelCatalogEntry {
            panel_name: SYN_ACTION_PANEL_NAME,
            panel_version: SYN_ACTION_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_ACTION_LOG),
            outcome_bearing: true,
            source_ttl_managed: true,
            // #2020/#2021: 2020001 replaced over-broad action adjudication.
            //
            // #2050 keeps this `false`. The generation adds a slot rather than
            // changing adjudication, so an anchor written under 2020001 still
            // *means* the same thing — but the rows it sits on were measured
            // without slot 117, and carrying the anchor forward without the
            // vector would hand Ward good exemplars that cannot be scored on
            // the very lane the generation exists to calibrate. Re-measuring
            // the source CF is the supported path, and it re-derives the anchor
            // from the same row.
            carry_superseded_anchors: false,
            superseded_versions: &[
                1_666_001,
                SYN_ACTION_PANEL_VERSION_PRE_1965,
                SYN_ACTION_PANEL_VERSION_PRE_2006,
                SYN_ACTION_PANEL_VERSION_PRE_2020,
                SYN_ACTION_PANEL_VERSION_PRE_2050,
                SYN_ACTION_PANEL_VERSION_PRE_2185,
                SYN_ACTION_PANEL_VERSION_PRE_REQUEST,
                SYN_ACTION_PANEL_VERSION_PRE_REQUEST_CLASSES,
            ],
            backfill_source_cf: Some(cf::CF_ACTION_LOG),
        },
        PanelCatalogEntry {
            panel_name: SYN_REFLEX_PANEL_NAME,
            panel_version: SYN_REFLEX_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_REFLEX_AUDIT),
            outcome_bearing: false,
            source_ttl_managed: true,
            carry_superseded_anchors: true,
            superseded_versions: &[1_666_002, SYN_REFLEX_PANEL_VERSION_PRE_1965],
            backfill_source_cf: Some(cf::CF_REFLEX_AUDIT),
        },
        PanelCatalogEntry {
            panel_name: SYN_PROCESS_PANEL_NAME,
            panel_version: SYN_PROCESS_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_PROCESS_HISTORY),
            outcome_bearing: false,
            source_ttl_managed: true,
            carry_superseded_anchors: true,
            superseded_versions: &[1_666_003, SYN_PROCESS_PANEL_VERSION_PRE_1965],
            backfill_source_cf: Some(cf::CF_PROCESS_HISTORY),
        },
        PanelCatalogEntry {
            panel_name: SYN_OBSERVATION_PANEL_NAME,
            panel_version: SYN_OBSERVATION_PANEL_VERSION,
            // Sampled: one constellation every
            // SYN_OBSERVATION_SAMPLE_EVERY_N_DEFAULT rows, so the CF row count
            // is deliberately NOT this panel's denominator.
            source: PanelSource::SubsetOfCf(cf::CF_OBSERVATIONS),
            outcome_bearing: false,
            source_ttl_managed: true,
            carry_superseded_anchors: true,
            superseded_versions: &[1_666_004, SYN_OBSERVATION_PANEL_VERSION_PRE_1965],
            backfill_source_cf: Some(cf::CF_OBSERVATIONS),
        },
        PanelCatalogEntry {
            panel_name: SYN_RECURRENCE_SUBJECT_PANEL_NAME,
            panel_version: SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
            // A record here is a recurrence *subject*, not an occurrence, so no
            // CF is its population.
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[1_667_001],
            backfill_source_cf: None,
        },
        // --- derived-snapshot panels: the declared version is a RESERVATION ---
        //
        // #2093. `panel_version` here is the family's reserved *base* generation
        // and by construction holds **zero** `Base` rows. A derived snapshot is
        // published under a generation the vault-global allocator mints
        // (`publish_graph_position_snapshot` -> `allocate_panel_generation`), and
        // #2062 floored dynamic allocation at
        // `CALYX_DYNAMIC_PANEL_GENERATION_FLOOR = 3_000_000_000` — far above any
        // `issue * 1000 + n` built-in constant. So no publish can ever write at
        // `1_685_00x`, and `active_version_records = 0` on these three rows is
        // the permanent, correct reading of a reservation, not an empty panel.
        //
        // Their live rows are attributed through
        // `PanelCoverageReport::owned_dynamic_generations`, which is where the
        // record counts, the live/retired split and the #2062 retirement ledger
        // for these publishers actually are. Reading `records=0` on this row as
        // "the publisher produced nothing" is the misreading #2093 was filed on.
        PanelCatalogEntry {
            panel_name: SYN_GRAPHPOS_APP_PANEL_NAME,
            panel_version: SYN_GRAPHPOS_APP_PANEL_VERSION,
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            // #2093: empty, and it must stay empty. `cd78ac90` (#1983) bumped
            // `syn-agent-event-v1` off `1_665_001` and appended that retired
            // generation to *this* entry instead of the agent-event entry
            // immediately below it in the same file. The census then attributed
            // 16,650 agent-event rows — 166 of them grounded — to a graph panel
            // declared `backfill_source_cf: None`, so 59 replayable anchors were
            // reported permanently `anchor_debt_unbackfillable` and
            // `panel_coverage` held `health.ok = false` with no reachable repair.
            // The generation now sits on the panel that wrote it, which has both
            // a re-measure path and `carry_superseded_anchors`.
            superseded_versions: &[],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_GRAPHPOS_PROCESS_PANEL_NAME,
            panel_version: SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_PATH_HIERARCHY_PANEL_NAME,
            panel_version: SYN_PATH_HIERARCHY_PANEL_VERSION,
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[],
            backfill_source_cf: None,
        },
        // --- outcome-bearing: a 0.0 here IS a gap (#1920 ask 3) ---
        PanelCatalogEntry {
            panel_name: SYN_EPISODE_PANEL_NAME,
            panel_version: SYN_EPISODE_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_EPISODES),
            outcome_bearing: true,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[
                SYN_EPISODE_PANEL_VERSION_PRE_1964,
                SYN_EPISODE_PANEL_VERSION_PRE_1904,
            ],
            backfill_source_cf: Some(cf::CF_EPISODES),
        },
        PanelCatalogEntry {
            panel_name: SYN_AGENT_EVENT_PANEL_NAME,
            panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_AGENT_EVENTS),
            outcome_bearing: true,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            // Newest-superseded first, and complete (#2093). `PRE_1983` —
            // `1_665_001`, the generation #1665 created and #1983 retired — was
            // missing here and misfiled onto `syn-graphpos-app-v1` above. The
            // shape now mirrors `syn-agent-transcript-v1`, whose analogous
            // `PRE_1983` landed on the right entry in the same commit.
            superseded_versions: &[
                SYN_AGENT_EVENT_PANEL_VERSION_PRE_1965,
                SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983,
            ],
            backfill_source_cf: Some(cf::CF_AGENT_EVENTS),
        },
        PanelCatalogEntry {
            panel_name: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            panel_version: SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_AGENT_TRANSCRIPTS),
            outcome_bearing: true,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[
                SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1965,
                SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1983,
                SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1921,
                SYN_AGENT_TRANSCRIPT_PANEL_VERSION_PRE_1904,
            ],
            backfill_source_cf: Some(cf::CF_AGENT_TRANSCRIPTS),
        },
        PanelCatalogEntry {
            panel_name: SYN_OUTCOME_PANEL_NAME,
            panel_version: SYN_OUTCOME_PANEL_VERSION,
            // CF_KV and CF_ROUTINE_STATE both feed this panel, and CF_KV holds
            // many unrelated row families, so neither count is a denominator.
            source: PanelSource::SubsetOfCf(cf::CF_KV),
            outcome_bearing: true,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[1_669_001, SYN_OUTCOME_PANEL_VERSION_PRE_1965],
            backfill_source_cf: Some(SYN_OUTCOME_BACKFILL_SOURCE),
        },
        PanelCatalogEntry {
            panel_name: SYN_MCP_USAGE_PANEL_NAME,
            panel_version: SYN_MCP_USAGE_PANEL_VERSION,
            // The `mcp-usage/v1/` key prefix within CF_KV.
            source: PanelSource::SubsetOfCf(cf::CF_KV),
            outcome_bearing: true,
            source_ttl_managed: false,
            carry_superseded_anchors: true,
            superseded_versions: &[1_691_001, SYN_MCP_USAGE_PANEL_VERSION_PRE_1965],
            backfill_source_cf: Some(SYN_MCP_USAGE_BACKFILL_SOURCE),
        },
    ]
}

/// Every live panel generation a caller may name in `panel_version` on a fused
/// query — the **declared-queryable set** (#2075).
///
/// `find operation=similar` / `storage operation=find_similar` accept an
/// explicit `panel_version` and search it directly. What they accept is decided
/// by exactly one thing: whether [`syn_queryable_panel_contract`] advertises a
/// query contract for that version. A version with no query contract fails
/// closed rather than being searched through a neighbouring panel's slot map, so the set of
/// versions a query can *reach* is precisely the live catalog generations that
/// have a contract.
///
/// This exists because three different questions were being answered with two
/// different facts:
///
/// * *which generations exist on disk* — `published_search_generations`;
/// * *which generation the vault manifest names active* — the active pointer;
/// * *which generations a caller may query* — nothing answered this, so the
///   unattended search-generation maintainer used the first two as a stand-in.
///
/// A panel that is re-versioned gets a brand-new version with no directory on
/// disk and (unless it is the active panel) no maintainer, so its first
/// generation was never built by anything: `find panel_version=<new>` failed
/// with `SYNAPSE_CALYX_FIND_INDEX_STALE` indefinitely while health called
/// search `ok`. Derived here, never declared twice, so the maintained set
/// cannot drift from the queryable set.
///
/// A version whose contract build *fails* is still declared-queryable: the code
/// declares it, the declaration is merely broken, and the caller of this
/// function must report that as a failure against that panel rather than
/// silently narrowing the set it maintains.
#[must_use]
pub fn declared_queryable_panel_versions(created_at_ms: u64) -> Vec<u32> {
    let mut versions: Vec<u32> = builtin_panel_catalog()
        .into_iter()
        .filter(|entry| {
            !matches!(
                syn_queryable_panel_contract(entry.panel_version, created_at_ms),
                Ok(None)
            )
        })
        .map(|entry| entry.panel_version)
        .collect();
    versions.sort_unstable();
    versions.dedup();
    versions
}

/// The catalog entry owning one panel version, whether active or superseded.
#[must_use]
pub fn panel_catalog_entry_for_version(panel_version: u32) -> Option<PanelCatalogEntry> {
    builtin_panel_catalog().into_iter().find(|entry| {
        entry.panel_version == panel_version || entry.superseded_versions.contains(&panel_version)
    })
}

/// The superseded generations of the panel a source CF feeds, newest first.
///
/// **#1980.** A record id is a content address over `(input_bytes,
/// panel_version, vault_salt)`, so bumping a panel version gives every record a
/// NEW `cx_id` — and an anchor is keyed by the `cx_id` it was written against.
/// A bump therefore strands every anchor the corpus had: the re-measured
/// generation is born ungrounded, and nothing reads the old one.
///
/// That is not a hypothesis. On this vault `syn-episode-v1` bumped
/// `1_904_002` -> `1_964_001` and the `Base` census reads 171 records grounded at
/// the superseded generation and **0 of the same 171** at the active one, from
/// the identical 171 `CF_EPISODES` source rows at coverage 1.0.
///
/// Grounding is not a property of the lens layout — it is an observed real
/// outcome of the *source row*, and the source row did not change. So the
/// anchors are carried across the bump rather than re-derived, and this is the
/// declaration the carry walks.
///
/// # Errors
///
/// Returns a write-scoped storage error when the source CF has no native
/// constellation contract. Fail-closed on purpose: silently returning "no prior
/// generations" for an unknown CF would re-create the exact silent
/// un-grounding this exists to stop.
pub fn superseded_panel_versions_for_source_cf(source_cf: &str) -> StorageResult<&'static [u32]> {
    let panel = anchor_panel_for_source_cf(source_cf)?;
    builtin_panel_catalog()
        .into_iter()
        .find(|entry| entry.panel_name == panel.panel_name)
        .map(|entry| entry.superseded_versions)
        .ok_or_else(|| {
            panel_lifecycle_error(
                "CALYX_PANEL_CATALOG_ENTRY_ABSENT",
                &format!(
                    "source CF {source_cf} resolves to panel {} but builtin_panel_catalog has no \
                     entry for that panel name",
                    panel.panel_name
                ),
                "add the panel to builtin_panel_catalog; an anchor carry-forward cannot run \
                 without its declared generation history",
            )
        })
}

fn validate_unit_metric(field: &str, value: f32, unit_range: bool) -> StorageResult<()> {
    if !value.is_finite() {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_CAPABILITY_INVALID",
            &format!("capability metric {field}={value} is not finite"),
            "recompute the assay pass; a non-finite metric is a measurement fault",
        ));
    }
    if unit_range && !(0.0..=1.0).contains(&value) {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_CAPABILITY_INVALID",
            &format!("capability metric {field}={value} must be within [0, 1]"),
            "clamp or recompute the assay descriptor into its documented [0, 1] range",
        ));
    }
    Ok(())
}

fn panel_lifecycle_error(code: &str, message: &str, remediation: &str) -> StorageError {
    StorageError::WriteFailed {
        cf_name: PANEL_LIFECYCLE_CF.to_owned(),
        detail: format!("{code}: {message}; remediation={remediation}"),
    }
}

/// Build the Calyx constellation for a timeline record.
///
/// # Errors
///
/// Returns an error when enum serialization, lens measurement, JSON encoding,
/// or exact integer scalar conversion fails.
#[expect(
    clippy::too_many_lines,
    reason = "the frozen timeline panel's complete ordered slot measurement is one schema contract"
)]
pub fn build_timeline_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &TimelineRecord,
) -> StorageResult<Constellation> {
    let mut slots = BTreeMap::new();
    slots.insert(
        TL_SLOT_KIND_ONEHOT,
        measure_text(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_one_hot("syn.timeline.kind_onehot.v1", Modality::Structured, 32),
            &timeline_kind_name(record.kind)?,
        )?,
    );
    slots.insert(
        TL_SLOT_APP_HASH,
        optional_hash_slot(
            SYN_TIMELINE_PANEL_NAME,
            "syn.timeline.app_hash.v1",
            record.app.as_deref(),
            1024,
        )?,
    );
    slots.insert(
        TL_SLOT_TITLE_SPARSE,
        measure_text_or_absent(
            SYN_TIMELINE_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text(
                "syn.timeline.title_sparse.v1",
                Modality::Structured,
                2048,
            ),
            timeline_title(record).as_deref().unwrap_or(""),
        )?,
    );
    let (hour, dow) = utc_hour_and_dow(record.ts_ns);
    slots.insert(
        TL_SLOT_HOUR_CYCLIC,
        measure_number(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_cyclic_time(
                "syn.timeline.hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            hour,
        )?,
    );
    slots.insert(
        TL_SLOT_DOW_CYCLIC,
        measure_number(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_cyclic_time("syn.timeline.dow_cyclic.v1", Modality::Structured, 7),
            dow,
        )?,
    );
    slots.insert(
        TL_SLOT_ACTOR_ONEHOT,
        measure_text(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_one_hot("syn.timeline.actor_onehot.v1", Modality::Structured, 8),
            actor_kind(&record.actor),
        )?,
    );
    slots.insert(
        TL_SLOT_RECENCY_RANK,
        measure_scalar_rank(
            SYN_TIMELINE_PANEL_NAME,
            "syn.timeline.event_time_rank.v1",
            0,
            RECENCY_RANK_MAX_UNIX_MS_MICROS,
            record.ts_ns / NS_PER_MS,
        )?,
    );

    slots.insert(
        TL_SLOT_TITLE_BM25,
        measure_text_or_absent(
            SYN_TIMELINE_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text_tf(
                "syn.timeline.title_bm25.v1",
                Modality::Structured,
                2048,
            ),
            timeline_title(record).as_deref().unwrap_or(""),
        )?,
    );

    // The panel's graded dense lens (#1963).
    slots.insert(
        TL_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_record_vector_unit_fields(
                "syn.timeline.record_vector.v1",
                Modality::Structured,
                TL_RECORD_VECTOR_DIM,
            ),
            &timeline_numeric_record(record, raw_bytes),
        )?,
    );

    let scalars = timeline_scalars(record, raw_bytes)?;
    let metadata = timeline_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_TIMELINE_PANEL_VERSION,
        source_pointer(cf::CF_TIMELINE, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for an episode record.
///
/// # Errors
///
/// Returns an error when enum serialization, lens measurement, JSON encoding,
/// or exact integer scalar conversion fails.
#[allow(
    clippy::too_many_lines,
    reason = "episode constellation construction is a one-to-one field-to-slot map; splitting would obscure the stable slot contract"
)]
pub fn build_episode_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &EpisodeRecord,
) -> StorageResult<Constellation> {
    let mut slots = BTreeMap::new();
    slots.insert(
        EP_SLOT_APP_HASH,
        optional_hash_slot(
            SYN_EPISODE_PANEL_NAME,
            "syn.episode.app_hash.v1",
            record.app.as_deref(),
            1024,
        )?,
    );
    slots.insert(
        EP_SLOT_DOCUMENT_HASH,
        optional_hash_slot(
            SYN_EPISODE_PANEL_NAME,
            "syn.episode.document_hash.v1",
            record.document.as_deref(),
            2048,
        )?,
    );
    let episode_url_host = record.url.as_deref().and_then(url_host);
    slots.insert(
        EP_SLOT_URL_HOST_HASH,
        optional_hash_slot(
            SYN_EPISODE_PANEL_NAME,
            "syn.episode.url_host_hash.v1",
            episode_url_host.as_deref(),
            2048,
        )?,
    );
    slots.insert(
        EP_SLOT_TITLE_SPARSE,
        measure_text_or_absent(
            SYN_EPISODE_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text(
                "syn.episode.title_sparse.v1",
                Modality::Structured,
                4096,
            ),
            episode_title_text(record).as_str(),
        )?,
    );
    slots.insert(
        EP_SLOT_TITLE_BM25,
        measure_text_or_absent(
            SYN_EPISODE_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text_tf(
                "syn.episode.title_bm25.v1",
                Modality::Structured,
                EP_TITLE_BM25_DIM,
            ),
            episode_title_text(record).as_str(),
        )?,
    );
    let (hour, dow) = utc_hour_and_dow(record.start_ts_ns);
    slots.insert(
        EP_SLOT_START_HOUR_CYCLIC,
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_cyclic_time(
                "syn.episode.start_hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            hour,
        )?,
    );
    slots.insert(
        EP_SLOT_START_DOW_CYCLIC,
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_cyclic_time(
                "syn.episode.start_dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            dow,
        )?,
    );
    let duration_ms = record.duration_ms();
    slots.insert(
        EP_SLOT_DURATION_LOG1P,
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_log1p(
                "syn.episode.duration_log1p.v1",
                Modality::Structured,
            ),
            duration_ms,
        )?,
    );
    slots.insert(
        EP_SLOT_DURATION_RANK,
        measure_scalar_rank(
            SYN_EPISODE_PANEL_NAME,
            "syn.episode.duration_rank.v1",
            0,
            MAX_DAY_DURATION_MS_MICROS,
            duration_ms,
        )?,
    );
    slots.insert(
        EP_SLOT_KEYSTROKES_ZSCORE,
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_zscore(
                "syn.episode.keystrokes_zscore.v1",
                Modality::Structured,
                0,
                10_000_000,
            ),
            record.keystroke_count,
        )?,
    );
    slots.insert(
        EP_SLOT_CLICKS_ZSCORE,
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_zscore(
                "syn.episode.clicks_zscore.v1",
                Modality::Structured,
                0,
                1_000_000,
            ),
            record.click_count,
        )?,
    );
    slots.insert(
        EP_SLOT_ROW_COUNT_ZSCORE,
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_zscore(
                "syn.episode.row_count_zscore.v1",
                Modality::Structured,
                0,
                1_000_000,
            ),
            record.row_count,
        )?,
    );
    slots.insert(
        EP_SLOT_STARTED_BOUNDARY_ONEHOT,
        measure_text(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_one_hot(
                "syn.episode.started_boundary_onehot.v1",
                Modality::Structured,
                16,
            ),
            &boundary_name(record.started_because)?,
        )?,
    );
    slots.insert(
        EP_SLOT_ENDED_BOUNDARY_ONEHOT,
        measure_text(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_one_hot(
                "syn.episode.ended_boundary_onehot.v1",
                Modality::Structured,
                16,
            ),
            &boundary_name(record.ended_because)?,
        )?,
    );
    slots.insert(
        EP_SLOT_INTERRUPTION_RATIO,
        measure_float(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_raw(
                "syn.episode.interruption_ratio_raw.v1",
                Modality::Structured,
            ),
            interruption_ratio(record),
        )?,
    );
    slots.insert(
        EP_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_record_vector_unit_fields(
                "syn.episode.record_vector.v2",
                Modality::Structured,
                EP_RECORD_VECTOR_DIM,
            ),
            &episode_numeric_record(record),
        )?,
    );

    let scalars = episode_scalars(record, raw_bytes)?;
    let metadata = episode_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_EPISODE_PANEL_VERSION,
        source_pointer(cf::CF_EPISODES, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Builds the authoritative active-panel contract for one live panel version.
///
/// Reuses the exact frozen `AlgorithmicLens` constructors the ingest builders
/// use so the published `Panel` carries the true slot identities (lens id,
/// shape, modality) rather than a synthesized guess. The content slots are
/// `Active`; retrieval-only temporal sidecars live in the native Registry CF
/// and are not part of the panel's content slot set.
///
/// Returns `None` for panel versions whose durable content-slot contract is not
/// enumerated here; callers must fail closed on `None` rather than publish a
/// partial contract. This is the source of truth consumed by
/// `SynapseCalyxVault::publish_active_panel`.
/// Complete immutable panel definition and the frozen lens registry needed to
/// reconstruct every active slot after a process restart.
pub struct SynActivePanelContract {
    /// Durable active-panel definition.
    pub panel: Panel,
    /// Frozen runtime contracts for every lens referenced by `panel`.
    pub registry: Registry,
}

/// Returns the complete reconstructable storage contract for a known panel
/// generation.
///
/// This describes how the panel's persisted slots are encoded. It does **not**
/// advertise that a search/neighbourhood operation can rank the panel. That
/// separate capability is declared by [`syn_queryable_panel_contract`]. In
/// particular, agent-event is reconstructable here but intentionally not
/// queryable: #1965 retired its tied record vector and every remaining direction
/// is finite.
///
/// # Errors
///
/// Returns an error when a built-in slot has a non-reconstructable runtime or
/// when its frozen lens contract and structured registry specification differ.
#[must_use = "panel contract errors and unknown generations must be handled"]
pub fn syn_reconstructable_panel_contract(
    panel_version: u32,
    created_at_ms: u64,
) -> StorageResult<Option<SynActivePanelContract>> {
    // #1958 ask 4: no panel contract is built while a built-in lens has no
    // declared source-field set. This runs on the daemon's startup path, so an
    // undeclared lens stops the process rather than becoming a silently
    // unaudited slot in a published panel.
    assert_syn_lens_provenance_complete()?;
    let mut registry = Registry::new();
    let slots = match panel_version {
        SYN_TIMELINE_PANEL_VERSION => timeline_panel_slots(panel_version, &mut registry)?,
        SYN_EPISODE_PANEL_VERSION => episode_panel_slots(panel_version, &mut registry)?,
        SYN_AGENT_EVENT_PANEL_VERSION => agent_event_panel_slots(panel_version, &mut registry)?,
        SYN_MCP_USAGE_PANEL_VERSION => mcp_usage_panel_slots(panel_version, &mut registry)?,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION => {
            agent_transcript_panel_slots(panel_version, &mut registry)?
        }
        SYN_ACTION_PANEL_VERSION => action_panel_slots(panel_version, &mut registry)?,
        _ => return Ok(None),
    };
    Ok(Some(SynActivePanelContract {
        panel: Panel {
            version: panel_version,
            slots,
            created_at: created_at_ms,
            kernel_ref: None,
            guard_ref: None,
        },
        registry,
    }))
}

/// Returns the complete contract only when the generation explicitly supports
/// fused search and neighbourhood-derived intelligence.
///
/// Queryability is a capability declaration, not an inference from the presence
/// of a storage schema. Every declared generation is also checked for a graded
/// dense lens, so a broken declaration is a hard error and remains visible to
/// the search maintainer. Reconstructable finite-only panels return `Ok(None)`.
///
/// # Errors
///
/// Returns an error when a declared queryable generation has no exact
/// reconstructable contract, when that contract has no graded dense lens, or
/// when a frozen slot runtime/provenance declaration is invalid.
#[must_use = "query-admission errors and non-queryable generations must be handled"]
pub fn syn_queryable_panel_contract(
    panel_version: u32,
    created_at_ms: u64,
) -> StorageResult<Option<SynActivePanelContract>> {
    if !syn_panel_is_queryable(panel_version) {
        return Ok(None);
    }
    let contract = syn_reconstructable_panel_contract(panel_version, created_at_ms)?.ok_or_else(
        || {
            panel_lifecycle_error(
                "CALYX_QUERYABLE_PANEL_CONTRACT_MISSING",
                &format!(
                    "panel {panel_version} is declared queryable but has no reconstructable slot contract"
                ),
                "declare the exact frozen slot runtimes before advertising this panel as queryable",
            )
        },
    )?;
    // #1963 ask 3: a panel that cannot support a neighbourhood analysis says so
    // at admission, not three analyses later.
    assert_panel_carries_graded_dense_lens(
        panel_version,
        &contract.panel.slots,
        &contract.registry,
    )?;
    Ok(Some(contract))
}

/// Whether a built-in panel is admitted to persisted retrieval generation.
///
/// This is intentionally distinct from reconstructability: finite-only panels
/// are valid exact analytical populations but must never acquire empty or
/// meaningless ANN lanes merely to obtain panel membership.
#[must_use]
pub(crate) const fn syn_panel_is_queryable(panel_version: u32) -> bool {
    matches!(
        panel_version,
        SYN_TIMELINE_PANEL_VERSION
            | SYN_EPISODE_PANEL_VERSION
            | SYN_MCP_USAGE_PANEL_VERSION
            | SYN_AGENT_TRANSCRIPT_PANEL_VERSION
            | SYN_ACTION_PANEL_VERSION
    )
}

/// The built-in contract for the live agent-event panel.
///
/// This mirrors [`build_agent_event_constellation`] exactly. It exists for
/// lifecycle, grading, and measurement consumers. It is deliberately absent
/// from [`syn_queryable_panel_contract`]: its measured-constant record vector was
/// retired by #1965, so the remaining finite-direction lanes cannot honestly
/// rank a neighbourhood or support a search-membership generation.
#[allow(
    clippy::too_many_lines,
    reason = "agent event panel contract is a one-to-one slot-to-frozen-lens map mirroring build_agent_event_constellation; splitting would obscure the stable contract"
)]
fn agent_event_panel_slots(
    panel_version: u32,
    registry: &mut Registry,
) -> StorageResult<Vec<Slot>> {
    Ok(vec![
        syn_content_slot(
            AE_SLOT_KIND_ONEHOT,
            "syn.agent_event.kind_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.agent_event.kind_onehot.v1",
                Modality::Structured,
                32,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_OPERATION_ONEHOT,
            "syn.agent_event.operation_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.agent_event.operation_onehot.v1",
                Modality::Structured,
                16,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_PROVIDER_HASH,
            "syn.agent_event.provider_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_event.provider_hash.v1",
                Modality::Structured,
                512,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_REQUEST_MODEL_HASH,
            "syn.agent_event.request_model_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_event.request_model_hash.v1",
                Modality::Structured,
                1024,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_RESPONSE_MODEL_HASH,
            "syn.agent_event.response_model_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_event.response_model_hash.v1",
                Modality::Structured,
                1024,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_TOOL_HASH,
            "syn.agent_event.tool_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_event.tool_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_ERROR_ONEHOT,
            "syn.agent_event.error_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.agent_event.error_onehot.v1",
                Modality::Structured,
                64,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_END_STATE_ONEHOT,
            "syn.agent_event.end_state_onehot.v2",
            RegistryAlgorithmicLens::syn_one_hot_index(
                "syn.agent_event.end_state_onehot.v2",
                Modality::Structured,
                3,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_HOUR_CYCLIC,
            "syn.agent_event.hour_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.agent_event.hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_DOW_CYCLIC,
            "syn.agent_event.dow_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.agent_event.dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_USAGE_TOTAL_LOG1P,
            "syn.agent_event.usage_total_log1p.v1",
            RegistryAlgorithmicLens::syn_scalar_log1p(
                "syn.agent_event.usage_total_log1p.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AE_SLOT_HAS_END_STATE,
            "syn.agent_event.has_end_state.v1",
            RegistryAlgorithmicLens::syn_one_hot_index(
                "syn.agent_event.has_end_state.v1",
                Modality::Structured,
                2,
            ),
            panel_version,
            registry,
        )?,
    ])
}

/// One panel slot's declared cosine grading (#1963 ask 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SynSlotCosineGrading {
    pub slot_id: u16,
    pub slot_key: String,
    /// `not_dense` | `constant` | `finite` | `graded`, from
    /// `calyx_registry::DenseCosineGrading`.
    pub grading: &'static str,
    /// Set when the image is a finite set: how many distinct directions the
    /// encoder can reach. A dense lane saturates its nearest-neighbour cosine
    /// as soon as the record count comfortably exceeds this.
    pub finite_directions: Option<u32>,
    /// Retrieval-only slots are post-retrieval ordinates, not recall lanes, and
    /// are exempt from the panel's graded-lens requirement.
    pub retrieval_only: bool,
}

/// The per-slot cosine-grading table for a built-in panel (#1963 ask 3).
///
/// A panel that cannot support a neighbourhood analysis should say so at
/// admission, not three analyses later. This is the readback half of that: the
/// refusal in [`assert_panel_carries_graded_dense_lens`] stops the impossible
/// case, and this names, per slot, *why* the panel's dense view looks the way
/// it does — including the finite-image sizes that predict saturation.
///
/// Read from the persisted `LensSpec`, so it reports the declaration that will
/// be written to the vault rather than an in-memory object.
///
/// Returns `Ok(None)` for panel versions with no built-in contract.
///
/// # Errors
///
/// Returns an error when the panel contract cannot be built, or when a slot's
/// persisted runtime kind does not parse back to an encoder.
pub fn syn_panel_cosine_grading(
    panel_version: u32,
) -> StorageResult<Option<Vec<SynSlotCosineGrading>>> {
    let Some(contract) = syn_reconstructable_panel_contract(panel_version, 0)? else {
        return Ok(None);
    };
    let mut rows = Vec::with_capacity(contract.panel.slots.len());
    for slot in &contract.panel.slots {
        let spec = contract.registry.lens_spec(slot.lens_id).ok_or_else(|| {
            panel_lifecycle_error(
                "CALYX_PANEL_SLOT_GRADING_UNDECLARED",
                &format!("slot {} has no persisted LensSpec", slot.slot_id),
                "register every built-in slot through syn_content_slot/syn_retrieval_only_slot",
            )
        })?;
        let grading = match &spec.runtime {
            LensRuntime::Algorithmic { kind } => {
                calyx_registry::algorithmic_encoder(kind, spec.output)
                    .ok_or_else(|| {
                        panel_lifecycle_error(
                            "CALYX_PANEL_SLOT_GRADING_UNDECLARED",
                            &format!("slot {} runtime kind {kind} does not parse", slot.slot_id),
                            "use a runtime kind persisted_syn_runtime_kind can round-trip",
                        )
                    })?
                    .dense_cosine_grading()
            }
            // A model-backed embedder's image is a continuum by construction.
            _ => calyx_registry::DenseCosineGrading::Graded,
        };
        rows.push(SynSlotCosineGrading {
            slot_id: slot.slot_id.get(),
            slot_key: slot.slot_key.key().to_owned(),
            grading: grading.as_str(),
            finite_directions: match grading {
                calyx_registry::DenseCosineGrading::Finite(n) => Some(n),
                _ => None,
            },
            retrieval_only: slot.retrieval_only,
        });
    }
    Ok(Some(rows))
}

/// Refuses a panel that carries no lens capable of a graded similarity (#1963).
///
/// Read from the **persisted** `LensSpec` each slot was registered with, not
/// from an in-memory encoder, so this validates the declaration that will
/// actually be written to the vault.
///
/// The rule is one line: at least one active, non-retrieval-only dense slot
/// whose encoder's image is a continuum. Without it, every neighbourhood
/// surface on the panel — find-similar ranking, the agreement cross-term, the
/// between-record graph, the blind-spot rule — is resolving ties rather than
/// ranking similarity, and nothing in the returned numbers says so.
/// `syn-timeline-v1 @ 1900001` was in exactly that state: five dense lenses,
/// all of them one-hot, periodic, or `dim = 1`, all returning nearest-neighbour
/// cosine exactly 1.0 for all 924 records.
///
/// This is a *possibility* check, deliberately. Whether a graded lens actually
/// grades over a given corpus is a measurement (`calyx_loom::
/// SimilarityDiscrimination`) that no compile-time declaration can make; what
/// this refuses is the case where the answer is no before any data exists.
///
/// Public so the gate itself is verifiable: a fail-closed rule that cannot be
/// driven from a harness is a rule nobody has watched fail.
///
/// # Errors
///
/// Returns a structured error when an active content slot has no declared
/// runtime grading or when the panel carries no graded dense lens.
pub fn assert_panel_carries_graded_dense_lens(
    panel_version: u32,
    slots: &[Slot],
    registry: &Registry,
) -> StorageResult<()> {
    let mut graded = Vec::new();
    let mut undeclared = Vec::new();
    let mut summary = Vec::new();
    for slot in slots {
        if slot.state != SlotState::Active || slot.retrieval_only {
            continue;
        }
        let Some(spec) = registry.lens_spec(slot.lens_id) else {
            undeclared.push(format!("{}: no persisted LensSpec", slot.slot_id));
            continue;
        };
        let LensRuntime::Algorithmic { kind } = &spec.runtime else {
            // A model-backed embedder's image is a continuum by construction;
            // there is no static declaration to consult and none is needed.
            graded.push(slot.slot_id.get());
            continue;
        };
        let Some(encoder) = calyx_registry::algorithmic_encoder(kind, spec.output) else {
            undeclared.push(format!("{}: unparseable runtime kind {kind}", slot.slot_id));
            continue;
        };
        let grading = encoder.dense_cosine_grading();
        summary.push(format!("{}={}", slot.slot_id, grading.as_str()));
        if grading.is_graded() {
            graded.push(slot.slot_id.get());
        }
    }
    if !undeclared.is_empty() {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_SLOT_GRADING_UNDECLARED",
            &format!(
                "panel {panel_version} has slots whose cosine grading cannot be read from their \
                 persisted spec: {undeclared:?}"
            ),
            "register every built-in slot through syn_content_slot/syn_retrieval_only_slot so its \
             runtime kind round-trips",
        ));
    }
    if graded.is_empty() {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_NO_GRADED_DENSE_LENS",
            &format!(
                "panel {panel_version} carries no dense content slot whose cosine can grade, so no \
                 neighbourhood analysis on it can rank: every candidate ties (#1963). \
                 per-slot grading: {summary:?}"
            ),
            "add a graded dense lens (syn_record_vector over scale-normalized fields is the \
             weightless option) before publishing this panel",
        ));
    }
    Ok(())
}

/// Builds one authoritative `Active`, content panel slot.
///
/// The frozen lens is the same one the ingest path measures with, so the slot's
/// `lens_id`, `shape`, and `modality` are not reconstructed.
///
/// **Fail-closed on a constant cosine lane (#1963).** A content slot is, by
/// definition, offered to every similarity surface: find-similar ranking, the
/// agreement cross-term, the between-record graph, the blind-spot rule, the
/// guard's per-slot threshold. All of them read one number — the cosine between
/// two slot vectors — and for some encoders that number is provably the same
/// for every pair of records, whatever the corpus. `syn.timeline.
/// event_time_rank.v1` was one: a dense `dim = 1` lane whose image is `[0, 1]`
/// has cosine identically `+1`. Such a lane is a legitimate *scalar*, and
/// nothing here says otherwise — it is only refused as a **content** slot, and
/// [`syn_retrieval_only_slot`] is the declaration that keeps it.
///
/// Public so the refusal is verifiable from a harness.
///
/// # Errors
///
/// Returns a structured error when the encoder cannot grade cosine, is constant,
/// has a magnitude-weighted record-vector contract, or cannot be frozen exactly.
pub fn syn_content_slot(
    slot_id: SlotId,
    slot_key: &str,
    lens: RegistryAlgorithmicLens,
    panel_version: u32,
    registry: &mut Registry,
) -> StorageResult<Slot> {
    reject_magnitude_weighted_record_vector(slot_id, slot_key, &lens, panel_version)?;
    let grading = lens.encoder().dense_cosine_grading();
    if grading.is_constant() {
        return Err(panel_lifecycle_error(
            "CALYX_PANEL_SLOT_COSINE_CONSTANT",
            &format!(
                "content slot {slot_id} key {slot_key} uses encoder {:?}, whose dense cosine is \
                 identically +1 for every pair of records: as a similarity lane it carries zero \
                 information by construction, not by corpus (#1963)",
                lens.encoder()
            ),
            "declare the slot with syn_retrieval_only_slot if it is a post-retrieval ordinate, \
             or give it a graded encoding (syn_scalar_rank_arc places a bounded scalar on the \
             unit half-circle)",
        ));
    }
    syn_slot(slot_id, slot_key, lens, panel_version, registry, false)
}

/// Panel generations that still carry a legacy magnitude-weighted
/// `syn_record_vector` content slot, each with the row count #1964 measured as
/// tied at nearest-neighbour cosine `1.000000`.
///
/// **This list may only shrink.** It is not a permission to keep writing the
/// defect; it names the generations that already have rows measured with it and
/// cannot be re-measured, because their panel declares
/// `backfill_source_cf: None` — there is no path that can rebuild those
/// constellations, so bumping the generation would strand every row on a
/// version nothing reads, permanently. Adding a backfill path is what removes
/// an entry from here; #1965 tracks that work.
///
/// A generation *not* in this list may not use `SynRecordVector` for a content
/// slot at all, which is what makes the #1964 defect unreachable for anything
/// new.
const RECORD_VECTOR_MAGNITUDE_GRANDFATHERED: &[(u32, &str, u64)] = &[
    (1_983_001, "syn-agent-event-v1", 9_091),
    (1_983_002, "syn-agent-transcript-v1", 50_973),
    (1_776_005, "syn-outcome-v1", 18),
    (1_776_006, "syn-mcp-usage-v1", 9_842),
];

/// Refuses a content slot whose record vector weights fields by raw magnitude
/// (#1964).
///
/// `syn_record_vector` places each numeric field at `hash(path) % dim`,
/// multiplies by the field's **raw value**, and unit-normalizes. Whichever field
/// carries the largest units therefore owns the direction. Nine built-in lanes
/// fed a unix-millisecond timestamp (~1.7e12) beside counts and flags in
/// `0..1e5`, which leaves every other field ~1e-9 of the norm — nine orders of
/// magnitude below `f32::EPSILON`, so those fields are not merely small in the
/// cosine, they are unrepresentable in it. Measured over the stored slot vectors
/// on the live vault, all nine returned nearest-neighbour cosine exactly
/// `1.000000` for every record.
///
/// The encoder that replaces it, `syn_record_vector_unit_fields`, refuses any
/// field outside `[-1, 1]` at measure time. This gate is the second half: it
/// stops a *panel* from declaring the legacy encoder as a similarity lane in the
/// first place, so the defect cannot be reintroduced by a new slot that merely
/// looks like the old one.
fn reject_magnitude_weighted_record_vector(
    slot_id: SlotId,
    slot_key: &str,
    lens: &RegistryAlgorithmicLens,
    panel_version: u32,
) -> StorageResult<()> {
    if !matches!(
        lens.encoder(),
        RegistryAlgorithmicEncoder::SynRecordVector { .. }
    ) {
        return Ok(());
    }
    if let Some((_, panel, rows)) = RECORD_VECTOR_MAGNITUDE_GRANDFATHERED
        .iter()
        .find(|(version, ..)| *version == panel_version)
    {
        tracing::warn!(
            slot_id = slot_id.get(),
            slot_key,
            panel,
            panel_version,
            tied_rows = rows,
            "panel carries a magnitude-weighted record vector (#1964): its nearest-neighbour \
             cosine is 1.000000 for every stored row, so this lane cannot rank. It is \
             grandfathered only because the panel has no backfill path (#1965)"
        );
        return Ok(());
    }
    Err(panel_lifecycle_error(
        "CALYX_PANEL_RECORD_VECTOR_MAGNITUDE_WEIGHTED",
        &format!(
            "content slot {slot_id} key {slot_key} on panel {panel_version} uses \
             syn_record_vector, which weights every numeric field by its raw magnitude and then \
             unit-normalizes: the field with the largest units owns the direction and the rest \
             fall below f32 resolution. Measured on the live vault, all nine built-in lanes doing \
             this returned nearest-neighbour cosine 1.000000 for every record (#1964)"
        ),
        "measure with syn_record_vector_unit_fields and place every field on a comparable scale \
         in [-1, 1] first — a timestamp as a day/week fraction, a count or byte length as \
         ln(1+n)/ln(1+scale), a part as a ratio of its whole",
    ))
}

/// Builds one `Active`, **retrieval-only** panel slot (#1963).
///
/// Calyx's `retrieval_only` means "consumed after retrieval, never a primary
/// recall lane" — `Slot::measurable_for` excludes it and the dedup signature
/// skips it. That is the honest classification for a recency ordinate like
/// `syn.timeline.event_time_rank.v1`: it is an exact, auditable rank that
/// belongs in a temporal boost, and it is not a similarity measurement.
fn syn_retrieval_only_slot(
    slot_id: SlotId,
    slot_key: &str,
    lens: RegistryAlgorithmicLens,
    panel_version: u32,
    registry: &mut Registry,
) -> StorageResult<Slot> {
    syn_slot(slot_id, slot_key, lens, panel_version, registry, true)
}

fn syn_slot(
    slot_id: SlotId,
    slot_key: &str,
    lens: RegistryAlgorithmicLens,
    panel_version: u32,
    registry: &mut Registry,
    retrieval_only: bool,
) -> StorageResult<Slot> {
    let contract = lens.contract().clone();
    let output = contract.shape();
    let modality = contract.modality();
    let spec = LensSpec {
        name: slot_key.to_owned(),
        runtime: LensRuntime::Algorithmic {
            kind: persisted_syn_runtime_kind(lens.encoder())?,
        },
        output: contract.shape(),
        modality: contract.modality(),
        weights_sha256: contract.weights_sha256(),
        corpus_hash: contract.corpus_hash(),
        norm_policy: contract.norm_policy(),
        max_batch: None,
        axis: Some(slot_key.to_owned()),
        asymmetry: Asymmetry::None,
        quant_default: QuantPolicy::None,
        truncate_dim: None,
        recall_delta: default_recall_delta(),
        retrieval_only,
        excluded_from_dedup: retrieval_only,
    };
    let lens_id = registry
        .register_frozen_with_spec(lens, contract, spec)
        .map_err(|error| {
            panel_lifecycle_error(
                "CALYX_PANEL_REGISTRY_INVALID",
                &format!("register active slot {slot_id} key {slot_key}: {error}"),
                "fix the static panel lens/runtime contract before publishing the active panel",
            )
        })?;
    Ok(Slot {
        slot_id,
        slot_key: SlotKey::new(slot_id, slot_key),
        lens_id,
        shape: output,
        modality,
        asymmetry: Asymmetry::None,
        quant: QuantPolicy::None,
        resource: SlotResource::default(),
        axis: None,
        retrieval_only,
        excluded_from_dedup: retrieval_only,
        bits_about: BTreeMap::new(),
        state: SlotState::Active,
        added_at_panel_version: panel_version,
    })
}

fn persisted_syn_runtime_kind(encoder: RegistryAlgorithmicEncoder) -> StorageResult<String> {
    let kind = match encoder {
        RegistryAlgorithmicEncoder::SynCyclicTime { period } => {
            format!("syn_cyclic_time:{period}")
        }
        RegistryAlgorithmicEncoder::SynScalarRaw => "syn_scalar_raw".to_owned(),
        RegistryAlgorithmicEncoder::SynScalarLog1p => "syn_scalar_log1p".to_owned(),
        RegistryAlgorithmicEncoder::SynScalarZScore {
            mean_micros,
            std_micros,
        } => format!("syn_scalar_zscore:{mean_micros}:{std_micros}"),
        RegistryAlgorithmicEncoder::SynScalarRank {
            min_micros,
            max_micros,
        } => format!("syn_scalar_rank:{min_micros}:{max_micros}"),
        RegistryAlgorithmicEncoder::SynScalarRankArc {
            min_micros,
            max_micros,
        } => format!("syn_scalar_rank_arc:{min_micros}:{max_micros}"),
        RegistryAlgorithmicEncoder::SynOneHot { buckets } => {
            format!("syn_one_hot:{buckets}")
        }
        RegistryAlgorithmicEncoder::SynOneHotIndex { levels } => {
            format!("syn_one_hot_index:{levels}")
        }
        RegistryAlgorithmicEncoder::SynHash { dim } => format!("syn_hash:{dim}"),
        RegistryAlgorithmicEncoder::SynSparseText { dim } => {
            format!("syn_sparse_text:{dim}")
        }
        RegistryAlgorithmicEncoder::SynSparseTextTf { dim } => {
            format!("syn_sparse_text_tf:{dim}")
        }
        RegistryAlgorithmicEncoder::SynTokenSlots { token_dim } => {
            format!("syn_token_slots:{token_dim}")
        }
        RegistryAlgorithmicEncoder::SynMultiHot { dim } => format!("syn_multi_hot:{dim}"),
        RegistryAlgorithmicEncoder::SynRecordVector { dim } => {
            format!("syn_record_vector:{dim}")
        }
        RegistryAlgorithmicEncoder::SynRecordVectorUnitFields { dim } => {
            format!("syn_record_vector_unit_fields:{dim}")
        }
        RegistryAlgorithmicEncoder::SynBin {
            buckets,
            min_micros,
            max_micros,
        } => format!("syn_bin:{buckets}:{min_micros}:{max_micros}"),
        RegistryAlgorithmicEncoder::SynOrdinal { levels } => format!("syn_ordinal:{levels}"),
        RegistryAlgorithmicEncoder::SynFrequency { count, total } => {
            format!("syn_frequency:{count}:{total}")
        }
        RegistryAlgorithmicEncoder::SynTargetMean {
            mean_micros,
            fold_count,
            outcome_hash,
        } => format!("syn_target_mean:{mean_micros}:{fold_count}:{outcome_hash}"),
        RegistryAlgorithmicEncoder::SynDelta { scale_micros } => {
            format!("syn_delta:{scale_micros}")
        }
        RegistryAlgorithmicEncoder::SynRate { scale_micros } => {
            format!("syn_rate:{scale_micros}")
        }
        RegistryAlgorithmicEncoder::SynCross { dim } => format!("syn_cross:{dim}"),
        RegistryAlgorithmicEncoder::SynAggregation { dim } => {
            format!("syn_aggregation:{dim}")
        }
        RegistryAlgorithmicEncoder::SynGraphSignature { snapshot } => {
            format!("syn_graph_signature:{snapshot}")
        }
        RegistryAlgorithmicEncoder::SynPathSignature { snapshot } => {
            format!("syn_path_signature:{snapshot}")
        }
        other => {
            return Err(panel_lifecycle_error(
                "CALYX_PANEL_REGISTRY_INVALID",
                &format!("static Synapse panel contains non-Syn encoder {other:?}"),
                "declare a reconstructable Syn* runtime for every static Synapse panel lens",
            ));
        }
    };
    Ok(kind)
}

/// The built-in contract for the agent-transcript panel (#1668).
///
/// Mirrors [`build_agent_transcript_constellation`] one slot at a time, in slot
/// order. The two must agree on lens name, kind and dimension or the lens ids
/// will not match what ingest measured with — the contract is a *lookup* of the
/// same frozen contracts, never a reconstruction from stored rows.
///
/// Declared for `SYN_AGENT_TRANSCRIPT_PANEL_VERSION` only. The superseded
/// generations (`..._PRE_1921` = 1904003, `..._PRE_1904` = 1665002) carry
/// different slot sets and are deliberately absent: `syn_reconstructable_panel_contract`
/// returns `None` for them rather than validating their rows against this map.
///
/// This is the largest corpus on the vault — 26,049 records at 1.0 coverage,
/// measured 2026-07-31 — and it is not the durable active panel, so before #1668
/// nothing could build it a search generation or query one.
///
/// Note slots 107 and 109: both are `syn_sparse_text_tf`, the term-frequency
/// lanes BM25 ranks on, which is what makes this panel reachable by
/// `query_mode=by_text` and not only by example.
#[allow(
    clippy::too_many_lines,
    reason = "agent transcript panel contract is a one-to-one slot-to-frozen-lens map mirroring build_agent_transcript_constellation; splitting would obscure the stable contract"
)]
fn agent_transcript_panel_slots(
    panel_version: u32,
    registry: &mut Registry,
) -> StorageResult<Vec<Slot>> {
    Ok(vec![
        syn_content_slot(
            AT_SLOT_ROLE_ONEHOT,
            "syn.agent_transcript.role_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.agent_transcript.role_onehot.v1",
                Modality::Structured,
                8,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_STATUS_ONEHOT,
            "syn.agent_transcript.status_onehot.v2",
            RegistryAlgorithmicLens::syn_one_hot_index(
                "syn.agent_transcript.status_onehot.v2",
                Modality::Structured,
                2,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_SOURCE_ONEHOT,
            "syn.agent_transcript.source_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.agent_transcript.source_onehot.v1",
                Modality::Structured,
                16,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_EVENT_KIND_HASH,
            "syn.agent_transcript.event_kind_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_transcript.event_kind_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_MODEL_HASH,
            "syn.agent_transcript.model_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_transcript.model_hash.v1",
                Modality::Structured,
                1024,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_TEXT_SPARSE,
            "syn.agent_transcript.text_sparse.v1",
            RegistryAlgorithmicLens::syn_sparse_text(
                "syn.agent_transcript.text_sparse.v1",
                Modality::Structured,
                4096,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_TOOL_HASH,
            "syn.agent_transcript.tool_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.agent_transcript.tool_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        // Retrieval-only since #1963: a dense `dim = 1` rank on `[0, 1]` has
        // cosine identically +1, so it is a post-retrieval ordinate and never a
        // similarity lane. `syn_content_slot` refuses it structurally.
        syn_retrieval_only_slot(
            AT_SLOT_LINE_RANK,
            "syn.agent_transcript.line_rank.v1",
            RegistryAlgorithmicLens::syn_scalar_rank(
                "syn.agent_transcript.line_rank.v1",
                Modality::Structured,
                0,
                AT_LINE_RANK_MAX_LINES_MICROS,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_INPUT_TOKENS_LOG1P,
            "syn.agent_transcript.input_tokens_log1p.v1",
            RegistryAlgorithmicLens::syn_scalar_log1p(
                "syn.agent_transcript.input_tokens_log1p.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_OUTPUT_TOKENS_LOG1P,
            "syn.agent_transcript.output_tokens_log1p.v1",
            RegistryAlgorithmicLens::syn_scalar_log1p(
                "syn.agent_transcript.output_tokens_log1p.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_CACHE_READ_LOG1P,
            "syn.agent_transcript.cache_read_log1p.v1",
            RegistryAlgorithmicLens::syn_scalar_log1p(
                "syn.agent_transcript.cache_read_log1p.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_CACHE_CREATION_LOG1P,
            "syn.agent_transcript.cache_creation_log1p.v1",
            RegistryAlgorithmicLens::syn_scalar_log1p(
                "syn.agent_transcript.cache_creation_log1p.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_RECORD_VECTOR_V2,
            "syn.agent_transcript.record_vector.v2",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.agent_transcript.record_vector.v2",
                Modality::Structured,
                96,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_TEXT_BM25,
            "syn.agent_transcript.text_bm25.v1",
            RegistryAlgorithmicLens::syn_sparse_text_tf(
                "syn.agent_transcript.text_bm25.v1",
                Modality::Structured,
                AT_TEXT_BM25_DIM,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            AT_SLOT_TEXT_FULL_BM25,
            "syn.agent_transcript.text_full_bm25.v1",
            RegistryAlgorithmicLens::syn_sparse_text_tf(
                "syn.agent_transcript.text_full_bm25.v1",
                Modality::Structured,
                AT_TEXT_FULL_BM25_DIM,
            ),
            panel_version,
            registry,
        )?,
    ])
}

#[expect(
    clippy::too_many_lines,
    reason = "the frozen timeline panel's complete ordered slot declarations are one schema contract"
)]
fn timeline_panel_slots(panel_version: u32, registry: &mut Registry) -> StorageResult<Vec<Slot>> {
    Ok(vec![
        syn_content_slot(
            TL_SLOT_KIND_ONEHOT,
            "syn.timeline.kind_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.timeline.kind_onehot.v1",
                Modality::Structured,
                32,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            TL_SLOT_APP_HASH,
            "syn.timeline.app_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.timeline.app_hash.v1",
                Modality::Structured,
                1024,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            TL_SLOT_TITLE_SPARSE,
            "syn.timeline.title_sparse.v1",
            RegistryAlgorithmicLens::syn_sparse_text(
                "syn.timeline.title_sparse.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            TL_SLOT_HOUR_CYCLIC,
            "syn.timeline.hour_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.timeline.hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            TL_SLOT_DOW_CYCLIC,
            "syn.timeline.dow_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.timeline.dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            TL_SLOT_ACTOR_ONEHOT,
            "syn.timeline.actor_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.timeline.actor_onehot.v1",
                Modality::Structured,
                8,
            ),
            panel_version,
            registry,
        )?,
        // Retrieval-only since #1963: a dense `dim = 1` rank on `[0, 1]` has
        // cosine identically +1, so it is a post-retrieval recency ordinate and
        // never a similarity lane. `syn_content_slot` refuses it structurally.
        syn_retrieval_only_slot(
            TL_SLOT_RECENCY_RANK,
            "syn.timeline.event_time_rank.v1",
            RegistryAlgorithmicLens::syn_scalar_rank(
                "syn.timeline.event_time_rank.v1",
                Modality::Structured,
                0,
                RECENCY_RANK_MAX_UNIX_MS_MICROS,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            TL_SLOT_TITLE_BM25,
            "syn.timeline.title_bm25.v1",
            RegistryAlgorithmicLens::syn_sparse_text_tf(
                "syn.timeline.title_bm25.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        // The panel's graded dense lens (#1963).
        syn_content_slot(
            TL_SLOT_RECORD_VECTOR,
            "syn.timeline.record_vector.v1",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.timeline.record_vector.v1",
                Modality::Structured,
                TL_RECORD_VECTOR_DIM,
            ),
            panel_version,
            registry,
        )?,
    ])
}

#[allow(
    clippy::too_many_lines,
    reason = "episode panel contract is a one-to-one slot-to-frozen-lens map mirroring build_episode_constellation; splitting would obscure the stable contract"
)]
fn episode_panel_slots(panel_version: u32, registry: &mut Registry) -> StorageResult<Vec<Slot>> {
    Ok(vec![
        syn_content_slot(
            EP_SLOT_APP_HASH,
            "syn.episode.app_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.episode.app_hash.v1",
                Modality::Structured,
                1024,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_DOCUMENT_HASH,
            "syn.episode.document_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.episode.document_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_URL_HOST_HASH,
            "syn.episode.url_host_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.episode.url_host_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_TITLE_SPARSE,
            "syn.episode.title_sparse.v1",
            RegistryAlgorithmicLens::syn_sparse_text(
                "syn.episode.title_sparse.v1",
                Modality::Structured,
                4096,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_TITLE_BM25,
            "syn.episode.title_bm25.v1",
            RegistryAlgorithmicLens::syn_sparse_text_tf(
                "syn.episode.title_bm25.v1",
                Modality::Structured,
                EP_TITLE_BM25_DIM,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_START_HOUR_CYCLIC,
            "syn.episode.start_hour_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.episode.start_hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_START_DOW_CYCLIC,
            "syn.episode.start_dow_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.episode.start_dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_DURATION_LOG1P,
            "syn.episode.duration_log1p.v1",
            RegistryAlgorithmicLens::syn_scalar_log1p(
                "syn.episode.duration_log1p.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        // Retrieval-only since #1963: a dense `dim = 1` rank on `[0, 1]` has
        // cosine identically +1, so it is a post-retrieval ordinate and never a
        // similarity lane. `syn_content_slot` refuses it structurally.
        syn_retrieval_only_slot(
            EP_SLOT_DURATION_RANK,
            "syn.episode.duration_rank.v1",
            RegistryAlgorithmicLens::syn_scalar_rank(
                "syn.episode.duration_rank.v1",
                Modality::Structured,
                0,
                MAX_DAY_DURATION_MS_MICROS,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_KEYSTROKES_ZSCORE,
            "syn.episode.keystrokes_zscore.v1",
            RegistryAlgorithmicLens::syn_scalar_zscore(
                "syn.episode.keystrokes_zscore.v1",
                Modality::Structured,
                0,
                10_000_000,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_CLICKS_ZSCORE,
            "syn.episode.clicks_zscore.v1",
            RegistryAlgorithmicLens::syn_scalar_zscore(
                "syn.episode.clicks_zscore.v1",
                Modality::Structured,
                0,
                1_000_000,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_ROW_COUNT_ZSCORE,
            "syn.episode.row_count_zscore.v1",
            RegistryAlgorithmicLens::syn_scalar_zscore(
                "syn.episode.row_count_zscore.v1",
                Modality::Structured,
                0,
                1_000_000,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_STARTED_BOUNDARY_ONEHOT,
            "syn.episode.started_boundary_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.episode.started_boundary_onehot.v1",
                Modality::Structured,
                16,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_ENDED_BOUNDARY_ONEHOT,
            "syn.episode.ended_boundary_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.episode.ended_boundary_onehot.v1",
                Modality::Structured,
                16,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_INTERRUPTION_RATIO,
            "syn.episode.interruption_ratio_raw.v1",
            RegistryAlgorithmicLens::syn_scalar_raw(
                "syn.episode.interruption_ratio_raw.v1",
                Modality::Structured,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            EP_SLOT_RECORD_VECTOR,
            "syn.episode.record_vector.v2",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.episode.record_vector.v2",
                Modality::Structured,
                EP_RECORD_VECTOR_DIM,
            ),
            panel_version,
            registry,
        )?,
    ])
}

/// The built-in contract for the MCP-usage panel (#1919).
///
/// This panel is not, and is not intended to become, the durable *active*
/// panel — the active panel serves recall and is the timeline. It is declared
/// here because it is the only fully-grounded outcome-bearing corpus this vault
/// has: measured 2026-07-31 on the live vault, `syn-mcp-usage-v1 @ 1776006`
/// holds **1,100 records at `grounded_fraction` 1.0000, `provisional=false`**,
/// every one carrying a `synapse:mcp_tool_call_outcome` anchor, which
/// `SYNAPSE_DECLARED_ENUM_ADJUDICATIONS` splits into good (`ok`) and bad.
///
/// Ward's conformal calibration needs exactly two things: an adjudicated corpus
/// in both polarities, and a *panel definition* to validate the named slots
/// against. It had the first here and not the second, because a vault manifest
/// publishes exactly one `Panel` snapshot and this is not it. That, not any
/// shortage of anchors, is what made the guard uncertifiable (#1919).
///
/// Reconstructing the definition from code is sound because these panels are
/// code-declared and content-addressed: the lens ids this produces are a hash of
/// the same frozen contracts the ingest path measured with, so a slot validated
/// here is the same slot that was written. It is not a substitute for the
/// panel-lifecycle work in #1668, which owns making a second panel *servable*;
/// it is the minimum needed to stop the guard being blocked by a definition
/// lookup rather than by evidence.
#[allow(
    clippy::too_many_lines,
    reason = "a panel contract is a one-to-one slot-to-frozen-lens map; splitting it would hide the slot set it exists to declare"
)]
fn mcp_usage_panel_slots(panel_version: u32, registry: &mut Registry) -> StorageResult<Vec<Slot>> {
    Ok(vec![
        syn_content_slot(
            MU_SLOT_TOOL_ONEHOT,
            "syn.mcp_usage.tool_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.mcp_usage.tool_onehot.v1",
                Modality::Structured,
                128,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_OPERATION_ONEHOT,
            "syn.mcp_usage.operation_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.mcp_usage.operation_onehot.v1",
                Modality::Structured,
                128,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_ROUTE_HASH,
            "syn.mcp_usage.route_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.mcp_usage.route_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_PARAM_SHAPE_HASH,
            "syn.mcp_usage.param_shape_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.mcp_usage.param_shape_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_STATUS_ONEHOT,
            "syn.mcp_usage.status_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.mcp_usage.status_onehot.v1",
                Modality::Structured,
                64,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_ERROR_ONEHOT,
            "syn.mcp_usage.error_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.mcp_usage.error_onehot.v1",
                Modality::Structured,
                128,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_PROFILE_HASH,
            "syn.mcp_usage.profile_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.mcp_usage.profile_hash.v1",
                Modality::Structured,
                1024,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_SURFACE_HASH,
            "syn.mcp_usage.tool_surface_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.mcp_usage.tool_surface_hash.v1",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        // Retrieval-only since #1963: a dense `dim = 1` rank on `[0, 1]` has
        // cosine identically +1, so it is a post-retrieval ordinate and never a
        // similarity lane. `syn_content_slot` refuses it structurally.
        syn_retrieval_only_slot(
            MU_SLOT_SESSION_SEQUENCE_RANK,
            "syn.mcp_usage.session_sequence_rank.v1",
            RegistryAlgorithmicLens::syn_scalar_rank(
                "syn.mcp_usage.session_sequence_rank.v1",
                Modality::Structured,
                0,
                RECENCY_RANK_MAX_UNIX_MS_MICROS,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_HOUR_CYCLIC,
            "syn.mcp_usage.hour_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.mcp_usage.hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_DOW_CYCLIC,
            "syn.mcp_usage.dow_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.mcp_usage.dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            MU_SLOT_RECORD_VECTOR_V2,
            "syn.mcp_usage.record_vector.v2",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.mcp_usage.record_vector.v2",
                Modality::Structured,
                128,
            ),
            panel_version,
            registry,
        )?,
    ])
}

/// Build the Calyx constellation for an agent event record.
///
/// # Errors
///
/// Returns an error when enum serialization, lens measurement, JSON encoding,
/// or exact integer scalar conversion fails.
#[allow(
    clippy::too_many_lines,
    reason = "agent event constellation construction is a one-to-one field-to-slot map; splitting would obscure the stable slot contract"
)]
pub fn build_agent_event_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &AgentEventRecord,
) -> StorageResult<Constellation> {
    let mut slots = BTreeMap::new();
    let operation_name = optional_gen_ai_operation_name(record.attributes.operation_name)?;
    slots.insert(
        AE_SLOT_KIND_ONEHOT,
        measure_text(
            SYN_AGENT_EVENT_PANEL_NAME,
            AlgorithmicLens::syn_one_hot(
                "syn.agent_event.kind_onehot.v1",
                Modality::Structured,
                32,
            ),
            &agent_event_kind_name(record.kind)?,
        )?,
    );
    slots.insert(
        AE_SLOT_OPERATION_ONEHOT,
        optional_onehot_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.operation_onehot.v1",
            operation_name.as_deref(),
            16,
        )?,
    );
    slots.insert(
        AE_SLOT_PROVIDER_HASH,
        optional_hash_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.provider_hash.v1",
            record.attributes.provider_name.as_deref(),
            512,
        )?,
    );
    slots.insert(
        AE_SLOT_REQUEST_MODEL_HASH,
        optional_hash_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.request_model_hash.v1",
            record.attributes.request_model.as_deref(),
            1024,
        )?,
    );
    slots.insert(
        AE_SLOT_RESPONSE_MODEL_HASH,
        optional_hash_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.response_model_hash.v1",
            record.attributes.response_model.as_deref(),
            1024,
        )?,
    );
    slots.insert(
        AE_SLOT_TOOL_HASH,
        optional_hash_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.tool_hash.v1",
            record.attributes.tool_name.as_deref(),
            2048,
        )?,
    );
    slots.insert(
        AE_SLOT_ERROR_ONEHOT,
        optional_onehot_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.error_onehot.v1",
            record.attributes.error_type.as_deref(),
            64,
        )?,
    );
    slots.insert(
        AE_SLOT_END_STATE_ONEHOT,
        optional_onehot_index_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.end_state_onehot.v2",
            record.end_state.map(agent_end_state_index),
            3,
        )?,
    );
    slots.insert(
        AE_SLOT_HAS_END_STATE,
        measure_number(
            SYN_AGENT_EVENT_PANEL_NAME,
            AlgorithmicLens::syn_one_hot_index(
                "syn.agent_event.has_end_state.v1",
                Modality::Structured,
                2,
            ),
            u8::from(record.end_state.is_some()),
        )?,
    );
    let (hour, dow) = utc_hour_and_dow(record.ts_ns);
    slots.insert(
        AE_SLOT_HOUR_CYCLIC,
        measure_number(
            SYN_AGENT_EVENT_PANEL_NAME,
            AlgorithmicLens::syn_cyclic_time(
                "syn.agent_event.hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            hour,
        )?,
    );
    slots.insert(
        AE_SLOT_DOW_CYCLIC,
        measure_number(
            SYN_AGENT_EVENT_PANEL_NAME,
            AlgorithmicLens::syn_cyclic_time(
                "syn.agent_event.dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            dow,
        )?,
    );
    slots.insert(
        AE_SLOT_USAGE_TOTAL_LOG1P,
        optional_log1p_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.usage_total_log1p.v1",
            agent_event_usage_total(record),
        )?,
    );
    let scalars = agent_event_scalars(record, raw_bytes)?;
    let metadata = agent_event_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_AGENT_EVENT_PANEL_VERSION,
        source_pointer(cf::CF_AGENT_EVENTS, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for an agent transcript record.
///
/// # Errors
///
/// Returns an error when enum serialization, lens measurement, JSON encoding,
/// or exact integer scalar conversion fails.
#[allow(
    clippy::too_many_lines,
    reason = "agent transcript constellation construction is a one-to-one field-to-slot map; splitting would obscure the stable slot contract"
)]
pub fn build_agent_transcript_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &AgentTranscriptRecord,
) -> StorageResult<Constellation> {
    let mut slots = BTreeMap::new();
    let role_name = optional_transcript_role_name(record.role)?;
    slots.insert(
        AT_SLOT_ROLE_ONEHOT,
        optional_onehot_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.role_onehot.v1",
            role_name.as_deref(),
            8,
        )?,
    );
    slots.insert(
        AT_SLOT_STATUS_ONEHOT,
        measure_text(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            AlgorithmicLens::syn_one_hot_index(
                "syn.agent_transcript.status_onehot.v2",
                Modality::Structured,
                2,
            ),
            &transcript_parse_status_index(record.status).to_string(),
        )?,
    );
    slots.insert(
        AT_SLOT_SOURCE_ONEHOT,
        measure_text(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            AlgorithmicLens::syn_one_hot(
                "syn.agent_transcript.source_onehot.v1",
                Modality::Structured,
                16,
            ),
            &transcript_source_name(record.source)?,
        )?,
    );
    slots.insert(
        AT_SLOT_EVENT_KIND_HASH,
        optional_hash_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.event_kind_hash.v1",
            record.event_kind.as_deref(),
            2048,
        )?,
    );
    slots.insert(
        AT_SLOT_MODEL_HASH,
        optional_hash_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.model_hash.v1",
            record.model.as_deref(),
            1024,
        )?,
    );
    let transcript_text_value = transcript_text(record);
    // #1924: the three text lanes are the only slots on this panel whose input
    // is unbounded — every field is capped at ingest, but the *number* of tool
    // calls on a row is not, so a turn issuing many large calls is the case that
    // crosses `MAX_TEXT_TOKENS`. They measure through `measure_text_or_absent`,
    // which leaves the refusing lane `Absent{Error}` and keeps the other twelve
    // slots. Every other slot below still uses `?`: their inputs are bounded by
    // construction, so a refusal there would be a defect, not a long row.
    slots.insert(
        AT_SLOT_TEXT_SPARSE,
        measure_text_or_absent(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text(
                "syn.agent_transcript.text_sparse.v1",
                Modality::Structured,
                4096,
            ),
            &transcript_text_value,
        )?,
    );
    slots.insert(
        AT_SLOT_TEXT_BM25,
        measure_text_or_absent(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text_tf(
                "syn.agent_transcript.text_bm25.v1",
                Modality::Structured,
                AT_TEXT_BM25_DIM,
            ),
            &transcript_text_value,
        )?,
    );
    slots.insert(
        AT_SLOT_TEXT_FULL_BM25,
        measure_text_or_absent(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            &AlgorithmicLens::syn_sparse_text_tf(
                "syn.agent_transcript.text_full_bm25.v1",
                Modality::Structured,
                AT_TEXT_FULL_BM25_DIM,
            ),
            &transcript_full_text(record),
        )?,
    );
    let transcript_tool_names = transcript_tool_names(record);
    slots.insert(
        AT_SLOT_TOOL_HASH,
        optional_hash_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.tool_hash.v1",
            Some(transcript_tool_names.as_str()),
            2048,
        )?,
    );
    slots.insert(
        AT_SLOT_LINE_RANK,
        measure_scalar_rank(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.line_rank.v1",
            0,
            AT_LINE_RANK_MAX_LINES_MICROS,
            record.line_no,
        )?,
    );
    slots.insert(
        AT_SLOT_INPUT_TOKENS_LOG1P,
        optional_log1p_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.input_tokens_log1p.v1",
            record.usage.as_ref().and_then(|usage| usage.input_tokens),
        )?,
    );
    slots.insert(
        AT_SLOT_OUTPUT_TOKENS_LOG1P,
        optional_log1p_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.output_tokens_log1p.v1",
            record.usage.as_ref().and_then(|usage| usage.output_tokens),
        )?,
    );
    slots.insert(
        AT_SLOT_CACHE_READ_LOG1P,
        optional_log1p_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.cache_read_log1p.v1",
            record
                .usage
                .as_ref()
                .and_then(|usage| usage.cache_read_input_tokens),
        )?,
    );
    slots.insert(
        AT_SLOT_CACHE_CREATION_LOG1P,
        optional_log1p_slot(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            "syn.agent_transcript.cache_creation_log1p.v1",
            record
                .usage
                .as_ref()
                .and_then(|usage| usage.cache_creation_input_tokens),
        )?,
    );
    slots.insert(
        AT_SLOT_RECORD_VECTOR_V2,
        measure_json(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            AlgorithmicLens::syn_record_vector_unit_fields(
                "syn.agent_transcript.record_vector.v2",
                Modality::Structured,
                96,
            ),
            &agent_transcript_numeric_record_v2(record),
        )?,
    );

    let scalars = agent_transcript_scalars(record, raw_bytes)?;
    let metadata = agent_transcript_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        source_pointer(cf::CF_AGENT_TRANSCRIPTS, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for an action audit row.
///
/// # Errors
///
/// Returns an error when lens measurement, JSON encoding, or exact scalar
/// conversion fails.
pub fn build_action_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
) -> StorageResult<Constellation> {
    ensure_json_object(record, cf::CF_ACTION_LOG)?;
    let mut slots = BTreeMap::new();
    let ts_ns = json_u64(record, &["ts_ns"]);
    slots.insert(
        ACT_SLOT_KIND_ONEHOT,
        measure_text(
            SYN_ACTION_PANEL_NAME,
            AlgorithmicLens::syn_one_hot("syn.action.kind_onehot.v2", Modality::Structured, 64),
            &action_identity(record),
        )?,
    );
    let target_text = action_target_text(record);
    if target_text.is_none() {
        warn_action_target_absent_on_success(source_key, record)?;
    }
    slots.insert(
        ACT_SLOT_TARGET_HASH,
        optional_hash_slot(
            SYN_ACTION_PANEL_NAME,
            "syn.action.target_hash.v2",
            target_text.as_deref(),
            2048,
        )?,
    );
    slots.insert(
        ACT_SLOT_TARGET_VECTOR,
        action_target_vector_slot(source_key, record)?,
    );
    let (request_vector, request_size_class, request_shape_class) =
        action_request_slots(source_key, record)?;
    slots.insert(ACT_SLOT_REQUEST_VECTOR, request_vector);
    slots.insert(ACT_SLOT_REQUEST_SIZE_CLASS, request_size_class);
    slots.insert(ACT_SLOT_REQUEST_SHAPE_CLASS, request_shape_class);
    slots.insert(
        ACT_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_ACTION_PANEL_NAME,
            AlgorithmicLens::syn_record_vector_unit_fields(
                "syn.action.record_vector.v2",
                Modality::Structured,
                32,
            ),
            &action_numeric_record(record),
        )?,
    );
    insert_time_slots(
        &mut slots,
        SYN_ACTION_PANEL_NAME,
        ACT_SLOT_HOUR_CYCLIC,
        ACT_SLOT_DOW_CYCLIC,
        "syn.action.hour_cyclic.v1",
        "syn.action.dow_cyclic.v1",
        ts_ns,
    )?;

    let scalars = action_scalars(record, raw_bytes)?;
    let metadata = action_metadata(source_key, raw_bytes, record);
    let mut constellation = constellation(
        context,
        SYN_ACTION_PANEL_VERSION,
        source_pointer(cf::CF_ACTION_LOG, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )?;
    if let Some(anchor) = action_outcome_anchor(source_key, record)? {
        let value = match anchor.value {
            GroundingAnchorValue::Bool(value) => AnchorValue::Bool(value),
            _ => {
                return Err(StorageError::WriteFailed {
                    cf_name: cf::CF_ACTION_LOG.to_owned(),
                    detail: "action outcome adjudication emitted a non-boolean reward; remediation=repair action_outcome_anchor to preserve the declared binary contract".to_owned(),
                });
            }
        };
        constellation.anchors.push(Anchor {
            kind: AnchorKind::Reward,
            value,
            source: anchor.source,
            observed_at: anchor.observed_at_ms,
            confidence: anchor.confidence,
        });
        constellation.flags.ungrounded = false;
    }
    Ok(constellation)
}

/// Build the Calyx constellation for a reflex audit row.
///
/// # Errors
///
/// Returns an error when enum serialization, lens measurement, JSON encoding,
/// or exact scalar conversion fails.
pub fn build_reflex_audit_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &StoredReflexAudit,
) -> StorageResult<Constellation> {
    let mut slots = BTreeMap::new();
    slots.insert(
        RF_SLOT_REFLEX_HASH,
        optional_hash_slot(
            SYN_REFLEX_PANEL_NAME,
            "syn.reflex.reflex_hash.v1",
            Some(record.reflex_id.as_str()),
            2048,
        )?,
    );
    slots.insert(
        RF_SLOT_OUTCOME_ONEHOT,
        measure_text(
            SYN_REFLEX_PANEL_NAME,
            AlgorithmicLens::syn_one_hot("syn.reflex.outcome_onehot.v1", Modality::Structured, 16),
            &snake_case_name(record.status, "ReflexState")?,
        )?,
    );
    slots.insert(
        RF_SLOT_LATENCY_LOG1P,
        optional_log1p_slot(
            SYN_REFLEX_PANEL_NAME,
            "syn.reflex.latency_ms_log1p.v1",
            reflex_latency_ms(record),
        )?,
    );
    slots.insert(
        RF_SLOT_STEP_COUNT_LOG1P,
        optional_log1p_slot(
            SYN_REFLEX_PANEL_NAME,
            "syn.reflex.step_count_log1p.v1",
            Some(u64::try_from(record.steps.len()).unwrap_or(u64::MAX)),
        )?,
    );
    insert_time_slots(
        &mut slots,
        SYN_REFLEX_PANEL_NAME,
        RF_SLOT_HOUR_CYCLIC,
        RF_SLOT_DOW_CYCLIC,
        "syn.reflex.hour_cyclic.v1",
        "syn.reflex.dow_cyclic.v1",
        Some(record.ts_ns),
    )?;

    let scalars = reflex_scalars(record, raw_bytes)?;
    let metadata = reflex_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_REFLEX_PANEL_VERSION,
        source_pointer(cf::CF_REFLEX_AUDIT, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for a process-history row.
///
/// # Errors
///
/// Returns an error when lens measurement, JSON encoding, or exact scalar
/// conversion fails.
pub fn build_process_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
) -> StorageResult<Constellation> {
    ensure_json_object(record, cf::CF_PROCESS_HISTORY)?;
    let mut slots = BTreeMap::new();
    let ts_ns = process_ts_ns(record)?;
    slots.insert(
        PR_SLOT_PROCESS_HASH,
        optional_hash_slot(
            SYN_PROCESS_PANEL_NAME,
            "syn.process.process_hash.v1",
            process_identity_text(record).as_deref(),
            2048,
        )?,
    );
    slots.insert(
        PR_SLOT_EVENT_ONEHOT,
        measure_text(
            SYN_PROCESS_PANEL_NAME,
            AlgorithmicLens::syn_one_hot("syn.process.event_onehot.v1", Modality::Structured, 32),
            &process_event_kind(record),
        )?,
    );
    insert_time_slots(
        &mut slots,
        SYN_PROCESS_PANEL_NAME,
        PR_SLOT_HOUR_CYCLIC,
        PR_SLOT_DOW_CYCLIC,
        "syn.process.hour_cyclic.v1",
        "syn.process.dow_cyclic.v1",
        ts_ns,
    )?;
    slots.insert(
        PR_SLOT_UPTIME_LOG1P,
        optional_log1p_slot(
            SYN_PROCESS_PANEL_NAME,
            "syn.process.uptime_ms_log1p.v1",
            json_u64(record, &["uptime_ms", "duration_ms"]),
        )?,
    );
    slots.insert(
        PR_SLOT_RECENCY_RANK,
        optional_rank_slot(
            SYN_PROCESS_PANEL_NAME,
            "syn.process.event_time_rank.v1",
            ts_ns.map(|value| value / NS_PER_MS),
            0,
            RECENCY_RANK_MAX_UNIX_MS_MICROS,
        )?,
    );

    let scalars = process_scalars(record, raw_bytes)?;
    let metadata = process_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_PROCESS_PANEL_VERSION,
        source_pointer(cf::CF_PROCESS_HISTORY, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for a sampled observation row.
///
/// # Errors
///
/// Returns an error when enum serialization, lens measurement, JSON encoding,
/// or exact scalar conversion fails.
pub fn build_observation_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &StoredObservation,
) -> StorageResult<Constellation> {
    let mut slots = BTreeMap::new();
    slots.insert(
        OB_SLOT_APP_HASH,
        optional_hash_slot(
            SYN_OBSERVATION_PANEL_NAME,
            "syn.observation.app_hash.v1",
            Some(record.foreground.process_name.as_str()),
            2048,
        )?,
    );
    let role_histogram = observation_role_histogram(record);
    slots.insert(
        OB_SLOT_ROLE_HISTOGRAM,
        optional_json_slot(
            SYN_OBSERVATION_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.observation.role_histogram.v1",
                Modality::Structured,
                128,
            ),
            role_histogram.as_ref(),
        )?,
    );
    let entity_labels = observation_entity_labels(record);
    slots.insert(
        OB_SLOT_ENTITY_MULTI_HOT,
        optional_json_slice_slot(
            SYN_OBSERVATION_PANEL_NAME,
            AlgorithmicLens::syn_multi_hot(
                "syn.observation.entity_multi_hot.v1",
                Modality::Structured,
                2048,
            ),
            &entity_labels,
        )?,
    );
    slots.insert(
        OB_SLOT_FLAGS_MULTI_HOT,
        optional_json_slice_slot(
            SYN_OBSERVATION_PANEL_NAME,
            AlgorithmicLens::syn_multi_hot(
                "syn.observation.flags_multi_hot.v1",
                Modality::Structured,
                2048,
            ),
            &observation_flags(record)?,
        )?,
    );
    insert_time_slots(
        &mut slots,
        SYN_OBSERVATION_PANEL_NAME,
        OB_SLOT_HOUR_CYCLIC,
        OB_SLOT_DOW_CYCLIC,
        "syn.observation.hour_cyclic.v1",
        "syn.observation.dow_cyclic.v1",
        Some(record.ts_ns),
    )?;
    let scalars = observation_scalars(record, raw_bytes)?;
    let metadata = observation_metadata(source_key, raw_bytes, record)?;
    constellation(
        context,
        SYN_OBSERVATION_PANEL_VERSION,
        source_pointer(cf::CF_OBSERVATIONS, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for a persisted Synapse outcome/audit row.
///
/// # Errors
///
/// Returns an error when JSON field extraction, lens measurement, JSON
/// encoding, or exact scalar conversion fails.
pub fn build_outcome_constellation(
    context: NativeConstellationContext,
    source_cf: &'static str,
    source_key: &[u8],
    raw_bytes: &[u8],
    constellation_input_bytes: &[u8],
    record: &Value,
) -> StorageResult<Constellation> {
    ensure_json_object(record, source_cf)?;
    let mut slots = BTreeMap::new();
    slots.insert(
        OUT_SLOT_SOURCE_CF_ONEHOT,
        measure_text(
            SYN_OUTCOME_PANEL_NAME,
            AlgorithmicLens::syn_one_hot(
                "syn.outcome.source_cf_onehot.v1",
                Modality::Structured,
                64,
            ),
            source_cf,
        )?,
    );
    slots.insert(
        OUT_SLOT_EVENT_ONEHOT,
        optional_onehot_slot(
            SYN_OUTCOME_PANEL_NAME,
            "syn.outcome.event_onehot.v1",
            outcome_event(record).as_deref(),
            128,
        )?,
    );
    slots.insert(
        OUT_SLOT_STATUS_ONEHOT,
        optional_onehot_slot(
            SYN_OUTCOME_PANEL_NAME,
            "syn.outcome.status_onehot.v1",
            outcome_status(record).as_deref(),
            64,
        )?,
    );
    slots.insert(
        OUT_SLOT_TARGET_HASH,
        optional_hash_slot(
            SYN_OUTCOME_PANEL_NAME,
            "syn.outcome.target_hash.v1",
            outcome_target(record).as_deref(),
            2048,
        )?,
    );
    insert_time_slots(
        &mut slots,
        SYN_OUTCOME_PANEL_NAME,
        OUT_SLOT_HOUR_CYCLIC,
        OUT_SLOT_DOW_CYCLIC,
        "syn.outcome.hour_cyclic.v1",
        "syn.outcome.dow_cyclic.v1",
        outcome_ts_ns(record),
    )?;
    slots.insert(
        OUT_SLOT_RECORD_VECTOR_V2,
        measure_json(
            SYN_OUTCOME_PANEL_NAME,
            AlgorithmicLens::syn_record_vector_unit_fields(
                "syn.outcome.record_vector.v2",
                Modality::Structured,
                128,
            ),
            &outcome_numeric_record_v2(record, raw_bytes),
        )?,
    );

    let scalars = outcome_scalars(record, raw_bytes)?;
    let metadata = outcome_metadata(source_cf, source_key, raw_bytes, record);
    constellation(
        context,
        SYN_OUTCOME_PANEL_VERSION,
        source_pointer(source_cf, source_key),
        constellation_input_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Build the Calyx constellation for a persisted MCP usage row.
///
/// # Errors
///
/// Returns an error when the key is outside the MCP usage namespace, the row is
/// not a JSON object, JSON field extraction fails, lens measurement fails, or
/// exact scalar conversion fails.
#[allow(
    clippy::too_many_lines,
    reason = "MCP usage panel construction is a stable slot-by-slot Calyx contract"
)]
pub fn build_mcp_usage_constellation(
    context: NativeConstellationContext,
    source_key: &[u8],
    raw_bytes: &[u8],
    constellation_input_bytes: &[u8],
    record: &Value,
) -> StorageResult<Constellation> {
    ensure_json_object(record, cf::CF_KV)?;
    if !source_key.starts_with(SYN_MCP_USAGE_KEY_PREFIX) {
        return Err(StorageError::WriteFailed {
            cf_name: cf::CF_KV.to_owned(),
            detail: format!(
                "MCP usage constellation requires a source key with prefix {}; got key_hex={}",
                String::from_utf8_lossy(SYN_MCP_USAGE_KEY_PREFIX),
                hex_encode(source_key)
            ),
        });
    }
    let mut slots = BTreeMap::new();
    slots.insert(
        MU_SLOT_TOOL_ONEHOT,
        optional_onehot_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.tool_onehot.v1",
            json_string(record, &["tool"]).as_deref(),
            128,
        )?,
    );
    slots.insert(
        MU_SLOT_OPERATION_ONEHOT,
        optional_onehot_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.operation_onehot.v1",
            json_string(record, &["operation"]).as_deref(),
            128,
        )?,
    );
    slots.insert(
        MU_SLOT_ROUTE_HASH,
        optional_hash_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.route_hash.v1",
            json_string(record, &["route_id"]).as_deref(),
            2048,
        )?,
    );
    slots.insert(
        MU_SLOT_PARAM_SHAPE_HASH,
        optional_hash_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.param_shape_hash.v1",
            json_string(record, &["argument_shape_sha256"]).as_deref(),
            2048,
        )?,
    );
    slots.insert(
        MU_SLOT_STATUS_ONEHOT,
        optional_onehot_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.status_onehot.v1",
            json_string(record, &["status"]).as_deref(),
            64,
        )?,
    );
    slots.insert(
        MU_SLOT_ERROR_ONEHOT,
        optional_onehot_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.error_onehot.v1",
            json_string(record, &["error_type"]).as_deref(),
            128,
        )?,
    );
    slots.insert(
        MU_SLOT_PROFILE_HASH,
        optional_hash_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.profile_hash.v1",
            json_string(record, &["profile"]).as_deref(),
            1024,
        )?,
    );
    slots.insert(
        MU_SLOT_SURFACE_HASH,
        optional_hash_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.tool_surface_hash.v1",
            json_string(record, &["tool_surface_sha256"]).as_deref(),
            2048,
        )?,
    );
    slots.insert(
        MU_SLOT_SESSION_SEQUENCE_RANK,
        optional_rank_slot(
            SYN_MCP_USAGE_PANEL_NAME,
            "syn.mcp_usage.session_sequence_rank.v1",
            json_u64(record, &["session_sequence_position"]),
            0,
            RECENCY_RANK_MAX_UNIX_MS_MICROS,
        )?,
    );
    insert_time_slots(
        &mut slots,
        SYN_MCP_USAGE_PANEL_NAME,
        MU_SLOT_HOUR_CYCLIC,
        MU_SLOT_DOW_CYCLIC,
        "syn.mcp_usage.hour_cyclic.v1",
        "syn.mcp_usage.dow_cyclic.v1",
        mcp_usage_ts_ns(record),
    )?;
    slots.insert(
        MU_SLOT_RECORD_VECTOR_V2,
        measure_json(
            SYN_MCP_USAGE_PANEL_NAME,
            AlgorithmicLens::syn_record_vector_unit_fields(
                "syn.mcp_usage.record_vector.v2",
                Modality::Structured,
                128,
            ),
            &mcp_usage_numeric_record_v2(record, raw_bytes),
        )?,
    );

    let scalars = mcp_usage_scalars(record, raw_bytes)?;
    let metadata = mcp_usage_metadata(source_key, raw_bytes, record);
    constellation(
        context,
        SYN_MCP_USAGE_PANEL_VERSION,
        source_pointer(cf::CF_KV, source_key),
        constellation_input_bytes,
        slots,
        scalars,
        metadata,
    )
}

/// Returns whether a persisted observation key is selected for Calyx
/// measurement under the configured bounded-rate sampler.
///
/// # Errors
///
/// Returns a storage error when the sampler config is invalid or the key does
/// not match the `ts_ns || seq` observation-key codec.
pub fn observation_constellation_sample_permits(source_key: &[u8]) -> StorageResult<bool> {
    let every_n = observation_constellation_sample_every_n()?;
    let seq = observation_source_key_seq(source_key)?;
    Ok(u64::from(seq) % every_n == 0)
}

pub fn emit_success_metric(report: &ConstellationPutReport) {
    synapse_telemetry::metrics::counter!(
        CALYX_CONSTELLATION_MEASUREMENTS_TOTAL,
        "panel" => report.panel_name,
        "source_cf" => report.source_cf,
        "outcome" => report.disposition.as_str(),
    )
    .increment(1);
    synapse_telemetry::metrics::histogram!(
        CALYX_CONSTELLATION_MEASUREMENT_DURATION_US,
        "panel" => report.panel_name,
        "source_cf" => report.source_cf,
    )
    .record(metric_u64_as_f64(report.duration_us));
}

pub fn emit_error_metric(
    panel_name: &'static str,
    source_cf: &'static str,
    error_type: &'static str,
    duration: Duration,
) {
    synapse_telemetry::metrics::counter!(
        CALYX_CONSTELLATION_MEASUREMENT_ERRORS_TOTAL,
        "panel" => panel_name,
        "source_cf" => source_cf,
        "error_type" => error_type,
    )
    .increment(1);
    synapse_telemetry::metrics::histogram!(
        CALYX_CONSTELLATION_MEASUREMENT_DURATION_US,
        "panel" => panel_name,
        "source_cf" => source_cf,
    )
    .record(metric_u64_as_f64(duration_us(duration)));
}

#[must_use]
pub fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(digest.as_ref())
}

#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "metrics histograms accept f64 values; duration precision loss is acceptable for observability buckets"
)]
const fn metric_u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "callers check the value is within the IEEE-754 exact integer range before converting"
)]
const fn exact_u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "ratios are intentionally represented as f64 lens inputs"
)]
const fn ratio_u64(numerator: u64, denominator: u64) -> f64 {
    numerator as f64 / denominator as f64
}

fn constellation(
    context: NativeConstellationContext,
    panel_version: u32,
    pointer: String,
    raw_bytes: &[u8],
    slots: BTreeMap<SlotId, SlotVector>,
    scalars: BTreeMap<String, f64>,
    metadata: BTreeMap<String, String>,
) -> StorageResult<Constellation> {
    validate_panel_slot_allocation(panel_version, &pointer, &metadata, &slots)?;
    Ok(Constellation {
        cx_id: context.cx_id,
        vault_id: context.vault_id,
        panel_version,
        created_at: context.created_at_ms,
        input_ref: InputRef {
            hash: input_hash(raw_bytes),
            pointer: Some(pointer),
            redacted: false,
        },
        modality: Modality::Structured,
        slots,
        scalars,
        metadata,
        anchors: Vec::new(),
        provenance: LedgerRef {
            seq: context.next_ledger_seq,
            hash: [0; 32],
        },
        flags: CxFlags {
            ungrounded: true,
            degraded: false,
            novel_region: false,
            redacted_input: false,
        },
    })
}

/// Fails closed when a constellation declares a slot id outside the block its
/// own panel exclusively owns (#1776).
///
/// Slot ids are global: Calyx keys `cf/slot_<id>` by `CxId` alone, so a slot
/// written by the wrong panel lands in another panel's physical column family
/// and silently makes incomparable vectors look comparable. This is the write-
/// time backstop behind the compile-time `PANEL_SLOT_BLOCKS` disjointness
/// assertion — it catches a literal id typed into the wrong panel's builder,
/// which the block table alone cannot see.
fn validate_panel_slot_allocation(
    panel_version: u32,
    pointer: &str,
    metadata: &BTreeMap<String, String>,
    slots: &BTreeMap<SlotId, SlotVector>,
) -> StorageResult<()> {
    let Some(panel) = metadata.get(META_PANEL_NAME) else {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_constellation".to_owned(),
            detail: format!(
                "SYNAPSE_PANEL_SLOT_UNSCOPED: panel_version={panel_version} pointer={pointer} has \
                 no {META_PANEL_NAME} metadata, so its slot ids cannot be checked against a panel \
                 allocation; every constellation must name its panel (issue #1776)"
            ),
        });
    };
    // Every block this panel owns, not only the first. A block is contiguous, so
    // a panel whose neighbours are already allocated cannot extend its original
    // range and must own a second one (#1900 added a BM25 lexical lane at 103 to
    // a timeline panel boxed in at 7). Matching only the first block would reject
    // a correctly allocated id in a later block.
    let blocks = PANEL_SLOT_BLOCKS
        .iter()
        .filter(|block| block.panel == panel.as_str())
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_constellation".to_owned(),
            detail: format!(
                "SYNAPSE_PANEL_SLOT_BLOCK_MISSING: panel_version={panel_version} pointer={pointer} \
                 panel={panel} has no entry in PANEL_SLOT_BLOCKS; add its exclusive global slot \
                 block in crates/synapse-storage/src/constellations.rs before writing it \
                 (issue #1776)"
            ),
        });
    }
    for slot in slots.keys() {
        let id = slot.get();
        if !blocks
            .iter()
            .any(|block| id >= block.first && id <= block.last)
        {
            let owned = blocks
                .iter()
                .map(|block| format!("{}..={}", block.first, block.last))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(StorageError::WriteFailed {
                cf_name: "calyx_constellation".to_owned(),
                detail: format!(
                    "SYNAPSE_PANEL_SLOT_OUT_OF_BLOCK: panel_version={panel_version} \
                     pointer={pointer} panel={panel} declared slot id {id}, which is outside every \
                     block it owns ({owned}). Calyx stores every slot in a global cf/slot_{id:02} \
                     column family keyed by CxId alone, so writing it here would mix this panel's \
                     vectors into another panel's physical column family (issue #1776). Use an id \
                     from one of this panel's blocks, or allocate a new block in PANEL_SLOT_BLOCKS"
                ),
            });
        }
    }
    Ok(())
}

fn timeline_scalars(
    record: &TimelineRecord,
    raw_bytes: &[u8],
) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "record_version",
        u64::from(record.record_version),
    )?;
    insert_u64_scalar(&mut scalars, "ts_unix_ms", record.ts_ns / NS_PER_MS)?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn episode_scalars(
    record: &EpisodeRecord,
    raw_bytes: &[u8],
) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "record_version",
        u64::from(record.record_version),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "start_unix_ms",
        record.start_ts_ns / NS_PER_MS,
    )?;
    insert_u64_scalar(&mut scalars, "end_unix_ms", record.end_ts_ns / NS_PER_MS)?;
    insert_u64_scalar(&mut scalars, "duration_ms", record.duration_ms())?;
    insert_u64_scalar(&mut scalars, "row_count", record.row_count)?;
    insert_u64_scalar(&mut scalars, "keystroke_count", record.keystroke_count)?;
    insert_u64_scalar(&mut scalars, "click_count", record.click_count)?;
    insert_u64_scalar(
        &mut scalars,
        "interruption_count",
        u64::from(record.interruption_count),
    )?;
    insert_u64_scalar(&mut scalars, "interrupted_ms", record.interrupted_ms)?;
    insert_u64_scalar(
        &mut scalars,
        "distinct_title_count",
        u64::from(record.distinct_title_count),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn agent_event_scalars(
    record: &AgentEventRecord,
    raw_bytes: &[u8],
) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "record_version",
        u64::from(record.record_version),
    )?;
    insert_u64_scalar(&mut scalars, "ts_unix_ms", record.ts_ns / NS_PER_MS)?;
    insert_optional_u64_scalar(
        &mut scalars,
        "usage_input_tokens",
        record.attributes.usage_input_tokens,
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "usage_output_tokens",
        record.attributes.usage_output_tokens,
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "usage_cache_read_input_tokens",
        record.attributes.usage_cache_read_input_tokens,
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "usage_cache_creation_input_tokens",
        record.attributes.usage_cache_creation_input_tokens,
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "usage_total_tokens",
        agent_event_usage_total(record),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "duration_ms",
        payload_u64(&record.payload, &["duration_ms"]),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

#[allow(
    clippy::too_many_lines,
    reason = "agent transcript scalar extraction is a one-to-one record field map kept together to preserve auditability"
)]
fn agent_transcript_scalars(
    record: &AgentTranscriptRecord,
    raw_bytes: &[u8],
) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "record_version",
        u64::from(record.record_version),
    )?;
    insert_u64_scalar(&mut scalars, "ts_unix_ms", record.ts_ns / NS_PER_MS)?;
    insert_u64_scalar(&mut scalars, "line_no", record.line_no)?;
    insert_u64_scalar(&mut scalars, "raw_line_bytes", record.raw_line_bytes)?;
    insert_optional_u64_scalar(&mut scalars, "turn_index", record.turn_index)?;
    insert_optional_u64_scalar(&mut scalars, "content_bytes", record.content_bytes)?;
    insert_u64_scalar(
        &mut scalars,
        "content_truncated",
        bool_u64(record.content_truncated),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "tool_call_count",
        u64::try_from(record.tool_calls.len()).unwrap_or(u64::MAX),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "tool_argument_bytes_total",
        transcript_tool_argument_bytes_total(record),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "tool_result_bytes_total",
        transcript_tool_result_bytes_total(record),
    )?;
    if let Some(usage) = record.usage.as_ref() {
        insert_optional_u64_scalar(&mut scalars, "usage_input_tokens", usage.input_tokens)?;
        insert_optional_u64_scalar(&mut scalars, "usage_output_tokens", usage.output_tokens)?;
        insert_optional_u64_scalar(
            &mut scalars,
            "usage_cache_read_input_tokens",
            usage.cache_read_input_tokens,
        )?;
        insert_optional_u64_scalar(
            &mut scalars,
            "usage_cache_creation_input_tokens",
            usage.cache_creation_input_tokens,
        )?;
        insert_optional_u64_scalar(
            &mut scalars,
            "usage_cache_creation_5m_input_tokens",
            usage.cache_creation_5m_input_tokens,
        )?;
        insert_optional_u64_scalar(
            &mut scalars,
            "usage_cache_creation_1h_input_tokens",
            usage.cache_creation_1h_input_tokens,
        )?;
        insert_optional_u64_scalar(
            &mut scalars,
            "usage_reasoning_output_tokens",
            usage.reasoning_output_tokens,
        )?;
        insert_optional_u64_scalar(
            &mut scalars,
            "total_cost_micro_usd",
            usage.total_cost_micro_usd,
        )?;
        insert_optional_u64_scalar(
            &mut scalars,
            "usage_total_tokens",
            transcript_usage_total(record),
        )?;
        insert_u64_scalar(
            &mut scalars,
            "model_usage_count",
            u64::try_from(usage.model_usage.len()).unwrap_or(u64::MAX),
        )?;
        insert_u64_scalar(
            &mut scalars,
            "model_usage_input_tokens",
            usage
                .model_usage
                .iter()
                .fold(0_u64, |sum, item| sum.saturating_add(item.input_tokens)),
        )?;
        insert_u64_scalar(
            &mut scalars,
            "model_usage_output_tokens",
            usage
                .model_usage
                .iter()
                .fold(0_u64, |sum, item| sum.saturating_add(item.output_tokens)),
        )?;
        insert_u64_scalar(
            &mut scalars,
            "model_usage_cache_read_input_tokens",
            usage.model_usage.iter().fold(0_u64, |sum, item| {
                sum.saturating_add(item.cache_read_input_tokens)
            }),
        )?;
        insert_u64_scalar(
            &mut scalars,
            "model_usage_cache_creation_input_tokens",
            usage.model_usage.iter().fold(0_u64, |sum, item| {
                sum.saturating_add(item.cache_creation_input_tokens)
            }),
        )?;
        insert_u64_scalar(
            &mut scalars,
            "model_usage_cost_micro_usd",
            usage.model_usage.iter().fold(0_u64, |sum, item| {
                sum.saturating_add(item.cost_micro_usd.unwrap_or(0))
            }),
        )?;
    }
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn action_scalars(record: &Value, raw_bytes: &[u8]) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_optional_u64_scalar(
        &mut scalars,
        "schema_version",
        json_u64(record, &["schema_version"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "ts_unix_ms",
        json_u64(record, &["ts_ns"]).map(|value| value / NS_PER_MS),
    )?;
    insert_optional_u64_scalar(&mut scalars, "seq", json_u64(record, &["seq"]))?;
    insert_optional_u64_scalar(
        &mut scalars,
        "payload_bytes",
        json_u64(record, &["payload_bytes"]),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "error_present",
        bool_u64(json_non_null(record, &["error"]) || json_non_null(record, &["error_code"])),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn reflex_scalars(
    record: &StoredReflexAudit,
    raw_bytes: &[u8],
) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "schema_version",
        u64::from(record.schema_version),
    )?;
    insert_u64_scalar(&mut scalars, "ts_unix_ms", record.ts_ns / NS_PER_MS)?;
    insert_u64_scalar(
        &mut scalars,
        "step_count",
        u64::try_from(record.steps.len()).unwrap_or(u64::MAX),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "error_present",
        bool_u64(record.error_code.as_deref().and_then(non_empty).is_some()),
    )?;
    insert_optional_u64_scalar(&mut scalars, "latency_ms", reflex_latency_ms(record))?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn process_scalars(record: &Value, raw_bytes: &[u8]) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_optional_u64_scalar(
        &mut scalars,
        "schema_version",
        json_u64(record, &["schema_version"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "ts_unix_ms",
        process_ts_ns(record)?.map(|value| value / NS_PER_MS),
    )?;
    insert_optional_u64_scalar(&mut scalars, "pid", json_u64(record, &["pid"]))?;
    insert_optional_u64_scalar(
        &mut scalars,
        "window_owner_pid",
        json_u64(record, &["window_owner_pid"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "uptime_ms",
        json_u64(record, &["uptime_ms", "duration_ms"]),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn observation_scalars(
    record: &StoredObservation,
    raw_bytes: &[u8],
) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "schema_version",
        u64::from(record.schema_version),
    )?;
    insert_u64_scalar(&mut scalars, "ts_unix_ms", record.ts_ns / NS_PER_MS)?;
    insert_u64_scalar(
        &mut scalars,
        "element_count",
        u64::try_from(record.elements.len()).unwrap_or(u64::MAX),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "entity_count",
        u64::try_from(record.entities.len()).unwrap_or(u64::MAX),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "hud_field_count",
        u64::try_from(record.hud.by_name.len()).unwrap_or(u64::MAX),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "hud_error_count",
        u64::try_from(record.hud.errors.len()).unwrap_or(u64::MAX),
    )?;
    insert_f64_scalar(
        &mut scalars,
        "foreground_dpi_scale",
        f64::from(record.foreground.dpi_scale),
    )?;
    insert_f64_scalar(
        &mut scalars,
        "assembled_in_ms",
        f64::from(record.diagnostics.assembled_in_ms),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "size_bytes",
        u64::from(record.diagnostics.size_bytes),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "size_estimate_tokens",
        u64::from(record.diagnostics.size_estimate_tokens),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    Ok(scalars)
}

fn outcome_scalars(record: &Value, raw_bytes: &[u8]) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "ts_unix_ms",
        outcome_ts_ns(record).map(|ts_ns| ts_ns / NS_PER_MS),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "code_count",
        json_u64(record, &["code_count"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "ladder_index",
        json_u64(record, &["ladder_index"]),
    )?;
    Ok(scalars)
}

fn mcp_usage_scalars(record: &Value, raw_bytes: &[u8]) -> StorageResult<BTreeMap<String, f64>> {
    let mut scalars = BTreeMap::new();
    insert_u64_scalar(
        &mut scalars,
        "raw_len_bytes",
        u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "schema_version",
        json_u64(record, &["schema_version"]),
    )?;
    insert_optional_u64_scalar(&mut scalars, "seq", json_u64(record, &["seq"]))?;
    insert_optional_u64_scalar(
        &mut scalars,
        "session_sequence_position",
        json_u64(record, &["session_sequence_position"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "duration_ms",
        json_u64(record, &["duration_ms"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "response_size_bytes",
        json_u64(record, &["response_size_bytes"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "response_content_count",
        json_u64(record, &["response_content_count"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "argument_top_level_key_count",
        json_u64(record, &["argument_top_level_key_count"]),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "argument_nested_path_count",
        json_u64(record, &["argument_nested_path_count"]),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "error_present",
        bool_u64(json_string(record, &["error_type"]).is_some()),
    )?;
    insert_u64_scalar(
        &mut scalars,
        "steering_emitted",
        bool_u64(json_bool(record, &["steering_emitted"]).unwrap_or(false)),
    )?;
    insert_optional_u64_scalar(
        &mut scalars,
        "finished_unix_ms",
        json_u64(record, &["finished_at_unix_ms"]),
    )?;
    Ok(scalars)
}

fn insert_u64_scalar(
    scalars: &mut BTreeMap<String, f64>,
    name: &'static str,
    value: u64,
) -> StorageResult<()> {
    if value > MAX_EXACT_F64_INT {
        return Err(measurement_error(
            "scalar value exceeds the IEEE-754 exact integer range",
            format!("{name}={value}"),
        ));
    }
    scalars.insert(name.to_owned(), exact_u64_as_f64(value));
    Ok(())
}

fn insert_f64_scalar(
    scalars: &mut BTreeMap<String, f64>,
    name: &'static str,
    value: f64,
) -> StorageResult<()> {
    if !value.is_finite() {
        return Err(measurement_error(
            "non-finite scalar value",
            format!("{name}={value}"),
        ));
    }
    scalars.insert(name.to_owned(), value);
    Ok(())
}

fn insert_optional_u64_scalar(
    scalars: &mut BTreeMap<String, f64>,
    name: &'static str,
    value: Option<u64>,
) -> StorageResult<()> {
    if let Some(value) = value {
        insert_u64_scalar(scalars, name, value)?;
    }
    Ok(())
}

fn timeline_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &TimelineRecord,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_TIMELINE_PANEL_NAME,
        cf::CF_TIMELINE,
        source_key,
        raw_bytes,
    );
    metadata.insert(META_EXACT_TS_NS.to_owned(), record.ts_ns.to_string());
    metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
    activate_temporal_lane(&mut metadata, record.ts_ns, source_key);
    metadata.insert(
        META_RECENCY_BASIS.to_owned(),
        RECENCY_BASIS_EVENT_TIME_RANK.to_owned(),
    );
    metadata.insert("timeline_kind".to_owned(), timeline_kind_name(record.kind)?);
    metadata.insert(
        "timeline_actor".to_owned(),
        actor_kind(&record.actor).to_owned(),
    );
    if let TimelineActor::Agent { session_id } = &record.actor {
        metadata.insert("timeline_agent_session_id".to_owned(), session_id.clone());
    }
    if let Some(app) = record.app.as_deref().and_then(non_empty) {
        metadata.insert("timeline_app".to_owned(), truncate_metadata(app));
    }
    if let Some(title) = timeline_title(record).as_deref().and_then(non_empty) {
        metadata.insert(
            "timeline_title_excerpt".to_owned(),
            truncate_metadata(title),
        );
    }
    if let Some(url) = payload_string(&record.payload, &["url"]) {
        metadata.insert("timeline_url_excerpt".to_owned(), truncate_metadata(&url));
        if let Some(host) = url_host(&url) {
            metadata.insert("timeline_url_host".to_owned(), truncate_metadata(&host));
        }
    }
    Ok(metadata)
}

fn episode_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &EpisodeRecord,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_EPISODE_PANEL_NAME,
        cf::CF_EPISODES,
        source_key,
        raw_bytes,
    );
    metadata.insert("episode_id".to_owned(), record.episode_id.clone());
    metadata.insert(META_EXACT_TS_NS.to_owned(), record.start_ts_ns.to_string());
    activate_temporal_lane(&mut metadata, record.start_ts_ns, source_key);
    metadata.insert(
        "episode_start_ts_ns".to_owned(),
        record.start_ts_ns.to_string(),
    );
    metadata.insert("episode_end_ts_ns".to_owned(), record.end_ts_ns.to_string());
    metadata.insert(
        "episode_actor".to_owned(),
        actor_kind(&record.actor).to_owned(),
    );
    if let TimelineActor::Agent { session_id } = &record.actor {
        metadata.insert("episode_agent_session_id".to_owned(), session_id.clone());
    }
    metadata.insert(
        "episode_started_because".to_owned(),
        boundary_name(record.started_because)?,
    );
    metadata.insert(
        "episode_ended_because".to_owned(),
        boundary_name(record.ended_because)?,
    );
    if let Some(app) = record.app.as_deref().and_then(non_empty) {
        metadata.insert("episode_app".to_owned(), truncate_metadata(app));
    }
    if let Some(document) = record.document.as_deref().and_then(non_empty) {
        metadata.insert("episode_document".to_owned(), truncate_metadata(document));
    }
    if let Some(url) = record.url.as_deref().and_then(non_empty) {
        metadata.insert("episode_url_excerpt".to_owned(), truncate_metadata(url));
        if let Some(host) = url_host(url) {
            metadata.insert("episode_url_host".to_owned(), truncate_metadata(&host));
        }
    }
    if let Some(title) = record.title_first.as_deref().and_then(non_empty) {
        metadata.insert(
            "episode_title_first_excerpt".to_owned(),
            truncate_metadata(title),
        );
    }
    if let Some(title) = record.title_last.as_deref().and_then(non_empty) {
        metadata.insert(
            "episode_title_last_excerpt".to_owned(),
            truncate_metadata(title),
        );
    }
    Ok(metadata)
}

#[allow(
    clippy::too_many_lines,
    reason = "agent event metadata extraction is a one-to-one record field map kept together to preserve auditability"
)]
fn agent_event_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &AgentEventRecord,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_AGENT_EVENT_PANEL_NAME,
        cf::CF_AGENT_EVENTS,
        source_key,
        raw_bytes,
    );
    metadata.insert(META_EXACT_TS_NS.to_owned(), record.ts_ns.to_string());
    metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
    activate_temporal_lane(&mut metadata, record.ts_ns, source_key);
    metadata.insert(
        META_RECENCY_BASIS.to_owned(),
        RECENCY_BASIS_EVENT_TIME_RANK.to_owned(),
    );
    metadata.insert(
        "agent_event_kind".to_owned(),
        agent_event_kind_name(record.kind)?,
    );
    if let Some(session_id) = record.session_id.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_event_session_id".to_owned(),
            truncate_metadata(session_id),
        );
    }
    if let Some(spawn_id) = record.spawn_id.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_event_spawn_id".to_owned(),
            truncate_metadata(spawn_id),
        );
    }
    if let Some(reason_code) = record.reason_code.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_event_reason_code".to_owned(),
            truncate_metadata(reason_code),
        );
    }
    if let Some(end_state) = optional_agent_end_state_name(record.end_state)? {
        metadata.insert("agent_event_end_state".to_owned(), end_state);
    }
    if let Some(state_from) = record.state_from.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_event_state_from".to_owned(),
            truncate_metadata(state_from),
        );
    }
    if let Some(state_to) = record.state_to.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_event_state_to".to_owned(),
            truncate_metadata(state_to),
        );
    }
    if let Some(operation) = optional_gen_ai_operation_name(record.attributes.operation_name)? {
        metadata.insert("gen_ai_operation_name".to_owned(), operation);
    }
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_provider_name",
        record.attributes.provider_name.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_agent_id",
        record.attributes.agent_id.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_agent_name",
        record.attributes.agent_name.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_conversation_id",
        record.attributes.conversation_id.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_request_model",
        record.attributes.request_model.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_response_model",
        record.attributes.response_model.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_tool_name",
        record.attributes.tool_name.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "gen_ai_tool_call_id",
        record.attributes.tool_call_id.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "error_type",
        record.attributes.error_type.as_deref(),
    );
    metadata.insert(
        "agent_event_has_payload".to_owned(),
        (!record.payload.is_null()).to_string(),
    );
    Ok(metadata)
}

fn agent_transcript_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &AgentTranscriptRecord,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_AGENT_TRANSCRIPT_PANEL_NAME,
        cf::CF_AGENT_TRANSCRIPTS,
        source_key,
        raw_bytes,
    );
    metadata.insert(META_EXACT_TS_NS.to_owned(), record.ts_ns.to_string());
    metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
    activate_temporal_lane(&mut metadata, record.ts_ns, source_key);
    metadata.insert(
        "agent_transcript_spawn_id".to_owned(),
        record.spawn_id.clone(),
    );
    metadata.insert(
        "agent_transcript_line_no".to_owned(),
        record.line_no.to_string(),
    );
    metadata.insert(
        "agent_transcript_source".to_owned(),
        transcript_source_name(record.source)?,
    );
    metadata.insert(
        "agent_transcript_status".to_owned(),
        transcript_parse_status_name(record.status)?,
    );
    if let Some(role) = optional_transcript_role_name(record.role)? {
        metadata.insert("agent_transcript_role".to_owned(), role);
    }
    insert_optional_metadata(
        &mut metadata,
        "agent_transcript_event_kind",
        record.event_kind.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "agent_transcript_conversation_id",
        record.conversation_id.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "agent_transcript_model",
        record.model.as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "agent_transcript_content_sha256",
        record.content_sha256.as_deref(),
    );
    metadata.insert(
        "agent_transcript_content_truncated".to_owned(),
        record.content_truncated.to_string(),
    );
    if let Some(source_error) = record.source_error.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_transcript_source_error_excerpt".to_owned(),
            truncate_metadata(source_error),
        );
    }
    if let Some(parse_error) = record.parse_error.as_deref().and_then(non_empty) {
        metadata.insert(
            "agent_transcript_parse_error_excerpt".to_owned(),
            truncate_metadata(parse_error),
        );
    }
    let tool_names = transcript_tool_names(record);
    if let Some(tool_names) = non_empty(&tool_names) {
        metadata.insert(
            "agent_transcript_tool_names".to_owned(),
            truncate_metadata(tool_names),
        );
    }
    let model_names = transcript_model_usage_names(record);
    if let Some(model_names) = non_empty(&model_names) {
        metadata.insert(
            "agent_transcript_model_usage_names".to_owned(),
            truncate_metadata(model_names),
        );
    }
    Ok(metadata)
}

fn action_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
) -> BTreeMap<String, String> {
    let mut metadata = common_metadata(
        SYN_ACTION_PANEL_NAME,
        cf::CF_ACTION_LOG,
        source_key,
        raw_bytes,
    );
    metadata.insert("action_kind".to_owned(), action_identity(record));
    metadata.insert("oracle.domain".to_owned(), "synapse.action".to_owned());
    metadata.insert("oracle.action".to_owned(), action_identity(record));
    insert_optional_metadata(
        &mut metadata,
        "action_tool",
        json_string(record, &["tool"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "action_verb",
        json_string(record, &["verb"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "action_status",
        json_string(record, &["status", "outcome"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "action_error_code",
        json_string(record, &["error_code"]).as_deref(),
    );
    if let Some(ts_ns) = json_u64(record, &["ts_ns"]) {
        metadata.insert(META_EXACT_TS_NS.to_owned(), ts_ns.to_string());
        metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
        activate_temporal_lane(&mut metadata, ts_ns, source_key);
    }
    if let Some(target) = action_target_text(record).as_deref().and_then(non_empty) {
        metadata.insert(
            "action_target_excerpt".to_owned(),
            truncate_metadata(target),
        );
    }
    metadata
}

/// Derives the one declared terminal action outcome. Nonterminal audit rows
/// remain measurable but ungrounded; no other status is assigned a polarity.
///
/// # Errors
///
/// Returns a structured error when a purported terminal row has an unknown,
/// contradictory, or malformed outcome contract.
pub fn action_outcome_anchor(
    source_key: &[u8],
    record: &Value,
) -> StorageResult<Option<GroundingAnchor>> {
    let row_kind = json_string(record, &["row_kind"]);
    let outcome = match row_kind.as_deref() {
        Some("command_audit") => match json_string(record, &["phase"]).as_deref() {
            Some("intent") => return Ok(None),
            Some("final") => match json_string(record, &["outcome"]).as_deref() {
                Some("ok") => true,
                Some("error") => false,
                value => {
                    return Err(StorageError::ReadFailed {
                        cf_name: cf::CF_ACTION_LOG.to_owned(),
                        detail: format!(
                            "terminal command_audit row has unsupported outcome {value:?}; remediation=repair the authoritative row to outcome=ok|error or extend the versioned adjudication contract"
                        ),
                    });
                }
            },
            value => {
                return Err(StorageError::ReadFailed {
                    cf_name: cf::CF_ACTION_LOG.to_owned(),
                    detail: format!(
                        "command_audit row has unsupported phase {value:?}; remediation=repair the authoritative row to phase=intent|final or extend the versioned adjudication contract"
                    ),
                });
            }
        },
        None | Some("action_audit") => match json_string(record, &["status"]).as_deref() {
            Some("ok") => true,
            Some("error" | "denied") => false,
            _ => return Ok(None),
        },
        Some(_) => return Ok(None),
    };
    let observed_at_ms = json_u64(record, &["ts_ns"])
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf::CF_ACTION_LOG.to_owned(),
            detail: "terminal action outcome has no finite u64 ts_ns; remediation=repair the authoritative action audit row before grounding it".to_owned(),
        })?
        / 1_000_000;
    Ok(Some(GroundingAnchor {
        kind_label: "reward".to_owned(),
        value: GroundingAnchorValue::Bool(outcome),
        source: source_pointer(cf::CF_ACTION_LOG, source_key),
        observed_at_ms,
        confidence: 1.0,
    }))
}

fn reflex_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &StoredReflexAudit,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_REFLEX_PANEL_NAME,
        cf::CF_REFLEX_AUDIT,
        source_key,
        raw_bytes,
    );
    metadata.insert(META_EXACT_TS_NS.to_owned(), record.ts_ns.to_string());
    metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
    activate_temporal_lane(&mut metadata, record.ts_ns, source_key);
    metadata.insert("reflex_id".to_owned(), truncate_metadata(&record.reflex_id));
    metadata.insert(
        "reflex_audit_id".to_owned(),
        truncate_metadata(&record.audit_id),
    );
    metadata.insert(
        "reflex_status".to_owned(),
        snake_case_name(record.status, "ReflexState")?,
    );
    insert_optional_metadata(&mut metadata, "reflex_event_id", record.event_id.as_deref());
    insert_optional_metadata(
        &mut metadata,
        "reflex_error_code",
        record.error_code.as_deref(),
    );
    Ok(metadata)
}

fn process_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_PROCESS_PANEL_NAME,
        cf::CF_PROCESS_HISTORY,
        source_key,
        raw_bytes,
    );
    metadata.insert("process_event_kind".to_owned(), process_event_kind(record));
    if let Some(ts_ns) = process_ts_ns(record)? {
        metadata.insert(META_EXACT_TS_NS.to_owned(), ts_ns.to_string());
        metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
        activate_temporal_lane(&mut metadata, ts_ns, source_key);
        metadata.insert(
            META_RECENCY_BASIS.to_owned(),
            RECENCY_BASIS_EVENT_TIME_RANK.to_owned(),
        );
    }
    insert_optional_metadata(
        &mut metadata,
        "process_target",
        json_string(record, &["target"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "process_path",
        json_string(record, &["process_path", "path", "target"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "process_status",
        json_string(record, &["status"]).as_deref(),
    );
    Ok(metadata)
}

fn observation_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &StoredObservation,
) -> StorageResult<BTreeMap<String, String>> {
    let mut metadata = common_metadata(
        SYN_OBSERVATION_PANEL_NAME,
        cf::CF_OBSERVATIONS,
        source_key,
        raw_bytes,
    );
    metadata.insert(META_EXACT_TS_NS.to_owned(), record.ts_ns.to_string());
    metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
    activate_temporal_lane(&mut metadata, record.ts_ns, source_key);
    metadata.insert(
        "observation_id".to_owned(),
        truncate_metadata(&record.observation_id),
    );
    metadata.insert(
        "observation_mode".to_owned(),
        snake_case_name(record.mode, "PerceptionMode")?,
    );
    metadata.insert(
        "observation_reason".to_owned(),
        truncate_metadata(&record.reason),
    );
    metadata.insert(
        "observation_process_name".to_owned(),
        truncate_metadata(&record.foreground.process_name),
    );
    metadata.insert(
        "observation_window_title_excerpt".to_owned(),
        truncate_metadata(&record.foreground.window_title),
    );
    if let Some(session_id) = record.session_id.as_deref().and_then(non_empty) {
        metadata.insert(
            "observation_session_id".to_owned(),
            truncate_metadata(session_id),
        );
    }
    if let Some(profile_id) = record.foreground.profile_id.as_deref().and_then(non_empty) {
        metadata.insert(
            "observation_profile_id".to_owned(),
            truncate_metadata(profile_id),
        );
    }
    Ok(metadata)
}

fn outcome_metadata(
    source_cf: &'static str,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
) -> BTreeMap<String, String> {
    let mut metadata = common_metadata(SYN_OUTCOME_PANEL_NAME, source_cf, source_key, raw_bytes);
    if let Some(ts_ns) = outcome_ts_ns(record) {
        metadata.insert(META_EXACT_TS_NS.to_owned(), ts_ns.to_string());
        metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
        activate_temporal_lane(&mut metadata, ts_ns, source_key);
        metadata.insert(
            META_RECENCY_BASIS.to_owned(),
            RECENCY_BASIS_EVENT_TIME_RANK.to_owned(),
        );
    }
    insert_optional_metadata(
        &mut metadata,
        "outcome_event",
        outcome_event(record).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "outcome_status",
        outcome_status(record).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "outcome_target",
        outcome_target(record).as_deref(),
    );
    metadata
}

fn mcp_usage_metadata(
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
) -> BTreeMap<String, String> {
    let mut metadata = common_metadata(SYN_MCP_USAGE_PANEL_NAME, cf::CF_KV, source_key, raw_bytes);
    if let Some(ts_ns) = mcp_usage_ts_ns(record) {
        metadata.insert(META_EXACT_TS_NS.to_owned(), ts_ns.to_string());
        metadata.insert(META_TIME_BASIS.to_owned(), TIME_BASIS_UTC.to_owned());
        activate_temporal_lane(&mut metadata, ts_ns, source_key);
        metadata.insert(
            META_RECENCY_BASIS.to_owned(),
            RECENCY_BASIS_EVENT_TIME_RANK.to_owned(),
        );
    }
    insert_optional_metadata(
        &mut metadata,
        "mcp_usage_tool",
        json_string(record, &["tool"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "mcp_usage_operation",
        json_string(record, &["operation"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "mcp_usage_route_id",
        json_string(record, &["route_id"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "mcp_usage_status",
        json_string(record, &["status"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "mcp_usage_error_type",
        json_string(record, &["error_type"]).as_deref(),
    );
    insert_optional_metadata(
        &mut metadata,
        "mcp_usage_argument_shape_sha256",
        json_string(record, &["argument_shape_sha256"]).as_deref(),
    );
    metadata
}

fn insert_optional_metadata(
    metadata: &mut BTreeMap<String, String>,
    key: &'static str,
    value: Option<&str>,
) {
    if let Some(value) = value.and_then(non_empty) {
        metadata.insert(key.to_owned(), truncate_metadata(value));
    }
}

fn common_metadata(
    panel_name: &'static str,
    source_cf: &str,
    source_key: &[u8],
    raw_bytes: &[u8],
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PANEL_NAME.to_owned(), panel_name.to_owned());
    metadata.insert(META_SOURCE_CF.to_owned(), source_cf.to_owned());
    metadata.insert(META_SOURCE_KEY_HEX.to_owned(), hex_encode(source_key));
    metadata.insert(META_RAW_SHA256.to_owned(), sha256_hex(raw_bytes));
    metadata.insert(META_RAW_LEN_BYTES.to_owned(), raw_bytes.len().to_string());
    metadata.insert(
        METADATA_TEMPORAL_LANE_STATE.to_owned(),
        TEMPORAL_LANE_INACTIVE.to_owned(),
    );
    metadata.insert(
        METADATA_TEMPORAL_INACTIVE_REASON.to_owned(),
        TEMPORAL_MISSING_CREATED_AT.to_owned(),
    );
    metadata
}

fn activate_temporal_lane(
    metadata: &mut BTreeMap<String, String>,
    event_time_ns: u64,
    source_key: &[u8],
) {
    metadata.insert(
        METADATA_TEMPORAL_LANE_STATE.to_owned(),
        TEMPORAL_LANE_ACTIVE.to_owned(),
    );
    metadata.remove(METADATA_TEMPORAL_INACTIVE_REASON);
    metadata.insert(
        METADATA_SOURCE_EVENT_TIME_SECS.to_owned(),
        (event_time_ns / NS_PER_SEC).to_string(),
    );
    metadata.insert(
        METADATA_SOURCE_EVENT_TIME_RAW.to_owned(),
        event_time_ns.to_string(),
    );
    metadata.insert(METADATA_SOURCE_SEQUENCE.to_owned(), hex_encode(source_key));
}

/// Bounds one runtime call while still amortizing setup across many source
/// rows. The page itself is 1,000 rows; 256 keeps a future neural runtime's
/// transient input/output allocations bounded without reducing the algorithmic
/// runtime to batch-size one.
const SYN_BACKFILL_MEASURE_BATCH_LIMIT: usize = 256;

#[derive(Clone, Copy)]
enum DeferredMeasurementPolicy {
    Strict,
    OverLimitAbsent { panel_name: &'static str },
}

struct DeferredMeasurement {
    lens: AlgorithmicLens,
    input: Input,
    policy: DeferredMeasurementPolicy,
}

#[derive(Default)]
pub(crate) struct DeferredMeasurementPlan {
    jobs: Vec<DeferredMeasurement>,
}

thread_local! {
    static DEFERRED_MEASUREMENTS: RefCell<Option<Vec<DeferredMeasurement>>> = const {
        RefCell::new(None)
    };
}

struct DeferredMeasurementCaptureGuard;

impl Drop for DeferredMeasurementCaptureGuard {
    fn drop(&mut self) {
        DEFERRED_MEASUREMENTS.with(|measurements| {
            measurements.borrow_mut().take();
        });
    }
}

/// Captures one constellation builder's lens calls without executing them.
///
/// # Errors
///
/// Returns a storage error when capture is nested on one worker thread or when
/// the builder itself fails. The guard clears thread-local state during unwind.
pub(crate) fn capture_deferred_measurements<T>(
    build: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<(T, DeferredMeasurementPlan)> {
    let installed = DEFERRED_MEASUREMENTS.with(|measurements| {
        let mut measurements = measurements.borrow_mut();
        if measurements.is_some() {
            false
        } else {
            *measurements = Some(Vec::new());
            true
        }
    });
    if !installed {
        return Err(measurement_error(
            "nested Calyx measurement batch capture",
            "a constellation builder attempted to open a second deferred measurement scope on one worker thread",
        ));
    }
    let guard = DeferredMeasurementCaptureGuard;
    let value = build()?;
    let jobs = DEFERRED_MEASUREMENTS.with(|measurements| measurements.borrow_mut().take());
    drop(guard);
    let Some(jobs) = jobs else {
        return Err(measurement_error(
            "Calyx measurement batch capture state disappeared",
            "the thread-local capture was cleared before its builder returned",
        ));
    };
    Ok((value, DeferredMeasurementPlan { jobs }))
}

fn defer_measurement(
    lens: AlgorithmicLens,
    input: Input,
    policy: DeferredMeasurementPolicy,
) -> Result<(), Box<(AlgorithmicLens, Input)>> {
    DEFERRED_MEASUREMENTS.with(|measurements| {
        let mut measurements = measurements.borrow_mut();
        let Some(jobs) = measurements.as_mut() else {
            return Err(Box::new((lens, input)));
        };
        jobs.push(DeferredMeasurement {
            lens,
            input,
            policy,
        });
        Ok(())
    })
}

fn deferred_measurement_placeholder() -> SlotVector {
    absent(AbsentReason::Deferred)
}

pub(crate) const fn is_deferred_measurement_placeholder(vector: &SlotVector) -> bool {
    matches!(
        vector,
        SlotVector::Absent {
            reason: AbsentReason::Deferred
        }
    )
}

/// Executes identical frozen lenses across all captured rows through the real
/// registry batch API, returning ordered slot replacements per source row.
///
/// # Errors
///
/// Returns a storage error when the active panel/registry contract is missing,
/// a planned lens is not uniquely mapped to a slot, preflight rejects a strict
/// input, batch measurement fails, cardinality drifts, or a job is unresolved.
#[allow(
    clippy::too_many_lines,
    reason = "contract lookup, pointwise preflight, grouped runtime calls, and cardinality validation form one fail-closed measurement transaction"
)]
pub(crate) fn resolve_deferred_measurement_plans(
    panel_version: u32,
    created_at_ms: u64,
    plans: &[&DeferredMeasurementPlan],
) -> StorageResult<Vec<Vec<(SlotId, SlotVector)>>> {
    let contract =
        syn_reconstructable_panel_contract(panel_version, created_at_ms)?.ok_or_else(|| {
            measurement_error(
                "Calyx measurement batch panel is not registered",
                panel_version,
            )
        })?;
    let mut slot_by_lens = BTreeMap::new();
    for slot in &contract.panel.slots {
        if let Some(first_slot) = slot_by_lens.insert(slot.lens_id, slot.slot_id) {
            return Err(measurement_error(
                "Calyx measurement batch lens is mapped to multiple slots",
                format!(
                    "panel_version={panel_version} lens_id={} first_slot={} duplicate_slot={}",
                    slot.lens_id, first_slot.0, slot.slot_id.0
                ),
            ));
        }
    }

    let mut replacements = plans
        .iter()
        .map(|plan| Vec::with_capacity(plan.jobs.len()))
        .collect::<Vec<_>>();
    let mut groups = BTreeMap::<_, Vec<(usize, &DeferredMeasurement)>>::new();
    for (row_index, plan) in plans.iter().enumerate() {
        let mut seen = BTreeSet::new();
        for job in &plan.jobs {
            let lens_id = job.lens.id();
            if !seen.insert(lens_id) {
                return Err(measurement_error(
                    "Calyx measurement batch row repeats a frozen lens",
                    format!("row_index={row_index} lens_id={lens_id}"),
                ));
            }
            let slot_id = slot_by_lens.get(&lens_id).copied().ok_or_else(|| {
                measurement_error(
                    "Calyx measurement batch lens is absent from the active panel",
                    format!(
                        "panel_version={panel_version} row_index={row_index} lens_id={lens_id}"
                    ),
                )
            })?;
            match job.lens.preflight_batch_input(&job.input) {
                Ok(()) => groups.entry(lens_id).or_default().push((row_index, job)),
                Err(source)
                    if source.code == CalyxErrorCode::LensInputTooLarge.code()
                        && matches!(
                            job.policy,
                            DeferredMeasurementPolicy::OverLimitAbsent { .. }
                        ) =>
                {
                    let DeferredMeasurementPolicy::OverLimitAbsent { panel_name } = job.policy
                    else {
                        unreachable!("policy guarded by matches above")
                    };
                    replacements[row_index]
                        .push((slot_id, refused_text_slot(panel_name, &job.lens, &source)));
                }
                Err(source) => {
                    return Err(measurement_error(
                        "Calyx Syn* lens batch preflight failed",
                        format!(
                            "panel_version={panel_version} row_index={row_index} lens_id={lens_id}: {source}"
                        ),
                    ));
                }
            }
        }
    }

    for (lens_id, jobs) in groups {
        let slot_id = *slot_by_lens.get(&lens_id).ok_or_else(|| {
            measurement_error("Calyx measurement batch lost its slot mapping", lens_id)
        })?;
        let inputs = jobs
            .iter()
            .map(|(_row_index, job)| job.input.clone())
            .collect::<Vec<_>>();
        let vectors = measure_registry_batch_with_runtime_limit(
            &contract.registry,
            lens_id,
            &inputs,
            Some(SYN_BACKFILL_MEASURE_BATCH_LIMIT),
        )
        .map_err(|source| {
            measurement_error(
                "Calyx Syn* registry batch measurement failed",
                format!(
                    "panel_version={panel_version} lens_id={lens_id} inputs={}: {source}",
                    inputs.len()
                ),
            )
        })?;
        if vectors.len() != jobs.len() {
            return Err(measurement_error(
                "Calyx Syn* registry batch cardinality mismatch",
                format!(
                    "panel_version={panel_version} lens_id={lens_id} inputs={} vectors={}",
                    jobs.len(),
                    vectors.len()
                ),
            ));
        }
        for ((row_index, _job), vector) in jobs.into_iter().zip(vectors) {
            replacements[row_index].push((slot_id, vector));
        }
    }
    for (row_index, (plan, resolved)) in plans.iter().zip(&replacements).enumerate() {
        if plan.jobs.len() != resolved.len() {
            return Err(measurement_error(
                "Calyx measurement batch left deferred jobs unresolved",
                format!(
                    "row_index={row_index} planned={} resolved={}",
                    plan.jobs.len(),
                    resolved.len()
                ),
            ));
        }
    }
    Ok(replacements)
}

fn measure_text(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    text: &str,
) -> StorageResult<SlotVector> {
    measure_input(
        panel_name,
        lens,
        Input::new(Modality::Structured, text.as_bytes()),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "all current call sites pass Copy numeric primitives and by-value keeps slot construction readable"
)]
fn measure_number<T>(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    value: T,
) -> StorageResult<SlotVector>
where
    T: ToString,
{
    measure_text(panel_name, lens, &value.to_string())
}

fn measure_float(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    value: f64,
) -> StorageResult<SlotVector> {
    if !value.is_finite() {
        return Err(measurement_error("non-finite numeric lens input", value));
    }
    measure_text(panel_name, lens, &value.to_string())
}

/// Saturates a rank input at the frozen bounds its lens declares (#2030).
///
/// `syn_scalar_rank` fails closed outside `[min, max]`, and that is correct:
/// the domain is part of the lens identity (`syn_scalar_rank:{min}:{max}`), so
/// a silently widened domain would mean two different encodings sharing one
/// lens id.
///
/// The defect is on this side of the boundary. Several quantities measured this
/// way are *unbounded counters*, not naturally bounded ones. `line_no` is the
/// clearest: an agent transcript grows without limit, so line `10_001` of a
/// long run is an ordinary value, not a corrupt one. Feeding it raw made the
/// lens reject the record, and because a backfill page aborts on its first
/// unmeasurable record, ONE long transcript stalled the entire
/// `syn-agent-transcript-v1` panel indefinitely: 107 of 111 backfill attempts
/// failed with `pages=0 inserted=0 anchored=0`, pinning five panels below the
/// 0.95 coverage floor and leaving `126_184` transcript records unmeasured.
///
/// Saturation is the semantics this module already documents for bounded
/// encodings. The frozen scales beside [`count_norm`] are described as "the
/// order of magnitude at which the field stops discriminating, not a maximum",
/// and `count_norm`/`ratio_norm` both `.clamp(0.0, 1.0)` for exactly this
/// reason. A `12_000`-line and a `20_000`-line transcript both reading 1.0 on a
/// retrieval-only ordering ordinate is the intended statement, just as a
/// 4-hour and an 8-hour episode both read ~1.0 on `EP_DURATION_SCALE_MS`.
///
/// Widening the frozen range was rejected: it changes the lens id, so it forces
/// a panel version bump and a full re-measure of every stored record, and it
/// only moves the cliff instead of removing it. The declared panel contract in
/// the registry lens tables is deliberately left untouched.
fn saturating_rank_input(value: u64, min_micros: i64, max_micros: i64) -> u64 {
    // `.max(0)` first, so the sign is provably gone before the cast.
    let lower = (min_micros / RANK_BOUND_MICROS_PER_UNIT)
        .max(0)
        .cast_unsigned();
    let upper = (max_micros / RANK_BOUND_MICROS_PER_UNIT)
        .max(0)
        .cast_unsigned();
    // `clamp` panics when lo > hi; the lens itself rejects min >= max, but this
    // helper must not be the thing that panics if that ever changes.
    value.clamp(lower, upper.max(lower))
}

/// The only way this module measures a `syn_scalar_rank`.
///
/// Bounds are passed once and used for BOTH the lens declaration and the input
/// saturation, so the two cannot drift and a caller cannot forget to saturate.
/// That structural coupling is the actual fix for #2030 — clamping at four
/// individual call sites would have left the fifth to be written later.
fn measure_scalar_rank(
    panel_name: &'static str,
    lens_name: &'static str,
    min_micros: i64,
    max_micros: i64,
    value: u64,
) -> StorageResult<SlotVector> {
    measure_number(
        panel_name,
        AlgorithmicLens::syn_scalar_rank(lens_name, Modality::Structured, min_micros, max_micros),
        saturating_rank_input(value, min_micros, max_micros),
    )
}

fn measure_json<T>(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    value: &T,
) -> StorageResult<SlotVector>
where
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(value).map_err(|source| StorageError::EncodeJson {
        type_name: "calyx_constellation_slot_input",
        source,
    })?;
    measure_input(panel_name, lens, Input::new(Modality::Structured, bytes))
}

fn optional_json_slot<T>(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    value: Option<&T>,
) -> StorageResult<SlotVector>
where
    T: Serialize,
{
    value.map_or_else(
        || Ok(absent(AbsentReason::NotApplicable)),
        |value| measure_json(panel_name, lens, value),
    )
}

fn optional_json_slice_slot<T>(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    value: &[T],
) -> StorageResult<SlotVector>
where
    T: Serialize,
{
    if value.is_empty() {
        Ok(absent(AbsentReason::NotApplicable))
    } else {
        measure_json(panel_name, lens, value)
    }
}

fn optional_hash_slot(
    panel_name: &'static str,
    lens_name: &'static str,
    value: Option<&str>,
    dim: u32,
) -> StorageResult<SlotVector> {
    value.and_then(non_empty).map_or_else(
        || Ok(absent(AbsentReason::NotApplicable)),
        |text| {
            measure_text(
                panel_name,
                AlgorithmicLens::syn_hash(lens_name, Modality::Structured, dim),
                text,
            )
        },
    )
}

fn optional_onehot_slot(
    panel_name: &'static str,
    lens_name: &'static str,
    value: Option<&str>,
    dim: u32,
) -> StorageResult<SlotVector> {
    value.and_then(non_empty).map_or_else(
        || Ok(absent(AbsentReason::NotApplicable)),
        |text| {
            measure_text(
                panel_name,
                AlgorithmicLens::syn_one_hot(lens_name, Modality::Structured, dim),
                text,
            )
        },
    )
}

/// Builds and atomically publishes one complete graph-position snapshot.
///
/// The caller supplies the exact source MVCC sequence used to aggregate
/// `transitions`. Empty graphs and zero-count edges fail before generation
/// allocation. Publication independently reads Registry, Graph, Base, and Slot
/// rows before returning.
///
/// # Errors
///
/// Returns a structured error when transitions are invalid, graph measurement
/// fails, or atomic publication and independent readback cannot be completed.
#[expect(
    clippy::too_many_lines,
    reason = "graph validation, measurement, generation allocation, atomic publication, and readback form one snapshot transaction"
)]
pub fn publish_graph_position_snapshot(
    vault: &SynapseCalyxVault,
    kind: GraphPositionKind,
    source_seq: u64,
    created_at_ms: u64,
    transitions: &[(String, String, u64)],
) -> StorageResult<SynapseCalyxDerivedSnapshotReadback> {
    if transitions.is_empty() {
        return Err(measurement_error(
            "graph-position snapshot has no transitions",
            kind.panel_name(),
        ));
    }
    if transitions
        .iter()
        .any(|(src, dst, count)| src.trim().is_empty() || dst.trim().is_empty() || *count == 0)
    {
        return Err(measurement_error(
            "graph-position transitions require non-blank endpoints and positive counts",
            kind.panel_name(),
        ));
    }
    if let Some((src, dst, count)) = transitions
        .iter()
        .find(|(_, _, count)| *count > MAX_EXACT_F64_INT)
    {
        return Err(measurement_error(
            "graph-position transition count",
            format!(
                "transition {src:?}->{dst:?} count {count} exceeds f64's exact integer range {MAX_EXACT_F64_INT}"
            ),
        ));
    }
    let snapshot = graph_snapshot_fingerprint(transitions);
    let operation_id = sha256_hex(
        &[
            b"synapse-derived-graph-operation-v1".as_slice(),
            kind.panel_name().as_bytes(),
            &source_seq.to_be_bytes(),
            &snapshot.to_be_bytes(),
        ]
        .concat(),
    );
    let mut names = BTreeSet::new();
    for (src, dst, _) in transitions {
        names.insert(src.clone());
        names.insert(dst.clone());
    }
    let ids = names
        .iter()
        .map(|name| (name.clone(), graph_node_cx_id(kind, name)))
        .collect::<BTreeMap<_, _>>();
    let edges = transitions
        .iter()
        .map(|(src, dst, count)| TransitionEdge {
            src: ids[src],
            dst: ids[dst],
            count: exact_u64_as_f64(*count),
        })
        .collect::<Vec<_>>();
    let graph = build_transition_graph(&edges)
        .map_err(|error| measurement_error("build transition graph", error))?;
    let structural = structural_signatures(&graph, StructuralParams::default())
        .map_err(|error| measurement_error("measure structural signatures", error))?;

    // #2081: the generation is minted only after the structure has actually
    // been measured.
    //
    // Every input to this publish that can fail — the transition graph, and the
    // spectral/betweenness/PageRank pass over it — is now upstream of the
    // allocator. That ordering matters because an allocated generation cannot
    // be given back: owner rows are permanent by design (attribution depends on
    // them), and `MAX_OWNERS` is a hard cliff. Under the previous ordering a
    // publisher whose compute failed deterministically minted one permanent,
    // zero-row, un-retirable owner per tick — and a zero-row generation
    // produces no `Base` census entry, so none of #2062's three safety nets
    // could see it: `dynamic_panels_multi_live` read `[]`, the unretired sweep
    // never fired, and the ORPHANED error's own advice ("swept by the next
    // successful publish") was unreachable because there was never going to be
    // a next successful publish. Twenty allocator-live generations read as one
    // on the public census.
    //
    // Nothing between here and the commit can fail without the vault having
    // already accepted rows, so what remains after this point is the ordinary
    // #2062 orphan case the retirement ledger was built for.
    vault
        .reserve_panel_generations(&derived_snapshot_base_generations())
        .map_err(|error| measurement_error("reserve graph panel generation", error))?;
    let allocation = vault
        .allocate_panel_generation(kind.panel_name(), &operation_id)
        .map_err(|error| measurement_error("allocate graph panel generation", error))?;
    let panel_version = allocation.panel_generation;

    let mut neighbors = BTreeMap::<String, BTreeSet<String>>::new();
    for (src, dst, _) in transitions {
        neighbors
            .entry(src.clone())
            .or_default()
            .insert(dst.clone());
        neighbors
            .entry(dst.clone())
            .or_default()
            .insert(src.clone());
    }
    let contract = syn_graph_position_panel_contract(kind, panel_version, snapshot, created_at_ms)?;
    let vault_id = vault.vault_id_value();
    let mut constellations = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let observed = structural.get(&ids[name]).ok_or_else(|| {
            measurement_error("structural pass omitted a graph node", kind.panel_name())
        })?;
        let signature = GraphPositionSignature {
            in_degree: observed.in_degree as u64,
            out_degree: observed.out_degree as u64,
            total_degree: observed.total_degree as u64,
            betweenness: observed.betweenness,
            eigenvector: observed.eigenvector,
            pagerank: observed.pagerank,
            clustering: observed.clustering,
            neighbor_labels: neighbors.get(name).into_iter().flatten().cloned().collect(),
        };
        let identity = graph_position_identity_bytes(kind, snapshot, name);
        let context = NativeConstellationContext {
            vault_id,
            cx_id: vault.cx_id_for_input(&identity, panel_version),
            created_at_ms,
            next_ledger_seq: source_seq.saturating_add(index as u64).saturating_add(1),
        };
        constellations.push(build_graph_position_constellation(
            context,
            kind,
            panel_version,
            snapshot,
            name,
            &signature,
        )?);
    }
    let mut graph_rows = Vec::with_capacity(transitions.len() + 1);
    graph_rows.push(SynapseCalyxDerivedGraphRow {
        key: derived_graph_key(kind, snapshot, b"manifest"),
        value: serde_json::to_vec(&json!({
            "schema_version": 1,
            "panel_name": kind.panel_name(),
            "panel_version": panel_version,
            "source_seq": source_seq,
            "snapshot": snapshot,
            "node_count": names.len(),
            "edge_count": transitions.len(),
        }))
        .map_err(|error| measurement_error("encode graph snapshot manifest", error))?,
    });
    for (index, (src, dst, count)) in transitions.iter().enumerate() {
        graph_rows.push(SynapseCalyxDerivedGraphRow {
            key: derived_graph_key(kind, snapshot, &(index as u64).to_be_bytes()),
            value: serde_json::to_vec(&json!({"src": src, "dst": dst, "count": count}))
                .map_err(|error| measurement_error("encode graph snapshot edge", error))?,
        });
    }
    vault
        .publish_derived_snapshot(&SynapseCalyxDerivedSnapshotRequest {
            panel_name: kind.panel_name(),
            operation_id: &operation_id,
            panel: contract.panel,
            registry: contract.registry,
            constellations,
            graph_rows,
            source_seq,
            snapshot,
        })
        .map_err(|error| measurement_error("publish graph-position snapshot", error))
}

/// Builds and atomically publishes one document/URL hierarchy snapshot.
///
/// Every supplied path contributes all of its ancestor prefixes. The persisted
/// records therefore describe both leaves and internal hierarchy nodes, making
/// subtree size and sibling position independently inspectable.
///
/// # Errors
///
/// Returns a structured error when a path is invalid, hierarchy measurement
/// fails, or atomic publication and independent readback cannot be completed.
#[expect(
    clippy::too_many_lines,
    reason = "path expansion, measurement, generation allocation, atomic publication, and readback form one snapshot transaction"
)]
pub fn publish_path_hierarchy_snapshot(
    vault: &SynapseCalyxVault,
    source_seq: u64,
    created_at_ms: u64,
    paths: &[String],
) -> StorageResult<SynapseCalyxDerivedSnapshotReadback> {
    let mut node_components = BTreeMap::<String, Vec<String>>::new();
    for path in paths {
        let components = hierarchy_components(path);
        if components.is_empty() {
            return Err(measurement_error(
                "build path hierarchy snapshot",
                "path is blank or contains no hierarchy components",
            ));
        }
        for depth in 1..=components.len() {
            let prefix = components[..depth].join("/");
            node_components
                .entry(prefix)
                .or_insert_with(|| components[..depth].to_vec());
        }
    }
    if node_components.is_empty() {
        return Err(measurement_error(
            "build path hierarchy snapshot",
            "hierarchy snapshot has no paths",
        ));
    }
    let mut children = BTreeMap::<String, BTreeSet<String>>::new();
    let transitions = path_hierarchy_transitions(paths)?;
    for (node, components) in &node_components {
        if components.len() <= 1 {
            continue;
        }
        let parent = components[..components.len() - 1].join("/");
        children
            .entry(parent.clone())
            .or_default()
            .insert(node.clone());
    }
    let snapshot = graph_snapshot_fingerprint(&transitions);
    let operation_id = sha256_hex(
        &[
            b"synapse-derived-path-operation-v1".as_slice(),
            &source_seq.to_be_bytes(),
            &snapshot.to_be_bytes(),
        ]
        .concat(),
    );
    vault
        .reserve_panel_generations(&derived_snapshot_base_generations())
        .map_err(|error| measurement_error("reserve path panel generation", error))?;
    let allocation = vault
        .allocate_panel_generation(SYN_PATH_HIERARCHY_PANEL_NAME, &operation_id)
        .map_err(|error| measurement_error("allocate path panel generation", error))?;
    let panel_version = allocation.panel_generation;
    let contract = syn_path_hierarchy_panel_contract(panel_version, snapshot, created_at_ms)?;
    let vault_id = vault.vault_id_value();
    let mut constellations = Vec::with_capacity(node_components.len());
    for (index, (node, components)) in node_components.iter().enumerate() {
        let parent = if components.len() > 1 {
            Some(components[..components.len() - 1].join("/"))
        } else {
            None
        };
        let siblings = parent
            .as_ref()
            .and_then(|value| children.get(value))
            .cloned()
            .unwrap_or_else(|| BTreeSet::from([node.clone()]));
        let sibling_rank = siblings.iter().position(|value| value == node).unwrap_or(0) as u64;
        let subtree_size = node_components
            .keys()
            .filter(|candidate| {
                *candidate == node
                    || candidate
                        .strip_prefix(node)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
            .count() as u64;
        let signature = PathPositionSignature {
            depth: (components.len() - 1) as u64,
            sibling_rank,
            sibling_count: siblings.len() as u64,
            subtree_size,
            ancestor_count: (components.len() - 1) as u64,
            path_len: node.len() as u64,
            is_root: components.len() == 1,
            is_leaf: !children.contains_key(node),
            ancestor_components: components[..components.len() - 1].to_vec(),
            path_hash_key: node.clone(),
        };
        let identity = path_position_identity_bytes(snapshot, node);
        let context = NativeConstellationContext {
            vault_id,
            cx_id: vault.cx_id_for_input(&identity, panel_version),
            created_at_ms,
            next_ledger_seq: source_seq.saturating_add(index as u64).saturating_add(1),
        };
        constellations.push(build_path_hierarchy_constellation(
            context,
            panel_version,
            snapshot,
            node,
            &signature,
        )?);
    }
    let mut graph_rows = Vec::with_capacity(transitions.len() + 1);
    graph_rows.push(SynapseCalyxDerivedGraphRow {
        key: derived_path_graph_key(snapshot, b"manifest"),
        value: serde_json::to_vec(&json!({
            "schema_version": 1,
            "panel_name": SYN_PATH_HIERARCHY_PANEL_NAME,
            "panel_version": panel_version,
            "source_seq": source_seq,
            "snapshot": snapshot,
            "node_count": node_components.len(),
            "edge_count": transitions.len(),
        }))
        .map_err(|error| measurement_error("encode path snapshot manifest", error))?,
    });
    for (index, (parent, child, count)) in transitions.iter().enumerate() {
        graph_rows.push(SynapseCalyxDerivedGraphRow {
            key: derived_path_graph_key(snapshot, &(index as u64).to_be_bytes()),
            value: serde_json::to_vec(&json!({
                "parent": parent,
                "child": child,
                "count": count,
            }))
            .map_err(|error| measurement_error("encode path snapshot edge", error))?,
        });
    }
    vault
        .publish_derived_snapshot(&SynapseCalyxDerivedSnapshotRequest {
            panel_name: SYN_PATH_HIERARCHY_PANEL_NAME,
            operation_id: &operation_id,
            panel: contract.panel,
            registry: contract.registry,
            constellations,
            graph_rows,
            source_seq,
            snapshot,
        })
        .map_err(|error| measurement_error("publish path hierarchy snapshot", error))
}

fn derived_snapshot_base_generations() -> Vec<(String, u32)> {
    vec![
        (
            SYN_GRAPHPOS_APP_PANEL_NAME.to_owned(),
            SYN_GRAPHPOS_APP_PANEL_VERSION,
        ),
        (
            SYN_GRAPHPOS_PROCESS_PANEL_NAME.to_owned(),
            SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
        ),
        (
            SYN_PATH_HIERARCHY_PANEL_NAME.to_owned(),
            SYN_PATH_HIERARCHY_PANEL_VERSION,
        ),
    ]
}

/// Returns the canonical parent/child edge set used to fingerprint a path
/// hierarchy snapshot.
///
/// # Errors
///
/// Returns a structured error when any supplied path is blank or has no hierarchy components.
pub fn path_hierarchy_transitions(paths: &[String]) -> StorageResult<Vec<(String, String, u64)>> {
    let mut transitions = BTreeSet::new();
    for path in paths {
        let components = hierarchy_components(path);
        if components.is_empty() {
            return Err(measurement_error(
                "build path hierarchy transitions",
                "path is blank or contains no hierarchy components",
            ));
        }
        for depth in 2..=components.len() {
            transitions.insert((
                components[..depth - 1].join("/"),
                components[..depth].join("/"),
                1,
            ));
        }
    }
    Ok(transitions.into_iter().collect())
}

fn hierarchy_components(path: &str) -> Vec<String> {
    path.split(['/', '\\'])
        .map(str::trim)
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect()
}

fn derived_path_graph_key(snapshot: u64, suffix: &[u8]) -> Vec<u8> {
    let mut key = b"derived-path\0v1\0".to_vec();
    append_framed(&mut key, &snapshot.to_be_bytes());
    append_framed(&mut key, suffix);
    key
}

fn graph_node_cx_id(kind: GraphPositionKind, name: &str) -> CxId {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-derived-graph-node-v1");
    hasher.update(kind.identity_tag());
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    CxId::from_bytes(bytes)
}

fn derived_graph_key(kind: GraphPositionKind, snapshot: u64, suffix: &[u8]) -> Vec<u8> {
    let mut key = b"derived-graph\0v1\0".to_vec();
    append_framed(&mut key, kind.panel_name().as_bytes());
    append_framed(&mut key, &snapshot.to_be_bytes());
    append_framed(&mut key, suffix);
    key
}

/// Builds the immutable contract for one derived graph-position snapshot.
///
/// Unlike built-in source-row panels, the generation is allocated when the
/// whole-graph snapshot is published. The snapshot fingerprint is part of the
/// signature lens id, so callers must persist this exact contract beside the
/// derived rows and must never infer it from the logical panel name.
///
/// # Errors
///
/// Returns a structured error when either frozen content slot cannot be
/// registered or the resulting panel lacks a graded dense lens.
pub fn syn_graph_position_panel_contract(
    kind: GraphPositionKind,
    panel_version: u32,
    snapshot: u64,
    created_at_ms: u64,
) -> StorageResult<SynActivePanelContract> {
    let mut registry = Registry::new();
    let slots = vec![
        syn_content_slot(
            kind.signature_slot(),
            "syn.graphpos.signature.v1",
            RegistryAlgorithmicLens::syn_graph_signature(
                "syn.graphpos.signature.v1",
                Modality::Structured,
                snapshot,
            ),
            panel_version,
            &mut registry,
        )?,
        syn_content_slot(
            kind.neighbors_slot(),
            "syn.graphpos.neighbor_histogram.v1",
            RegistryAlgorithmicLens::syn_multi_hot(
                "syn.graphpos.neighbor_histogram.v1",
                Modality::Structured,
                GP_NEIGHBOR_HISTOGRAM_DIM,
            ),
            panel_version,
            &mut registry,
        )?,
    ];
    assert_panel_carries_graded_dense_lens(panel_version, &slots, &registry)?;
    Ok(SynActivePanelContract {
        panel: Panel {
            version: panel_version,
            slots,
            created_at: created_at_ms,
            kernel_ref: None,
            guard_ref: None,
        },
        registry,
    })
}

/// Builds the immutable contract for one derived hierarchy snapshot.
///
/// # Errors
///
/// Returns a structured error when either frozen content slot cannot be
/// registered or the resulting panel lacks a graded dense lens.
pub fn syn_path_hierarchy_panel_contract(
    panel_version: u32,
    snapshot: u64,
    created_at_ms: u64,
) -> StorageResult<SynActivePanelContract> {
    let mut registry = Registry::new();
    let slots = vec![
        syn_content_slot(
            PH_SLOT_SIGNATURE,
            "syn.path_hierarchy.signature.v1",
            RegistryAlgorithmicLens::syn_path_signature(
                "syn.path_hierarchy.signature.v1",
                Modality::Structured,
                snapshot,
            ),
            panel_version,
            &mut registry,
        )?,
        syn_content_slot(
            PH_SLOT_ANCESTORS,
            "syn.path_hierarchy.ancestor_set.v1",
            RegistryAlgorithmicLens::syn_multi_hot(
                "syn.path_hierarchy.ancestor_set.v1",
                Modality::Structured,
                PH_ANCESTOR_DIM,
            ),
            panel_version,
            &mut registry,
        )?,
        syn_content_slot(
            PH_SLOT_PATH_HASH,
            "syn.path_hierarchy.path_hash.v1",
            RegistryAlgorithmicLens::syn_hash(
                "syn.path_hierarchy.path_hash.v1",
                Modality::Structured,
                PH_PATH_HASH_DIM,
            ),
            panel_version,
            &mut registry,
        )?,
    ];
    assert_panel_carries_graded_dense_lens(panel_version, &slots, &registry)?;
    Ok(SynActivePanelContract {
        panel: Panel {
            version: panel_version,
            slots,
            created_at: created_at_ms,
            kernel_ref: None,
            guard_ref: None,
        },
        registry,
    })
}

fn action_panel_slots(panel_version: u32, registry: &mut Registry) -> StorageResult<Vec<Slot>> {
    let mut slots = vec![
        syn_content_slot(
            ACT_SLOT_KIND_ONEHOT,
            "syn.action.kind_onehot.v2",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.action.kind_onehot.v2",
                Modality::Structured,
                64,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            ACT_SLOT_TARGET_HASH,
            "syn.action.target_hash.v2",
            RegistryAlgorithmicLens::syn_hash(
                "syn.action.target_hash.v2",
                Modality::Structured,
                2048,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            ACT_SLOT_RECORD_VECTOR,
            "syn.action.record_vector.v2",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.action.record_vector.v2",
                Modality::Structured,
                32,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            ACT_SLOT_HOUR_CYCLIC,
            "syn.action.hour_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.action.hour_cyclic.v1",
                Modality::Structured,
                24,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            ACT_SLOT_DOW_CYCLIC,
            "syn.action.dow_cyclic.v1",
            RegistryAlgorithmicLens::syn_cyclic_time(
                "syn.action.dow_cyclic.v1",
                Modality::Structured,
                7,
            ),
            panel_version,
            registry,
        )?,
        // #2050's graded dense target lane beside slot 49's exact-match hash.
        syn_content_slot(
            ACT_SLOT_TARGET_VECTOR,
            "syn.action.target_vector.v2",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.action.target_vector.v2",
                Modality::Structured,
                ACT_TARGET_VECTOR_DIM,
            ),
            panel_version,
            registry,
        )?,
        // Point-in-time request cause, independently of target identity.
        syn_content_slot(
            ACT_SLOT_REQUEST_VECTOR,
            "syn.action.request_vector.v1",
            RegistryAlgorithmicLens::syn_record_vector_unit_fields(
                "syn.action.request_vector.v1",
                Modality::Structured,
                ACT_REQUEST_VECTOR_DIM,
            ),
            panel_version,
            registry,
        )?,
    ];
    slots.extend(action_request_class_panel_slots(panel_version, registry)?);
    Ok(slots)
}

fn action_request_class_panel_slots(
    panel_version: u32,
    registry: &mut Registry,
) -> StorageResult<[Slot; 2]> {
    Ok([
        syn_content_slot(
            ACT_SLOT_REQUEST_SIZE_CLASS,
            "syn.action.request_size_class.v1",
            RegistryAlgorithmicLens::syn_one_hot_index(
                "syn.action.request_size_class.v1",
                Modality::Structured,
                ACT_REQUEST_SIZE_CLASS_LEVELS,
            ),
            panel_version,
            registry,
        )?,
        syn_content_slot(
            ACT_SLOT_REQUEST_SHAPE_CLASS,
            "syn.action.request_shape_class.v1",
            RegistryAlgorithmicLens::syn_one_hot_index(
                "syn.action.request_shape_class.v1",
                Modality::Structured,
                ACT_REQUEST_SHAPE_CLASS_LEVELS,
            ),
            panel_version,
            registry,
        )?,
    ])
}

fn optional_onehot_index_slot(
    panel_name: &'static str,
    lens_name: &'static str,
    value: Option<u32>,
    levels: u32,
) -> StorageResult<SlotVector> {
    value.map_or_else(
        || Ok(absent(AbsentReason::NotApplicable)),
        |value| {
            measure_number(
                panel_name,
                AlgorithmicLens::syn_one_hot_index(lens_name, Modality::Structured, levels),
                value,
            )
        },
    )
}

fn optional_log1p_slot(
    panel_name: &'static str,
    lens_name: &'static str,
    value: Option<u64>,
) -> StorageResult<SlotVector> {
    value.map_or_else(
        || Ok(absent(AbsentReason::NotApplicable)),
        |value| {
            measure_number(
                panel_name,
                AlgorithmicLens::syn_scalar_log1p(lens_name, Modality::Structured),
                value,
            )
        },
    )
}

fn optional_rank_slot(
    panel_name: &'static str,
    lens_name: &'static str,
    value: Option<u64>,
    min_micros: i64,
    max_micros: i64,
) -> StorageResult<SlotVector> {
    value.map_or_else(
        || Ok(absent(AbsentReason::NotApplicable)),
        |value| measure_scalar_rank(panel_name, lens_name, min_micros, max_micros, value),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "slot ids and frozen lens names are part of each panel's stable public contract"
)]
fn insert_time_slots(
    slots: &mut BTreeMap<SlotId, SlotVector>,
    panel_name: &'static str,
    hour_slot: SlotId,
    dow_slot: SlotId,
    hour_lens_name: &'static str,
    dow_lens_name: &'static str,
    ts_ns: Option<u64>,
) -> StorageResult<()> {
    if let Some(ts_ns) = ts_ns {
        let (hour, dow) = utc_hour_and_dow(ts_ns);
        slots.insert(
            hour_slot,
            measure_number(
                panel_name,
                AlgorithmicLens::syn_cyclic_time(hour_lens_name, Modality::Structured, 24),
                hour,
            )?,
        );
        slots.insert(
            dow_slot,
            measure_number(
                panel_name,
                AlgorithmicLens::syn_cyclic_time(dow_lens_name, Modality::Structured, 7),
                dow,
            )?,
        );
    } else {
        slots.insert(hour_slot, absent(AbsentReason::NotApplicable));
        slots.insert(dow_slot, absent(AbsentReason::NotApplicable));
    }
    Ok(())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the helper owns freshly constructed lens/input values at every call site and immediately measures them"
)]
fn measure_input(
    panel_name: &'static str,
    lens: AlgorithmicLens,
    input: Input,
) -> StorageResult<SlotVector> {
    let (lens, input) = match defer_measurement(lens, input, DeferredMeasurementPolicy::Strict) {
        Ok(()) => return Ok(deferred_measurement_placeholder()),
        Err(unplanned) => *unplanned,
    };
    lens.measure(&input).map_err(|source| {
        measurement_error(
            "Calyx Syn* lens measurement failed",
            format!("{panel_name}: {}: {source}", lens.id()),
        )
    })
}

/// Prefix of the `Absent{Error}` reason a lens refusal is recorded under.
///
/// Stable and matched on by the coverage readback, so "this lens refused this
/// row" is a countable fact rather than a log line (#1924).
pub const SLOT_REFUSED_ABSENT_PREFIX: &str = "lens_refused:";

/// Measures one text slot, degrading an **over-limit** refusal to a per-slot
/// `Absent{Error}` instead of failing the whole constellation (#1924).
///
/// ## Why the blast radius is per-slot, and why only for this one code
///
/// Failing closed on an over-long document is correct for the *lens*. It was
/// the *record* that was wrong: `build_agent_transcript_constellation` measured
/// every slot with `?`, so a row one token over the text lane's bound also lost
/// its role one-hot, its status one-hot, its event-kind hash, its token
/// scalars, its record vector and its temporal lenses. The row became
/// unmeasured rather than partially measured — the opposite of the per-slot
/// discipline #1915 established for estimator refusals.
///
/// The degrade is deliberately keyed to `CALYX_LENS_INPUT_TOO_LARGE` alone,
/// which is a property of the data. Every other lens error —
/// `CALYX_LENS_DIM_MISMATCH`, `CALYX_LENS_NUMERICAL_INVARIANT`,
/// `CALYX_LENS_FROZEN_VIOLATION` — is a property of the code, means the lens is
/// broken, and still aborts the record. Turning those into an absent slot would
/// be a silent fallback, which is exactly what this codebase refuses to do.
///
/// The recorded reason carries the lens id and the refusal text, so the readback
/// can answer "which rows lost which lens, and why" without re-deriving
/// anything from logs.
fn measure_text_or_absent(
    panel_name: &'static str,
    lens: &AlgorithmicLens,
    text: &str,
) -> StorageResult<SlotVector> {
    if non_empty(text).is_none() {
        return Ok(absent(AbsentReason::NotApplicable));
    }
    let input = Input::new(Modality::Structured, text.as_bytes());
    let (lens, input) = match defer_measurement(
        lens.clone(),
        input,
        DeferredMeasurementPolicy::OverLimitAbsent { panel_name },
    ) {
        Ok(()) => return Ok(deferred_measurement_placeholder()),
        Err(unplanned) => *unplanned,
    };
    match lens.measure(&input) {
        Ok(vector) => Ok(vector),
        Err(source) if source.code == CalyxErrorCode::LensInputTooLarge.code() => {
            Ok(refused_text_slot(panel_name, &lens, &source))
        }
        Err(source) => Err(measurement_error(
            "Calyx Syn* lens measurement failed",
            format!("{panel_name}: {}: {source}", lens.id()),
        )),
    }
}

fn refused_text_slot(
    panel_name: &'static str,
    lens: &AlgorithmicLens,
    source: &CalyxError,
) -> SlotVector {
    let lens_id = lens.id().to_string();
    synapse_telemetry::metrics::counter!(
        CALYX_SLOT_LENS_REFUSED_TOTAL,
        "panel" => panel_name,
        "lens" => lens_id.clone(),
    )
    .increment(1);
    tracing::warn!(
        code = "CALYX_SLOT_LENS_REFUSED",
        panel_name,
        lens_id = %lens_id,
        detail = %source.message,
        remediation = "the record keeps every other lens; query the panel's \
                       records_slot_refused coverage to see how often this fires",
        "a lens refused an over-limit input; the slot is Absent{{Error}} and the rest of the \
         constellation is measured"
    );
    absent(AbsentReason::Error(format!(
        "{SLOT_REFUSED_ABSENT_PREFIX}{lens_id}: {}",
        source.message
    )))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "call sites pass lightweight display values or owned error strings; taking by value avoids temporary lifetime plumbing in error construction"
)]
fn measurement_error(action: &'static str, detail: impl ToString) -> StorageError {
    StorageError::WriteFailed {
        cf_name: "calyx_constellation".to_owned(),
        detail: format!("{action}: {}", detail.to_string()),
    }
}

fn timeline_kind_name(kind: TimelineKind) -> StorageResult<String> {
    snake_case_name(kind, "TimelineKind")
}

fn agent_event_kind_name(kind: AgentEventKind) -> StorageResult<String> {
    snake_case_name(kind, "AgentEventKind")
}

fn agent_end_state_name(end_state: AgentEndState) -> StorageResult<String> {
    snake_case_name(end_state, "AgentEndState")
}

const fn agent_end_state_index(end_state: AgentEndState) -> u32 {
    match end_state {
        AgentEndState::Indeterminate => 0,
        AgentEndState::Success => 1,
        AgentEndState::Error => 2,
    }
}

fn optional_agent_end_state_name(
    end_state: Option<AgentEndState>,
) -> StorageResult<Option<String>> {
    end_state.map(agent_end_state_name).transpose()
}

fn gen_ai_operation_name(operation: GenAiOperationName) -> StorageResult<String> {
    snake_case_name(operation, "GenAiOperationName")
}

fn optional_gen_ai_operation_name(
    operation: Option<GenAiOperationName>,
) -> StorageResult<Option<String>> {
    operation.map(gen_ai_operation_name).transpose()
}

fn transcript_source_name(source: TranscriptSource) -> StorageResult<String> {
    snake_case_name(source, "TranscriptSource")
}

fn transcript_parse_status_name(status: TranscriptParseStatus) -> StorageResult<String> {
    snake_case_name(status, "TranscriptParseStatus")
}

const fn transcript_parse_status_index(status: TranscriptParseStatus) -> u32 {
    match status {
        TranscriptParseStatus::Parsed => 0,
        TranscriptParseStatus::Invalid => 1,
    }
}

fn transcript_role_name(role: TranscriptRole) -> StorageResult<String> {
    snake_case_name(role, "TranscriptRole")
}

fn optional_transcript_role_name(role: Option<TranscriptRole>) -> StorageResult<Option<String>> {
    role.map(transcript_role_name).transpose()
}

fn boundary_name(boundary: EpisodeBoundary) -> StorageResult<String> {
    snake_case_name(boundary, "EpisodeBoundary")
}

fn snake_case_name<T>(value: T, label: &'static str) -> StorageResult<String>
where
    T: Serialize,
{
    serde_json::to_value(value)
        .map_err(|source| StorageError::EncodeJson {
            type_name: label,
            source,
        })?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| measurement_error("enum did not serialize to a snake-case string", label))
}

const fn actor_kind(actor: &TimelineActor) -> &'static str {
    match actor {
        TimelineActor::Human => "human",
        TimelineActor::Agent { .. } => "agent",
    }
}

fn timeline_title(record: &TimelineRecord) -> Option<String> {
    payload_string(
        &record.payload,
        &["title", "window_title", "document_title", "tab_title"],
    )
}

fn episode_title_text(record: &EpisodeRecord) -> String {
    [record.title_first.as_deref(), record.title_last.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(non_empty)
        .collect::<Vec<_>>()
        .join("\n")
}

fn payload_string(payload: &Value, keys: &[&str]) -> Option<String> {
    let object = payload.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .and_then(non_empty)
        .map(str::to_owned)
}

fn payload_u64(payload: &Value, keys: &[&str]) -> Option<u64> {
    let object = payload.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

const fn utc_hour_and_dow(ts_ns: u64) -> (u64, u64) {
    let secs = ts_ns / NS_PER_SEC;
    let hour = (secs / SECS_PER_HOUR) % 24;
    let days = secs / SECS_PER_DAY;
    let dow_monday_zero = (days + 3) % 7;
    (hour, dow_monday_zero)
}

const fn interruption_ratio(record: &EpisodeRecord) -> f64 {
    let duration = record.duration_ms();
    if duration == 0 {
        0.0
    } else {
        ratio_u64(record.interrupted_ms, duration)
    }
}

/// The timeline record reduced to comparably-scaled numbers (#1963).
///
/// `syn_record_vector` multiplies each field's value by a signed hash of its
/// **path** and unit-normalizes the sum, so a raw magnitude decides the
/// resulting direction on its own: feeding `ts_unix_ms` (~1.7e12) beside a byte
/// count (~200) would make the vector a re-encoding of the timestamp and
/// nothing else. Every component here is mapped into roughly `[0, 1]` first,
/// which is what makes the direction a summary of the record rather than of its
/// largest unit.
///
/// Deliberately **not** included: the absolute timestamp. The nearest neighbour
/// in a dense event stream is always seconds away, so any absolute-time
/// component saturates the nearest-neighbour cosine at 1.0 — measured, not
/// assumed: manual FSV showed a half-circle encoding of the same
/// frozen event-time rank returning `distinct=1` over all 932 rows. Time enters
/// only as *position within* a day and a week, which is a genuine property of
/// the activity rather than a serial number.
///
/// Every field read here is declared in
/// `synapse_calyx::lens_provenance::SYN_SLOT_SOURCE_FIELDS` for slot 104.
#[expect(
    clippy::cast_precision_loss,
    reason = "day/week modulo values are below 604800 and exactly representable as f64"
)]
fn timeline_numeric_record(record: &TimelineRecord, raw_bytes: &[u8]) -> Value {
    let secs = record.ts_ns / NS_PER_SEC;
    let app = record.app.as_deref().unwrap_or("");
    let title = timeline_title(record).unwrap_or_default();
    let url_host = payload_string(&record.payload, &["url"])
        .as_deref()
        .and_then(url_host)
        .unwrap_or_default();
    json!({
        "kind_ordinal": timeline_kind_ordinal(record.kind),
        "actor_is_agent": f64::from(u8::from(matches!(record.actor, TimelineActor::Agent { .. }))),
        "day_fraction": (secs % SECS_PER_DAY) as f64 / SECS_PER_DAY as f64,
        "week_fraction": (secs % (7 * SECS_PER_DAY)) as f64 / (7 * SECS_PER_DAY) as f64,
        "has_app": present_fraction(app),
        "app_len_norm": log_len_norm(app, TL_APP_LEN_SCALE),
        "has_title": present_fraction(&title),
        "title_len_norm": log_len_norm(&title, TL_TITLE_LEN_SCALE),
        "has_url_host": present_fraction(&url_host),
        "url_host_len_norm": log_len_norm(&url_host, TL_URL_HOST_LEN_SCALE),
        "raw_len_norm": count_norm(
            u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
            TL_RAW_LEN_SCALE,
        ),
    })
}

/// Frozen ordinal position of a timeline kind on `[0, 1]`.
///
/// Written as an exhaustive match rather than a lookup over
/// [`TimelineKind`]'s declaration order so that adding a variant is a
/// compile error here: a new kind must be given an explicit position, and
/// appending one must not renumber the existing ones — that would change what
/// every already-measured record means without changing the lens id.
const fn timeline_kind_ordinal(kind: TimelineKind) -> f64 {
    // Denominator frozen at 11 = (12 declared kinds - 1). A thirteenth kind
    // takes position 12 and this denominator becomes 12, which is a genuine
    // re-measurement and therefore a new lens version, not an edit here.
    const LAST: f64 = 11.0;
    let index = match kind {
        TimelineKind::FocusChange => 0.0,
        TimelineKind::TitleChange => 1.0,
        TimelineKind::IdleStart => 2.0,
        TimelineKind::IdleEnd => 3.0,
        TimelineKind::SessionStart => 4.0,
        TimelineKind::SessionEnd => 5.0,
        TimelineKind::InteractionSummary => 6.0,
        TimelineKind::Clipboard => 7.0,
        TimelineKind::FileActivity => 8.0,
        TimelineKind::BrowserNav => 9.0,
        TimelineKind::DemoMarker => 10.0,
        TimelineKind::Purge => 11.0,
    };
    index / LAST
}

/// `1.0` when the field carries a value, `0.0` when it does not.
fn present_fraction(value: &str) -> f64 {
    f64::from(u8::from(!value.is_empty()))
}

/// A character count mapped into roughly `[0, 1]` by `ln(1+n) / ln(1+scale)`.
///
/// Log rather than linear because these lengths are heavy-tailed: a 400-character
/// title is not four times the record a 100-character one is, and a linear scale
/// would let one outlier dominate the whole vector's direction.
fn log_len_norm(value: &str, scale: f64) -> f64 {
    count_norm(
        u64::try_from(value.chars().count()).unwrap_or(u64::MAX),
        scale,
    )
}

/// A count, byte length or duration mapped into `[0, 1]` by
/// `ln(1+n) / ln(1+scale)`, clamped at the top.
///
/// Log rather than linear because these are heavy-tailed: a 40-minute episode
/// is not forty times the episode a one-minute one is, and on a linear scale one
/// outlier would own the whole vector's direction — the #1964 defect in
/// miniature. Clamping is deliberate winsorization: above `scale` the field
/// saturates and stops discriminating, which is an explicit, documented loss at
/// the tail rather than a silent takeover of every other field.
#[expect(
    clippy::cast_precision_loss,
    reason = "log normalization intentionally maps an integer magnitude into an approximate continuous feature before clamping"
)]
fn count_norm(value: u64, scale: f64) -> f64 {
    debug_assert!(scale > 0.0, "count_norm scale must be positive");
    ((value as f64).ln_1p() / scale.ln_1p()).clamp(0.0, 1.0)
}

/// Position within the day, in `[0, 1)`.
///
/// This is how a timestamp enters a record vector. The absolute instant must
/// not: it is a serial number seven orders of magnitude above every other field,
/// and #1964 measured what that does. Time-of-day is a genuine property of the
/// activity; the epoch offset is a property of the clock.
#[expect(
    clippy::cast_precision_loss,
    reason = "the modulo is below 86400 and exactly representable as f64"
)]
fn day_fraction_of(ts_ns: u64) -> f64 {
    ((ts_ns / NS_PER_SEC) % SECS_PER_DAY) as f64 / SECS_PER_DAY as f64
}

/// Position within the week, in `[0, 1)`. See [`day_fraction_of`].
#[expect(
    clippy::cast_precision_loss,
    reason = "the modulo is below 604800 and exactly representable as f64"
)]
fn week_fraction_of(ts_ns: u64) -> f64 {
    const SECS_PER_WEEK: u64 = 7 * SECS_PER_DAY;
    ((ts_ns / NS_PER_SEC) % SECS_PER_WEEK) as f64 / SECS_PER_WEEK as f64
}

/// A part-of-whole ratio clamped into `[0, 1]`, `0.0` when the whole is empty.
fn ratio_norm(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 || !part.is_finite() || !whole.is_finite() {
        0.0
    } else {
        (part / whole).clamp(0.0, 1.0)
    }
}

/// Frozen scales for [`episode_numeric_record`], one per field.
///
/// Each is the order of magnitude at which the field stops discriminating, not
/// a maximum: a 4-hour episode and an 8-hour episode both read ~1.0 on
/// `EP_DURATION_SCALE_MS`, and that is the intended statement.
/// Dimension of the episode record vector.
///
/// [`episode_numeric_record`] emits 11 fields, which the encoder places by a
/// signed hash of the field path. 64 buckets keeps the expected collision count
/// well under one; a collision merges two already-normalized components rather
/// than losing a field.
const EP_RECORD_VECTOR_DIM: u32 = 64;
const EP_DURATION_SCALE_MS: f64 = 3_600_000.0;
const EP_ROW_COUNT_SCALE: f64 = 1_000.0;
const EP_KEYSTROKE_SCALE: f64 = 5_000.0;
const EP_CLICK_SCALE: f64 = 1_000.0;
const EP_INTERRUPTION_SCALE: f64 = 50.0;
const EP_DISTINCT_TITLE_SCALE: f64 = 50.0;

/// Every field an episode's graded dense lens measures, on a comparable scale.
///
/// Rebuilt for #1964. The superseded shape fed `start_unix_ms` and
/// `end_unix_ms` raw; see [`SYN_EPISODE_PANEL_VERSION_PRE_1964`] for what that
/// cost. `end_ts_ns` is no longer read at all: an episode's extent is already
/// carried by `duration_norm`, and its end instant adds only a second copy of
/// the clock.
///
/// Every field here is declared in
/// `synapse_calyx::lens_provenance::SYN_SLOT_SOURCE_FIELDS` for slot 113, and
/// `syn_record_vector_unit_fields` refuses the record outright if any field
/// leaves `[-1, 1]` — so a future edit that reintroduces a raw magnitude fails
/// at measure time instead of quietly flattening the panel.
#[expect(
    clippy::cast_precision_loss,
    reason = "duration and event-count ratios are intentionally approximate continuous normalized features"
)]
fn episode_numeric_record(record: &EpisodeRecord) -> Value {
    let duration_ms = record.duration_ms();
    json!({
        "duration_norm": count_norm(duration_ms, EP_DURATION_SCALE_MS),
        "row_count_norm": count_norm(record.row_count, EP_ROW_COUNT_SCALE),
        "keystroke_norm": count_norm(record.keystroke_count, EP_KEYSTROKE_SCALE),
        "click_norm": count_norm(record.click_count, EP_CLICK_SCALE),
        "interruption_count_norm": count_norm(u64::from(record.interruption_count), EP_INTERRUPTION_SCALE),
        "interrupted_ratio": ratio_norm(record.interrupted_ms as f64, duration_ms as f64),
        "distinct_title_norm": count_norm(u64::from(record.distinct_title_count), EP_DISTINCT_TITLE_SCALE),
        "start_day_fraction": day_fraction_of(record.start_ts_ns),
        "start_week_fraction": week_fraction_of(record.start_ts_ns),
        "keystrokes_per_row": ratio_norm(record.keystroke_count as f64, record.row_count as f64),
        "clicks_per_row": ratio_norm(record.click_count as f64, record.row_count as f64),
    })
}

const AT_LINE_NO_SCALE: f64 = 1_000_000.0;
const AT_TURN_INDEX_SCALE: f64 = 10_000.0;
const AT_BYTES_SCALE: f64 = 1_000_000.0;
const AT_TOOL_RESULT_BYTES_SCALE: f64 = 10_000_000.0;
const AT_TOOL_CALL_COUNT_SCALE: f64 = 100.0;
const AT_TOKEN_SCALE: f64 = 10_000_000.0;
const AT_REASONING_TOKEN_SCALE: f64 = 1_000_000.0;
const AT_COST_MICRO_USD_SCALE: f64 = 100_000_000.0;
const AT_MODEL_USAGE_COUNT_SCALE: f64 = 100.0;

/// Comparable-scale transcript summary used by slot 110 (#1965).
///
/// Counts and byte lengths are log-bounded at frozen, corpus-sized ceilings;
/// the absolute timestamp is deliberately excluded and represented only by
/// periodic day/week position. `syn_record_vector_unit_fields` then enforces
/// the `[0, 1]` contract at measurement time.
fn agent_transcript_numeric_record_v2(record: &AgentTranscriptRecord) -> Value {
    json!({
        "line_no_norm": count_norm(record.line_no, AT_LINE_NO_SCALE),
        "turn_index_norm": count_norm(record.turn_index.unwrap_or(0), AT_TURN_INDEX_SCALE),
        "raw_line_bytes_norm": count_norm(record.raw_line_bytes, AT_BYTES_SCALE),
        "content_bytes_norm": count_norm(record.content_bytes.unwrap_or(0), AT_BYTES_SCALE),
        "content_truncated": f64::from(u8::from(record.content_truncated)),
        "tool_call_count_norm": count_norm(record.tool_calls.len() as u64, AT_TOOL_CALL_COUNT_SCALE),
        "tool_argument_bytes_norm": count_norm(transcript_tool_argument_bytes_total(record), AT_BYTES_SCALE),
        "tool_result_bytes_norm": count_norm(transcript_tool_result_bytes_total(record), AT_TOOL_RESULT_BYTES_SCALE),
        "usage_input_tokens_norm": count_norm(record
            .usage
            .as_ref()
            .and_then(|usage| usage.input_tokens)
            .unwrap_or(0), AT_TOKEN_SCALE),
        "usage_output_tokens_norm": count_norm(record
            .usage
            .as_ref()
            .and_then(|usage| usage.output_tokens)
            .unwrap_or(0), AT_TOKEN_SCALE),
        "usage_cache_read_input_tokens_norm": count_norm(record
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_input_tokens)
            .unwrap_or(0), AT_TOKEN_SCALE),
        "usage_cache_creation_input_tokens_norm": count_norm(record
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_creation_input_tokens)
            .unwrap_or(0), AT_TOKEN_SCALE),
        "usage_reasoning_output_tokens_norm": count_norm(record
            .usage
            .as_ref()
            .and_then(|usage| usage.reasoning_output_tokens)
            .unwrap_or(0), AT_REASONING_TOKEN_SCALE),
        "total_cost_micro_usd_norm": count_norm(record
            .usage
            .as_ref()
            .and_then(|usage| usage.total_cost_micro_usd)
            .unwrap_or(0), AT_COST_MICRO_USD_SCALE),
        "model_usage_count_norm": count_norm(record
            .usage
            .as_ref()
            .map_or(0, |usage| usage.model_usage.len()) as u64, AT_MODEL_USAGE_COUNT_SCALE),
        "usage_total_tokens_norm": count_norm(transcript_usage_total(record).unwrap_or(0), AT_TOKEN_SCALE),
        "day_fraction": day_fraction_of(record.ts_ns),
        "week_fraction": week_fraction_of(record.ts_ns),
    })
}

const fn bool_u64(value: bool) -> u64 {
    value as u64
}

fn agent_event_usage_total(record: &AgentEventRecord) -> Option<u64> {
    let input = record.attributes.usage_input_tokens;
    let output = record.attributes.usage_output_tokens;
    let cache_read = record.attributes.usage_cache_read_input_tokens;
    let cache_creation = record.attributes.usage_cache_creation_input_tokens;
    let mut total = input.unwrap_or(0).saturating_add(output.unwrap_or(0));
    if input.is_none() {
        total = total
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_creation.unwrap_or(0));
    }
    (input.is_some() || output.is_some() || cache_read.is_some() || cache_creation.is_some())
        .then_some(total)
}

fn transcript_usage_total(record: &AgentTranscriptRecord) -> Option<u64> {
    let usage = record.usage.as_ref()?;
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    let cache_read = usage.cache_read_input_tokens;
    let cache_creation = usage.cache_creation_input_tokens;
    let mut total = input.unwrap_or(0).saturating_add(output.unwrap_or(0));
    if input.is_none() {
        total = total
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_creation.unwrap_or(0));
    }
    (input.is_some() || output.is_some() || cache_read.is_some() || cache_creation.is_some())
        .then_some(total)
}

/// Anchor kind carrying the adjudicated outcome of one observed tool call.
///
/// Deliberately the same label `agent_events.rs` already writes for the ~29
/// calls Synapse brokers end to end, because it is the same claim about the
/// same kind of event. One kind, two evidence paths, one axis to measure bits
/// on.
pub const AGENT_TOOL_CALL_SUCCESS_ANCHOR_KIND: &str = "synapse:agent_tool_call_success";

/// `event_kind` written by `ambient_agents.rs::classify_user` for a line whose
/// content is a `tool_result` block. The adjudication below is scoped to
/// exactly these rows and nothing else.
pub const AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND: &str = "user/tool_result";

/// `TranscriptToolCall::status` for a result the provider explicitly marked
/// `is_error: true`.
pub const TRANSCRIPT_TOOL_STATUS_ERROR: &str = "error";
/// `TranscriptToolCall::status` for a result the provider explicitly marked
/// `is_error: false`.
///
/// Distinct from `status: None`, which means the field was **absent**. Both
/// adjudicate to success (see [`agent_transcript_tool_outcome`]), but they are
/// different observations and #1918 exists because two different things were
/// allowed to read as one once already.
pub const TRANSCRIPT_TOOL_STATUS_OK: &str = "ok";

/// Why a `tool_result` row carries no adjudicable outcome (#1926).
///
/// Deliberately **not** an error. A row in one of these states still gets its
/// durable evidence row and its constellation; it simply gets no anchor,
/// because there is no single outcome to state about it. Making it an error
/// would propagate out of `commit_transcript_chunk`, fail the chunk, hold the
/// ambient cursor, and wedge that session's ingestion permanently — collateral
/// damage on an unrelated subsystem, not fail-closed behaviour. Fail-closed
/// here means "never write an outcome I had to guess", and writing nothing
/// achieves exactly that.
///
/// Each variant is counted and labelled so the gap is visible in
/// `corpus_histogram`'s `tool_outcome` dimension rather than inferred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTranscriptUnadjudicable {
    /// The row carried more than one result block, so a row-level anchor would
    /// be an aggregation of several outcomes rather than an adjudication of one.
    ///
    /// Measured as zero across the whole real corpus (9,034 lines, every one
    /// carrying exactly one block), because Claude Code splits parallel results
    /// onto separate lines. That is a property of this transcript format, not
    /// of the protocol: the Messages API pairs two `tool_use` blocks with one
    /// user message carrying two `tool_result` blocks. So this is unreachable
    /// today and must not be assumed unreachable tomorrow.
    MultipleResults,
    /// The row carried a tool status outside the declared set, so its polarity
    /// is not something this code is entitled to decide.
    UndeclaredStatus,
}

impl AgentTranscriptUnadjudicable {
    /// Stable label for readbacks and histograms.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MultipleResults => "unadjudicable_multiple_results",
            Self::UndeclaredStatus => "unadjudicable_undeclared_status",
        }
    }
}

/// What one transcript row's tool outcome resolves to (#1926).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTranscriptToolOutcome {
    /// Not an observed tool result; there is nothing to adjudicate.
    NotAToolResult,
    /// A single observed result with a declared polarity.
    Adjudicated(AgentTranscriptToolAdjudication),
    /// An observed result this code will not decide the polarity of.
    Unadjudicable(AgentTranscriptUnadjudicable),
}

impl AgentTranscriptToolOutcome {
    /// Stable label for readbacks and histograms.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NotAToolResult => "no_tool_outcome",
            Self::Adjudicated(adjudication) => adjudication.label(),
            Self::Unadjudicable(reason) => reason.label(),
        }
    }
}

/// How a row's adjudicated tool outcome was observed (#1926).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTranscriptToolAdjudication {
    /// The provider wrote `is_error: true`.
    DeclaredError,
    /// The provider wrote `is_error: false`.
    DeclaredOk,
    /// The provider omitted `is_error`.
    OmittedIsError,
}

impl AgentTranscriptToolAdjudication {
    /// Stable label for readbacks and histograms.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DeclaredError => "declared_error",
            Self::DeclaredOk => "declared_ok",
            Self::OmittedIsError => "omitted_is_error",
        }
    }

    /// The adjudicated outcome this observation carries.
    #[must_use]
    pub const fn success(self) -> bool {
        !matches!(self, Self::DeclaredError)
    }
}

/// The **declared** adjudication of one observed tool call (#1926).
///
/// ## Why this is a declaration and not an inference
///
/// The Anthropic Messages API defines `is_error` on a `tool_result` block as an
/// *optional* boolean set to `true` when the call failed. Success is encoded by
/// writing `false` **or by omitting the field entirely** — the reference
/// lowering emits `is_error: true` on failure and `undefined` otherwise. So
/// "absent" is not missing data whose meaning must be guessed; it is the
/// protocol's own spelling of "not an error".
///
/// That is what makes `Bool(is_error != true)` an adjudication Synapse is
/// entitled to write as a grounded anchor rather than a provisional label, and
/// it is the same standard `SYNAPSE_DECLARED_ENUM_ADJUDICATIONS` holds an enum
/// anchor to: the split must already be made by a cited authority, never
/// invented by the consumer.
///
/// ## Why one anchor per row is exact, not an aggregation
///
/// Measured over the whole real corpus (54 session files): every one of 9,034
/// `user/tool_result` lines carries **exactly one** `tool_result` block, and
/// zero lines mix a success and an error. So a row is one tool call, and the
/// row-level anchor loses nothing. A row that ever carried more than one block,
/// or a mix, is refused below rather than silently reduced to a majority — an
/// aggregated outcome is not an adjudicated one.
///
/// ## Why this is total rather than fallible
///
/// It answers "what outcome does this row carry", and "none I can state" is a
/// real answer to that question, not a failure to answer it. An earlier draft
/// returned `Err` for a row it would not adjudicate; that error propagated
/// through `commit_transcript_chunk`, failed the chunk, held the ambient
/// cursor, and would have wedged a session's transcript ingestion permanently
/// the first time a legal multi-result line appeared. Refusing to guess must
/// cost an anchor, never the evidence.
///
/// The refusal is preserved and made *visible* instead: every unadjudicable row
/// carries a distinct label through `corpus_histogram`'s `tool_outcome`
/// dimension, so the gap is counted from the vault rather than inferred.
#[must_use]
pub fn agent_transcript_tool_outcome(record: &AgentTranscriptRecord) -> AgentTranscriptToolOutcome {
    if record.event_kind.as_deref() != Some(AGENT_TRANSCRIPT_TOOL_RESULT_EVENT_KIND)
        || record.status != TranscriptParseStatus::Parsed
    {
        return AgentTranscriptToolOutcome::NotAToolResult;
    }
    let [tool_call] = record.tool_calls.as_slice() else {
        return AgentTranscriptToolOutcome::Unadjudicable(
            AgentTranscriptUnadjudicable::MultipleResults,
        );
    };
    match tool_call.status.as_deref() {
        Some(TRANSCRIPT_TOOL_STATUS_ERROR) => {
            AgentTranscriptToolOutcome::Adjudicated(AgentTranscriptToolAdjudication::DeclaredError)
        }
        Some(TRANSCRIPT_TOOL_STATUS_OK) => {
            AgentTranscriptToolOutcome::Adjudicated(AgentTranscriptToolAdjudication::DeclaredOk)
        }
        None => {
            AgentTranscriptToolOutcome::Adjudicated(AgentTranscriptToolAdjudication::OmittedIsError)
        }
        Some(_) => AgentTranscriptToolOutcome::Unadjudicable(
            AgentTranscriptUnadjudicable::UndeclaredStatus,
        ),
    }
}

/// Anchor `source` recorded for an outcome adjudicated from an observed
/// transcript row, distinct from `synapse-agent-event` so a readback can tell
/// the two evidence paths apart without joining anything.
pub const SOURCE_AGENT_TRANSCRIPT_TOOL_RESULT: &str = "synapse-agent-transcript-tool-result";

/// The one ledger-payload shape every grounded anchor is stamped with.
///
/// Lives here rather than beside a caller so the schema string, the field set,
/// and the hashing decisions have exactly one definition. Values are hashed,
/// never carried verbatim, except for the numeric/boolean cases that cannot
/// leak content.
#[must_use]
pub fn grounding_anchor_ledger_payload(
    source_cf: &str,
    source_key: &[u8],
    source_value: &[u8],
    anchor: &GroundingAnchor,
) -> Value {
    json!({
        "schema": "synapse.grounding_anchor.v2",
        "source_cf": source_cf,
        "source_row_sha256": sha256_hex(source_key),
        "source_value_sha256": sha256_hex(source_value),
        "anchor_kind_sha256": sha256_hex(anchor.kind_label.as_bytes()),
        "anchor_value": grounding_anchor_value_payload(&anchor.value),
        "anchor_source_sha256": sha256_hex(anchor.source.as_bytes()),
        "observed_at_ms": anchor.observed_at_ms,
        "confidence": anchor.confidence,
    })
}

fn grounding_anchor_value_payload(value: &GroundingAnchorValue) -> Value {
    match value {
        GroundingAnchorValue::Bool(value) => json!({ "type": "bool", "value": value }),
        GroundingAnchorValue::Enum(value) => json!({
            "type": "enum",
            "value_sha256": sha256_hex(value.as_bytes()),
        }),
        GroundingAnchorValue::Number(value) => json!({ "type": "number", "value": value }),
        GroundingAnchorValue::Text(value) => json!({
            "type": "text",
            "value_sha256": sha256_hex(value.as_bytes()),
        }),
    }
}

/// Builds the grounded outcome anchor for one transcript row, or `None` when
/// the row carries no adjudicated outcome (#1926).
///
/// An unadjudicable row logs at warn and yields no anchor. It does not fail:
/// see [`agent_transcript_tool_outcome`] for why refusing to guess must never
/// cost the evidence row.
#[must_use]
pub fn agent_transcript_outcome_anchor(record: &AgentTranscriptRecord) -> Option<GroundingAnchor> {
    let adjudication = match agent_transcript_tool_outcome(record) {
        AgentTranscriptToolOutcome::NotAToolResult => return None,
        AgentTranscriptToolOutcome::Adjudicated(adjudication) => adjudication,
        AgentTranscriptToolOutcome::Unadjudicable(reason) => {
            tracing::warn!(
                code = "AGENT_TRANSCRIPT_TOOL_OUTCOME_UNADJUDICABLE",
                spawn_id = %record.spawn_id,
                line_no = record.line_no,
                reason = reason.label(),
                tool_calls = record.tool_calls.len(),
                remediation = "the evidence row and its constellation are intact; this row \
                               carries no grounded outcome. Query corpus_histogram's \
                               tool_outcome dimension to count how many rows are in this state \
                               before extending the declared adjudication",
                "an observed tool result carries no outcome this code is entitled to declare"
            );
            return None;
        }
    };
    Some(GroundingAnchor {
        kind_label: AGENT_TOOL_CALL_SUCCESS_ANCHOR_KIND.to_owned(),
        value: GroundingAnchorValue::Bool(adjudication.success()),
        source: SOURCE_AGENT_TRANSCRIPT_TOOL_RESULT.to_owned(),
        // The row's own ingestion timestamp. Nanoseconds to milliseconds is the
        // same reduction `agent_events.rs` applies, so both evidence paths land
        // on one time base.
        observed_at_ms: record.ts_ns / 1_000_000,
        confidence: 1.0,
    })
}

fn transcript_text(record: &AgentTranscriptRecord) -> String {
    let mut parts = Vec::new();
    if let Some(content) = record.content_summary.as_deref().and_then(non_empty) {
        parts.push(content.to_owned());
    }
    if let Some(source_error) = record.source_error.as_deref().and_then(non_empty) {
        parts.push(format!("source_error: {source_error}"));
    }
    if let Some(parse_error) = record.parse_error.as_deref().and_then(non_empty) {
        parts.push(format!("parse_error: {parse_error}"));
    }
    parts.join("\n")
}

/// Every piece of prose the transcript record carries (#1921).
///
/// [`transcript_text`] reads three optional fields, and on the real corpus all
/// three are empty on ~90% of rows: a `user/tool_result` line returns from
/// `ambient_agents.rs::classify_user` before `set_content` is ever called, and a
/// tool-use-only assistant turn skips it for the same reason. The prose on those
/// rows is not missing — it sits in `tool_calls[]`, 18.2x more of it by
/// character count than reaches [`transcript_text`], and until this projection
/// existed no lens read it.
///
/// The tool name is included deliberately and first: it is the single most
/// query-worthy token on a tool row ("what was I doing with `search_rebuild`"),
/// and it is the only part of a tool call that is a stable identifier rather
/// than free text.
///
/// Ordering is deterministic — content, then each tool call in the record's own
/// order, name before arguments before result. The lens is a bag-of-terms so
/// order does not change the vector, but a deterministic projection means a
/// re-measure of an unchanged row reproduces byte-identical bytes, which is what
/// makes the backfill idempotent and `reproduce` meaningful.
///
/// Bounding is inherited, not re-applied: every field concatenated here was
/// already truncated at ingest to its declared cap, and the measured p100 of the
/// result is 1,388 tokens against the lens's 4,096 limit. This function must
/// therefore never silently truncate — if a future record does exceed the limit,
/// the lens fails closed and names the row, which is the correct outcome.
fn transcript_full_text(record: &AgentTranscriptRecord) -> String {
    let mut parts = Vec::new();
    let base = transcript_text(record);
    if !base.is_empty() {
        parts.push(base);
    }
    for tool in &record.tool_calls {
        if let Some(name) = non_empty(&tool.tool_name) {
            parts.push(name.to_owned());
        }
        if let Some(arguments) = tool.arguments.as_deref().and_then(non_empty) {
            parts.push(arguments.to_owned());
        }
        if let Some(result) = tool.result_summary.as_deref().and_then(non_empty) {
            parts.push(result.to_owned());
        }
    }
    parts.join("\n")
}

fn transcript_tool_names(record: &AgentTranscriptRecord) -> String {
    record
        .tool_calls
        .iter()
        .filter_map(|tool| non_empty(&tool.tool_name))
        .collect::<Vec<_>>()
        .join("\n")
}

fn transcript_model_usage_names(record: &AgentTranscriptRecord) -> String {
    record
        .usage
        .as_ref()
        .map(|usage| {
            usage
                .model_usage
                .iter()
                .filter_map(|model| non_empty(&model.model))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn transcript_tool_argument_bytes_total(record: &AgentTranscriptRecord) -> u64 {
    record.tool_calls.iter().fold(0_u64, |sum, tool| {
        sum.saturating_add(tool.arguments_bytes.unwrap_or(0))
    })
}

fn transcript_tool_result_bytes_total(record: &AgentTranscriptRecord) -> u64 {
    record.tool_calls.iter().fold(0_u64, |sum, tool| {
        sum.saturating_add(tool.result_bytes.unwrap_or(0))
    })
}

fn action_identity(record: &Value) -> String {
    let tool = json_string(record, &["tool"]);
    let verb = json_string(record, &["verb"]);
    match (tool, verb) {
        (Some(tool), Some(verb)) => format!("{tool}:{verb}"),
        (Some(tool), None) => tool,
        (None, Some(verb)) => verb,
        (None, None) => {
            json_string(record, &["row_kind"]).unwrap_or_else(|| "unknown_action".to_owned())
        }
    }
}

/// Every JSON pointer the action target lane reads, in precedence order.
///
/// Shared by [`action_target_text`] and [`warn_action_target_absent_on_success`]
/// so the projection and the warning that reports its miss cannot name two
/// different search spaces.
///
/// ## The first two are the authoritative persisted paths (#2050)
///
/// `synapse-mcp`'s action audit writer (`server::action_audit::
/// write_action_audit_row_readback`) persists the session's bound target at two
/// top-level `CF_ACTION_LOG` paths:
///
/// * `agent_logical_foreground.target` — `target_claims::target_wire(&target)`,
///   written by `action_audit_agent_logical_foreground` only on the
///   `status: "set"` branch (a session that owns a logical foreground target).
/// * `foreground_lane.target` — the same `target_wire` value, written by
///   `action_audit_foreground_lane` on its `Ok(Some(target))` branch.
///
/// `TargetWire` is `#[serde(tag = "kind", rename_all = "snake_case")]`, so the
/// persisted value is an object — `{"kind":"window","window_hwnd":<i64>}` or
/// `{"kind":"cdp","window_hwnd":<i64>,"cdp_target_id":"<id>"}` — which
/// [`json_value_text`] canonicalizes to its JSON text. That text is the
/// authoritative identity of the window/tab the action actually drove.
///
/// Command-audit rows independently persist `/target`; preflight request shapes
/// may carry the same identity below `payload_bounded` or `request_snapshot`.
/// This list previously started at `/target` and then searched terminal
/// `details`. A real action audit row carries no top-level `/target`, so **every
/// target-bound success measured as
/// `AbsentReason::NotApplicable` on `ACT_SLOT_TARGET_HASH`** while some failure
/// payloads happened to carry `details.target`. The result was a target lane
/// populated only by bad cases: Ward reported `slot 49 has 0 adjudicated good
/// exemplar(s)` no matter how many verified target-bound successes were run.
///
/// Order is load-bearing. The authoritative session target wins when present;
/// command-audit and explicit preflight request fields follow. Terminal
/// `details` and `details.request` are deliberately excluded: they are
/// post-treatment response/error shapes, and using them populated the target
/// lane on failures without an equivalent success field.
const ACTION_TARGET_POINTERS: &[&str] = &[
    "/agent_logical_foreground/target",
    "/foreground_lane/target",
    "/target",
    "/payload_bounded/target",
    "/request_snapshot/target",
];

fn action_target_text(record: &Value) -> Option<String> {
    json_pointer_text(record, ACTION_TARGET_POINTERS)
}

/// The resolved target **value**, under the same precedence
/// [`action_target_text`] uses.
///
/// Slot 49 hashes the canonicalized text of this value; slot 117 needs the
/// value itself, because a structured target's identity is per field and the
/// text form has already flattened that structure away.
fn action_target_value(record: &Value) -> Option<&Value> {
    ACTION_TARGET_POINTERS
        .iter()
        .find_map(|pointer| record.pointer(pointer))
        .filter(|value| !value.is_null())
}

/// A short, collision-resistant name for one feature *value*.
///
/// The hashed thing is the **feature name**, never the vector cell, so this only
/// has to make two different values name two different features. 16 hex chars
/// (64 bits) does that with room to spare, and keeps the JSON key — and the
/// bytes `syn_record_vector_unit_fields` content-addresses — bounded regardless
/// of how long a window title or URL is.
fn action_target_feature_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    // `sha256_hex(...).truncate(16)` encoded all 32 digest bytes only to drop
    // the last 24. Encoding the first eight bytes produces the exact same
    // lowercase 16-character frozen feature key without the oversized String.
    hex_encode(&digest[..8])
}

/// Splits one field value into the identity components a partial match should
/// score on. Empty components are dropped; the whole value is never emitted
/// here, because the exact-value feature already carries it.
fn action_target_components(value: &str) -> Vec<String> {
    value
        .split(ACT_TARGET_COMPONENT_SEPARATORS)
        .filter_map(non_empty)
        .map(str::to_lowercase)
        .collect()
}

/// Builds the flat `feature name -> weight` record slot 117 measures.
///
/// ## What makes this similarity-bearing where `syn_hash` is not
///
/// `syn.action.target_hash.v2` content-addresses the **whole** target value into
/// one sparse cell, so its similarity is the indicator `same value / different
/// value` — it cannot say that two actions drove the same window in different
/// tabs, or the same executable under a different argument, and it is not a
/// dense vector, so Ward cannot read it at all.
///
/// This decomposes the same target into overlapping named features and lets
/// `syn_record_vector_unit_fields` project them into a fixed dense space. Cosine
/// between two targets is then the (unbiased, see [`ACT_TARGET_VECTOR_DIM`])
/// normalized overlap of their feature sets:
///
/// * identical target                       -> 1.0
/// * same kind and shape, different values  -> the kind/shape mass only
/// * shared path or URL components          -> graded by how much they share
/// * unrelated targets                      -> ~0
///
/// which is the separable good-vs-bad geometry Ward needs and `2020001` could
/// not produce on any slot.
///
/// ## What it deliberately does not read
///
/// Only the target. Not `status.outcome`, not `error_code`, not the
/// foreground-lane claim status — nothing that is, or is downstream of, the
/// adjudicated outcome. This is the slot-86 exclusion doctrine applied at
/// construction time rather than at calibration time: a lane that carries the
/// label cannot be a feature for steering, and the cheapest way to guarantee
/// that is for the lens to be a pure function of the target identity.
///
/// # Errors
///
/// Returns a measurement error when a present target yields no encodable
/// feature, or carries more than [`ACT_TARGET_MAX_FIELDS`] fields. Both are
/// refused loudly rather than measured as a degenerate or truncated vector: a
/// silently-dropped target is exactly the corpus loss #2050 was opened for.
#[expect(
    clippy::cast_precision_loss,
    reason = "component count is bounded by ACT_TARGET_MAX_COMPONENTS before conversion"
)]
fn action_target_features(target: &Value) -> StorageResult<serde_json::Map<String, Value>> {
    let mut features = serde_json::Map::new();
    let mut components_emitted = 0_usize;
    let mut insert_components = |field: &str, value: &str, features: &mut serde_json::Map<_, _>| {
        let components = action_target_components(value);
        if components.is_empty() || components_emitted >= ACT_TARGET_MAX_COMPONENTS {
            return;
        }
        let budget = ACT_TARGET_MAX_COMPONENTS - components_emitted;
        let taken = components.len().min(budget);
        // `mass / sqrt(k)` over k components: the field's components carry
        // exactly `mass^2` of squared magnitude however many there are, so a
        // long path cannot outweigh a short one.
        let weight = ACT_TARGET_COMPONENT_MASS / (taken as f64).sqrt();
        for component in components.into_iter().take(taken) {
            let key = format!("c|{field}|{}", action_target_feature_digest(&component));
            features.insert(key, json!(weight));
        }
        components_emitted += taken;
    };

    match target {
        Value::Object(map) => {
            if map.len() > ACT_TARGET_MAX_FIELDS {
                return Err(measurement_error(
                    "action target vector",
                    format!(
                        "target object carries {} fields, more than the frozen maximum {ACT_TARGET_MAX_FIELDS}; \
                         truncating it would make two different targets measure identically, so it is refused. \
                         Remediation: the action audit writer has changed the persisted target shape — widen \
                         ACT_TARGET_MAX_FIELDS under a NEW action panel generation and re-measure",
                        map.len()
                    ),
                ));
            }
            let mut shape = String::new();
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                shape.push_str(key);
                shape.push('\u{1f}');
                let Some(text) = map.get(key).and_then(json_value_text) else {
                    continue;
                };
                let Some(text) = non_empty(&text) else {
                    continue;
                };
                features.insert(
                    format!("f|{key}|{}", action_target_feature_digest(text)),
                    json!(ACT_TARGET_EXACT_WEIGHT),
                );
                if key == "kind" {
                    features.insert(
                        format!("k|{}", action_target_feature_digest(text)),
                        json!(ACT_TARGET_KIND_WEIGHT),
                    );
                } else {
                    insert_components(key, text, &mut features);
                }
            }
            features.insert(
                format!("sh|{}", action_target_feature_digest(&shape)),
                json!(ACT_TARGET_SHAPE_WEIGHT),
            );
        }
        other => {
            // A scalar or array target. It has no field structure, so it is one
            // unnamed field with its own shape.
            let text = json_value_text(other)
                .and_then(|text| non_empty(&text).map(str::to_owned))
                .ok_or_else(|| {
                    measurement_error(
                        "action target vector",
                        format!(
                            "target value {} canonicalizes to empty text and carries no encodable identity",
                            value_kind(other)
                        ),
                    )
                })?;
            features.insert(
                format!("f|$|{}", action_target_feature_digest(&text)),
                json!(ACT_TARGET_EXACT_WEIGHT),
            );
            features.insert(
                format!("k|{}", action_target_feature_digest(value_kind(other))),
                json!(ACT_TARGET_KIND_WEIGHT),
            );
            insert_components("$", &text, &mut features);
            features.insert(
                format!("sh|{}", action_target_feature_digest("$\u{1f}")),
                json!(ACT_TARGET_SHAPE_WEIGHT),
            );
        }
    }

    if features.is_empty() {
        return Err(measurement_error(
            "action target vector",
            "a present action target produced no encodable feature; refusing to measure an empty \
             target vector, which would place an unencodable target at the origin and make it \
             indistinguishable from every other unencodable one",
        ));
    }
    Ok(features)
}

/// Measures [`ACT_SLOT_TARGET_VECTOR`] for one action row.
///
/// `Absent(NotApplicable)` when the row bound no target at all — the same
/// answer slot 49 gives, and a true statement about a row that drove nothing.
/// Every other failure is loud.
fn action_target_vector_slot(source_key: &[u8], record: &Value) -> StorageResult<SlotVector> {
    let Some(target) = action_target_value(record) else {
        return Ok(absent(AbsentReason::NotApplicable));
    };
    let features = action_target_features(target).map_err(|error| {
        measurement_error(
            "action target vector",
            format!(
                "source_cf={} source_key_hex={} action={}: {error}",
                cf::CF_ACTION_LOG,
                hex_encode(source_key),
                action_identity(record)
            ),
        )
    })?;
    measure_json(
        SYN_ACTION_PANEL_NAME,
        AlgorithmicLens::syn_record_vector_unit_fields(
            "syn.action.target_vector.v2",
            Modality::Structured,
            ACT_TARGET_VECTOR_DIM,
        ),
        &Value::Object(features),
    )
}

/// One point-in-time request projection source.
///
/// Current command rows carry a digest of the complete redacted request, and
/// action-preflight rows carry a digest of the complete request snapshot after
/// the writer's public-readback redaction pass. Their bounded structural
/// projection may therefore summarize an oversized payload without pretending
/// the visible structure is the whole input. Legacy action-audit requests have
/// no such digest and must refuse any structural overflow instead.
struct ActionRequestSource<'a> {
    payload: &'a Value,
    source: &'static str,
    full_payload_sha256: Option<&'a str>,
    payload_bytes: Option<u64>,
    payload_truncated: Option<bool>,
}

/// Selects only request state known before the action outcome.
///
/// The command writer records the same redacted request on intent and final
/// rows, so `payload_bounded` is point-in-time correct on both. The action-audit
/// writer publishes `request_snapshot` only on its preflight/started rows and
/// publishes null on every terminal outcome. Older action rows have no safe
/// status-independent request snapshot and remain explicitly absent. Never
/// infer a request from terminal details or outcome.
fn action_request_source(record: &Value) -> StorageResult<Option<ActionRequestSource<'_>>> {
    if json_string(record, &["row_kind"]).as_deref() == Some("command_audit") {
        let payload = record.get("payload_bounded").ok_or_else(|| {
            measurement_error(
                "action request vector",
                "command_audit row is missing required pre-action payload_bounded; remediation=repair the command-audit writer or preserve/quarantine the malformed row",
            )
        })?;
        let digest = validated_action_request_sha256(record, "payload_sha256", "command_audit")?;
        let payload_bytes =
            validated_action_request_payload_bytes(record, "payload_bytes", "command_audit")?;
        let payload_truncated = record
            .get("payload_truncated")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                measurement_error(
                    "action request vector",
                    "command_audit row is missing boolean payload_truncated; remediation=repair the malformed audit row before re-measurement",
                )
            })?;
        return Ok(Some(ActionRequestSource {
            payload,
            source: "command_payload_bounded",
            full_payload_sha256: Some(digest),
            payload_bytes: Some(payload_bytes),
            payload_truncated: Some(payload_truncated),
        }));
    }

    let Some(payload) = record
        .get("request_snapshot")
        .filter(|payload| !payload.is_null())
    else {
        return Ok(None);
    };
    let digest =
        validated_action_request_sha256(record, "request_snapshot_sha256", "action preflight")?;
    let payload_bytes = validated_action_request_payload_bytes(
        record,
        "request_snapshot_bytes",
        "action preflight",
    )?;
    Ok(Some(ActionRequestSource {
        payload,
        source: "action_preflight_request_snapshot",
        full_payload_sha256: Some(digest),
        payload_bytes: Some(payload_bytes),
        payload_truncated: Some(false),
    }))
}

fn validated_action_request_sha256<'a>(
    record: &'a Value,
    field: &str,
    row_kind: &str,
) -> StorageResult<&'a str> {
    let digest = record
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            measurement_error(
                "action request vector",
                format!(
                    "{row_kind} row is missing required full redacted {field}; remediation=repair the audit writer or preserve/quarantine the malformed row"
                ),
            )
        })?;
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(measurement_error(
            "action request vector",
            format!(
                "{row_kind} {field} must use the writer's canonical sha256:<64 lowercase hex> representation; remediation=repair the malformed audit row before re-measurement"
            ),
        ));
    };
    if hex.len() != 64
        || !hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        || hex.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err(measurement_error(
            "action request vector",
            format!(
                "{row_kind} {field} must use sha256:<64 lowercase hex>, got tagged length {} and digest length {}; remediation=repair the malformed audit row before re-measurement",
                digest.len(),
                hex.len()
            ),
        ));
    }
    Ok(digest)
}

fn validated_action_request_payload_bytes(
    record: &Value,
    field: &str,
    row_kind: &str,
) -> StorageResult<u64> {
    let bytes = record.get(field).and_then(Value::as_u64).ok_or_else(|| {
        measurement_error(
            "action request vector",
            format!(
                "{row_kind} row is missing unsigned {field}; remediation=repair the malformed audit row before re-measurement"
            ),
        )
    })?;
    if bytes > ACT_REQUEST_MAX_PAYLOAD_BYTES {
        return Err(measurement_error(
            "action request vector",
            format!(
                "{row_kind} {field}={bytes} exceeds the frozen authenticated-request ceiling {ACT_REQUEST_MAX_PAYLOAD_BYTES}; remediation=repair the malformed row, or raise the transport and lens bounds together under a new action panel generation"
            ),
        ));
    }
    Ok(bytes)
}

fn insert_request_exact_feature(
    features: &mut serde_json::Map<String, Value>,
    namespace: &str,
    value: &str,
    weight: f64,
) {
    features.insert(
        format!("{namespace}|{}", action_target_feature_digest(value)),
        json!(weight),
    );
}

const fn request_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Adds a bounded, order-stable structural projection of one request payload.
/// Returns `true` exactly when the node/depth budget prevented full structural
/// expansion. Object keys are sorted before traversal; feature names contain
/// only short digests of paths and values, never raw request content.
fn collect_action_request_nodes(
    value: &Value,
    path: &str,
    depth: usize,
    visited: &mut usize,
    features: &mut serde_json::Map<String, Value>,
    shape: &mut ActionRequestShapeStats,
) -> bool {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH || *visited >= ACT_REQUEST_MAX_NODES {
        shape.overflowed = true;
        return true;
    }
    *visited += 1;
    shape.visited_nodes = *visited;
    shape.max_depth = shape.max_depth.max(depth);
    let path_digest = action_target_feature_digest(path);
    match value {
        Value::Object(map) => {
            shape.objects += 1;
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let field_shape = keys
                .iter()
                .map(|key| key.as_str())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            features.insert(
                format!(
                    "node|{path_digest}|object|{}",
                    action_target_feature_digest(&field_shape)
                ),
                json!(ACT_REQUEST_SHAPE_WEIGHT),
            );
            let mut overflowed = false;
            for key in keys {
                let child_path = format!("{path}/{}", action_target_feature_digest(key));
                overflowed |= collect_action_request_nodes(
                    &map[key],
                    &child_path,
                    depth + 1,
                    visited,
                    features,
                    shape,
                );
            }
            overflowed
        }
        Value::Array(items) => {
            shape.arrays += 1;
            features.insert(
                format!("node|{path_digest}|array|len={}", items.len()),
                json!(ACT_REQUEST_SHAPE_WEIGHT),
            );
            let mut overflowed = false;
            for (index, item) in items.iter().enumerate() {
                let child_path = format!("{path}/{index}");
                overflowed |= collect_action_request_nodes(
                    item,
                    &child_path,
                    depth + 1,
                    visited,
                    features,
                    shape,
                );
            }
            overflowed
        }
        scalar => {
            match scalar {
                Value::Null => shape.nulls += 1,
                Value::Bool(_) => shape.booleans += 1,
                Value::Number(_) => shape.numbers += 1,
                Value::String(_) => shape.strings += 1,
                Value::Array(_) | Value::Object(_) => {
                    unreachable!("container request values are handled before the scalar branch")
                }
            }
            let text = json_value_text(scalar).unwrap_or_else(|| request_value_kind(scalar).into());
            features.insert(
                format!(
                    "node|{path_digest}|{}|{}",
                    request_value_kind(scalar),
                    action_target_feature_digest(&text)
                ),
                json!(ACT_REQUEST_EXACT_WEIGHT),
            );
            false
        }
    }
}

#[derive(Default)]
struct ActionRequestShapeStats {
    visited_nodes: usize,
    max_depth: usize,
    objects: usize,
    arrays: usize,
    strings: usize,
    numbers: usize,
    booleans: usize,
    nulls: usize,
    overflowed: bool,
}

const fn request_shape_count_class(value: usize) -> u8 {
    match value {
        0 => 0,
        1 => 1,
        2..=4 => 2,
        5..=16 => 3,
        _ => 4,
    }
}

/// Eight frozen magnitude regimes spanning an empty payload through the 1 MiB
/// authenticated request ceiling. These are domain thresholds, not empirical
/// quantiles, so replaying the same row never depends on what else is present in
/// the vault.
const fn action_request_size_class(bytes: u64) -> u32 {
    match bytes {
        0 => 0,
        1..=63 => 1,
        64..=255 => 2,
        256..=1_023 => 3,
        1_024..=4_095 => 4,
        4_096..=16_383 => 5,
        16_384..=65_535 => 6,
        _ => 7,
    }
}

/// Compresses only the structural atoms of a request into a bounded category.
///
/// This is feature hashing used as a cardinality contract: the signature names
/// root value kind, binned node/depth counts, binned container/scalar counts,
/// and explicit traversal overflow. It includes no scalar value, digest, time,
/// source key, status, response, or outcome. The 30-bucket collision boundary
/// is therefore disclosed and deterministic while the exact request remains in
/// slot 118 for audit and similarity.
fn action_request_shape_class(payload: &Value, shape: &ActionRequestShapeStats) -> u32 {
    let signature = format!(
        "root={}|nodes={}|depth={}|objects={}|arrays={}|strings={}|numbers={}|bools={}|nulls={}|overflow={}",
        request_value_kind(payload),
        request_shape_count_class(shape.visited_nodes),
        request_shape_count_class(shape.max_depth),
        request_shape_count_class(shape.objects),
        request_shape_count_class(shape.arrays),
        request_shape_count_class(shape.strings),
        request_shape_count_class(shape.numbers),
        request_shape_count_class(shape.booleans),
        request_shape_count_class(shape.nulls),
        u8::from(shape.overflowed),
    );
    let digest = Sha256::digest(signature.as_bytes());
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
        % ACT_REQUEST_SHAPE_CLASS_LEVELS
}

fn action_request_features(
    record: &Value,
    request: &ActionRequestSource<'_>,
) -> StorageResult<(serde_json::Map<String, Value>, ActionRequestShapeStats)> {
    let mut features = serde_json::Map::new();
    insert_request_exact_feature(
        &mut features,
        "source",
        request.source,
        ACT_REQUEST_SHAPE_WEIGHT,
    );
    for (namespace, value) in [
        ("tool", json_string(record, &["tool"])),
        ("verb", json_string(record, &["verb"])),
        ("channel", json_string(record, &["channel"])),
    ] {
        if let Some(value) = value.as_deref() {
            insert_request_exact_feature(
                &mut features,
                namespace,
                value,
                ACT_REQUEST_ENVELOPE_WEIGHT,
            );
        }
    }
    if let Some(target) = action_target_value(record)
        && let Some(target) = json_value_text(target)
    {
        insert_request_exact_feature(
            &mut features,
            "target",
            &target,
            ACT_REQUEST_ENVELOPE_WEIGHT,
        );
    }
    if let Some(digest) = request.full_payload_sha256 {
        insert_request_exact_feature(
            &mut features,
            "payload_sha256",
            digest,
            ACT_REQUEST_EXACT_WEIGHT,
        );
    }
    if let Some(bytes) = request.payload_bytes {
        let normalized_bytes = exact_u64_as_f64(bytes).ln_1p()
            / exact_u64_as_f64(ACT_REQUEST_MAX_PAYLOAD_BYTES).ln_1p();
        features.insert(
            "payload_bytes_log1p_scaled".to_owned(),
            json!(normalized_bytes),
        );
    }
    if let Some(truncated) = request.payload_truncated {
        insert_request_exact_feature(
            &mut features,
            "payload_storage",
            if truncated { "truncated" } else { "complete" },
            ACT_REQUEST_SHAPE_WEIGHT,
        );
    }

    let mut visited = 0_usize;
    let mut shape = ActionRequestShapeStats::default();
    let overflowed = collect_action_request_nodes(
        request.payload,
        "$",
        0,
        &mut visited,
        &mut features,
        &mut shape,
    );
    if overflowed {
        let Some(digest) = request.full_payload_sha256 else {
            return Err(measurement_error(
                "action request vector",
                format!(
                    "action preflight request exceeds the frozen {ACT_REQUEST_MAX_NODES}-node/16-depth structural budget and has no complete payload digest; remediation=upgrade the audit writer to persist an exact redacted request digest, then allocate a new action panel generation"
                ),
            ));
        };
        insert_request_exact_feature(
            &mut features,
            "structural_overflow_complete_digest",
            digest,
            ACT_REQUEST_EXACT_WEIGHT,
        );
    }
    Ok((features, shape))
}

/// Measures the exact, size-class and shape-class pre-execution request causes,
/// or explicit absence on all three when a historical row did not persist a
/// request independently of its outcome.
fn action_request_slots(
    source_key: &[u8],
    record: &Value,
) -> StorageResult<(SlotVector, SlotVector, SlotVector)> {
    let Some(request) = action_request_source(record)? else {
        return Ok((
            absent(AbsentReason::NotApplicable),
            absent(AbsentReason::NotApplicable),
            absent(AbsentReason::NotApplicable),
        ));
    };
    let (features, shape) = action_request_features(record, &request).map_err(|error| {
        measurement_error(
            "action request causes",
            format!(
                "source_cf={} source_key_hex={} action={}: {error}",
                cf::CF_ACTION_LOG,
                hex_encode(source_key),
                action_identity(record)
            ),
        )
    })?;
    let request_vector = measure_json(
        SYN_ACTION_PANEL_NAME,
        AlgorithmicLens::syn_record_vector_unit_fields(
            "syn.action.request_vector.v1",
            Modality::Structured,
            ACT_REQUEST_VECTOR_DIM,
        ),
        &Value::Object(features),
    )?;
    let payload_bytes = request.payload_bytes.ok_or_else(|| {
        measurement_error(
            "action request size class",
            "a measurable request source omitted its authenticated payload byte length; remediation=repair the source contract rather than inventing a size class",
        )
    })?;
    let request_size_class = measure_number(
        SYN_ACTION_PANEL_NAME,
        AlgorithmicLens::syn_one_hot_index(
            "syn.action.request_size_class.v1",
            Modality::Structured,
            ACT_REQUEST_SIZE_CLASS_LEVELS,
        ),
        action_request_size_class(payload_bytes),
    )?;
    let request_shape_class = measure_number(
        SYN_ACTION_PANEL_NAME,
        AlgorithmicLens::syn_one_hot_index(
            "syn.action.request_shape_class.v1",
            Modality::Structured,
            ACT_REQUEST_SHAPE_CLASS_LEVELS,
        ),
        action_request_shape_class(request.payload, &shape),
    )?;
    Ok((request_vector, request_size_class, request_shape_class))
}

/// Reports a terminal action SUCCESS whose own foreground state says a target
/// applied but whose target no known path carries.
///
/// Explicit `Absent` is the correct typed value for target-independent actions.
/// It becomes corpus loss only when the same row claims `set`, a session target
/// lane, or that this specific action required the real foreground. Merely
/// holding a foreground lease does not make lease/profile/shell operations
/// target-bearing. Unknown status vocabulary fails measurement rather than
/// being guessed as applicable or not applicable.
fn warn_action_target_absent_on_success(source_key: &[u8], record: &Value) -> StorageResult<()> {
    if json_string(record, &["status", "outcome"]).as_deref() != Some("ok") {
        return Ok(());
    }
    let agent_status = json_pointer_text(record, &["/agent_logical_foreground/status"]);
    let lane_status = json_pointer_text(record, &["/foreground_lane/status"]);
    let agent_target_applies = match agent_status.as_deref() {
        Some("set") => true,
        None | Some("missing_session" | "missing" | "read_error") => false,
        Some(status) => {
            return Err(measurement_error(
                "action target applicability",
                format!(
                    "source_cf={} source_key_hex={} unknown agent_logical_foreground.status={status}; remediation=declare the new status semantics and allocate a new action panel generation rather than guessing whether target absence is applicable",
                    cf::CF_ACTION_LOG,
                    hex_encode(source_key)
                ),
            ));
        }
    };
    let lane_target_applies = match lane_status.as_deref() {
        Some("conflicting_owner" | "claimed_by_session" | "unclaimed_session_target") => true,
        None
        | Some("missing_session" | "missing" | "read_error" | "explicit_real_foreground_lease") => {
            false
        }
        Some(status) => {
            return Err(measurement_error(
                "action target applicability",
                format!(
                    "source_cf={} source_key_hex={} unknown foreground_lane.status={status}; remediation=declare the new status semantics and allocate a new action panel generation rather than guessing whether target absence is applicable",
                    cf::CF_ACTION_LOG,
                    hex_encode(source_key)
                ),
            ));
        }
    };
    let real_foreground_target_applies = record
        .pointer("/foreground_tier/required_foreground")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !agent_target_applies && !lane_target_applies && !real_foreground_target_applies {
        return Ok(());
    }
    tracing::warn!(
        code = "CALYX_ACTION_TARGET_ABSENT_ON_SUCCESS",
        panel_name = SYN_ACTION_PANEL_NAME,
        panel_version = SYN_ACTION_PANEL_VERSION,
        slot = ACT_SLOT_TARGET_HASH.get(),
        source_cf = cf::CF_ACTION_LOG,
        source_key_hex = %hex_encode(source_key),
        action = %action_identity(record),
        agent_logical_foreground_status = agent_status.as_deref().unwrap_or("<absent>"),
        foreground_lane_status = lane_status.as_deref().unwrap_or("<absent>"),
        required_real_foreground = real_foreground_target_applies,
        searched_pointers = ACTION_TARGET_POINTERS.join(","),
        "terminal action success claims a target-bearing foreground state but carries no target \
         under any known path; its target-hash slot measures Absent and the row cannot serve as a \
         good exemplar for target-lane calibration. Remediation: repair the audit writer and \
         ACTION_TARGET_POINTERS together under a new frozen action-panel generation; never bind \
         an unrelated target merely to populate this lens"
    );
    Ok(())
}

fn reflex_latency_ms(record: &StoredReflexAudit) -> Option<u64> {
    json_u64(
        &record.details,
        &[
            "latency_ms",
            "elapsed_ms",
            "duration_ms",
            "dispatch_latency_ms",
            "total_latency_ms",
        ],
    )
}

fn process_event_kind(record: &Value) -> String {
    json_string(record, &["row_kind", "event", "status"])
        .unwrap_or_else(|| "process_event".to_owned())
}

fn process_identity_text(record: &Value) -> Option<String> {
    json_pointer_text(
        record,
        &[
            "/process_path",
            "/path",
            "/target",
            "/command_line",
            "/matched_title",
            // #2097: observed-process rows carry no path, target or command
            // line by design — the kernel process-table snapshot exposes only
            // the image name, and that is deliberately all the topology
            // observer records. Appended **last** so it is consulted only when
            // every richer identity is absent; existing launch rows always
            // match `/target` first and are therefore measured unchanged.
            "/image_name",
        ],
    )
}

fn process_ts_ns(record: &Value) -> StorageResult<Option<u64>> {
    if let Some(value) = record.get("ts_ns") {
        return optional_exact_u64(value, "ts_ns");
    }
    let Some(value) = record.get("launched_at_unix_ms") else {
        return Ok(None);
    };
    optional_exact_u64(value, "launched_at_unix_ms")?
        .map(|value| {
            value.checked_mul(NS_PER_MS).ok_or_else(|| {
                measurement_error(
                    "process timestamp overflows nanoseconds",
                    format!("launched_at_unix_ms={value}"),
                )
            })
        })
        .transpose()
}

fn optional_exact_u64(value: &Value, field: &str) -> StorageResult<Option<u64>> {
    if value.is_null() {
        return Ok(None);
    }
    value.as_u64().map(Some).ok_or_else(|| {
        measurement_error(
            "process timestamp must be an unsigned JSON integer or null",
            format!("field={field} value={value}"),
        )
    })
}

fn observation_role_histogram(record: &StoredObservation) -> Option<Value> {
    let mut counts = BTreeMap::<String, u64>::new();
    for role in record
        .elements
        .iter()
        .filter_map(|element| non_empty(&element.role))
    {
        *counts.entry(role.to_ascii_lowercase()).or_default() += 1;
    }
    if let Some(focused) = record.focused.as_ref()
        && let Some(role) = non_empty(&focused.role)
    {
        *counts
            .entry(format!("focused_{}", role.to_ascii_lowercase()))
            .or_default() += 1;
    }
    if counts.is_empty() {
        None
    } else {
        counts.insert(
            "_total_elements".to_owned(),
            u64::try_from(record.elements.len()).unwrap_or(u64::MAX),
        );
        Some(json!(counts))
    }
}

fn observation_entity_labels(record: &StoredObservation) -> Vec<String> {
    record
        .entities
        .iter()
        .filter_map(|entity| non_empty(&entity.class_label))
        .map(str::to_ascii_lowercase)
        .collect()
}

fn observation_flags(record: &StoredObservation) -> StorageResult<Vec<String>> {
    let mut flags = Vec::new();
    flags.push(format!(
        "mode:{}",
        snake_case_name(record.mode, "PerceptionMode")?
    ));
    flags.push(format!(
        "a11y:{}",
        sensor_status_code(&record.diagnostics.a11y_status)
    ));
    flags.push(format!(
        "capture:{}",
        sensor_status_code(&record.diagnostics.capture_status)
    ));
    flags.push(format!(
        "detection:{}",
        sensor_status_code(&record.diagnostics.detection_status)
    ));
    flags.push(format!(
        "audio:{}",
        sensor_status_code(&record.diagnostics.audio_status)
    ));
    if record.foreground.is_fullscreen {
        flags.push("foreground_fullscreen".to_owned());
    }
    if record.foreground.is_dwm_composed {
        flags.push("dwm_composed".to_owned());
    }
    if record.diagnostics.elements_truncated {
        flags.push("elements_truncated".to_owned());
    }
    if record.diagnostics.entities_truncated {
        flags.push("entities_truncated".to_owned());
    }
    if record.diagnostics.is_minimized {
        flags.push("target_minimized".to_owned());
    }
    if record.redacted {
        flags.push("redacted".to_owned());
    }
    flags.sort();
    flags.dedup();
    Ok(flags)
}

const fn sensor_status_code(status: &SensorStatus) -> &'static str {
    match status {
        SensorStatus::Healthy => "healthy",
        SensorStatus::DegradedLatency { .. } => "degraded_latency",
        SensorStatus::DegradedSensorFailed { .. } => "degraded_sensor_failed",
        // #2054: a producer that was asked for nothing is its own flag token,
        // so a stored observation never carries `detection:healthy` for a
        // profile whose detector never ran.
        SensorStatus::NotConfigured { .. } => "not_configured",
        SensorStatus::Disabled => "disabled",
        SensorStatus::Unavailable => "unavailable",
    }
}

fn outcome_event(record: &Value) -> Option<String> {
    json_string(record, &["event", "action", "kind"])
}

fn outcome_status(record: &Value) -> Option<String> {
    json_string(
        record,
        &["after_status", "status", "outcome", "decision", "lifecycle"],
    )
    .or_else(|| {
        json_bool(record, &["matched"]).map(|matched| {
            if matched {
                "matched".to_owned()
            } else {
                "not_matched".to_owned()
            }
        })
    })
    .or_else(|| {
        json_u64(record, &["code_count"]).map(|count| {
            if count > 0 {
                "codes_found".to_owned()
            } else {
                "no_codes_found".to_owned()
            }
        })
    })
}

fn outcome_target(record: &Value) -> Option<String> {
    json_string(
        record,
        &[
            "routine_id",
            "approval_id",
            "escalation_id",
            "event_id",
            "audit_id",
            "source",
            "spawn_id",
            "session_id",
            "episode_id",
            "target",
        ],
    )
    .or_else(|| json_pointer_text(record, &["/detail/approval_id", "/detail/escalation_id"]))
}

fn outcome_ts_ns(record: &Value) -> Option<u64> {
    json_u64(record, &["ts_ns"]).or_else(|| {
        json_u64(
            record,
            &[
                "at_unix_ms",
                "updated_at_unix_ms",
                "read_at_unix_ms",
                "created_at_unix_ms",
                "bound_at_unix_ms",
            ],
        )
        .map(|ms| ms.saturating_mul(NS_PER_MS))
    })
}

fn mcp_usage_ts_ns(record: &Value) -> Option<u64> {
    json_u64(record, &["finished_at_unix_ms"])
        .or_else(|| json_u64(record, &["started_at_unix_ms"]))
        .map(|ms| ms.saturating_mul(NS_PER_MS))
}

#[expect(
    clippy::cast_precision_loss,
    reason = "each value is clamped to a small declared ceiling before conversion"
)]
fn outcome_numeric_record_v2(record: &Value, raw_bytes: &[u8]) -> Value {
    let scaled = |value: u64, ceiling: u64| (value.min(ceiling) as f64) / (ceiling as f64);
    json!({
        "raw_len_scaled": scaled(u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX), 10_000),
        "has_event": bool_u64(outcome_event(record).is_some()),
        "has_status": bool_u64(outcome_status(record).is_some()),
        "has_target": bool_u64(outcome_target(record).is_some()),
        "has_timestamp": bool_u64(outcome_ts_ns(record).is_some()),
        "code_count_scaled": scaled(json_u64(record, &["code_count"]).unwrap_or(0), 10),
        "ladder_index_scaled": scaled(json_u64(record, &["ladder_index"]).unwrap_or(0), 10),
    })
}

#[expect(
    clippy::cast_precision_loss,
    reason = "each value is clamped to a small declared ceiling before conversion"
)]
fn mcp_usage_numeric_record_v2(record: &Value, raw_bytes: &[u8]) -> Value {
    let scaled = |value: u64, ceiling: u64| (value.min(ceiling) as f64) / (ceiling as f64);
    json!({
        // Rounded ceilings above the live 2026-08-04 p99 bound each independent
        // magnitude. Rare extremes clamp instead of dominating cosine geometry.
        "raw_len_scaled": scaled(u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX), 10_000),
        "duration_scaled": scaled(json_u64(record, &["duration_ms"]).unwrap_or(0), 100_000),
        "response_size_scaled": scaled(json_u64(record, &["response_size_bytes"]).unwrap_or(0), 1_000_000),
        "argument_top_level_scaled": scaled(json_u64(record, &["argument_top_level_key_count"]).unwrap_or(0), 10),
        "argument_nested_scaled": scaled(json_u64(record, &["argument_nested_path_count"]).unwrap_or(0), 10),
        "has_tool": bool_u64(json_string(record, &["tool"]).is_some()),
        "has_operation": bool_u64(json_string(record, &["operation"]).is_some()),
        "has_route_id": bool_u64(json_string(record, &["route_id"]).is_some()),
        "has_profile": bool_u64(json_string(record, &["profile"]).is_some()),
        "has_surface_hash": bool_u64(json_string(record, &["tool_surface_sha256"]).is_some()),
        "has_session": bool_u64(json_string(record, &["mcp_session_id_sha256"]).is_some()),
        "has_error": bool_u64(json_string(record, &["error_type"]).is_some()),
        "steering_emitted": bool_u64(json_bool(record, &["steering_emitted"]).unwrap_or(false)),
    })
}

#[expect(
    clippy::cast_precision_loss,
    reason = "scaled values are ceiling-clamped and week phase is below f64's exact integer range"
)]
fn action_numeric_record(record: &Value) -> Value {
    let scaled = |value: u64, ceiling: u64| (value.min(ceiling) as f64) / (ceiling as f64);
    let ts_ns = json_u64(record, &["ts_ns"]).unwrap_or(0);
    let week_ns = 7 * 86_400_000_000_000_u64;
    json!({
        "sequence_scaled": scaled(json_u64(record, &["seq"]).unwrap_or(0), 1_000_000),
        "week_phase": (ts_ns % week_ns) as f64 / week_ns as f64,
        "has_tool": bool_u64(json_string(record, &["tool"]).is_some()),
        "has_verb": bool_u64(json_string(record, &["verb"]).is_some()),
        "has_target": bool_u64(action_target_text(record).is_some()),
    })
}

fn json_string(record: &Value, keys: &[&str]) -> Option<String> {
    let object = record.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .and_then(non_empty)
        .map(str::to_owned)
}

fn ensure_json_object(record: &Value, cf_name: &'static str) -> StorageResult<()> {
    if record.is_object() {
        Ok(())
    } else {
        Err(StorageError::WriteFailed {
            cf_name: cf_name.to_owned(),
            detail: format!("Calyx panel measurement requires a JSON object row; got {record}"),
        })
    }
}

fn json_u64(record: &Value, keys: &[&str]) -> Option<u64> {
    let object = record.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
}

fn json_bool(record: &Value, keys: &[&str]) -> Option<bool> {
    let object = record.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_bool))
}

fn json_non_null(record: &Value, keys: &[&str]) -> bool {
    record.as_object().is_some_and(|object| {
        keys.iter()
            .any(|key| object.get(*key).is_some_and(|value| !value.is_null()))
    })
}

fn json_pointer_text(record: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| record.pointer(pointer))
        .and_then(json_value_text)
        .and_then(|value| non_empty(&value).map(str::to_owned))
}

fn json_value_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).ok(),
    }
}

fn observation_constellation_sample_every_n() -> StorageResult<u64> {
    match env::var(SYN_OBSERVATION_SAMPLE_EVERY_N_ENV) {
        Ok(raw) => {
            let parsed = raw.parse::<u64>().map_err(|error| StorageError::WriteFailed {
                cf_name: cf::CF_OBSERVATIONS.to_owned(),
                detail: format!(
                    "{SYN_OBSERVATION_SAMPLE_EVERY_N_ENV} must be a positive integer; got {raw:?}: {error}"
                ),
            })?;
            if parsed == 0 {
                return Err(StorageError::WriteFailed {
                    cf_name: cf::CF_OBSERVATIONS.to_owned(),
                    detail: format!("{SYN_OBSERVATION_SAMPLE_EVERY_N_ENV} must be >= 1"),
                });
            }
            Ok(parsed)
        }
        Err(env::VarError::NotPresent) => Ok(SYN_OBSERVATION_SAMPLE_EVERY_N_DEFAULT),
        Err(env::VarError::NotUnicode(value)) => Err(StorageError::WriteFailed {
            cf_name: cf::CF_OBSERVATIONS.to_owned(),
            detail: format!(
                "{} is not valid Unicode: {}",
                SYN_OBSERVATION_SAMPLE_EVERY_N_ENV,
                value.display()
            ),
        }),
    }
}

fn observation_source_key_seq(source_key: &[u8]) -> StorageResult<u32> {
    if source_key.len() != 12 {
        return Err(StorageError::WriteFailed {
            cf_name: cf::CF_OBSERVATIONS.to_owned(),
            detail: format!(
                "CF_OBSERVATIONS key must be ts_ns||seq (12 bytes) for deterministic Calyx sampling; got {} bytes",
                source_key.len()
            ),
        });
    }
    let mut seq = [0_u8; 4];
    seq.copy_from_slice(&source_key[8..12]);
    Ok(u32::from_be_bytes(seq))
}

fn url_host(url: &str) -> Option<String> {
    let trimmed = non_empty(url)?;
    let after_scheme = trimmed
        .split_once("://")
        .map_or(trimmed, |(_scheme, remainder)| remainder);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .and_then(non_empty)?;
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_user, host)| host);
    let host = host_port.strip_prefix('[').map_or_else(
        || {
            host_port
                .split_once(':')
                .map_or(host_port, |(host, _port)| host)
        },
        |stripped| {
            stripped
                .split_once(']')
                .map_or(host_port, |(ipv6, _rest)| ipv6)
        },
    );
    non_empty(host).map(str::to_ascii_lowercase)
}

fn source_pointer(source_cf: &str, source_key: &[u8]) -> String {
    format!("{POINTER_SCHEME}://{source_cf}/{}", hex_encode(source_key))
}

fn append_framed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn truncate_metadata(value: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut output = String::new();
    for ch in value.chars().take(MAX_CHARS) {
        output.push(ch);
    }
    output
}
