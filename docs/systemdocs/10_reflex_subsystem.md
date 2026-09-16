# 10. Reflex Subsystem

The `synapse-reflex` crate is the event-driven reflex / automation engine (the "M3" subsystem). A *reflex* is a registered rule that, on a periodic high-resolution scheduler tick or on a matching event, dispatches one or more `synapse_core::Action`s through the shared `synapse_action::ActionHandle`. Reflexes never own or mirror held-input state; all input is enqueued through the action handle (the interlock authority). See [09_action_subsystem.md](09_action_subsystem.md) for the action emitter and `ActionHandle`.

**Source files covered:**

- `crates/synapse-reflex/src/lib.rs` — public surface, re-exports, top-level constants, `ReflexCancelOutcome`
- `crates/synapse-reflex/src/runtime.rs` — `ReflexRuntime` handle and accessors
- `crates/synapse-reflex/src/lifecycle.rs` — register / cancel / disable lifecycle
- `crates/synapse-reflex/src/bus.rs` — `EventBus`, subscribers, drop-oldest queue
- `crates/synapse-reflex/src/conflict.rs` — resource conflict resolution, starvation
- `crates/synapse-reflex/src/dispatch.rs` — action gate, dispatch context, denial audit
- `crates/synapse-reflex/src/error.rs` — `ReflexError` variants and codes
- `crates/synapse-reflex/src/audit.rs` — reflex audit row writer
- `crates/synapse-reflex/src/audit_state.rs` — registration / cancellation / disabled audits
- `crates/synapse-reflex/src/listing.rs` — `list` / `history`, audit-derived status reconstruction
- `crates/synapse-reflex/src/action_combo_bridge.rs` — `Action::Combo` → one-shot combo reflex bridge
- `crates/synapse-reflex/src/scheduler.rs` — `ReflexScheduler`, `SchedulerConfig`, `ScheduledReflex`, spawn variants, validation
- `crates/synapse-reflex/src/scheduler_loop.rs` — scheduler thread loop, degraded fallback, status-marking helpers
- `crates/synapse-reflex/src/scheduler_tick.rs` — per-tick logic, trigger collection, debounce, starvation, tick sampling
- `crates/synapse-reflex/src/scheduler_stateful.rs` — stateful controller stepping, stateful conflict pre-resolution
- `crates/synapse-reflex/src/scheduler_combo.rs` — active-combo stepping and `Action::Combo` dispatch
- `crates/synapse-reflex/src/scheduler_handle.rs` — `SchedulerHandle` (statuses, cancel, disable, stop)
- `crates/synapse-reflex/src/scheduler_stats.rs` — `p99_jitter_us`
- `crates/synapse-reflex/src/scheduler_windows.rs` — Windows high-resolution waitable timer
- `crates/synapse-reflex/src/kinds/{aim_track,combo,hold_button,hold_lifetime,hold_move,on_event,path_follow}.rs` — reflex kind controllers
- `crates/synapse-reflex/Cargo.toml`

---

## 1. Overview

### 1.1 What a reflex is

A reflex is a `ScheduledReflex` (`crates/synapse-reflex/src/scheduler.rs`) with these fields:

| Field | Type | Meaning |
|---|---|---|
| `reflex_id` | `ReflexId` (`String`) | Unique identifier |
| `trigger` | `SchedulerTrigger` | `EveryTick` or `OnEvent(EventFilter)` |
| `then` | `Vec<Action>` | Actions dispatched when triggered (used by the `Actions` driver) |
| `driver` | `ScheduledReflexDriver` | `Actions`, `AimTrack`, `HoldMove`, `HoldButton`, `Combo`, `PathFollow` |
| `priority` | `u32` | Lower value = stronger; default `DEFAULT_REFLEX_PRIORITY = 100`, max `MAX_REFLEX_PRIORITY = 1000` |
| `lifetime` | `ReflexLifetime` | `UntilCancelled`, `OneShot`, `Duration{ms}`, `UntilDeadline{ms}`, `UntilEvent{filter}` |
| `exclusive` | `bool` | If true, contends an entire device class against other exclusive reflexes |
| `debounce` | `Duration` | Minimum spacing between `OnEvent` firings |

Constructor helpers set sensible per-kind defaults: `every_tick`, `on_event`, `on_event_with_debounce`, `aim_track`, `hold_move`, `hold_button`, `combo` (lifetime `OneShot`), `path_follow` (lifetime `OneShot`), plus builders `with_priority`, `with_lifetime`, `with_exclusive`.

### 1.2 ReflexRuntime

`ReflexRuntime` (`crates/synapse-reflex/src/runtime.rs`) is the owning handle. It holds the storage `Db`, the `ActionHandle`, the `EventBus`, a `SchedulerConfig`, an optional `StoredAuditContext`, an optional `ReflexActionGateHandle`, an optional `AimTrackTargetSourceHandle`, the `Vec<ScheduledReflex>` registered set, a `HashSet<ReflexId>` of operator-disabled ids, and an optional `SchedulerHandle`.

