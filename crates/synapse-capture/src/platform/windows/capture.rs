use std::{slice, thread, time::Duration};

use synapse_core::Rect;
use windows::{
    Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Resource, ID3D11Texture2D,
    },
    core::Interface as _,
};
use windows_capture::{
    capture::{CaptureControlError, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler},
    dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat, Error as DxgiError},
    frame::{DirtyRegion, Frame},
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

use crate::{
    CaptureConfig, CaptureError, CaptureTarget, CapturedFrame, CapturedFrameBuffer, DxgiFormat,
    controller::{CaptureThreadContext, push_frame},
};

use super::{
    common::{capture_unsupported, hwnd_from_i64},
    dpi::{current_thread_priority, set_capture_thread_priority},
    target::validate_hwnd,
};

pub fn run_graphics_capture(
    config: CaptureConfig,
    ctx: CaptureThreadContext,
) -> Result<(), CaptureError> {
    match config.target.clone() {
        CaptureTarget::Primary => {
            let monitor = Monitor::primary().map_err(capture_unsupported)?;
            start_graphics_capture_with_item(monitor, config, ctx)
        }
        CaptureTarget::Monitor { monitor_index } => {
            let monitor =
                Monitor::from_index(usize::try_from(monitor_index.saturating_add(1)).map_err(
                    |err| CaptureError::TargetInvalid {
                        detail: err.to_string(),
                    },
                )?)
                .map_err(|err| CaptureError::TargetInvalid {
                    detail: err.to_string(),
                })?;
            start_graphics_capture_with_item(monitor, config, ctx)
        }
        CaptureTarget::Window { hwnd } => {
            validate_hwnd(hwnd)?;
            let hwnd = hwnd_from_i64(hwnd)?;
            let window = Window::from_raw_hwnd(hwnd.0);
            start_graphics_capture_with_item(window, config, ctx)
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn run_dxgi_capture(
    config: CaptureConfig,
    ctx: CaptureThreadContext,
) -> Result<(), CaptureError> {
    let monitor = match config.target {
        CaptureTarget::Primary => Monitor::primary().map_err(capture_unsupported)?,
        CaptureTarget::Monitor { monitor_index } => {
            Monitor::from_index(usize::try_from(monitor_index.saturating_add(1)).map_err(
                |err| CaptureError::TargetInvalid {
                    detail: err.to_string(),
                },
            )?)
            .map_err(|err| CaptureError::TargetInvalid {
                detail: err.to_string(),
            })?
        }
        CaptureTarget::Window { .. } => {
            return Err(CaptureError::TargetInvalid {
                detail: "DXGI duplication supports monitor targets only".to_owned(),
            });
        }
    };
    let mut api = DxgiDuplicationApi::new(monitor).map_err(|err| dxgi_error(&err))?;
    let timeout_ms = u32::try_from(config.min_update_interval_ms.max(1)).unwrap_or(u32::MAX);
    let mut frame_seq = 0_u64;

    while !ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
        match api.acquire_next_frame(timeout_ms) {
            Ok(frame) => {
                let captured = captured_frame_from_texture(
                    frame.texture(),
                    frame.width(),
                    frame.height(),
                    dxgi_format(frame.format()),
                    frame_seq,
                    None,
                )?;
                push_frame(&ctx, captured)?;
                frame_seq = frame_seq.saturating_add(1);
            }
            Err(DxgiError::Timeout) => {
                thread::sleep(Duration::from_millis(config.min_update_interval_ms.max(1)));
            }
            Err(DxgiError::AccessLost) => {
                return Err(CaptureError::TargetLost {
                    detail: "DXGI output duplication access lost".to_owned(),
                });
            }
            Err(err) => return Err(dxgi_error(&err)),
        }
    }

    Ok(())
}
#[allow(clippy::needless_pass_by_value)]
fn start_graphics_capture_with_item<T>(
    item: T,
    config: CaptureConfig,
    ctx: CaptureThreadContext,
) -> Result<(), CaptureError>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType> + Send + 'static,
{
    let settings = Settings::new(
        item,
        if config.cursor_visible {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        },
        DrawBorderSettings::WithoutBorder,
        if config.secondary_windows {
            SecondaryWindowSettings::Include
        } else {
            SecondaryWindowSettings::Exclude
        },
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(
            config.min_update_interval_ms.max(1),
        )),
        if config.dirty_region_only {
            DirtyRegionSettings::ReportAndRender
        } else {
            DirtyRegionSettings::Default
        },
        ColorFormat::Bgra8,
        GraphicsHandlerFlags { ctx: ctx.clone() },
    );
    let control = GraphicsHandler::start_free_threaded(settings).map_err(graphics_capture_error)?;
    let poll_interval = Duration::from_millis(config.min_update_interval_ms.max(1));

    while !ctx.stop.load(std::sync::atomic::Ordering::Relaxed) && !control.is_finished() {
        thread::sleep(poll_interval);
    }

    if ctx.stop.load(std::sync::atomic::Ordering::Relaxed) && !control.is_finished() {
        control.stop().map_err(capture_control_error)
    } else {
        control.wait().map_err(capture_control_error)
    }
}

struct GraphicsHandlerFlags {
    ctx: CaptureThreadContext,
}

struct GraphicsHandler {
    ctx: CaptureThreadContext,
    frame_seq: u64,
}

impl GraphicsCaptureApiHandler for GraphicsHandler {
    type Flags = GraphicsHandlerFlags;
    type Error = CaptureError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        set_capture_thread_priority()?;
        ctx.flags
            .ctx
            .stats
            .set_thread_priority(current_thread_priority());
        Ok(Self {
            ctx: ctx.flags.ctx,
            frame_seq: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }

        let format = match frame.color_format() {
            ColorFormat::Bgra8 => DxgiFormat::Bgra8,
            ColorFormat::Rgba8 => DxgiFormat::Rgba8,
            ColorFormat::Rgba16F => DxgiFormat::Rgba16F,
        };
        let captured = captured_frame_from_texture(
            frame.as_raw_texture(),
            frame.width(),
            frame.height(),
            format,
            self.frame_seq,
            union_dirty_regions(&frame.dirty_regions().unwrap_or_default()),
        )?;
        push_frame(&self.ctx, captured)?;
        self.frame_seq = self.frame_seq.saturating_add(1);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.ctx
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

fn capture_control_error(err: CaptureControlError<CaptureError>) -> CaptureError {
    match err {
        CaptureControlError::StoppedHandlerError(err) => err,
        CaptureControlError::GraphicsCaptureApiError(err) => graphics_capture_error(err),
        err => CaptureError::ThreadFailed {
            detail: err.to_string(),
        },
    }
}

fn graphics_capture_error(err: GraphicsCaptureApiError<CaptureError>) -> CaptureError {
    match err {
        GraphicsCaptureApiError::NewHandlerError(err)
        | GraphicsCaptureApiError::FrameHandlerError(err) => err,
        err => CaptureError::ThreadFailed {
            detail: err.to_string(),
        },
    }
}

fn dxgi_error(err: &DxgiError) -> CaptureError {
    CaptureError::GraphicsApiUnsupported {
        detail: err.to_string(),
    }
}

const fn dxgi_format(format: DxgiDuplicationFormat) -> DxgiFormat {
    match format {
        DxgiDuplicationFormat::Rgba16F => DxgiFormat::Rgba16F,
        DxgiDuplicationFormat::Rgb10A2 => DxgiFormat::Rgb10A2,
        DxgiDuplicationFormat::Rgb10XrA2 => DxgiFormat::Rgb10XrA2,
        DxgiDuplicationFormat::Rgba8 => DxgiFormat::Rgba8,
        DxgiDuplicationFormat::Rgba8Srgb => DxgiFormat::Rgba8Srgb,
        DxgiDuplicationFormat::Bgra8 => DxgiFormat::Bgra8,
        DxgiDuplicationFormat::Bgra8Srgb => DxgiFormat::Bgra8Srgb,
    }
}

fn captured_frame_from_texture(
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DxgiFormat,
    frame_seq: u64,
    dirty_region: Option<Rect>,
) -> Result<CapturedFrame, CaptureError> {
    Ok(CapturedFrame {
        pixels: copy_texture_to_owned_buffer(texture, width, height, format)?,
        width,
        height,
        format,
        captured_at: std::time::Instant::now(),
        frame_seq,
        dirty_region,
    })
}

fn copy_texture_to_owned_buffer(
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DxgiFormat,
) -> Result<CapturedFrameBuffer, CaptureError> {
    let bytes_per_pixel = bytes_per_pixel_for_format(format)?;
    let staging = create_full_frame_staging_texture(texture, width, height)?;
    let context = unsafe { texture.GetDevice() }
        .and_then(|device| unsafe { device.GetImmediateContext() })
        .map_err(capture_unsupported)?;
    let source: ID3D11Resource = texture.cast().map_err(capture_unsupported)?;
    let target: ID3D11Resource = staging.cast().map_err(capture_unsupported)?;
    unsafe {
        context.CopyResource(&target, &source);
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&target, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }
        .map_err(capture_unsupported)?;
    let bytes = copy_mapped_rows_tight(&mapped, width, height, usize::from(bytes_per_pixel));
    unsafe {
        context.Unmap(&target, 0);
    }
    let bytes = bytes?;
    Ok(CapturedFrameBuffer {
        bytes,
        row_stride_bytes: frame_row_len(width, usize::from(bytes_per_pixel))?,
        bytes_per_pixel,
    })
}

fn create_full_frame_staging_texture(
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, CaptureError> {
    let mut desc = texture_desc(texture);
    if desc.Width != width || desc.Height != height {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!(
                "capture texture dimensions {}x{} did not match frame dimensions {}x{}",
                desc.Width, desc.Height, width, height
            ),
        });
    }
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0.cast_unsigned();
    desc.MiscFlags = 0;
    desc.SampleDesc.Count = 1;
    desc.SampleDesc.Quality = 0;

    let device = unsafe { texture.GetDevice() }.map_err(capture_unsupported)?;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut staging)) }
        .map_err(capture_unsupported)?;
    staging.ok_or_else(|| CaptureError::GraphicsApiUnsupported {
        detail: "CreateTexture2D returned no owned staging texture".to_owned(),
    })
}

