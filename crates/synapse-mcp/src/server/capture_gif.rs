//! `capture_gif` MCP tool (#1339).
//!
//! Claude-in-Chrome ships `gif_creator` to record interactions as shareable
//! GIFs. Synapse had `demo_record_*` (UIA JSONL for profile authoring) and
//! terminal asciicast, but no VISUAL screen/browser GIF recorder.
//!
//! `capture_gif` records the bound window (or an explicit HWND, or the browser
//! window behind a CDP target) by capturing periodic CPU/GDI crops of that
//! window's physically visible desktop rectangle, then encodes an animated GIF. It is a single
//! synchronous call — no dangling recording state machine — and reports captured
//! vs requested frame counts so there are never silent frame drops. Minimized,
//! hidden, cloaked, and off-screen targets fail closed; visible occluders remain
//! in the frames because the pixels are a physical-desktop readback.

use std::time::{Duration, Instant};

use image::{
    Delay, Frame, RgbaImage,
    codecs::gif::{GifEncoder, Repeat},
};
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use serde_json::json;

use super::{
    CaptureGifParams, CaptureGifResponse, ErrorData, Json, Parameters, SessionTarget,
    SynapseService, tool, tool_router,
};
use crate::m1::{mcp_error, validate_window_hwnd_shape};

const DEFAULT_DURATION_MS: u64 = 3_000;
const MAX_DURATION_MS: u64 = 60_000;
const DEFAULT_INTERVAL_MS: u64 = 500;
const MIN_DURATION_MS: u64 = 100;
const MIN_INTERVAL_MS: u64 = 250;
const DEFAULT_MAX_LONG_EDGE: u32 = 800;
const MAX_LONG_EDGE: u32 = 2_048;
const FRAME_TIMEOUT_MS: u64 = 1_500;

