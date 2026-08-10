use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{AudioError, AudioResult, detectors::rms_db};

pub const DEFAULT_SAMPLE_RATE_HZ: u32 = 48_000;
pub const STEREO_CHANNELS: u16 = 2;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self {
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            channels: STEREO_CHANNELS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioWindow {
    pub format: AudioFormat,
    pub frames: usize,
    /// Frames in this window supplied by a real WASAPI packet. The remaining
    /// frames are explicit timeline gaps and contain zero-valued samples.
    pub device_frames: usize,
    pub timeline_gap_frames: usize,
    pub samples: Vec<f32>,
    pub rms_db: f32,
}

/// Authoritative timing metadata for the first frame of one WASAPI packet.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioPacketTimeline {
    pub device_position: u64,
    pub qpc_position_100ns: u64,
    pub data_discontinuity: bool,
    pub timestamp_error: bool,
}

/// Observable result of placing one packet on the real device timeline.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioPacketWriteOutcome {
    pub expected_device_position: Option<u64>,
    pub actual_device_position: u64,
    pub gap_frames: u64,
    pub data_discontinuity: bool,
}

impl AudioWindow {
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn pcm_i16_le(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.samples.len().saturating_mul(2));
        for sample in &self.samples {
            let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }
}

#[derive(Debug)]
pub struct AudioRing {
    inner: Mutex<RingState>,
    max_seconds: u32,
}

#[derive(Debug)]
struct RingState {
    format: AudioFormat,
    samples: Vec<f32>,
    device_frame_mask: Vec<u8>,
    total_frames: u64,
    last_packet_position: Option<u64>,
    next_device_position: Option<u64>,
    last_packet_qpc_100ns: Option<u64>,
    last_packet_observed_at: Option<Instant>,
}

impl AudioRing {
    #[must_use]
    pub fn new(max_seconds: u32) -> Self {
        let format = AudioFormat::default();
        Self {
            inner: Mutex::new(RingState {
                format,
                samples: vec![0.0; capacity_samples(max_seconds, format)],
                device_frame_mask: vec![0; capacity_frames(max_seconds, format)],
                total_frames: 0,
                last_packet_position: None,
                next_device_position: None,
                last_packet_qpc_100ns: None,
                last_packet_observed_at: None,
            }),
            max_seconds,
        }
    }

    #[must_use]
    pub const fn max_seconds(&self) -> u32 {
        self.max_seconds
    }

    #[must_use]
    pub fn format(&self) -> AudioFormat {
        self.lock().format
    }

    #[must_use]
    pub fn frames_available(&self) -> usize {
        let state = self.lock();
        let capacity_frames = self.capacity_frames(&state);
        let trailing = trailing_gap_frames(&state).min(capacity_frames);
        let available = available_frames(&state, capacity_frames)
            .saturating_add(trailing)
            .min(capacity_frames);
        drop(state);
        available
    }

    #[must_use]
    pub fn total_frames(&self) -> u64 {
        self.lock().total_frames
    }

    pub fn set_format(&self, format: AudioFormat) {
        let mut state = self.lock();
        if state.format != format {
            state.format = format;
            state.samples = vec![0.0; capacity_samples(self.max_seconds, format)];
            state.device_frame_mask = vec![0; capacity_frames(self.max_seconds, format)];
            state.total_frames = 0;
            state.last_packet_position = None;
            state.next_device_position = None;
            state.last_packet_qpc_100ns = None;
            state.last_packet_observed_at = None;
        }
    }

