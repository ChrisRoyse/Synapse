use std::{thread, time::Duration};

use crate::{
    CaptureConfig, CaptureError, CapturedFrame, CapturedFrameBuffer, DxgiFormat,
    controller::{CaptureThreadContext, push_frame},
};

use super::{
    bitmap::screen_region_to_bgra_bitmap_with_cursor, target::capture_target_screen_region,
};

pub fn capture_gdi_frame(
    config: &CaptureConfig,
    frame_seq: u64,
) -> Result<CapturedFrame, CaptureError> {
    let region = capture_target_screen_region(&config.target)?;
    let bitmap = screen_region_to_bgra_bitmap_with_cursor(region, config.cursor_visible)?;
    let row_stride_bytes = usize::try_from(bitmap.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!(
                "CPU/GDI capture row stride overflow for {}x{} target {region:?}",
                bitmap.width, bitmap.height
            ),
        })?;
    let expected_len = row_stride_bytes
        .checked_mul(usize::try_from(bitmap.height).unwrap_or(usize::MAX))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!(
                "CPU/GDI capture byte length overflow for {}x{} target {region:?}",
                bitmap.width, bitmap.height
            ),
        })?;
    if bitmap.bytes.len() != expected_len {
        return Err(CaptureError::ThreadFailed {
            detail: format!(
                "CPU/GDI capture byte readback mismatch for target {region:?}: expected {expected_len}, got {}",
                bitmap.bytes.len()
            ),
        });
    }
    Ok(CapturedFrame {
        pixels: CapturedFrameBuffer {
            bytes: bitmap.bytes,
            row_stride_bytes,
            bytes_per_pixel: 4,
        },
        width: bitmap.width,
        height: bitmap.height,
        format: DxgiFormat::Bgra8,
        captured_at: std::time::Instant::now(),
        frame_seq,
        dirty_region: None,
    })
}

/// Runs the no-explicit-GPU-API Windows capture loop using only a screen DC, a
/// compatible memory DC, and a host-readable DIB section. Windows or the display
/// driver may still accelerate GDI internally, so this does not attest zero VRAM.
///
/// The target region is resolved again before every frame. That matters for a
/// window target: a window that becomes minimized, hidden, cloaked, invalid, or
/// partially off the physical virtual desktop must terminate capture with a
/// typed error instead of returning stale/black pixels.
#[allow(clippy::needless_pass_by_value)]
pub fn run_gdi_capture(
    config: CaptureConfig,
    ctx: CaptureThreadContext,
) -> Result<(), CaptureError> {
    let interval = Duration::from_millis(config.min_update_interval_ms.max(1));
    let mut frame_seq = 1_u64;
    let mut next_capture_at = std::time::Instant::now() + interval;

    while !ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
        // The channel is the demand signal. Synapse currently retains one
        // preflight/readback frame; while that frame has not been consumed,
        // another full-screen copy would only be dropped. Waiting here makes
        // an armed target effectively idle instead of polling the desktop for
        // no consumer.
        if ctx.tx.is_full() || std::time::Instant::now() < next_capture_at {
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        push_frame(&ctx, capture_gdi_frame(&config, frame_seq)?)?;
        frame_seq = frame_seq.saturating_add(1);
        next_capture_at = std::time::Instant::now() + interval;
    }

    Ok(())
}
