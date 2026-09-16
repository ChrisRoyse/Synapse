use crate::{ModelDescriptor, default_model_dir};

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

pub const REGISTERED_MODELS: &[RegisteredModel] = &[RTDETR_V2_S_COCO_ONNX];

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
