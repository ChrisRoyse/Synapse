use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::{CaptureBackend, FRAMES_DROPPED_METRIC};

const THREAD_PRIORITY_UNKNOWN: i32 = i32::MIN;
const THREAD_PRIORITY_UNSUPPORTED: i32 = i32::MIN + 1;
const THREAD_PRIORITY_TIME_CRITICAL: i32 = i32::MAX;
const BACKEND_UNKNOWN: i32 = 0;
const BACKEND_GRAPHICS_CAPTURE_API: i32 = 1;
const BACKEND_DXGI_DUPLICATION: i32 = 2;

#[derive(Debug)]
pub struct CaptureStats {
    frames_captured: AtomicU64,
    frames_dropped: AtomicU64,
    latest_frame_seq: AtomicU64,
    latest_frame_width: AtomicU32,
    latest_frame_height: AtomicU32,
    thread_priority: AtomicI32,
    effective_backend: AtomicI32,
}

impl Default for CaptureStats {
    fn default() -> Self {
        Self {
            frames_captured: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            latest_frame_seq: AtomicU64::new(0),
            latest_frame_width: AtomicU32::new(0),
            latest_frame_height: AtomicU32::new(0),
            thread_priority: AtomicI32::new(THREAD_PRIORITY_UNKNOWN),
            effective_backend: AtomicI32::new(BACKEND_UNKNOWN),
        }
    }
}

impl CaptureStats {
    #[must_use]
    pub fn frames_captured(&self) -> u64 {
        self.frames_captured.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn thread_priority(&self) -> CaptureThreadPriority {
        decode_thread_priority(self.thread_priority.load(Ordering::Relaxed))
    }

    #[must_use]
    pub fn effective_backend(&self) -> Option<CaptureBackend> {
        decode_backend(self.effective_backend.load(Ordering::Relaxed))
    }

    #[must_use]
    pub fn latest_frame(&self) -> Option<CaptureFrameStats> {
        let width = self.latest_frame_width.load(Ordering::Relaxed);
        let height = self.latest_frame_height.load(Ordering::Relaxed);
        if width == 0 || height == 0 {
            return None;
        }
        Some(CaptureFrameStats {
            frame_seq: self.latest_frame_seq.load(Ordering::Relaxed),
            width,
            height,
        })
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn record_captured_frame(&self, frame_seq: u64, width: u32, height: u32) {
        self.latest_frame_width.store(width, Ordering::Relaxed);
        self.latest_frame_height.store(height, Ordering::Relaxed);
        self.latest_frame_seq.store(frame_seq, Ordering::Relaxed);
        self.frames_captured.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn increment_dropped(&self) {
        self.frames_dropped.fetch_add(1, Ordering::Relaxed);
        synapse_telemetry::metrics::counter!(FRAMES_DROPPED_METRIC).increment(1);
    }

    pub(crate) fn set_thread_priority(&self, priority: CaptureThreadPriority) {
        self.thread_priority
            .store(encode_thread_priority(priority), Ordering::Relaxed);
    }

    pub(crate) fn set_effective_backend(&self, backend: CaptureBackend) {
        self.effective_backend
            .store(encode_backend(backend), Ordering::Relaxed);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CaptureFrameStats {
    pub frame_seq: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CaptureThreadPriority {
    TimeCritical,
    Other(i32),
    Unsupported,
    Unknown,
}

const fn encode_thread_priority(priority: CaptureThreadPriority) -> i32 {
    match priority {
        CaptureThreadPriority::TimeCritical => THREAD_PRIORITY_TIME_CRITICAL,
        CaptureThreadPriority::Unsupported => THREAD_PRIORITY_UNSUPPORTED,
        CaptureThreadPriority::Unknown => THREAD_PRIORITY_UNKNOWN,
        CaptureThreadPriority::Other(value) => value,
    }
}

const fn decode_thread_priority(value: i32) -> CaptureThreadPriority {
    match value {
        THREAD_PRIORITY_TIME_CRITICAL => CaptureThreadPriority::TimeCritical,
        THREAD_PRIORITY_UNSUPPORTED => CaptureThreadPriority::Unsupported,
        THREAD_PRIORITY_UNKNOWN => CaptureThreadPriority::Unknown,
        other => CaptureThreadPriority::Other(other),
    }
}

const fn encode_backend(backend: CaptureBackend) -> i32 {
    match backend {
        CaptureBackend::GraphicsCaptureApi => BACKEND_GRAPHICS_CAPTURE_API,
        CaptureBackend::DxgiDuplication => BACKEND_DXGI_DUPLICATION,
    }
}

const fn decode_backend(value: i32) -> Option<CaptureBackend> {
    match value {
        BACKEND_GRAPHICS_CAPTURE_API => Some(CaptureBackend::GraphicsCaptureApi),
        BACKEND_DXGI_DUPLICATION => Some(CaptureBackend::DxgiDuplication),
        _ => None,
    }
}