    /// Appends one real capture packet on the device's stream timeline.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::TimelineInvalid`] if WASAPI marks the timestamp
    /// invalid or supplies a regressing/overlapping device or QPC timeline.
    /// Discontinuity flags are observable outcomes; their authoritative
    /// device-position gaps are materialized as silence.
    pub fn push_packet(
        &self,
        samples: &[f32],
        timeline: AudioPacketTimeline,
    ) -> AudioResult<AudioPacketWriteOutcome> {
        let mut state = self.lock();
        let channels = usize::from(state.format.channels);
        let capacity_frames = self.capacity_frames(&state);
        if channels == 0 || capacity_frames == 0 {
            return Err(timeline_invalid(format!(
                "ring has unusable format sample_rate_hz={} channels={}; repair: restart the daemon with a valid WASAPI mix format",
                state.format.sample_rate_hz, state.format.channels
            )));
        }
        if !samples.len().is_multiple_of(channels) {
            return Err(timeline_invalid(format!(
                "packet sample count {} is not divisible by channel count {channels}; repair: inspect the endpoint mix format and restart audio capture",
                samples.len()
            )));
        }
        if timeline.timestamp_error {
            return Err(timeline_invalid(format!(
                "WASAPI marked packet timestamp invalid at device_position={} qpc_100ns={}; repair: inspect the endpoint/driver clock and restart audio capture",
                timeline.device_position, timeline.qpc_position_100ns
            )));
        }
        let packet_frames = u64::try_from(samples.len() / channels).map_err(|_| {
            timeline_invalid(
                "packet frame count does not fit u64; repair: inspect capture buffer sizing",
            )
        })?;
        if packet_frames == 0 {
            return Err(timeline_invalid(
                "WASAPI returned an empty packet after reporting data; repair: inspect the endpoint driver and restart audio capture",
            ));
        }
        validate_packet_timeline(&state, timeline)?;

        let expected_device_position = state.next_device_position;
        let gap_frames = if let Some(expected) = expected_device_position {
            let gap = timeline.device_position.checked_sub(expected).ok_or_else(|| {
                timeline_invalid(format!(
                    "packet overlaps or regresses: expected_position={expected} actual_position={}; repair: inspect endpoint reset/device change and restart audio capture",
                    timeline.device_position
                ))
            })?;
            write_silence(&mut state, capacity_frames, channels, gap)?;
            gap
        } else {
            0
        };
        write_device_frames(&mut state, capacity_frames, channels, samples)?;
        state.last_packet_position = Some(timeline.device_position);
        state.next_device_position = Some(
            timeline
                .device_position
                .checked_add(packet_frames)
                .ok_or_else(|| {
                    timeline_invalid(
                        "device position overflowed u64; repair: restart the audio stream",
                    )
                })?,
        );
        state.last_packet_qpc_100ns = Some(timeline.qpc_position_100ns);
        state.last_packet_observed_at = Some(Instant::now());
        drop(state);
        Ok(AudioPacketWriteOutcome {
            expected_device_position,
            actual_device_position: timeline.device_position,
            gap_frames,
            data_discontinuity: timeline.data_discontinuity,
        })
    }

    /// Returns the last `seconds` of interleaved f32 samples.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::LoopbackInitFailed`] when `seconds` is negative,
    /// non-finite, or exceeds this ring's configured capacity.
    pub fn tail_seconds(&self, seconds: f32) -> AudioResult<AudioWindow> {
        if !seconds.is_finite() || seconds < 0.0 || f64::from(seconds) > f64::from(self.max_seconds)
        {
            return Err(AudioError::LoopbackInitFailed {
                detail: format!(
                    "audio tail seconds must be between 0 and {}, got {seconds}",
                    self.max_seconds
                ),
            });
        }

        let state = self.lock();
        let channels = usize::from(state.format.channels);
        let capacity_frames = self.capacity_frames(&state);
        let requested = requested_frames(seconds, state.format.sample_rate_hz);
        let stored_available = available_frames(&state, capacity_frames);
        let trailing_gap = trailing_gap_frames(&state).min(capacity_frames);
        let available = stored_available
            .saturating_add(trailing_gap)
            .min(capacity_frames);
        let frames = requested.min(available);
        let trailing_in_window = trailing_gap.min(frames);
        let stored_in_window = frames.saturating_sub(trailing_in_window);
        let mut samples = Vec::with_capacity(frames.saturating_mul(channels));
        let start = state.total_frames.saturating_sub(stored_in_window as u64);
        let mut device_frames = 0_usize;
        for frame_offset in 0..stored_in_window {
            let absolute = start.saturating_add(frame_offset as u64);
            let frame_index = ring_index(absolute, capacity_frames);
            let sample_index = frame_index * channels;
            samples.extend_from_slice(&state.samples[sample_index..sample_index + channels]);
            device_frames =
                device_frames.saturating_add(usize::from(state.device_frame_mask[frame_index]));
        }
        samples.resize(frames.saturating_mul(channels), 0.0);
        Ok(AudioWindow {
            format: state.format,
            frames,
            device_frames,
            timeline_gap_frames: frames.saturating_sub(device_frames),
            rms_db: rms_db(&samples),
            samples,
        })
    }

    fn capacity_frames(&self, state: &RingState) -> usize {
        usize::try_from(state.format.sample_rate_hz)
            .unwrap_or(usize::MAX)
            .saturating_mul(self.max_seconds as usize)
    }

