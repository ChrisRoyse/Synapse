use chrono::{DateTime, Utc};
use rmcp::ErrorData;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::OnceLock,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};
use synapse_calyx::{SynapseCalyxGpuReservation, readback_gpu_reservations};
use synapse_core::{
    DetectedEntity, Detection, DetectionBatch, PerceptionMode, ProfileDetection, Rect,
    SensorStatus, entity_id, error_codes,
};
use synapse_models::{
    DEFAULT_DETECTION_MODEL_ID, DetectOpts, DetectionFrame, Detector, ModelBackend, ModelLoader,
    lightweight_cpu_detection_model, registered_model,
};

const DEFAULT_DETECTION_CONFIDENCE_THRESHOLD: f32 = 0.5;
const STALE_TRACK_MS: i64 = 3_000;
const MIN_TRACK_MATCH_DISTANCE_PX: f32 = 96.0;
const DETECTION_GPU_ADMISSION_MIB: u64 = 4_096;
const DETECTION_WORKER_TIMEOUT_MS: u32 = 120_000;
const DETECTION_WORKER_SHUTDOWN_TIMEOUT_MS: u32 = 5_000;
const DETECTION_WORKER_POLL_MS: u64 = 2;
const DETECTION_WORKER_PROTOCOL: &str = "synapse.detection.worker.v1";
const DETECTION_BACKEND_ENV: &str = "SYNAPSE_DETECTION_BACKEND";

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

/// Which configuration fault stops detection inference (#2054, #2064).
///
/// The two are not interchangeable and must never share a wire label.
/// `NotConfigured` is a profile that deliberately runs no detector: the observe
/// completes and reports `SensorStatus::NotConfigured`. `Misconfigured` is a
/// profile that names a detector this daemon cannot load: every observe in a
/// pixel-bearing mode *fails*. Health reported the second as `configured` until
/// #2064 — with `configured_model_registered: false` as the only tell, a field a
/// reader had to already suspect to look at — which is a health surface claiming
/// a capability that errors on first use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetectionFaultKind {
    NotConfigured,
    Misconfigured,
}

impl DetectionFaultKind {
    /// The `perception.detection.status` wire label. Deliberately never `ok` or
    /// `healthy`: those are the words #2054 exists to stop being reused here.
    #[must_use]
    pub const fn status(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Misconfigured => "misconfigured",
        }
    }
}

/// Why the detection stage will perform no successful model inference (#2054).
///
/// Carried onto `diagnostics.detection_status` as
/// `SensorStatus::NotConfigured` and into `health.subsystems.perception`, so
/// both surfaces name the same cause with the same remediation instead of
/// holding two independent opinions about whether a detector runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectionInferenceFault {
    pub kind: DetectionFaultKind,
    pub reason_code: String,
    pub detail: String,
    pub remediation: String,
}

/// Decides whether the configured detector would actually run inference.
///
/// Pure over [`DetectionRuntimeConfig`]: `None` means an observe in a
/// pixel-bearing perception mode performs real model inference, `Some` names
/// why it does not. Both the observe path and the `health` tool call this, so a
/// health reader and an observation can never disagree.
#[must_use]
pub fn detection_inference_gate(
    config: &DetectionRuntimeConfig,
) -> Option<DetectionInferenceFault> {
    let remediation = format!(
        "set [detection].model_id to a registered detector ({}) and [detection].max_detections>0 in the active profile, then re-apply the profile",
        registered_detection_model_ids().join(" | ")
    );
    let mut causes = Vec::new();
    if config.model_id.is_none() {
        causes.push("the active profile declares no [detection].model_id");
    }
    if config.max_detections == 0 {
        causes.push("the active profile declares [detection].max_detections=0");
    }
    if !causes.is_empty() {
        return Some(DetectionInferenceFault {
            kind: DetectionFaultKind::NotConfigured,
            reason_code: error_codes::DETECTION_NOT_CONFIGURED.to_owned(),
            detail: format!(
                "no detector inference ran: {}; effective detection config model_id={} max_detections={} confidence_threshold={}",
                causes.join(" and "),
                config.model_id.as_deref().unwrap_or("<none>"),
                config.max_detections,
                config.confidence_threshold
            ),
            remediation,
        });
    }
    // #2064: a named-but-unloadable detector is the third state. It is not
    // `not_configured` (the operator did ask for inference) and it is emphatically
    // not `configured` (nothing can run). Resolved through exactly the id set the
    // remediation above advertises, which is the same set the detection worker
    // resolves against, so health cannot call loadable what the worker rejects.
    let model_id = config.model_id.as_deref()?;
    if loadable_detection_model(model_id) {
        return None;
    }
    Some(DetectionInferenceFault {
        kind: DetectionFaultKind::Misconfigured,
        reason_code: error_codes::DETECTION_MODEL_NOT_LOADED.to_owned(),
        detail: format!(
            "the active profile names detector model_id={model_id:?}, which this daemon cannot load: it resolves to no registered detector. Every observe in a pixel-bearing perception mode fails; effective detection config max_detections={} confidence_threshold={}",
            config.max_detections, config.confidence_threshold
        ),
        remediation,
    })
}

/// Whether the detection worker would resolve this id to a loadable detector.
///
/// Membership in [`registered_detection_model_ids`], not bare registry
/// membership: a registered *non-detector* (the ASR model shares the registry)
/// is just as unloadable here, and it is an id an operator can plausibly
/// mistype into `[detection].model_id`.
fn loadable_detection_model(model_id: &str) -> bool {
    registered_detection_model_ids().contains(&model_id)
}

