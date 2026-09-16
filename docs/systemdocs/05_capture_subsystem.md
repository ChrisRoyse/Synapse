# 05. Capture Subsystem

**Source files covered:**

- `crates/synapse-capture/Cargo.toml`
- `crates/synapse-capture/src/lib.rs`
- `crates/synapse-capture/src/backend.rs`
- `crates/synapse-capture/src/config.rs`
- `crates/synapse-capture/src/controller.rs`
- `crates/synapse-capture/src/coords.rs`
- `crates/synapse-capture/src/dpi.rs`
- `crates/synapse-capture/src/error.rs`
- `crates/synapse-capture/src/frame.rs`
- `crates/synapse-capture/src/bitmap.rs`
- `crates/synapse-capture/src/stats.rs`
- `crates/synapse-capture/src/platform/mod.rs`
- `crates/synapse-capture/src/platform/non_windows.rs`
- `crates/synapse-capture/src/platform/windows/mod.rs`
- `crates/synapse-capture/src/platform/windows/common.rs`
- `crates/synapse-capture/src/platform/windows/capture.rs`
- `crates/synapse-capture/src/platform/windows/bitmap.rs`
- `crates/synapse-capture/src/platform/windows/coords.rs`
- `crates/synapse-capture/src/platform/windows/dpi.rs`
- `crates/synapse-capture/src/platform/windows/target.rs`

---

## 1. Purpose & Overview

### 1.1 What it captures

The `synapse-capture` crate (package `synapse-capture`) is the screen-capture layer. It produces GPU-backed frames and CPU-side BGRA bitmaps from:

- **The primary monitor** (`CaptureTarget::Primary`).
- **A specific monitor** by index (`CaptureTarget::Monitor { monitor_index }`).
- **A specific window** by HWND (`CaptureTarget::Window { hwnd }`).

The capture loop streams `CapturedFrame` structs (D3D11 textures) over a bounded channel. Separate one-shot helpers in `crates/synapse-capture/src/bitmap.rs` copy screen/window regions into raw BGRA byte bitmaps (`CapturedBgraBitmap`) or WinRT `SoftwareBitmap`s, which feed OCR/detection callers. See [07_perception_subsystem.md](07_perception_subsystem.md) for the downstream consumers.

### 1.2 Platform support

Platform code is dispatched in `crates/synapse-capture/src/platform/mod.rs`:

| Platform | Module | Behaviour |
| --- | --- | --- |
| Windows (`cfg(windows)`) | `platform/windows/` | Real capture via `Windows.Graphics.Capture` (WGC), DXGI Desktop Duplication, and GDI. |
| Non-Windows (`cfg(not(windows))`) | `platform/non_windows.rs` | **Fails loud.** Every capture entry point returns `CaptureError::GraphicsApiUnsupported`. No synthetic/placeholder frames are produced (a prior `CapturedFrame::synthetic` constructor was deliberately removed, per the note in `frame.rs`). Coordinate/DPI stubs are pass-through (e.g. `screen_to_window_impl` returns the point unchanged; `init_process_dpi_awareness_impl` returns `Unsupported`).

Windows dependencies (`windows`, `windows-capture`) are gated under `[target.'cfg(windows)'.dependencies]` in `Cargo.toml`. Cross-platform dependencies: `crossbeam`, `serde`, `synapse-core`, `synapse-telemetry`, `thiserror`, `tracing`.

### 1.3 Backends

Two real backends exist on Windows, modelled by `CaptureBackend` in `crates/synapse-capture/src/backend.rs`:

| `CaptureBackend` variant | Underlying API | Targets supported |
| --- | --- | --- |
| `GraphicsCaptureApi` | `Windows.Graphics.Capture` (WGC) via the `windows-capture` crate | Monitor, primary, **and** window |
| `DxgiDuplication` | DXGI Desktop Duplication (`DxgiDuplicationApi`) | Monitor / primary **only** (window targets rejected) |

Selection is driven by `CaptureBackendPreference`:

