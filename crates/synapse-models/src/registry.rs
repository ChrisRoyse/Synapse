use crate::{ModelDescriptor, default_model_dir};
use crate::{ModelError, ModelResult, normalize_sha256, sha256_file};
use sha2::Digest;
use std::{
    fmt::Write as _,
    fs,
    io::{Read, Seek, SeekFrom, Write},
};

/// Legacy fixed-shape trailer: three mandatory `u64` lengths and no digests.
///
/// It cannot express an absent optional model, which is exactly the defect that
/// made a disabled STT capability fail the entire install (#1863). It is still
/// recognized so an old binary produces a precise remediation instead of a
/// misleading "magic is corrupt".
const EMBEDDED_MODEL_BUNDLE_MAGIC_V1: &[u8; 16] = b"SYNMODEL_BUNDLE1";

/// Self-describing slot table (#1863).
///
/// Layout, appended after the base image:
///
/// ```text
/// [slot payloads, concatenated in slot-table order]
/// [slot table: for each slot]
///     u64  length_le            (0 == the model is positively ABSENT)
///     [32] sha256 raw digest    (all-zero when absent)
/// u32  slot_count_le
/// u32  format_version_le  (= 2)
/// [16] magic "SYNMODEL_BUNDLE2"
/// ```
///
/// Two properties matter. Absence is *recorded*, not inferred from a missing
/// payload, so "this build has no STT model" and "this build is truncated" are
/// distinguishable. And each slot carries its own digest, so the executable
/// states what it contains instead of relying on a second hardcoded copy of the
/// hashes in the installer, which is where pin drift comes from.
const EMBEDDED_MODEL_BUNDLE_MAGIC_V2: &[u8; 16] = b"SYNMODEL_BUNDLE2";
const EMBEDDED_MODEL_BUNDLE_FORMAT_VERSION: u32 = 2;
const EMBEDDED_MODEL_BUNDLE_MAGIC_SIZE: usize = 16;
/// Same value as [`EMBEDDED_MODEL_BUNDLE_MAGIC_SIZE`], typed for offset math so
/// seeks stay in `u64` and need no lossy casts.
const EMBEDDED_MODEL_BUNDLE_MAGIC_LEN: u64 = 16;
const EMBEDDED_MODEL_BUNDLE_SLOT_SIZE: usize = 40;
/// Same value as [`EMBEDDED_MODEL_BUNDLE_SLOT_SIZE`], typed for offset math.
const EMBEDDED_MODEL_BUNDLE_SLOT_LEN: u64 = 40;
/// `slot_count` + `format_version` + magic.
const EMBEDDED_MODEL_BUNDLE_FOOTER_SIZE: u64 = 4 + 4 + 16;
/// Refuses an absurd slot count before any allocation is sized from it.
const EMBEDDED_MODEL_BUNDLE_MAX_SLOTS: u32 = 64;

/// One packaged (or positively absent) model inside the executable bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddedModelSlot {
    /// Registry id this slot position corresponds to.
    pub id: &'static str,
    /// Absolute offset of the payload inside the executable. Meaningless when
    /// `length` is zero.
    pub offset: u64,
    /// Payload length; `0` means the model is not packaged in this build.
    pub length: u64,
    /// Lowercase hex digest recorded by the packager; `None` when absent.
    pub sha256: Option<String>,
}

impl EmbeddedModelSlot {
    #[must_use]
    pub const fn is_present(&self) -> bool {
        self.length > 0
    }
}

/// The decoded embedded model bundle of the running executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddedModelBundle {
    pub executable: std::path::PathBuf,
    pub executable_len: u64,
    pub base_len: u64,
    pub slots: Vec<EmbeddedModelSlot>,
}

impl EmbeddedModelBundle {
    #[must_use]
    pub fn slot(&self, id: &str) -> Option<&EmbeddedModelSlot> {
        self.slots.iter().find(|slot| slot.id == id)
    }
}

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
/// Pinned digest of the optional end-to-end STT artifact.
///
/// MUST equal `sha256` in `models/whisper-tiny-int8.pin.json`, which is what the
/// installer enforces when it packages the model.
/// `scripts/build-whisper-e2e-onnx.ps1` rewrites both together, and
/// `scripts/synapse-setup.ps1` fails closed if they ever drift apart, so the
/// daemon and the installer can never disagree about which bytes are legitimate
/// (#1863).
pub const WHISPER_TINY_INT8_ONNX_SHA256: &str =
    "sha256:f43e21f9aaa360ebcf94270a83c900a8ce69d2f402040d30e05cfaa73074602f";