fn texture_desc(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&raw mut desc);
    }
    desc
}

fn copy_mapped_rows_tight(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<Vec<u8>, CaptureError> {
    if mapped.pData.is_null() {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: "capture frame Map returned null pData".to_owned(),
        });
    }
    let row_len = frame_row_len(width, bytes_per_pixel)?;
    let row_pitch =
        usize::try_from(mapped.RowPitch).map_err(|err| CaptureError::GraphicsApiUnsupported {
            detail: format!("invalid capture frame row pitch {}: {err}", mapped.RowPitch),
        })?;
    if row_pitch < row_len {
        return Err(CaptureError::GraphicsApiUnsupported {
            detail: format!(
                "capture frame row pitch {row_pitch} is smaller than row length {row_len}"
            ),
        });
    }
    let height = usize::try_from(height).map_err(|err| CaptureError::TargetInvalid {
        detail: err.to_string(),
    })?;
    let byte_len = row_len
        .checked_mul(height)
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid capture frame dimensions {width}x{height}"),
        })?;
    let mut bytes = Vec::with_capacity(byte_len);
    let base = mapped.pData.cast::<u8>();
    for row in 0..height {
        let row_offset = row
            .checked_mul(row_pitch)
            .ok_or_else(|| CaptureError::TargetInvalid {
                detail: format!("invalid capture frame row offset row={row} pitch={row_pitch}"),
            })?;
        let source = unsafe { slice::from_raw_parts(base.add(row_offset), row_len) };
        bytes.extend_from_slice(source);
    }
    Ok(bytes)
}