| `CaptureBackendPreference` variant | Resolved backend (`resolved_backend`) |
| --- | --- |
| `Auto` | `GraphicsCaptureApi` (with runtime fallback to DXGI — see §7) |
| `GraphicsCaptureApi` | `GraphicsCaptureApi` |
| `DxgiDuplication` | `DxgiDuplication` |

---

## 2. Public API

All items below are re-exported from the crate root (`crates/synapse-capture/src/lib.rs`).

### 2.1 Crate constants (`lib.rs`)

| Constant | Type | Value |
| --- | --- | --- |
| `CAPTURE_CHANNEL_CAPACITY` | `usize` | `2` |
| `FRAMES_DROPPED_METRIC` | `&str` | `"synapse_capture_frames_dropped_total"` |

### 2.2 Backend types (`backend.rs`)

| Item | Signature | Notes |
| --- | --- | --- |
| `CaptureBackend` | `enum { GraphicsCaptureApi, DxgiDuplication }` | `Copy, Clone, Debug, Eq, PartialEq` |
| `CaptureBackendPreference` | `enum { Auto, GraphicsCaptureApi, DxgiDuplication }` | `Copy, Clone, Debug, Eq, PartialEq` |
| `CaptureBackendPreference::from_force_dxgi_value` | `fn(value: Option<&str>) -> Self` | Delegates to `capture_backend_from_env`. |
| `resolved_backend` | `const fn(preference: CaptureBackendPreference) -> CaptureBackend` | Maps preference → concrete backend. |
| `capture_backend_from_env` | `fn(value: Option<&str>) -> CaptureBackendPreference` | `Some("1"\|"true"\|"TRUE"\|"yes"\|"YES")` → `DxgiDuplication`; otherwise `Auto`. |

`backend_after_fallback` and `should_fallback_to_dxgi` are crate-internal (`pub(crate)`, test-only re-export). See §7.

### 2.3 Configuration types (`config.rs`)

| Item | Signature / fields |
| --- | --- |
| `CaptureTarget` | `enum { Primary, Monitor { monitor_index: u32 }, Window { hwnd: i64 } }`. `Default = Primary`. `Clone, Debug, Eq, PartialEq`. |
| `CaptureConfig` | struct (fields in §5). `Clone, Debug, Eq, PartialEq`. |
| `CaptureConfig::default()` | Returns defaults (§5). |
| `CaptureConfig::with_env_backend` | `fn(self) -> Self` — sets `backend_preference` from env var `SYNAPSE_CAPTURE_FORCE_DXGI`. |
| `CaptureConfig::selected_backend` | `const fn(&self) -> CaptureBackend` — `resolved_backend(self.backend_preference)`. |
| `ResolvedCaptureTarget` | struct `{ target: CaptureTarget, backend: CaptureBackend }`. `Clone, Debug, Eq, PartialEq`. |

### 2.4 Controller types & functions (`controller.rs`)

| Item | Signature |
| --- | --- |
| `CaptureHandle` | `struct` (opaque fields). `Debug`. |
| `CaptureHandle::receiver` | `fn(&self) -> Receiver<CapturedFrame>` (clones the rx). |
| `CaptureHandle::stats` | `fn(&self) -> Arc<CaptureStats>` |
| `CaptureHandle::channel_len` | `fn(&self) -> usize` |
| `CaptureHandle::channel_capacity` | `const fn(&self) -> usize` (returns `CAPTURE_CHANNEL_CAPACITY`) |
| `CaptureHandle::target` | `const fn(&self) -> &ResolvedCaptureTarget` |
| `CaptureHandle::config` | `const fn(&self) -> &CaptureConfig` |
| `CaptureHandle::is_stop_requested` | `fn(&self) -> bool` |
| `CaptureHandle::stop` | `fn(self) -> Result<(), CaptureError>` — sets stop flag, joins thread. |
| `CaptureController` | `struct { active: Option<CaptureHandle>, generation: u64 }`. `Debug, Default`. |
| `CaptureController::new` | `const fn() -> Self` |
| `CaptureController::switch_to` | `fn(&mut self, config: CaptureConfig) -> Result<u64, CaptureError>` — starts new handle, then stops previous; returns new generation. |
| `CaptureController::active` | `const fn(&self) -> Option<&CaptureHandle>` |
| `CaptureController::generation` | `const fn(&self) -> u64` |
| `register_capture_metrics` | `fn()` — describes the `FRAMES_DROPPED_METRIC` counter. |
| `resolve_capture_target` | `fn(config: &CaptureConfig) -> Result<ResolvedCaptureTarget, CaptureError>` |
| `spawn_capture_loop` | `fn(config: CaptureConfig) -> Result<CaptureHandle, CaptureError>` |
| `validate_hwnd` | `fn(hwnd: i64) -> Result<(), CaptureError>` |
| `validate_monitor` | `fn(monitor_index: u32) -> Result<(), CaptureError>` (public, not re-exported at root) |

