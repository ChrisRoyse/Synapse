mod error;
mod event_extensions;
pub mod hud;
mod observe;
mod ocr;
mod template_match;

pub use error::{PerceptionError, PerceptionResult};
pub use event_extensions::{
    evaluate_event_extensions, validate_event_extension, validate_event_extensions,
};
pub use hud::{
    ExtractionSource, FieldExtraction, FieldExtractionRequest, HudAnchor, HudAnchorRegion,
    ResolvedHudRegion, extract_field, parse_hud_text, resolve_anchor_region, resolve_hud_region,
    resolve_hud_region_rect,
};
pub use observe::{
    A11yTreeSummary, ObservationAssembler, ObservationInput, ObserveInclude, assemble,
    assemble_from_input, auto_mode, auto_mode_with_a11y, bounded_sensor_latency,
    is_interactable_node, is_known_game_process, parse_perception_mode,
};
pub use ocr::{
    OcrProvider, SystemOcrProvider, TextRegion, TextRegionConfidenceSource, is_empty_region,
    read_text, read_text_with_provider,
};
pub use template_match::{
    HudTemplate, TemplateCounterConfig, TemplateCounterReading, TemplateSlotReading,
    extract_template_counter_from_frame, extract_template_counter_from_region,
};

#[cfg(windows)]
pub use ocr::{read_text_from_bgra_bitmap, read_text_from_software_bitmap};
