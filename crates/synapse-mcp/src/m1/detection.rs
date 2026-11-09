use chrono::{DateTime, Utc};
use std::time::Instant;
use synapse_core::{
    DetectedEntity, Detection, PerceptionMode, ProfileDetection, Rect, SensorStatus, entity_id,
    error_codes,
};
use synapse_models::{
    DEFAULT_DETECTION_MODEL_ID, DetectOpts, DetectionFrame, Detector, LoadedModel, ModelLoader,
    default_detection_model_descriptor, detection_model_not_loaded, registered_model,
};

use crate::m1::M1State;

const DEFAULT_DETECTION_CONFIDENCE_THRESHOLD: f32 = 0.5;
const DEFAULT_DETECTION_MAX_DETECTIONS: u32 = 32;
const STALE_TRACK_MS: i64 = 3_000;
const MIN_TRACK_MATCH_DISTANCE_PX: f32 = 96.0;

#[derive(Clone, Debug, PartialEq)]
pub struct DetectionRuntimeConfig {
    pub model_id: Option<String>,
    pub classes_of_interest: Vec<String>,
    pub confidence_threshold: f32,
    pub max_detections: u32,
}

impl DetectionRuntimeConfig {
    #[must_use]
    pub fn from_profile(profile: &ProfileDetection) -> Self {
        Self {
            model_id: profile.model_id.clone(),
            classes_of_interest: profile.classes_of_interest.clone(),
            confidence_threshold: profile.confidence_threshold,
            max_detections: profile.max_detections,
        }
    }
}

impl Default for DetectionRuntimeConfig {
    fn default() -> Self {
        Self {
            model_id: None,
            classes_of_interest: Vec::new(),
            confidence_threshold: DEFAULT_DETECTION_CONFIDENCE_THRESHOLD,
            max_detections: DEFAULT_DETECTION_MAX_DETECTIONS,
        }
    }
}

#[derive(Debug, Default)]
pub struct DetectionRuntime {
    loader: ModelLoader,
    loaded: Option<LoadedDetectionModel>,
    tracker: EntityTracker,
    next_frame_seq: u64,
}

impl DetectionRuntime {
    fn next_frame_seq(&mut self) -> u64 {
        self.next_frame_seq = self.next_frame_seq.saturating_add(1);
        self.next_frame_seq
    }

    fn load_model(&mut self, model_id: Option<&str>) -> synapse_models::ModelResult<&LoadedModel> {
        let resolved_model_id = model_id.unwrap_or(DEFAULT_DETECTION_MODEL_ID);
        if self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.model_id == resolved_model_id)
        {
            return self
                .loaded
                .as_ref()
                .map(|loaded| &loaded.model)
                .ok_or_else(|| detection_model_not_loaded("detection model cache was empty"));
        }

        let descriptor = if resolved_model_id == DEFAULT_DETECTION_MODEL_ID {
            default_detection_model_descriptor()
        } else {
            registered_model(resolved_model_id)
                .ok_or_else(|| {
                    detection_model_not_loaded(format!(
                        "detection model id {resolved_model_id:?} is not registered"
                    ))
                })?
                .descriptor()
        };
        if !descriptor.path.exists() {
            return Err(detection_model_not_loaded(format!(
                "side-load {} before requesting detection model {resolved_model_id}",
                descriptor.path.display()
            )));
        }
        let model = self.loader.load(descriptor)?;
        tracing::info!(
            code = "M1_DETECTION_MODEL_LOADED",
            model_id = %resolved_model_id,
            backend = ?model.selected_backend(),
            session_id = model.session_id(),
            "detection model loaded"
        );
        self.loaded = Some(LoadedDetectionModel {
            model_id: resolved_model_id.to_owned(),
            model,
        });
        self.loaded
            .as_ref()
            .map(|loaded| &loaded.model)
            .ok_or_else(|| detection_model_not_loaded("detection model cache was empty after load"))
    }
}

#[derive(Debug)]
struct LoadedDetectionModel {
    model_id: String,
    model: LoadedModel,
}

pub fn default_detection_config() -> DetectionRuntimeConfig {
    DetectionRuntimeConfig::default()
}