Created via `spawn(db, action_handle, event_bus)` or `spawn_with_config(...)`. Spawn currently cannot fail after receiving initialized handles. The runtime does **not** start a scheduler on spawn; the scheduler is (re)created on each `register` (see §4).

Read accessors: `statuses()`, `active_count()`, `last_tick_jitter_us()`, `sample_count()`, `sample_limit()`, `p99_tick_jitter_us()`, `late_tick_count()`, `degraded_tick_count()`, `degraded_latency()`, `action_handle()`, `event_bus()`, `audit_context()`. Setters: `set_audit_context`, `set_action_gate`, `set_aim_track_target_source`.

### 1.3 Event bus (`bus.rs`)

`EventBus` is a clone-able handle around an `Arc<EventBusInner>`. Subscribers are kept in an `ArcSwap<Vec<Arc<Subscriber>>>` (lock-free read on publish; a `Mutex<()>` serializes subscription mutations).

Constants:

| Constant | Value | Meaning |
|---|---|---|
| `SUBSCRIBER_QUEUE_CAPACITY` | `4096` | Bounded crossbeam channel size per subscriber |
| `DEFAULT_MAX_SUBSCRIPTIONS` | `64` | Default active-subscription cap |
| `DEFAULT_MAX_SUBSCRIPTIONS_NONZERO` | `NonZeroUsize(64)` | Same, non-zero typed |
| `EVENTS_DROPPED_METRIC` | `"events_dropped_for_subscriber"` | Metrics counter name |

`subscribe(filter, kinds, snapshot_first)` validates the filter (else `EventBusError::FilterInvalid`), enforces the subscription cap (else `EventBusError::SubscriptionCapReached`), and returns a `SubscriberHandle`. Empty `kinds` means all kinds (subject to `filter`). A subscriber matches when `kinds` is empty or contains the event kind **and** the filter matches.

`publish(event)` is non-blocking (ADR-0007: no batching in the bus). For each matching subscriber it calls `enqueue_drop_oldest`: on a full queue it pops the oldest event and retries, counting drops. Drops set the subscriber `lossy` flag, increment `dropped_since_read`, and increment the `EVENTS_DROPPED_METRIC` counter. Returns `PublishReport { matched, queued, dropped }`.

`SubscriberHandle` exposes `id()`, `snapshot_first()`, `len()`, `is_empty()`, `take_lossy()` (swap-reset), `take_dropped_since_read()` (swap-reset), and `drain()` (drains all queued events). Dropping the registration unsubscribes automatically.

### 1.4 Scheduler (high level)

`ReflexScheduler::spawn*` validates config and reflexes, subscribes to `EventFilter::All`, builds per-kind controller state, and spawns an OS thread named `synapse-reflex-scheduler` running `run_scheduler_thread`. Spawn returns a `SchedulerHandle`. Details in §4.

---

## 2. Reflex kinds

All controllers live under `crates/synapse-reflex/src/kinds/`. Each exposes a `step_dispatch*` (and for held kinds `register_dispatch` / `cancel_dispatch`) that takes/uses the `ActionHandle` and `EventBus`. The `_with` variants accept a dispatch closure and are what the scheduler uses (routing through the action gate).

### 2.1 aim_track — `kinds/aim_track.rs`

**Purpose:** continuously steer the mouse cursor toward a target using exponential-moving-average (EMA) smoothing, gain, deadzone, and a per-tick speed cap. Trigger is `EveryTick`; output is `Action::MouseMoveRelative`.

**`AimTrackParams`:**

| Field | Type | Default |
|---|---|---|
| `target` | `AimTrackTarget` | (required) |
| `axis` | `ReflexAimAxis` | `Xy` |
| `gain` | `f32` | `DEFAULT_GAIN = 1.0` |
| `deadzone_px` | `f32` | `DEFAULT_DEADZONE_PX = 2.0` |
| `max_speed_px_per_tick` | `f32` | `DEFAULT_MAX_SPEED_PX_PER_TICK = 5.0` |
| `ema_alpha` | `f32` | `DEFAULT_EMA_ALPHA` (re-export of `synapse_core::DEFAULT_AIM_TRACK_EMA_ALPHA`) |
| `backend` | `Backend` | `Software` |

`AimTrackTarget` variants: `Point(Point)`, `EntityId(EntityId)`, `TrackId(u64)`, `ElementId(ElementId)`, `ElementRect(Rect)`. (`From<AimTarget>` maps `Screen→Point`, `Element→ElementId`, `Track→TrackId`.) Targets resolved against `AimTrackContext { cursor, entities, elements, tick_index, tick_elapsed }`; entity/element targets resolve to bbox center. The entity/element snapshot comes from an `AimTrackTargetSource::snapshot()` (`AimTrackTargetSnapshot`).

**Validation** (`AimTrackParams::validate`): `gain` and `deadzone_px` must be finite & non-negative; `max_speed_px_per_tick` finite & positive; `ema_alpha` finite and within `0.0..=1.0`. Otherwise `ReflexError::ParamsInvalid`.