pub const WHISPER_TINY_INT8_ONNX_LENGTH: u64 = 77_318_368;
pub const ORT_EXTENSIONS_WHISPER_FILENAME: &str = "onnxruntime_extensions.dll";
pub const ORT_EXTENSIONS_WHISPER_SHA256: &str =
    "sha256:0a0acea84ac7d90e5b6e81a6c37e19b8c356bd19809d44839ef945b3f2fee353";
pub const ORT_EXTENSIONS_WHISPER_LENGTH: u64 = 3_333_664;
/// How the optional STT artifact is acquired.
///
/// Deliberately not a URL: no public repository publishes the ONNX Runtime
/// Extensions end-to-end Whisper graph this runtime requires, so the honest
/// acquisition path is the committed recipe that regenerates it.
pub const WHISPER_TINY_INT8_ONNX_RECIPE: &str = "recipe://scripts/build-whisper-e2e-onnx.ps1";

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
    /// Whether the install is allowed to complete without this model (#1863).
    ///
    /// A required model missing is a build defect and must fail the package
    /// step. An optional model missing is a capability gap: the install
    /// completes and the dependent subsystem reports itself unavailable with
    /// the exact reason, never silently degraded.
    pub required: bool,
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
    /// The payload is verified against the digest the packager recorded in the
    /// slot table before it is returned, so a truncated or tampered payload is
    /// refused here rather than reaching ONNX Runtime.
    ///
    /// # Errors
    ///
    /// - [`ModelError::EmbeddedSlotAbsent`] when this model is optional and the
    ///   slot table positively records it as not packaged.
    /// - [`ModelError::EmbeddedBundleLegacyFormat`] for a pre-#1863 bundle.
    /// - [`ModelError::HashMismatch`] when the payload does not match the
    ///   recorded digest.
    /// - [`ModelError::LoadFailed`] when the bundle is missing, structurally
    ///   invalid, or unreadable.
    pub fn embedded_bytes(self) -> ModelResult<Vec<u8>> {
        let bundle = embedded_model_bundle()?;
        let executable = bundle.executable.clone();
        let slot = bundle
            .slot(self.id)
            .ok_or_else(|| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!(
                    "model {} has no executable bundle slot; bundle slots: {:?}",
                    self.id,
                    bundle.slots.iter().map(|slot| slot.id).collect::<Vec<_>>()
                ),
            })?
            .clone();
        if !slot.is_present() {
            return Err(ModelError::EmbeddedSlotAbsent {
                id: self.id.to_owned(),
                detail: format!(
                    "the slot table in {} records `{}` ({}) as not packaged in this build",
                    executable.display(),
                    self.id,
                    self.label
                ),
                remediation: self.acquisition_remediation(),
            });
        }
        let mut file = fs::File::open(&executable).map_err(|error| ModelError::LoadFailed {
            path: executable.clone(),
            detail: format!("failed to open running executable model bundle: {error}"),
        })?;
        let length = usize::try_from(slot.length).map_err(|error| ModelError::LoadFailed {
            path: executable.clone(),
            detail: format!("embedded model payload is too large for this process: {error}"),
        })?;
        let offset = slot.offset;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!("failed to seek to embedded model payload: {error}"),
            })?;
        let mut bytes = vec![0_u8; length];
        file.read_exact(&mut bytes)
            .map_err(|error| ModelError::LoadFailed {
                path: executable.clone(),
                detail: format!("failed to read embedded model payload: {error}"),
            })?;
        // The slot table states what the packager wrote. Verify the payload
        // against it here so truncation or tampering is caught at the boundary
        // rather than surfacing later as an opaque ONNX Runtime parse failure.
        let recorded = slot.sha256.ok_or_else(|| ModelError::LoadFailed {
            path: executable.clone(),
            detail: format!("bundle slot `{}` is present but records no digest", self.id),
        })?;
        let actual = hex_digest(&bytes)?;
        if actual != recorded {
            return Err(ModelError::HashMismatch {
                path: executable,
                expected: recorded,
                actual,
            });
        }
        Ok(bytes)
    }

    /// Operator-facing remediation for acquiring this model (#1863).
    #[must_use]
    fn acquisition_remediation(self) -> String {
        if self.id == WHISPER_TINY_INT8_ONNX_ID {
            format!(
                "audio speech-to-text is an optional capability and this build has no STT model. \
                 Produce the end-to-end artifact with `scripts/build-whisper-e2e-onnx.ps1` \
                 (source model {}, recipe {}), then re-run scripts/synapse-setup.ps1 with \
                 SYNAPSE_WHISPER_ONNX_SOURCE pointing at the produced file so it is packaged and \
                 pinned",
                self.source_model, self.source_repo
            )
        } else {
            format!(
                "re-run scripts/synapse-setup.ps1 so the pinned model {} ({}) is downloaded, \
                 verified and packaged into the executable",
                self.id, self.download_url
            )
        }
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

fn hex_digest(bytes: &[u8]) -> ModelResult<String> {
    let mut encoded = String::with_capacity(64);
    for byte in sha2::Sha256::digest(bytes) {
        write!(&mut encoded, "{byte:02x}").map_err(|error| ModelError::LoadFailed {
            path: std::path::PathBuf::new(),
            detail: format!("failed to encode digest: {error}"),
        })?;
    }
    Ok(encoded)
}

/// Confirms the trailing magic identifies a bundle this build can read.
///
/// A pre-#1863 image is named explicitly rather than being reported as "no
/// bundle", because those are different operator problems: one needs
/// repackaging, the other means the binary was never packaged at all.
fn assert_bundle_magic(
    executable: &std::path::Path,
    file: &mut fs::File,
    executable_len: u64,
) -> ModelResult<()> {
    let mut magic = [0_u8; EMBEDDED_MODEL_BUNDLE_MAGIC_SIZE];
    file.seek(SeekFrom::Start(
        executable_len - EMBEDDED_MODEL_BUNDLE_MAGIC_LEN,
    ))
    .and_then(|_| file.read_exact(&mut magic))
    .map_err(|error| ModelError::LoadFailed {
        path: executable.to_path_buf(),
        detail: format!("failed to read model bundle magic: {error}"),
    })?;
    if &magic == EMBEDDED_MODEL_BUNDLE_MAGIC_V1 {
        return Err(ModelError::EmbeddedBundleLegacyFormat {
            format: "SYNMODEL_BUNDLE1".to_owned(),
            remediation: format!(
                "{} was packaged by a pre-#1863 installer whose bundle cannot express optional \
                 models; re-run scripts/synapse-setup.ps1 to repackage it",
                executable.display()
            ),
        });
    }
    if &magic != EMBEDDED_MODEL_BUNDLE_MAGIC_V2 {
        return Err(ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: "executable carries no embedded model bundle (magic absent); it was not \
                     packaged by scripts/synapse-setup.ps1"
                .to_owned(),
        });
    }
    Ok(())
}

