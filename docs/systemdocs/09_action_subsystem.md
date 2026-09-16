# 09. Action Subsystem

The `synapse-action` crate (`crates/synapse-action`) is Synapse's synthetic-input engine: it emits human-like mouse motion, keyboard input, and gamepad reports against the real OS input stack, behind a serialized actor with rate limiting, an input lease, crash recovery, and an operator panic kill-switch.

**Source files covered:**

- `crates/synapse-action/src/lib.rs` — crate root and public re-exports
- `crates/synapse-action/src/emitter.rs` (+ `emitter/state.rs`, `dispatch.rs`, `keyboard.rs`, `lifecycle.rs`, `rate_limits.rs`, `routing.rs`, `backends.rs`) — serialization actor
- `crates/synapse-action/src/backend.rs`→`backend/mod.rs` (+ `software.rs`/`software/*`, `vigem.rs`, `unavailable.rs`, `recording.rs`, `text_dispatch.rs`, `mouse_coordinates.rs`) — concrete backends
- `crates/synapse-action/src/handle.rs` — `ActionHandle`, per-session input ownership
- `crates/synapse-action/src/invoke.rs` — UIA semantic element click
- `crates/synapse-action/src/hotkey.rs` — operator panic hotkey
- `crates/synapse-action/src/clipboard.rs` — clipboard snapshot/restore
- `crates/synapse-action/src/curve.rs`, `path.rs`, `velocity.rs`, `dynamics.rs`, `click_timing.rs`, `stroke.rs`, `humanize.rs` — humanization algorithms
- `crates/synapse-action/src/safety.rs`, `rate_limit.rs`, `validation.rs`, `lease.rs`, `recovery.rs`, `error.rs` — safety / limits / errors
- `crates/synapse-action/Cargo.toml`

> The action emitter is driven by the M3 reflex runtime for combo scheduling and aim tracking; see [10_reflex_subsystem.md](10_reflex_subsystem.md). MCP tools (`act_*`, `control_lease_*`) wrap this crate; see [15_mcp_server_architecture.md](15_mcp_server_architecture.md).

---

## 1. Overview

### 1.1 How synthetic input is emitted

On Windows the **software backend** (`backend/software/`) emits real input through Win32 `SendInput` (`crates/synapse-action/src/backend/software/input.rs`). Keyboard text uses `KEYBDINPUT` packets; mouse buttons/scroll/strokes use `MOUSEINPUT` packets. Cursor positioning prefers `SetPhysicalCursorPos` / `GetPhysicalCursorPos` and falls back to a `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` `SendInput` packet, with a `±2 px` readback tolerance and monitor-DPI compensation (`backend/software/mouse.rs`). Named key dispatch and key-direction press/release use the `enigo` crate; raw scancodes (`KeyCode::HidCode`) go through `enigo.raw`. Text typing emits Unicode units via `KEYEVENTF_UNICODE` keydown+keyup pairs.

The **`ViGEm` backend** (`backend/vigem.rs`) emits virtual X360/DS4 gamepad reports through `vigem-client`. The **hardware backend** is retired and fails closed (`backend/unavailable.rs`, `HardwareUnavailableBackend`). The **recording backend** (`backend/recording.rs`) captures `RecordedInput` events instead of touching the OS, used cross-platform for tests and benches.

Non-Windows: software uses `software_non_windows.rs` and `enigo` with the `x11rb` feature; clipboard uses `arboard` (Linux/X11). Many Windows-only paths (`invoke_element`, operator hotkey) fail closed off Windows.

### 1.2 Humanization goal

Synthetic motion and typing are made to resemble human input rather than instant teleports: Bézier / minimum-jerk / `WindMouse` cursor paths, Gaussian tremor, overshoot-and-correct, per-keystroke Gaussian inter-keystroke intervals biased by common English bigrams, and OS-derived double-click timing. All randomness is **deterministic**: every humanization function uses a `DeterministicRng` (a SplitMix64-style generator) seeded either from an explicit seed or from a hash of the inputs, so the same request yields the same motion. See §3.

---

## 2. Public API per module (full signatures)

All public items are re-exported from `lib.rs`. Modules: `pub mod` for `backend, click_timing, clipboard, curve, dynamics, emitter, error, handle, hotkey, humanize, invoke, lease, path, rate_limit, recovery, safety, stroke, validation, velocity`.

### 2.1 `emitter` — serialization actor (`crates/synapse-action/src/emitter/`)

The `ActionEmitter` is a single-actor task that owns held-input state and serializes all action dispatch. It is fed by an `ActionHandle` over Tokio mpsc channels.