**Constants:** `TRACK_LOST_AFTER = Duration::from_millis(500)`; `REFLEX_TRACK_LOST_KIND = "reflex_track_lost"`; `REFLEX_AIM_TRACK_CORRECTION_KIND = "aim_track_correction"`.

**Control algorithm** (`next_delta`): compute raw delta `(target - cursor)` (zeroing the off-axis component for `XOnly`/`YOnly`). If `hypot(raw) <= deadzone_px`, output `(0,0)` and store it as the smoothed value (no action). Otherwise apply gain, clamp the magnitude to `max_speed_px_per_tick` (proportional scale by `max_speed / distance` when over), then EMA-smooth against the previous smoothed delta:

```text
smoothed.x = alpha * capped.x + (1 - alpha) * previous.x
smoothed.y = alpha * capped.y + (1 - alpha) * previous.y
```

(implemented with `f64::mul_add`; first tick uses `capped` directly since there is no previous value). The smoothed delta becomes the dispatched `MouseMoveRelative { dx, dy }`.

**State / track-lost:** when the target cannot be resolved, `lost_for` accumulates `tick_elapsed`; once `lost_for > TRACK_LOST_AFTER`, `step_action` returns `ReflexError::TrackLost` and `step_dispatch` publishes a `reflex_track_lost` event before propagating the error. Resolving the target resets `lost_for` to zero.

**Output `AimTrackOutput`:** `Dispatched { action, target, raw_delta, smoothed_delta }` or `Idle { reason }` where reason is `"deadzone"` (target present, inside deadzone) or `"target_absent"`.

### 2.2 combo — `kinds/combo.rs`

**Purpose:** dispatch a precomputed, time-ordered sequence of primitive actions built from `ComboStep`s. Trigger `EveryTick`, lifetime `OneShot` by default.

**`ComboParams`:** `steps: Vec<ComboStep>`, `backend: Backend`.

**Phases (`ComboPhase`):** `Pending → Running → Completed`.

**Schedule build (`build_schedule`):** each `ComboStep` expands into `TimedComboAction { due_ms, sequence, action }`. `KeyPress { key, hold_ms }` expands into a `KeyDown` at `at_ms` and a `KeyUp` at `at_ms + hold_ms`. `KeyDown`/`KeyUp`, `MouseButton`, `MouseMoveRel`, `PadButton`, `PadStick` map to one action each. The list is sorted by `(due_ms, sequence)`.

**Stepping:** `start_dispatch` moves `Pending→Running` and dispatches everything due at elapsed 0. `step_dispatch` (when `Running`) adds `tick_elapsed` to `elapsed` and dispatches every action whose `due_ms <= elapsed_ms` (`dispatch_due_with` loops the cursor). When the cursor reaches the end, phase becomes `Completed` and a `reflex_combo_completed` (`REFLEX_COMBO_COMPLETED_KIND`) event is emitted once.

**Output `ComboOutput`:** `Started { actions, remaining }`, `Dispatched { actions, elapsed_ms, remaining }`, `Completed { scheduled_actions, dispatched_actions, actions }`, `Idle { reason }` (`"already_running"` / `"already_completed"`). `completion_audit_details()` includes per-dispatch `jitter_ms = |elapsed_ms - due_ms|` and `max_jitter_ms`.

### 2.3 hold_button — `kinds/hold_button.rs`

**Purpose:** press a mouse or pad button down, hold it for a lifetime, then release. Trigger `EveryTick`.

**`HoldButtonParams`:** `button: ReflexButtonTarget` (`Mouse { button }` or `Pad { pad, button }`), `backend: Backend` (default `Software`).

**Phases (`HoldButtonPhase`):** `Pending → Holding → Released`.

**Lifecycle:** `register_dispatch` (Pending) dispatches the `Down` action and moves to `Holding` (`HoldButtonOutput::Registered`). `step_dispatch` advances the `HoldLifetimeTracker` (§2.7); while held it returns `Holding { elapsed_ms }`; when the lifetime ends it dispatches `Up`, moves to `Released`, emits `reflex_lifetime_expired`, and returns `ReflexError::LifetimeExpired`. `cancel_dispatch` releases with reason `Cancelled`. `HoldButtonController::new` constructed with no safety cap (`None`).

**Output `HoldButtonOutput`:** `Registered`, `Holding { elapsed_ms }`, `Released { reason: HoldReleaseReason }`, `Idle { reason }` (`"already_holding"`, `"already_released"`, `"not_holding"`).

### 2.4 hold_move — `kinds/hold_move.rs`

**Purpose:** hold one or more keyboard keys down (e.g. movement keys) for a lifetime, optionally re-asserting `KeyDown` periodically. Trigger `EveryTick`.

**`HoldMoveParams`:** `keys: Vec<Key>`, `backend: Backend` (default `Software`), `re_assert: bool` (default `false`).

