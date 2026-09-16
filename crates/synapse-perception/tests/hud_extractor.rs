use std::{
    cell::Cell,
    error::Error,
    io::{self, Write},
};

use image::{GrayImage, Luma};
use synapse_core::{
    HudExtractor, HudFieldSpec, HudParser, HudRegion, HudValue, Rect,
    default_hud_confidence_threshold, error_codes,
};
use synapse_perception::{
    ExtractionSource, FieldExtractionRequest, HudTemplate, OcrProvider, PerceptionResult,
    TemplateCounterConfig, TextRegion, extract_field, extract_template_counter_from_region,
};

type TestResult = Result<(), Box<dyn Error>>;

fn regression_log(args: std::fmt::Arguments<'_>) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_fmt(args)?;
    stdout.write_all(b"\n")
}

#[test]
fn extractor_accepts_high_confidence_template_without_ocr() -> TestResult {
    let templates = status_templates()?;
    let region = synthetic_region(&[2, 2, 2, 2, 2, 0, 0, 0, 0, 0]);
    let field = template_field(HudParser::Number, default_hud_confidence_threshold());
    let provider = FixedOcrProvider::new(vec![word("999", 0.99)]);

    regression_log(format_args!(
        "regression_check=hud_extractor edge=template_accept before=threshold:{:.2} ocr_calls:{}",
        field.confidence_threshold,
        provider.calls()
    ))?;
    let extraction = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: screen_region(),
        region_image: &region,
        templates: &templates,
        ocr_provider: &provider,
        stale_ms: 7,
    })?;
    regression_log(format_args!(
        "regression_check=hud_extractor edge=template_accept after=source:{:?} raw:{} parsed:{:?} confidence:{:.3} ocr_calls:{}",
        extraction.source,
        extraction.reading.raw_text,
        extraction.reading.parsed,
        extraction.reading.confidence,
        provider.calls()
    ))?;

    assert_eq!(extraction.source, ExtractionSource::TemplateMatch);
    assert_eq!(extraction.reading.parsed, HudValue::Number(10.0));
    assert_eq!(extraction.reading.raw_text, "10");
    assert_eq!(extraction.reading.stale_ms, 7);
    assert_eq!(provider.calls(), 0);
    Ok(())
}

#[test]
fn extractor_falls_back_to_ocr_when_template_confidence_is_below_threshold() -> TestResult {
    let templates = status_templates()?;
    let region = degraded_region();
    let field = template_field(HudParser::Number, default_hud_confidence_threshold());
    let provider = FixedOcrProvider::new(vec![word("6", 0.99)]);
    let candidate = extract_template_counter_from_region(
        &region,
        &templates,
        TemplateCounterConfig {
            min_confidence: 0.0,
            ..TemplateCounterConfig::default()
        },
    )?;

    regression_log(format_args!(
        "regression_check=hud_extractor edge=ocr_fallback before=template_confidence:{:.3} threshold:{:.3} ocr_calls:{}",
        candidate.confidence,
        field.confidence_threshold,
        provider.calls()
    ))?;
    assert!(candidate.confidence < f64::from(field.confidence_threshold));

    let extraction = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: screen_region(),
        region_image: &region,
        templates: &templates,
        ocr_provider: &provider,
        stale_ms: 0,
    })?;
    regression_log(format_args!(
        "regression_check=hud_extractor edge=ocr_fallback after=source:{:?} raw:{} parsed:{:?} template_confidence:{:.3} ocr_calls:{} last_region:{:?}",
        extraction.source,
        extraction.reading.raw_text,
        extraction.reading.parsed,
        extraction
            .template_reading
            .as_ref()
            .map_or(-1.0, |reading| reading.confidence),
        provider.calls(),
        provider.last_region()
    ))?;

    assert_eq!(extraction.source, ExtractionSource::OcrFallback);
    assert_eq!(extraction.reading.parsed, HudValue::Number(6.0));
    assert_eq!(extraction.reading.raw_text, "6");
    assert_eq!(provider.calls(), 1);
    assert_eq!(provider.last_region(), Some(screen_region()));
    Ok(())
}

