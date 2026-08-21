# 05. Capture Subsystem

**Source files covered:**

- `crates/synapse-capture/Cargo.toml`
- `crates/synapse-capture/src/{backend,bitmap,config,controller,coords,dpi,error,frame,lib,stats}.rs`
- `crates/synapse-capture/src/platform/{mod,non_windows}.rs`
- `crates/synapse-capture/src/platform/windows/{bitmap,capture,common,coords,dpi,mod,target}.rs`

---

## 1. Contract

`synapse-capture` reads real Windows desktop pixels into owned BGRA8 memory. The shipped
capture backend is GDI `BitBlt` with `SRCCOPY | CAPTUREBLT`; it does not call DXGI,
Direct3D, or `Windows.Graphics.Capture`, and it never falls back to one of those APIs.

The canonical backend identity is
`gdi_bitblt_no_explicit_gpu_api`. That name is intentionally narrow: Windows, DWM, or
the display driver may accelerate GDI internally. The identity is therefore a statement
about the APIs Synapse invokes, not an attestation that physical GPU memory use is zero.
Physical GPU use requires a separate operating-system Source-of-Truth readback.

Capture is a physical visible-surface observation:

- Primary-monitor and indexed-monitor targets read their desktop rectangle.
- Window targets read the window's DWM extended-frame rectangle from the composited
  virtual desktop.
- Occluding windows remain in the returned pixels.
- Hidden, minimized, DWM-cloaked, invalid, or partly off-virtual-desktop windows fail
  with a typed error. Synapse does not reconstruct a hidden surface.
- Exact raw-CDP browser screenshots are a separate MCP/browser capability; they are the
  supported path for an owned background browser tab.

An explicit `PrintWindow` helper remains isolated for the bounded hidden-desktop worker.
Normal screen/window capture never selects it automatically because it re-enters the
target process through `WM_PRINT`/`WM_PRINTCLIENT` and may return blank pixels.

## 2. Platform and dependency closure

| Platform | Behavior |
|---|---|
| Windows | Real GDI capture using screen/memory DCs and a top-down 32-bit DIB section. |
| Non-Windows | Every pixel-producing entry point returns `CAPTURE_GRAPHICS_API_UNSUPPORTED`; no synthetic or placeholder frame is produced. |

The crate no longer depends on `windows-capture` and the workspace does not enable the
Windows D3D11, DXGI, or Graphics Capture feature sets for this path. The Windows capture
feature surface is GDI, DWM, HiDPI, and window/monitor management.

## 3. Public policy and constants

| Item | Current value / meaning |
|---|---|
| `CaptureBackend` | `GdiBitBlt` is the only executable backend. |
| `CaptureBackendPreference::GdiBitBlt` | The only admitted preference. |
| `GraphicsCaptureApi`, `DxgiDuplication`, `InvalidEnvironment` preferences | Compatibility/error states retained so contradictory requests can be rejected explicitly; none executes another backend. |
| `CAPTURE_CHANNEL_CAPACITY` | `1` |
| `DEFAULT_CAPTURE_INTERVAL_MS` | `250` |
| `MIN_CAPTURE_INTERVAL_MS` | `250` |
| `MAX_CAPTURE_INTERVAL_MS` | `60_000` |
| `MAX_CAPTURE_BYTES` | `40 * 1024 * 1024` bytes per captured BGRA surface |
| `GPU_CAPTURE_BACKENDS_COMPILED` | `false` |
| `NO_EXPLICIT_GPU_API_CAPTURE_BACKEND` | `"gdi_bitblt_no_explicit_gpu_api"` |
| `FRAMES_DROPPED_METRIC` | `"synapse_capture_frames_dropped_total"` |

`CaptureConfig::default()` is:

| Field | Default | Contract |
|---|---|---|
| `target` | `Primary` | Primary monitor, indexed monitor, or HWND. |
| `min_update_interval_ms` | `250` | Must be in `250..=60_000`; otherwise typed refusal. |
| `cursor_visible` | `true` | The streaming path composites the currently visible cursor with `DrawIconEx`; cursor-query/draw failure is an error. |
| `secondary_windows` | `false` | `true` is a WGC composition semantic and is refused. |
| `dirty_region_only` | `false` | `true` requires unavailable GPU capture metadata and is refused. |
| `backend_preference` | `GdiBitBlt` | Other preferences are refused before target resolution. |

### Environment policy

`capture_backend_preference_from_environment()` accepts only:

