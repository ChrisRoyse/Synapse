use rmcp::{ErrorData, schemars::JsonSchema};
use std::time::Instant;

use schemars::{Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use synapse_audio::{
    AudioError, AudioRuntime, AudioWindow, MAX_RING_SECONDS, Transcription,
    direction::estimate_direction,
    ring::{DEFAULT_SAMPLE_RATE_HZ, STEREO_CHANNELS},
};
use synapse_core::{AudioContext, AudioEvent, DirectionEstimate, SensorStatus, error_codes};
use synapse_perception::ObservationInput;

use crate::{
    m1::mcp_error,
    m3::{
        M3ToolStub, SharedM3State,
        permissions::{Permission, RequiredPermissions, required},
    },
};

const DEFAULT_SECONDS: f64 = 5.0;
const DEFAULT_LANGUAGE: &str = "en";
const PCM_FORMAT: &str = "s16le";
const WHISPER_TINY_MODEL_ID: &str = "whisper_tiny_int8";
const BYTES_PER_SAMPLE: usize = 2;
const SUMMARY_SECONDS: f32 = 1.0;
const MAX_SUMMARY_EVENTS: usize = 5;
const VAD_FRAME_SECONDS: f32 = 0.02;
const VAD_SPEECH_DB: f32 = -35.0;

const fn default_seconds() -> f64 {
    DEFAULT_SECONDS
}

fn default_language() -> String {
    DEFAULT_LANGUAGE.to_owned()
}

fn seconds_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "number",
        "minimum": 0,
        "maximum": MAX_RING_SECONDS,
        "default": DEFAULT_SECONDS
    })
}

fn language_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "default": DEFAULT_LANGUAGE
    })
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioTailParams {
    #[serde(default = "default_seconds")]
    #[schemars(schema_with = "seconds_schema")]
    pub seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioTailResponse {
    pub pcm: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: String,
    pub requested_seconds: f64,
    pub captured_seconds: f64,
    pub frames: usize,
    pub rms_db: f32,
    pub vad_speech_pct: f32,
    pub recent_events: Vec<AudioEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction_estimate: Option<DirectionEstimate>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioTranscribeParams {
    #[serde(default = "default_seconds")]
    #[schemars(schema_with = "seconds_schema")]
    pub seconds: f64,
    #[serde(default = "default_language")]
    #[schemars(schema_with = "language_schema")]
    pub language: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioTranscribeResponse {
    pub text: String,
    pub confidence: f32,
    pub confidence_source: String,
    pub latency_ms: u64,
    pub model_id: String,
}

#[must_use]
pub const fn audio_tail() -> M3ToolStub {
    M3ToolStub::new("audio_tail")
}

#[must_use]
pub const fn audio_transcribe() -> M3ToolStub {
    M3ToolStub::new("audio_transcribe")
}

#[must_use]
pub fn required_permissions_tail(_params: &AudioTailParams) -> RequiredPermissions {
    required([Permission::ReadAudio])
}

#[must_use]
pub fn required_permissions_transcribe(_params: &AudioTranscribeParams) -> RequiredPermissions {
    required([Permission::ReadAudio])
}

pub fn tail_audio(
    m3_state: &SharedM3State,
    params: &AudioTailParams,
) -> Result<AudioTailResponse, ErrorData> {
    validate_seconds(params.seconds)?;
    if params.seconds <= 0.0 {
        return Ok(AudioTailResponse {
            pcm: Vec::new(),
            sample_rate: DEFAULT_SAMPLE_RATE_HZ,
            channels: STEREO_CHANNELS,
            format: PCM_FORMAT.to_owned(),
            requested_seconds: 0.0,
            captured_seconds: 0.0,
            frames: 0,
            rms_db: synapse_audio::detectors::silence_db(),
            vad_speech_pct: 0.0,
            recent_events: Vec::new(),
            direction_estimate: None,
        });
    }

    let runtime = m3_state
        .lock()
        .map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned",
            )
        })?
        .ensure_audio_runtime()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    tail_audio_from_runtime(&runtime, params.seconds)
}