| Item | Signature / Notes |
|---|---|
| `ActionEmitter::new` | `fn(rx: Receiver<ActionMessage>, snapshot_rx: Receiver<ActionSnapshotMessage>) -> Self` — production backends |
| `ActionEmitter::new_with_backends` | `fn(rx, snapshot_rx, backends: Backends) -> Self` |
| `ActionEmitter::new_with_backends_and_policy` | `fn(rx, snapshot_rx, backends, Arc<RwLock<BackendResolutionPolicy>>) -> Self` |
| `ActionEmitter::channel` | `fn() -> (ActionHandle, ActionEmitterSnapshotHandle, Self)` |
| `ActionEmitter::channel_with_backend(s)` | variants taking `Arc<dyn ActionBackend>` / `Backends` |
| `ActionEmitter::spawn` | `fn(cancel: CancellationToken) -> (ActionHandle, ActionEmitterSnapshotHandle, JoinHandle<ActionStateSnapshot>)` |
| `ActionEmitter::run` | `async fn(self, cancel) -> ActionStateSnapshot` (also `run_with_connection_closed_cancel`, `run_with_shutdown_reason`) |
| `ActionEmitter::snapshot` | `fn(&self) -> ActionStateSnapshot` (held keys/buttons/pads + timer keys) |
| `ActionEmitter::pending_len` | `fn(&self) -> usize` |
| `ActionEmitter::rate_limit_control` | `fn(&self) -> BackendRateLimitControl` |
| `HELD_KEY_MAX_DURATION_MS` | `const u64 = 30_000` — stuck-key auto-release ceiling |

Actor loop (`lifecycle.rs::run_with_shutdown_reason`) uses `tokio::select!` with `biased;` priority: **(1)** safety lane (`safety_rx`, unbounded — `ReleaseAll`/`KeyUp`), **(2)** held-key auto-release timer events, **(3)** snapshot requests, **(4)** shutdown cancel → `release_all`, **(5)** connection-closed cancel → `release_all`, **(6)** normal action queue (`rx`). After a `ReleaseAll` ack, all pending normal-queue actions are drained and rejected with `SAFETY_RELEASE_ALL_FIRED`. On drop, all held-key timers abort.

Dispatch (`dispatch.rs::execute`): validate → resolve backend → consume rate-limit token (unless `ReleaseAll`/`KeyUp`) → cancel any release-target auto-release timers → run `backend.execute(&action, &mut state)` on `spawn_blocking` (a panic/join failure fails closed with `BackendUnavailable` and resets state to empty) → on success, schedule a 30 s auto-release timer for any newly-held key.

State types (`emitter/state.rs`):

| Item | Signature / Notes |
|---|---|
| `EmitState` | actor-owned held state: `held_keys`/`held_buttons` as `BitSet`, per-index `BTreeSet<ResolvedBackend>`, `pad_state: HashMap<PadId, GamepadReport>`, `active_backend` |
| `EmitState::new` / `Default` | empty state |
| `EmitState::snapshot` | `fn(&self) -> ActionStateSnapshot` |
| `ActionStateSnapshot` | `held_keys`, `held_key_bits`, `held_key_timer_keys`, `held_key_timer_count`, `held_buttons`, `held_button_bits`, `pad_state`, `held_keys_by_backend`, `held_buttons_by_backend` |
| `ActionEmitterSnapshotHandle::snapshot` | `async fn(&self) -> ActionResult<ActionStateSnapshot>` |
| `ActionSnapshotMessage` | `= oneshot::Sender<ActionStateSnapshot>` |

Rate-limit control (`emitter/rate_limits.rs`):

| Item | Signature / Notes |
|---|---|
| `BackendRateLimitControl::try_snapshot` | `fn(&self) -> ActionResult<BackendRateLimitSnapshot>` |
| `BackendRateLimitControl::override_backend` | `fn(&self, ResolvedBackend, capacity: u32, refill_rate_per_s: u32) -> ActionResult<BackendRateLimitOverrideReadback>` |
| `BackendRateLimitControl::reset_backend` | `fn(&self, ResolvedBackend) -> ActionResult<BackendRateLimitOverrideReadback>` (restores default) |
| `BackendRateLimitSnapshot` | `{ software, vigem, hardware: TokenBucketSnapshot }` |

`Backends` (`emitter/backends.rs`): per-resolved-kind `Arc<dyn ActionBackend>` table (`production()`, `all_routed_to(...)`, `pick(ResolvedBackend)`).

### 2.2 `backend` — concrete backends (`crates/synapse-action/src/backend/mod.rs`)