pub fn populate_detection_from_state(
    state: &mut M1State,
    input: &mut synapse_perception::ObservationInput,
) {
    let mode = input.mode_override.unwrap_or(state.perception_mode);
    if !matches!(mode, PerceptionMode::PixelOnly | PerceptionMode::Hybrid) {
        input.detection_status = SensorStatus::Disabled;
        return;
    }
    if input.foreground.window_bounds.w <= 0 || input.foreground.window_bounds.h <= 0 {
        input.detection_status = SensorStatus::DegradedSensorFailed {
            reason_code: error_codes::DETECTION_NO_FRAME.to_owned(),
        };
        return;
    }
    if !valid_detection_config(&state.detection_config) {
        input.detection_status = SensorStatus::DegradedSensorFailed {
            reason_code: error_codes::DETECTION_MODEL_INFER_FAILED.to_owned(),
        };
        tracing::warn!(
            code = "M1_DETECTION_CONFIG_INVALID",
            confidence_threshold = state.detection_config.confidence_threshold,
            max_detections = state.detection_config.max_detections,
            "detection configuration is invalid"
        );
        return;
    }
    if state.detection_config.max_detections == 0 {
        input.detection_status = SensorStatus::Healthy;
        return;
    }

    let started = Instant::now();
    let captured =
        match synapse_capture::screen_region_to_bgra_bitmap(input.foreground.window_bounds) {
            Ok(captured) => captured,
            Err(error) => {
                input.detection_status = SensorStatus::DegradedSensorFailed {
                    reason_code: error.code().to_owned(),
                };
                tracing::warn!(
                    code = "M1_DETECTION_CAPTURE_FAILED",
                    error = %error,
                    "foreground capture failed before detection inference"
                );
                return;
            }
        };
    let rgb = match bgra_to_rgb(&captured.bytes, captured.width, captured.height) {
        Ok(rgb) => rgb,
        Err(detail) => {
            input.detection_status = SensorStatus::DegradedSensorFailed {
                reason_code: error_codes::DETECTION_NO_FRAME.to_owned(),
            };
            tracing::warn!(
                code = "M1_DETECTION_FRAME_INVALID",
                detail,
                "captured detection frame was invalid"
            );
            return;
        }
    };

    let frame = DetectionFrame {
        frame_seq: state.detection_runtime.next_frame_seq(),
        width: captured.width,
        height: captured.height,
        rgb,
    };
    let opts = DetectOpts {
        confidence_threshold: threshold_percent(state.detection_config.confidence_threshold),
        max_detections: usize::try_from(state.detection_config.max_detections)
            .unwrap_or(usize::MAX),
    };
    let batch = match state
        .detection_runtime
        .load_model(state.detection_config.model_id.as_deref())
        .and_then(|model| model.infer(frame, opts))
    {
        Ok(batch) => batch,
        Err(error) => {
            input.detection_status = SensorStatus::DegradedSensorFailed {
                reason_code: error.code().to_owned(),
            };
            tracing::warn!(
                code = "M1_DETECTION_INFERENCE_FAILED",
                model_id = ?state.detection_config.model_id,
                error = %error,
                "detection inference failed"
            );
            return;
        }
    };
    let detections = filter_classes(batch.items, &state.detection_config.classes_of_interest);
    let entities =
        state
            .detection_runtime
            .tracker
            .update(detections, batch.inferred_at, captured.region);
    input.entities.extend(entities);
    input.detection_status = SensorStatus::Healthy;
    input.sensor_latency_ms.insert(
        "detection".to_owned(),
        started.elapsed().as_secs_f32() * 1000.0,
    );
}

fn valid_detection_config(config: &DetectionRuntimeConfig) -> bool {
    config.confidence_threshold.is_finite() && (0.0..=1.0).contains(&config.confidence_threshold)
}

fn threshold_percent(value: f32) -> u16 {
    let scaled = (value.clamp(0.0, 1.0) * 100.0).round();
    if scaled <= 0.0 {
        0
    } else if scaled >= 100.0 {
        100
    } else {
        scaled as u16
    }
}

fn filter_classes(detections: Vec<Detection>, classes_of_interest: &[String]) -> Vec<Detection> {
    if classes_of_interest.is_empty() {
        return detections;
    }
    detections
        .into_iter()
        .filter(|detection| {
            classes_of_interest
                .iter()
                .any(|class| class.eq_ignore_ascii_case(&detection.class_label))
        })
        .collect()
}

fn bgra_to_rgb(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("BGRA dimensions {width}x{height} overflow byte length"))?;
    if bytes.len() != expected {
        return Err(format!(
            "BGRA byte length mismatch: got {}, expected {expected} for {width}x{height}",
            bytes.len()
        ));
    }
    let mut rgb = Vec::with_capacity(expected / 4 * 3);
    for pixel in bytes.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }
    Ok(rgb)
}

#[derive(Clone, Debug)]
struct TrackedEntity {
    track_id: u64,
    class_label: String,
    bbox: Rect,
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
}

#[derive(Debug)]
struct EntityTracker {
    next_track_id: u64,
    active: Vec<TrackedEntity>,
}

impl Default for EntityTracker {
    fn default() -> Self {
        Self {
            next_track_id: 1,
            active: Vec::new(),
        }
    }
}