pub fn populate_audio_summary(m3_state: &SharedM3State, input: &mut ObservationInput) {
    let started = Instant::now();
    let mut status = SensorStatus::Disabled;
    if let Ok(mut state) = m3_state.lock() {
        if state.enable_audio {
            match state.ensure_audio_runtime() {
                Ok(runtime) => {
                    let loopback = runtime.loopback_status();
                    if let Some(reason_code) = loopback.last_error_code {
                        status = SensorStatus::DegradedSensorFailed { reason_code };
                    } else if runtime.config().start_loopback && loopback.running {
                        match audio_context_from_runtime(&runtime) {
                            Ok(context) => {
                                input.audio = context;
                                status = SensorStatus::Healthy;
                            }
                            Err(error) => {
                                status = SensorStatus::DegradedSensorFailed {
                                    reason_code: error.code().to_owned(),
                                };
                            }
                        }
                    }
                }
                Err(error) => {
                    status = SensorStatus::DegradedSensorFailed {
                        reason_code: error.code().to_owned(),
                    };
                }
            }
        }
    } else {
        status = SensorStatus::DegradedSensorFailed {
            reason_code: error_codes::TOOL_INTERNAL_ERROR.to_owned(),
        };
    }
    input.audio_status = status;
    input
        .sensor_latency_ms
        .insert("audio".to_owned(), started.elapsed().as_secs_f32() * 1000.0);
}

pub fn audio_context_from_runtime(runtime: &AudioRuntime) -> Result<AudioContext, AudioError> {
    let window = runtime.tail_seconds(SUMMARY_SECONDS)?;
    let mut context = runtime.detector_snapshot().context;
    context.rms_db = window.rms_db;
    if context.recent_events.len() > MAX_SUMMARY_EVENTS {
        let keep_from = context.recent_events.len() - MAX_SUMMARY_EVENTS;
        context.recent_events.drain(0..keep_from);
    }
    let direction = runtime.estimate_direction_tail(SUMMARY_SECONDS)?;
    context.direction_estimate = (direction.confidence > 0.0).then_some(direction);
    Ok(context)
}

pub fn tail_audio_from_runtime(
    runtime: &AudioRuntime,
    seconds: f64,
) -> Result<AudioTailResponse, ErrorData> {
    validate_seconds(seconds)?;
    let runtime_seconds = runtime_seconds(seconds);
    let window = runtime
        .tail_seconds(runtime_seconds)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let mut recent_events = runtime.detector_snapshot().context.recent_events;
    if recent_events.len() > MAX_SUMMARY_EVENTS {
        let keep_from = recent_events.len() - MAX_SUMMARY_EVENTS;
        recent_events.drain(0..keep_from);
    }
    Ok(response_from_window(&window, seconds, recent_events))
}

pub fn transcribe_audio(
    m3_state: &SharedM3State,
    params: &AudioTranscribeParams,
) -> Result<AudioTranscribeResponse, ErrorData> {
    validate_seconds(params.seconds)?;
    let language = normalize_language_param(&params.language)?;
    let runtime = m3_state
        .lock()
        .map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned",
            )
        })?
        .ensure_audio_runtime()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    transcribe_audio_from_runtime(&runtime, params.seconds, language)
}

pub fn transcribe_audio_from_runtime(
    runtime: &AudioRuntime,
    seconds: f64,
    language: &str,
) -> Result<AudioTranscribeResponse, ErrorData> {
    validate_seconds(seconds)?;
    let language = normalize_language_param(language)?;
    let transcription = runtime
        .transcribe_tail(runtime_seconds(seconds), language)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(response_from_transcription(transcription))
}

