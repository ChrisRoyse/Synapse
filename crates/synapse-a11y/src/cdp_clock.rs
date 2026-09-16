//! Target-scoped controllable browser clock over raw CDP (#1198).
//!
//! Chrome's virtual-time API is useful, but the Playwright-style controls this
//! issue asks for (`setFixedTime`, `fastForward`, `pauseAt`) need deterministic
//! page-visible Date/timer behavior. This module installs a small init-script
//! shim into one page target and drives it with `Runtime.evaluate`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{A11yError, A11yResult};

const CLOCK_VERSION: &str = "synapse-clock-2026-06-21-v1";
const MAX_CLOCK_MS: u64 = 8_640_000_000_000_000;
const DEFAULT_LOOP_LIMIT: u32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdpClockOperation {
    Status,
    Install,
    SetFixedTime,
    FastForward,
    PauseAt,
}

impl CdpClockOperation {
    const fn wire(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Install => "install",
            Self::SetFixedTime => "setFixedTime",
            Self::FastForward => "fastForward",
            Self::PauseAt => "pauseAt",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CdpClockReadback {
    pub installed: bool,
    pub version: Option<String>,
    pub now_ms: Option<u64>,
    pub pending_timer_count: Option<u64>,
    pub fired_timer_count: Option<u64>,
    pub last_timer_id: Option<u64>,
    pub next_timer_ms: Option<u64>,
    pub error_count: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CdpClockResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub init_script_identifier: Option<String>,
    pub init_script_newly_added: bool,
    pub installed_at_unix_ms: u64,
    pub readback: CdpClockReadback,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CdpClockDurableDrainReadback {
    pub found: usize,
    pub uninstalled: usize,
    pub failures: Vec<String>,
    pub active_after: usize,
}

#[derive(Clone, Debug)]
struct ClockSlot {
    endpoint: String,
    init_script_identifier: String,
    installed_at_unix_ms: u64,
}

fn registry() -> &'static Mutex<HashMap<String, ClockSlot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, ClockSlot>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Runs one browser-clock operation against `target_id`.
///
/// `install` adds the shim for future documents and runs it immediately in the
/// current document. Other mutating operations require the shim to already be
/// present in the current page and fail loudly if it is not.
pub async fn cdp_clock(
    endpoint: &str,
    target_id: &str,
    operation: CdpClockOperation,
    time_unix_ms: Option<u64>,
    delta_ms: Option<u64>,
) -> A11yResult<CdpClockResult> {
    if operation != CdpClockOperation::Status
        && !crate::cdp_network::durable_browser_mutation_owners_enabled()
    {
        return Err(A11yError::CdpAttachFailed {
            detail: "durable browser mutation owners are disabled by operator panic; refusing browser clock mutation".to_owned(),
        });
    }
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "browser clock target id must not be empty".to_owned(),
        });
    }

    match operation {
        CdpClockOperation::Install => install_clock(endpoint, target_id, time_unix_ms).await,
        CdpClockOperation::Status => clock_status(endpoint, target_id).await,
        CdpClockOperation::SetFixedTime => {
            let time = time_unix_ms.ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: "setFixedTime requires time_unix_ms".to_owned(),
            })?;
            run_clock_command(endpoint, target_id, operation, json!({ "timeMs": time })).await
        }
        CdpClockOperation::FastForward => {
            let delta = delta_ms.ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: "fastForward requires delta_ms".to_owned(),
            })?;
            run_clock_command(endpoint, target_id, operation, json!({ "deltaMs": delta })).await
        }
        CdpClockOperation::PauseAt => {
            let time = time_unix_ms.ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: "pauseAt requires time_unix_ms".to_owned(),
            })?;
            run_clock_command(endpoint, target_id, operation, json!({ "timeMs": time })).await
        }
    }
}