`CaptureHandle` implements `Drop`, which sets the stop flag (`Ordering::Relaxed`) so a dropped handle signals the capture thread to terminate.

### 2.5 Frame types (`frame.rs`)

| Item | Signature / fields |
| --- | --- |
| `D3d11Texture` | Windows: `type` alias for `ID3D11Texture2D`. Non-Windows: empty placeholder struct. |
| `SendablePtr<T>` | `struct(T)` with `unsafe impl Send/Sync`. `new(inner: T) -> Self`, `get(&self) -> &T`. `Clone` when `T: Clone`. |
| `DxgiFormat` | `enum { Bgra8, Bgra8Srgb, Rgba8, Rgba8Srgb, Rgba16F, Rgb10A2, Rgb10XrA2, Unknown(u32) }`. `Copy, Clone, Debug, Eq, PartialEq`. |
| `CapturedFrame` | `struct { texture: SendablePtr<D3d11Texture>, width: u32, height: u32, format: DxgiFormat, captured_at: Instant, frame_seq: u64, dirty_region: Option<Rect> }`. `Clone, Debug`. |
| `CapturedSoftwareBitmap` | (Windows only) `struct { region: Rect, bitmap: windows::Graphics::Imaging::SoftwareBitmap }`. |
| `CapturedBgraBitmap` | `struct { region: Rect, width: u32, height: u32, bytes: Vec<u8> }`. Cross-platform. `Clone, Debug`. |
| `CapturedWindowBgraBitmap` | `struct { bitmap: CapturedBgraBitmap, capture_backend: &'static str }`. `Clone, Debug`. |

### 2.6 Bitmap helper functions (`bitmap.rs`)

All return `Result<_, CaptureError>`. Functions marked **(Windows only)** are `#[cfg(windows)]`; the rest exist on all platforms but fail loud off Windows.