| Item | Signature / Notes |
|---|---|
| `trait ActionBackend` | `fn execute(&self, action: &Action, state: &mut EmitState) -> Result<(), ActionError>` (`Send + Sync`) |
| `enum ResolvedBackend` | `Software \| Vigem \| Hardware`; `as_str()`, `to_backend() -> Backend` |
| `struct BackendResolutionPolicy` | `{ default_backend, keyboard_default, mouse_default, pad_default: Backend }` |
| `BackendResolutionPolicy::auto` | all `Backend::Auto` |
| `BackendResolutionPolicy::from_profile_backends` | `fn(ProfileBackends) -> Self` |
| `BackendResolutionPolicy::auto_backend_for` | `fn(self, &Action) -> ResolvedBackend` |
| `resolve_backend` | `fn(Backend, &Action) -> Result<ResolvedBackend, ActionError>` |
| `resolve_backend_with_policy` | `fn(Backend, &Action, BackendResolutionPolicy) -> Result<ResolvedBackend, ActionError>` |
| `VigemBackend` | `new()`, `ensure_ready() -> Result<(), ActionError>`; `Drop` neutralizes all pads on Windows |
| `HardwareUnavailableBackend` | `new()`; `execute` always errors `BackendUnavailable` (hardware backend removed) |
| `RecordingBackend` | `new()`, `events()`, `event_count()`, `events_since(n)`, `held_keys()`, `held_buttons()`, `pad_state()` |
| `RecordedInput` | enum of recorded events (KeyDown/Up, DelayMs, UnicodeUnitDown/Up, MouseMove*, MouseButton*, MouseStrokePoint, MouseScroll, AimAt, ComboAt, PadButton*/Stick/Trigger/Report, ReleaseAll) |

**Auto-resolution defaults** (`auto_backend_for`): keyboard→Software, mouse→Software, pad→`Vigem`, `ReleaseAll`→Software (unless the class/default policy specifies a concrete backend). The actor routes pads and `ReleaseAll` as `Backend::Auto` regardless of the per-action field (`emitter/routing.rs::requested_backend`).

`SoftwareBackend::execute` dispatches by `Action` variant to `keyboard::*`, `mouse::*`, `text::type_text`, `combo`, `release_all`; gamepad variants return `BackendUnavailable`. Public helpers `software::cursor_position()` and `software::set_cursor_position(Point) -> Result<Point, …>`.

### 2.3 `handle` — `ActionHandle` (`crates/synapse-action/src/handle.rs`)

`ACTION_QUEUE_CAPACITY = 256` (bounded normal queue). `ActionMessage = (Action, oneshot::Sender<ActionResult<()>>)`. `RELEASE_ALL_HANDLE: OnceLock<ActionHandle>` is the process-global handle the panic hook fires through.

| Item | Signature / Notes |
|---|---|
| `ActionHandle::new` / `channel` | `channel() -> (Self, Receiver<ActionMessage>)` |
| `with_session_id` | `fn(&self, Option<String>) -> Self` (clones sharing ledger + gate) |
| `install_combo_scheduler` | `fn(&self, Arc<dyn ActionComboScheduler>) -> ActionResult<()>` (routes `Action::Combo` into the reflex runtime) |
| `execute` | `async fn(&self, Action) -> ActionResult<()>` (validate, enqueue, await ack; records session input ownership on success) |
| `try_execute` | `fn(&self, Action) -> ActionResult<()>` (non-blocking enqueue) |
| `fire_release_all_blocking_with_timeout` | `fn(&self, Duration) -> ActionResult<()>` (synchronous; used by the panic hook) |
| `session_inputs_snapshot` | `fn(&self) -> ActionResult<SessionInputSnapshot>` |
| `release_session_inputs` | `async fn(&self, &str) -> ActionResult<SessionReleaseSummary>` (targeted key-up/mouse-up/neutral-pad for inputs no other session owns; never `ReleaseAll`) |
| `release_session_inputs_and_lease` | `async fn(&self, &str) -> ActionResult<SessionInputLeaseReleaseSummary>` (release inputs, verify ledger cleared, then release lease) |
| `trait ActionComboScheduler` | `fn schedule_combo(&self, Vec<ComboStep>, Backend) -> ActionResult<()>` |

The per-session ownership ledger tracks which session(s) hold each key/button/pad (shared inputs retained until the last owner releases). Safety actions (`ReleaseAll`, `KeyUp`) are routed through the unbounded `safety_tx` lane so they cannot be blocked by a full normal queue; sending `ReleaseAll` also calls `request_release_interrupt()`.

### 2.4 `invoke` — semantic element click (`crates/synapse-action/src/invoke.rs`)

| Item | Signature / Notes |
|---|---|
| `invoke_element` | `fn(&ElementId) -> ActionResult<()>` (Windows: UIA `InvokePattern`/`Toggle`/`SelectionItem`/`ExpandCollapse`/`LegacyIAccessible.DoDefaultAction`; no cursor move. Non-Windows: `BackendUnavailable`) |
| `click_element_or_fallback<B: ActionBackend>` | `fn(&ElementId, &B, &mut EmitState, MouseButton) -> ActionResult<ElementClickOutcome>` (semantic UIA click; does **not** synthesize a coordinate click — unsupported patterns → `ACTION_ELEMENT_PATTERN_UNSUPPORTED`) |
| `enum ElementClickOutcome` | `Invoked`, `Toggled{before,after}`, `Selected{was,is}`, `Expanded{…}`, `Collapsed{…}`, `LegacyDefaultAction{Option<String>}`, `CoordinateFallback(CoordinateFallbackPlan)` |
| `struct CoordinateFallbackPlan` | `{ screen_point: Point, window_point: Point }` |