/// Reads the bundle footer and validates the declared slot count.
///
/// Returns the slot count only when it is in range AND matches this build's
/// [`REGISTERED_MODELS`]. A mismatch means the installer and the daemon are from
/// different versions, which would silently mis-map payloads because slots are
/// read positionally — so it fails closed rather than guessing.
fn read_bundle_slot_count(
    executable: &std::path::Path,
    file: &mut fs::File,
    executable_len: u64,
) -> ModelResult<u32> {
    let mut footer = [0_u8; 8];
    file.seek(SeekFrom::Start(
        executable_len - EMBEDDED_MODEL_BUNDLE_FOOTER_SIZE,
    ))
    .and_then(|_| file.read_exact(&mut footer))
    .map_err(|error| ModelError::LoadFailed {
        path: executable.to_path_buf(),
        detail: format!("failed to read model bundle footer: {error}"),
    })?;
    let slot_count = u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]);
    let format_version = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
    if format_version != EMBEDDED_MODEL_BUNDLE_FORMAT_VERSION {
        return Err(ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!(
                "unsupported embedded model bundle format version {format_version} (this build \
                 understands {EMBEDDED_MODEL_BUNDLE_FORMAT_VERSION}); re-run \
                 scripts/synapse-setup.ps1 with a matching installer"
            ),
        });
    }
    if slot_count == 0 || slot_count > EMBEDDED_MODEL_BUNDLE_MAX_SLOTS {
        return Err(ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!(
                "embedded model bundle declares {slot_count} slots, outside the supported range \
                 1..={EMBEDDED_MODEL_BUNDLE_MAX_SLOTS}"
            ),
        });
    }
    let slot_count_usize = usize::try_from(slot_count).map_err(|error| ModelError::LoadFailed {
        path: executable.to_path_buf(),
        detail: format!("embedded model bundle slot count is unrepresentable: {error}"),
    })?;
    if slot_count_usize != REGISTERED_MODELS.len() {
        return Err(ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!(
                "embedded model bundle declares {slot_count} slots but this build registers {} \
                 models; the installer and the daemon are from different versions — re-run \
                 scripts/synapse-setup.ps1 from this checkout",
                REGISTERED_MODELS.len()
            ),
        });
    }
    Ok(slot_count)
}

