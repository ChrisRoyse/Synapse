use rmcp::ErrorData;
use synapse_core::{OcrBackend, OcrConfidenceSource, OcrResult, OcrWord, Rect, error_codes};
use synapse_perception::{
    TextRegion, TextRegionConfidenceSource, read_text as platform_read_text,
    read_text_with_provider,
};

use crate::m1::{M1State, ReadTextParams, current_input, mcp_error};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadTextCaptureSource {
    Screen,
    Window {
        hwnd: i64,
        window_region: Rect,
    },
    /// OCR the entire window using the captured WGC frame's native dimensions.
    /// Used when `window_hwnd` is supplied with no region/element target.
    WholeWindow {
        hwnd: i64,
    },
}

#[derive(Clone, Debug)]
pub struct ResolvedReadTextRequest {
    pub region: Rect,
    pub capture_source: ReadTextCaptureSource,
    pub requested_backend: OcrBackend,
    pub effective_backend: OcrBackend,
    pub lang_hint: Option<String>,
    pub synthetic: bool,
    /// When true, a clean OCR pass that finds zero glyphs stays a hard
    /// `OCR_NO_TEXT` error instead of the #1557 empty-observation success — for
    /// callers that want to pin fail-closed absence. Carried on the resolved
    /// request for completeness; the fail-closed gate is enforced separately by
    /// [`enforce_require_text`] from the tool params.
    #[allow(dead_code)]
    pub require_text: bool,
}

impl ResolvedReadTextRequest {
    #[must_use]
    pub fn lang(&self) -> String {
        self.lang_hint
            .as_deref()
            .map(str::trim)
            .filter(|lang| !lang.is_empty())
            .unwrap_or("und")
            .to_owned()
    }
}

pub fn resolve_read_text_request(
    state: &M1State,
    params: &ReadTextParams,
    target_hwnd: Option<i64>,
) -> Result<ResolvedReadTextRequest, ErrorData> {
    let (region, capture_source) = text_region(state, params, target_hwnd)?;
    if !matches!(capture_source, ReadTextCaptureSource::WholeWindow { .. }) {
        validate_ocr_region(region)?;
    }
    Ok(ResolvedReadTextRequest {
        region,
        capture_source,
        requested_backend: params.backend,
        effective_backend: effective_ocr_backend(params.backend)?,
        lang_hint: params.lang_hint.clone(),
        synthetic: state.synthetic.is_some(),
        require_text: params.require_text,
    })
}

pub fn read_text_request_uncached(
    request: &ResolvedReadTextRequest,
) -> Result<OcrResult, ErrorData> {
    if request.synthetic {
        let provider = SyntheticOcrProvider {
            region: request.region,
        };
        return ocr_result_or_empty(read_text_with_provider(&provider, request.region), request);
    }
    match request.effective_backend {
        OcrBackend::Winrt => ocr_result_or_empty(platform_read_text(request.region), request),
        OcrBackend::Crnn => Err(crnn_unavailable_error()),
        OcrBackend::Auto => Err(mcp_error(
            error_codes::OCR_BACKEND_UNAVAILABLE,
            "internal OCR backend resolution left backend=auto after request validation",
        )),
    }
}

#[cfg(windows)]
pub fn read_text_request_from_bgra(
    request: &ResolvedReadTextRequest,
    captured: &synapse_capture::CapturedBgraBitmap,
) -> Result<OcrResult, ErrorData> {
    if request.synthetic {
        return read_text_request_uncached(request);
    }
    let request = read_text_request_for_captured_bitmap(request.clone(), captured)?;
    match request.effective_backend {
        OcrBackend::Winrt => ocr_result_or_empty(
            synapse_perception::read_text_from_bgra_bitmap(
                request.region,
                captured.width,
                captured.height,
                &captured.bytes,
            ),
            &request,
        ),
        OcrBackend::Crnn => Err(crnn_unavailable_error()),
        OcrBackend::Auto => Err(mcp_error(
            error_codes::OCR_BACKEND_UNAVAILABLE,
            "internal OCR backend resolution left backend=auto after request validation",
        )),
    }
}

