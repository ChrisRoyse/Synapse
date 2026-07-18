use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use calyx_core::{
    AbsentReason, Constellation, CxFlags, CxId, Input, InputRef, LedgerRef, Lens, Modality, SlotId,
    SlotVector, VaultId,
};
use calyx_lenses::AlgorithmicLens;
use calyx_lenses::measure::{absent, input_hash};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_calyx::SynapseCalyxPutDisposition;
use synapse_core::types::{
    AgentEndState, AgentEventKind, AgentEventRecord, AgentTranscriptRecord, EpisodeBoundary,
    EpisodeRecord, GenAiOperationName, SensorStatus, StoredObservation, StoredReflexAudit,
    TimelineActor, TimelineKind, TimelineRecord, TranscriptParseStatus, TranscriptRole,
    TranscriptSource,
};
use synapse_telemetry::metrics::{
    CALYX_CONSTELLATION_MEASUREMENT_DURATION_US, CALYX_CONSTELLATION_MEASUREMENT_ERRORS_TOTAL,
    CALYX_CONSTELLATION_MEASUREMENTS_TOTAL,
};

use crate::{StorageError, StorageResult, cf};

pub const SYN_TIMELINE_PANEL_NAME: &str = "syn-timeline-v1";
pub const SYN_TIMELINE_PANEL_VERSION: u32 = 1_664_001;
pub const SYN_EPISODE_PANEL_NAME: &str = "syn-episode-v1";
pub const SYN_EPISODE_PANEL_VERSION: u32 = 1_664_002;
pub const SYN_AGENT_EVENT_PANEL_NAME: &str = "syn-agent-event-v1";
pub const SYN_AGENT_EVENT_PANEL_VERSION: u32 = 1_665_001;
pub const SYN_AGENT_TRANSCRIPT_PANEL_NAME: &str = "syn-agent-transcript-v1";
pub const SYN_AGENT_TRANSCRIPT_PANEL_VERSION: u32 = 1_665_002;
pub const SYN_ACTION_PANEL_NAME: &str = "syn-action-v1";
pub const SYN_ACTION_PANEL_VERSION: u32 = 1_666_001;
pub const SYN_REFLEX_PANEL_NAME: &str = "syn-reflex-v1";
pub const SYN_REFLEX_PANEL_VERSION: u32 = 1_666_002;
pub const SYN_PROCESS_PANEL_NAME: &str = "syn-process-v1";
pub const SYN_PROCESS_PANEL_VERSION: u32 = 1_666_003;
pub const SYN_OBSERVATION_PANEL_NAME: &str = "syn-observation-v1";
pub const SYN_OBSERVATION_PANEL_VERSION: u32 = 1_666_004;
pub const SYN_OBSERVATION_SAMPLE_EVERY_N_ENV: &str = "SYNAPSE_CALYX_OBSERVATION_SAMPLE_EVERY_N";
pub const SYN_OBSERVATION_SAMPLE_EVERY_N_DEFAULT: u64 = 10;

pub const META_PANEL_NAME: &str = "synapse_panel_name";
pub const META_SOURCE_CF: &str = "synapse_source_cf";
pub const META_SOURCE_KEY_HEX: &str = "synapse_source_key_hex";
pub const META_RAW_SHA256: &str = "synapse_raw_sha256";
pub const META_RAW_LEN_BYTES: &str = "synapse_raw_len_bytes";
pub const META_TIME_BASIS: &str = "synapse_time_basis";
pub const META_EXACT_TS_NS: &str = "synapse_ts_ns";
pub const META_RECENCY_BASIS: &str = "synapse_recency_basis";

const POINTER_SCHEME: &str = "synapse";
const TIME_BASIS_UTC: &str = "utc";
const RECENCY_BASIS_EVENT_TIME_RANK: &str = "frozen_event_unix_ms_rank_1970_2100";
const CALYX_DURABLE_SLOT_ID_MAX: u16 = 47;
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