/// Per-slot values decoded from the bundle's slot table.
struct DecodedSlotTable {
    /// Payload byte length per slot; `0` means positively absent.
    lengths: Vec<u64>,
    /// Lowercase hex digest per slot; `None` for an absent slot.
    digests: Vec<Option<String>>,
    /// Sum of all slot lengths, used to locate the base image.
    payload_len: u64,
}

/// Decodes the fixed-width slot table.
///
/// `table` must be exactly `slot_count * EMBEDDED_MODEL_BUNDLE_SLOT_SIZE` bytes;
/// the caller sizes it from the declared slot count, so the indexing below is
/// bounded by construction.
fn decode_slot_table(
    executable: &std::path::Path,
    table: &[u8],
    slot_count: usize,
) -> ModelResult<DecodedSlotTable> {
    let mut lengths = Vec::with_capacity(slot_count);
    let mut digests = Vec::with_capacity(slot_count);
    let mut payload_len = 0_u64;
    for index in 0..slot_count {
        let base = index * EMBEDDED_MODEL_BUNDLE_SLOT_SIZE;
        let mut length_bytes = [0_u8; 8];
        length_bytes.copy_from_slice(&table[base..base + 8]);
        let length = u64::from_le_bytes(length_bytes);
        payload_len = payload_len
            .checked_add(length)
            .ok_or_else(|| ModelError::LoadFailed {
                path: executable.to_path_buf(),
                detail: "embedded model bundle payload lengths overflow".to_owned(),
            })?;
        let digest = &table[base + 8..base + EMBEDDED_MODEL_BUNDLE_SLOT_SIZE];
        digests.push(if length == 0 {
            None
        } else {
            Some(
                digest
                    .iter()
                    .fold(String::with_capacity(64), |mut accumulator, byte| {
                        let _ = write!(&mut accumulator, "{byte:02x}");
                        accumulator
                    }),
            )
        });
        lengths.push(length);
    }
    Ok(DecodedSlotTable {
        lengths,
        digests,
        payload_len,
    })
}

/// Decodes the embedded model bundle appended to the running executable.
///
/// Slot positions map onto [`REGISTERED_MODELS`] in declaration order, so the
/// table is read positionally and every entry keeps its registry id. A slot with
/// `length == 0` is a *positive statement of absence* written by the packager,
/// which is what lets an optional capability be missing without the whole
/// bundle being invalid (#1863).
///
/// # Errors
///
/// Returns [`ModelError::EmbeddedBundleLegacyFormat`] for a pre-#1863 bundle and
/// [`ModelError::LoadFailed`] when the executable carries no bundle or the slot
/// table is structurally impossible for the file's size.
pub fn embedded_model_bundle() -> ModelResult<EmbeddedModelBundle> {
    let executable = std::env::current_exe().map_err(|error| ModelError::LoadFailed {
        path: std::path::PathBuf::new(),
        detail: format!("failed to resolve running executable for embedded model: {error}"),
    })?;
    embedded_model_bundle_at(&executable)
}

