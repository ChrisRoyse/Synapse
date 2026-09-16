# 13. Models Subsystem

**Source files covered:**

- `crates/synapse-models/src/lib.rs` — crate root, re-exports, `DetectOpts`, `DetectionFrame`, `Detector` trait
- `crates/synapse-models/src/registry.rs` — registered model table, `RegisteredModel`, COCO-80 class map
- `crates/synapse-models/src/download.rs` — `ModelDescriptor`, `default_model_dir`, `model_download_failed`, ORT extensions discovery
- `crates/synapse-models/src/verify.rs` — streaming SHA-256 hashing (`sha256_file`, `normalize_sha256`)
- `crates/synapse-models/src/ep.rs` — `ModelBackend`, `default_provider_order`, `create_ort_session`
- `crates/synapse-models/src/session.rs` — `ModelLoader`, `SessionFactory`, `OrtSessionFactory`, `LoadedModel`, RT-DETR pre/post-processing
- `crates/synapse-models/src/error.rs` — `ModelError`, `ModelResult`, error-code mapping
- `crates/synapse-models/Cargo.toml` — dependencies and feature flags

---

## 13.1 Overview — Model Lifecycle

The `synapse-models` crate manages the lifecycle of ONNX detection models. The pipeline is **registry → download → verify → session**:

1. **Registry** (`registry.rs`) — A compile-time table (`REGISTERED_MODELS`) declares each known model with its filename, expected SHA-256, download URL, license, and input shape. The default detection model is `rtdetr_v2_s_coco_onnx`.
2. **Download** (`download.rs`) — Models live on disk under `default_model_dir()`. **Automated download is disabled in M1**; `model_download_failed` returns a `DownloadFailed` error directing the operator to side-load a verified file. There is no network fetch implemented in this crate.
3. **Verify** (`verify.rs`) — Before a session is built, the on-disk file's SHA-256 is computed via a streaming 64 KiB-chunked hash and compared against the descriptor's expected hash. A mismatch yields `HashMismatch`.
4. **Session** (`session.rs` + `ep.rs`) — `ModelLoader::load` verifies the file, then asks a `SessionFactory` to build a persistent ONNX Runtime session, trying execution providers in order (CUDA → DirectML → CPU). A successful build produces a `LoadedModel` that implements the `Detector` trait for inference.

The `ort` (ONNX Runtime) integration is **feature-gated**. Without the `ort` feature compiled in, sessions fall back to a `Placeholder` handle and `OrtSessionFactory` returns `BackendUnavailable`.

The ONNX Runtime binding is **`ort` version `2.0.0-rc.12`** (workspace pin in `Cargo.toml`), used with API level `api-24` (`crates/synapse-models/Cargo.toml`).

Cross-references:
- Whisper audio transcription using ONNX Runtime extensions — see [08_audio_subsystem.md](08_audio_subsystem.md). The `whisper_tiny_int8` model receives special handling in `create_ort_session` (operator-library registration).
- The detection pipeline consuming this crate (frame capture, perception loop) — see [07_perception_subsystem.md](07_perception_subsystem.md).

---

## 13.2 Registry — `registry.rs`

### 13.2.1 `RegisteredModel` struct

A `Copy`-able compile-time descriptor of a known model (`crates/synapse-models/src/registry.rs`):

| Field | Type | Description |
|-------|------|-------------|
| `id` | `&'static str` | Stable model identifier |
| `label` | `&'static str` | Human-readable name |
| `filename` | `&'static str` | On-disk filename under the model dir |
| `sha256` | `&'static str` | Expected digest (`sha256:` prefixed) |
| `download_url` | `&'static str` | Source URL for side-loading |
| `license_spdx` | `&'static str` | SPDX license identifier |
| `source_model` | `&'static str` | Upstream model reference |
| `source_repo` | `&'static str` | Upstream repository URL |
| `input_shape` | `[usize; 4]` | NCHW input tensor shape |
| `class_map` | `&'static [&'static str]` | Class-index → label table |

