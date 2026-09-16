use std::time::Instant;

use image::GrayImage;
use regex::Regex;
use synapse_core::{HudExtractor, HudFieldSpec, HudParser, HudReading, HudValue, Rect};

use crate::{
    HudTemplate, OcrProvider, PerceptionError, PerceptionResult, TemplateCounterConfig,
    TemplateCounterReading, TextRegion, extract_template_counter_from_region,
    read_text_with_provider,
};

const NUMBER_PATTERN: &str = r"[-+]?(?:\d+(?:\.\d*)?|\.\d+)";
const FRACTION_PATTERN: &str =
    r"(?P<num>[-+]?(?:\d+(?:\.\d*)?|\.\d+))\s*/\s*(?P<den>[-+]?(?:\d+(?:\.\d*)?|\.\d+))";

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExtractionSource {
    TemplateMatch,
    Ocr,
    OcrFallback,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldExtraction {
    pub field_name: String,
    pub reading: HudReading,
    pub source: ExtractionSource,
    pub template_reading: Option<TemplateCounterReading>,
    pub ocr_text: Option<String>,
    pub elapsed_ms: f64,
}

pub struct FieldExtractionRequest<'a> {
    pub field: &'a HudFieldSpec,
    pub screen_region: Rect,
    pub region_image: &'a GrayImage,
    pub templates: &'a [HudTemplate],
    pub ocr_provider: &'a dyn OcrProvider,
    pub stale_ms: u32,
}

/// Extracts one profile HUD field from a cropped image and optional OCR provider.
///
/// Template-match HUD fields first run the slotted template counter with a
/// permissive matcher, then compare the aggregate confidence against the
/// field's `confidence_threshold`. Low-confidence template readings fall back
/// to OCR over the same screen region and parse the resulting text with the
/// field parser.
///
/// # Errors
///
/// Returns `HUD_EXTRACTION_FAILED` for invalid thresholds, unsupported
/// extractors, unparseable OCR text, low-confidence OCR output, or when both
/// template extraction and OCR fallback fail.
pub fn extract_field(request: &FieldExtractionRequest<'_>) -> PerceptionResult<FieldExtraction> {
    let started = Instant::now();
    validate_confidence_threshold(request.field.confidence_threshold)?;

    let extraction = match &request.field.extractor {
        HudExtractor::TemplateMatch { .. } => extract_template_field(request, started),
        HudExtractor::WinrtOcr => extract_ocr_field(request, started, ExtractionSource::Ocr),
        HudExtractor::Crnn { model_id } => Err(hud_error(format!(
            "HUD extractor crnn model {model_id:?} is not wired for field {:?}",
            request.field.name
        ))),
        HudExtractor::ColorRatio { .. } => Err(hud_error(format!(
            "HUD extractor color_ratio is not handled by the OCR fallback extractor for field {:?}",
            request.field.name
        ))),
    }?;

    Ok(extraction)
}

/// Parses raw HUD OCR text according to a profile HUD parser.
///
/// # Errors
///
/// Returns `HUD_EXTRACTION_FAILED` when the parser cannot produce a value from
/// the supplied text or when a profile-provided regex is invalid.
pub fn parse_hud_text(parser: &HudParser, raw_text: &str) -> PerceptionResult<HudValue> {
    let text = raw_text.trim();
    if text.is_empty() {
        return Err(hud_error("HUD OCR text was empty after trimming"));
    }

    match parser {
        HudParser::Number => parse_number(text).map(HudValue::Number),
        HudParser::BoundedInteger { min, max, .. } => {
            parse_bounded_integer(text, *min, *max).map(HudValue::Number)
        }
        HudParser::FractionNumerator => parse_fraction_part(text, "num").map(HudValue::Number),
        HudParser::FractionDenominator => parse_fraction_part(text, "den").map(HudValue::Number),
        HudParser::Regex { pattern, group } => parse_regex_group(pattern, *group, text),
        HudParser::Enum { mapping } => mapping
            .get(text)
            .cloned()
            .map(HudValue::Enum)
            .ok_or_else(|| hud_error(format!("HUD enum parser found no mapping for {text:?}"))),
    }
}