async fn install_clock(
    endpoint: &str,
    target_id: &str,
    time_unix_ms: Option<u64>,
) -> A11yResult<CdpClockResult> {
    let now_ms = time_unix_ms.unwrap_or_else(now_unix_ms).min(MAX_CLOCK_MS);
    let mut newly_added = false;
    let slot = {
        let existing = registry()
            .lock()
            .ok()
            .and_then(|slots| slots.get(target_id).cloned())
            .filter(|slot| slot.endpoint == endpoint);
        if let Some(slot) = existing {
            slot
        } else {
            let added = crate::cdp_action::cdp_add_init_script_target(
                endpoint,
                target_id,
                clock_init_script(),
                None,
                None,
                Some(false),
            )
            .await?;
            newly_added = true;
            let slot = ClockSlot {
                endpoint: endpoint.to_owned(),
                init_script_identifier: added.identifier,
                installed_at_unix_ms: now_unix_ms(),
            };
            if let Ok(mut slots) = registry().lock() {
                slots.insert(target_id.to_owned(), slot.clone());
            }
            slot
        }
    };

    let readback = run_clock_js(
        endpoint,
        target_id,
        "install",
        json!({
            "nowMs": now_ms,
            "loopLimit": DEFAULT_LOOP_LIMIT,
        }),
    )
    .await?;
    Ok(CdpClockResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
        operation: CdpClockOperation::Install.wire().to_owned(),
        init_script_identifier: Some(slot.init_script_identifier),
        init_script_newly_added: newly_added,
        installed_at_unix_ms: slot.installed_at_unix_ms,
        readback,
    })
}

async fn clock_status(endpoint: &str, target_id: &str) -> A11yResult<CdpClockResult> {
    let slot = registry()
        .lock()
        .ok()
        .and_then(|slots| slots.get(target_id).cloned())
        .filter(|slot| slot.endpoint == endpoint);
    let readback = run_clock_js(endpoint, target_id, "status", json!({})).await?;
    Ok(CdpClockResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
        operation: CdpClockOperation::Status.wire().to_owned(),
        init_script_identifier: slot
            .as_ref()
            .map(|slot| slot.init_script_identifier.clone()),
        init_script_newly_added: false,
        installed_at_unix_ms: slot.as_ref().map_or(0, |slot| slot.installed_at_unix_ms),
        readback,
    })
}

async fn run_clock_command(
    endpoint: &str,
    target_id: &str,
    operation: CdpClockOperation,
    args: Value,
) -> A11yResult<CdpClockResult> {
    let slot = registry()
        .lock()
        .ok()
        .and_then(|slots| slots.get(target_id).cloned())
        .filter(|slot| slot.endpoint == endpoint);
    let readback = run_clock_js(endpoint, target_id, operation.wire(), args).await?;
    Ok(CdpClockResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
        operation: operation.wire().to_owned(),
        init_script_identifier: slot
            .as_ref()
            .map(|slot| slot.init_script_identifier.clone()),
        init_script_newly_added: false,
        installed_at_unix_ms: slot.as_ref().map_or(0, |slot| slot.installed_at_unix_ms),
        readback,
    })
}

async fn run_clock_js(
    endpoint: &str,
    target_id: &str,
    method: &str,
    args: Value,
) -> A11yResult<CdpClockReadback> {
    let method_json =
        serde_json::to_string(method).map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("serialize browser clock method: {error}"),
        })?;
    let args_json = serde_json::to_string(&args).map_err(|error| A11yError::CdpAxtreeFailed {
        detail: format!("serialize browser clock args: {error}"),
    })?;
    let expression = format!(
        "(() => {{
            const clock = (globalThis.__synapseClock && globalThis.__synapseClock.version === {version:?})
                ? globalThis.__synapseClock
                : ({shim});
            const result = clock.call({method_json}, {args_json});
            return result;
        }})()",
        version = CLOCK_VERSION,
        shim = clock_init_script(),
    );
    let evaluated =
        crate::cdp_action::cdp_evaluate_expression(endpoint, target_id, &expression, false, true)
            .await?;
    serde_json::from_value::<CdpClockReadback>(evaluated.value).map_err(|error| {
        A11yError::CdpAxtreeFailed {
            detail: format!("browser clock readback decode: {error}"),
        }
    })
}