| Function | Signature | Notes |
| --- | --- | --- |
| `captured_frame_region_to_software_bitmap` | `(frame: &CapturedFrame, region: Rect) -> Result<CapturedSoftwareBitmap, _>` | **(Windows only)** |
| `captured_frame_region_to_bgra_bitmap` | `(frame: &CapturedFrame, region: Rect) -> Result<CapturedBgraBitmap, _>` | **(Windows only)** |
| `screen_region_to_software_bitmap` | `(region: Rect) -> Result<CapturedSoftwareBitmap, _>` | **(Windows only)** |
| `screen_region_to_bgra_bitmap` | `(region: Rect) -> Result<CapturedBgraBitmap, _>` | Cross-platform signature; GDI capture on Windows, `GraphicsApiUnsupported` elsewhere. |
| `window_region_to_bgra_bitmap` | `(hwnd: i64, region: Rect, timeout_ms: u64) -> Result<CapturedWindowBgraBitmap, _>` | Passive WGC `CreateForWindow`; no automatic `PrintWindow`. |
| `window_full_frame_to_bgra_bitmap` | `(hwnd: i64, timeout_ms: u64) -> Result<CapturedWindowBgraBitmap, _>` | Whole-window via WGC native dims, no coordinate math (#1203). |
| `window_region_to_bgra_bitmap_printwindow` | `(hwnd: i64, region: Rect) -> Result<CapturedWindowBgraBitmap, _>` | Explicit opt-in `PrintWindow` capture. |
| `window_capture_region` | `(hwnd: i64) -> Result<Rect, _>` | Full window bitmap region; uses restored placement for minimized windows. |
| `client_region_to_window_region` | `(hwnd: i64, region: Rect) -> Result<Rect, _>` | Client-relative → full-window WGC coordinate space. |

### 2.7 Coordinate functions (`coords.rs`)

| Function | Signature |
| --- | --- |
| `screen_to_window` | `(point: Point, hwnd: i64) -> Result<Point, CaptureError>` |
| `window_to_screen` | `(point: Point, hwnd: i64) -> Result<Point, CaptureError>` |
| `screen_to_window_with_origin` | `const fn(point: Point, window_origin: Point) -> Point` |
| `window_to_screen_with_origin` | `const fn(point: Point, window_origin: Point) -> Point` |

### 2.8 DPI functions & types (`dpi.rs`)

| Item | Signature |
| --- | --- |
| `DpiAwarenessStatus` | `enum { Set, AlreadySet, Unsupported }`. `Copy, Clone, Debug, Eq, PartialEq`. |
| `init_process_dpi_awareness` | `fn() -> Result<DpiAwarenessStatus, CaptureError>` |
| `is_per_monitor_v2_dpi_aware` | `fn() -> bool` |
| `current_thread_priority` | `fn() -> CaptureThreadPriority` |

### 2.9 Stats types (`stats.rs`)

| Item | Signature / fields |
| --- | --- |
| `CaptureStats` | atomic counters; `Debug`. `Default` initializes all to zero/unknown. |
| `CaptureStats::frames_captured` | `fn(&self) -> u64` |
| `CaptureStats::frames_dropped` | `fn(&self) -> u64` |
| `CaptureStats::thread_priority` | `fn(&self) -> CaptureThreadPriority` |
| `CaptureStats::effective_backend` | `fn(&self) -> Option<CaptureBackend>` |
| `CaptureStats::latest_frame` | `fn(&self) -> Option<CaptureFrameStats>` (None when width or height is 0) |
| `CaptureFrameStats` | `struct { frame_seq: u64, width: u32, height: u32 }`. `Copy, Clone, Debug, Eq, PartialEq`. |
| `CaptureThreadPriority` | `enum { TimeCritical, Other(i32), Unsupported, Unknown }`. `Copy, Clone, Debug, Eq, PartialEq`. |

Mutators (`record_captured_frame`, `increment_dropped`, `set_thread_priority`, `set_effective_backend`) are `pub(crate)`. Internally, stats are stored as atomics with sentinel encodings: thread priority uses `i32::MIN` = Unknown, `i32::MIN + 1` = Unsupported, `i32::MAX` = TimeCritical; backend uses `0` = Unknown, `1` = GraphicsCaptureApi, `2` = DxgiDuplication.

---

## 3. Capture Pipeline / Algorithm

### 3.1 Threading model

`spawn_capture_loop` (`crates/synapse-capture/src/controller.rs`) does the following:

1. Calls `register_capture_metrics()`.
2. Resolves the target via `resolve_capture_target(&config)`.
3. Creates a bounded crossbeam channel: `channel::bounded(CAPTURE_CHANNEL_CAPACITY)` → capacity **2**.
4. Allocates `stop: Arc<AtomicBool>` and `stats: Arc<CaptureStats>`.
5. Builds a `CaptureThreadContext { tx, rx (clone), stop (clone), stats (clone) }`.
6. Spawns an OS thread named `"synapse-capture"` running `run_capture_thread(config, ctx)`.
7. Returns a `CaptureHandle` holding `rx`, `stop`, `stats`, the `JoinHandle`, the resolved target, and the config.

A spawn failure maps to `CaptureError::ThreadFailed`.

### 3.2 Capture thread (`run_capture_thread`)

1. `platform::set_capture_thread_priority()?` — on Windows sets `THREAD_PRIORITY_TIME_CRITICAL`.
2. Records thread priority into stats.
3. Dispatches on `config.backend_preference`:
   - `Auto`: sets effective backend = `GraphicsCaptureApi`, runs `platform::run_graphics_capture`. If it errors **and** `should_fallback_to_dxgi` returns true (the error is `GraphicsApiUnsupported`), logs a warn (`code = "CAPTURE_GRAPHICS_API_UNSUPPORTED"`), sets effective backend = `DxgiDuplication`, and runs `platform::run_dxgi_capture`. Other errors propagate.
   - `GraphicsCaptureApi`: sets effective backend, runs `run_graphics_capture` (no fallback).
   - `DxgiDuplication`: sets effective backend, runs `run_dxgi_capture`.

### 3.3 WGC capture loop (`platform/windows/capture.rs`)

`run_graphics_capture` selects the capture item from the target:

- `Primary` → `Monitor::primary()`.
- `Monitor { monitor_index }` → `Monitor::from_index(monitor_index + 1)` (the crate uses **1-based** indices; `saturating_add(1)`).
- `Window { hwnd }` → validates HWND, then `Window::from_raw_hwnd`.

`start_graphics_capture_with_item` constructs `windows_capture::settings::Settings`:

| Setting | Value derivation |
| --- | --- |
| Cursor | `cursor_visible` → `WithCursor` / `WithoutCursor` |
| Border | always `DrawBorderSettings::WithoutBorder` |
| Secondary windows | `secondary_windows` → `Include` / `Exclude` |
| Min update interval | `MinimumUpdateIntervalSettings::Custom(Duration::from_millis(min_update_interval_ms.max(1)))` |
| Dirty regions | `dirty_region_only` → `ReportAndRender` / `Default` |
| Color format | always `ColorFormat::Bgra8` |

It starts a free-threaded handler (`GraphicsHandler::start_free_threaded`) and polls in a loop every `min_update_interval_ms.max(1)` ms while `!stop && !control.is_finished()`. On stop it calls `control.stop()`, otherwise `control.wait()`.

`GraphicsHandler::on_frame_arrived` builds a `CapturedFrame`:
- `texture` = cloned raw D3D11 texture wrapped in `SendablePtr`.
- `format` mapped from WGC `ColorFormat` (`Bgra8`/`Rgba8`/`Rgba16F`).
- `dirty_region` = `union_dirty_regions(...)` — the bounding `Rect` covering all reported dirty regions (computed via min-left/top, max-right/bottom with saturating arithmetic).
- `frame_seq` is a per-handler monotonically increasing `u64` (`saturating_add(1)`).
Then `push_frame` is called. `on_closed` sets the stop flag.

### 3.4 DXGI capture loop

`run_dxgi_capture` rejects `Window` targets (`TargetInvalid`). It builds a `DxgiDuplicationApi::new(monitor)` and loops while `!stop`:
- `acquire_next_frame(timeout_ms)` where `timeout_ms = min_update_interval_ms.max(1)` cast to `u32`.
- `Ok(frame)` → build `CapturedFrame` (texture clone, `format` via `dxgi_format`, `dirty_region: None`), `push_frame`, increment `frame_seq`.
- `Err(Timeout)` → sleep `min_update_interval_ms.max(1)` ms.
- `Err(AccessLost)` → returns `CaptureError::TargetLost`.
- other errors → `GraphicsApiUnsupported` (via `dxgi_error`).

### 3.5 Backpressure: `push_frame`

`push_frame` (`controller.rs`) records the frame in stats, then `try_send`:
- `Ok` → done.
- `Full(frame)` → drains one frame from the receiver (`rx.try_recv()`), increments `frames_dropped` (and the telemetry counter), then retries `try_send`. A second failure → `ThreadFailed`. **Drop-oldest** policy on a capacity-2 channel.
- `Disconnected` → `ThreadFailed { detail: "capture receiver disconnected" }`.

### 3.6 Frame format

The streaming pipeline requests `ColorFormat::Bgra8` from WGC, so live frames are **BGRA8** GPU textures. `DxgiFormat` can also represent `Rgba8`/`Rgba16F`/`Rgb10A2` etc. for DXGI frames. CPU readback (`copy_region_bgra`, `crates/synapse-capture/src/platform/windows/bitmap.rs`) supports `Bgra8`/`Bgra8Srgb` (no swap) and `Rgba8`/`Rgba8Srgb` (swaps byte 0↔2 per pixel to produce BGRA); any other format → `GraphicsApiUnsupported`. Output bitmaps are always 32-bit BGRA, 4 bytes/pixel.

### 3.7 GPU → CPU readback (`copy_region_bgra`)

For frame-region copies: validates the region against the texture desc, creates a `D3D11_USAGE_STAGING` texture with `CPU_ACCESS_READ`, `CopySubresourceRegion` with a `D3D11_BOX` sub-rect, `Map(D3D11_MAP_READ)`, then `copy_mapped_rows` honors `RowPitch` padding (rejecting null `pData` or a pitch smaller than the row byte length), and optionally swaps RGBA→BGRA. The staging readback path is used by `captured_frame_region_to_*` functions.

### 3.8 GDI screen capture (`copy_screen_region_bgra`)

`screen_region_to_*` uses GDI: `GetDC(None)` (screen), `CreateCompatibleDC`, a thread-local cached `GdiCaptureScratch` (a top-down 32-bit `CreateDIBSection`), `BitBlt(SRCCOPY)`, then copies the DIB bits. The scratch buffer is keyed on width/height/byte_len and reused across calls on the same thread (`SCREEN_CAPTURE_SCRATCH` thread-local).

### 3.9 Per-window one-shot capture (`graphics_capture_window_frame`)

The window bitmap helpers spawn a short-lived capture loop with a forced `GraphicsCaptureApi` backend, `cursor_visible: false`, `secondary_windows: false`, `dirty_region_only: false`, `min_update_interval_ms: 16`, then `recv_timeout(timeout_ms.max(1))` for one frame and copy the selected region. All-zero output is treated as failure (`PrintWindowDisabled`) — Synapse refuses the target-reentering `PrintWindow` fallback automatically.

### 3.10 `PrintWindow` capture (`window_region_to_bgra_bitmap_printwindow`)

Explicit opt-in. Computes window extent, calls `RedrawWindow` then `PrintWindow(PW_RENDERFULLCONTENT)` into a GDI scratch DIB, crops the region, and returns `PrintWindowBlack` if the result is all zeros. `capture_backend` is reported as `"printwindow"`; WGC paths report `"graphics_capture_window_bgra"`.

---

## 4. Coordinate Systems & DPI Handling

### 4.1 Coordinate transforms (`coords.rs`, `platform/windows/coords.rs`)

`synapse_core::Point { x: i32, y: i32 }` is the unit. `synapse_core::Rect { x, y, w, h: i32 }`.

| Function | Transform |
| --- | --- |
| `screen_to_window(point, hwnd)` | Validates HWND, then Win32 `ScreenToClient`. Error → `TargetInvalid { "ScreenToClient failed" }`. |
| `window_to_screen(point, hwnd)` | Validates HWND, then Win32 `ClientToScreen`. Error → `TargetInvalid { "ClientToScreen failed" }`. |
| `screen_to_window_with_origin(point, origin)` | Pure subtraction: `(point.x - origin.x, point.y - origin.y)`. |
| `window_to_screen_with_origin(point, origin)` | Pure addition: `(point.x + origin.x, point.y + origin.y)`. |

Off Windows, `screen_to_window_impl` / `window_to_screen_impl` return the point unchanged.

### 4.2 Client-region → frame-region conversion (`bitmap.rs`)

`client_region_to_window_region` maps a client-relative `Rect` into the full-window WGC frame coordinate space, correcting for invisible DWM resize borders (#1203):

**Non-minimized path:**
- `client_{width,height}` from `GetClientRect`.
- `frame_rect` from `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`; `(frame_width, frame_height)` from `rect_extent`.
- `client_origin` = `ClientToScreen((0,0))`.
- `offset_x = client_origin.x - frame_rect.left`, `offset_y = client_origin.y - frame_rect.top` (saturating).
- Validates the client region fits within the client area, then `frame_region = { x: region.x + offset_x, y: region.y + offset_y, w, h }`, validated inside the frame bounds.

**Minimized path (`IsIconic`):** uses `window_capture_extent` (restored `WINDOWPLACEMENT.rcNormalPosition` extent, falling back to `GetWindowRect`) and `minimized_client_offset_in_window_bitmap`, which derives non-client offsets either from `(full - client)/2` heuristics or from `AdjustWindowRectExForDpi` based on window style bits, menu presence, and `GetDpiForWindow`.

### 4.3 DPI awareness (`dpi.rs`, `platform/windows/dpi.rs`)

| Function | Behaviour |
| --- | --- |
| `init_process_dpi_awareness` | `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`. `Ok(())` → `Set`; `E_ACCESSDENIED` → `AlreadySet`; other error → `ThreadFailed`. Off Windows → `Unsupported`. |
| `is_per_monitor_v2_dpi_aware` | `AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), PER_MONITOR_AWARE_V2)`. Off Windows → `false`. |
| `current_thread_priority` | `GetThreadPriority(GetCurrentThread())`; `TimeCritical` if equal to `THREAD_PRIORITY_TIME_CRITICAL`, else `Other(value)`. Off Windows → `Unsupported`. |
| `set_capture_thread_priority` | `SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)`; error → `ThreadFailed`. Off Windows → `Ok(())`. |

Per-monitor-v2 awareness ensures the capture math operates in physical pixels.

---

## 5. Configuration Knobs (`config.rs`)

`CaptureConfig` fields and `Default::default()` values:

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `target` | `CaptureTarget` | `Primary` | What to capture (primary monitor / monitor index / window HWND). |
| `min_update_interval_ms` | `u64` | `16` | Minimum WGC update interval; also DXGI acquire timeout & poll/sleep interval. Coerced to `.max(1)` at use sites (~60 fps cap at 16 ms). |
| `cursor_visible` | `bool` | `true` | Include the cursor in the capture. |
| `secondary_windows` | `bool` | `true` | Include secondary windows (WGC `SecondaryWindowSettings`). |
| `dirty_region_only` | `bool` | `true` | WGC dirty-region reporting (`ReportAndRender` vs `Default`). |
| `backend_preference` | `CaptureBackendPreference` | `Auto` | Backend selection (§7). |

Env override: `with_env_backend()` reads `SYNAPSE_CAPTURE_FORCE_DXGI`; values `1`/`true`/`TRUE`/`yes`/`YES` force `DxgiDuplication`, anything else → `Auto`.

---

## 6. Error Types (`error.rs`)

`CaptureError` (`thiserror::Error`). The `code()` method returns a stable string code (constants live in `synapse_core::error_codes`).

| Variant | Fields | `code()` | `Display` format |
| --- | --- | --- | --- |
| `GraphicsApiUnsupported` | `detail: String` | `CAPTURE_GRAPHICS_API_UNSUPPORTED` | `CAPTURE_GRAPHICS_API_UNSUPPORTED: {detail}` |
| `PrintWindowDisabled` | `detail: String` | `CAPTURE_PRINTWINDOW_DISABLED` | `CAPTURE_PRINTWINDOW_DISABLED: {detail}` |
| `PrintWindowBlack` | `detail: String` | `CAPTURE_PRINTWINDOW_BLACK` | `CAPTURE_PRINTWINDOW_BLACK: {detail}` |
| `TargetLost` | `detail: String` | `CAPTURE_TARGET_LOST` | `CAPTURE_TARGET_LOST: {detail}` |
| `TargetInvalid` | `detail: String` | `CAPTURE_TARGET_INVALID` | `CAPTURE_TARGET_INVALID: {detail}` |
| `NoDirtyRegions` | — | `CAPTURE_NO_DIRTY_REGIONS` | `CAPTURE_NO_DIRTY_REGIONS` |
| `ThreadFailed` | `detail: String` | `"CAPTURE_THREAD_FAILED"` (string literal, not an `error_codes` constant) | `CAPTURE_THREAD_FAILED: {detail}` |

Common sources:
- `GraphicsApiUnsupported`: WGC unavailable, unsupported readback format, GDI/D3D failures, or **all non-Windows builds**.
- `TargetInvalid`: invalid HWND (`<= 0` or not a live window), monitor index conversion failure, region outside texture/window/frame bounds, DXGI on window target, empty regions.
- `PrintWindowDisabled` / `PrintWindowBlack`: WGC all-zero / failed (fallback refused) vs explicit `PrintWindow` returning black.
- `TargetLost`: DXGI duplication access lost.
- `ThreadFailed`: thread spawn/panic, disconnected receiver, full channel after drop-retry, DPI/priority set failure.

`NoDirtyRegions` is defined but not constructed anywhere in this crate's source.

---

## 7. Backend Selection Logic (`backend.rs`)

### 7.1 Resolution

`resolved_backend(preference)`:
- `Auto` or `GraphicsCaptureApi` → `CaptureBackend::GraphicsCaptureApi`
- `DxgiDuplication` → `CaptureBackend::DxgiDuplication`

`CaptureConfig::selected_backend()` is a thin wrapper over `resolved_backend(backend_preference)`. This is the "static" choice used by `resolve_capture_target`.

### 7.2 Environment-driven preference

`capture_backend_from_env(value)` / `CaptureBackendPreference::from_force_dxgi_value`:
`Some("1" | "true" | "TRUE" | "yes" | "YES")` → `DxgiDuplication`; everything else (including `None`) → `Auto`.

### 7.3 Target validation in `resolve_capture_target`

If the selected backend is `DxgiDuplication` **and** the target is a `Window`, returns `TargetInvalid { "DXGI duplication supports monitor targets only" }`. Otherwise it runs `validate_target`:
- `Primary` → always Ok.
- `Monitor { idx }` → `validate_monitor` (resolves `Monitor::from_index(idx + 1)`).
- `Window { hwnd }` → `validate_hwnd` (`hwnd > 0` and `IsWindow`).

### 7.4 Runtime fallback (Auto only)

Inside `run_capture_thread`, when `preference == Auto`:
- `should_fallback_to_dxgi(preference, err)` returns `true` only for `(Auto, GraphicsApiUnsupported)`.
- `backend_after_fallback(preference, err)` returns `DxgiDuplication` for `(Auto, GraphicsApiUnsupported)`, else `resolved_backend(preference)`.

So: under `Auto`, WGC is tried first; if WGC reports `GraphicsApiUnsupported`, the loop transparently falls back to DXGI duplication (and updates `effective_backend` in `CaptureStats`). Any other WGC error propagates without fallback. Explicit `GraphicsCaptureApi` / `DxgiDuplication` preferences never fall back.

---

## 8. Cross-References

- Downstream consumers of `CapturedBgraBitmap` / `CapturedSoftwareBitmap` (OCR, detection): see [07_perception_subsystem.md](07_perception_subsystem.md).
- The `synapse-mcp` server is the primary caller of the screen/window bitmap helpers (per comments in `lib.rs` and `frame.rs`).
- Geometry primitives `Point`/`Rect` are defined in `crates/synapse-core/src/types/geometry.rs`; error-code constants in `crates/synapse-core/src/error_codes.rs`.
