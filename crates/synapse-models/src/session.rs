use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::Utc;
#[cfg(feature = "ort")]
use image::{RgbImage, imageops::FilterType};
#[cfg(feature = "ort")]
use ort::value::Tensor;
use synapse_core::DetectionBatch;
#[cfg(feature = "ort")]
use synapse_core::{Detection, Rect};

#[cfg(feature = "ort")]
use crate::detection_infer_failed;
use crate::{
    DetectOpts, DetectionFrame, Detector, ModelBackend, ModelDescriptor, ModelError, ModelResult,
    default_provider_order, normalize_sha256, sha256_file,
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct LoadedModel {
    descriptor: ModelDescriptor,
    selected_backend: ModelBackend,
    session_id: u64,
    session: SessionHandle,
}

impl LoadedModel {
    #[must_use]
    pub const fn session_id(&self) -> u64 {
        self.session_id
    }

    #[must_use]
    pub const fn selected_backend(&self) -> ModelBackend {
        self.selected_backend
    }

    #[must_use]
    pub const fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn session(&self) -> &SessionHandle {
        &self.session
    }
}

impl Detector for LoadedModel {
    fn infer(&self, frame: DetectionFrame, opts: DetectOpts) -> ModelResult<DetectionBatch> {
        #[cfg(not(feature = "ort"))]
        let _ = &opts;
        let frame = frame.validate()?;
        match &self.session {
            SessionHandle::Placeholder => {
                Ok(empty_detection_batch(&self.descriptor, frame.frame_seq))
            }
            #[cfg(feature = "ort")]
            SessionHandle::Ort(session) => {
                let input = preprocess_rgb_frame(&frame, &self.descriptor)?;
                let (logits, pred_boxes) = {
                    let mut session_guard = session.lock().map_err(|_err| {
                        detection_infer_failed("ORT detection session lock was poisoned")
                    })?;
                    let outputs = session_guard
                        .run(ort::inputs! {
                            "pixel_values" => input,
                        })
                        .map_err(|err| {
                            detection_infer_failed(format!("ORT inference failed: {err}"))
                        })?;
                    let logits = outputs
                        .get("logits")
                        .ok_or_else(|| detection_infer_failed("ORT output `logits` was missing"))?
                        .try_extract_tensor::<f32>()
                        .map_err(|err| {
                            detection_infer_failed(format!("failed to extract `logits`: {err}"))
                        })?
                        .1
                        .to_vec();
                    let pred_boxes = outputs
                        .get("pred_boxes")
                        .ok_or_else(|| {
                            detection_infer_failed("ORT output `pred_boxes` was missing")
                        })?
                        .try_extract_tensor::<f32>()
                        .map_err(|err| {
                            detection_infer_failed(format!("failed to extract `pred_boxes`: {err}"))
                        })?
                        .1
                        .to_vec();
                    drop(outputs);
                    drop(session_guard);
                    (logits, pred_boxes)
                };
                let items =
                    decode_rtdetr_outputs(&frame, &self.descriptor, &logits, &pred_boxes, &opts)?;
                Ok(DetectionBatch {
                    model_id: self.descriptor.id.clone(),
                    frame_seq: frame.frame_seq,
                    inferred_at: Utc::now(),
                    items,
                })
            }
        }
    }
}

fn empty_detection_batch(descriptor: &ModelDescriptor, frame_seq: u64) -> DetectionBatch {
    DetectionBatch {
        model_id: descriptor.id.clone(),
        frame_seq,
        inferred_at: Utc::now(),
        items: Vec::new(),
    }
}

#[cfg(feature = "ort")]
fn preprocess_rgb_frame(
    frame: &DetectionFrame,
    descriptor: &ModelDescriptor,
) -> ModelResult<Tensor<f32>> {
    let [batch, channels, input_h, input_w] = descriptor_input_shape(descriptor)?;
    if batch != 1 || channels != 3 {
        return Err(detection_infer_failed(format!(
            "unsupported detection input shape {:?}; expected [1, 3, height, width]",
            descriptor.input_shape
        )));
    }
    let resize_width = u32::try_from(input_w).map_err(|_err| {
        detection_infer_failed(format!("input width {input_w} does not fit u32"))
    })?;
    let resize_height = u32::try_from(input_h).map_err(|_err| {
        detection_infer_failed(format!("input height {input_h} does not fit u32"))
    })?;
    let source =
        RgbImage::from_raw(frame.width, frame.height, frame.rgb.clone()).ok_or_else(|| {
            detection_infer_failed(format!(
                "failed to construct RGB frame {} from {} bytes",
                frame.frame_seq,
                frame.rgb.len()
            ))
        })?;
    let resized =
        image::imageops::resize(&source, resize_width, resize_height, FilterType::Triangle);
    let pixels = input_w.checked_mul(input_h).ok_or_else(|| {
        detection_infer_failed(format!("input shape {input_w}x{input_h} overflows"))
    })?;
    let mut data = vec![
        0.0_f32;
        pixels.checked_mul(3).ok_or_else(|| {
            detection_infer_failed(format!("input shape {input_w}x{input_h}x3 overflows"))
        })?
    ];
    for (index, pixel) in resized.pixels().enumerate() {
        data[index] = f32::from(pixel[0]) / 255.0;
        data[pixels + index] = f32::from(pixel[1]) / 255.0;
        data[(pixels * 2) + index] = f32::from(pixel[2]) / 255.0;
    }
    Tensor::from_array(([1usize, 3, input_h, input_w], data.into_boxed_slice()))
        .map_err(|err| detection_infer_failed(format!("failed to create input tensor: {err}")))
}

#[cfg(feature = "ort")]
fn descriptor_input_shape(descriptor: &ModelDescriptor) -> ModelResult<[usize; 4]> {
    <[usize; 4]>::try_from(descriptor.input_shape.as_slice()).map_err(|_err| {
        detection_infer_failed(format!(
            "unsupported detection input shape {:?}; expected four dimensions",
            descriptor.input_shape
        ))
    })
}

#[cfg(feature = "ort")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "RT-DETR post-processing converts normalized f32 model outputs into screen pixel rectangles"
)]
fn decode_rtdetr_outputs(
    frame: &DetectionFrame,
    descriptor: &ModelDescriptor,
    logits: &[f32],
    pred_boxes: &[f32],
    opts: &DetectOpts,
) -> ModelResult<Vec<Detection>> {
    let classes = descriptor.class_map.len();
    if classes == 0 {
        return Err(detection_infer_failed(format!(
            "model {} has no class map",
            descriptor.id
        )));
    }
    if !logits.len().is_multiple_of(classes) {
        return Err(detection_infer_failed(format!(
            "`logits` length {} is not divisible by class count {classes}",
            logits.len()
        )));
    }
    let queries = logits.len() / classes;
    let expected_box_values = queries.checked_mul(4).ok_or_else(|| {
        detection_infer_failed(format!("query count {queries} overflows box length"))
    })?;
    if pred_boxes.len() != expected_box_values {
        return Err(detection_infer_failed(format!(
            "`pred_boxes` length {} did not match query count {queries}",
            pred_boxes.len()
        )));
    }

    let threshold = f32::from(opts.confidence_threshold.min(100)) / 100.0;
    let mut detections = Vec::new();
    for query_index in 0..queries {
        let logits_start = query_index * classes;
        let Some((class_index, score)) =
            best_class_score(&logits[logits_start..logits_start + classes])
        else {
            continue;
        };
        if score < threshold {
            continue;
        }
        let box_start = query_index * 4;
        let bbox = normalized_cxcywh_to_rect(
            pred_boxes[box_start],
            pred_boxes[box_start + 1],
            pred_boxes[box_start + 2],
            pred_boxes[box_start + 3],
            frame.width,
            frame.height,
        );
        if bbox.w <= 0 || bbox.h <= 0 {
            continue;
        }
        detections.push(Detection {
            class_label: descriptor.class_map[class_index].clone(),
            bbox,
            confidence: score,
            track_id: None,
        });
    }
    detections.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
    detections.truncate(opts.max_detections);
    Ok(detections)
}