const fn clock_init_script() -> &'static str {
    CLOCK_INIT_SCRIPT
}

pub fn durable_clock_active_count_readback() -> Result<usize, String> {
    registry()
        .lock()
        .map(|slots| slots.len())
        .map_err(|_| "browser clock registry lock is poisoned".to_owned())
}

pub async fn durable_clocks_disable_and_drain_all() -> CdpClockDurableDrainReadback {
    let slots = match registry().lock() {
        Ok(mut slots) => std::mem::take(&mut *slots),
        Err(_) => {
            return CdpClockDurableDrainReadback {
                failures: vec!["browser clock registry lock is poisoned".to_owned()],
                active_after: usize::MAX,
                ..Default::default()
            };
        }
    };
    let found = slots.len();
    let mut uninstalled = 0usize;
    let mut failures = Vec::new();
    let mut failed_slots = Vec::new();
    for (target_id, slot) in slots {
        let current_cleanup =
            run_clock_js(&slot.endpoint, &target_id, "uninstall", json!({})).await;
        let future_cleanup = crate::cdp_action::cdp_remove_init_script_target(
            &slot.endpoint,
            &target_id,
            &slot.init_script_identifier,
        )
        .await;
        match (current_cleanup, future_cleanup) {
            (Ok(readback), Ok(_)) if !readback.installed => {
                uninstalled = uninstalled.saturating_add(1);
            }
            (current, future) => {
                failures.push(format!(
                    "uninstall browser clock for target {target_id:?}: current={:?}; future={:?}",
                    current.err(),
                    future.err()
                ));
                failed_slots.push((target_id, slot));
            }
        }
    }
    if !failed_slots.is_empty() {
        match registry().lock() {
            Ok(mut slots) => slots.extend(failed_slots),
            Err(_) => failures.push(
                "browser clock registry lock poisoned while retaining failed cleanup".to_owned(),
            ),
        }
    }
    let active_after = durable_clock_active_count_readback().unwrap_or(usize::MAX);
    CdpClockDurableDrainReadback {
        found,
        uninstalled,
        failures,
        active_after,
    }
}