A11y errors map to action errors: stale/`ElementNotAvailable` → `TransientElementExpired`; unsupported/value-unsupported → `ElementPatternUnsupported`; read-only/not-enabled → `TargetInvalid`; invalid id/no foreground → `ElementNotResolved`; activation refused → `ForegroundActivationRefused`.

### 2.5 `hotkey` — operator panic kill-switch (`crates/synapse-action/src/hotkey.rs`)

| Item | Signature / Notes |
|---|---|
| `install_operator_hotkey<F: Fn()+Send+'static>` | `fn(F) -> ActionResult<OperatorHotkeyGuard>` |
| `OperatorHotkeyGuard` | RAII; unregisters hook/hotkey and joins threads on drop |
| `enum OperatorHotkeyStatus` | `Unknown=0, Registered=1, DisabledByEnv=2, Unavailable=3`; `.label()` |
| `operator_hotkey_status` / `set_operator_hotkey_status` | process-global atomic for `/health` |
| `operator_release_epoch` | `fn() -> u64` |
| `operator_release_requested_since` | `fn(epoch: u64) -> bool` |
| `request_release_interrupt` | `fn()` (increments epoch; long sleeps poll this) |

Hotkey is `SYNAPSE_OPERATOR_HOTKEY` / `SYNAPSE_MCP_OPERATOR_HOTKEY`, default `ctrl+alt+shift+p`. Parsing **requires** Ctrl+Alt+Shift plus exactly one alphanumeric key. On Windows it installs both a `WH_KEYBOARD_LL` low-level keyboard hook (re-armed to the chain head every `500 ms`) and a `RegisterHotKey` backup; both run on `THREAD_PRIORITY_HIGHEST` threads. Duplicate signals within `750 ms` are debounced. On fire it calls `request_release_interrupt()` and dispatches the handler (typically firing `ReleaseAll` and operator lease preemption).

### 2.6 `clipboard` (`crates/synapse-action/src/clipboard.rs`)

| Item | Signature / Notes |
|---|---|
| `enum ClipboardFormat` | `Text` (CF_TEXT, ASCII-only on writes), `Unicode` (CF_UNICODETEXT) |
| `ClipboardSnapshot` | opaque captured formats; `format_count()`, `byte_count()`, `is_empty()` |
| `ClipboardRestoreReport` | `{ format_count, byte_count }` |
| `read_text` | `fn(ClipboardFormat) -> ActionResult<String>` |
| `write_text` | `fn(ClipboardFormat, &str) -> ActionResult<()>` (CF_TEXT non-ASCII → `BackendUnavailable`) |
| `clear` | `fn() -> ActionResult<()>` |
| `snapshot` | `fn() -> ActionResult<ClipboardSnapshot>` |
| `restore` | `fn(&ClipboardSnapshot) -> ActionResult<ClipboardRestoreReport>` |
| `with_restored_clipboard<T>` | `fn(impl FnOnce() -> ActionResult<T>) -> ActionResult<T>` (snapshot, run op, always restore) |

Windows uses raw `OpenClipboard`/`EnumClipboardFormats`/`GetClipboardData`/`SetClipboardData` with a message-only owner window; `OpenClipboard` retries for `250 ms` at `10 ms` intervals. Linux/X11 uses `arboard` and supports only the single text snapshot it captured. Other targets fail closed.

---

## 3. Humanization algorithms (steps and formulas)

All RNGs are a SplitMix64-style `DeterministicRng`: `state += 0x9e3779b97f4a7c15`, then the `0xbf58476d1ce4e5b9` / `0x94d049bb133111eb` mix with `>>30/>>27/>>31` shifts. Gaussian samples use Box–Muller: `z0 = sqrt(-2·ln(u1))·cos(2π·u2)`. Seed hashing mixes input bits with `mix_u64`.

### 3.1 `curve.rs` — `sample_curve` (cursor aim path)

`sample_curve(curve: &AimCurve, start: Point, end: Point, duration_ms: u32, seed: Option<u64>) -> Vec<Point>`. Sample count = `max(8, duration_ms/4)`; endpoints are forced to exactly `start` and `end`.

- `AimCurve::Instant` → `[start, end]` (single jump).
- `AimCurve::Linear` → linear interpolation per axis (`t`).
- `AimCurve::EaseInOut` → smoothstep `3t² − 2t³` per axis.
- `AimCurve::Bezier { p1, p2 }` → cubic Bézier of the normalized axis parameter using control points `p1`,`p2`:
  ```
  axis(t) = 3(1−t)²·t·p + 3(1−t)·t²·p₂ + t³     (with implicit P0=0, P3=1)
  ```