impl EntityTracker {
    fn update(
        &mut self,
        detections: Vec<Detection>,
        observed_at: DateTime<Utc>,
        origin: Rect,
    ) -> Vec<DetectedEntity> {
        self.prune_stale(observed_at);
        let mut used_tracks = Vec::new();
        detections
            .into_iter()
            .map(|detection| {
                let bbox = Rect {
                    x: origin.x.saturating_add(detection.bbox.x),
                    y: origin.y.saturating_add(detection.bbox.y),
                    w: detection.bbox.w,
                    h: detection.bbox.h,
                };
                let match_index = self.best_match(&detection.class_label, bbox, &used_tracks);
                if let Some(index) = match_index {
                    used_tracks.push(index);
                    let previous = self.active[index].clone();
                    self.active[index].bbox = bbox;
                    self.active[index].last_seen_at = observed_at;
                    DetectedEntity {
                        entity_id: entity_id(previous.track_id),
                        track_id: previous.track_id,
                        class_label: detection.class_label,
                        bbox,
                        confidence: detection.confidence,
                        first_seen_at: previous.first_seen_at,
                        last_seen_at: observed_at,
                        velocity_px_per_s: velocity(
                            previous.bbox,
                            bbox,
                            previous.last_seen_at,
                            observed_at,
                        ),
                    }
                } else {
                    let track_id = self.allocate_track_id();
                    self.active.push(TrackedEntity {
                        track_id,
                        class_label: detection.class_label.clone(),
                        bbox,
                        first_seen_at: observed_at,
                        last_seen_at: observed_at,
                    });
                    used_tracks.push(self.active.len().saturating_sub(1));
                    DetectedEntity {
                        entity_id: entity_id(track_id),
                        track_id,
                        class_label: detection.class_label,
                        bbox,
                        confidence: detection.confidence,
                        first_seen_at: observed_at,
                        last_seen_at: observed_at,
                        velocity_px_per_s: None,
                    }
                }
            })
            .collect()
    }

    fn prune_stale(&mut self, observed_at: DateTime<Utc>) {
        self.active.retain(|track| {
            observed_at
                .signed_duration_since(track.last_seen_at)
                .num_milliseconds()
                <= STALE_TRACK_MS
        });
    }

    fn best_match(&self, class_label: &str, bbox: Rect, used_tracks: &[usize]) -> Option<usize> {
        self.active
            .iter()
            .enumerate()
            .filter(|(index, track)| {
                !used_tracks.contains(index)
                    && track.class_label == class_label
                    && track_matches(track.bbox, bbox)
            })
            .min_by(|(_left_index, left), (_right_index, right)| {
                center_distance(left.bbox, bbox).total_cmp(&center_distance(right.bbox, bbox))
            })
            .map(|(index, _track)| index)
    }

    fn allocate_track_id(&mut self) -> u64 {
        let track_id = self.next_track_id;
        self.next_track_id = self.next_track_id.saturating_add(1);
        track_id
    }
}

fn track_matches(previous: Rect, current: Rect) -> bool {
    let size_gate = previous
        .w
        .max(previous.h)
        .max(current.w)
        .max(current.h)
        .max(1) as f32
        * 1.5;
    iou(previous, current) >= 0.10
        || center_distance(previous, current) <= size_gate.max(MIN_TRACK_MATCH_DISTANCE_PX)
}

fn velocity(
    previous: Rect,
    current: Rect,
    previous_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
) -> Option<(f32, f32)> {
    let elapsed_ms = observed_at
        .signed_duration_since(previous_at)
        .num_milliseconds();
    if elapsed_ms <= 0 {
        return None;
    }
    let seconds = elapsed_ms as f32 / 1000.0;
    let (prev_x, prev_y) = center(previous);
    let (cur_x, cur_y) = center(current);
    Some(((cur_x - prev_x) / seconds, (cur_y - prev_y) / seconds))
}

fn center_distance(left: Rect, right: Rect) -> f32 {
    let (left_x, left_y) = center(left);
    let (right_x, right_y) = center(right);
    (left_x - right_x).hypot(left_y - right_y)
}

fn center(rect: Rect) -> (f32, f32) {
    (
        rect.x as f32 + (rect.w as f32 / 2.0),
        rect.y as f32 + (rect.h as f32 / 2.0),
    )
}

fn iou(left: Rect, right: Rect) -> f32 {
    let x1 = left.x.max(right.x);
    let y1 = left.y.max(right.y);
    let x2 = left
        .x
        .saturating_add(left.w)
        .min(right.x.saturating_add(right.w));
    let y2 = left
        .y
        .saturating_add(left.h)
        .min(right.y.saturating_add(right.h));
    let intersection_w = x2.saturating_sub(x1).max(0);
    let intersection_h = y2.saturating_sub(y1).max(0);
    let intersection = intersection_w.saturating_mul(intersection_h);
    if intersection <= 0 {
        return 0.0;
    }
    let left_area = left.w.max(0).saturating_mul(left.h.max(0));
    let right_area = right.w.max(0).saturating_mul(right.h.max(0));
    let union = left_area
        .saturating_add(right_area)
        .saturating_sub(intersection);
    if union <= 0 {
        return 0.0;
    }
    intersection as f32 / union as f32
}