`RegisteredModel::descriptor(self)` converts the static entry into a runtime `ModelDescriptor`, resolving `path` to `default_model_dir().join(filename)`.

### 13.2.2 Registered models table

`REGISTERED_MODELS` contains a single entry. There is **one registered model** (`crates/synapse-models/src/registry.rs:144`).

| Property | Value |
|----------|-------|
| `id` | `rtdetr_v2_s_coco_onnx` |
| `label` | `RT-DETRv2-S COCO ONNX` |
| `filename` | `rtdetr_v2_s_coco.onnx` |
| `sha256` | `sha256:583a236ac21c95a7fd94f284fc21485e42355bfef82c27011ba78fbc09ee87e2` |
| `download_url` | `https://huggingface.co/onnx-community/rtdetr_v2_r18vd-ONNX/resolve/main/onnx/model.onnx` |
| `license_spdx` | `Apache-2.0` |
| `source_model` | `PekingU/rtdetr_v2_r18vd` |
| `source_repo` | `https://github.com/lyuwenyu/RT-DETR` |
| `input_shape` | `[1, 3, 640, 640]` (`DEFAULT_DETECTION_INPUT_SHAPE`) |
| `class_map` | `COCO80_CLASS_MAP` (80 COCO classes) |
| **File size** | Not determined from source (no size field exists in the registry) |

The default detection model id is `DEFAULT_DETECTION_MODEL_ID = "rtdetr_v2_s_coco_onnx"` (= `RTDETR_V2_S_COCO_ONNX_ID`).

### 13.2.3 Class map

`COCO80_CLASS_MAP: [&str; 80]` lists the 80 COCO categories in order (`person`, `bicycle`, `car`, … `toothbrush`). The class index from the model's `logits` output maps directly into this array.

### 13.2.4 Registry functions

| Function | Returns | Behavior |
|----------|---------|----------|
| `default_detection_model()` | `RegisteredModel` | Returns `RTDETR_V2_S_COCO_ONNX` (`const fn`) |
| `default_detection_model_descriptor()` | `ModelDescriptor` | `.descriptor()` of the default model |
| `registered_model(id: &str)` | `Option<RegisteredModel>` | Linear lookup by `id` in `REGISTERED_MODELS` |

---

## 13.3 Download / Descriptor — `download.rs`

### 13.3.1 `ModelDescriptor`

The runtime descriptor (`crates/synapse-models/src/download.rs`), serde-serializable with `deny_unknown_fields`:

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | Model identifier |
| `path` | `PathBuf` | Absolute on-disk path to the `.onnx` file |
| `sha256` | `String` | Expected digest (may carry `sha256:` prefix) |
| `input_shape` | `Vec<usize>` | NCHW input tensor dimensions |
| `class_map` | `Vec<String>` | Class-index → label list |

Constructor helper `ModelDescriptor::yolov10n_general(sha256, class_map)` builds a legacy `yolov10n_general` descriptor with shape `[1, 3, 640, 640]` and path `default_model_dir().join("yolov10n_general.onnx")`.

### 13.3.2 Model storage location — `default_model_dir`

```rust
pub fn default_model_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("synapse")
        .join("models")
}
```

Resolves to `%LOCALAPPDATA%\synapse\models` (e.g. `C:\Users\<user>\AppData\Local\synapse\models`). If `LOCALAPPDATA` is unset, it falls back to `./synapse/models` (relative to the current directory).

### 13.3.3 Download mechanism

There is **no implemented download/retry mechanism** in this crate. The sole download-related function is `model_download_failed`:

```rust
pub fn model_download_failed(source: &str) -> ModelError {
    let source = source.trim();
    let detail = if source.is_empty() {
        "model download source was empty".to_owned()
    } else {
        format!("model downloads are disabled in M1; side-load a verified model from {source}")
    };
    ModelError::DownloadFailed { detail }
}
```