// Slot ids are local to each panel/constellation. Calyx's durable slot CF tag
// codec currently supports 0..=47, so newer panels intentionally reuse local
// slots instead of extending past the durable tag budget.
const ACT_SLOT_KIND_ONEHOT: SlotId = SlotId::new(1);
const ACT_SLOT_TARGET_HASH: SlotId = SlotId::new(2);
const ACT_SLOT_PARAMS_RECORD_VECTOR: SlotId = SlotId::new(3);
const ACT_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(4);
const ACT_SLOT_DOW_CYCLIC: SlotId = SlotId::new(5);

const RF_SLOT_REFLEX_HASH: SlotId = SlotId::new(1);
const RF_SLOT_OUTCOME_ONEHOT: SlotId = SlotId::new(2);
const RF_SLOT_LATENCY_LOG1P: SlotId = SlotId::new(3);
const RF_SLOT_STEP_COUNT_LOG1P: SlotId = SlotId::new(4);
const RF_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(5);
const RF_SLOT_DOW_CYCLIC: SlotId = SlotId::new(6);
const RF_SLOT_RECORD_VECTOR: SlotId = SlotId::new(7);

const PR_SLOT_PROCESS_HASH: SlotId = SlotId::new(1);
const PR_SLOT_EVENT_ONEHOT: SlotId = SlotId::new(2);
const PR_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(3);
const PR_SLOT_DOW_CYCLIC: SlotId = SlotId::new(4);
const PR_SLOT_UPTIME_LOG1P: SlotId = SlotId::new(5);
const PR_SLOT_RECENCY_RANK: SlotId = SlotId::new(6);
const PR_SLOT_RECORD_VECTOR: SlotId = SlotId::new(7);

const OB_SLOT_APP_HASH: SlotId = SlotId::new(1);
const OB_SLOT_ROLE_HISTOGRAM: SlotId = SlotId::new(2);
const OB_SLOT_ENTITY_MULTI_HOT: SlotId = SlotId::new(3);
const OB_SLOT_HUD_RECORD_VECTOR: SlotId = SlotId::new(4);
const OB_SLOT_FLAGS_MULTI_HOT: SlotId = SlotId::new(5);
const OB_SLOT_HOUR_CYCLIC: SlotId = SlotId::new(6);
const OB_SLOT_DOW_CYCLIC: SlotId = SlotId::new(7);
const OB_SLOT_RECORD_VECTOR: SlotId = SlotId::new(8);

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
    slots.insert(
        AT_SLOT_TEXT_SPARSE,
        measure_text(
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            AlgorithmicLens::syn_sparse_text(
                "syn.agent_transcript.text_sparse.v1",
                Modality::Structured,
                4096,
            ),
            &transcript_text(record),
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
    validate_durable_slot_budget(panel_version, &pointer, &slots)?;
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

fn validate_durable_slot_budget(
    panel_version: u32,
    pointer: &str,
    slots: &BTreeMap<SlotId, SlotVector>,
) -> StorageResult<()> {
    for slot in slots.keys() {
        if slot.get() > CALYX_DURABLE_SLOT_ID_MAX {
            return Err(StorageError::WriteFailed {
                cf_name: "calyx_constellation".to_owned(),
                detail: format!(
                    "panel_version={panel_version} pointer={pointer} uses slot id {} beyond Calyx durable slot CF tag maximum {CALYX_DURABLE_SLOT_ID_MAX}; slot ids are local to each panel and must stay within 0..={CALYX_DURABLE_SLOT_ID_MAX}",
                    slot.get()
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
    source_cf: &'static str,
    source_key: &[u8],
    raw_bytes: &[u8],
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert(META_PANEL_NAME.to_owned(), panel_name.to_owned());
    metadata.insert(META_SOURCE_CF.to_owned(), source_cf.to_owned());
    metadata.insert(META_SOURCE_KEY_HEX.to_owned(), hex_encode(source_key));
    metadata.insert(META_RAW_SHA256.to_owned(), sha256_hex(raw_bytes));
    metadata.insert(META_RAW_LEN_BYTES.to_owned(), raw_bytes.len().to_string());
    metadata
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

fn source_pointer(source_cf: &'static str, source_key: &[u8]) -> String {
    format!("{POINTER_SCHEME}://{source_cf}/{}", hex_encode(source_key))
}

fn truncate_metadata(value: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut output = String::new();
    for ch in value.chars().take(MAX_CHARS) {
        output.push(ch);
    }
    output
}