**Validation:** non-empty key set and unique keys, else `ReflexError::ParamsInvalid`.

**Constants:** `HELD_KEY_REFLEX_SAFETY_GRACE_MS = 1_000`; `HOLD_MOVE_REASSERT_INTERVAL_MS = 50`. The lifetime tracker is created with a safety cap of `HELD_KEY_MAX_DURATION_MS + HELD_KEY_REFLEX_SAFETY_GRACE_MS` (`HELD_KEY_MAX_DURATION_MS` from `synapse-action`).

**Phases (`HoldMovePhase`):** `Pending → Holding → Released`.

**Lifecycle:** `register_dispatch` dispatches one `KeyDown` per key (in order), moves to `Holding`. `step_dispatch`: when the lifetime has not ended and `re_assert` is set and a re-assert is due (`elapsed - last_reassert_at >= 50ms`, or no prior re-assert), re-dispatches all `KeyDown`s and returns `Reasserted`. When the lifetime ends, dispatches `KeyUp` for each key **in reverse order**, moves to `Released`, emits `reflex_lifetime_expired`, and returns `ReflexError::LifetimeExpired`. `cancel_dispatch` releases with reason `Cancelled` (also reverse order; see also cancel-release in §4.4).

**Output `HoldMoveOutput`:** `Registered { actions }`, `Holding { elapsed_ms }`, `Reasserted { elapsed_ms, actions }`, `Released { reason, actions }`, `Idle { reason }`.

### 2.5 hold_lifetime — `kinds/hold_lifetime.rs`

Shared lifetime engine used by `hold_button` and `hold_move`. Not a standalone reflex kind.

`HoldLifetimeContext { tick_elapsed, events, cancelled }`. `HoldReleaseReason`: `Cancelled`, `Duration`, `Deadline`, `Event`, `OneShot`, `SafetyCap` (`as_str` → `"cancelled"`, `"duration"`, `"deadline"`, `"event"`, `"one_shot"`, `"safety_cap"`).

`HoldLifetimeTracker::step` precedence: (1) `cancelled` → `Cancelled`; (2) `OneShot` lifetime → `OneShot` immediately; otherwise accumulate `tick_elapsed`, then (3) `safety_cap` reached → `SafetyCap`; (4) `Duration{ms}` elapsed → `Duration`; (5) `UntilDeadline{ms}` elapsed → `Deadline`; (6) `UntilEvent{filter}` matched by any event → `Event`; else `None` (keep holding). `validate_lifetime` validates the `UntilEvent` filter (else `ReflexError::FilterInvalid`). `emit_lifetime_expired` publishes a `reflex_lifetime_expired` (`REFLEX_LIFETIME_EXPIRED_KIND`) event.

### 2.6 on_event — `kinds/on_event.rs`

Not a separate driver — it backs the `Actions` driver with the `OnEvent` trigger. Provides debounce state, per-tick firing limit, and the fired / debounced / recursion-limit events & audits.

**Constants:** `MAX_ON_EVENT_FIRINGS_PER_TICK = 4`; `REFLEX_DEBOUNCED_KIND = "reflex_debounced"`; `REFLEX_FIRED_KIND = "reflex_fired"`; `REFLEX_RECURSION_LIMIT_KIND = "reflex_recursion_limit"`.

`OnEventState` tracks `last_fire: Option<Instant>`; `allows_fire(now, debounce)` is true when no prior fire or `now - last_fire >= debounce`. `OnEventTickGuard` caps total `OnEvent` firings per tick at `MAX_ON_EVENT_FIRINGS_PER_TICK`; when exceeded it publishes `reflex_recursion_limit` and writes an audit once per tick (`report_limit_once`).

`publish_fired` emits a `reflex_fired` event (correlated to the trigger event via an `EventRef { relation: "trigger" }`), writes a fired audit (status `Active`, one completed `StoredReflexStep` per action), and logs. `publish_debounced` emits a `reflex_debounced` event + audit with `debounce_ms`, `suppressed_count`, and a `reason` (`"same_tick"` or `"debounce_window"`).

### 2.7 path_follow — `kinds/path_follow.rs`

**Purpose:** drive the cursor along a planned stroke path (optionally with a held mouse button), dispatching `MouseMove` samples on a precomputed millisecond schedule. Trigger `EveryTick`, lifetime `OneShot` by default.

**`PathFollowParams`:** `path: PathSpec`, `button: Option<MouseButton>`, `profile: VelocityProfile`, `timing: StrokeTiming`, `humanize: Option<HumanizeParams>`, `backend: Backend`.

**Constants:** `REFLEX_PATH_FOLLOW_TICK_KIND = "reflex_path_follow_tick"`; `REFLEX_PATH_FOLLOW_COMPLETED_KIND = "reflex_path_follow_completed"`; `MAX_PATH_FOLLOW_SAMPLES = 60_001`; `MAX_PATH_FOLLOW_DURATION_MS = 60_000.0`; `MAX_PATH_FOLLOW_PATH_POINTS = 4096`.

