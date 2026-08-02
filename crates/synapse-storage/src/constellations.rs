use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::time::Duration;

use calyx_core::{
    AbsentReason, Asymmetry, CalyxErrorCode, Constellation, CxFlags, CxId, Input, InputRef,
    LedgerRef, Lens, METADATA_SOURCE_EVENT_TIME_RAW, METADATA_SOURCE_EVENT_TIME_SECS,
    METADATA_SOURCE_SEQUENCE, METADATA_TEMPORAL_INACTIVE_REASON, METADATA_TEMPORAL_LANE_STATE,
    Modality, Panel, QuantPolicy, Slot, SlotId, SlotKey, SlotResource, SlotState, SlotVector,
    TEMPORAL_LANE_ACTIVE, TEMPORAL_LANE_INACTIVE, TEMPORAL_MISSING_CREATED_AT, VaultId,
};
use calyx_lenses::AlgorithmicLens;
use calyx_lenses::measure::{absent, input_hash};
use calyx_registry::{
    AlgorithmicEncoder as RegistryAlgorithmicEncoder, AlgorithmicLens as RegistryAlgorithmicLens,
    LensRuntime, LensSpec, Registry, default_recall_delta,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_calyx::SynapseCalyxPutDisposition;
use synapse_calyx::lens_provenance;
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
pub const SYN_TIMELINE_PANEL_VERSION: u32 = 1_900_001;
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
pub const SYN_EPISODE_PANEL_VERSION: u32 = 1_904_002;
/// The episode layout #1904 superseded.
pub const SYN_EPISODE_PANEL_VERSION_PRE_1904: u32 = 1_664_002;
pub const SYN_AGENT_EVENT_PANEL_NAME: &str = "syn-agent-event-v1";
pub const SYN_AGENT_EVENT_PANEL_VERSION: u32 = 1_665_001;
pub const SYN_AGENT_TRANSCRIPT_PANEL_NAME: &str = "syn-agent-transcript-v1";
/// Current agent-transcript slot layout.
///
/// #1904 added `AT_SLOT_TEXT_BM25`, giving the largest text corpus on the vault
/// its first lexically-rankable lane. #1921 then measured that lane and found it
/// could see only 9.2% of the corpus, and added `AT_SLOT_TEXT_FULL_BM25` over
/// the prose the record already carried but no lens had ever read. See the
/// episode constant above for why a new lens is a new generation.
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION: u32 = 1_921_001;
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
pub const SYN_ACTION_PANEL_VERSION: u32 = 1_776_001;
pub const SYN_REFLEX_PANEL_NAME: &str = "syn-reflex-v1";
pub const SYN_REFLEX_PANEL_VERSION: u32 = 1_776_002;
pub const SYN_PROCESS_PANEL_NAME: &str = "syn-process-v1";
pub const SYN_PROCESS_PANEL_VERSION: u32 = 1_776_003;
pub const SYN_OBSERVATION_PANEL_NAME: &str = "syn-observation-v1";
pub const SYN_OBSERVATION_PANEL_VERSION: u32 = 1_776_004;
pub const SYN_OUTCOME_PANEL_NAME: &str = "syn-outcome-v1";
pub const SYN_OUTCOME_PANEL_VERSION: u32 = 1_776_005;
pub const SYN_MCP_USAGE_PANEL_NAME: &str = "syn-mcp-usage-v1";
pub const SYN_MCP_USAGE_PANEL_VERSION: u32 = 1_776_006;
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
const CALYX_DURABLE_SLOT_ID_MAX: u16 = 112;
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
const EP_SLOT_RECORD_VECTOR: SlotId = SlotId::new(22);
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
const AE_SLOT_RECORD_VECTOR: SlotId = SlotId::new(34);

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
const AT_SLOT_RECORD_VECTOR: SlotId = SlotId::new(47);
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
/// Measured by `crates/synapse-calyx/examples/lexical_dimension_sizing_fsv.rs`
/// over the real provider session corpus these rows are derived from (46 files,
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
/// Re-measure with `crates/synapse-calyx/examples/lexical_dimension_sizing_fsv.rs`
/// before changing this; do not adjust it by intuition.
const AT_TEXT_FULL_BM25_DIM: u32 = 2_097_152;

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
const ACT_SLOT_PARAMS_RECORD_VECTOR: SlotId = SlotId::new(50);
const ACT_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(51);
const ACT_SLOT_DOW_CYCLIC: SlotId = SlotId::new(52);

const RF_SLOT_REFLEX_HASH: SlotId = SlotId::new(53);
const RF_SLOT_OUTCOME_ONEHOT: SlotId = SlotId::new(54);
const RF_SLOT_LATENCY_LOG1P: SlotId = SlotId::new(55);
const RF_SLOT_STEP_COUNT_LOG1P: SlotId = SlotId::new(56);
const RF_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(57);
const RF_SLOT_DOW_CYCLIC: SlotId = SlotId::new(58);
const RF_SLOT_RECORD_VECTOR: SlotId = SlotId::new(59);

const PR_SLOT_PROCESS_HASH: SlotId = SlotId::new(60);
const PR_SLOT_EVENT_ONEHOT: SlotId = SlotId::new(61);
const PR_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(62);
const PR_SLOT_DOW_CYCLIC: SlotId = SlotId::new(63);
const PR_SLOT_UPTIME_LOG1P: SlotId = SlotId::new(64);
const PR_SLOT_RECENCY_RANK: SlotId = SlotId::new(65);
const PR_SLOT_RECORD_VECTOR: SlotId = SlotId::new(66);

const OB_SLOT_APP_HASH: SlotId = SlotId::new(67);
const OB_SLOT_ROLE_HISTOGRAM: SlotId = SlotId::new(68);
const OB_SLOT_ENTITY_MULTI_HOT: SlotId = SlotId::new(69);
const OB_SLOT_HUD_RECORD_VECTOR: SlotId = SlotId::new(70);
const OB_SLOT_FLAGS_MULTI_HOT: SlotId = SlotId::new(71);
const OB_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(72);
const OB_SLOT_DOW_CYCLIC: SlotId = SlotId::new(73);
const OB_SLOT_RECORD_VECTOR: SlotId = SlotId::new(74);

const OUT_SLOT_SOURCE_CF_ONEHOT: SlotId = SlotId::new(75);
const OUT_SLOT_EVENT_ONEHOT: SlotId = SlotId::new(76);
const OUT_SLOT_STATUS_ONEHOT: SlotId = SlotId::new(77);
const OUT_SLOT_TARGET_HASH: SlotId = SlotId::new(78);
const OUT_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(79);
const OUT_SLOT_DOW_CYCLIC: SlotId = SlotId::new(80);
const OUT_SLOT_RECORD_VECTOR: SlotId = SlotId::new(81);

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
const MU_SLOT_RECORD_VECTOR: SlotId = SlotId::new(93);

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
    PanelSlotBlock {
        panel: SYN_AGENT_EVENT_PANEL_NAME,
        first: 23,
        last: 34,
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
        panel: SYN_MCP_USAGE_PANEL_NAME,
        first: 82,
        last: 93,
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
    AppUsage,
    Routine,
}

impl RecurrenceSubjectKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
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
pub struct TemporalMetadataBackfillReport {
    pub source_cf: String,
    pub examined_rows: u64,
    pub inserted_rows: u64,
    pub backfilled_rows: u64,
    pub already_current_rows: u64,
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
    pub latest_seq: u64,
    pub resume_after_physical: Option<Vec<u8>>,
    pub more: bool,
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
        cf::CF_KV | cf::CF_ROUTINE_STATE => CalyxAnchorPanel {
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
    (EP_SLOT_RECORD_VECTOR, "syn.episode.record_vector.v1"),
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
        "syn.agent_event.end_state_onehot.v1",
    ),
    (AE_SLOT_HOUR_CYCLIC, "syn.agent_event.hour_cyclic.v1"),
    (AE_SLOT_DOW_CYCLIC, "syn.agent_event.dow_cyclic.v1"),
    (
        AE_SLOT_USAGE_TOTAL_LOG1P,
        "syn.agent_event.usage_total_log1p.v1",
    ),
    (AE_SLOT_RECORD_VECTOR, "syn.agent_event.record_vector.v1"),
    (AT_SLOT_ROLE_ONEHOT, "syn.agent_transcript.role_onehot.v1"),
    (
        AT_SLOT_STATUS_ONEHOT,
        "syn.agent_transcript.status_onehot.v1",
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
        AT_SLOT_RECORD_VECTOR,
        "syn.agent_transcript.record_vector.v1",
    ),
    (ACT_SLOT_KIND_ONEHOT, "syn.action.kind_onehot.v1"),
    (ACT_SLOT_TARGET_HASH, "syn.action.target_hash.v1"),
    (
        ACT_SLOT_PARAMS_RECORD_VECTOR,
        "syn.action.params_record_vector.v1",
    ),
    (ACT_SLOT_HOUR_CYCLIC, "syn.action.hour_cyclic.v1"),
    (ACT_SLOT_DOW_CYCLIC, "syn.action.dow_cyclic.v1"),
    (RF_SLOT_REFLEX_HASH, "syn.reflex.reflex_hash.v1"),
    (RF_SLOT_OUTCOME_ONEHOT, "syn.reflex.outcome_onehot.v1"),
    (RF_SLOT_LATENCY_LOG1P, "syn.reflex.latency_ms_log1p.v1"),
    (RF_SLOT_STEP_COUNT_LOG1P, "syn.reflex.step_count_log1p.v1"),
    (RF_SLOT_HOUR_CYCLIC, "syn.reflex.hour_cyclic.v1"),
    (RF_SLOT_DOW_CYCLIC, "syn.reflex.dow_cyclic.v1"),
    (RF_SLOT_RECORD_VECTOR, "syn.reflex.record_vector.v1"),
    (PR_SLOT_PROCESS_HASH, "syn.process.process_hash.v1"),
    (PR_SLOT_EVENT_ONEHOT, "syn.process.event_onehot.v1"),
    (PR_SLOT_HOUR_CYCLIC, "syn.process.hour_cyclic.v1"),
    (PR_SLOT_DOW_CYCLIC, "syn.process.dow_cyclic.v1"),
    (PR_SLOT_UPTIME_LOG1P, "syn.process.uptime_ms_log1p.v1"),
    (PR_SLOT_RECENCY_RANK, "syn.process.event_time_rank.v1"),
    (PR_SLOT_RECORD_VECTOR, "syn.process.record_vector.v1"),
    (OB_SLOT_APP_HASH, "syn.observation.app_hash.v1"),
    (OB_SLOT_ROLE_HISTOGRAM, "syn.observation.role_histogram.v1"),
    (
        OB_SLOT_ENTITY_MULTI_HOT,
        "syn.observation.entity_multi_hot.v1",
    ),
    (OB_SLOT_HUD_RECORD_VECTOR, "syn.observation.hud_scalars.v1"),
    (
        OB_SLOT_FLAGS_MULTI_HOT,
        "syn.observation.flags_multi_hot.v1",
    ),
    (OB_SLOT_HOUR_CYCLIC, "syn.observation.hour_cyclic.v1"),
    (OB_SLOT_DOW_CYCLIC, "syn.observation.dow_cyclic.v1"),
    (OB_SLOT_RECORD_VECTOR, "syn.observation.record_vector.v1"),
    (OUT_SLOT_SOURCE_CF_ONEHOT, "syn.outcome.source_cf_onehot.v1"),
    (OUT_SLOT_EVENT_ONEHOT, "syn.outcome.event_onehot.v1"),
    (OUT_SLOT_STATUS_ONEHOT, "syn.outcome.status_onehot.v1"),
    (OUT_SLOT_TARGET_HASH, "syn.outcome.target_hash.v1"),
    (OUT_SLOT_HOUR_CYCLIC, "syn.outcome.hour_cyclic.v1"),
    (OUT_SLOT_DOW_CYCLIC, "syn.outcome.dow_cyclic.v1"),
    (OUT_SLOT_RECORD_VECTOR, "syn.outcome.record_vector.v1"),
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
    (MU_SLOT_RECORD_VECTOR, "syn.mcp_usage.record_vector.v1"),
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
/// Called from [`syn_active_panel_contract`], which is on the daemon's startup
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

/// The built-in panel catalog: every `syn-*` panel, its active generation, and
/// the declarations #1920 ask 3 and #1927 asks 1/3 require.
///
/// `builtin_panel_catalog` previously existed with a doc comment saying it was
/// "for the `panel list` action" and **had no caller anywhere in the workspace**
/// (#1920's census comment found this). It is now the single source of truth the
/// panel-coverage readback joins the physical census against.
#[must_use]
pub fn builtin_panel_catalog() -> Vec<PanelCatalogEntry> {
    vec![
        // --- observation-shaped: correctly unanchored (#1920 ask 3) ---
        PanelCatalogEntry {
            panel_name: SYN_TIMELINE_PANEL_NAME,
            panel_version: SYN_TIMELINE_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_TIMELINE),
            outcome_bearing: false,
            source_ttl_managed: false,
            superseded_versions: &[SYN_TIMELINE_PANEL_VERSION_PRE_1900],
            backfill_source_cf: Some(cf::CF_TIMELINE),
        },
        PanelCatalogEntry {
            panel_name: SYN_ACTION_PANEL_NAME,
            panel_version: SYN_ACTION_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_ACTION_LOG),
            outcome_bearing: false,
            source_ttl_managed: true,
            superseded_versions: &[1_666_001],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_REFLEX_PANEL_NAME,
            panel_version: SYN_REFLEX_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_REFLEX_AUDIT),
            outcome_bearing: false,
            source_ttl_managed: true,
            superseded_versions: &[1_666_002],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_PROCESS_PANEL_NAME,
            panel_version: SYN_PROCESS_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_PROCESS_HISTORY),
            outcome_bearing: false,
            source_ttl_managed: true,
            superseded_versions: &[1_666_003],
            backfill_source_cf: None,
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
            superseded_versions: &[1_666_004],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_RECURRENCE_SUBJECT_PANEL_NAME,
            panel_version: SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
            // A record here is a recurrence *subject*, not an occurrence, so no
            // CF is its population.
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            superseded_versions: &[1_667_001],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_GRAPHPOS_APP_PANEL_NAME,
            panel_version: SYN_GRAPHPOS_APP_PANEL_VERSION,
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            superseded_versions: &[],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_GRAPHPOS_PROCESS_PANEL_NAME,
            panel_version: SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
            superseded_versions: &[],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_PATH_HIERARCHY_PANEL_NAME,
            panel_version: SYN_PATH_HIERARCHY_PANEL_VERSION,
            source: PanelSource::Derived,
            outcome_bearing: false,
            source_ttl_managed: false,
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
            superseded_versions: &[SYN_EPISODE_PANEL_VERSION_PRE_1904],
            backfill_source_cf: Some(cf::CF_EPISODES),
        },
        PanelCatalogEntry {
            panel_name: SYN_AGENT_EVENT_PANEL_NAME,
            panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_AGENT_EVENTS),
            outcome_bearing: true,
            source_ttl_managed: false,
            superseded_versions: &[],
            // No re-measure path exists for this CF (#1927 ask 2 reports the
            // shortfall rather than treating the absence as coverage).
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            panel_version: SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
            source: PanelSource::FullCf(cf::CF_AGENT_TRANSCRIPTS),
            outcome_bearing: true,
            source_ttl_managed: false,
            superseded_versions: &[
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
            superseded_versions: &[1_669_001],
            backfill_source_cf: None,
        },
        PanelCatalogEntry {
            panel_name: SYN_MCP_USAGE_PANEL_NAME,
            panel_version: SYN_MCP_USAGE_PANEL_VERSION,
            // The `mcp-usage/v1/` key prefix within CF_KV.
            source: PanelSource::SubsetOfCf(cf::CF_KV),
            outcome_bearing: true,
            source_ttl_managed: false,
            superseded_versions: &[1_691_001],
            backfill_source_cf: None,
        },
    ]
}

