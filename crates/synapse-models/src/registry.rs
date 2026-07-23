use crate::{ModelDescriptor, default_model_dir};
use crate::{ModelError, ModelResult, normalize_sha256, sha256_file};
use sha2::Digest;
use std::{
    fmt::Write as _,
    fs,
    io::{Read, Seek, SeekFrom, Write},
};

const EMBEDDED_MODEL_BUNDLE_MAGIC: &[u8; 16] = b"SYNMODEL_BUNDLE1";
const EMBEDDED_MODEL_BUNDLE_TRAILER_LEN: u64 = 40;
const EMBEDDED_MODEL_BUNDLE_TRAILER_SIZE: usize = 40;

pub const DEFAULT_DETECTION_MODEL_ID: &str = RTDETR_V2_S_COCO_ONNX_ID;

pub const RTDETR_V2_S_COCO_ONNX_ID: &str = "rtdetr_v2_s_coco_onnx";
pub const RTDETR_V2_S_COCO_ONNX_FILENAME: &str = "rtdetr_v2_s_coco.onnx";
pub const RTDETR_V2_S_COCO_ONNX_SHA256: &str =
    "sha256:583a236ac21c95a7fd94f284fc21485e42355bfef82c27011ba78fbc09ee87e2";
pub const RTDETR_V2_S_COCO_ONNX_DOWNLOAD_URL: &str =
    "https://huggingface.co/onnx-community/rtdetr_v2_r18vd-ONNX/resolve/main/onnx/model.onnx";
pub const RTDETR_V2_S_COCO_ONNX_LICENSE: &str = "Apache-2.0";
pub const RTDETR_V2_S_COCO_ONNX_SOURCE_MODEL: &str = "PekingU/rtdetr_v2_r18vd";
pub const RTDETR_V2_S_COCO_ONNX_SOURCE_REPO: &str = "https://github.com/lyuwenyu/RT-DETR";

pub const RTDETR_V2_S_COCO_INT8_ONNX_ID: &str = "rtdetr_v2_s_coco_int8_cpu_onnx";
pub const RTDETR_V2_S_COCO_INT8_ONNX_FILENAME: &str = "rtdetr_v2_s_coco_int8_cpu.onnx";
pub const RTDETR_V2_S_COCO_INT8_ONNX_SHA256: &str =
    "sha256:fed736d2593cf2ab099f665eeeb6d315d909783eea830f80a807fc1ac1c1b2ec";
pub const RTDETR_V2_S_COCO_INT8_ONNX_DOWNLOAD_URL: &str =
    "https://huggingface.co/onnx-community/rtdetr_v2_r18vd-ONNX/resolve/main/onnx/model_int8.onnx";
pub const WHISPER_TINY_INT8_ONNX_ID: &str = "whisper_tiny_int8";
pub const WHISPER_TINY_INT8_ONNX_FILENAME: &str = "whisper-tiny-int8.onnx";
pub const WHISPER_TINY_INT8_ONNX_SHA256: &str =
    "sha256:147afac751f89ad8e8f82133464edc81ecff9391e98ccdcae2474384be68ec86";

pub const DEFAULT_DETECTION_INPUT_SHAPE: [usize; 4] = [1, 3, 640, 640];

pub const COCO80_CLASS_MAP: [&str; 80] = [
    "person",
    "bicycle",
    "car",
    "motorbike",
    "aeroplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "sofa",
    "pottedplant",
    "bed",
    "diningtable",
    "toilet",
    "tvmonitor",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RegisteredModel {
    pub id: &'static str,
    pub label: &'static str,
    pub filename: &'static str,
    pub sha256: &'static str,
    pub download_url: &'static str,
    pub license_spdx: &'static str,
    pub source_model: &'static str,
    pub source_repo: &'static str,
    pub input_shape: [usize; 4],
    pub class_map: &'static [&'static str],
}

impl RegisteredModel {
    #[must_use]
    pub fn descriptor(self) -> ModelDescriptor {
        ModelDescriptor {
            id: self.id.to_owned(),
            path: default_model_dir().join(self.filename),
            sha256: self.sha256.to_owned(),
            input_shape: self.input_shape.to_vec(),
            class_map: self
                .class_map
                .iter()
                .map(|class_label| (*class_label).to_owned())
                .collect(),
        }
    }

