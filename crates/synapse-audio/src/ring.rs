use std::sync::{Mutex, MutexGuard};

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
    pub samples: Vec<f32>,
    pub rms_db: f32,
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
    total_frames: u64,
}

impl AudioRing {
    #[must_use]
    pub fn new(max_seconds: u32) -> Self {
        let format = AudioFormat::default();
        Self {
            inner: Mutex::new(RingState {
                format,
                samples: vec![0.0; capacity_samples(max_seconds, format)],
                total_frames: 0,
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
        available_frames(&state, self.capacity_frames(&state))
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
            state.total_frames = 0;
        }
    }

    pub fn push_interleaved(&self, samples: &[f32]) {
        let mut state = self.lock();
        let channels = usize::from(state.format.channels);
        let capacity_frames = self.capacity_frames(&state);
        if channels == 0 || capacity_frames == 0 {
            return;
        }
        for frame in samples.chunks_exact(channels) {
            let write = (usize::try_from(state.total_frames).unwrap_or(usize::MAX)
                % capacity_frames)
                * channels;
            state.samples[write..write + channels].copy_from_slice(frame);
            state.total_frames = state.total_frames.saturating_add(1);
        }
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
        let available = available_frames(&state, capacity_frames);
        let frames = requested.min(available);
        let mut samples = Vec::with_capacity(frames.saturating_mul(channels));
        let start = state.total_frames.saturating_sub(frames as u64);
        for frame_offset in 0..frames {
            let absolute = start.saturating_add(frame_offset as u64);
            let index =
                (usize::try_from(absolute).unwrap_or(usize::MAX) % capacity_frames) * channels;
            samples.extend_from_slice(&state.samples[index..index + channels]);
        }
        Ok(AudioWindow {
            format: state.format,
            frames,
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

fn available_frames(state: &RingState, capacity_frames: usize) -> usize {
    usize::try_from(state.total_frames)
        .unwrap_or(usize::MAX)
        .min(capacity_frames)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn requested_frames(seconds: f32, sample_rate_hz: u32) -> usize {
    (f64::from(seconds) * f64::from(sample_rate_hz)).round() as usize
}