/// Decodes the embedded model bundle of an arbitrary executable.
///
/// Used by the running daemon for its own image and by diagnostics that must
/// inspect a candidate binary before it is installed.
///
/// # Errors
///
/// See [`embedded_model_bundle`].
pub fn embedded_model_bundle_at(executable: &std::path::Path) -> ModelResult<EmbeddedModelBundle> {
    let mut file = fs::File::open(executable).map_err(|error| ModelError::LoadFailed {
        path: executable.to_path_buf(),
        detail: format!("failed to open executable model bundle: {error}"),
    })?;
    let executable_len = file
        .metadata()
        .map_err(|error| ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!("failed to inspect executable model bundle: {error}"),
        })?
        .len();
    if executable_len < EMBEDDED_MODEL_BUNDLE_FOOTER_SIZE {
        return Err(ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!(
                "executable is {executable_len} bytes, too small to carry a model bundle footer"
            ),
        });
    }

    assert_bundle_magic(executable, &mut file, executable_len)?;

    // The helper proves the declared count equals REGISTERED_MODELS.len(), so
    // that is the authoritative slot count from here on.
    let slot_count = read_bundle_slot_count(executable, &mut file, executable_len)?;
    let slot_count_usize = REGISTERED_MODELS.len();

    let table_len = u64::from(slot_count) * EMBEDDED_MODEL_BUNDLE_SLOT_LEN;
    let trailer_len = table_len + EMBEDDED_MODEL_BUNDLE_FOOTER_SIZE;
    let table_start =
        executable_len
            .checked_sub(trailer_len)
            .ok_or_else(|| ModelError::LoadFailed {
                path: executable.to_path_buf(),
                detail: format!(
                    "embedded model bundle slot table ({trailer_len} bytes) does not fit in a \
                     {executable_len}-byte executable"
                ),
            })?;
    let table_len_usize = usize::try_from(table_len).map_err(|error| ModelError::LoadFailed {
        path: executable.to_path_buf(),
        detail: format!("embedded model bundle slot table is too large: {error}"),
    })?;
    let mut table = vec![0_u8; table_len_usize];
    file.seek(SeekFrom::Start(table_start))
        .and_then(|_| file.read_exact(&mut table))
        .map_err(|error| ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!("failed to read model bundle slot table: {error}"),
        })?;

    let DecodedSlotTable {
        lengths,
        digests,
        payload_len,
    } = decode_slot_table(executable, &table, slot_count_usize)?;

    let base_len = table_start
        .checked_sub(payload_len)
        .ok_or_else(|| ModelError::LoadFailed {
            path: executable.to_path_buf(),
            detail: format!(
                "embedded model bundle payloads ({payload_len} bytes) plus trailer \
                 ({trailer_len} bytes) exceed the {executable_len}-byte executable; the image is \
                 truncated — re-run scripts/synapse-setup.ps1"
            ),
        })?;

    let mut offset = base_len;
    let mut slots = Vec::with_capacity(slot_count_usize);
    for (index, model) in REGISTERED_MODELS.iter().enumerate() {
        let length = lengths[index];
        slots.push(EmbeddedModelSlot {
            id: model.id,
            offset,
            length,
            sha256: digests[index].clone(),
        });
        offset += length;
    }

    Ok(EmbeddedModelBundle {
        executable: executable.to_path_buf(),
        executable_len,
        base_len,
        slots,
    })
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
    required: true,
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
    required: true,
};

/// Optional end-to-end speech-to-text model (#1863).
///
/// This is not an off-the-shelf `HuggingFace` export. The runtime feeds it raw
/// container bytes on `audio_stream` and reads decoded text from `str`, which is
/// the shape produced by the ONNX Runtime Extensions Whisper end-to-end recipe
/// (audio decode + log-mel + beam search + BPE detokenize fused into one graph).
/// No public repository publishes that artifact, so `download_url` names the
/// committed recipe that regenerates it rather than a fictitious direct link.
/// Audio is off by default (`--enable-audio`), so a build without it installs
/// successfully and reports STT unavailable with this remediation.
pub const WHISPER_TINY_INT8_ONNX: RegisteredModel = RegisteredModel {
    id: WHISPER_TINY_INT8_ONNX_ID,
    label: "Whisper Tiny English INT8 CPU ONNX (end-to-end)",
    filename: WHISPER_TINY_INT8_ONNX_FILENAME,
    sha256: WHISPER_TINY_INT8_ONNX_SHA256,
    download_url: WHISPER_TINY_INT8_ONNX_RECIPE,
    license_spdx: "MIT",
    source_model: "openai/whisper-tiny.en",
    source_repo: "https://github.com/microsoft/onnxruntime-extensions",
    input_shape: [1, 0, 0, 0],
    class_map: &[],
    required: false,
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
