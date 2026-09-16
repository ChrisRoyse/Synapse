use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_core::{AudioContext, AudioEvent, Event, EventSource};

use crate::{AudioEventSink, ring::AudioFormat};

pub const AUDIO_EVENTS_TOTAL: &str = "audio_events_total";
pub const AUDIO_RMS_DB: &str = "audio_rms_db";
pub const LOUD_TRANSIENT: &str = "loud_transient";
pub const SPEECH_STARTED: &str = "speech_started";
pub const SPEECH_ENDED: &str = "speech_ended";
pub const MUSIC_STARTED: &str = "music_started";
pub const MUSIC_ENDED: &str = "music_ended";

const RECENT_EVENT_CAP: usize = 64;
const RMS_FLOOR: f32 = 0.000_001;
const LOUD_RATIO: f32 = 5.0;
const LOUD_ABSOLUTE_RMS: f32 = 0.25;
const SPEECH_START_DB: f32 = -35.0;
const SPEECH_END_DB: f32 = -45.0;
const MUSIC_START_DB: f32 = -38.0;
const MUSIC_END_DB: f32 = -48.0;

#[derive(Clone, Debug, Default)]
pub struct SharedDetectorState {
    inner: Arc<Mutex<DetectorState>>,
}

#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "audio detectors track independent recent/active latches rather than one exclusive state"
)]
struct DetectorState {
    rms_db: f32,
    moving_rms: f32,
    vad_speech_recent: bool,
    speech_active: bool,
    music_active: bool,
    loud_active: bool,
    quiet_loud_frames: u64,
    silent_speech_frames: u64,
    silent_music_frames: u64,
    recent_events: VecDeque<AudioEvent>,
}