#[cfg(feature = "ort")]
fn best_class_score(logits: &[f32]) -> Option<(usize, f32)> {
    logits
        .iter()
        .enumerate()
        .map(|(index, value)| (index, sigmoid(*value)))
        .max_by(|left, right| left.1.total_cmp(&right.1))
}

#[cfg(feature = "ort")]
fn sigmoid(value: f32) -> f32 {
    if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exp = value.exp();
        exp / (1.0 + exp)
    }
}

#[cfg(feature = "ort")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "RT-DETR normalized box coordinates are f32 and must become integer screen rectangles"
)]
fn normalized_cxcywh_to_rect(cx: f32, cy: f32, w: f32, h: f32, width: u32, height: u32) -> Rect {
    let width_f = width as f32;
    let height_f = height as f32;
    let x1 = ((cx - (w / 2.0)) * width_f).clamp(0.0, width_f);
    let y1 = ((cy - (h / 2.0)) * height_f).clamp(0.0, height_f);
    let x2 = ((cx + (w / 2.0)) * width_f).clamp(0.0, width_f);
    let y2 = ((cy + (h / 2.0)) * height_f).clamp(0.0, height_f);
    let left = x1.min(x2).round() as i32;
    let top = y1.min(y2).round() as i32;
    let right = x1.max(x2).round() as i32;
    let bottom = y1.max(y2).round() as i32;
    Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    }
}

pub enum SessionHandle {
    Placeholder,
    #[cfg(feature = "ort")]
    Ort(std::sync::Arc<std::sync::Mutex<ort::session::Session>>),
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Placeholder => formatter.write_str("Placeholder"),
            #[cfg(feature = "ort")]
            Self::Ort(_session) => formatter.write_str("Ort(Session)"),
        }
    }
}

pub struct SessionBuildResult {
    pub selected_backend: ModelBackend,
    pub session: SessionHandle,
}