- `AimCurve::Natural { params }` (`AimNaturalParams`): a deterministic Bézier with jittered control points centered at `(0.25,0.10)` and `(0.75,0.90)` (Gaussian, σ = `control_point_jitter`). Steps:
  1. Seed from `seed` ⊕ `params.seed` ⊕ hash of `start/end/duration/params`.
  2. With probability `overshoot_prob`, set the Bézier target beyond `end` by a factor uniformly drawn from `overshoot_factor_range`; else target = `end`.
  3. Reserve `micro_correct_steps` (≤ count−2) trailing samples; the remaining samples follow the Bézier.
  4. Interior samples jitter their time parameter by Gaussian σ = `timing_stddev_ms / duration_ms` and jitter position by Gaussian σ = `tremor_stddev_px`.
  5. Micro-correction tail linearly settles from the last main sample back to the true `end`.

### 3.2 `path.rs` — geometric path sampling

`PathSpec` → `SpatialPath`/`ArcLengthPath`. `t`-parameter (`point_at`) vs. arc-length-parameterized (`point_at_arclen`, LUT of `DEFAULT_ARCLEN_LUT_SEGMENTS = 2048` segments, linear interpolation between LUT entries).

| Public fn | Notes |
|---|---|
| `path_point_at(&PathSpec, t) -> PathResult<PathPoint>` | t ∈ [0,1] |
| `sample_path(&PathSpec, samples) -> PathResult<Vec<PathPoint>>` | ≥2 samples |
| `path_length(&PathSpec) -> PathResult<f64>` | LUT arc length |
| `path_point_at_arclen(&PathSpec, s)` / `sample_path_arclen` | s ∈ [0, length] |

Supported `PathSpec` curves: `Line` (lerp), `Arc`/`Circle` (parametric `center + r·(cosθ, sinθ)`), `CubicBezier` (Bernstein basis), `Polyline` (per-segment lerp), `CatmullRom` (centripetal/parameterized via `alpha`∈[0,1], `tension`∈[0,1]; knot spacing `tᵢ₊₁ = tᵢ + |Δp|^alpha`, evaluated as a cubic Hermite). Validation rejects non-finite points, degenerate segments/curves, out-of-range `t`/`s`, and too-few points (Polyline ≥2; CatmullRom ≥4 open / ≥3 closed).

### 3.3 `velocity.rs` & `dynamics.rs`

**`velocity.rs`** maps normalized time→position for a `VelocityProfile`:

| Profile | `position_at_time(t)` | `normalized_velocity_at_time(t)` |
|---|---|---|
| `Constant` / `Linear` | `t` | `1.0` |
| `EaseInOut` | smoothstep `3t² − 2t³` | `6t(1−t)` |
| `MinimumJerk` | `10t³ − 15t⁴ + 6t⁵` | `30t²(1−t)²` |

- `time_at_position(profile, position)` inverts the profile (closed-form for linear; `INVERSION_STEPS = 64` bisection for eased/min-jerk).
- `sample_timed_path / sample_timed_arclen_path(spec/path, profile, samples, duration_ms) -> Vec<TimedPathPoint>` — positions equally spaced in arc length, elapsed time = `duration_ms · time_at_position(...)`. `TimedPathPoint { elapsed_ms, arclen, point }`.
- `fitts_law_duration_ms(distance_px, target_width_px, a_ms, b_ms) -> f64`:
  ```
  duration = a + b · log₂(distance / target_width + 1)
  ```

**`dynamics.rs`** builds the per-character typing schedule:

- `sample_typing_schedule(text, &KeystrokeDynamics, seed) -> Vec<KeystrokeEvent>`. `KeystrokeEvent { char, key: Key, iki_ms_before: u32, modifier_state: ModifierMask }`. Index 0 always has `iki_ms_before = 0`.
- `ModifierMask` bitflags: `NONE=0`, `SHIFT=1`, `CTRL=1<<1`, `ALT=1<<2`, `META=1<<3`; `bits()`, `contains()`, `is_empty()`.
- Per-character inter-keystroke interval (`sample_iki_ms`):
  - `KeystrokeDynamics::Burst` → `0`.
  - `KeystrokeDynamics::Linear { ms_per_char }` → constant.
  - `KeystrokeDynamics::Natural { params: KeystrokeNaturalParams }` → if `bigram_bias` and the (previous,current) pair is a common bigram, IKI = `round(mean_iki_ms · 0.75)`; otherwise `round(mean_iki_ms + Gaussian(σ = stddev_ms))`. Negative/non-finite values clamp to 0.
- `BIGRAMS`: the 50 most-common English bigrams used for the 0.75× speedup:
  ```
  th he in er an re on at en nd ti es or te of ed is it al ar st to nt ng
  se ha as ou io le ve co me de hi ri ro ic ne ea ra ce li ch ll be ma si om ur
  ```