/// Registered detector ids an operator may name in `[detection].model_id`.
///
/// Read from the model registry rather than hard-coded so the remediation text
/// can never advertise an id the daemon would reject.
#[must_use]
pub fn registered_detection_model_ids() -> Vec<&'static str> {
    synapse_models::REGISTERED_MODELS
        .iter()
        .filter(|model| !model.class_map.is_empty())
        .map(|model| model.id)
        .collect()
}

#[derive(Debug, Default)]
pub struct DetectionRuntime {
    tracker: EntityTracker,
    next_frame_seq: u64,
    #[cfg(windows)]
    worker: Option<PersistentDetectionWorker>,
}

impl DetectionRuntime {
    fn next_frame_seq(&mut self) -> u64 {
        self.next_frame_seq = self.next_frame_seq.saturating_add(1);
        self.next_frame_seq
    }

    #[must_use]
    pub fn persistent_worker_readback(&self) -> Option<DetectionWorkerRuntimeReadback> {
        #[cfg(windows)]
        {
            self.worker
                .as_ref()
                .map(PersistentDetectionWorker::readback)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectionWorkerRuntimeReadback {
    pub worker_pid: u32,
    pub model_id: String,
    pub backend: String,
    pub session_id: u64,
    pub requests_started: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DetectionWorkerRequest {
    #[serde(default)]
    request_id: u64,
    model_id: String,
    frame_seq: u64,
    width: u32,
    height: u32,
    rgb_path: PathBuf,
    progress_path: PathBuf,
    opts: DetectOpts,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DetectionWorkerEnvelope {
    request_id: u64,
    worker_pid: u32,
    session_id: u64,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DetectionWorkerReady {
    protocol: String,
    worker_pid: u32,
    model_id: String,
    backend: String,
    session_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation_id: Option<String>,
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
            request_id: 0,
            worker_pid: std::process::id(),
            session_id: 0,
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

pub(crate) fn run_detection_worker_from_process_args() -> Option<anyhow::Result<ExitCode>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let mode = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--mode").then(|| pair[1].to_string_lossy().into_owned()));
    if mode.as_deref() != Some("detection-worker") {
        return None;
    }
    let path_after = |flag: &str| {
        args.windows(2)
            .find_map(|pair| (pair[0] == flag).then(|| PathBuf::from(&pair[1])))
    };
    let string_after = |flag: &str| {
        args.windows(2)
            .find_map(|pair| (pair[0] == flag).then(|| pair[1].to_string_lossy().into_owned()))
    };
    if let Some(mailbox) = path_after("--detection-worker-mailbox") {
        return Some(
            string_after("--detection-worker-model-id")
                .ok_or_else(|| anyhow::anyhow!("--detection-worker-model-id is required"))
                .and_then(|model_id| run_persistent_detection_worker(&mailbox, &model_id)),
        );
    }
    Some(run_detection_worker_from_cli(
        path_after("--detection-worker-request"),
        path_after("--detection-worker-response"),
    ))
}

fn run_detection_worker(
    request_path: &std::path::Path,
) -> Result<DetectionWorkerEnvelope, (String, String)> {
    let request = read_detection_worker_request(request_path)?;
    write_worker_progress(&request.progress_path, "request_validated")?;
    let worker = load_detection_worker_model(&request.model_id, &request.progress_path)?;
    run_loaded_detection_request(&worker, request)
}

struct LoadedDetectionWorker {
    model_id: String,
    backend: ModelBackend,
    model: synapse_models::LoadedModel,
    _reservation: Option<SynapseCalyxGpuReservation>,
    reservation_id: Option<String>,
}

fn load_detection_worker_model(
    model_id: &str,
    progress_path: &Path,
) -> Result<LoadedDetectionWorker, (String, String)> {
    let backend = selected_detection_backend()?;
    let registered = if backend == ModelBackend::Cpu && model_id == DEFAULT_DETECTION_MODEL_ID {
        lightweight_cpu_detection_model()
    } else if model_id == DEFAULT_DETECTION_MODEL_ID {
        synapse_models::default_detection_model()
    } else {
        registered_model(model_id).ok_or_else(|| {
            (
                error_codes::DETECTION_MODEL_NOT_LOADED.to_owned(),
                format!("detection model id {model_id:?} is not registered"),
            )
        })?
    };
    let descriptor = registered
        .materialize_embedded_verified()
        .map_err(|error| (error.code().to_owned(), error.to_string()))?;
    write_worker_progress(progress_path, "model_verified")?;
    let reservation = if backend == ModelBackend::Cuda {
        let reservation = SynapseCalyxGpuReservation::acquire(
            0,
            "synapse-mcp-detection-worker",
            format!("synapse-detection-worker-pid-{}", std::process::id()),
            format!(
                "isolated ORT CUDA detector model={model_id}; declared_session_and_inference_envelope_mib={DETECTION_GPU_ADMISSION_MIB}"
            ),
            DETECTION_GPU_ADMISSION_MIB,
        )
        .map_err(|error| (error.code.to_owned(), error.to_string()))?;
        write_worker_progress(progress_path, "gpu_reservation_acquired")?;
        configure_cuda_runtime_dlls()?;
        write_worker_progress(progress_path, "cuda_runtime_verified")?;
        Some(reservation)
    } else {
        write_worker_progress(progress_path, "cpu_backend_selected")?;
        None
    };
    let reservation_id = reservation.as_ref().and_then(|reservation| {
        reservation
            .admitted_snapshot()
            .reservations
            .iter()
            .find(|row| row.pid == std::process::id())
            .map(|row| row.reservation_id.clone())
    });
    let loader = ModelLoader::new(vec![backend]);
    let model = loader
        .load_verified(descriptor)
        .map_err(|error| (error.code().to_owned(), error.to_string()))?;
    write_worker_progress(
        progress_path,
        if backend == ModelBackend::Cuda {
            "cuda_session_loaded"
        } else {
            "cpu_session_loaded"
        },
    )?;
    Ok(LoadedDetectionWorker {
        model_id: model_id.to_owned(),
        backend,
        model,
        _reservation: reservation,
        reservation_id,
    })
}

fn run_loaded_detection_request(
    worker: &LoadedDetectionWorker,
    request: DetectionWorkerRequest,
) -> Result<DetectionWorkerEnvelope, (String, String)> {
    if request.model_id != worker.model_id {
        return Err((
            "DETECTION_WORKER_MODEL_MISMATCH".to_owned(),
            format!(
                "persistent worker loaded model {:?}, but request {} named {:?}",
                worker.model_id, request.request_id, request.model_id
            ),
        ));
    }
    let rgb = fs::read(&request.rgb_path).map_err(|error| {
        (
            "DETECTION_WORKER_FRAME_READ_FAILED".to_owned(),
            error.to_string(),
        )
    })?;
    write_worker_progress(&request.progress_path, "frame_read")?;
    let batch = worker
        .model
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
    write_worker_progress(&request.progress_path, "inference_completed")?;
    Ok(DetectionWorkerEnvelope {
        request_id: request.request_id,
        worker_pid: std::process::id(),
        session_id: worker.model.session_id(),
        ok: true,
        batch: Some(batch),
        reservation_id: worker.reservation_id.clone(),
        error_code: None,
        error_detail: None,
    })
}

fn read_detection_worker_request(
    request_path: &Path,
) -> Result<DetectionWorkerRequest, (String, String)> {
    let request_bytes = fs::read(request_path).map_err(|error| {
        (
            "DETECTION_WORKER_REQUEST_READ_FAILED".to_owned(),
            error.to_string(),
        )
    })?;
    serde_json::from_slice(&request_bytes).map_err(|error| {
        (
            "DETECTION_WORKER_REQUEST_INVALID".to_owned(),
            error.to_string(),
        )
    })
}

fn write_worker_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), (String, String)> {
    use std::io::Write as _;

    if path.exists() {
        return Err((
            "DETECTION_WORKER_PROTOCOL_DIRTY".to_owned(),
            format!(
                "refusing to overwrite unread worker protocol file {}",
                path.display()
            ),
        ));
    }
    let staging = path.with_extension(format!("staging-{}", std::process::id()));
    let bytes = serde_json::to_vec(value).map_err(|error| {
        (
            "DETECTION_WORKER_PROTOCOL_ENCODE_FAILED".to_owned(),
            error.to_string(),
        )
    })?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staging)
        .map_err(|error| {
            (
                "DETECTION_WORKER_PROTOCOL_STAGE_FAILED".to_owned(),
                format!("create {}: {error}", staging.display()),
            )
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            (
                "DETECTION_WORKER_PROTOCOL_STAGE_FAILED".to_owned(),
                format!("write/sync {}: {error}", staging.display()),
            )
        })?;
    fs::rename(&staging, path).map_err(|error| {
        (
            "DETECTION_WORKER_PROTOCOL_COMMIT_FAILED".to_owned(),
            format!(
                "rename {} -> {}: {error}",
                staging.display(),
                path.display()
            ),
        )
    })
}

fn run_persistent_detection_worker(mailbox: &Path, model_id: &str) -> anyhow::Result<ExitCode> {
    if !mailbox.is_absolute() || !mailbox.is_dir() {
        anyhow::bail!(
            "detection worker mailbox must be an existing absolute directory: {}",
            mailbox.display()
        );
    }
    let ready_path = mailbox.join("ready.json");
    let request_path = mailbox.join("request.json");
    let response_path = mailbox.join("response.json");
    let shutdown_path = mailbox.join("shutdown.json");
    let progress_path = mailbox.join("progress.txt");
    for path in [&ready_path, &request_path, &response_path, &shutdown_path] {
        if path.exists() {
            anyhow::bail!(
                "detection worker mailbox contains stale protocol file {}",
                path.display()
            );
        }
    }
    let worker = load_detection_worker_model(model_id, &progress_path)
        .map_err(|(code, detail)| anyhow::anyhow!("{code}: {detail}"))?;
    let ready = DetectionWorkerReady {
        protocol: DETECTION_WORKER_PROTOCOL.to_owned(),
        worker_pid: std::process::id(),
        model_id: worker.model_id.clone(),
        backend: match worker.backend {
            ModelBackend::Cuda => "cuda",
            ModelBackend::Cpu => "cpu",
        }
        .to_owned(),
        session_id: worker.model.session_id(),
        reservation_id: worker.reservation_id.clone(),
    };
    write_worker_json_atomic(&ready_path, &ready)
        .map_err(|(code, detail)| anyhow::anyhow!("{code}: {detail}"))?;
    write_worker_progress(&progress_path, "persistent_worker_ready")
        .map_err(|(code, detail)| anyhow::anyhow!("{code}: {detail}"))?;

    loop {
        if shutdown_path.exists() {
            write_worker_progress(&progress_path, "shutdown_requested")
                .map_err(|(code, detail)| anyhow::anyhow!("{code}: {detail}"))?;
            drop(worker);
            return Ok(ExitCode::SUCCESS);
        }
        if response_path.exists() || !request_path.exists() {
            thread::sleep(Duration::from_millis(DETECTION_WORKER_POLL_MS));
            continue;
        }
        let request = read_detection_worker_request(&request_path)
            .map_err(|(code, detail)| anyhow::anyhow!("{code}: {detail}"))?;
        fs::remove_file(&request_path).map_err(|error| {
            anyhow::anyhow!(
                "DETECTION_WORKER_PROTOCOL_CLEANUP_FAILED: remove {}: {error}",
                request_path.display()
            )
        })?;
        let request_id = request.request_id;
        let envelope =
            run_loaded_detection_request(&worker, request).unwrap_or_else(|(code, detail)| {
                DetectionWorkerEnvelope {
                    request_id,
                    worker_pid: std::process::id(),
                    session_id: worker.model.session_id(),
                    ok: false,
                    batch: None,
                    reservation_id: worker.reservation_id.clone(),
                    error_code: Some(code),
                    error_detail: Some(detail),
                }
            });
        write_worker_json_atomic(&response_path, &envelope)
            .map_err(|(code, detail)| anyhow::anyhow!("{code}: {detail}"))?;
    }
}

fn selected_detection_backend() -> Result<ModelBackend, (String, String)> {
    static SELECTED: OnceLock<Result<ModelBackend, (String, String)>> = OnceLock::new();
    SELECTED.get_or_init(probe_detection_backend).clone()
}

fn probe_detection_backend() -> Result<ModelBackend, (String, String)> {
    match std::env::var(DETECTION_BACKEND_ENV) {
        Ok(value) if value.eq_ignore_ascii_case("cuda") => return Ok(ModelBackend::Cuda),
        Ok(value) if value.eq_ignore_ascii_case("cpu") => return Ok(ModelBackend::Cpu),
        Ok(value) if value.eq_ignore_ascii_case("auto") => {}
        Ok(value) => {
            return Err((
                "DETECTION_BACKEND_CONFIG_INVALID".to_owned(),
                format!("{DETECTION_BACKEND_ENV} must be auto, cuda, or cpu; got {value:?}"),
            ));
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(error) => {
            return Err((
                "DETECTION_BACKEND_CONFIG_INVALID".to_owned(),
                format!("{DETECTION_BACKEND_ENV} is not valid Unicode: {error}"),
            ));
        }
    }

    match readback_gpu_reservations(0) {
        Ok(snapshot) => Ok(
            if snapshot
                .reservations
                .iter()
                .any(|row| row.owner == "synapse-mcp")
            {
                ModelBackend::Cuda
            } else {
                ModelBackend::Cpu
            },
        ),
        Err(error) => {
            let detail = error.to_string();
            if detail.contains("NVML init failed loading")
                || (detail.contains("NVML device_by_index(0) failed")
                    && (detail.contains("Not Found") || detail.contains("No device")))
            {
                Ok(ModelBackend::Cpu)
            } else {
                Err((
                    "DETECTION_BACKEND_PROBE_FAILED".to_owned(),
                    format!("could not prove whether CUDA device 0 is absent or broken: {detail}"),
                ))
            }
        }
    }
}

/// Materialization state of the detector the executable bundles.
///
/// #2054: this describes what the daemon *could* load, not what the active
/// profile asks it to run. The two were reported as one `detection_model=...`
/// blob, which read as proof that a detector runs on every observe. Every
/// field here is now named `bundled_*` on the wire and paired with the active
/// profile's [`detection_inference_gate`] verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectionBundleReadback {
    pub provider: &'static str,
    pub model_id: &'static str,
    pub materialized: bool,
    pub materialized_verified: bool,
    pub materialized_bytes: Option<u64>,
    pub materialized_modified_unix_ms: Option<u64>,
    pub materialized_path: String,
}

pub(crate) fn detection_bundle_readback() -> Result<DetectionBundleReadback, (String, String)> {
    let backend = selected_detection_backend()?;
    let model = if backend == ModelBackend::Cpu {
        lightweight_cpu_detection_model()
    } else {
        synapse_models::default_detection_model()
    };
    let descriptor = model.descriptor();
    let metadata = fs::metadata(&descriptor.path)
        .ok()
        .filter(|row| row.is_file());
    let materialized = metadata.is_some();
    let expected_bytes = synapse_models::embedded_model_bundle()
        .map_err(|error| (error.code().to_owned(), error.to_string()))?
        .slot(model.id)
        .filter(|slot| slot.is_present())
        .map(|slot| slot.length)
        .ok_or_else(|| {
            (
                "DETECTION_BUNDLE_SLOT_ABSENT".to_owned(),
                format!(
                    "running executable has no populated model slot for {:?}; re-run scripts/synapse-setup.ps1",
                    model.id
                ),
            )
        })?;
    // Health is an availability probe, not the model-load integrity boundary.
    // It deliberately uses only cheap file identity metadata; the persistent
    // worker performs the full SHA-256 verification immediately before it
    // creates the one retained ONNX Runtime session.
    let materialized_verified = metadata
        .as_ref()
        .is_some_and(|row| row.len() == expected_bytes);
    let materialized_bytes = metadata.as_ref().map(std::fs::Metadata::len);
    let materialized_modified_unix_ms = metadata
        .as_ref()
        .and_then(|row| row.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
    Ok(DetectionBundleReadback {
        provider: match backend {
            ModelBackend::Cuda => "cuda",
            ModelBackend::Cpu => "cpu",
        },
        model_id: model.id,
        materialized,
        materialized_verified,
        materialized_bytes,
        materialized_modified_unix_ms,
        materialized_path: descriptor.path.display().to_string(),
    })
}

#[cfg(windows)]
fn configure_cuda_runtime_dlls() -> Result<(), (String, String)> {
    use windows::{Win32::System::LibraryLoader::LoadLibraryW, core::PCWSTR};

    const REQUIRED: &[&str] = &[
        "cudart64_12.dll",
        "cublas64_12.dll",
        "cublasLt64_12.dll",
        "cufft64_11.dll",
        "cudnn64_9.dll",
    ];
    let mut bins = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let python_root = PathBuf::from(appdata).join("Python");
        if let Ok(versions) = fs::read_dir(python_root) {
            for version in versions.flatten() {
                let nvidia = version.path().join("site-packages").join("nvidia");
                if let Ok(packages) = fs::read_dir(nvidia) {
                    for package in packages.flatten() {
                        let bin = package.path().join("bin");
                        if bin.is_dir() {
                            bins.push(bin);
                        }
                    }
                }
            }
        }
    }
    if let Some(cuda_path) = std::env::var_os("CUDA_PATH_V12_9") {
        let bin = PathBuf::from(cuda_path).join("bin");
        if bin.is_dir() {
            bins.push(bin);
        }
    }
    bins.sort();
    bins.dedup();
    let missing = REQUIRED
        .iter()
        .filter(|name| !bins.iter().any(|bin| bin.join(name).is_file()))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err((
            "DETECTION_CUDA_RUNTIME_MISSING".to_owned(),
            format!(
                "ONNX Runtime CUDA requires CUDA 12.x + cuDNN 9; missing DLLs [{}] across discovered runtime bins [{}]",
                missing.join(", "),
                bins.iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = bins.clone();
    paths.extend(std::env::split_paths(&inherited));
    let joined = std::env::join_paths(paths).map_err(|error| {
        (
            "DETECTION_CUDA_RUNTIME_PATH_INVALID".to_owned(),
            error.to_string(),
        )
    })?;
    // The detection worker is a dedicated single-threaded process at this
    // point, before ORT creates any threads or reads PATH.
    unsafe { std::env::set_var("PATH", joined) };
    for name in REQUIRED {
        let wide = name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.map_err(|error| {
            (
                "DETECTION_CUDA_RUNTIME_LOAD_FAILED".to_owned(),
                format!("LoadLibraryW({name}) failed after verified discovery: {error}"),
            )
        })?;
    }
    Ok(())
}

fn write_worker_progress(path: &std::path::Path, stage: &str) -> Result<(), (String, String)> {
    fs::write(path, stage).map_err(|error| {
        (
            "DETECTION_WORKER_PROGRESS_WRITE_FAILED".to_owned(),
            format!("write stage {stage:?} to {}: {error}", path.display()),
        )
    })
}

#[cfg(windows)]
#[derive(Debug)]
struct PersistentDetectionWorker {
    mailbox: tempfile::TempDir,
    process: crate::desktop_worker::OwnedWorkerProcess,
    model_id: String,
    backend: String,
    session_id: u64,
    reservation_id: Option<String>,
    next_request_id: u64,
}

#[cfg(windows)]
impl PersistentDetectionWorker {
    fn start(model_id: &str) -> synapse_models::ModelResult<Self> {
        use synapse_models::detection_model_not_loaded;

        let mailbox = tempfile::Builder::new()
            .prefix("synapse-detection-worker-")
            .tempdir()
            .map_err(|error| {
                detection_model_not_loaded(format!(
                    "create persistent detector mailbox failed: {error}"
                ))
            })?;
        let args = vec![
            "--mode".to_owned(),
            "detection-worker".to_owned(),
            "--detection-worker-mailbox".to_owned(),
            mailbox.path().to_string_lossy().into_owned(),
            "--detection-worker-model-id".to_owned(),
            model_id.to_owned(),
        ];
        let mut process =
            crate::desktop_worker::spawn_owned_current_exe_worker(&args).map_err(|error| {
                detection_model_not_loaded(format!(
                    "start persistent owned detector process failed: {error}"
                ))
            })?;
        let ready_path = mailbox.path().join("ready.json");
        let progress_path = mailbox.path().join("progress.txt");
        let started = Instant::now();
        let ready = loop {
            if ready_path.is_file() {
                let bytes = fs::read(&ready_path).map_err(|error| {
                    detection_model_not_loaded(format!(
                        "persistent detector pid {} ready read failed: {error}",
                        process.pid()
                    ))
                })?;
                let ready: DetectionWorkerReady =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        detection_model_not_loaded(format!(
                            "persistent detector pid {} ready JSON was invalid: {error}",
                            process.pid()
                        ))
                    })?;
                break ready;
            }
            if let Some(exit_code) = process.terminal_exit_code().map_err(|error| {
                detection_model_not_loaded(format!(
                    "persistent detector process status read failed: {error}"
                ))
            })? {
                let progress = fs::read_to_string(&progress_path)
                    .unwrap_or_else(|error| format!("unavailable ({error})"));
                return Err(detection_model_not_loaded(format!(
                    "persistent detector pid {} exited before ready with kernel exit_code={exit_code}; last_stage={progress:?}",
                    process.pid()
                )));
            }
            if started.elapsed() >= Duration::from_millis(u64::from(DETECTION_WORKER_TIMEOUT_MS)) {
                let progress = fs::read_to_string(&progress_path)
                    .unwrap_or_else(|error| format!("unavailable ({error})"));
                let verdict = process.wait_for_exit(0).map_err(|error| {
                    detection_model_not_loaded(format!(
                        "persistent detector startup timed out and exact cleanup failed: {error}; last_stage={progress:?}"
                    ))
                })?;
                return Err(detection_model_not_loaded(format!(
                    "persistent detector pid {} did not become ready within {DETECTION_WORKER_TIMEOUT_MS} ms and was terminated with kernel exit_code={}; last_stage={progress:?}",
                    verdict.pid, verdict.exit_code
                )));
            }
            thread::sleep(Duration::from_millis(DETECTION_WORKER_POLL_MS));
        };
        if ready.protocol != DETECTION_WORKER_PROTOCOL
            || ready.worker_pid != process.pid()
            || ready.model_id != model_id
            || !matches!(ready.backend.as_str(), "cpu" | "cuda")
            || ready.session_id == 0
            || (ready.backend == "cuda") != ready.reservation_id.is_some()
        {
            return Err(detection_model_not_loaded(format!(
                "persistent detector ready attestation mismatch: expected protocol={DETECTION_WORKER_PROTOCOL:?} pid={} model={model_id:?}; actual={ready:?}",
                process.pid()
            )));
        }
        verify_persistent_worker_reservation(
            ready.worker_pid,
            &ready.backend,
            ready.reservation_id.as_deref(),
            true,
        )
        .map_err(detection_model_not_loaded)?;
        fs::remove_file(&ready_path).map_err(|error| {
            detection_model_not_loaded(format!(
                "persistent detector pid {} ready acknowledgement cleanup failed for {}: {error}",
                process.pid(),
                ready_path.display()
            ))
        })?;

        Ok(Self {
            mailbox,
            process,
            model_id: model_id.to_owned(),
            backend: ready.backend,
            session_id: ready.session_id,
            reservation_id: ready.reservation_id,
            next_request_id: 0,
        })
    }

    fn readback(&self) -> DetectionWorkerRuntimeReadback {
        DetectionWorkerRuntimeReadback {
            worker_pid: self.process.pid(),
            model_id: self.model_id.clone(),
            backend: self.backend.clone(),
            session_id: self.session_id,
            requests_started: self.next_request_id,
        }
    }

    fn infer(
        &mut self,
        frame: DetectionFrame,
        opts: DetectOpts,
    ) -> synapse_models::ModelResult<DetectionBatch> {
        use std::io::Write as _;
        use synapse_models::{detection_infer_failed, detection_model_not_loaded};

        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            detection_infer_failed(format!(
                "persistent detector pid {} exhausted request ids",
                self.process.pid()
            ))
        })?;
        let request_id = self.next_request_id;
        let request_path = self.mailbox.path().join("request.json");
        let response_path = self.mailbox.path().join("response.json");
        let rgb_path = self.mailbox.path().join(format!("frame-{request_id}.rgb"));
        let progress_path = self.mailbox.path().join("progress.txt");
        for path in [&request_path, &response_path, &rgb_path] {
            if path.exists() {
                return Err(detection_infer_failed(format!(
                    "persistent detector pid {} protocol is dirty before request {request_id}: {} already exists",
                    self.process.pid(),
                    path.display()
                )));
            }
        }
        let mut rgb_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&rgb_path)
            .map_err(|error| {
                detection_infer_failed(format!(
                    "create frame for persistent detector pid {} request {request_id}: {error}",
                    self.process.pid()
                ))
            })?;
        rgb_file
            .write_all(&frame.rgb)
            .and_then(|()| rgb_file.sync_all())
            .map_err(|error| {
                detection_infer_failed(format!(
                    "write/sync frame for persistent detector pid {} request {request_id}: {error}",
                    self.process.pid()
                ))
            })?;
        drop(rgb_file);
        let request = DetectionWorkerRequest {
            request_id,
            model_id: self.model_id.clone(),
            frame_seq: frame.frame_seq,
            width: frame.width,
            height: frame.height,
            rgb_path: rgb_path.clone(),
            progress_path: progress_path.clone(),
            opts,
        };
        write_worker_json_atomic(&request_path, &request).map_err(|(code, detail)| {
            detection_infer_failed(format!(
                "persistent detector pid {} request {request_id} publish failed: {code}: {detail}",
                self.process.pid()
            ))
        })?;

        let started = Instant::now();
        let response = loop {
            if response_path.is_file() {
                let bytes = fs::read(&response_path).map_err(|error| {
                    detection_infer_failed(format!(
                        "persistent detector pid {} response {request_id} read failed: {error}",
                        self.process.pid()
                    ))
                })?;
                let envelope: DetectionWorkerEnvelope =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        detection_infer_failed(format!(
                            "persistent detector pid {} response {request_id} JSON was invalid: {error}",
                            self.process.pid()
                        ))
                    })?;
                break envelope;
            }
            if let Some(exit_code) = self.process.terminal_exit_code().map_err(|error| {
                detection_infer_failed(format!(
                    "persistent detector process status read failed during request {request_id}: {error}"
                ))
            })? {
                let progress = fs::read_to_string(&progress_path)
                    .unwrap_or_else(|error| format!("unavailable ({error})"));
                return Err(detection_model_not_loaded(format!(
                    "persistent detector pid {} exited during request {request_id} with kernel exit_code={exit_code}; last_stage={progress:?}",
                    self.process.pid()
                )));
            }
            if started.elapsed() >= Duration::from_millis(u64::from(DETECTION_WORKER_TIMEOUT_MS)) {
                let progress = fs::read_to_string(&progress_path)
                    .unwrap_or_else(|error| format!("unavailable ({error})"));
                return Err(detection_infer_failed(format!(
                    "persistent detector pid {} request {request_id} timed out after {DETECTION_WORKER_TIMEOUT_MS} ms; last_stage={progress:?}",
                    self.process.pid()
                )));
            }
            thread::sleep(Duration::from_millis(DETECTION_WORKER_POLL_MS));
        };

        fs::remove_file(&response_path).map_err(|error| {
            detection_infer_failed(format!(
                "persistent detector pid {} response {request_id} acknowledgement cleanup failed: {error}",
                self.process.pid()
            ))
        })?;
        fs::remove_file(&rgb_path).map_err(|error| {
            detection_infer_failed(format!(
                "persistent detector pid {} frame {request_id} cleanup failed: {error}",
                self.process.pid()
            ))
        })?;
        if request_path.exists() {
            return Err(detection_infer_failed(format!(
                "persistent detector pid {} published response {request_id} without consuming {}",
                self.process.pid(),
                request_path.display()
            )));
        }
        if response.request_id != request_id
            || response.worker_pid != self.process.pid()
            || response.session_id != self.session_id
            || response.reservation_id != self.reservation_id
        {
            return Err(detection_infer_failed(format!(
                "persistent detector response attestation mismatch for request {request_id}: expected pid={} session_id={} reservation={:?}; actual={response:?}",
                self.process.pid(),
                self.session_id,
                self.reservation_id
            )));
        }
        if !response.ok {
            return Err(detection_model_not_loaded(format!(
                "persistent detector pid {} request {request_id} failed code={} detail={}",
                self.process.pid(),
                response.error_code.as_deref().unwrap_or("<missing>"),
                response.error_detail.as_deref().unwrap_or("<missing>")
            )));
        }
        response.batch.ok_or_else(|| {
            detection_infer_failed(format!(
                "persistent detector pid {} request {request_id} returned ok without a detection batch",
                self.process.pid()
            ))
        })
    }

