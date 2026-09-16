pub mod detectors;
pub mod direction;
pub mod error;
pub mod loopback;
pub mod ring;
pub mod stt;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use synapse_core::Event;

pub use error::{AudioError, AudioResult};
pub use loopback::LoopbackStatus;
pub use ring::{AudioFormat, AudioRing, AudioWindow};
pub use stt::{Transcription, TranscriptionConfidenceSource, WhisperTinyStt};
pub use synapse_core::DirectionEstimate;

pub const DEFAULT_RING_SECONDS: u32 = 30;
pub const MAX_RING_SECONDS: u32 = 30;

pub type AudioEventSink = Arc<dyn Fn(Event) + Send + Sync + 'static>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    #[serde(default = "default_ring_seconds")]
    pub ring_seconds: u32,
    #[serde(default)]
    pub start_loopback: bool,
    #[serde(default)]
    pub detectors_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model_path: Option<PathBuf>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            ring_seconds: DEFAULT_RING_SECONDS,
            start_loopback: false,
            detectors_enabled: false,
            stt_model_path: None,
        }
    }
}

#[derive(Debug)]
pub struct AudioRuntime {
    config: AudioConfig,
    ring: Arc<AudioRing>,
    detector_state: detectors::SharedDetectorState,
    loopback: Option<loopback::LoopbackHandle>,
    stt: Mutex<WhisperTinyStt>,
}

impl AudioRuntime {
    /// Spawns the M3 audio runtime.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::LoopbackInitFailed`] when the ring buffer duration
    /// is outside the scaffold's supported range or when the caller requests
    /// loopback/detector startup and WASAPI initialization fails.
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn spawn(config: AudioConfig) -> AudioResult<Self> {
        Self::spawn_with_event_sink(config, Arc::new(|_event| {}))
    }

    /// Spawns the runtime and sends detector events to `event_sink`.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::LoopbackInitFailed`] when the ring buffer duration
    /// is invalid or the configured loopback capture cannot initialize.
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn spawn_with_event_sink(
        config: AudioConfig,
        event_sink: AudioEventSink,
    ) -> AudioResult<Self> {
        validate_config(&config)?;
        let ring = Arc::new(AudioRing::new(config.ring_seconds));
        let detector_state = detectors::SharedDetectorState::default();
        let loopback = if config.start_loopback {
            Some(loopback::start_loopback(
                Arc::clone(&ring),
                config
                    .detectors_enabled
                    .then(|| detectors::DetectorProcessor::new(detector_state.clone(), event_sink)),
            )?)
        } else {
            None
        };
        let stt = Mutex::new(WhisperTinyStt::new(config.stt_model_path.clone()));
        Ok(Self {
            config,
            ring,
            detector_state,
            loopback,
            stt,
        })
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn loopback_started(&self) -> bool {
        self.loopback
            .as_ref()
            .is_some_and(loopback::LoopbackHandle::is_running)
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn detectors_started(&self) -> bool {
        self.config.detectors_enabled && self.loopback_started()
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn ring(&self) -> Arc<AudioRing> {
        Arc::clone(&self.ring)
    }

    /// Returns the most recent audio samples from the runtime ring.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::LoopbackInitFailed`] when `seconds` is greater
    /// than the configured ring duration.
    #[tracing::instrument(skip_all, fields(component = "audio_runtime", seconds))]
    pub fn tail_seconds(&self, seconds: f32) -> AudioResult<AudioWindow> {
        self.ring.tail_seconds(seconds)
    }

    /// Estimates direction from the most recent stereo audio samples.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::LoopbackInitFailed`] when `seconds` is outside
    /// the configured ring duration.
    #[tracing::instrument(skip_all, fields(component = "audio_runtime", seconds))]
    pub fn estimate_direction_tail(&self, seconds: f32) -> AudioResult<DirectionEstimate> {
        let window = self.tail_seconds(seconds)?;
        Ok(direction::estimate_direction(&window))
    }

    /// Transcribes the most recent audio samples in the ring.
    ///
    /// # Errors
    ///
    /// Returns `AUDIO_STT_MODEL_NOT_LOADED` when the pinned Whisper model is
    /// absent, `MODEL_HASH_MISMATCH` when the local model bytes do not match
    /// the pinned digest, or a structured model load/inference error when ORT
    /// rejects the model.
    #[tracing::instrument(skip_all, fields(component = "audio_runtime", seconds))]
    pub fn transcribe_tail(
        &self,
        seconds: f32,
        language: impl AsRef<str>,
    ) -> AudioResult<Transcription> {
        let window = self.tail_seconds(seconds)?;
        self.stt
            .lock()
            .map_err(|_| AudioError::ModelLoadFailed {
                path: self
                    .config
                    .stt_model_path
                    .clone()
                    .unwrap_or_else(stt::default_model_path),
                detail: "STT runtime lock was poisoned".to_owned(),
            })?
            .transcribe_window(&window, language)
    }

    /// Transcribes an audio fixture or captured clip file directly.
    ///
    /// # Errors
    ///
    /// Returns the same structured errors as [`Self::transcribe_tail`], plus a
    /// read error if the supplied audio path cannot be opened.
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn transcribe_file(
        &self,
        path: impl AsRef<std::path::Path>,
        language: impl AsRef<str>,
    ) -> AudioResult<Transcription> {
        self.stt
            .lock()
            .map_err(|_| AudioError::ModelLoadFailed {
                path: self
                    .config
                    .stt_model_path
                    .clone()
                    .unwrap_or_else(stt::default_model_path),
                detail: "STT runtime lock was poisoned".to_owned(),
            })?
            .transcribe_file(path, language)
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn detector_snapshot(&self) -> detectors::DetectorSnapshot {
        self.detector_state.snapshot()
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn loopback_status(&self) -> LoopbackStatus {
        self.loopback.as_ref().map_or_else(
            || LoopbackStatus {
                running: false,
                frames_captured: 0,
                last_error_code: None,
            },
            loopback::LoopbackHandle::status,
        )
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "audio_runtime"))]
    pub fn stt_model_loaded(&self) -> bool {
        self.stt.lock().is_ok_and(|stt| stt.is_loaded())
    }
}

fn validate_config(config: &AudioConfig) -> AudioResult<()> {
    if config.ring_seconds == 0 || config.ring_seconds > MAX_RING_SECONDS {
        return Err(AudioError::LoopbackInitFailed {
            detail: format!(
                "audio ring_seconds must be between 1 and {MAX_RING_SECONDS}, got {}",
                config.ring_seconds
            ),
        });
    }
    if config.detectors_enabled && !config.start_loopback {
        return Err(AudioError::LoopbackInitFailed {
            detail: "audio detectors require loopback startup".to_owned(),
        });
    }
    Ok(())
}

const fn default_ring_seconds() -> u32 {
    DEFAULT_RING_SECONDS
}