**Planning / validation (`new`):** validate path control-point count `<= MAX_PATH_FOLLOW_PATH_POINTS` (Line=2, Arc/Circle=1, CubicBezier=4, Polyline/CatmullRom=len). Then `plan_timed_stroke(..., StrokeMotionModel::Path, ...)` (from `synapse-action`) produces a `StrokePlan`. `validate_plan` rejects empty sample streams, sample count `> MAX_PATH_FOLLOW_SAMPLES`, and non-finite / `<= 0` / `> MAX_PATH_FOLLOW_DURATION_MS` plan durations. `build_schedule` turns each sample into a `MouseMove { to: Screen{point}, curve: Instant, duration_ms: 0 }` at `due_ms = ceil(sample.elapsed_ms)`; if `button` is set, a `MouseButton Down` is scheduled at the first sample's time and a `MouseButton Up` at the last sample's time. Sorted by `(due_ms, sequence)`.

**Phases (`PathFollowPhase`):** `Pending → Running → Completed`. Stepping mirrors combo (`dispatch_due_with` advances by `tick_elapsed`); completion emits `reflex_path_follow_completed` once. Per-tick dispatched samples are recorded as `reflex_path_follow_tick` audits (see §5).

**Output `PathFollowOutput`:** `Started`, `Dispatched`, `Completed`, `Idle { reason }`; non-idle variants carry `records: Vec<PathFollowDispatchRecord>` (`due_ms`, `sequence`, `sample_index`, `elapsed_ms`, `action`).

---

## 3. Dispatch and conflict resolution

### 3.1 Dispatch and the action gate (`dispatch.rs`)

`ReflexActionDispatchContext` wraps the `ActionHandle`, an optional `ReflexActionGateHandle`, an optional audit `Db`, an optional `StoredAuditContext`, and the current `tick_index`. `dispatch_action(reflex_id, action)` first calls `ensure_action_allowed`, then `ActionHandle::try_execute` (errors mapped to `ReflexError::ParamsInvalid`).

`ReflexActionGate` is the trait `fn ensure_action_allowed(reflex_id, action) -> Result<(), ReflexActionPermissionDenied>`. On denial, the context writes an `action_denied` audit row (status `ReflexState::ActionDenied`, step status `REFLEX_ACTION_DENIED_STEP_STATUS = "action_denied"`, error code `REFLEX_ACTION_PERMISSION_DENIED`, details kind `REFLEX_ACTION_PERMISSION_DENIED_KIND = "reflex_action_permission_denied"`, plus `policy_code`/`policy_reason`/`profile_id`/`use_scope`/`detail`) and returns `ReflexError::ActionPermissionDenied`. `ReflexActionPermissionDenied` carries `policy_code`, `policy_reason`, `profile_id`, `use_scope`, `detail` (all `Option<String>` except `detail`).

When dispatch is denied at tick time, the scheduler marks the reflex `ActionDenied` and stops re-running it; any other dispatch error sets `dispatch_blocked` and records the error code on the status without changing the active flag.

### 3.2 Conflict resolution (`conflict.rs`)

Each candidate reflex's actions are decomposed into `ConflictResource`s: `KeyboardText`, `Key(Key)`, `MouseCursor`, `MouseButton(MouseButton)`, `PadButton{pad,button}`, `PadStick{pad,stick}`, `PadTrigger{pad,trigger}`, `PadReport{pad}`. Device classes: `Keyboard`, `Mouse`, `Pad{pad}`.

`conflicts_with` rules: `KeyboardText` conflicts with `KeyboardText` and any `Key`; two `MouseCursor`s always conflict; `PadReport{pad}` conflicts with any same-pad resource (and vice versa); same-variant resources conflict when equal (same key / same button / same pad+button / pad+stick / pad+trigger).

`ConflictCandidate` carries `candidate_index`, `reflex_slot`, `reflex_id`, `priority`, `registration_order`, `exclusive`, `resources`. **Ranking:** `outranks(left, right)` is true when `left.priority < right.priority`, or equal priority and `left.registration_order > right.registration_order` (later registration wins ties). `compare_precedence` sorts by ascending priority, then descending registration order.

`resolve_conflicts`: a candidate with no resources is an automatic winner. Otherwise it loses to the strongest-outranking other candidate that contends a resource (or, when both are `exclusive`, contends a device class → label `exclusive:<class>`). Losers are recorded as `ConflictLoser { candidate_index, loser_slot, loser_reflex_id, winner_slot, winner_reflex_id, resource }`; winners are returned sorted by precedence.

`StatefulConflictSelection` (in `scheduler_stateful.rs`) runs the same resolution over the projected actions of stateful controllers *before* stepping them, so a losing stateful reflex is skipped this tick (`blocked_slots`).

### 3.3 Starvation (`conflict.rs` + `scheduler_tick.rs`)

| Constant | Value |
|---|---|
| `REFLEX_STARVED_KIND` | `"reflex_starved"` |
| `STARVATION_AFTER` | `Duration::from_secs(2)` |