fn extract_template_field(
    request: &FieldExtractionRequest<'_>,
    started: Instant,
) -> PerceptionResult<FieldExtraction> {
    let candidate_config = TemplateCounterConfig {
        min_confidence: 0.0,
        ..TemplateCounterConfig::default()
    };

    match extract_template_counter_from_region(
        request.region_image,
        request.templates,
        candidate_config,
    ) {
        Ok(reading) if reading.confidence >= f64::from(request.field.confidence_threshold) => {
            let confidence = confidence_to_f32(reading.confidence);
            Ok(FieldExtraction {
                field_name: request.field.name.clone(),
                reading: HudReading {
                    raw_text: reading.value.to_string(),
                    parsed: HudValue::Number(f64::from(reading.value)),
                    confidence,
                    stale_ms: request.stale_ms,
                },
                source: ExtractionSource::TemplateMatch,
                template_reading: Some(reading),
                ocr_text: None,
                elapsed_ms: elapsed_ms(started),
            })
        }
        Ok(reading) => {
            let fallback = ocr_reading(request, ExtractionSource::OcrFallback)?;
            Ok(FieldExtraction {
                field_name: request.field.name.clone(),
                reading: fallback.reading,
                source: ExtractionSource::OcrFallback,
                template_reading: Some(reading),
                ocr_text: Some(fallback.ocr_text),
                elapsed_ms: elapsed_ms(started),
            })
        }
        Err(template_error) => {
            let fallback = ocr_reading(request, ExtractionSource::OcrFallback).map_err(|ocr_error| {
                hud_error(format!(
                    "template extraction failed ({template_error}); OCR fallback also failed ({ocr_error})"
                ))
            })?;
            Ok(FieldExtraction {
                field_name: request.field.name.clone(),
                reading: fallback.reading,
                source: ExtractionSource::OcrFallback,
                template_reading: None,
                ocr_text: Some(fallback.ocr_text),
                elapsed_ms: elapsed_ms(started),
            })
        }
    }
}

fn extract_ocr_field(
    request: &FieldExtractionRequest<'_>,
    started: Instant,
    source: ExtractionSource,
) -> PerceptionResult<FieldExtraction> {
    let fallback = ocr_reading(request, source)?;
    Ok(FieldExtraction {
        field_name: request.field.name.clone(),
        reading: fallback.reading,
        source,
        template_reading: None,
        ocr_text: Some(fallback.ocr_text),
        elapsed_ms: elapsed_ms(started),
    })
}

struct OcrReading {
    reading: HudReading,
    ocr_text: String,
}

fn ocr_reading(
    request: &FieldExtractionRequest<'_>,
    source: ExtractionSource,
) -> PerceptionResult<OcrReading> {
    let regions = match read_text_with_provider(request.ocr_provider, request.screen_region) {
        Ok(regions) => regions,
        Err(PerceptionError::OcrNoText { .. }) => {
            return default_ocr_reading_for_no_text(request);
        }
        Err(error) => {
            return Err(hud_error(format!(
                "HUD OCR provider failed for field {:?}: {error}",
                request.field.name
            )));
        }
    };
    let text = match joined_text(&regions) {
        Ok(text) => text,
        Err(_error) => return default_ocr_reading_for_no_text(request),
    };
    let confidence = min_word_confidence(&regions);
    if confidence < request.field.confidence_threshold {
        return Err(hud_error(format!(
            "HUD OCR confidence {confidence:.3} below threshold {:.3} for field {:?}",
            request.field.confidence_threshold, request.field.name
        )));
    }

    let parsed = parse_hud_text(&request.field.parser, &text)?;
    let raw_text = match source {
        ExtractionSource::Ocr | ExtractionSource::OcrFallback => text.clone(),
        ExtractionSource::TemplateMatch => unreachable!("template extraction does not call OCR"),
    };
    Ok(OcrReading {
        reading: HudReading {
            raw_text,
            parsed,
            confidence,
            stale_ms: request.stale_ms,
        },
        ocr_text: text,
    })
}

fn default_ocr_reading_for_no_text(
    request: &FieldExtractionRequest<'_>,
) -> PerceptionResult<OcrReading> {
    let Some(parsed) = parser_no_text_default(&request.field.parser)? else {
        return Err(hud_error(format!(
            "HUD OCR returned no text for field {:?}",
            request.field.name
        )));
    };

    Ok(OcrReading {
        reading: HudReading {
            raw_text: String::new(),
            parsed,
            confidence: 0.0,
            stale_ms: request.stale_ms,
        },
        ocr_text: String::new(),
    })
}

fn joined_text(regions: &[TextRegion]) -> PerceptionResult<String> {
    let text = regions
        .iter()
        .map(|region| region.text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        Err(hud_error("HUD OCR returned only empty text regions"))
    } else {
        Ok(text)
    }
}

fn min_word_confidence(regions: &[TextRegion]) -> f32 {
    regions
        .iter()
        .map(|region| {
            if region.confidence.is_finite() {
                region.confidence.clamp(0.0, 1.0)
            } else {
                0.0
            }
        })
        .fold(1.0_f32, f32::min)
}