Automated model downloading is disabled in milestone M1. Models must be **side-loaded manually** into `default_model_dir()`; the operator obtains the file from the registry's `download_url`. There is no retry loop, no HTTP client, and no progress tracking present in `synapse-models`.

### 13.3.4 ORT extensions discovery (`ort` feature only)

`local_ort_extensions_library() -> Option<PathBuf>` searches `default_model_dir()/ort-extensions/wheel/onnxruntime_extensions/` for a file whose name starts with `_extensions_pydll` and ends with `.pyd` (case-insensitive extension match), returning the lexicographically first match. This is used by the Whisper path in `create_ort_session` (see [08_audio_subsystem.md](08_audio_subsystem.md)).

---

## 13.4 Verification — `verify.rs`

### 13.4.1 Streaming SHA-256 (`sha256_file`)

Hashing uses the `sha2` crate's `Sha256` with a fixed **64 KiB heap-allocated buffer**, reading the file in a loop until EOF (`crates/synapse-models/src/verify.rs`):

```rust
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}
```

This streaming/chunked approach avoids loading the entire model into memory. The digest is returned as **lowercase hex** without a `sha256:` prefix, formatted by the internal `hex_lower` helper (manual nibble-to-hex conversion).

### 13.4.2 Normalization (`normalize_sha256`)

```rust
pub fn normalize_sha256(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix("sha256:")
        .unwrap_or(trimmed)
        .to_ascii_lowercase()
}
```

Trims whitespace, strips a leading `sha256:` prefix if present, and lowercases. This is applied to the descriptor's expected hash before comparison, so registry entries may carry the `sha256:` prefix while the computed digest does not.

### 13.4.3 Verification flow

In `ModelLoader::load_with_factory` (`session.rs`): compute `sha256_file(&descriptor.path)`, normalize the expected `descriptor.sha256`, and compare. Inequality returns `ModelError::HashMismatch { path, expected, actual }` **before** any session is created. An I/O failure opening/reading the file maps to `ModelError::LoadFailed`.

---

## 13.5 Execution Providers — `ep.rs`

### 13.5.1 `ModelBackend` enum

Serde-serialized as `snake_case` (`crates/synapse-models/src/ep.rs`):

| Variant | serde name | Notes |
|---------|-----------|-------|
| `Cuda` | `cuda` | NVIDIA CUDA EP |
| `DirectMl` | `direct_ml` | Windows DirectML EP |
| `Cpu` | `cpu` | CPU EP (`#[default]`) |

### 13.5.2 Default provider order

```rust
pub fn default_provider_order() -> Vec<ModelBackend> {
    vec![Cuda, DirectMl, Cpu]
}
```

**Order: CUDA → DirectML → CPU.** The loader tries each in sequence and selects the first that successfully builds a session. `Cpu` is the default single-variant fallback and the enum's `#[default]`.

### 13.5.3 `create_ort_session` (`ort` feature only)

Builds one `ort::session::Session` for a descriptor and a single provider:

1. Creates a `Session::builder()`; failures → `LoadFailed`.
2. **Whisper special case:** if `descriptor.id == "whisper_tiny_int8"`, registers the ONNX Runtime extensions operator library — preferring a local library from `local_ort_extensions_library()` via `with_operator_library`, otherwise falling back to `with_extensions()`. Either failure → `LoadFailed`. See [08_audio_subsystem.md](08_audio_subsystem.md).
3. Selects the EP and builds it with `.error_on_failure()`:
   - `Cuda` → `ep::CUDA::default()`
   - `DirectMl` → `ep::DirectML::default()`
   - `Cpu` → `ep::CPU::default().with_arena_allocator(false)`
4. `builder.with_execution_providers([execution_provider])` — on error logs a `warn` and returns `BackendUnavailable { attempted: vec![provider] }`.
5. `builder.commit_from_file(&descriptor.path)` loads the model; failure → `LoadFailed`.