`StarvationState::record_loss(elapsed)` accumulates `contended_for`; the first time it reaches `STARVATION_AFTER` (and has not already reported) it returns `true`. `record_starvation` (`scheduler_tick.rs`) iterates the losers from both the stateful and triggered conflict passes: each losing slot accumulates contention, and on first crossing of the 2-second threshold a `reflex_starved` event + audit (status `Starved`, code `REFLEX_STARVED`) is published and the status is marked `Starved`. Slots that did not lose this tick reset their `StarvationState` and, if still active, are marked back from `Starved` to `Active`.

---

## 4. Scheduler timing model and lifecycle

### 4.1 SchedulerConfig (`scheduler.rs`)

| Field | Default | Meaning |
|---|---|---|
| `target_interval` | `1 ms` | Target tick period |
| `fallback_interval` | `2 ms` | Tokio interval used in degraded mode |
| `late_after` | `2 ms` (`target_interval * 2`) | A tick is "late" if its elapsed time exceeds this |
| `sample_limit` | `DEFAULT_SAMPLE_LIMIT = 4096` | Tick-sample ring buffer size |
| `max_ticks` | `None` | Optional tick cap (testing); `with_max_ticks(n)` |
| `force_degraded` | `false` | Forces the tokio fallback loop |

`validate()` rejects zero `target_interval`, zero `fallback_interval`, and zero `sample_limit` (`ReflexError::ParamsInvalid`).

Other limits: `MAX_SCHEDULED_REFLEXES = 32`, `MAX_REFLEX_PRIORITY = 1000`, `DEFAULT_REFLEX_PRIORITY = 100`, `REFLEX_TICK_LATE_KIND = "reflex_tick_late"`.

### 4.2 Thread loop and degraded fallback (`scheduler_loop.rs`)