fn frame_row_len(width: u32, bytes_per_pixel: usize) -> Result<usize, CaptureError> {
    usize::try_from(width)
        .ok()
        .and_then(|value| value.checked_mul(bytes_per_pixel))
        .ok_or_else(|| CaptureError::TargetInvalid {
            detail: format!("invalid capture frame width {width}"),
        })
}

fn bytes_per_pixel_for_format(format: DxgiFormat) -> Result<u8, CaptureError> {
    match format {
        DxgiFormat::Bgra8
        | DxgiFormat::Bgra8Srgb
        | DxgiFormat::Rgba8
        | DxgiFormat::Rgba8Srgb
        | DxgiFormat::Rgb10A2
        | DxgiFormat::Rgb10XrA2 => Ok(4),
        DxgiFormat::Rgba16F => Ok(8),
        DxgiFormat::Unknown(value) => Err(CaptureError::GraphicsApiUnsupported {
            detail: format!("capture frame copy does not support unknown DXGI format {value}"),
        }),
    }
}

fn union_dirty_regions(regions: &[DirtyRegion]) -> Option<Rect> {
    let first = regions.first()?;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.x.saturating_add(first.width);
    let mut bottom = first.y.saturating_add(first.height);

    for region in &regions[1..] {
        left = left.min(region.x);
        top = top.min(region.y);
        right = right.max(region.x.saturating_add(region.width));
        bottom = bottom.max(region.y.saturating_add(region.height));
    }

    Some(Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    })
}
