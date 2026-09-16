# 08. Audio Subsystem

The `synapse-audio` crate captures system audio, buffers it in a ring, runs lightweight
acoustic event detectors, performs Whisper-based speech-to-text, and estimates
direction-of-arrival from stereo channels.

**Source files covered:**

- `crates/synapse-audio/src/lib.rs`
- `crates/synapse-audio/src/ring.rs`
- `crates/synapse-audio/src/loopback.rs`
- `crates/synapse-audio/src/stt.rs`
- `crates/synapse-audio/src/stt/window.rs`
- `crates/synapse-audio/src/detectors.rs`
- `crates/synapse-audio/src/direction.rs`
- `crates/synapse-audio/src/error.rs`
- `crates/synapse-audio/Cargo.toml`
- `crates/synapse-core/src/types/observation.rs` (`DirectionEstimate`, `AudioContext`, `AudioEvent`)

---

## 1. Overview

The crate is organized around `AudioRuntime` (`crates/synapse-audio/src/lib.rs`), which
wires together five concerns:

1. **Loopback capture** — WASAPI loopback of the default render endpoint (Windows only),
   running on a dedicated MMCSS "Pro Audio" thread (`loopback.rs`).
2. **Ring buffer** — fixed-capacity interleaved-f32 circular buffer holding up to
   `MAX_RING_SECONDS` (30 s) of audio (`ring.rs`).
3. **Detectors** — RMS/crest-based loud-transient, speech, and music latches that emit
   `Event`s to a sink (`detectors.rs`).
4. **Speech-to-text** — `WhisperTinyStt`, an INT8 Whisper-tiny ONNX model executed via ORT
   (`stt.rs`, `stt/window.rs`). See [13_models_subsystem.md](13_models_subsystem.md) for
   Whisper model loading/verification.
5. **Direction estimation** — stereo energy-panning blended with cross-correlation ITD
   (`direction.rs`).

### 1.1 Crate dependencies (`Cargo.toml`)

| Dependency | Purpose |
|---|---|
| `ort` | ONNX Runtime for Whisper inference |
| `synapse-models` (feature `directml`) | Model loading/verification (`ModelLoader`, `LoadedModel`, `SessionHandle`) |
| `synapse-core` | `Event`, `AudioEvent`, `AudioContext`, `DirectionEstimate`, `error_codes` |
| `wasapi`, `windows` (`cfg(windows)` only) | WASAPI loopback + MMCSS thread priority |
| `metrics`, `tracing`, `chrono`, `serde`, `serde_json`, `thiserror`, `tokio` | telemetry, logging, serialization, errors |

Lints: `unsafe_code = "allow"`, `clippy::all = "deny"`, `unwrap_used`/`expect_used = "deny"`.

### 1.2 `AudioRuntime` and `AudioConfig`

`AudioConfig` fields (`crates/synapse-audio/src/lib.rs`):

| Field | Type | Default | Notes |
|---|---|---|---|
| `ring_seconds` | `u32` | `30` (`DEFAULT_RING_SECONDS`) | Validated 1..=`MAX_RING_SECONDS` (30) |
| `start_loopback` | `bool` | `false` | Starts WASAPI capture thread |
| `detectors_enabled` | `bool` | `false` | Requires `start_loopback == true` |
| `stt_model_path` | `Option<PathBuf>` | `None` | Falls back to `default_model_path()` |

Key `AudioRuntime` methods:

| Signature | Behavior |
|---|---|
| `spawn(config: AudioConfig) -> AudioResult<Self>` | Spawns with a no-op event sink |
| `spawn_with_event_sink(config: AudioConfig, event_sink: AudioEventSink) -> AudioResult<Self>` | Validates config, builds ring, optionally starts loopback + detectors, creates STT |
| `config(&self) -> &AudioConfig` | |
| `loopback_started(&self) -> bool` | True if the loopback handle reports running |
| `detectors_started(&self) -> bool` | `detectors_enabled && loopback_started()` |
| `ring(&self) -> Arc<AudioRing>` | |
| `tail_seconds(&self, seconds: f32) -> AudioResult<AudioWindow>` | Delegates to `AudioRing::tail_seconds` |
| `estimate_direction_tail(&self, seconds: f32) -> AudioResult<DirectionEstimate>` | Tail window -> `direction::estimate_direction` |
| `transcribe_tail(&self, seconds: f32, language: impl AsRef<str>) -> AudioResult<Transcription>` | Tail window -> STT |
| `transcribe_file(&self, path: impl AsRef<Path>, language: impl AsRef<str>) -> AudioResult<Transcription>` | Reads file, transcribes |
| `detector_snapshot(&self) -> DetectorSnapshot` | |
| `loopback_status(&self) -> LoopbackStatus` | Defaults to non-running status if no handle |
| `stt_model_loaded(&self) -> bool` | |