    fn shutdown(mut self) -> synapse_models::ModelResult<()> {
        use synapse_models::detection_infer_failed;

        let shutdown_path = self.mailbox.path().join("shutdown.json");
        write_worker_json_atomic(
            &shutdown_path,
            &serde_json::json!({
                "protocol": DETECTION_WORKER_PROTOCOL,
                "requested_by_pid": std::process::id(),
            }),
        )
        .map_err(|(code, detail)| {
            detection_infer_failed(format!(
                "persistent detector pid {} shutdown publish failed: {code}: {detail}",
                self.process.pid()
            ))
        })?;
        let verdict = self
            .process
            .wait_for_exit(DETECTION_WORKER_SHUTDOWN_TIMEOUT_MS)
            .map_err(|error| {
                detection_infer_failed(format!(
                    "persistent detector pid {} shutdown/cleanup failed: {error}",
                    self.process.pid()
                ))
            })?;
        let reservation_cleanup = verify_persistent_worker_reservation(
            verdict.pid,
            &self.backend,
            self.reservation_id.as_deref(),
            false,
        );
        if verdict.timed_out || verdict.exit_code != 0 {
            return Err(detection_infer_failed(format!(
                "persistent detector pid {} shutdown verdict was timed_out={} exit_code={}; reservation_cleanup={}",
                verdict.pid,
                verdict.timed_out,
                verdict.exit_code,
                reservation_cleanup
                    .as_ref()
                    .map_or_else(|error| error.as_str(), |()| "verified_absent")
            )));
        }
        reservation_cleanup.map_err(detection_infer_failed)
    }
}