/// The catalog entry owning one panel version, whether active or superseded.
#[must_use]
pub fn panel_catalog_entry_for_version(panel_version: u32) -> Option<PanelCatalogEntry> {
    builtin_panel_catalog().into_iter().find(|entry| {
        entry.panel_version == panel_version || entry.superseded_versions.contains(&panel_version)
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
        measure_text(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_sparse_text(
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
        measure_number(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_rank(
                "syn.timeline.event_time_rank.v1",
                Modality::Structured,
                0,
                RECENCY_RANK_MAX_UNIX_MS_MICROS,
            ),
            record.ts_ns / NS_PER_MS,
        )?,
    );

    slots.insert(
        TL_SLOT_TITLE_BM25,
        measure_text(
            SYN_TIMELINE_PANEL_NAME,
            AlgorithmicLens::syn_sparse_text_tf(
                "syn.timeline.title_bm25.v1",
                Modality::Structured,
                2048,
            ),
            timeline_title(record).as_deref().unwrap_or(""),
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
        measure_text(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_sparse_text(
                "syn.episode.title_sparse.v1",
                Modality::Structured,
                4096,
            ),
            episode_title_text(record).as_str(),
        )?,
    );
    slots.insert(
        EP_SLOT_TITLE_BM25,
        measure_text(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_sparse_text_tf(
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
        measure_number(
            SYN_EPISODE_PANEL_NAME,
            AlgorithmicLens::syn_scalar_rank(
                "syn.episode.duration_rank.v1",
                Modality::Structured,
                0,
                MAX_DAY_DURATION_MS_MICROS,
            ),
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
            AlgorithmicLens::syn_record_vector(
                "syn.episode.record_vector.v1",
                Modality::Structured,
                64,
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

/// Returns the complete built-in contract for a known panel generation.
///
/// Unknown generations return `Ok(None)` and are never synthesized from a
/// nearby version.
///
/// # Errors
///
/// Returns an error when a built-in slot has a non-reconstructable runtime or
/// when its frozen lens contract and structured registry specification differ.
#[must_use = "panel contract errors and unknown generations must be handled"]
pub fn syn_active_panel_contract(
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
        SYN_MCP_USAGE_PANEL_VERSION => mcp_usage_panel_slots(panel_version, &mut registry)?,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION => {
            agent_transcript_panel_slots(panel_version, &mut registry)?
        }
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

/// Builds one `Active`, content (non-retrieval-only) panel slot from the same
/// frozen lens the ingest path measures with, so the slot's `lens_id`/`shape`/
/// `modality` are authoritative rather than reconstructed.
fn syn_content_slot(
    slot_id: SlotId,
    slot_key: &str,
    lens: RegistryAlgorithmicLens,
    panel_version: u32,
    registry: &mut Registry,
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
        retrieval_only: false,
        excluded_from_dedup: false,
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
        retrieval_only: false,
        excluded_from_dedup: false,
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
        RegistryAlgorithmicEncoder::SynOneHot { buckets } => {
            format!("syn_one_hot:{buckets}")
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
/// different slot sets and are deliberately absent: `syn_active_panel_contract`
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
            "syn.agent_transcript.status_onehot.v1",
            RegistryAlgorithmicLens::syn_one_hot(
                "syn.agent_transcript.status_onehot.v1",
                Modality::Structured,
                8,
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
        syn_content_slot(
            AT_SLOT_LINE_RANK,
            "syn.agent_transcript.line_rank.v1",
            RegistryAlgorithmicLens::syn_scalar_rank(
                "syn.agent_transcript.line_rank.v1",
                Modality::Structured,
                0,
                10_000_000_000,
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
            AT_SLOT_RECORD_VECTOR,
            "syn.agent_transcript.record_vector.v1",
            RegistryAlgorithmicLens::syn_record_vector(
                "syn.agent_transcript.record_vector.v1",
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
        syn_content_slot(
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
        syn_content_slot(
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
            "syn.episode.record_vector.v1",
            RegistryAlgorithmicLens::syn_record_vector(
                "syn.episode.record_vector.v1",
                Modality::Structured,
                64,
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
        syn_content_slot(
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
            MU_SLOT_RECORD_VECTOR,
            "syn.mcp_usage.record_vector.v1",
            RegistryAlgorithmicLens::syn_record_vector(
                "syn.mcp_usage.record_vector.v1",
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
    let end_state_name = optional_agent_end_state_name(record.end_state)?;
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
        optional_onehot_slot(
            SYN_AGENT_EVENT_PANEL_NAME,
            "syn.agent_event.end_state_onehot.v1",
            end_state_name.as_deref(),
            8,
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
    slots.insert(
        AE_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_AGENT_EVENT_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.agent_event.record_vector.v1",
                Modality::Structured,
                64,
            ),
            &agent_event_numeric_record(record),
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
            AlgorithmicLens::syn_one_hot(
                "syn.agent_transcript.status_onehot.v1",
                Modality::Structured,
                8,
            ),
            &transcript_parse_status_name(record.status)?,
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
            AlgorithmicLens::syn_sparse_text(
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
            AlgorithmicLens::syn_sparse_text_tf(
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
            AlgorithmicLens::syn_sparse_text_tf(
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
        measure_number(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            AlgorithmicLens::syn_scalar_rank(
                "syn.agent_transcript.line_rank.v1",
                Modality::Structured,
                0,
                10_000_000_000,
            ),
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
        AT_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.agent_transcript.record_vector.v1",
                Modality::Structured,
                96,
            ),
            &agent_transcript_numeric_record(record),
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
            AlgorithmicLens::syn_one_hot("syn.action.kind_onehot.v1", Modality::Structured, 64),
            &action_kind(record),
        )?,
    );
    slots.insert(
        ACT_SLOT_TARGET_HASH,
        optional_hash_slot(
            SYN_ACTION_PANEL_NAME,
            "syn.action.target_hash.v1",
            action_target_text(record).as_deref(),
            2048,
        )?,
    );
    slots.insert(
        ACT_SLOT_PARAMS_RECORD_VECTOR,
        measure_json(
            SYN_ACTION_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.action.params_record_vector.v1",
                Modality::Structured,
                96,
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
    constellation(
        context,
        SYN_ACTION_PANEL_VERSION,
        source_pointer(cf::CF_ACTION_LOG, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    )
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
    slots.insert(
        RF_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_REFLEX_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.reflex.record_vector.v1",
                Modality::Structured,
                64,
            ),
            &reflex_numeric_record(record),
        )?,
    );

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
    let ts_ns = process_ts_ns(record);
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
    slots.insert(
        PR_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_PROCESS_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.process.record_vector.v1",
                Modality::Structured,
                64,
            ),
            &process_numeric_record(record),
        )?,
    );

    let scalars = process_scalars(record, raw_bytes)?;
    let metadata = process_metadata(source_key, raw_bytes, record);
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
    let hud_numeric_record = observation_hud_numeric_record(record);
    slots.insert(
        OB_SLOT_HUD_RECORD_VECTOR,
        optional_json_slot(
            SYN_OBSERVATION_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.observation.hud_scalars.v1",
                Modality::Structured,
                128,
            ),
            hud_numeric_record.as_ref(),
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
    slots.insert(
        OB_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_OBSERVATION_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.observation.record_vector.v1",
                Modality::Structured,
                128,
            ),
            &observation_numeric_record(record),
        )?,
    );

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
        OUT_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_OUTCOME_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.outcome.record_vector.v1",
                Modality::Structured,
                128,
            ),
            &outcome_numeric_record(record, raw_bytes),
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
        MU_SLOT_RECORD_VECTOR,
        measure_json(
            SYN_MCP_USAGE_PANEL_NAME,
            AlgorithmicLens::syn_record_vector(
                "syn.mcp_usage.record_vector.v1",
                Modality::Structured,
                128,
            ),
            &mcp_usage_numeric_record(record, raw_bytes),
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
        process_ts_ns(record).map(|value| value / NS_PER_MS),
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
    metadata.insert("action_kind".to_owned(), action_kind(record));
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
) -> BTreeMap<String, String> {
    let mut metadata = common_metadata(
        SYN_PROCESS_PANEL_NAME,
        cf::CF_PROCESS_HISTORY,
        source_key,
        raw_bytes,
    );
    metadata.insert("process_event_kind".to_owned(), process_event_kind(record));
    if let Some(ts_ns) = process_ts_ns(record) {
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
    metadata
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
        |value| {
            measure_number(
                panel_name,
                AlgorithmicLens::syn_scalar_rank(
                    lens_name,
                    Modality::Structured,
                    min_micros,
                    max_micros,
                ),
                value,
            )
        },
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
    lens: AlgorithmicLens,
    text: &str,
) -> StorageResult<SlotVector> {
    let input = Input::new(Modality::Structured, text.as_bytes());
    match lens.measure(&input) {
        Ok(vector) => Ok(vector),
        Err(source) if source.code == CalyxErrorCode::LensInputTooLarge.code() => {
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
                "a lens refused an over-limit input; the slot is Absent{{Error}} and the rest of \
                 the constellation is measured"
            );
            Ok(absent(AbsentReason::Error(format!(
                "{SLOT_REFUSED_ABSENT_PREFIX}{lens_id}: {}",
                source.message
            ))))
        }
        Err(source) => Err(measurement_error(
            "Calyx Syn* lens measurement failed",
            format!("{panel_name}: {}: {source}", lens.id()),
        )),
    }
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

fn episode_numeric_record(record: &EpisodeRecord) -> Value {
    json!({
        "duration_ms": record.duration_ms(),
        "row_count": record.row_count,
        "keystroke_count": record.keystroke_count,
        "click_count": record.click_count,
        "interruption_count": record.interruption_count,
        "interrupted_ms": record.interrupted_ms,
        "distinct_title_count": record.distinct_title_count,
        "start_unix_ms": record.start_ts_ns / NS_PER_MS,
        "end_unix_ms": record.end_ts_ns / NS_PER_MS,
    })
}

fn agent_event_numeric_record(record: &AgentEventRecord) -> Value {
    json!({
        "usage_input_tokens": record.attributes.usage_input_tokens.unwrap_or(0),
        "usage_output_tokens": record.attributes.usage_output_tokens.unwrap_or(0),
        "usage_cache_read_input_tokens": record
            .attributes
            .usage_cache_read_input_tokens
            .unwrap_or(0),
        "usage_cache_creation_input_tokens": record
            .attributes
            .usage_cache_creation_input_tokens
            .unwrap_or(0),
        "usage_total_tokens": agent_event_usage_total(record).unwrap_or(0),
        "duration_ms": payload_u64(&record.payload, &["duration_ms"]).unwrap_or(0),
        "has_session_id": present_u64(record.session_id.as_deref()),
        "has_spawn_id": present_u64(record.spawn_id.as_deref()),
        "has_tool_name": present_u64(record.attributes.tool_name.as_deref()),
        "has_error_type": present_u64(record.attributes.error_type.as_deref()),
        "has_end_state": bool_u64(record.end_state.is_some()),
        "ts_unix_ms": record.ts_ns / NS_PER_MS,
    })
}

fn agent_transcript_numeric_record(record: &AgentTranscriptRecord) -> Value {
    json!({
        "line_no": record.line_no,
        "turn_index": record.turn_index.unwrap_or(0),
        "raw_line_bytes": record.raw_line_bytes,
        "content_bytes": record.content_bytes.unwrap_or(0),
        "content_truncated": bool_u64(record.content_truncated),
        "tool_call_count": record.tool_calls.len(),
        "tool_argument_bytes_total": transcript_tool_argument_bytes_total(record),
        "tool_result_bytes_total": transcript_tool_result_bytes_total(record),
        "usage_input_tokens": record
            .usage
            .as_ref()
            .and_then(|usage| usage.input_tokens)
            .unwrap_or(0),
        "usage_output_tokens": record
            .usage
            .as_ref()
            .and_then(|usage| usage.output_tokens)
            .unwrap_or(0),
        "usage_cache_read_input_tokens": record
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_input_tokens)
            .unwrap_or(0),
        "usage_cache_creation_input_tokens": record
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_creation_input_tokens)
            .unwrap_or(0),
        "usage_reasoning_output_tokens": record
            .usage
            .as_ref()
            .and_then(|usage| usage.reasoning_output_tokens)
            .unwrap_or(0),
        "total_cost_micro_usd": record
            .usage
            .as_ref()
            .and_then(|usage| usage.total_cost_micro_usd)
            .unwrap_or(0),
        "model_usage_count": record
            .usage
            .as_ref()
            .map_or(0, |usage| usage.model_usage.len()),
        "usage_total_tokens": transcript_usage_total(record).unwrap_or(0),
        "ts_unix_ms": record.ts_ns / NS_PER_MS,
    })
}

fn present_u64(value: Option<&str>) -> u64 {
    bool_u64(value.and_then(non_empty).is_some())
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

fn action_kind(record: &Value) -> String {
    let tool = json_string(record, &["tool"]);
    let verb = json_string(record, &["verb"]);
    let status = json_string(record, &["status", "outcome", "phase"]);
    match (tool, verb, status) {
        (Some(tool), Some(verb), _) => format!("{tool}:{verb}"),
        (Some(tool), None, Some(status)) => format!("{tool}:{status}"),
        (Some(tool), None, None) => tool,
        (None, Some(verb), _) => verb,
        (None, None, Some(status)) => status,
        (None, None, None) => {
            json_string(record, &["row_kind"]).unwrap_or_else(|| "unknown_action".to_owned())
        }
    }
}

fn action_target_text(record: &Value) -> Option<String> {
    json_pointer_text(
        record,
        &[
            "/target",
            "/payload_bounded/target",
            "/details/target",
            "/details/request/target",
            "/actor/tool",
        ],
    )
}

fn action_numeric_record(record: &Value) -> Value {
    json!({
        "record_present": 1,
        "schema_version": json_u64(record, &["schema_version"]).unwrap_or(0),
        "ts_unix_ms": json_u64(record, &["ts_ns"]).map_or(0, |value| value / NS_PER_MS),
        "seq": json_u64(record, &["seq"]).unwrap_or(0),
        "payload_bytes": json_u64(record, &["payload_bytes"]).unwrap_or(0),
        "payload_truncated": bool_u64(json_bool(record, &["payload_truncated"]).unwrap_or(false)),
        "has_target": bool_u64(action_target_text(record).as_deref().and_then(non_empty).is_some()),
        "has_error": bool_u64(json_non_null(record, &["error"]) || json_non_null(record, &["error_code"])),
        "redacted": bool_u64(json_bool(record, &["redacted"]).unwrap_or(false)),
    })
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

fn reflex_numeric_record(record: &StoredReflexAudit) -> Value {
    json!({
        "record_present": 1,
        "schema_version": record.schema_version,
        "ts_unix_ms": record.ts_ns / NS_PER_MS,
        "step_count": record.steps.len(),
        "latency_ms": reflex_latency_ms(record).unwrap_or(0),
        "has_event_id": bool_u64(record.event_id.as_deref().and_then(non_empty).is_some()),
        "has_error": bool_u64(record.error_code.as_deref().and_then(non_empty).is_some()),
        "redacted": bool_u64(record.redacted),
    })
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
        ],
    )
}

fn process_ts_ns(record: &Value) -> Option<u64> {
    json_u64(record, &["ts_ns"]).or_else(|| {
        json_u64(record, &["launched_at_unix_ms"]).map(|value| value.saturating_mul(NS_PER_MS))
    })
}

fn process_numeric_record(record: &Value) -> Value {
    json!({
        "record_present": 1,
        "schema_version": json_u64(record, &["schema_version"]).unwrap_or(0),
        "ts_unix_ms": process_ts_ns(record).map_or(0, |value| value / NS_PER_MS),
        "pid": json_u64(record, &["pid"]).unwrap_or(0),
        "window_owner_pid": json_u64(record, &["window_owner_pid"]).unwrap_or(0),
        "has_hwnd": bool_u64(json_non_null(record, &["hwnd"])),
        "reused_existing_window": bool_u64(json_bool(record, &["reused_existing_window"]).unwrap_or(false)),
        "uptime_ms": json_u64(record, &["uptime_ms", "duration_ms"]).unwrap_or(0),
        "has_cdp_debug_port": bool_u64(json_non_null(record, &["cdp_debug_port"])),
        "has_desktop": bool_u64(json_non_null(record, &["desktop"])),
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

fn observation_hud_numeric_record(record: &StoredObservation) -> Option<Value> {
    if record.hud.by_name.is_empty() && record.hud.errors.is_empty() {
        return None;
    }
    let mut fields = BTreeMap::<String, f64>::new();
    fields.insert(
        "hud_field_count".to_owned(),
        metric_u64_as_f64(u64::try_from(record.hud.by_name.len()).unwrap_or(u64::MAX)),
    );
    fields.insert(
        "hud_error_count".to_owned(),
        metric_u64_as_f64(u64::try_from(record.hud.errors.len()).unwrap_or(u64::MAX)),
    );
    for (name, reading) in &record.hud.by_name {
        if let synapse_core::HudValue::Number(value) = reading.parsed
            && value.is_finite()
        {
            fields.insert(format!("field/{name}/value"), value);
        }
        fields.insert(
            format!("field/{name}/confidence"),
            f64::from(reading.confidence),
        );
        fields.insert(
            format!("field/{name}/stale_ms"),
            f64::from(reading.stale_ms),
        );
    }
    Some(json!(fields))
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

fn observation_numeric_record(record: &StoredObservation) -> Value {
    json!({
        "record_present": 1,
        "schema_version": record.schema_version,
        "ts_unix_ms": record.ts_ns / NS_PER_MS,
        "element_count": record.elements.len(),
        "entity_count": record.entities.len(),
        "hud_field_count": record.hud.by_name.len(),
        "hud_error_count": record.hud.errors.len(),
        "recent_event_count": record.recent_events.len(),
        "fs_recent_count": record.fs_recent.len(),
        "audio_recent_event_count": record.audio.recent_events.len(),
        "foreground_dpi_scale": record.foreground.dpi_scale,
        "foreground_monitor_index": record.foreground.monitor_index,
        "foreground_fullscreen": bool_u64(record.foreground.is_fullscreen),
        "foreground_dwm_composed": bool_u64(record.foreground.is_dwm_composed),
        "assembled_in_ms": record.diagnostics.assembled_in_ms,
        "size_bytes": record.diagnostics.size_bytes,
        "size_estimate_tokens": record.diagnostics.size_estimate_tokens,
        "elements_truncated": bool_u64(record.diagnostics.elements_truncated),
        "entities_truncated": bool_u64(record.diagnostics.entities_truncated),
        "redacted": bool_u64(record.redacted),
    })
}

const fn sensor_status_code(status: &SensorStatus) -> &'static str {
    match status {
        SensorStatus::Healthy => "healthy",
        SensorStatus::DegradedLatency { .. } => "degraded_latency",
        SensorStatus::DegradedSensorFailed { .. } => "degraded_sensor_failed",
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

fn outcome_numeric_record(record: &Value, raw_bytes: &[u8]) -> Value {
    json!({
        "raw_len_bytes": u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
        "has_event": bool_u64(outcome_event(record).is_some()),
        "has_status": bool_u64(outcome_status(record).is_some()),
        "has_target": bool_u64(outcome_target(record).is_some()),
        "has_timestamp": bool_u64(outcome_ts_ns(record).is_some()),
        "code_count": json_u64(record, &["code_count"]).unwrap_or(0),
        "ladder_index": json_u64(record, &["ladder_index"]).unwrap_or(0),
    })
}

fn mcp_usage_numeric_record(record: &Value, raw_bytes: &[u8]) -> Value {
    json!({
        "raw_len_bytes": u64::try_from(raw_bytes.len()).unwrap_or(u64::MAX),
        "schema_version": json_u64(record, &["schema_version"]).unwrap_or(0),
        "seq": json_u64(record, &["seq"]).unwrap_or(0),
        "session_sequence_position": json_u64(record, &["session_sequence_position"]).unwrap_or(0),
        "duration_ms": json_u64(record, &["duration_ms"]).unwrap_or(0),
        "response_size_bytes": json_u64(record, &["response_size_bytes"]).unwrap_or(0),
        "response_content_count": json_u64(record, &["response_content_count"]).unwrap_or(0),
        "argument_top_level_key_count": json_u64(record, &["argument_top_level_key_count"]).unwrap_or(0),
        "argument_nested_path_count": json_u64(record, &["argument_nested_path_count"]).unwrap_or(0),
        "has_tool": bool_u64(json_string(record, &["tool"]).is_some()),
        "has_operation": bool_u64(json_string(record, &["operation"]).is_some()),
        "has_route_id": bool_u64(json_string(record, &["route_id"]).is_some()),
        "has_profile": bool_u64(json_string(record, &["profile"]).is_some()),
        "has_surface_hash": bool_u64(json_string(record, &["tool_surface_sha256"]).is_some()),
        "has_session": bool_u64(json_string(record, &["mcp_session_id_sha256"]).is_some()),
        "has_error": bool_u64(json_string(record, &["error_type"]).is_some()),
        "steering_emitted": bool_u64(json_bool(record, &["steering_emitted"]).unwrap_or(false)),
        "finished_unix_ms": json_u64(record, &["finished_at_unix_ms"]).unwrap_or(0),
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