- `key_for_char` maps characters to `Key` + `ModifierMask`: `A–Z`→lowercase letter + SHIFT; shifted symbols (`!@#$%^&*()_+{}|:"<>?~`) map to their base key + SHIFT; `\n`/`\r`→enter, `\t`→tab, space→space; other characters become a `KeyCode::Symbol` Unicode key.

### 3.4 `click_timing.rs` — double-click / inter-click delay

Constants: `DEFAULT_DOUBLE_CLICK_WINDOW_MS = 500`, `MIN_INTER_CLICK_DELAY_MS = 30`, `MAX_INTER_CLICK_DELAY_MS = 150`.

- `cached_double_click_timing() -> DoubleClickTiming { window_ms, inter_click_delay_ms, source }` — cached in a `OnceLock`. On Windows `window_ms` comes from `GetDoubleClickTime()` (0 → default 500); `source = "windows_get_double_click_time"`. Non-Windows uses the default; `source = "default_non_windows"`.
- `inter_click_delay_ms_for_window(window_ms) -> u32` = `clamp(window_ms/4, 30, 150)`, then capped to `window_ms − 1` (window floored at 2). Guarantees `delay < window`.

### 3.5 `stroke.rs` — timed mouse stroke planning

`STROKE_TICK_MS = 1.0` (1 ms cadence). `plan_timed_stroke(path: &PathSpec, profile: VelocityProfile, timing: &StrokeTiming, motion_model: StrokeMotionModel, humanize: Option<HumanizeParams>) -> StrokeResult<StrokePlan>`. `StrokePlan { samples: Vec<TimedPathPoint>, duration_ms, path_length_px }`.

Steps:
1. Build `ArcLengthPath`; `path_length_px = length()`.
2. Duration: `StrokeTiming::DurationMs` → that value; `StrokeTiming::SpeedPxPerSec` → `path_length_px / px_per_sec · 1000`. Must be finite and > 0.
3. Sample by motion model:
   - `StrokeMotionModel::Path` — `ceil(duration_ms / 1.0) + 1` samples (≥2), arc-length-uniform position via `position_at_time(profile, …)`, 1 ms cadence.
   - `StrokeMotionModel::WindMouse { gravity, wind, max_step, damped_distance, seed }` — **line paths only** (else `WindMouseRequiresLine`). The WindMouse algorithm iterates a point toward the target with a gravity pull (`gravity/distance · Δ`), a stochastic wind term (decayed by `1/√3`, injected by `±wind/√5` Gaussian-ish uniform), velocity clamped to `max_step·U(0.5,1.0)` when overspeed, switching to damped mode within `damped_distance`. Bounded to `WIND_MOUSE_MAX_POINTS = 60_001` points and a `1.0 px` convergence tolerance (else `WindMouseDidNotConverge`); resulting points are spread uniformly in time across `duration_ms`.
4. Apply `humanize_timed_path(&samples, humanize)` (§3.6).

`screen_point_from_path_point(point, index) -> StrokeResult<Point>` rounds a finite, in-`i32`-range `PathPoint` to a physical pixel `Point` (else `ScreenPointOutOfRange`).

### 3.6 `humanize.rs` — tremor / overshoot / micro-pauses

`humanize_timed_path(samples: &[TimedPathPoint], params: Option<HumanizeParams>) -> HumanizeResult<Vec<TimedPathPoint>>`. `None` params, or all of `tremor_base_stddev_px`/`overshoot_prob`/`micro_pause_prob` ≈ 0, returns the samples unchanged. `DEFAULT_CORRECTION_MS = 16.0`.

Per interior sample (endpoints untouched):
1. **Tremor**: jitter x and y by `Gaussian(σ)` where
   ```
   σ = tremor_base_stddev_px · (1 + tremor_velocity_scale / (1 + v))
   ```
   and `v` is the instantaneous arc-length velocity (`|Δarclen|/|Δt|`). Slower motion ⇒ larger tremor.
2. **Micro-pause**: with probability `micro_pause_prob`, add a duplicate sample at the same point with elapsed time advanced by a uniform draw in `micro_pause_ms_range`; this offset accumulates for all later samples.
3. **Overshoot** (`apply_overshoot`, once at the end): with probability `overshoot_prob`, the final sample is pushed past the true endpoint along the last motion direction by `distance · (factor − 1)` (factor uniform in `overshoot_factor_range`, each ≥ 1.0), then a correction sample returns to the true endpoint after `DEFAULT_CORRECTION_MS + min(micro_pause, 16ms)`.

Validation: ≥2 samples, finite + monotonic timestamps, non-negative tremor params, probabilities ∈ [0,1], overshoot factors ≥ 1.0 and ordered, micro-pause range ordered.

---

## 4. Safety, rate limiting, validation, lease, recovery

### 4.1 `safety.rs` — panic hook