#[cfg(windows)]
fn verify_persistent_worker_reservation(
    worker_pid: u32,
    backend: &str,
    reservation_id: Option<&str>,
    should_exist: bool,
) -> Result<(), String> {
    if backend == "cpu" {
        if reservation_id.is_some() {
            return Err(format!(
                "CPU persistent detector pid {worker_pid} unexpectedly reported GPU reservation {reservation_id:?}"
            ));
        }
        return Ok(());
    }
    let snapshot = readback_gpu_reservations(0).map_err(|error| {
        format!("persistent detector pid {worker_pid} GPU reservation readback failed: {error}")
    })?;
    let matching = snapshot
        .reservations
        .iter()
        .find(|row| row.pid == worker_pid && reservation_id == Some(row.reservation_id.as_str()));
    if matching.is_some() != should_exist {
        return Err(format!(
            "persistent detector pid {worker_pid} reservation expectation failed: expected_exists={should_exist} reservation_id={reservation_id:?} state_path={} matching={matching:?}",
            snapshot.state_path
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn infer_in_owned_worker(
    runtime: &mut DetectionRuntime,
    model_id: &str,
    frame: DetectionFrame,
    opts: DetectOpts,
) -> synapse_models::ModelResult<DetectionBatch> {
    if runtime
        .worker
        .as_ref()
        .is_some_and(|worker| worker.model_id != model_id)
    {
        let previous = runtime.worker.take().ok_or_else(|| {
            synapse_models::detection_infer_failed(
                "persistent detector model-change invariant was violated",
            )
        })?;
        previous.shutdown()?;
    }
    if runtime.worker.is_none() {
        runtime.worker = Some(PersistentDetectionWorker::start(model_id)?);
    }
    let result = match runtime.worker.as_mut() {
        Some(worker) => worker.infer(frame, opts),
        None => Err(synapse_models::detection_infer_failed(
            "persistent detector startup returned without an owned worker",
        )),
    };
    if result.is_err() {
        if let Some(worker) = runtime.worker.take() {
            if let Err(cleanup_error) = worker.shutdown() {
                let inference_error = match &result {
                    Ok(_) => "<missing inference error>".to_owned(),
                    Err(error) => error.to_string(),
                };
                return Err(synapse_models::detection_infer_failed(format!(
                    "persistent detector inference failed ({inference_error}); cleanup also failed ({cleanup_error})"
                )));
            }
        }
    }
    result
}

pub fn default_detection_config() -> DetectionRuntimeConfig {
    DetectionRuntimeConfig::default()
}

/// Runs the detection stage for one observation.
///
/// # Errors
///
/// Every refusal here is built with [`crate::m1::mcp_error`] /
/// [`crate::m1::mcp_error_with_remediation`], so the **specific** cause travels
/// in `error.data.code` (#2074).
///
/// This was the last m1 module still constructing raw
/// `ErrorData::invalid_params(msg, None)`, and that `None` was load-bearing in
/// the wrong direction: `normalize_tool_error` in `server/handler.rs` rewrites
/// any error with `data == None` and JSON-RPC code `INVALID_PARAMS` into
/// `data.code = "TOOL_PARAMS_INVALID"`. Its guard is purely structural — it
/// cannot tell an rmcp `deny_unknown_fields` deserialize dead-end from a
/// daemon-side profile fault — so `observe` reported
/// `data.code = "TOOL_PARAMS_INVALID"` for a `DETECTION_MODEL_NOT_LOADED`
/// refusal whose caller's parameters were entirely valid. The real code was
/// recoverable only by string-parsing the message, so no consumer could use one
/// branching rule across the surface the way the storage/hygiene facades allow.
///
/// Supplying `data` here short-circuits that rewrite at the source rather than
/// teaching the normalizer to read messages, which would re-introduce the same
/// string-parsing one layer down. The message text is deliberately unchanged, so
/// anything matching on it today still matches.
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
        return Err(crate::m1::mcp_error(
            error_codes::DETECTION_NO_FRAME,
            format!(
                "{}: detection requires positive foreground bounds, got {:?}",
                error_codes::DETECTION_NO_FRAME,
                input.foreground.window_bounds
            ),
        ));
    }
    if !valid_detection_config(config) {
        tracing::warn!(
            code = "M1_DETECTION_CONFIG_INVALID",
            confidence_threshold = config.confidence_threshold,
            max_detections = config.max_detections,
            "detection configuration is invalid"
        );
        return Err(crate::m1::mcp_error(
            error_codes::DETECTION_MODEL_INFER_FAILED,
            format!(
                "{}: confidence_threshold={} max_detections={}",
                error_codes::DETECTION_MODEL_INFER_FAILED,
                config.confidence_threshold,
                config.max_detections
            ),
        ));
    }
    // GPU inference is profile-opt-in. A numeric default must never silently
    // load the default ORT model for an ordinary productivity profile: the
    // profile must name the exact model whose resource envelope was reviewed.
    //
    // #2054: opting out is legitimate, but it is not health. This branch used
    // to report `SensorStatus::Healthy` without loading a model, capturing a
    // frame, or running a single inference, which made a profile whose
    // detector never runs indistinguishable from one whose detector completed
    // inference. The status now names the exact cause, and no `detection`
    // latency is recorded because no detection work was timed.
    if let Some(fault) = detection_inference_gate(config) {
        match fault.kind {
            DetectionFaultKind::NotConfigured => {
                tracing::warn!(
                    code = error_codes::DETECTION_NOT_CONFIGURED,
                    mode = ?mode,
                    model_id = ?config.model_id,
                    max_detections = config.max_detections,
                    confidence_threshold = config.confidence_threshold,
                    remediation = %fault.remediation,
                    "detection stage ran no model inference: the active profile configures no detector"
                );
                input.detection_status = SensorStatus::NotConfigured {
                    reason_code: fault.reason_code,
                    detail: format!("{}; {}", fault.detail, fault.remediation),
                };
                return Ok(());
            }
            // #2064: this used to fall through, capture a frame, spawn the
            // isolated worker, and fail there with a generic message. The
            // verdict is already known from configuration alone, so it fails
            // here instead — same outcome, named cause, and no capture or GPU
            // reservation spent proving it.
            DetectionFaultKind::Misconfigured => {
                tracing::error!(
                    code = error_codes::DETECTION_MODEL_NOT_LOADED,
                    mode = ?mode,
                    model_id = ?config.model_id,
                    remediation = %fault.remediation,
                    "detection stage refused inference: the active profile names a detector this daemon cannot load"
                );
                return Err(crate::m1::mcp_error_with_remediation(
                    error_codes::DETECTION_MODEL_NOT_LOADED,
                    format!(
                        "{}: {}; {}",
                        error_codes::DETECTION_MODEL_NOT_LOADED,
                        fault.detail,
                        fault.remediation
                    ),
                    &fault.remediation,
                ));
            }
        }
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
                return Err(crate::m1::mcp_error(
                    error_codes::DETECTION_NO_FRAME,
                    error.to_string(),
                ));
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
            return Err(crate::m1::mcp_error(
                error_codes::DETECTION_NO_FRAME,
                detail,
            ));
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
        runtime,
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
            return Err(crate::m1::mcp_error(
                error.code(),
                format!("{}: {error}", error.code()),
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