### 13.5.4 Feature flags (`Cargo.toml`)

| Feature | Enables |
|---------|---------|
| `default` | (none) — ORT not compiled, `Placeholder` sessions only |
| `ort` | `dep:ort`, `ort/api-24`, `ort/copy-dylibs`, `ort/download-binaries`, `ort/std`, `ort/tls-native` |
| `cuda` | `ort` + `ort/cuda` |
| `directml` | `ort` + `ort/directml` |

ORT is an **optional** dependency (`default-features = false`), pinned at workspace version `2.0.0-rc.12`.

---

## 13.6 Session Management — `session.rs`

### 13.6.1 `SessionHandle`

An enum wrapping the runtime session:

- `Placeholder` — used when the `ort` feature is absent; inference returns an empty `DetectionBatch`.
- `Ort(Arc<Mutex<ort::session::Session>>)` (`ort` feature only) — a persistent ONNX Runtime session shared behind an `Arc<Mutex<…>>`. Custom `Debug` impl prints `"Ort(Session)"`.

### 13.6.2 `SessionFactory` trait

```rust
pub trait SessionFactory {
    fn create_session(
        &self,
        descriptor: &ModelDescriptor,
        providers: &[ModelBackend],
    ) -> ModelResult<SessionBuildResult>;
}
```

`SessionBuildResult { selected_backend: ModelBackend, session: SessionHandle }`.

### 13.6.3 `OrtSessionFactory`

- **Without `ort` feature:** always returns `BackendUnavailable { attempted: providers }`.
- **With `ort` feature:** iterates the provider list:
  - Empty providers → `BackendUnavailable { attempted: [] }`.
  - On success → returns `SessionBuildResult` with the selected backend and an `Arc<Mutex<Session>>`.
  - If the **CPU** provider returns `LoadFailed`, it is treated as fatal and returned immediately (`"CPU provider rejected verified model: …"`) — the CPU EP is the last-resort backend so its model rejection is not retried.
  - Other provider failures are accumulated; if all fail, logs a `warn` with the failure list and returns `BackendUnavailable { attempted: providers }`.

### 13.6.4 `ModelLoader`

Holds an ordered provider list (`Vec<ModelBackend>`). `Default` uses `default_provider_order()`.

| Method | Behavior |
|--------|----------|
| `new(providers)` | Construct with explicit provider order (`const fn`) |
| `providers()` | Borrow the provider slice |
| `load_with_factory(descriptor, factory)` | Verify SHA-256, then build session; assigns a monotonic `session_id` and logs `info`. Core loader path. |
| `load_if_present(descriptor, factory)` | `Ok(None)` if `descriptor.path` does not exist; otherwise `load_with_factory(...).map(Some)` |
| `load_yolov10n_if_present(descriptor, factory)` | Legacy alias delegating to `load_if_present` |
| `load(descriptor)` | Convenience: `load_with_factory(descriptor, &OrtSessionFactory)` |

`session_id` comes from a process-global `static NEXT_SESSION_ID: AtomicU64` (starts at 1), incremented with `Ordering::Relaxed`.

### 13.6.5 `LoadedModel` and inference

`LoadedModel { descriptor, selected_backend, session_id, session }` exposes `session_id()`, `selected_backend()`, `descriptor()`, `session()` accessors and implements the `Detector` trait.

**Threading:** the ORT session is held in `Arc<Mutex<Session>>`. `infer` acquires the mutex lock for the duration of `session.run(...)`; a poisoned lock yields `DetectionInferFailed`. Output tensors and the guard are dropped before post-processing, releasing the lock as early as possible. Inference is therefore serialized per session.

