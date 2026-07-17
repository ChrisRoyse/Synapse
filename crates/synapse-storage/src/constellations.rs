use std::collections::BTreeMap;
use std::time::Duration;

use calyx_core::{
    AbsentReason, Constellation, CxFlags, CxId, Input, InputRef, LedgerRef, Lens, Modality, SlotId,
    SlotVector, VaultId,
};
use calyx_registry::measure::{absent, input_hash};
use calyx_registry::runtime::algorithmic::AlgorithmicLens;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_calyx::SynapseCalyxPutDisposition;
use synapse_core::types::{
    EpisodeBoundary, EpisodeRecord, TimelineActor, TimelineKind, TimelineRecord,
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
const MAX_EXACT_F64_INT: u64 = 9_007_199_254_740_991;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;
const SECS_PER_HOUR: u64 = 60 * 60;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;
const RECENCY_RANK_MAX_UNIX_MS: i64 = 4_102_444_800_000;
const MAX_DAY_DURATION_MS: i64 = 86_400_000;

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
                RECENCY_RANK_MAX_UNIX_MS * 1_000_000,
            ),
            record.ts_ns / NS_PER_MS,
        )?,
    );

    let scalars = timeline_scalars(record, raw_bytes)?;
    let metadata = timeline_metadata(source_key, raw_bytes, record)?;
    Ok(constellation(
        context,
        SYN_TIMELINE_PANEL_VERSION,
        source_pointer(cf::CF_TIMELINE, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    ))
}

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
                MAX_DAY_DURATION_MS * 1_000_000,
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
    Ok(constellation(
        context,
        SYN_EPISODE_PANEL_VERSION,
        source_pointer(cf::CF_EPISODES, source_key),
        raw_bytes,
        slots,
        scalars,
        metadata,
    ))
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
    .record(report.duration_us as f64);
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
    .record(duration_us(duration) as f64);
}

pub fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(digest.as_ref())
}

fn constellation(
    context: NativeConstellationContext,
    panel_version: u32,
    pointer: String,
    raw_bytes: &[u8],
    slots: BTreeMap<SlotId, SlotVector>,
    scalars: BTreeMap<String, f64>,
    metadata: BTreeMap<String, String>,
) -> Constellation {
    Constellation {
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
    }
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
    scalars.insert(name.to_owned(), value as f64);
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
        metadata.insert(
            "timeline_agent_session_id".to_owned(),
            session_id.to_string(),
        );
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
        metadata.insert(
            "episode_agent_session_id".to_owned(),
            session_id.to_string(),
        );
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
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(|source| StorageError::EncodeJson {
        type_name: "calyx_constellation_slot_input",
        source,
    })?;
    measure_input(panel_name, lens, Input::new(Modality::Structured, bytes))
}

fn optional_hash_slot(
    panel_name: &'static str,
    lens_name: &'static str,
    value: Option<&str>,
    dim: u32,
) -> StorageResult<SlotVector> {
    match value.and_then(non_empty) {
        Some(text) => measure_text(
            panel_name,
            AlgorithmicLens::syn_hash(lens_name, Modality::Structured, dim),
            text,
        ),
        None => Ok(absent(AbsentReason::NotApplicable)),
    }
}

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

fn measurement_error(action: &'static str, detail: impl ToString) -> StorageError {
    StorageError::WriteFailed {
        cf_name: "calyx_constellation".to_owned(),
        detail: format!("{action}: {}", detail.to_string()),
    }
}

fn timeline_kind_name(kind: TimelineKind) -> StorageResult<String> {
    snake_case_name(kind, "TimelineKind")
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

fn actor_kind(actor: &TimelineActor) -> &'static str {
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

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn utc_hour_and_dow(ts_ns: u64) -> (u64, u64) {
    let secs = ts_ns / NS_PER_SEC;
    let hour = (secs / SECS_PER_HOUR) % 24;
    let days = secs / SECS_PER_DAY;
    let dow_monday_zero = (days + 3) % 7;
    (hour, dow_monday_zero)
}

fn interruption_ratio(record: &EpisodeRecord) -> f64 {
    let duration = record.duration_ms();
    if duration == 0 {
        0.0
    } else {
        record.interrupted_ms as f64 / duration as f64
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
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped
            .split_once(']')
            .map_or(host_port, |(ipv6, _rest)| ipv6)
    } else {
        host_port
            .split_once(':')
            .map_or(host_port, |(host, _port)| host)
    };
    non_empty(host).map(|value| value.to_ascii_lowercase())
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