On Windows, `run_scheduler_thread` starts a `WindowsHighResolutionTimer` (`scheduler_windows.rs`): sets the thread to `THREAD_PRIORITY_TIME_CRITICAL`, registers MMCSS "Pro Audio" at `AVRT_PRIORITY_CRITICAL`, and creates a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` waitable timer. Each iteration computes `deadline = last + target_interval` and `wait_until` parks on the timer until `SPIN_WINDOW = 1 ms` before the deadline, then busy-spins (`spin_loop`) the final window for sub-millisecond wake accuracy. `tick(elapsed, degraded=false)` runs with the measured elapsed time.

If the timer fails to start or `wait_until` errors, or `force_degraded` is set, or the platform is non-Windows, the loop falls back to `run_degraded`: a current-thread tokio runtime drives `tokio::time::interval(fallback_interval)` and calls `tick(elapsed, degraded=true)`. A `degraded=true` warning is logged.

`should_tick` stops when the `stop` AtomicBool is set or `tick_index >= max_ticks`.

### 4.3 Per-tick sequence (`scheduler_tick.rs`)

Each `tick`:
1. Drain queued events from the bus subscription.
2. `expire_action_until_event_lifetimes` — fire `UntilEvent` lifetimes for `Actions`-driver reflexes whose filter matches a drained event (emit `reflex_lifetime_expired`, mark expired).
3. `step_active_combos` — advance in-flight `Action::Combo` controllers (`scheduler_combo.rs`).
4. If not blocked, `step_stateful_controllers` — pre-resolve stateful conflicts, then step `aim_track`, `hold_move`, `hold_button`, `combo`, `path_follow` controllers for each active, non-blocked slot.
5. If not blocked, `dispatch_triggered_reflexes` — collect `Actions`-driver reflexes triggered by `EveryTick` or matching `OnEvent` (with debounce suppression), resolve conflicts, and dispatch winners (respecting `MAX_ON_EVENT_FIRINGS_PER_TICK`).
6. `record_starvation` for accumulated losers.
7. `record_tick_sample`.

**Debounce** (`collect_triggered_reflexes`): within one tick an `OnEvent` reflex accepts at most one matching event when `debounce > 0` (subsequent same-tick matches recorded as `"same_tick"` suppression); matches inside the debounce window (per `OnEventState`) are recorded as `"debounce_window"` suppression. Suppressions emit `reflex_debounced`.

**Dispatch blocking:** the first dispatch error in a tick sets `dispatch_blocked`, which short-circuits the remaining phases and marks the tick late.

### 4.4 Tick sampling and lateness (`scheduler_tick.rs`)

`TickSample { tick_index, elapsed_us, jitter_us, target_us, pulled_events, dispatched_actions, late, degraded }`. `jitter_us = |elapsed_us - target_us|`. A tick is `late` when `elapsed > late_after` (reason `"deadline_miss"`) **or** `dispatch_blocked` (reason `"dispatch_blocked"`). A `reflex_tick_late` event + audit is emitted only on an edge (when the late signal changes), de-duplicated via `last_tick_late_signal`. Samples are stored in a `sample_limit`-bounded `VecDeque` (oldest dropped). `p99_jitter_us` (`scheduler_stats.rs`) sorts the jitter values and returns the value at index `ceil(len*99/100) - 1` (0 for empty).

### 4.5 Registration / cancellation lifecycle (`lifecycle.rs`)

**`register(reflex)`:** rejects `priority > MAX_REFLEX_PRIORITY` (`PriorityInvalid`); drops terminal reflexes from the working set; rejects a duplicate active registration matching trigger+then+driver+priority+lifetime+exclusive+debounce (`ParamsInvalid`); appends and runs `validate_reflexes`. It then **spawns a fresh scheduler** (selecting the spawn variant by whether an action gate and/or aim-track source are present), re-applies any operator-disabled ids, replaces the old scheduler (stopping it), records the new reflex set, and writes a registration audit. Returns the new `ReflexStatus`.

`validate_reflexes`: cap `<= MAX_SCHEDULED_REFLEXES` (`CapReached`), unique ids (`ParamsInvalid`), `priority <= MAX_REFLEX_PRIORITY` (`PriorityInvalid`), valid trigger filter, valid lifetime.

**`cancel(reflex_id)`** returns a `ReflexCancelOutcome` (`Cancelled { status }`, `NotFound`, `AlreadyExpired { status }`). If the reflex is unknown live, it is reconstructed from audit (`terminal_status_from_audit`). For live reflexes it cancels via the scheduler, dispatches cancel-release actions (`cancel_release_actions`: `HoldMove`→reverse `KeyUp`s, `HoldButton`→`Up`, `PathFollow`→button `Up` if any; `Actions`/`AimTrack`/`Combo`→none), and writes a cancellation audit.

**Operator disable / release-all:** `disable_all_by_operator()` (reason `"operator_hotkey"`) and `disable_all_for_release_all()` (reason `"release_all"`) call `disable_all_with_reason`, which disables all eligible scheduler reflexes, **stops the scheduler** (so no in-flight tick reasserts held input after the emitter drains), records the disabled ids, and writes disabled audits. `SchedulerHandle::disable_reflexes` only disables reflexes currently in `Active`/`Paused`/`Starved`.

**Combo bridge (`action_combo_bridge.rs`):** `install_action_combo_scheduler` installs an `ActionComboScheduler` so that `ActionHandle::execute(Action::Combo)` registers a one-shot combo reflex on the owning `ReflexRuntime` (new `reflex_id`, `ComboParams::new(steps, backend)`).

### 4.6 SchedulerHandle (`scheduler_handle.rs`)

`samples()`, `wait_for_samples(count, timeout)`, `statuses()`, `set_priority(id, priority)`, `cancel_reflex(id)` (marks control inactive + status `Cancelled`), `disable_reflexes(ids)`, `disable_all_reflexes()`, and `stop()` (sets stop flag and joins the thread; a panicked thread → `ParamsInvalid`). `Drop` stops the thread.

`ReflexState` transitions used by status-marking helpers (`scheduler_loop.rs`): `Active`, `Expired` (one-shot fire, lifetime expired, combo/path-follow completed, track-lost), `Starved`, `ActionDenied`, `Cancelled`, `Disabled`, `Paused`.

---

## 5. Audit (`audit.rs`, `audit_state.rs`, and emit sites)

`write_audit(db, audit)` (`audit.rs`) writes one `StoredReflexAudit` to column family `cf::CF_REFLEX_AUDIT` with key `"{reflex_id}:{ts_ns:020}:{audit_id}"` (zero-padded 20-digit ts for lexicographic ordering; `audit_id` is a UUIDv7). Duplicate timestamps coexist because the audit id disambiguates the key.

Every audit row is a `StoredReflexAudit { schema_version (SCHEMA_VERSION), audit_id, reflex_id, ts_ns, status, event_id, audit_context, steps, error_code, details, redacted, redactions }`.

Audit rows written across the subsystem:

| Trigger | Source | status | error_code | details `kind` |
|---|---|---|---|---|
| Registration | `audit_state.rs` | `Active` | none | `reflex_registered` |
| Cancellation | `audit_state.rs` | `Cancelled` | none | `reflex_cancelled` |
| Operator/release-all disable | `audit_state.rs` | `Disabled` | `REFLEX_DISABLED_BY_OPERATOR` | `reflex_disabled_by_operator` (+ `reason`) |
| Reflex fired (on_event) | `kinds/on_event.rs` | `Active` | none | `reflex_fired` (per-action completed steps) |
| Debounced | `kinds/on_event.rs` | `Active` | `REFLEX_DEBOUNCED` | `reflex_debounced` |
| Recursion limit | `kinds/on_event.rs` | `Active` | `REFLEX_RECURSION_LIMIT` | `reflex_recursion_limit` |
| Action denied | `dispatch.rs` | `ActionDenied` | `REFLEX_ACTION_PERMISSION_DENIED` | `reflex_action_permission_denied` |
| Lifetime expired (incl. one-shot, combo/path-follow completion) | `scheduler_loop.rs` | `Expired` | `REFLEX_LIFETIME_EXPIRED` | `reflex_lifetime_expired` (+ optional `combo_completion`/`path_follow_completion`) |
| Track lost | `scheduler_loop.rs` | `Expired` | `REFLEX_TRACK_LOST` | `reflex_track_lost` |
| Starvation | `scheduler_tick.rs` | `Starved` | `REFLEX_STARVED` | `reflex_starved` |
| Tick late | `scheduler_tick.rs` | `Active` (reflex_id `"__scheduler__"`) | `REFLEX_TICK_LATE` | `reflex_tick_late` |
| Aim-track correction | `scheduler_stateful.rs` | `Active` | none | `aim_track_correction` (cursor/target/raw_delta/smoothed_delta/params/target_context) |
| Path-follow tick | `scheduler_stateful.rs` | `Active` | none | `reflex_path_follow_tick` (per-sample dispatch steps) |

`StoredAuditContext` (when set via `set_audit_context`) is attached to every audit row written by the runtime/scheduler. Audit-write failures are logged at `warn` (not fatal) except registration/cancellation/disabled writes, which propagate as `ReflexError::ParamsInvalid` and are flushed.

### 5.1 Listing & history (`listing.rs`)

`list(include_expired)` returns live statuses (hiding `ActionDenied`/`Cancelled`/`Expired` unless `include_expired`); when `include_expired`, terminal statuses are reconstructed from `CF_REFLEX_AUDIT` (`terminal_statuses_from_audit`) and merged in (so terminal reflexes survive daemon restarts). `history(reflex_id, limit)` flushes then scans the audit CF (by `"{reflex_id}:"` prefix when given) and returns rows newest-first (sorted by `ts_ns`, then `audit_id`, then `reflex_id`), truncated to `limit` (limit 0 → empty).

`AuditStatusAccumulator` rebuilds a `ReflexStatus` from audit rows: it reads `registered_at`/`kind_summary`/`priority`/`lifetime`/`exclusive` from the `reflex_registered` (and terminal) rows, counts `reflex_fired` rows into `fire_count` / `last_fired_at`, and emits a terminal status from the last `ActionDenied`/`Cancelled`/`Expired` row (defaults: kind `"unknown"`, priority `DEFAULT_REFLEX_PRIORITY`, lifetime default, exclusive `false`).

---

## 6. Error types (`error.rs`)

`ReflexResult<T> = Result<T, ReflexError>`. Each variant maps to a stable `synapse_core::error_codes` constant via `ReflexError::code()`.

| Variant | Fields | Error code |
|---|---|---|
| `CapReached` | `detail` | `REFLEX_CAP_REACHED` |
| `KindInvalid` | `detail` | `REFLEX_KIND_INVALID` |
| `ParamsInvalid` | `detail` | `REFLEX_PARAMS_INVALID` |
| `TargetInvalid` | `detail` | `REFLEX_TARGET_INVALID` |
| `FilterInvalid` | `detail` | `REFLEX_FILTER_INVALID` |
| `PriorityInvalid` | `detail` | `REFLEX_PRIORITY_INVALID` |
| `TickLate` | `late_by_us: u64` | `REFLEX_TICK_LATE` |
| `TrackLost` | `reflex_id` | `REFLEX_TRACK_LOST` |
| `Starved` | `reflex_id` | `REFLEX_STARVED` |
| `ActionPermissionDenied` | `reflex_id`, `detail` | `REFLEX_ACTION_PERMISSION_DENIED` |
| `DisabledByOperator` | `detail` | `REFLEX_DISABLED_BY_OPERATOR` |
| `LifetimeExpired` | `reflex_id` | `REFLEX_LIFETIME_EXPIRED` |
| `RecursionLimit` | `reflex_id` | `REFLEX_RECURSION_LIMIT` |

`EventBusError` (`bus.rs`) is separate: `SubscriptionCapReached { limit }` → `SUBSCRIPTION_CAP_REACHED`, `FilterInvalid { detail }` → `REFLEX_FILTER_INVALID`.

### 6.1 Top-level constants (`lib.rs`)

`REFLEX_CANCELLED_KIND = "reflex_cancelled"`, `REFLEX_DISABLED_KIND = "reflex_disabled_by_operator"`, `REFLEX_REGISTERED_KIND = "reflex_registered"`. `ReflexCancelOutcome`: `Cancelled { status }`, `NotFound`, `AlreadyExpired { status }`.

---

## 7. Dependencies (`Cargo.toml`)

`synapse-action`, `synapse-core`, `synapse-storage` (workspace crates); `arc-swap`, `chrono`, `crossbeam`, `metrics`, `serde`, `serde_json`, `thiserror`, `tokio`, `tracing`, `uuid`; on Windows, `windows`. Clippy lints are strict (`unwrap_used`/`expect_used` denied). Benchmarks: `reflex_tick_jitter_idle`, `reflex_tick_jitter_under_load`, `event_to_subscriber`, `reflex_combo_step_interval`.