fn parse_number(text: &str) -> PerceptionResult<f64> {
    let number_regex = Regex::new(NUMBER_PATTERN)
        .map_err(|err| hud_error(format!("internal HUD number regex is invalid: {err}")))?;
    let value = number_regex
        .find(text)
        .ok_or_else(|| hud_error(format!("HUD number parser found no number in {text:?}")))?
        .as_str();
    value.parse::<f64>().map_err(|err| {
        hud_error(format!(
            "HUD number parser could not parse {value:?}: {err}"
        ))
    })
}

fn parse_bounded_integer(text: &str, min: u32, max: u32) -> PerceptionResult<f64> {
    validate_bounded_integer_range(min, max)?;
    let value = parse_number(text)?;
    if !value.is_finite() || value.fract() != 0.0 {
        return Err(hud_error(format!(
            "HUD bounded_integer parser expected an integer in {min}..={max}, got {value}"
        )));
    }
    if value < 0.0 || value > f64::from(u32::MAX) {
        return Err(hud_error(format!(
            "HUD bounded_integer parser value {value} is outside u32 range"
        )));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let integer = value as u32;
    if !(min..=max).contains(&integer) {
        return Err(hud_error(format!(
            "HUD bounded_integer parser value {integer} outside {min}..={max}"
        )));
    }
    Ok(f64::from(integer))
}

fn parser_no_text_default(parser: &HudParser) -> PerceptionResult<Option<HudValue>> {
    match parser {
        HudParser::BoundedInteger {
            min,
            max,
            default_on_no_text: Some(default),
        } => {
            validate_bounded_integer_range(*min, *max)?;
            if !(*min..=*max).contains(default) {
                return Err(hud_error(format!(
                    "HUD bounded_integer no-text default {default} outside {min}..={max}"
                )));
            }
            Ok(Some(HudValue::Number(f64::from(*default))))
        }
        _ => Ok(None),
    }
}

fn validate_bounded_integer_range(min: u32, max: u32) -> PerceptionResult<()> {
    if min > max {
        return Err(hud_error(format!(
            "HUD bounded_integer parser min {min} exceeds max {max}"
        )));
    }
    Ok(())
}

fn parse_fraction_part(text: &str, part: &'static str) -> PerceptionResult<f64> {
    let fraction_regex = Regex::new(FRACTION_PATTERN)
        .map_err(|err| hud_error(format!("internal HUD fraction regex is invalid: {err}")))?;
    let captures = fraction_regex
        .captures(text)
        .ok_or_else(|| hud_error(format!("HUD fraction parser found no fraction in {text:?}")))?;
    let value = captures
        .name(part)
        .ok_or_else(|| hud_error(format!("HUD fraction parser found no {part} group")))?
        .as_str();
    value.parse::<f64>().map_err(|err| {
        hud_error(format!(
            "HUD fraction parser could not parse {value:?}: {err}"
        ))
    })
}

fn parse_regex_group(pattern: &str, group: u32, text: &str) -> PerceptionResult<HudValue> {
    let regex = Regex::new(pattern).map_err(|err| {
        hud_error(format!(
            "HUD regex parser pattern {pattern:?} is invalid: {err}"
        ))
    })?;
    let captures = regex
        .captures(text)
        .ok_or_else(|| hud_error(format!("HUD regex parser found no match in {text:?}")))?;
    let group_index = usize::try_from(group)
        .map_err(|_err| hud_error(format!("HUD regex group {group} does not fit usize")))?;
    let value = captures
        .get(group_index)
        .ok_or_else(|| hud_error(format!("HUD regex parser found no group {group}")))?
        .as_str()
        .trim();
    if value.is_empty() {
        return Err(hud_error(format!("HUD regex group {group} was empty")));
    }

    match value.parse::<f64>() {
        Ok(number) => Ok(HudValue::Number(number)),
        Err(_err) => Ok(HudValue::Text(value.to_owned())),
    }
}

fn validate_confidence_threshold(confidence_threshold: f32) -> PerceptionResult<()> {
    if !confidence_threshold.is_finite() || !(0.0..=1.0).contains(&confidence_threshold) {
        return Err(hud_error(format!(
            "HUD confidence_threshold must be finite and in 0..=1, got {confidence_threshold}"
        )));
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
const fn confidence_to_f32(confidence: f64) -> f32 {
    confidence.clamp(0.0, 1.0) as f32
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn hud_error(detail: impl Into<String>) -> PerceptionError {
    PerceptionError::HudExtractionFailed {
        detail: detail.into(),
    }
}