`AudioEventSink = Arc<dyn Fn(Event) + Send + Sync + 'static>`.

`validate_config` rejects `ring_seconds == 0`, `ring_seconds > 30`, or
`detectors_enabled` without `start_loopback`, returning `AudioError::LoopbackInitFailed`.

---

## 2. Ring buffer (`crates/synapse-audio/src/ring.rs`)

### 2.1 Constants

| Constant | Value | Meaning |
|---|---|---|
| `DEFAULT_SAMPLE_RATE_HZ` | `48_000` | Default ring sample rate |
| `STEREO_CHANNELS` | `2` | Default channel count |

### 2.2 `AudioFormat`

`#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]`, `deny_unknown_fields`.

| Field | Type | Default |
|---|---|---|
| `sample_rate_hz` | `u32` | `48_000` |
| `channels` | `u16` | `2` |

### 2.3 `AudioWindow`

`#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]`, `deny_unknown_fields`.

| Field | Type | Meaning |
|---|---|---|
| `format` | `AudioFormat` | Format the samples were captured in |
| `frames` | `usize` | Number of frames (per-channel samples) returned |
| `samples` | `Vec<f32>` | Interleaved samples, length `frames * channels` |
| `rms_db` | `f32` | RMS of `samples` in dB (via `detectors::rms_db`) |

Method: `pcm_i16_le(&self) -> Vec<u8>` — clamps each sample to `[-1, 1]`, scales by
`i16::MAX`, rounds, emits little-endian `i16` bytes.

### 2.4 `AudioRing`

`#[derive(Debug)]`. Holds `inner: Mutex<RingState>` and `max_seconds: u32`. `RingState`
contains `format: AudioFormat`, `samples: Vec<f32>`, and `total_frames: u64` (monotonic
write counter).

**Sizing.** Backing buffer is allocated up front as
`capacity_samples = sample_rate_hz * seconds * channels` floats. Frame capacity is
`sample_rate_hz * max_seconds`. The buffer never reallocates during capture; it is only
re-sized when `set_format` changes the format.

| Signature | Behavior |
|---|---|
| `new(max_seconds: u32) -> Self` | Allocates buffer for default format; zero-filled |
| `max_seconds(&self) -> u32` | |
| `format(&self) -> AudioFormat` | |
| `frames_available(&self) -> usize` | `min(total_frames, capacity_frames)` |
| `total_frames(&self) -> u64` | Lifetime frame count |
| `set_format(&self, format: AudioFormat)` | If format changed: reallocates buffer, resets `total_frames` to 0 |
| `push_interleaved(&self, samples: &[f32])` | Writes frame-by-frame at `(total_frames % capacity_frames) * channels`; increments `total_frames` |
| `tail_seconds(&self, seconds: f32) -> AudioResult<AudioWindow>` | Returns last `seconds` of samples |

**Write/overwrite behavior.** `push_interleaved` iterates `chunks_exact(channels)`,
writing each frame to a slot computed modulo `capacity_frames`, so once full the ring
overwrites the oldest frames. Partial trailing samples (not a full frame) are dropped by
`chunks_exact`. If `channels == 0` or `capacity_frames == 0`, the push is a no-op.

**Read behavior.** `tail_seconds` validates `seconds` is finite, non-negative, and
`<= max_seconds` (else `AudioError::LoopbackInitFailed`). It computes
`requested = round(seconds * sample_rate_hz)`, clamps to `available`, then copies frames
starting at `total_frames - frames`, each indexed modulo `capacity_frames`. The returned
`AudioWindow` carries the current `format`, the realized `frames`, and `rms_db` over the
copied samples.