#[tool_router(router = capture_gif_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Record a physically visible window as an animated GIF using no-explicit-GPU-API GDI desktop pixels (Windows/the display driver may still accelerate GDI internally). The target may be the session-bound window, an explicit window_hwnd, or the browser window behind a CDP tab target. Minimized, hidden, cloaked, and partly off-screen targets fail closed; visible occluders remain in the frames. interval_ms must be 250..=60000. Frames are downscaled aspect-preserving to max_long_edge (default 800, range 1..=2048), encoded one at a time into a same-directory staging file, and atomically published only after every requested frame succeeds. Use capture_screenshot for a single still."
    )]
    pub async fn capture_gif(
        &self,
        params: Parameters<CaptureGifParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CaptureGifResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "capture_gif",
            "tool.invocation kind=capture_gif"
        );

        let (duration_ms, interval_ms) = capture_gif_timing(&params)?;
        let max_long_edge = params.max_long_edge.unwrap_or(DEFAULT_MAX_LONG_EDGE);
        if !(1..=MAX_LONG_EDGE).contains(&max_long_edge) {
            return Err(capture_gif_bounds_error(
                "max_long_edge",
                format!("1..={MAX_LONG_EDGE}"),
                u64::from(max_long_edge),
                "pass max_long_edge between 1 and 2048, or omit it for the bounded default",
            ));
        }

        let window_hwnd = self.capture_gif_resolve_window(params.window_hwnd, &request_context)?;

        let output_path = std::path::PathBuf::from(&params.path);
        if !output_path.is_absolute() {
            return Err(mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                format!("capture_gif path must be absolute: {:?}", params.path),
            ));
        }
        if output_path.exists() && !params.overwrite {
            return Err(mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "capture_gif refuses to overwrite existing file without overwrite=true: {:?}",
                    params.path
                ),
            ));
        }

        let frames_requested =
            usize::try_from((duration_ms / interval_ms).max(1)).map_err(|error| {
                mcp_error(
                    synapse_core::error_codes::TOOL_PARAMS_INVALID,
                    format!("capture_gif frame-count conversion failed: {error}"),
                )
            })?;
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                mcp_error(
                    synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    format!("capture_gif could not create output directory: {error}"),
                )
            })?;
        }
        let parent = output_path.parent().ok_or_else(|| {
            mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                format!("capture_gif output path has no parent: {:?}", params.path),
            )
        })?;
        let mut staged = tempfile::Builder::new()
            .prefix(".synapse-capture-gif-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .map_err(|error| {
                mcp_error(
                    synapse_core::error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "capture_gif could not create a same-directory staging file for {:?}: {error}",
                        params.path
                    ),
                )
            })?;
        let delay_ms = u32::try_from(interval_ms).map_err(|error| {
            mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                format!("capture_gif interval_ms conversion failed: {error}"),
            )
        })?;
        let delay = Delay::from_numer_denom_ms(delay_ms, 1);
        let mut encoder = GifEncoder::new_with_speed(staged.as_file_mut(), 10);
        encoder.set_repeat(Repeat::Infinite).map_err(|error| {
            mcp_error(
                synapse_core::error_codes::STORAGE_WRITE_FAILED,
                format!("capture_gif set_repeat failed before publication: {error}"),
            )
        })?;

        let started = Instant::now();
        let mut native_dims: Option<(u32, u32)> = None;
        let mut target_dims: Option<(u32, u32)> = None;
        let mut capture_backend: Option<String> = None;

        for frame_index in 0..frames_requested {
            let frame_started = Instant::now();
            let captured =
                synapse_capture::window_full_frame_to_bgra_bitmap(window_hwnd, FRAME_TIMEOUT_MS)
                    .map_err(|error| {
                        ErrorData::new(
                            ErrorCode(-32099),
                            format!(
                                "capture_gif frame {}/{} failed for hwnd {window_hwnd:#x}: {error}; no output was published",
                                frame_index + 1,
                                frames_requested
                            ),
                            Some(json!({
                                "code": error.code(),
                                "tool": "capture_gif",
                                "operation": "capture_frame",
                                "frame_index": frame_index,
                                "frames_requested": frames_requested,
                                "window_hwnd": window_hwnd,
                                "source_of_truth": "CPU/GDI capture result before GIF staging publication",
                                "staging_publication": "not_published",
                                "remediation": "repair the exact target visibility or capture condition reported by the typed capture error, then retry the complete GIF request",
                            })),
                        )
                    })?;
            capture_backend.get_or_insert_with(|| captured.capture_backend.to_owned());
            let bitmap = captured.bitmap;
            match native_dims {
                None => {
                    native_dims = Some((bitmap.width, bitmap.height));
                    target_dims = Some(capture_gif_target_dims(
                        bitmap.width,
                        bitmap.height,
                        max_long_edge,
                    ));
                }
                Some((width, height)) if (width, height) != (bitmap.width, bitmap.height) => {
                    return Err(mcp_error(
                        synapse_core::error_codes::CAPTURE_TARGET_INVALID,
                        format!(
                            "capture_gif target dimensions changed at frame {}/{} from {width}x{height} to {}x{}; no output was published",
                            frame_index + 1,
                            frames_requested,
                            bitmap.width,
                            bitmap.height
                        ),
                    ));
                }
                Some(_) => {}
            }
            let rgba = bgra_to_rgba(bitmap.bytes, bitmap.width, bitmap.height)?;
            let (tw, th) = target_dims.unwrap_or((bitmap.width, bitmap.height));
            let rgba = if (tw, th) == (bitmap.width, bitmap.height) {
                rgba
            } else {
                image::imageops::resize(&rgba, tw, th, image::imageops::FilterType::Triangle)
            };
            encoder
                .encode_frame(Frame::from_parts(rgba, 0, 0, delay))
                .map_err(|error| {
                    mcp_error(
                        synapse_core::error_codes::STORAGE_WRITE_FAILED,
                        format!(
                            "capture_gif frame {}/{} encode failed before publication: {error}",
                            frame_index + 1,
                            frames_requested
                        ),
                    )
                })?;

            if frame_index + 1 < frames_requested {
                let spent = frame_started.elapsed();
                if let Some(remaining) = Duration::from_millis(interval_ms).checked_sub(spent) {
                    tokio::time::sleep(remaining).await;
                }
            }
        }
        drop(encoder);
        staged.as_file_mut().sync_all().map_err(|error| {
            mcp_error(
                synapse_core::error_codes::STORAGE_WRITE_FAILED,
                format!("capture_gif staging-file sync failed before publication: {error}"),
            )
        })?;
        let (tw, th) = target_dims.unwrap_or((1, 1));
        let (nw, nh) = native_dims.unwrap_or((tw, th));
        let frames_captured = frames_requested;
        let staged_path = staged.into_temp_path();
        let (bytes_written, _sha256) = super::m1_tools::publish_atomic_artifact(
            staged_path.as_ref(),
            &output_path,
            params.overwrite,
            "capture_gif",
        )?;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(duration_ms);
        tracing::info!(
            code = "CAPTURE_GIF_RECORDED",
            hwnd = window_hwnd,
            frames_captured,
            frames_requested,
            width = tw,
            height = th,
            bytes_written,
            elapsed_ms,
            "readback=capture_gif outcome=encoded"
        );

        Ok(Json(CaptureGifResponse {
            path: output_path.to_string_lossy().into_owned(),
            frames_captured,
            frames_requested,
            width: tw,
            height: th,
            native_width: nw,
            native_height: nh,
            interval_ms,
            duration_ms,
            elapsed_ms,
            bytes_written,
            capture_backend: capture_backend
                .unwrap_or_else(|| "gdi_bitblt_visible_window_bgra".to_owned()),
            window_hwnd,
        }))
    }

    fn capture_gif_resolve_window(
        &self,
        explicit: Option<i64>,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<i64, ErrorData> {
        if let Some(hwnd) = explicit {
            return validate_window_hwnd_shape("capture_gif", hwnd);
        }
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
        let target = self.session_target(session_id.as_deref())?;
        let hwnd = match target {
            Some(SessionTarget::Window { hwnd }) => hwnd,
            Some(SessionTarget::Cdp { window_hwnd, .. }) => window_hwnd,
            None => Err(mcp_error(
                synapse_core::error_codes::TARGET_NOT_SET,
                "capture_gif requires a window_hwnd or a bound session target (set_target)",
            ))?,
        };
        validate_window_hwnd_shape("capture_gif", hwnd)
    }
}

