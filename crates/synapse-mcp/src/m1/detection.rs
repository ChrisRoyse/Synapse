use chrono::{DateTime, Utc};
use rmcp::ErrorData;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, process::ExitCode, time::Instant};
use synapse_calyx::SynapseCalyxGpuReservation;
use synapse_core::{
    DetectedEntity, Detection, DetectionBatch, PerceptionMode, ProfileDetection, Rect,
    SensorStatus, entity_id, error_codes,
};
use synapse_models::{
    DEFAULT_DETECTION_MODEL_ID, DetectOpts, DetectionFrame, Detector, ModelBackend, ModelLoader,
    default_detection_model_descriptor, registered_model,
};

const DEFAULT_DETECTION_CONFIDENCE_THRESHOLD: f32 = 0.5;
const STALE_TRACK_MS: i64 = 3_000;
const MIN_TRACK_MATCH_DISTANCE_PX: f32 = 96.0;
const DETECTION_GPU_ADMISSION_MIB: u64 = 4_096;
const DETECTION_WORKER_TIMEOUT_MS: u32 = 30_000;

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
            max_detections: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct DetectionRuntime {
    tracker: EntityTracker,
    next_frame_seq: u64,
}

impl DetectionRuntime {
    fn next_frame_seq(&mut self) -> u64 {
        self.next_frame_seq = self.next_frame_seq.saturating_add(1);
        self.next_frame_seq
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DetectionWorkerRequest {
    model_id: String,
    frame_seq: u64,
    width: u32,
    height: u32,
    rgb_path: PathBuf,
    opts: DetectOpts,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DetectionWorkerEnvelope {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch: Option<DetectionBatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_detail: Option<String>,
}

pub(crate) fn run_detection_worker_from_cli(
    request_path: Option<PathBuf>,
    response_path: Option<PathBuf>,
) -> anyhow::Result<ExitCode> {
    let request_path =
        request_path.ok_or_else(|| anyhow::anyhow!("--detection-worker-request is required"))?;
    let response_path =
        response_path.ok_or_else(|| anyhow::anyhow!("--detection-worker-response is required"))?;
    let envelope = run_detection_worker(&request_path).unwrap_or_else(|(code, detail)| {
        DetectionWorkerEnvelope {
            ok: false,
            batch: None,
            reservation_id: None,
            error_code: Some(code),
            error_detail: Some(detail),
        }
    });
    let bytes = serde_json::to_vec(&envelope)?;
    fs::write(&response_path, bytes)?;
    Ok(if envelope.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn run_detection_worker(
    request_path: &std::path::Path,
) -> Result<DetectionWorkerEnvelope, (String, String)> {
    let request_bytes = fs::read(request_path).map_err(|error| {
        (
            "DETECTION_WORKER_REQUEST_READ_FAILED".to_owned(),
            error.to_string(),
        )
    })?;
    let request: DetectionWorkerRequest =
        serde_json::from_slice(&request_bytes).map_err(|error| {
            (
                "DETECTION_WORKER_REQUEST_INVALID".to_owned(),
                error.to_string(),
            )
        })?;
    let rgb = fs::read(&request.rgb_path).map_err(|error| {
        (
            "DETECTION_WORKER_FRAME_READ_FAILED".to_owned(),
            error.to_string(),
        )
    })?;
    let descriptor = if request.model_id == DEFAULT_DETECTION_MODEL_ID {
        default_detection_model_descriptor()
    } else {
        registered_model(&request.model_id)
            .ok_or_else(|| {
                (
                    error_codes::DETECTION_MODEL_NOT_LOADED.to_owned(),
                    format!(
                        "detection model id {:?} is not registered",
                        request.model_id
                    ),
                )
            })?
            .descriptor()
    };
    if !descriptor.path.exists() {
        return Err((
            error_codes::DETECTION_MODEL_NOT_LOADED.to_owned(),
            format!(
                "side-load {} before requesting detection model {}",
                descriptor.path.display(),
                request.model_id
            ),
        ));
    }
    let reservation = SynapseCalyxGpuReservation::acquire(
        0,
        "synapse-mcp-detection-worker",
        format!("synapse-detection-worker-pid-{}", std::process::id()),
        format!(
            "isolated ORT DirectML detector model={}; declared_session_and_inference_envelope_mib={DETECTION_GPU_ADMISSION_MIB}",
            request.model_id
        ),
        DETECTION_GPU_ADMISSION_MIB,
    )
    .map_err(|error| (error.code.to_owned(), error.to_string()))?;
    let reservation_id = reservation
        .admitted_snapshot()
        .reservations
        .iter()
        .find(|row| row.pid == std::process::id())
        .map(|row| row.reservation_id.clone());
    let loader = ModelLoader::new(vec![ModelBackend::DirectMl]);
    let model = loader
        .load(descriptor)
        .map_err(|error| (error.code().to_owned(), error.to_string()))?;
    let batch = model
        .infer(
            DetectionFrame {
                frame_seq: request.frame_seq,
                width: request.width,
                height: request.height,
                rgb,
            },
            request.opts,
        )
        .map_err(|error| (error.code().to_owned(), error.to_string()))?;
    drop(model);
    drop(reservation);
    Ok(DetectionWorkerEnvelope {
        ok: true,
        batch: Some(batch),
        reservation_id,
        error_code: None,
        error_detail: None,
    })
}

#[cfg(windows)]
fn infer_in_owned_worker(
    model_id: &str,
    frame: DetectionFrame,
    opts: DetectOpts,
) -> synapse_models::ModelResult<DetectionBatch> {
    use synapse_models::{detection_infer_failed, detection_model_not_loaded};

    let temp = tempfile::Builder::new()
        .prefix("synapse-detection-worker-")
        .tempdir()
        .map_err(|error| {
            detection_infer_failed(format!("create worker temp directory: {error}"))
        })?;
    let request_path = temp.path().join("request.json");
    let response_path = temp.path().join("response.json");
    let rgb_path = temp.path().join("frame.rgb");
    fs::write(&rgb_path, &frame.rgb)
        .map_err(|error| detection_infer_failed(format!("write worker RGB frame: {error}")))?;
    let request = DetectionWorkerRequest {
        model_id: model_id.to_owned(),
        frame_seq: frame.frame_seq,
        width: frame.width,
        height: frame.height,
        rgb_path,
        opts,
    };
    fs::write(
        &request_path,
        serde_json::to_vec(&request)
            .map_err(|error| detection_infer_failed(format!("encode worker request: {error}")))?,
    )
    .map_err(|error| detection_infer_failed(format!("write worker request: {error}")))?;
    let args = vec![
        "--mode".to_owned(),
        "detection-worker".to_owned(),
        "--detection-worker-request".to_owned(),
        request_path.to_string_lossy().into_owned(),
        "--detection-worker-response".to_owned(),
        response_path.to_string_lossy().into_owned(),
    ];
    let verdict =
        crate::desktop_worker::run_owned_current_exe_worker(&args, DETECTION_WORKER_TIMEOUT_MS)
            .map_err(|error| {
                detection_infer_failed(format!("owned detector process failed: {error}"))
            })?;
    if verdict.timed_out {
        return Err(detection_infer_failed(format!(
            "isolated detector pid {} timed out after {DETECTION_WORKER_TIMEOUT_MS} ms and was terminated with kernel exit_code={}",
            verdict.pid, verdict.exit_code
        )));
    }
    let response_bytes = fs::read(&response_path).map_err(|error| {
        detection_infer_failed(format!(
            "isolated detector pid {} exited {} without readable response: {error}",
            verdict.pid, verdict.exit_code
        ))
    })?;
    let response: DetectionWorkerEnvelope =
        serde_json::from_slice(&response_bytes).map_err(|error| {
            detection_infer_failed(format!(
                "isolated detector pid {} returned invalid response JSON: {error}",
                verdict.pid
            ))
        })?;
    if verdict.exit_code != 0 || !response.ok {
        return Err(detection_model_not_loaded(format!(
            "isolated detector pid {} failed exit_code={} code={} detail={}",
            verdict.pid,
            verdict.exit_code,
            response.error_code.as_deref().unwrap_or("<missing>"),
            response.error_detail.as_deref().unwrap_or("<missing>")
        )));
    }
    response.batch.ok_or_else(|| {
        detection_infer_failed(format!(
            "isolated detector pid {} returned ok without a detection batch",
            verdict.pid
        ))
    })
}

pub fn default_detection_config() -> DetectionRuntimeConfig {
    DetectionRuntimeConfig::default()
}

pub fn populate_detection_from_state(
    runtime: &mut DetectionRuntime,
    config: &DetectionRuntimeConfig,
    perception_mode: PerceptionMode,
    input: &mut synapse_perception::ObservationInput,
) -> Result<(), ErrorData> {
    let mode = input.mode_override.unwrap_or(perception_mode);
    if !matches!(mode, PerceptionMode::PixelOnly | PerceptionMode::Hybrid) {
        input.detection_status = SensorStatus::Disabled;
        return Ok(());
    }
    if input.foreground.window_bounds.w <= 0 || input.foreground.window_bounds.h <= 0 {
        return Err(ErrorData::invalid_params(
            format!(
                "{}: detection requires positive foreground bounds, got {:?}",
                error_codes::DETECTION_NO_FRAME,
                input.foreground.window_bounds
            ),
            None,
        ));
    }
    if !valid_detection_config(config) {
        tracing::warn!(
            code = "M1_DETECTION_CONFIG_INVALID",
            confidence_threshold = config.confidence_threshold,
            max_detections = config.max_detections,
            "detection configuration is invalid"
        );
        return Err(ErrorData::invalid_params(
            format!(
                "{}: confidence_threshold={} max_detections={}",
                error_codes::DETECTION_MODEL_INFER_FAILED,
                config.confidence_threshold,
                config.max_detections
            ),
            None,
        ));
    }
    // GPU inference is profile-opt-in. A numeric default must never silently
    // load the default ORT model for an ordinary productivity profile: the
    // profile must name the exact model whose resource envelope was reviewed.
    if config.model_id.is_none() || config.max_detections == 0 {
        input.detection_status = SensorStatus::Healthy;
        return Ok(());
    }

    let started = Instant::now();
    let captured =
        match synapse_capture::screen_region_to_bgra_bitmap(input.foreground.window_bounds) {
            Ok(captured) => captured,
            Err(error) => {
                tracing::error!(
                    code = "M1_DETECTION_CAPTURE_FAILED",
                    error = %error,
                    "foreground capture failed before detection inference"
                );
                return Err(rmcp::ErrorData::internal_error(error.to_string(), None));
            }
        };
    let rgb = match bgra_to_rgb(&captured.bytes, captured.width, captured.height) {
        Ok(rgb) => rgb,
        Err(detail) => {
            tracing::error!(
                code = "M1_DETECTION_FRAME_INVALID",
                detail,
                "captured detection frame was invalid"
            );
            return Err(rmcp::ErrorData::invalid_params(detail, None));
        }
    };

    let frame = DetectionFrame {
        frame_seq: runtime.next_frame_seq(),
        width: captured.width,
        height: captured.height,
        rgb,
    };
    let opts = DetectOpts {
        confidence_threshold: threshold_percent(config.confidence_threshold),
        max_detections: usize::try_from(config.max_detections).unwrap_or(usize::MAX),
    };
    #[cfg(windows)]
    let inference = infer_in_owned_worker(
        config
            .model_id
            .as_deref()
            .unwrap_or(DEFAULT_DETECTION_MODEL_ID),
        frame,
        opts,
    );
    #[cfg(not(windows))]
    let inference = Err(synapse_models::detection_model_not_loaded(
        "isolated DirectML detection worker is only available on Windows",
    ));
    let batch = match inference {
        Ok(batch) => batch,
        Err(error) => {
            tracing::error!(
                code = "M1_DETECTION_INFERENCE_FAILED",
                model_id = ?config.model_id,
                error = %error,
                "detection inference failed"
            );
            return Err(rmcp::ErrorData::internal_error(
                format!("{}: {error}", error.code()),
                None,
            ));
        }
    };
    let detections = filter_classes(batch.items, &config.classes_of_interest);
    let entities = runtime
        .tracker
        .update(detections, batch.inferred_at, captured.region);
    input.entities.extend(entities);
    input.detection_status = SensorStatus::Healthy;
    input.sensor_latency_ms.insert(
        "detection".to_owned(),
        started.elapsed().as_secs_f32() * 1000.0,
    );
    Ok(())
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
