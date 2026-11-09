use serde::{Deserialize, Serialize};
use synapse_core::Rect;

use crate::{PerceptionError, PerceptionResult};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextRegion {
    pub text: String,
    pub bbox: Rect,
    pub confidence: f32,
    #[serde(default)]
    pub confidence_source: TextRegionConfidenceSource,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRegionConfidenceSource {
    Engine,
    Uia,
    Synthetic,
    Heuristic,
    #[default]
    Unsupported,
}

pub trait OcrProvider {
    /// Reads text from a screen-coordinate region.
    ///
    /// # Errors
    ///
    /// Returns a structured perception error when OCR cannot run or finds no text.
    fn read_text(&self, region: Rect) -> PerceptionResult<Vec<TextRegion>>;
}

#[derive(Copy, Clone, Debug, Default)]
pub struct SystemOcrProvider;

impl OcrProvider for SystemOcrProvider {
    fn read_text(&self, region: Rect) -> PerceptionResult<Vec<TextRegion>> {
        read_text(region)
    }
}

/// Reads OCR text from a screen-coordinate region.
///
/// # Errors
///
/// Returns `OCR_NO_TEXT` for an empty region and `OCR_BACKEND_UNAVAILABLE`
/// when the platform OCR backend cannot run.
pub fn read_text(region: Rect) -> PerceptionResult<Vec<TextRegion>> {
    if is_empty_region(region) {
        return Err(PerceptionError::OcrNoText { region });
    }
    platform::read_text(region)
}

/// Reads OCR text with an injected provider.
///
/// # Errors
///
/// Returns `OCR_NO_TEXT` for invalid/empty regions or empty provider output.
pub fn read_text_with_provider(
    provider: &dyn OcrProvider,
    region: Rect,
) -> PerceptionResult<Vec<TextRegion>> {
    if is_empty_region(region) {
        return Err(PerceptionError::OcrNoText { region });
    }
    let words = provider.read_text(region)?;
    if words.is_empty() {
        return Err(PerceptionError::OcrNoText { region });
    }
    Ok(words)
}

#[must_use]
pub const fn is_empty_region(region: Rect) -> bool {
    region.w <= 0 || region.h <= 0
}

#[cfg(windows)]
/// Runs `WinRT` OCR over a caller-provided `SoftwareBitmap`.
///
/// # Errors
///
/// Returns `OCR_BACKEND_UNAVAILABLE` when `WinRT` cannot initialize or rejects
/// the bitmap, and `OCR_NO_TEXT` when OCR completes with no recognized words.
pub fn read_text_from_software_bitmap(
    region: Rect,
    bitmap: &windows::Graphics::Imaging::SoftwareBitmap,
) -> PerceptionResult<Vec<TextRegion>> {
    if is_empty_region(region) {
        return Err(PerceptionError::OcrNoText { region });
    }
    platform::read_text_from_software_bitmap(region, bitmap)
}

#[cfg(windows)]
/// Runs `WinRT` OCR over caller-provided BGRA screen-region bytes.
///
/// # Errors
///
/// Returns `OCR_BACKEND_UNAVAILABLE` when the bitmap dimensions or byte length
/// are invalid or `WinRT` cannot run, and `OCR_NO_TEXT` when OCR completes with
/// no recognized words.
pub fn read_text_from_bgra_bitmap(
    region: Rect,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> PerceptionResult<Vec<TextRegion>> {
    if is_empty_region(region) {
        return Err(PerceptionError::OcrNoText { region });
    }
    platform::read_text_from_bgra_bitmap(region, width, height, bytes)
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use serde::Deserialize;
    use synapse_core::Rect;

    use super::{PerceptionError, PerceptionResult, TextRegion, TextRegionConfidenceSource};

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HostOcrWord {
        text: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        confidence: f32,
        #[serde(default)]
        confidence_source: TextRegionConfidenceSource,
    }

    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    enum HostOcrPayload {
        Words(Vec<HostOcrWord>),
        Word(HostOcrWord),
    }

    const POWERSHELL_CANDIDATES: &[&str] = &[
        "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
        "/mnt/c/Program Files/PowerShell/7/pwsh.exe",
        "powershell.exe",
        "pwsh.exe",
    ];

    pub fn read_text(region: Rect) -> PerceptionResult<Vec<TextRegion>> {
        let powershell = powershell_path()?;
        let script = host_ocr_script(region);
        let output = Command::new(&powershell)
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
            .arg(script)
            .output()
            .map_err(|err| backend_unavailable(format!("host OCR command failed: {err}")))?;

        if !output.status.success() {
            return Err(backend_unavailable(format!(
                "host OCR exited with status {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|err| backend_unavailable(format!("host OCR stdout was not UTF-8: {err}")))?;
        let trimmed = stdout.trim();
        if trimmed.is_empty() || trimmed == "[]" {
            return Err(PerceptionError::OcrNoText { region });
        }
        let payload: HostOcrPayload = serde_json::from_str(trimmed)
            .map_err(|err| backend_unavailable(format!("host OCR JSON parse failed: {err}")))?;
        let words = match payload {
            HostOcrPayload::Words(words) => words,
            HostOcrPayload::Word(word) => vec![word],
        };
        if words.is_empty() {
            return Err(PerceptionError::OcrNoText { region });
        }
        let regions = words
            .into_iter()
            .filter(|word| !word.text.trim().is_empty())
            .map(|word| TextRegion {
                text: word.text,
                bbox: Rect {
                    x: region.x.saturating_add(word.x),
                    y: region.y.saturating_add(word.y),
                    w: word.w,
                    h: word.h,
                },
                confidence: word.confidence,
                confidence_source: word.confidence_source,
            })
            .collect::<Vec<_>>();
        if regions.is_empty() {
            Err(PerceptionError::OcrNoText { region })
        } else {
            Ok(regions)
        }
    }

    fn powershell_path() -> PerceptionResult<PathBuf> {
        POWERSHELL_CANDIDATES
            .iter()
            .map(Path::new)
            .find(|path| path.exists())
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                backend_unavailable(
                    "host Windows PowerShell executable was not found under /mnt/c or PATH"
                        .to_owned(),
                )
            })
    }

    const fn backend_unavailable(detail: String) -> PerceptionError {
        PerceptionError::OcrBackendUnavailable { detail }
    }

    fn host_ocr_script(region: Rect) -> String {
        format!(
            "$x = {}\n$y = {}\n$w = {}\n$h = {}\n{}",
            region.x, region.y, region.w, region.h, HOST_OCR_SCRIPT_BODY
        )
    }

    const HOST_OCR_SCRIPT_BODY: &str = r"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.IRandomAccessStreamWithContentType, Windows.Storage.Streams, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.SoftwareBitmap, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
$null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
$asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and
    $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
})[0]
function Await($op, $type) {
    $asTask = $asTaskGeneric.MakeGenericMethod($type)
    $task = $asTask.Invoke($null, @($op))
    $task.Wait() | Out-Null
    $task.Result
}
$dir = Join-Path $env:TEMP 'synapse-ocr'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$path = Join-Path $dir ('ocr-{0}-{1}.png' -f $PID, [Guid]::NewGuid().ToString('N'))
$bitmap = $null
$graphics = $null
try {
    $bitmap = New-Object System.Drawing.Bitmap($w, $h)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($x, $y, 0, 0, $bitmap.Size)
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($path)) ([Windows.Storage.StorageFile])
    $stream = Await ($file.OpenReadAsync()) ([Windows.Storage.Streams.IRandomAccessStreamWithContentType])
    $decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
    $softwareBitmap = Await ($decoder.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
    $engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
    if ($null -eq $engine) {
        throw 'Windows.Media.Ocr returned no recognizer for the user profile languages'
    }
    $result = Await ($engine.RecognizeAsync($softwareBitmap)) ([Windows.Media.Ocr.OcrResult])
    $rows = @()
    foreach ($line in $result.Lines) {
        foreach ($word in $line.Words) {
            $rect = $word.BoundingRect
            $rows += [pscustomobject]@{
                text = [string]$word.Text
                x = [int][Math]::Round($rect.X)
                y = [int][Math]::Round($rect.Y)
                w = [int][Math]::Round($rect.Width)
                h = [int][Math]::Round($rect.Height)
                confidence = 0.0
                confidence_source = 'unsupported'
            }
        }
    }
    if ($rows.Count -eq 0) {
        '[]'
    } else {
        $rows | ConvertTo-Json -Compress
    }
} finally {
    if ($null -ne $graphics) { $graphics.Dispose() }
    if ($null -ne $bitmap) { $bitmap.Dispose() }
    Remove-Item -LiteralPath $path -ErrorAction SilentlyContinue
}
";
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
mod platform {
    use synapse_core::Rect;

    use super::{PerceptionError, PerceptionResult, TextRegion, TextRegionConfidenceSource};

    pub fn read_text(_region: Rect) -> PerceptionResult<Vec<TextRegion>> {
        Err(PerceptionError::OcrBackendUnavailable {
            detail: "OCR backend is implemented on Windows and WSL/Linux with Windows host OCR"
                .to_owned(),
        })
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::OnceLock;

    use image::{ImageBuffer, Rgba, imageops::FilterType};
    use synapse_capture::screen_region_to_bgra_bitmap;
    use synapse_core::Rect;
    use windows::{
        Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
        Media::Ocr::{OcrEngine, OcrResult},
        Storage::Streams::DataWriter,
    };

    use super::{PerceptionError, PerceptionResult, TextRegion, TextRegionConfidenceSource};

    const OCR_MIN_RECOGNITION_HEIGHT_PX: u32 = 64;
    const OCR_MAX_UPSCALE: u32 = 6;
    const OCR_SPARSE_TILE_MIN_WIDTH_PX: u32 = 640;
    const OCR_SPARSE_TILE_TARGET_WIDTH_PX: u32 = 480;
    const OCR_SPARSE_TILE_OVERLAP_PX: u32 = 96;
    const OCR_SPARSE_ASPECT_RATIO_NUMERATOR: u32 = 6;
    const OCR_BACKGROUND_DIFF_THRESHOLD: u16 = 90;
    const OCR_CONTENT_PADDING_PX: u32 = 6;

    pub fn read_text(region: Rect) -> PerceptionResult<Vec<TextRegion>> {
        let captured = screen_region_to_bgra_bitmap(region)
            .map_err(|err| backend_unavailable(err.to_string()))?;
        read_text_from_bgra_bitmap(region, captured.width, captured.height, &captured.bytes)
    }

    pub fn read_text_from_software_bitmap(
        region: Rect,
        bitmap: &windows::Graphics::Imaging::SoftwareBitmap,
    ) -> PerceptionResult<Vec<TextRegion>> {
        let engine = ocr_engine()?;
        let result = engine
            .RecognizeAsync(bitmap)
            .map_err(|err| backend_unavailable(err.to_string()))?
            .join()
            .map_err(|err| backend_unavailable(err.to_string()))?;
        text_regions_from_result(region, &result, 1.0)
    }

    pub fn read_text_from_bgra_bitmap(
        region: Rect,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> PerceptionResult<Vec<TextRegion>> {
        let engine = ocr_engine()?;
        let max_dimension = ocr_max_image_dimension()?;
        let primary = recognize_bgra_region(engine, region, width, height, bytes, max_dimension);
        let fallback = if should_try_sparse_ocr(width, height, max_dimension)
            || matches!(
                &primary,
                Err(PerceptionError::OcrNoText { .. }
                    | PerceptionError::OcrBackendUnavailable { .. })
            ) {
            sparse_bgra_alternative(engine, region, width, height, bytes, max_dimension)
        } else {
            None
        };
        select_ocr_candidate(primary, fallback)
    }

    fn ocr_engine() -> PerceptionResult<&'static OcrEngine> {
        static ENGINE: OnceLock<Result<OcrEngine, String>> = OnceLock::new();
        ENGINE
            .get_or_init(|| {
                OcrEngine::TryCreateFromUserProfileLanguages().map_err(|err| err.to_string())
            })
            .as_ref()
            .map_err(|detail| backend_unavailable(detail.clone()))
    }

    fn text_regions_from_result(
        region: Rect,
        result: &OcrResult,
        scale: f64,
    ) -> PerceptionResult<Vec<TextRegion>> {
        let lines = result
            .Lines()
            .map_err(|err| backend_unavailable(err.to_string()))?;
        let mut output = Vec::new();
        for line_index in 0..lines
            .Size()
            .map_err(|err| backend_unavailable(err.to_string()))?
        {
            let line = lines
                .GetAt(line_index)
                .map_err(|err| backend_unavailable(err.to_string()))?;
            let words = line
                .Words()
                .map_err(|err| backend_unavailable(err.to_string()))?;
            for word_index in 0..words
                .Size()
                .map_err(|err| backend_unavailable(err.to_string()))?
            {
                let word = words
                    .GetAt(word_index)
                    .map_err(|err| backend_unavailable(err.to_string()))?;
                let bbox = word
                    .BoundingRect()
                    .map_err(|err| backend_unavailable(err.to_string()))?;
                output.push(TextRegion {
                    text: word
                        .Text()
                        .map_err(|err| backend_unavailable(err.to_string()))?
                        .to_string_lossy(),
                    bbox: Rect {
                        x: region.x.saturating_add(round_scaled_to_i32(bbox.X, scale)),
                        y: region.y.saturating_add(round_scaled_to_i32(bbox.Y, scale)),
                        w: round_scaled_to_i32(bbox.Width, scale).max(1),
                        h: round_scaled_to_i32(bbox.Height, scale).max(1),
                    },
                    confidence: 0.0,
                    confidence_source: TextRegionConfidenceSource::Unsupported,
                });
            }
        }
        if output.is_empty() {
            Err(PerceptionError::OcrNoText { region })
        } else {
            Ok(output)
        }
    }

    fn recognize_bgra_region(
        engine: &OcrEngine,
        region: Rect,
        width: u32,
        height: u32,
        bytes: &[u8],
        max_dimension: u32,
    ) -> PerceptionResult<Vec<TextRegion>> {
        let (bitmap, scale) = ocr_bitmap_from_bgra(width, height, bytes, max_dimension)?;
        let result = engine
            .RecognizeAsync(&bitmap)
            .map_err(|err| backend_unavailable(err.to_string()))?
            .join()
            .map_err(|err| backend_unavailable(err.to_string()))?;
        text_regions_from_result(region, &result, f64::from(scale))
    }

    fn select_ocr_candidate(
        primary: PerceptionResult<Vec<TextRegion>>,
        fallback: Option<Vec<TextRegion>>,
    ) -> PerceptionResult<Vec<TextRegion>> {
        match (primary, fallback) {
            (Ok(primary), Some(fallback))
                if ocr_candidate_score(&fallback) > ocr_candidate_score(&primary) =>
            {
                Ok(fallback)
            }
            (Ok(primary), _) => Ok(primary),
            (Err(_primary), Some(fallback)) if !fallback.is_empty() => Ok(fallback),
            (Err(primary), _) => Err(primary),
        }
    }

    fn sparse_bgra_alternative(
        engine: &OcrEngine,
        region: Rect,
        width: u32,
        height: u32,
        bytes: &[u8],
        max_dimension: u32,
    ) -> Option<Vec<TextRegion>> {
        validate_bgra_len(width, height, bytes).ok()?;
        let mut best = if let Some(bounds) = content_bounds_for_bgra(width, height, bytes)
            && let Some(candidate) = recognize_bgra_subregion(
                engine,
                region,
                width,
                height,
                bytes,
                bounds,
                max_dimension,
            ) {
            Some(candidate)
        } else {
            None
        };
        if should_try_sparse_tiling(width, height, max_dimension) {
            let mut tiled = Vec::new();
            for tile in sparse_ocr_tiles(width, height, max_dimension) {
                if let Some(mut words) = recognize_bgra_subregion(
                    engine,
                    region,
                    width,
                    height,
                    bytes,
                    tile,
                    max_dimension,
                ) {
                    tiled.append(&mut words);
                }
            }
            dedupe_text_regions(&mut tiled);
            if !tiled.is_empty()
                && best.as_ref().is_none_or(|current| {
                    ocr_candidate_score(&tiled) > ocr_candidate_score(current)
                })
            {
                best = Some(tiled);
            }
        }
        best
    }

    fn recognize_bgra_subregion(
        engine: &OcrEngine,
        base_region: Rect,
        source_width: u32,
        source_height: u32,
        source_bytes: &[u8],
        subregion: BitmapRect,
        max_dimension: u32,
    ) -> Option<Vec<TextRegion>> {
        let bytes = crop_bgra(source_width, source_height, source_bytes, subregion).ok()?;
        let region = Rect {
            x: base_region.x.saturating_add(u32_to_i32(subregion.x)),
            y: base_region.y.saturating_add(u32_to_i32(subregion.y)),
            w: u32_to_i32(subregion.w).max(1),
            h: u32_to_i32(subregion.h).max(1),
        };
        recognize_bgra_region(
            engine,
            region,
            subregion.w,
            subregion.h,
            &bytes,
            max_dimension,
        )
        .ok()
    }

    fn ocr_candidate_score(words: &[TextRegion]) -> usize {
        let text_len = words
            .iter()
            .map(|word| word.text.chars().count())
            .sum::<usize>();
        words.len().saturating_mul(1_000).saturating_add(text_len)
    }

    fn round_scaled_to_i32(value: f32, scale: f64) -> i32 {
        let divisor = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        round_to_i32(f64::from(value) / divisor)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn round_to_i32(value: f64) -> i32 {
        if !value.is_finite() {
            0
        } else if value >= f64::from(i32::MAX) {
            i32::MAX
        } else if value <= f64::from(i32::MIN) {
            i32::MIN
        } else {
            value.round() as i32
        }
    }

    const fn backend_unavailable(detail: String) -> PerceptionError {
        PerceptionError::OcrBackendUnavailable { detail }
    }

    fn ocr_max_image_dimension() -> PerceptionResult<u32> {
        OcrEngine::MaxImageDimension().map_err(|err| backend_unavailable(err.to_string()))
    }

    fn ocr_bitmap_from_bgra(
        width: u32,
        height: u32,
        bytes: &[u8],
        max_dimension: u32,
    ) -> PerceptionResult<(SoftwareBitmap, u32)> {
        validate_bgra_len(width, height, bytes)?;
        let scale = recognition_upscale(width, height, max_dimension);
        let (width, height, bytes) = if scale > 1 {
            let width = width.checked_mul(scale).ok_or_else(|| {
                backend_unavailable(format!("scaled OCR width overflowed: {width} * {scale}"))
            })?;
            let height = height.checked_mul(scale).ok_or_else(|| {
                backend_unavailable(format!("scaled OCR height overflowed: {height} * {scale}"))
            })?;
            (
                width,
                height,
                upscale_bgra(bytes, width / scale, height / scale, scale)?,
            )
        } else {
            (width, height, bytes.to_vec())
        };
        let bitmap = software_bitmap_from_bgra(&bytes, width, height)?;
        Ok((bitmap, scale))
    }

    fn validate_bgra_len(width: u32, height: u32, bytes: &[u8]) -> PerceptionResult<()> {
        if width == 0 || height == 0 {
            return Err(backend_unavailable(format!(
                "OCR BGRA bitmap dimensions must be non-zero: {width}x{height}"
            )));
        }
        let expected_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| backend_unavailable("OCR BGRA dimensions overflowed".to_owned()))?;
        let actual_len = u64::try_from(bytes.len())
            .map_err(|_err| backend_unavailable("OCR BGRA byte length was invalid".to_owned()))?;
        if actual_len != expected_len {
            return Err(backend_unavailable(format!(
                "OCR BGRA buffer length mismatch: expected {expected_len} bytes, got {actual_len}"
            )));
        }
        Ok(())
    }

    fn recognition_upscale(width: u32, height: u32, max_dimension: u32) -> u32 {
        if height == 0 || height >= OCR_MIN_RECOGNITION_HEIGHT_PX {
            1
        } else {
            let height_scale = OCR_MIN_RECOGNITION_HEIGHT_PX
                .div_ceil(height)
                .clamp(1, OCR_MAX_UPSCALE);
            let dimension_scale = if max_dimension == 0 || width == 0 {
                OCR_MAX_UPSCALE
            } else {
                let width_scale = max_dimension / width;
                let height_scale = max_dimension / height;
                width_scale.min(height_scale).max(1)
            };
            height_scale.min(dimension_scale).clamp(1, OCR_MAX_UPSCALE)
        }
    }

    fn upscale_bgra(
        bytes: &[u8],
        width: u32,
        height: u32,
        scale: u32,
    ) -> PerceptionResult<Vec<u8>> {
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, bytes.to_vec())
            .ok_or_else(|| {
                backend_unavailable("captured OCR BGRA buffer size was invalid".to_owned())
            })?;
        let scaled_width = width.checked_mul(scale).ok_or_else(|| {
            backend_unavailable(format!("scaled OCR width overflowed: {width} * {scale}"))
        })?;
        let scaled_height = height.checked_mul(scale).ok_or_else(|| {
            backend_unavailable(format!("scaled OCR height overflowed: {height} * {scale}"))
        })?;
        Ok(
            image::imageops::resize(&image, scaled_width, scaled_height, FilterType::Nearest)
                .into_raw(),
        )
    }

    fn software_bitmap_from_bgra(
        bytes: &[u8],
        width: u32,
        height: u32,
    ) -> PerceptionResult<SoftwareBitmap> {
        let width = i32::try_from(width)
            .map_err(|err| backend_unavailable(format!("OCR bitmap width was invalid: {err}")))?;
        let height = i32::try_from(height)
            .map_err(|err| backend_unavailable(format!("OCR bitmap height was invalid: {err}")))?;
        let writer = DataWriter::new().map_err(|err| backend_unavailable(err.to_string()))?;
        writer
            .WriteBytes(bytes)
            .map_err(|err| backend_unavailable(err.to_string()))?;
        let buffer = writer
            .DetachBuffer()
            .map_err(|err| backend_unavailable(err.to_string()))?;
        SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &buffer,
            BitmapPixelFormat::Bgra8,
            width,
            height,
            BitmapAlphaMode::Ignore,
        )
        .map_err(|err| backend_unavailable(err.to_string()))
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct BitmapRect {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    }

    fn should_try_sparse_ocr(width: u32, height: u32, max_dimension: u32) -> bool {
        content_candidate_possible(width, height)
            || should_try_sparse_tiling(width, height, max_dimension)
    }

    const fn content_candidate_possible(width: u32, height: u32) -> bool {
        width >= OCR_SPARSE_TILE_MIN_WIDTH_PX || height < OCR_MIN_RECOGNITION_HEIGHT_PX
    }

    fn should_try_sparse_tiling(width: u32, height: u32, max_dimension: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        let too_wide_for_engine = max_dimension > 0 && width > max_dimension;
        let sparse_horizontal_strip = width >= OCR_SPARSE_TILE_MIN_WIDTH_PX
            && u64::from(width)
                >= u64::from(height).saturating_mul(u64::from(OCR_SPARSE_ASPECT_RATIO_NUMERATOR));
        too_wide_for_engine || sparse_horizontal_strip
    }

    fn sparse_ocr_tiles(width: u32, height: u32, max_dimension: u32) -> Vec<BitmapRect> {
        if !should_try_sparse_tiling(width, height, max_dimension) {
            return Vec::new();
        }
        let tile_width = OCR_SPARSE_TILE_TARGET_WIDTH_PX.min(width).max(1);
        let overlap = OCR_SPARSE_TILE_OVERLAP_PX.min(tile_width.saturating_sub(1));
        let step = tile_width.saturating_sub(overlap).max(1);
        let mut tiles = Vec::new();
        let mut x = 0;
        loop {
            let remaining = width.saturating_sub(x);
            let w = remaining.min(tile_width);
            tiles.push(BitmapRect {
                x,
                y: 0,
                w,
                h: height,
            });
            if x.saturating_add(w) >= width {
                break;
            }
            x = x.saturating_add(step);
            if x >= width {
                break;
            }
        }
        tiles
    }

    fn content_bounds_for_bgra(width: u32, height: u32, bytes: &[u8]) -> Option<BitmapRect> {
        if !content_candidate_possible(width, height)
            || validate_bgra_len(width, height, bytes).is_err()
        {
            return None;
        }
        let background = corner_average_bgr(width, height, bytes)?;
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0;
        let mut max_y = 0;
        let mut found = false;
        for y in 0..height {
            for x in 0..width {
                if pixel_differs_from_background(width, bytes, x, y, background) {
                    found = true;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        if !found {
            return None;
        }
        let padding = OCR_CONTENT_PADDING_PX
            .min(width.saturating_sub(1))
            .min(height.saturating_sub(1));
        min_x = min_x.saturating_sub(padding);
        min_y = min_y.saturating_sub(padding);
        max_x = max_x.saturating_add(padding).min(width.saturating_sub(1));
        max_y = max_y.saturating_add(padding).min(height.saturating_sub(1));
        let bounds = BitmapRect {
            x: min_x,
            y: min_y,
            w: max_x.saturating_sub(min_x).saturating_add(1),
            h: max_y.saturating_sub(min_y).saturating_add(1),
        };
        let original_area = u64::from(width).saturating_mul(u64::from(height));
        let bounds_area = u64::from(bounds.w).saturating_mul(u64::from(bounds.h));
        (bounds_area.saturating_mul(10) < original_area.saturating_mul(9)).then_some(bounds)
    }

    fn corner_average_bgr(width: u32, height: u32, bytes: &[u8]) -> Option<[u8; 3]> {
        let corners = [
            (0, 0),
            (width.saturating_sub(1), 0),
            (0, height.saturating_sub(1)),
            (width.saturating_sub(1), height.saturating_sub(1)),
        ];
        let mut sums = [0_u32; 3];
        for (x, y) in corners {
            let offset = bgra_offset(width, x, y)?;
            sums[0] = sums[0].saturating_add(u32::from(*bytes.get(offset)?));
            sums[1] = sums[1].saturating_add(u32::from(*bytes.get(offset + 1)?));
            sums[2] = sums[2].saturating_add(u32::from(*bytes.get(offset + 2)?));
        }
        Some([
            u8::try_from(sums[0] / 4).ok()?,
            u8::try_from(sums[1] / 4).ok()?,
            u8::try_from(sums[2] / 4).ok()?,
        ])
    }

    fn pixel_differs_from_background(
        width: u32,
        bytes: &[u8],
        x: u32,
        y: u32,
        background: [u8; 3],
    ) -> bool {
        let Some(offset) = bgra_offset(width, x, y) else {
            return false;
        };
        let Some(blue) = bytes.get(offset) else {
            return false;
        };
        let Some(green) = bytes.get(offset + 1) else {
            return false;
        };
        let Some(red) = bytes.get(offset + 2) else {
            return false;
        };
        let diff = u16::from(blue.abs_diff(background[0]))
            .saturating_add(u16::from(green.abs_diff(background[1])))
            .saturating_add(u16::from(red.abs_diff(background[2])));
        diff > OCR_BACKGROUND_DIFF_THRESHOLD
    }

    fn crop_bgra(
        source_width: u32,
        source_height: u32,
        source_bytes: &[u8],
        region: BitmapRect,
    ) -> PerceptionResult<Vec<u8>> {
        validate_bgra_len(source_width, source_height, source_bytes)?;
        if region.w == 0
            || region.h == 0
            || region.x >= source_width
            || region.y >= source_height
            || region.x.saturating_add(region.w) > source_width
            || region.y.saturating_add(region.h) > source_height
        {
            return Err(backend_unavailable(format!(
                "OCR BGRA crop region out of bounds: source={source_width}x{source_height} region={region:?}"
            )));
        }
        let row_len = usize::try_from(region.w)
            .ok()
            .and_then(|w| w.checked_mul(4))
            .ok_or_else(|| backend_unavailable("OCR BGRA crop row length overflowed".to_owned()))?;
        let capacity = usize::try_from(region.h)
            .ok()
            .and_then(|h| h.checked_mul(row_len))
            .ok_or_else(|| backend_unavailable("OCR BGRA crop size overflowed".to_owned()))?;
        let mut output = Vec::with_capacity(capacity);
        for row in region.y..region.y.saturating_add(region.h) {
            let start = bgra_offset(source_width, region.x, row).ok_or_else(|| {
                backend_unavailable("OCR BGRA crop start offset overflowed".to_owned())
            })?;
            let end = start.checked_add(row_len).ok_or_else(|| {
                backend_unavailable("OCR BGRA crop end offset overflowed".to_owned())
            })?;
            let slice = source_bytes.get(start..end).ok_or_else(|| {
                backend_unavailable("OCR BGRA crop row was outside source bytes".to_owned())
            })?;
            output.extend_from_slice(slice);
        }
        Ok(output)
    }

    fn dedupe_text_regions(words: &mut Vec<TextRegion>) {
        words.sort_by(|left, right| {
            left.bbox
                .y
                .cmp(&right.bbox.y)
                .then(left.bbox.x.cmp(&right.bbox.x))
                .then(left.text.cmp(&right.text))
        });
        let mut deduped: Vec<TextRegion> = Vec::new();
        'word: for word in words.drain(..) {
            for existing in &mut deduped {
                if rect_overlap_ratio(existing.bbox, word.bbox) >= 0.45 {
                    if ocr_candidate_score(std::slice::from_ref(&word))
                        > ocr_candidate_score(std::slice::from_ref(existing))
                    {
                        *existing = word;
                    }
                    continue 'word;
                }
            }
            deduped.push(word);
        }
        *words = deduped;
    }

    fn rect_overlap_ratio(left: Rect, right: Rect) -> f64 {
        let left_end = (
            left.x.saturating_add(left.w.max(0)),
            left.y.saturating_add(left.h.max(0)),
        );
        let right_end = (
            right.x.saturating_add(right.w.max(0)),
            right.y.saturating_add(right.h.max(0)),
        );
        let overlap_w = left_end
            .0
            .min(right_end.0)
            .saturating_sub(left.x.max(right.x));
        let overlap_h = left_end
            .1
            .min(right_end.1)
            .saturating_sub(left.y.max(right.y));
        if overlap_w <= 0 || overlap_h <= 0 {
            return 0.0;
        }
        let overlap_area = f64::from(overlap_w) * f64::from(overlap_h);
        let left_area = f64::from(left.w.max(0)) * f64::from(left.h.max(0));
        let right_area = f64::from(right.w.max(0)) * f64::from(right.h.max(0));
        let min_area = left_area.min(right_area);
        if min_area <= 0.0 {
            0.0
        } else {
            overlap_area / min_area
        }
    }

    fn bgra_offset(width: u32, x: u32, y: u32) -> Option<usize> {
        u64::from(y)
            .checked_mul(u64::from(width))?
            .checked_add(u64::from(x))?
            .checked_mul(4)?
            .try_into()
            .ok()
    }

    fn u32_to_i32(value: u32) -> i32 {
        i32::try_from(value).unwrap_or(i32::MAX)
    }
}