**Inference path (`ort` feature):**
1. `frame.validate()` checks dimensions and RGB byte length.
2. `preprocess_rgb_frame` — builds an `RgbImage`, resizes to the descriptor's `H×W` with `FilterType::Triangle`, normalizes to `[0,1]` f32, and lays out as planar NCHW `[1,3,H,W]` `Tensor<f32>`.
3. `session.run` with input name `"pixel_values"`.
4. Extracts outputs `"logits"` and `"pred_boxes"` (both `f32`); missing/extraction failure → `DetectionInferFailed`.
5. `decode_rtdetr_outputs` — applies sigmoid to logits, picks best class per query, thresholds at `confidence_threshold/100`, converts normalized cx/cy/w/h boxes to pixel `Rect`, sorts by confidence descending, truncates to `max_detections`.
6. Returns a `DetectionBatch { model_id, frame_seq, inferred_at, items }`.

For the `Placeholder` handle, `infer` returns an empty batch.

### 13.6.6 Crate-level types (`lib.rs`)

| Type | Purpose |
|------|---------|
| `DetectOpts { confidence_threshold: u16, max_detections: usize }` | Inference tuning. Defaults: threshold `50`, max `100`. |
| `DetectionFrame { frame_seq, width, height, rgb }` | Input frame. `validate()` enforces non-zero dims and `rgb.len() == width*height*3`. |
| `Detector` trait | `Send + Sync`; `fn infer(&self, frame, opts) -> ModelResult<DetectionBatch>` |

---

## 13.7 Error Types — `error.rs`

`ModelResult<T> = Result<T, ModelError>`. Each variant maps to a stable code from `synapse_core::error_codes` via `ModelError::code()`.

| Variant | Fields | Error code | `Display` message |
|---------|--------|-----------|-------------------|
| `DownloadFailed` | `detail: String` | `MODEL_DOWNLOAD_FAILED` | `model download failed: {detail}` |
| `HashMismatch` | `path, expected, actual` | `MODEL_HASH_MISMATCH` | `model hash mismatch for {path}: expected {expected}, got {actual}` |
| `LoadFailed` | `path, detail` | `MODEL_LOAD_FAILED` | `model load failed for {path}: {detail}` |
| `BackendUnavailable` | `attempted: Vec<ModelBackend>` | `MODEL_BACKEND_UNAVAILABLE` | `no model backend was available; attempted {attempted:?}` |
| `DetectionModelNotLoaded` | `detail: String` | `DETECTION_MODEL_NOT_LOADED` | `detection model is not loaded: {detail}` |
| `DetectionNoFrame` | `detail: String` | `DETECTION_NO_FRAME` | `no detection frame available: {detail}` |
| `DetectionInferFailed` | `detail: String` | `DETECTION_MODEL_INFER_FAILED` | `detection inference failed: {detail}` |

Constructor helpers: `detection_model_not_loaded`, `detection_no_frame`, `detection_infer_failed` (all `impl Into<String>`); `model_download_failed` lives in `download.rs`.

---

## 13.8 Public API Surface (`lib.rs` re-exports)

- **download:** `ModelDescriptor`, `default_model_dir`, `model_download_failed`
- **ep:** `ModelBackend`, `default_provider_order`
- **error:** `ModelError`, `ModelResult`, `detection_infer_failed`, `detection_model_not_loaded`, `detection_no_frame`
- **registry:** `COCO80_CLASS_MAP`, `DEFAULT_DETECTION_INPUT_SHAPE`, `DEFAULT_DETECTION_MODEL_ID`, `REGISTERED_MODELS`, `RTDETR_V2_S_COCO_ONNX` (+ its component consts), `RegisteredModel`, `default_detection_model`, `default_detection_model_descriptor`, `registered_model`
- **session:** `LoadedModel`, `ModelLoader`, `OrtSessionFactory`, `SessionBuildResult`, `SessionFactory`, `SessionHandle`
- **verify:** `normalize_sha256`, `sha256_file`
- **crate root:** `DetectOpts`, `DetectionFrame`, `Detector`