#[test]
fn extractor_fails_closed_when_ocr_fallback_has_no_digits() -> TestResult {
    let templates = status_templates()?;
    let region = degraded_region();
    let field = template_field(HudParser::Number, default_hud_confidence_threshold());
    let provider = FixedOcrProvider::new(vec![word("HP", 0.99)]);

    regression_log(format_args!(
        "regression_check=hud_extractor edge=ocr_no_digits before=threshold:{:.3} provider_text=HP",
        field.confidence_threshold
    ))?;
    let result = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: screen_region(),
        region_image: &region,
        templates: &templates,
        ocr_provider: &provider,
        stale_ms: 0,
    });
    regression_log(format_args!(
        "regression_check=hud_extractor edge=ocr_no_digits after={result:?} ocr_calls:{}",
        provider.calls()
    ))?;

    assert_eq!(
        result.err().map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );
    assert_eq!(provider.calls(), 1);
    Ok(())
}

#[test]
fn extractor_fails_closed_for_invalid_threshold_and_empty_ocr() -> TestResult {
    let templates = status_templates()?;
    let region = synthetic_region(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let invalid_field = template_field(HudParser::Number, 1.01);
    let provider = FixedOcrProvider::new(vec![word("2", 0.99)]);

    regression_log(format_args!(
        "regression_check=hud_extractor edge=invalid_threshold before=threshold:{:.2} ocr_calls:{}",
        invalid_field.confidence_threshold,
        provider.calls()
    ))?;
    let invalid = extract_field(&FieldExtractionRequest {
        field: &invalid_field,
        screen_region: screen_region(),
        region_image: &region,
        templates: &templates,
        ocr_provider: &provider,
        stale_ms: 0,
    });
    regression_log(format_args!(
        "regression_check=hud_extractor edge=invalid_threshold after={invalid:?} ocr_calls:{}",
        provider.calls()
    ))?;
    assert_eq!(
        invalid.err().map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );
    assert_eq!(provider.calls(), 0);

    let ocr_field = ocr_field(HudParser::Number, default_hud_confidence_threshold());
    let empty_provider = FixedOcrProvider::new(Vec::new());
    regression_log(format_args!(
        "regression_check=hud_extractor edge=empty_ocr before=words:0 threshold:{:.2}",
        ocr_field.confidence_threshold
    ))?;
    let empty = extract_field(&FieldExtractionRequest {
        field: &ocr_field,
        screen_region: screen_region(),
        region_image: &region,
        templates: &[],
        ocr_provider: &empty_provider,
        stale_ms: 0,
    });
    regression_log(format_args!(
        "regression_check=hud_extractor edge=empty_ocr after={empty:?} ocr_calls:{}",
        empty_provider.calls()
    ))?;
    assert_eq!(
        empty.err().map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );
    assert_eq!(empty_provider.calls(), 1);
    Ok(())
}

#[test]
fn extractor_reads_bounded_integer_xp_and_defaults_no_text_to_zero() -> TestResult {
    let field = xp_ocr_field();
    let region = GrayImage::from_pixel(64, 16, Luma([0]));
    let provider = FixedOcrProvider::new(vec![word("5", 0.99)]);

    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_level before=region:{:?} provider_text=5 provider_confidence=0.99",
        xp_screen_region()
    ))?;
    let extraction = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: xp_screen_region(),
        region_image: &region,
        templates: &[],
        ocr_provider: &provider,
        stale_ms: 11,
    })?;
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_level after=source:{:?} raw:{:?} parsed:{:?} confidence:{:.3} ocr_calls:{}",
        extraction.source,
        extraction.reading.raw_text,
        extraction.reading.parsed,
        extraction.reading.confidence,
        provider.calls()
    ))?;

    assert_eq!(extraction.source, ExtractionSource::Ocr);
    assert_eq!(extraction.reading.parsed, HudValue::Number(5.0));
    assert_eq!(extraction.reading.raw_text, "5");
    assert!((extraction.reading.confidence - 0.99).abs() <= f32::EPSILON);
    assert_eq!(extraction.reading.stale_ms, 11);
    assert_eq!(provider.calls(), 1);
    assert_eq!(provider.last_region(), Some(xp_screen_region()));

    let empty_provider = FixedOcrProvider::new(Vec::new());
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_no_text before=words:0 default_on_no_text=0"
    ))?;
    let empty = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: xp_screen_region(),
        region_image: &region,
        templates: &[],
        ocr_provider: &empty_provider,
        stale_ms: 0,
    })?;
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_no_text after=source:{:?} raw:{:?} parsed:{:?} confidence:{:.3} ocr_calls:{}",
        empty.source,
        empty.reading.raw_text,
        empty.reading.parsed,
        empty.reading.confidence,
        empty_provider.calls()
    ))?;

    assert_eq!(empty.source, ExtractionSource::Ocr);
    assert_eq!(empty.reading.parsed, HudValue::Number(0.0));
    assert_eq!(empty.reading.raw_text, "");
    assert!(empty.reading.confidence <= f32::EPSILON);
    assert_eq!(empty_provider.calls(), 1);
    assert_eq!(empty_provider.last_region(), Some(xp_screen_region()));
    Ok(())
}