- `SYNAPSE_CAPTURE_BACKEND` absent, or `cpu`, `gdi`, or `gdi_bitblt`;
- legacy `SYNAPSE_CAPTURE_FORCE_DXGI` absent, or `0`, `false`, or `no`.

Any other, non-Unicode, automatic, WGC, or DXGI request returns
`CAPTURE_UNSUPPORTED_SEMANTICS`. `CaptureConfig::with_env_backend()` preserves such a
failure as `InvalidEnvironment`, and the controller rejects it. There is no silent
environment override and no fallback.

## 4. Targets and geometry

`CaptureTarget` has `Primary`, `Monitor { monitor_index }`, and
`Window { hwnd }` variants. Target resolution is repeated before every streamed frame,
so a window that becomes hidden, minimized, cloaked, invalid, or partly off-screen
terminates the worker rather than yielding stale or fabricated pixels.

For a window, `visible_window_screen_region` performs these checks in order:

1. the HWND is live (`IsWindow`);
2. the window is visible (`IsWindowVisible`);
3. it is not minimized (`IsIconic`);
4. it is not DWM-cloaked (`DWMWA_CLOAKED`);
5. its `DWMWA_EXTENDED_FRAME_BOUNDS` rectangle is non-empty and wholly inside the
   Windows virtual-screen bounding rectangle.

`client_region_to_window_region` converts a client-relative rectangle into that DWM
extended-frame coordinate space with `GetClientRect`, `ClientToScreen`, and the frame
bounds. Empty or out-of-range rectangles return `CAPTURE_TARGET_INVALID`.

The process uses per-monitor-v2 DPI awareness so these coordinates represent physical
pixels. Off Windows, coordinate helpers remain compile-compatible, but capture itself
still fails loud.

## 5. Frame and bitmap representation

`CapturedFrame` owns a `CapturedFrameBuffer`:

```text
CapturedFrame {
  pixels: { bytes: Vec<u8>, row_stride_bytes, bytes_per_pixel },
  width, height, format, captured_at, frame_seq, dirty_region
}
```

The GDI producer emits four-byte `Bgra8` pixels with a tightly checked row stride and
`dirty_region: None`. `DxgiFormat` is a retained public format name; no DXGI object or GPU
texture is held by `CapturedFrame`. Region conversion validates dimensions, stride, byte
length, and supported four-byte BGRA/RGBA formats before copying into a
`CapturedBgraBitmap`. Windows-only helpers can then create a WinRT `SoftwareBitmap` for
OCR consumers.

All capture-size arithmetic is checked. A requested surface above the 40 MiB host-memory
envelope returns `CAPTURE_UNSUPPORTED_SEMANTICS` before its pixel buffer is accepted.

## 6. One-shot capture

| Function | Behavior |
|---|---|
| `screen_region_to_bgra_bitmap` | Validates an absolute physical screen region and copies it through GDI without the cursor. |
| `screen_region_to_software_bitmap` | Same source pixels, converted to a WinRT `SoftwareBitmap` on Windows. |
| `window_region_to_bgra_bitmap` | Validates a visible window and window-relative region, translates it to physical screen coordinates, and captures one GDI frame without the cursor. |
| `window_full_frame_to_bgra_bitmap` | Captures the complete DWM extended-frame rectangle of a visible window. |
| `window_region_to_bgra_bitmap_printwindow` | Explicit bounded-worker-only `PrintWindow` path; all-zero output is `CAPTURE_PRINTWINDOW_BLACK`. |
| `captured_frame_region_to_bgra_bitmap` / `...software_bitmap` | Copies a validated subregion from an owned frame buffer. |

The `timeout_ms` arguments on the two normal window helpers are compatibility parameters;
the current GDI call is synchronous and does not claim cancellable timeout semantics.
There is exactly one attempt and no alternate-backend retry.

Normal window results identify the backend as `gdi_bitblt_visible_window_bgra` and report
attempt/retry/elapsed metadata. `PrintWindow` results identify themselves as
`printwindow`, never as normal visible-surface capture.

## 7. Streaming lifecycle and load bounds

`spawn_capture_loop` establishes reality before returning:

1. validate the no-explicit-GPU-API config and target;
2. allocate a capacity-1 crossbeam channel and `CaptureStats`;
3. capture frame sequence `0` synchronously;
4. push that real frame into the channel;
5. only then spawn the named `synapse-capture` worker.