#[cfg(windows)]
pub fn read_text_request_for_captured_bitmap(
    mut request: ResolvedReadTextRequest,
    captured: &synapse_capture::CapturedBgraBitmap,
) -> Result<ResolvedReadTextRequest, ErrorData> {
    if matches!(
        request.capture_source,
        ReadTextCaptureSource::WholeWindow { .. }
    ) {
        request.region = Rect {
            x: 0,
            y: 0,
            w: i32::try_from(captured.width).map_err(|_| {
                mcp_error(
                    error_codes::OCR_NO_TEXT,
                    format!(
                        "whole-window OCR bitmap width {} exceeds i32",
                        captured.width
                    ),
                )
            })?,
            h: i32::try_from(captured.height).map_err(|_| {
                mcp_error(
                    error_codes::OCR_NO_TEXT,
                    format!(
                        "whole-window OCR bitmap height {} exceeds i32",
                        captured.height
                    ),
                )
            })?,
        };
        validate_ocr_region(request.region)?;
    }
    Ok(request)
}

/// Runs WinRT OCR over a web element's captured BGRA bitmap and returns an
/// `OcrResult` whose word boxes are relative to the captured element (#703).
///
/// Used by the `read_text` handler when `element_id` is a CDP/web node, which
/// the UIA element-bounds path cannot resolve. The bitmap comes from a CDP
/// element-clipped screenshot, so the OCR region is the whole bitmap.
///
/// # Errors
///
/// `OCR_NO_TEXT` if the bitmap dimensions exceed `i32`; any `WinRT` OCR backend
/// error from `read_text_from_bgra_bitmap`. A clean pass that finds no glyphs is
/// a success with `no_text: true` (empty observation), not an error (#1557).
#[cfg(windows)]
pub fn ocr_result_from_web_bitmap(
    width: u32,
    height: u32,
    bgra: &[u8],
    lang_hint: Option<&str>,
) -> Result<OcrResult, ErrorData> {
    let w = i32::try_from(width).map_err(|_| {
        mcp_error(
            error_codes::OCR_NO_TEXT,
            format!("web element OCR bitmap width {width} exceeds i32"),
        )
    })?;
    let h = i32::try_from(height).map_err(|_| {
        mcp_error(
            error_codes::OCR_NO_TEXT,
            format!("web element OCR bitmap height {height} exceeds i32"),
        )
    })?;
    let region = Rect { x: 0, y: 0, w, h };
    let request = ResolvedReadTextRequest {
        region,
        capture_source: ReadTextCaptureSource::Screen,
        requested_backend: OcrBackend::Auto,
        effective_backend: OcrBackend::Winrt,
        lang_hint: lang_hint.map(str::to_owned),
        synthetic: false,
        require_text: false,
    };
    ocr_result_or_empty(
        synapse_perception::read_text_from_bgra_bitmap(region, width, height, bgra),
        &request,
    )
}

fn text_region(
    state: &M1State,
    params: &ReadTextParams,
    target_hwnd: Option<i64>,
) -> Result<(Rect, ReadTextCaptureSource), ErrorData> {
    let target_hwnd = params.window_hwnd.or(target_hwnd);
    if let Some(region) = params.region {
        let capture_source = match target_hwnd {
            Some(hwnd) => target_window_region_capture_source(hwnd, region)?,
            None => ReadTextCaptureSource::Screen,
        };
        return Ok((region, capture_source));
    }
    if let Some(element_id) = &params.element_id {
        if state.synthetic.is_none() {
            let region = synapse_a11y::element_bounding_rect(element_id).map_err(|err| {
                mcp_error(
                    error_codes::OCR_NO_TEXT,
                    format!("element_id has no live visible OCR region: {err}"),
                )
            })?;
            return Ok((region, ReadTextCaptureSource::Screen));
        }
        let input = current_input(state, 2)?;
        let region = input
            .elements
            .iter()
            .find(|node| &node.element_id == element_id)
            .map(|node| node.bbox)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::OCR_NO_TEXT,
                    "element_id has no visible OCR region",
                )
            })?;
        return Ok((region, ReadTextCaptureSource::Screen));
    }

    let Some(hwnd) = target_hwnd else {
        let input = current_input(state, 2)?;
        let region = input.focused.map(|focused| focused.bbox).ok_or_else(|| {
            mcp_error(
                error_codes::OCR_NO_TEXT,
                "read_text requires region, element_id, or a focused element with a visible OCR region",
            )
        })?;
        return Ok((region, ReadTextCaptureSource::Screen));
    };

    fail_if_minimized_target_needs_window_capture(hwnd)?;
    Ok((
        Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        ReadTextCaptureSource::WholeWindow { hwnd },
    ))
}