`install_panic_hook()` (idempotent via `OnceLock`) wraps the existing panic hook: on panic it fires `RELEASE_ALL_HANDLE.fire_release_all_blocking_with_timeout(10 ms)` (`PANIC_RELEASE_ALL_TIMEOUT_MS = 10`), logs `SAFETY_RELEASE_ALL_FIRED`, then chains to the previous hook. This guarantees a panicking task still drops all held physical input.

Stuck-key safety (emitter): every newly-held key gets a `HELD_KEY_MAX_DURATION_MS = 30_000` ms auto-release timer; on expiry the emitter emits `KeyUp` and logs `STUCK_KEY_AUTO_RELEASED`.

### 4.2 `rate_limit.rs` — per-backend token buckets

| Constant | Value |
|---|---|
| `SOFTWARE_RATE_LIMIT_PER_S` | `5_000` (also used for `Hardware`) |
| `VIGEM_RATE_LIMIT_PER_S` | `1_000` |

`TokenBucket { capacity, tokens, refill_rate_per_s, last_refill }` is lock-free (atomics, CAS loops). `new(capacity, refill_rate_per_s)`, `for_backend(ResolvedBackend)` (capacity = refill rate), `try_consume(n) -> bool`, `refill()`, `retry_after_ms(n) -> u64`, `snapshot() -> TokenBucketSnapshot`. Refill adds `elapsed_ns · rate / 1e9` tokens (sub-token elapsed time preserved), clamped to capacity; a zero rate never refills. The emitter consumes **1 token per action** (`rate_limits::consume`); `ReleaseAll` and `KeyUp` are exempt (`action_consumes_rate_limit`). Exhaustion → `ActionError::RateLimited { retry_after_ms }`.

### 4.3 `validation.rs`

`MAX_DRAG_DISTANCE_PX = 4096.0`. `validate_action(&Action) -> ActionResult<()>` currently blocks only `Action::MouseDrag` whose straight-line distance exceeds the limit → `DragDistanceExceedsLimit`. Called at handle enqueue and again inside every backend `execute`.

### 4.4 `lease.rs` — foreground input lease

