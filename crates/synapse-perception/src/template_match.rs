use std::{path::Path, time::Instant};

use image::{DynamicImage, GenericImageView, GrayImage};
use synapse_core::Rect;

use crate::{PerceptionError, PerceptionResult};

pub const MINECRAFT_STATUS_SLOTS: u32 = 10;
pub const MINECRAFT_STATUS_MAX_VALUE: u32 = 20;
pub const DEFAULT_MIN_TEMPLATE_CONFIDENCE: f64 = 0.85;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HudTemplate {
    pub label: String,
    pub value: u32,
    pub image: GrayImage,
}

impl HudTemplate {
    /// Builds a grayscale template from an in-memory image.
    ///
    /// # Errors
    ///
    /// Returns `HUD_EXTRACTION_FAILED` when the template has no pixels.
    pub fn from_gray(
        label: impl Into<String>,
        value: u32,
        image: GrayImage,
    ) -> PerceptionResult<Self> {
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return Err(hud_error("template image must have non-zero dimensions"));
        }
        Ok(Self {
            label: label.into(),
            value,
            image,
        })
    }

    /// Loads a PNG or other `image`-supported template from disk.
    ///
    /// # Errors
    ///
    /// Returns `HUD_EXTRACTION_FAILED` when the image cannot be read or is empty.
    pub fn load(
        label: impl Into<String>,
        value: u32,
        path: impl AsRef<Path>,
    ) -> PerceptionResult<Self> {
        let path = path.as_ref();
        let image = image::open(path).map_err(|err| {
            hud_error(format!(
                "template {} could not be opened: {err}",
                path.display()
            ))
        })?;
        Self::from_gray(label, value, image.to_luma8())
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TemplateCounterConfig {
    pub slots: u32,
    pub min_confidence: f64,
    pub max_value: u32,
}

impl Default for TemplateCounterConfig {
    fn default() -> Self {
        Self {
            slots: MINECRAFT_STATUS_SLOTS,
            min_confidence: DEFAULT_MIN_TEMPLATE_CONFIDENCE,
            max_value: MINECRAFT_STATUS_MAX_VALUE,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TemplateCounterReading {
    pub value: u32,
    pub confidence: f64,
    pub elapsed_ms: f64,
    pub slots: Vec<TemplateSlotReading>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TemplateSlotReading {
    pub index: u32,
    pub label: String,
    pub value: u32,
    pub confidence: f64,
    pub x: u32,
    pub y: u32,
}

/// Crops a frame region and extracts a slotted HUD counter.
///
/// # Errors
///
/// Returns `HUD_EXTRACTION_FAILED` when the region is outside the frame, the
/// template set is invalid, or any slot fails the confidence threshold.
pub fn extract_template_counter_from_frame(
    frame: &DynamicImage,
    region: Rect,
    templates: &[HudTemplate],
    config: TemplateCounterConfig,
) -> PerceptionResult<TemplateCounterReading> {
    let crop = crop_frame_region(frame, region)?;
    extract_template_counter_from_region(&crop, templates, config)
}

/// Extracts a slotted HUD counter from an already-cropped grayscale region.
///
/// # Errors
///
/// Returns `HUD_EXTRACTION_FAILED` when the template set is invalid, the slot
/// geometry cannot contain the templates, or any slot fails the confidence
/// threshold.
pub fn extract_template_counter_from_region(
    region: &GrayImage,
    templates: &[HudTemplate],
    config: TemplateCounterConfig,
) -> PerceptionResult<TemplateCounterReading> {
    let started = Instant::now();
    validate_config(region, templates, config)?;

    let (region_w, _) = region.dimensions();
    let mut slots =
        Vec::with_capacity(usize::try_from(config.slots).map_err(|_err| {
            hud_error(format!("slot count {} does not fit usize", config.slots))
        })?);
    let mut value = 0_u32;
    let mut confidence_sum = 0.0_f64;

    for index in 0..config.slots {
        let (slot_x, slot_w) = slot_bounds(region_w, config.slots, index)?;
        let slot = image::imageops::crop_imm(region, slot_x, 0, slot_w, region.height()).to_image();
        let best = best_slot_match(&slot, templates).ok_or_else(|| {
            hud_error(format!(
                "slot {index} has no valid template candidate for {slot_w}x{}",
                region.height()
            ))
        })?;

        if best.confidence < config.min_confidence {
            return Err(hud_error(format!(
                "slot {index} confidence {:.3} below threshold {:.3}",
                best.confidence, config.min_confidence
            )));
        }

        let next_value = value
            .checked_add(best.value)
            .ok_or_else(|| hud_error(format!("HUD counter overflow while adding slot {index}")))?;
        if next_value > config.max_value {
            return Err(hud_error(format!(
                "HUD counter value {next_value} exceeds max {}",
                config.max_value
            )));
        }
        value = next_value;
        confidence_sum += best.confidence;
        slots.push(TemplateSlotReading {
            index,
            label: best.label,
            value: best.value,
            confidence: best.confidence,
            x: slot_x.saturating_add(best.x),
            y: best.y,
        });
    }

    let confidence = confidence_sum / f64::from(config.slots);
    Ok(TemplateCounterReading {
        value,
        confidence,
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
        slots,
    })
}

#[derive(Clone, Debug)]
struct Candidate {
    label: String,
    value: u32,
    confidence: f64,
    x: u32,
    y: u32,
}

fn crop_frame_region(frame: &DynamicImage, region: Rect) -> PerceptionResult<GrayImage> {
    if region.x < 0 || region.y < 0 || region.w <= 0 || region.h <= 0 {
        return Err(hud_error(format!(
            "frame crop region must be positive and in-frame, got {region:?}"
        )));
    }

    let x = u32::try_from(region.x)
        .map_err(|_err| hud_error(format!("region x is out of range: {}", region.x)))?;
    let y = u32::try_from(region.y)
        .map_err(|_err| hud_error(format!("region y is out of range: {}", region.y)))?;
    let w = u32::try_from(region.w)
        .map_err(|_err| hud_error(format!("region w is out of range: {}", region.w)))?;
    let h = u32::try_from(region.h)
        .map_err(|_err| hud_error(format!("region h is out of range: {}", region.h)))?;
    let (frame_w, frame_h) = frame.dimensions();
    let right = x
        .checked_add(w)
        .ok_or_else(|| hud_error("frame crop region x+w overflows"))?;
    let bottom = y
        .checked_add(h)
        .ok_or_else(|| hud_error("frame crop region y+h overflows"))?;
    if right > frame_w || bottom > frame_h {
        return Err(hud_error(format!(
            "frame crop region {region:?} exceeds frame {frame_w}x{frame_h}"
        )));
    }
    Ok(frame.crop_imm(x, y, w, h).to_luma8())
}

fn validate_config(
    region: &GrayImage,
    templates: &[HudTemplate],
    config: TemplateCounterConfig,
) -> PerceptionResult<()> {
    let (region_w, region_h) = region.dimensions();
    if region_w == 0 || region_h == 0 {
        return Err(hud_error("HUD region must have non-zero dimensions"));
    }
    if templates.is_empty() {
        return Err(hud_error(
            "HUD template matcher requires at least one template",
        ));
    }
    if config.slots == 0 {
        return Err(hud_error("HUD template matcher requires at least one slot"));
    }
    if !(0.0..=1.0).contains(&config.min_confidence) {
        return Err(hud_error(format!(
            "HUD template confidence threshold must be in 0..=1, got {}",
            config.min_confidence
        )));
    }
    if region_w < config.slots {
        return Err(hud_error(format!(
            "HUD region width {region_w} cannot be split into {} non-empty slots",
            config.slots
        )));
    }

    let (_, first_slot_w) = slot_bounds(region_w, config.slots, 0)?;
    for template in templates {
        let (template_w, template_h) = template.image.dimensions();
        if template_w == 0 || template_h == 0 {
            return Err(hud_error(format!(
                "template {} has zero dimensions",
                template.label
            )));
        }
        if template_w > first_slot_w || template_h > region_h {
            return Err(hud_error(format!(
                "template {} dimensions {template_w}x{template_h} exceed slot {first_slot_w}x{region_h}",
                template.label
            )));
        }
    }
    Ok(())
}

fn slot_bounds(region_w: u32, slots: u32, index: u32) -> PerceptionResult<(u32, u32)> {
    if slots == 0 || index >= slots {
        return Err(hud_error(format!(
            "invalid slot request index={index} slots={slots}"
        )));
    }
    let start = scaled_slot_edge(region_w, slots, index)?;
    let end = scaled_slot_edge(region_w, slots, index.saturating_add(1))?;
    let width = end
        .checked_sub(start)
        .ok_or_else(|| hud_error(format!("slot {index} end precedes start")))?;
    if width == 0 {
        return Err(hud_error(format!("slot {index} has zero width")));
    }
    Ok((start, width))
}

fn scaled_slot_edge(region_w: u32, slots: u32, index: u32) -> PerceptionResult<u32> {
    let numerator = u64::from(region_w)
        .checked_mul(u64::from(index))
        .ok_or_else(|| hud_error("slot geometry multiplication overflowed"))?;
    u32::try_from(numerator / u64::from(slots))
        .map_err(|_err| hud_error("slot geometry does not fit u32"))
}

fn best_slot_match(slot: &GrayImage, templates: &[HudTemplate]) -> Option<Candidate> {
    let mut best: Option<Candidate> = None;
    for template in templates {
        if let Some(candidate) = best_template_location(slot, template)
            && best
                .as_ref()
                .is_none_or(|current| candidate.confidence > current.confidence)
        {
            best = Some(candidate);
        }
    }
    best
}

fn best_template_location(slot: &GrayImage, template: &HudTemplate) -> Option<Candidate> {
    let (slot_w, slot_h) = slot.dimensions();
    let (template_w, template_h) = template.image.dimensions();
    if template_w > slot_w || template_h > slot_h {
        return None;
    }

    let mut best_score = None;
    let mut best_x = 0_u32;
    let mut best_y = 0_u32;
    for y in 0..=slot_h.saturating_sub(template_h) {
        for x in 0..=slot_w.saturating_sub(template_w) {
            if let Some(score) = normalized_cross_correlation(slot, &template.image, x, y)
                && best_score.is_none_or(|current| score > current)
            {
                best_score = Some(score);
                best_x = x;
                best_y = y;
            }
        }
    }

    best_score.map(|confidence| Candidate {
        label: template.label.clone(),
        value: template.value,
        confidence,
        x: best_x,
        y: best_y,
    })
}

fn normalized_cross_correlation(
    slot: &GrayImage,
    template: &GrayImage,
    offset_x: u32,
    offset_y: u32,
) -> Option<f64> {
    let (template_w, template_h) = template.dimensions();
    let n = f64::from(template_w) * f64::from(template_h);
    if n == 0.0 {
        return None;
    }

    let mut template_sum = 0.0_f64;
    let mut slot_sum = 0.0_f64;
    for y in 0..template_h {
        for x in 0..template_w {
            template_sum += f64::from(template.get_pixel(x, y).0[0]);
            slot_sum += f64::from(slot.get_pixel(offset_x + x, offset_y + y).0[0]);
        }
    }
    let template_mean = template_sum / n;
    let slot_mean = slot_sum / n;

    let mut numerator = 0.0_f64;
    let mut template_energy = 0.0_f64;
    let mut slot_energy = 0.0_f64;
    for y in 0..template_h {
        for x in 0..template_w {
            let template_delta = f64::from(template.get_pixel(x, y).0[0]) - template_mean;
            let slot_delta = f64::from(slot.get_pixel(offset_x + x, offset_y + y).0[0]) - slot_mean;
            numerator = template_delta.mul_add(slot_delta, numerator);
            template_energy = template_delta.mul_add(template_delta, template_energy);
            slot_energy = slot_delta.mul_add(slot_delta, slot_energy);
        }
    }

    let denominator = template_energy.sqrt() * slot_energy.sqrt();
    if denominator <= f64::EPSILON {
        return None;
    }
    Some((numerator / denominator).clamp(-1.0, 1.0))
}

fn hud_error(detail: impl Into<String>) -> PerceptionError {
    PerceptionError::HudExtractionFailed {
        detail: detail.into(),
    }
}
