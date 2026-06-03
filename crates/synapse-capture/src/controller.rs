use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use crossbeam::channel::{self, Receiver, Sender, TrySendError};

use crate::{
    CAPTURE_CHANNEL_CAPACITY, CaptureBackend, CaptureBackendPreference, CaptureConfig,
    CaptureError, CaptureStats, CaptureTarget, CapturedFrame, FRAMES_DROPPED_METRIC,
    ResolvedCaptureTarget,
    backend::{backend_after_fallback, should_fallback_to_dxgi},
    dpi::current_thread_priority,
    platform,
};

#[derive(Debug)]
pub struct CaptureHandle {
    pub(crate) rx: Receiver<CapturedFrame>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) stats: Arc<CaptureStats>,
    join: Option<JoinHandle<Result<(), CaptureError>>>,
    target: ResolvedCaptureTarget,
    config: CaptureConfig,
}

impl CaptureHandle {
    #[must_use]
    pub fn receiver(&self) -> Receiver<CapturedFrame> {
        self.rx.clone()
    }

    #[must_use]
    pub fn stats(&self) -> Arc<CaptureStats> {
        self.stats.clone()
    }

    #[must_use]
    pub fn channel_len(&self) -> usize {
        self.rx.len()
    }

    #[must_use]
    pub const fn channel_capacity(&self) -> usize {
        CAPTURE_CHANNEL_CAPACITY
    }

    #[must_use]
    pub const fn target(&self) -> &ResolvedCaptureTarget {
        &self.target
    }

    #[must_use]
    pub const fn config(&self) -> &CaptureConfig {
        &self.config
    }

    #[must_use]
    pub fn is_stop_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Requests shutdown and joins the capture thread.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if the capture thread panicked or the backend
    /// reports a terminal capture error during shutdown.
    pub fn stop(mut self) -> Result<(), CaptureError> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            join.join().map_err(|_err| CaptureError::ThreadFailed {
                detail: "capture thread panicked".to_owned(),
            })??;
        }
        Ok(())
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
pub struct CaptureController {
    active: Option<CaptureHandle>,
    generation: u64,
}