    fn lock(&self) -> MutexGuard<'_, RingState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn capacity_samples(seconds: u32, format: AudioFormat) -> usize {
    usize::try_from(format.sample_rate_hz)
        .unwrap_or(usize::MAX)
        .saturating_mul(seconds as usize)
        .saturating_mul(usize::from(format.channels))
}

fn capacity_frames(seconds: u32, format: AudioFormat) -> usize {
    usize::try_from(format.sample_rate_hz)
        .unwrap_or(usize::MAX)
        .saturating_mul(seconds as usize)
}

fn available_frames(state: &RingState, capacity_frames: usize) -> usize {
    usize::try_from(state.total_frames)
        .unwrap_or(usize::MAX)
        .min(capacity_frames)
}

fn trailing_gap_frames(state: &RingState) -> usize {
    state.last_packet_observed_at.map_or(0, |observed| {
        duration_frames(observed.elapsed(), state.format.sample_rate_hz)
    })
}

fn duration_frames(duration: Duration, sample_rate_hz: u32) -> usize {
    let frames = duration
        .as_nanos()
        .saturating_mul(u128::from(sample_rate_hz))
        / 1_000_000_000;
    usize::try_from(frames).unwrap_or(usize::MAX)
}

fn ring_index(absolute: u64, capacity_frames: usize) -> usize {
    let capacity = u64::try_from(capacity_frames).unwrap_or(u64::MAX);
    usize::try_from(absolute % capacity).unwrap_or(0)
}

fn write_silence(
    state: &mut RingState,
    capacity_frames: usize,
    channels: usize,
    frames: u64,
) -> AudioResult<()> {
    let capacity = u64::try_from(capacity_frames).unwrap_or(u64::MAX);
    if frames >= capacity {
        state.samples.fill(0.0);
        state.device_frame_mask.fill(0);
        state.total_frames = state.total_frames.checked_add(frames).ok_or_else(|| {
            timeline_invalid("ring timeline overflowed u64; repair: restart the audio stream")
        })?;
        return Ok(());
    }
    for _ in 0..frames {
        let frame_index = ring_index(state.total_frames, capacity_frames);
        let sample_index = frame_index * channels;
        state.samples[sample_index..sample_index + channels].fill(0.0);
        state.device_frame_mask[frame_index] = 0;
        state.total_frames = state.total_frames.checked_add(1).ok_or_else(|| {
            timeline_invalid("ring timeline overflowed u64; repair: restart the audio stream")
        })?;
    }
    Ok(())
}

fn write_device_frames(
    state: &mut RingState,
    capacity_frames: usize,
    channels: usize,
    samples: &[f32],
) -> AudioResult<()> {
    for frame in samples.chunks_exact(channels) {
        let frame_index = ring_index(state.total_frames, capacity_frames);
        let sample_index = frame_index * channels;
        state.samples[sample_index..sample_index + channels].copy_from_slice(frame);
        state.device_frame_mask[frame_index] = 1;
        state.total_frames = state.total_frames.checked_add(1).ok_or_else(|| {
            timeline_invalid("ring timeline overflowed u64; repair: restart the audio stream")
        })?;
    }
    Ok(())
}

fn validate_packet_timeline(state: &RingState, timeline: AudioPacketTimeline) -> AudioResult<()> {
    let (Some(previous_position), Some(previous_qpc)) =
        (state.last_packet_position, state.last_packet_qpc_100ns)
    else {
        return Ok(());
    };
    let _device_delta = timeline
        .device_position
        .checked_sub(previous_position)
        .ok_or_else(|| {
            timeline_invalid(format!(
                "device position regressed from {previous_position} to {}; repair: inspect endpoint reset/device change and restart audio capture",
                timeline.device_position
            ))
        })?;
    let _qpc_delta = timeline
        .qpc_position_100ns
        .checked_sub(previous_qpc)
        .ok_or_else(|| {
            timeline_invalid(format!(
                "QPC timestamp regressed from {previous_qpc} to {}; repair: inspect the endpoint/driver clock and restart audio capture",
                timeline.qpc_position_100ns
            ))
        })?;
    Ok(())
}

fn timeline_invalid(detail: impl Into<String>) -> AudioError {
    AudioError::TimelineInvalid {
        detail: detail.into(),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn requested_frames(seconds: f32, sample_rate_hz: u32) -> usize {
    (f64::from(seconds) * f64::from(sample_rate_hz)).round() as usize
}