fn capture_gif_timing(params: &CaptureGifParams) -> Result<(u64, u64), ErrorData> {
    let duration_ms = params.duration_ms.unwrap_or(DEFAULT_DURATION_MS);
    if !(MIN_DURATION_MS..=MAX_DURATION_MS).contains(&duration_ms) {
        return Err(capture_gif_bounds_error(
            "duration_ms",
            format!("{MIN_DURATION_MS}..={MAX_DURATION_MS}"),
            duration_ms,
            "pass duration_ms between 100 and 60000, or omit it for the default",
        ));
    }

    let interval_ms = params.interval_ms.unwrap_or(DEFAULT_INTERVAL_MS);
    if !(MIN_INTERVAL_MS..=MAX_DURATION_MS).contains(&interval_ms) {
        return Err(capture_gif_bounds_error(
            "interval_ms",
            format!("{MIN_INTERVAL_MS}..={MAX_DURATION_MS}"),
            interval_ms,
            "pass interval_ms between 250 and 60000, or omit it for the default",
        ));
    }

    Ok((duration_ms, interval_ms))
}

fn capture_gif_bounds_error(
    field: &'static str,
    accepted_range: String,
    actual_value: u64,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("capture_gif {field} must be {accepted_range}; got {actual_value}"),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "tool": "capture_gif",
            "operation": "record",
            "field": field,
            "source_id": field,
            "accepted_range": accepted_range,
            "actual_value": actual_value,
            "source_of_truth": "MCP request parameters",
            "remediation": remediation,
        })),
    )
}

fn capture_gif_target_dims(width: u32, height: u32, max_long_edge: u32) -> (u32, u32) {
    if width <= max_long_edge && height <= max_long_edge {
        return (width.max(1), height.max(1));
    }
    let long = width.max(height);
    let scale = f64::from(max_long_edge) / f64::from(long);
    let tw = ((f64::from(width) * scale).round() as u32).max(1);
    let th = ((f64::from(height) * scale).round() as u32).max(1);
    (tw, th)
}

fn bgra_to_rgba(mut bgra: Vec<u8>, width: u32, height: u32) -> Result<RgbaImage, ErrorData> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            mcp_error(
                synapse_core::error_codes::CAPTURE_TARGET_INVALID,
                format!("capture_gif frame dimensions overflow: {width}x{height}"),
            )
        })?;
    if bgra.len() != expected {
        return Err(mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "capture_gif frame byte length {} does not equal {width}x{height}x4={expected}",
                bgra.len()
            ),
        ));
    }
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    RgbaImage::from_raw(width, height, bgra).ok_or_else(|| {
        mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "capture_gif could not build RGBA frame buffer",
        )
    })
}