impl CaptureController {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active: None,
            generation: 0,
        }
    }

    /// Starts a new capture target, then stops the previous session.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if the new target cannot be resolved/opened or
    /// the old session cannot be stopped.
    pub fn switch_to(&mut self, config: CaptureConfig) -> Result<u64, CaptureError> {
        let handle = spawn_capture_loop(config)?;
        if let Some(previous) = self.active.take()
            && let Err(error) = previous.stop()
        {
            let _ = handle.stop();
            return Err(error);
        }

        self.generation = self.generation.saturating_add(1);
        self.active = Some(handle);
        Ok(self.generation)
    }

    #[must_use]
    pub const fn active(&self) -> Option<&CaptureHandle> {
        self.active.as_ref()
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

pub fn register_capture_metrics() {
    synapse_telemetry::metrics::describe_counter!(
        FRAMES_DROPPED_METRIC,
        "Frames dropped by the bounded capture channel"
    );
}

/// Resolves and validates a capture target without starting capture.
///
/// # Errors
///
/// Returns [`CaptureError`] when the target is invalid for the current platform.
pub fn resolve_capture_target(
    config: &CaptureConfig,
) -> Result<ResolvedCaptureTarget, CaptureError> {
    let backend = config.selected_backend();
    if matches!(backend, CaptureBackend::DxgiDuplication)
        && matches!(config.target, CaptureTarget::Window { .. })
    {
        return Err(CaptureError::TargetInvalid {
            detail: "DXGI duplication supports monitor targets only".to_owned(),
        });
    }
    validate_target(&config.target)?;
    Ok(ResolvedCaptureTarget {
        target: config.target.clone(),
        backend,
    })
}

/// Starts a capture loop using the configured target and backend.
///
/// # Errors
///
/// Returns [`CaptureError`] when the target is invalid or the capture thread
/// cannot be spawned.
pub fn spawn_capture_loop(config: CaptureConfig) -> Result<CaptureHandle, CaptureError> {
    register_capture_metrics();
    let target = resolve_capture_target(&config)?;
    let (tx, rx) = channel::bounded(CAPTURE_CHANNEL_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(CaptureStats::default());
    let ctx = CaptureThreadContext {
        tx,
        rx: rx.clone(),
        stop: stop.clone(),
        stats: stats.clone(),
    };
    let thread_config = config.clone();
    let join = thread::Builder::new()
        .name("synapse-capture".to_owned())
        .spawn(move || run_capture_thread(thread_config, ctx))
        .map_err(|err| CaptureError::ThreadFailed {
            detail: err.to_string(),
        })?;

    Ok(CaptureHandle {
        rx,
        stop,
        stats,
        join: Some(join),
        target,
        config,
    })
}
#[derive(Clone)]
// `tx`/`rx`/`stop` are consumed by the real Windows capture loop; off Windows the
// capture entry points fail loudly without ever reading them.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct CaptureThreadContext {
    pub tx: Sender<CapturedFrame>,
    pub rx: Receiver<CapturedFrame>,
    pub stop: Arc<AtomicBool>,
    pub stats: Arc<CaptureStats>,
}

fn run_capture_thread(
    config: CaptureConfig,
    ctx: CaptureThreadContext,
) -> Result<(), CaptureError> {
    platform::set_capture_thread_priority()?;
    ctx.stats.set_thread_priority(current_thread_priority());
    match config.backend_preference {
        CaptureBackendPreference::Auto => {
            ctx.stats
                .set_effective_backend(CaptureBackend::GraphicsCaptureApi);
            match platform::run_graphics_capture(config.clone(), ctx.clone()) {
                Ok(()) => Ok(()),
                Err(err) if should_fallback_to_dxgi(config.backend_preference, &err) => {
                    ctx.stats
                        .set_effective_backend(CaptureBackend::DxgiDuplication);
                    tracing::warn!(
                        code = "CAPTURE_GRAPHICS_API_UNSUPPORTED",
                        fallback_backend = ?backend_after_fallback(config.backend_preference, &err),
                        error = %err,
                        "graphics capture unsupported; falling back to dxgi duplication"
                    );
                    platform::run_dxgi_capture(config, ctx)
                }
                Err(err) => Err(err),
            }
        }
        CaptureBackendPreference::GraphicsCaptureApi => {
            ctx.stats
                .set_effective_backend(CaptureBackend::GraphicsCaptureApi);
            platform::run_graphics_capture(config, ctx)
        }
        CaptureBackendPreference::DxgiDuplication => {
            ctx.stats
                .set_effective_backend(CaptureBackend::DxgiDuplication);
            platform::run_dxgi_capture(config, ctx)
        }
    }
}
// Only the real Windows capture loop pushes frames; off Windows capture fails
// loudly before any frame is produced.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn push_frame(ctx: &CaptureThreadContext, frame: CapturedFrame) -> Result<(), CaptureError> {
    ctx.stats
        .record_captured_frame(frame.frame_seq, frame.width, frame.height);
    match ctx.tx.try_send(frame) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(frame)) => {
            let _ = ctx.rx.try_recv();
            ctx.stats.increment_dropped();
            ctx.tx
                .try_send(frame)
                .map_err(|err| CaptureError::ThreadFailed {
                    detail: err.to_string(),
                })
        }
        Err(TrySendError::Disconnected(_frame)) => Err(CaptureError::ThreadFailed {
            detail: "capture receiver disconnected".to_owned(),
        }),
    }
}

fn validate_target(target: &CaptureTarget) -> Result<(), CaptureError> {
    match target {
        CaptureTarget::Primary => Ok(()),
        CaptureTarget::Monitor { monitor_index } => validate_monitor(*monitor_index),
        CaptureTarget::Window { hwnd } => validate_hwnd(*hwnd),
    }
}

pub fn validate_hwnd(hwnd: i64) -> Result<(), CaptureError> {
    if hwnd <= 0 {
        return Err(CaptureError::TargetInvalid {
            detail: format!("invalid HWND {hwnd}"),
        });
    }

    platform::validate_hwnd_impl(hwnd)
}

pub fn validate_monitor(monitor_index: u32) -> Result<(), CaptureError> {
    platform::validate_monitor_impl(monitor_index)
}