    /// Reads this model from the authenticated model bundle appended to the
    /// running executable by the Synapse installer.
    ///
    /// # Errors
    ///
    /// Returns a structured load error if the executable has no valid bundle,
    /// its lengths are invalid, or the selected payload cannot be read.
    pub fn embedded_bytes(self) -> ModelResult<Vec<u8>> {
        let executable = std::env::current_exe().map_err(|error| ModelError::LoadFailed {
            path: self.descriptor().path,
            detail: format!("failed to resolve running executable for embedded model: {error}"),
        })?;
        let mut file = fs::File::open(&executable).map_err(|error| ModelError::LoadFailed {
            path: executable.clone(),
            detail: format!("failed to open running executable model bundle: {error}"),
        })?;
        let file_len = file
            .metadata()
            .map_err(|error| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!("failed to inspect running executable model bundle: {error}"),
            })?
            .len();
        if file_len < EMBEDDED_MODEL_BUNDLE_TRAILER_LEN {
            return Err(ModelError::LoadFailed {
                path: executable,
                detail: "running executable has no embedded model bundle trailer".to_owned(),
            });
        }
        file.seek(SeekFrom::End(-40))
            .map_err(|error| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!("failed to seek to embedded model bundle trailer: {error}"),
            })?;
        let mut trailer = [0_u8; EMBEDDED_MODEL_BUNDLE_TRAILER_SIZE];
        file.read_exact(&mut trailer)
            .map_err(|error| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!("failed to read embedded model bundle trailer: {error}"),
            })?;
        if &trailer[24..] != EMBEDDED_MODEL_BUNDLE_MAGIC {
            return Err(ModelError::LoadFailed {
                path: executable,
                detail: "running executable model bundle magic is missing or corrupt".to_owned(),
            });
        }
        let mut length_bytes = [0_u8; 8];
        length_bytes.copy_from_slice(&trailer[0..8]);
        let gpu_len = u64::from_le_bytes(length_bytes);
        length_bytes.copy_from_slice(&trailer[8..16]);
        let cpu_len = u64::from_le_bytes(length_bytes);
        length_bytes.copy_from_slice(&trailer[16..24]);
        let whisper_len = u64::from_le_bytes(length_bytes);
        let payload_len = gpu_len
            .checked_add(cpu_len)
            .and_then(|value| value.checked_add(whisper_len))
            .ok_or_else(|| ModelError::LoadFailed {
                path: executable.clone(),
                detail: "embedded model bundle payload lengths overflow".to_owned(),
            })?;
        let payload_start = file_len
            .checked_sub(EMBEDDED_MODEL_BUNDLE_TRAILER_LEN)
            .and_then(|value| value.checked_sub(payload_len))
            .ok_or_else(|| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!(
                    "embedded model bundle lengths exceed executable size: executable={file_len} gpu={gpu_len} cpu={cpu_len} whisper={whisper_len}"
                ),
            })?;
        let (offset, length) = match self.id {
            RTDETR_V2_S_COCO_ONNX_ID => (payload_start, gpu_len),
            RTDETR_V2_S_COCO_INT8_ONNX_ID => (payload_start + gpu_len, cpu_len),
            WHISPER_TINY_INT8_ONNX_ID => (payload_start + gpu_len + cpu_len, whisper_len),
            _ => {
                return Err(ModelError::LoadFailed {
                    path: executable,
                    detail: format!("model {} has no executable bundle slot", self.id),
                });
            }
        };
        let length = usize::try_from(length).map_err(|error| ModelError::LoadFailed {
            path: executable.clone(),
            detail: format!("embedded model payload is too large for this process: {error}"),
        })?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!("failed to seek to embedded model payload: {error}"),
            })?;
        let mut bytes = vec![0_u8; length];
        file.read_exact(&mut bytes)
            .map_err(|error| ModelError::LoadFailed {
                path: executable,
                detail: format!("failed to read embedded model payload: {error}"),
            })?;
        Ok(bytes)
    }

    /// Materializes the executable-embedded model once and verifies the exact
    /// bytes at the runtime Source of Truth before returning its descriptor.
    ///
    /// # Errors
    ///
    /// Returns a structured model-load error when the embedded bytes or the
    /// materialized file do not match the pinned registry hash.
    pub fn materialize_embedded(self) -> ModelResult<ModelDescriptor> {
        let descriptor = self.descriptor();
        let expected = normalize_sha256(self.sha256);
        if descriptor.path.exists()
            && sha256_file(&descriptor.path).ok().as_deref() == Some(expected.as_str())
        {
            return Ok(descriptor);
        }
        let bytes = self.embedded_bytes()?;
        let mut embedded_actual = String::with_capacity(64);
        for byte in sha2::Sha256::digest(&bytes) {
            write!(&mut embedded_actual, "{byte:02x}").map_err(|error| ModelError::LoadFailed {
                path: descriptor.path.clone(),
                detail: format!("failed to encode embedded model digest: {error}"),
            })?;
        }
        if embedded_actual != expected {
            return Err(ModelError::HashMismatch {
                path: descriptor.path,
                expected,
                actual: embedded_actual,
            });
        }
        if let Some(parent) = descriptor.path.parent() {
            fs::create_dir_all(parent).map_err(|error| ModelError::LoadFailed {
                path: descriptor.path.clone(),
                detail: format!("failed to create embedded model directory: {error}"),
            })?;
        }
        let temporary = descriptor
            .path
            .with_extension(format!("onnx.materialize-{}", std::process::id()));
        let mut file = fs::File::create(&temporary).map_err(|error| ModelError::LoadFailed {
            path: temporary.clone(),
            detail: format!("failed to create embedded model staging file: {error}"),
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| ModelError::LoadFailed {
                path: temporary.clone(),
                detail: format!("failed to durably stage embedded model: {error}"),
            })?;
        if descriptor.path.exists() {
            fs::remove_file(&descriptor.path).map_err(|error| ModelError::LoadFailed {
                path: descriptor.path.clone(),
                detail: format!("failed to replace corrupt materialized model: {error}"),
            })?;
        }
        fs::rename(&temporary, &descriptor.path).map_err(|error| ModelError::LoadFailed {
            path: descriptor.path.clone(),
            detail: format!("failed to commit embedded model materialization: {error}"),
        })?;
        let actual = sha256_file(&descriptor.path).map_err(|error| ModelError::LoadFailed {
            path: descriptor.path.clone(),
            detail: format!("failed to read back materialized embedded model: {error}"),
        })?;
        if actual != expected {
            return Err(ModelError::HashMismatch {
                path: descriptor.path,
                expected,
                actual,
            });
        }
        Ok(descriptor)
    }
}