#[test]
fn extractor_rejects_bounded_integer_xp_edges() -> TestResult {
    let field = xp_ocr_field();
    let region = GrayImage::from_pixel(64, 16, Luma([0]));

    let out_of_range_provider = FixedOcrProvider::new(vec![word("31", 0.99)]);
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_out_of_range before=provider_text=31 allowed=0..30"
    ))?;
    let out_of_range = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: xp_screen_region(),
        region_image: &region,
        templates: &[],
        ocr_provider: &out_of_range_provider,
        stale_ms: 0,
    });
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_out_of_range after={out_of_range:?} ocr_calls:{}",
        out_of_range_provider.calls()
    ))?;
    assert_eq!(
        out_of_range.err().map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );

    let fractional_provider = FixedOcrProvider::new(vec![word("5.5", 0.99)]);
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_fractional before=provider_text=5.5 allowed=integer"
    ))?;
    let fractional = extract_field(&FieldExtractionRequest {
        field: &field,
        screen_region: xp_screen_region(),
        region_image: &region,
        templates: &[],
        ocr_provider: &fractional_provider,
        stale_ms: 0,
    });
    regression_log(format_args!(
        "regression_check=hud_extractor edge=xp_fractional after={fractional:?} ocr_calls:{}",
        fractional_provider.calls()
    ))?;
    assert_eq!(
        fractional.err().map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );
    Ok(())
}

#[test]
fn extractor_fallback_path_p99_is_under_30ms_with_synthetic_provider() -> TestResult {
    let templates = status_templates()?;
    let region = degraded_region();
    let field = template_field(HudParser::Number, default_hud_confidence_threshold());
    let provider = FixedOcrProvider::new(vec![word("6", 0.99)]);
    let mut durations = Vec::with_capacity(64);

    regression_log(format_args!(
        "regression_check=hud_extractor edge=fallback_p99 before=samples:{} threshold:{:.3}",
        durations.capacity(),
        field.confidence_threshold
    ))?;
    for _sample in 0..durations.capacity() {
        let extraction = extract_field(&FieldExtractionRequest {
            field: &field,
            screen_region: screen_region(),
            region_image: &region,
            templates: &templates,
            ocr_provider: &provider,
            stale_ms: 0,
        })?;
        assert_eq!(extraction.source, ExtractionSource::OcrFallback);
        durations.push(extraction.elapsed_ms);
    }
    durations.sort_by(f64::total_cmp);
    let p99_index = durations
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    let p99 = durations
        .get(p99_index)
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing p99 sample"))?;
    regression_log(format_args!(
        "regression_check=hud_extractor edge=fallback_p99 after=p99_ms:{p99:.3} samples:{} ocr_calls:{}",
        durations.len(),
        provider.calls()
    ))?;

    assert!(p99 <= 30.0, "synthetic fallback p99 was {p99:.3} ms");
    Ok(())
}

#[derive(Debug)]
struct FixedOcrProvider {
    calls: Cell<u32>,
    last_region: Cell<Option<Rect>>,
    words: Vec<TextRegion>,
}

impl FixedOcrProvider {
    const fn new(words: Vec<TextRegion>) -> Self {
        Self {
            calls: Cell::new(0),
            last_region: Cell::new(None),
            words,
        }
    }

    const fn calls(&self) -> u32 {
        self.calls.get()
    }

    const fn last_region(&self) -> Option<Rect> {
        self.last_region.get()
    }
}

impl OcrProvider for FixedOcrProvider {
    fn read_text(&self, region: Rect) -> PerceptionResult<Vec<TextRegion>> {
        self.calls.set(self.calls.get().saturating_add(1));
        self.last_region.set(Some(region));
        Ok(self.words.clone())
    }
}

fn template_field(parser: HudParser, confidence_threshold: f32) -> HudFieldSpec {
    HudFieldSpec {
        name: "minecraft.hp_hearts".to_owned(),
        region: HudRegion::Absolute {
            x: 100,
            y: 200,
            w: 180,
            h: 16,
        },
        extractor: HudExtractor::TemplateMatch {
            templates: vec![
                "hearts/full.png".to_owned(),
                "hearts/half.png".to_owned(),
                "hearts/empty.png".to_owned(),
            ],
        },
        parser,
        confidence_threshold,
    }
}