The worker runs at Windows `THREAD_PRIORITY_BELOW_NORMAL`. Its channel is also its demand
signal: while the one retained frame remains unread, the worker performs no new desktop
pixel copy. After a consumer drains the channel, the worker waits until the configured
interval is due, resolves the target again, and captures the next frame. The 25 ms sleep
inside this wait is a low-cost stop/demand check, not a 40 fps capture cadence.

`push_frame` defensively uses drop-oldest behavior if a producer races a full channel and
increments `synapse_capture_frames_dropped_total`; routine demand-driven operation avoids
creating a frame that has no consumer. The minimum capture interval remains 250 ms even
when the channel is drained rapidly.

`CaptureHandle::stop()` sets the stop flag, joins the worker, and propagates a terminal
error. Dropping the handle requests stop. `CaptureController::switch_to()` starts and
preflights the new target before stopping the previous target; if stopping the previous
worker fails, the new worker is stopped and the error is returned.

## 8. Failure and health readback

`CaptureError::code()` exposes stable typed outcomes:

| Variant | Code / typical cause |
|---|---|
| `GraphicsApiUnsupported` | `CAPTURE_GRAPHICS_API_UNSUPPORTED`: unavailable Windows/GDI/WinRT operation, or any non-Windows pixel request. |
| `UnsupportedSemantics` | `CAPTURE_UNSUPPORTED_SEMANTICS`: forbidden backend/config, unsupported visibility, off-screen geometry, cadence, or memory envelope. |
| `TargetInvalid` | `CAPTURE_TARGET_INVALID`: dead/invalid HWND, monitor, empty rectangle, bounds, stride, or format geometry. |
| `PrintWindowDisabled` | `CAPTURE_PRINTWINDOW_DISABLED`: retained compatibility code; normal capture never attempts `PrintWindow`. |
| `PrintWindowBlack` | `CAPTURE_PRINTWINDOW_BLACK`: explicit bounded-worker `PrintWindow` returned all-zero pixels. |
| `ThreadFailed` | `CAPTURE_THREAD_FAILED`: worker spawn/panic/disconnection, priority setup, state, or byte-readback invariant failure. |
| `TargetLost`, `NoDirtyRegions` | Retained compatibility variants; the GDI loop does not use DXGI target-loss or dirty-region metadata. |

If the worker exits with an error, it logs the typed code/message and persists them in
`CaptureStats::terminal_error`; `worker_finished` is set independently. MCP health reads
that terminal state. An active runtime whose status is failed, stopped unexpectedly, or
unknown makes perception health fail rather than presenting stale frame counters as
healthy.

Health also exposes requested/effective backend and
`capture_gpu_backends_compiled=false`. Setup verifies those build/provider claims, but it
explicitly does not reinterpret them as a physical zero-VRAM measurement.

## 9. MCP integration boundaries

- `capture_screenshot` and target/window OCR use the visible-surface one-shot helpers.
  They inherit the visibility and occlusion semantics above.
- `capture_gif` captures and encodes one frame at a time, enforces the
  `250..=60_000` ms interval and its own output-size bounds, fails on the first typed
  capture/dimension/encode error, synchronizes a unique temporary file, and atomically
  publishes only the complete verified GIF.
- Raw-CDP `Page.captureScreenshot` is the exact target-scoped background browser path.
- The normal authenticated Chrome bridge refuses unsupported target-scoped page
  screenshots before mutation rather than silently switching to OS pixels.
- `hidden_desktop_pip_frame` is the explicit bounded-worker `PrintWindow` consumer. It
  does not change normal desktop capture semantics.

## 10. Operational truth

The authoritative claims are deliberately separate:

- `gdi_bitblt_no_explicit_gpu_api` proves which capture API Synapse selected.
- Captured BGRA bytes/hash and output-file readback prove what pixels were stored.
- Capture runtime health proves whether the worker is active or ended with a typed error.
- OS process/module/provider and physical GPU counters are separate Sources of Truth for
  resource-policy verification.

Do not infer hidden-window support, occlusion-free pixels, high-frequency polling, or
physical zero VRAM from the GDI backend label.

## 11. Cross-references

- Downstream OCR/detection consumers: [07_perception_subsystem.md](07_perception_subsystem.md)
- Environment variables: [03_configuration.md](03_configuration.md)
- Geometry primitives and error-code catalog: `crates/synapse-core/src/types/geometry.rs`
  and `crates/synapse-core/src/error_codes.rs`
- MCP tool behavior and readback: [16_api_tools_reference.md](16_api_tools_reference.md)