---

## 3. Loopback capture (`crates/synapse-audio/src/loopback.rs`)

Capture is **WASAPI loopback** of the default **render** endpoint, initialized in
**capture** direction. It is Windows-only; on non-Windows targets
`start_platform_loopback` returns `AudioError::LoopbackInitFailed` ("WASAPI loopback is
only available on Windows").

Metric: `AUDIO_LOOPBACK_FRAMES_TOTAL = "audio_loopback_frames_total"` (counter,
incremented per captured frame batch).

### 3.1 `LoopbackStatus`

`#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]`, `deny_unknown_fields`.

| Field | Type | Meaning |
|---|---|---|
| `running` | `bool` | Capture thread active |
| `frames_captured` | `u64` | Lifetime captured frames |
| `last_error_code` | `Option<String>` | Last `AudioError::code()`, omitted when `None` |

### 3.2 Public API

| Signature | Behavior |
|---|---|
| `start_loopback(ring: Arc<AudioRing>, detectors: Option<DetectorProcessor>) -> AudioResult<LoopbackHandle>` | Validates ring duration (1..=30), spawns capture thread |
| `LoopbackHandle::is_running(&self) -> bool` | Atomic load |
| `LoopbackHandle::status(&self) -> LoopbackStatus` | |

`LoopbackHandle` `Drop` sets the stop flag and joins the capture thread.

### 3.3 Capture thread (Windows)

`start_platform_loopback` spawns a thread named `synapse-audio-loopback` and waits up to
**5 s** on a `sync_channel(1)` for the thread to report readiness; failure to report
yields `LoopbackInitFailed`.

The thread sequence (`capture_loop`):

1. `ComMtaGuard::init()` — `wasapi::initialize_mta()`; `deinitialize` on drop.
2. `MmcssGuard::start()` — `AvSetMmThreadCharacteristicsW("Pro Audio")` +
   `AvSetMmThreadPriority(AVRT_PRIORITY_CRITICAL)`; reverted on drop.
3. `WasapiLoopback::start()` — enumerates the default `Render` device, requests a
   `WaveFormat` of **32-bit float, 48 kHz, 2 channels**, initializes the client in
   `EventsShared { autoconvert: true, buffer_duration_hns: min_period }`, direction
   `Capture`, sets an event handle, gets the capture client, starts the stream.
4. `ring.set_format(capture.format)`, marks running, signals readiness.
5. Loop until stop: `wait()` on the event handle (200 ms timeout -> "timeout" -> continue;
   other errors -> `AudioError::DeviceLost`); drain packets via `read_packet`,
   `ring.push_interleaved`, optional `detectors.process(...)`, increment frame stats and
   metric.

`WasapiLoopback` fields: `audio_client`, `event: wasapi::Handle`, `capture_client`,
`format: AudioFormat`, `block_align: usize`.

`read_packet` reads `get_next_packet_size` frames into a `block_align`-sized byte buffer,
then `raw_f32_stereo` converts: if the buffer is flagged silent it returns zeros
(`frames * 2` samples); otherwise it reads 4-byte little-endian f32 chunks, clamped to
`[-1, 1]`. Returns `None` when there are zero frames.

Error mapping: `loopback_init` -> `LoopbackInitFailed`; `device_lost` -> `DeviceLost`.

---

## 4. Speech-to-text (`crates/synapse-audio/src/stt.rs`)

### 4.1 Model

| Constant | Value |
|---|---|
| `WHISPER_TINY_INT8_FILENAME` | `whisper-tiny-int8.onnx` |
| `WHISPER_TINY_INT8_SHA256` | `147afac751f89ad8e8f82133464edc81ecff9391e98ccdcae2474384be68ec86` |

Model is **Whisper tiny, INT8 ONNX**, run with backend `ModelBackend::Cpu` (via
`ModelLoader::new(vec![ModelBackend::Cpu])`). `default_model_path()` =
`synapse_models::default_model_dir().join(WHISPER_TINY_INT8_FILENAME)`. The descriptor id
is `whisper_tiny_int8`. See [13_models_subsystem.md](13_models_subsystem.md) for loading,
hashing, and session management.

Only language **`en`** is supported (`normalize_language`); empty input defaults to `en`,
any other value yields `AudioError::LoopbackInitFailed` ("only `en` is wired in M3").

### 4.2 `Transcription`

`#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]`, `deny_unknown_fields`.

| Field | Type | Meaning |
|---|---|---|
| `text` | `String` | Decoded, trimmed transcript |
| `confidence` | `f32` | Heuristic (see below) |
| `language` | `String` | Normalized language (`en`) |
| `audio_seconds` | `f32` | Source clip duration |
| `elapsed_ms` | `u128` | Inference wall time |
| `model_path` | `PathBuf` | Resolved model path |
| `backend` | `Option<ModelBackend>` | Selected backend; `None` for blank results |
| `session_id` | `Option<u64>` | ORT session id; `None` for blank results |

### 4.3 `WhisperTinyStt`

Fields: `descriptor: ModelDescriptor`, `loader: ModelLoader`,
`loaded: Mutex<Option<LoadedModel>>`. `descriptor.input_shape` is `[1, 0]`,
`class_map` empty.

| Signature | Behavior |
|---|---|
| `new(model_path: Option<PathBuf>) -> Self` | Builds descriptor; defers loading |
| `model_path(&self) -> &Path` | |
| `is_loaded(&self) -> bool` | True if the session cache is populated |
| `transcribe_file(&self, audio_path: impl AsRef<Path>, language: impl AsRef<str>) -> AudioResult<Transcription>` | Reads bytes, `transcribe_bytes(.., audio_seconds=0.0)` |
| `transcribe_window(&self, window: &AudioWindow, language: impl AsRef<str>) -> AudioResult<Transcription>` | Converts to 16 kHz mono WAV, transcribes |

**Silence shortcut.** `transcribe_window` returns a blank `Transcription` when
`frames == 0`, `samples` empty, or `window.rms_db <= SILENCE_RMS_DB` (`-70.0` dB).
`transcribe_bytes` likewise returns blank on empty bytes.

**Sample rate / format.** Whisper requires **16 kHz mono**. `stt/window.rs` constant
`WHISPER_SAMPLE_RATE_HZ = 16_000`. `wav_bytes_from_window` builds a 44-byte WAV header
(PCM, 1 channel, 16 kHz, 16-bit) and writes downmixed `i16` samples. `mono_16khz`
resamples by nearest-source-frame indexing (`source_frame = idx * source_rate / 16000`)
and averages channels to mono.

**Inference inputs (`run_session`).** A single ORT `Session::run` with these tensors:

| Input | Shape | Value |
|---|---|---|
| `audio_stream` | `[1, bytes.len()]` | WAV/encoded bytes |
| `max_length` | `[1]` | `96` |
| `min_length` | `[1]` | `0` |
| `num_beams` | `[1]` | `1` |
| `num_return_sequences` | `[1]` | `1` |
| `length_penalty` | `[1]` | `1.0` |
| `repetition_penalty` | `[1]` | `1.0` |
| `decoder_input_ids` | `[1, 4]` | `EN_DECODER_PROMPT = [50258, 50259, 50359, 50363]` |

Output `str` is extracted via `try_extract_strings`, first item taken and trimmed. Missing
`str` output or extraction failure -> `AudioError::ModelLoadFailed`.

**Confidence heuristic (`estimate_confidence`).** Empty text -> `0.0`. For `en`, if the
transcript contains none of `hello/world/this/is/the/synapse` (space-padded) -> `0.48`;
otherwise -> `0.86`.

`audio_seconds(window) = window.frames / max(sample_rate_hz, 1)` (`stt/window.rs`).

---

## 5. Detectors (`crates/synapse-audio/src/detectors.rs`)

`DetectorProcessor::process(&mut self, samples: &[f32], format: AudioFormat)` is called per
captured packet. It computes linear RMS, peak, **crest factor** (`peak / rms`), and RMS in
dB; updates a moving RMS via `moving_rms = moving_rms * 0.95 + rms * 0.05`; updates the
three detector latches; and publishes any emitted events. `vad_speech_recent` is set to
the current `speech_active` value each pass.

### 5.1 Event kinds (string constants)

`LOUD_TRANSIENT`, `SPEECH_STARTED`, `SPEECH_ENDED`, `MUSIC_STARTED`, `MUSIC_ENDED`.
Metrics: `AUDIO_EVENTS_TOTAL` (counter, tagged by `kind`), `AUDIO_RMS_DB` (gauge).

### 5.2 Thresholds / constants

| Constant | Value | Use |
|---|---|---|
| `RECENT_EVENT_CAP` | `64` | Max retained recent events |
| `RMS_FLOOR` | `1e-6` | Linear RMS floor / dB floor reference |
| `LOUD_RATIO` | `5.0` | Surge multiple over moving RMS |
| `LOUD_ABSOLUTE_RMS` | `0.25` | Absolute loud onset threshold |
| `SPEECH_START_DB` | `-35.0` | Speech onset |
| `SPEECH_END_DB` | `-45.0` | Speech offset |
| `MUSIC_START_DB` | `-38.0` | Music onset |
| `MUSIC_END_DB` | `-48.0` | Music offset / loud reset |

### 5.3 Detectors

**Loud transient.** Fires `LOUD_TRANSIENT` (confidence base `0.95`) when not already
active and either: `rms > prior_moving * LOUD_RATIO && rms_db > -24.0` (surge), or
`rms >= 0.25 && prior_moving < 0.05` (absolute onset). Resets (`update_loud_reset`) after
`rms_db <= MUSIC_END_DB` for `sample_rate_hz / 4` frames (~0.25 s).

**Speech (RMS-gated VAD latch).** `update_speech`: at `rms_db >= -35` dB, sets
`speech_active` and emits `SPEECH_STARTED` (base `0.85`). At `rms_db <= -45` dB,
accumulates silent frames; after `sample_rate_hz / 2` (~0.5 s) clears `speech_active` and
emits `SPEECH_ENDED`.

**Music.** `update_music`: "music-like" when `rms_db >= -38` dB and
`crest in [1.2, 4.0]`. On first music-like frame, emits `MUSIC_STARTED` (base `0.7`). At
`rms_db <= -48` dB for `sample_rate_hz` frames (~1 s), emits `MUSIC_ENDED`.

These are independent latches (a frame may be both speech- and music-active).

Confidence is downscaled by `confidence_for_rms`: when `rms_db <= SPEECH_END_DB`, the base
confidence is halved; result clamped to `[0, 1]`.

### 5.4 Helpers / published event

- `rms_linear(samples)` — RMS over `[-1,1]`-clamped samples; `0.0` if empty.
- `rms_db(samples) = linear_to_db(rms_linear(samples))`.
- `linear_to_db(v) = 20 * log10(max(v, RMS_FLOOR))`.
- `silence_db() = -120.0`.

`DetectorProcessor::publish` builds an `Event` with `source = PerceptionAudio`, monotonic
`seq` (starting at 1), and `data` JSON: `rms_db`, `sample_rate_hz`, `channels`,
`confidence`, `azimuth_deg`, `crest_factor`.

### 5.5 `DetectorSnapshot`

`#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]`, `deny_unknown_fields`.

| Field | Type | Source |
|---|---|---|
| `context` | `AudioContext` | `rms_db`, `vad_speech_recent`, `recent_events`, `direction_estimate: None` |
| `moving_rms_db` | `f32` | `linear_to_db(moving_rms)` |
| `speech_active` | `bool` | |
| `music_active` | `bool` | |

`AudioContext` (`synapse-core`): `rms_db: f32`, `vad_speech_recent: bool`,
`recent_events: Vec<AudioEvent>`, `direction_estimate: Option<DirectionEstimate>`.
`AudioEvent`: `at: DateTime<Utc>`, `kind: String`, `azimuth_deg: Option<f32>`,
`confidence: f32`.

---

## 6. Direction estimation (`crates/synapse-audio/src/direction.rs`)

`estimate_direction(window: &AudioWindow) -> DirectionEstimate` combines two cues from the
stereo channels: **inter-channel level (energy panning)** and **inter-channel time
difference (ITD via normalized cross-correlation)**. Positive azimuth = left, negative =
right (see `lag_azimuth` sign).

`DirectionEstimate` (`crates/synapse-core/src/types/observation.rs`,
`#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]`):
`azimuth_deg: f32`, `confidence: f32`.

### 6.1 Constants

| Constant | Value | Meaning |
|---|---|---|
| `MIN_DIRECTION_RMS` | `1e-4` | Below this combined RMS -> undefined |
| `MAX_ITD_SECONDS` | `0.001` | Max inter-aural time difference (~1 ms) |
| `CENTER_CONFIDENCE` | `0.35` | Confidence for weak-energy/correlated case |
| `AMBIENT_CONFIDENCE` | `0.12` | Confidence for diffuse/ambient case |

### 6.2 Algorithm

Returns `undefined()` (`{0.0, 0.0}`) when `channels < 2`, `frames == 0`, fewer samples than
channels, empty channels, or combined RMS `<= MIN_DIRECTION_RMS`.
`combined_rms = sqrt((left_rms^2 + right_rms^2) * 0.5)`.

**Energy azimuth** — from the per-channel RMS, using a constant-power pan law:

```text
pan        = (4 / PI) * atan2(right_rms, left_rms) - 1
energy_deg = clamp(pan * 90, -90, 90)
```

**ITD / lag azimuth** — `max_lag = round(sample_rate_hz * 0.001)` samples (min 1).
`best_lag` scans `lag in [-max_lag, max_lag]`, scoring each with normalized
cross-correlation `dot / sqrt(left_energy * right_energy)` (clamped `[-1,1]`, 0 if denom
~0); best score is floored at 0. Then:

```text
lag_deg = clamp(-(lag_samples / max_lag) * 90, -90, 90)
```

**Blend** (`blend_azimuth`) — if `lag_score < 0.2` or `|lag_deg| < 5`, use `energy_deg`
alone; otherwise `azimuth_deg = clamp(energy_deg * 0.75 + lag_deg * 0.25, -90, 90)`.

**Confidence** (`confidence`):

```text
energy_strength = |energy_deg| / 90
lag_strength    = (|lag_deg| / 90) * clamp(lag_score, 0, 1)
strength        = max(energy_strength, lag_strength)
```

- `strength < 0.05 && lag_score < 0.2` -> `AMBIENT_CONFIDENCE` (0.12)
- `strength < 0.05` -> `CENTER_CONFIDENCE * clamp(lag_score, 0, 1)` (up to 0.35)
- otherwise -> `clamp(0.25 + 0.65*strength + 0.10*clamp(lag_score,0,1), 0, 1)`

---

## 7. Error types (`crates/synapse-audio/src/error.rs`)

`AudioResult<T> = Result<T, AudioError>`. `AudioError`
`#[derive(Clone, Debug, Eq, PartialEq, Error)]`. `code()` maps each variant to a
`synapse_core::error_codes` string constant.

| Variant | Fields | `Display` | `code()` (`error_codes`) |
|---|---|---|---|
| `DeviceLost` | `detail: String` | `audio device lost: {detail}` | `AUDIO_DEVICE_LOST` |
| `LoopbackInitFailed` | `detail: String` | `audio loopback init failed: {detail}` | `AUDIO_LOOPBACK_INIT_FAILED` |
| `SttModelNotLoaded` | `detail: String` | `audio STT model not loaded: {detail}` | `AUDIO_STT_MODEL_NOT_LOADED` |
| `ModelHashMismatch` | `path: PathBuf, expected: String, actual: String` | `audio STT model hash mismatch for {path}: expected {expected}, got {actual}` | `MODEL_HASH_MISMATCH` |
| `ModelLoadFailed` | `path: PathBuf, detail: String` | `audio STT model load failed for {path}: {detail}` | `MODEL_LOAD_FAILED` |
| `ModelBackendUnavailable` | `attempted: Vec<ModelBackend>` | `audio STT model backend unavailable; attempted {attempted:?}` | `MODEL_BACKEND_UNAVAILABLE` |

`From<ModelError>` maps `HashMismatch`/`LoadFailed`/`BackendUnavailable` to their audio
counterparts; any other `ModelError` becomes `ModelLoadFailed { path: "<unknown>", detail:
<display> }`.

**Note:** `LoopbackInitFailed` is reused beyond device init — it also signals invalid
`ring_seconds`, out-of-range `tail_seconds`, unsupported STT language, and audio-file read
failures.