fn ocr_field(parser: HudParser, confidence_threshold: f32) -> HudFieldSpec {
    HudFieldSpec {
        extractor: HudExtractor::WinrtOcr,
        ..template_field(parser, confidence_threshold)
    }
}

fn xp_ocr_field() -> HudFieldSpec {
    HudFieldSpec {
        name: "minecraft.xp_level".to_owned(),
        region: HudRegion::Absolute {
            x: xp_screen_region().x,
            y: xp_screen_region().y,
            w: xp_screen_region().w,
            h: xp_screen_region().h,
        },
        extractor: HudExtractor::WinrtOcr,
        parser: HudParser::BoundedInteger {
            min: 0,
            max: 30,
            default_on_no_text: Some(0),
        },
        confidence_threshold: default_hud_confidence_threshold(),
    }
}

fn word(text: &str, confidence: f32) -> TextRegion {
    TextRegion {
        text: text.to_owned(),
        bbox: screen_region(),
        confidence,
        confidence_source: synapse_perception::TextRegionConfidenceSource::Engine,
    }
}

const fn screen_region() -> Rect {
    Rect {
        x: 100,
        y: 200,
        w: 180,
        h: 16,
    }
}

const fn xp_screen_region() -> Rect {
    Rect {
        x: 128,
        y: 664,
        w: 64,
        h: 16,
    }
}

fn status_templates() -> PerceptionResult<Vec<HudTemplate>> {
    Ok(vec![
        HudTemplate::from_gray("full", 2, full_template())?,
        HudTemplate::from_gray("half", 1, half_template())?,
        HudTemplate::from_gray("empty", 0, empty_template())?,
    ])
}

fn synthetic_region(values: &[u32; 10]) -> GrayImage {
    let full = full_template();
    let half = half_template();
    let empty = empty_template();
    let mut region = GrayImage::from_pixel(180, 16, Luma([8]));
    for (index, value) in values.iter().enumerate() {
        let slot_x = u32::try_from(index).map_or(0, |item| item.saturating_mul(18));
        let template = match value {
            2 => &full,
            1 => &half,
            _ => &empty,
        };
        blit(&mut region, template, slot_x.saturating_add(4), 3);
    }
    region
}

fn degraded_region() -> GrayImage {
    let degraded = degraded_full_template();
    let mut region = GrayImage::from_pixel(180, 16, Luma([8]));
    for index in 0_u32..10 {
        blit(
            &mut region,
            &degraded,
            index.saturating_mul(18).saturating_add(4),
            3,
        );
    }
    region
}

fn full_template() -> GrayImage {
    GrayImage::from_fn(9, 9, |x, y| {
        if heart_fill(x, y) {
            Luma([230])
        } else if heart_outline(x, y) {
            Luma([120])
        } else {
            Luma([24])
        }
    })
}

fn degraded_full_template() -> GrayImage {
    let mut template = full_template();
    for y in 0..template.height() {
        for x in 0..template.width() {
            if x >= 4 || y >= 5 {
                template.put_pixel(x, y, Luma([32]));
            }
        }
    }
    template
}

fn half_template() -> GrayImage {
    GrayImage::from_fn(9, 9, |x, y| {
        if heart_fill(x, y) && x <= 4 {
            Luma([230])
        } else if heart_outline(x, y) {
            Luma([120])
        } else {
            Luma([24])
        }
    })
}

fn empty_template() -> GrayImage {
    GrayImage::from_fn(9, 9, |x, y| {
        if heart_outline(x, y) {
            Luma([190])
        } else {
            Luma([24])
        }
    })
}

const fn heart_fill(x: u32, y: u32) -> bool {
    matches!(
        (x, y),
        (2..=3 | 5..=6, 1..=2) | (1..=7, 3..=4) | (2..=6, 5) | (3..=5, 6) | (4, 7)
    )
}

const fn heart_outline(x: u32, y: u32) -> bool {
    matches!(
        (x, y),
        (1..=3 | 5..=7, 0)
            | (0 | 8, 2..=4)
            | (1 | 7, 5)
            | (2 | 6, 6)
            | (3 | 5, 7)
            | (4, 8)
    )
}

fn blit(target: &mut GrayImage, source: &GrayImage, x: u32, y: u32) {
    for source_y in 0..source.height() {
        for source_x in 0..source.width() {
            let target_x = x.saturating_add(source_x);
            let target_y = y.saturating_add(source_y);
            if target_x < target.width() && target_y < target.height() {
                target.put_pixel(target_x, target_y, *source.get_pixel(source_x, source_y));
            }
        }
    }
}