Process-global, renewable, TTL-bounded lease over the single physical cursor/keyboard/foreground (epic #719). **Refuse, not block**: contention returns `Busy` immediately. Only *leased foreground* `SendInput`/`SetPhysicalCursorPos` actions need the lease; background tiers (CDP, UIA patterns, `PostMessage`) never take it.

| Constant | Value |
|---|---|
| `DEFAULT_LEASE_TTL_MS` | `5_000` |
| `MIN_LEASE_TTL_MS` | `100` |
| `MAX_LEASE_TTL_MS` | `30_000` |
| `OPERATOR_LEASE_OWNER_SESSION_ID` | `"__operator__"` |
| `OPERATOR_PREEMPT_LEASE_TTL_MS` | `30_000` (= MAX) |

| API | Notes |
|---|---|
| `try_acquire(session_id, ttl) -> LeaseOutcome` | `Acquired` / `Renewed` (same owner) / `Busy{holder,retry_after_ms}` / `CleanupPending{expired,retry_after_ms=100}` |
| `renew(session_id, Option<ttl>)` / `release(session_id)` | `Result<LeaseStatus, LeaseError::NotHeld>` |
| `handoff(from, to, ttl)` | atomic transfer without an unheld gap → `LeaseHandoff{prior,current}` |
| `release_if_owner(session_id) -> bool` | infallible owner-scoped release |
| `force_preempt(reason)` / `force_clear(reason)` / `force_clear_if_owner(session_id, reason)` | operator overrides; preempt installs the `__operator__` holder for 30 s |
| `ttl_from_ms(ms) -> Duration` | clamps to [100, 30000] |
| `status() -> LeaseStatus` | lazily expires lapsed lease; never blocks `/health` |
| `expired_cleanup_snapshot()` / `complete_expired_cleanup(session_id)` | pending held-input cleanup ledger |

A lapsed owner is expired lazily but leaves a **cleanup-pending** record; no new session can acquire until that session's held-input ledger is drained (prevents a freed lease while a crashed session still holds physical input). `force_preempt` leaves a visible bounded operator holder (agents fail closed, do not instantly reacquire). The mutex is poison-recovering so a lease that panicked mid-action stays reclaimable. Re-exported in `lib.rs` as `complete_expired_input_lease_cleanup`, `expired_input_lease_cleanup_snapshot`, `force_clear_input_lease`, `force_preempt_input_lease`, `input_lease_ttl_from_ms`.

### 4.5 `recovery.rs` — crash recovery ledger

A durable JSONL ledger of currently-held inputs so a *previous* daemon's stuck keys/buttons/pads are released at next startup. Path resolution: `SYNAPSE_ACTION_RECOVERY_FILE` env → daemon DB dir → `SYNAPSE_DB` env → `%LOCALAPPDATA%/synapse/action_recovery.jsonl`.

| API | Notes |
|---|---|
| `configure_crash_recovery_file(Option<&Path>) -> ActionResult<PathBuf>` | sets the process-wide path |
| `recover_stale_inputs_from_configured_path() -> ActionResult<ActionCrashRecoveryReport>` | re-exported as both names |
| `ActionCrashRecoveryReport` | `{ recovery_file, recovered_keys, recovered_buttons, recovered_pads, ignored_trailing_bytes }` |

The software/`ViGEm` backends append `KeyHeld`/`KeyReleased`/`ButtonHeld`/`ButtonReleased`/`PadHeld`/`PadReleased` events (`fsync` after each write); when the reconstructed ledger is empty the file is deleted. On startup, recovery replays releases (keys/buttons reversed, pads neutralized) through fresh `SoftwareBackend` + `VigemBackend`, then removes the file. A torn trailing line is detected (`ignored_trailing_bytes`) and that partial record is ignored.

---

## 5. Error types (`crates/synapse-action/src/error.rs`)

`ActionResult<T> = Result<T, ActionError>`. Each variant has `.code() -> &'static str` (mapping to `synapse_core::error_codes`), `.detail() -> &str`, `.with_detail(...)`, and `.retry_after_ms() -> Option<u64>` (Some only for `RateLimited` and `ForegroundLeaseBusy`).

| Variant | Error code | Notes |
|---|---|---|
| `QueueFull { detail }` | `ACTION_QUEUE_FULL` | bounded 256-deep normal queue saturated |
| `RateLimited { detail, retry_after_ms }` | `ACTION_RATE_LIMITED` | token bucket exhausted |
| `ForegroundLeaseBusy { detail, holder_session_id, requesting_session_id, retry_after_ms }` | `ACTION_FOREGROUND_LEASE_BUSY` | another session holds the input lease |
| `BackendUnavailable { detail }` | `ACTION_BACKEND_UNAVAILABLE` | backend missing/closed, join panic, clipboard failure, etc. |
| `ForegroundActivationRefused { detail }` | `FOREGROUND_ACTIVATION_REFUSED` | OS refused foreground activation |
| `TargetInvalid { detail }` | `ACTION_TARGET_INVALID` | bad/unresolved target, off-desktop point, planning failure |
| `HoldExceededMax { detail }` | `ACTION_HOLD_EXCEEDED_MAX` | hold duration over limit |
| `VigemNotInstalled { detail }` | `ACTION_VIGEM_NOT_INSTALLED` | `ViGEmBus` device absent |
| `VigemPluginFailed { detail }` | `ACTION_VIGEM_PLUGIN_FAILED` | virtual pad plug-in/mutex failure |
| `ElementNotResolved { detail }` | `ACTION_ELEMENT_NOT_RESOLVED` | UIA re-resolution failed |
| `ElementPatternUnsupported { element_id, detail }` | `ACTION_ELEMENT_PATTERN_UNSUPPORTED` | no supported click pattern |
| `TransientElementExpired { element_id, detail }` | `TRANSIENT_ELEMENT_EXPIRED` | stale a11y element |
| `ForegroundLost { detail }` | `ACTION_FOREGROUND_LOST` | foreground changed mid-action |
| `UnsupportedKey { detail }` | `ACTION_UNSUPPORTED_KEY` | key code not mappable by the backend |
| `DragDistanceExceedsLimit { detail }` | `ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT` | drag > 4096 px |
| `StuckKeyAutoReleased { detail }` | `STUCK_KEY_AUTO_RELEASED` | 30 s held-key timer fired |
| `SafetyReleaseAllFired { detail }` | `SAFETY_RELEASE_ALL_FIRED` | pending action discarded by `ReleaseAll` |
| `SafetyOperatorHotkeyFired { detail }` | `SAFETY_OPERATOR_HOTKEY_FIRED` | operator hotkey interrupted a long action (e.g. a stroke) |

Related path/stroke/humanize/velocity errors are separate enums (`PathError`, `VelocityError`, `StrokeError`, `HumanizeError`, `LeaseError`) surfaced inside planning rather than as `ActionError` codes; backends fold their failures into `TargetInvalid`/`BackendUnavailable`.

---

## 6. Cargo manifest notes (`crates/synapse-action/Cargo.toml`)

- Cross-platform deps: `bit-set`, `serde`/`serde_json`, `sha2`, `synapse-core`, `thiserror`, `tokio`/`tokio-util`, `tracing`.
- Windows-only: `enigo`, `synapse-a11y`, `synapse-capture`, `vigem-client` (feature `unstable_ds4`), `windows`.
- Unix-non-macOS: `arboard`, `enigo` (feature `x11rb`).
- Benches: `action_curve_step_calc_natural`, `action_software_press`, `action_software_click`, `action_vigem_pad_report`, `action_recording_round_trip`.
- Lints: `clippy::all = deny`, `pedantic`/`nursery = warn`, `unwrap_used`/`expect_used = deny`; `unsafe_code = allow` (required for Win32 FFI).