fn validate_ocr_region(region: Rect) -> Result<(), ErrorData> {
    if region.w <= 0 || region.h <= 0 {
        return Err(mcp_error(
            error_codes::OCR_NO_TEXT,
            format!(
                "read_text OCR region must be non-empty: bbox=({}, {}, {}, {})",
                region.x, region.y, region.w, region.h
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn target_window_region_capture_source(
    hwnd: i64,
    client_region: Rect,
) -> Result<ReadTextCaptureSource, ErrorData> {
    synapse_capture::validate_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("read_text window_hwnd {hwnd:#x} is not a live window: {error}"),
        )
    })?;
    let window_region =
        synapse_capture::client_region_to_window_region(hwnd, client_region).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "read_text could not convert client-relative region {client_region:?} for hwnd {hwnd:#x} into target-window bitmap coordinates: {error}"
                ),
            )
        })?;
    Ok(ReadTextCaptureSource::Window {
        hwnd,
        window_region,
    })
}

#[cfg(not(windows))]
fn target_window_region_capture_source(
    _hwnd: i64,
    _window_region: Rect,
) -> Result<ReadTextCaptureSource, ErrorData> {
    Err(mcp_error(
        error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "read_text target-window OCR requires Windows window capture",
    ))
}

#[cfg(windows)]
fn fail_if_minimized_target_needs_window_capture(hwnd: i64) -> Result<(), ErrorData> {
    synapse_capture::validate_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("read_text window_hwnd {hwnd:#x} is not a live window: {error}"),
        )
    })?;
    if synapse_a11y::is_window_minimized(hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!("read_text could not determine minimized state for hwnd {hwnd:#x}: {error}"),
        )
    })? {
        return Err(mcp_error(
            error_codes::A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE,
            format!(
                "read_text target hwnd {hwnd:#x} is minimized and no explicit window-relative OCR region was supplied; whole-window WGC OCR requires a live non-minimized target window"
            ),
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn fail_if_minimized_target_needs_window_capture(_hwnd: i64) -> Result<(), ErrorData> {
    Err(mcp_error(
        error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "read_text target-window OCR requires Windows window capture",
    ))
}

pub fn effective_ocr_backend(backend: OcrBackend) -> Result<OcrBackend, ErrorData> {
    match backend {
        OcrBackend::Winrt | OcrBackend::Auto => Ok(OcrBackend::Winrt),
        OcrBackend::Crnn => Err(crnn_unavailable_error()),
    }
}

fn crnn_unavailable_error() -> ErrorData {
    mcp_error(
        error_codes::OCR_BACKEND_UNAVAILABLE,
        "CRNN OCR backend is declared in schema but no CRNN runtime/model provider is wired on this host; request backend=winrt or backend=auto",
    )
}

fn ocr_result_from_text_regions(
    regions: Vec<TextRegion>,
    request: &ResolvedReadTextRequest,
) -> OcrResult {
    let full_text = regions
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let no_text = regions.is_empty();
    let confidence = aggregate_confidence(&regions);
    let confidence_source = aggregate_confidence_source(&regions);
    OcrResult {
        full_text,
        words: regions
            .into_iter()
            .map(|word| OcrWord {
                confidence: word_confidence(&word.text, word.confidence, word.confidence_source),
                confidence_source: word_confidence_source(
                    &word.text,
                    word.confidence,
                    word.confidence_source,
                ),
                text: word.text,
                bbox: word.bbox,
            })
            .collect(),
        confidence,
        confidence_source,
        region: request.region,
        lang: request.lang(),
        no_text,
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    }
}

/// The empty-region success readback for #1557: a clean OCR pass that found no
/// glyphs is a valid observation (`no_text: true`), not a failure.
fn empty_ocr_result(request: &ResolvedReadTextRequest) -> OcrResult {
    OcrResult {
        full_text: String::new(),
        words: Vec::new(),
        confidence: 0.0,
        confidence_source: OcrConfidenceSource::Unsupported,
        region: request.region,
        lang: request.lang(),
        no_text: true,
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    }
}

/// The #1557 `require_text` fail-closed gate, applied to a FINAL `OcrResult`.
///
/// Kept OUT of [`ocr_result_or_empty`] on purpose: `read_text` caches the raw
/// empty observation in `CF_OCR_CACHE` (keyed on pixels+backend), so a gate
/// buried in the OCR pass is silently bypassed on a cache hit — the exact defect
/// manual FSV caught, where `require_text:true` returned a cached `no_text:true`
/// success. Enforcing absence here, on the final post-cache result, makes it
/// fire identically on cache hits and misses, independent of how the perception
/// layer represented "no glyphs" (`Err(OcrNoText)` vs `Ok(vec![])`). It also
/// avoids masking a legitimate blank as a hard error mid-pass, which would wrong-
/// foot the window-capture fallback in `read_text_with_target_hwnd`.
pub fn enforce_require_text(result: &OcrResult, require_text: bool) -> Result<(), ErrorData> {
    if require_text && result.no_text {
        return Err(mcp_error(
            error_codes::OCR_NO_TEXT,
            format!(
                "read_text found no glyphs in OCR region ({}, {}, {}, {}) and require_text was set, so absence is a hard OCR_NO_TEXT failure",
                result.region.x, result.region.y, result.region.w, result.region.h
            ),
        ));
    }
    Ok(())
}

/// Turns a raw OCR word result into an `OcrResult`, treating the perception
/// layer's "no glyphs" outcome as a valid empty observation (#1557) instead of a
/// hard error.
///
/// "No glyphs" has two equivalent representations at the perception boundary:
/// `Err(OcrNoText)` (some backends raise it) and `Ok(vec![])` (WinRT returns an
/// empty region vector for a clean blank capture). Both collapse to the same
/// empty observation, which is what gets cached. The `require_text` fail-closed
/// policy is applied separately by [`enforce_require_text`] on the final
/// post-cache result, so it is representation- and cache-agnostic. Every other
/// perception failure (backend unavailable, capture failed, …) stays a typed
/// hard error exactly as before.
fn ocr_result_or_empty(
    words: synapse_perception::PerceptionResult<Vec<TextRegion>>,
    request: &ResolvedReadTextRequest,
) -> Result<OcrResult, ErrorData> {
    match words {
        Ok(regions) => Ok(ocr_result_from_text_regions(regions, request)),
        Err(synapse_perception::PerceptionError::OcrNoText { .. }) => Ok(empty_ocr_result(request)),
        Err(err) => Err(mcp_error(err.code(), err.to_string())),
    }
}

fn aggregate_confidence(regions: &[TextRegion]) -> f32 {
    if regions.is_empty() {
        return 0.0;
    }
    let sum = regions
        .iter()
        .map(|word| word_confidence(&word.text, word.confidence, word.confidence_source))
        .sum::<f32>();
    let count = u16::try_from(regions.len()).unwrap_or(u16::MAX);
    sum / f32::from(count)
}

fn aggregate_confidence_source(regions: &[TextRegion]) -> OcrConfidenceSource {
    regions
        .iter()
        .map(|word| word_confidence_source(&word.text, word.confidence, word.confidence_source))
        .fold(None, |current, source| {
            Some(match current {
                None => source,
                Some(existing) => merge_confidence_sources(existing, source),
            })
        })
        .unwrap_or(OcrConfidenceSource::Unsupported)
}

fn word_confidence(text: &str, confidence: f32, source: TextRegionConfidenceSource) -> f32 {
    if matches!(source, TextRegionConfidenceSource::Unsupported) {
        return 0.0;
    }
    identifier_aware_confidence(text, confidence)
}

fn word_confidence_source(
    text: &str,
    confidence: f32,
    source: TextRegionConfidenceSource,
) -> OcrConfidenceSource {
    let source = ocr_confidence_source(source);
    if source == OcrConfidenceSource::Unsupported {
        return source;
    }
    if is_ambiguous_identifier_token(text)
        && normalize_confidence(confidence) > AMBIGUOUS_IDENTIFIER_CONFIDENCE_CAP
    {
        OcrConfidenceSource::Heuristic
    } else {
        source
    }
}

fn ocr_confidence_source(source: TextRegionConfidenceSource) -> OcrConfidenceSource {
    match source {
        TextRegionConfidenceSource::Engine => OcrConfidenceSource::Engine,
        TextRegionConfidenceSource::Uia => OcrConfidenceSource::Uia,
        TextRegionConfidenceSource::Synthetic => OcrConfidenceSource::Synthetic,
        TextRegionConfidenceSource::Heuristic => OcrConfidenceSource::Heuristic,
        TextRegionConfidenceSource::Unsupported => OcrConfidenceSource::Unsupported,
    }
}

fn merge_confidence_sources(
    left: OcrConfidenceSource,
    right: OcrConfidenceSource,
) -> OcrConfidenceSource {
    use OcrConfidenceSource::{Engine, Heuristic, Synthetic, Uia, Unsupported};
    if left == right {
        left
    } else if matches!(left, Heuristic) || matches!(right, Heuristic) {
        Heuristic
    } else if matches!(left, Unsupported) || matches!(right, Unsupported) {
        Unsupported
    } else if matches!(left, Uia) || matches!(right, Uia) {
        Uia
    } else if matches!(left, Synthetic) || matches!(right, Synthetic) {
        Synthetic
    } else {
        Engine
    }
}

fn identifier_aware_confidence(text: &str, confidence: f32) -> f32 {
    let confidence = normalize_confidence(confidence);
    if is_ambiguous_identifier_token(text) {
        confidence.min(AMBIGUOUS_IDENTIFIER_CONFIDENCE_CAP)
    } else {
        confidence
    }
}

const AMBIGUOUS_IDENTIFIER_CONFIDENCE_CAP: f32 = 0.74;

fn is_ambiguous_identifier_token(text: &str) -> bool {
    let token = text.trim_matches(|ch: char| !identifier_char(ch));
    if token.len() < 2 || !token.chars().any(ambiguous_identifier_char) {
        return false;
    }
    let has_digit = token.chars().any(|ch| ch.is_ascii_digit());
    let has_separator = token.chars().any(identifier_separator);
    if looks_like_ordinary_lowercase_word(token) && !has_digit && !has_separator {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    let short_all_ambiguous =
        token.chars().count() <= 4 && token.chars().all(ambiguous_identifier_char);
    let has_ambiguous_pair = [
        "v1", "vl", "vi", "i1", "l1", "1l", "1i", "o0", "0o", "ol", "lo",
    ]
    .iter()
    .any(|pair| lower.contains(pair));
    has_digit
        || has_separator
        || short_all_ambiguous
        || has_ambiguous_pair
        || lower == "vl"
        || lower == "v1"
}

fn looks_like_ordinary_lowercase_word(token: &str) -> bool {
    token.len() >= 3
        && token
            .chars()
            .all(|ch| ch.is_ascii_lowercase() && ch.is_ascii_alphabetic())
}

const fn identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '-' | '_' | '.' | ':' | '/' | '\\' | '|' | '[' | ']' | '(' | ')'
        )
}

const fn identifier_separator(ch: char) -> bool {
    matches!(ch, '-' | '_' | '.' | ':' | '/' | '\\' | '|')
}

const fn ambiguous_identifier_char(ch: char) -> bool {
    matches!(ch, '0' | 'O' | 'o' | '1' | 'I' | 'l' | '|' | 'v' | 'V')
}

const fn normalize_confidence(confidence: f32) -> f32 {
    if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

struct SyntheticOcrProvider {
    region: Rect,
}

impl synapse_perception::OcrProvider for SyntheticOcrProvider {
    fn read_text(&self, _region: Rect) -> synapse_perception::PerceptionResult<Vec<TextRegion>> {
        Ok(vec![TextRegion {
            text: "Synapse".to_owned(),
            bbox: Rect {
                x: self.region.x.saturating_add(4),
                y: self.region.y.saturating_add(4),
                w: 72,
                h: 18,
            },
            confidence: 0.99,
            confidence_source: TextRegionConfidenceSource::Synthetic,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AMBIGUOUS_IDENTIFIER_CONFIDENCE_CAP, ReadTextCaptureSource, ResolvedReadTextRequest,
        enforce_require_text, identifier_aware_confidence, is_ambiguous_identifier_token,
        ocr_result_from_text_regions, ocr_result_or_empty,
    };
    use synapse_core::{OcrBackend, OcrConfidenceSource, Rect, error_codes};
    use synapse_perception::TextRegionConfidenceSource;

    #[test]
    fn caps_short_tokens_collapsed_to_only_ambiguous_glyphs() {
        for token in ["II", "OO", "ll", "00", "v1", "vl", "AMBIG724v1"] {
            assert!(
                is_ambiguous_identifier_token(token),
                "{token} should be ambiguity-capped"
            );
            assert_eq!(
                identifier_aware_confidence(token, 1.0),
                AMBIGUOUS_IDENTIFIER_CONFIDENCE_CAP
            );
        }
    }

    #[test]
    fn does_not_cap_ordinary_words_containing_some_ambiguous_letters() {
        assert!(!is_ambiguous_identifier_token("look"));
        assert_eq!(identifier_aware_confidence("look", 0.96), 0.96);
    }

    #[test]
    fn unsupported_ocr_confidence_is_not_reported_as_measured() {
        let request = ResolvedReadTextRequest {
            region: Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 40,
            },
            capture_source: ReadTextCaptureSource::Screen,
            requested_backend: OcrBackend::Auto,
            effective_backend: OcrBackend::Winrt,
            lang_hint: None,
            synthetic: false,
            require_text: false,
        };
        let result = ocr_result_from_text_regions(
            vec![synapse_perception::TextRegion {
                text: "Synapse".to_owned(),
                bbox: request.region,
                confidence: 1.0,
                confidence_source: TextRegionConfidenceSource::Unsupported,
            }],
            &request,
        );

        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.confidence_source, OcrConfidenceSource::Unsupported);
        assert_eq!(result.words[0].confidence, 0.0);
        assert_eq!(
            result.words[0].confidence_source,
            OcrConfidenceSource::Unsupported
        );
    }

    fn empty_ocr_test_request(require_text: bool) -> ResolvedReadTextRequest {
        ResolvedReadTextRequest {
            region: Rect {
                x: 410,
                y: 74,
                w: 1178,
                h: 746,
            },
            capture_source: ReadTextCaptureSource::Screen,
            requested_backend: OcrBackend::Winrt,
            effective_backend: OcrBackend::Winrt,
            lang_hint: None,
            synthetic: false,
            require_text,
        }
    }

    #[test]
    fn ocr_no_text_is_empty_observation_by_default() {
        // #1557: a clean OCR pass over a valid region that finds zero glyphs is a
        // valid empty observation (no_text:true), not an OCR_NO_TEXT failure.
        let request = empty_ocr_test_request(false);
        let result = ocr_result_or_empty(
            Err(synapse_perception::PerceptionError::OcrNoText {
                region: request.region,
            }),
            &request,
        )
        .expect("empty region must be a success observation, not a failure");
        assert!(result.no_text);
        assert!(result.full_text.is_empty());
        assert!(result.words.is_empty());
        assert_eq!(result.region, request.region);
        println!(
            "readback=read_text no_text={} full_text={:?} words={} region={:?}",
            result.no_text,
            result.full_text,
            result.words.len(),
            result.region
        );
    }

    #[test]
    fn ocr_no_text_stays_error_when_require_text_pinned() {
        // The opt-in fail-closed path is preserved for callers that pin absence.
        // The empty observation is Ok (and cacheable); the require_text gate is
        // applied separately on the final result via enforce_require_text.
        let request = empty_ocr_test_request(true);
        let result = ocr_result_or_empty(
            Err(synapse_perception::PerceptionError::OcrNoText {
                region: request.region,
            }),
            &request,
        )
        .expect("an empty observation is Ok; the fail-closed gate runs post-cache");
        assert!(result.no_text);
        let error = enforce_require_text(&result, true)
            .expect_err("require_text must keep fail-closed OCR_NO_TEXT");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&serde_json::json!(error_codes::OCR_NO_TEXT))
        );
    }

    #[test]
    fn ocr_backend_failure_is_never_masked_as_empty() {
        // Backend/capture failures stay hard errors; only "no glyphs" is empty.
        let request = empty_ocr_test_request(false);
        let error = ocr_result_or_empty(
            Err(synapse_perception::PerceptionError::OcrBackendUnavailable {
                detail: "winrt engine missing".to_owned(),
            }),
            &request,
        )
        .expect_err("backend failures must not be masked as an empty observation");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&serde_json::json!(error_codes::OCR_BACKEND_UNAVAILABLE))
        );
    }

    #[test]
    fn ocr_empty_regions_vec_respects_require_text() {
        // Manual FSV #1557 regression: WinRT signals "no glyphs" as Ok(vec![]) rather
        // than Err(OcrNoText), and read_text caches that empty observation. The
        // require_text gate runs on the final (post-cache) result, so it fires on
        // this representation AND on a cache hit — the exact bug manual FSV caught where
        // require_text:true returned a cached no_text:true success.
        let result = ocr_result_or_empty(Ok(Vec::new()), &empty_ocr_test_request(false))
            .expect("an empty Ok vec is a valid empty observation");
        assert!(result.no_text, "empty Ok vec must set no_text");
        assert!(result.words.is_empty());
        assert!(result.full_text.is_empty());

        enforce_require_text(&result, false)
            .expect("default require_text=false keeps the empty observation as success");
        let error = enforce_require_text(&result, true)
            .expect_err("require_text=true must fail-closed on an empty observation");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&serde_json::json!(error_codes::OCR_NO_TEXT))
        );
        println!(
            "readback=read_text empty_observation no_text={} gate(false)=ok gate(true)=OCR_NO_TEXT",
            result.no_text
        );
    }

    #[cfg(windows)]
    #[test]
    fn whole_window_request_uses_captured_bitmap_extent() {
        let request = ResolvedReadTextRequest {
            region: Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            capture_source: ReadTextCaptureSource::WholeWindow { hwnd: 0x1234 },
            requested_backend: OcrBackend::Auto,
            effective_backend: OcrBackend::Winrt,
            lang_hint: None,
            synthetic: false,
            require_text: false,
        };
        let captured = synapse_capture::CapturedBgraBitmap {
            region: Rect {
                x: 0,
                y: 0,
                w: 3031,
                h: 1829,
            },
            width: 3031,
            height: 1829,
            bytes: Vec::new(),
        };

        let resolved = super::read_text_request_for_captured_bitmap(request, &captured)
            .expect("captured extent is valid");

        assert_eq!(
            resolved.region,
            Rect {
                x: 0,
                y: 0,
                w: 3031,
                h: 1829,
            }
        );
    }
}