fn response_from_window(
    window: &AudioWindow,
    seconds: f64,
    recent_events: Vec<AudioEvent>,
) -> AudioTailResponse {
    let requested_samples = requested_samples(window, seconds);
    let mut pcm = Vec::with_capacity(requested_samples.saturating_mul(BYTES_PER_SAMPLE));
    let missing_samples = requested_samples.saturating_sub(window.samples.len());
    pcm.resize(missing_samples.saturating_mul(BYTES_PER_SAMPLE), 0);
    pcm.extend_from_slice(&window.pcm_i16_le());
    let direction = estimate_direction(window);

    AudioTailResponse {
        pcm,
        sample_rate: window.format.sample_rate_hz,
        channels: window.format.channels,
        format: PCM_FORMAT.to_owned(),
        requested_seconds: seconds,
        captured_seconds: captured_seconds(window),
        frames: window.frames,
        rms_db: window.rms_db,
        vad_speech_pct: vad_speech_pct(window),
        recent_events,
        direction_estimate: (direction.confidence > 0.0).then_some(direction),
    }
}

fn requested_samples(window: &AudioWindow, seconds: f64) -> usize {
    requested_frames(seconds, window.format.sample_rate_hz)
        .saturating_mul(usize::from(window.format.channels))
}

fn validate_seconds(seconds: f64) -> Result<(), ErrorData> {
    if !seconds.is_finite() || seconds < 0.0 || seconds > f64::from(MAX_RING_SECONDS) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("audio seconds must be between 0 and {MAX_RING_SECONDS}; got {seconds}"),
        ));
    }
    Ok(())
}

fn normalize_language_param(language: &str) -> Result<&'static str, ErrorData> {
    let language = language.trim();
    if language.is_empty() || language.eq_ignore_ascii_case(DEFAULT_LANGUAGE) {
        Ok(DEFAULT_LANGUAGE)
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("audio_transcribe language must be {DEFAULT_LANGUAGE:?}; got {language:?}"),
        ))
    }
}

