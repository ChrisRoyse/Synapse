use serde::{Deserialize, Serialize};
use synapse_core::DetectionBatch;

mod download;
mod ep;
mod error;
mod registry;
mod session;
mod verify;

pub use download::{ModelDescriptor, default_model_dir, model_download_failed};
pub use ep::{ModelBackend, default_provider_order};
pub use error::{
    ModelError, ModelResult, detection_infer_failed, detection_model_not_loaded, detection_no_frame,
};
pub use registry::{
    COCO80_CLASS_MAP, DEFAULT_DETECTION_INPUT_SHAPE, DEFAULT_DETECTION_MODEL_ID, REGISTERED_MODELS,
    RTDETR_V2_S_COCO_ONNX, RTDETR_V2_S_COCO_ONNX_DOWNLOAD_URL, RTDETR_V2_S_COCO_ONNX_FILENAME,
    RTDETR_V2_S_COCO_ONNX_ID, RTDETR_V2_S_COCO_ONNX_LICENSE, RTDETR_V2_S_COCO_ONNX_SHA256,
    RTDETR_V2_S_COCO_ONNX_SOURCE_MODEL, RTDETR_V2_S_COCO_ONNX_SOURCE_REPO, RegisteredModel,
    default_detection_model, default_detection_model_descriptor, registered_model,
};
pub use session::{
    LoadedModel, ModelLoader, OrtSessionFactory, SessionBuildResult, SessionFactory, SessionHandle,
};
pub use verify::{normalize_sha256, sha256_file};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectOpts {
    pub confidence_threshold: u16,
    pub max_detections: usize,
}

impl Default for DetectOpts {
    fn default() -> Self {
        Self {
            confidence_threshold: 50,
            max_detections: 100,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectionFrame {
    pub frame_seq: u64,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rgb: Vec<u8>,
}

impl DetectionFrame {
    /// Validates that a detection frame carries image pixels.
    ///
    /// # Errors
    ///
    /// Returns `DETECTION_NO_FRAME` when the frame has a zero width or height.
    pub fn validate(self) -> ModelResult<Self> {
        if self.width == 0 || self.height == 0 {
            return Err(detection_no_frame(format!(
                "frame {} has invalid dimensions {}x{}",
                self.frame_seq, self.width, self.height
            )));
        }
        let expected = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| {
                detection_no_frame(format!(
                    "frame {} dimensions {}x{} overflow RGB byte length",
                    self.frame_seq, self.width, self.height
                ))
            })?;
        if self.rgb.len() != expected {
            return Err(detection_no_frame(format!(
                "frame {} has {} RGB bytes, expected {expected} for {}x{}",
                self.frame_seq,
                self.rgb.len(),
                self.width,
                self.height
            )));
        }
        Ok(self)
    }
}

pub trait Detector: Send + Sync {
    /// Runs object detection for one frame.
    ///
    /// # Errors
    ///
    /// Implementations return `DETECTION_MODEL_NOT_LOADED` when no model is
    /// loaded, `DETECTION_NO_FRAME` when no image pixels are available, and
    /// `DETECTION_MODEL_INFER_FAILED` when model execution fails.
    fn infer(&self, frame: DetectionFrame, opts: DetectOpts) -> ModelResult<DetectionBatch>;
}