pub const RTDETR_V2_S_COCO_ONNX: RegisteredModel = RegisteredModel {
    id: RTDETR_V2_S_COCO_ONNX_ID,
    label: "RT-DETRv2-S COCO ONNX",
    filename: RTDETR_V2_S_COCO_ONNX_FILENAME,
    sha256: RTDETR_V2_S_COCO_ONNX_SHA256,
    download_url: RTDETR_V2_S_COCO_ONNX_DOWNLOAD_URL,
    license_spdx: RTDETR_V2_S_COCO_ONNX_LICENSE,
    source_model: RTDETR_V2_S_COCO_ONNX_SOURCE_MODEL,
    source_repo: RTDETR_V2_S_COCO_ONNX_SOURCE_REPO,
    input_shape: DEFAULT_DETECTION_INPUT_SHAPE,
    class_map: &COCO80_CLASS_MAP,
};

pub const RTDETR_V2_S_COCO_INT8_ONNX: RegisteredModel = RegisteredModel {
    id: RTDETR_V2_S_COCO_INT8_ONNX_ID,
    label: "RT-DETRv2-S COCO INT8 CPU ONNX",
    filename: RTDETR_V2_S_COCO_INT8_ONNX_FILENAME,
    sha256: RTDETR_V2_S_COCO_INT8_ONNX_SHA256,
    download_url: RTDETR_V2_S_COCO_INT8_ONNX_DOWNLOAD_URL,
    license_spdx: RTDETR_V2_S_COCO_ONNX_LICENSE,
    source_model: RTDETR_V2_S_COCO_ONNX_SOURCE_MODEL,
    source_repo: RTDETR_V2_S_COCO_ONNX_SOURCE_REPO,
    input_shape: DEFAULT_DETECTION_INPUT_SHAPE,
    class_map: &COCO80_CLASS_MAP,
};

pub const WHISPER_TINY_INT8_ONNX: RegisteredModel = RegisteredModel {
    id: WHISPER_TINY_INT8_ONNX_ID,
    label: "Whisper Tiny English INT8 CPU ONNX",
    filename: WHISPER_TINY_INT8_ONNX_FILENAME,
    sha256: WHISPER_TINY_INT8_ONNX_SHA256,
    download_url: "bundled://synapse/whisper-tiny-int8",
    license_spdx: "MIT",
    source_model: "openai/whisper-tiny.en",
    source_repo: "https://github.com/microsoft/Olive",
    input_shape: [1, 0, 0, 0],
    class_map: &[],
};

pub const REGISTERED_MODELS: &[RegisteredModel] = &[
    RTDETR_V2_S_COCO_ONNX,
    RTDETR_V2_S_COCO_INT8_ONNX,
    WHISPER_TINY_INT8_ONNX,
];

#[must_use]
pub const fn lightweight_cpu_detection_model() -> RegisteredModel {
    RTDETR_V2_S_COCO_INT8_ONNX
}

#[must_use]
pub const fn default_detection_model() -> RegisteredModel {
    RTDETR_V2_S_COCO_ONNX
}

#[must_use]
pub fn default_detection_model_descriptor() -> ModelDescriptor {
    default_detection_model().descriptor()
}

#[must_use]
pub fn registered_model(id: &str) -> Option<RegisteredModel> {
    REGISTERED_MODELS
        .iter()
        .copied()
        .find(|model| model.id == id)
}