const CLOCK_INIT_SCRIPT: &str = r#"
(() => {
  const VERSION = "synapse-clock-2026-06-21-v1";
  if (globalThis.__synapseClock && globalThis.__synapseClock.version === VERSION) {
    return globalThis.__synapseClock;
  }
  const NativeDate = globalThis.Date;
  const nativeGlobalDescriptors = new Map();
  for (const name of ["Date", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame"]) {
    nativeGlobalDescriptors.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
  }
  const nativePerformanceNowDescriptor = globalThis.performance
    ? Object.getOwnPropertyDescriptor(globalThis.performance, "now")
    : undefined;
  const nativePerformanceNow = globalThis.performance && globalThis.performance.now
    ? globalThis.performance.now.bind(globalThis.performance)
    : () => 0;
  const state = {
    installed: false,
    nowMs: NativeDate.now(),
    timers: new Map(),
    nextTimerId: 1,
    firedTimerCount: 0,
    errorCount: 0,
    lastError: null,
    loopLimit: 10000,
    performanceOriginMs: NativeDate.now() - nativePerformanceNow()
  };
  function toFiniteMs(value, fallback) {
    const n = Number(value);
    return Number.isFinite(n) ? Math.max(0, n) : fallback;
  }
  function timerDelay(value) {
    const n = Number(value);
    if (!Number.isFinite(n)) return 0;
    return Math.max(0, n);
  }
  function status() {
    let next = null;
    for (const timer of state.timers.values()) {
      if (next === null || timer.due < next) next = timer.due;
    }
    return {
      installed: state.installed,
      version: VERSION,
      now_ms: Math.floor(state.nowMs),
      pending_timer_count: state.timers.size,
      fired_timer_count: state.firedTimerCount,
      last_timer_id: state.nextTimerId - 1,
      next_timer_ms: next === null ? null : Math.floor(next),
      error_count: state.errorCount,
      last_error: state.lastError
    };
  }
  function recordError(error) {
    state.errorCount += 1;
    state.lastError = error && error.stack ? String(error.stack) : String(error);
  }
  function runHandler(timer) {
    try {
      if (typeof timer.handler === "function") {
        timer.handler.apply(globalThis, timer.args);
      } else {
        (0, eval)(String(timer.handler));
      }
    } catch (error) {
      recordError(error);
    }
  }
  function drainUntil(targetMs) {
    const target = toFiniteMs(targetMs, state.nowMs);
    let guard = 0;
    while (true) {
      let next = null;
      for (const timer of state.timers.values()) {
        if (timer.due <= target && (next === null || timer.due < next.due || (timer.due === next.due && timer.id < next.id))) {
          next = timer;
        }
      }
      if (!next) break;
      if (++guard > state.loopLimit) {
        throw new Error("Synapse browser clock loop limit exceeded while draining timers");
      }
      state.nowMs = next.due;
      state.timers.delete(next.id);
      state.firedTimerCount += 1;
      runHandler(next);
      if (next.kind === "interval" && !next.cancelled) {
        const step = Math.max(1, next.delay);
        next.due = state.nowMs + step;
        state.timers.set(next.id, next);
      }
    }
    state.nowMs = target;
  }
  function schedule(kind, handler, delay, args) {
    const id = state.nextTimerId++;
    const normalizedDelay = timerDelay(delay);
    state.timers.set(id, {
      id,
      kind,
      handler,
      args,
      delay: normalizedDelay,
      due: state.nowMs + normalizedDelay,
      cancelled: false
    });
    return id;
  }
  function clear(id) {
    const timer = state.timers.get(Number(id));
    if (timer) timer.cancelled = true;
    state.timers.delete(Number(id));
  }
  function SynapseDate(...args) {
    if (this instanceof SynapseDate) {
      if (args.length === 0) return new NativeDate(state.nowMs);
      return new NativeDate(...args);
    }
    return new NativeDate(state.nowMs).toString();
  }
  Object.setPrototypeOf(SynapseDate, NativeDate);
  SynapseDate.prototype = NativeDate.prototype;
  SynapseDate.now = () => Math.floor(state.nowMs);
  SynapseDate.parse = NativeDate.parse.bind(NativeDate);
  SynapseDate.UTC = NativeDate.UTC.bind(NativeDate);
  function install(args) {
    if (args && Object.prototype.hasOwnProperty.call(args, "nowMs")) {
      state.nowMs = toFiniteMs(args.nowMs, state.nowMs);
    }
    if (args && Object.prototype.hasOwnProperty.call(args, "loopLimit")) {
      state.loopLimit = Math.max(1, Math.floor(Number(args.loopLimit) || state.loopLimit));
    }
    if (!state.installed) {
      globalThis.Date = SynapseDate;
      globalThis.setTimeout = (handler, delay, ...rest) => schedule("timeout", handler, delay, rest);
      globalThis.clearTimeout = clear;
      globalThis.setInterval = (handler, delay, ...rest) => schedule("interval", handler, delay, rest);
      globalThis.clearInterval = clear;
      globalThis.requestAnimationFrame = (handler) => schedule("raf", (ts) => handler(ts), 16, [Math.max(0, state.nowMs - state.performanceOriginMs)]);
      globalThis.cancelAnimationFrame = clear;
      if (globalThis.performance) {
        try {
          Object.defineProperty(globalThis.performance, "now", {
            configurable: true,
            value: () => Math.max(0, state.nowMs - state.performanceOriginMs)
          });
        } catch (_) {}
      }
      state.installed = true;
    }
    return status();
  }
  function uninstall() {
    state.timers.clear();
    for (const [name, descriptor] of nativeGlobalDescriptors.entries()) {
      try {
        if (descriptor) Object.defineProperty(globalThis, name, descriptor);
        else delete globalThis[name];
      } catch (error) {
        recordError(error);
      }
    }
    if (globalThis.performance) {
      try {
        if (nativePerformanceNowDescriptor) {
          Object.defineProperty(globalThis.performance, "now", nativePerformanceNowDescriptor);
        } else {
          delete globalThis.performance.now;
        }
      } catch (error) {
        recordError(error);
      }
    }
    state.installed = false;
    const readback = status();
    try { delete globalThis.__synapseClock; } catch (_) {}
    return readback;
  }
  function setFixedTime(args) {
    if (!state.installed) throw new Error("Synapse browser clock is not installed");
    state.nowMs = toFiniteMs(args && args.timeMs, state.nowMs);
    return status();
  }
  function fastForward(args) {
    if (!state.installed) throw new Error("Synapse browser clock is not installed");
    const delta = toFiniteMs(args && args.deltaMs, 0);
    drainUntil(state.nowMs + delta);
    return status();
  }
  function pauseAt(args) {
    if (!state.installed) throw new Error("Synapse browser clock is not installed");
    const target = toFiniteMs(args && args.timeMs, state.nowMs);
    if (target < state.nowMs) throw new Error("Synapse browser clock pauseAt cannot move backwards");
    drainUntil(target);
    return status();
  }
  const api = {
    version: VERSION,
    call(method, args) {
      if (method === "install") return install(args || {});
      if (method === "status") return status();
      if (method === "setFixedTime") return setFixedTime(args || {});
      if (method === "fastForward") return fastForward(args || {});
      if (method === "pauseAt") return pauseAt(args || {});
      if (method === "uninstall") return uninstall();
      throw new Error("Unknown Synapse browser clock method: " + method);
    }
  };
  globalThis.__synapseClock = api;
  return api;
})()
"#;

impl<'de> Deserialize<'de> for CdpClockReadback {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            installed: bool,
            #[serde(default)]
            version: Option<String>,
            #[serde(default)]
            now_ms: Option<u64>,
            #[serde(default)]
            pending_timer_count: Option<u64>,
            #[serde(default)]
            fired_timer_count: Option<u64>,
            #[serde(default)]
            last_timer_id: Option<u64>,
            #[serde(default)]
            next_timer_ms: Option<u64>,
            #[serde(default)]
            error_count: Option<u64>,
            #[serde(default)]
            last_error: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            installed: wire.installed,
            version: wire.version,
            now_ms: wire.now_ms,
            pending_timer_count: wire.pending_timer_count,
            fired_timer_count: wire.fired_timer_count,
            last_timer_id: wire.last_timer_id,
            next_timer_ms: wire.next_timer_ms,
            error_count: wire.error_count,
            last_error: wire.last_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_readback_accepts_status_payload() {
        let readback: CdpClockReadback = serde_json::from_value(json!({
            "installed": true,
            "version": CLOCK_VERSION,
            "now_ms": 1234,
            "pending_timer_count": 2,
            "fired_timer_count": 1,
            "last_timer_id": 3,
            "next_timer_ms": 1300,
            "error_count": 0,
            "last_error": null,
        }))
        .expect("clock readback");
        assert!(readback.installed);
        assert_eq!(readback.now_ms, Some(1234));
        assert_eq!(readback.pending_timer_count, Some(2));
        assert_eq!(readback.next_timer_ms, Some(1300));
    }

    #[test]
    fn clock_script_contains_expected_surface() {
        assert!(CLOCK_INIT_SCRIPT.contains("setFixedTime"));
        assert!(CLOCK_INIT_SCRIPT.contains("fastForward"));
        assert!(CLOCK_INIT_SCRIPT.contains("pauseAt"));
        assert!(CLOCK_INIT_SCRIPT.contains("requestAnimationFrame"));
    }
}
