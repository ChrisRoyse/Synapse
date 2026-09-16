#[cfg(windows)]
use std::{fmt, sync::mpsc, thread, time::Duration};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
use crate::ring::{AudioFormat, DEFAULT_SAMPLE_RATE_HZ, STEREO_CHANNELS};
use crate::{
    AudioError, AudioResult, MAX_RING_SECONDS, detectors::DetectorProcessor, ring::AudioRing,
};

pub const AUDIO_LOOPBACK_FRAMES_TOTAL: &str = "audio_loopback_frames_total";
const AUDIO_LOOPBACK_UNDERRUNS_TOTAL: &str = "audio_loopback_underruns_total";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopbackStatus {
    pub running: bool,
    pub frames_captured: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
}

#[derive(Debug)]
pub struct LoopbackHandle {
    stop: Arc<AtomicBool>,
    stats: Arc<LoopbackStats>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct LoopbackStats {
    running: AtomicBool,
    frames_captured: AtomicU64,
    last_error_code: Mutex<Option<String>>,
}

impl LoopbackStats {
    #[cfg(windows)]
    const fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            frames_captured: AtomicU64::new(0),
            last_error_code: Mutex::new(None),
        }
    }

    #[cfg(windows)]
    fn set_error(&self, error: &AudioError) {
        let mut last = match self.last_error_code.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *last = Some(error.code().to_owned());
    }
}

impl LoopbackHandle {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.stats.running.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn status(&self) -> LoopbackStatus {
        let last_error_code = match self.stats.last_error_code.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        LoopbackStatus {
            running: self.is_running(),
            frames_captured: self.stats.frames_captured.load(Ordering::Acquire),
            last_error_code,
        }
    }
}

impl Drop for LoopbackHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _join_result = thread.join();
        }
    }
}

/// Starts WASAPI loopback capture on Windows.
///
/// # Errors
///
/// Returns [`AudioError::LoopbackInitFailed`] when the capture thread cannot
/// initialize the default render endpoint.
#[tracing::instrument(skip_all)]
pub fn start_loopback(
    ring: Arc<AudioRing>,
    detectors: Option<DetectorProcessor>,
) -> AudioResult<LoopbackHandle> {
    if ring.max_seconds() == 0 || ring.max_seconds() > MAX_RING_SECONDS {
        return Err(AudioError::LoopbackInitFailed {
            detail: format!("invalid ring duration {}", ring.max_seconds()),
        });
    }
    start_platform_loopback(ring, detectors)
}

#[cfg(not(windows))]
fn start_platform_loopback(
    _ring: Arc<AudioRing>,
    _detectors: Option<DetectorProcessor>,
) -> AudioResult<LoopbackHandle> {
    Err(AudioError::LoopbackInitFailed {
        detail: "WASAPI loopback is only available on Windows".to_owned(),
    })
}

#[cfg(windows)]
fn start_platform_loopback(
    ring: Arc<AudioRing>,
    detectors: Option<DetectorProcessor>,
) -> AudioResult<LoopbackHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(LoopbackStats::new());
    let (tx, rx) = mpsc::sync_channel(1);
    let thread_stop = Arc::clone(&stop);
    let thread_stats = Arc::clone(&stats);
    let thread = thread::Builder::new()
        .name("synapse-audio-loopback".to_owned())
        .spawn(move || run_capture_thread(ring, detectors, thread_stop, thread_stats, tx))
        .map_err(|err| AudioError::LoopbackInitFailed {
            detail: format!("failed to spawn audio loopback thread: {err}"),
        })?;

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(LoopbackHandle {
            stop,
            stats,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _join_result = thread.join();
            Err(error)
        }
        Err(err) => {
            stop.store(true, Ordering::Release);
            let _join_result = thread.join();
            Err(AudioError::LoopbackInitFailed {
                detail: format!("audio loopback startup did not report readiness: {err}"),
            })
        }
    }
}

#[cfg(windows)]
#[allow(clippy::needless_pass_by_value)]
fn run_capture_thread(
    ring: Arc<AudioRing>,
    detectors: Option<DetectorProcessor>,
    stop: Arc<AtomicBool>,
    stats: Arc<LoopbackStats>,
    startup: mpsc::SyncSender<AudioResult<()>>,
) {
    let result = capture_loop(ring, detectors, stop, Arc::clone(&stats), startup);
    stats.running.store(false, Ordering::Release);
    if let Err(error) = result {
        stats.set_error(&error);
        tracing::warn!(code = error.code(), error = %error, "audio loopback stopped");
    }
}

#[cfg(windows)]
#[allow(clippy::needless_pass_by_value)]
fn capture_loop(
    ring: Arc<AudioRing>,
    mut detectors: Option<DetectorProcessor>,
    stop: Arc<AtomicBool>,
    stats: Arc<LoopbackStats>,
    startup: mpsc::SyncSender<AudioResult<()>>,
) -> AudioResult<()> {
    let _com = ComMtaGuard::init()?;
    let _mmcss = MmcssGuard::start()?;
    let capture = match WasapiLoopback::start() {
        Ok(capture) => capture,
        Err(error) => {
            let _send_result = startup.send(Err(error.clone()));
            return Err(error);
        }
    };
    ring.set_format(capture.format);
    stats.running.store(true, Ordering::Release);
    if startup.send(Ok(())).is_err() {
        return Ok(());
    }

    while !stop.load(Ordering::Acquire) {
        if let Err(error) = capture.wait() {
            if error == "timeout" {
                continue;
            }
            return Err(AudioError::DeviceLost { detail: error });
        }
        while let Some(samples) = capture.read_packet()? {
            let frames = samples.len() / usize::from(capture.format.channels);
            ring.push_interleaved(&samples);
            if let Some(processor) = detectors.as_mut() {
                processor.process(&samples, capture.format);
            }
            let frames = u64::try_from(frames).unwrap_or(u64::MAX);
            stats.frames_captured.fetch_add(frames, Ordering::AcqRel);
            metrics::counter!(AUDIO_LOOPBACK_FRAMES_TOTAL).increment(frames);
        }
    }
    capture.stop()
}