pub trait SessionFactory {
    /// Creates one persistent session for a verified model descriptor.
    ///
    /// # Errors
    ///
    /// Returns `MODEL_BACKEND_UNAVAILABLE` if no requested execution provider
    /// can create a session, or `MODEL_LOAD_FAILED` if ONNX Runtime rejects the
    /// verified model file.
    fn create_session(
        &self,
        descriptor: &ModelDescriptor,
        providers: &[ModelBackend],
    ) -> ModelResult<SessionBuildResult>;
}

#[derive(Clone, Debug)]
pub struct ModelLoader {
    providers: Vec<ModelBackend>,
}

impl Default for ModelLoader {
    fn default() -> Self {
        Self {
            providers: default_provider_order(),
        }
    }
}

impl ModelLoader {
    #[must_use]
    pub const fn new(providers: Vec<ModelBackend>) -> Self {
        Self { providers }
    }

    #[must_use]
    pub fn providers(&self) -> &[ModelBackend] {
        &self.providers
    }

    /// Verifies the model file and creates a persistent runtime session.
    ///
    /// # Errors
    ///
    /// Returns `MODEL_HASH_MISMATCH` before session creation when file bytes do
    /// not match the descriptor, or the session factory's structured model
    /// error when runtime creation fails.
    pub fn load_with_factory(
        &self,
        descriptor: ModelDescriptor,
        factory: &dyn SessionFactory,
    ) -> ModelResult<LoadedModel> {
        let actual = sha256_file(&descriptor.path).map_err(|err| ModelError::LoadFailed {
            path: descriptor.path.clone(),
            detail: err.to_string(),
        })?;
        let expected = normalize_sha256(&descriptor.sha256);
        if actual != expected {
            return Err(ModelError::HashMismatch {
                path: descriptor.path,
                expected,
                actual,
            });
        }

        let build = factory.create_session(&descriptor, &self.providers)?;
        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            model_id = descriptor.id,
            session_id,
            backend = ?build.selected_backend,
            "loaded ONNX model"
        );
        Ok(LoadedModel {
            descriptor,
            selected_backend: build.selected_backend,
            session_id,
            session: build.session,
        })
    }

    /// Loads a model only if its descriptor path exists.
    ///
    /// # Errors
    ///
    /// Returns the same structured errors as [`Self::load_with_factory`] when
    /// the file exists but verification or runtime creation fails.
    pub fn load_if_present(
        &self,
        descriptor: ModelDescriptor,
        factory: &dyn SessionFactory,
    ) -> ModelResult<Option<LoadedModel>> {
        if !descriptor.path.exists() {
            return Ok(None);
        }
        self.load_with_factory(descriptor, factory).map(Some)
    }

    /// Loads the legacy canonical `YOLOv10n` model only if it exists.
    ///
    /// # Errors
    ///
    /// Returns the same structured errors as [`Self::load_with_factory`] when
    /// the file exists but verification or runtime creation fails.
    pub fn load_yolov10n_if_present(
        &self,
        descriptor: ModelDescriptor,
        factory: &dyn SessionFactory,
    ) -> ModelResult<Option<LoadedModel>> {
        self.load_if_present(descriptor, factory)
    }

    /// Uses the built-in ORT session factory.
    ///
    /// # Errors
    ///
    /// Returns `MODEL_BACKEND_UNAVAILABLE` when this build has no ORT runtime
    /// feature or no requested execution provider can create a session.
    pub fn load(&self, descriptor: ModelDescriptor) -> ModelResult<LoadedModel> {
        self.load_with_factory(descriptor, &OrtSessionFactory)
    }
}

pub struct OrtSessionFactory;

#[cfg(not(feature = "ort"))]
impl SessionFactory for OrtSessionFactory {
    fn create_session(
        &self,
        _descriptor: &ModelDescriptor,
        providers: &[ModelBackend],
    ) -> ModelResult<SessionBuildResult> {
        Err(ModelError::BackendUnavailable {
            attempted: providers.to_vec(),
        })
    }
}

#[cfg(feature = "ort")]
impl SessionFactory for OrtSessionFactory {
    fn create_session(
        &self,
        descriptor: &ModelDescriptor,
        providers: &[ModelBackend],
    ) -> ModelResult<SessionBuildResult> {
        if providers.is_empty() {
            return Err(ModelError::BackendUnavailable {
                attempted: Vec::new(),
            });
        }

        let mut backend_failures = Vec::new();
        for provider in providers {
            match crate::ep::create_ort_session(descriptor, *provider) {
                Ok(session) => {
                    return Ok(SessionBuildResult {
                        selected_backend: *provider,
                        session: SessionHandle::Ort(std::sync::Arc::new(std::sync::Mutex::new(
                            session,
                        ))),
                    });
                }
                Err(ModelError::LoadFailed { detail, .. }) if *provider == ModelBackend::Cpu => {
                    return Err(ModelError::LoadFailed {
                        path: descriptor.path.clone(),
                        detail: format!("CPU provider rejected verified model: {detail}"),
                    });
                }
                Err(err) => backend_failures.push((*provider, err.to_string())),
            }
        }

        tracing::warn!(failures = ?backend_failures, "all model backends failed");
        Err(ModelError::BackendUnavailable {
            attempted: providers.to_vec(),
        })
    }
}