impl Default for DetectorState {
    fn default() -> Self {
        Self {
            rms_db: silence_db(),
            moving_rms: RMS_FLOOR,
            vad_speech_recent: false,
            speech_active: false,
            music_active: false,
            loud_active: false,
            quiet_loud_frames: 0,
            silent_speech_frames: 0,
            silent_music_frames: 0,
            recent_events: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorSnapshot {
    pub context: AudioContext,
    pub moving_rms_db: f32,
    pub speech_active: bool,
    pub music_active: bool,
}

impl SharedDetectorState {
    #[must_use]
    pub fn snapshot(&self) -> DetectorSnapshot {
        let state = self.lock();
        DetectorSnapshot {
            context: AudioContext {
                rms_db: state.rms_db,
                vad_speech_recent: state.vad_speech_recent,
                recent_events: state.recent_events.iter().cloned().collect(),
                direction_estimate: None,
            },
            moving_rms_db: linear_to_db(state.moving_rms),
            speech_active: state.speech_active,
            music_active: state.music_active,
        }
    }

    fn lock(&self) -> MutexGuard<'_, DetectorState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

pub struct DetectorProcessor {
    state: SharedDetectorState,
    sink: AudioEventSink,
    next_seq: Arc<AtomicU64>,
}

impl DetectorProcessor {
    #[must_use]
    pub fn new(state: SharedDetectorState, sink: AudioEventSink) -> Self {
        Self {
            state,
            sink,
            next_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn process(&mut self, samples: &[f32], format: AudioFormat) {
        if samples.is_empty() {
            return;
        }
        let channels = usize::from(format.channels.max(1));
        let frames = u64::try_from(samples.len() / channels).unwrap_or(u64::MAX);
        let rms = rms_linear(samples);
        let peak = samples
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let crest = if rms <= RMS_FLOOR { 0.0 } else { peak / rms };
        let rms_db = linear_to_db(rms);
        metrics::gauge!(AUDIO_RMS_DB).set(f64::from(rms_db));

        let mut events = Vec::new();
        {
            let mut state = self.state.lock();
            let prior_moving = state.moving_rms.max(RMS_FLOOR);
            state.rms_db = rms_db;
            state.moving_rms = state.moving_rms.mul_add(0.95, rms * 0.05);

            let loud_surge = rms > prior_moving * LOUD_RATIO && rms_db > -24.0;
            let loud_absolute_onset =
                rms >= LOUD_ABSOLUTE_RMS && prior_moving < LOUD_ABSOLUTE_RMS / LOUD_RATIO;
            if (loud_surge || loud_absolute_onset) && !state.loud_active {
                state.loud_active = true;
                state.quiet_loud_frames = 0;
                events.push(detector_event(LOUD_TRANSIENT, rms_db, 0.95));
            }
            update_loud_reset(&mut state, rms_db, frames, format);
            update_speech(&mut state, &mut events, rms_db, frames, format);
            update_music(&mut state, &mut events, rms_db, crest, frames, format);
            for event in &events {
                push_recent(&mut state.recent_events, event.clone());
            }
            state.vad_speech_recent = state.speech_active;
        }

        for event in &events {
            self.publish(event, rms_db, format, crest);
        }
    }

    fn publish(&self, audio: &AudioEvent, rms_db: f32, format: AudioFormat, crest_factor: f32) {
        let seq = self.next_seq.fetch_add(1, Ordering::AcqRel);
        metrics::counter!(AUDIO_EVENTS_TOTAL, "kind" => audio.kind.clone()).increment(1);
        let event = Event {
            seq,
            at: audio.at,
            source: EventSource::PerceptionAudio,
            kind: audio.kind.clone(),
            data: json!({
                "rms_db": rms_db,
                "sample_rate_hz": format.sample_rate_hz,
                "channels": format.channels,
                "confidence": audio.confidence,
                "azimuth_deg": audio.azimuth_deg,
                "crest_factor": crest_factor,
            }),
            correlations: Vec::new(),
        };
        (self.sink)(event);
    }
}

fn update_loud_reset(state: &mut DetectorState, rms_db: f32, frames: u64, format: AudioFormat) {
    if rms_db <= MUSIC_END_DB {
        state.quiet_loud_frames = state.quiet_loud_frames.saturating_add(frames);
        if state.quiet_loud_frames >= u64::from(format.sample_rate_hz) / 4 {
            state.loud_active = false;
            state.quiet_loud_frames = 0;
        }
    } else {
        state.quiet_loud_frames = 0;
    }
}

fn update_speech(
    state: &mut DetectorState,
    events: &mut Vec<AudioEvent>,
    rms_db: f32,
    frames: u64,
    format: AudioFormat,
) {
    if rms_db >= SPEECH_START_DB {
        state.silent_speech_frames = 0;
        if !state.speech_active {
            state.speech_active = true;
            events.push(detector_event(SPEECH_STARTED, rms_db, 0.85));
        }
        return;
    }
    if rms_db <= SPEECH_END_DB {
        state.silent_speech_frames = state.silent_speech_frames.saturating_add(frames);
        if state.speech_active && state.silent_speech_frames >= u64::from(format.sample_rate_hz) / 2
        {
            state.speech_active = false;
            state.silent_speech_frames = 0;
            events.push(detector_event(SPEECH_ENDED, rms_db, 0.85));
        }
    }
}

fn update_music(
    state: &mut DetectorState,
    events: &mut Vec<AudioEvent>,
    rms_db: f32,
    crest: f32,
    frames: u64,
    format: AudioFormat,
) {
    let music_like = rms_db >= MUSIC_START_DB && (1.2..=4.0).contains(&crest);
    if music_like {
        state.silent_music_frames = 0;
        if !state.music_active {
            state.music_active = true;
            events.push(detector_event(MUSIC_STARTED, rms_db, 0.7));
        }
        return;
    }
    if rms_db <= MUSIC_END_DB {
        state.silent_music_frames = state.silent_music_frames.saturating_add(frames);
        if state.music_active && state.silent_music_frames >= u64::from(format.sample_rate_hz) {
            state.music_active = false;
            state.silent_music_frames = 0;
            events.push(detector_event(MUSIC_ENDED, rms_db, 0.7));
        }
    }
}

fn detector_event(kind: &str, rms_db: f32, confidence: f32) -> AudioEvent {
    AudioEvent {
        at: Utc::now(),
        kind: kind.to_owned(),
        azimuth_deg: None,
        confidence: confidence_for_rms(rms_db, confidence),
    }
}

fn push_recent(recent: &mut VecDeque<AudioEvent>, event: AudioEvent) {
    if recent.len() >= RECENT_EVENT_CAP {
        let _oldest = recent.pop_front();
    }
    recent.push_back(event);
}

fn confidence_for_rms(rms_db: f32, base: f32) -> f32 {
    if rms_db <= SPEECH_END_DB {
        base * 0.5
    } else {
        base
    }
    .clamp(0.0, 1.0)
}

#[must_use]
pub fn rms_db(samples: &[f32]) -> f32 {
    linear_to_db(rms_linear(samples))
}

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn rms_linear(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum = samples
        .iter()
        .copied()
        .map(|sample| {
            let sample = sample.clamp(-1.0, 1.0);
            sample * sample
        })
        .sum::<f32>();
    (sum / samples.len() as f32).sqrt()
}

#[must_use]
pub fn linear_to_db(value: f32) -> f32 {
    20.0 * value.max(RMS_FLOOR).log10()
}

#[must_use]
pub const fn silence_db() -> f32 {
    -120.0
}