#[cfg(windows)]
struct WasapiLoopback {
    audio_client: wasapi::AudioClient,
    event: wasapi::Handle,
    capture_client: wasapi::AudioCaptureClient,
    format: AudioFormat,
    block_align: usize,
}

#[cfg(windows)]
impl WasapiLoopback {
    fn start() -> AudioResult<Self> {
        let enumerator = wasapi::DeviceEnumerator::new().map_err(loopback_init)?;
        let device = enumerator
            .get_default_device(&wasapi::Direction::Render)
            .map_err(loopback_init)?;
        let mut audio_client = device.get_iaudioclient().map_err(loopback_init)?;
        let desired = wasapi::WaveFormat::new(
            32,
            32,
            &wasapi::SampleType::Float,
            DEFAULT_SAMPLE_RATE_HZ as usize,
            usize::from(STEREO_CHANNELS),
            None,
        );
        let (_default_period, min_period) =
            audio_client.get_device_period().map_err(loopback_init)?;
        let mode = wasapi::StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: min_period,
        };
        audio_client
            .initialize_client(&desired, &wasapi::Direction::Capture, &mode)
            .map_err(loopback_init)?;
        let event = audio_client.set_get_eventhandle().map_err(loopback_init)?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .map_err(loopback_init)?;
        audio_client.start_stream().map_err(loopback_init)?;
        Ok(Self {
            audio_client,
            event,
            capture_client,
            format: AudioFormat {
                sample_rate_hz: desired.get_samplespersec(),
                channels: desired.get_nchannels(),
            },
            block_align: desired.get_blockalign() as usize,
        })
    }

    fn wait(&self) -> Result<(), String> {
        match self.event.wait_for_event(200) {
            Ok(()) => Ok(()),
            Err(wasapi::WasapiError::EventTimeout) => Err("timeout".to_owned()),
            Err(err) => Err(err.to_string()),
        }
    }

    fn read_packet(&self) -> AudioResult<Option<Vec<f32>>> {
        let frames = self
            .capture_client
            .get_next_packet_size()
            .map_err(device_lost)?
            .unwrap_or(0);
        if frames == 0 {
            metrics::counter!(AUDIO_LOOPBACK_UNDERRUNS_TOTAL).increment(1);
            return Ok(None);
        }
        let bytes = usize::try_from(frames)
            .unwrap_or(usize::MAX)
            .checked_mul(self.block_align)
            .ok_or_else(|| AudioError::DeviceLost {
                detail: "audio packet byte count overflowed".to_owned(),
            })?;
        let mut raw = vec![0_u8; bytes];
        let (read_frames, info) = self
            .capture_client
            .read_from_device(&mut raw)
            .map_err(device_lost)?;
        if read_frames == 0 {
            metrics::counter!(AUDIO_LOOPBACK_UNDERRUNS_TOTAL).increment(1);
            return Ok(None);
        }
        Ok(Some(raw_f32_stereo(&raw, read_frames, &info)))
    }

    fn stop(&self) -> AudioResult<()> {
        self.audio_client.stop_stream().map_err(device_lost)
    }
}

#[cfg(windows)]
fn raw_f32_stereo(raw: &[u8], frames: u32, info: &wasapi::BufferInfo) -> Vec<f32> {
    let samples = usize::try_from(frames)
        .unwrap_or(usize::MAX)
        .saturating_mul(usize::from(STEREO_CHANNELS));
    if info.flags.silent {
        return vec![0.0; samples];
    }
    let mut out = Vec::with_capacity(samples);
    for bytes in raw.chunks_exact(4).take(samples) {
        out.push(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).clamp(-1.0, 1.0));
    }
    out
}

#[cfg(windows)]
fn loopback_init(error: impl fmt::Display) -> AudioError {
    AudioError::LoopbackInitFailed {
        detail: error.to_string(),
    }
}

#[cfg(windows)]
fn device_lost(error: impl fmt::Display) -> AudioError {
    AudioError::DeviceLost {
        detail: error.to_string(),
    }
}

#[cfg(windows)]
struct ComMtaGuard;

#[cfg(windows)]
impl ComMtaGuard {
    fn init() -> AudioResult<Self> {
        wasapi::initialize_mta().ok().map_err(loopback_init)?;
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for ComMtaGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[cfg(windows)]
struct MmcssGuard(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl MmcssGuard {
    fn start() -> AudioResult<Self> {
        use windows::{
            Win32::System::Threading::{
                AVRT_PRIORITY_CRITICAL, AvRevertMmThreadCharacteristics,
                AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
            },
            core::w,
        };

        let mut task_index = 0_u32;
        // SAFETY: The task name is a static null-terminated UTF-16 literal and
        // task_index is initialized as required for the first MMCSS call.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &raw mut task_index) }
            .map_err(loopback_init)?;
        // SAFETY: handle is the live task handle returned above.
        if let Err(error) = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL) } {
            // SAFETY: handle is owned by this function on the failure path.
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
            return Err(loopback_init(error));
        }
        Ok(Self(handle))
    }
}

#[cfg(windows)]
impl Drop for MmcssGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 was returned by AvSetMmThreadCharacteristicsW and is
        // reverted exactly once by this guard.
        let _ =
            unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0) };
    }
}