fn response_from_transcription(transcription: Transcription) -> AudioTranscribeResponse {
    AudioTranscribeResponse {
        text: transcription.text,
        confidence: transcription.confidence,
        confidence_source: transcription.confidence_source.as_str().to_owned(),
        latency_ms: u64::try_from(transcription.elapsed_ms).unwrap_or(u64::MAX),
        model_id: WHISPER_TINY_MODEL_ID.to_owned(),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn requested_frames(seconds: f64, sample_rate_hz: u32) -> usize {
    (seconds * f64::from(sample_rate_hz)).round() as usize
}

#[allow(clippy::cast_precision_loss)]
fn captured_seconds(window: &AudioWindow) -> f64 {
    window.frames as f64 / f64::from(window.format.sample_rate_hz.max(1))
}

#[allow(clippy::cast_possible_truncation)]
fn runtime_seconds(seconds: f64) -> f32 {
    seconds as f32
}

#[allow(clippy::cast_precision_loss)]
fn vad_speech_pct(window: &AudioWindow) -> f32 {
    if window.frames == 0 || window.samples.is_empty() {
        return 0.0;
    }
    let channels = usize::from(window.format.channels.max(1));
    let frame_count = ((f64::from(window.format.sample_rate_hz) * f64::from(VAD_FRAME_SECONDS))
        .round() as usize)
        .max(1);
    let samples_per_chunk = frame_count.saturating_mul(channels).max(channels);
    let mut total = 0_usize;
    let mut speech = 0_usize;
    for chunk in window.samples.chunks(samples_per_chunk) {
        if chunk.len() < channels {
            continue;
        }
        total = total.saturating_add(1);
        if synapse_audio::detectors::rms_db(chunk) >= VAD_SPEECH_DB {
            speech = speech.saturating_add(1);
        }
    }
    if total == 0 {
        0.0
    } else {
        (speech as f32 / total as f32) * 100.0
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::float_cmp,
    reason = "unit tests intentionally assert exact sentinel values and failure paths"
)]
mod tests {
    use synapse_audio::{AudioConfig, AudioFormat, AudioRuntime};

    use super::*;

    #[test]
    fn response_pads_partial_ring_to_requested_byte_count() -> anyhow::Result<()> {
        let runtime = AudioRuntime::spawn(AudioConfig::default())?;
        let ring = runtime.ring();
        ring.set_format(AudioFormat {
            sample_rate_hz: 48_000,
            channels: 2,
        });
        ring.push_interleaved(&vec![0.25; 48_000 * 2]);

        let response = tail_audio_from_runtime(&runtime, 2.0)
            .map_err(|error| anyhow::anyhow!("tail_audio failed: {error:?}"))?;

        assert_eq!(response.sample_rate, 48_000);
        assert_eq!(response.channels, 2);
        assert_eq!(response.format, PCM_FORMAT);
        assert_eq!(response.requested_seconds, 2.0);
        assert_eq!(response.captured_seconds, 1.0);
        assert_eq!(response.frames, 48_000);
        assert!(response.rms_db > -13.0);
        assert!(response.vad_speech_pct > 0.0);
        assert_eq!(response.pcm.len(), 2 * 48_000 * 2 * BYTES_PER_SAMPLE);
        assert!(
            response.pcm[..48_000 * 2 * BYTES_PER_SAMPLE]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert!(
            response.pcm[48_000 * 2 * BYTES_PER_SAMPLE..]
                .iter()
                .any(|byte| *byte != 0)
        );
        Ok(())
    }

    #[test]
    fn audio_context_summary_reports_rms_and_direction_without_pcm() -> anyhow::Result<()> {
        let runtime = AudioRuntime::spawn(AudioConfig::default())?;
        let ring = runtime.ring();
        ring.set_format(AudioFormat {
            sample_rate_hz: 48_000,
            channels: 2,
        });
        let mut samples = Vec::with_capacity(48_000 * 2);
        for _ in 0..48_000 {
            samples.push(0.30);
            samples.push(0.05);
        }
        ring.push_interleaved(&samples);

        let context = audio_context_from_runtime(&runtime)?;

        assert!(context.rms_db > -20.0);
        let direction = context
            .direction_estimate
            .expect("asymmetric stereo should produce a direction estimate");
        assert!(direction.azimuth_deg < 0.0);
        assert!(direction.confidence > 0.0);
        assert!(context.recent_events.is_empty());
        Ok(())
    }

    #[test]
    fn transcribe_maps_silence_without_model_load_and_rejects_language() -> anyhow::Result<()> {
        let runtime = AudioRuntime::spawn(AudioConfig::default())?;

        let blank = transcribe_audio_from_runtime(&runtime, 5.0, "en")
            .map_err(|error| anyhow::anyhow!("transcribe silence failed: {error:?}"))?;
        assert_eq!(blank.text, "");
        assert_eq!(blank.confidence, 0.0);
        assert_eq!(blank.confidence_source, "not_applicable");
        assert_eq!(blank.latency_ms, 0);
        assert_eq!(blank.model_id, WHISPER_TINY_MODEL_ID);

        let invalid = transcribe_audio_from_runtime(&runtime, 5.0, "xx")
            .expect_err("unsupported language should fail before STT");
        assert_eq!(
            error_data_code(&invalid),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        Ok(())
    }

    #[test]
    fn transcribe_non_silence_maps_missing_model_code() -> anyhow::Result<()> {
        let runtime = AudioRuntime::spawn(AudioConfig {
            stt_model_path: Some("missing-whisper-tiny-int8.onnx".into()),
            ..AudioConfig::default()
        })?;
        let ring = runtime.ring();
        ring.set_format(AudioFormat {
            sample_rate_hz: 16_000,
            channels: 1,
        });
        ring.push_interleaved(&vec![0.5; 16_000]);

        let error = transcribe_audio_from_runtime(&runtime, 1.0, "en")
            .expect_err("missing model should fail for non-silent audio");
        assert_eq!(
            error_data_code(&error),
            Some(error_codes::AUDIO_STT_MODEL_NOT_LOADED)
        );
        Ok(())
    }

    fn error_data_code(error: &ErrorData) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }
}
