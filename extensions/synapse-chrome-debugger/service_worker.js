const PROTOCOL_VERSION = 1;
const BRIDGE_BUILD_ID = "synapse-chrome-bridge-2026-06-30-window-id-v2";
const BRIDGE_BUILD_SHA256 = "4b2998af1e4c6a12d3d5137bd7fe3955b5dd9afa0d1d4ad3d95228a39ad901d3";
const DEBUGGER_COMMAND_TIMEOUT_MS = 5000;
const CAPTURE_VISIBLE_TAB_MIN_INTERVAL_MS = 600;
const PAGE_SCREENSHOT_COMMAND_RESPONSE_BUDGET_MS = 25000;
let captureVisibleTabQueue = Promise.resolve();
let lastCaptureVisibleTabAtMs = 0;
const COMMAND_CAPABILITIES = Object.freeze([
  "alarmReconnect",
  "externalPopupRiskSuppression",
  "listTabs",
  "openTab",
  "closeTab",
  "targetInfo",
  "targetInfoPageText",
  "frames",
  "frameLocators",
  "cookies",
  "downloads",
  "storageState",
  "navigateTab",
  "activateTab",
  "pageVitals",
  "pageContent",
  "pageScreenshot",
  "pagePdf",
  "setContent",
  "ariaSnapshot",
  "assertPoll",
  "locateElements",
  "inspectElement",
  "scrollIntoView",
  "waitForText",
  "waitForFunction",
  "waitForLoadState",
  "waitForUrl",
  "waitForRequest",
  "waitForResponse",
  "waitForSelector",
  "clock",
  "pageEvents",
  "evaluateScript",
  "initScript",
  "exposeBinding",
  "handleDialog",
  "fileUpload",
  "cdpInput",
  "viewportEmulation",
  "deviceEmulation",
  "geolocationEmulation",
  "localeEmulation",
  "mediaEmulation",
  "networkConditions",
  "domAction",
  "coordinateClick",
  "reloadSelf",
  "typeActiveElement",
  "setFieldValue"
]);
const EXPECTED_EXTENSION_ID = "leoocgnkjnplbfdbklajepahofecgfbk";
const DAEMON_BASE_URL = "http://127.0.0.1:7700";
const SET_CONTENT_SEED_PATH = "/__synapse_bridge_set_content_seed.html";
const ERROR_ATTACH_FAILED = "A11Y_CDP_ATTACH_FAILED";
const ERROR_AXTREE_FAILED = "A11Y_CDP_AXTREE_FAILED";
const ERROR_DEBUGGER_WARNING_UNSUPPRESSED = "A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED";
const ERROR_EXTENSION_TIMEOUT = "A11Y_CDP_EXTENSION_TIMEOUT";
const ERROR_EXTENSION_STALE = "CHROME_BRIDGE_EXTENSION_STALE";
const ERROR_EXTENSION_ID_MISMATCH = "SYNAPSE_CHROME_EXTENSION_ID_MISMATCH";
const ERROR_DAEMON_UNAVAILABLE = "SYNAPSE_CHROME_DAEMON_UNAVAILABLE";
const ERROR_CHROME_SCRIPTING_EXECUTE_FAILED = "CHROME_SCRIPTING_EXECUTE_FAILED";
const ERROR_CHROME_DOM_SELECTOR_INVALID = "CHROME_DOM_SELECTOR_INVALID";
const ERROR_CHROME_DOM_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
const ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS = "CHROME_DOM_ELEMENT_AMBIGUOUS";
const ERROR_CHROME_DOM_ELEMENT_NOT_ACTIONABLE = "CHROME_DOM_ELEMENT_NOT_ACTIONABLE";
const ERROR_CHROME_DOM_ACTION_UNSUPPORTED = "CHROME_DOM_ACTION_UNSUPPORTED";
const ERROR_CHROME_DOM_ACTION_POSTCONDITION_FAILED = "CHROME_DOM_ACTION_POSTCONDITION_FAILED";
const ERROR_ACTION_TARGET_INVALID = "ACTION_TARGET_INVALID";
const TAB_TARGET_PREFIX = "chrome-tab:";
const BRIDGE_TOKEN_HEADER = "X-Synapse-Bridge-Token";
const DAEMON_WS_BASE_URL = "ws://127.0.0.1:7700";
const WINDOW_BOUNDS_SCALE_CANDIDATES = Object.freeze([1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3]);
const WEBSOCKET_KEEPALIVE_MS = 20000;
const RECONNECT_INITIAL_MS = 1000;
const RECONNECT_MAX_MS = 30000;
const DISCONNECTED_KEEPALIVE_MS = 20000;
const RECONNECT_WAKE_ALARM_NAME = "synapse-daemon-bridge-reconnect";
const RECONNECT_WAKE_ALARM_DELAY_MINUTES = 0.5;
const RECONNECT_WAKE_ALARM_PERIOD_MINUTES = 0.5;
const AGENT_NAVIGATION_CLAIM_TTL_MS = 30000;
const MAX_RECENT_NAVIGATION_KEYS = 128;
const MAX_PAGE_TEXT_CHARS = 4096;
const MAX_PAGE_EVENT_BUFFER = 1000;
const MAX_NETWORK_EVENT_BUFFER = 2000;
const MAX_DOWNLOAD_EVENT_BUFFER = 1000;
const MAX_BINDING_EVENT_BUFFER = 1000;
const MAX_BINDING_PAYLOAD_CHARS = 65536;
const MAX_DIALOG_EVENT_BUFFER = 1000;
const MAX_DIALOG_PROMPT_TEXT_CHARS = 8192;
const MAX_FILE_CHOOSER_EVENT_BUFFER = 1000;
const MAX_FILE_UPLOAD_PATHS = 256;
const MAX_FILE_UPLOAD_PATH_CHARS = 32768;
const OPEN_WINDOW_BOUNDS_TOLERANCE_PX = 96;
const EXTERNAL_POPUP_RISK_PERMISSIONS = Object.freeze(["debugger", "nativeMessaging"]);
const POPUP_RISK_SUPPRESSION_RECHECK_MS = 60000;
const VIEWPORT_BASELINE_BY_TAB = new Map();
const DEVICE_BASELINE_BY_TAB = new Map();
const LOCALE_BASELINE_BY_TAB = new Map();
const MEDIA_BASELINE_BY_TAB = new Map();
const NETWORK_BASELINE_BY_TAB = new Map();
const INIT_SCRIPT_DEBUGGER_SESSIONS = new Map();
const BINDING_DEBUGGER_SESSIONS = new Map();
const DIALOG_DEBUGGER_SESSIONS = new Map();
const FILE_CHOOSER_DEBUGGER_SESSIONS = new Map();

let hostId = null;
let bridgeToken = null;
let connectInFlight = null;
let webSocket = null;
let keepAliveTimer = null;
let reconnectTimer = null;
let reconnectAttempt = 0;
let disconnectedKeepAliveTimer = null;
let permanentlyDisabled = false;
const agentNavigationClaims = new Map();
const recentNavigationKeys = [];
const pageEventBuffers = new Map();
const networkEventBuffers = new Map();
const downloadEventBuffer = [];
let downloadEventSeq = 0;
let popupRiskSuppressionInFlight = null;
let popupRiskSuppressionState = {
  ok: false,
  status: "not_checked",
  reason: "not_checked",
  checked_at_unix_ms: 0,
  hazard_count: 0,
  remaining_hazard_count: 0,
  disabled_count: 0,
  failure_count: 0,
  management_available: Boolean(chrome.management?.getAll),
  hazards: [],
  disabled: [],
  remaining_hazards: [],
  failures: []
};

async function startBridge() {
  if (chrome.runtime.id !== EXPECTED_EXTENSION_ID) {
    disableBridgePermanently(
      `extension id mismatch: actual=${chrome.runtime.id} expected=${EXPECTED_EXTENSION_ID}; ` +
        `reload the unpacked extension from the Synapse extension directory so the daemon can ` +
        `authenticate the bridge origin`,
      ERROR_EXTENSION_ID_MISMATCH
    );
    return;
  }
  ensureReconnectWakeAlarm();
  await ensureExternalPopupRiskSuppression("startup");
  connectDaemon();
}

function startBridgeFromEvent() {
  startBridge().catch((error) => {
    console.error(`Synapse daemon bridge startup failed: ${errorMessage(error)}`);
    connectDaemon();
  });
}

function connectDaemon() {
  if (permanentlyDisabled) {
    return;
  }
  if (connectInFlight) {
    return;
  }
  if (webSocket && isWebSocketUsable(webSocket)) {
    return;
  }
  if (webSocket && !isWebSocketUsable(webSocket)) {
    closeWebSocket();
  }
  if (hostId && bridgeToken) {
    connectWebSocket();
    return;
  }
  clearReconnectTimer();
  connectInFlight = ensureExternalPopupRiskSuppression("connectDaemon")
    .then(() => registerDaemon())
    .catch((error) => {
      if (error?.code === ERROR_DEBUGGER_WARNING_UNSUPPRESSED) {
        scheduleReconnect(
          `direct daemon register refused unsafe profile: ${errorMessage(error)}`,
          ERROR_DEBUGGER_WARNING_UNSUPPRESSED
        );
        return;
      }
      scheduleReconnect(
        `direct daemon register failed: ${errorMessage(error)}`,
        ERROR_DAEMON_UNAVAILABLE
      );
    })
    .finally(() => {
      connectInFlight = null;
    });
}

async function registerDaemon() {
  const registered = await daemonFetchJson("/chrome-debugger/native/register", {
    method: "POST",
    body: {
      origin: chrome.runtime.getURL(""),
      pid: 0,
      parent_window: null,
      bridge_protocol_version: PROTOCOL_VERSION,
      transport: "direct_http"
    }
  });
  if (
    !registered?.ok ||
    typeof registered.host_id !== "string" ||
    !registered.host_id ||
    typeof registered.bridge_token !== "string" ||
    !registered.bridge_token
  ) {
    throw new Error(`daemon register returned invalid response ${JSON.stringify(registered)}`);
  }
  hostId = registered.host_id;
  bridgeToken = registered.bridge_token;
  await postDaemonMessage({
    type: "hello",
    transport: "direct_http",
    userAgent: navigator.userAgent,
    ...bridgeIdentity()
  });
  connectWebSocket();
}

function disableBridgePermanently(detail, code) {
  closeWebSocket();
  clearReconnectTimer();
  stopDisconnectedKeepAlive();
  permanentlyDisabled = true;
  hostId = null;
  bridgeToken = null;
  console.error(
    `Synapse daemon bridge disabled permanently for this extension load: ${detail}; ` +
      `code=${code}`
  );
}

function scheduleReconnect(detail, code) {
  if (permanentlyDisabled) {
    return;
  }
  closeWebSocket();
  hostId = null;
  bridgeToken = null;
  const delayMs = Math.min(
    RECONNECT_MAX_MS,
    RECONNECT_INITIAL_MS * 2 ** Math.min(reconnectAttempt, 5)
  );
  reconnectAttempt += 1;
  console.warn(
    `Synapse daemon bridge disconnected; reconnect scheduled: code=${code} ` +
      `attempt=${reconnectAttempt} delay_ms=${delayMs} detail=${detail}`
  );
  startDisconnectedKeepAlive();
  if (reconnectTimer) {
    return;
  }
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectDaemon();
  }, delayMs);
}

function clearReconnectTimer() {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
}

function resetReconnectState() {
  reconnectAttempt = 0;
  clearReconnectTimer();
  stopDisconnectedKeepAlive();
  ensureReconnectWakeAlarm();
}

async function ensureExternalPopupRiskSuppression(reason) {
  const now = Date.now();
  if (
    popupRiskSuppressionState.status !== "not_checked" &&
    now - popupRiskSuppressionState.checked_at_unix_ms < POPUP_RISK_SUPPRESSION_RECHECK_MS
  ) {
    return popupRiskSuppressionState;
  }
  if (popupRiskSuppressionInFlight) {
    return popupRiskSuppressionInFlight;
  }
  popupRiskSuppressionInFlight = refreshExternalPopupRiskSuppression(reason)
    .catch((error) => {
      popupRiskSuppressionState = {
        ok: false,
        status: "suppression_error",
        reason,
        checked_at_unix_ms: Date.now(),
        hazard_count: 0,
        remaining_hazard_count: 0,
        disabled_count: 0,
        failure_count: 1,
        management_available: Boolean(chrome.management?.getAll),
        hazards: [],
        disabled: [],
        remaining_hazards: [],
        failures: [{
          id: "<suppression_scan>",
          name: "suppression_scan",
          error: errorMessage(error)
        }]
      };
      return popupRiskSuppressionState;
    })
    .finally(() => {
      popupRiskSuppressionInFlight = null;
    });
  return popupRiskSuppressionInFlight;
}

async function requireExternalPopupRisksSuppressed(commandKind, params) {
  if (!commandRequiresExternalPopupSuppression(commandKind)) {
    return;
  }
  const state = await ensureExternalPopupRiskSuppression(`command:${String(commandKind)}`);
  if (state.ok && state.remaining_hazard_count === 0) {
    return;
  }
  throw bridgeError(
    ERROR_DEBUGGER_WARNING_UNSUPPRESSED,
    `normal Synapse Chrome Bridge refused ${String(commandKind)} before queueing a Chrome ` +
      `tabs/scripting command because external debugger/nativeMessaging popup risk remains ` +
      `unsuppressed; hwnd=${String(params?.hwnd ?? "unknown")} ` +
      formatPopupRiskSuppressionForError(state)
  );
}

function commandRequiresExternalPopupSuppression(kind) {
  return [
    "openTab",
    "listTabs",
    "closeTab",
    "targetInfo",
    "targetInfoPageText",
    "frames",
    "typeActiveElement",
    "setFieldValue",
    "pageVitals",
    "pageContent",
    "setContent",
    "ariaSnapshot",
    "assertPoll",
    "locateElements",
    "inspectElement",
    "scrollIntoView",
    "waitForText",
    "waitForFunction",
    "waitForSelector",
    "clock",
    "pageEvents",
    "evaluateScript",
    "initScript",
    "exposeBinding",
    "handleDialog",
    "cdpInput",
    "viewportEmulation",
    "deviceEmulation",
    "geolocationEmulation",
    "localeEmulation",
    "mediaEmulation",
    "networkConditions",
    "navigateTab",
    "activateTab",
    "domAction",
    "coordinateClick"
  ].includes(kind);
}

async function refreshExternalPopupRiskSuppression(reason) {
  const checkedAt = Date.now();
  const managementAvailable = Boolean(chrome.management?.getAll);
  if (!managementAvailable) {
    popupRiskSuppressionState = {
      ok: false,
      status: "management_unavailable",
      reason,
      checked_at_unix_ms: checkedAt,
      hazard_count: 0,
      remaining_hazard_count: 0,
      disabled_count: 0,
      failure_count: 1,
      management_available: false,
      hazards: [],
      disabled: [],
      remaining_hazards: [],
      failures: [{
        id: "<chrome.management>",
        name: "chrome.management",
        error: "management permission/API unavailable"
      }]
    };
    return popupRiskSuppressionState;
  }

  const before = await chrome.management.getAll();
  const hazards = enabledExternalPopupRiskExtensions(before);
  const disabled = [];
  const failures = [];

  for (const hazard of hazards) {
    if (hazard.may_disable === false) {
      failures.push({
        ...hazard,
        error: "chrome.management reports mayDisable=false"
      });
      continue;
    }
    try {
      await chrome.management.setEnabled(hazard.id, false);
      disabled.push(hazard);
    } catch (error) {
      failures.push({
        ...hazard,
        error: errorMessage(error)
      });
    }
  }

  const after = disabled.length > 0 ? await chrome.management.getAll() : before;
  const remaining = enabledExternalPopupRiskExtensions(after);
  const remainingIds = new Set(remaining.map((entry) => entry.id));
  for (const entry of disabled) {
    if (remainingIds.has(entry.id)) {
      failures.push({
        ...entry,
        error: "chrome.management.setEnabled returned but extension remained enabled"
      });
    }
  }

  const ok = remaining.length === 0 && failures.length === 0;
  popupRiskSuppressionState = {
    ok,
    status: ok ? (hazards.length > 0 ? "suppressed" : "clear") : "unsuppressed",
    reason,
    checked_at_unix_ms: checkedAt,
    hazard_count: hazards.length,
    remaining_hazard_count: remaining.length,
    disabled_count: disabled.length,
    failure_count: failures.length,
    management_available: true,
    hazards,
    disabled,
    remaining_hazards: remaining,
    failures
  };
  if (!ok) {
    console.warn(
      `Synapse external popup risk suppression incomplete: ` +
        formatPopupRiskSuppressionForError(popupRiskSuppressionState)
    );
  } else if (disabled.length > 0) {
    console.warn(
      `Synapse disabled ${disabled.length} external debugger/nativeMessaging extension(s): ` +
        disabled.map((entry) => `${entry.id}:${entry.name}`).join(" | ")
    );
  }
  return popupRiskSuppressionState;
}

function enabledExternalPopupRiskExtensions(items) {
  return (Array.isArray(items) ? items : [])
    .filter((info) => info?.id && info.id !== chrome.runtime.id && info.enabled === true)
    .map((info) => {
      const permissions = Array.isArray(info.permissions) ? info.permissions : [];
      const hazardPermissions = permissions
        .filter((permission) => EXTERNAL_POPUP_RISK_PERMISSIONS.includes(permission))
        .sort();
      return {
        id: String(info.id),
        name: String(info.name || ""),
        type: String(info.type || ""),
        install_type: String(info.installType || ""),
        may_disable: info.mayDisable !== false,
        permissions: permissions.map(String).sort(),
        hazard_permissions: [...new Set(hazardPermissions)],
        enabled: info.enabled === true,
        disabled_reason: info.disabledReason ? String(info.disabledReason) : ""
      };
    })
    .filter((info) => info.hazard_permissions.length > 0)
    .sort((a, b) => a.id.localeCompare(b.id));
}

function popupRiskSuppressionSnapshot() {
  return JSON.parse(JSON.stringify(popupRiskSuppressionState));
}

function formatPopupRiskSuppressionForError(state) {
  const remaining = Array.isArray(state?.remaining_hazards) ? state.remaining_hazards : [];
  const failures = Array.isArray(state?.failures) ? state.failures : [];
  const remainingText = remaining.length > 0
    ? remaining
        .slice(0, 8)
        .map((entry) => `${entry.id}:${entry.name}:${entry.hazard_permissions.join(",")}`)
        .join(" | ")
    : "<none>";
  const failureText = failures.length > 0
    ? failures
        .slice(0, 8)
        .map((entry) => `${entry.id}:${entry.name}:${entry.error || "unknown"}`)
        .join(" | ")
    : "<none>";
  return (
    `suppression_status=${String(state?.status || "unknown")} ` +
    `suppression_reason=${String(state?.reason || "unknown")} ` +
    `management_available=${Boolean(state?.management_available)} ` +
    `hazard_count=${Number(state?.hazard_count || 0)} ` +
    `disabled_count=${Number(state?.disabled_count || 0)} ` +
    `remaining_hazard_count=${Number(state?.remaining_hazard_count || 0)} ` +
    `failure_count=${Number(state?.failure_count || 0)} ` +
    `remaining_hazards=${remainingText} failures=${failureText} ` +
    `remediation=disable the named extension IDs in Chrome or repair ` +
    `HKCU\\Software\\Policies\\Google\\Chrome ACL so Synapse can apply ` +
    `ExtensionSettings blocked_permissions for debugger/nativeMessaging`
  );
}

function connectWebSocket() {
  if (permanentlyDisabled || !hostId || !bridgeToken || webSocket) {
    return;
  }
  const url = new URL("/chrome-debugger/native/ws", DAEMON_WS_BASE_URL);
  url.searchParams.set("host_id", hostId);
  url.searchParams.set("bridge_token", bridgeToken);
  const socket = new WebSocket(url.toString());
  webSocket = socket;
  socket.onopen = () => {
    resetReconnectState();
    console.info(`Synapse daemon bridge connected: host_id=${hostId}`);
    startWebSocketKeepAlive(socket);
  };
  socket.onmessage = (event) => {
    handleWebSocketMessage(event.data).catch((error) => {
      scheduleReconnect(
        `direct daemon websocket message failed: ${errorMessage(error)}`,
        ERROR_DAEMON_UNAVAILABLE
      );
    });
  };
  socket.onerror = () => {
    if (webSocket === socket) {
      scheduleReconnect("direct daemon websocket error", ERROR_DAEMON_UNAVAILABLE);
    }
  };
  socket.onclose = () => {
    if (webSocket === socket) {
      scheduleReconnect("direct daemon websocket closed", ERROR_DAEMON_UNAVAILABLE);
    }
  };
}

function isWebSocketUsable(socket) {
  return socket.readyState === WebSocket.CONNECTING || socket.readyState === WebSocket.OPEN;
}

async function handleWebSocketMessage(raw) {
  if (typeof raw !== "string") {
    throw new Error(`daemon websocket returned non-text payload type=${typeof raw}`);
  }
  let message;
  try {
    message = JSON.parse(raw);
  } catch (_) {
    throw new Error(`daemon websocket returned non-JSON payload=${JSON.stringify(raw.slice(0, 512))}`);
  }
  if (message?.ok === false) {
    throw new Error(`daemon websocket refused command delivery: ${JSON.stringify(message)}`);
  }
  if (message?.command) {
    await handleCommand(message.command);
  }
}

function startWebSocketKeepAlive(socket) {
  stopWebSocketKeepAlive();
  keepAliveTimer = setInterval(() => {
    if (webSocket !== socket || socket.readyState !== WebSocket.OPEN) {
      stopWebSocketKeepAlive();
      return;
    }
    try {
      socket.send(JSON.stringify({
        type: "keepalive",
        host_id: hostId,
        sent_at_unix_ms: Date.now()
      }));
    } catch (error) {
      scheduleReconnect(
        `direct daemon websocket keepalive failed: ${errorMessage(error)}`,
        ERROR_DAEMON_UNAVAILABLE
      );
    }
  }, WEBSOCKET_KEEPALIVE_MS);
}

function stopWebSocketKeepAlive() {
  if (keepAliveTimer) {
    clearInterval(keepAliveTimer);
    keepAliveTimer = null;
  }
}

function startDisconnectedKeepAlive() {
  ensureReconnectWakeAlarm();
  if (disconnectedKeepAliveTimer) {
    return;
  }
  disconnectedKeepAliveTimer = setInterval(() => {
    if (permanentlyDisabled || (webSocket && isWebSocketUsable(webSocket))) {
      stopDisconnectedKeepAlive();
      return;
    }
    try {
      const platformInfo = chrome.runtime.getPlatformInfo();
      if (platformInfo && typeof platformInfo.catch === "function") {
        platformInfo.catch((error) => {
          console.warn(
            `Synapse daemon bridge reconnect keepalive failed: ${errorMessage(error)}`
          );
        });
      }
    } catch (error) {
      console.warn(`Synapse daemon bridge reconnect keepalive threw: ${errorMessage(error)}`);
    }
  }, DISCONNECTED_KEEPALIVE_MS);
}

function stopDisconnectedKeepAlive() {
  if (disconnectedKeepAliveTimer) {
    clearInterval(disconnectedKeepAliveTimer);
    disconnectedKeepAliveTimer = null;
  }
}

function ensureReconnectWakeAlarm() {
  if (!chrome.alarms?.create) {
    console.error(
      "Synapse daemon bridge cannot register reconnect wake alarm; " +
        "manifest must include required chrome.alarms permission"
    );
    return;
  }
  try {
    const created = chrome.alarms.create(RECONNECT_WAKE_ALARM_NAME, {
      delayInMinutes: RECONNECT_WAKE_ALARM_DELAY_MINUTES,
      periodInMinutes: RECONNECT_WAKE_ALARM_PERIOD_MINUTES
    });
    if (created && typeof created.catch === "function") {
      created.catch((error) => {
        console.warn(`Synapse reconnect wake alarm setup failed: ${errorMessage(error)}`);
      });
    }
  } catch (error) {
    console.warn(`Synapse reconnect wake alarm setup threw: ${errorMessage(error)}`);
  }
}

function handleReconnectWakeAlarm(alarm) {
  if (alarm?.name !== RECONNECT_WAKE_ALARM_NAME || permanentlyDisabled) {
    return;
  }
  startBridgeFromEvent();
}

function handleManagementStateChanged(info) {
  if (!info || info.id === chrome.runtime.id) {
    return;
  }
  popupRiskSuppressionState = {
    ...popupRiskSuppressionState,
    ok: false,
    status: "stale",
    reason: "chrome.management event",
    checked_at_unix_ms: 0
  };
  ensureExternalPopupRiskSuppression("chrome.management event").catch((error) => {
    console.warn(`Synapse popup risk suppression refresh failed: ${errorMessage(error)}`);
  });
}

function closeWebSocket() {
  stopWebSocketKeepAlive();
  const socket = webSocket;
  webSocket = null;
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    try {
      socket.close();
    } catch (_) {
      // Closing is best-effort during reconnect cleanup.
    }
  }
}

chrome.runtime.onInstalled.addListener(startBridgeFromEvent);
chrome.runtime.onStartup.addListener(startBridgeFromEvent);
if (chrome.alarms?.onAlarm?.addListener) {
  chrome.alarms.onAlarm.addListener(handleReconnectWakeAlarm);
}
if (chrome.management?.onEnabled?.addListener) {
  chrome.management.onEnabled.addListener(handleManagementStateChanged);
}
if (chrome.management?.onInstalled?.addListener) {
  chrome.management.onInstalled.addListener(handleManagementStateChanged);
}
if (chrome.management?.onDisabled?.addListener) {
  chrome.management.onDisabled.addListener(handleManagementStateChanged);
}
chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (!changeInfo?.url && !changeInfo?.title && changeInfo?.status !== "complete") {
    return;
  }
  updatePageEventTabState(tabId, tab, changeInfo);
  postTabNavigationEvent("tabs.onUpdated", tab).catch((error) => {
    console.warn(`Synapse tab navigation event dropped: ${errorMessage(error)}`);
  });
});
chrome.tabs.onCreated.addListener((tab) => {
  recordTabCreatedForPageEvents(tab);
});
chrome.tabs.onRemoved.addListener((tabId) => {
  recordTabRemovedForPageEvents(tabId);
  INIT_SCRIPT_DEBUGGER_SESSIONS.delete(tabId);
  BINDING_DEBUGGER_SESSIONS.delete(tabId);
  DIALOG_DEBUGGER_SESSIONS.delete(tabId);
  FILE_CHOOSER_DEBUGGER_SESSIONS.delete(tabId);
});
if (chrome.debugger?.onDetach?.addListener) {
  chrome.debugger.onDetach.addListener((source, reason) => {
    if (Number.isInteger(source?.tabId)) {
      INIT_SCRIPT_DEBUGGER_SESSIONS.delete(source.tabId);
      markBindingDebuggerDetached(source.tabId);
      markDialogDebuggerDetached(source.tabId);
      markFileChooserDebuggerDetached(source.tabId);
      console.warn(`Synapse persistent debugger session detached for tab ${source.tabId}: ${String(reason || "unknown")}`);
    }
  });
}
if (chrome.debugger?.onEvent?.addListener) {
  chrome.debugger.onEvent.addListener((source, method, params) => {
    if (method === "Runtime.bindingCalled" && Number.isInteger(source?.tabId)) {
      recordBindingCalledEvent(source.tabId, params || {});
    } else if (method === "Page.javascriptDialogOpening" && Number.isInteger(source?.tabId)) {
      recordDialogOpeningEvent(source.tabId, params || {});
    } else if (method === "Page.javascriptDialogClosed" && Number.isInteger(source?.tabId)) {
      recordDialogClosedEvent(source.tabId, params || {});
    } else if (method === "Page.fileChooserOpened" && Number.isInteger(source?.tabId)) {
      recordFileChooserOpenedEvent(source.tabId, params || {});
    }
  });
}
chrome.tabs.onActivated.addListener((activeInfo) => {
  chrome.tabs.get(activeInfo.tabId)
    .then((tab) => postTabNavigationEvent("tabs.onActivated", tab))
    .catch((error) => {
      console.warn(`Synapse active tab navigation readback failed: ${errorMessage(error)}`);
    });
});
if (chrome.webNavigation?.onBeforeNavigate?.addListener) {
  chrome.webNavigation.onBeforeNavigate.addListener((details) => {
    recordWebNavigationPageEvent("framestartednavigating", details);
  });
}
if (chrome.webNavigation?.onCommitted?.addListener) {
  chrome.webNavigation.onCommitted.addListener((details) => {
    recordWebNavigationPageEvent("framenavigated", details, {
      navigation_type: webNavigationTransition(details)
    });
  });
}
if (chrome.webNavigation?.onDOMContentLoaded?.addListener) {
  chrome.webNavigation.onDOMContentLoaded.addListener((details) => {
    recordWebNavigationPageEvent("domcontentloaded", details);
  });
}
if (chrome.webNavigation?.onCompleted?.addListener) {
  chrome.webNavigation.onCompleted.addListener((details) => {
    recordWebNavigationPageEvent("load", details);
    recordWebNavigationPageEvent("framestoppedloading", details);
  });
}
if (chrome.webNavigation?.onHistoryStateUpdated?.addListener) {
  chrome.webNavigation.onHistoryStateUpdated.addListener((details) => {
    recordWebNavigationPageEvent("framenavigated", details, {
      navigation_type: webNavigationTransition(details) || "history_api"
    });
  });
}
if (chrome.webRequest?.onBeforeRequest?.addListener) {
  chrome.webRequest.onBeforeRequest.addListener(
    (details) => recordWebRequestStarted(details),
    { urls: ["<all_urls>"] }
  );
}
if (chrome.webRequest?.onHeadersReceived?.addListener) {
  chrome.webRequest.onHeadersReceived.addListener(
    (details) => recordWebRequestHeadersReceived(details),
    { urls: ["<all_urls>"] }
  );
}
if (chrome.webRequest?.onCompleted?.addListener) {
  chrome.webRequest.onCompleted.addListener(
    (details) => recordWebRequestCompleted(details),
    { urls: ["<all_urls>"] }
  );
}
if (chrome.webRequest?.onErrorOccurred?.addListener) {
  chrome.webRequest.onErrorOccurred.addListener(
    (details) => recordWebRequestFailed(details),
    { urls: ["<all_urls>"] }
  );
}
if (chrome.downloads?.onCreated?.addListener) {
  chrome.downloads.onCreated.addListener((item) => recordDownloadEvent("created", item, null));
}
if (chrome.downloads?.onChanged?.addListener) {
  chrome.downloads.onChanged.addListener((delta) => {
    const id = Number(delta && delta.id);
    if (chrome.downloads?.search && Number.isSafeInteger(id)) {
      chrome.downloads.search({ id }).then((items) => {
        recordDownloadEvent("changed", items && items[0] ? items[0] : { id }, delta);
      }).catch(() => recordDownloadEvent("changed", { id }, delta));
    } else {
      recordDownloadEvent("changed", { id }, delta);
    }
  });
}
if (chrome.downloads?.onErased?.addListener) {
  chrome.downloads.onErased.addListener((id) => recordDownloadEvent("erased", { id }, null));
}

startBridgeFromEvent();

async function handleCommand(command) {
  if (!command || typeof command !== "object") {
    throw bridgeError(ERROR_ATTACH_FAILED, "native command was not an object");
  }
  const { id, kind, params = {} } = command;
  if (!id || typeof id !== "string") {
    throw bridgeError(ERROR_ATTACH_FAILED, "native command id is required");
  }
  try {
    let result;
    let reloadAfterResponse = false;
    await requireExternalPopupRisksSuppressed(kind, params);
    if (kind === "snapshot") {
      result = rejectAttachCommand(kind, params);
    } else if (kind === "clickNode") {
      result = rejectAttachCommand(kind, params);
    } else if (kind === "typeNode") {
      result = rejectAttachCommand(kind, params);
    } else if (kind === "nodeValue") {
      result = rejectAttachCommand(kind, params);
    } else if (kind === "openTab") {
      result = await handleOpenTab(params);
    } else if (kind === "listTabs") {
      result = await handleListTabs(params);
    } else if (kind === "closeTab") {
      result = await handleCloseTab(params);
    } else if (kind === "capturePageScreenshot") {
      result = rejectAttachCommand(kind, params);
    } else if (kind === "targetInfo" || kind === "targetInfoPageText") {
      result = await handleTargetInfo(params);
    } else if (kind === "frames") {
      result = await handleFrames(params);
    } else if (kind === "pageContent") {
      result = await handlePageContent(params);
    } else if (kind === "pageScreenshot") {
      result = await handlePageScreenshot(params);
    } else if (kind === "pagePdf") {
      result = await handlePagePdf(params);
    } else if (kind === "downloads") {
      result = await handleDownloads(params);
    } else if (kind === "cookies") {
      result = await handleCookies(params);
    } else if (kind === "storageState") {
      result = await handleStorageState(params);
    } else if (kind === "setContent") {
      result = await handleSetContent(params);
    } else if (kind === "ariaSnapshot") {
      result = await handleAriaSnapshot(params);
    } else if (kind === "assertPoll") {
      result = await handleAssertPoll(params);
    } else if (kind === "locateElements") {
      result = await handleLocateElements(params);
    } else if (kind === "inspectElement") {
      result = await handleInspectElement(params);
    } else if (kind === "scrollIntoView") {
      result = await handleScrollIntoView(params);
    } else if (kind === "waitForText") {
      result = await handleWaitForText(params);
    } else if (kind === "waitForFunction") {
      result = await handleWaitForFunction(params);
    } else if (kind === "waitForLoadState") {
      result = await handleWaitForLoadState(params);
    } else if (kind === "waitForUrl") {
      result = await handleWaitForUrl(params);
    } else if (kind === "waitForRequest") {
      result = await handleWaitForNetwork(params, false);
    } else if (kind === "waitForResponse") {
      result = await handleWaitForNetwork(params, true);
    } else if (kind === "waitForSelector") {
      result = await handleWaitForSelector(params);
    } else if (kind === "clock") {
      result = await handleClock(params);
    } else if (kind === "pageEvents") {
      result = await handlePageEvents(params);
    } else if (kind === "cdpInput") {
      result = await handleCdpInput(params);
    } else if (kind === "viewportEmulation") {
      result = await handleViewportEmulation(params);
    } else if (kind === "deviceEmulation") {
      result = await handleDeviceEmulation(params);
    } else if (kind === "geolocationEmulation") {
      result = await handleGeolocationEmulation(params);
    } else if (kind === "localeEmulation") {
      result = await handleLocaleEmulation(params);
    } else if (kind === "mediaEmulation") {
      result = await handleMediaEmulation(params);
    } else if (kind === "networkConditions") {
      result = await handleNetworkConditions(params);
    } else if (kind === "typeActiveElement") {
      result = await handleTypeActiveElement(params);
    } else if (kind === "setFieldValue") {
      result = await handleSetFieldValue(params);
    } else if (kind === "evaluateScript") {
      result = await handleEvaluateScript(params);
    } else if (kind === "initScript") {
      result = await handleInitScript(params);
    } else if (kind === "exposeBinding") {
      result = await handleExposeBinding(params);
    } else if (kind === "handleDialog") {
      result = await handleDialog(params);
    } else if (kind === "fileUpload") {
      result = await handleFileUpload(params);
    } else if (kind === "pageVitals") {
      result = await handlePageVitals(params);
    } else if (kind === "navigateTab") {
      result = await handleNavigateTab(params);
    } else if (kind === "activateTab") {
      result = await handleActivateTab(params);
    } else if (kind === "domAction") {
      result = await handleDomAction(params);
    } else if (kind === "coordinateClick") {
      result = await handleCoordinateClick(params);
    } else if (kind === "reloadSelf") {
      result = handleReloadSelf(params);
      reloadAfterResponse = true;
    } else {
      throw bridgeError(
        ERROR_EXTENSION_STALE,
        `unknown command kind ${String(kind)}; loaded_build_id=${BRIDGE_BUILD_ID} ` +
          `loaded_capabilities=${COMMAND_CAPABILITIES.join(",")}`
      );
    }
    await postResponse(id, true, result, null);
    if (reloadAfterResponse) {
      scheduleRuntimeReload(result.reload_delay_ms);
    }
  } catch (error) {
    await postResponse(id, false, null, errorPayload(error));
  }
}

function bridgeIdentity() {
  return {
    extensionId: chrome.runtime.id,
    version: chrome.runtime.getManifest().version,
    protocolVersion: PROTOCOL_VERSION,
    buildId: BRIDGE_BUILD_ID,
    buildSha256: BRIDGE_BUILD_SHA256,
    debuggerApiAvailable: runtimeDebuggerApiAvailable(),
    capabilities: [...COMMAND_CAPABILITIES],
    commandCapabilities: [...COMMAND_CAPABILITIES],
    popupRiskSuppression: popupRiskSuppressionSnapshot()
  };
}

function runtimeDebuggerApiAvailable() {
  return Boolean(
    chrome.debugger &&
    (chrome.debugger.attach || chrome.debugger.sendCommand || chrome.debugger.getTargets)
  );
}

function requireDebuggerApiAvailable(kind, params) {
  if (runtimeDebuggerApiAvailable()) {
    return;
  }
  throw bridgeError(
    ERROR_DEBUGGER_WARNING_UNSUPPRESSED,
    `Synapse Chrome Bridge refused ${String(kind)} because chrome.debugger is unavailable ` +
      `in the loaded extension runtime; hwnd=${String(params?.hwnd ?? "unknown")} ` +
      `remediation=run scripts\\install-synapse-chrome-debugger.ps1 and cdp_bridge_reload ` +
      `so the active already-open Chrome profile loads the debugger-capable bridge build`
  );
}

function rejectAttachCommand(kind, params) {
  throw bridgeError(
    ERROR_DEBUGGER_WARNING_UNSUPPRESSED,
    `Synapse Chrome Bridge refused unsupported legacy attach command ${String(kind)}; ` +
      `target-scoped CDP input is exposed through cdpInput, while full DOM snapshots and ` +
      `element-scoped DOM CDP work still requires the dedicated raw-CDP automation profile. ` +
      `hwnd=${String(params?.hwnd ?? "unknown")}`
  );
}

async function handleOpenTab(params) {
  const requestedUrl = normalizeOpenUrl(params.url);
  const agentSessionId = normalizeOptionalSessionId(params.agentSessionId);
  const beforePages = await tabTargets();
  const openWindow = await selectOpenWindowForHwndHint(params);
  let tab;
  const createParams = {
    url: requestedUrl || "about:blank",
    active: false
  };
  if (Number.isInteger(openWindow?.windowId)) {
    createParams.windowId = openWindow.windowId;
  }
  try {
    tab = await chrome.tabs.create(createParams);
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.tabs.create(active=false): ${errorMessage(error)}`);
  }
  if (!tab || typeof tab.id !== "number") {
    throw bridgeError(ERROR_AXTREE_FAILED, "chrome.tabs.create returned no numeric tab id");
  }
  markAgentNavigation(tab.id, {
    action: "open",
    requestedUrl: requestedUrl || "about:blank",
    sessionId: agentSessionId
  });
  const target = await waitForTabTarget(tab.id, 10000);
  const state = await tabPageState(tab.id, target);
  if (
    Number.isInteger(openWindow?.windowId) &&
    Number.isInteger(state.chrome_window_id) &&
    state.chrome_window_id !== openWindow.windowId
  ) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.tabs.create returned tab in Chrome window ${state.chrome_window_id}, expected ${openWindow.windowId}`
    );
  }
  const openedWindow = Number.isInteger(state.chrome_window_id)
    ? await chromeWindowState(state.chrome_window_id)
    : null;
  const afterPages = await tabTargets();
  return {
    extension_id: chrome.runtime.id,
    target_id: target.id,
    tab_id: tab.id,
    chrome_window_id: state.chrome_window_id,
    chrome_window_focused: typeof openedWindow?.focused === "boolean" ? openedWindow.focused : null,
    chrome_window_state: typeof openedWindow?.state === "string" ? openedWindow.state : "",
    chrome_window_selection_reason: openWindow?.selectionReason || "default_chrome_tabs_create",
    chrome_window_candidate_count: openWindow?.candidateCount || 0,
    chrome_window_non_focused_count: openWindow?.nonFocusedCount || 0,
    target_active: Boolean(state.active),
    target_highlighted: Boolean(state.highlighted),
    target_type: target.type || "page",
    url: state.url || target.url || tab.url || requestedUrl || "about:blank",
    title: state.title || target.title || tab.title || "",
    target_attached: Boolean(target.attached),
    target_count_before: beforePages.length,
    target_count_after: afterPages.length
  };
}

async function handleCloseTab(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const beforePages = await tabTargets();
  try {
    await chrome.tabs.remove(selected.tabId);
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.tabs.remove(${selected.tabId}): ${errorMessage(error)}`);
  }
  const afterPages = await waitForTargetAbsent(selected.target.id, 10000);
  return {
    extension_id: chrome.runtime.id,
    target_id: selected.target.id,
    tab_id: selected.tabId,
    target_count_before: beforePages.length,
    target_count_after: afterPages.length
  };
}

async function handleListTabs(params) {
  const openWindow = await selectOpenWindowForHwndHint(params);
  const query = {};
  if (Number.isInteger(openWindow?.windowId)) {
    query.windowId = openWindow.windowId;
  }
  let tabs;
  try {
    tabs = await chrome.tabs.query(query);
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.tabs.query(${JSON.stringify(query)}): ${errorMessage(error)}`);
  }
  const targets = tabs
    .filter((tab) => typeof tab.id === "number")
    .map((tab) => tabListEntryFromTabTarget(tabTargetFromTab(tab), tab));
  let selectedWindow = null;
  if (Number.isInteger(openWindow?.windowId)) {
    selectedWindow = await chromeWindowState(openWindow.windowId);
  }
  return {
    extension_id: chrome.runtime.id,
    chrome_window_id: Number.isInteger(openWindow?.windowId) ? openWindow.windowId : null,
    chrome_window_focused: typeof selectedWindow?.focused === "boolean" ? selectedWindow.focused : null,
    chrome_window_state: typeof selectedWindow?.state === "string" ? selectedWindow.state : "",
    chrome_window_selection_reason: openWindow?.selectionReason || "all_chrome_windows",
    chrome_window_candidate_count: openWindow?.candidateCount || 0,
    chrome_window_non_focused_count: openWindow?.nonFocusedCount || 0,
    target_count: targets.length,
    active_tab_count: targets.filter((target) => target.active).length,
    tabs: targets
  };
}

async function handleTargetInfo(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  const activeElement = await tabActiveElementState(selected.tabId);
  const pageText = await tabPageTextState(selected.tabId);
  const pageVitals = await tabPageVitalsState(selected.tabId);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    target_type: state.target_type || selected.target.type || "page",
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    active: Boolean(state.active),
    highlighted: Boolean(state.highlighted),
    pinned: Boolean(state.pinned),
    readback_backend: "chrome.tabs.get+chrome.scripting.executeScript",
    active_element: activeElement,
    page_text: pageText,
    page_vitals: pageVitals,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleEvaluateScript(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const expression = normalizeEvaluateExpression(params.expression);
  const args = normalizeEvaluateArgs(params.args);
  const awaitPromise = normalizeEvaluateBoolean(params.awaitPromise, true, "awaitPromise");
  const returnByValue = normalizeEvaluateBoolean(params.returnByValue, true, "returnByValue");
  const expressionToRun = args.length > 0 ? buildEvaluateFunctionInvocation(expression, args) : expression;
  const protocolVersion = "1.3";
  let attachment = null;
  let evaluation;
  try {
    attachment = await attachDebuggerForCommand(selected.tabId, protocolVersion);
    evaluation = await sendDebuggerCommand(attachment.debuggee, "Runtime.evaluate", {
      expression: expressionToRun,
      awaitPromise,
      returnByValue,
      userGesture: true
    });
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger Runtime.evaluate failed for tab ${selected.tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attachment?.shouldDetach) {
      try {
        await chrome.debugger.detach(attachment.debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for evaluateScript tab ${selected.tabId}: ${errorMessage(error)}`);
      }
    }
  }
  if (evaluation?.exceptionDetails) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `Runtime.evaluate exception: ${formatDebuggerExceptionDetails(evaluation.exceptionDetails)}`
    );
  }
  const remote = evaluation?.result || {};
  const state = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    scope: "page",
    result_type: String(remote.type || ""),
    result_subtype: remote.subtype === undefined ? null : String(remote.subtype),
    returned_by_value: Boolean(returnByValue),
    value: Object.prototype.hasOwnProperty.call(remote, "value") ? remote.value : null,
    description: remote.description === undefined ? null : String(remote.description),
    unserializable_value: remote.unserializableValue === undefined ? null : String(remote.unserializableValue),
    readback_backend: "chrome.debugger.Runtime.evaluate",
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleInitScript(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeInitScriptOperation(params.operation);
  const protocolVersion = "1.3";
  let identifier = "";
  let addAttachment = null;
  let temporaryRemoveAttachment = null;
  let persistentSession = false;
  let detachedAfterRemove = false;
  try {
    if (operation === "add") {
      const source = normalizeInitScriptSource(params.source);
      addAttachment = await ensureInitScriptDebuggerSession(selected.tabId, protocolVersion);
      await ensureInitScriptPageDomainEnabled(addAttachment.debuggee, addAttachment.session);
      persistentSession = true;
      const commandParams = {
        source,
        includeCommandLineAPI: normalizeEvaluateBoolean(
          params.includeCommandLineAPI,
          false,
          "includeCommandLineAPI"
        ),
        runImmediately: normalizeEvaluateBoolean(params.runImmediately, false, "runImmediately")
      };
      const worldName = normalizeOptionalNonEmptyString(params.worldName, "worldName");
      if (worldName) {
        commandParams.worldName = worldName;
      }
      const result = await sendDebuggerCommand(
        addAttachment.debuggee,
        "Page.addScriptToEvaluateOnNewDocument",
        commandParams
      );
      identifier = String(result?.identifier || "");
      if (!identifier) {
        throw new Error("Page.addScriptToEvaluateOnNewDocument returned no identifier");
      }
      addAttachment.session.identifiers.add(identifier);
    } else {
      identifier = normalizeOptionalNonEmptyString(params.identifier, "identifier");
      if (!identifier) {
        throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "initScript operation=remove requires identifier");
      }
      let debuggee = { tabId: selected.tabId };
      if (INIT_SCRIPT_DEBUGGER_SESSIONS.has(selected.tabId)) {
        persistentSession = true;
        await ensureInitScriptPageDomainEnabled(debuggee, INIT_SCRIPT_DEBUGGER_SESSIONS.get(selected.tabId));
      } else {
        temporaryRemoveAttachment = await attachDebuggerForCommand(selected.tabId, protocolVersion);
        debuggee = temporaryRemoveAttachment.debuggee;
        await sendDebuggerCommand(debuggee, "Page.enable", {});
      }
      await sendDebuggerCommand(debuggee, "Page.removeScriptToEvaluateOnNewDocument", {
        identifier
      });
      if (persistentSession) {
        detachedAfterRemove = await maybeDetachInitScriptDebuggerSession(selected.tabId, debuggee, identifier);
      }
    }
  } catch (error) {
    if (operation === "add" && addAttachment?.newlyCreated && !identifier) {
      INIT_SCRIPT_DEBUGGER_SESSIONS.delete(selected.tabId);
      if (addAttachment.newlyAttached && !bindingSessionHasActiveNames(selected.tabId)) {
        try {
          await chrome.debugger.detach(addAttachment.debuggee);
        } catch (detachError) {
          console.warn(`Synapse chrome.debugger detach failed after initScript add error for tab ${selected.tabId}: ${errorMessage(detachError)}`);
        }
      }
    }
    if (error?.code) {
      throw error;
    }
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger initScript ${operation} failed for tab ${selected.tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (temporaryRemoveAttachment?.shouldDetach) {
      try {
        await chrome.debugger.detach(temporaryRemoveAttachment.debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for initScript tab ${selected.tabId}: ${errorMessage(error)}`);
      }
    }
  }
  const state = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    operation,
    identifier,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    readback_backend: "chrome.debugger.Page.addScriptToEvaluateOnNewDocument/Page.removeScriptToEvaluateOnNewDocument",
    debugger_session_persistent: persistentSession,
    debugger_session_detached: detachedAfterRemove,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleExposeBinding(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeBindingOperation(params.operation);
  const name = normalizeBindingName(params.name);
  const sinceSeq = normalizeOptionalNonNegativeInteger(params.sinceSeq, "sinceSeq");
  const maxCalls = normalizeBindingMaxCalls(params.maxCalls);
  let session = BINDING_DEBUGGER_SESSIONS.get(selected.tabId) || null;
  let status = null;
  try {
    if (operation === "add") {
      const executionContextName = normalizeOptionalNonEmptyString(
        params.executionContextName,
        "executionContextName"
      );
      const ensured = await ensureBindingDebuggerSession(selected.tabId);
      session = ensured.session;
      const bindingNewlyAdded = await addBindingToSession(
        ensured.debuggee,
        session,
        name,
        executionContextName
      );
      status = bindingStatusFromSession(session, name, ensured.newlyArmed, bindingNewlyAdded, false);
    } else if (operation === "read") {
      if (!session) {
        throw bridgeError(
          ERROR_ACTION_TARGET_INVALID,
          `browser_expose_binding read requested binding ${JSON.stringify(name)} on tab ${selected.tabId}, but no capture is armed; call operation=add first`
        );
      }
      status = bindingStatusFromSession(session, name, false, false, false);
    } else {
      let bindingRemoved = false;
      if (session?.activeNames?.has(name)) {
        const ensured = await ensureBindingDebuggerSession(selected.tabId);
        session = ensured.session;
        await sendDebuggerCommand(ensured.debuggee, "Runtime.removeBinding", { name });
        session.activeNames.delete(name);
        bindingRemoved = true;
        if (session.activeNames.size === 0 && !hasActiveInitScriptDebuggerSession(selected.tabId)) {
          await detachBindingDebuggerSession(selected.tabId, ensured.debuggee, session);
        }
      }
      status = session
        ? bindingStatusFromSession(session, name, false, false, bindingRemoved)
        : emptyBindingStatus(selected.tabId, name, bindingRemoved);
    }
  } catch (error) {
    if (error?.code) {
      throw error;
    }
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger exposeBinding ${operation} failed for tab ${selected.tabId}: ${errorMessage(error)}`
    );
  }
  session = BINDING_DEBUGGER_SESSIONS.get(selected.tabId) || session;
  const read = session
    ? readBindingCalls(session, name, sinceSeq, maxCalls)
    : emptyBindingRead(status);
  const state = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    operation,
    name,
    newly_armed: Boolean(status.newly_armed),
    binding_newly_added: Boolean(status.binding_newly_added),
    binding_removed: Boolean(status.binding_removed),
    armed_at_unix_ms: Number(read.armed_at_unix_ms || status.armed_at_unix_ms || 0),
    binding_active: Boolean(read.binding_active),
    active_binding_count: Number(read.active_binding_count || 0),
    active_binding_names: Array.isArray(read.active_binding_names) ? read.active_binding_names : [],
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    calls: read.calls,
    next_cursor: read.next_cursor,
    returned: read.returned,
    total_buffered: read.total_buffered,
    dropped: read.dropped,
    readback_backend: "chrome.debugger.Runtime.addBinding+Runtime.bindingCalled+Runtime.removeBinding",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleDialog(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeDialogOperation(params.operation);
  const policy = normalizeDialogDefaultPolicy(params.defaultPolicy);
  const sinceSeq = normalizeOptionalNonNegativeInteger(params.sinceSeq, "sinceSeq");
  const limit = normalizeDialogLimit(params.limit);
  const promptText = normalizeDialogPromptText(params.promptText);
  if (operation === "set_policy" && !policy) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "handleDialog operation=set_policy requires defaultPolicy"
    );
  }
  if (operation !== "accept" && promptText !== null) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "handleDialog promptText is only valid for operation=accept"
    );
  }

  let session = DIALOG_DEBUGGER_SESSIONS.get(selected.tabId) || null;
  let captureNewlyArmed = false;
  let handledDialog = null;
  let handleAction = null;
  let effectivePromptText = null;
  try {
    if (operation === "status" || operation === "set_policy") {
      const defaultPolicy = policy || "dismiss";
      const ensured = await ensureDialogDebuggerSession(selected.tabId, defaultPolicy);
      session = ensured.session;
      captureNewlyArmed = ensured.newlyArmed;
    } else {
      if (!session?.attached) {
        throw bridgeError(
          ERROR_ACTION_TARGET_INVALID,
          `browser_handle_dialog ${operation} requested for tab ${selected.tabId}, but dialog capture is not armed; call operation=status first`
        );
      }
      const pending = dialogPendingEntry(session);
      if (!pending) {
        throw bridgeError(
          ERROR_ACTION_TARGET_INVALID,
          `browser_handle_dialog ${operation} requested for tab ${selected.tabId}, but no JavaScript dialog is pending`
        );
      }
      const debuggee = { tabId: selected.tabId };
      handleAction = operation;
      effectivePromptText = operation === "accept"
        ? (promptText ?? pending.default_prompt ?? "")
        : null;
      await sendDebuggerCommand(debuggee, "Page.handleJavaScriptDialog", dialogManualHandleParams(
        operation,
        effectivePromptText
      ));
      handledDialog = markDialogManualHandled(session, pending.seq, operation, effectivePromptText);
    }
  } catch (error) {
    if (error?.code) {
      throw error;
    }
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger handleDialog ${operation} failed for tab ${selected.tabId}: ${errorMessage(error)}`
    );
  }

  session = DIALOG_DEBUGGER_SESSIONS.get(selected.tabId) || session;
  const read = session
    ? readDialogEntries(session, sinceSeq, limit)
    : emptyDialogRead();
  const state = await tabPageState(selected.tabId, selected.target);
  const pendingDialog = read.pending_dialog || dialogPendingEntry(session);
  const lastDialog = session?.entries?.length
    ? session.entries[session.entries.length - 1]
    : null;
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    operation,
    default_policy: session?.defaultPolicy || policy || "dismiss",
    capture_newly_armed: Boolean(captureNewlyArmed),
    handled: Boolean(handledDialog),
    handle_action: handleAction,
    prompt_text: effectivePromptText,
    pending_dialog: pendingDialog,
    handled_dialog: handledDialog,
    last_dialog: lastDialog,
    entries: read.entries,
    next_cursor: read.next_cursor,
    returned: read.returned,
    total_buffered: read.total_buffered,
    dropped: read.dropped,
    opened_count: Number(session?.openedCount || 0),
    closed_count: Number(session?.closedCount || 0),
    auto_handled_count: Number(session?.autoHandledCount || 0),
    error_count: Number(session?.errorCount || 0),
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    readback_backend: "chrome.debugger.Page.javascriptDialogOpening+Page.handleJavaScriptDialog",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleFrames(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  if (!chrome.webNavigation || typeof chrome.webNavigation.getAllFrames !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.webNavigation.getAllFrames unavailable; extension is missing webNavigation permission"
    );
  }

  let navigationFrames;
  try {
    navigationFrames = await chrome.webNavigation.getAllFrames({ tabId: selected.tabId });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.webNavigation.getAllFrames(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  if (!Array.isArray(navigationFrames)) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.webNavigation.getAllFrames(${selected.tabId}) returned non-array frame data`
    );
  }

  const frameMetadata = await tabFrameMetadata(selected.tabId);
  const targetId = state.target_id || selected.target.id;
  const built = buildFrameTreeEntries(navigationFrames, frameMetadata.byFrameId, targetId);
  return {
    extension_id: chrome.runtime.id,
    target_id: targetId,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    frame_count: built.frames.length,
    oopif_target_count: built.frames.filter((frame) => frame.is_out_of_process).length,
    attached_frame_target_count: 0,
    frames: built.frames,
    blocked_frame_targets: [],
    frame_snapshot_errors: [...frameMetadata.errors, ...built.errors],
    readback_backend: "chrome.webNavigation.getAllFrames+chrome.scripting.executeScript(frame metadata)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handlePageVitals(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  const pageVitals = await tabPageVitalsState(selected.tabId);
  let viewport = null;
  let viewportErrorDetail = null;
  try {
    viewport = await tabViewportMetricsState(selected.tabId);
  } catch (error) {
    viewportErrorDetail = errorMessage(error);
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    readback_backend: "chrome.tabs.get+chrome.scripting.executeScript",
    viewport,
    viewport_error_detail: viewportErrorDetail,
    page_vitals: pageVitals,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handlePageContent(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const maxBytes = normalizeMaxContentBytes(params.maxBytes);
  const state = await tabPageState(selected.tabId, selected.target);
  const content = await tabPageContentState(selected.tabId, maxBytes);
  if (!content.available) {
    const code = String(content.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED);
    throw bridgeError(
      code,
      `pageContent read failed for tab ${selected.tabId}: code=${String(content.error_code || "UNKNOWN")} ` +
        `detail=${String(content.error_detail || "")}`
    );
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || content.url || "",
    title: state.title || content.title || "",
    ready_state: content.ready_state || state.ready_state || "",
    history_current_index: Number.isSafeInteger(content.history_current_index) ? content.history_current_index : -1,
    history_entry_count: Number.isSafeInteger(content.history_entry_count) ? content.history_entry_count : 0,
    html: content.html || "",
    html_len: Number.isSafeInteger(content.html_len) ? content.html_len : 0,
    truncated: Boolean(content.truncated),
    max_bytes: maxBytes,
    readback_backend: "chrome.scripting.executeScript(document.documentElement.outerHTML)",
    frame_id: content.frame_id,
    frame_document_id: content.frame_document_id,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleCookies(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  if (!chrome.cookies || typeof chrome.cookies.getAll !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.cookies API unavailable; extension is missing cookies permission"
    );
  }

  const operation = normalizeCookieOperation(params.operation);
  const url = normalizeCookieUrl(params.url, state.url, operation);
  let cookies = [];
  let affected = [];
  let verificationCookie = null;
  let failed = [];

  if (operation === "get") {
    cookies = await chrome.cookies.getAll(cookieGetAllDetails(params, url));
  } else if (operation === "set") {
    const setDetails = cookieSetDetails(params, url);
    try {
      const cookie = await chrome.cookies.set(setDetails);
      if (!cookie) {
        throw new Error("chrome.cookies.set returned null");
      }
      affected.push(serializeCookie(cookie));
      verificationCookie = await chrome.cookies.get({
        url: setDetails.url,
        name: setDetails.name,
        storeId: cookie.storeId
      });
      cookies = verificationCookie ? [verificationCookie] : [];
    } catch (error) {
      throw bridgeError(ERROR_AXTREE_FAILED, `chrome.cookies.set failed: ${errorMessage(error)}`);
    }
  } else if (operation === "clear") {
    const details = cookieGetAllDetails(params, url);
    const candidates = await chrome.cookies.getAll(details);
    for (const cookie of candidates) {
      try {
        const removed = await chrome.cookies.remove({
          url: cookieRemovalUrl(cookie, url),
          name: cookie.name,
          storeId: cookie.storeId
        });
        if (removed) {
          affected.push(serializeCookie(cookie));
        }
      } catch (error) {
        failed.push({
          name: String(cookie?.name || ""),
          domain: String(cookie?.domain || ""),
          path: String(cookie?.path || ""),
          error_detail: errorMessage(error)
        });
      }
    }
    cookies = await chrome.cookies.getAll(details);
  }

  const serializedCookies = cookies.map(serializeCookie);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: url || state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    operation,
    ok: failed.length === 0,
    cookie_count: serializedCookies.length,
    affected_count: affected.length,
    cookies: serializedCookies,
    affected_cookies: affected,
    verification_cookie: verificationCookie ? serializeCookie(verificationCookie) : null,
    failures: failed,
    readback_backend: "chrome.cookies+chrome.tabs.get",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleStorageState(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }

  const operation = normalizeStorageOperation(params.operation);
  const store = normalizeStorageStore(params.store);
  const includeSessionStorage = Boolean(params.includeSessionStorage);
  let result = null;
  let frameResults = [];
  let cookieResult = null;

  if (operation === "save_state") {
    const cookieUrl = normalizeCookieUrl(params.url, state.url, "get");
    const cookies = chrome.cookies && typeof chrome.cookies.getAll === "function"
      ? await chrome.cookies.getAll(cookieGetAllDetails(params, cookieUrl))
      : [];
    const injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId, allFrames: true },
      func: readStorageStateInPage,
      args: [includeSessionStorage]
    });
    frameResults = frameExecutionResults(injected);
    result = buildStorageStateFromFrameResults(frameResults, cookies);
  } else if (operation === "load_state") {
    const stateValue = plainObjectOrEmpty(params.state);
    const cookies = Array.isArray(stateValue.cookies) ? stateValue.cookies : [];
    const cookieFailures = [];
    const cookieAffected = [];
    if (cookies.length > 0) {
      if (!chrome.cookies || typeof chrome.cookies.set !== "function") {
        throw bridgeError(
          ERROR_AXTREE_FAILED,
          "chrome.cookies API unavailable; extension is missing cookies permission"
        );
      }
      for (const cookie of cookies) {
        try {
          const setDetails = cookieSetDetailsFromState(cookie);
          const setCookie = await chrome.cookies.set(setDetails);
          if (!setCookie) {
            throw new Error("chrome.cookies.set returned null");
          }
          cookieAffected.push(serializeCookie(setCookie));
        } catch (error) {
          cookieFailures.push({
            name: String(cookie?.name || ""),
            domain: String(cookie?.domain || ""),
            path: String(cookie?.path || ""),
            error_detail: errorMessage(error)
          });
        }
      }
    }
    const injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId },
      func: applyStorageStateInPage,
      args: [stateValue, includeSessionStorage, Boolean(params.clearBeforeLoad)]
    });
    frameResults = frameExecutionResults(injected);
    const first = frameResults.find((frame) => frame.result) || null;
    result = first?.result || null;
    cookieResult = {
      affected_count: cookieAffected.length,
      affected_cookies: cookieAffected,
      failure_count: cookieFailures.length,
      failures: cookieFailures
    };
    if (!result || typeof result !== "object") {
      throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "storageState load returned no structured result");
    }
    if (!result.ok) {
      throw bridgeError(
        String(result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
        `storageState load failed: ${String(result.error_detail || "")}`
      );
    }
    result.cookie_result = cookieResult;
    result.ok = result.ok && cookieFailures.length === 0;
  } else {
    const injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId },
      func: runStorageCommandInPage,
      args: [{
        operation,
        store,
        key: params.key == null ? null : String(params.key),
        value: params.value === undefined ? null : params.value
      }]
    });
    frameResults = frameExecutionResults(injected);
    const first = frameResults.find((frame) => frame.result) || null;
    result = first?.result || null;
    if (!result || typeof result !== "object") {
      throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "storageState command returned no structured result");
    }
    if (!result.ok) {
      throw bridgeError(
        String(result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
        `storageState command failed: ${String(result.error_detail || "")}`
      );
    }
  }

  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    operation,
    store,
    include_session_storage: includeSessionStorage,
    ok: Boolean(result?.ok ?? true),
    result,
    storage_state: operation === "save_state" ? result.storage_state : null,
    readback_backend: "chrome.scripting.executeScript(storageState)+chrome.cookies",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_result_count: frameResults.length,
    frame_results: frameResults.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleSetContent(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const html = String(params.html ?? "");
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const agentSessionId = normalizeOptionalSessionId(params.agentSessionId);
  if (html.length === 0) {
    throw bridgeError(ERROR_AXTREE_FAILED, "setContent requires non-empty html");
  }
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  const before = await tabPageState(selected.tabId, selected.target);
  const execution = await executeSetContentScript(selected, html, waitTimeoutMs, agentSessionId, before);
  const { injected, seed } = execution;
  const frameResults = frameExecutionResults(injected);
  const first = frameResults.find((frame) => frame.result) || null;
  const result = first?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(ERROR_AXTREE_FAILED, "chrome.scripting.executeScript setContent returned no structured result");
  }
  if (!result.ok) {
    throw bridgeError(
      String(result.error_code || ERROR_AXTREE_FAILED),
      `setContent failed: ${String(result.error_detail || "")}`
    );
  }
  const after = await waitForTabPageState(
    selected.tabId,
    selected.target,
    waitTimeoutMs,
    {
      description: "document.readyState complete after setContent",
      matches: (state) => state.ready_state === "complete"
    }
  );
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    before_url: before.url || "",
    before_title: before.title || "",
    after_url: after.url || result.url || "",
    after_title: after.title || result.title || "",
    ready_state: result.ready_state || after.ready_state || "",
    history_current_index: Number.isSafeInteger(result.history_current_index) ? result.history_current_index : -1,
    history_entry_count: Number.isSafeInteger(result.history_entry_count) ? result.history_entry_count : 0,
    html_len: Number.isSafeInteger(result.html_len) ? result.html_len : html.length,
    seeded_url: seed?.url || "",
    seeded_from_url: seed?.from_url || "",
    seeded_reason: seed?.reason || "",
    readback_backend: "chrome.scripting.executeScript(document.open/write/close)+chrome.tabs.get",
    backend_tier_used: "chrome_tabs_extension",
    frame_id: Number.isSafeInteger(first.frame_id) ? first.frame_id : null,
    frame_document_id: first.document_id,
    frame_result_count: frameResults.length,
    frame_results: frameResults.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function executeSetContentScript(selected, html, waitTimeoutMs, agentSessionId, before) {
  try {
    const injected = await runSetContentScript(selected.tabId, html, waitTimeoutMs);
    return { injected, seed: null };
  } catch (error) {
    if (!isScriptingAccessDeniedError(error)) {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `chrome.scripting.executeScript setContent(${selected.tabId}) failed: ${errorMessage(error)}`
      );
    }
    const seed = await seedTabForSetContent(selected, waitTimeoutMs, agentSessionId, before, error);
    try {
      const injected = await runSetContentScript(selected.tabId, html, waitTimeoutMs);
      return { injected, seed };
    } catch (retryError) {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `chrome.scripting.executeScript setContent(${selected.tabId}) failed after seed navigation ` +
          `url=${JSON.stringify(seed.url)} original=${errorMessage(error)} retry=${errorMessage(retryError)}`
      );
    }
  }
}

async function runSetContentScript(tabId, html, waitTimeoutMs) {
  return chrome.scripting.executeScript({
    target: { tabId },
    world: "MAIN",
    func: setContentInPage,
    args: [{ html, waitTimeoutMs }]
  });
}

async function seedTabForSetContent(selected, waitTimeoutMs, agentSessionId, before, cause) {
  const seedUrl = setContentSeedUrl(selected.tabId);
  markAgentNavigation(selected.tabId, {
    action: "setContentSeed",
    requestedUrl: seedUrl,
    sessionId: agentSessionId
  });
  try {
    await chrome.tabs.update(selected.tabId, { url: seedUrl });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.tabs.update(${selected.tabId}) seed for setContent failed: ${errorMessage(error)}; ` +
        `original executeScript failure=${errorMessage(cause)}`
    );
  }
  await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs, {
    description: `setContent seed url ${JSON.stringify(seedUrl)}`,
    matches: (state) => state.url === seedUrl || state.url.startsWith(`${seedUrl}#`)
  });
  return {
    url: seedUrl,
    from_url: before?.url || "",
    reason: errorMessage(cause)
  };
}

function setContentSeedUrl(tabId) {
  const nonce = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  return `${DAEMON_BASE_URL}${SET_CONTENT_SEED_PATH}?tab=${encodeURIComponent(String(tabId))}&nonce=${encodeURIComponent(nonce)}`;
}

function isScriptingAccessDeniedError(error) {
  const message = errorMessage(error);
  return (
    message.includes("Cannot access contents of url") ||
    message.includes("Extension manifest must request permission") ||
    message.includes("Cannot access a chrome:// URL") ||
    message.includes("Cannot access chrome://") ||
    message.includes("The extensions gallery cannot be scripted")
  );
}

async function handleAriaSnapshot(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  const maxNodes = normalizeAriaMaxNodes(params.maxNodes);
  const maxDepth = normalizeAriaMaxDepth(params.maxDepth);
  const root = parseChromeBridgeElementId(params.rootElementId, selected.tabId, "ariaSnapshot");
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: root.frameId === null ? { tabId: selected.tabId } : { tabId: selected.tabId, frameIds: [root.frameId] },
      func: runAriaSnapshotInPage,
      args: [{ rootPath: root.path, maxNodes, maxDepth }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.scripting.executeScript ariaSnapshot(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(results);
  const selectedFrame =
    frames.find((frame) => frame.result?.ok) ||
    frames.find((frame) => frame.result) ||
    null;
  const result = selectedFrame?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(ERROR_AXTREE_FAILED, "chrome.scripting.executeScript ariaSnapshot returned no structured result");
  }
  if (!result.ok) {
    throw bridgeError(
      String(result.error_code || ERROR_AXTREE_FAILED),
      `ariaSnapshot failed: ${String(result.error_detail || "")}`
    );
  }
  const frameId = Number.isSafeInteger(selectedFrame.frame_id) ? selectedFrame.frame_id : 0;
  const nodes = Array.isArray(result.nodes)
    ? result.nodes.map((node) => prefixAriaSnapshotNode(node, selected.tabId, frameId))
    : [];
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: result.url || state.url || "",
    title: result.title || state.title || "",
    ready_state: result.ready_state || state.ready_state || "",
    root_element_id: root.raw || null,
    snapshot: renderPrefixedAriaSnapshot(nodes),
    nodes,
    node_count: nodes.length,
    total_ax_nodes: Number.isSafeInteger(result.total_ax_nodes) ? result.total_ax_nodes : nodes.length,
    max_nodes: maxNodes,
    max_depth: maxDepth,
    truncated_by_max_nodes: Boolean(result.truncated_by_max_nodes),
    truncated_by_depth: Boolean(result.truncated_by_depth),
    frame_tree_frame_count: 1,
    attached_frame_target_count: 0,
    blocked_frame_targets: [],
    frame_snapshot_errors: [],
    readback_backend: "chrome.scripting.executeScript(debugger-free DOM/ARIA snapshot)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_id: frameId,
    frame_document_id: selectedFrame.document_id,
    frame_result_count: frames.length,
    frame_results: frames.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleAssertPoll(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  const locator = plainObjectOrEmpty(params.locator);
  const limit = normalizeAssertLimit(params.limit);
  const root = parseChromeBridgeElementId(locator.root_element_id, selected.tabId, "assertPoll");
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: root.frameId === null ? { tabId: selected.tabId, allFrames: true } : { tabId: selected.tabId, frameIds: [root.frameId] },
      func: runAssertPollInPage,
      args: [{ locator, limit, rootPath: root.path }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript assertPoll(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(results);
  const resultFrames = frames.filter((frame) => frame.result && typeof frame.result === "object");
  if (resultFrames.length === 0) {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript assertPoll returned no structured result");
  }
  const failingFrame = resultFrames.find((frame) => frame.result && frame.result.ok === false);
  if (failingFrame) {
    throw bridgeError(
      String(failingFrame.result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `assertPoll failed: ${String(failingFrame.result.error_detail || "")}`
    );
  }
  const matchCount = resultFrames.reduce((sum, frame) => {
    const value = frame.result?.match_count;
    return sum + (Number.isSafeInteger(value) && value > 0 ? value : 0);
  }, 0);
  const elementFrame = resultFrames.find((frame) => frame.result?.element_state) || null;
  const elementState = elementFrame?.result?.element_state || null;
  const elementId = elementFrame && elementFrame.result?.local_element_id
    ? chromeBridgeElementId(selected.tabId, Number.isSafeInteger(elementFrame.frame_id) ? elementFrame.frame_id : 0, String(elementFrame.result.local_element_id))
    : null;
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: elementFrame?.result?.url || state.url || "",
    title: elementFrame?.result?.title || state.title || "",
    ready_state: elementFrame?.result?.ready_state || state.ready_state || "",
    match_count: matchCount,
    element_id: elementId,
    state: elementState,
    readback_backend: "chrome.scripting.executeScript(debugger-free DOM assertion poll)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_result_count: frames.length,
    frame_results: frames.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleLocateElements(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = await tabPageState(selected.tabId, selected.target);
  const locator = plainObjectOrEmpty(params.locator);
  const limit = normalizeAssertLimit(params.limit);
  const root = parseChromeBridgeElementId(locator.root_element_id, selected.tabId, "locateElements");
  const frameScope = await resolveBridgeLocatorFrameScope(selected, locator, root, "locateElements");
  if (!frameScope.resolved) {
    return {
      extension_id: chrome.runtime.id,
      target_id: state.target_id || selected.target.id,
      tab_id: selected.tabId,
      chrome_window_id: state.chrome_window_id,
      url: state.url || "",
      title: state.title || "",
      ready_state: state.ready_state || "",
      engine: String(locator.engine || "css").toLowerCase(),
      query: String(locator.query || ""),
      match_count: 0,
      returned_count: 0,
      truncated: false,
      element_ids: [],
      frame: frameScope.frame,
      readback_backend: "chrome.webNavigation.getAllFrames frame locator",
      backend_tier_used: "chrome_tabs_extension",
      required_foreground: false,
      frame_result_count: frameScope.frame_result_count || 0,
      frame_results: frameScope.frame_results || [],
      target_candidate_count: selected.targetCandidateCount,
      target_selection_reason: selected.selectionReason
    };
  }
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: frameScope.target,
      func: runAssertPollInPage,
      args: [{ locator, limit, rootPath: root.path }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript locateElements(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(results);
  const resultFrames = frames.filter((frame) => frame.result && typeof frame.result === "object");
  if (resultFrames.length === 0) {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript locateElements returned no structured result");
  }
  const failingFrame = resultFrames.find((frame) => frame.result && frame.result.ok === false);
  if (failingFrame) {
    throw bridgeError(
      String(failingFrame.result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `locateElements failed: ${String(failingFrame.result.error_detail || "")}`
    );
  }
  const matchCount = resultFrames.reduce((sum, frame) => {
    const value = frame.result?.match_count;
    return sum + (Number.isSafeInteger(value) && value > 0 ? value : 0);
  }, 0);
  if (locator.strict === true && !Number.isSafeInteger(locator.nth) && matchCount > 1) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `strict mode: ${String(locator.engine || "css")} selector ${JSON.stringify(locator.query || "")} resolved to ${matchCount} elements; refine the query, set nth, or disable strict`
    );
  }
  const elementIds = [];
  for (const frame of resultFrames) {
    if (!Number.isSafeInteger(frame.frame_id)) {
      continue;
    }
    const localIds = Array.isArray(frame.result?.local_element_ids)
      ? frame.result.local_element_ids
      : (frame.result?.local_element_id ? [frame.result.local_element_id] : []);
    for (const localId of localIds) {
      if (elementIds.length >= limit) {
        break;
      }
      elementIds.push(chromeBridgeElementId(selected.tabId, frame.frame_id, String(localId)));
    }
    if (elementIds.length >= limit) {
      break;
    }
  }
  const nthSet = Number.isSafeInteger(locator.nth);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    engine: String(locator.engine || "css").toLowerCase(),
    query: String(locator.query || ""),
    match_count: matchCount,
    returned_count: elementIds.length,
    truncated: !nthSet && matchCount > elementIds.length,
    element_ids: elementIds,
    frame: frameScope.frame,
    readback_backend: frameScope.frame
      ? "chrome.webNavigation.getAllFrames frame locator + chrome.scripting.executeScript(debugger-free DOM locator)"
      : "chrome.scripting.executeScript(debugger-free DOM locator)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_result_count: frames.length,
    frame_results: frames.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleInspectElement(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const parsed = parseChromeBridgeElementId(params.elementId, selected.tabId, "inspectElement");
  if (parsed.frameId === null || !parsed.path) {
    throw bridgeError(ERROR_ATTACH_FAILED, "inspectElement requires a normal bridge element id with frame and path");
  }
  const maxHtmlBytes = normalizeInspectMaxHtmlBytes(params.maxHtmlBytes);
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId, frameIds: [parsed.frameId] },
      func: runBridgeElementCommandInPage,
      args: [{ operation: "inspect", elementPath: parsed.path, maxHtmlBytes }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript inspectElement(${selected.tabId}, frame=${parsed.frameId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(injected);
  const frame = frames.find((item) => item.result) || null;
  const result = frame?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript inspectElement returned no structured result");
  }
  if (!result.ok) {
    throw bridgeError(
      String(result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `inspectElement failed: ${String(result.error_detail || "")}`
    );
  }
  const state = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    element_id: params.elementId,
    url: result.url || state.url || "",
    title: result.title || state.title || "",
    ready_state: result.ready_state || state.ready_state || "",
    element: result.element,
    readback_backend: "chrome.scripting.executeScript(debugger-free DOM inspect + elementFromPoint)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_id: parsed.frameId,
    frame_document_id: frame?.document_id,
    frame_result_count: frames.length,
    frame_results: frames.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleScrollIntoView(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const parsed = parseChromeBridgeElementId(params.elementId, selected.tabId, "scrollIntoView");
  if (parsed.frameId === null || !parsed.path) {
    throw bridgeError(ERROR_ATTACH_FAILED, "scrollIntoView requires a normal bridge element id with frame and path");
  }
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId, frameIds: [parsed.frameId] },
      func: runBridgeElementCommandInPage,
      args: [{ operation: "scroll", elementPath: parsed.path }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript scrollIntoView(${selected.tabId}, frame=${parsed.frameId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(injected);
  const frame = frames.find((item) => item.result) || null;
  const result = frame?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript scrollIntoView returned no structured result");
  }
  if (!result.ok) {
    throw bridgeError(
      String(result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `scrollIntoView failed: ${String(result.error_detail || "")}`
    );
  }
  const state = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    element_id: params.elementId,
    url: result.url || state.url || "",
    title: result.title || state.title || "",
    ready_state: result.ready_state || state.ready_state || "",
    scroll: {
      target_id: state.target_id || selected.target.id,
      element_id: params.elementId,
      ...result.scroll
    },
    readback_backend: "chrome.scripting.executeScript(debugger-free Element.scrollIntoView + geometry readback)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_id: parsed.frameId,
    frame_document_id: frame?.document_id,
    frame_result_count: frames.length,
    frame_results: frames.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleWaitForText(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = normalizeWaitForTextState(params.state);
  const expectedText = String(params.text ?? "");
  const timeoutMs = normalizeWaitTimeout(params.timeoutMs);
  const pollingIntervalMs = normalizePollingInterval(params.pollingIntervalMs);
  if (state !== "timeout" && expectedText.length === 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, "waitForText text must be non-empty unless state=timeout");
  }
  if (state === "timeout" && expectedText.length > 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, "waitForText state=timeout does not accept text");
  }
  const startedAt = Date.now();
  let pollCount = 0;
  let lastTextLen = 0;
  let lastPageText = null;
  while (true) {
    if (state === "timeout") {
      await sleep(timeoutMs);
      lastPageText = await tabPageTextState(selected.tabId);
      lastTextLen = Number.isSafeInteger(lastPageText?.text_len) ? lastPageText.text_len : 0;
      break;
    }
    pollCount += 1;
    lastPageText = await tabPageTextState(selected.tabId);
    if (!lastPageText.available) {
      throw bridgeError(
        String(lastPageText.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
        `waitForText page text read failed: ${String(lastPageText.error_detail || "")}`
      );
    }
    const pageText = String(lastPageText.text || "");
    lastTextLen = Number.isSafeInteger(lastPageText.text_len) ? lastPageText.text_len : pageText.length;
    const contains = pageText.includes(expectedText);
    const conditionMet = state === "text_gone" ? !contains : contains;
    if (conditionMet) {
      break;
    }
    const elapsed = elapsedSince(startedAt);
    if (elapsed >= timeoutMs) {
      const stateNow = await tabPageState(selected.tabId, selected.target);
      return waitForTextResult(selected, stateNow, lastPageText, state, expectedText, false, true, elapsed, timeoutMs, pollingIntervalMs, pollCount, lastTextLen);
    }
    await sleep(Math.min(pollingIntervalMs, Math.max(1, timeoutMs - elapsed)));
  }
  const pageState = await tabPageState(selected.tabId, selected.target);
  return waitForTextResult(
    selected,
    pageState,
    lastPageText,
    state,
    expectedText,
    true,
    false,
    elapsedSince(startedAt),
    timeoutMs,
    pollingIntervalMs,
    pollCount,
    lastTextLen
  );
}

async function handleWaitForFunction(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const expression = String(params.expression ?? "");
  if (expression.trim().length === 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, "waitForFunction expression must be non-empty");
  }
  const args = Array.isArray(params.args) ? params.args : [];
  const timeoutMs = normalizeWaitTimeout(params.timeoutMs);
  const pollingIntervalMs = normalizePollingInterval(params.pollingIntervalMs);
  const startedAt = Date.now();
  let pollCount = 0;
  let last = null;
  while (true) {
    pollCount += 1;
    const poll = await waitForFunctionProbe(selected, expression, args);
    last = poll;
    if (poll.condition_met) {
      const pageState = await tabPageState(selected.tabId, selected.target);
      return waitForFunctionResult(
        selected,
        pageState,
        poll,
        true,
        false,
        elapsedSince(startedAt),
        timeoutMs,
        pollingIntervalMs,
        pollCount,
        expression,
        args
      );
    }
    const elapsed = elapsedSince(startedAt);
    if (elapsed >= timeoutMs) {
      const pageState = await tabPageState(selected.tabId, selected.target);
      return waitForFunctionResult(
        selected,
        pageState,
        last,
        false,
        true,
        elapsed,
        timeoutMs,
        pollingIntervalMs,
        pollCount,
        expression,
        args
      );
    }
    await sleep(Math.min(pollingIntervalMs, Math.max(1, timeoutMs - elapsed)));
  }
}

async function handleWaitForLoadState(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = normalizeWaitForLoadStateState(params.state);
  const timeoutMs = normalizeWaitTimeout(params.timeoutMs);
  const pollingIntervalMs = normalizePollingInterval(params.pollingIntervalMs);
  const startedAt = Date.now();
  const initialState = await tabPageState(selected.tabId, selected.target);
  const { buffer } = ensurePageEventBuffer(selected.tabId, initialState);
  let pollCount = 0;
  let last = null;
  while (true) {
    pollCount += 1;
    const pageState = await tabPageState(selected.tabId, selected.target);
    updatePageSnapshot(buffer, pageState);
    const events = loadStateEventSummary(buffer, startedAt);
    const probe = await loadStateProbe(selected);
    last = loadStatePollSummary(pageState, probe, events);
    const conditionMet = loadStateConditionMet(state, last);
    const elapsed = elapsedSince(startedAt);
    if (conditionMet) {
      return waitForLoadStateResult(
        selected,
        state,
        last,
        true,
        false,
        elapsed,
        timeoutMs,
        pollingIntervalMs,
        pollCount
      );
    }
    if (elapsed >= timeoutMs) {
      return waitForLoadStateResult(
        selected,
        state,
        last,
        false,
        true,
        elapsed,
        timeoutMs,
        pollingIntervalMs,
        pollCount
      );
    }
    await sleep(Math.min(pollingIntervalMs, Math.max(1, timeoutMs - elapsed)));
  }
}

async function handleWaitForUrl(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const urlPattern = String(params.url ?? "");
  if (urlPattern.trim().length === 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, "waitForUrl url must be non-empty");
  }
  const matchKind = normalizeWaitForUrlMatchKind(params.matchKind);
  const timeoutMs = normalizeWaitTimeout(params.timeoutMs);
  const pollingIntervalMs = normalizePollingInterval(params.pollingIntervalMs);
  const startedAt = Date.now();
  const initialState = await tabPageState(selected.tabId, selected.target);
  const { buffer } = ensurePageEventBuffer(selected.tabId, initialState);
  let pollCount = 0;
  let last = null;
  while (true) {
    pollCount += 1;
    const pageState = await tabPageState(selected.tabId, selected.target);
    updatePageSnapshot(buffer, pageState);
    const navigationEventCount = urlNavigationEventCount(buffer, startedAt);
    const conditionMet = waitForUrlMatches(pageState.url, urlPattern, matchKind);
    const elapsed = elapsedSince(startedAt);
    if (conditionMet) {
      return waitForUrlResult(
        selected,
        pageState,
        urlPattern,
        matchKind,
        true,
        false,
        elapsed,
        timeoutMs,
        pollingIntervalMs,
        pollCount,
        navigationEventCount
      );
    }
    if (elapsed >= timeoutMs) {
      return waitForUrlResult(
        selected,
        pageState,
        urlPattern,
        matchKind,
        false,
        true,
        elapsed,
        timeoutMs,
        pollingIntervalMs,
        pollCount,
        navigationEventCount
      );
    }
    await sleep(Math.min(pollingIntervalMs, Math.max(1, timeoutMs - elapsed)));
  }
}

async function handleWaitForNetwork(params, requireResponse) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const wait = normalizeNetworkWaitParams(params, requireResponse);
  const state = await tabPageState(selected.tabId, selected.target);
  const { buffer } = ensureNetworkEventBuffer(selected.tabId, state);
  const recorder = await ensureInPageNetworkRecorder(selected.tabId);
  const startSeq = buffer.nextSeq;
  const startedAt = Date.now();
  let pollCount = 0;
  while (true) {
    pollCount += 1;
    await mergeInPageNetworkEvents(selected.tabId, buffer, recorder);
    const matched = findNetworkWaitEntry(buffer, wait, requireResponse, startSeq);
    const elapsed = elapsedSince(startedAt);
    if (matched) {
      return waitForNetworkResult(
        selected,
        wait,
        requireResponse,
        true,
        false,
        elapsed,
        pollCount,
        matched,
        buffer
      );
    }
    if (elapsed >= wait.timeoutMs) {
      return waitForNetworkResult(
        selected,
        wait,
        requireResponse,
        false,
        true,
        elapsed,
        pollCount,
        null,
        buffer
      );
    }
    await sleep(Math.min(wait.pollingIntervalMs, Math.max(1, wait.timeoutMs - elapsed)));
  }
}

async function handleWaitForSelector(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const state = normalizeWaitForSelectorState(params.state);
  const timeoutMs = normalizeWaitTimeout(params.timeoutMs);
  const pollingIntervalMs = normalizePollingInterval(params.pollingIntervalMs);
  const locator = plainObjectOrEmpty(params.locator);
  const limit = normalizeAssertLimit(params.limit);
  const root = parseChromeBridgeElementId(locator.root_element_id, selected.tabId, "waitForSelector");
  const startedAt = Date.now();
  let pollCount = 0;
  let last = null;
  while (true) {
    pollCount += 1;
    const poll = await waitForSelectorPoll(selected, locator, limit, root, state);
    last = poll;
    if (poll.condition_met) {
      const pageState = await tabPageState(selected.tabId, selected.target);
      return waitForSelectorResult(selected, pageState, locator, state, true, false, elapsedSince(startedAt), timeoutMs, pollingIntervalMs, pollCount, poll);
    }
    const elapsed = elapsedSince(startedAt);
    if (elapsed >= timeoutMs) {
      const pageState = await tabPageState(selected.tabId, selected.target);
      return waitForSelectorResult(selected, pageState, locator, state, false, true, elapsed, timeoutMs, pollingIntervalMs, pollCount, last);
    }
    await sleep(Math.min(pollingIntervalMs, Math.max(1, timeoutMs - elapsed)));
  }
}

async function handleClock(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeClockOperation(params.operation);
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  const state = await tabPageState(selected.tabId, selected.target);
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId },
      world: "MAIN",
      func: runClockInPage,
      args: [{
        operation,
        timeMs: params.timeMs ?? null,
        deltaMs: params.deltaMs ?? null,
        loopLimit: 10000
      }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.scripting.executeScript clock(${selected.tabId}, ${operation}) failed: ${errorMessage(error)}`
    );
  }
  const frameResults = frameExecutionResults(injected);
  const first = frameResults.find((frame) => frame.result) || null;
  const result = first?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(ERROR_AXTREE_FAILED, "chrome.scripting.executeScript clock returned no structured result");
  }
  if (!result.ok) {
    throw bridgeError(
      String(result.error_code || ERROR_AXTREE_FAILED),
      `clock failed: ${String(result.error_detail || "")}`
    );
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    operation,
    init_script_identifier: result.init_script_identifier || "chrome.scripting.executeScript:MAIN:synapse-clock",
    init_script_newly_added: Boolean(result.init_script_newly_added),
    installed_at_unix_ms: Number.isSafeInteger(result.installed_at_unix_ms) ? result.installed_at_unix_ms : 0,
    readback: result.readback || {},
    url: result.url || state.url || "",
    title: result.title || state.title || "",
    ready_state: result.ready_state || state.ready_state || "",
    readback_backend: "chrome.scripting.executeScript(MAIN synapse clock shim)",
    backend_tier_used: "chrome_tabs_extension",
    frame_id: Number.isSafeInteger(first.frame_id) ? first.frame_id : null,
    frame_document_id: first.document_id,
    frame_result_count: frameResults.length,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handlePageEvents(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const filters = normalizePageEventsFilter(params);
  const state = await tabPageState(selected.tabId, selected.target);
  const bufferResult = ensurePageEventBuffer(selected.tabId, state);
  const workerProbe = await drainPageWorkerEvents(selected.tabId, state);
  const read = readPageEventBuffer(bufferResult.buffer, filters);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    capture_newly_armed: bufferResult.newlyArmed,
    armed_at_unix_ms: bufferResult.buffer.armedAtUnixMs,
    capacity: bufferResult.buffer.capacity,
    next_cursor: read.next_cursor,
    returned: read.returned,
    total_buffered: read.total_buffered,
    dropped: read.dropped,
    filters: {
      since_seq: filters.sinceSeq,
      limit: filters.limit,
      event_kind: filters.eventKind,
      worker_type: filters.workerType
    },
    entries: read.entries,
    pages: read.pages,
    workers: read.workers,
    readback_backend: "chrome.webNavigation+chrome.tabs+chrome.scripting.executeScript(MAIN synapse page-events shim)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    web_navigation_available: Boolean(chrome.webNavigation),
    worker_probe_available: workerProbe.available,
    worker_probe_error_code: workerProbe.error_code || null,
    worker_probe_error_detail: workerProbe.error_detail || null,
    worker_probe_frame_result_count: workerProbe.frame_result_count || 0,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function drainPageWorkerEvents(tabId, state) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return {
      available: false,
      error_code: "CHROME_SCRIPTING_UNAVAILABLE",
      error_detail: "Chrome scripting API is unavailable; extension is missing scripting permission",
      frame_result_count: 0
    };
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      world: "MAIN",
      func: runPageEventsWorkerProbe
    });
  } catch (error) {
    return {
      available: false,
      error_code: ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      error_detail: errorMessage(error),
      frame_result_count: 0
    };
  }
  const frames = frameExecutionResults(injected);
  for (const frame of frames) {
    const result = frame.result;
    if (!result || typeof result !== "object" || !Array.isArray(result.events)) {
      continue;
    }
    for (const event of result.events) {
      pushPageEvent(tabId, {
        ...event,
        target_id: state.target_id || targetIdForTabId(tabId),
        frame_id: event.frame_id == null && Number.isSafeInteger(frame.frame_id)
          ? String(frame.frame_id)
          : event.frame_id,
        url: event.url || result.url || state.url || "",
        title: event.title || result.title || state.title || ""
      });
    }
  }
  return {
    available: true,
    error_code: null,
    error_detail: null,
    frame_result_count: frames.length
  };
}

async function handleCapturePageScreenshot(params) {
  return rejectAttachCommand("capturePageScreenshot", params);
}

async function handlePageScreenshot(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const request = normalizePageScreenshotRequest(params, selected.tabId);
  const commandStartedAtMs = Date.now();
  const commandDeadlineMs =
    commandStartedAtMs + Math.min(request.waitTimeoutMs, PAGE_SCREENSHOT_COMMAND_RESPONSE_BUDGET_MS);
  const before = await tabPageState(selected.tabId, selected.target);
  if (!Number.isInteger(before.chrome_window_id)) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `pageScreenshot could not resolve a Chrome window id for tab ${selected.tabId}`
    );
  }
  const beforePageText = await tabPageTextState(selected.tabId);
  const previousActive = Number.isInteger(before.chrome_window_id)
    ? await activeTabForWindow(before.chrome_window_id)
    : null;
  const previousActiveTabId = Number.isInteger(previousActive?.id) ? previousActive.id : null;
  const beforeWindow = await chromeWindowState(before.chrome_window_id);
  const previousFocusedWindow = await focusedChromeWindow();
  const previousFocusedWindowId = Number.isInteger(previousFocusedWindow?.id)
    ? previousFocusedWindow.id
    : null;
  let activeForCapture = Boolean(before.active);
  let focusedWindowForCapture = Boolean(beforeWindow?.focused);
  let restoredPreviousWindowFocus = focusedWindowForCapture;
  let requiredForeground = !focusedWindowForCapture;
  let restoredPreviousActive = false;
  let setup = null;
  const token = `synapse-page-screenshot-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  const tiles = [];
  const captureAttempts = [];
  try {
    setup = await runPageScreenshotSetup(selected.tabId, {
      ...request,
      token
    });
    if (!setup.ok) {
      throw bridgeError(
        String(setup.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
        `pageScreenshot setup failed: ${String(setup.error_detail || "")}`
      );
    }
    if (!before.active) {
      await chrome.tabs.update(selected.tabId, { active: true });
      await waitForTabActiveState(
        selected.tabId,
        true,
        remainingPageScreenshotBudgetMs(commandDeadlineMs, "activate_tab")
      );
      activeForCapture = true;
    }
    if (!focusedWindowForCapture) {
      await chrome.windows.update(before.chrome_window_id, { focused: true });
      await waitForChromeWindowFocused(
        before.chrome_window_id,
        true,
        remainingPageScreenshotBudgetMs(commandDeadlineMs, "focus_window")
      );
      focusedWindowForCapture = true;
      restoredPreviousWindowFocus = false;
    }
    const positions = pageScreenshotTilePositions(setup.clip_css, setup.metrics);
    for (const [index, position] of positions.entries()) {
      remainingPageScreenshotBudgetMs(commandDeadlineMs, `scroll_tile_${index + 1}`);
      const scrollReadback = await runPageScreenshotScroll(selected.tabId, position);
      remainingPageScreenshotBudgetMs(commandDeadlineMs, `delay_tile_${index + 1}`);
      await sleep(request.captureDelayMs);
      let imageDataUrl;
      const captureStartedAtMs = Date.now();
      const attempt = {
        attempt: index + 1,
        scroll_x_css: scrollReadback.scroll_x,
        scroll_y_css: scrollReadback.scroll_y,
        requested_scroll_x_css: position.x,
        requested_scroll_y_css: position.y,
        ok: false,
        elapsed_ms: 0,
        timeout_ms: 0,
        retryable: false,
        image_data_url_len: 0,
        error_detail: ""
      };
      try {
        const captureTimeoutMs = remainingPageScreenshotBudgetMs(
          commandDeadlineMs,
          `capture_visible_tab_tile_${index + 1}`
        );
        attempt.timeout_ms = captureTimeoutMs;
        imageDataUrl = await captureVisibleTabWithQuota(before.chrome_window_id, {
          format: request.format,
          quality: request.quality
        }, captureTimeoutMs, {
          tabId: selected.tabId,
          windowId: before.chrome_window_id,
          targetId: selected.target.id,
          tileIndex: index + 1,
          tileCount: positions.length,
          requestedScrollX: position.x,
          requestedScrollY: position.y
        });
        attempt.ok = true;
        attempt.image_data_url_len = String(imageDataUrl || "").length;
      } catch (error) {
        attempt.error_detail = errorMessage(error);
        attempt.retryable = error?.code === ERROR_EXTENSION_TIMEOUT;
        throw bridgeError(
          error?.code === ERROR_EXTENSION_TIMEOUT ? ERROR_EXTENSION_TIMEOUT : ERROR_ATTACH_FAILED,
          `pageScreenshot chrome.tabs.captureVisibleTab(window=${before.chrome_window_id}) failed: ${errorMessage(error)}`
        );
      } finally {
        attempt.elapsed_ms = Date.now() - captureStartedAtMs;
        captureAttempts.push(attempt);
      }
      if (typeof imageDataUrl !== "string" || !imageDataUrl.startsWith("data:image/")) {
        throw bridgeError(
          ERROR_AXTREE_FAILED,
          `pageScreenshot captureVisibleTab returned unsupported payload for tab ${selected.tabId}`
        );
      }
      tiles.push({
        scroll_x_css: scrollReadback.scroll_x,
        scroll_y_css: scrollReadback.scroll_y,
        viewport_width_css: scrollReadback.viewport_width,
        viewport_height_css: scrollReadback.viewport_height,
        image_data_url: imageDataUrl,
        image_data_url_len: imageDataUrl.length
      });
    }
  } finally {
    if (setup?.ok) {
      try {
        await runPageScreenshotCleanup(selected.tabId, {
          token,
          scrollX: setup.before_scroll_x,
          scrollY: setup.before_scroll_y,
          backgroundRestore: setup.background_restore
        });
      } catch (error) {
        console.warn(`Synapse pageScreenshot cleanup failed for tab ${selected.tabId}: ${errorMessage(error)}`);
      }
    }
    if (
      previousActiveTabId !== null &&
      previousActiveTabId !== selected.tabId &&
      activeForCapture
    ) {
      try {
        await chrome.tabs.update(previousActiveTabId, { active: true });
        await waitForTabActiveState(previousActiveTabId, true, 2000);
        restoredPreviousActive = true;
      } catch (error) {
        console.warn(`Synapse pageScreenshot active-tab restore failed for tab ${previousActiveTabId}: ${errorMessage(error)}`);
      }
    } else if (before.active) {
      restoredPreviousActive = true;
    }
    if (
      previousFocusedWindowId !== null &&
      previousFocusedWindowId !== before.chrome_window_id &&
      focusedWindowForCapture
    ) {
      try {
        await chrome.windows.update(previousFocusedWindowId, { focused: true });
        await waitForChromeWindowFocused(previousFocusedWindowId, true, 2000);
        restoredPreviousWindowFocus = true;
      } catch (error) {
        console.warn(`Synapse pageScreenshot Chrome window focus restore failed for window ${previousFocusedWindowId}: ${errorMessage(error)}`);
      }
    } else if (beforeWindow?.focused) {
      restoredPreviousWindowFocus = true;
    }
  }
  const after = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    url: after.url || before.url || "",
    title: after.title || before.title || "",
    ready_state: after.ready_state || before.ready_state || "",
    before_page: before,
    after_page: after,
    before_page_text: beforePageText,
    before_active: Boolean(before.active),
    active_for_capture: activeForCapture,
    previous_active_tab_id: previousActiveTabId,
    restored_previous_active: restoredPreviousActive,
    before_window: beforeWindow,
    previous_focused_window_id: previousFocusedWindowId,
    focused_window_for_capture: focusedWindowForCapture,
    restored_previous_window_focus: restoredPreviousWindowFocus,
    image_format: request.format,
    quality: request.quality,
    omit_background: request.omitBackground,
    scope: request.scope,
    clip_css: setup.clip_css,
    output_css_width: setup.clip_css.w,
    output_css_height: setup.clip_css.h,
    device_pixel_ratio: setup.metrics.device_pixel_ratio,
    scroll_width_css: setup.metrics.scroll_width,
    scroll_height_css: setup.metrics.scroll_height,
    viewport_width_css: setup.metrics.viewport_width,
    viewport_height_css: setup.metrics.viewport_height,
    tile_count: tiles.length,
    tiles,
    capture_attempt_count: captureAttempts.length,
    capture_attempts: captureAttempts,
    mask_count: setup.mask_count,
    masks: setup.masks,
    readback_backend: "chrome.scripting.executeScript(page metrics/masks/scroll) + chrome.tabs.captureVisibleTab",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: requiredForeground,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function normalizePageScreenshotRequest(params, selectedTabId) {
  const scope = normalizePageScreenshotScope(params.scope);
  const clip = normalizePageScreenshotClip(params.clip);
  const elementId = stringOrNull(params.elementId);
  const parsedElement = elementId
    ? parseChromeBridgeElementId(elementId, selectedTabId, "pageScreenshot")
    : { raw: null, frameId: null, path: null };
  if (parsedElement.frameId !== null && parsedElement.frameId !== 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `pageScreenshot element screenshots currently support top-frame element ids only; got frame_id=${parsedElement.frameId}`
    );
  }
  if (scope === "element" && !parsedElement.path) {
    throw bridgeError(ERROR_ATTACH_FAILED, "pageScreenshot scope=element requires elementId");
  }
  if (scope !== "element" && elementId) {
    throw bridgeError(ERROR_ATTACH_FAILED, "pageScreenshot elementId is only valid with scope=element");
  }
  if (scope === "clip" && !clip) {
    throw bridgeError(ERROR_ATTACH_FAILED, "pageScreenshot scope=clip requires clip");
  }
  if (scope !== "clip" && clip) {
    throw bridgeError(ERROR_ATTACH_FAILED, "pageScreenshot clip is only valid with scope=clip");
  }
  return {
    scope,
    clip,
    elementPath: parsedElement.path,
    format: normalizePageScreenshotFormat(params.format),
    quality: normalizePageScreenshotQuality(params.quality),
    omitBackground: Boolean(params.omitBackground),
    masks: normalizePageScreenshotMasks(params.masks, selectedTabId),
    waitTimeoutMs: normalizeWaitTimeout(params.waitTimeoutMs),
    captureDelayMs: normalizePageScreenshotDelay(params.captureDelayMs)
  };
}

function normalizePageScreenshotScope(value) {
  const scope = String(value || "viewport").trim().toLowerCase();
  if (["viewport", "full_page", "clip", "element"].includes(scope)) {
    return scope;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `pageScreenshot scope must be viewport, full_page, clip, or element; got ${JSON.stringify(value)}`
  );
}

function normalizePageScreenshotFormat(value) {
  const format = String(value || "png").trim().toLowerCase();
  if (format === "png" || format === "jpeg") {
    return format;
  }
  if (format === "jpg") {
    return "jpeg";
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `pageScreenshot format must be png or jpeg; got ${JSON.stringify(value)}`
  );
}

function normalizePageScreenshotQuality(value) {
  if (value === undefined || value === null) {
    return 90;
  }
  const quality = Number(value);
  if (!Number.isSafeInteger(quality) || quality < 0 || quality > 100) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `pageScreenshot quality must be an integer in 0..=100; got ${JSON.stringify(value)}`
    );
  }
  return quality;
}

function normalizePageScreenshotDelay(value) {
  if (value === undefined || value === null) {
    return 120;
  }
  const delay = Number(value);
  if (!Number.isSafeInteger(delay) || delay < 0 || delay > 2000) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `pageScreenshot captureDelayMs must be an integer in 0..=2000; got ${JSON.stringify(value)}`
    );
  }
  return delay;
}

function normalizePageScreenshotClip(value) {
  if (value === undefined || value === null) {
    return null;
  }
  const x = Number(value.x);
  const y = Number(value.y);
  const w = Number(value.w ?? value.width);
  const h = Number(value.h ?? value.height);
  for (const [field, number] of [["x", x], ["y", y], ["w", w], ["h", h]]) {
    if (!Number.isFinite(number)) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `pageScreenshot clip.${field} must be finite; got ${JSON.stringify(value[field])}`
      );
    }
  }
  if (x < 0 || y < 0 || w <= 0 || h <= 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `pageScreenshot clip must be non-negative and non-empty; got ${JSON.stringify(value)}`
    );
  }
  return { x, y, w, h };
}

function normalizePageScreenshotMasks(value, selectedTabId) {
  if (value === undefined || value === null) {
    return [];
  }
  if (!Array.isArray(value)) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "pageScreenshot masks must be an array");
  }
  if (value.length > 32) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "pageScreenshot supports at most 32 masks");
  }
  return value.map((mask, index) => {
    if (!mask || typeof mask !== "object" || Array.isArray(mask)) {
      throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, `pageScreenshot mask[${index}] must be an object`);
    }
    const selector = stringOrNull(mask.selector);
    const elementId = stringOrNull(mask.elementId);
    if ((selector ? 1 : 0) + (elementId ? 1 : 0) !== 1) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `pageScreenshot mask[${index}] requires exactly one of selector or elementId`
      );
    }
    let elementPath = null;
    if (elementId) {
      const parsed = parseChromeBridgeElementId(elementId, selectedTabId, "pageScreenshot mask");
      if (parsed.frameId !== 0) {
        throw bridgeError(
          ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
          `pageScreenshot mask[${index}] elementId must be in the top frame; got frame_id=${parsed.frameId}`
        );
      }
      elementPath = parsed.path;
    }
    const color = stringOrNull(mask.color) || "#ff00ff";
    if (color.length > 128 || /[\u0000-\u001f]/.test(color)) {
      throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, `pageScreenshot mask[${index}] color is invalid`);
    }
    return { selector, elementPath, color };
  });
}

async function runPageScreenshotSetup(tabId, request) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId },
      func: pageScreenshotSetupInPage,
      args: [request]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript pageScreenshot setup(${tabId}) failed: ${errorMessage(error)}`
    );
  }
  const first = Array.isArray(injected) ? injected[0] : null;
  if (!first || !first.result || typeof first.result !== "object") {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "pageScreenshot setup returned no structured result");
  }
  return first.result;
}

async function runPageScreenshotScroll(tabId, position) {
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId },
      func: pageScreenshotScrollInPage,
      args: [position]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript pageScreenshot scroll(${tabId}) failed: ${errorMessage(error)}`
    );
  }
  const first = Array.isArray(injected) ? injected[0] : null;
  if (!first || !first.result || typeof first.result !== "object") {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "pageScreenshot scroll returned no structured result");
  }
  if (!first.result.ok) {
    throw bridgeError(
      String(first.result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `pageScreenshot scroll failed: ${String(first.result.error_detail || "")}`
    );
  }
  return first.result;
}

async function runPageScreenshotCleanup(tabId, request) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return null;
  }
  const injected = await chrome.scripting.executeScript({
    target: { tabId },
    func: pageScreenshotCleanupInPage,
    args: [request]
  });
  return Array.isArray(injected) ? injected[0]?.result : null;
}

async function waitForTabActiveState(tabId, expected, waitTimeoutMs) {
  const started = Date.now();
  let last = null;
  while (Date.now() - started <= waitTimeoutMs) {
    last = await tabPageState(tabId, null);
    if (Boolean(last.active) === expected) {
      return last;
    }
    await sleep(50);
  }
  throw bridgeError(
    ERROR_EXTENSION_TIMEOUT,
    `tab ${tabId} active=${expected} readback did not settle within ${waitTimeoutMs}ms; last_active=${Boolean(last?.active)}`
  );
}

function pageScreenshotTilePositions(clip, metrics) {
  const viewportW = Math.max(1, Number(metrics.viewport_width || 1));
  const viewportH = Math.max(1, Number(metrics.viewport_height || 1));
  const left = Number(clip.x);
  const top = Number(clip.y);
  const right = left + Number(clip.w);
  const bottom = top + Number(clip.h);
  const maxScrollX = Math.max(0, Number(metrics.scroll_width || viewportW) - viewportW);
  const maxScrollY = Math.max(0, Number(metrics.scroll_height || viewportH) - viewportH);
  const xs = [];
  const ys = [];
  for (let x = left; x < right; x += viewportW) {
    xs.push(Math.min(Math.max(0, x), maxScrollX));
  }
  for (let y = top; y < bottom; y += viewportH) {
    ys.push(Math.min(Math.max(0, y), maxScrollY));
  }
  if (xs.length === 0) {
    xs.push(Math.min(Math.max(0, left), maxScrollX));
  }
  if (ys.length === 0) {
    ys.push(Math.min(Math.max(0, top), maxScrollY));
  }
  const uniqueXs = [...new Set(xs.map((x) => Math.round(x * 1000) / 1000))];
  const uniqueYs = [...new Set(ys.map((y) => Math.round(y * 1000) / 1000))];
  const positions = [];
  for (const y of uniqueYs) {
    for (const x of uniqueXs) {
      positions.push({ x, y });
    }
  }
  if (positions.length > 400) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `pageScreenshot would require ${positions.length} tiles; limit is 400`
    );
  }
  return positions;
}

function remainingPageScreenshotBudgetMs(deadlineMs, phase) {
  const remaining = Math.floor(Number(deadlineMs) - Date.now());
  if (!Number.isFinite(remaining) || remaining <= 0) {
    throw bridgeError(
      ERROR_EXTENSION_TIMEOUT,
      `pageScreenshot ${phase} exceeded extension response budget before daemon command timeout`
    );
  }
  return remaining;
}

function pageScreenshotSetupInPage(request) {
  const fail = (code, detail) => ({ ok: false, error_code: code, error_detail: detail });
  const finite = (value) => Number.isFinite(Number(value));
  const rectToPlain = (rect) => ({
    x: Number(rect.left + window.scrollX),
    y: Number(rect.top + window.scrollY),
    w: Number(rect.width),
    h: Number(rect.height),
    viewport_x: Number(rect.left),
    viewport_y: Number(rect.top)
  });
  const metrics = () => {
    const doc = document.documentElement;
    const body = document.body;
    const scrollWidth = Math.max(
      doc ? doc.scrollWidth : 0,
      body ? body.scrollWidth : 0,
      window.innerWidth || 0
    );
    const scrollHeight = Math.max(
      doc ? doc.scrollHeight : 0,
      body ? body.scrollHeight : 0,
      window.innerHeight || 0
    );
    return {
      scroll_width: Number(scrollWidth),
      scroll_height: Number(scrollHeight),
      viewport_width: Number(window.innerWidth || 0),
      viewport_height: Number(window.innerHeight || 0),
      device_pixel_ratio: Number(window.devicePixelRatio || 1),
      scroll_x: Number(window.scrollX || 0),
      scroll_y: Number(window.scrollY || 0)
    };
  };
  const elementByPath = (path) => {
    if (!path) {
      return null;
    }
    // Shadow-aware: "s" token re-enters the current element's open shadowRoot.
    const parts = String(path).split(".");
    let node = document.documentElement;
    if (Number(parts[0]) !== 0) {
      return null;
    }
    if (parts.length === 1) {
      return node;
    }
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      let scope = node;
      if (enterShadow) {
        if (!node || !node.shadowRoot) {
          return null;
        }
        scope = node.shadowRoot;
        enterShadow = false;
      }
      if (!scope || !scope.children || !Number.isSafeInteger(index) || index < 0 || index >= scope.children.length) {
        return null;
      }
      node = scope.children[index];
    }
    return node;
  };
  try {
    const beforeScrollX = Number(window.scrollX || 0);
    const beforeScrollY = Number(window.scrollY || 0);
    let clip = null;
    let element_rect = null;
    if (request.scope === "viewport") {
      const m = metrics();
      clip = { x: m.scroll_x, y: m.scroll_y, w: m.viewport_width, h: m.viewport_height };
    } else if (request.scope === "full_page") {
      const m = metrics();
      clip = { x: 0, y: 0, w: m.scroll_width, h: m.scroll_height };
    } else if (request.scope === "clip") {
      clip = request.clip;
    } else if (request.scope === "element") {
      const element = elementByPath(request.elementPath);
      if (!element) {
        return fail("CHROME_DOM_ELEMENT_NOT_FOUND", `element path ${JSON.stringify(request.elementPath)} was not found`);
      }
      try {
        element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      } catch (_) {
        element.scrollIntoView();
      }
      const rect = element.getBoundingClientRect();
      element_rect = rectToPlain(rect);
      clip = { x: element_rect.x, y: element_rect.y, w: element_rect.w, h: element_rect.h };
    }
    if (!clip || !finite(clip.x) || !finite(clip.y) || !finite(clip.w) || !finite(clip.h) || clip.w <= 0 || clip.h <= 0) {
      return fail("CAPTURE_TARGET_INVALID", `pageScreenshot resolved an empty/invalid clip ${JSON.stringify(clip)}`);
    }
    const token = String(request.token || "");
    const masks = [];
    for (const [index, mask] of Array.from(request.masks || []).entries()) {
      const target = mask.elementPath ? elementByPath(mask.elementPath) : document.querySelector(String(mask.selector || ""));
      if (!target) {
        return fail("CHROME_DOM_ELEMENT_NOT_FOUND", `mask[${index}] target was not found`);
      }
      const rect = target.getBoundingClientRect();
      if (!rect || rect.width <= 0 || rect.height <= 0) {
        return fail("CAPTURE_TARGET_INVALID", `mask[${index}] target has an empty box`);
      }
      const plain = rectToPlain(rect);
      const overlay = document.createElement("div");
      overlay.setAttribute("data-synapse-page-screenshot-mask", token);
      overlay.style.position = "absolute";
      overlay.style.left = `${plain.x}px`;
      overlay.style.top = `${plain.y}px`;
      overlay.style.width = `${plain.w}px`;
      overlay.style.height = `${plain.h}px`;
      overlay.style.background = String(mask.color || "#ff00ff");
      overlay.style.zIndex = "2147483647";
      overlay.style.pointerEvents = "none";
      overlay.style.margin = "0";
      overlay.style.padding = "0";
      overlay.style.border = "0";
      document.documentElement.appendChild(overlay);
      masks.push({ index, selector: mask.selector || null, element_path: mask.elementPath || null, color: String(mask.color || "#ff00ff"), rect: plain });
    }
    let backgroundRestore = null;
    if (request.omitBackground) {
      const doc = document.documentElement;
      const body = document.body;
      backgroundRestore = {
        html_background: doc ? doc.style.background : null,
        body_background: body ? body.style.background : null,
        html_background_color: doc ? doc.style.backgroundColor : null,
        body_background_color: body ? body.style.backgroundColor : null
      };
      if (doc) {
        doc.style.background = "transparent";
        doc.style.backgroundColor = "transparent";
      }
      if (body) {
        body.style.background = "transparent";
        body.style.backgroundColor = "transparent";
      }
    }
    return {
      ok: true,
      clip_css: {
        x: Number(clip.x),
        y: Number(clip.y),
        w: Number(clip.w),
        h: Number(clip.h)
      },
      element_rect,
      metrics: metrics(),
      before_scroll_x: beforeScrollX,
      before_scroll_y: beforeScrollY,
      mask_count: masks.length,
      masks,
      background_restore: backgroundRestore
    };
  } catch (error) {
    return fail("CHROME_SCRIPTING_EXECUTE_FAILED", String(error && error.message || error));
  }
}

function pageScreenshotScrollInPage(position) {
  try {
    window.scrollTo(Number(position.x || 0), Number(position.y || 0));
    return {
      ok: true,
      scroll_x: Number(window.scrollX || 0),
      scroll_y: Number(window.scrollY || 0),
      viewport_width: Number(window.innerWidth || 0),
      viewport_height: Number(window.innerHeight || 0),
      device_pixel_ratio: Number(window.devicePixelRatio || 1)
    };
  } catch (error) {
    return {
      ok: false,
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: String(error && error.message || error)
    };
  }
}

function pageScreenshotCleanupInPage(request) {
  const token = String(request.token || "");
  let removed = 0;
  if (token) {
    for (const node of Array.from(document.querySelectorAll("[data-synapse-page-screenshot-mask]"))) {
      if (node.getAttribute("data-synapse-page-screenshot-mask") === token) {
        node.remove();
        removed += 1;
      }
    }
  }
  const restore = request.backgroundRestore;
  if (restore) {
    if (document.documentElement) {
      document.documentElement.style.background = restore.html_background || "";
      document.documentElement.style.backgroundColor = restore.html_background_color || "";
    }
    if (document.body) {
      document.body.style.background = restore.body_background || "";
      document.body.style.backgroundColor = restore.body_background_color || "";
    }
  }
  window.scrollTo(Number(request.scrollX || 0), Number(request.scrollY || 0));
  return {
    ok: true,
    removed,
    scroll_x: Number(window.scrollX || 0),
    scroll_y: Number(window.scrollY || 0)
  };
}

function recordDownloadEvent(eventKind, item, delta) {
  const entry = downloadEntryFromItem(item || {});
  downloadEventSeq += 1;
  downloadEventBuffer.push({
    seq: downloadEventSeq,
    event_kind: String(eventKind || "unknown"),
    timestamp_unix_ms: Date.now(),
    download_id: entry.id,
    url: entry.url,
    final_url: entry.final_url,
    filename: entry.filename,
    filename_basename: entry.filename_basename,
    state: entry.state,
    danger: entry.danger,
    error: entry.error,
    bytes_received: entry.bytes_received,
    total_bytes: entry.total_bytes,
    file_size: entry.file_size,
    delta: downloadDeltaToPlain(delta)
  });
  while (downloadEventBuffer.length > MAX_DOWNLOAD_EVENT_BUFFER) {
    downloadEventBuffer.shift();
  }
}

function downloadDeltaToPlain(delta) {
  if (!delta || typeof delta !== "object") {
    return null;
  }
  const out = {};
  for (const key of ["id", "url", "finalUrl", "filename", "state", "danger", "mime", "startTime", "endTime", "error", "paused", "canResume", "exists"]) {
    if (Object.prototype.hasOwnProperty.call(delta, key)) {
      const value = delta[key];
      out[key] = value && typeof value === "object" && Object.prototype.hasOwnProperty.call(value, "current")
        ? value.current
        : value;
    }
  }
  for (const key of ["bytesReceived", "totalBytes", "fileSize"]) {
    if (Object.prototype.hasOwnProperty.call(delta, key)) {
      const value = delta[key];
      const raw = value && typeof value === "object" && Object.prototype.hasOwnProperty.call(value, "current")
        ? value.current
        : value;
      out[key] = Number(raw || 0);
    }
  }
  return out;
}

async function handleDownloads(params) {
  ensureDownloadsApiAvailable();
  const request = normalizeDownloadsRequest(params);
  const startedAt = Date.now();
  let items = await queryMatchingDownloads(request);
  let selected = selectDownloadItem(items, request);
  let timedOut = false;
  let conditionMet = request.operation === "list" || downloadItemSatisfiesWait(selected, request);

  if (request.operation !== "list" && !conditionMet) {
    const deadline = startedAt + request.waitTimeoutMs;
    while (Date.now() < deadline) {
      await sleep(Math.min(250, Math.max(25, deadline - Date.now())));
      items = await queryMatchingDownloads(request);
      selected = selectDownloadItem(items, request);
      conditionMet = downloadItemSatisfiesWait(selected, request);
      if (conditionMet) {
        break;
      }
    }
    if (!conditionMet) {
      timedOut = true;
    }
  }

  const elapsedMs = Math.max(0, Date.now() - startedAt);
  const events = matchingDownloadEvents(request);
  return {
    extension_id: chrome.runtime.id,
    operation: request.operation,
    items,
    selected_item: selected || null,
    returned: items.length,
    event_count: events.length,
    next_event_cursor: downloadEventSeq + 1,
    events,
    condition_met: conditionMet,
    timed_out: timedOut,
    elapsed_ms: elapsedMs,
    timeout_ms: request.operation === "list" ? 0 : request.waitTimeoutMs,
    readback_backend: "chrome.downloads",
    backend_tier_used: "chrome_downloads_api",
    required_foreground: false
  };
}

function ensureDownloadsApiAvailable() {
  if (!chrome.downloads || typeof chrome.downloads.search !== "function") {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      "chrome.downloads.search unavailable; extension is missing the downloads permission"
    );
  }
}

function normalizeDownloadsRequest(params) {
  const operation = normalizeDownloadsOperation(params.operation);
  const request = {
    operation,
    downloadId: normalizeOptionalDownloadId(params.downloadId ?? params.download_id),
    urlContains: normalizeOptionalDownloadString(params.urlContains ?? params.url_contains, "urlContains", 2048),
    filenameContains: normalizeOptionalDownloadString(params.filenameContains ?? params.filename_contains, "filenameContains", 1024),
    mimeContains: normalizeOptionalDownloadString(params.mimeContains ?? params.mime_contains, "mimeContains", 256),
    state: normalizeOptionalDownloadState(params.state),
    sinceUnixMs: normalizeOptionalUnixMs(params.sinceUnixMs ?? params.since_unix_ms),
    limit: normalizeDownloadsLimit(params.limit),
    waitTimeoutMs: normalizeDownloadsTimeout(params.waitTimeoutMs ?? params.wait_timeout_ms),
    sinceEventSeq: normalizeOptionalEventSeq(params.sinceEventSeq ?? params.since_event_seq)
  };
  if (operation === "save" || operation === "move") {
    request.state = "complete";
  }
  return request;
}

function normalizeDownloadsOperation(value) {
  const raw = value === null || value === undefined ? "list" : String(value).trim().toLowerCase();
  if (["list", "wait", "save", "move"].includes(raw)) {
    return raw;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `downloads operation must be list, wait, save, or move; got ${JSON.stringify(value)}`);
}

function normalizeOptionalDownloadId(value) {
  if (value === null || value === undefined) {
    return null;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads downloadId must be a non-negative integer; got ${JSON.stringify(value)}`);
  }
  return number;
}

function normalizeOptionalDownloadString(value, fieldName, maxLength) {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value !== "string") {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads ${fieldName} must be a string`);
  }
  if (value.length > maxLength) {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads ${fieldName} exceeds ${maxLength} bytes`);
  }
  const trimmed = value.trim();
  return trimmed.length === 0 ? null : trimmed;
}

function normalizeOptionalDownloadState(value) {
  if (value === null || value === undefined) {
    return null;
  }
  const raw = String(value).trim().toLowerCase();
  if (["in_progress", "interrupted", "complete"].includes(raw)) {
    return raw;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `downloads state must be in_progress, interrupted, or complete; got ${JSON.stringify(value)}`);
}

function normalizeOptionalUnixMs(value) {
  if (value === null || value === undefined) {
    return null;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads sinceUnixMs must be a non-negative integer; got ${JSON.stringify(value)}`);
  }
  return number;
}

function normalizeOptionalEventSeq(value) {
  if (value === null || value === undefined) {
    return null;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads sinceEventSeq must be a non-negative integer; got ${JSON.stringify(value)}`);
  }
  return number;
}

function normalizeDownloadsLimit(value) {
  if (value === null || value === undefined) {
    return 50;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > 500) {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads limit must be an integer in 1..=500; got ${JSON.stringify(value)}`);
  }
  return number;
}

function normalizeDownloadsTimeout(value) {
  if (value === null || value === undefined) {
    return 30000;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > 300000) {
    throw bridgeError(ERROR_ATTACH_FAILED, `downloads waitTimeoutMs must be an integer in 1..=300000; got ${JSON.stringify(value)}`);
  }
  return number;
}

async function queryMatchingDownloads(request) {
  const query = {};
  if (request.downloadId !== null) {
    query.id = request.downloadId;
  }
  const items = await chrome.downloads.search(query);
  return Array.from(items || [])
    .map(downloadEntryFromItem)
    .filter((item) => downloadEntryMatchesRequest(item, request))
    .sort(compareDownloadEntriesNewestFirst)
    .slice(0, request.limit);
}

function downloadEntryFromItem(item) {
  const id = Number(item && item.id);
  const filename = String(item && item.filename || "");
  return {
    id: Number.isSafeInteger(id) ? id : -1,
    url: String(item && item.url || ""),
    final_url: String(item && (item.finalUrl || item.final_url) || ""),
    filename,
    filename_basename: filenameBasename(filename),
    mime: String(item && item.mime || ""),
    start_time: String(item && item.startTime || ""),
    end_time: String(item && item.endTime || ""),
    estimated_end_time: String(item && item.estimatedEndTime || ""),
    state: String(item && item.state || ""),
    paused: Boolean(item && item.paused),
    can_resume: Boolean(item && item.canResume),
    danger: String(item && item.danger || ""),
    error: item && item.error ? String(item.error) : null,
    bytes_received: nonNegativeNumber(item && item.bytesReceived),
    total_bytes: signedNumber(item && item.totalBytes),
    file_size: signedNumber(item && item.fileSize),
    exists: item && typeof item.exists === "boolean" ? item.exists : null,
    incognito: Boolean(item && item.incognito),
    referrer: String(item && item.referrer || "")
  };
}

function filenameBasename(filename) {
  const raw = String(filename || "");
  if (!raw) {
    return "";
  }
  const slashIndex = Math.max(raw.lastIndexOf("/"), raw.lastIndexOf("\\"));
  return slashIndex >= 0 ? raw.slice(slashIndex + 1) : raw;
}

function nonNegativeNumber(value) {
  const number = Number(value || 0);
  return Number.isFinite(number) && number > 0 ? number : 0;
}

function signedNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : -1;
}

function downloadEntryMatchesRequest(item, request) {
  if (request.downloadId !== null && item.id !== request.downloadId) {
    return false;
  }
  if (request.state && item.state !== request.state) {
    return false;
  }
  if (request.sinceUnixMs !== null && downloadEntryStartUnixMs(item) < request.sinceUnixMs) {
    return false;
  }
  if (request.urlContains && !downloadFieldContains(`${item.url}\n${item.final_url}\n${item.referrer}`, request.urlContains)) {
    return false;
  }
  if (request.filenameContains && !downloadFieldContains(`${item.filename}\n${item.filename_basename}`, request.filenameContains)) {
    return false;
  }
  if (request.mimeContains && !downloadFieldContains(item.mime, request.mimeContains)) {
    return false;
  }
  return true;
}

function downloadFieldContains(value, needle) {
  return String(value || "").toLowerCase().includes(String(needle || "").toLowerCase());
}

function downloadEntryStartUnixMs(item) {
  const parsed = Date.parse(String(item && item.start_time || ""));
  return Number.isFinite(parsed) ? parsed : 0;
}

function compareDownloadEntriesNewestFirst(a, b) {
  const byStart = downloadEntryStartUnixMs(b) - downloadEntryStartUnixMs(a);
  if (byStart !== 0) {
    return byStart;
  }
  return Number(b.id || 0) - Number(a.id || 0);
}

function selectDownloadItem(items, request) {
  if (!items || items.length === 0) {
    return null;
  }
  if (request.downloadId !== null) {
    return items.find((item) => item.id === request.downloadId) || null;
  }
  return items[0] || null;
}

function downloadItemSatisfiesWait(item, request) {
  if (!item) {
    return false;
  }
  const desiredState = request.state || "complete";
  return item.state === desiredState;
}

function matchingDownloadEvents(request) {
  return downloadEventBuffer
    .filter((event) => request.sinceEventSeq === null || event.seq >= request.sinceEventSeq)
    .filter((event) => {
      const item = {
        id: event.download_id,
        url: event.url,
        final_url: event.final_url,
        filename: event.filename,
        filename_basename: event.filename_basename,
        mime: "",
        start_time: "",
        state: event.state || "",
        referrer: ""
      };
      if (request.downloadId !== null && item.id !== request.downloadId) {
        return false;
      }
      if (request.urlContains && !downloadFieldContains(`${item.url}\n${item.final_url}`, request.urlContains)) {
        return false;
      }
      if (request.filenameContains && !downloadFieldContains(`${item.filename}\n${item.filename_basename}`, request.filenameContains)) {
        return false;
      }
      return true;
    })
    .slice(-request.limit);
}

async function handlePagePdf(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const request = normalizePagePdfRequest(params);
  const before = await tabPageState(selected.tabId, selected.target);
  const printed = await dispatchPagePdf(selected.tabId, request);
  if (typeof printed.data !== "string" || printed.data.length === 0) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `pagePdf Page.printToPDF returned no base64 data for tab ${selected.tabId}`
    );
  }
  const after = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id || "",
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    url: after.url || before.url || "",
    title: after.title || before.title || "",
    data_base64: printed.data,
    data_base64_len: printed.data.length,
    pdf_byte_length: base64ByteLength(printed.data),
    landscape: request.landscape,
    print_background: request.printBackground,
    display_header_footer: request.displayHeaderFooter,
    scale: request.scale,
    paper_width: request.paperWidth,
    paper_height: request.paperHeight,
    margin_top: request.marginTop,
    margin_bottom: request.marginBottom,
    margin_left: request.marginLeft,
    margin_right: request.marginRight,
    page_ranges: request.pageRanges,
    prefer_css_page_size: request.preferCSSPageSize,
    readback_backend: "chrome.debugger.Page.printToPDF",
    backend_tier_used: "chrome_debugger_print_to_pdf",
    protocol_version: printed.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function normalizePagePdfRequest(params) {
  return {
    landscape: normalizePagePdfBoolean(params.landscape, false, "landscape"),
    printBackground: normalizePagePdfBoolean(params.printBackground, false, "printBackground"),
    displayHeaderFooter: normalizePagePdfBoolean(params.displayHeaderFooter, false, "displayHeaderFooter"),
    headerTemplate: normalizePagePdfString(params.headerTemplate, "", "headerTemplate", 8192),
    footerTemplate: normalizePagePdfString(params.footerTemplate, "", "footerTemplate", 8192),
    scale: normalizePagePdfNumber(params.scale, "scale", 1, 0.1, 2),
    paperWidth: normalizePagePdfNumber(params.paperWidth, "paperWidth", 8.5, 0.1, 200),
    paperHeight: normalizePagePdfNumber(params.paperHeight, "paperHeight", 11, 0.1, 200),
    marginTop: normalizePagePdfNumber(params.marginTop, "marginTop", 0.4, 0, 20),
    marginBottom: normalizePagePdfNumber(params.marginBottom, "marginBottom", 0.4, 0, 20),
    marginLeft: normalizePagePdfNumber(params.marginLeft, "marginLeft", 0.4, 0, 20),
    marginRight: normalizePagePdfNumber(params.marginRight, "marginRight", 0.4, 0, 20),
    pageRanges: normalizePagePdfString(params.pageRanges, "", "pageRanges", 1024),
    preferCSSPageSize: normalizePagePdfBoolean(params.preferCSSPageSize, false, "preferCSSPageSize"),
    waitTimeoutMs: normalizePagePdfTimeout(params.waitTimeoutMs)
  };
}

function normalizePagePdfBoolean(value, defaultValue, fieldName) {
  if (value === null || value === undefined) {
    return defaultValue;
  }
  if (typeof value !== "boolean") {
    throw bridgeError(ERROR_ATTACH_FAILED, `pagePdf ${fieldName} must be a boolean`);
  }
  return value;
}

function normalizePagePdfString(value, defaultValue, fieldName, maxLength) {
  if (value === null || value === undefined) {
    return defaultValue;
  }
  if (typeof value !== "string") {
    throw bridgeError(ERROR_ATTACH_FAILED, `pagePdf ${fieldName} must be a string`);
  }
  if (value.length > maxLength) {
    throw bridgeError(ERROR_ATTACH_FAILED, `pagePdf ${fieldName} exceeds ${maxLength} bytes`);
  }
  return value;
}

function normalizePagePdfNumber(value, fieldName, defaultValue, min, max) {
  if (value === null || value === undefined) {
    return defaultValue;
  }
  const number = Number(value);
  if (!Number.isFinite(number) || number < min || number > max) {
    throw bridgeError(ERROR_ATTACH_FAILED, `pagePdf ${fieldName} must be finite and in ${min}..=${max}`);
  }
  return number;
}

function normalizePagePdfTimeout(value) {
  if (value === null || value === undefined) {
    return DEBUGGER_COMMAND_TIMEOUT_MS;
  }
  const number = Number(value);
  if (!Number.isInteger(number) || number < 1 || number > 60000) {
    throw bridgeError(ERROR_ATTACH_FAILED, "pagePdf waitTimeoutMs must be an integer in 1..=60000");
  }
  return number;
}

async function dispatchPagePdf(tabId, request) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    const result = await sendDebuggerCommand(debuggee, "Page.printToPDF", {
      landscape: request.landscape,
      displayHeaderFooter: request.displayHeaderFooter,
      printBackground: request.printBackground,
      scale: request.scale,
      paperWidth: request.paperWidth,
      paperHeight: request.paperHeight,
      marginTop: request.marginTop,
      marginBottom: request.marginBottom,
      marginLeft: request.marginLeft,
      marginRight: request.marginRight,
      pageRanges: request.pageRanges,
      headerTemplate: request.headerTemplate,
      footerTemplate: request.footerTemplate,
      preferCSSPageSize: request.preferCSSPageSize
    }, request.waitTimeoutMs);
    return {
      data: result && result.data,
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `pagePdf chrome.debugger Page.printToPDF failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for pagePdf tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

function base64ByteLength(value) {
  const normalized = String(value || "").replace(/\s+/g, "");
  if (!normalized) {
    return 0;
  }
  const padding = normalized.endsWith("==") ? 2 : normalized.endsWith("=") ? 1 : 0;
  return Math.max(0, Math.floor((normalized.length * 3) / 4) - padding);
}

async function handleTypeActiveElement(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const text = String(params.text ?? "");
  const before = await tabPageState(selected.tabId, selected.target);
  const beforePageText = await tabPageTextState(selected.tabId);
  return await handleCdpInputText(
    selected,
    "type_text",
    {
      ...params,
      activeElement: true,
      text
    },
    before,
    beforePageText,
    5000,
    true,
    5000
  );
}

async function createDomActionPopupTabs(selected, beforeState, actionResult) {
  const intents = [];
  const seen = new Set();
  const pushIntent = (intent, fallbackKind) => {
    if (!intent || typeof intent !== "object") {
      return;
    }
    const kind = String(intent.kind || fallbackKind || "").trim();
    const requestedUrl = String(intent.resolved_url || intent.url || "").trim();
    if (!kind || !requestedUrl || intent.opened === true) {
      return;
    }
    const key = `${kind}\n${requestedUrl}\n${String(intent.target || "")}`;
    if (seen.has(key)) {
      return;
    }
    seen.add(key);
    intents.push({ ...intent, kind, requested_url: requestedUrl });
  };

  const actionReadback = actionResult?.action_readback || {};
  const popupIntents = Array.isArray(actionResult?.popup_intents)
    ? actionResult.popup_intents
    : Array.isArray(actionReadback.popup_intents)
      ? actionReadback.popup_intents
      : [];
  for (const intent of popupIntents) {
    pushIntent(intent, "window_open");
  }
  pushIntent(
    actionResult?.anchor_target_blank_intent || actionReadback.anchor_target_blank_intent,
    "anchor_target_blank"
  );

  const created = [];
  for (const intent of intents) {
    const createParams = {
      url: normalizeOpenUrl(intent.requested_url) || "about:blank",
      active: false,
      openerTabId: selected.tabId
    };
    if (Number.isInteger(beforeState?.chrome_window_id)) {
      createParams.windowId = beforeState.chrome_window_id;
    }
    let tab;
    try {
      tab = await chrome.tabs.create(createParams);
    } catch (error) {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `domAction popup intent create failed kind=${intent.kind} url=${JSON.stringify(createParams.url)} opener_tab_id=${selected.tabId}: ${errorMessage(error)}`
      );
    }
    if (!tab || typeof tab.id !== "number") {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `domAction popup intent create returned no numeric tab id kind=${intent.kind} opener_tab_id=${selected.tabId}`
      );
    }
    const target = await waitForTabTarget(tab.id, 10000);
    const state = await tabPageState(tab.id, target);
    pushPageEvent(selected.tabId, {
      event_kind: "page_created",
      target_id: selected.target.id,
      target_type: "page",
      target_attached: false,
      page_target_id: target.id,
      opener_id: selected.target.id,
      can_access_opener: true,
      url: state.url || target.url || tab.url || createParams.url,
      title: state.title || target.title || tab.title || "",
      observed_at_unix_ms: Date.now()
    });
    created.push({
      kind: intent.kind,
      opener_tab_id: selected.tabId,
      tab_id: tab.id,
      target_id: target.id,
      chrome_window_id: state.chrome_window_id,
      url: state.url || target.url || tab.url || createParams.url,
      title: state.title || target.title || tab.title || "",
      target_attached: Boolean(target.attached),
      active: Boolean(state.active),
      highlighted: Boolean(state.highlighted),
      source_opened_by_browser: Boolean(intent.opened),
      source_was_blocked: intent.opened === false
    });
  }
  return created;
}

async function handleNavigateTab(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const action = normalizeNavigateAction(params.action);
  const requestedUrl = action === "navigate" ? requiredUrl(params.url) : null;
  const agentSessionId = normalizeOptionalSessionId(params.agentSessionId);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const ignoreCache = Boolean(params.ignoreCache);
  const before = await tabPageState(selected.tabId, selected.target);
  let readbackExpectation = null;
  // #1344: a navigate to a URL that triggers a Chrome download leaves the tab on
  // its current URL, so the url-change readback would (wrongly) time out. Snapshot
  // the download-event sequence before the navigate so we can detect a download
  // that this navigate started and report it as a structured outcome instead.
  let downloadSeqBefore = null;
  try {
    if (action === "navigate") {
      markAgentNavigation(selected.tabId, {
        action,
        requestedUrl,
        sessionId: agentSessionId
      });
      downloadSeqBefore = downloadEventSeq;
      await chrome.tabs.update(selected.tabId, { url: requestedUrl });
      if (requestedUrl !== before.url) {
        readbackExpectation = {
          description: `tab url to become ${JSON.stringify(requestedUrl)} or differ from ${JSON.stringify(before.url)}`,
          matches: (state) => state.url === requestedUrl || state.url !== before.url
        };
      }
    } else if (action === "reload") {
      markAgentNavigation(selected.tabId, {
        action,
        requestedUrl: before.url,
        sessionId: agentSessionId
      });
      await chrome.tabs.reload(selected.tabId, { bypassCache: ignoreCache });
    } else if (action === "back") {
      markAgentNavigation(selected.tabId, {
        action,
        requestedUrl: null,
        sessionId: agentSessionId
      });
      await chrome.tabs.goBack(selected.tabId);
      readbackExpectation = {
        description: `tab url to change after chrome.tabs.goBack from ${JSON.stringify(before.url)}`,
        matches: (state) => state.url !== before.url
      };
    } else if (action === "forward") {
      markAgentNavigation(selected.tabId, {
        action,
        requestedUrl: null,
        sessionId: agentSessionId
      });
      await chrome.tabs.goForward(selected.tabId);
      readbackExpectation = {
        description: `tab url to change after chrome.tabs.goForward from ${JSON.stringify(before.url)}`,
        matches: (state) => state.url !== before.url
      };
    }
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.tabs.${tabsNavigationMethod(action)}(${selected.tabId}) failed: ${errorMessage(error)}; ` +
        `before url=${JSON.stringify(before.url)} title=${JSON.stringify(before.title)} ` +
        `status=${JSON.stringify(before.ready_state)}`
    );
  }
  // #1344: for a navigate, race the tab-url readback against a Chrome download
  // started by this navigate. A matching download is a valid outcome, reported
  // structurally instead of a navigation timeout.
  if (action === "navigate") {
    const outcome = await waitForNavigateOrDownload(
      selected.tabId,
      selected.target,
      waitTimeoutMs,
      readbackExpectation,
      downloadSeqBefore,
      requestedUrl,
      before.url
    );
    if (outcome.kind === "download") {
      const dl = outcome.download;
      const after = outcome.state || before;
      return {
        extension_id: chrome.runtime.id,
        target_id: after.target_id || selected.target.id,
        tab_id: selected.tabId,
        action,
        requested_url: requestedUrl,
        before_url: before.url,
        before_title: before.title,
        after_url: after.url,
        after_title: after.title,
        ready_state: after.ready_state,
        history_current_index: -1,
        history_entry_count: 0,
        history_readback_source: "not_available_chrome_tabs",
        readback_backend: "chrome.tabs.get+chrome.downloads",
        navigation_error_text: null,
        is_download: true,
        download_status: dl.state === "complete" ? "download_completed" : "download_started",
        download_id: dl.download_id,
        download_url: dl.url,
        download_final_url: dl.final_url,
        download_filename: dl.filename,
        download_state: dl.state,
        download_match_reason: dl.match_reason,
        target_candidate_count: selected.targetCandidateCount,
        target_selection_reason: selected.selectionReason
      };
    }
    const after = outcome.state;
    return {
      extension_id: chrome.runtime.id,
      target_id: after.target_id || selected.target.id,
      tab_id: selected.tabId,
      action,
      requested_url: requestedUrl,
      before_url: before.url,
      before_title: before.title,
      after_url: after.url,
      after_title: after.title,
      ready_state: after.ready_state,
      history_current_index: -1,
      history_entry_count: 0,
      history_readback_source: "not_available_chrome_tabs",
      readback_backend: "chrome.tabs.get",
      navigation_error_text: null,
      is_download: false,
      target_candidate_count: selected.targetCandidateCount,
      target_selection_reason: selected.selectionReason
    };
  }
  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs, readbackExpectation);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    action,
    requested_url: requestedUrl,
    before_url: before.url,
    before_title: before.title,
    after_url: after.url,
    after_title: after.title,
    ready_state: after.ready_state,
    history_current_index: -1,
    history_entry_count: 0,
    history_readback_source: "not_available_chrome_tabs",
    readback_backend: "chrome.tabs.get",
    navigation_error_text: null,
    is_download: null,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

// #1344: wait for a navigate to either settle on a new tab URL or start a Chrome
// download. Returns {kind:"navigated", state} or {kind:"download", download, state}.
// Fails loud (ERROR_EXTENSION_TIMEOUT) only when NEITHER a tab navigation nor a
// matching download is observed within the budget.
async function waitForNavigateOrDownload(
  tabId,
  fallbackTarget,
  waitTimeoutMs,
  expectation,
  downloadSeqBefore,
  requestedUrl,
  beforeUrl
) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    const download = matchNavigateDownload(downloadSeqBefore, requestedUrl, last, beforeUrl);
    if (download) {
      return { kind: "download", download, state: last };
    }
    try {
      last = await tabPageState(tabId, fallbackTarget);
      lastError = null;
      const loaded = last.ready_state === "complete";
      if (loaded && (!expectation || expectation.matches(last))) {
        return { kind: "navigated", state: last };
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  // Final download check before declaring failure — the download event may have
  // landed in the same tick the budget expired.
  const download = matchNavigateDownload(downloadSeqBefore, requestedUrl, last, beforeUrl);
  if (download) {
    return { kind: "download", download, state: last };
  }
  const detail = last
    ? `waiting for ${expectation?.description || "complete tab state"} or a Chrome download; ` +
      `last url=${JSON.stringify(last.url)} title=${JSON.stringify(last.title)} ` +
      `status=${JSON.stringify(last.ready_state)} targetId=${JSON.stringify(last.target_id)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no tab state readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `tab readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

// #1344: find a download event recorded after `seqBefore` that this navigate most
// likely started. A URL match (item.url/final_url === requestedUrl) is accepted
// unconditionally; otherwise a download "created" after the navigate is accepted
// only when the tab did NOT navigate away (still on beforeUrl), which is exactly
// the timeout-prone case. Returns null when there is no defensible match.
function matchNavigateDownload(seqBefore, requestedUrl, tabState, beforeUrl) {
  if (seqBefore === null || seqBefore === undefined) {
    return null;
  }
  const fresh = downloadEventBuffer.filter((event) => event.seq > seqBefore);
  if (fresh.length === 0) {
    return null;
  }
  const urlMatch = requestedUrl
    ? fresh.find((event) => event.url === requestedUrl || event.final_url === requestedUrl)
    : null;
  if (urlMatch) {
    return downloadMatchResult(urlMatch, "url_match");
  }
  const tabUnchanged = !tabState || tabState.url === beforeUrl;
  if (tabUnchanged) {
    const created = fresh.find((event) => event.event_kind === "created") || fresh[0];
    if (created) {
      return downloadMatchResult(created, "download_created_during_navigate");
    }
  }
  return null;
}

function downloadMatchResult(event, matchReason) {
  return {
    download_id: event.download_id ?? null,
    url: event.url ?? null,
    final_url: event.final_url ?? null,
    filename: event.filename ?? null,
    state: event.state ?? null,
    match_reason: matchReason
  };
}

// Make a specific tab the active tab WITHIN ITS OWN WINDOW without taking the
// OS foreground. chrome.tabs.update({active:true}) selects the tab in its
// window; per the Chrome API it "does not affect whether the window is focused"
// (windows.update handles that, which we deliberately never call). This is the
// background-safe Playwright bringToFront analogue (#1189) that lets agents
// activate a tab for render-dependent capture without seizing the human's
// foreground — eliminating the global SendKeys workaround (#1204).
async function handleActivateTab(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  try {
    await chrome.tabs.update(selected.tabId, { active: true });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.tabs.update(${selected.tabId}, {active:true}) failed: ${errorMessage(error)}; ` +
        `before active=${before.active} url=${JSON.stringify(before.url)}`
    );
  }
  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs, {
    description: "tab to become the active tab in its window (active=true)",
    matches: (state) => state.active === true
  });
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    before_active: before.active,
    active: after.active,
    highlighted: after.highlighted,
    url: after.url,
    title: after.title,
    ready_state: after.ready_state,
    readback_backend: "chrome.tabs.get",
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleDomAction(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const action = normalizeDomAction(params.action);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const autoWait = Boolean(params.autoWait);
  const autoWaitTimeoutMs = normalizeDomActionAutoWaitTimeout(params.autoWaitTimeoutMs, autoWait);
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }

  const before = await tabPageState(selected.tabId, selected.target);
  const beforePageText = await tabPageTextState(selected.tabId);
  const requestedElementId = stringOrNull(params.elementId);
  const bridgeElement = requestedElementId && requestedElementId.startsWith("chrome-tab:")
    ? parseChromeBridgeElementId(requestedElementId, selected.tabId, "domAction")
    : { raw: requestedElementId, frameId: null, path: null };
  const request = {
    action,
    selector: stringOrNull(params.selector),
    elementId: requestedElementId,
    elementPath: bridgeElement.path,
    role: stringOrNull(params.role),
    name: stringOrNull(params.name),
    value: stringOrNull(params.value),
    option: stringOrNull(params.option),
    optionLabel: stringOrNull(params.optionLabel),
    optionIndex: params.optionIndex === null || params.optionIndex === undefined ? null : normalizeInteger(params.optionIndex, "optionIndex"),
    options: action === "select" ? normalizeSelectOptions(params.options) : [],
    eventType: action === "dispatch_event" ? normalizeEventType(params.eventType) : null,
    eventInit: action === "dispatch_event" ? normalizeEventInit(params.eventInit) : null,
    clicks: normalizeClickCount(params.clicks, action),
    button: normalizeMouseButton(params.button, "domAction"),
    modifiers: normalizeClickModifiers(params.modifiers, "domAction"),
    position: normalizeClickPosition(params, "domAction"),
    autoWait,
    autoWaitTimeoutMs,
    maxPageTextChars: MAX_PAGE_TEXT_CHARS
  };
  const resolveTarget = { tabId: selected.tabId };
  if (Number.isSafeInteger(bridgeElement.frameId)) {
    resolveTarget.frameIds = [bridgeElement.frameId];
  } else {
    resolveTarget.allFrames = true;
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: resolveTarget,
      func: performDomActionInPage,
      args: [{ ...request, resolveOnly: true }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript domAction(${selected.tabId}, ${action}) resolve failed: ${errorMessage(error)}`
    );
  }

  const frameResults = frameExecutionResults(injected);
  const okFrames = frameResults.filter((frame) => frame.result && frame.result.ok);
  const totalMatches = frameResults.reduce((sum, frame) => sum + frameResolvedMatchCount(frame), 0);
  const ambiguousFrames = frameResults.filter((frame) =>
    frame.result?.error_code === ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS
  );
  if (okFrames.length > 1 || totalMatches > 1 || ambiguousFrames.length > 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS,
      `domAction ${action} matched ${totalMatches} actionable element(s) across ${frameResults.length} frame(s); ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  const first = okFrames[0] || null;
  if (!first || !first.result || typeof first.result !== "object") {
    const failed = frameResults.find((frame) => frame.result)?.result;
    throw bridgeError(
      String(failed?.error_code || ERROR_CHROME_DOM_ELEMENT_NOT_FOUND),
      `domAction ${action} failed across ${frameResults.length} frame(s): ${String(failed?.error_detail || "no matching frame result")}; ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  if (!Number.isSafeInteger(first.frame_id)) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `domAction ${action} resolved a frame without a targetable frame_id; ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }

  let actionInjected;
  try {
    actionInjected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId, frameIds: [first.frame_id] },
      world: "MAIN",
      func: performDomActionInPage,
      args: [request]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript domAction(${selected.tabId}, ${action}, frame=${first.frame_id}) failed: ${errorMessage(error)}`
    );
  }
  const actionFrameResults = frameExecutionResults(actionInjected);
  const actionFrame = actionFrameResults.find((frame) => frame.result) || null;
  const actionResult = actionFrame?.result;
  if (!actionResult || typeof actionResult !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript domAction returned no structured action result"
    );
  }
  if (!actionResult.ok) {
    throw bridgeError(
      String(actionResult.error_code || ERROR_AXTREE_FAILED),
      `domAction ${action} failed in resolved frame ${first.frame_id}: ${String(actionResult.error_detail || "")}; ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }

  const createdPopupTabs = await createDomActionPopupTabs(selected, before, actionResult);
  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs);
  const afterPageText = await tabPageTextState(selected.tabId);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    action,
    before_page: before,
    after_page: after,
    before_page_text: beforePageText,
    after_page_text: afterPageText,
    created_popup_tabs: createdPopupTabs,
    readback_backend: "chrome.scripting.executeScript+chrome.tabs.get",
    required_foreground: false,
    frame_id: first.frame_id,
    frame_document_id: first.document_id,
    frame_result_count: frameResults.length,
    frame_results: frameResults.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason,
    ...actionResult
  };
}

async function handleCdpInput(params) {
  requireDebuggerApiAvailable("cdpInput", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const action = normalizeCdpInputAction(params.action);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const autoWait = params.autoWait === undefined ? true : Boolean(params.autoWait);
  const autoWaitTimeoutMs = normalizeDomActionAutoWaitTimeout(params.autoWaitTimeoutMs, autoWait);
  const before = await tabPageState(selected.tabId, selected.target);
  const beforePageText = await tabPageTextState(selected.tabId);
  if (action === "drag" || action === "html5_drag" || action === "html5_real_drag") {
    return await handleCdpInputDrag(
      selected,
      action,
      params,
      before,
      beforePageText,
      waitTimeoutMs,
      autoWait,
      autoWaitTimeoutMs
    );
  }
  if (action === "set_text" || action === "type_text") {
    return await handleCdpInputText(
      selected,
      action,
      params,
      before,
      beforePageText,
      waitTimeoutMs,
      autoWait,
      autoWaitTimeoutMs
    );
  }
  let point;
  let frameId = 0;
  let resolved = {
    resolved_by: "viewport_coordinate",
    matched_count: null,
    before_element: null,
    after_element: null,
    auto_wait: false,
    auto_wait_readback: null,
    frame_result_count: 0,
    frame_results: [],
    element_id: null
  };

  const coordinate = normalizeCdpInputCoordinate(params, action);
  if (coordinate) {
    point = { x: coordinate.x, y: coordinate.y };
  } else {
    if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
      throw bridgeError(
        ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
        "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
      );
    }
    const requestedElementId = stringOrNull(params.elementId);
    const bridgeElement = requestedElementId && requestedElementId.startsWith("chrome-tab:")
      ? parseChromeBridgeElementId(requestedElementId, selected.tabId, "cdpInput")
      : { raw: requestedElementId, frameId: null, path: null };
    const request = {
      action,
      selector: stringOrNull(params.selector),
      elementId: requestedElementId,
      elementPath: bridgeElement.path,
      role: stringOrNull(params.role),
      name: stringOrNull(params.name),
      value: stringOrNull(params.value),
      autoWait,
      autoWaitTimeoutMs,
      resolveOnly: true,
      resolveActionability: true,
      maxPageTextChars: MAX_PAGE_TEXT_CHARS
    };
    const resolveTarget = { tabId: selected.tabId };
    if (Number.isSafeInteger(bridgeElement.frameId)) {
      resolveTarget.frameIds = [bridgeElement.frameId];
    } else {
      resolveTarget.allFrames = true;
    }
    let injected;
    try {
      injected = await chrome.scripting.executeScript({
        target: resolveTarget,
        func: performDomActionInPage,
        args: [request]
      });
    } catch (error) {
      throw bridgeError(
        ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
        `chrome.scripting.executeScript cdpInput(${selected.tabId}, ${action}) resolve failed: ${errorMessage(error)}`
      );
    }

    const frameResults = frameExecutionResults(injected);
    const okFrames = frameResults.filter((frame) => frame.result && frame.result.ok);
    const totalMatches = frameResults.reduce((sum, frame) => sum + frameResolvedMatchCount(frame), 0);
    const ambiguousFrames = frameResults.filter((frame) =>
      frame.result?.error_code === ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS
    );
    if (okFrames.length > 1 || totalMatches > 1 || ambiguousFrames.length > 0) {
      throw bridgeError(
        ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS,
        `cdpInput ${action} matched ${totalMatches} actionable element(s) across ${frameResults.length} frame(s); ` +
          `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
      );
    }
    const first = okFrames[0] || null;
    if (!first || !first.result || typeof first.result !== "object") {
      const failed = frameResults.find((frame) => frame.result)?.result;
      throw bridgeError(
        String(failed?.error_code || ERROR_CHROME_DOM_ELEMENT_NOT_FOUND),
        `cdpInput ${action} failed across ${frameResults.length} frame(s): ${String(failed?.error_detail || "no matching frame result")}; ` +
          `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
      );
    }
    if (first.frame_id !== 0) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `cdpInput ${action} currently supports top-frame targets only because chrome.debugger ` +
          `Input coordinates are root-viewport CSS pixels; resolved frame_id=${first.frame_id}. ` +
          `Re-resolve a top-frame element or use a raw-CDP frame-aware target.`
      );
    }
    const actionPoint = first.result.action_point;
    if (!actionPoint || !Number.isFinite(actionPoint.x) || !Number.isFinite(actionPoint.y)) {
      throw bridgeError(
        ERROR_CHROME_DOM_ELEMENT_NOT_ACTIONABLE,
        `cdpInput ${action} resolved element without a finite viewport action point`
      );
    }
    point = { x: actionPoint.x, y: actionPoint.y };
    frameId = first.frame_id;
    resolved = {
      resolved_by: first.result.resolved_by,
      matched_count: first.result.matched_count,
      before_element: first.result.before_element,
      after_element: first.result.after_element,
      auto_wait: first.result.auto_wait,
      auto_wait_readback: first.result.auto_wait_readback,
      frame_result_count: frameResults.length,
      frame_results: frameResults.map(summarizeFrameExecutionResult),
      element_id: requestedElementId
    };
  }

  // tap/click/dblclick deliver real trusted Input.* events through
  // chrome.debugger, which the renderer only processes for the ACTIVE tab
  // (an inactive tab silently drops mousePressed/mouseReleased; the real mouse
  // drag lane shares this constraint and activates the same way). Activate the
  // owned tab in its already-focused Chrome window first; hover does not need
  // it. This is in-Chrome tab activation, not OS foreground theft
  // (required_foreground stays false). #1348 headline #1/#2.
  const needsActivation =
    action === "tap" || action === "click" || action === "dblclick";
  const touchActivation = needsActivation
    ? await activateTabForCdpTouch(selected.tabId, before, action)
    : {
        attempted: false,
        activated: false,
        before_active: before.active,
        after_active: before.active,
        required_foreground: false,
        readback_backend: "not_required_for_hover"
      };
  const dispatch = await dispatchCdpInput(selected.tabId, action, point, {
    button: normalizeMouseButton(params.button, "cdpInput"),
    modifiers: normalizeClickModifiers(params.modifiers, "cdpInput"),
    clickCount: normalizeClickCount(params.clicks)
  });
  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs);
  const afterPageText = await tabPageTextState(selected.tabId);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    action,
    before_page: before,
    after_page: after,
    before_page_text: beforePageText,
    after_page_text: afterPageText,
    readback_backend: "chrome.debugger.Input",
    required_foreground: false,
    frame_id: frameId,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason,
    method: dispatch.method,
    dispatched_events: dispatch.dispatched_events,
    viewport_point: point,
    debugger_protocol_version: dispatch.protocol_version,
    tab_activation_for_touch: touchActivation,
    ...resolved
  };
}

async function handleCdpInputText(
  selected,
  action,
  params,
  before,
  beforePageText,
  waitTimeoutMs,
  autoWait,
  autoWaitTimeoutMs
) {
  const text = String(params.text ?? "");
  if (action === "type_text" && text.length === 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "cdpInput type_text requires non-empty text; use set_text with an explicit editable target to clear or replace text"
    );
  }
  const tabActivation = await activateTabForCdpTouch(selected.tabId, before, action);
  const prepared = await cdpTextInputTargetScript(selected, action, params, {
    operation: "prepare",
    autoWait,
    autoWaitTimeoutMs
  });
  const dispatch = await dispatchCdpTextInput(selected.tabId, action, text);
  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs);
  const afterPageText = await tabPageTextState(selected.tabId);
  const readback = await cdpTextInputTargetScript(selected, action, params, {
    operation: "read",
    autoWait: false,
    autoWaitTimeoutMs: 0
  });
  const afterValue = String(readback.after_value ?? "");
  if (action === "set_text" && afterValue !== text) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_POSTCONDITION_FAILED,
      `cdpInput set_text postcondition failed: requested_len=${text.length} after_len=${afterValue.length} ` +
        `resolved_by=${String(readback.resolved_by || prepared.resolved_by)} frame_id=${String(readback.frame_id ?? prepared.frame_id)}`
    );
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    action,
    before_page: before,
    after_page: after,
    before_page_text: beforePageText,
    after_page_text: afterPageText,
    readback_backend: "chrome.debugger.Input.insertText+chrome.scripting.executeScript(editable readback)",
    required_foreground: false,
    frame_id: readback.frame_id ?? prepared.frame_id,
    frame_document_id: readback.frame_document_id ?? prepared.frame_document_id,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason,
    method: dispatch.method,
    dispatched_events: dispatch.dispatched_events,
    debugger_protocol_version: dispatch.protocol_version,
    tab_activation_for_input: tabActivation,
    viewport_point: prepared.action_point || null,
    resolved_by: readback.resolved_by || prepared.resolved_by,
    match_count: readback.match_count ?? prepared.match_count,
    matched_count: readback.match_count ?? prepared.match_count,
    tag_name: readback.tag_name || prepared.tag_name,
    is_editable: true,
    editable_kind: readback.editable_kind || prepared.editable_kind,
    selection_mode: prepared.selection_mode,
    before_value: prepared.before_value,
    after_value: afterValue,
    expected_value: action === "set_text" ? text : afterValue,
    chars_requested: Array.from(text).length,
    chars_typed: Array.from(text).length,
    before_active_element: prepared.before_active_element,
    after_active_element: readback.after_active_element,
    before_element: prepared.before_element,
    after_element: readback.after_element,
    auto_wait: prepared.auto_wait,
    auto_wait_readback: prepared.auto_wait_readback,
    frame_result_count: prepared.frame_result_count,
    frame_results: prepared.frame_results
  };
}

async function cdpTextInputTargetInPage(request) {
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const ERROR_ELEMENT_AMBIGUOUS = "CHROME_DOM_ELEMENT_AMBIGUOUS";
  const ERROR_ELEMENT_NOT_ACTIONABLE = "CHROME_DOM_ELEMENT_NOT_ACTIONABLE";
  const ERROR_ACTION_UNSUPPORTED = "CHROME_DOM_ACTION_UNSUPPORTED";
  const ERROR_BROWSER_WAIT_TIMEOUT = "BROWSER_WAIT_TIMEOUT";

  const action = String(request?.action || "").trim().toLowerCase();
  const operation = String(request?.operation || "read").trim().toLowerCase();
  const selector = request?.selector == null ? "" : String(request.selector).trim();
  const elementPath = request?.elementPath == null ? "" : String(request.elementPath).trim();
  const wantActive = Boolean(request?.activeElement);
  const autoWait = Boolean(request?.autoWait);
  const autoWaitTimeoutMs = Number.isSafeInteger(request?.autoWaitTimeoutMs)
    ? Math.max(50, Math.min(request.autoWaitTimeoutMs, 30000))
    : 2000;
  const locatorCount = (selector ? 1 : 0) + (elementPath ? 1 : 0) + (wantActive ? 1 : 0);
  if (!["set_text", "type_text"].includes(action)) {
    return fail(ERROR_ACTION_UNSUPPORTED, `unsupported cdp text action ${JSON.stringify(action)}`);
  }
  if (!["prepare", "read"].includes(operation)) {
    return fail(ERROR_ACTION_UNSUPPORTED, `unsupported cdp text operation ${JSON.stringify(operation)}`);
  }
  if (locatorCount !== 1) {
    return fail(
      ERROR_ACTION_UNSUPPORTED,
      `cdpInput ${action} requires exactly one of selector, elementId, or activeElement=true`
    );
  }

  const resolved = resolveEditableElement();
  if (!resolved.ok) {
    return resolved;
  }
  const element = resolved.element;
  const beforeElement = elementSummary(element);
  const beforeActiveElement = readActiveElementLocal();
  const beforeValue = readValue(element);

  if (operation === "read") {
    return {
      ok: true,
      action,
      operation,
      resolved_by: resolved.resolvedBy,
      match_count: resolved.matchCount,
      tag_name: tag(element),
      editable_kind: editableKind(element),
      before_value: beforeValue,
      after_value: beforeValue,
      before_active_element: beforeActiveElement,
      after_active_element: readActiveElementLocal(),
      before_element: beforeElement,
      after_element: elementSummary(element),
      selection_mode: "read_only",
      auto_wait: false,
      auto_wait_readback: null
    };
  }

  let actionability = actionabilitySnapshot(element);
  let autoWaitReadback = null;
  if (autoWait && !actionability.ok) {
    autoWaitReadback = await waitForActionability(element, autoWaitTimeoutMs);
    actionability = autoWaitReadback.last_actionability || actionability;
    if (!autoWaitReadback.ok) {
      return fail(
        ERROR_BROWSER_WAIT_TIMEOUT,
        `cdpInput ${action} auto_wait timed out after ${autoWaitTimeoutMs} ms waiting for editable actionability; unmet predicates: ${autoWaitReadback.predicate_detail}`,
        {
          resolved_by: resolved.resolvedBy,
          match_count: resolved.matchCount,
          before_element: beforeElement,
          before_active_element: beforeActiveElement,
          auto_wait: true,
          auto_wait_readback: autoWaitReadback
        }
      );
    }
  }
  if (!actionability.ok) {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, `resolved editable is not actionable: ${actionability.predicate_detail}`, {
      resolved_by: resolved.resolvedBy,
      match_count: resolved.matchCount,
      before_element: beforeElement,
      before_active_element: beforeActiveElement,
      actionability
    });
  }

  const actionPoint = elementClickPoint(element);
  let selectionMode = "focused";
  try {
    if (typeof element.focus === "function") {
      element.focus({ preventScroll: true });
    }
  } catch (_) {
    try {
      element.focus();
    } catch (_) {
      // The active-element readback below is the verdict.
    }
  }
  if (action === "set_text") {
    selectionMode = selectAllEditable(element);
  }
  const afterActiveElement = readActiveElementLocal();
  if (!activeMatchesElement(element)) {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "focused editable did not become the active input target before CDP text dispatch", {
      resolved_by: resolved.resolvedBy,
      match_count: resolved.matchCount,
      before_element: beforeElement,
      before_active_element: beforeActiveElement,
      after_active_element: afterActiveElement,
      selection_mode: selectionMode
    });
  }

  return {
    ok: true,
    action,
    operation,
    resolved_by: resolved.resolvedBy,
    match_count: resolved.matchCount,
    tag_name: tag(element),
    editable_kind: editableKind(element),
    before_value: beforeValue,
    after_value: readValue(element),
    before_active_element: beforeActiveElement,
    after_active_element: afterActiveElement,
    before_element: beforeElement,
    after_element: elementSummary(element),
    action_point: actionPoint,
    selection_mode: selectionMode,
    auto_wait: autoWait,
    auto_wait_readback: autoWaitReadback
  };

  function resolveEditableElement() {
    let element = null;
    let resolvedBy = "";
    let matchCount = 0;
    if (selector) {
      let nodes;
      try {
        nodes = deepQueryAllLocal(selector);
      } catch (error) {
        return fail("CHROME_DOM_SELECTOR_INVALID", `querySelectorAll(${selector}) threw: ${String(error?.message || error)}`);
      }
      const editable = nodes.filter((node) => editableKind(node) && isElementVisible(node));
      matchCount = editable.length;
      if (editable.length === 0) {
        return fail(ERROR_ELEMENT_NOT_FOUND, `selector matched ${nodes.length} node(s), 0 editable+visible`, {
          selector_match_count: nodes.length,
          match_count: 0
        });
      }
      if (editable.length > 1) {
        return fail(ERROR_ELEMENT_AMBIGUOUS, `selector matched ${editable.length} editable+visible nodes; refine the selector`, {
          editable_match_count: editable.length,
          match_count: editable.length
        });
      }
      element = editable[0];
      resolvedBy = "selector";
    } else if (elementPath) {
      element = elementByPath(elementPath);
      matchCount = element ? 1 : 0;
      resolvedBy = "element_id";
      if (!element) {
        return fail(ERROR_ELEMENT_NOT_FOUND, `bridge element path ${elementPath} did not resolve in this frame`, {
          element_path: elementPath,
          match_count: 0
        });
      }
    } else if (wantActive) {
      element = activeElementLocal();
      matchCount = element ? 1 : 0;
      resolvedBy = "active_element";
      if (!element) {
        return fail("CHROME_ACTIVE_ELEMENT_MISSING", "document.activeElement is absent", { match_count: 0 });
      }
    }
    if (!element || !(element instanceof Element)) {
      return fail(ERROR_ELEMENT_NOT_FOUND, "no editable element resolved", { match_count: 0 });
    }
    const kind = editableKind(element);
    if (!kind) {
      return fail("CHROME_ACTIVE_ELEMENT_NOT_EDITABLE", `resolved element ${tag(element)} is not a supported editable target`, {
        resolved_by: resolvedBy,
        match_count: matchCount,
        before_element: elementSummary(element)
      });
    }
    return { ok: true, element, resolvedBy, matchCount };
  }

  function waitForActionability(element, timeoutMs) {
    const started = Date.now();
    const deadline = started + timeoutMs;
    let pollCount = 0;
    let last = null;
    return new Promise((resolve) => {
      const poll = () => {
        last = actionabilitySnapshot(element);
        pollCount += 1;
        if (last.ok || Date.now() >= deadline) {
          resolve({
            ok: Boolean(last.ok),
            requirement: "editable_action_ready",
            timeout_ms: timeoutMs,
            poll_count: pollCount,
            elapsed_ms: Math.max(0, Date.now() - started),
            unmet_predicates: last.unmet_predicates,
            predicate_detail: last.predicate_detail,
            last_actionability: last
          });
          return;
        }
        setTimeout(poll, 50);
      };
      poll();
    });
  }

  function actionabilitySnapshot(element) {
    const attached = Boolean(element && element.isConnected);
    const editable = Boolean(editableKind(element));
    const enabled = attached && isElementEnabled(element);
    const visible = attached && isElementVisible(element);
    const hit = visible ? hitTest(element) : { receives_events: false, detail: "not_visible" };
    const unmet = [];
    if (!attached) unmet.push({ predicate: "attached", reason: "detached" });
    if (!editable) unmet.push({ predicate: "editable", reason: "not_editable" });
    if (!enabled) unmet.push({ predicate: "enabled", reason: "disabled_or_aria_disabled" });
    if (!visible) unmet.push({ predicate: "visible", reason: "not_visible" });
    if (!hit.receives_events) unmet.push({ predicate: "receives_events", reason: hit.detail || "hit_test_failed" });
    return {
      ok: unmet.length === 0,
      attached,
      editable,
      enabled,
      visible,
      hit_test: hit,
      unmet_predicates: unmet,
      predicate_detail: unmet.length ? unmet.map((item) => item.predicate).join(",") : "none"
    };
  }

  function hitTest(element) {
    const point = elementClickPoint(element);
    const top = document.elementFromPoint(point.x, point.y);
    const receives = Boolean(top && (top === element || element.contains(top) || top.contains(element)));
    return {
      receives_events: receives,
      point,
      top_tag_name: top instanceof Element ? tag(top) : null,
      top_id: top instanceof Element ? String(top.id || "") : null,
      detail: receives ? "ok" : "covered_or_outside"
    };
  }

  function elementClickPoint(element) {
    try {
      element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    } catch (_) {
      try {
        element.scrollIntoView();
      } catch (_) {
        // Fall through to the current box.
      }
    }
    const rect = element.getBoundingClientRect();
    return {
      x: Math.max(0, Math.min(window.innerWidth - 1, rect.left + rect.width / 2)),
      y: Math.max(0, Math.min(window.innerHeight - 1, rect.top + rect.height / 2))
    };
  }

  function selectAllEditable(element) {
    const kind = editableKind(element);
    if (kind === "value") {
      if (typeof element.select === "function") {
        element.select();
      } else if (typeof element.setSelectionRange === "function") {
        const value = String(element.value ?? "");
        element.setSelectionRange(0, value.length);
      } else {
        throw new Error(`value editable ${tag(element)} does not expose select() or setSelectionRange()`);
      }
      return "selected_value";
    }
    if (kind === "contenteditable") {
      const range = document.createRange();
      range.selectNodeContents(element);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      return "selected_contenteditable";
    }
    throw new Error(`resolved ${tag(element)} is not editable`);
  }

  function activeMatchesElement(element) {
    const active = activeElementLocal();
    return Boolean(active && (active === element || element.contains(active)));
  }

  function activeElementLocal() {
    let active = document.activeElement;
    while (active && active.shadowRoot && active.shadowRoot.activeElement) {
      active = active.shadowRoot.activeElement;
    }
    return active instanceof Element ? active : null;
  }

  function readActiveElementLocal() {
    const active = activeElementLocal();
    if (!active) {
      return {
        available: true,
        readback_source: "chrome.scripting.executeScript",
        has_active_element: false,
        is_editable: false,
        tag_name: "",
        id: "",
        name: "",
        value: null,
        selected_text: null
      };
    }
    return {
      available: true,
      readback_source: "chrome.scripting.executeScript",
      has_active_element: true,
      is_editable: Boolean(editableKind(active)),
      tag_name: String(active.tagName || ""),
      id: String(active.id || ""),
      name: String(active.getAttribute("name") || ""),
      value: editableKind(active) ? readValue(active) : null,
      selected_text: selectedText(active)
    };
  }

  function selectedText(active) {
    try {
      if (typeof active.selectionStart === "number" && typeof active.selectionEnd === "number" && "value" in active) {
        return String(active.value ?? "").slice(active.selectionStart, active.selectionEnd);
      }
      const selection = window.getSelection();
      return selection && selection.rangeCount > 0 ? String(selection.toString() || "") : null;
    } catch (_) {
      return null;
    }
  }

  function elementSummary(element) {
    if (!(element instanceof Element)) {
      return null;
    }
    const value = readValue(element);
    return {
      tag_name: tag(element),
      role: String(element.getAttribute("role") || ""),
      id: String(element.id || ""),
      name_attr: String(element.getAttribute("name") || ""),
      type_attr: String(element.getAttribute("type") || ""),
      text: trimForReadback(element.textContent || "", 160),
      value_len: value.length,
      visible: isElementVisible(element),
      enabled: isElementEnabled(element),
      editable_kind: editableKind(element)
    };
  }

  function elementByPath(path) {
    if (!path) {
      return null;
    }
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children ? scope.children[index] : null;
    }
    return current instanceof Element ? current : null;
  }

  function deepQueryAllLocal(sel) {
    const out = [];
    const seen = new Set();
    const scopes = [document];
    const stack = [document];
    while (stack.length) {
      const node = stack.pop();
      const all = node.querySelectorAll ? node.querySelectorAll("*") : [];
      for (const el of all) {
        if (el.shadowRoot) {
          scopes.push(el.shadowRoot);
          stack.push(el.shadowRoot);
        }
      }
    }
    for (const scope of scopes) {
      const matches = scope.querySelectorAll(sel);
      for (const el of matches) {
        if (!seen.has(el)) {
          seen.add(el);
          out.push(el);
        }
      }
    }
    return out;
  }

  function editableKind(element) {
    if (!(element instanceof Element) || element.disabled || element.readOnly) {
      return null;
    }
    const lower = tag(element);
    if (lower === "textarea") {
      return "value";
    }
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "text").toLowerCase();
      const nonText = ["button", "submit", "reset", "checkbox", "radio", "range", "color", "file", "image", "hidden"];
      return nonText.includes(type) ? null : "value";
    }
    if (
      element.isContentEditable ||
      String(element.getAttribute("contenteditable") || "").toLowerCase() === "true"
    ) {
      return "contenteditable";
    }
    return null;
  }

  function readValue(element) {
    const kind = editableKind(element);
    if (kind === "value" && "value" in element) {
      return String(element.value ?? "");
    }
    if (kind === "contenteditable") {
      return readContentEditableValue(element);
    }
    return "";
  }

  function readContentEditableValue(element) {
    const textContent = String(element.textContent ?? "");
    if (textContent.length === 0) {
      return "";
    }
    return String(element.innerText || textContent || "");
  }

  function isElementEnabled(element) {
    if (Boolean(element.disabled)) {
      return false;
    }
    return String(element.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
  }

  function isElementVisible(element) {
    if (!(element instanceof Element) || !element.isConnected) {
      return false;
    }
    const style = window.getComputedStyle(element);
    if (!style || style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
      return false;
    }
    if (Number(style.opacity) === 0) {
      return false;
    }
    const rects = element.getClientRects();
    return rects && rects.length > 0 && Array.from(rects).some((rect) => rect.width > 0 && rect.height > 0);
  }

  function tag(element) {
    return String(element?.localName || element?.tagName || "").toLowerCase();
  }

  function trimForReadback(value, maxLen) {
    const text = String(value ?? "");
    return text.length > maxLen ? text.slice(0, maxLen) : text;
  }

  function fail(code, detail, extra = {}) {
    return {
      ok: false,
      error_code: code,
      error_detail: detail,
      ...extra
    };
  }
}

async function cdpTextInputTargetScript(selected, action, params, options) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  const requestedElementId = stringOrNull(params.elementId);
  const bridgeElement = requestedElementId && requestedElementId.startsWith("chrome-tab:")
    ? parseChromeBridgeElementId(requestedElementId, selected.tabId, "cdpInput")
    : { raw: requestedElementId, frameId: null, path: null };
  const request = {
    action,
    operation: String(options.operation || "read"),
    selector: stringOrNull(params.selector),
    elementId: requestedElementId,
    elementPath: bridgeElement.path,
    activeElement: Boolean(params.activeElement),
    autoWait: Boolean(options.autoWait),
    autoWaitTimeoutMs: Number.isSafeInteger(options.autoWaitTimeoutMs) ? options.autoWaitTimeoutMs : 2000
  };
  if (!request.selector && !request.elementId && !request.activeElement) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput ${action} requires selector, elementId, or activeElement=true`
    );
  }
  const scriptTarget = { tabId: selected.tabId };
  if (Number.isSafeInteger(bridgeElement.frameId)) {
    scriptTarget.frameIds = [bridgeElement.frameId];
  } else {
    scriptTarget.allFrames = true;
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: scriptTarget,
      func: cdpTextInputTargetInPage,
      args: [request]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript cdpInput(${selected.tabId}, ${action}, ${request.operation}) failed: ${errorMessage(error)}`
    );
  }
  const frameResults = frameExecutionResults(injected);
  const okFrames = frameResults.filter((frame) => frame.result && frame.result.ok);
  const totalMatches = frameResults.reduce((sum, frame) => sum + frameResolvedMatchCount(frame), 0);
  const ambiguousFrames = frameResults.filter((frame) =>
    frame.result?.error_code === ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS
  );
  if (okFrames.length > 1 || totalMatches > 1 || ambiguousFrames.length > 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS,
      `cdpInput ${action} ${request.operation} matched ${totalMatches} editable element(s) across ${frameResults.length} frame(s); ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  const first = okFrames[0] || null;
  if (!first || !first.result || typeof first.result !== "object") {
    const failed = frameResults.find((frame) => frame.result)?.result;
    throw bridgeError(
      String(failed?.error_code || ERROR_CHROME_DOM_ELEMENT_NOT_FOUND),
      `cdpInput ${action} ${request.operation} failed across ${frameResults.length} frame(s): ${String(failed?.error_detail || "no matching frame result")}; ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  if (first.frame_id !== 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput ${action} currently supports top-frame text targets only because chrome.debugger Input text dispatch uses the active page selection; ` +
        `resolved frame_id=${first.frame_id}. Re-resolve a top-frame editor or use a raw-CDP frame-aware target.`
    );
  }
  return {
    ...first.result,
    frame_id: first.frame_id,
    frame_document_id: first.document_id,
    frame_result_count: frameResults.length,
    frame_results: frameResults.map(summarizeFrameExecutionResult)
  };
}

async function dispatchCdpTextInput(tabId, action, text) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    if (action === "set_text" && text.length === 0) {
      await sendDebuggerCommand(debuggee, "Input.dispatchKeyEvent", {
        type: "rawKeyDown",
        key: "Delete",
        code: "Delete",
        windowsVirtualKeyCode: 46,
        nativeVirtualKeyCode: 46,
        commands: ["deleteForward"]
      });
      await sendDebuggerCommand(debuggee, "Input.dispatchKeyEvent", {
        type: "keyUp",
        key: "Delete",
        code: "Delete",
        windowsVirtualKeyCode: 46,
        nativeVirtualKeyCode: 46
      });
      return {
        method: "Input.dispatchKeyEvent(Delete)",
        dispatched_events: ["rawKeyDown(Delete)", "keyUp(Delete)"],
        protocol_version: protocolVersion
      };
    }
    await sendDebuggerCommand(debuggee, "Input.insertText", { text });
    return {
      method: "Input.insertText",
      dispatched_events: ["insertText"],
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger ${action} text dispatch failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for text input tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function handleViewportEmulation(params) {
  requireDebuggerApiAvailable("viewportEmulation", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeViewportEmulationOperation(params.operation);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  const beforeViewport = await tabViewportMetricsState(selected.tabId);
  let requested = null;
  let dispatch;
  if (operation === "set") {
    requested = {
      width: normalizeViewportDimension(params.width, "width"),
      height: normalizeViewportDimension(params.height, "height"),
      device_scale_factor: normalizeViewportScaleFactor(params.deviceScaleFactor),
      mobile: normalizeViewportMobile(params.isMobile)
    };
    if (!VIEWPORT_BASELINE_BY_TAB.has(selected.tabId)) {
      VIEWPORT_BASELINE_BY_TAB.set(selected.tabId, beforeViewport);
    }
    dispatch = await dispatchViewportEmulationSet(selected.tabId, requested);
    await applyViewportDprShim(selected.tabId, requested.device_scale_factor);
  } else {
    rejectViewportResetField(params.width, "width");
    rejectViewportResetField(params.height, "height");
    rejectViewportResetField(params.deviceScaleFactor, "deviceScaleFactor");
    rejectViewportResetField(params.isMobile, "isMobile");
    dispatch = await dispatchViewportEmulationReset(selected.tabId);
    await clearViewportDprShim(selected.tabId);
  }
  let after = requested
    ? await waitForViewportReadback(selected.tabId, selected.target, waitTimeoutMs, requested)
    : await waitForViewportResetReadback(selected.tabId, selected.target, waitTimeoutMs, beforeViewport);
  let resetFallback = null;
  let resetBaselineRestore = null;
  if (!requested) {
    const baseline = VIEWPORT_BASELINE_BY_TAB.get(selected.tabId);
    const baselineRequest = viewportRequestFromBaseline(baseline);
    if (baselineRequest && !viewportMatchesRequest(after.viewport, baselineRequest)) {
      resetFallback = await reloadViewportTabAfterReset(
        selected.tabId,
        selected.target,
        waitTimeoutMs
      );
      try {
        after = await waitForViewportReadback(
          selected.tabId,
          selected.target,
          Math.min(waitTimeoutMs, 2000),
          baselineRequest
        );
      } catch (_error) {
        resetBaselineRestore = await dispatchViewportBaselineRestore(selected.tabId, baselineRequest);
        after = await waitForViewportReadback(selected.tabId, selected.target, waitTimeoutMs, baselineRequest);
      }
    }
    VIEWPORT_BASELINE_BY_TAB.delete(selected.tabId);
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: after.page.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.page.chrome_window_id,
    operation,
    requested,
    page_url: after.page.url || "",
    page_title: after.page.title || "",
    ready_state: after.page.ready_state || "",
    viewport: after.viewport,
    before_page: before,
    before_viewport: beforeViewport,
    readback_backend: "chrome.debugger.Emulation + chrome.scripting.executeScript(MAIN viewport readback/devicePixelRatio shim)",
    backend_tier_used: "chrome_tabs_extension_debugger",
    required_foreground: false,
    source_of_truth: "normal Chrome bridge page script window.innerWidth/window.innerHeight/devicePixelRatio",
    method: viewportEmulationMethod(dispatch, resetFallback, resetBaselineRestore),
    debugger_protocol_version: dispatch.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleDeviceEmulation(params) {
  requireDebuggerApiAvailable("deviceEmulation", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeDeviceEmulationOperation(params.operation);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  const beforeDevice = await tabDeviceMetricsState(selected.tabId);
  let descriptor = null;
  let dispatch;
  let restoredUserAgent = null;
  if (operation === "set") {
    descriptor = normalizeDeviceDescriptor(params);
    if (!DEVICE_BASELINE_BY_TAB.has(selected.tabId)) {
      DEVICE_BASELINE_BY_TAB.set(selected.tabId, beforeDevice);
    }
    dispatch = await dispatchDeviceEmulationSet(selected.tabId, descriptor);
    await applyDeviceEmulationShim(selected.tabId, descriptor);
  } else {
    rejectDeviceResetField(params.userAgent, "userAgent");
    rejectDeviceResetField(params.width, "width");
    rejectDeviceResetField(params.height, "height");
    rejectDeviceResetField(params.deviceScaleFactor, "deviceScaleFactor");
    rejectDeviceResetField(params.isMobile, "isMobile");
    rejectDeviceResetField(params.hasTouch, "hasTouch");
    rejectDeviceResetField(params.maxTouchPoints, "maxTouchPoints");
    const baseline = DEVICE_BASELINE_BY_TAB.get(selected.tabId);
    restoredUserAgent = typeof baseline?.user_agent === "string" && baseline.user_agent
      ? baseline.user_agent
      : null;
    dispatch = await dispatchDeviceEmulationReset(selected.tabId, restoredUserAgent);
    await clearDeviceEmulationShim(selected.tabId);
  }
  let after = descriptor
    ? await waitForDeviceReadback(selected.tabId, selected.target, waitTimeoutMs, descriptor)
    : await waitForDeviceResetReadback(selected.tabId, selected.target, waitTimeoutMs, beforeDevice);
  let resetFallback = null;
  let resetBaselineRestore = null;
  if (!descriptor) {
    const baseline = DEVICE_BASELINE_BY_TAB.get(selected.tabId);
    const baselineRequest = viewportRequestFromBaseline(baseline?.viewport);
    if (baselineRequest && !viewportMatchesRequest(after.device.viewport, baselineRequest)) {
      resetFallback = await reloadViewportTabAfterReset(
        selected.tabId,
        selected.target,
        waitTimeoutMs
      );
      try {
        after = await waitForDeviceResetBaselineReadback(
          selected.tabId,
          selected.target,
          Math.min(waitTimeoutMs, 2000),
          baseline
        );
      } catch (_error) {
        resetBaselineRestore = await dispatchViewportBaselineRestore(selected.tabId, baselineRequest);
        after = await waitForDeviceResetBaselineReadback(
          selected.tabId,
          selected.target,
          waitTimeoutMs,
          baseline
        );
      }
    }
    DEVICE_BASELINE_BY_TAB.delete(selected.tabId);
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: after.page.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.page.chrome_window_id,
    operation,
    descriptor,
    restored_user_agent: restoredUserAgent,
    page_url: after.page.url || "",
    page_title: after.page.title || "",
    ready_state: after.page.ready_state || "",
    device: after.device,
    before_page: before,
    before_device: beforeDevice,
    readback_backend: "chrome.debugger.Emulation + chrome.scripting.executeScript(MAIN device readback/device shim)",
    backend_tier_used: "chrome_tabs_extension_debugger",
    required_foreground: false,
    source_of_truth: "normal Chrome bridge page script navigator/userAgent/viewport/touch media queries",
    method: viewportEmulationMethod(dispatch, resetFallback, resetBaselineRestore),
    debugger_protocol_version: dispatch.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleGeolocationEmulation(params) {
  requireDebuggerApiAvailable("geolocationEmulation", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeGeolocationEmulationOperation(params.operation);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  const origin = geolocationOriginFromUrl(before.url || selected.target.url || "");
  let requested = null;
  let permissionSetting = "prompt";
  let dispatch;
  if (operation === "set") {
    requested = normalizeGeolocationOverride(params);
    permissionSetting = normalizeGeolocationPermissionSetting(params.grantPermission);
    dispatch = await dispatchGeolocationEmulationSet(selected.tabId, origin, requested, permissionSetting);
    await applyGeolocationEmulationShim(selected.tabId, {
      requested,
      permission_state: permissionSetting
    });
  } else {
    rejectGeolocationResetField(params.latitude, "latitude");
    rejectGeolocationResetField(params.longitude, "longitude");
    rejectGeolocationResetField(params.accuracy, "accuracy");
    rejectGeolocationResetField(params.altitude, "altitude");
    rejectGeolocationResetField(params.altitudeAccuracy, "altitudeAccuracy");
    rejectGeolocationResetField(params.heading, "heading");
    rejectGeolocationResetField(params.speed, "speed");
    rejectGeolocationResetField(params.grantPermission, "grantPermission");
    dispatch = await dispatchGeolocationEmulationReset(selected.tabId, origin);
    await clearGeolocationEmulationShim(selected.tabId);
  }
  const after = await waitForGeolocationReadback(
    selected.tabId,
    selected.target,
    waitTimeoutMs,
    requested,
    permissionSetting
  );
  return {
    extension_id: chrome.runtime.id,
    target_id: after.page.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.page.chrome_window_id,
    operation,
    origin,
    requested,
    permission_setting: permissionSetting,
    page_url: after.page.url || "",
    page_title: after.page.title || "",
    ready_state: after.page.ready_state || "",
    geolocation: after.geolocation,
    before_page: before,
    readback_backend: "chrome.debugger.Emulation geolocation + Browser.setPermission best-effort + chrome.scripting.executeScript(MAIN geolocation/permission shim)",
    backend_tier_used: "chrome_tabs_extension_debugger",
    required_foreground: false,
    source_of_truth: "normal Chrome bridge page script navigator.permissions + navigator.geolocation",
    method: dispatch.method,
    debugger_protocol_version: dispatch.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleLocaleEmulation(params) {
  requireDebuggerApiAvailable("localeEmulation", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeLocaleEmulationOperation(params.operation);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  const beforeLocale = await tabLocaleState(selected.tabId);
  let requested = null;
  let dispatch;
  if (operation === "set") {
    requested = normalizeLocaleOverride(params);
    if (!LOCALE_BASELINE_BY_TAB.has(selected.tabId)) {
      LOCALE_BASELINE_BY_TAB.set(selected.tabId, beforeLocale);
    }
    dispatch = await dispatchLocaleEmulationSet(selected.tabId, requested);
    await applyLocaleEmulationShim(selected.tabId, requested);
  } else {
    rejectLocaleResetField(params.locale, "locale");
    rejectLocaleResetField(params.timezoneId, "timezoneId");
    dispatch = await dispatchLocaleEmulationReset(selected.tabId);
    await clearLocaleEmulationShim(selected.tabId);
  }
  let after = requested
    ? await waitForLocaleReadback(selected.tabId, selected.target, waitTimeoutMs, requested)
    : await waitForLocaleResetReadback(
      selected.tabId,
      selected.target,
      waitTimeoutMs,
      LOCALE_BASELINE_BY_TAB.get(selected.tabId)
    );
  if (!requested) {
    LOCALE_BASELINE_BY_TAB.delete(selected.tabId);
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: after.page.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.page.chrome_window_id,
    operation,
    requested,
    page_url: after.page.url || "",
    page_title: after.page.title || "",
    ready_state: after.page.ready_state || "",
    locale: after.locale,
    before_page: before,
    before_locale: beforeLocale,
    readback_backend: "chrome.debugger.Emulation locale/timezone + chrome.scripting.executeScript(MAIN Intl/Date shim/readback)",
    backend_tier_used: "chrome_tabs_extension_debugger",
    required_foreground: false,
    source_of_truth: "normal Chrome bridge page script Intl.DateTimeFormat/NumberFormat and Date shim/readback",
    method: dispatch.method,
    debugger_protocol_version: dispatch.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleMediaEmulation(params) {
  requireDebuggerApiAvailable("mediaEmulation", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeMediaEmulationOperation(params.operation);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  const beforeMedia = await tabMediaState(selected.tabId);
  let requested = null;
  let dispatch;
  if (operation === "set") {
    requested = normalizeMediaOverride(params);
    if (!MEDIA_BASELINE_BY_TAB.has(selected.tabId)) {
      MEDIA_BASELINE_BY_TAB.set(selected.tabId, beforeMedia);
    }
    dispatch = await dispatchMediaEmulationSet(selected.tabId, requested);
    await applyMediaEmulationShim(selected.tabId, requested);
  } else {
    rejectMediaResetField(params.media, "media");
    rejectMediaResetField(params.colorScheme, "colorScheme");
    rejectMediaResetField(params.reducedMotion, "reducedMotion");
    dispatch = await dispatchMediaEmulationReset(selected.tabId);
    await clearMediaEmulationShim(selected.tabId);
  }
  const after = requested
    ? await waitForMediaReadback(selected.tabId, selected.target, waitTimeoutMs, requested)
    : await waitForMediaResetReadback(
      selected.tabId,
      selected.target,
      waitTimeoutMs,
      MEDIA_BASELINE_BY_TAB.get(selected.tabId)
    );
  if (!requested) {
    MEDIA_BASELINE_BY_TAB.delete(selected.tabId);
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: after.page.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.page.chrome_window_id,
    operation,
    requested,
    page_url: after.page.url || "",
    page_title: after.page.title || "",
    ready_state: after.page.ready_state || "",
    media: after.media,
    before_page: before,
    before_media: beforeMedia,
    readback_backend: "chrome.debugger.Emulation.setEmulatedMedia + chrome.scripting.executeScript(MAIN matchMedia shim/readback)",
    backend_tier_used: "chrome_tabs_extension_debugger",
    required_foreground: false,
    source_of_truth: "normal Chrome bridge page script matchMedia shim/readback",
    method: dispatch.method,
    debugger_protocol_version: dispatch.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function handleNetworkConditions(params) {
  requireDebuggerApiAvailable("networkConditions", params);
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeNetworkConditionsOperation(params.operation);
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  const before = await tabPageState(selected.tabId, selected.target);
  const beforeNetwork = await tabNetworkState(selected.tabId);
  let requested = null;
  let dispatch;
  if (operation === "set") {
    requested = normalizeNetworkConditionsOverride(params);
    if (!NETWORK_BASELINE_BY_TAB.has(selected.tabId)) {
      NETWORK_BASELINE_BY_TAB.set(selected.tabId, beforeNetwork);
    }
    dispatch = await dispatchNetworkConditionsSet(selected.tabId, requested);
    await applyNetworkConditionsShim(selected.tabId, requested);
  } else {
    rejectNetworkConditionsResetField(params.offline, "offline");
    rejectNetworkConditionsResetField(params.latencyMs, "latencyMs");
    rejectNetworkConditionsResetField(params.downloadThroughputBytesPerSec, "downloadThroughputBytesPerSec");
    rejectNetworkConditionsResetField(params.uploadThroughputBytesPerSec, "uploadThroughputBytesPerSec");
    rejectNetworkConditionsResetField(params.connectionType, "connectionType");
    dispatch = await dispatchNetworkConditionsReset(selected.tabId);
    await clearNetworkConditionsShim(selected.tabId);
  }
  const after = requested
    ? await waitForNetworkConditionsReadback(selected.tabId, selected.target, waitTimeoutMs, requested)
    : await waitForNetworkConditionsResetReadback(
      selected.tabId,
      selected.target,
      waitTimeoutMs,
      NETWORK_BASELINE_BY_TAB.get(selected.tabId)
    );
  if (!requested) {
    NETWORK_BASELINE_BY_TAB.delete(selected.tabId);
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: after.page.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.page.chrome_window_id,
    operation,
    requested,
    page_url: after.page.url || "",
    page_title: after.page.title || "",
    ready_state: after.page.ready_state || "",
    network: after.network,
    before_page: before,
    before_network: beforeNetwork,
    readback_backend: "chrome.debugger.Network.emulateNetworkConditions + chrome.scripting.executeScript(MAIN navigator/fetch shim/readback)",
    backend_tier_used: "chrome_tabs_extension_debugger",
    required_foreground: false,
    source_of_truth: "normal Chrome bridge page script navigator.onLine/fetch/networkInformation shim/readback",
    method: dispatch.method,
    debugger_protocol_version: dispatch.protocol_version,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function normalizeCdpInputAction(value) {
  const action = String(value || "").trim().toLowerCase();
  if (
    action === "hover" ||
    action === "tap" ||
    action === "click" ||
    action === "dblclick" ||
    action === "set_text" ||
    action === "type_text" ||
    action === "drag" ||
    action === "html5_drag" ||
    action === "html5_real_drag"
  ) {
    return action;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `cdpInput action must be hover, tap, click, dblclick, set_text, type_text, drag, html5_drag, or html5_real_drag; got ${JSON.stringify(value)}`
  );
}

// CDP Input.dispatchMouseEvent modifier bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8.
function cdpModifierBitmask(modifiers) {
  if (!Array.isArray(modifiers)) {
    return 0;
  }
  let mask = 0;
  for (const item of modifiers) {
    const normalized = String(item || "").trim().toLowerCase();
    if (normalized === "alt" || normalized === "option") {
      mask |= 1;
    } else if (normalized === "ctrl" || normalized === "control") {
      mask |= 2;
    } else if (["meta", "super", "cmd", "command"].includes(normalized)) {
      mask |= 4;
    } else if (normalized === "shift") {
      mask |= 8;
    }
  }
  return mask;
}

function normalizeViewportEmulationOperation(value) {
  const operation = String(value || "set").trim().toLowerCase();
  if (operation === "set" || operation === "reset") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `viewportEmulation operation must be set or reset; got ${JSON.stringify(value)}`
  );
}

function normalizeViewportDimension(value, fieldName) {
  if (!Number.isSafeInteger(value) || value < 1 || value > 10000000) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `viewportEmulation ${fieldName} must be an integer in 1..=10000000; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeViewportScaleFactor(value) {
  const resolved = value === null || value === undefined ? 1 : Number(value);
  if (!Number.isFinite(resolved) || resolved <= 0 || resolved > 1000) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `viewportEmulation deviceScaleFactor must be finite and in 0..=1000; got ${JSON.stringify(value)}`
    );
  }
  return resolved;
}

function normalizeViewportMobile(value) {
  if (value === null || value === undefined) {
    return false;
  }
  if (typeof value !== "boolean") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `viewportEmulation isMobile must be a boolean; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function rejectViewportResetField(value, fieldName) {
  if (value !== null && value !== undefined) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `viewportEmulation ${fieldName} is not valid for operation=reset`
    );
  }
}

function normalizeDeviceEmulationOperation(value) {
  const operation = String(value || "set").trim().toLowerCase();
  if (operation === "set" || operation === "reset") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `deviceEmulation operation must be set or reset; got ${JSON.stringify(value)}`
  );
}

function normalizeDeviceDescriptor(params) {
  const userAgent = String(params.userAgent || "").trim();
  if (!userAgent) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "deviceEmulation userAgent is required for operation=set");
  }
  const hasTouch = normalizeDeviceBoolean(params.hasTouch, "hasTouch", false);
  const maxTouchPoints = normalizeDeviceMaxTouchPoints(params.maxTouchPoints, hasTouch);
  return {
    user_agent: userAgent,
    width: normalizeViewportDimension(params.width, "width"),
    height: normalizeViewportDimension(params.height, "height"),
    device_scale_factor: normalizeViewportScaleFactor(params.deviceScaleFactor),
    is_mobile: normalizeDeviceBoolean(params.isMobile, "isMobile", false),
    has_touch: hasTouch,
    max_touch_points: maxTouchPoints
  };
}

function normalizeDeviceBoolean(value, fieldName, defaultValue) {
  if (value === null || value === undefined) {
    return defaultValue;
  }
  if (typeof value !== "boolean") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `deviceEmulation ${fieldName} must be a boolean; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeDeviceMaxTouchPoints(value, hasTouch) {
  const resolved = value === null || value === undefined
    ? (hasTouch ? 5 : 0)
    : value;
  if (!Number.isSafeInteger(resolved) || resolved < 0 || resolved > 100) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `deviceEmulation maxTouchPoints must be an integer in 0..=100; got ${JSON.stringify(value)}`
    );
  }
  if (hasTouch && resolved === 0) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "deviceEmulation maxTouchPoints must be >=1 when hasTouch=true");
  }
  if (!hasTouch && resolved !== 0) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "deviceEmulation maxTouchPoints must be 0 when hasTouch=false");
  }
  return resolved;
}

function rejectDeviceResetField(value, fieldName) {
  if (value !== null && value !== undefined) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `deviceEmulation ${fieldName} is not valid for operation=reset`
    );
  }
}

function normalizeGeolocationEmulationOperation(value) {
  const operation = String(value || "set").trim().toLowerCase();
  if (operation === "set" || operation === "reset") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `geolocationEmulation operation must be set or reset; got ${JSON.stringify(value)}`
  );
}

function normalizeGeolocationOverride(params) {
  const latitude = normalizeGeolocationNumber(params.latitude, "latitude", -90, 90, true);
  const longitude = normalizeGeolocationNumber(params.longitude, "longitude", -180, 180, true);
  const accuracy = normalizeGeolocationNumber(params.accuracy ?? 0, "accuracy", 0, Number.MAX_VALUE, true);
  return {
    latitude,
    longitude,
    accuracy,
    altitude: normalizeGeolocationOptionalNumber(params.altitude, "altitude"),
    altitude_accuracy: normalizeGeolocationOptionalNumber(params.altitudeAccuracy, "altitudeAccuracy", 0, Number.MAX_VALUE),
    heading: normalizeGeolocationOptionalNumber(params.heading, "heading", 0, 360),
    speed: normalizeGeolocationOptionalNumber(params.speed, "speed", 0, Number.MAX_VALUE)
  };
}

function normalizeGeolocationPermissionSetting(grantPermission) {
  if (grantPermission === null || grantPermission === undefined) {
    return "granted";
  }
  if (typeof grantPermission !== "boolean") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `geolocationEmulation grantPermission must be a boolean; got ${JSON.stringify(grantPermission)}`
    );
  }
  return grantPermission ? "granted" : "denied";
}

function normalizeGeolocationNumber(value, fieldName, min, max, required) {
  if (value === null || value === undefined) {
    if (required) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `geolocationEmulation ${fieldName} is required for operation=set`
      );
    }
    return null;
  }
  const number = Number(value);
  if (!Number.isFinite(number) || number < min || number > max) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `geolocationEmulation ${fieldName} must be finite and in ${min}..=${max}; got ${JSON.stringify(value)}`
    );
  }
  return number;
}

function normalizeGeolocationOptionalNumber(value, fieldName, min = -Number.MAX_VALUE, max = Number.MAX_VALUE) {
  const number = normalizeGeolocationNumber(value, fieldName, min, max, false);
  return number === null ? null : number;
}

function rejectGeolocationResetField(value, fieldName) {
  if (value !== null && value !== undefined) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `geolocationEmulation ${fieldName} is not valid for operation=reset`
    );
  }
}

function geolocationOriginFromUrl(url) {
  try {
    const parsed = new URL(String(url || ""));
    if (parsed.origin && parsed.origin !== "null") {
      return parsed.origin;
    }
  } catch (_) {}
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `geolocationEmulation requires a page with a non-opaque origin; current URL=${JSON.stringify(url || "")}`
  );
}

function normalizeLocaleEmulationOperation(value) {
  const operation = String(value || "set").trim().toLowerCase();
  if (operation === "set" || operation === "reset") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `localeEmulation operation must be set or reset; got ${JSON.stringify(value)}`
  );
}

function normalizeLocaleOverride(params) {
  const locale = normalizeLocaleToken(params.locale, "locale", /^[A-Za-z0-9_-]+$/u, false);
  const timezoneId = normalizeLocaleToken(params.timezoneId, "timezoneId", /^[A-Za-z0-9/_+-]+$/u, false);
  if (!locale && !timezoneId) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "localeEmulation operation=set requires locale and/or timezoneId"
    );
  }
  return {
    locale,
    timezone_id: timezoneId
  };
}

function normalizeLocaleToken(value, fieldName, allowedPattern, required) {
  if (value === null || value === undefined) {
    if (required) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `localeEmulation ${fieldName} is required for operation=set`
      );
    }
    return null;
  }
  if (typeof value !== "string") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `localeEmulation ${fieldName} must be a string; got ${JSON.stringify(value)}`
    );
  }
  if (value.trim() !== value || value.length === 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `localeEmulation ${fieldName} must be non-empty without surrounding whitespace`
    );
  }
  if ([...value].length > 128) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `localeEmulation ${fieldName} must be at most 128 characters`
    );
  }
  if (!allowedPattern.test(value)) {
    const allowed = fieldName === "timezoneId"
      ? "ASCII letters, digits, '/', '_', '-' or '+'"
      : "ASCII letters, digits, '_' or '-'";
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `localeEmulation ${fieldName} must contain only ${allowed}`
    );
  }
  return value;
}

function rejectLocaleResetField(value, fieldName) {
  if (value !== null && value !== undefined) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `localeEmulation ${fieldName} is not valid for operation=reset`
    );
  }
}

function normalizeMediaEmulationOperation(value) {
  const operation = String(value || "set").trim().toLowerCase();
  if (operation === "set" || operation === "reset") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `mediaEmulation operation must be set or reset; got ${JSON.stringify(value)}`
  );
}

function normalizeMediaOverride(params) {
  const media = normalizeMediaToken(params.media, "media", ["screen", "print"]);
  const colorScheme = normalizeMediaToken(params.colorScheme, "colorScheme", ["light", "dark", "no-preference"]);
  const reducedMotion = normalizeMediaToken(params.reducedMotion, "reducedMotion", ["reduce", "no-preference"]);
  if (!media && !colorScheme && !reducedMotion) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "mediaEmulation operation=set requires media, colorScheme and/or reducedMotion"
    );
  }
  return {
    media,
    color_scheme: colorScheme,
    reduced_motion: reducedMotion
  };
}

function normalizeMediaToken(value, fieldName, allowed) {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value !== "string") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `mediaEmulation ${fieldName} must be a string; got ${JSON.stringify(value)}`
    );
  }
  const normalized = value.trim().toLowerCase();
  if (normalized !== value || !allowed.includes(normalized)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `mediaEmulation ${fieldName} must be one of ${allowed.join(", ")}`
    );
  }
  return normalized;
}

function rejectMediaResetField(value, fieldName) {
  if (value !== null && value !== undefined) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `mediaEmulation ${fieldName} is not valid for operation=reset`
    );
  }
}

function normalizeNetworkConditionsOperation(value) {
  const operation = String(value || "set").trim().toLowerCase();
  if (operation === "set" || operation === "reset") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `networkConditions operation must be set or reset; got ${JSON.stringify(value)}`
  );
}

function normalizeNetworkConditionsOverride(params) {
  const offline = normalizeNetworkBoolean(params.offline, "offline", false);
  const latencyMs = normalizeNetworkNumber(params.latencyMs, "latencyMs", 0, 0, 300000);
  const downloadThroughputBytesPerSec = normalizeNetworkNumber(
    params.downloadThroughputBytesPerSec,
    "downloadThroughputBytesPerSec",
    -1,
    -1,
    1000000000000
  );
  const uploadThroughputBytesPerSec = normalizeNetworkNumber(
    params.uploadThroughputBytesPerSec,
    "uploadThroughputBytesPerSec",
    -1,
    -1,
    1000000000000
  );
  const connectionType = normalizeNetworkConnectionType(params.connectionType);
  if (
    !offline &&
    latencyMs === 0 &&
    downloadThroughputBytesPerSec === -1 &&
    uploadThroughputBytesPerSec === -1 &&
    !connectionType
  ) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "networkConditions operation=set requires offline=true, latencyMs, throughput and/or connectionType"
    );
  }
  return {
    offline,
    latency_ms: latencyMs,
    download_throughput_bytes_per_sec: downloadThroughputBytesPerSec,
    upload_throughput_bytes_per_sec: uploadThroughputBytesPerSec,
    connection_type: connectionType
  };
}

function normalizeNetworkBoolean(value, fieldName, defaultValue) {
  if (value === null || value === undefined) {
    return defaultValue;
  }
  if (typeof value !== "boolean") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `networkConditions ${fieldName} must be a boolean; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeNetworkNumber(value, fieldName, defaultValue, minimum, maximum) {
  if (value === null || value === undefined) {
    return defaultValue;
  }
  const number = Number(value);
  if (!Number.isFinite(number) || number < minimum || number > maximum || (minimum === -1 && number < 0 && number !== -1)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `networkConditions ${fieldName} must be finite and in ${minimum}..=${maximum}${minimum === -1 ? " or -1" : ""}; got ${JSON.stringify(value)}`
    );
  }
  return number;
}

function normalizeNetworkConnectionType(value) {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value !== "string") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `networkConditions connectionType must be a string; got ${JSON.stringify(value)}`
    );
  }
  const normalized = value.trim().toLowerCase();
  const allowed = ["none", "cellular2g", "cellular3g", "cellular4g", "bluetooth", "ethernet", "wifi", "wimax", "other"];
  if (normalized !== value || !allowed.includes(normalized)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `networkConditions connectionType must be one of ${allowed.join(", ")}`
    );
  }
  return normalized;
}

function rejectNetworkConditionsResetField(value, fieldName) {
  if (value !== null && value !== undefined) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `networkConditions ${fieldName} is not valid for operation=reset`
    );
  }
}

async function dispatchDeviceEmulationSet(tabId, descriptor) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Emulation.setUserAgentOverride", {
      userAgent: descriptor.user_agent
    });
    await sendDebuggerCommand(debuggee, "Emulation.setDeviceMetricsOverride", {
      width: descriptor.width,
      height: descriptor.height,
      deviceScaleFactor: descriptor.device_scale_factor,
      mobile: descriptor.is_mobile,
      screenWidth: descriptor.width,
      screenHeight: descriptor.height
    });
    await sendDebuggerCommand(debuggee, "Emulation.setTouchEmulationEnabled", {
      enabled: descriptor.has_touch,
      maxTouchPoints: descriptor.has_touch ? descriptor.max_touch_points : 0
    });
    await sendDebuggerCommand(debuggee, "Emulation.setEmitTouchEventsForMouse", {
      enabled: descriptor.has_touch,
      configuration: descriptor.is_mobile ? "mobile" : "desktop"
    });
    return {
      method: "Emulation.setUserAgentOverride+Emulation.setDeviceMetricsOverride+Emulation.setTouchEmulationEnabled+Emulation.setEmitTouchEventsForMouse",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger deviceEmulation set failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for deviceEmulation set tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchDeviceEmulationReset(tabId, restoredUserAgent) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Emulation.clearDeviceMetricsOverride", {});
    await sendDebuggerCommand(debuggee, "Emulation.setDeviceMetricsOverride", {
      width: 0,
      height: 0,
      deviceScaleFactor: 0,
      mobile: false,
      screenWidth: 0,
      screenHeight: 0
    });
    await sendDebuggerCommand(debuggee, "Emulation.setTouchEmulationEnabled", {
      enabled: false
    });
    await sendDebuggerCommand(debuggee, "Emulation.setEmitTouchEventsForMouse", {
      enabled: false,
      configuration: "desktop"
    });
    if (restoredUserAgent) {
      await sendDebuggerCommand(debuggee, "Emulation.setUserAgentOverride", {
        userAgent: restoredUserAgent
      });
    }
    return {
      method: restoredUserAgent
        ? "Emulation.clearDeviceMetricsOverride+Emulation.setDeviceMetricsOverride(width=0,height=0)+Emulation.setTouchEmulationEnabled(false)+Emulation.setUserAgentOverride(original)"
        : "Emulation.clearDeviceMetricsOverride+Emulation.setDeviceMetricsOverride(width=0,height=0)+Emulation.setTouchEmulationEnabled(false)",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger deviceEmulation reset failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for deviceEmulation reset tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchGeolocationEmulationSet(tabId, origin, requested, permissionSetting) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  let permissionMethod = `Browser.setPermission(${permissionSetting})`;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    try {
      await sendDebuggerCommand(debuggee, "Browser.setPermission", {
        permission: { name: "geolocation" },
        setting: permissionSetting,
        origin
      });
    } catch (error) {
      permissionMethod = `Browser.setPermission(${permissionSetting}) best_effort_failed=${errorMessage(error)}`;
    }
    const params = {
      latitude: requested.latitude,
      longitude: requested.longitude,
      accuracy: requested.accuracy
    };
    if (requested.altitude !== null && requested.altitude !== undefined) {
      params.altitude = requested.altitude;
    }
    if (requested.altitude_accuracy !== null && requested.altitude_accuracy !== undefined) {
      params.altitudeAccuracy = requested.altitude_accuracy;
    }
    if (requested.heading !== null && requested.heading !== undefined) {
      params.heading = requested.heading;
    }
    if (requested.speed !== null && requested.speed !== undefined) {
      params.speed = requested.speed;
    }
    await sendDebuggerCommand(debuggee, "Emulation.setGeolocationOverride", params);
    return {
      method: `Emulation.setGeolocationOverride+${permissionMethod}`,
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger geolocationEmulation set failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for geolocationEmulation set tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchGeolocationEmulationReset(tabId, origin) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  let permissionMethod = "Browser.setPermission(prompt)";
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Emulation.clearGeolocationOverride", {});
    try {
      await sendDebuggerCommand(debuggee, "Browser.setPermission", {
        permission: { name: "geolocation" },
        setting: "prompt",
        origin
      });
    } catch (error) {
      permissionMethod = `Browser.setPermission(prompt) best_effort_failed=${errorMessage(error)}`;
    }
    return {
      method: `Emulation.clearGeolocationOverride+${permissionMethod}`,
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger geolocationEmulation reset failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for geolocationEmulation reset tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchLocaleEmulationSet(tabId, requested) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  const methods = [];
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    if (requested.locale) {
      await sendDebuggerCommand(debuggee, "Emulation.setLocaleOverride", {
        locale: requested.locale
      });
      methods.push("Emulation.setLocaleOverride");
    }
    if (requested.timezone_id) {
      await sendDebuggerCommand(debuggee, "Emulation.setTimezoneOverride", {
        timezoneId: requested.timezone_id
      });
      methods.push("Emulation.setTimezoneOverride");
    }
    return {
      method: methods.join("+"),
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger localeEmulation set failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for localeEmulation set tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchLocaleEmulationReset(tabId) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Emulation.setLocaleOverride", {});
    await sendDebuggerCommand(debuggee, "Emulation.setTimezoneOverride", {
      timezoneId: ""
    });
    return {
      method: "Emulation.setLocaleOverride(default)+Emulation.setTimezoneOverride(default)",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger localeEmulation reset failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for localeEmulation reset tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchMediaEmulationSet(tabId, requested) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    const features = [];
    if (requested.color_scheme) {
      features.push({ name: "prefers-color-scheme", value: requested.color_scheme });
    }
    if (requested.reduced_motion) {
      features.push({ name: "prefers-reduced-motion", value: requested.reduced_motion });
    }
    await sendDebuggerCommand(debuggee, "Emulation.setEmulatedMedia", {
      media: requested.media || "",
      features
    });
    return {
      method: "Emulation.setEmulatedMedia(media,features)",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger mediaEmulation set failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for mediaEmulation set tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchMediaEmulationReset(tabId) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Emulation.setEmulatedMedia", {
      media: "",
      features: []
    });
    return {
      method: "Emulation.setEmulatedMedia(default)",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger mediaEmulation reset failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for mediaEmulation reset tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchNetworkConditionsSet(tabId, requested) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Network.enable", {});
    const params = {
      offline: Boolean(requested.offline),
      latency: Number(requested.latency_ms),
      downloadThroughput: Number(requested.download_throughput_bytes_per_sec),
      uploadThroughput: Number(requested.upload_throughput_bytes_per_sec)
    };
    if (requested.connection_type) {
      params.connectionType = requested.connection_type;
    }
    await sendDebuggerCommand(debuggee, "Network.emulateNetworkConditions", params);
    return {
      method: "Network.enable+Network.emulateNetworkConditions",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger networkConditions set failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for networkConditions set tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchNetworkConditionsReset(tabId) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Network.enable", {});
    await sendDebuggerCommand(debuggee, "Network.emulateNetworkConditions", {
      offline: false,
      latency: 0,
      downloadThroughput: -1,
      uploadThroughput: -1
    });
    return {
      method: "Network.enable+Network.emulateNetworkConditions(default)",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger networkConditions reset failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for networkConditions reset tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchViewportEmulationSet(tabId, requested) {
  return await dispatchViewportEmulation(tabId, "set", {
    width: requested.width,
    height: requested.height,
    deviceScaleFactor: requested.device_scale_factor,
    mobile: requested.mobile,
    screenWidth: requested.width,
    screenHeight: requested.height
  });
}

async function dispatchViewportEmulationReset(tabId) {
  return await dispatchViewportEmulation(tabId, "reset", {
    width: 0,
    height: 0,
    deviceScaleFactor: 0,
    mobile: false,
    screenWidth: 0,
    screenHeight: 0
  });
}

async function dispatchViewportBaselineRestore(tabId, requested) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Emulation.setDeviceMetricsOverride", {
      width: requested.width,
      height: requested.height,
      deviceScaleFactor: requested.device_scale_factor,
      mobile: false,
      screenWidth: requested.screen_width || requested.width,
      screenHeight: requested.screen_height || requested.height
    });
    return {
      method: `Emulation.setDeviceMetricsOverride(baseline width=${requested.width},height=${requested.height},deviceScaleFactor=${requested.device_scale_factor},mobile=false)`,
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger viewportEmulation baseline restore failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for viewportEmulation baseline restore tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchViewportEmulation(tabId, operation, params) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    if (operation === "set") {
      await sendDebuggerCommand(debuggee, "Emulation.setDeviceMetricsOverride", params);
      return {
        method: `Emulation.setDeviceMetricsOverride(width,height,deviceScaleFactor,mobile=${String(Boolean(params.mobile))})`,
        protocol_version: protocolVersion
      };
    }
    await sendDebuggerCommand(debuggee, "Emulation.clearDeviceMetricsOverride", {});
    await sendDebuggerCommand(debuggee, "Emulation.setDeviceMetricsOverride", params);
    return {
      method: "Emulation.clearDeviceMetricsOverride+Emulation.setDeviceMetricsOverride(width=0,height=0,deviceScaleFactor=0,mobile=false)",
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger viewportEmulation ${operation} failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for viewportEmulation tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function waitForViewportReadback(tabId, fallbackTarget, waitTimeoutMs, requested) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const viewport = await tabViewportMetricsState(tabId);
      last = { page, viewport };
      lastError = null;
      if (!requested || viewportMatchesRequest(viewport, requested)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last viewport=${JSON.stringify(last.viewport)} requested=${JSON.stringify(requested)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no viewport readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `viewportEmulation readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

async function waitForViewportResetReadback(tabId, fallbackTarget, waitTimeoutMs, beforeViewport) {
  const started = Date.now();
  const timeoutMs = Math.min(waitTimeoutMs, 1000);
  let last = null;
  let lastError = null;
  while (Date.now() - started <= timeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const viewport = await tabViewportMetricsState(tabId);
      last = { page, viewport };
      lastError = null;
      if (!sameViewportMetrics(viewport, beforeViewport)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  if (last) {
    return last;
  }
  const detail = lastError
    ? `last readback error=${JSON.stringify(lastError)}`
    : "no viewport readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `viewportEmulation reset readback unavailable within ${timeoutMs} ms; ${detail}`);
}

async function waitForDeviceReadback(tabId, fallbackTarget, waitTimeoutMs, descriptor) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const device = await tabDeviceMetricsState(tabId);
      last = { page, device };
      lastError = null;
      if (deviceMatchesDescriptor(device, descriptor)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last device=${JSON.stringify(summarizeDeviceReadback(last.device))} descriptor=${JSON.stringify(descriptor)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no device readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `deviceEmulation readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

async function waitForDeviceResetReadback(tabId, fallbackTarget, waitTimeoutMs, beforeDevice) {
  const started = Date.now();
  const timeoutMs = Math.min(waitTimeoutMs, 1000);
  let last = null;
  let lastError = null;
  while (Date.now() - started <= timeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const device = await tabDeviceMetricsState(tabId);
      last = { page, device };
      lastError = null;
      if (!sameDeviceMetrics(device, beforeDevice)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  if (last) {
    return last;
  }
  const detail = lastError
    ? `last readback error=${JSON.stringify(lastError)}`
    : "no device readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `deviceEmulation reset readback unavailable within ${timeoutMs} ms; ${detail}`);
}

async function waitForDeviceResetBaselineReadback(tabId, fallbackTarget, waitTimeoutMs, baseline) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const device = await tabDeviceMetricsState(tabId);
      last = { page, device };
      lastError = null;
      if (deviceMatchesBaseline(device, baseline)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last device=${JSON.stringify(summarizeDeviceReadback(last.device))} baseline=${JSON.stringify(summarizeDeviceReadback(baseline))}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no device readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `deviceEmulation reset baseline readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

async function waitForGeolocationReadback(tabId, fallbackTarget, waitTimeoutMs, requested, permissionSetting) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const geolocation = await tabGeolocationState(tabId);
      last = { page, geolocation };
      lastError = null;
      if (geolocationMatchesRequest(geolocation, requested, permissionSetting)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last geolocation=${JSON.stringify(summarizeGeolocationReadback(last.geolocation))} requested=${JSON.stringify(requested)} permission=${permissionSetting}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no geolocation readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `geolocationEmulation readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function geolocationMatchesRequest(geolocation, requested, permissionSetting) {
  if (!geolocation) {
    return false;
  }
  if (permissionSetting === "prompt" && !requested) {
    return true;
  }
  if (String(geolocation.permission_state || "") !== permissionSetting) {
    return false;
  }
  if (permissionSetting === "denied") {
    return !geolocation.position && geolocation.error && Number(geolocation.error.code) === 1;
  }
  if (!requested || !geolocation.position) {
    return false;
  }
  return (
    Math.abs(Number(geolocation.position.latitude) - requested.latitude) <= 0.000001 &&
    Math.abs(Number(geolocation.position.longitude) - requested.longitude) <= 0.000001 &&
    Math.abs(Number(geolocation.position.accuracy) - requested.accuracy) <= 0.01 &&
    sameNullableNumber(geolocation.position.altitude, requested.altitude) &&
    sameNullableNumber(geolocation.position.altitude_accuracy, requested.altitude_accuracy) &&
    sameNullableNumber(geolocation.position.heading, requested.heading) &&
    sameNullableNumber(geolocation.position.speed, requested.speed)
  );
}

function sameNullableNumber(left, right) {
  const leftNull = left === null || left === undefined || Number.isNaN(Number(left));
  const rightNull = right === null || right === undefined || Number.isNaN(Number(right));
  if (leftNull || rightNull) {
    return leftNull && rightNull;
  }
  return Math.abs(Number(left) - Number(right)) <= 0.000001;
}

function summarizeGeolocationReadback(geolocation) {
  if (!geolocation) {
    return null;
  }
  return {
    permission_state: geolocation.permission_state,
    position: geolocation.position,
    error: geolocation.error
  };
}

async function waitForLocaleReadback(tabId, fallbackTarget, waitTimeoutMs, requested) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const locale = await tabLocaleState(tabId);
      last = { page, locale };
      lastError = null;
      if (localeMatchesRequest(locale, requested)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last locale=${JSON.stringify(last.locale)} requested=${JSON.stringify(requested)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no locale readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `localeEmulation readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function localeMatchesRequest(locale, requested) {
  if (!locale) {
    return false;
  }
  if (!requested) {
    return true;
  }
  if (requested.timezone_id && String(locale.time_zone || "") !== requested.timezone_id) {
    return false;
  }
  if (requested.locale && !localeReadbackMatchesRequestedLocale(locale.locale, requested.locale)) {
    return false;
  }
  return true;
}

async function waitForLocaleResetReadback(tabId, fallbackTarget, waitTimeoutMs, baseline) {
  if (!baseline) {
    const page = await tabPageState(tabId, fallbackTarget);
    const locale = await tabLocaleState(tabId);
    return { page, locale };
  }
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const locale = await tabLocaleState(tabId);
      last = { page, locale };
      lastError = null;
      if (sameLocaleReadback(locale, baseline)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last locale=${JSON.stringify(last.locale)} baseline=${JSON.stringify(baseline)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no locale readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `localeEmulation reset readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function sameLocaleReadback(left, right) {
  return (
    left &&
    right &&
    String(left.locale || "") === String(right.locale || "") &&
    String(left.calendar || "") === String(right.calendar || "") &&
    String(left.numbering_system || "") === String(right.numbering_system || "") &&
    String(left.time_zone || "") === String(right.time_zone || "") &&
    String(left.sample_number || "") === String(right.sample_number || "") &&
    String(left.sample_date || "") === String(right.sample_date || "") &&
    String(left.date_string || "") === String(right.date_string || "") &&
    Number(left.timezone_offset_minutes) === Number(right.timezone_offset_minutes)
  );
}

function localeReadbackMatchesRequestedLocale(actual, requested) {
  const actualNormalized = String(actual || "").replace(/_/g, "-").toLowerCase();
  const requestedNormalized = String(requested || "").replace(/_/g, "-").toLowerCase();
  return (
    actualNormalized === requestedNormalized ||
    actualNormalized.startsWith(`${requestedNormalized}-`)
  );
}

async function waitForMediaReadback(tabId, fallbackTarget, waitTimeoutMs, requested) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const media = await tabMediaState(tabId);
      last = { page, media };
      lastError = null;
      if (mediaMatchesRequest(media, requested)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last media=${JSON.stringify(last.media)} requested=${JSON.stringify(requested)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no media readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `mediaEmulation readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function mediaMatchesRequest(media, requested) {
  if (!media || !requested) {
    return false;
  }
  if (requested.media === "print" && !media.media_print) {
    return false;
  }
  if (requested.media === "screen" && !media.media_screen) {
    return false;
  }
  if (requested.color_scheme === "dark" && !media.color_scheme_dark) {
    return false;
  }
  if (requested.color_scheme === "light" && !media.color_scheme_light) {
    return false;
  }
  if (requested.color_scheme === "no-preference" && !media.color_scheme_no_preference) {
    return false;
  }
  if (requested.reduced_motion === "reduce" && !media.reduced_motion_reduce) {
    return false;
  }
  if (requested.reduced_motion === "no-preference" && !media.reduced_motion_no_preference) {
    return false;
  }
  return true;
}

async function waitForMediaResetReadback(tabId, fallbackTarget, waitTimeoutMs, baseline) {
  if (!baseline) {
    const page = await tabPageState(tabId, fallbackTarget);
    const media = await tabMediaState(tabId);
    return { page, media };
  }
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const media = await tabMediaState(tabId);
      last = { page, media };
      lastError = null;
      if (sameMediaReadback(media, baseline)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last media=${JSON.stringify(last.media)} baseline=${JSON.stringify(baseline)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no media readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `mediaEmulation reset readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function sameMediaReadback(left, right) {
  return (
    left &&
    right &&
    Boolean(left.media_screen) === Boolean(right.media_screen) &&
    Boolean(left.media_print) === Boolean(right.media_print) &&
    Boolean(left.color_scheme_dark) === Boolean(right.color_scheme_dark) &&
    Boolean(left.color_scheme_light) === Boolean(right.color_scheme_light) &&
    Boolean(left.color_scheme_no_preference) === Boolean(right.color_scheme_no_preference) &&
    Boolean(left.reduced_motion_reduce) === Boolean(right.reduced_motion_reduce) &&
    Boolean(left.reduced_motion_no_preference) === Boolean(right.reduced_motion_no_preference)
  );
}

async function waitForNetworkConditionsReadback(tabId, fallbackTarget, waitTimeoutMs, requested) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const network = await tabNetworkState(tabId);
      last = { page, network };
      lastError = null;
      if (networkConditionsMatchRequest(network, requested)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last network=${JSON.stringify(last.network)} requested=${JSON.stringify(requested)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no network readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `networkConditions readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function networkConditionsMatchRequest(network, requested) {
  if (!network || !requested) {
    return false;
  }
  if (Boolean(network.online) === Boolean(requested.offline)) {
    return false;
  }
  if (requested.connection_type && String(network.connection_type || "") !== requested.connection_type) {
    return false;
  }
  if (requested.latency_ms > 0 && Math.abs(Number(network.rtt_ms || 0) - Number(requested.latency_ms)) > 1) {
    return false;
  }
  if (requested.download_throughput_bytes_per_sec > 0) {
    const expectedDownlink = requested.download_throughput_bytes_per_sec * 8 / 1000000;
    if (Math.abs(Number(network.downlink_mbps || 0) - expectedDownlink) > 0.01) {
      return false;
    }
  }
  return true;
}

async function waitForNetworkConditionsResetReadback(tabId, fallbackTarget, waitTimeoutMs, baseline) {
  if (!baseline) {
    const page = await tabPageState(tabId, fallbackTarget);
    const network = await tabNetworkState(tabId);
    return { page, network };
  }
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      const page = await tabPageState(tabId, fallbackTarget);
      const network = await tabNetworkState(tabId);
      last = { page, network };
      lastError = null;
      if (sameNetworkConditionsReadback(network, baseline)) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `last network=${JSON.stringify(last.network)} baseline=${JSON.stringify(baseline)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
      : "no network readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `networkConditions reset readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

function sameNetworkConditionsReadback(left, right) {
  return (
    left &&
    right &&
    Boolean(left.online) === Boolean(right.online) &&
    String(left.connection_type || "") === String(right.connection_type || "") &&
    String(left.effective_type || "") === String(right.effective_type || "") &&
    optionalNumberSame(left.downlink_mbps, right.downlink_mbps) &&
    optionalNumberSame(left.rtt_ms, right.rtt_ms) &&
    Boolean(left.save_data) === Boolean(right.save_data)
  );
}

function optionalNumberSame(left, right) {
  if ((left === null || left === undefined) && (right === null || right === undefined)) {
    return true;
  }
  return Math.abs(Number(left) - Number(right)) <= 0.01;
}

async function applyViewportDprShim(tabId, deviceScaleFactor) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: installViewportDprShimInPage,
      args: [deviceScaleFactor]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript viewportEmulation(${tabId}) devicePixelRatio shim failed: ${errorMessage(error)}`
    );
  }
}

async function applyDeviceEmulationShim(tabId, descriptor) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: installDeviceEmulationShimInPage,
      args: [descriptor]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript deviceEmulation(${tabId}) device shim failed: ${errorMessage(error)}`
    );
  }
}

async function clearDeviceEmulationShim(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: clearDeviceEmulationShimInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript deviceEmulation(${tabId}) device shim reset failed: ${errorMessage(error)}`
    );
  }
}

async function applyGeolocationEmulationShim(tabId, state) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: installGeolocationEmulationShimInPage,
      args: [state]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript geolocationEmulation(${tabId}) geolocation shim failed: ${errorMessage(error)}`
    );
  }
}

async function clearGeolocationEmulationShim(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: clearGeolocationEmulationShimInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript geolocationEmulation(${tabId}) geolocation shim reset failed: ${errorMessage(error)}`
    );
  }
}

async function applyLocaleEmulationShim(tabId, requested) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: installLocaleEmulationShimInPage,
      args: [requested]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript localeEmulation(${tabId}) locale shim failed: ${errorMessage(error)}`
    );
  }
}

async function clearLocaleEmulationShim(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: clearLocaleEmulationShimInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript localeEmulation(${tabId}) locale shim reset failed: ${errorMessage(error)}`
    );
  }
}

async function applyMediaEmulationShim(tabId, requested) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: installMediaEmulationShimInPage,
      args: [requested]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript mediaEmulation(${tabId}) media shim failed: ${errorMessage(error)}`
    );
  }
}

async function clearMediaEmulationShim(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: clearMediaEmulationShimInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript mediaEmulation(${tabId}) media shim reset failed: ${errorMessage(error)}`
    );
  }
}

async function applyNetworkConditionsShim(tabId, requested) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: installNetworkConditionsShimInPage,
      args: [requested]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript networkConditions(${tabId}) network shim failed: ${errorMessage(error)}`
    );
  }
}

async function clearNetworkConditionsShim(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: clearNetworkConditionsShimInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript networkConditions(${tabId}) network shim reset failed: ${errorMessage(error)}`
    );
  }
}

async function clearViewportDprShim(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  try {
    await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: clearViewportDprShimInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript viewportEmulation(${tabId}) devicePixelRatio shim reset failed: ${errorMessage(error)}`
    );
  }
}

async function reloadViewportTabAfterReset(tabId, fallbackTarget, waitTimeoutMs) {
  try {
    await chrome.tabs.reload(tabId, { bypassCache: true });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `chrome.tabs.reload(${tabId}) after viewportEmulation reset failed: ${errorMessage(error)}`
    );
  }
  await waitForTabPageState(tabId, fallbackTarget, waitTimeoutMs);
  return {
    method: "chrome.tabs.reload(bypassCache=true)"
  };
}

function viewportEmulationMethod(dispatch, resetFallback, resetBaselineRestore) {
  const parts = [dispatch.method];
  if (resetFallback) {
    parts.push(`baselineReload:${resetFallback.method}`);
  }
  if (resetBaselineRestore) {
    parts.push(`baselineMetricsRestore:${resetBaselineRestore.method}`);
  }
  return parts.join("+");
}

function viewportMatchesRequest(viewport, requested) {
  return (
    viewport &&
    viewport.inner_width === requested.width &&
    viewport.inner_height === requested.height &&
    Math.abs(Number(viewport.device_pixel_ratio) - requested.device_scale_factor) <= 0.01
  );
}

function viewportRequestFromBaseline(baseline) {
  if (
    !baseline ||
    !Number.isSafeInteger(baseline.inner_width) ||
    !Number.isSafeInteger(baseline.inner_height) ||
    baseline.inner_width <= 0 ||
    baseline.inner_height <= 0
  ) {
    return null;
  }
  const deviceScaleFactor = Number(baseline.device_pixel_ratio);
  return {
    width: baseline.inner_width,
    height: baseline.inner_height,
    device_scale_factor: Number.isFinite(deviceScaleFactor) && deviceScaleFactor > 0
      ? deviceScaleFactor
      : 1,
    screen_width: Number.isSafeInteger(baseline.screen_width) && baseline.screen_width > 0
      ? baseline.screen_width
      : baseline.inner_width,
    screen_height: Number.isSafeInteger(baseline.screen_height) && baseline.screen_height > 0
      ? baseline.screen_height
      : baseline.inner_height,
    mobile: false
  };
}

function sameViewportMetrics(left, right) {
  return (
    left &&
    right &&
    left.inner_width === right.inner_width &&
    left.inner_height === right.inner_height &&
    Math.abs(Number(left.device_pixel_ratio) - Number(right.device_pixel_ratio)) <= 0.01
  );
}

function deviceMatchesDescriptor(device, descriptor) {
  if (!device || !descriptor) {
    return false;
  }
  return (
    viewportMatchesRequest(device.viewport, {
      width: descriptor.width,
      height: descriptor.height,
      device_scale_factor: descriptor.device_scale_factor
    }) &&
    device.user_agent === descriptor.user_agent &&
    Number(device.max_touch_points) === descriptor.max_touch_points &&
    Boolean(device.ontouchstart_available) === descriptor.has_touch &&
    Boolean(device.pointer_coarse) === descriptor.has_touch &&
    Boolean(device.any_pointer_coarse) === descriptor.has_touch &&
    Boolean(device.hover_none) === descriptor.has_touch &&
    Boolean(device.any_hover_none) === descriptor.has_touch
  );
}

function sameDeviceMetrics(left, right) {
  return (
    left &&
    right &&
    sameViewportMetrics(left.viewport, right.viewport) &&
    left.user_agent === right.user_agent &&
    Number(left.max_touch_points) === Number(right.max_touch_points) &&
    Boolean(left.ontouchstart_available) === Boolean(right.ontouchstart_available) &&
    Boolean(left.pointer_coarse) === Boolean(right.pointer_coarse) &&
    Boolean(left.any_pointer_coarse) === Boolean(right.any_pointer_coarse) &&
    Boolean(left.hover_none) === Boolean(right.hover_none) &&
    Boolean(left.any_hover_none) === Boolean(right.any_hover_none)
  );
}

function deviceMatchesBaseline(device, baseline) {
  return sameDeviceMetrics(device, baseline);
}

function summarizeDeviceReadback(device) {
  if (!device) {
    return null;
  }
  return {
    viewport: device.viewport,
    user_agent: device.user_agent,
    max_touch_points: device.max_touch_points,
    ontouchstart_available: device.ontouchstart_available,
    pointer_coarse: device.pointer_coarse,
    any_pointer_coarse: device.any_pointer_coarse,
    hover_none: device.hover_none,
    any_hover_none: device.any_hover_none
  };
}

async function tabViewportMetricsState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: readViewportMetricsInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript viewportEmulation(${tabId}) readback failed: ${errorMessage(error)}`
    );
  }
  const metrics = results?.[0]?.result;
  if (!metrics || typeof metrics !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript viewportEmulation(${tabId}) returned no viewport metrics`
    );
  }
  return metrics;
}

async function tabDeviceMetricsState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: readDeviceMetricsInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript deviceEmulation(${tabId}) readback failed: ${errorMessage(error)}`
    );
  }
  const metrics = results?.[0]?.result;
  if (!metrics || typeof metrics !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript deviceEmulation(${tabId}) returned no device metrics`
    );
  }
  return metrics;
}

async function tabGeolocationState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: readGeolocationStateInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript geolocationEmulation(${tabId}) readback failed: ${errorMessage(error)}`
    );
  }
  const metrics = results?.[0]?.result;
  if (!metrics || typeof metrics !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript geolocationEmulation(${tabId}) returned no geolocation metrics`
    );
  }
  return metrics;
}

async function tabLocaleState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: readLocaleStateInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript localeEmulation(${tabId}) readback failed: ${errorMessage(error)}`
    );
  }
  const metrics = results?.[0]?.result;
  if (!metrics || typeof metrics !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript localeEmulation(${tabId}) returned no locale metrics`
    );
  }
  return metrics;
}

async function tabMediaState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: readMediaStateInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript mediaEmulation(${tabId}) readback failed: ${errorMessage(error)}`
    );
  }
  const metrics = results?.[0]?.result;
  if (!metrics || typeof metrics !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript mediaEmulation(${tabId}) returned no media metrics`
    );
  }
  return metrics;
}

async function tabNetworkState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: false },
      world: "MAIN",
      func: readNetworkConditionsStateInPage
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript networkConditions(${tabId}) readback failed: ${errorMessage(error)}`
    );
  }
  const metrics = results?.[0]?.result;
  if (!metrics || typeof metrics !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript networkConditions(${tabId}) returned no network metrics`
    );
  }
  return metrics;
}

function installViewportDprShimInPage(deviceScaleFactor) {
  const requested = Number(deviceScaleFactor);
  if (!Number.isFinite(requested) || requested <= 0) {
    throw new Error(`invalid deviceScaleFactor for Synapse viewport DPR shim: ${String(deviceScaleFactor)}`);
  }
  const stateKey = "__synapseViewportDprOverride";
  let state = globalThis[stateKey];
  if (!state || typeof state !== "object") {
    state = {
      original_had_own_descriptor: Object.prototype.hasOwnProperty.call(globalThis, "devicePixelRatio"),
      original_own_descriptor: Object.getOwnPropertyDescriptor(globalThis, "devicePixelRatio") || null,
      original_device_pixel_ratio: Number(globalThis.devicePixelRatio) || 1,
      value: requested
    };
    Object.defineProperty(globalThis, stateKey, {
      configurable: true,
      enumerable: false,
      writable: true,
      value: state
    });
  }
  state.value = requested;
  Object.defineProperty(globalThis, "devicePixelRatio", {
    configurable: true,
    enumerable: true,
    get() {
      const current = globalThis[stateKey];
      const value = Number(current && current.value);
      if (Number.isFinite(value) && value > 0) {
        return value;
      }
      const original = Number(current && current.original_device_pixel_ratio);
      return Number.isFinite(original) && original > 0 ? original : 1;
    }
  });
  globalThis.dispatchEvent(new Event("resize"));
  return {
    applied: true,
    requested_device_pixel_ratio: requested,
    original_device_pixel_ratio: state.original_device_pixel_ratio
  };
}

function clearViewportDprShimInPage() {
  const stateKey = "__synapseViewportDprOverride";
  const state = globalThis[stateKey];
  if (state && typeof state === "object") {
    if (state.original_had_own_descriptor && state.original_own_descriptor) {
      Object.defineProperty(globalThis, "devicePixelRatio", state.original_own_descriptor);
    } else {
      delete globalThis.devicePixelRatio;
    }
    delete globalThis[stateKey];
  }
  globalThis.dispatchEvent(new Event("resize"));
  return {
    cleared: Boolean(state),
    device_pixel_ratio: Number(globalThis.devicePixelRatio) || 1
  };
}

function installDeviceEmulationShimInPage(descriptor) {
  const stateKey = "__synapseDeviceEmulationOverride";
  const requested = descriptor && typeof descriptor === "object" ? descriptor : {};
  const state = globalThis[stateKey] && typeof globalThis[stateKey] === "object"
    ? globalThis[stateKey]
    : {
      original_global_device_pixel_ratio_had_own_descriptor: Object.prototype.hasOwnProperty.call(globalThis, "devicePixelRatio"),
      original_global_device_pixel_ratio_descriptor: Object.getOwnPropertyDescriptor(globalThis, "devicePixelRatio") || null,
      original_navigator_user_agent_descriptor: Object.getOwnPropertyDescriptor(Navigator.prototype, "userAgent") || null,
      original_navigator_max_touch_points_descriptor: Object.getOwnPropertyDescriptor(Navigator.prototype, "maxTouchPoints") || null,
      original_global_ontouchstart_had_own_descriptor: Object.prototype.hasOwnProperty.call(globalThis, "ontouchstart"),
      original_global_ontouchstart_descriptor: Object.getOwnPropertyDescriptor(globalThis, "ontouchstart") || null,
      original_match_media: typeof globalThis.matchMedia === "function" ? globalThis.matchMedia : null,
      value: {}
    };
  state.value = {
    device_pixel_ratio: Number(requested.device_scale_factor) || 1,
    user_agent: String(requested.user_agent || ""),
    has_touch: Boolean(requested.has_touch),
    max_touch_points: Number.isFinite(Number(requested.max_touch_points))
      ? Number(requested.max_touch_points)
      : 0
  };
  Object.defineProperty(globalThis, stateKey, {
    configurable: true,
    enumerable: false,
    writable: true,
    value: state
  });
  Object.defineProperty(globalThis, "devicePixelRatio", {
    configurable: true,
    enumerable: true,
    get() {
      const current = globalThis[stateKey];
      const value = Number(current && current.value && current.value.device_pixel_ratio);
      return Number.isFinite(value) && value > 0 ? value : 1;
    }
  });
  Object.defineProperty(Navigator.prototype, "userAgent", {
    configurable: true,
    enumerable: true,
    get() {
      const current = globalThis[stateKey];
      return String(current && current.value && current.value.user_agent || "");
    }
  });
  Object.defineProperty(Navigator.prototype, "maxTouchPoints", {
    configurable: true,
    enumerable: true,
    get() {
      const current = globalThis[stateKey];
      const value = Number(current && current.value && current.value.max_touch_points);
      return Number.isFinite(value) && value >= 0 ? value : 0;
    }
  });
  if (state.value.has_touch) {
    Object.defineProperty(globalThis, "ontouchstart", {
      configurable: true,
      enumerable: true,
      writable: true,
      value: null
    });
  } else if (state.original_global_ontouchstart_had_own_descriptor && state.original_global_ontouchstart_descriptor) {
    Object.defineProperty(globalThis, "ontouchstart", state.original_global_ontouchstart_descriptor);
  } else {
    try {
      delete globalThis.ontouchstart;
    } catch (_) {}
  }
  const nativeMatchMedia = state.original_match_media;
  globalThis.matchMedia = function synapseDeviceMatchMedia(query) {
    function mediaQueryList(media, matches) {
      return {
        matches: Boolean(matches),
        media: String(media || ""),
        onchange: null,
        addListener() {},
        removeListener() {},
        addEventListener() {},
        removeEventListener() {},
        dispatchEvent() {
          return false;
        }
      };
    }
    const normalized = String(query || "").replace(/\s+/g, "").toLowerCase();
    const current = globalThis[stateKey];
    const hasTouch = Boolean(current && current.value && current.value.has_touch);
    if (
      normalized === "(pointer:coarse)" ||
      normalized === "(any-pointer:coarse)" ||
      normalized === "(hover:none)" ||
      normalized === "(any-hover:none)"
    ) {
      return mediaQueryList(query, hasTouch);
    }
    if (typeof nativeMatchMedia === "function") {
      return nativeMatchMedia.call(this, query);
    }
    return mediaQueryList(query, false);
  };
  globalThis.dispatchEvent(new Event("resize"));
  return {
    applied: true,
    user_agent: state.value.user_agent,
    max_touch_points: state.value.max_touch_points,
    has_touch: state.value.has_touch,
    device_pixel_ratio: state.value.device_pixel_ratio
  };
}

function clearDeviceEmulationShimInPage() {
  const stateKey = "__synapseDeviceEmulationOverride";
  const state = globalThis[stateKey];
  if (state && typeof state === "object") {
    if (state.original_global_device_pixel_ratio_had_own_descriptor && state.original_global_device_pixel_ratio_descriptor) {
      Object.defineProperty(globalThis, "devicePixelRatio", state.original_global_device_pixel_ratio_descriptor);
    } else {
      delete globalThis.devicePixelRatio;
    }
    if (state.original_navigator_user_agent_descriptor) {
      Object.defineProperty(Navigator.prototype, "userAgent", state.original_navigator_user_agent_descriptor);
    }
    if (state.original_navigator_max_touch_points_descriptor) {
      Object.defineProperty(Navigator.prototype, "maxTouchPoints", state.original_navigator_max_touch_points_descriptor);
    }
    if (state.original_global_ontouchstart_had_own_descriptor && state.original_global_ontouchstart_descriptor) {
      Object.defineProperty(globalThis, "ontouchstart", state.original_global_ontouchstart_descriptor);
    } else {
      try {
        delete globalThis.ontouchstart;
      } catch (_) {}
    }
    if (typeof state.original_match_media === "function") {
      globalThis.matchMedia = state.original_match_media;
    }
    delete globalThis[stateKey];
  }
  globalThis.dispatchEvent(new Event("resize"));
  return {
    cleared: Boolean(state),
    user_agent: String(globalThis.navigator ? globalThis.navigator.userAgent || "" : ""),
    max_touch_points: Number(globalThis.navigator ? globalThis.navigator.maxTouchPoints || 0 : 0)
  };
}

function installGeolocationEmulationShimInPage(state) {
  const stateKey = "__synapseGeolocationEmulationOverride";
  const requestedState = state && typeof state === "object" ? state : {};
  const requested = requestedState.requested && typeof requestedState.requested === "object"
    ? requestedState.requested
    : {};
  let shimState = globalThis[stateKey];
  if (!shimState || typeof shimState !== "object") {
    const permissions = globalThis.navigator && globalThis.navigator.permissions
      ? globalThis.navigator.permissions
      : null;
    const geolocation = globalThis.navigator && globalThis.navigator.geolocation
      ? globalThis.navigator.geolocation
      : null;
    shimState = {
      permissions,
      permissions_query_had_own_descriptor: permissions ? Object.prototype.hasOwnProperty.call(permissions, "query") : false,
      permissions_query_descriptor: permissions ? Object.getOwnPropertyDescriptor(permissions, "query") || null : null,
      permissions_query_original: permissions && typeof permissions.query === "function"
        ? permissions.query.bind(permissions)
        : null,
      geolocation,
      geolocation_get_had_own_descriptor: geolocation ? Object.prototype.hasOwnProperty.call(geolocation, "getCurrentPosition") : false,
      geolocation_get_descriptor: geolocation ? Object.getOwnPropertyDescriptor(geolocation, "getCurrentPosition") || null : null,
      geolocation_get_original: geolocation && typeof geolocation.getCurrentPosition === "function"
        ? geolocation.getCurrentPosition.bind(geolocation)
        : null,
      geolocation_watch_had_own_descriptor: geolocation ? Object.prototype.hasOwnProperty.call(geolocation, "watchPosition") : false,
      geolocation_watch_descriptor: geolocation ? Object.getOwnPropertyDescriptor(geolocation, "watchPosition") || null : null,
      geolocation_watch_original: geolocation && typeof geolocation.watchPosition === "function"
        ? geolocation.watchPosition.bind(geolocation)
        : null,
      geolocation_clear_had_own_descriptor: geolocation ? Object.prototype.hasOwnProperty.call(geolocation, "clearWatch") : false,
      geolocation_clear_descriptor: geolocation ? Object.getOwnPropertyDescriptor(geolocation, "clearWatch") || null : null,
      geolocation_clear_original: geolocation && typeof geolocation.clearWatch === "function"
        ? geolocation.clearWatch.bind(geolocation)
        : null,
      next_watch_id: 1,
      value: {}
    };
    Object.defineProperty(globalThis, stateKey, {
      configurable: true,
      enumerable: false,
      writable: true,
      value: shimState
    });
  }
  shimState.value = {
    permission_state: String(requestedState.permission_state || "granted"),
    requested: {
      latitude: Number(requested.latitude),
      longitude: Number(requested.longitude),
      accuracy: Number(requested.accuracy || 0),
      altitude: requested.altitude === null || requested.altitude === undefined ? null : Number(requested.altitude),
      altitude_accuracy: requested.altitude_accuracy === null || requested.altitude_accuracy === undefined ? null : Number(requested.altitude_accuracy),
      heading: requested.heading === null || requested.heading === undefined ? null : Number(requested.heading),
      speed: requested.speed === null || requested.speed === undefined ? null : Number(requested.speed)
    }
  };
  function currentState() {
    const current = globalThis[stateKey];
    return current && current.value ? current.value : shimState.value;
  }
  function permissionStatus(permissionState) {
    return {
      state: String(permissionState || "prompt"),
      onchange: null,
      addEventListener() {},
      removeEventListener() {},
      dispatchEvent() {
        return false;
      }
    };
  }
  function geolocationError() {
    return {
      code: 1,
      message: "User denied Geolocation",
      PERMISSION_DENIED: 1,
      POSITION_UNAVAILABLE: 2,
      TIMEOUT: 3
    };
  }
  function geolocationPosition() {
    const value = currentState().requested || {};
    return {
      coords: {
        latitude: Number(value.latitude),
        longitude: Number(value.longitude),
        accuracy: Number(value.accuracy || 0),
        altitude: value.altitude === null || value.altitude === undefined ? null : Number(value.altitude),
        altitudeAccuracy: value.altitude_accuracy === null || value.altitude_accuracy === undefined ? null : Number(value.altitude_accuracy),
        heading: value.heading === null || value.heading === undefined ? null : Number(value.heading),
        speed: value.speed === null || value.speed === undefined ? null : Number(value.speed)
      },
      timestamp: Date.now()
    };
  }
  if (shimState.permissions && typeof shimState.permissions.query === "function") {
    Object.defineProperty(shimState.permissions, "query", {
      configurable: true,
      enumerable: false,
      writable: true,
      value(descriptor) {
        const name = descriptor && typeof descriptor === "object" ? String(descriptor.name || "") : "";
        if (name.toLowerCase() === "geolocation") {
          return Promise.resolve(permissionStatus(currentState().permission_state));
        }
        if (typeof shimState.permissions_query_original === "function") {
          return shimState.permissions_query_original(descriptor);
        }
        return Promise.reject(new TypeError("navigator.permissions.query unavailable"));
      }
    });
  }
  if (shimState.geolocation) {
    Object.defineProperty(shimState.geolocation, "getCurrentPosition", {
      configurable: true,
      enumerable: false,
      writable: true,
      value(success, error) {
        globalThis.setTimeout(() => {
          if (currentState().permission_state === "denied") {
            if (typeof error === "function") {
              error(geolocationError());
            }
            return;
          }
          if (typeof success === "function") {
            success(geolocationPosition());
          }
        }, 0);
      }
    });
    Object.defineProperty(shimState.geolocation, "watchPosition", {
      configurable: true,
      enumerable: false,
      writable: true,
      value(success, error) {
        const watchId = shimState.next_watch_id++;
        globalThis.setTimeout(() => {
          if (currentState().permission_state === "denied") {
            if (typeof error === "function") {
              error(geolocationError());
            }
            return;
          }
          if (typeof success === "function") {
            success(geolocationPosition());
          }
        }, 0);
        return watchId;
      }
    });
    Object.defineProperty(shimState.geolocation, "clearWatch", {
      configurable: true,
      enumerable: false,
      writable: true,
      value(watchId) {
        if (typeof shimState.geolocation_clear_original === "function") {
          try {
            shimState.geolocation_clear_original(watchId);
          } catch (_) {}
        }
      }
    });
  }
  return {
    applied: true,
    permission_state: shimState.value.permission_state,
    requested: shimState.value.requested
  };
}

function clearGeolocationEmulationShimInPage() {
  const stateKey = "__synapseGeolocationEmulationOverride";
  const state = globalThis[stateKey];
  function restoreOwn(target, field, hadOwn, descriptor) {
    if (!target) {
      return;
    }
    if (hadOwn && descriptor) {
      Object.defineProperty(target, field, descriptor);
    } else {
      try {
        delete target[field];
      } catch (_) {}
    }
  }
  if (state && typeof state === "object") {
    restoreOwn(state.permissions, "query", state.permissions_query_had_own_descriptor, state.permissions_query_descriptor);
    restoreOwn(state.geolocation, "getCurrentPosition", state.geolocation_get_had_own_descriptor, state.geolocation_get_descriptor);
    restoreOwn(state.geolocation, "watchPosition", state.geolocation_watch_had_own_descriptor, state.geolocation_watch_descriptor);
    restoreOwn(state.geolocation, "clearWatch", state.geolocation_clear_had_own_descriptor, state.geolocation_clear_descriptor);
    delete globalThis[stateKey];
  }
  return {
    cleared: Boolean(state)
  };
}

function installLocaleEmulationShimInPage(requested) {
  const stateKey = "__synapseLocaleEmulationOverride";
  const requestedState = requested && typeof requested === "object" ? requested : {};
  const locale = requestedState.locale === null || requestedState.locale === undefined
    ? null
    : String(requestedState.locale);
  const timezoneId = requestedState.timezone_id === null || requestedState.timezone_id === undefined
    ? null
    : String(requestedState.timezone_id);
  let state = globalThis[stateKey];
  if (!state || typeof state !== "object") {
    const nativeIntl = globalThis.Intl || {};
    const nativeDate = globalThis.Date;
    const datePrototype = nativeDate && nativeDate.prototype ? nativeDate.prototype : null;
    state = {
      native_intl: nativeIntl,
      native_date_time_format: nativeIntl.DateTimeFormat,
      native_number_format: nativeIntl.NumberFormat,
      date_prototype: datePrototype,
      date_to_string_had_own_descriptor: datePrototype ? Object.prototype.hasOwnProperty.call(datePrototype, "toString") : false,
      date_to_string_descriptor: datePrototype ? Object.getOwnPropertyDescriptor(datePrototype, "toString") || null : null,
      date_get_timezone_offset_had_own_descriptor: datePrototype ? Object.prototype.hasOwnProperty.call(datePrototype, "getTimezoneOffset") : false,
      date_get_timezone_offset_descriptor: datePrototype ? Object.getOwnPropertyDescriptor(datePrototype, "getTimezoneOffset") || null : null,
      value: {}
    };
    Object.defineProperty(globalThis, stateKey, {
      configurable: true,
      enumerable: false,
      writable: true,
      value: state
    });
  }
  state.value = { locale, timezone_id: timezoneId };
  function currentState() {
    const current = globalThis[stateKey];
    return current && current.value ? current.value : state.value;
  }
  function localeOr(locales) {
    const value = currentState();
    return locales === undefined || locales === null ? value.locale || undefined : locales;
  }
  function optionsOr(options, includeTimeZone) {
    const value = currentState();
    const next = options && typeof options === "object" ? { ...options } : {};
    if (includeTimeZone && !Object.prototype.hasOwnProperty.call(next, "timeZone") && value.timezone_id) {
      next.timeZone = value.timezone_id;
    }
    return next;
  }
  if (typeof state.native_date_time_format === "function") {
    const NativeDateTimeFormat = state.native_date_time_format;
    function SynapseDateTimeFormat(locales, options) {
      return new NativeDateTimeFormat(localeOr(locales), optionsOr(options, true));
    }
    Object.defineProperty(SynapseDateTimeFormat, "prototype", {
      configurable: false,
      enumerable: false,
      writable: false,
      value: NativeDateTimeFormat.prototype
    });
    if (typeof NativeDateTimeFormat.supportedLocalesOf === "function") {
      Object.defineProperty(SynapseDateTimeFormat, "supportedLocalesOf", {
        configurable: true,
        enumerable: false,
        writable: true,
        value(locales, options) {
          return NativeDateTimeFormat.supportedLocalesOf(locales, options);
        }
      });
    }
    state.native_intl.DateTimeFormat = SynapseDateTimeFormat;
  }
  if (typeof state.native_number_format === "function") {
    const NativeNumberFormat = state.native_number_format;
    function SynapseNumberFormat(locales, options) {
      return new NativeNumberFormat(localeOr(locales), optionsOr(options, false));
    }
    Object.defineProperty(SynapseNumberFormat, "prototype", {
      configurable: false,
      enumerable: false,
      writable: false,
      value: NativeNumberFormat.prototype
    });
    if (typeof NativeNumberFormat.supportedLocalesOf === "function") {
      Object.defineProperty(SynapseNumberFormat, "supportedLocalesOf", {
        configurable: true,
        enumerable: false,
        writable: true,
        value(locales, options) {
          return NativeNumberFormat.supportedLocalesOf(locales, options);
        }
      });
    }
    state.native_intl.NumberFormat = SynapseNumberFormat;
  }
  function timeZoneParts(date, timezone) {
    const formatter = new state.native_date_time_format("en-US", {
      timeZone: timezone,
      hour12: false,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      timeZoneName: "long"
    });
    const parts = {};
    for (const part of formatter.formatToParts(date)) {
      if (part && part.type && part.type !== "literal") {
        parts[part.type] = part.value;
      }
    }
    let hour = Number(parts.hour || 0);
    if (hour === 24) {
      hour = 0;
    }
    return {
      year: Number(parts.year || 0),
      month: Number(parts.month || 1),
      day: Number(parts.day || 1),
      hour,
      minute: Number(parts.minute || 0),
      second: Number(parts.second || 0),
      time_zone_name: String(parts.timeZoneName || timezone)
    };
  }
  function timezoneOffsetMinutes(date, timezone) {
    const parts = timeZoneParts(date, timezone);
    const localAsUtc = Date.UTC(
      parts.year,
      parts.month - 1,
      parts.day,
      parts.hour,
      parts.minute,
      parts.second
    );
    return -Math.round((localAsUtc - date.getTime()) / 60000);
  }
  function pad2(value) {
    return String(Math.trunc(Math.abs(Number(value) || 0))).padStart(2, "0");
  }
  function dateStringInTimezone(date, timezone) {
    const parts = timeZoneParts(date, timezone);
    const offset = timezoneOffsetMinutes(date, timezone);
    const dayNames = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const monthNames = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const dayName = dayNames[new Date(Date.UTC(parts.year, parts.month - 1, parts.day)).getUTCDay()] || "Thu";
    const monthName = monthNames[Math.max(0, Math.min(11, parts.month - 1))] || "Jan";
    const gmtSign = offset <= 0 ? "+" : "-";
    const absolute = Math.abs(offset);
    const gmt = `GMT${gmtSign}${pad2(Math.floor(absolute / 60))}${pad2(absolute % 60)}`;
    return `${dayName} ${monthName} ${pad2(parts.day)} ${parts.year} ${pad2(parts.hour)}:${pad2(parts.minute)}:${pad2(parts.second)} ${gmt} (${parts.time_zone_name})`;
  }
  if (state.date_prototype) {
    Object.defineProperty(state.date_prototype, "getTimezoneOffset", {
      configurable: true,
      enumerable: false,
      writable: true,
      value() {
        const timezone = currentState().timezone_id;
        if (timezone) {
          return timezoneOffsetMinutes(this, timezone);
        }
        const descriptor = state.date_get_timezone_offset_descriptor;
        if (descriptor && typeof descriptor.value === "function") {
          return descriptor.value.call(this);
        }
        return 0;
      }
    });
    Object.defineProperty(state.date_prototype, "toString", {
      configurable: true,
      enumerable: false,
      writable: true,
      value() {
        const timezone = currentState().timezone_id;
        if (timezone) {
          return dateStringInTimezone(this, timezone);
        }
        const descriptor = state.date_to_string_descriptor;
        if (descriptor && typeof descriptor.value === "function") {
          return descriptor.value.call(this);
        }
        return "[object Date]";
      }
    });
  }
  return {
    applied: true,
    requested: state.value
  };
}

function clearLocaleEmulationShimInPage() {
  const stateKey = "__synapseLocaleEmulationOverride";
  const state = globalThis[stateKey];
  function restoreOwn(target, field, hadOwn, descriptor) {
    if (!target) {
      return;
    }
    if (hadOwn && descriptor) {
      Object.defineProperty(target, field, descriptor);
    } else {
      try {
        delete target[field];
      } catch (_) {}
    }
  }
  if (state && typeof state === "object") {
    if (state.native_intl && state.native_date_time_format) {
      state.native_intl.DateTimeFormat = state.native_date_time_format;
    }
    if (state.native_intl && state.native_number_format) {
      state.native_intl.NumberFormat = state.native_number_format;
    }
    restoreOwn(state.date_prototype, "toString", state.date_to_string_had_own_descriptor, state.date_to_string_descriptor);
    restoreOwn(state.date_prototype, "getTimezoneOffset", state.date_get_timezone_offset_had_own_descriptor, state.date_get_timezone_offset_descriptor);
    delete globalThis[stateKey];
  }
  return {
    cleared: Boolean(state)
  };
}

function installMediaEmulationShimInPage(requested) {
  const stateKey = "__synapseMediaEmulationOverride";
  const requestedState = requested && typeof requested === "object" ? requested : {};
  const media = requestedState.media === null || requestedState.media === undefined
    ? null
    : String(requestedState.media);
  const colorScheme = requestedState.color_scheme === null || requestedState.color_scheme === undefined
    ? null
    : String(requestedState.color_scheme);
  const reducedMotion = requestedState.reduced_motion === null || requestedState.reduced_motion === undefined
    ? null
    : String(requestedState.reduced_motion);
  let state = globalThis[stateKey];
  if (!state || typeof state !== "object") {
    state = {
      original_match_media: typeof globalThis.matchMedia === "function"
        ? globalThis.matchMedia.bind(globalThis)
        : null,
      match_media_had_own_descriptor: Object.prototype.hasOwnProperty.call(globalThis, "matchMedia"),
      match_media_descriptor: Object.getOwnPropertyDescriptor(globalThis, "matchMedia") || null,
      style_element_id: "__synapseMediaEmulationCssOverride",
      value: {}
    };
    Object.defineProperty(globalThis, stateKey, {
      configurable: true,
      enumerable: false,
      writable: true,
      value: state
    });
  }
  state.value = { media, color_scheme: colorScheme, reduced_motion: reducedMotion };
  function normalizeQuery(query) {
    return String(query || "").trim().toLowerCase().replace(/\s+/g, " ");
  }
  function currentState() {
    const current = globalThis[stateKey];
    return current && current.value ? current.value : state.value;
  }
  function overrideMatch(query) {
    const value = currentState();
    const normalized = normalizeQuery(query);
    if (value.media === "print") {
      if (normalized === "print") {
        return true;
      }
      if (normalized === "screen") {
        return false;
      }
    } else if (value.media === "screen") {
      if (normalized === "screen") {
        return true;
      }
      if (normalized === "print") {
        return false;
      }
    }
    if (value.color_scheme === "dark" && normalized === "(prefers-color-scheme: dark)") {
      return true;
    }
    if (value.color_scheme === "dark" && (normalized === "(prefers-color-scheme: light)" || normalized === "(prefers-color-scheme: no-preference)")) {
      return false;
    }
    if (value.color_scheme === "light" && normalized === "(prefers-color-scheme: light)") {
      return true;
    }
    if (value.color_scheme === "light" && (normalized === "(prefers-color-scheme: dark)" || normalized === "(prefers-color-scheme: no-preference)")) {
      return false;
    }
    if (value.color_scheme === "no-preference" && normalized === "(prefers-color-scheme: no-preference)") {
      return true;
    }
    if (value.color_scheme === "no-preference" && (normalized === "(prefers-color-scheme: dark)" || normalized === "(prefers-color-scheme: light)")) {
      return false;
    }
    if (value.reduced_motion === "reduce" && normalized === "(prefers-reduced-motion: reduce)") {
      return true;
    }
    if (value.reduced_motion === "reduce" && normalized === "(prefers-reduced-motion: no-preference)") {
      return false;
    }
    if (value.reduced_motion === "no-preference" && normalized === "(prefers-reduced-motion: no-preference)") {
      return true;
    }
    if (value.reduced_motion === "no-preference" && normalized === "(prefers-reduced-motion: reduce)") {
      return false;
    }
    return null;
  }
  function nativeMatch(query) {
    if (typeof state.original_match_media !== "function") {
      return null;
    }
    try {
      return state.original_match_media(query);
    } catch (_) {
      return null;
    }
  }
  function mediaQueryList(query, forcedMatch, nativeResult) {
    const mediaText = String(query || "");
    const listeners = [];
    const result = {
      matches: Boolean(forcedMatch),
      media: nativeResult && typeof nativeResult.media === "string" ? nativeResult.media : mediaText,
      onchange: null,
      addListener(listener) {
        if (typeof listener === "function" && !listeners.includes(listener)) {
          listeners.push(listener);
        }
      },
      removeListener(listener) {
        const index = listeners.indexOf(listener);
        if (index >= 0) {
          listeners.splice(index, 1);
        }
      },
      addEventListener(_type, listener) {
        this.addListener(listener);
      },
      removeEventListener(_type, listener) {
        this.removeListener(listener);
      },
      dispatchEvent(event) {
        for (const listener of listeners.slice()) {
          try {
            listener.call(result, event || result);
          } catch (_) {}
        }
        if (typeof result.onchange === "function") {
          try {
            result.onchange.call(result, event || result);
          } catch (_) {}
        }
        return true;
      }
    };
    return result;
  }
  Object.defineProperty(globalThis, "matchMedia", {
    configurable: true,
    enumerable: true,
    writable: true,
    value(query) {
      const forced = overrideMatch(query);
      const nativeResult = nativeMatch(query);
      if (forced === null && nativeResult) {
        return nativeResult;
      }
      return mediaQueryList(query, forced === null ? false : forced, nativeResult);
    }
  });
  function removeMediaStyleElement() {
    try {
      const existing = globalThis.document && document.getElementById(state.style_element_id);
      if (existing && existing.parentNode) {
        existing.parentNode.removeChild(existing);
      }
    } catch (_) {}
  }
  function collectMatchedRules(ruleList, output) {
    if (!ruleList) {
      return;
    }
    for (const rule of Array.from(ruleList)) {
      try {
        const condition = rule && (
          typeof rule.conditionText === "string"
            ? rule.conditionText
            : rule.media && typeof rule.media.mediaText === "string"
              ? rule.media.mediaText
              : ""
        );
        if (condition && rule.cssRules) {
          let matches = false;
          try {
            matches = Boolean(globalThis.matchMedia && globalThis.matchMedia(condition).matches);
          } catch (_) {
            matches = false;
          }
          if (matches) {
            collectMatchedRules(rule.cssRules, output);
          }
        } else if (rule && typeof rule.cssText === "string") {
          output.push(rule.cssText);
        }
      } catch (_) {}
    }
  }
  function applyMatchedMediaStyleRules() {
    removeMediaStyleElement();
    if (!globalThis.document || !document.documentElement || !document.createElement) {
      return 0;
    }
    const rules = [];
    for (const sheet of Array.from(document.styleSheets || [])) {
      try {
        collectMatchedRules(sheet.cssRules, rules);
      } catch (_) {}
    }
    if (!rules.length) {
      return 0;
    }
    const style = document.createElement("style");
    style.id = state.style_element_id;
    style.setAttribute("data-synapse-media-emulation", "true");
    style.textContent = rules.join("\n");
    const parent = document.head || document.documentElement;
    parent.appendChild(style);
    return rules.length;
  }
  const appliedRuleCount = applyMatchedMediaStyleRules();
  try {
    globalThis.dispatchEvent(new Event("resize"));
  } catch (_) {}
  return {
    applied: true,
    applied_css_rule_count: appliedRuleCount,
    requested: state.value
  };
}

function clearMediaEmulationShimInPage() {
  const stateKey = "__synapseMediaEmulationOverride";
  const state = globalThis[stateKey];
  if (state && typeof state === "object") {
    try {
      const existing = globalThis.document && document.getElementById(state.style_element_id);
      if (existing && existing.parentNode) {
        existing.parentNode.removeChild(existing);
      }
    } catch (_) {}
    if (state.match_media_had_own_descriptor && state.match_media_descriptor) {
      Object.defineProperty(globalThis, "matchMedia", state.match_media_descriptor);
    } else {
      try {
        delete globalThis.matchMedia;
      } catch (_) {}
    }
    delete globalThis[stateKey];
  }
  try {
    globalThis.dispatchEvent(new Event("resize"));
  } catch (_) {}
  return {
    cleared: Boolean(state)
  };
}

function installNetworkConditionsShimInPage(requested) {
  const stateKey = "__synapseNetworkConditionsOverride";
  const requestedState = requested && typeof requested === "object" ? requested : {};
  const offline = Boolean(requestedState.offline);
  const latencyMs = Number.isFinite(Number(requestedState.latency_ms)) ? Number(requestedState.latency_ms) : 0;
  const downloadThroughput = Number.isFinite(Number(requestedState.download_throughput_bytes_per_sec))
    ? Number(requestedState.download_throughput_bytes_per_sec)
    : -1;
  const uploadThroughput = Number.isFinite(Number(requestedState.upload_throughput_bytes_per_sec))
    ? Number(requestedState.upload_throughput_bytes_per_sec)
    : -1;
  const connectionType = requestedState.connection_type === null || requestedState.connection_type === undefined
    ? null
    : String(requestedState.connection_type);
  const navigatorObject = globalThis.navigator || null;
  const navigatorProto = navigatorObject ? Object.getPrototypeOf(navigatorObject) : null;
  let state = globalThis[stateKey];
  if (!state || typeof state !== "object") {
    const connection = navigatorObject && navigatorObject.connection ? navigatorObject.connection : null;
    state = {
      native_fetch: typeof globalThis.fetch === "function" ? globalThis.fetch : null,
      fetch_had_own_descriptor: Object.prototype.hasOwnProperty.call(globalThis, "fetch"),
      fetch_descriptor: Object.getOwnPropertyDescriptor(globalThis, "fetch") || null,
      navigator_object: navigatorObject,
      navigator_proto: navigatorProto,
      navigator_online_had_own_descriptor: navigatorProto ? Object.prototype.hasOwnProperty.call(navigatorProto, "onLine") : false,
      navigator_online_descriptor: navigatorProto ? Object.getOwnPropertyDescriptor(navigatorProto, "onLine") || null : null,
      navigator_connection_had_own_descriptor: navigatorObject ? Object.prototype.hasOwnProperty.call(navigatorObject, "connection") : false,
      navigator_connection_descriptor: navigatorObject ? Object.getOwnPropertyDescriptor(navigatorObject, "connection") || null : null,
      connection,
      connection_descriptors: {},
      synthetic_connection: null,
      value: {}
    };
    Object.defineProperty(globalThis, stateKey, {
      configurable: true,
      enumerable: false,
      writable: true,
      value: state
    });
  }
  state.value = {
    offline,
    latency_ms: Math.max(0, latencyMs),
    download_throughput_bytes_per_sec: downloadThroughput,
    upload_throughput_bytes_per_sec: uploadThroughput,
    connection_type: connectionType
  };
  function currentState() {
    const current = globalThis[stateKey];
    return current && current.value ? current.value : state.value;
  }
  function effectiveType(value) {
    if (value.connection_type === "cellular2g") {
      return "2g";
    }
    if (value.connection_type === "cellular3g") {
      return "3g";
    }
    if (value.connection_type === "cellular4g") {
      return "4g";
    }
    if (value.offline || value.connection_type === "none") {
      return "slow-2g";
    }
    return null;
  }
  function downlinkMbps(value) {
    const throughput = Number(value.download_throughput_bytes_per_sec);
    return Number.isFinite(throughput) && throughput > 0 ? throughput * 8 / 1000000 : null;
  }
  function delayMs(value) {
    const latency = Math.max(0, Number(value.latency_ms) || 0);
    const throughput = Number(value.download_throughput_bytes_per_sec);
    if (Number.isFinite(throughput) && throughput > 0) {
      const sampleBytes = 65536;
      return Math.max(latency, Math.min(5000, Math.max(100, Math.round(sampleBytes / throughput * 1000))));
    }
    return latency;
  }
  function offlineError() {
    try {
      return new TypeError("Failed to fetch");
    } catch (_) {
      return new Error("Failed to fetch");
    }
  }
  if (state.navigator_proto) {
    try {
      Object.defineProperty(state.navigator_proto, "onLine", {
        configurable: true,
        enumerable: true,
        get() {
          return !currentState().offline;
        }
      });
    } catch (_) {}
  }
  function defineConnectionGetter(target, field, getter) {
    if (!target) {
      return;
    }
    if (!Object.prototype.hasOwnProperty.call(state.connection_descriptors, field)) {
      state.connection_descriptors[field] = {
        had_own: Object.prototype.hasOwnProperty.call(target, field),
        descriptor: Object.getOwnPropertyDescriptor(target, field) || null
      };
    }
    try {
      Object.defineProperty(target, field, {
        configurable: true,
        enumerable: true,
        get: getter
      });
    } catch (_) {}
  }
  let connectionTarget = state.connection;
  if (!connectionTarget) {
    state.synthetic_connection = state.synthetic_connection || {};
    connectionTarget = state.synthetic_connection;
    if (state.navigator_object) {
      try {
        Object.defineProperty(state.navigator_object, "connection", {
          configurable: true,
          enumerable: true,
          get() {
            return state.synthetic_connection;
          }
        });
      } catch (_) {}
    }
  }
  defineConnectionGetter(connectionTarget, "type", () => currentState().connection_type);
  defineConnectionGetter(connectionTarget, "effectiveType", () => effectiveType(currentState()));
  defineConnectionGetter(connectionTarget, "downlink", () => downlinkMbps(currentState()));
  defineConnectionGetter(connectionTarget, "rtt", () => Math.max(0, Number(currentState().latency_ms) || 0));
  defineConnectionGetter(connectionTarget, "saveData", () => false);
  if (typeof state.native_fetch === "function") {
    Object.defineProperty(globalThis, "fetch", {
      configurable: true,
      enumerable: true,
      writable: true,
      value(...args) {
        const value = currentState();
        const wait = delayMs(value);
        return new Promise((resolve, reject) => {
          globalThis.setTimeout(() => {
            if (currentState().offline) {
              reject(offlineError());
              return;
            }
            try {
              Promise.resolve(state.native_fetch.apply(globalThis, args)).then(resolve, reject);
            } catch (error) {
              reject(error);
            }
          }, wait);
        });
      }
    });
  }
  try {
    globalThis.dispatchEvent(new Event(offline ? "offline" : "online"));
  } catch (_) {}
  return {
    applied: true,
    requested: state.value
  };
}

function clearNetworkConditionsShimInPage() {
  const stateKey = "__synapseNetworkConditionsOverride";
  const state = globalThis[stateKey];
  function restoreDescriptor(target, field, hadOwn, descriptor) {
    if (!target) {
      return;
    }
    if (hadOwn && descriptor) {
      try {
        Object.defineProperty(target, field, descriptor);
      } catch (_) {}
    } else {
      try {
        delete target[field];
      } catch (_) {}
    }
  }
  if (state && typeof state === "object") {
    restoreDescriptor(globalThis, "fetch", state.fetch_had_own_descriptor, state.fetch_descriptor);
    restoreDescriptor(state.navigator_proto, "onLine", state.navigator_online_had_own_descriptor, state.navigator_online_descriptor);
    if (state.connection) {
      for (const [field, saved] of Object.entries(state.connection_descriptors || {})) {
        restoreDescriptor(state.connection, field, saved.had_own, saved.descriptor);
      }
    }
    if (state.navigator_object && state.synthetic_connection) {
      restoreDescriptor(
        state.navigator_object,
        "connection",
        state.navigator_connection_had_own_descriptor,
        state.navigator_connection_descriptor
      );
    }
    delete globalThis[stateKey];
  }
  try {
    globalThis.dispatchEvent(new Event("online"));
  } catch (_) {}
  return {
    cleared: Boolean(state)
  };
}

async function readGeolocationStateInPage() {
  const permissionState = await (async () => {
    try {
      if (!globalThis.navigator || !navigator.permissions || typeof navigator.permissions.query !== "function") {
        return "unsupported";
      }
      const status = await navigator.permissions.query({ name: "geolocation" });
      return String(status && status.state || "unknown");
    } catch (error) {
      const name = error && error.name ? error.name : error;
      return `error:${String(name)}`;
    }
  })();
  const result = await new Promise(resolve => {
    if (!globalThis.navigator || !navigator.geolocation) {
      resolve({
        position: null,
        error: { code: -1, message: "navigator.geolocation unavailable" }
      });
      return;
    }
    let settled = false;
    const finish = value => {
      if (!settled) {
        settled = true;
        resolve(value);
      }
    };
    const timer = globalThis.setTimeout(() => finish({
      position: null,
      error: { code: 3, message: "timeout waiting for geolocation callback" }
    }), 1500);
    navigator.geolocation.getCurrentPosition(
      position => {
        globalThis.clearTimeout(timer);
        const coords = position.coords || {};
        finish({
          position: {
            latitude: Number(coords.latitude),
            longitude: Number(coords.longitude),
            accuracy: Number(coords.accuracy),
            altitude: coords.altitude == null ? null : Number(coords.altitude),
            altitude_accuracy: coords.altitudeAccuracy == null ? null : Number(coords.altitudeAccuracy),
            heading: coords.heading == null || Number.isNaN(Number(coords.heading)) ? null : Number(coords.heading),
            speed: coords.speed == null || Number.isNaN(Number(coords.speed)) ? null : Number(coords.speed),
            timestamp: Number(position.timestamp || 0)
          },
          error: null
        });
      },
      error => {
        globalThis.clearTimeout(timer);
        finish({
          position: null,
          error: {
            code: Number(error && error.code || 0),
            message: String(error && error.message || "")
          }
        });
      },
      { enableHighAccuracy: false, maximumAge: 0, timeout: 1000 }
    );
  });
  return {
    permission_state: permissionState,
    position: result.position,
    error: result.error
  };
}

function readLocaleStateInPage() {
  const sampleDate = new Date(Date.UTC(2020, 0, 2, 3, 4, 5));
  const options = Intl.DateTimeFormat().resolvedOptions();
  const dateFormatter = new Intl.DateTimeFormat(undefined, {
    dateStyle: "full",
    timeStyle: "long"
  });
  return {
    locale: String(options.locale || ""),
    calendar: String(options.calendar || ""),
    numbering_system: String(options.numberingSystem || ""),
    time_zone: String(options.timeZone || ""),
    sample_number: new Intl.NumberFormat().format(1234567.89),
    sample_date: dateFormatter.format(sampleDate),
    date_string: sampleDate.toString(),
    timezone_offset_minutes: Number(sampleDate.getTimezoneOffset())
  };
}

function readMediaStateInPage() {
  const matches = query => {
    try {
      return Boolean(globalThis.matchMedia && globalThis.matchMedia(query).matches);
    } catch (_) {
      return false;
    }
  };
  return {
    media_screen: matches("screen"),
    media_print: matches("print"),
    color_scheme_dark: matches("(prefers-color-scheme: dark)"),
    color_scheme_light: matches("(prefers-color-scheme: light)"),
    color_scheme_no_preference: matches("(prefers-color-scheme: no-preference)"),
    reduced_motion_reduce: matches("(prefers-reduced-motion: reduce)"),
    reduced_motion_no_preference: matches("(prefers-reduced-motion: no-preference)")
  };
}

function readNetworkConditionsStateInPage() {
  const connection = globalThis.navigator && navigator.connection ? navigator.connection : null;
  function stringOrNull(value) {
    return value === null || value === undefined ? null : String(value);
  }
  function numberOrNull(value) {
    const number = Number(value);
    return Number.isFinite(number) ? number : null;
  }
  return {
    online: Boolean(globalThis.navigator ? navigator.onLine : true),
    connection_type: connection ? stringOrNull(connection.type) : null,
    effective_type: connection ? stringOrNull(connection.effectiveType) : null,
    downlink_mbps: connection ? numberOrNull(connection.downlink) : null,
    rtt_ms: connection ? numberOrNull(connection.rtt) : null,
    save_data: connection && connection.saveData !== undefined && connection.saveData !== null
      ? Boolean(connection.saveData)
      : null
  };
}

async function handleCdpInputDrag(selected, action, params, before, beforePageText, waitTimeoutMs, autoWait, autoWaitTimeoutMs) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  const sourceSelector = stringOrNull(params.sourceSelector);
  const targetSelector = stringOrNull(params.targetSelector);
  if (!sourceSelector || !targetSelector) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput ${action} requires non-empty sourceSelector and targetSelector`
    );
  }
  const source = await resolveCdpInputSelector(
    selected,
    action,
    "source",
    sourceSelector,
    autoWait,
    autoWaitTimeoutMs
  );
  const target = await resolveCdpInputSelector(
    selected,
    action,
    "target",
    targetSelector,
    autoWait,
    autoWaitTimeoutMs
  );
  const steps = normalizeCdpInputDragSteps(params.dragSteps);
  const durationMs = normalizeCdpInputDragDuration(params.dragDurationMs);
  let dragActivation = {
    attempted: false,
    activated: false,
    before_active: before.active,
    after_active: before.active,
    required_foreground: false,
    readback_backend: "not_required_for_html5_drag"
  };
  let dispatch;
  if (action === "drag") {
    // Real trusted mouse drag: chrome.debugger Input.* only dispatches to the
    // ACTIVE tab, so activate the owned tab in its already-focused Chrome
    // window first (in-Chrome activation, required_foreground=false), then
    // dispatch the real Input.dispatchMouseEvent press/move/release. NO
    // synthetic fallback (#1355): a guarded pointer-DnD target must see
    // isTrusted=true, or the verb fails loud — never a synthetic no-op that
    // looks like success.
    dragActivation = await activateTabForCdpTouch(selected.tabId, before, "drag");
    const realDrag = await dispatchCdpInputMouseDrag(
      selected.tabId,
      source.point,
      target.point,
      steps,
      durationMs
    );
    dispatch = { ...realDrag, backend: "chrome.debugger.Input" };
  } else if (action === "html5_real_drag") {
    // Real, TRUSTED HTML5 drop via CDP Input.dispatchDragEvent (#1356). Chrome
    // only delivers Input.* to the active tab, so activate first (in-Chrome,
    // required_foreground=false), then dispatch dragEnter/dragOver/drop on the
    // target point with a DragData built from data_mime_type/data_text. Unlike
    // the synthetic html5_drag lane these DragEvents are isTrusted=true and
    // carry a real DataTransfer, so native drop zones that gate on trust fire.
    dragActivation = await activateTabForCdpTouch(selected.tabId, before, "html5_real_drag");
    const realDrop = await dispatchRealHtml5Drop(selected.tabId, target.point, params);
    dispatch = { ...realDrop, backend: "chrome.debugger.Input.dispatchDragEvent" };
  } else {
    dispatch = await dispatchHtml5DragDrop(selected.tabId, sourceSelector, targetSelector, params);
  }
  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs);
  const afterPageText = await tabPageTextState(selected.tabId);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    action,
    before_page: before,
    after_page: after,
    before_page_text: beforePageText,
    after_page_text: afterPageText,
    readback_backend: dispatch.backend || (action === "drag" ? "chrome.debugger.Input" : "chrome.scripting.executeScript(DragEvent+DataTransfer)"),
    required_foreground: false,
    tab_activation_for_touch: dragActivation,
    source_selector: sourceSelector,
    target_selector: targetSelector,
    source_frame_id: source.frame_id,
    target_frame_id: target.frame_id,
    frame_id: source.frame_id,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason,
    method: dispatch.method,
    dispatched_events: dispatch.dispatched_events,
    drag_steps: steps,
    drag_duration_ms: durationMs,
    source_viewport_point: source.point,
    target_viewport_point: target.point,
    debugger_protocol_version: dispatch.protocol_version || null,
    cdp_fallback_error: dispatch.cdp_fallback_error || null,
    html5_readback: dispatch.html5_readback || null,
    synthetic_mouse_readback: dispatch.synthetic_mouse_readback || null,
    source: source.resolved,
    target: target.resolved
  };
}

async function resolveCdpInputSelector(selected, action, label, selector, autoWait, autoWaitTimeoutMs) {
  const request = {
    action,
    selector,
    elementId: null,
    elementPath: null,
    role: null,
    name: null,
    value: null,
    autoWait,
    autoWaitTimeoutMs,
    resolveOnly: true,
    resolveActionability: true,
    maxPageTextChars: MAX_PAGE_TEXT_CHARS
  };
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId, allFrames: true },
      func: performDomActionInPage,
      args: [request]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript cdpInput(${selected.tabId}, ${action}, ${label}) resolve failed: ${errorMessage(error)}`
    );
  }

  const frameResults = frameExecutionResults(injected);
  const okFrames = frameResults.filter((frame) => frame.result && frame.result.ok);
  const totalMatches = frameResults.reduce((sum, frame) => sum + frameResolvedMatchCount(frame), 0);
  const ambiguousFrames = frameResults.filter((frame) =>
    frame.result?.error_code === ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS
  );
  if (okFrames.length > 1 || totalMatches > 1 || ambiguousFrames.length > 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ELEMENT_AMBIGUOUS,
      `cdpInput ${action} ${label} selector matched ${totalMatches} actionable element(s) across ${frameResults.length} frame(s); ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  const first = okFrames[0] || null;
  if (!first || !first.result || typeof first.result !== "object") {
    const failed = frameResults.find((frame) => frame.result)?.result;
    throw bridgeError(
      String(failed?.error_code || ERROR_CHROME_DOM_ELEMENT_NOT_FOUND),
      `cdpInput ${action} ${label} selector failed across ${frameResults.length} frame(s): ${String(failed?.error_detail || "no matching frame result")}; ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  if (first.frame_id !== 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput ${action} currently supports top-frame drag targets only because Input coordinates and HTML5 dispatch are same-document scoped; ` +
        `${label}_frame_id=${first.frame_id}. Re-resolve top-frame elements or use a raw-CDP frame-aware target.`
    );
  }
  const actionPoint = first.result.action_point;
  if (!actionPoint || !Number.isFinite(actionPoint.x) || !Number.isFinite(actionPoint.y)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ELEMENT_NOT_ACTIONABLE,
      `cdpInput ${action} ${label} selector resolved without a finite viewport action point`
    );
  }
  return {
    frame_id: first.frame_id,
    point: { x: actionPoint.x, y: actionPoint.y },
    resolved: {
      label,
      resolved_by: first.result.resolved_by,
      matched_count: first.result.matched_count,
      before_element: first.result.before_element,
      after_element: first.result.after_element,
      auto_wait: first.result.auto_wait,
      auto_wait_readback: first.result.auto_wait_readback,
      frame_result_count: frameResults.length,
      frame_results: frameResults.map(summarizeFrameExecutionResult)
    }
  };
}

function normalizeCdpInputDragSteps(value) {
  if (value === null || value === undefined) {
    return 12;
  }
  if (!Number.isSafeInteger(value) || value < 1 || value > 100) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput dragSteps must be an integer in 1..=100; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeCdpInputDragDuration(value) {
  if (value === null || value === undefined) {
    return 350;
  }
  if (!Number.isSafeInteger(value) || value < 0 || value > 10000) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput dragDurationMs must be an integer in 0..=10000; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeCdpInputCoordinate(params, action) {
  const hasX = params.x !== null && params.x !== undefined;
  const hasY = params.y !== null && params.y !== undefined;
  if (!hasX && !hasY) {
    return null;
  }
  if (action !== "tap") {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "cdpInput hover requires an element locator, not x/y");
  }
  if (!hasX || !hasY) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "cdpInput tap coordinate input requires both x and y");
  }
  const coordinateSpace = String(params.coordinateSpace || "viewport").trim().toLowerCase();
  if (coordinateSpace !== "viewport") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `cdpInput tap coordinateSpace must be viewport because Input.dispatchTouchEvent consumes viewport CSS pixels; got ${JSON.stringify(params.coordinateSpace)}`
    );
  }
  return {
    x: normalizeCoordinateValue(params.x, "x"),
    y: normalizeCoordinateValue(params.y, "y")
  };
}

async function dispatchCdpInput(tabId, action, point, options = {}) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  let touchEmulationEnabled = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    if (action === "hover") {
      await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
        type: "mouseMoved",
        x: point.x,
        y: point.y,
        button: "none",
        pointerType: "mouse"
      });
      return {
        method: "Input.dispatchMouseEvent(mouseMoved)",
        dispatched_events: ["mouseMoved"],
        protocol_version: protocolVersion
      };
    }
    if (action === "click" || action === "dblclick") {
      // Real, trusted mouse click via chrome.debugger: isTrusted=true so
      // activation-guarded handlers fire (synthetic dispatchEvent does not).
      const button = String(options.button || "left");
      const modifiers = cdpModifierBitmask(options.modifiers);
      const dispatched = [];
      // Move first so :hover/pointer state matches a real click.
      await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
        type: "mouseMoved", x: point.x, y: point.y, button: "none", modifiers, pointerType: "mouse"
      });
      dispatched.push("mouseMoved");
      const pairs = action === "dblclick" ? 2 : 1;
      for (let clickCount = 1; clickCount <= pairs; clickCount += 1) {
        await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
          type: "mousePressed", x: point.x, y: point.y, button, buttons: 1, clickCount, modifiers, pointerType: "mouse"
        });
        await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
          type: "mouseReleased", x: point.x, y: point.y, button, buttons: 0, clickCount, modifiers, pointerType: "mouse"
        });
        dispatched.push(`mousePressed(clickCount=${clickCount})`, `mouseReleased(clickCount=${clickCount})`);
      }
      return {
        method: "Input.dispatchMouseEvent(mousePressed,mouseReleased)",
        dispatched_events: dispatched,
        protocol_version: protocolVersion
      };
    }
    await sendDebuggerCommand(debuggee, "Emulation.setTouchEmulationEnabled", {
      enabled: true,
      maxTouchPoints: 1
    });
    touchEmulationEnabled = true;
    await sendDebuggerCommand(debuggee, "Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{
        x: point.x,
        y: point.y,
        radiusX: 1,
        radiusY: 1,
        force: 1,
        id: 1
      }]
    });
    await sendDebuggerCommand(debuggee, "Input.dispatchTouchEvent", {
      type: "touchEnd",
      touchPoints: []
    });
    await sendDebuggerCommand(debuggee, "Emulation.setTouchEmulationEnabled", {
      enabled: false
    });
    touchEmulationEnabled = false;
    return {
      method: "Emulation.setTouchEmulationEnabled + Input.dispatchTouchEvent(touchStart,touchEnd)",
      dispatched_events: ["setTouchEmulationEnabled(true)", "touchStart", "touchEnd", "setTouchEmulationEnabled(false)"],
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger ${action} dispatch failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      if (touchEmulationEnabled) {
        try {
          await sendDebuggerCommand(debuggee, "Emulation.setTouchEmulationEnabled", {
            enabled: false
          }, 1000);
        } catch (error) {
          console.warn(`Synapse touch emulation cleanup failed for tab ${tabId}: ${errorMessage(error)}`);
        }
      }
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchCdpInputMouseDrag(tabId, sourcePoint, targetPoint, steps, durationMs) {
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  const dispatched = [];
  const delayMs = steps > 0 && durationMs > 0 ? Math.max(0, Math.floor(durationMs / steps)) : 0;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
      type: "mouseMoved",
      x: sourcePoint.x,
      y: sourcePoint.y,
      button: "none",
      pointerType: "mouse"
    });
    dispatched.push("mouseMoved(source)");
    await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
      type: "mousePressed",
      x: sourcePoint.x,
      y: sourcePoint.y,
      button: "left",
      buttons: 1,
      clickCount: 1,
      pointerType: "mouse"
    });
    dispatched.push("mousePressed(left)");
    for (let index = 1; index <= steps; index += 1) {
      const ratio = index / steps;
      const x = sourcePoint.x + (targetPoint.x - sourcePoint.x) * ratio;
      const y = sourcePoint.y + (targetPoint.y - sourcePoint.y) * ratio;
      await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
        type: "mouseMoved",
        x,
        y,
        button: "left",
        buttons: 1,
        pointerType: "mouse"
      });
      dispatched.push(`dragMove(${index}/${steps})`);
      if (delayMs > 0 && index < steps) {
        await sleep(delayMs);
      }
    }
    await sendDebuggerCommand(debuggee, "Input.dispatchMouseEvent", {
      type: "mouseReleased",
      x: targetPoint.x,
      y: targetPoint.y,
      button: "left",
      buttons: 0,
      clickCount: 1,
      pointerType: "mouse"
    });
    dispatched.push("mouseReleased(left)");
    return {
      method: "Input.dispatchMouseEvent(mouseMoved,mousePressed,dragMove,mouseReleased)",
      dispatched_events: dispatched,
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger drag dispatch failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for drag tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

// #1356: real, TRUSTED HTML5 drop. Dispatches CDP Input.dispatchDragEvent
// dragEnter/dragOver/drop at the resolved target point with a DragData built
// from the caller's data_mime_type/data_text. These events are isTrusted=true
// and carry a real DataTransfer, so native drop zones that gate on trust (which
// the synthetic html5_drag lane cannot drive) actually fire. No drag
// interception is needed because the DragData is constructed, not captured from
// a page-initiated drag.
async function dispatchRealHtml5Drop(tabId, targetPoint, params) {
  const mimeType = stringOrNull(params.dragDataMimeType) || "text/plain";
  const dataText = typeof params.dragDataText === "string" ? params.dragDataText : "";
  const dragData = {
    items: [{ mimeType, data: dataText }],
    // DragOperationsMask copy(1): the most universally accepted dropEffect.
    dragOperationsMask: 1
  };
  const debuggee = { tabId };
  const protocolVersion = "1.3";
  let attached = false;
  try {
    await chrome.debugger.attach(debuggee, protocolVersion);
    attached = true;
    const dispatched = [];
    for (const type of ["dragEnter", "dragOver", "drop"]) {
      await sendDebuggerCommand(debuggee, "Input.dispatchDragEvent", {
        type,
        x: targetPoint.x,
        y: targetPoint.y,
        data: dragData
      });
      dispatched.push(type);
    }
    return {
      method: "Input.dispatchDragEvent(dragEnter,dragOver,drop)",
      dispatched_events: dispatched,
      drag_data_mime_type: mimeType,
      drag_data_text_len: dataText.length,
      protocol_version: protocolVersion
    };
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `chrome.debugger html5_real_drag dispatch failed for tab ${tabId}: ${errorMessage(error)}`
    );
  } finally {
    if (attached) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for html5_real_drag tab ${tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function dispatchHtml5DragDrop(tabId, sourceSelector, targetSelector, params) {
  const data = {
    mimeType: stringOrNull(params.dragDataMimeType) || "text/plain",
    text: stringOrNull(params.dragDataText) || ""
  };
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId, frameIds: [0] },
      world: "MAIN",
      func: performHtml5DragDropInPage,
      args: [{ sourceSelector, targetSelector, data }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript html5_drag(${tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frameResults = frameExecutionResults(injected);
  const first = frameResults.find((frame) => frame.result) || null;
  const result = first?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `html5_drag returned no structured result; frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  if (!result.ok) {
    throw bridgeError(
      String(result.error_code || ERROR_CHROME_DOM_ACTION_UNSUPPORTED),
      `html5_drag failed: ${String(result.error_detail || "")}; frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  return {
    method: "DragEvent(dragstart,dragenter,dragover,drop,dragend)+DataTransfer",
    dispatched_events: result.events_dispatched || [],
    protocol_version: null,
    html5_readback: result
  };
}

async function activateTabForCdpTouch(tabId, beforeState, action = "tap") {
  const beforeActive = Boolean(beforeState?.active);
  if (beforeActive) {
    return {
      attempted: true,
      activated: false,
      before_active: true,
      after_active: true,
      required_foreground: false,
      readback_backend: "chrome.tabs.get"
    };
  }
  if (!chrome.tabs || typeof chrome.tabs.update !== "function") {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `cdpInput ${action} requires activating the target tab before Input.* dispatch, but chrome.tabs.update is unavailable`
    );
  }
  try {
    await chrome.tabs.update(tabId, { active: true });
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `cdpInput ${action} could not activate tab ${tabId} before Input.* dispatch: ${errorMessage(error)}`
    );
  }
  const started = Date.now();
  let after = null;
  while (Date.now() - started <= 2000) {
    after = await tabPageState(tabId, null);
    if (after.active) {
      return {
        attempted: true,
        activated: true,
        before_active: false,
        after_active: true,
        required_foreground: false,
        readback_backend: "chrome.tabs.update(active=true)+chrome.tabs.get"
      };
    }
    await sleep(50);
  }
  throw bridgeError(
    ERROR_EXTENSION_TIMEOUT,
    `cdpInput ${action} activated tab ${tabId} but active readback did not settle within 2000ms; after_active=${Boolean(after?.active)}`
  );
}

async function sendDebuggerCommand(debuggee, method, params, timeoutMs = DEBUGGER_COMMAND_TIMEOUT_MS) {
  try {
    return await promiseWithTimeout(
      chrome.debugger.sendCommand(debuggee, method, params),
      timeoutMs,
      `${method} timed out after ${timeoutMs}ms`
    );
  } catch (error) {
    throw new Error(`${method}: ${errorMessage(error)}`);
  }
}

async function attachDebuggerForCommand(tabId, protocolVersion = "1.3") {
  const debuggee = { tabId };
  if (persistentDebuggerSessionIsAttached(tabId)) {
    return { debuggee, shouldDetach: false, persistent: true, protocolVersion };
  }
  await chrome.debugger.attach(debuggee, protocolVersion);
  return { debuggee, shouldDetach: true, persistent: false, protocolVersion };
}

async function ensureInitScriptDebuggerSession(tabId, protocolVersion = "1.3") {
  const debuggee = { tabId };
  let session = INIT_SCRIPT_DEBUGGER_SESSIONS.get(tabId);
  if (session) {
    return { debuggee, session, newlyAttached: false, newlyCreated: false, protocolVersion };
  }
  const reusePersistentDebugger = persistentDebuggerSessionIsAttached(tabId);
  if (!reusePersistentDebugger) {
    await chrome.debugger.attach(debuggee, protocolVersion);
  }
  session = {
    protocolVersion,
    pageEnabled: false,
    identifiers: new Set(),
    attached: true,
    attachedAtUnixMs: Date.now()
  };
  INIT_SCRIPT_DEBUGGER_SESSIONS.set(tabId, session);
  return { debuggee, session, newlyAttached: !reusePersistentDebugger, newlyCreated: true, protocolVersion };
}

async function ensureInitScriptPageDomainEnabled(debuggee, session) {
  if (!session || session.pageEnabled) {
    return;
  }
  await sendDebuggerCommand(debuggee, "Page.enable", {});
  session.pageEnabled = true;
}

async function maybeDetachInitScriptDebuggerSession(tabId, debuggee, identifier) {
  const session = INIT_SCRIPT_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    return false;
  }
  if (identifier) {
    session.identifiers.delete(identifier);
  }
  if (session.identifiers.size > 0) {
    return false;
  }
  INIT_SCRIPT_DEBUGGER_SESSIONS.delete(tabId);
  if (bindingSessionHasActiveNames(tabId) || dialogSessionIsActive(tabId) || fileChooserSessionIsActive(tabId)) {
    return false;
  }
  try {
    await chrome.debugger.detach(debuggee);
    markBindingDebuggerDetached(tabId, false);
    markDialogDebuggerDetached(tabId, false);
    markFileChooserDebuggerDetached(tabId, false);
    return true;
  } catch (error) {
    console.warn(`Synapse chrome.debugger detach failed for initScript cleanup tab ${tabId}: ${errorMessage(error)}`);
    return false;
  }
}

function bindingSessionIsAttached(tabId) {
  const session = BINDING_DEBUGGER_SESSIONS.get(tabId);
  return Boolean(session?.attached);
}

function dialogSessionIsAttached(tabId) {
  const session = DIALOG_DEBUGGER_SESSIONS.get(tabId);
  return Boolean(session?.attached);
}

function fileChooserSessionIsAttached(tabId) {
  const session = FILE_CHOOSER_DEBUGGER_SESSIONS.get(tabId);
  return Boolean(session?.attached);
}

function persistentDebuggerSessionIsAttached(tabId) {
  return INIT_SCRIPT_DEBUGGER_SESSIONS.has(tabId) ||
    bindingSessionIsAttached(tabId) ||
    dialogSessionIsAttached(tabId) ||
    fileChooserSessionIsAttached(tabId);
}

function bindingSessionHasActiveNames(tabId) {
  const session = BINDING_DEBUGGER_SESSIONS.get(tabId);
  return Boolean(session?.activeNames?.size);
}

function hasActiveInitScriptDebuggerSession(tabId) {
  const session = INIT_SCRIPT_DEBUGGER_SESSIONS.get(tabId);
  return Boolean(session?.identifiers?.size);
}

function dialogSessionIsActive(tabId) {
  return dialogSessionIsAttached(tabId);
}

function fileChooserSessionIsActive(tabId) {
  return fileChooserSessionIsAttached(tabId);
}

async function ensureBindingDebuggerSession(tabId, protocolVersion = "1.3") {
  const debuggee = { tabId };
  let session = BINDING_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    session = {
      protocolVersion,
      runtimeEnabled: false,
      attached: false,
      activeNames: new Set(),
      calls: [],
      nextSeq: 0,
      dropped: 0,
      capacity: MAX_BINDING_EVENT_BUFFER,
      armedAtUnixMs: 0
    };
    BINDING_DEBUGGER_SESSIONS.set(tabId, session);
  }
  let newlyArmed = false;
  if (!session.attached) {
    if (!persistentDebuggerSessionIsAttached(tabId)) {
      await chrome.debugger.attach(debuggee, protocolVersion);
    }
    session.attached = true;
    session.protocolVersion = protocolVersion;
    session.armedAtUnixMs = Date.now();
    newlyArmed = true;
  }
  if (!session.runtimeEnabled) {
    await sendDebuggerCommand(debuggee, "Runtime.enable", {});
    session.runtimeEnabled = true;
  }
  return { debuggee, session, newlyArmed, protocolVersion };
}

async function addBindingToSession(debuggee, session, name, executionContextName) {
  if (session.activeNames.has(name)) {
    return false;
  }
  const params = { name };
  if (executionContextName) {
    params.executionContextName = executionContextName;
  }
  await sendDebuggerCommand(debuggee, "Runtime.addBinding", params);
  session.activeNames.add(name);
  return true;
}

async function detachBindingDebuggerSession(tabId, debuggee, session) {
  if (hasActiveInitScriptDebuggerSession(tabId) || dialogSessionIsActive(tabId) || fileChooserSessionIsActive(tabId)) {
    return false;
  }
  try {
    await chrome.debugger.detach(debuggee);
    markBindingDebuggerDetached(tabId, false);
    markDialogDebuggerDetached(tabId, false);
    markFileChooserDebuggerDetached(tabId, false);
    return true;
  } catch (error) {
    console.warn(`Synapse chrome.debugger detach failed for binding cleanup tab ${tabId}: ${errorMessage(error)}`);
    session.attached = false;
    session.runtimeEnabled = false;
    return false;
  }
}

function markBindingDebuggerDetached(tabId, clearActiveNames = true) {
  const session = BINDING_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    return;
  }
  session.attached = false;
  session.runtimeEnabled = false;
  if (clearActiveNames) {
    session.activeNames.clear();
  }
}

function recordBindingCalledEvent(tabId, params) {
  const session = BINDING_DEBUGGER_SESSIONS.get(tabId);
  const name = String(params?.name || "");
  if (!session || !session.activeNames.has(name)) {
    return;
  }
  const rawPayload = typeof params?.payload === "string" ? params.payload : String(params?.payload ?? "");
  const payloadChars = Array.from(rawPayload);
  const payloadLen = payloadChars.length;
  const payloadTruncated = payloadLen > MAX_BINDING_PAYLOAD_CHARS;
  const payload = payloadTruncated
    ? payloadChars.slice(0, MAX_BINDING_PAYLOAD_CHARS).join("")
    : rawPayload;
  const call = {
    seq: session.nextSeq,
    name,
    payload,
    payload_len: payloadLen,
    payload_truncated: payloadTruncated,
    execution_context_id: Number.isInteger(params?.executionContextId) ? params.executionContextId : 0,
    timestamp_ms: Date.now()
  };
  session.nextSeq += 1;
  if (!payloadTruncated) {
    try {
      call.payload_json = JSON.parse(payload);
    } catch (_error) {
      // Non-JSON binding payloads are valid and remain available as strings.
    }
  }
  while (session.calls.length >= session.capacity) {
    session.calls.shift();
    session.dropped += 1;
  }
  session.calls.push(call);
}

function bindingStatusFromSession(session, name, newlyArmed, bindingNewlyAdded, bindingRemoved) {
  const activeBindingNames = Array.from(session.activeNames).sort();
  return {
    newly_armed: Boolean(newlyArmed),
    binding_newly_added: Boolean(bindingNewlyAdded),
    binding_removed: Boolean(bindingRemoved),
    armed_at_unix_ms: Number(session.armedAtUnixMs || 0),
    binding_active: session.activeNames.has(name),
    active_binding_count: activeBindingNames.length,
    active_binding_names: activeBindingNames
  };
}

function emptyBindingStatus(_tabId, _name, bindingRemoved) {
  return {
    newly_armed: false,
    binding_newly_added: false,
    binding_removed: Boolean(bindingRemoved),
    armed_at_unix_ms: 0,
    binding_active: false,
    active_binding_count: 0,
    active_binding_names: []
  };
}

function readBindingCalls(session, name, sinceSeq, maxCalls) {
  const cursor = sinceSeq === null ? 0 : sinceSeq;
  const matching = session.calls.filter((call) => call.name === name);
  const calls = matching
    .filter((call) => call.seq >= cursor)
    .slice(0, maxCalls);
  const status = bindingStatusFromSession(session, name, false, false, false);
  return {
    ...status,
    calls,
    next_cursor: Number(session.nextSeq || 0),
    returned: calls.length,
    total_buffered: matching.length,
    dropped: Number(session.dropped || 0)
  };
}

function emptyBindingRead(status) {
  return {
    ...status,
    calls: [],
    next_cursor: 0,
    returned: 0,
    total_buffered: 0,
    dropped: 0
  };
}

async function ensureDialogDebuggerSession(tabId, defaultPolicy, protocolVersion = "1.3") {
  const debuggee = { tabId };
  let session = DIALOG_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    session = {
      protocolVersion,
      pageEnabled: false,
      attached: false,
      entries: [],
      nextSeq: 0,
      dropped: 0,
      capacity: MAX_DIALOG_EVENT_BUFFER,
      armedAtUnixMs: 0,
      defaultPolicy: defaultPolicy || "dismiss",
      pendingSeq: null,
      openedCount: 0,
      closedCount: 0,
      autoHandledCount: 0,
      errorCount: 0
    };
    DIALOG_DEBUGGER_SESSIONS.set(tabId, session);
  }
  session.defaultPolicy = defaultPolicy || session.defaultPolicy || "dismiss";
  let newlyArmed = false;
  if (!session.attached) {
    if (!persistentDebuggerSessionIsAttached(tabId)) {
      await chrome.debugger.attach(debuggee, protocolVersion);
    }
    session.attached = true;
    session.protocolVersion = protocolVersion;
    session.armedAtUnixMs = Date.now();
    newlyArmed = true;
  }
  if (!session.pageEnabled) {
    await sendDebuggerCommand(debuggee, "Page.enable", {});
    session.pageEnabled = true;
  }
  return { debuggee, session, newlyArmed, protocolVersion };
}

function markDialogDebuggerDetached(tabId, clearPending = true) {
  const session = DIALOG_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    return;
  }
  session.attached = false;
  session.pageEnabled = false;
  if (clearPending) {
    session.pendingSeq = null;
  }
}

function recordDialogOpeningEvent(tabId, params) {
  const session = DIALOG_DEBUGGER_SESSIONS.get(tabId);
  if (!session?.attached) {
    return;
  }
  const defaultPrompt = params?.defaultPrompt === undefined || params?.defaultPrompt === null
    ? null
    : String(params.defaultPrompt);
  const policy = session.defaultPolicy || "dismiss";
  const entry = {
    seq: Number(session.nextSeq || 0),
    url: String(params?.url || ""),
    frame_id: String(params?.frameId || ""),
    dialog_type: String(params?.type || ""),
    message: String(params?.message || ""),
    default_prompt: defaultPrompt,
    has_browser_handler: Boolean(params?.hasBrowserHandler),
    opened_at_unix_ms: Date.now(),
    pending: true,
    default_policy: policy,
    auto_action: null,
    auto_handled_at_unix_ms: null,
    auto_handle_error: null,
    manual_action: null,
    manual_prompt_text: null,
    manual_handled_at_unix_ms: null,
    manual_handle_error: null,
    closed_at_unix_ms: null,
    close_result: null,
    user_input: null
  };
  session.nextSeq += 1;
  while (session.entries.length >= session.capacity) {
    session.entries.shift();
    session.dropped += 1;
  }
  session.pendingSeq = entry.seq;
  session.openedCount += 1;
  session.entries.push(entry);

  const action = dialogAutoActionForPolicy(policy);
  if (!action) {
    return;
  }
  sendDebuggerCommand({ tabId }, "Page.handleJavaScriptDialog", dialogAutoHandleParams(policy, defaultPrompt))
    .then(() => markDialogAutoHandled(session, entry.seq, action))
    .catch((error) => markDialogAutoError(
      session,
      entry.seq,
      action,
      `Page.handleJavaScriptDialog default policy failed: ${errorMessage(error)}`
    ));
}

function recordDialogClosedEvent(tabId, params) {
  const session = DIALOG_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    return;
  }
  const entry = dialogPendingEntry(session);
  if (!entry) {
    return;
  }
  entry.pending = false;
  entry.closed_at_unix_ms = Date.now();
  entry.close_result = Boolean(params?.result);
  entry.user_input = params?.userInput === undefined || params?.userInput === null
    ? ""
    : String(params.userInput);
  session.pendingSeq = null;
  session.closedCount += 1;
}

function dialogPendingEntry(session) {
  if (!session || session.pendingSeq === null || session.pendingSeq === undefined) {
    return null;
  }
  return session.entries.find((entry) => entry.seq === session.pendingSeq) || null;
}

function markDialogAutoHandled(session, seq, action) {
  const entry = session.entries.find((candidate) => candidate.seq === seq);
  if (!entry) {
    return;
  }
  entry.auto_action = action;
  entry.auto_handled_at_unix_ms = Date.now();
  entry.auto_handle_error = null;
  session.autoHandledCount += 1;
}

function markDialogAutoError(session, seq, action, error) {
  const entry = session.entries.find((candidate) => candidate.seq === seq);
  if (entry) {
    entry.auto_action = action;
    entry.auto_handle_error = error;
  }
  session.errorCount += 1;
}

function markDialogManualHandled(session, seq, action, promptText) {
  const entry = session.entries.find((candidate) => candidate.seq === seq);
  if (!entry) {
    return null;
  }
  const handledAt = Date.now();
  entry.pending = false;
  entry.manual_action = action;
  entry.manual_prompt_text = action === "accept" ? promptText : null;
  entry.manual_handled_at_unix_ms = handledAt;
  entry.manual_handle_error = null;
  entry.closed_at_unix_ms = handledAt;
  entry.close_result = action === "accept";
  entry.user_input = action === "accept" ? String(promptText ?? "") : "";
  session.pendingSeq = null;
  session.closedCount += 1;
  return { ...entry };
}

function dialogAutoActionForPolicy(policy) {
  if (policy === "accept") {
    return "accepted";
  }
  if (policy === "dismiss") {
    return "dismissed";
  }
  return null;
}

function dialogAutoHandleParams(policy, defaultPrompt) {
  const params = { accept: policy === "accept" };
  if (params.accept && defaultPrompt !== null && defaultPrompt !== undefined) {
    params.promptText = String(defaultPrompt);
  }
  return params;
}

function dialogManualHandleParams(operation, promptText) {
  const params = { accept: operation === "accept" };
  if (params.accept && promptText !== null && promptText !== undefined) {
    params.promptText = String(promptText);
  }
  return params;
}

function readDialogEntries(session, sinceSeq, limit) {
  const cursor = sinceSeq === null ? 0 : sinceSeq;
  const entries = session.entries
    .filter((entry) => entry.seq >= cursor)
    .slice(0, limit)
    .map((entry) => ({ ...entry }));
  const pending = dialogPendingEntry(session);
  return {
    entries,
    pending_dialog: pending ? { ...pending } : null,
    next_cursor: Number(session.nextSeq || 0),
    returned: entries.length,
    total_buffered: session.entries.length,
    dropped: Number(session.dropped || 0)
  };
}

function emptyDialogRead() {
  return {
    entries: [],
    pending_dialog: null,
    next_cursor: 0,
    returned: 0,
    total_buffered: 0,
    dropped: 0
  };
}

async function handleFileUpload(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const operation = normalizeFileUploadOperation(params.operation);
  const limit = normalizeFileChooserLimit(params.limit);
  const sinceSeq = normalizeOptionalNonNegativeInteger(params.sinceSeq, "sinceSeq");
  const files = normalizeFileUploadPaths(params.files, operation);
  let session = FILE_CHOOSER_DEBUGGER_SESSIONS.get(selected.tabId) || null;
  let captureNewlyArmed = false;
  let inputReadback = null;
  let handledChooser = null;
  let canceledChooser = null;

  if (operation === "arm_chooser" || operation === "read_chooser") {
    if (operation === "arm_chooser") {
      const ensured = await ensureFileChooserDebuggerSession(selected.tabId, true);
      session = ensured.session;
      captureNewlyArmed = ensured.newlyArmed;
    }
  } else if (operation === "set_chooser") {
    const ensured = await ensureFileChooserDebuggerSession(selected.tabId, true);
    session = ensured.session;
    const pending = fileChooserPendingEntry(session);
    if (!pending) {
      throw bridgeError(
        ERROR_ACTION_TARGET_INVALID,
        `browser_file_upload set_chooser requested for tab ${selected.tabId}, but no file chooser is pending`
      );
    }
    if (!Number.isSafeInteger(pending.backend_node_id) || pending.backend_node_id <= 0) {
      throw bridgeError(
        ERROR_ACTION_TARGET_INVALID,
        `browser_file_upload pending chooser seq=${pending.seq} has no backendNodeId`
      );
    }
    inputReadback = await setFilesForBackendNode(
      ensured.debuggee,
      pending.backend_node_id,
      files,
      "pending_chooser"
    );
    handledChooser = markFileChooserHandled(session, pending.seq, files, inputReadback);
  } else if (operation === "cancel_chooser") {
    if (!session?.attached) {
      throw bridgeError(
        ERROR_ACTION_TARGET_INVALID,
        `browser_file_upload cancel_chooser requested for tab ${selected.tabId}, but chooser capture is not armed`
      );
    }
    const pending = fileChooserPendingEntry(session);
    if (!pending) {
      throw bridgeError(
        ERROR_ACTION_TARGET_INVALID,
        `browser_file_upload cancel_chooser requested for tab ${selected.tabId}, but no file chooser is pending`
      );
    }
    canceledChooser = markFileChooserCanceled(session, pending.seq);
  } else {
    inputReadback = await setFilesForLocator(selected, params, files, operation);
  }

  session = FILE_CHOOSER_DEBUGGER_SESSIONS.get(selected.tabId) || session;
  const read = session
    ? readFileChooserEntries(session, sinceSeq, limit)
    : emptyFileChooserRead();
  const state = await tabPageState(selected.tabId, selected.target);
  return {
    extension_id: chrome.runtime.id,
    target_id: state.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: state.chrome_window_id,
    operation,
    capture_newly_armed: captureNewlyArmed,
    selector: stringOrNull(params.selector),
    element_id: stringOrNull(params.elementId),
    active_element: Boolean(params.activeElement),
    requested_file_count: files.length,
    input: inputReadback,
    handled_chooser: handledChooser,
    canceled_chooser: canceledChooser,
    pending_chooser: read.pending_chooser,
    entries: read.entries,
    next_cursor: read.next_cursor,
    returned: read.returned,
    total_buffered: read.total_buffered,
    dropped: read.dropped,
    opened_count: session ? Number(session.openedCount || 0) : 0,
    handled_count: session ? Number(session.handledCount || 0) : 0,
    canceled_count: session ? Number(session.canceledCount || 0) : 0,
    error_count: session ? Number(session.errorCount || 0) : 0,
    url: state.url || "",
    title: state.title || "",
    ready_state: state.ready_state || "",
    readback_backend: "chrome.debugger.DOM.setFileInputFiles+Runtime.callFunctionOn",
    chooser_readback_backend: "chrome.debugger.Page.setInterceptFileChooserDialog+Page.fileChooserOpened",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function setFilesForLocator(selected, params, files, operation) {
  const attach = await attachDebuggerForCommand(selected.tabId);
  const debuggee = attach.debuggee;
  try {
    const resolved = await resolveFileInputObject(debuggee, selected, params);
    if (operation === "clear") {
      await clearFileInputObject(debuggee, resolved.object_id);
    } else {
      await sendDebuggerCommand(debuggee, "DOM.setFileInputFiles", {
        objectId: resolved.object_id,
        files
      });
    }
    return await readFileInputObject(debuggee, resolved.object_id, {
      resolved_by: resolved.resolved_by,
      match_count: resolved.match_count,
      frame_id: resolved.frame_id,
      element_path: resolved.element_path
    });
  } finally {
    try {
      await sendDebuggerCommand(debuggee, "Runtime.releaseObjectGroup", {
        objectGroup: "synapseFileUpload"
      });
    } catch (_) {
      // Object-group cleanup is best-effort.
    }
    if (attach.shouldDetach) {
      try {
        await chrome.debugger.detach(debuggee);
      } catch (error) {
        console.warn(`Synapse chrome.debugger detach failed for fileUpload tab ${selected.tabId}: ${errorMessage(error)}`);
      }
    }
  }
}

async function clearFileInputObject(debuggee, objectId) {
  const cleared = await sendDebuggerCommand(debuggee, "Runtime.callFunctionOn", {
    objectId,
    functionDeclaration: fileUploadClearInputInPage.toString(),
    returnByValue: true,
    awaitPromise: false,
    userGesture: true
  });
  if (cleared?.exceptionDetails) {
    throw bridgeError(
      ERROR_ACTION_TARGET_INVALID,
      `browser_file_upload clear threw: ${runtimeExceptionText(cleared.exceptionDetails)}`
    );
  }
  if (cleared?.result?.value?.ok !== true) {
    throw bridgeError(ERROR_ACTION_TARGET_INVALID, "browser_file_upload clear returned no success readback");
  }
}

async function resolveFileInputObject(debuggee, selected, params) {
  const selector = stringOrNull(params.selector);
  const elementId = stringOrNull(params.elementId);
  const activeElement = Boolean(params.activeElement);
  const locatorCount = (selector ? 1 : 0) + (elementId ? 1 : 0) + (activeElement ? 1 : 0);
  if (locatorCount !== 1) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "browser_file_upload requires exactly one of selector, elementId, or activeElement=true for set_files/clear"
    );
  }
  const parsed = elementId
    ? parseChromeBridgeElementId(elementId, selected.tabId, "browser_file_upload")
    : { raw: null, frameId: 0, path: null };
  if (parsed.frameId !== null && parsed.frameId !== 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `browser_file_upload direct elementId currently supports top-frame bridge element ids only; got frame ${parsed.frameId}; use arm_chooser + click + set_chooser for framed inputs`
    );
  }
  const expression = `(${fileUploadResolveInputInPage.toString()})(${JSON.stringify({
    selector,
    elementPath: parsed.path,
    activeElement
  })})`;
  const evaluated = await sendDebuggerCommand(debuggee, "Runtime.evaluate", {
    expression,
    objectGroup: "synapseFileUpload",
    returnByValue: false,
    awaitPromise: false,
    userGesture: true
  });
  if (evaluated?.exceptionDetails) {
    throw bridgeError(
      ERROR_ACTION_TARGET_INVALID,
      `browser_file_upload input resolution threw: ${runtimeExceptionText(evaluated.exceptionDetails)}`
    );
  }
  const objectId = evaluated?.result?.objectId;
  if (!objectId) {
    throw bridgeError(
      ERROR_ACTION_TARGET_INVALID,
      "browser_file_upload input resolution returned no objectId"
    );
  }
  return {
    object_id: objectId,
    resolved_by: elementId ? "element_id" : (activeElement ? "active_element" : "selector"),
    match_count: 1,
    frame_id: 0,
    element_path: parsed.path
  };
}

async function setFilesForBackendNode(debuggee, backendNodeId, files, resolvedBy) {
  await sendDebuggerCommand(debuggee, "DOM.setFileInputFiles", {
    backendNodeId,
    files
  });
  const resolved = await sendDebuggerCommand(debuggee, "DOM.resolveNode", {
    backendNodeId,
    objectGroup: "synapseFileUpload"
  });
  const objectId = resolved?.object?.objectId;
  if (!objectId) {
    throw bridgeError(
      ERROR_ACTION_TARGET_INVALID,
      `browser_file_upload DOM.resolveNode returned no objectId for backendNodeId ${backendNodeId}`
    );
  }
  try {
    return await readFileInputObject(debuggee, objectId, {
      resolved_by: resolvedBy,
      match_count: 1,
      frame_id: null,
      backend_node_id: backendNodeId
    });
  } finally {
    try {
      await sendDebuggerCommand(debuggee, "Runtime.releaseObjectGroup", {
        objectGroup: "synapseFileUpload"
      });
    } catch (_) {
      // Best-effort cleanup.
    }
  }
}

async function readFileInputObject(debuggee, objectId, context) {
  const read = await sendDebuggerCommand(debuggee, "Runtime.callFunctionOn", {
    objectId,
    functionDeclaration: fileUploadReadInputInPage.toString(),
    returnByValue: true,
    awaitPromise: false,
    userGesture: true
  });
  if (read?.exceptionDetails) {
    throw bridgeError(
      ERROR_ACTION_TARGET_INVALID,
      `browser_file_upload readback threw: ${runtimeExceptionText(read.exceptionDetails)}`
    );
  }
  const value = read?.result?.value;
  if (!value || typeof value !== "object") {
    throw bridgeError(ERROR_ACTION_TARGET_INVALID, "browser_file_upload readback returned no object value");
  }
  return {
    ...context,
    ...value
  };
}

async function ensureFileChooserDebuggerSession(tabId, intercept, protocolVersion = "1.3") {
  const debuggee = { tabId };
  let session = FILE_CHOOSER_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    session = {
      protocolVersion,
      pageEnabled: false,
      interceptEnabled: false,
      attached: false,
      entries: [],
      nextSeq: 0,
      dropped: 0,
      capacity: MAX_FILE_CHOOSER_EVENT_BUFFER,
      armedAtUnixMs: 0,
      pendingSeq: null,
      openedCount: 0,
      handledCount: 0,
      canceledCount: 0,
      errorCount: 0
    };
    FILE_CHOOSER_DEBUGGER_SESSIONS.set(tabId, session);
  }
  let newlyArmed = false;
  if (!session.attached) {
    if (!persistentDebuggerSessionIsAttached(tabId)) {
      await chrome.debugger.attach(debuggee, protocolVersion);
    }
    session.attached = true;
    session.protocolVersion = protocolVersion;
    session.armedAtUnixMs = Date.now();
    newlyArmed = true;
  }
  if (!session.pageEnabled) {
    await sendDebuggerCommand(debuggee, "Page.enable", {});
    session.pageEnabled = true;
  }
  if (intercept && !session.interceptEnabled) {
    await sendDebuggerCommand(debuggee, "Page.setInterceptFileChooserDialog", { enabled: true });
    session.interceptEnabled = true;
  }
  return { debuggee, session, newlyArmed, protocolVersion };
}

function markFileChooserDebuggerDetached(tabId, clearPending = true) {
  const session = FILE_CHOOSER_DEBUGGER_SESSIONS.get(tabId);
  if (!session) {
    return;
  }
  session.attached = false;
  session.pageEnabled = false;
  session.interceptEnabled = false;
  if (clearPending) {
    session.pendingSeq = null;
  }
}

function recordFileChooserOpenedEvent(tabId, params) {
  const session = FILE_CHOOSER_DEBUGGER_SESSIONS.get(tabId);
  if (!session?.attached) {
    return;
  }
  const entry = {
    seq: Number(session.nextSeq || 0),
    frame_id: String(params?.frameId || ""),
    mode: String(params?.mode || ""),
    backend_node_id: Number.isSafeInteger(params?.backendNodeId) ? params.backendNodeId : null,
    opened_at_unix_ms: Date.now(),
    pending: true,
    handled_at_unix_ms: null,
    canceled_at_unix_ms: null,
    requested_file_count: null,
    file_names: [],
    input: null,
    error: null
  };
  session.nextSeq += 1;
  while (session.entries.length >= session.capacity) {
    session.entries.shift();
    session.dropped += 1;
  }
  session.pendingSeq = entry.seq;
  session.openedCount += 1;
  session.entries.push(entry);
}

function fileChooserPendingEntry(session) {
  if (!session || session.pendingSeq === null || session.pendingSeq === undefined) {
    return null;
  }
  return session.entries.find((entry) => entry.seq === session.pendingSeq) || null;
}

function markFileChooserHandled(session, seq, files, inputReadback) {
  const entry = session.entries.find((candidate) => candidate.seq === seq);
  if (!entry) {
    return null;
  }
  entry.pending = false;
  entry.handled_at_unix_ms = Date.now();
  entry.requested_file_count = files.length;
  entry.file_names = files.map(fileBaseName);
  entry.input = inputReadback;
  entry.error = null;
  session.pendingSeq = null;
  session.handledCount += 1;
  return { ...entry };
}

function markFileChooserCanceled(session, seq) {
  const entry = session.entries.find((candidate) => candidate.seq === seq);
  if (!entry) {
    return null;
  }
  entry.pending = false;
  entry.canceled_at_unix_ms = Date.now();
  session.pendingSeq = null;
  session.canceledCount += 1;
  return { ...entry };
}

function readFileChooserEntries(session, sinceSeq, limit) {
  const cursor = sinceSeq === null ? 0 : sinceSeq;
  const entries = session.entries
    .filter((entry) => entry.seq >= cursor)
    .slice(0, limit)
    .map((entry) => ({ ...entry }));
  const pending = fileChooserPendingEntry(session);
  return {
    entries,
    pending_chooser: pending ? { ...pending } : null,
    next_cursor: Number(session.nextSeq || 0),
    returned: entries.length,
    total_buffered: session.entries.length,
    dropped: Number(session.dropped || 0)
  };
}

function emptyFileChooserRead() {
  return {
    entries: [],
    pending_chooser: null,
    next_cursor: 0,
    returned: 0,
    total_buffered: 0,
    dropped: 0
  };
}

function normalizeFileUploadOperation(value) {
  const normalized = String(value || "set_files").trim().toLowerCase().replace(/-/g, "_");
  if (["set_files", "clear", "arm_chooser", "read_chooser", "set_chooser", "cancel_chooser"].includes(normalized)) {
    return normalized;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `browser_file_upload operation must be set_files, clear, arm_chooser, read_chooser, set_chooser, or cancel_chooser; got ${JSON.stringify(value)}`
  );
}

function normalizeFileUploadPaths(value, operation) {
  if (operation === "clear" || operation === "arm_chooser" || operation === "read_chooser" || operation === "cancel_chooser") {
    return [];
  }
  if (!Array.isArray(value) || value.length === 0) {
    throw bridgeError(ERROR_ACTION_TARGET_INVALID, `browser_file_upload ${operation} requires one or more files`);
  }
  if (value.length > MAX_FILE_UPLOAD_PATHS) {
    throw bridgeError(
      ERROR_ACTION_TARGET_INVALID,
      `browser_file_upload supports at most ${MAX_FILE_UPLOAD_PATHS} files per call, got ${value.length}`
    );
  }
  return value.map((item, index) => {
    const text = String(item || "");
    if (!text) {
      throw bridgeError(ERROR_ACTION_TARGET_INVALID, `browser_file_upload files[${index}] must not be empty`);
    }
    if (text.length > MAX_FILE_UPLOAD_PATH_CHARS) {
      throw bridgeError(
        ERROR_ACTION_TARGET_INVALID,
        `browser_file_upload files[${index}] is longer than ${MAX_FILE_UPLOAD_PATH_CHARS} characters`
      );
    }
    if (text.includes("\u0000")) {
      throw bridgeError(ERROR_ACTION_TARGET_INVALID, `browser_file_upload files[${index}] must not contain NUL`);
    }
    return text;
  });
}

function normalizeFileChooserLimit(value) {
  if (value === undefined || value === null) {
    return 20;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > 100) {
    throw bridgeError(ERROR_ATTACH_FAILED, `browser_file_upload limit must be an integer from 1 through 100, got ${JSON.stringify(value)}`);
  }
  return number;
}

function fileBaseName(path) {
  const text = String(path || "");
  const normalized = text.replace(/\\/g, "/");
  const parts = normalized.split("/");
  return parts[parts.length - 1] || text;
}

function runtimeExceptionText(exceptionDetails) {
  return String(
    exceptionDetails?.exception?.description ||
    exceptionDetails?.text ||
    exceptionDetails?.exception?.value ||
    "unknown Runtime exception"
  );
}

function fileUploadResolveInputInPage(request) {
  const selector = request && request.selector ? String(request.selector) : "";
  const elementPath = request && request.elementPath ? String(request.elementPath) : "";
  const activeElement = Boolean(request && request.activeElement);

  function elementByPath(path) {
    if (!path) {
      return null;
    }
    // Shadow-aware path: numeric tokens index into children; the literal token
    // "s" means the next numeric token indexes into the current element's open
    // shadowRoot.children. Pure-numeric (legacy light-DOM) paths still resolve.
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children[index] || null;
    }
    return current instanceof Element ? current : null;
  }

  function isVisible(element) {
    if (!element || typeof element.getClientRects !== "function") {
      return false;
    }
    if (element.getClientRects().length > 0) {
      return true;
    }
    return Boolean(element.offsetWidth || element.offsetHeight);
  }

  function isFileInput(element) {
    return element instanceof HTMLInputElement &&
      String(element.getAttribute("type") || "").toLowerCase() === "file" &&
      !element.disabled &&
      isVisible(element);
  }

  let element = null;
  if (selector) {
    let nodes;
    try {
      nodes = Array.from(document.querySelectorAll(selector));
    } catch (error) {
      throw new Error(`selector ${JSON.stringify(selector)} is invalid: ${error && error.message || error}`);
    }
    const inputs = nodes.filter(isFileInput);
    if (inputs.length === 0) {
      throw new Error(`selector ${JSON.stringify(selector)} matched ${nodes.length} node(s), 0 enabled visible file inputs`);
    }
    if (inputs.length > 1) {
      throw new Error(`selector ${JSON.stringify(selector)} matched ${inputs.length} enabled visible file inputs; refine the selector`);
    }
    element = inputs[0];
  } else if (elementPath) {
    element = elementByPath(elementPath);
    if (!isFileInput(element)) {
      throw new Error(`element path ${JSON.stringify(elementPath)} did not resolve to an enabled visible input[type=file]`);
    }
  } else if (activeElement) {
    element = document.activeElement;
    if (!isFileInput(element)) {
      throw new Error("activeElement is not an enabled visible input[type=file]");
    }
  }
  if (!element) {
    throw new Error("no file input resolved");
  }
  return element;
}

function fileUploadReadInputInPage() {
  const element = this;
  if (!(element instanceof HTMLInputElement) || String(element.type || "").toLowerCase() !== "file") {
    throw new Error("resolved object is not an input[type=file]");
  }
  const files = Array.from(element.files || []).map((file) => ({
    name: String(file.name || ""),
    size: Number(file.size || 0),
    type: String(file.type || ""),
    last_modified: Number(file.lastModified || 0)
  }));
  return {
    tag_name: "input",
    type_attr: String(element.type || ""),
    id: String(element.id || ""),
    name_attr: String(element.getAttribute("name") || ""),
    accept: String(element.getAttribute("accept") || ""),
    multiple: Boolean(element.multiple),
    webkitdirectory: Boolean(element.webkitdirectory),
    disabled: Boolean(element.disabled),
    file_count: files.length,
    files,
    value: String(element.value || "")
  };
}

function fileUploadClearInputInPage() {
  const element = this;
  if (!(element instanceof HTMLInputElement) || String(element.type || "").toLowerCase() !== "file") {
    throw new Error("resolved object is not an input[type=file]");
  }
  element.value = "";
  element.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  element.dispatchEvent(new Event("change", { bubbles: true, composed: true }));
  return {
    ok: true,
    file_count: element.files ? element.files.length : 0,
    value: String(element.value || "")
  };
}

function promiseWithTimeout(promise, timeoutMs, message) {
  let timer = null;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(message)), timeoutMs);
    })
  ]).finally(() => {
    if (timer !== null) {
      clearTimeout(timer);
    }
  });
}

function handleReloadSelf(params = {}) {
  const expectedExtensionId = String(params.expectedExtensionId || EXPECTED_EXTENSION_ID).trim();
  if (expectedExtensionId !== chrome.runtime.id) {
    throw bridgeError(
      ERROR_EXTENSION_ID_MISMATCH,
      `reloadSelf refused extension_id mismatch: actual=${chrome.runtime.id} expected=${expectedExtensionId}`
    );
  }
  const expectedBuildId = String(params.expectedBuildId || BRIDGE_BUILD_ID).trim();
  if (expectedBuildId && expectedBuildId !== BRIDGE_BUILD_ID) {
    throw bridgeError(
      ERROR_EXTENSION_STALE,
      `reloadSelf refused stale loaded build: loaded_build_id=${BRIDGE_BUILD_ID} expected_build_id=${expectedBuildId}`
    );
  }
  const delayMs = normalizeReloadDelay(params.reloadDelayMs);
  return {
    ok: true,
    ...bridgeIdentity(),
    host_id: hostId,
    reload_requested_at_unix_ms: Date.now(),
    reload_delay_ms: delayMs
  };
}

function scheduleRuntimeReload(delayMs) {
  const boundedDelayMs = normalizeReloadDelay(delayMs);
  postDaemonMessage({
    type: "event",
    event: "extensionReloadScheduled",
    ...bridgeIdentity(),
    reload_delay_ms: boundedDelayMs,
    scheduled_at_unix_ms: Date.now()
  }).catch((error) => {
    console.warn(`Synapse reload schedule event dropped: ${errorMessage(error)}`);
  });
  setTimeout(() => {
    try {
      chrome.runtime.reload();
    } catch (error) {
      console.error(`Synapse runtime.reload failed: ${errorMessage(error)}`);
    }
  }, boundedDelayMs);
}

async function selectTabTarget(params, options = {}) {
  const tabs = await tabTargets();
  if (tabs.length === 0) {
    throw bridgeError(ERROR_AXTREE_FAILED, "chrome.tabs.query returned no tab targets");
  }
  const targetIdHint = String(params.targetIdHint || "").trim();
  const expectedWindowId = optionalInteger(params.expectedChromeWindowId);
  const tabIdHint = tabIdFromTargetId(targetIdHint);
  if (Number.isInteger(tabIdHint)) {
    const selectedById = tabs.find((target) => target.tabId === tabIdHint);
    if (selectedById) {
      if (
        Number.isInteger(expectedWindowId) &&
        Number.isInteger(selectedById.chromeWindowId) &&
        selectedById.chromeWindowId !== expectedWindowId
      ) {
        throw bridgeError(
          ERROR_AXTREE_FAILED,
          `targetIdHint ${targetIdHint} is in Chrome window ${selectedById.chromeWindowId}, expected ${expectedWindowId}`
        );
      }
      return selectedPage(selectedById, tabs.length, "chrome_tab_id_hint");
    }
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `targetIdHint ${targetIdHint} did not match any chrome.tabs tab id`
    );
  }
  if (options.requireTargetId) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      targetIdHint
        ? `targetIdHint ${targetIdHint} is not a tabs bridge target id; expected ${TAB_TARGET_PREFIX}<tabId>`
        : `targetIdHint is required for mutating tab navigation; expected ${TAB_TARGET_PREFIX}<tabId>`
    );
  }
  let effectiveExpectedWindowId = expectedWindowId;
  const expectedWindow = await expectedChromeWindowForHwndHint(params);
  if (Number.isInteger(expectedWindow?.windowId)) {
    if (Number.isInteger(effectiveExpectedWindowId) && effectiveExpectedWindowId !== expectedWindow.windowId) {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `expectedChromeWindowId ${effectiveExpectedWindowId} disagrees with hwnd ${String(params.hwnd)} mapping ${expectedWindow.windowId}`
      );
    }
    effectiveExpectedWindowId = expectedWindow.windowId;
  }
  const scopedTabs = Number.isInteger(effectiveExpectedWindowId)
    ? tabs.filter((target) => target.chromeWindowId === effectiveExpectedWindowId)
    : tabs;
  const urlHint = String(params.foregroundUrlHint || "").trim();
  const titleHint = String(params.foregroundTitle || "").trim();
  if (urlHint) {
    const matches = scopedTabs.filter((target) => urlMatchesHint(target.url || "", urlHint));
    if (matches.length === 1) {
      return selectedPage(matches[0], tabs.length, "url_hint");
    }
    if (matches.length > 1) {
      throw bridgeError(ERROR_AXTREE_FAILED, `url hint matched ${matches.length} tab targets`);
    }
  }
  if (titleHint) {
    const matches = scopedTabs.filter((target) => {
      const title = target.title || "";
      return title && (titleHint.includes(title) || title.includes(titleHint));
    });
    if (matches.length === 1) {
      return selectedPage(matches[0], tabs.length, "foreground_title");
    }
    if (matches.length > 1) {
      throw bridgeError(ERROR_AXTREE_FAILED, `title hint matched ${matches.length} tab targets`);
    }
  }
  if (Number.isInteger(effectiveExpectedWindowId)) {
    if (scopedTabs.length === 1) {
      return selectedPage(scopedTabs[0], tabs.length, "expected_chrome_window_id_single_tab");
    }
    if (scopedTabs.length > 1) {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `expectedChromeWindowId ${effectiveExpectedWindowId} contains ${scopedTabs.length} tab targets; targetIdHint is required`
      );
    }
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `expectedChromeWindowId ${effectiveExpectedWindowId} matched no tab targets`
    );
  }
  return selectedPage(tabs[0], tabs.length, "fallback_first_tab");
}

async function handleCoordinateClick(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const waitTimeoutMs = normalizeWaitTimeout(params.waitTimeoutMs);
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }

  const before = await tabPageState(selected.tabId, selected.target);
  const beforePageText = await tabPageTextState(selected.tabId);
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId },
      func: performCoordinateClickInPage,
      args: [{
        x: normalizeCoordinateValue(params.x, "x"),
        y: normalizeCoordinateValue(params.y, "y"),
        coordinateSpace: normalizeCoordinateSpace(params.coordinateSpace),
        clicks: normalizeClickCount(params.clicks),
        button: normalizeMouseButton(params.button, "coordinateClick"),
        modifiers: normalizeClickModifiers(params.modifiers, "coordinateClick"),
        maxPageTextChars: MAX_PAGE_TEXT_CHARS
      }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript coordinateClick(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }

  const first = Array.isArray(injected) ? injected[0] : null;
  const clickResult = first?.result;
  if (!clickResult || typeof clickResult !== "object") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript coordinateClick returned no structured result"
    );
  }
  if (!clickResult.ok) {
    throw bridgeError(
      String(clickResult.error_code || ERROR_AXTREE_FAILED),
      `coordinateClick failed: ${String(clickResult.error_detail || "")}`
    );
  }

  const after = await waitForTabPageState(selected.tabId, selected.target, waitTimeoutMs);
  const afterPageText = await tabPageTextState(selected.tabId);
  const activeElement = await tabActiveElementState(selected.tabId);
  return {
    extension_id: chrome.runtime.id,
    target_id: after.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: after.chrome_window_id,
    before_page: before,
    after_page: after,
    before_page_text: beforePageText,
    after_page_text: afterPageText,
    active_element: activeElement,
    readback_backend: "chrome.scripting.executeScript+chrome.tabs.get",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason,
    ...clickResult
  };
}

async function selectOpenWindowForHwndHint(params) {
  if (params.hwnd === undefined || params.hwnd === null) {
    return null;
  }
  let windows;
  try {
    windows = await chrome.windows.getAll({ windowTypes: ["normal"] });
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.windows.getAll(normal): ${errorMessage(error)}`);
  }
  const candidates = windows
    .filter((windowInfo) => Number.isInteger(windowInfo.id))
    .map((windowInfo) => ({
      id: windowInfo.id,
      focused: Boolean(windowInfo.focused),
      state: String(windowInfo.state || ""),
      type: String(windowInfo.type || ""),
      bounds: chromeWindowBounds(windowInfo),
      activeTabTitle: "",
      activeTabUrl: ""
    }));
  await Promise.all(candidates.map(async (candidate) => {
    const activeTab = await activeTabForWindow(candidate.id);
    candidate.activeTabTitle = String(activeTab?.title || "");
    candidate.activeTabUrl = String(activeTab?.url || "");
  }));
  const expectedWindowId = optionalInteger(params.expectedChromeWindowId);
  if (Number.isInteger(expectedWindowId)) {
    const selected = candidates.find((windowInfo) => windowInfo.id === expectedWindowId);
    if (!selected) {
      const candidateSummary = candidates
        .map((windowInfo) => chromeWindowCandidateSummaryItem(windowInfo, normalizeExpectedWindowBounds(params.expectedWindowBounds)))
        .join(",");
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `Chrome window mapping refused hwnd hint ${String(params.hwnd)}: expectedChromeWindowId ${expectedWindowId} was not present; ` +
          `candidate_count=${candidates.length} candidates=[${candidateSummary}]`
      );
    }
    return {
      windowId: selected.id,
      focused: selected.focused,
      state: selected.state,
      selectionReason: "expected_chrome_window_id_for_hwnd_hint",
      candidateCount: candidates.length,
      nonFocusedCount: candidates.filter((windowInfo) => !windowInfo.focused).length
    };
  }
  const expectedBounds = normalizeExpectedWindowBounds(params.expectedWindowBounds);
  let ambiguousBoundsMatches = [];
  if (expectedBounds) {
    const boundsMatches = candidates
      .map((windowInfo) => ({
        windowInfo,
        score: windowBoundsDeltaScore(windowInfo.bounds, expectedBounds)
      }))
      .filter((candidate) => Number.isFinite(candidate.score) && candidate.score <= OPEN_WINDOW_BOUNDS_TOLERANCE_PX)
      .sort((left, right) => left.score - right.score);
    if (boundsMatches.length === 1 || (boundsMatches.length > 1 && boundsMatches[0].score < boundsMatches[1].score)) {
      const selected = boundsMatches[0].windowInfo;
      return {
        windowId: selected.id,
        focused: selected.focused,
        state: selected.state,
        selectionReason: "passive_window_bounds_match_for_hwnd_hint",
        candidateCount: candidates.length,
        nonFocusedCount: candidates.filter((windowInfo) => !windowInfo.focused).length
      };
    }
    if (boundsMatches.length > 1) {
      ambiguousBoundsMatches = boundsMatches.map((candidate) => candidate.windowInfo);
    }
  }
  const expectedTitle = normalizeExpectedWindowTitle(params.expectedWindowTitle);
  if (expectedTitle) {
    const titleCandidates = ambiguousBoundsMatches.length > 0 ? ambiguousBoundsMatches : candidates;
    const titleMatches = titleCandidates.filter((windowInfo) =>
      chromeWindowTitleMatches(windowInfo.activeTabTitle, expectedTitle)
    );
    if (titleMatches.length === 1) {
      const selected = titleMatches[0];
      return {
        windowId: selected.id,
        focused: selected.focused,
        state: selected.state,
        selectionReason: ambiguousBoundsMatches.length > 0
          ? "passive_window_bounds_and_title_match_for_hwnd_hint"
          : "passive_active_tab_title_match_for_hwnd_hint",
        candidateCount: candidates.length,
        nonFocusedCount: candidates.filter((windowInfo) => !windowInfo.focused).length
      };
    }
    if (titleMatches.length > 1) {
      throw bridgeError(
        ERROR_AXTREE_FAILED,
        `Chrome window mapping refused hwnd hint ${String(params.hwnd)}: expected_window_title matched ${titleMatches.length} Chrome windows`
      );
    }
  }
  if (ambiguousBoundsMatches.length > 1) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `Chrome window mapping refused hwnd hint ${String(params.hwnd)}: expected_window_bounds matched ${ambiguousBoundsMatches.length} Chrome windows within ${OPEN_WINDOW_BOUNDS_TOLERANCE_PX}px and expected_window_title did not disambiguate`
    );
  }
  if (expectedBounds || expectedTitle) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `Chrome window mapping refused hwnd hint ${String(params.hwnd)}: expected_window_bounds/title did not identify a Chrome window; ` +
        `candidate_count=${candidates.length} candidates=[${chromeWindowCandidateSummary(candidates, expectedBounds)}]`
    );
  }
  const nonFocused = candidates.filter((windowInfo) => !windowInfo.focused);
  const nonFocusedNonMinimized = nonFocused.filter((windowInfo) => windowInfo.state !== "minimized");
  if (nonFocusedNonMinimized.length === 1) {
    const selected = nonFocusedNonMinimized[0];
    return {
      windowId: selected.id,
      focused: selected.focused,
      state: selected.state,
      selectionReason: "single_non_focused_non_minimized_chrome_window_for_hwnd_hint",
      candidateCount: candidates.length,
      nonFocusedCount: nonFocused.length
    };
  }
  if (nonFocused.length === 1) {
    return {
      windowId: nonFocused[0].id,
      focused: nonFocused[0].focused,
      state: nonFocused[0].state,
      selectionReason: "single_non_focused_chrome_window_for_hwnd_hint",
      candidateCount: candidates.length,
      nonFocusedCount: nonFocused.length
    };
  }
  if (nonFocused.length === 0 && candidates.length === 1 && candidates[0].focused) {
    return {
      windowId: candidates[0].id,
      focused: true,
      state: candidates[0].state,
      selectionReason: "focused_chrome_window_inactive_tab_for_hwnd_hint",
      candidateCount: candidates.length,
      nonFocusedCount: nonFocused.length
    };
  }
  const candidateSummary = candidates
    .map((windowInfo) => chromeWindowCandidateSummaryItem(windowInfo, expectedBounds))
    .join(",");
  throw bridgeError(
    ERROR_AXTREE_FAILED,
    `Chrome window mapping refused hwnd hint ${String(params.hwnd)}: candidate_count=${candidates.length} non_focused_count=${nonFocused.length} non_focused_non_minimized_count=${nonFocusedNonMinimized.length} candidates=[${candidateSummary}]; OS HWND cannot be mapped inside the normal extension bridge`
  );
}

async function expectedChromeWindowForHwndHint(params) {
  if (params.hwnd === undefined || params.hwnd === null) {
    return null;
  }
  if (!normalizeExpectedWindowBounds(params.expectedWindowBounds) && !normalizeExpectedWindowTitle(params.expectedWindowTitle)) {
    return null;
  }
  return selectOpenWindowForHwndHint(params);
}

async function activeTabForWindow(windowId) {
  if (!Number.isInteger(windowId)) {
    return null;
  }
  try {
    const tabs = await chrome.tabs.query({ windowId, active: true });
    return tabs[0] || null;
  } catch (_) {
    return null;
  }
}

function normalizeExpectedWindowBounds(raw) {
  if (!raw || typeof raw !== "object") {
    return null;
  }
  const x = Number(raw.x);
  const y = Number(raw.y);
  const w = Number(raw.w);
  const h = Number(raw.h);
  if (![x, y, w, h].every(Number.isFinite) || w <= 0 || h <= 0) {
    return null;
  }
  return { x, y, w, h };
}

function normalizeExpectedWindowTitle(raw) {
  if (raw === undefined || raw === null) {
    return "";
  }
  return String(raw).trim();
}

function titleCompareVariants(raw) {
  const base = String(raw || "")
    .toLowerCase()
    .replace(/\s+/g, " ")
    .trim();
  if (!base) {
    return [];
  }
  const withoutChromeSuffix = base.replace(/\s+-\s+google chrome$/, "").trim();
  const withoutAttentionPrefix = withoutChromeSuffix.replace(/^\(\d+\)\s*/, "").trim();
  return Array.from(new Set([
    base,
    withoutChromeSuffix,
    withoutAttentionPrefix
  ].filter((value) => value.length >= 3)));
}

function chromeWindowTitleMatches(activeTabTitle, expectedWindowTitle) {
  const actualVariants = titleCompareVariants(activeTabTitle);
  const expectedVariants = titleCompareVariants(expectedWindowTitle);
  for (const actual of actualVariants) {
    for (const expected of expectedVariants) {
      if (actual === expected || actual.includes(expected) || expected.includes(actual)) {
        return true;
      }
    }
  }
  return false;
}

function chromeWindowCandidateSummary(candidates, expectedBounds) {
  return candidates
    .map((windowInfo) => chromeWindowCandidateSummaryItem(windowInfo, expectedBounds))
    .join(",");
}

function chromeWindowCandidateSummaryItem(windowInfo, expectedBounds) {
  const score = expectedBounds
    ? windowBoundsDeltaScore(windowInfo.bounds, expectedBounds)
    : Number.POSITIVE_INFINITY;
  const scoreText = Number.isFinite(score) ? String(score) : "na";
  const title = String(windowInfo.activeTabTitle || "").replace(/\s+/g, " ").trim().slice(0, 80);
  return `id=${windowInfo.id}/focused=${windowInfo.focused}/state=${windowInfo.state || "unknown"}/score=${scoreText}/title=${JSON.stringify(title)}`;
}

function chromeWindowBounds(windowInfo) {
  const left = Number(windowInfo.left);
  const top = Number(windowInfo.top);
  const width = Number(windowInfo.width);
  const height = Number(windowInfo.height);
  if (![left, top, width, height].every(Number.isFinite) || width <= 0 || height <= 0) {
    return null;
  }
  return { x: left, y: top, w: width, h: height };
}

function windowBoundsDeltaScore(actual, expected) {
  if (!actual || !expected) {
    return Number.POSITIVE_INFINITY;
  }
  let best = Number.POSITIVE_INFINITY;
  for (const scale of WINDOW_BOUNDS_SCALE_CANDIDATES) {
    const score = Math.max(
      Math.abs((actual.x * scale) - expected.x),
      Math.abs((actual.y * scale) - expected.y),
      Math.abs((actual.w * scale) - expected.w),
      Math.abs((actual.h * scale) - expected.h)
    );
    if (score < best) {
      best = score;
    }
  }
  return best;
}

async function chromeWindowState(windowId) {
  if (!Number.isInteger(windowId)) {
    return null;
  }
  try {
    const windowInfo = await chrome.windows.get(windowId);
    return {
      id: windowInfo.id,
      focused: Boolean(windowInfo.focused),
      state: String(windowInfo.state || ""),
      type: String(windowInfo.type || "")
    };
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.windows.get(${windowId}) failed: ${errorMessage(error)}`
    );
  }
}

async function focusedChromeWindow() {
  let windows;
  try {
    windows = await chrome.windows.getAll({ windowTypes: ["normal"] });
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.windows.getAll(normal): ${errorMessage(error)}`);
  }
  return windows.find((windowInfo) => Boolean(windowInfo.focused)) || null;
}

async function waitForChromeWindowFocused(windowId, expected, waitTimeoutMs) {
  const started = Date.now();
  let last = null;
  while (Date.now() - started <= waitTimeoutMs) {
    last = await chromeWindowState(windowId);
    if (Boolean(last?.focused) === expected) {
      return last;
    }
    await sleep(50);
  }
  throw bridgeError(
    ERROR_EXTENSION_TIMEOUT,
    `Chrome window ${windowId} focused=${expected} readback did not settle within ${waitTimeoutMs}ms; last_focused=${Boolean(last?.focused)}`
  );
}

async function tabTargets() {
  let tabs;
  try {
    tabs = await chrome.tabs.query({});
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.tabs.query: ${errorMessage(error)}`);
  }
  return tabs
    .filter((tab) => typeof tab.id === "number")
    .map((tab) => tabTargetFromTab(tab));
}

function selectedPage(target, targetCandidateCount, selectionReason) {
  return {
    target,
    tabId: target.tabId,
    url: target.url || "",
    title: target.title || "",
    targetCandidateCount,
    selectionReason
  };
}

function tabTargetFromTab(tab) {
  return {
    id: targetIdForTabId(tab.id),
    tabId: tab.id,
    chromeWindowId: typeof tab.windowId === "number" ? tab.windowId : null,
    type: "page",
    url: String(tab.pendingUrl || tab.url || ""),
    title: String(tab.title || ""),
    status: String(tab.status || ""),
    active: Boolean(tab.active),
    highlighted: Boolean(tab.highlighted),
    pinned: Boolean(tab.pinned),
    attached: false
  };
}

function tabPageStateFromTab(tab) {
  const target = tabTargetFromTab(tab);
  return {
    target_id: target.id,
    tab_id: target.tabId,
    chrome_window_id: target.chromeWindowId,
    target_type: target.type || "page",
    url: target.url || "",
    title: target.title || "",
    ready_state: target.status || "",
    active: Boolean(target.active),
    highlighted: Boolean(target.highlighted),
    pinned: Boolean(target.pinned),
    history_current_index: -1,
    history_entry_count: 0
  };
}

function tabListEntryFromTabTarget(target, tab) {
  return {
    target_id: target.id,
    tab_id: target.tabId,
    chrome_window_id: target.chromeWindowId,
    index: Number.isInteger(tab.index) ? tab.index : -1,
    target_type: target.type || "page",
    url: target.url || "",
    title: target.title || "",
    ready_state: target.status || "",
    active: Boolean(target.active),
    highlighted: Boolean(target.highlighted),
    pinned: Boolean(target.pinned),
    target_attached: Boolean(target.attached)
  };
}

function targetIdForTabId(tabId) {
  return `${TAB_TARGET_PREFIX}${tabId}`;
}

function tabIdFromTargetId(targetId) {
  if (!targetId.startsWith(TAB_TARGET_PREFIX)) {
    return null;
  }
  const value = Number(targetId.slice(TAB_TARGET_PREFIX.length));
  if (!Number.isSafeInteger(value) || value < 0) {
    return null;
  }
  return value;
}

function urlMatchesHint(url, hint) {
  return url === hint || url.startsWith(hint) || hint.startsWith(url);
}

async function waitForTabTarget(tabId, waitTimeoutMs) {
  const started = Date.now();
  let lastCount = 0;
  while (Date.now() - started <= waitTimeoutMs) {
    const pages = await tabTargets();
    lastCount = pages.length;
    const target = pages.find((candidate) => candidate.tabId === tabId);
    if (target?.id) {
      return target;
    }
    await sleep(100);
  }
  throw bridgeError(
    ERROR_EXTENSION_TIMEOUT,
    `chrome.tabs.query did not expose a tab target for new tab ${tabId} within ${waitTimeoutMs} ms; lastTabTargetCount=${lastCount}`
  );
}

async function waitForTargetAbsent(targetId, waitTimeoutMs) {
  const started = Date.now();
  let pages = [];
  while (Date.now() - started <= waitTimeoutMs) {
    pages = await tabTargets();
    if (!pages.some((candidate) => candidate.id === targetId)) {
      return pages;
    }
    await sleep(100);
  }
  throw bridgeError(
    ERROR_EXTENSION_TIMEOUT,
    `chrome.tabs.query still contains closed target ${JSON.stringify(targetId)} after ${waitTimeoutMs} ms; lastTabTargetCount=${pages.length}`
  );
}

async function waitForTabPageState(tabId, fallbackTarget, waitTimeoutMs, expectation = null) {
  const started = Date.now();
  let last = null;
  let lastError = null;
  while (Date.now() - started <= waitTimeoutMs) {
    try {
      last = await tabPageState(tabId, fallbackTarget);
      lastError = null;
      const loaded = last.ready_state === "complete";
      if (loaded && (!expectation || expectation.matches(last))) {
        return last;
      }
    } catch (error) {
      lastError = error?.message ? String(error.message) : String(error);
    }
    await sleep(100);
  }
  const detail = last
    ? `waiting for ${expectation?.description || "complete tab state"}; ` +
      `last url=${JSON.stringify(last.url)} title=${JSON.stringify(last.title)} ` +
      `status=${JSON.stringify(last.ready_state)} targetId=${JSON.stringify(last.target_id)}`
    : lastError
      ? `last readback error=${JSON.stringify(lastError)}`
    : "no tab state readback";
  throw bridgeError(ERROR_EXTENSION_TIMEOUT, `tab readback did not settle within ${waitTimeoutMs} ms; ${detail}`);
}

async function tabPageState(tabId, fallbackTarget = null) {
  let tab;
  try {
    tab = await chrome.tabs.get(tabId);
  } catch (error) {
    throw bridgeError(ERROR_AXTREE_FAILED, `chrome.tabs.get(${tabId}): ${errorMessage(error)}`);
  }
  const target = tabTargetFromTab(tab);
  return {
    target_id: String(target?.id || fallbackTarget?.id || ""),
    tab_id: tabId,
    chrome_window_id: typeof tab.windowId === "number" ? tab.windowId : target?.chromeWindowId || null,
    target_type: String(target?.type || fallbackTarget?.type || "page"),
    url: String(tab.pendingUrl || tab.url || target?.url || fallbackTarget?.url || ""),
    title: String(tab.title || target?.title || fallbackTarget?.title || ""),
    ready_state: String(tab.status || ""),
    active: Boolean(tab.active),
    highlighted: Boolean(tab.highlighted),
    pinned: Boolean(tab.pinned),
    history_current_index: -1,
    history_entry_count: 0
  };
}

async function tabActiveElementState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      error_code: "CHROME_SCRIPTING_UNAVAILABLE",
      error_detail: "Chrome scripting API is unavailable; extension is missing scripting permission"
    };
  }
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      func: readActiveElementInPage
    });
    const frames = frameExecutionResults(results);
    const selected =
      frames.find((frame) => frame.result?.has_active_element && frame.result?.is_editable) ||
      frames.find((frame) => frame.result?.has_active_element) ||
      frames.find((frame) => frame.result) ||
      null;
    if (!selected || typeof selected.result !== "object" || selected.result === null) {
      return {
        available: false,
        readback_source: "chrome.scripting.executeScript",
        error_code: "CHROME_SCRIPTING_EMPTY_RESULT",
        error_detail: "chrome.scripting.executeScript returned no active-element result"
      };
    }
    return {
      available: true,
      readback_source: "chrome.scripting.executeScript",
      frame_id: selected.frame_id,
      frame_document_id: selected.document_id,
      frame_result_count: frames.length,
      frame_results: frames.map(summarizeFrameExecutionResult),
      ...selected.result
    };
  } catch (error) {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: errorMessage(error)
    };
  }
}

async function tabPageTextState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      text: null,
      text_len: 0,
      text_truncated: false,
      max_chars: MAX_PAGE_TEXT_CHARS,
      error_code: "CHROME_SCRIPTING_UNAVAILABLE",
      error_detail: "Chrome scripting API is unavailable; extension is missing scripting permission"
    };
  }
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      func: readPageTextInPage,
      args: [MAX_PAGE_TEXT_CHARS]
    });
    const frames = frameExecutionResults(results);
    const aggregated = aggregatePageTextFrames(frames, MAX_PAGE_TEXT_CHARS);
    if (!aggregated) {
      return {
        available: false,
        readback_source: "chrome.scripting.executeScript",
        text: null,
        text_len: 0,
        text_truncated: false,
        max_chars: MAX_PAGE_TEXT_CHARS,
        error_code: "CHROME_SCRIPTING_EMPTY_RESULT",
        error_detail: "chrome.scripting.executeScript returned no page-text result"
      };
    }
    return {
      available: true,
      readback_source: "chrome.scripting.executeScript",
      ...aggregated
    };
  } catch (error) {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      text: null,
      text_len: 0,
      text_truncated: false,
      max_chars: MAX_PAGE_TEXT_CHARS,
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: errorMessage(error)
    };
  }
}

async function tabPageContentState(tabId, maxBytes) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      html: null,
      html_len: 0,
      truncated: false,
      max_bytes: maxBytes,
      error_code: "CHROME_SCRIPTING_UNAVAILABLE",
      error_detail: "Chrome scripting API is unavailable; extension is missing scripting permission"
    };
  }
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId },
      func: readPageContentInPage,
      args: [maxBytes]
    });
    const frames = frameExecutionResults(results);
    const selected = frames.find((frame) => frame.result) || null;
    if (!selected || typeof selected.result !== "object" || selected.result === null) {
      return {
        available: false,
        readback_source: "chrome.scripting.executeScript",
        html: null,
        html_len: 0,
        truncated: false,
        max_bytes: maxBytes,
        error_code: "CHROME_SCRIPTING_EMPTY_RESULT",
        error_detail: "chrome.scripting.executeScript returned no page-content result"
      };
    }
    return {
      available: true,
      readback_source: "chrome.scripting.executeScript",
      frame_id: selected.frame_id,
      frame_document_id: selected.document_id,
      ...selected.result
    };
  } catch (error) {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      html: null,
      html_len: 0,
      truncated: false,
      max_bytes: maxBytes,
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: errorMessage(error)
    };
  }
}

function trimForReadback(value, maxChars) {
  const limit = Number.isSafeInteger(maxChars) && maxChars >= 0 ? maxChars : 240;
  return Array.from(String(value || "").replace(/\s+/g, " ").trim()).slice(0, limit).join("");
}

function frameExecutionResults(results) {
  if (!Array.isArray(results)) {
    return [];
  }
  return results.map((entry, index) => ({
    index,
    frame_id: Number.isSafeInteger(entry?.frameId) ? entry.frameId : null,
    document_id: entry?.documentId == null ? null : String(entry.documentId),
    result: entry?.result && typeof entry.result === "object" ? entry.result : null
  }));
}

function summarizeFrameExecutionResult(frame) {
  const result = frame?.result && typeof frame.result === "object" ? frame.result : {};
  const text = typeof result.text === "string" ? result.text : null;
  return {
    index: Number.isSafeInteger(frame?.index) ? frame.index : null,
    frame_id: Number.isSafeInteger(frame?.frame_id) ? frame.frame_id : null,
    document_id: frame?.document_id || null,
    ok: result.ok == null ? null : Boolean(result.ok),
    error_code: result.error_code == null ? null : String(result.error_code),
    error_detail: result.error_detail == null ? null : trimForReadback(result.error_detail, 240),
    matched_count: Number.isSafeInteger(result.matched_count) ? result.matched_count : null,
    resolved_by: result.resolved_by == null ? null : String(result.resolved_by),
    url: result.url == null ? (result.in_page_before_url == null ? null : String(result.in_page_before_url)) : String(result.url),
    title: result.title == null ? (result.in_page_title == null ? null : String(result.in_page_title)) : String(result.title),
    ready_state: result.ready_state == null ? (result.in_page_ready_state == null ? null : String(result.in_page_ready_state)) : String(result.ready_state),
    has_active_element: result.has_active_element == null ? null : Boolean(result.has_active_element),
    is_editable: result.is_editable == null ? null : Boolean(result.is_editable),
    tag_name: result.tag_name == null ? null : String(result.tag_name),
    text_len: Number.isSafeInteger(result.text_len) ? result.text_len : null,
    text_truncated: result.text_truncated == null ? null : Boolean(result.text_truncated),
    text_sample: text == null ? null : trimForReadback(text, 240)
  };
}

async function tabFrameMetadata(tabId) {
  const byFrameId = new Map();
  const errors = [];
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    errors.push("chrome.scripting.executeScript unavailable for frame metadata enrichment");
    return { byFrameId, errors };
  }
  try {
    const injected = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      func: readFrameMetadataInPage
    });
    for (const frame of frameExecutionResults(injected)) {
      if (!Number.isSafeInteger(frame.frame_id) || !frame.result || typeof frame.result !== "object") {
        continue;
      }
      byFrameId.set(String(frame.frame_id), frame.result);
      if (!frame.result.ok) {
        errors.push(
          `frame_id=${String(frame.frame_id)} metadata_error=${trimForReadback(frame.result.error_detail, 240)}`
        );
      }
    }
  } catch (error) {
    errors.push(`chrome.scripting.executeScript(frame metadata): ${trimForReadback(errorMessage(error), 240)}`);
  }
  return { byFrameId, errors };
}

function readFrameMetadataInPage() {
  try {
    return {
      ok: true,
      url: String(location.href || ""),
      name: String(window.name || ""),
      origin: String(location.origin || ""),
      security_origin: String(location.origin || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || "")
    };
  } catch (error) {
    return {
      ok: false,
      error_code: "CHROME_FRAME_METADATA_FAILED",
      error_detail: error && error.message ? String(error.message) : String(error)
    };
  }
}

function buildFrameTreeEntries(navigationFrames, metadataByFrameId, targetId) {
  const errors = [];
  const rows = [];
  const seen = new Set();
  for (const [navigationIndex, raw] of navigationFrames.entries()) {
    if (!Number.isSafeInteger(raw?.frameId)) {
      errors.push(`skipped frame at index=${navigationIndex}: missing numeric frameId`);
      continue;
    }
    const frameId = String(raw.frameId);
    if (seen.has(frameId)) {
      errors.push(`skipped duplicate frame_id=${frameId}`);
      continue;
    }
    seen.add(frameId);
    const parentFrameId = Number.isSafeInteger(raw.parentFrameId) && raw.parentFrameId >= 0
      ? String(raw.parentFrameId)
      : null;
    const metadata = metadataByFrameId.get(frameId) || {};
    const url = stringOrEmptyPreserve(metadata.url || raw.url || "");
    const origin = normalizeFrameOrigin(metadata.origin, url);
    rows.push({
      frame_id: frameId,
      parent_frame_id: parentFrameId,
      cdp_target_id: targetId,
      target_type: parentFrameId ? "iframe" : "page",
      target_attached: false,
      url,
      name: nullableNonemptyString(metadata.name),
      origin,
      security_origin: origin ? origin : null,
      loader_id: null,
      depth: 0,
      sibling_index: 0,
      child_count: 0,
      is_out_of_process: false,
      frame_element_id: null,
      frame_element_backend_node_id: null,
      frame_element_cdp_target_id: null,
      frame_element_source: "chrome.webNavigation.getAllFrames",
      navigation_index: navigationIndex,
      process_id: Number.isSafeInteger(raw.processId) ? raw.processId : null,
      error_occurred: Boolean(raw.errorOccurred)
    });
  }

  const byId = new Map(rows.map((row) => [row.frame_id, row]));
  for (const row of rows) {
    if (row.parent_frame_id && !byId.has(row.parent_frame_id)) {
      errors.push(`frame_id=${row.frame_id} references missing parent_frame_id=${row.parent_frame_id}`);
      row.parent_frame_id = null;
      row.target_type = "page";
    }
    if (row.error_occurred) {
      errors.push(`frame_id=${row.frame_id} url=${JSON.stringify(row.url)} reported navigation error`);
    }
  }

  const childrenByParent = new Map();
  for (const row of rows) {
    const key = row.parent_frame_id || "";
    if (!childrenByParent.has(key)) {
      childrenByParent.set(key, []);
    }
    childrenByParent.get(key).push(row);
  }
  for (const children of childrenByParent.values()) {
    children.sort((left, right) => left.navigation_index - right.navigation_index);
    children.forEach((child, index) => {
      child.sibling_index = index;
    });
  }

  const roots = (childrenByParent.get("") || []).slice();
  const rootProcessId = roots.find((row) => Number.isSafeInteger(row.process_id))?.process_id ?? null;
  for (const row of rows) {
    row.child_count = (childrenByParent.get(row.frame_id) || []).length;
    row.depth = frameTreeDepth(row, byId);
    row.is_out_of_process =
      row.parent_frame_id !== null &&
      Number.isSafeInteger(rootProcessId) &&
      Number.isSafeInteger(row.process_id) &&
      row.process_id !== rootProcessId;
  }

  const ordered = [];
  const visited = new Set();
  function visit(row) {
    if (!row || visited.has(row.frame_id)) {
      return;
    }
    visited.add(row.frame_id);
    ordered.push(row);
    for (const child of childrenByParent.get(row.frame_id) || []) {
      visit(child);
    }
  }
  for (const root of roots.sort((left, right) => left.navigation_index - right.navigation_index)) {
    visit(root);
  }
  for (const row of rows.sort((left, right) => left.navigation_index - right.navigation_index)) {
    visit(row);
  }

  return {
    frames: ordered.map(({ navigation_index, process_id, error_occurred, ...frame }) => frame),
    errors
  };
}

function frameTreeDepth(row, byId) {
  let depth = 0;
  let current = row;
  const seen = new Set();
  while (current?.parent_frame_id && byId.has(current.parent_frame_id)) {
    if (seen.has(current.frame_id)) {
      return depth;
    }
    seen.add(current.frame_id);
    depth += 1;
    current = byId.get(current.parent_frame_id);
  }
  return depth;
}

function normalizeFrameOrigin(rawOrigin, url) {
  const origin = stringOrEmptyPreserve(rawOrigin);
  if (origin && origin !== "null") {
    return origin;
  }
  try {
    const parsed = new URL(String(url || ""));
    return parsed.origin === "null" ? "" : parsed.origin;
  } catch (_) {
    return "";
  }
}

function nullableNonemptyString(value) {
  const text = stringOrEmptyPreserve(value);
  return text ? text : null;
}

function stringOrEmptyPreserve(value) {
  return value === null || value === undefined ? "" : String(value);
}

async function bridgeFrameEntriesForTab(tabId, targetId, commandName) {
  if (!chrome.webNavigation || typeof chrome.webNavigation.getAllFrames !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `${commandName} frame locator requires chrome.webNavigation.getAllFrames`
    );
  }
  let navigationFrames;
  try {
    navigationFrames = await chrome.webNavigation.getAllFrames({ tabId });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `${commandName} chrome.webNavigation.getAllFrames(${tabId}) failed: ${errorMessage(error)}`
    );
  }
  if (!Array.isArray(navigationFrames)) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `${commandName} chrome.webNavigation.getAllFrames(${tabId}) returned non-array frame data`
    );
  }
  const frameMetadata = await tabFrameMetadata(tabId);
  return buildFrameTreeEntries(navigationFrames, frameMetadata.byFrameId, targetId);
}

function bridgeFrameLocator(locator) {
  const frame = locator && typeof locator.frame === "object" && !Array.isArray(locator.frame)
    ? locator.frame
    : null;
  if (!frame) {
    return null;
  }
  const selectors = [
    normalizedFrameLocatorValue(frame.frame_id),
    normalizedFrameLocatorValue(frame.frame_element_id),
    normalizedFrameLocatorValue(frame.name),
    normalizedFrameLocatorValue(frame.url),
    Number.isSafeInteger(frame.index) ? frame.index : null
  ].filter((value) => value !== null);
  return selectors.length > 0 ? frame : null;
}

function matchingBridgeFrames(locator, frames) {
  if (Number.isSafeInteger(locator.index)) {
    const frame = frames[locator.index];
    return frame ? [frame] : [];
  }
  const frameId = normalizedFrameLocatorValue(locator.frame_id);
  if (frameId !== null) {
    return frames.filter((frame) => frame.frame_id === frameId);
  }
  const frameElementId = normalizedFrameLocatorValue(locator.frame_element_id);
  if (frameElementId !== null) {
    return frames.filter((frame) => frame.frame_element_id === frameElementId);
  }
  const name = normalizedFrameLocatorValue(locator.name);
  if (name !== null) {
    return frames.filter((frame) => frame.name === name);
  }
  const url = normalizedFrameLocatorValue(locator.url);
  if (url !== null) {
    return frames.filter((frame) => frame.url === url);
  }
  return [];
}

function normalizedFrameLocatorValue(value) {
  if (value === null || value === undefined) {
    return null;
  }
  const text = String(value).trim();
  return text ? text : null;
}

function bridgeLocatedFrame(frame, matchedFrameCount) {
  return {
    resolved: true,
    matched_frame_count: matchedFrameCount,
    frame_id: frame.frame_id || null,
    parent_frame_id: frame.parent_frame_id || null,
    cdp_target_id: frame.cdp_target_id || null,
    url: frame.url || null,
    name: frame.name || null,
    origin: frame.origin || null,
    is_out_of_process: Boolean(frame.is_out_of_process),
    frame_element_id: frame.frame_element_id || null,
    frame_element_cdp_target_id: frame.frame_element_cdp_target_id || null,
    frame_element_source: frame.frame_element_source || "chrome.webNavigation.getAllFrames"
  };
}

function unresolvedBridgeLocatedFrame(locator) {
  return {
    resolved: false,
    matched_frame_count: 0,
    frame_id: null,
    parent_frame_id: null,
    cdp_target_id: null,
    url: null,
    name: null,
    origin: null,
    is_out_of_process: false,
    frame_element_id: normalizedFrameLocatorValue(locator?.frame_element_id),
    frame_element_cdp_target_id: null,
    frame_element_source: "not_found"
  };
}

async function resolveBridgeLocatorFrameScope(selected, locator, root, commandName) {
  const frameLocator = bridgeFrameLocator(locator);
  if (!frameLocator) {
    return {
      resolved: true,
      target: root.frameId === null
        ? { tabId: selected.tabId, allFrames: true }
        : { tabId: selected.tabId, frameIds: [root.frameId] },
      frame: null,
      frame_result_count: 0,
      frame_results: []
    };
  }
  const targetId = selected.target?.id || `${TAB_TARGET_PREFIX}${selected.tabId}`;
  const built = await bridgeFrameEntriesForTab(selected.tabId, targetId, commandName);
  const matches = matchingBridgeFrames(frameLocator, built.frames);
  if (matches.length === 0) {
    return {
      resolved: false,
      target: null,
      frame: unresolvedBridgeLocatedFrame(frameLocator),
      frame_result_count: built.frames.length,
      frame_results: [],
      frame_snapshot_errors: built.errors
    };
  }
  if (matches.length > 1) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `${commandName} frame locator matched ${matches.length} frames; refine by frame_id, frame_element_id, or index`
    );
  }
  const selectedFrame = matches[0];
  const frameId = Number(selectedFrame.frame_id);
  if (!Number.isSafeInteger(frameId) || frameId < 0) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `${commandName} selected frame_id=${JSON.stringify(selectedFrame.frame_id)} is not targetable by chrome.scripting.executeScript`
    );
  }
  if (root.frameId !== null && root.frameId !== frameId) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `${commandName} root_element_id frame ${root.frameId} does not match frame locator frame ${frameId}`
    );
  }
  return {
    resolved: true,
    target: { tabId: selected.tabId, frameIds: [frameId] },
    frame: bridgeLocatedFrame(selectedFrame, matches.length),
    frame_result_count: built.frames.length,
    frame_results: [],
    frame_snapshot_errors: built.errors
  };
}

function frameResolvedMatchCount(frame) {
  const result = frame?.result && typeof frame.result === "object" ? frame.result : {};
  for (const key of ["matched_count", "match_count", "editable_match_count"]) {
    const value = result[key];
    if (Number.isSafeInteger(value) && value > 0) {
      return value;
    }
  }
  return result.ok ? 1 : 0;
}

function aggregatePageTextFrames(frames, maxChars) {
  const limit = Number.isSafeInteger(maxChars) && maxChars >= 0 ? Math.min(maxChars, 65536) : 4096;
  const availableFrames = frames.filter((frame) => frame.result && typeof frame.result === "object");
  if (availableFrames.length === 0) {
    return null;
  }
  const frameTexts = availableFrames.map((frame) => String(frame.result.text || ""));
  const combined = frameTexts.filter((text) => text.length > 0).join("\n\n");
  const combinedChars = Array.from(combined);
  return {
    text: combinedChars.slice(0, limit).join(""),
    text_len: combinedChars.length,
    text_truncated: combinedChars.length > limit || availableFrames.some((frame) => Boolean(frame.result.text_truncated)),
    max_chars: limit,
    readback_scope: "all_frames",
    frame_count: frames.length,
    frame_text_available_count: availableFrames.length,
    frame_text_nonempty_count: frameTexts.filter((text) => text.length > 0).length,
    frames: availableFrames.map(summarizeFrameExecutionResult)
  };
}

function waitForTextResult(
  selected,
  pageState,
  pageText,
  state,
  expectedText,
  conditionMet,
  timedOut,
  elapsedMs,
  timeoutMs,
  pollingIntervalMs,
  pollCount,
  observedTextLen
) {
  return {
    extension_id: chrome.runtime.id,
    target_id: pageState.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: pageState.chrome_window_id,
    url: pageState.url || pageText?.url || "",
    title: pageState.title || pageText?.title || "",
    ready_state: pageText?.ready_state || pageState.ready_state || "",
    state,
    text: expectedText || null,
    condition_met: Boolean(conditionMet),
    timed_out: Boolean(timedOut),
    elapsed_ms: elapsedMs,
    timeout_ms: timeoutMs,
    polling_interval_ms: pollingIntervalMs,
    poll_count: pollCount,
    observed_text_len: observedTextLen,
    readback_backend: "chrome.scripting.executeScript(page text polling)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function waitForFunctionProbe(selected, expression, args) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId },
      world: "MAIN",
      func: runWaitForFunctionProbeInPage,
      args: [{ expression, args }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript waitForFunction(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(injected);
  const first = frames.find((frame) => frame.result && typeof frame.result === "object");
  if (!first) {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript waitForFunction returned no structured result");
  }
  if (first.result.ok === false) {
    throw bridgeError(
      String(first.result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `waitForFunction failed: ${String(first.result.error_detail || "")}`
    );
  }
  return first.result;
}

function waitForFunctionResult(
  selected,
  pageState,
  poll,
  conditionMet,
  timedOut,
  elapsedMs,
  timeoutMs,
  pollingIntervalMs,
  pollCount,
  expression,
  args
) {
  const current = poll && typeof poll === "object" ? poll : {};
  return {
    extension_id: chrome.runtime.id,
    target_id: pageState.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: pageState.chrome_window_id,
    url: current.url || pageState.url || "",
    title: current.title || pageState.title || "",
    ready_state: current.ready_state || pageState.ready_state || "",
    condition_met: Boolean(conditionMet),
    timed_out: Boolean(timedOut),
    elapsed_ms: elapsedMs,
    timeout_ms: timeoutMs,
    polling_interval_ms: pollingIntervalMs,
    poll_count: pollCount,
    expression_len: expression.length,
    arg_count: args.length,
    value: Object.prototype.hasOwnProperty.call(current, "value") ? current.value : null,
    value_type: String(current.value_type || "undefined"),
    value_description: current.value_description ?? null,
    unserializable_value: current.unserializable_value ?? null,
    readback_backend: "chrome.scripting.executeScript(MAIN waitForFunction predicate polling)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

async function loadStateProbe(selected) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId },
      world: "MAIN",
      func: runLoadStateProbeInPage,
      args: [{ quietMs: 500 }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript waitForLoadState(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(injected);
  const first = frames.find((frame) => frame.result && typeof frame.result === "object");
  if (!first) {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript waitForLoadState returned no structured result");
  }
  if (first.result.ok === false) {
    throw bridgeError(
      String(first.result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `waitForLoadState failed: ${String(first.result.error_detail || "")}`
    );
  }
  return first.result;
}

function loadStateEventSummary(buffer, startedAt) {
  const entries = Array.isArray(buffer?.entries)
    ? buffer.entries.filter((entry) => Number(entry.observed_at_unix_ms || 0) >= startedAt)
    : [];
  const mainFrameEntries = entries.filter((entry) => entry.frame_id === "0" || entry.frame_id === null || entry.frame_id === undefined);
  const hasKind = (kind) => mainFrameEntries.some((entry) => String(entry.event_kind || "").toLowerCase() === kind);
  return {
    event_count: entries.length,
    domcontentloaded_seen: hasKind("domcontentloaded"),
    load_seen: hasKind("load"),
    lifecycle_network_idle_seen: hasKind("networkidle")
  };
}

function loadStatePollSummary(pageState, probe, events) {
  const current = probe && typeof probe === "object" ? probe : {};
  const eventSummary = events && typeof events === "object" ? events : {};
  return {
    target_id: pageState.target_id || "",
    chrome_window_id: pageState.chrome_window_id,
    url: current.url || pageState.url || "",
    title: current.title || pageState.title || "",
    ready_state: current.ready_state || pageState.ready_state || "",
    event_count: Number.isSafeInteger(eventSummary.event_count) ? eventSummary.event_count : 0,
    network_event_count: Number.isSafeInteger(current.network_event_count) ? current.network_event_count : 0,
    max_in_flight_requests: Number.isSafeInteger(current.max_in_flight_requests) ? current.max_in_flight_requests : 0,
    in_flight_requests: Number.isSafeInteger(current.in_flight_requests) ? current.in_flight_requests : 0,
    network_idle_quiet_ms: Number.isSafeInteger(current.network_idle_quiet_ms) ? current.network_idle_quiet_ms : 0,
    domcontentloaded_seen: Boolean(eventSummary.domcontentloaded_seen || current.domcontentloaded_seen),
    load_seen: Boolean(eventSummary.load_seen || current.load_seen),
    lifecycle_network_idle_seen: Boolean(eventSummary.lifecycle_network_idle_seen || current.lifecycle_network_idle_seen)
  };
}

function loadStateConditionMet(state, poll) {
  const readyState = String(poll?.ready_state || "").toLowerCase();
  if (state === "domcontentloaded") {
    return poll?.domcontentloaded_seen === true || readyState === "interactive" || readyState === "complete";
  }
  if (state === "load") {
    return poll?.load_seen === true || readyState === "complete";
  }
  return readyState === "complete" &&
    Number(poll?.in_flight_requests || 0) === 0 &&
    Number(poll?.network_idle_quiet_ms || 0) >= 500;
}

function waitForLoadStateResult(
  selected,
  state,
  poll,
  conditionMet,
  timedOut,
  elapsedMs,
  timeoutMs,
  pollingIntervalMs,
  pollCount
) {
  const current = poll && typeof poll === "object" ? poll : {};
  return {
    extension_id: chrome.runtime.id,
    target_id: current.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: current.chrome_window_id,
    url: current.url || "",
    title: current.title || "",
    ready_state: current.ready_state || "",
    state,
    condition_met: Boolean(conditionMet),
    timed_out: Boolean(timedOut),
    elapsed_ms: elapsedMs,
    timeout_ms: timeoutMs,
    polling_interval_ms: pollingIntervalMs,
    poll_count: pollCount,
    event_count: current.event_count || 0,
    network_event_count: current.network_event_count || 0,
    max_in_flight_requests: current.max_in_flight_requests || 0,
    in_flight_requests: current.in_flight_requests || 0,
    network_idle_quiet_ms: current.network_idle_quiet_ms || 0,
    lifecycle_network_idle_seen: Boolean(current.lifecycle_network_idle_seen),
    readback_backend: "chrome.webNavigation + chrome.scripting.executeScript(MAIN load-state/fetch/XHR/resource-timing polling)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function urlNavigationEventCount(buffer, startedAt) {
  if (!Array.isArray(buffer?.entries)) {
    return 0;
  }
  return buffer.entries.filter((entry) => {
    if (Number(entry.observed_at_unix_ms || 0) < startedAt) {
      return false;
    }
    const kind = String(entry.event_kind || "").toLowerCase();
    return kind === "framenavigated" || kind === "domcontentloaded" || kind === "load";
  }).length;
}

function waitForUrlMatches(candidateUrl, pattern, matchKind) {
  const candidate = String(candidateUrl || "");
  if (matchKind === "exact") {
    return candidate === pattern;
  }
  const regexSource = matchKind === "glob" ? globToRegexSource(pattern) : pattern;
  try {
    return new RegExp(regexSource).test(candidate);
  } catch (error) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `waitForUrl ${matchKind} pattern is invalid: ${errorMessage(error)}`
    );
  }
}

function globToRegexSource(glob) {
  let source = "^";
  for (const ch of String(glob || "")) {
    if (ch === "*") {
      source += ".*";
    } else if (ch === "?") {
      source += ".";
    } else {
      source += regexEscape(ch);
    }
  }
  return source + "$";
}

function regexEscape(value) {
  return String(value).replace(/[\\^$.*+?()[\]{}|]/g, "\\$&");
}

function waitForUrlResult(
  selected,
  pageState,
  urlPattern,
  matchKind,
  conditionMet,
  timedOut,
  elapsedMs,
  timeoutMs,
  pollingIntervalMs,
  pollCount,
  navigationEventCount
) {
  return {
    extension_id: chrome.runtime.id,
    target_id: pageState.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: pageState.chrome_window_id,
    url_pattern: urlPattern,
    match_kind: matchKind,
    condition_met: Boolean(conditionMet),
    timed_out: Boolean(timedOut),
    elapsed_ms: elapsedMs,
    timeout_ms: timeoutMs,
    polling_interval_ms: pollingIntervalMs,
    poll_count: pollCount,
    navigation_event_count: navigationEventCount,
    url: pageState.url || "",
    title: pageState.title || "",
    ready_state: pageState.ready_state || "",
    readback_backend: "chrome.tabs.get + chrome.webNavigation URL polling",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function waitForNetworkResult(
  selected,
  wait,
  requireResponse,
  conditionMet,
  timedOut,
  elapsedMs,
  pollCount,
  matchedEntry,
  buffer
) {
  return {
    extension_id: chrome.runtime.id,
    target_id: selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: selected.target.chrome_window_id,
    wait_kind: requireResponse ? "response" : "request",
    url_pattern: wait.url,
    match_kind: wait.matchKind,
    method: wait.method,
    status: wait.status,
    resource_type: wait.resourceType,
    condition_met: Boolean(conditionMet),
    timed_out: Boolean(timedOut),
    elapsed_ms: elapsedMs,
    timeout_ms: wait.timeoutMs,
    polling_interval_ms: wait.pollingIntervalMs,
    poll_count: pollCount,
    event_count: buffer.nextSeq,
    total_buffered: buffer.entries.length,
    dropped: buffer.dropped,
    matched_entry: matchedEntry ? networkEntryToWire(matchedEntry) : null,
    readback_backend: "chrome.webRequest + in-page fetch/XHR event buffer",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function findNetworkWaitEntry(buffer, wait, requireResponse, startSeq) {
  if (!buffer || !Array.isArray(buffer.entries)) {
    return null;
  }
  return buffer.entries.find((entry) => {
    if (Number(entry.seq || 0) <= startSeq) {
      return false;
    }
    if (requireResponse && !(entry.response_received && entry.status !== null && entry.status !== undefined)) {
      return false;
    }
    if (wait.method && !stringEqualsIgnoreCase(entry.method, wait.method)) {
      return false;
    }
    if (wait.status !== null && wait.status !== undefined && Number(entry.status) !== wait.status) {
      return false;
    }
    if (wait.resourceType && !networkResourceTypeMatches(entry, wait.resourceType)) {
      return false;
    }
    if (wait.url) {
      const candidate = requireResponse ? (entry.response_url || entry.url || "") : (entry.url || "");
      if (!waitForUrlMatches(candidate, wait.url, wait.matchKind)) {
        return false;
      }
    }
    return true;
  }) || null;
}

function networkResourceTypeMatches(entry, expected) {
  const actual = String(entry.resource_type || "");
  if (actual.toLowerCase() === String(expected || "").toLowerCase()) {
    return true;
  }
  const raw = String(entry.web_request_type || "");
  return raw.toLowerCase() === String(expected || "").toLowerCase();
}

function networkEntryToWire(entry) {
  return {
    seq: Number(entry.seq || 0),
    request_id: String(entry.request_id || ""),
    url: entry.url || null,
    method: entry.method || null,
    resource_type: entry.resource_type || null,
    request_headers: entry.request_headers || null,
    response_received: Boolean(entry.response_received),
    response_url: entry.response_url || null,
    status: entry.status === null || entry.status === undefined ? null : Number(entry.status),
    status_text: entry.status_text || null,
    response_headers: entry.response_headers || null,
    response_timing: entry.response_timing || null,
    protocol: entry.protocol || null,
    remote_ip_address: entry.remote_ip_address || null,
    remote_port: entry.remote_port === null || entry.remote_port === undefined ? null : Number(entry.remote_port),
    encoded_data_length: entry.encoded_data_length === null || entry.encoded_data_length === undefined ? null : Number(entry.encoded_data_length),
    loading_finished: Boolean(entry.loading_finished),
    loading_failed: Boolean(entry.loading_failed),
    failure_error_text: entry.failure_error_text || null
  };
}

async function waitForSelectorPoll(selected, locator, limit, root, state) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  const frameScope = await resolveBridgeLocatorFrameScope(selected, locator, root, "waitForSelector");
  if (!frameScope.resolved) {
    const normalized = {
      match_count: 0,
      returned_count: 0,
      visible_count: 0,
      truncated: false,
      returned: [],
      visible: [],
      url: "",
      title: "",
      ready_state: "",
      frame: frameScope.frame,
      frame_result_count: frameScope.frame_result_count || 0,
      frame_results: frameScope.frame_results || []
    };
    const condition = waitForSelectorCondition(state, normalized);
    return {
      ...normalized,
      condition_met: condition.conditionMet,
      element_id: condition.elementId
    };
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: frameScope.target,
      func: runAssertPollInPage,
      args: [{ locator, limit, rootPath: root.path }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
      `chrome.scripting.executeScript waitForSelector(${selected.tabId}) failed: ${errorMessage(error)}`
    );
  }
  const frames = frameExecutionResults(injected);
  const resultFrames = frames.filter((frame) => frame.result && typeof frame.result === "object");
  if (resultFrames.length === 0) {
    throw bridgeError(ERROR_CHROME_SCRIPTING_EXECUTE_FAILED, "chrome.scripting.executeScript waitForSelector returned no structured result");
  }
  const failingFrame = resultFrames.find((frame) => frame.result && frame.result.ok === false);
  if (failingFrame) {
    throw bridgeError(
      String(failingFrame.result.error_code || ERROR_CHROME_SCRIPTING_EXECUTE_FAILED),
      `waitForSelector failed: ${String(failingFrame.result.error_detail || "")}`
    );
  }
  const normalized = selectorPollSummary(selected, resultFrames, limit, frameScope);
  const condition = waitForSelectorCondition(state, normalized);
  return {
    ...normalized,
    condition_met: condition.conditionMet,
    element_id: condition.elementId,
    frame_result_count: frames.length,
    frame_results: frames.map(summarizeFrameExecutionResult)
  };
}

function selectorPollSummary(selected, resultFrames, limit, frameScope = null) {
  let matchCount = 0;
  let truncated = false;
  let url = "";
  let title = "";
  let readyState = "";
  const returned = [];
  for (const frame of resultFrames) {
    const result = frame.result || {};
    const frameMatchCount = Number.isSafeInteger(result.match_count) && result.match_count > 0
      ? result.match_count
      : 0;
    matchCount += frameMatchCount;
    truncated = truncated || Boolean(result.truncated);
    url = url || String(result.url || "");
    title = title || String(result.title || "");
    readyState = readyState || String(result.ready_state || "");
    const localIds = Array.isArray(result.local_element_ids)
      ? result.local_element_ids
      : (result.local_element_id ? [result.local_element_id] : []);
    const states = Array.isArray(result.element_states)
      ? result.element_states
      : (result.element_state ? [result.element_state] : []);
    for (let index = 0; index < localIds.length && returned.length < limit; index += 1) {
      if (!Number.isSafeInteger(frame.frame_id)) {
        continue;
      }
      returned.push({
        elementId: chromeBridgeElementId(selected.tabId, frame.frame_id, String(localIds[index])),
        state: states[index] && typeof states[index] === "object" ? states[index] : null
      });
    }
  }
  const visible = returned.filter((item) => item.state?.is_visible === true);
  return {
    match_count: matchCount,
    returned_count: returned.length,
    visible_count: visible.length,
    truncated,
    returned,
    visible,
    url,
    title,
    ready_state: readyState,
    frame: frameScope?.frame || null
  };
}

function waitForSelectorCondition(state, poll) {
  if (state === "attached") {
    return {
      conditionMet: poll.match_count > 0,
      elementId: poll.returned[0]?.elementId || null
    };
  }
  if (state === "visible") {
    return {
      conditionMet: poll.visible.length > 0,
      elementId: poll.visible[0]?.elementId || null
    };
  }
  if (state === "hidden") {
    const absent = poll.match_count === 0;
    const hidden = !absent && poll.visible_count === 0 && !poll.truncated;
    return {
      conditionMet: absent || hidden,
      elementId: hidden ? (poll.returned[0]?.elementId || null) : null
    };
  }
  return {
    conditionMet: poll.match_count === 0,
    elementId: null
  };
}

function waitForSelectorResult(
  selected,
  pageState,
  locator,
  state,
  conditionMet,
  timedOut,
  elapsedMs,
  timeoutMs,
  pollingIntervalMs,
  pollCount,
  poll
) {
  const current = poll || {
    match_count: 0,
    returned_count: 0,
    visible_count: 0,
    truncated: false,
    element_id: null,
    url: "",
    title: "",
    ready_state: "",
    frame: null,
    frame_result_count: 0,
    frame_results: []
  };
  return {
    extension_id: chrome.runtime.id,
    target_id: pageState.target_id || selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: pageState.chrome_window_id,
    url: current.url || pageState.url || "",
    title: current.title || pageState.title || "",
    ready_state: current.ready_state || pageState.ready_state || "",
    engine: String(locator.engine || "css").toLowerCase(),
    query: String(locator.query || ""),
    state,
    condition_met: Boolean(conditionMet),
    timed_out: Boolean(timedOut),
    elapsed_ms: elapsedMs,
    timeout_ms: timeoutMs,
    polling_interval_ms: pollingIntervalMs,
    poll_count: pollCount,
    match_count: current.match_count || 0,
    returned_count: current.returned_count || 0,
    visible_count: current.visible_count || 0,
    truncated: Boolean(current.truncated),
    element_id: current.element_id || null,
    frame: current.frame || null,
    readback_backend: "chrome.scripting.executeScript(locator polling)",
    backend_tier_used: "chrome_tabs_extension",
    required_foreground: false,
    frame_result_count: current.frame_result_count || 0,
    frame_results: current.frame_results || [],
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason
  };
}

function optionalInteger(value) {
  if (value === undefined || value === null || value === "") {
    return null;
  }
  const numberValue = Number(value);
  return Number.isSafeInteger(numberValue) ? numberValue : null;
}

async function withTimeout(promise, timeoutMs, description) {
  let timeoutId;
  const timeout = new Promise((_, reject) => {
    timeoutId = setTimeout(() => {
      reject(new Error(`${description} timed out after ${timeoutMs}ms`));
    }, timeoutMs);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    clearTimeout(timeoutId);
  }
}

function delay(timeoutMs) {
  return new Promise((resolve) => setTimeout(resolve, timeoutMs));
}

function elapsedSince(startedAt) {
  return Math.max(0, Date.now() - startedAt);
}

function runLoadStateProbeInPage(request) {
  const quietMs = Number.isSafeInteger(request?.quietMs) && request.quietMs >= 0 ? request.quietMs : 500;
  const stateKey = "__synapseLoadStateProbe";
  const nowMs = () => {
    if (typeof performance !== "undefined" && typeof performance.now === "function") {
      return performance.now();
    }
    return Date.now();
  };
  const safeNumber = (value) => (typeof value === "number" && Number.isFinite(value) ? value : 0);
  const performanceEntries = (kind) => {
    try {
      if (typeof performance !== "undefined" && typeof performance.getEntriesByType === "function") {
        return performance.getEntriesByType(kind) || [];
      }
    } catch (_error) {}
    return [];
  };
  const navigationTiming = () => {
    const entries = performanceEntries("navigation");
    return entries.length > 0 && entries[0] ? entries[0] : null;
  };
  const latestResourceActivityMs = () => {
    let latest = 0;
    for (const entry of performanceEntries("resource")) {
      latest = Math.max(
        latest,
        safeNumber(entry.responseEnd),
        safeNumber(entry.duration) > 0 ? safeNumber(entry.startTime) + safeNumber(entry.duration) : 0,
        safeNumber(entry.fetchStart),
        safeNumber(entry.startTime)
      );
    }
    return latest;
  };
  const ensureState = () => {
    const existing = globalThis[stateKey];
    if (existing && existing.version === 1) {
      return existing;
    }
    const installedAtMs = nowMs();
    const state = {
      version: 1,
      installedAtMs,
      lastActivityAtMs: installedAtMs,
      domcontentloadedAtMs: 0,
      loadAtMs: 0,
      inFlightRequests: 0,
      maxInFlightRequests: 0,
      networkEventCount: 0,
      totalStarted: 0,
      totalFinished: 0,
      fetchWrapped: false,
      xhrWrapped: false,
      lifecycleListenersInstalled: false,
      lifecycleNetworkIdleSeen: false
    };
    globalThis[stateKey] = state;
    return state;
  };
  const state = ensureState();
  const markActivity = () => {
    state.lastActivityAtMs = Math.max(state.lastActivityAtMs || 0, nowMs());
  };
  const markRequestStarted = () => {
    state.totalStarted += 1;
    state.networkEventCount += 1;
    state.inFlightRequests += 1;
    state.maxInFlightRequests = Math.max(state.maxInFlightRequests, state.inFlightRequests);
    markActivity();
  };
  const markRequestFinished = () => {
    state.totalFinished += 1;
    state.networkEventCount += 1;
    if (state.inFlightRequests > 0) {
      state.inFlightRequests -= 1;
    }
    markActivity();
  };
  const installFetchProbe = () => {
    if (state.fetchWrapped || typeof globalThis.fetch !== "function") {
      return;
    }
    const originalFetch = globalThis.fetch;
    const wrappedFetch = function synapseLoadStateFetchWrapper(...args) {
      markRequestStarted();
      let result;
      try {
        result = originalFetch.apply(this, args);
      } catch (error) {
        markRequestFinished();
        throw error;
      }
      return Promise.resolve(result).finally(() => {
        markRequestFinished();
      });
    };
    try {
      Object.defineProperty(wrappedFetch, "__synapseLoadStateWrapped", { value: true });
    } catch (_error) {}
    try {
      globalThis.fetch = wrappedFetch;
      state.fetchWrapped = true;
    } catch (_error) {}
  };
  const installXhrProbe = () => {
    if (state.xhrWrapped || typeof XMLHttpRequest === "undefined" || !XMLHttpRequest.prototype) {
      return;
    }
    const proto = XMLHttpRequest.prototype;
    if (proto.__synapseLoadStateWrapped) {
      state.xhrWrapped = true;
      return;
    }
    const originalSend = proto.send;
    if (typeof originalSend !== "function") {
      return;
    }
    proto.send = function synapseLoadStateXhrSendWrapper(...args) {
      markRequestStarted();
      let finished = false;
      const finishOnce = () => {
        if (!finished) {
          finished = true;
          markRequestFinished();
        }
      };
      try {
        this.addEventListener("loadend", finishOnce, { once: true });
      } catch (_error) {}
      try {
        return originalSend.apply(this, args);
      } catch (error) {
        finishOnce();
        throw error;
      }
    };
    try {
      Object.defineProperty(proto, "__synapseLoadStateWrapped", { value: true });
    } catch (_error) {}
    state.xhrWrapped = true;
  };
  try {
    installFetchProbe();
  } catch (_error) {}
  try {
    installXhrProbe();
  } catch (_error) {}
  if (!state.lifecycleListenersInstalled) {
    try {
      document.addEventListener("DOMContentLoaded", () => {
        state.domcontentloadedAtMs = state.domcontentloadedAtMs || nowMs();
      }, { once: true });
    } catch (_error) {}
    try {
      window.addEventListener("load", () => {
        state.loadAtMs = state.loadAtMs || nowMs();
        markActivity();
      }, { once: true });
    } catch (_error) {}
    state.lifecycleListenersInstalled = true;
  }

  const nav = navigationTiming();
  if (nav && safeNumber(nav.domContentLoadedEventEnd) > 0) {
    state.domcontentloadedAtMs = Math.max(state.domcontentloadedAtMs || 0, safeNumber(nav.domContentLoadedEventEnd));
  }
  if (nav && safeNumber(nav.loadEventEnd) > 0) {
    state.loadAtMs = Math.max(state.loadAtMs || 0, safeNumber(nav.loadEventEnd));
  }
  const readyState = String(document?.readyState || "");
  if ((readyState === "interactive" || readyState === "complete") && !state.domcontentloadedAtMs) {
    state.domcontentloadedAtMs = nowMs();
  }
  if (readyState === "complete" && !state.loadAtMs) {
    state.loadAtMs = nowMs();
  }
  const resourceEntries = performanceEntries("resource");
  const latestResourceMs = latestResourceActivityMs();
  const quietSinceMs = Math.max(
    state.installedAtMs || 0,
    state.lastActivityAtMs || 0,
    state.loadAtMs || 0,
    latestResourceMs
  );
  const quietElapsedMs = Math.max(0, Math.floor(nowMs() - quietSinceMs));
  const networkIdle = readyState === "complete" && state.inFlightRequests === 0 && quietElapsedMs >= quietMs;
  if (networkIdle) {
    state.lifecycleNetworkIdleSeen = true;
  }
  return {
    ok: true,
    version: state.version,
    url: String(location.href || ""),
    title: String(document?.title || ""),
    ready_state: readyState,
    domcontentloaded_seen: Boolean(state.domcontentloadedAtMs),
    load_seen: Boolean(state.loadAtMs),
    lifecycle_network_idle_seen: Boolean(state.lifecycleNetworkIdleSeen),
    in_flight_requests: state.inFlightRequests,
    max_in_flight_requests: state.maxInFlightRequests,
    network_event_count: state.networkEventCount + resourceEntries.length,
    resource_entry_count: resourceEntries.length,
    total_started: state.totalStarted,
    total_finished: state.totalFinished,
    network_idle_quiet_ms: quietElapsedMs,
    quiet_window_ms: quietMs,
    latest_resource_activity_ms: Math.floor(latestResourceMs)
  };
}

async function runWaitForFunctionProbeInPage(request) {
  const expression = String(request?.expression ?? "");
  const args = Array.isArray(request?.args) ? request.args : [];
  const serializeValue = (value) => {
    const valueType = typeof value;
    if (value === undefined) {
      return {
        value: null,
        value_type: "undefined",
        value_description: "undefined",
        unserializable_value: null
      };
    }
    if (valueType === "bigint") {
      return {
        value: String(value),
        value_type: valueType,
        value_description: `${String(value)}n`,
        unserializable_value: `${String(value)}n`
      };
    }
    if (valueType === "number" && !Number.isFinite(value)) {
      return {
        value: null,
        value_type: valueType,
        value_description: String(value),
        unserializable_value: String(value)
      };
    }
    if (valueType === "function" || valueType === "symbol") {
      return {
        value: null,
        value_type: valueType,
        value_description: String(value),
        unserializable_value: null
      };
    }
    try {
      return {
        value: JSON.parse(JSON.stringify(value)),
        value_type: valueType,
        value_description: null,
        unserializable_value: null
      };
    } catch (_error) {
      return {
        value: null,
        value_type: valueType,
        value_description: String(value),
        unserializable_value: null
      };
    }
  };
  try {
    const candidate = (0, eval)(`(${expression})`);
    const value = typeof candidate === "function" ? candidate(...args) : candidate;
    const resolved = await Promise.resolve(value);
    const serialized = serializeValue(resolved);
    return {
      ok: true,
      condition_met: Boolean(resolved),
      value: serialized.value,
      value_type: serialized.value_type,
      value_description: serialized.value_description,
      unserializable_value: serialized.unserializable_value,
      url: String(location.href || ""),
      title: document && typeof document.title === "string" ? document.title : "",
      ready_state: document && typeof document.readyState === "string" ? document.readyState : ""
    };
  } catch (error) {
    return {
      ok: false,
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: error && error.stack ? String(error.stack) : String(error && error.message ? error.message : error),
      url: String(location.href || ""),
      title: document && typeof document.title === "string" ? document.title : "",
      ready_state: document && typeof document.readyState === "string" ? document.readyState : ""
    };
  }
}

async function tabPageVitalsState(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      visibility_state: null,
      document_hidden: null,
      ready_state: null,
      lcp_supported: null,
      lcp_entry_count: 0,
      lcp: null,
      error_code: "CHROME_SCRIPTING_UNAVAILABLE",
      error_detail: "Chrome scripting API is unavailable; extension is missing scripting permission"
    };
  }
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      func: readPageVitalsInPage
    });
    const frames = frameExecutionResults(results);
    const selected =
      frames.find((frame) => frame.result?.available) ||
      frames.find((frame) => frame.result) ||
      null;
    if (!selected || typeof selected.result !== "object" || selected.result === null) {
      return {
        available: false,
        readback_source: "chrome.scripting.executeScript",
        visibility_state: null,
        document_hidden: null,
        ready_state: null,
        lcp_supported: null,
        lcp_entry_count: 0,
        lcp: null,
        error_code: "CHROME_SCRIPTING_EMPTY_RESULT",
        error_detail: "chrome.scripting.executeScript returned no page-vitals result"
      };
    }
    return {
      available: true,
      readback_source: "chrome.scripting.executeScript",
      frame_id: selected.frame_id,
      frame_document_id: selected.document_id,
      frame_result_count: frames.length,
      frame_results: frames.map(summarizeFrameExecutionResult),
      ...selected.result
    };
  } catch (error) {
    return {
      available: false,
      readback_source: "chrome.scripting.executeScript",
      visibility_state: null,
      document_hidden: null,
      ready_state: null,
      lcp_supported: null,
      lcp_entry_count: 0,
      lcp: null,
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: errorMessage(error)
    };
  }
}

function readPageTextInPage(maxChars) {
  const limit = Number.isSafeInteger(maxChars) && maxChars >= 0 ? Math.min(maxChars, 65536) : 4096;
  const source =
    document.body && typeof document.body.innerText === "string"
      ? document.body.innerText
      : document.documentElement && typeof document.documentElement.innerText === "string"
        ? document.documentElement.innerText
        : document.body && typeof document.body.textContent === "string"
          ? document.body.textContent
          : document.documentElement && typeof document.documentElement.textContent === "string"
            ? document.documentElement.textContent
            : "";
  let text = "";
  let textLen = 0;
  for (const ch of String(source || "")) {
    if (textLen < limit) {
      text += ch;
    }
    textLen += 1;
  }
  return {
    text,
    text_len: textLen,
    text_truncated: textLen > limit,
    max_chars: limit,
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || "")
  };
}

function readPageContentInPage(maxBytes) {
  const max = Number.isSafeInteger(maxBytes) && maxBytes >= 0 ? Math.min(maxBytes, 2 * 1024 * 1024) : 2 * 1024 * 1024;
  const html = String(document.documentElement?.outerHTML || "");
  return {
    ok: true,
    html: Array.from(html).slice(0, max).join(""),
    html_len: html.length,
    truncated: html.length > max,
    max_bytes: max,
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || ""),
    history_current_index: -1,
    history_entry_count: Number.isSafeInteger(history.length) ? history.length : 0
  };
}

function runStorageCommandInPage(request) {
  function storageItemsLocal(store) {
    const items = [];
    for (let index = 0; index < store.length; index += 1) {
      const name = store.key(index);
      if (name === null) {
        continue;
      }
      items.push({ name: String(name), value: String(store.getItem(name) ?? "") });
    }
    items.sort((left, right) => left.name.localeCompare(right.name));
    return items;
  }

  function storageValueToStringLocal(value) {
    if (value === undefined || value === null) {
      return "";
    }
    if (typeof value === "string") {
      return value;
    }
    return JSON.stringify(value);
  }

  function failLocal(code, detail) {
    return {
      ok: false,
      error_code: code,
      error_detail: detail,
      origin: String(location?.origin || ""),
      url: String(location?.href || ""),
      title: String(document?.title || ""),
      ready_state: String(document?.readyState || "")
    };
  }

  function errorMessageLocal(error) {
    return error && typeof error.message === "string" ? error.message : String(error);
  }

  try {
    const operation = String(request?.operation || "get");
    const storeName = String(request?.store || "local");
    const store = storeName === "session" ? window.sessionStorage : window.localStorage;
    const key = request?.key === null || request?.key === undefined ? null : String(request.key);
    const beforeItems = storageItemsLocal(store);
    let afterItems = beforeItems;
    let value = null;
    let changed = false;

    if (operation === "get") {
      afterItems = key === null
        ? beforeItems
        : (store.getItem(key) === null ? [] : [{ name: key, value: String(store.getItem(key)) }]);
    } else if (operation === "set") {
      if (key === null || key === "") {
        return failLocal("CHROME_STORAGE_KEY_INVALID", "storage set requires non-empty key");
      }
      value = storageValueToStringLocal(request?.value);
      store.setItem(key, value);
      changed = true;
      afterItems = storageItemsLocal(store);
    } else if (operation === "clear") {
      if (key === null || key === "") {
        store.clear();
      } else {
        store.removeItem(key);
      }
      changed = true;
      afterItems = storageItemsLocal(store);
    } else {
      return failLocal("CHROME_STORAGE_OPERATION_UNSUPPORTED", `unsupported operation ${operation}`);
    }

    return {
      ok: true,
      operation,
      store: storeName,
      origin: String(location.origin || ""),
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || ""),
      key,
      value,
      before_count: beforeItems.length,
      after_count: afterItems.length,
      changed,
      items: afterItems
    };
  } catch (error) {
    return failLocal("CHROME_STORAGE_ACTION_FAILED", errorMessageLocal(error));
  }
}

function readStorageStateInPage(includeSessionStorage) {
  function storageItemsLocal(store) {
    const items = [];
    for (let index = 0; index < store.length; index += 1) {
      const name = store.key(index);
      if (name === null) {
        continue;
      }
      items.push({ name: String(name), value: String(store.getItem(name) ?? "") });
    }
    items.sort((left, right) => left.name.localeCompare(right.name));
    return items;
  }

  function failLocal(code, detail) {
    return {
      ok: false,
      error_code: code,
      error_detail: detail,
      origin: String(location?.origin || ""),
      url: String(location?.href || ""),
      title: String(document?.title || ""),
      ready_state: String(document?.readyState || "")
    };
  }

  function errorMessageLocal(error) {
    return error && typeof error.message === "string" ? error.message : String(error);
  }

  try {
    return {
      ok: true,
      origin: String(location.origin || ""),
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || ""),
      localStorage: storageItemsLocal(window.localStorage),
      sessionStorage: includeSessionStorage ? storageItemsLocal(window.sessionStorage) : []
    };
  } catch (error) {
    return failLocal("CHROME_STORAGE_STATE_READ_FAILED", errorMessageLocal(error));
  }
}

function applyStorageStateInPage(state, includeSessionStorage, clearBeforeLoad) {
  function storageItemsLocal(store) {
    const items = [];
    for (let index = 0; index < store.length; index += 1) {
      const name = store.key(index);
      if (name === null) {
        continue;
      }
      items.push({ name: String(name), value: String(store.getItem(name) ?? "") });
    }
    items.sort((left, right) => left.name.localeCompare(right.name));
    return items;
  }

  function failLocal(code, detail) {
    return {
      ok: false,
      error_code: code,
      error_detail: detail,
      origin: String(location?.origin || ""),
      url: String(location?.href || ""),
      title: String(document?.title || ""),
      ready_state: String(document?.readyState || "")
    };
  }

  function errorMessageLocal(error) {
    return error && typeof error.message === "string" ? error.message : String(error);
  }

  try {
    const origin = String(location.origin || "");
    const origins = Array.isArray(state?.origins) ? state.origins : [];
    const entry = origins.find((item) => String(item?.origin || "") === origin) || null;
    if (!entry) {
      return {
        ok: true,
        origin,
        url: String(location.href || ""),
        title: String(document.title || ""),
        ready_state: String(document.readyState || ""),
        matched_origin: false,
        local_applied_count: 0,
        session_applied_count: 0,
        local_after_count: storageItemsLocal(window.localStorage).length,
        session_after_count: storageItemsLocal(window.sessionStorage).length
      };
    }
    if (clearBeforeLoad) {
      window.localStorage.clear();
      if (includeSessionStorage) {
        window.sessionStorage.clear();
      }
    }
    let localApplied = 0;
    for (const item of Array.isArray(entry.localStorage) ? entry.localStorage : []) {
      const name = String(item?.name || "");
      if (!name) {
        continue;
      }
      window.localStorage.setItem(name, String(item.value ?? ""));
      localApplied += 1;
    }
    let sessionApplied = 0;
    if (includeSessionStorage) {
      for (const item of Array.isArray(entry.sessionStorage) ? entry.sessionStorage : []) {
        const name = String(item?.name || "");
        if (!name) {
          continue;
        }
        window.sessionStorage.setItem(name, String(item.value ?? ""));
        sessionApplied += 1;
      }
    }
    return {
      ok: true,
      origin,
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || ""),
      matched_origin: true,
      clear_before_load: Boolean(clearBeforeLoad),
      local_applied_count: localApplied,
      session_applied_count: sessionApplied,
      local_after_count: storageItemsLocal(window.localStorage).length,
      session_after_count: storageItemsLocal(window.sessionStorage).length
    };
  } catch (error) {
    return failLocal("CHROME_STORAGE_STATE_LOAD_FAILED", errorMessageLocal(error));
  }
}

function runAssertPollInPage(request) {
  const ERROR_SELECTOR_INVALID = "CHROME_DOM_SELECTOR_INVALID";
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const loc = request && typeof request.locator === "object" && !Array.isArray(request.locator)
    ? request.locator
    : {};
  const limit = Number.isSafeInteger(request?.limit) ? Math.max(1, Math.min(request.limit, 500)) : 2;
  const root = elementByPath(request?.rootPath) || document.documentElement;
  if (!root) {
    return fail(ERROR_ELEMENT_NOT_FOUND, `root element path ${JSON.stringify(request?.rootPath)} was not found`);
  }
  const resolved = resolveLocator(root, loc);
  if (!resolved.ok) {
    return resolved;
  }
  const candidates = resolved.elements;
  const returned = candidates.slice(0, limit);
  const selected = returned[0] || null;
  const elementStates = returned.map(elementState);
  return {
    ok: true,
    match_count: candidates.length,
    returned_count: returned.length,
    local_element_id: selected ? domPath(selected) : null,
    local_element_ids: returned.map(domPath),
    element_states: elementStates,
    truncated: candidates.length > returned.length && !Number.isSafeInteger(loc.nth),
    element_state: selected ? elementState(selected) : null,
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || "")
  };

  function resolveLocator(rootElement, locator) {
    const engine = String(locator.engine || "css").toLowerCase();
    let candidates;
    try {
      if (engine === "css") {
        candidates = cssCandidates(rootElement, String(locator.query || ""));
      } else if (engine === "xpath") {
        candidates = xpathCandidates(rootElement, String(locator.query || ""));
      } else if (engine === "text") {
        candidates = allElements(rootElement).filter((element) =>
          matchText(elementVisibleText(element), String(locator.query || ""), locator.exact, locator.regex)
        );
      } else if (engine === "role") {
        candidates = roleCandidates(rootElement, locator);
      } else if (engine === "label") {
        candidates = allElements(rootElement).filter((element) =>
          isFormLike(element) && matchText(accessibleName(element), String(locator.query || ""), locator.exact, locator.regex)
        );
      } else if (engine === "placeholder") {
        candidates = allElements(rootElement).filter((element) =>
          matchText(element.getAttribute("placeholder") || "", String(locator.query || ""), locator.exact, locator.regex)
        );
      } else if (engine === "alttext") {
        candidates = allElements(rootElement).filter((element) =>
          matchText(element.getAttribute("alt") || "", String(locator.query || ""), locator.exact, locator.regex)
        );
      } else if (engine === "title") {
        candidates = allElements(rootElement).filter((element) =>
          matchText(element.getAttribute("title") || "", String(locator.query || ""), locator.exact, locator.regex)
        );
      } else if (engine === "testid") {
        const attr = String(locator.testid_attribute || "data-testid");
        candidates = allElements(rootElement).filter((element) =>
          matchText(element.getAttribute(attr) || "", String(locator.query || ""), locator.exact ?? true, locator.regex)
        );
      } else if (engine === "layout") {
        candidates = layoutCandidates(rootElement, locator);
      } else {
        return fail(ERROR_SELECTOR_INVALID, `unsupported locator engine ${JSON.stringify(engine)}`);
      }
    } catch (error) {
      return fail(ERROR_SELECTOR_INVALID, errorMessageLocal(error));
    }
    if (locator.has_text != null && String(locator.has_text).trim()) {
      const expected = normalizeText(String(locator.has_text));
      candidates = candidates.filter((element) => normalizeText(elementVisibleText(element)).includes(expected));
    }
    candidates = applyNth(candidates, locator.nth);
    return { ok: true, elements: candidates };
  }

  function cssCandidates(rootElement, query) {
    if (!query.trim()) {
      return [];
    }
    const found = [];
    if (rootElement instanceof Element && rootElement.matches?.(query)) {
      found.push(rootElement);
    }
    // Run the selector inside each tree scope (light DOM + every open shadow
    // root) and union the results. An invalid selector throws from the first
    // querySelectorAll and is reported as ERROR_SELECTOR_INVALID upstream.
    for (const scope of shadowScopes(rootElement)) {
      const matches = scope.querySelectorAll(query);
      for (const node of matches) {
        found.push(node);
      }
    }
    return uniqueElements(found);
  }

  function xpathCandidates(rootElement, query) {
    if (!query.trim()) {
      return [];
    }
    const snapshot = document.evaluate(query, rootElement, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
    const out = [];
    for (let index = 0; index < snapshot.snapshotLength; index += 1) {
      const node = snapshot.snapshotItem(index);
      if (node instanceof Element) {
        out.push(node);
      }
    }
    return out;
  }

  function roleCandidates(rootElement, locator) {
    const role = normalizeText(locator.query);
    const name = locator.name == null ? null : String(locator.name);
    return allElements(rootElement).filter((element) => {
      if (!locator.include_hidden && !isElementVisible(element)) {
        return false;
      }
      if (normalizeText(inferRole(element)) !== role) {
        return false;
      }
      if (name !== null && !matchText(accessibleName(element), name, locator.name_exact, locator.name_regex)) {
        return false;
      }
      if (locator.checked !== null && locator.checked !== undefined && checkedState(element) !== Boolean(locator.checked)) {
        return false;
      }
      if (locator.disabled !== null && locator.disabled !== undefined && isElementEnabled(element) === Boolean(locator.disabled)) {
        return false;
      }
      if (locator.selected !== null && locator.selected !== undefined && boolAttr(element, "aria-selected") !== Boolean(locator.selected)) {
        return false;
      }
      if (locator.pressed !== null && locator.pressed !== undefined && boolAttr(element, "aria-pressed") !== Boolean(locator.pressed)) {
        return false;
      }
      if (locator.expanded !== null && locator.expanded !== undefined && boolAttr(element, "aria-expanded") !== Boolean(locator.expanded)) {
        return false;
      }
      if (locator.level !== null && locator.level !== undefined) {
        const level = Number(element.getAttribute("aria-level") || headingLevel(element) || NaN);
        if (level !== Number(locator.level)) {
          return false;
        }
      }
      return true;
    });
  }

  function layoutCandidates(rootElement, locator) {
    const base = cssCandidates(rootElement, String(locator.query || ""));
    const anchors = cssCandidates(rootElement, String(locator.anchor || ""));
    if (anchors.length === 0) {
      return [];
    }
    const relation = String(locator.relation || "near").toLowerCase();
    const maxDistance = Number.isFinite(Number(locator.max_distance)) ? Number(locator.max_distance) : 50;
    const anchorRects = anchors.map((anchor) => anchor.getBoundingClientRect());
    return base
      .map((element) => ({ element, score: layoutScore(element.getBoundingClientRect(), anchorRects, relation) }))
      .filter((entry) => Number.isFinite(entry.score) && (relation !== "near" || entry.score <= maxDistance))
      .sort((a, b) => a.score - b.score)
      .map((entry) => entry.element);
  }

  function layoutScore(rect, anchorRects, relation) {
    let best = Infinity;
    for (const anchor of anchorRects) {
      let ok = true;
      let dx = 0;
      let dy = 0;
      if (relation === "right-of") {
        ok = rect.left >= anchor.right;
        dx = rect.left - anchor.right;
      } else if (relation === "left-of") {
        ok = rect.right <= anchor.left;
        dx = anchor.left - rect.right;
      } else if (relation === "above") {
        ok = rect.bottom <= anchor.top;
        dy = anchor.top - rect.bottom;
      } else if (relation === "below") {
        ok = rect.top >= anchor.bottom;
        dy = rect.top - anchor.bottom;
      } else {
        dx = Math.max(anchor.left - rect.right, rect.left - anchor.right, 0);
        dy = Math.max(anchor.top - rect.bottom, rect.top - anchor.bottom, 0);
      }
      if (!ok) {
        continue;
      }
      best = Math.min(best, Math.hypot(dx, dy));
    }
    return best;
  }

  function applyNth(elements, nth) {
    if (!Number.isSafeInteger(nth)) {
      return elements;
    }
    const index = nth < 0 ? elements.length + nth : nth;
    return index >= 0 && index < elements.length ? [elements[index]] : [];
  }

  function elementState(element) {
    const attributes = {};
    for (const attr of Array.from(element.attributes || [])) {
      attributes[attr.name] = String(attr.value ?? "");
    }
    return {
      tag_name: tag(element),
      text: String(element.innerText || element.textContent || ""),
      value: "value" in element ? String(element.value ?? "") : null,
      attributes,
      is_visible: isElementVisible(element),
      is_enabled: isElementEnabled(element),
      is_checked: checkedState(element)
    };
  }

  function allElements(rootElement) {
    // Deep, shadow-piercing enumeration in document order. Descends into every
    // open shadowRoot (closed roots are unreachable by spec). Mirrors the
    // raw-CDP path and Playwright's default open-shadow piercing.
    const out = [];
    const seen = new Set();
    collect(rootElement);
    return out;

    function collect(node) {
      if (!node) {
        return;
      }
      if (node instanceof Element) {
        if (seen.has(node)) {
          return;
        }
        seen.add(node);
        out.push(node);
        if (node.shadowRoot) {
          for (const child of node.shadowRoot.children) {
            collect(child);
          }
        }
      }
      const kids = node.children;
      if (kids) {
        for (const child of kids) {
          collect(child);
        }
      }
    }
  }

  function shadowScopes(rootElement) {
    // The rootElement's own tree scope plus every open shadowRoot reachable from
    // it. Each scope supports scope-local querySelectorAll (CSS combinators only
    // resolve within a single tree scope, matching Playwright semantics).
    const scopes = [rootElement];
    for (const element of allElements(rootElement)) {
      if (element.shadowRoot) {
        scopes.push(element.shadowRoot);
      }
    }
    return scopes;
  }

  function uniqueElements(elements) {
    const seen = new Set();
    const out = [];
    for (const element of elements) {
      if (!(element instanceof Element) || seen.has(element)) {
        continue;
      }
      seen.add(element);
      out.push(element);
    }
    return out;
  }

  function matchText(actual, expected, exact, regex) {
    const source = normalizeText(actual);
    const needle = String(expected || "");
    if (regex) {
      try {
        return new RegExp(needle).test(String(actual || ""));
      } catch (_) {
        return false;
      }
    }
    const normalizedNeedle = normalizeText(needle);
    return exact ? source === normalizedNeedle : source.includes(normalizedNeedle);
  }

  function normalizeText(value) {
    return String(value || "").replace(/\s+/g, " ").trim().toLowerCase();
  }

  function elementVisibleText(element) {
    return String(element.innerText || element.textContent || "");
  }

  function isFormLike(element) {
    return ["input", "textarea", "select", "button", "output"].includes(tag(element)) || element.hasAttribute("contenteditable");
  }

  function checkedState(element) {
    if ("checked" in element) {
      return Boolean(element.checked);
    }
    const aria = String(element.getAttribute("aria-checked") || "").toLowerCase();
    if (aria === "true") {
      return true;
    }
    if (aria === "false") {
      return false;
    }
    return null;
  }

  function boolAttr(element, name) {
    const value = String(element.getAttribute(name) || "").toLowerCase();
    if (value === "true") {
      return true;
    }
    if (value === "false") {
      return false;
    }
    return null;
  }

  function headingLevel(element) {
    const lower = tag(element);
    return /^h[1-6]$/.test(lower) ? Number(lower.slice(1)) : null;
  }

  function accessibleName(element) {
    // Resolve id-based references within the element's own tree scope (its
    // shadow root, or the document for light DOM) so labels inside shadow
    // components are found. getElementById/querySelector exist on both
    // Document and ShadowRoot.
    const scope = (element.getRootNode && element.getRootNode()) || document;
    const labelledBy = String(element.getAttribute("aria-labelledby") || "").trim();
    if (labelledBy) {
      const text = labelledBy
        .split(/\s+/)
        .map((id) => scope.getElementById?.(id)?.textContent || "")
        .join(" ")
        .trim();
      if (text) {
        return text;
      }
    }
    for (const attr of ["aria-label", "alt", "title", "placeholder", "value"]) {
      const value = String(element.getAttribute(attr) || "").trim();
      if (value) {
        return value;
      }
    }
    if (element.id) {
      const label = scope.querySelector?.(`label[for="${cssEscapeLocal(element.id)}"]`);
      if (label?.textContent?.trim()) {
        return label.textContent;
      }
    }
    const wrappingLabel = element.closest?.("label");
    if (wrappingLabel?.textContent?.trim()) {
      return wrappingLabel.textContent;
    }
    return element.textContent || "";
  }

  function inferRole(element) {
    const explicit = String(element.getAttribute("role") || "").trim().toLowerCase();
    if (explicit) {
      return explicit;
    }
    const lower = tag(element);
    if (lower === "a" && element.hasAttribute("href")) return "link";
    if (lower === "button") return "button";
    if (lower === "select") return "combobox";
    if (lower === "textarea") return "textbox";
    if (lower === "form") return "form";
    if (/^h[1-6]$/.test(lower)) return "heading";
    if (lower === "main") return "main";
    if (lower === "nav") return "navigation";
    if (lower === "section") return "region";
    if (lower === "img") return "img";
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset", "image"].includes(type)) return "button";
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      return "textbox";
    }
    return lower === "body" ? "document" : "generic";
  }

  function isElementEnabled(element) {
    if (Boolean(element.disabled)) {
      return false;
    }
    return String(element.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
  }

  function isElementVisible(element) {
    if (!(element instanceof Element) || !element.isConnected) {
      return false;
    }
    const style = window.getComputedStyle(element);
    if (!style || style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
      return false;
    }
    if (Number(style.opacity) === 0) {
      return false;
    }
    const rects = element.getClientRects();
    return rects && rects.length > 0 && Array.from(rects).some((rect) => rect.width > 0 && rect.height > 0);
  }

  function elementByPath(path) {
    if (!path) {
      return null;
    }
    // Shadow-aware path: numeric tokens index into children; the literal token
    // "s" means the next numeric token indexes into the current element's open
    // shadowRoot.children. Pure-numeric (legacy light-DOM) paths still resolve.
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children[index] || null;
    }
    return current instanceof Element ? current : null;
  }

  function domPath(element) {
    const parts = [];
    let current = element;
    while (current && current instanceof Element && current !== document.documentElement) {
      const parent = current.parentElement;
      if (parent) {
        parts.unshift(Array.prototype.indexOf.call(parent.children, current));
        current = parent;
        continue;
      }
      // No element parent: if we are at the top of an open shadow tree, cross the
      // boundary via the host and emit an "s" marker so elementByPath can re-enter.
      const root = current.parentNode;
      if (root && root instanceof ShadowRoot) {
        parts.unshift(Array.prototype.indexOf.call(root.children, current));
        parts.unshift("s");
        current = root.host;
        continue;
      }
      break;
    }
    parts.unshift(0);
    return parts.join(".");
  }

  function tag(element) {
    return String(element?.tagName || "").toLowerCase();
  }

  function cssEscapeLocal(value) {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return String(value).replace(/["\\]/g, "\\$&");
  }

  function errorMessageLocal(error) {
    return error && error.message ? String(error.message) : String(error);
  }

  function fail(errorCode, errorDetail, extra = {}) {
    return {
      ok: false,
      error_code: errorCode,
      error_detail: errorDetail,
      ...extra
    };
  }
}

async function runBridgeElementCommandInPage(request) {
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const operation = String(request?.operation || "inspect");
  const element = elementByPath(request?.elementPath);
  if (!element) {
    return fail(ERROR_ELEMENT_NOT_FOUND, `element path ${JSON.stringify(request?.elementPath)} was not found`);
  }
  if (operation === "scroll") {
    return await scrollCommand(element);
  }
  return await inspectCommand(element);

  async function inspectCommand(target) {
    const maxHtmlBytes = Number.isSafeInteger(request?.maxHtmlBytes)
      ? Math.max(0, Math.min(request.maxHtmlBytes, 2 * 1024 * 1024))
      : 256 * 1024;
    const inspection = elementInspection(target, maxHtmlBytes);
    inspection.actionability = await actionability(target);
    return {
      ok: true,
      element: inspection,
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || "")
    };
  }

  async function scrollCommand(target) {
    const beforeContainer = nearestScrollContainer(target);
    const before = scrollSnapshot(target, beforeContainer);
    let scrollPerformed = false;
    if (!rectFullyInViewport(before.viewport_rect) || !rectFullyInScrollContainer(before.viewport_rect, before.scroll_container)) {
      try {
        target.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "instant" });
        scrollPerformed = true;
      } catch (error) {
        try {
          target.scrollIntoView();
          scrollPerformed = true;
        } catch (_) {
          return fail("CHROME_DOM_ELEMENT_NOT_ACTIONABLE", `scrollIntoView failed: ${errorMessage(error)}`);
        }
      }
    }
    if (scrollPerformed) {
      await twoAnimationFrames();
    }
    const afterContainer = nearestScrollContainer(target);
    const after = scrollSnapshot(target, afterContainer);
    return {
      ok: true,
      scroll: {
        before_viewport: before.viewport,
        after_viewport: after.viewport,
        before_box_model: before.box_model,
        after_box_model: after.box_model,
        before_scroll_container: before.scroll_container,
        after_scroll_container: after.scroll_container,
        window_scroll_changed:
          before.viewport.scroll_x !== after.viewport.scroll_x ||
          before.viewport.scroll_y !== after.viewport.scroll_y,
        container_scroll_changed: scrollContainerChanged(before.scroll_container, after.scroll_container),
        scroll_performed: scrollPerformed,
        node_fully_in_viewport_after: rectFullyInViewport(after.viewport_rect),
        readback_backend: "Element.scrollIntoView({block:nearest,inline:nearest}) when needed + getBoundingClientRect + scroll container readback"
      },
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || "")
    };
  }

  function elementInspection(target, maxHtmlBytes) {
    const max = Number.isSafeInteger(maxHtmlBytes) && maxHtmlBytes >= 0 ? maxHtmlBytes : 0;
    const outer = stringValue(target.outerHTML);
    const inner = stringValue(target.innerHTML);
    const innerText = stringValue(target.innerText);
    const textContent = stringValue(target.textContent);
    const attributes = {};
    for (const attr of Array.from(target.attributes || [])) {
      attributes[attr.name] = String(attr.value ?? "");
    }
    const rect = viewportRect(target);
    const scrollX = Number(window.scrollX || 0);
    const scrollY = Number(window.scrollY || 0);
    return {
      tag_name: stringValue(target.tagName),
      outer_html: sliceChars(outer, max),
      inner_html: sliceChars(inner, max),
      inner_text: sliceChars(innerText, max),
      text_content: sliceChars(textContent, max),
      html_truncated:
        outer.length > max ||
        inner.length > max ||
        innerText.length > max ||
        textContent.length > max,
      max_html_bytes: max,
      attributes,
      input_value: "value" in target ? stringValue(target.value) : null,
      is_visible: isVisible(target),
      is_enabled: isEnabled(target),
      is_checked: isChecked(target),
      is_editable: isEditable(target),
      bounding_box: {
        x: rect.left + scrollX,
        y: rect.top + scrollY,
        viewport_x: rect.left,
        viewport_y: rect.top,
        width: rect.width,
        height: rect.height
      },
      device_pixel_ratio: Number(window.devicePixelRatio || 1)
    };
  }

  async function actionability(target) {
    const firstBox = boxModel(target);
    await twoAnimationFrames();
    const secondBox = boxModel(target);
    const dom = domState(target);
    const visible = dom.attached && isVisible(target) && Boolean(firstBox?.content?.width > 0 && firstBox?.content?.height > 0);
    const runningAnimation = hasRunningAnimation(target);
    const stable = !runningAnimation && boxesStable(firstBox, secondBox);
    const hit = hitTest(target, firstBox);
    const failures = [];
    if (!dom.attached) {
      failures.push(failure("attached", "detached", "target element is not connected to the document"));
    }
    if (!visible) {
      failures.push(failure("visible", "not_visible", "target element has no visible rendered box"));
    }
    if (!stable) {
      failures.push(failure(
        "stable",
        runningAnimation ? "animation_running" : "box_changed",
        runningAnimation
          ? "target element has a running CSS/Web Animation"
          : "target element box changed between animation-frame samples"
      ));
    }
    if (!dom.enabled) {
      failures.push(failure("enabled", "disabled", "target element is disabled or aria-disabled"));
    }
    if (!hit.receives_events) {
      failures.push(failure(
        "receives_events",
        "obscured",
        hit.top_node_description
          ? `action point is covered by ${hit.top_node_description}`
          : "document.elementFromPoint did not resolve to the target or one of its descendants"
      ));
    }
    const actionReady = dom.attached && visible && stable && dom.enabled && hit.receives_events;
    return {
      target_id: "",
      backend_node_id: 0,
      attached: dom.attached,
      visible,
      stable,
      enabled: dom.enabled,
      editable: dom.editable,
      receives_events: hit.receives_events,
      action_ready: actionReady,
      editable_action_ready: actionReady && dom.editable,
      failure_reasons: failures,
      box_model: firstBox,
      second_box_model: secondBox,
      box_model_error: firstBox ? null : "getBoundingClientRect returned no box",
      second_box_model_error: secondBox ? null : "second getBoundingClientRect returned no box",
      dom_state: dom,
      hit_test: hit
    };
  }

  function domState(target) {
    const style = target instanceof Element ? window.getComputedStyle(target) : null;
    const rect = viewportRect(target);
    const disabled = Boolean(target.disabled);
    const ariaDisabled = String(target.getAttribute?.("aria-disabled") || "").toLowerCase() === "true";
    const readonly = Boolean(target.readOnly);
    const enabled = !disabled && !ariaDisabled;
    return {
      attached: Boolean(target && target.isConnected),
      tag_name: tag(target),
      id: stringValue(target.id),
      class_name: stringValue(target.className),
      name_attr: stringValue(target.getAttribute?.("name")),
      type_attr: stringValue(target.getAttribute?.("type")),
      display: stringValue(style?.display),
      visibility: stringValue(style?.visibility),
      pointer_events: stringValue(style?.pointerEvents),
      disabled,
      aria_disabled: ariaDisabled,
      readonly,
      enabled,
      editable: isEditable(target),
      viewport_rect: {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height
      },
      node_description: nodeDescription(target)
    };
  }

  function boxModel(target) {
    if (!(target instanceof Element) || typeof target.getBoundingClientRect !== "function") {
      return null;
    }
    const rect = target.getBoundingClientRect();
    if (!rect) {
      return null;
    }
    const pageRect = {
      x: Number(rect.left || 0) + Number(window.scrollX || 0),
      y: Number(rect.top || 0) + Number(window.scrollY || 0),
      width: Number(rect.width || 0),
      height: Number(rect.height || 0)
    };
    return {
      content: pageRect,
      border: { ...pageRect },
      width: Math.round(pageRect.width),
      height: Math.round(pageRect.height)
    };
  }

  function hitTest(target, model) {
    if (!model || !model.content || model.content.width <= 0 || model.content.height <= 0) {
      return {
        point: null,
        receives_events: false,
        top_backend_node_id: null,
        top_frame_id: null,
        target_or_descendant_from_point: false,
        top_node_description: null,
        error: "no action point"
      };
    }
    const rect = viewportRect(target);
    const point = {
      x: rect.left + rect.width / 2,
      y: rect.top + rect.height / 2
    };
    let top = null;
    let error = null;
    try {
      top = document.elementFromPoint(point.x, point.y);
    } catch (caught) {
      error = errorMessage(caught);
    }
    const targetOrDescendant = top instanceof Element && (top === target || target.contains(top));
    return {
      point,
      receives_events: Boolean(targetOrDescendant),
      top_backend_node_id: null,
      top_frame_id: null,
      target_or_descendant_from_point: Boolean(targetOrDescendant),
      top_node_description: top instanceof Element ? nodeDescription(top) : null,
      error
    };
  }

  function scrollSnapshot(target, scrollContainer) {
    const rect = viewportRect(target);
    return {
      viewport: {
        inner_width: Math.trunc(Number(window.innerWidth || 0)),
        inner_height: Math.trunc(Number(window.innerHeight || 0)),
        scroll_x: Number(window.scrollX || 0),
        scroll_y: Number(window.scrollY || 0),
        device_pixel_ratio: Number(window.devicePixelRatio || 1)
      },
      viewport_rect: {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height
      },
      box_model: boxModel(target),
      scroll_container: scrollContainerSnapshot(scrollContainer)
    };
  }

  function nearestScrollContainer(target) {
    let current = target instanceof Element ? target.parentElement : null;
    while (current && current !== document.documentElement) {
      const style = window.getComputedStyle(current);
      const overflow = `${style.overflow || ""} ${style.overflowX || ""} ${style.overflowY || ""}`;
      const scrollableStyle = /(auto|scroll|overlay)/i.test(overflow);
      const scrollableGeometry =
        current.scrollHeight > current.clientHeight ||
        current.scrollWidth > current.clientWidth;
      if (scrollableStyle && scrollableGeometry) {
        return current;
      }
      current = current.parentElement;
    }
    return null;
  }

  function scrollContainerSnapshot(target) {
    if (!(target instanceof Element)) {
      return null;
    }
    const rect = viewportRect(target);
    return {
      element_id: domPath(target),
      tag_name: tag(target),
      id: stringValue(target.id),
      class_name: stringValue(target.className),
      scroll_top: Number(target.scrollTop || 0),
      scroll_left: Number(target.scrollLeft || 0),
      scroll_height: Number(target.scrollHeight || 0),
      scroll_width: Number(target.scrollWidth || 0),
      client_height: Number(target.clientHeight || 0),
      client_width: Number(target.clientWidth || 0),
      viewport_rect: {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height
      }
    };
  }

  function scrollContainerChanged(before, after) {
    if (!before && !after) {
      return false;
    }
    if (!before || !after) {
      return true;
    }
    return before.scroll_top !== after.scroll_top || before.scroll_left !== after.scroll_left;
  }

  function rectFullyInViewport(rect) {
    if (!rect) {
      return false;
    }
    const epsilon = 0.5;
    return (
      rect.x >= -epsilon &&
      rect.y >= -epsilon &&
      rect.x + rect.width <= Number(window.innerWidth || 0) + epsilon &&
      rect.y + rect.height <= Number(window.innerHeight || 0) + epsilon
    );
  }

  function rectFullyInScrollContainer(rect, scrollContainer) {
    if (!rect || !scrollContainer || !scrollContainer.viewport_rect) {
      return true;
    }
    const container = scrollContainer.viewport_rect;
    const epsilon = 0.5;
    return (
      rect.x >= container.x - epsilon &&
      rect.y >= container.y - epsilon &&
      rect.x + rect.width <= container.x + container.width + epsilon &&
      rect.y + rect.height <= container.y + container.height + epsilon
    );
  }

  function boxesStable(a, b) {
    if (!a || !b || !a.content || !b.content) {
      return false;
    }
    const epsilon = 0.25;
    return (
      Math.abs(a.content.x - b.content.x) <= epsilon &&
      Math.abs(a.content.y - b.content.y) <= epsilon &&
      Math.abs(a.content.width - b.content.width) <= epsilon &&
      Math.abs(a.content.height - b.content.height) <= epsilon
    );
  }

  function hasRunningAnimation(target) {
    if (!(target instanceof Element) || typeof target.getAnimations !== "function") {
      return false;
    }
    try {
      return target.getAnimations({ subtree: false }).some((animation) => animation.playState === "running");
    } catch (_) {
      return false;
    }
  }

  function elementByPath(path) {
    if (!path) {
      return null;
    }
    // Shadow-aware path: numeric tokens index into children; the literal token
    // "s" means the next numeric token indexes into the current element's open
    // shadowRoot.children. Pure-numeric (legacy light-DOM) paths still resolve.
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children[index] || null;
    }
    return current instanceof Element ? current : null;
  }

  function domPath(target) {
    const parts = [];
    let current = target;
    while (current && current instanceof Element && current !== document.documentElement) {
      const parent = current.parentElement;
      if (parent) {
        parts.unshift(Array.prototype.indexOf.call(parent.children, current));
        current = parent;
        continue;
      }
      const root = current.parentNode;
      if (root && root instanceof ShadowRoot) {
        parts.unshift(Array.prototype.indexOf.call(root.children, current));
        parts.unshift("s");
        current = root.host;
        continue;
      }
      break;
    }
    parts.unshift(0);
    return parts.join(".");
  }

  function viewportRect(target) {
    if (!(target instanceof Element) || typeof target.getBoundingClientRect !== "function") {
      return { left: 0, top: 0, width: 0, height: 0 };
    }
    const rect = target.getBoundingClientRect();
    return {
      left: Number(rect.left || 0),
      top: Number(rect.top || 0),
      width: Number(rect.width || 0),
      height: Number(rect.height || 0)
    };
  }

  function isVisible(target) {
    if (!(target instanceof Element) || !target.isConnected) {
      return false;
    }
    const style = window.getComputedStyle(target);
    if (!style || style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
      return false;
    }
    if (Number(style.opacity) === 0) {
      return false;
    }
    const rects = target.getClientRects();
    return rects && rects.length > 0 && Array.from(rects).some((rect) => rect.width > 0 && rect.height > 0);
  }

  function isEnabled(target) {
    if (!(target instanceof Element)) {
      return false;
    }
    if (Boolean(target.disabled)) {
      return false;
    }
    return String(target.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
  }

  function isEditable(target) {
    if (!(target instanceof Element) || !isEnabled(target)) {
      return false;
    }
    const lower = tag(target);
    if (lower === "textarea") {
      return !Boolean(target.readOnly);
    }
    if (lower === "input") {
      const type = String(target.getAttribute("type") || "text").toLowerCase();
      const editableTypes = new Set(["text", "search", "url", "tel", "email", "password", "number", "date", "datetime-local", "month", "time", "week", "color"]);
      return editableTypes.has(type) && !Boolean(target.readOnly);
    }
    return Boolean(
      target.isContentEditable ||
        String(target.getAttribute("contenteditable") || "").toLowerCase() === "true" ||
        String(target.getAttribute("role") || "").toLowerCase() === "textbox"
    );
  }

  function isChecked(target) {
    if (target && "checked" in target) {
      return Boolean(target.checked);
    }
    return String(target?.getAttribute?.("aria-checked") || "").toLowerCase() === "true";
  }

  function nodeDescription(target) {
    if (!(target instanceof Element)) {
      return "";
    }
    const id = target.id ? `#${target.id}` : "";
    const cls = typeof target.className === "string" && target.className.trim()
      ? `.${target.className.trim().split(/\s+/).slice(0, 3).join(".")}`
      : "";
    const name = target.getAttribute("name") ? `[name="${target.getAttribute("name")}"]` : "";
    return `${tag(target)}${id}${cls}${name}`;
  }

  function failure(predicate, reason, detail) {
    return { predicate, reason, detail };
  }

  function twoAnimationFrames() {
    return new Promise((resolve) => {
      let done = false;
      const finish = () => {
        if (done) {
          return;
        }
        done = true;
        resolve();
      };
      setTimeout(finish, 100);
      if (typeof requestAnimationFrame === "function") {
        requestAnimationFrame(() => requestAnimationFrame(finish));
      }
    });
  }

  function sliceChars(value, max) {
    return Array.from(String(value || "")).slice(0, Math.max(0, max)).join("");
  }

  function stringValue(value) {
    return value === null || value === undefined ? "" : String(value);
  }

  function tag(target) {
    return stringValue(target?.tagName).toLowerCase();
  }

  function errorMessage(error) {
    return error && error.message ? String(error.message) : String(error);
  }

  function fail(errorCode, errorDetail, extra = {}) {
    return {
      ok: false,
      error_code: errorCode,
      error_detail: errorDetail,
      ...extra
    };
  }
}

async function inspectElementInPage(request) {
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const element = bridgeElementByPath(request?.elementPath);
  if (!element) {
    return bridgeFail(ERROR_ELEMENT_NOT_FOUND, `element path ${JSON.stringify(request?.elementPath)} was not found`);
  }
  const maxHtmlBytes = Number.isSafeInteger(request?.maxHtmlBytes)
    ? Math.max(0, Math.min(request.maxHtmlBytes, 2 * 1024 * 1024))
    : 256 * 1024;
  const inspection = bridgeElementInspection(element, maxHtmlBytes);
  inspection.actionability = await bridgeActionability(element);
  return {
    ok: true,
    element: inspection,
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || "")
  };
}

async function scrollElementIntoViewInPage(request) {
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const element = bridgeElementByPath(request?.elementPath);
  if (!element) {
    return bridgeFail(ERROR_ELEMENT_NOT_FOUND, `element path ${JSON.stringify(request?.elementPath)} was not found`);
  }
  const beforeContainer = bridgeNearestScrollContainer(element);
  const before = bridgeScrollSnapshot(element, beforeContainer);
  let scrollPerformed = false;
  if (!bridgeRectFullyInViewport(before.viewport_rect) || !bridgeRectFullyInScrollContainer(before.viewport_rect, before.scroll_container)) {
    try {
      element.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "instant" });
      scrollPerformed = true;
    } catch (error) {
      try {
        element.scrollIntoView();
        scrollPerformed = true;
      } catch (_) {
        return bridgeFail("CHROME_DOM_ELEMENT_NOT_ACTIONABLE", `scrollIntoView failed: ${bridgeErrorMessage(error)}`);
      }
    }
  }
  if (scrollPerformed) {
    await bridgeTwoAnimationFrames();
  }
  const afterContainer = bridgeNearestScrollContainer(element);
  const after = bridgeScrollSnapshot(element, afterContainer);
  return {
    ok: true,
    scroll: {
      before_viewport: before.viewport,
      after_viewport: after.viewport,
      before_box_model: before.box_model,
      after_box_model: after.box_model,
      before_scroll_container: before.scroll_container,
      after_scroll_container: after.scroll_container,
      window_scroll_changed:
        before.viewport.scroll_x !== after.viewport.scroll_x ||
        before.viewport.scroll_y !== after.viewport.scroll_y,
      container_scroll_changed: bridgeScrollContainerChanged(before.scroll_container, after.scroll_container),
      scroll_performed: scrollPerformed,
      node_fully_in_viewport_after: bridgeRectFullyInViewport(after.viewport_rect),
      readback_backend: "Element.scrollIntoView({block:nearest,inline:nearest}) when needed + getBoundingClientRect + scroll container readback"
    },
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || "")
  };
}

function bridgeElementInspection(element, maxHtmlBytes) {
  const max = Number.isSafeInteger(maxHtmlBytes) && maxHtmlBytes >= 0 ? maxHtmlBytes : 0;
  const outer = bridgeString(element.outerHTML);
  const inner = bridgeString(element.innerHTML);
  const innerText = bridgeString(element.innerText);
  const textContent = bridgeString(element.textContent);
  const attributes = {};
  for (const attr of Array.from(element.attributes || [])) {
    attributes[attr.name] = String(attr.value ?? "");
  }
  const rect = bridgeViewportRect(element);
  const scrollX = Number(window.scrollX || 0);
  const scrollY = Number(window.scrollY || 0);
  return {
    tag_name: bridgeString(element.tagName),
    outer_html: bridgeSliceChars(outer, max),
    inner_html: bridgeSliceChars(inner, max),
    inner_text: bridgeSliceChars(innerText, max),
    text_content: bridgeSliceChars(textContent, max),
    html_truncated:
      outer.length > max ||
      inner.length > max ||
      innerText.length > max ||
      textContent.length > max,
    max_html_bytes: max,
    attributes,
    input_value: "value" in element ? bridgeString(element.value) : null,
    is_visible: bridgeIsVisible(element),
    is_enabled: bridgeIsEnabled(element),
    is_checked: bridgeIsChecked(element),
    is_editable: bridgeIsEditable(element),
    bounding_box: {
      x: rect.left + scrollX,
      y: rect.top + scrollY,
      viewport_x: rect.left,
      viewport_y: rect.top,
      width: rect.width,
      height: rect.height
    },
    device_pixel_ratio: Number(window.devicePixelRatio || 1)
  };
}

async function bridgeActionability(element) {
  const firstBox = bridgeBoxModel(element);
  await bridgeTwoAnimationFrames();
  const secondBox = bridgeBoxModel(element);
  const domState = bridgeDomState(element);
  const visible = domState.attached && bridgeIsVisible(element) && Boolean(firstBox?.content?.width > 0 && firstBox?.content?.height > 0);
  const runningAnimation = bridgeHasRunningAnimation(element);
  const stable = !runningAnimation && bridgeBoxesStable(firstBox, secondBox);
  const hitTest = bridgeHitTest(element, firstBox);
  const enabled = domState.enabled;
  const editable = domState.editable;
  const receivesEvents = hitTest.receives_events;
  const failureReasons = [];
  if (!domState.attached) {
    failureReasons.push(bridgeFailure("attached", "detached", "target element is not connected to the document"));
  }
  if (!visible) {
    failureReasons.push(bridgeFailure("visible", "not_visible", "target element has no visible rendered box"));
  }
  if (!stable) {
    failureReasons.push(bridgeFailure(
      "stable",
      runningAnimation ? "animation_running" : "box_changed",
      runningAnimation
        ? "target element has a running CSS/Web Animation"
        : "target element box changed between animation-frame samples"
    ));
  }
  if (!enabled) {
    failureReasons.push(bridgeFailure("enabled", "disabled", "target element is disabled or aria-disabled"));
  }
  if (!receivesEvents) {
    failureReasons.push(bridgeFailure(
      "receives_events",
      "obscured",
      hitTest.top_node_description
        ? `action point is covered by ${hitTest.top_node_description}`
        : "document.elementFromPoint did not resolve to the target or one of its descendants"
    ));
  }
  const actionReady = domState.attached && visible && stable && enabled && receivesEvents;
  return {
    target_id: "",
    backend_node_id: 0,
    attached: domState.attached,
    visible,
    stable,
    enabled,
    editable,
    receives_events: receivesEvents,
    action_ready: actionReady,
    editable_action_ready: actionReady && editable,
    failure_reasons: failureReasons,
    box_model: firstBox,
    second_box_model: secondBox,
    box_model_error: firstBox ? null : "getBoundingClientRect returned no box",
    second_box_model_error: secondBox ? null : "second getBoundingClientRect returned no box",
    dom_state: domState,
    hit_test: hitTest
  };
}

function bridgeDomState(element) {
  const style = element instanceof Element ? window.getComputedStyle(element) : null;
  const rect = bridgeViewportRect(element);
  const disabled = Boolean(element.disabled);
  const ariaDisabled = String(element.getAttribute?.("aria-disabled") || "").toLowerCase() === "true";
  const readonly = Boolean(element.readOnly);
  const enabled = !disabled && !ariaDisabled;
  return {
    attached: Boolean(element && element.isConnected),
    tag_name: bridgeTag(element),
    id: bridgeString(element.id),
    class_name: bridgeString(element.className),
    name_attr: bridgeString(element.getAttribute?.("name")),
    type_attr: bridgeString(element.getAttribute?.("type")),
    display: bridgeString(style?.display),
    visibility: bridgeString(style?.visibility),
    pointer_events: bridgeString(style?.pointerEvents),
    disabled,
    aria_disabled: ariaDisabled,
    readonly,
    enabled,
    editable: bridgeIsEditable(element),
    viewport_rect: {
      x: rect.left,
      y: rect.top,
      width: rect.width,
      height: rect.height
    },
    node_description: bridgeNodeDescription(element)
  };
}

function bridgeBoxModel(element) {
  if (!(element instanceof Element) || typeof element.getBoundingClientRect !== "function") {
    return null;
  }
  const rect = element.getBoundingClientRect();
  if (!rect) {
    return null;
  }
  const scrollX = Number(window.scrollX || 0);
  const scrollY = Number(window.scrollY || 0);
  const pageRect = {
    x: Number(rect.left || 0) + scrollX,
    y: Number(rect.top || 0) + scrollY,
    width: Number(rect.width || 0),
    height: Number(rect.height || 0)
  };
  return {
    content: pageRect,
    border: { ...pageRect },
    width: Math.round(pageRect.width),
    height: Math.round(pageRect.height)
  };
}

function bridgeHitTest(element, boxModel) {
  if (!boxModel || !boxModel.content || boxModel.content.width <= 0 || boxModel.content.height <= 0) {
    return {
      point: null,
      receives_events: false,
      top_backend_node_id: null,
      top_frame_id: null,
      target_or_descendant_from_point: false,
      top_node_description: null,
      error: "no action point"
    };
  }
  const viewportRect = bridgeViewportRect(element);
  const point = {
    x: viewportRect.left + viewportRect.width / 2,
    y: viewportRect.top + viewportRect.height / 2
  };
  let top = null;
  let error = null;
  try {
    top = document.elementFromPoint(point.x, point.y);
  } catch (caught) {
    error = bridgeErrorMessage(caught);
  }
  const targetOrDescendant = top instanceof Element && (top === element || element.contains(top));
  return {
    point,
    receives_events: Boolean(targetOrDescendant),
    top_backend_node_id: null,
    top_frame_id: null,
    target_or_descendant_from_point: Boolean(targetOrDescendant),
    top_node_description: top instanceof Element ? bridgeNodeDescription(top) : null,
    error
  };
}

function bridgeScrollSnapshot(element, scrollContainer) {
  const viewportRect = bridgeViewportRect(element);
  return {
    viewport: {
      inner_width: Math.trunc(Number(window.innerWidth || 0)),
      inner_height: Math.trunc(Number(window.innerHeight || 0)),
      scroll_x: Number(window.scrollX || 0),
      scroll_y: Number(window.scrollY || 0),
      device_pixel_ratio: Number(window.devicePixelRatio || 1)
    },
    viewport_rect: {
      x: viewportRect.left,
      y: viewportRect.top,
      width: viewportRect.width,
      height: viewportRect.height
    },
    box_model: bridgeBoxModel(element),
    scroll_container: bridgeScrollContainerSnapshot(scrollContainer)
  };
}

function bridgeNearestScrollContainer(element) {
  let current = element instanceof Element ? element.parentElement : null;
  while (current && current !== document.documentElement) {
    const style = window.getComputedStyle(current);
    const overflow = `${style.overflow || ""} ${style.overflowX || ""} ${style.overflowY || ""}`;
    const scrollableStyle = /(auto|scroll|overlay)/i.test(overflow);
    const scrollableGeometry =
      current.scrollHeight > current.clientHeight ||
      current.scrollWidth > current.clientWidth;
    if (scrollableStyle && scrollableGeometry) {
      return current;
    }
    current = current.parentElement;
  }
  return null;
}

function bridgeScrollContainerSnapshot(element) {
  if (!(element instanceof Element)) {
    return null;
  }
  const rect = bridgeViewportRect(element);
  return {
    element_id: bridgeDomPath(element),
    tag_name: bridgeTag(element),
    id: bridgeString(element.id),
    class_name: bridgeString(element.className),
    scroll_top: Number(element.scrollTop || 0),
    scroll_left: Number(element.scrollLeft || 0),
    scroll_height: Number(element.scrollHeight || 0),
    scroll_width: Number(element.scrollWidth || 0),
    client_height: Number(element.clientHeight || 0),
    client_width: Number(element.clientWidth || 0),
    viewport_rect: {
      x: rect.left,
      y: rect.top,
      width: rect.width,
      height: rect.height
    }
  };
}

function bridgeScrollContainerChanged(before, after) {
  if (!before && !after) {
    return false;
  }
  if (!before || !after) {
    return true;
  }
  return before.scroll_top !== after.scroll_top || before.scroll_left !== after.scroll_left;
}

function bridgeRectFullyInViewport(rect) {
  if (!rect) {
    return false;
  }
  const epsilon = 0.5;
  return (
    rect.x >= -epsilon &&
    rect.y >= -epsilon &&
    rect.x + rect.width <= Number(window.innerWidth || 0) + epsilon &&
    rect.y + rect.height <= Number(window.innerHeight || 0) + epsilon
  );
}

function bridgeRectFullyInScrollContainer(rect, scrollContainer) {
  if (!rect || !scrollContainer || !scrollContainer.viewport_rect) {
    return true;
  }
  const container = scrollContainer.viewport_rect;
  const epsilon = 0.5;
  return (
    rect.x >= container.x - epsilon &&
    rect.y >= container.y - epsilon &&
    rect.x + rect.width <= container.x + container.width + epsilon &&
    rect.y + rect.height <= container.y + container.height + epsilon
  );
}

function bridgeBoxesStable(a, b) {
  if (!a || !b || !a.content || !b.content) {
    return false;
  }
  const epsilon = 0.25;
  return (
    Math.abs(a.content.x - b.content.x) <= epsilon &&
    Math.abs(a.content.y - b.content.y) <= epsilon &&
    Math.abs(a.content.width - b.content.width) <= epsilon &&
    Math.abs(a.content.height - b.content.height) <= epsilon
  );
}

function bridgeHasRunningAnimation(element) {
  if (!(element instanceof Element) || typeof element.getAnimations !== "function") {
    return false;
  }
  try {
    return element.getAnimations({ subtree: false }).some((animation) => animation.playState === "running");
  } catch (_) {
    return false;
  }
}

function bridgeElementByPath(path) {
  if (!path) {
    return null;
  }
  // Shadow-aware: "s" token re-enters the current element's open shadowRoot.
  const parts = String(path).split(".");
  if (Number(parts[0]) !== 0) {
    return null;
  }
  let current = document.documentElement;
  let enterShadow = false;
  for (const raw of parts.slice(1)) {
    if (raw === "s") {
      enterShadow = true;
      continue;
    }
    const index = Number(raw);
    if (!current || !Number.isSafeInteger(index) || index < 0) {
      return null;
    }
    let scope = current;
    if (enterShadow) {
      if (!current.shadowRoot) {
        return null;
      }
      scope = current.shadowRoot;
      enterShadow = false;
    }
    current = scope.children[index] || null;
  }
  return current instanceof Element ? current : null;
}

function bridgeDomPath(element) {
  const parts = [];
  let current = element;
  while (current && current instanceof Element && current !== document.documentElement) {
    const parent = current.parentElement;
    if (parent) {
      parts.unshift(Array.prototype.indexOf.call(parent.children, current));
      current = parent;
      continue;
    }
    const root = current.parentNode;
    if (root && root instanceof ShadowRoot) {
      parts.unshift(Array.prototype.indexOf.call(root.children, current));
      parts.unshift("s");
      current = root.host;
      continue;
    }
    break;
  }
  parts.unshift(0);
  return parts.join(".");
}

function bridgeViewportRect(element) {
  if (!(element instanceof Element) || typeof element.getBoundingClientRect !== "function") {
    return { left: 0, top: 0, width: 0, height: 0 };
  }
  const rect = element.getBoundingClientRect();
  return {
    left: Number(rect.left || 0),
    top: Number(rect.top || 0),
    width: Number(rect.width || 0),
    height: Number(rect.height || 0)
  };
}

function bridgeIsVisible(element) {
  if (!(element instanceof Element) || !element.isConnected) {
    return false;
  }
  const style = window.getComputedStyle(element);
  if (!style || style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
    return false;
  }
  if (Number(style.opacity) === 0) {
    return false;
  }
  const rects = element.getClientRects();
  return rects && rects.length > 0 && Array.from(rects).some((rect) => rect.width > 0 && rect.height > 0);
}

function bridgeIsEnabled(element) {
  if (!(element instanceof Element)) {
    return false;
  }
  if (Boolean(element.disabled)) {
    return false;
  }
  return String(element.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
}

function bridgeIsEditable(element) {
  if (!(element instanceof Element) || !bridgeIsEnabled(element)) {
    return false;
  }
  const lower = bridgeTag(element);
  if (lower === "textarea") {
    return !Boolean(element.readOnly);
  }
  if (lower === "input") {
    const type = String(element.getAttribute("type") || "text").toLowerCase();
    const editableTypes = new Set(["text", "search", "url", "tel", "email", "password", "number", "date", "datetime-local", "month", "time", "week", "color"]);
    return editableTypes.has(type) && !Boolean(element.readOnly);
  }
  return Boolean(
    element.isContentEditable ||
      String(element.getAttribute("contenteditable") || "").toLowerCase() === "true" ||
      String(element.getAttribute("role") || "").toLowerCase() === "textbox"
  );
}

function bridgeIsChecked(element) {
  if (element && "checked" in element) {
    return Boolean(element.checked);
  }
  return String(element?.getAttribute?.("aria-checked") || "").toLowerCase() === "true";
}

function bridgeNodeDescription(element) {
  if (!(element instanceof Element)) {
    return "";
  }
  const id = element.id ? `#${element.id}` : "";
  const cls = typeof element.className === "string" && element.className.trim()
    ? `.${element.className.trim().split(/\s+/).slice(0, 3).join(".")}`
    : "";
  const name = element.getAttribute("name") ? `[name="${element.getAttribute("name")}"]` : "";
  return `${bridgeTag(element)}${id}${cls}${name}`;
}

function bridgeFailure(predicate, reason, detail) {
  return { predicate, reason, detail };
}

function bridgeTwoAnimationFrames() {
  return new Promise((resolve) => {
    let done = false;
    const finish = () => {
      if (done) {
        return;
      }
      done = true;
      resolve();
    };
    setTimeout(finish, 100);
    if (typeof requestAnimationFrame === "function") {
      requestAnimationFrame(() => requestAnimationFrame(finish));
    }
  });
}

function bridgeSliceChars(value, max) {
  return Array.from(String(value || "")).slice(0, Math.max(0, max)).join("");
}

function bridgeString(value) {
  return value === null || value === undefined ? "" : String(value);
}

function bridgeTag(element) {
  return bridgeString(element?.tagName).toLowerCase();
}

function bridgeErrorMessage(error) {
  return error && error.message ? String(error.message) : String(error);
}

function bridgeFail(errorCode, errorDetail, extra = {}) {
  return {
    ok: false,
    error_code: errorCode,
    error_detail: errorDetail,
    ...extra
  };
}

function runAriaSnapshotInPage(request) {
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const maxNodes = Number.isSafeInteger(request?.maxNodes) ? Math.max(1, Math.min(request.maxNodes, 5000)) : 500;
  const maxDepth = Number.isSafeInteger(request?.maxDepth) ? Math.max(1, Math.min(request.maxDepth, 128)) : 32;
  const root = elementByPath(request?.rootPath) || document.body || document.documentElement;
  if (!root) {
    return fail(ERROR_ELEMENT_NOT_FOUND, `root element path ${JSON.stringify(request?.rootPath)} was not found`);
  }
  const nodes = [];
  let truncatedByMaxNodes = false;
  let truncatedByDepth = false;
  walk(root, null, 0);
  return {
    ok: true,
    nodes,
    total_ax_nodes: nodes.length,
    truncated_by_max_nodes: truncatedByMaxNodes,
    truncated_by_depth: truncatedByDepth,
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || "")
  };

  function walk(element, parentPath, depth) {
    if (!(element instanceof Element)) {
      return;
    }
    if (depth > maxDepth) {
      truncatedByDepth = true;
      return;
    }
    if (nodes.length >= maxNodes) {
      truncatedByMaxNodes = true;
      return;
    }
    const path = domPath(element);
    const children = Array.from(element.children || []);
    nodes.push({
      element_id: path,
      parent_element_id: parentPath,
      depth,
      role: inferRole(element),
      name: accessibleName(element).replace(/\s+/g, " ").trim(),
      value: elementValue(element),
      enabled: isElementEnabled(element),
      focused: document.activeElement === element,
      children_count: children.length
    });
    for (const child of children) {
      walk(child, path, depth + 1);
      if (nodes.length >= maxNodes) {
        truncatedByMaxNodes = true;
        break;
      }
    }
  }

  function accessibleName(element) {
    const labelledBy = String(element.getAttribute("aria-labelledby") || "").trim();
    if (labelledBy) {
      const text = labelledBy
        .split(/\s+/)
        .map((id) => document.getElementById(id)?.textContent || "")
        .join(" ")
        .trim();
      if (text) {
        return text;
      }
    }
    for (const attr of ["aria-label", "alt", "title", "placeholder", "value"]) {
      const value = String(element.getAttribute(attr) || "").trim();
      if (value) {
        return value;
      }
    }
    if (element.id) {
      const label = document.querySelector(`label[for="${cssEscapeLocal(element.id)}"]`);
      if (label?.textContent?.trim()) {
        return label.textContent;
      }
    }
    const wrappingLabel = element.closest?.("label");
    if (wrappingLabel?.textContent?.trim()) {
      return wrappingLabel.textContent;
    }
    const role = inferRole(element);
    if (["button", "link", "heading", "checkbox", "radio", "textbox"].includes(role)) {
      return element.textContent || "";
    }
    return "";
  }

  function inferRole(element) {
    const explicit = String(element.getAttribute("role") || "").trim().toLowerCase();
    if (explicit) {
      return explicit;
    }
    const lower = tag(element);
    if (lower === "html" || lower === "body") return "document";
    if (lower === "a" && element.hasAttribute("href")) return "link";
    if (lower === "button") return "button";
    if (lower === "select") return "combobox";
    if (lower === "textarea") return "textbox";
    if (lower === "form") return "form";
    if (/^h[1-6]$/.test(lower)) return "heading";
    if (lower === "main") return "main";
    if (lower === "nav") return "navigation";
    if (lower === "section") return "region";
    if (lower === "img") return "img";
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset", "image"].includes(type)) return "button";
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      return "textbox";
    }
    return "generic";
  }

  function elementValue(element) {
    if ("value" in element) {
      return String(element.value ?? "");
    }
    for (const attr of ["aria-valuetext", "aria-valuenow"]) {
      const value = String(element.getAttribute(attr) || "").trim();
      if (value) {
        return value;
      }
    }
    return null;
  }

  function isElementEnabled(element) {
    if (Boolean(element.disabled)) {
      return false;
    }
    return String(element.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
  }

  function elementByPath(path) {
    if (!path) {
      return null;
    }
    // Shadow-aware path: numeric tokens index into children; the literal token
    // "s" means the next numeric token indexes into the current element's open
    // shadowRoot.children. Pure-numeric (legacy light-DOM) paths still resolve.
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children[index] || null;
    }
    return current instanceof Element ? current : null;
  }

  function domPath(element) {
    const parts = [];
    let current = element;
    while (current && current instanceof Element && current !== document.documentElement) {
      const parent = current.parentElement;
      if (parent) {
        parts.unshift(Array.prototype.indexOf.call(parent.children, current));
        current = parent;
        continue;
      }
      // No element parent: if we are at the top of an open shadow tree, cross the
      // boundary via the host and emit an "s" marker so elementByPath can re-enter.
      const root = current.parentNode;
      if (root && root instanceof ShadowRoot) {
        parts.unshift(Array.prototype.indexOf.call(root.children, current));
        parts.unshift("s");
        current = root.host;
        continue;
      }
      break;
    }
    parts.unshift(0);
    return parts.join(".");
  }

  function tag(element) {
    return String(element?.tagName || "").toLowerCase();
  }

  function cssEscapeLocal(value) {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return String(value).replace(/["\\]/g, "\\$&");
  }

  function fail(errorCode, errorDetail, extra = {}) {
    return {
      ok: false,
      error_code: errorCode,
      error_detail: errorDetail,
      ...extra
    };
  }
}

async function setContentInPage(request) {
  const html = String((request && request.html) ?? "");
  const waitTimeoutMs = Number.isSafeInteger(request?.waitTimeoutMs)
    ? Math.max(1, Math.min(request.waitTimeoutMs, 30000))
    : 10000;
  if (html.length === 0) {
    return {
      ok: false,
      error_code: "CHROME_DOM_ACTION_UNSUPPORTED",
      error_detail: "setContent requires non-empty html"
    };
  }
  try {
    const waitForLoad = new Promise((resolve) => {
      let done = false;
      const finish = () => {
        if (done) {
          return;
        }
        done = true;
        setTimeout(resolve, 0);
      };
      window.addEventListener("load", finish, { once: true });
      setTimeout(finish, waitTimeoutMs);
    });
    document.open();
    document.write(html);
    document.close();
    if (document.readyState !== "complete") {
      await waitForLoad;
    } else {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    const historyLength = Number.isSafeInteger(history.length) ? history.length : 0;
    return {
      ok: true,
      html_len: html.length,
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || ""),
      history_current_index: -1,
      history_entry_count: historyLength,
      inline_script_marker: document.body?.getAttribute("data-inline-script") || null,
      load_event_marker: document.body?.getAttribute("data-load-event") || null,
      script_proof_present: Boolean(document.getElementById("issue1159-script-proof"))
    };
  } catch (error) {
    return {
      ok: false,
      error_code: "CHROME_SCRIPTING_EXECUTE_FAILED",
      error_detail: error && error.message ? String(error.message) : String(error)
    };
  }
}

function runClockInPage(request) {
  const VERSION = "synapse-clock-2026-06-21-v1";
  const operation = String(request?.operation || "status");
  const timeMs = request?.timeMs;
  const deltaMs = request?.deltaMs;
  const loopLimit = Number.isSafeInteger(request?.loopLimit) ? request.loopLimit : 10000;

  function errorDetail(error) {
    return error && error.stack ? String(error.stack) : String(error);
  }

  try {
    const clock = ensureSynapseClock();
    let readback;
    if (operation === "install") {
      readback = clock.call("install", { nowMs: timeMs, loopLimit });
    } else if (operation === "status") {
      readback = clock.call("status", {});
    } else if (operation === "setFixedTime") {
      readback = clock.call("setFixedTime", { timeMs });
    } else if (operation === "fastForward") {
      readback = clock.call("fastForward", { deltaMs });
    } else if (operation === "pauseAt") {
      readback = clock.call("pauseAt", { timeMs });
    } else {
      throw new Error("Unknown Synapse browser clock method: " + operation);
    }
    return {
      ok: true,
      init_script_identifier: "chrome.scripting.executeScript:MAIN:synapse-clock",
      init_script_newly_added: Boolean(clock.lastInstallNewlyAdded),
      installed_at_unix_ms: Number.isSafeInteger(clock.installedAtUnixMs) ? clock.installedAtUnixMs : 0,
      readback,
      url: String(location.href || ""),
      title: String(document.title || ""),
      ready_state: String(document.readyState || "")
    };
  } catch (error) {
    return {
      ok: false,
      error_code: "CHROME_CLOCK_FAILED",
      error_detail: errorDetail(error)
    };
  }

  function ensureSynapseClock() {
    if (globalThis.__synapseClock && globalThis.__synapseClock.version === VERSION) {
      return globalThis.__synapseClock;
    }

    const NativeDate = globalThis.Date;
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
      performanceOriginMs: NativeDate.now() - nativePerformanceNow(),
      installedAtUnixMs: 0
    };

    function toFiniteMs(value, fallback) {
      const n = Number(value);
      return Number.isFinite(n) ? Math.max(0, n) : fallback;
    }

    function timerDelay(value) {
      const n = Number(value);
      if (!Number.isFinite(n)) {
        return 0;
      }
      return Math.max(0, n);
    }

    function status() {
      let next = null;
      for (const timer of state.timers.values()) {
        if (next === null || timer.due < next) {
          next = timer.due;
        }
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
          if (
            timer.due <= target &&
            (next === null || timer.due < next.due || (timer.due === next.due && timer.id < next.id))
          ) {
            next = timer;
          }
        }
        if (!next) {
          break;
        }
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
      if (timer) {
        timer.cancelled = true;
      }
      state.timers.delete(Number(id));
    }

    function SynapseDate(...args) {
      if (this instanceof SynapseDate) {
        if (args.length === 0) {
          return new NativeDate(state.nowMs);
        }
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
      if (args && Object.prototype.hasOwnProperty.call(args, "nowMs") && args.nowMs !== null) {
        state.nowMs = toFiniteMs(args.nowMs, state.nowMs);
      }
      if (args && Object.prototype.hasOwnProperty.call(args, "loopLimit")) {
        state.loopLimit = Math.max(1, Math.floor(Number(args.loopLimit) || state.loopLimit));
      }
      api.lastInstallNewlyAdded = !state.installed;
      if (!state.installed) {
        globalThis.Date = SynapseDate;
        globalThis.setTimeout = (handler, delay, ...rest) => schedule("timeout", handler, delay, rest);
        globalThis.clearTimeout = clear;
        globalThis.setInterval = (handler, delay, ...rest) => schedule("interval", handler, delay, rest);
        globalThis.clearInterval = clear;
        globalThis.requestAnimationFrame = (handler) =>
          schedule("raf", (ts) => handler(ts), 16, [Math.max(0, state.nowMs - state.performanceOriginMs)]);
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
        state.installedAtUnixMs = NativeDate.now();
        api.installedAtUnixMs = state.installedAtUnixMs;
      }
      return status();
    }

    function setFixedTime(args) {
      if (!state.installed) {
        throw new Error("Synapse browser clock is not installed");
      }
      state.nowMs = toFiniteMs(args && args.timeMs, state.nowMs);
      return status();
    }

    function fastForward(args) {
      if (!state.installed) {
        throw new Error("Synapse browser clock is not installed");
      }
      const delta = toFiniteMs(args && args.deltaMs, 0);
      drainUntil(state.nowMs + delta);
      return status();
    }

    function pauseAt(args) {
      if (!state.installed) {
        throw new Error("Synapse browser clock is not installed");
      }
      const target = toFiniteMs(args && args.timeMs, state.nowMs);
      if (target < state.nowMs) {
        throw new Error("Synapse browser clock pauseAt cannot move backwards");
      }
      drainUntil(target);
      return status();
    }

    const api = {
      version: VERSION,
      installedAtUnixMs: 0,
      lastInstallNewlyAdded: false,
      call(method, args) {
        if (method === "install") {
          return install(args || {});
        }
        if (method === "status") {
          api.lastInstallNewlyAdded = false;
          return status();
        }
        if (method === "setFixedTime") {
          api.lastInstallNewlyAdded = false;
          return setFixedTime(args || {});
        }
        if (method === "fastForward") {
          api.lastInstallNewlyAdded = false;
          return fastForward(args || {});
        }
        if (method === "pauseAt") {
          api.lastInstallNewlyAdded = false;
          return pauseAt(args || {});
        }
        throw new Error("Unknown Synapse browser clock method: " + method);
      }
    };

    globalThis.__synapseClock = api;
    return api;
  }
}

function runPageEventsWorkerProbe() {
  const VERSION = "synapse-page-events-2026-06-22-v1";

  function workerUrl(scriptURL) {
    try {
      return new URL(String(scriptURL || ""), location.href).href;
    } catch (_) {
      return String(scriptURL || "");
    }
  }

  function nowMs() {
    return Date.now();
  }

  function ensureProbe() {
    if (globalThis.__synapsePageEvents && globalThis.__synapsePageEvents.version === VERSION) {
      return globalThis.__synapsePageEvents;
    }
    const state = {
      version: VERSION,
      nextWorkerId: 1,
      events: [],
      workers: new Map(),
      installedAtUnixMs: nowMs(),
      originalWorker: globalThis.Worker,
      originalSharedWorker: globalThis.SharedWorker,
      serviceWorkerContainer: navigator.serviceWorker || null,
      originalServiceWorkerRegister: navigator.serviceWorker?.register || null
    };

    function record(event) {
      const workerId = String(event.worker_id || "");
      const entry = {
        event_kind: String(event.event_kind || ""),
        worker_id: workerId || null,
        worker_type: event.worker_type == null ? null : String(event.worker_type),
        worker_url: event.worker_url == null ? null : String(event.worker_url),
        title: String(document.title || ""),
        url: String(location.href || ""),
        observed_at_unix_ms: nowMs()
      };
      if (!entry.event_kind || !entry.worker_id) {
        return;
      }
      state.events.push(entry);
      if (entry.event_kind === "worker_destroyed") {
        const existing = state.workers.get(workerId) || {};
        state.workers.set(workerId, {
          worker_id: workerId,
          worker_type: entry.worker_type || existing.worker_type || "",
          url: entry.worker_url || existing.url || "",
          title: entry.title || existing.title || "",
          attached: false,
          destroyed: true
        });
      } else {
        state.workers.set(workerId, {
          worker_id: workerId,
          worker_type: entry.worker_type || "",
          url: entry.worker_url || "",
          title: entry.title || "",
          attached: true,
          destroyed: false
        });
      }
    }

    function nextId(prefix) {
      const id = `${prefix}-${state.nextWorkerId}`;
      state.nextWorkerId += 1;
      return id;
    }

    function installWorkerShim() {
      const NativeWorker = state.originalWorker;
      if (typeof NativeWorker !== "function" || NativeWorker.__synapsePageEventsWrapped) {
        return;
      }
      function SynapseWorker(scriptURL, options) {
        const worker = Reflect.construct(
          NativeWorker,
          options === undefined ? [scriptURL] : [scriptURL, options],
          new.target || NativeWorker
        );
        const id = nextId("worker");
        const url = workerUrl(scriptURL);
        try {
          Object.defineProperty(worker, "__synapseWorkerId", {
            configurable: false,
            enumerable: false,
            value: id
          });
        } catch (_) {}
        record({
          event_kind: "worker_created",
          worker_id: id,
          worker_type: "worker",
          worker_url: url
        });
        const nativeTerminate = worker.terminate;
        if (typeof nativeTerminate === "function") {
          worker.terminate = function terminateWithSynapseReadback(...args) {
            record({
              event_kind: "worker_destroyed",
              worker_id: id,
              worker_type: "worker",
              worker_url: url
            });
            return nativeTerminate.apply(this, args);
          };
        }
        return worker;
      }
      Object.defineProperty(SynapseWorker, "__synapsePageEventsWrapped", {
        configurable: false,
        enumerable: false,
        value: true
      });
      Object.setPrototypeOf(SynapseWorker, NativeWorker);
      SynapseWorker.prototype = NativeWorker.prototype;
      globalThis.Worker = SynapseWorker;
    }

    function installSharedWorkerShim() {
      const NativeSharedWorker = state.originalSharedWorker;
      if (typeof NativeSharedWorker !== "function" || NativeSharedWorker.__synapsePageEventsWrapped) {
        return;
      }
      function SynapseSharedWorker(scriptURL, options) {
        const worker = Reflect.construct(
          NativeSharedWorker,
          options === undefined ? [scriptURL] : [scriptURL, options],
          new.target || NativeSharedWorker
        );
        record({
          event_kind: "worker_created",
          worker_id: nextId("shared-worker"),
          worker_type: "shared_worker",
          worker_url: workerUrl(scriptURL)
        });
        return worker;
      }
      Object.defineProperty(SynapseSharedWorker, "__synapsePageEventsWrapped", {
        configurable: false,
        enumerable: false,
        value: true
      });
      Object.setPrototypeOf(SynapseSharedWorker, NativeSharedWorker);
      SynapseSharedWorker.prototype = NativeSharedWorker.prototype;
      globalThis.SharedWorker = SynapseSharedWorker;
    }

    function installServiceWorkerShim() {
      const container = state.serviceWorkerContainer;
      const nativeRegister = state.originalServiceWorkerRegister;
      if (!container || typeof nativeRegister !== "function" || nativeRegister.__synapsePageEventsWrapped) {
        return;
      }
      const wrapped = async function registerWithSynapseReadback(scriptURL, options) {
        const registration = await nativeRegister.call(this, scriptURL, options);
        const script = registration?.active?.scriptURL ||
          registration?.installing?.scriptURL ||
          registration?.waiting?.scriptURL ||
          workerUrl(scriptURL);
        record({
          event_kind: "worker_created",
          worker_id: `service-worker:${script}`,
          worker_type: "service_worker",
          worker_url: script
        });
        return registration;
      };
      Object.defineProperty(wrapped, "__synapsePageEventsWrapped", {
        configurable: false,
        enumerable: false,
        value: true
      });
      try {
        Object.defineProperty(container, "register", {
          configurable: true,
          value: wrapped
        });
      } catch (_) {
        try {
          container.register = wrapped;
        } catch (_) {}
      }
    }

    state.drain = () => {
      const drained = state.events.splice(0, state.events.length);
      return {
        events: drained,
        workers: Array.from(state.workers.values())
      };
    };

    try {
      installWorkerShim();
    } catch (error) {
      state.events.push({
        event_kind: "worker_info_changed",
        worker_id: "worker-shim-install-error",
        worker_type: "worker",
        worker_url: String(error && error.message ? error.message : error),
        title: String(document.title || ""),
        url: String(location.href || ""),
        observed_at_unix_ms: nowMs()
      });
    }
    try {
      installSharedWorkerShim();
    } catch (_) {}
    try {
      installServiceWorkerShim();
    } catch (_) {}
    globalThis.__synapsePageEvents = state;
    return state;
  }

  const probe = ensureProbe();
  const drained = probe.drain();
  return {
    ok: true,
    version: VERSION,
    installed_at_unix_ms: probe.installedAtUnixMs,
    url: String(location.href || ""),
    title: String(document.title || ""),
    ready_state: String(document.readyState || ""),
    events: drained.events,
    workers: drained.workers
  };
}

function readViewportMetricsInPage() {
  const viewport = globalThis.visualViewport || null;
  return {
    inner_width: Math.round(globalThis.innerWidth || 0),
    inner_height: Math.round(globalThis.innerHeight || 0),
    device_pixel_ratio: Number(globalThis.devicePixelRatio || 0),
    screen_width: Math.round(globalThis.screen ? globalThis.screen.width || 0 : 0),
    screen_height: Math.round(globalThis.screen ? globalThis.screen.height || 0 : 0),
    outer_width: Math.round(globalThis.outerWidth || 0),
    outer_height: Math.round(globalThis.outerHeight || 0),
    scroll_width: Math.round(globalThis.document?.documentElement?.scrollWidth || 0),
    scroll_height: Math.round(globalThis.document?.documentElement?.scrollHeight || 0),
    visual_viewport_width: viewport ? Number(viewport.width) : null,
    visual_viewport_height: viewport ? Number(viewport.height) : null
  };
}

function readDeviceMetricsInPage() {
  function media(query) {
    try {
      return Boolean(globalThis.matchMedia && globalThis.matchMedia(query).matches);
    } catch (_) {
      return false;
    }
  }
  const viewport = globalThis.visualViewport || null;
  return {
    viewport: {
      inner_width: Math.round(globalThis.innerWidth || 0),
      inner_height: Math.round(globalThis.innerHeight || 0),
      device_pixel_ratio: Number(globalThis.devicePixelRatio || 0),
      screen_width: Math.round(globalThis.screen ? globalThis.screen.width || 0 : 0),
      screen_height: Math.round(globalThis.screen ? globalThis.screen.height || 0 : 0),
      outer_width: Math.round(globalThis.outerWidth || 0),
      outer_height: Math.round(globalThis.outerHeight || 0),
      scroll_width: Math.round(globalThis.document?.documentElement?.scrollWidth || 0),
      scroll_height: Math.round(globalThis.document?.documentElement?.scrollHeight || 0),
      visual_viewport_width: viewport ? Number(viewport.width) : null,
      visual_viewport_height: viewport ? Number(viewport.height) : null
    },
    user_agent: String(globalThis.navigator ? globalThis.navigator.userAgent || "" : ""),
    max_touch_points: Number(globalThis.navigator ? globalThis.navigator.maxTouchPoints || 0 : 0),
    ontouchstart_available: Boolean("ontouchstart" in globalThis),
    pointer_coarse: media("(pointer: coarse)"),
    any_pointer_coarse: media("(any-pointer: coarse)"),
    hover_none: media("(hover: none)"),
    any_hover_none: media("(any-hover: none)")
  };
}

function readPageVitalsInPage() {
  function stringValue(value) {
    return value === null || value === undefined ? "" : String(value);
  }
  function numberValue(value) {
    return typeof value === "number" && Number.isFinite(value) ? value : 0;
  }
  function trimText(value, maxChars) {
    const normalized = stringValue(value).replace(/\s+/g, " ").trim();
    return Array.from(normalized).slice(0, maxChars).join("");
  }
  function cssEscape(value) {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return stringValue(value).replace(/["\\#.:,[\]>+~*'()=]/g, "\\$&");
  }
  function elementSelector(element) {
    if (!element || element.nodeType !== 1) {
      return null;
    }
    const parts = [];
    let current = element;
    while (current && current.nodeType === 1 && parts.length < 6) {
      const local = stringValue(current.localName || current.tagName).toLowerCase();
      let part = local || "element";
      if (current.id) {
        part += "#" + cssEscape(current.id);
        parts.unshift(part);
        break;
      }
      const classes = Array.from(current.classList || []).slice(0, 2);
      for (const className of classes) {
        part += "." + cssEscape(className);
      }
      const parent = current.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children || []).filter((node) => node.localName === current.localName);
        if (siblings.length > 1) {
          part += `:nth-of-type(${siblings.indexOf(current) + 1})`;
        }
      }
      parts.unshift(part);
      current = parent;
    }
    return parts.join(" > ");
  }
  function lcpSummary(entry) {
    const element = entry && entry.element && entry.element.nodeType === 1 ? entry.element : null;
    return {
      name: stringValue(entry && entry.name),
      entry_type: stringValue(entry && entry.entryType) || "largest-contentful-paint",
      start_time: numberValue(entry && entry.startTime),
      render_time: numberValue(entry && entry.renderTime),
      load_time: numberValue(entry && entry.loadTime),
      size: numberValue(entry && entry.size),
      element_tag_name: element ? stringValue(element.tagName).toLowerCase() : null,
      element_id: element ? stringValue(element.id) : null,
      element_class_name: element ? stringValue(element.className) : null,
      element_selector: element ? elementSelector(element) : null,
      element_text: element ? trimText(element.innerText || element.textContent || "", 2048) : null,
      element_current_src: element && "currentSrc" in element ? stringValue(element.currentSrc) : null,
      element_url: element && "url" in entry ? stringValue(entry.url) : null
    };
  }
  try {
    const supported = Boolean(
      typeof PerformanceObserver !== "undefined" &&
        Array.isArray(PerformanceObserver.supportedEntryTypes) &&
        PerformanceObserver.supportedEntryTypes.includes("largest-contentful-paint")
    );
    const entries = supported && performance && typeof performance.getEntriesByType === "function"
      ? performance.getEntriesByType("largest-contentful-paint")
      : [];
    const last = entries.length > 0 ? entries[entries.length - 1] : null;
    return {
      available: true,
      readback_source: "PerformanceObserver.supportedEntryTypes+performance.getEntriesByType",
      visibility_state: stringValue(document.visibilityState),
      document_hidden: Boolean(document.hidden),
      ready_state: stringValue(document.readyState),
      lcp_supported: supported,
      lcp_entry_count: entries.length,
      lcp: last ? lcpSummary(last) : null,
      error_code: null,
      error_detail: null
    };
  } catch (error) {
    return {
      available: false,
      readback_source: "PerformanceObserver.supportedEntryTypes+performance.getEntriesByType",
      visibility_state: typeof document !== "undefined" ? stringValue(document.visibilityState) : null,
      document_hidden: typeof document !== "undefined" ? Boolean(document.hidden) : null,
      ready_state: typeof document !== "undefined" ? stringValue(document.readyState) : null,
      lcp_supported: false,
      lcp_entry_count: 0,
      lcp: null,
      error_code: "PAGE_VITALS_READ_FAILED",
      error_detail: error && error.message ? String(error.message) : String(error)
    };
  }
}

function readActiveElementInPage() {
  // Descend into open shadow roots: document.activeElement reports the shadow
  // HOST, not the focused element inside it (#1335). Walk to the innermost
  // focused node so readback reflects a focused shadow-DOM editor.
  let element = document.activeElement;
  while (element && element.shadowRoot && element.shadowRoot.activeElement) {
    element = element.shadowRoot.activeElement;
  }
  if (!element) {
    return {
      has_active_element: false,
      is_editable: false,
      tag_name: "",
      id: "",
      name: "",
      value: null,
      selected_text: null
    };
  }
  const tagName = String(element.tagName || "");
  const lowerTag = tagName.toLowerCase();
  const contentEditable =
    element.isContentEditable ||
    String(element.getAttribute?.("contenteditable") || "").toLowerCase() === "true";
  const valueCapable = "value" in element;
  const isEditable =
    contentEditable ||
    lowerTag === "textarea" ||
    lowerTag === "select" ||
    (lowerTag === "input" && !["button", "checkbox", "color", "file", "hidden", "image", "radio", "range", "reset", "submit"].includes(String(element.type || "").toLowerCase()));
  let value = null;
  if (valueCapable) {
    value = String(element.value ?? "");
  } else if (contentEditable) {
    value = readContentEditableValue(element);
  }
  let selectedText = null;
  try {
    if (typeof element.selectionStart === "number" && typeof element.selectionEnd === "number" && value !== null) {
      selectedText = String(value).slice(element.selectionStart, element.selectionEnd);
    } else {
      const selection = document.getSelection?.();
      if (selection && selection.rangeCount > 0) {
        selectedText = String(selection.toString() || "");
      }
    }
  } catch (_) {
    selectedText = null;
  }
  return {
    has_active_element: true,
    is_editable: Boolean(isEditable),
    tag_name: tagName,
    id: String(element.id || ""),
    name: String(element.getAttribute?.("name") || ""),
    value,
    selected_text: selectedText
  };

  function readContentEditableValue(el) {
    const textContent = String(el.textContent ?? "");
    if (textContent.length === 0) {
      return "";
    }
    return String(el.innerText || textContent || "");
  }
}

function typeActiveElementInPage(text) {
  let element = document.activeElement;
  while (element && element.shadowRoot && element.shadowRoot.activeElement) {
    element = element.shadowRoot.activeElement;
  }
  const before = readActiveElementLocal();
  if (!element || !before.has_active_element) {
    return {
      ok: false,
      error_code: "CHROME_ACTIVE_ELEMENT_MISSING",
      error_detail: "document.activeElement is absent",
      before_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...before }
    };
  }
  if (!before.is_editable) {
    return {
      ok: false,
      error_code: "CHROME_ACTIVE_ELEMENT_NOT_EDITABLE",
      error_detail: `active element ${String(before.tag_name || "")} is not editable`,
      before_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...before }
    };
  }

  const inputText = String(text ?? "");
  const eventsDispatched = [];
  const beforeInput = dispatchSyntheticInputEventLocal(element, "beforeinput", inputText, true);
  eventsDispatched.push("beforeinput");
  if (!beforeInput) {
    return {
      ok: false,
      error_code: "CHROME_BEFOREINPUT_CANCELLED",
      error_detail: "active element beforeinput listener cancelled background typing",
      before_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...before },
      events_dispatched: eventsDispatched
    };
  }

  const expectedValue = applyTextToEditableLocal(element, inputText);
  dispatchSyntheticInputEventLocal(element, "input", inputText, false);
  eventsDispatched.push("input");
  element.dispatchEvent(new Event("change", { bubbles: true }));
  eventsDispatched.push("change");

  const after = readActiveElementLocal();
  const afterValue = after.value === null || after.value === undefined ? "" : String(after.value);
  if (afterValue !== expectedValue) {
    return {
      ok: false,
      error_code: "CHROME_ACTIVE_ELEMENT_VALUE_MISMATCH",
      error_detail: `after value did not match expected value; expected_len=${expectedValue.length} after_len=${afterValue.length}`,
      before_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...before },
      after_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...after },
      expected_value: expectedValue,
      events_dispatched: eventsDispatched
    };
  }

  return {
    ok: true,
    before_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...before },
    after_active_element: { available: true, readback_source: "chrome.scripting.executeScript", ...after },
    expected_value: expectedValue,
    events_dispatched: eventsDispatched
  };

  function readActiveElementLocal() {
    let active = document.activeElement;
    while (active && active.shadowRoot && active.shadowRoot.activeElement) {
      active = active.shadowRoot.activeElement;
    }
    if (!active) {
      return {
        has_active_element: false,
        is_editable: false,
        tag_name: "",
        id: "",
        name: "",
        value: null,
        selected_text: null
      };
    }
    const tagName = String(active.tagName || "");
    const lowerTag = tagName.toLowerCase();
    const contentEditable =
      active.isContentEditable ||
      String(active.getAttribute?.("contenteditable") || "").toLowerCase() === "true";
    const valueCapable = "value" in active;
    const isEditable =
      contentEditable ||
      lowerTag === "textarea" ||
      lowerTag === "select" ||
      (lowerTag === "input" && !["button", "checkbox", "color", "file", "hidden", "image", "radio", "range", "reset", "submit"].includes(String(active.type || "").toLowerCase()));
    let value = null;
    if (valueCapable) {
      value = String(active.value ?? "");
    } else if (contentEditable) {
      value = readContentEditableValue(active);
    }
    let selectedText = null;
    try {
      if (typeof active.selectionStart === "number" && typeof active.selectionEnd === "number" && value !== null) {
        selectedText = String(value).slice(active.selectionStart, active.selectionEnd);
      } else {
        const selection = document.getSelection?.();
        if (selection && selection.rangeCount > 0) {
          selectedText = String(selection.toString() || "");
        }
      }
    } catch (_) {
      selectedText = null;
    }
    return {
      has_active_element: true,
      is_editable: Boolean(isEditable),
      tag_name: tagName,
      id: String(active.id || ""),
      name: String(active.getAttribute?.("name") || ""),
      value,
      selected_text: selectedText
    };
  }

  function applyTextToEditableLocal(active, inputText) {
    const tagName = String(active.tagName || "").toLowerCase();
    const contentEditable =
      active.isContentEditable ||
      String(active.getAttribute?.("contenteditable") || "").toLowerCase() === "true";
    if ((tagName === "input" || tagName === "textarea") && "value" in active) {
      const beforeValue = String(active.value ?? "");
      const start = typeof active.selectionStart === "number" ? active.selectionStart : beforeValue.length;
      const end = typeof active.selectionEnd === "number" ? active.selectionEnd : start;
      const expected = beforeValue.slice(0, start) + inputText + beforeValue.slice(end);
      // Use the native prototype value setter (not `active.value = …`) so
      // controlled React/Vue/Angular inputs do not revert the write: a plain
      // assignment updates the framework's cached value tracker, so the input
      // event reports "no change" and the next render restores the old state.
      // The prototype setter bypasses that tracker — the same react-safe path
      // setFieldValueInPage uses (#1348). The caller's post-set readback still
      // fails loud (CHROME_ACTIVE_ELEMENT_VALUE_MISMATCH) if a page reverts it.
      const proto =
        tagName === "input" ? window.HTMLInputElement.prototype : window.HTMLTextAreaElement.prototype;
      const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
      if (descriptor && typeof descriptor.set === "function") {
        descriptor.set.call(active, expected);
      } else {
        active.value = expected;
      }
      if (typeof active.setSelectionRange === "function") {
        const caret = start + inputText.length;
        active.setSelectionRange(caret, caret);
      }
      return expected;
    }
    if (contentEditable) {
      const selection = document.getSelection?.();
      if (selection && selection.rangeCount > 0 && active.contains(selection.anchorNode)) {
        const range = selection.getRangeAt(0);
        range.deleteContents();
        const textNode = document.createTextNode(inputText);
        range.insertNode(textNode);
        range.setStartAfter(textNode);
        range.setEndAfter(textNode);
        selection.removeAllRanges();
        selection.addRange(range);
      } else {
        active.appendChild(document.createTextNode(inputText));
      }
      return readContentEditableValue(active);
    }
    throw new Error(`active element ${tagName || "unknown"} is not text editable`);
  }

  function readContentEditableValue(active) {
    const textContent = String(active.textContent ?? "");
    if (textContent.length === 0) {
      return "";
    }
    return String(active.innerText || textContent || "");
  }

  function dispatchSyntheticInputEventLocal(active, type, inputText, cancelable) {
    let event;
    try {
      event = new InputEvent(type, {
        bubbles: true,
        cancelable,
        data: inputText,
        inputType: "insertText"
      });
    } catch (_) {
      event = new Event(type, { bubbles: true, cancelable });
    }
    return active.dispatchEvent(event);
  }
}

// #1000/#717: background-safe field REPLACE for the user's normal Chrome. Runs
// entirely in-page via chrome.scripting (no debugger attach, no OS foreground,
// works on occluded/inactive tabs that UIA cannot see). Resolves the target by
// a strict CSS selector (exactly one editable, visible match), a normal Chrome
// bridge element id, or the current active element, then replaces the value
// with the NATIVE prototype setter so React/Vue/Angular controlled inputs do
// not silently revert the change.
async function handleSetFieldValue(params) {
  const selected = await selectTabTarget(params, { requireTargetId: true });
  const text = String(params.text ?? "");
  const selector = stringOrNull(params.selector);
  const elementId = stringOrNull(params.elementId);
  const activeElement = Boolean(params.activeElement);
  const locatorCount = (selector ? 1 : 0) + (elementId ? 1 : 0) + (activeElement ? 1 : 0);
  if (locatorCount !== 1) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "setFieldValue requires exactly one of selector, elementId, or activeElement=true"
    );
  }
  const parsedElement = elementId
    ? parseChromeBridgeElementId(elementId, selected.tabId, "setFieldValue")
    : null;
  const scriptTarget = parsedElement
    ? { tabId: selected.tabId, frameIds: [parsedElement.frameId] }
    : { tabId: selected.tabId, allFrames: true };
  const scriptArgs = {
    selector,
    elementId,
    elementPath: parsedElement ? parsedElement.path : null,
    activeElement,
    text
  };
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.scripting.executeScript unavailable; extension is missing scripting permission"
    );
  }
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: scriptTarget,
      func: setFieldValueInPage,
      args: [{ ...scriptArgs, resolveOnly: true }]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.scripting.executeScript setFieldValue(${selected.tabId}) resolve failed: ${errorMessage(error)}`
    );
  }
  const frameResults = frameExecutionResults(results);
  const okFrames = frameResults.filter((frame) => frame.result && frame.result.ok);
  const totalMatches = frameResults.reduce((sum, frame) => sum + frameResolvedMatchCount(frame), 0);
  if (okFrames.length > 1 || totalMatches > 1) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `setFieldValue matched ${totalMatches} editable element(s) across ${frameResults.length} frame(s); ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  const first = okFrames[0] || null;
  if (!first || !first.result || typeof first.result !== "object") {
    const failed = frameResults.find((frame) => frame.result)?.result;
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `setFieldValue failed across ${frameResults.length} frame(s): code=${String(failed?.error_code || "UNKNOWN")} ` +
        `detail=${String(failed?.error_detail || "no matching frame result")}; frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  if (!Number.isSafeInteger(first.frame_id)) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `setFieldValue resolved a frame without a targetable frame_id; ` +
        `frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  if (String(first.result.editable_kind || "") === "contenteditable") {
    const before = await tabPageState(selected.tabId, selected.target);
    const beforePageText = await tabPageTextState(selected.tabId);
    return await handleCdpInputText(
      selected,
      "set_text",
      {
        selector,
        elementId,
        activeElement,
        text
      },
      before,
      beforePageText,
      5000,
      true,
      5000
    );
  }

  let actionResults;
  try {
    actionResults = await chrome.scripting.executeScript({
      target: { tabId: selected.tabId, frameIds: [first.frame_id] },
      func: setFieldValueInPage,
      args: [scriptArgs]
    });
  } catch (error) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `chrome.scripting.executeScript setFieldValue(${selected.tabId}, frame=${first.frame_id}) failed: ${errorMessage(error)}`
    );
  }
  const actionFrameResults = frameExecutionResults(actionResults);
  const actionFrame = actionFrameResults.find((frame) => frame.result) || null;
  const result = actionFrame?.result;
  if (!result || typeof result !== "object") {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      "chrome.scripting.executeScript setFieldValue returned no structured action result"
    );
  }
  if (!result.ok) {
    throw bridgeError(
      ERROR_AXTREE_FAILED,
      `setFieldValue failed in resolved frame ${first.frame_id}: code=${String(result.error_code || "UNKNOWN")} ` +
        `detail=${String(result.error_detail || "")}; frame_results=${JSON.stringify(frameResults.map(summarizeFrameExecutionResult).slice(0, 8))}`
    );
  }
  return {
    extension_id: chrome.runtime.id,
    target_id: selected.target.id,
    tab_id: selected.tabId,
    chrome_window_id: selected.target.chromeWindowId,
    chars_requested: [...text].length,
    readback_backend: "chrome.scripting.executeScript",
    frame_id: first.frame_id,
    frame_document_id: first.document_id,
    frame_result_count: frameResults.length,
    frame_results: frameResults.map(summarizeFrameExecutionResult),
    target_candidate_count: selected.targetCandidateCount,
    target_selection_reason: selected.selectionReason,
    ...result
  };
}

function setFieldValueInPage(request) {
  const selector = request && request.selector != null ? String(request.selector).trim() : "";
  const elementPath = request && request.elementPath != null ? String(request.elementPath).trim() : "";
  const wantActive = Boolean(request && request.activeElement);
  const inputText = String((request && request.text) ?? "");
  const resolveOnly = Boolean(request && request.resolveOnly);
  const locatorCount = (selector ? 1 : 0) + (elementPath ? 1 : 0) + (wantActive ? 1 : 0);

  function elementByPath(path) {
    if (!path) {
      return null;
    }
    // Shadow-aware: "s" token re-enters the current element's open shadowRoot.
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children ? scope.children[index] : null;
    }
    return current instanceof Element ? current : null;
  }

  function deepQueryAllLocal(sel) {
    // Shadow-piercing querySelectorAll union across the document and every open
    // shadow root. Throws on an invalid selector.
    const out = [];
    const seen = new Set();
    const scopes = [document];
    const stack = [document];
    while (stack.length) {
      const node = stack.pop();
      const all = node.querySelectorAll ? node.querySelectorAll("*") : [];
      for (const el of all) {
        if (el.shadowRoot) {
          scopes.push(el.shadowRoot);
          stack.push(el.shadowRoot);
        }
      }
    }
    for (const scope of scopes) {
      const matches = scope.querySelectorAll(sel);
      for (const el of matches) {
        if (!seen.has(el)) {
          seen.add(el);
          out.push(el);
        }
      }
    }
    return out;
  }

  function editableKind(el) {
    if (!el || el.nodeType !== 1) {
      return null;
    }
    if (el.disabled || el.readOnly) {
      return null;
    }
    const tag = String(el.tagName || "").toLowerCase();
    if (tag === "textarea") {
      return "value";
    }
    if (tag === "input") {
      const type = String(el.getAttribute("type") || "text").toLowerCase();
      const nonText = ["button", "submit", "reset", "checkbox", "radio", "range", "color", "file", "image", "hidden"];
      return nonText.includes(type) ? null : "value";
    }
    return (
      el.isContentEditable ||
        String(el.getAttribute && el.getAttribute("contenteditable") || "").toLowerCase() === "true"
    ) ? "contenteditable" : null;
  }
  function isEditable(el) {
    return Boolean(editableKind(el));
  }
  function isVisible(el) {
    if (!el || typeof el.getClientRects !== "function") {
      return false;
    }
    if (el.getClientRects().length > 0) {
      return true;
    }
    // Occluded windows still lay out the DOM; fall back to box model.
    return Boolean(el.offsetWidth || el.offsetHeight);
  }
  function readValue(el) {
    const tag = String(el.tagName || "").toLowerCase();
    if ((tag === "input" || tag === "textarea") && "value" in el) {
      return String(el.value ?? "");
    }
    if (
      el.isContentEditable ||
      String((el.getAttribute && el.getAttribute("contenteditable")) || "").toLowerCase() === "true"
    ) {
      return readContentEditableValue(el);
    }
    return String(el.innerText || el.textContent || "");
  }

  function readContentEditableValue(el) {
    const textContent = String(el.textContent ?? "");
    if (textContent.length === 0) {
      return "";
    }
    return String(el.innerText || textContent || "");
  }

  let element = null;
  let resolvedBy = "";
  let matchCount = 0;
  if (locatorCount !== 1) {
    return {
      ok: false,
      error_code: "CHROME_SET_FIELD_BAD_LOCATOR",
      error_detail: "setFieldValue requires exactly one of selector, element_id, or active_element=true"
    };
  }
  if (selector) {
    let nodes;
    try {
      nodes = deepQueryAllLocal(selector);
    } catch (error) {
      return {
        ok: false,
        error_code: "CHROME_SET_FIELD_SELECTOR_INVALID",
        error_detail: `querySelectorAll(${selector}) threw: ${String(error && error.message || error)}`
      };
    }
    const editable = nodes.filter((node) => isEditable(node) && isVisible(node));
    matchCount = editable.length;
    if (editable.length === 0) {
      return {
        ok: false,
        error_code: "CHROME_SET_FIELD_NOT_FOUND",
        error_detail: `selector matched ${nodes.length} node(s), 0 editable+visible`,
        selector_match_count: nodes.length
      };
    }
    if (editable.length > 1) {
      return {
        ok: false,
        error_code: "CHROME_SET_FIELD_NOT_UNIQUE",
        error_detail: `selector matched ${editable.length} editable+visible nodes; refine the selector for a unique target`,
        editable_match_count: editable.length
      };
    }
    element = editable[0];
    resolvedBy = "selector";
  } else if (elementPath) {
    element = elementByPath(elementPath);
    resolvedBy = "element_id";
    matchCount = element ? 1 : 0;
    if (!element) {
      return {
        ok: false,
        error_code: "CHROME_SET_FIELD_NOT_FOUND",
        error_detail: `bridge element path ${elementPath} did not resolve in this frame`,
        element_path: elementPath
      };
    }
  } else if (wantActive) {
    element = document.activeElement;
    resolvedBy = "active_element";
    matchCount = element ? 1 : 0;
  } else {
    return {
      ok: false,
      error_code: "CHROME_SET_FIELD_BAD_LOCATOR",
      error_detail: "setFieldValue requires exactly one of selector, element_id, or active_element=true"
    };
  }

  if (!element) {
    return { ok: false, error_code: "CHROME_ACTIVE_ELEMENT_MISSING", error_detail: "no element resolved" };
  }
  if (!isEditable(element)) {
    return {
      ok: false,
      error_code: "CHROME_ACTIVE_ELEMENT_NOT_EDITABLE",
      error_detail: `resolved element ${String(element.tagName || "")} is not an editable field`
    };
  }

  const beforeValue = readValue(element);
  const tag = String(element.tagName || "").toLowerCase();
  const kind = editableKind(element);
  if (resolveOnly) {
    return {
      ok: true,
      resolved_by: resolvedBy,
      match_count: matchCount,
      tag_name: tag,
      editable_kind: kind,
      is_editable: true,
      before_value: beforeValue
    };
  }
  try {
    element.focus();
  } catch (_) {
    // focus failures are not fatal; value mutation + readback is the source of truth
  }

  // beforeinput is cancelable; a page handler may legitimately block input.
  let beforeInputEvent;
  try {
    beforeInputEvent = new InputEvent("beforeinput", {
      bubbles: true,
      cancelable: true,
      data: inputText,
      inputType: "insertReplacementText"
    });
  } catch (_) {
    beforeInputEvent = new Event("beforeinput", { bubbles: true, cancelable: true });
  }
  const beforeInputOk = element.dispatchEvent(beforeInputEvent);
  if (!beforeInputOk) {
    return {
      ok: false,
      error_code: "CHROME_BEFOREINPUT_CANCELLED",
      error_detail: "a page beforeinput listener cancelled the background replace",
      before_value: beforeValue
    };
  }

  let expected;
  if ((tag === "input" || tag === "textarea") && "value" in element) {
    const proto = tag === "input" ? window.HTMLInputElement.prototype : window.HTMLTextAreaElement.prototype;
    const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
    if (descriptor && typeof descriptor.set === "function") {
      descriptor.set.call(element, inputText);
    } else {
      element.value = inputText;
    }
    if (typeof element.setSelectionRange === "function") {
      try {
        element.setSelectionRange(inputText.length, inputText.length);
      } catch (_) {
        // selection is best-effort; ignore on inputs that disallow it
      }
    }
    expected = String(element.value ?? "");
  } else {
    element.textContent = inputText;
    expected = readValue(element);
  }

  let inputEvent;
  try {
    inputEvent = new InputEvent("input", { bubbles: true, cancelable: false, data: inputText, inputType: "insertReplacementText" });
  } catch (_) {
    inputEvent = new Event("input", { bubbles: true });
  }
  element.dispatchEvent(inputEvent);
  element.dispatchEvent(new Event("change", { bubbles: true }));

  const afterValue = readValue(element);
  if (afterValue !== expected) {
    return {
      ok: false,
      error_code: "CHROME_SET_FIELD_VALUE_MISMATCH",
      error_detail: `post-set readback diverged from the applied value (expected_len=${expected.length} after_len=${afterValue.length}); a page framework may have reverted it`,
      resolved_by: resolvedBy,
      match_count: matchCount,
      tag_name: tag,
      before_value: beforeValue,
      after_value: afterValue,
      expected_value: expected
    };
  }

  return {
    ok: true,
    resolved_by: resolvedBy,
    match_count: matchCount,
    tag_name: tag,
    editable_kind: kind,
    is_editable: true,
    before_value: beforeValue,
    after_value: afterValue,
    expected_value: expected
  };
}

function normalizeDomAction(action) {
  const normalized = String(action || "").trim().toLowerCase();
  if (normalized === "dispatchevent" || normalized === "dispatch-event") {
    return "dispatch_event";
  }
  if (normalized === "selecttext" || normalized === "select-text") {
    return "select_text";
  }
  if (["click", "dblclick", "press", "select", "submit", "dispatch_event", "clear", "focus", "blur", "select_text", "check", "uncheck"].includes(normalized)) {
    return normalized;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `domAction action must be one of click, dblclick, press, select, submit, dispatch_event, clear, focus, blur, select_text, check, uncheck; got ${JSON.stringify(action)}`
  );
}

function stringOrNull(value) {
  if (value === null || value === undefined) {
    return null;
  }
  const text = String(value).trim();
  return text ? text : null;
}

function plainObjectOrEmpty(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return {};
  }
  return value;
}

function normalizeAriaMaxNodes(value) {
  if (value === undefined || value === null) {
    return 500;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > 5000) {
    throw bridgeError(ERROR_ATTACH_FAILED, "ariaSnapshot maxNodes must be an integer from 1 through 5000");
  }
  return number;
}

function normalizeAriaMaxDepth(value) {
  if (value === undefined || value === null) {
    return 32;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > 128) {
    throw bridgeError(ERROR_ATTACH_FAILED, "ariaSnapshot maxDepth must be an integer from 1 through 128");
  }
  return number;
}

function normalizeAssertLimit(value) {
  if (value === undefined || value === null) {
    return 2;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > 500) {
    throw bridgeError(ERROR_ATTACH_FAILED, "assertPoll limit must be an integer from 1 through 500");
  }
  return number;
}

function normalizeInspectMaxHtmlBytes(value) {
  if (value === undefined || value === null) {
    return 256 * 1024;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0 || number > 2 * 1024 * 1024) {
    throw bridgeError(ERROR_ATTACH_FAILED, "inspectElement maxHtmlBytes must be an integer from 0 through 2097152");
  }
  return number;
}

function parseChromeBridgeElementId(value, expectedTabId, commandName) {
  const raw = stringOrNull(value);
  if (!raw) {
    return { raw: null, frameId: null, path: null };
  }
  // Path segments are numeric child indices or the literal "s" shadow-host-hop
  // token (#1335), e.g. 0.1.1.s.0; light-DOM paths stay all-numeric.
  const match = /^chrome-tab:(\d+):frame:(\d+):path:((?:s|\d+)(?:\.(?:s|\d+))*)$/.exec(raw);
  if (!match) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `${commandName} root element id must be a normal bridge element id like chrome-tab:<tabId>:frame:<frameId>:path:<domPath>; got ${JSON.stringify(raw)}`
    );
  }
  const tabId = Number(match[1]);
  const frameId = Number(match[2]);
  if (tabId !== expectedTabId) {
    throw bridgeError(
      ERROR_ATTACH_FAILED,
      `${commandName} root element id tab ${tabId} does not match selected tab ${expectedTabId}`
    );
  }
  return { raw, frameId, path: match[3] };
}

function chromeBridgeElementId(tabId, frameId, localPath) {
  return `chrome-tab:${tabId}:frame:${frameId}:path:${String(localPath || "0")}`;
}

function prefixAriaSnapshotNode(node, tabId, frameId) {
  const localId = String(node?.element_id || "0");
  const localParent = node?.parent_element_id == null ? null : String(node.parent_element_id);
  return {
    element_id: chromeBridgeElementId(tabId, frameId, localId),
    parent_element_id: localParent ? chromeBridgeElementId(tabId, frameId, localParent) : null,
    depth: Number.isSafeInteger(node?.depth) ? node.depth : 0,
    role: String(node?.role || "generic"),
    name: String(node?.name || ""),
    value: node?.value == null ? null : String(node.value),
    enabled: node?.enabled !== false,
    focused: Boolean(node?.focused),
    children_count: Number.isSafeInteger(node?.children_count) ? node.children_count : 0
  };
}

function renderPrefixedAriaSnapshot(nodes) {
  return nodes.map((node) => {
    let line = `${"  ".repeat(Math.max(0, node.depth))}- ${node.role || "generic"}`;
    if (node.name) {
      line += ` "${escapeSnapshotScalar(node.name)}"`;
    }
    if (node.value) {
      line += `: "${escapeSnapshotScalar(node.value)}"`;
    }
    if (!node.enabled) {
      line += " [disabled]";
    }
    if (node.focused) {
      line += " [focused]";
    }
    return line;
  }).join("\n");
}

function escapeSnapshotScalar(value) {
  return String(value || "").replace(/\\/g, "\\\\").replace(/"/g, "\\\"");
}

function normalizeClickCount(value, action = "click") {
  if (value === null || value === undefined) {
    return action === "dblclick" ? 2 : 1;
  }
  const clicks = Number(value);
  if (!Number.isSafeInteger(clicks) || clicks < 1 || clicks > 3) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction clicks must be an integer in 1..=3; got ${JSON.stringify(value)}`
    );
  }
  if (action === "dblclick" && clicks !== 2) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction dblclick requires clicks/clickCount to be omitted or 2; got ${JSON.stringify(value)}`
    );
  }
  return clicks;
}

function normalizeMouseButton(value, owner = "domAction") {
  if (value === null || value === undefined || String(value).trim() === "") {
    return "left";
  }
  const normalized = String(value).trim().toLowerCase();
  if (["left", "right", "middle"].includes(normalized)) {
    return normalized;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `${owner} button must be one of left, right, middle; got ${JSON.stringify(value)}`
  );
}

function normalizeClickModifiers(value, owner = "domAction") {
  if (value === null || value === undefined) {
    return [];
  }
  if (!Array.isArray(value)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `${owner} modifiers must be an array of ctrl/shift/alt/meta strings; got ${JSON.stringify(value)}`
    );
  }
  const seen = new Set();
  for (const item of value) {
    const normalized = String(item || "").trim().toLowerCase();
    let canonical = null;
    if (normalized === "ctrl" || normalized === "control") {
      canonical = "ctrl";
    } else if (normalized === "shift") {
      canonical = "shift";
    } else if (normalized === "alt" || normalized === "option") {
      canonical = "alt";
    } else if (["meta", "super", "cmd", "command"].includes(normalized)) {
      canonical = "meta";
    }
    if (!canonical) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `${owner} modifier must be ctrl, shift, alt, or meta; got ${JSON.stringify(item)}`
      );
    }
    seen.add(canonical);
  }
  return ["alt", "ctrl", "meta", "shift"].filter((modifier) => seen.has(modifier));
}

function normalizeClickPosition(params, owner = "domAction") {
  const nested = params?.position && typeof params.position === "object" && !Array.isArray(params.position)
    ? params.position
    : null;
  const rawX = params?.positionX ?? params?.offsetX ?? nested?.x;
  const rawY = params?.positionY ?? params?.offsetY ?? nested?.y;
  if (rawX === null || rawX === undefined || rawY === null || rawY === undefined) {
    if (rawX === null || rawX === undefined) {
      if (rawY === null || rawY === undefined) {
        return null;
      }
    }
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `${owner} position requires both x and y; got x=${JSON.stringify(rawX)} y=${JSON.stringify(rawY)}`
    );
  }
  const x = Number(rawX);
  const y = Number(rawY);
  if (!Number.isSafeInteger(x) || !Number.isSafeInteger(y) || x < 0 || y < 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `${owner} position must use non-negative safe-integer CSS pixels; got x=${JSON.stringify(rawX)} y=${JSON.stringify(rawY)}`
    );
  }
  return { x, y };
}

function normalizeEventType(value) {
  const eventType = String(value || "").trim();
  if (!eventType) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction dispatch_event requires non-empty eventType; got ${JSON.stringify(value)}`
    );
  }
  if ([...eventType].some((ch) => ch < " " || ch === "\u007f")) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction dispatch_event eventType contains control characters: ${JSON.stringify(value)}`
    );
  }
  return eventType;
}

function normalizeEventInit(value) {
  if (value === null || value === undefined) {
    return {};
  }
  // Some MCP transports stringify nested object params, so a bubbling EventInit
  // can arrive as a JSON-encoded object string. Parse it back before validating
  // so callers can forge e.g. {"bubbles":true} input events (#1347).
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "") {
      return {};
    }
    let parsed;
    try {
      parsed = JSON.parse(trimmed);
    } catch (error) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `domAction dispatch_event eventInit string was not valid JSON: ${String(error && error.message ? error.message : error)}`
      );
    }
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `domAction dispatch_event eventInit string must encode a JSON object; got ${JSON.stringify(parsed)}`
      );
    }
    return parsed;
  }
  if (typeof value !== "object" || Array.isArray(value)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction dispatch_event eventInit must be a JSON object; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeInteger(value, fieldName) {
  const number = Number(value);
  if (!Number.isSafeInteger(number)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction ${fieldName} must be a safe integer; got ${JSON.stringify(value)}`
    );
  }
  return number;
}

function normalizeSelectOptions(value) {
  if (value === null || value === undefined) {
    return [];
  }
  if (!Array.isArray(value)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `domAction select options must be an array of {value,label,index}; got ${JSON.stringify(value)}`
    );
  }
  return value.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw bridgeError(
        ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
        `domAction select options[${index}] must be an object; got ${JSON.stringify(item)}`
      );
    }
    const spec = {};
    if (item.value !== null && item.value !== undefined) {
      spec.value = String(item.value);
    }
    if (item.label !== null && item.label !== undefined) {
      spec.label = String(item.label);
    }
    if (item.index !== null && item.index !== undefined) {
      spec.index = normalizeInteger(item.index, `options[${index}].index`);
    }
    return spec;
  });
}

function normalizeCoordinateValue(value, fieldName) {
  const coordinate = Number(value);
  if (!Number.isSafeInteger(coordinate)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `coordinateClick ${fieldName} must be a safe integer; got ${JSON.stringify(value)}`
    );
  }
  return coordinate;
}

function normalizeCoordinateSpace(value) {
  const normalized = String(value || "screen").trim().toLowerCase();
  if (["screen", "window", "viewport"].includes(normalized)) {
    return normalized;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `coordinateClick coordinateSpace must be one of screen, window, viewport; got ${JSON.stringify(value)}`
  );
}

async function performDomActionInPage(request) {
  const ERROR_SELECTOR_INVALID = "CHROME_DOM_SELECTOR_INVALID";
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const ERROR_ELEMENT_AMBIGUOUS = "CHROME_DOM_ELEMENT_AMBIGUOUS";
  const ERROR_ELEMENT_NOT_ACTIONABLE = "CHROME_DOM_ELEMENT_NOT_ACTIONABLE";
  const ERROR_ACTION_UNSUPPORTED = "CHROME_DOM_ACTION_UNSUPPORTED";
  const ERROR_POSTCONDITION_FAILED = "CHROME_DOM_ACTION_POSTCONDITION_FAILED";
  const ERROR_BROWSER_WAIT_TIMEOUT = "BROWSER_WAIT_TIMEOUT";

  const action = String(request?.action || "").trim().toLowerCase();
  const locator = {
    selector: stringOrEmpty(request?.selector),
    elementId: stringOrEmpty(request?.elementId),
    elementPath: stringOrEmpty(request?.elementPath),
    role: normalizeText(request?.role),
    name: normalizeText(request?.name),
    value: stringOrEmpty(request?.value),
    option: stringOrEmpty(request?.option),
    optionLabel: stringOrEmpty(request?.optionLabel),
    optionIndex: Number.isSafeInteger(request?.optionIndex) ? request.optionIndex : null,
    options: Array.isArray(request?.options) ? request.options : []
  };
  const clicks = Number.isSafeInteger(request?.clicks) ? request.clicks : (action === "dblclick" ? 2 : 1);
  const clickOptions = {
    button: normalizeMouseButtonLocal(request?.button),
    modifiers: normalizeClickModifiersLocal(request?.modifiers),
    position: normalizeClickPositionLocal(request?.position)
  };
  const eventType = stringOrEmpty(request?.eventType);
  const eventInit = plainObjectOrEmpty(request?.eventInit);
  const resolveOnly = Boolean(request?.resolveOnly);
  const resolveActionability = Boolean(request?.resolveActionability);
  const autoWait = Boolean(request?.autoWait);
  const autoWaitTimeoutMs = Number.isSafeInteger(request?.autoWaitTimeoutMs)
    ? Math.max(50, Math.min(request.autoWaitTimeoutMs, 30000))
    : 2000;
  const maxPageTextChars = Number.isSafeInteger(request?.maxPageTextChars)
    ? Math.min(Math.max(request.maxPageTextChars, 0), 65536)
    : 4096;

  if (!["click", "dblclick", "press", "select", "submit", "dispatch_event", "clear", "focus", "blur", "select_text", "check", "uncheck", "hover", "tap", "drag", "html5_drag", "html5_real_drag"].includes(action)) {
    return fail(ERROR_ACTION_UNSUPPORTED, `unsupported DOM action ${JSON.stringify(action)}`);
  }
  if (action === "dispatch_event" && !eventType) {
    return fail(ERROR_ACTION_UNSUPPORTED, "dispatch_event requires a non-empty eventType");
  }

  const beforeUrl = String(location.href || "");
  const beforePageText = readPageText(maxPageTextChars);
  const resolved = resolveDomActionElement(action, locator);
  if (!resolved.ok) {
    return resolved;
  }
  const element = resolved.element;
  const beforeElement = elementSummary(element);

  const bypassActionability = ["dispatch_event", "focus", "blur", "select_text"].includes(action);
  const deferActionability = autoWait && !bypassActionability;
  if (!bypassActionability && !deferActionability && !isElementEnabled(element)) {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "resolved element is disabled or aria-disabled", {
      matched_count: resolved.matchedCount,
      resolved_by: resolved.resolvedBy,
      before_element: beforeElement
    });
  }
  if (!bypassActionability && !deferActionability && !isElementVisible(element) && action !== "submit") {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "resolved element is not visible/actionable", {
      matched_count: resolved.matchedCount,
      resolved_by: resolved.resolvedBy,
      before_element: beforeElement
    });
  }
  if (resolveOnly) {
    let autoWaitReadback = null;
    if (resolveActionability && !bypassActionability) {
      autoWaitReadback = await waitForDomActionability(element, action, autoWaitTimeoutMs);
      if (!autoWaitReadback.ok) {
        return fail(
          ERROR_BROWSER_WAIT_TIMEOUT,
          `domAction ${action} auto_wait timed out after ${autoWaitTimeoutMs} ms waiting for ${autoWaitReadback.requirement}; unmet predicates: ${autoWaitReadback.predicate_detail}`,
          {
            matched_count: resolved.matchedCount,
            resolved_by: resolved.resolvedBy,
            before_element: beforeElement,
            auto_wait: true,
            auto_wait_readback: autoWaitReadback,
            source_of_truth: "Element.scrollIntoView({block:nearest,inline:nearest}) + getBoundingClientRect + elementFromPoint"
          }
        );
      }
    }
    let actionPoint = null;
    try {
      const point = elementClickPoint(element, null);
      actionPoint = { x: point.client_x, y: point.client_y };
    } catch (error) {
      return fail(error?.code || ERROR_ELEMENT_NOT_ACTIONABLE, errorMessageLocal(error), {
        matched_count: resolved.matchedCount,
        resolved_by: resolved.resolvedBy,
        before_element: beforeElement,
        auto_wait: autoWait,
        auto_wait_readback: autoWaitReadback
      });
    }
    return {
      ok: true,
      action,
      matched_count: resolved.matchedCount,
      resolved_by: resolved.resolvedBy,
      before_element: beforeElement,
      after_element: elementSummary(element),
      action_point: actionPoint,
      resolve_only: true,
      auto_wait: autoWait,
      auto_wait_readback: autoWaitReadback,
      in_page_before_url: beforeUrl,
      in_page_after_url: String(location.href || ""),
      in_page_title: String(document.title || ""),
      in_page_ready_state: String(document.readyState || ""),
      in_page_before_text: beforePageText,
      in_page_after_text: beforePageText
    };
  }

  const eventsDispatched = [];
  let actionReadback = {};
  let autoWaitReadback = null;
  if (autoWait && !bypassActionability) {
    autoWaitReadback = await waitForDomActionability(element, action, autoWaitTimeoutMs);
    if (!autoWaitReadback.ok) {
      return fail(
        ERROR_BROWSER_WAIT_TIMEOUT,
        `domAction ${action} auto_wait timed out after ${autoWaitTimeoutMs} ms waiting for ${autoWaitReadback.requirement}; unmet predicates: ${autoWaitReadback.predicate_detail}`,
        {
          matched_count: resolved.matchedCount,
          resolved_by: resolved.resolvedBy,
          before_element: beforeElement,
          auto_wait: true,
          auto_wait_readback: autoWaitReadback,
          source_of_truth: "Element.scrollIntoView({block:nearest,inline:nearest}) + getBoundingClientRect + elementFromPoint"
        }
      );
    }
  }
  if (!bypassActionability && !isElementEnabled(element)) {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "resolved element is disabled or aria-disabled", {
      matched_count: resolved.matchedCount,
      resolved_by: resolved.resolvedBy,
      before_element: beforeElement,
      auto_wait: autoWait,
      auto_wait_readback: autoWaitReadback
    });
  }
  if (!bypassActionability && !isElementVisible(element) && action !== "submit") {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "resolved element is not visible/actionable", {
      matched_count: resolved.matchedCount,
      resolved_by: resolved.resolvedBy,
      before_element: beforeElement,
      auto_wait: autoWait,
      auto_wait_readback: autoWaitReadback
    });
  }
  try {
    if (action === "click" || action === "press" || action === "dblclick") {
      actionReadback = performClick(element, action, clicks, clickOptions, eventsDispatched);
    } else if (action === "select") {
      actionReadback = performNativeSelect(element, locator, eventsDispatched);
    } else if (action === "submit") {
      actionReadback = performSubmit(element, eventsDispatched);
    } else if (action === "dispatch_event") {
      actionReadback = performDispatchEvent(element, eventType, eventInit, eventsDispatched);
    } else if (action === "clear") {
      actionReadback = performClear(element, eventsDispatched);
    } else if (action === "focus") {
      actionReadback = performFocus(element, eventsDispatched);
    } else if (action === "blur") {
      actionReadback = performBlur(element, eventsDispatched);
    } else if (action === "select_text") {
      actionReadback = performSelectText(element, eventsDispatched);
    } else if (action === "check") {
      actionReadback = performCheckState(element, true, eventsDispatched);
    } else if (action === "uncheck") {
      actionReadback = performCheckState(element, false, eventsDispatched);
    } else if (action === "hover" || action === "tap" || action === "drag" || action === "html5_drag" || action === "html5_real_drag") {
      throw actionError(ERROR_ACTION_UNSUPPORTED, `DOM action ${action} is resolver-only; dispatch through cdpInput`);
    }
  } catch (error) {
    return fail(error?.code || ERROR_ACTION_UNSUPPORTED, errorMessageLocal(error), {
      matched_count: resolved.matchedCount,
      resolved_by: resolved.resolvedBy,
      before_element: beforeElement,
      auto_wait: autoWait,
      auto_wait_readback: autoWaitReadback,
      events_dispatched: eventsDispatched
    });
  }

  const afterElement = element.isConnected ? elementSummary(element) : null;
  const afterPageText = readPageText(maxPageTextChars);
  return {
    ok: true,
    action,
    matched_count: resolved.matchedCount,
    resolved_by: resolved.resolvedBy,
    before_element: beforeElement,
    after_element: afterElement,
    auto_wait: autoWait,
    auto_wait_readback: autoWaitReadback,
    events_dispatched: eventsDispatched,
    action_readback: actionReadback,
    in_page_before_url: beforeUrl,
    in_page_after_url: String(location.href || ""),
    in_page_title: String(document.title || ""),
    in_page_ready_state: String(document.readyState || ""),
    in_page_before_text: beforePageText,
    in_page_after_text: afterPageText
  };

  async function waitForDomActionability(element, actionName, timeoutMs) {
    const started = Date.now();
    const deadline = started + timeoutMs;
    const requirement = actionName === "clear" ? "editable_action_ready" : "action_ready";
    let pollCount = 0;
    let lastActionability = null;
    let lastScroll = null;
    while (true) {
      lastScroll = await scrollIntoViewIfNeededSnapshot(element);
      lastActionability = await domActionabilitySnapshot(element);
      pollCount += 1;
      const ready = requirement === "editable_action_ready"
        ? lastActionability.editable_action_ready
        : lastActionability.action_ready;
      const unmetPredicates = actionabilityRelevantFailures(lastActionability, requirement);
      const predicateDetail = unmetPredicates.length
        ? unmetPredicates.map((failure) => failure.predicate).join(",")
        : "unknown";
      const readback = {
        ok: Boolean(ready),
        requirement,
        timeout_ms: timeoutMs,
        poll_count: pollCount,
        elapsed_ms: Math.max(0, Date.now() - started),
        unmet_predicates: unmetPredicates,
        predicate_detail: predicateDetail,
        last_actionability: lastActionability,
        scroll: lastScroll
      };
      if (ready) {
        return readback;
      }
      const now = Date.now();
      if (now >= deadline) {
        return readback;
      }
      await delay(Math.min(50, Math.max(0, deadline - now)));
    }
  }

  async function domActionabilitySnapshot(element) {
    const firstBox = domBoxModel(element);
    await twoAnimationFramesLocal();
    const secondBox = domBoxModel(element);
    const attached = Boolean(element && element.isConnected);
    const visible = attached && isElementVisible(element) && Boolean(firstBox?.content?.width > 0 && firstBox?.content?.height > 0);
    const enabled = isElementEnabled(element);
    const editable = Boolean(editableKind(element)) && !Boolean(element.disabled) && !Boolean(element.readOnly);
    const runningAnimation = hasRunningAnimationLocal(element);
    const stable = !runningAnimation && domBoxesStable(firstBox, secondBox);
    const hitTest = domHitTest(element, firstBox);
    const receivesEvents = hitTest.receives_events;
    const failureReasons = [];
    if (!attached) {
      failureReasons.push(domFailure("attached", "detached", "target element is not connected to the document"));
    }
    if (!visible) {
      failureReasons.push(domFailure("visible", "not_visible", "target element has no visible rendered box"));
    }
    if (!stable) {
      failureReasons.push(domFailure(
        "stable",
        runningAnimation ? "animation_running" : "box_changed",
        runningAnimation
          ? "target element has a running CSS/Web Animation"
          : "target element box changed between animation-frame samples"
      ));
    }
    if (!enabled) {
      failureReasons.push(domFailure("enabled", "disabled", "target element is disabled or aria-disabled"));
    }
    if (!editable && editableKind(element) && Boolean(element.readOnly)) {
      failureReasons.push(domFailure("editable", "readonly", "target editable element is readonly"));
    }
    if (!receivesEvents) {
      failureReasons.push(domFailure(
        "receives_events",
        "obscured",
        hitTest.top_node_description
          ? `action point is covered by ${hitTest.top_node_description}`
          : "document.elementFromPoint did not resolve to the target or one of its descendants"
      ));
    }
    const actionReady = attached && visible && stable && enabled && receivesEvents;
    return {
      attached,
      visible,
      stable,
      enabled,
      editable,
      receives_events: receivesEvents,
      action_ready: actionReady,
      editable_action_ready: actionReady && editable,
      failure_reasons: failureReasons,
      box_model: firstBox,
      second_box_model: secondBox,
      hit_test: hitTest
    };
  }

  async function scrollIntoViewIfNeededSnapshot(element) {
    const beforeContainer = nearestScrollContainerLocal(element);
    const before = domScrollSnapshot(element, beforeContainer);
    let scrollPerformed = false;
    if (!rectFullyInViewportLocal(before.viewport_rect) || !rectFullyInScrollContainerLocal(before.viewport_rect, before.scroll_container)) {
      try {
        element.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "instant" });
        scrollPerformed = true;
      } catch (_) {
        try {
          element.scrollIntoView();
          scrollPerformed = true;
        } catch (error) {
          return {
            ok: false,
            error: errorMessageLocal(error),
            before_viewport: before.viewport,
            before_scroll_container: before.scroll_container,
            scroll_performed: false
          };
        }
      }
    }
    if (scrollPerformed) {
      await twoAnimationFramesLocal();
    }
    const afterContainer = nearestScrollContainerLocal(element);
    const after = domScrollSnapshot(element, afterContainer);
    return {
      ok: true,
      before_viewport: before.viewport,
      after_viewport: after.viewport,
      before_box_model: before.box_model,
      after_box_model: after.box_model,
      before_scroll_container: before.scroll_container,
      after_scroll_container: after.scroll_container,
      window_scroll_changed:
        before.viewport.scroll_x !== after.viewport.scroll_x ||
        before.viewport.scroll_y !== after.viewport.scroll_y,
      container_scroll_changed: scrollContainerChangedLocal(before.scroll_container, after.scroll_container),
      node_fully_in_viewport_after: rectFullyInViewportLocal(after.viewport_rect),
      scroll_performed: scrollPerformed
    };
  }

  function actionabilityRelevantFailures(actionability, requirement) {
    const includesEditable = requirement === "editable_action_ready";
    return (Array.isArray(actionability?.failure_reasons) ? actionability.failure_reasons : [])
      .filter((failure) =>
        ["attached", "visible", "stable", "enabled", "receives_events"].includes(failure.predicate) ||
        (includesEditable && failure.predicate === "editable")
      )
      .map((failure) => ({
        predicate: failure.predicate,
        reason: failure.reason,
        detail: failure.detail
      }));
  }

  function domHitTest(element, boxModel) {
    if (!boxModel || !boxModel.content || boxModel.content.width <= 0 || boxModel.content.height <= 0) {
      return {
        point: null,
        receives_events: false,
        target_or_descendant_from_point: false,
        top_node_description: null,
        error: "no action point"
      };
    }
    const rect = viewportRectLocal(element);
    const point = {
      x: rect.left + rect.width / 2,
      y: rect.top + rect.height / 2
    };
    let top = null;
    let error = null;
    try {
      top = document.elementFromPoint(point.x, point.y);
    } catch (caught) {
      error = errorMessageLocal(caught);
    }
    const targetOrDescendant = top instanceof Element && (top === element || element.contains(top));
    return {
      point,
      receives_events: Boolean(targetOrDescendant),
      target_or_descendant_from_point: Boolean(targetOrDescendant),
      top_node_description: top instanceof Element ? nodeDescriptionLocal(top) : null,
      error
    };
  }

  function domBoxModel(element) {
    if (!(element instanceof Element) || typeof element.getBoundingClientRect !== "function") {
      return null;
    }
    const rect = element.getBoundingClientRect();
    if (!rect) {
      return null;
    }
    const pageRect = {
      x: Number(rect.left || 0) + Number(window.scrollX || 0),
      y: Number(rect.top || 0) + Number(window.scrollY || 0),
      width: Number(rect.width || 0),
      height: Number(rect.height || 0)
    };
    return {
      content: pageRect,
      border: { ...pageRect },
      width: Math.round(pageRect.width),
      height: Math.round(pageRect.height)
    };
  }

  function domScrollSnapshot(element, scrollContainer) {
    const rect = viewportRectLocal(element);
    return {
      viewport: {
        inner_width: Math.trunc(Number(window.innerWidth || 0)),
        inner_height: Math.trunc(Number(window.innerHeight || 0)),
        scroll_x: Number(window.scrollX || 0),
        scroll_y: Number(window.scrollY || 0),
        device_pixel_ratio: Number(window.devicePixelRatio || 1)
      },
      viewport_rect: {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height
      },
      box_model: domBoxModel(element),
      scroll_container: scrollContainerSnapshotLocal(scrollContainer)
    };
  }

  function nearestScrollContainerLocal(element) {
    let current = element instanceof Element ? element.parentElement : null;
    while (current && current !== document.documentElement) {
      const style = window.getComputedStyle(current);
      const overflow = `${style.overflow || ""} ${style.overflowX || ""} ${style.overflowY || ""}`;
      const scrollableStyle = /(auto|scroll|overlay)/i.test(overflow);
      const scrollableGeometry =
        current.scrollHeight > current.clientHeight ||
        current.scrollWidth > current.clientWidth;
      if (scrollableStyle && scrollableGeometry) {
        return current;
      }
      current = current.parentElement;
    }
    return null;
  }

  function scrollContainerSnapshotLocal(element) {
    if (!(element instanceof Element)) {
      return null;
    }
    const rect = viewportRectLocal(element);
    return {
      tag_name: tag(element),
      id: stringOrEmpty(element.id),
      class_name: stringOrEmpty(element.className),
      scroll_top: Number(element.scrollTop || 0),
      scroll_left: Number(element.scrollLeft || 0),
      scroll_height: Number(element.scrollHeight || 0),
      scroll_width: Number(element.scrollWidth || 0),
      client_height: Number(element.clientHeight || 0),
      client_width: Number(element.clientWidth || 0),
      viewport_rect: {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height
      }
    };
  }

  function scrollContainerChangedLocal(before, after) {
    if (!before && !after) {
      return false;
    }
    if (!before || !after) {
      return true;
    }
    return before.scroll_top !== after.scroll_top || before.scroll_left !== after.scroll_left;
  }

  function rectFullyInViewportLocal(rect) {
    if (!rect) {
      return false;
    }
    const epsilon = 0.5;
    return (
      rect.x >= -epsilon &&
      rect.y >= -epsilon &&
      rect.x + rect.width <= Number(window.innerWidth || 0) + epsilon &&
      rect.y + rect.height <= Number(window.innerHeight || 0) + epsilon
    );
  }

  function rectFullyInScrollContainerLocal(rect, scrollContainer) {
    if (!rect || !scrollContainer || !scrollContainer.viewport_rect) {
      return true;
    }
    const container = scrollContainer.viewport_rect;
    const epsilon = 0.5;
    return (
      rect.x >= container.x - epsilon &&
      rect.y >= container.y - epsilon &&
      rect.x + rect.width <= container.x + container.width + epsilon &&
      rect.y + rect.height <= container.y + container.height + epsilon
    );
  }

  function domBoxesStable(first, second) {
    if (!first || !second || !first.content || !second.content) {
      return false;
    }
    const epsilon = 0.25;
    return (
      Math.abs(first.content.x - second.content.x) <= epsilon &&
      Math.abs(first.content.y - second.content.y) <= epsilon &&
      Math.abs(first.content.width - second.content.width) <= epsilon &&
      Math.abs(first.content.height - second.content.height) <= epsilon
    );
  }

  function hasRunningAnimationLocal(element) {
    if (!(element instanceof Element) || typeof element.getAnimations !== "function") {
      return false;
    }
    try {
      return element.getAnimations({ subtree: false }).some((animation) => animation.playState === "running");
    } catch (_) {
      return false;
    }
  }

  function viewportRectLocal(element) {
    if (!(element instanceof Element) || typeof element.getBoundingClientRect !== "function") {
      return { left: 0, top: 0, width: 0, height: 0 };
    }
    const rect = element.getBoundingClientRect();
    return {
      left: Number(rect.left || 0),
      top: Number(rect.top || 0),
      width: Number(rect.width || 0),
      height: Number(rect.height || 0)
    };
  }

  function nodeDescriptionLocal(element) {
    if (!(element instanceof Element)) {
      return "";
    }
    const id = element.id ? `#${element.id}` : "";
    const cls = typeof element.className === "string" && element.className.trim()
      ? `.${element.className.trim().split(/\s+/).slice(0, 3).join(".")}`
      : "";
    const name = element.getAttribute("name") ? `[name="${element.getAttribute("name")}"]` : "";
    return `${tag(element)}${id}${cls}${name}`;
  }

  function domFailure(predicate, reason, detail) {
    return { predicate, reason, detail };
  }

  function twoAnimationFramesLocal() {
    return new Promise((resolve) => {
      let done = false;
      const finish = () => {
        if (done) {
          return;
        }
        done = true;
        resolve();
      };
      setTimeout(finish, 100);
      if (typeof requestAnimationFrame === "function") {
        requestAnimationFrame(() => requestAnimationFrame(finish));
      }
    });
  }

  function delay(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function resolveDomActionElement(actionName, loc) {
    let candidates = [];
    let resolvedBy = "semantic";
    if (loc.selector) {
      resolvedBy = "selector";
      try {
        candidates = deepQueryAllLocal(loc.selector);
      } catch (error) {
        return fail(ERROR_SELECTOR_INVALID, `invalid selector ${JSON.stringify(loc.selector)}: ${errorMessageLocal(error)}`);
      }
    } else if (loc.elementPath) {
      resolvedBy = "element_path";
      const byPath = elementByPathLocal(loc.elementPath);
      candidates = byPath ? [byPath] : [];
    } else if (loc.elementId) {
      resolvedBy = "element_id";
      const id = loc.elementId.startsWith("#") ? loc.elementId.slice(1) : loc.elementId;
      const byId = id ? document.getElementById(id) : null;
      candidates = byId ? [byId] : [];
    } else {
      candidates = semanticCandidates(actionName);
    }

    const filtered = candidates.filter((candidate) => {
      if (!(candidate instanceof Element)) {
        return false;
      }
      if (loc.role && normalizeText(inferRole(candidate)) !== loc.role) {
        return false;
      }
      if (loc.name && !textMatches(accessibleName(candidate), loc.name)) {
        return false;
      }
      if (loc.value && actionName !== "select" && !valueMatches(candidate, loc.value)) {
        return false;
      }
      return true;
    });

    if (filtered.length === 0) {
      return fail(ERROR_ELEMENT_NOT_FOUND, `no DOM element matched ${locatorDebug(loc)}`, {
        matched_count: 0,
        resolved_by: resolvedBy
      });
    }
    if (filtered.length > 1) {
      return fail(ERROR_ELEMENT_AMBIGUOUS, `${filtered.length} DOM elements matched ${locatorDebug(loc)}`, {
        matched_count: filtered.length,
        resolved_by: resolvedBy,
        candidates: filtered.slice(0, 5).map(elementSummary)
      });
    }
    return {
      ok: true,
      element: filtered[0],
      matchedCount: filtered.length,
      resolvedBy
    };
  }

  function elementByPathLocal(path) {
    if (!path) {
      return null;
    }
    // Shadow-aware: "s" token re-enters the current element's open shadowRoot.
    const parts = String(path).split(".");
    if (Number(parts[0]) !== 0) {
      return null;
    }
    let current = document.documentElement;
    let enterShadow = false;
    for (const raw of parts.slice(1)) {
      if (raw === "s") {
        enterShadow = true;
        continue;
      }
      const index = Number(raw);
      if (!current || !Number.isSafeInteger(index) || index < 0) {
        return null;
      }
      let scope = current;
      if (enterShadow) {
        if (!current.shadowRoot) {
          return null;
        }
        scope = current.shadowRoot;
        enterShadow = false;
      }
      current = scope.children[index] || null;
    }
    return current instanceof Element ? current : null;
  }

  function deepQueryAllLocal(selector) {
    // Shadow-piercing querySelectorAll: run the selector in the document scope
    // and in every reachable open shadow root, union the results. CSS combinators
    // resolve within a single tree scope (Playwright semantics). Throws on an
    // invalid selector so the caller can report it.
    const out = [];
    const seen = new Set();
    const scopes = [document];
    const stack = [document];
    while (stack.length) {
      const node = stack.pop();
      const all = node.querySelectorAll ? node.querySelectorAll("*") : [];
      for (const el of all) {
        if (el.shadowRoot) {
          scopes.push(el.shadowRoot);
          stack.push(el.shadowRoot);
        }
      }
    }
    for (const scope of scopes) {
      const matches = scope.querySelectorAll(selector);
      for (const el of matches) {
        if (!seen.has(el)) {
          seen.add(el);
          out.push(el);
        }
      }
    }
    return out;
  }

  function semanticCandidates(actionName) {
    if (["dispatch_event", "focus", "blur", "select_text"].includes(actionName)) {
      return deepQueryAllLocal("*");
    }
    if (actionName === "clear") {
      return deepQueryAllLocal("input,textarea,[contenteditable],[role='textbox']");
    }
    if (actionName === "check" || actionName === "uncheck") {
      return deepQueryAllLocal("input[type='checkbox'],input[type='radio'],[role='checkbox'],[role='radio']");
    }
    if (actionName === "select") {
      return deepQueryAllLocal("select,[role='combobox'],[role='listbox']");
    }
    if (actionName === "submit") {
      return deepQueryAllLocal("form,button,input[type='submit'],input[type='image'],input[type='button'],[role='button']");
    }
    return deepQueryAllLocal("button,a[href],input[type='button'],input[type='submit'],input[type='reset'],summary,[role='button'],[role='link'],[role='menuitem'],[role='tab'],[tabindex]");
  }

  function performClick(element, actionName, count, options, events) {
    scrollIntoViewIfPossible(element);
    const bounded = Math.min(Math.max(count, 1), 3);
    const button = options?.button || "left";
    const modifiers = Array.isArray(options?.modifiers) ? options.modifiers : [];
    const needsSyntheticEvents = actionName === "dblclick" || bounded !== 1 || button !== "left" || modifiers.length > 0 || Boolean(options?.position);
    const popupIntents = [];
    const restoreWindowOpen = installWindowOpenCapture(popupIntents);
    const anchorIntent = captureAnchorTargetBlankIntent(element);
    let readback;
    try {
      if (!needsSyntheticEvents && typeof element.click === "function") {
        // A real left click always emits the pointer/mouse press-release sequence
        // before activation. Many components (e.g. react-select's control, which
        // opens on `mousedown`, not `click`) never respond to a bare
        // element.click() that fires only a `click` event. Emit the real
        // pointerdown/mousedown/pointerup/mouseup sequence first, then use the
        // native click() for trusted activation defaults (checkbox/link/form
        // submit). This is a strict superset of the prior behavior. (#1311)
        const point = elementClickPoint(element, null);
        dispatchPointerLikeEvent(element, "pointerdown", point, button, modifiers, 1, true);
        events.push("pointerdown");
        dispatchMouseLikeEvent(element, "mousedown", point, button, modifiers, 1, true);
        events.push("mousedown");
        dispatchPointerLikeEvent(element, "pointerup", point, button, modifiers, 1, false);
        events.push("pointerup");
        dispatchMouseLikeEvent(element, "mouseup", point, button, modifiers, 1, false);
        events.push("mouseup");
        element.click();
        events.push("click");
        readback = {
          click_count: 1,
          button,
          modifiers,
          position: null,
          activation_event: "click",
          native_click_method: true,
          synthetic_press_sequence: true
        };
      } else {
        const point = elementClickPoint(element, options?.position || null);
        let activationEvent = "click";
        for (let index = 0; index < bounded; index += 1) {
          const detail = index + 1;
          dispatchPointerLikeEvent(element, "pointerdown", point, button, modifiers, detail, true);
          events.push("pointerdown");
          dispatchMouseLikeEvent(element, "mousedown", point, button, modifiers, detail, true);
          events.push("mousedown");
          dispatchPointerLikeEvent(element, "pointerup", point, button, modifiers, detail, false);
          events.push("pointerup");
          dispatchMouseLikeEvent(element, "mouseup", point, button, modifiers, detail, false);
          events.push("mouseup");
          activationEvent = activationEventForButton(button);
          dispatchMouseLikeEvent(element, activationEvent, point, button, modifiers, detail, false);
          events.push(activationEvent);
          if (button === "left" && detail === 2 && (actionName === "dblclick" || bounded >= 2)) {
            dispatchMouseLikeEvent(element, "dblclick", point, button, modifiers, detail, false);
            events.push("dblclick");
          }
        }
        readback = {
          click_count: bounded,
          button,
          modifiers,
          position: options?.position || null,
          viewport_point: {
            client_x: point.client_x,
            client_y: point.client_y
          },
          element_point: {
            x: point.element_x,
            y: point.element_y
          },
          activation_event: activationEvent
        };
      }
    } finally {
      restoreWindowOpen();
    }
    return {
      ...readback,
      popup_intents: popupIntents,
      anchor_target_blank_intent: anchorIntent
    };
  }

  function installWindowOpenCapture(intents) {
    if (typeof window.open !== "function") {
      return () => {};
    }
    const originalOpen = window.open;
    const capturedOpen = function capturedSynapseWindowOpen(url, target, features) {
      let opened = false;
      let thrown = null;
      try {
        const result = originalOpen.apply(window, arguments);
        opened = Boolean(result);
        return result;
      } catch (error) {
        thrown = errorMessageLocal(error);
        throw error;
      } finally {
        intents.push({
          kind: "window_open",
          url: stringifyPopupArg(url),
          resolved_url: resolvePopupUrl(url),
          target: stringifyPopupArg(target),
          features: stringifyPopupArg(features),
          opened,
          error: thrown
        });
      }
    };
    window.open = capturedOpen;
    return () => {
      try {
        if (window.open === capturedOpen) {
          window.open = originalOpen;
        }
      } catch (_) {
        // Best-effort restore; the action result still reports captured intent.
      }
    };
  }

  function captureAnchorTargetBlankIntent(element) {
    const anchor = typeof element.closest === "function" ? element.closest("a[href]") : null;
    if (!(anchor instanceof HTMLAnchorElement)) {
      return null;
    }
    const target = String(anchor.getAttribute("target") || "").trim();
    const normalizedTarget = target.toLowerCase();
    if (!normalizedTarget || normalizedTarget === "_self") {
      return null;
    }
    return {
      kind: "anchor_target_blank",
      url: String(anchor.getAttribute("href") || ""),
      resolved_url: String(anchor.href || ""),
      target,
      rel: String(anchor.getAttribute("rel") || ""),
      opened: false,
      text: trimForReadback(anchor.innerText || anchor.textContent || "", 200)
    };
  }

  function stringifyPopupArg(value) {
    return value === null || value === undefined ? "" : String(value);
  }

  function resolvePopupUrl(value) {
    const raw = stringifyPopupArg(value);
    if (!raw) {
      return "about:blank";
    }
    try {
      return new URL(raw, location.href).href;
    } catch (_) {
      return raw;
    }
  }

  function elementClickPoint(element, position) {
    const rect = element.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0) {
      throw actionError(ERROR_ELEMENT_NOT_ACTIONABLE, `resolved ${tag(element)} element has no clickable box`);
    }
    const elementX = position ? Number(position.x) : rect.width / 2;
    const elementY = position ? Number(position.y) : rect.height / 2;
    if (!Number.isFinite(elementX) || !Number.isFinite(elementY) || elementX < 0 || elementY < 0 || elementX >= rect.width || elementY >= rect.height) {
      throw actionError(
        ERROR_ELEMENT_NOT_ACTIONABLE,
        `click position (${elementX}, ${elementY}) falls outside ${tag(element)} box ${Math.round(rect.width)}x${Math.round(rect.height)}`
      );
    }
    return {
      client_x: rect.left + elementX,
      client_y: rect.top + elementY,
      element_x: elementX,
      element_y: elementY
    };
  }

  function dispatchPointerLikeEvent(element, eventType, point, button, modifiers, detail, pressed) {
    if (typeof PointerEvent !== "function") {
      dispatchMouseLikeEvent(element, eventType.replace(/^pointer/, "mouse"), point, button, modifiers, detail, pressed);
      return;
    }
    element.dispatchEvent(new PointerEvent(eventType, {
      ...mouseEventInit(point, button, modifiers, detail, pressed),
      pointerId: 1,
      pointerType: "mouse",
      isPrimary: true,
      width: 1,
      height: 1,
      pressure: pressed ? 0.5 : 0
    }));
  }

  function dispatchMouseLikeEvent(element, eventType, point, button, modifiers, detail, pressed) {
    element.dispatchEvent(new MouseEvent(eventType, mouseEventInit(point, button, modifiers, detail, pressed)));
  }

  function mouseEventInit(point, button, modifiers, detail, pressed) {
    return {
      bubbles: true,
      cancelable: true,
      composed: true,
      view: window,
      detail,
      button: mouseButtonCode(button),
      buttons: pressed ? mouseButtonBits(button) : 0,
      clientX: point.client_x,
      clientY: point.client_y,
      screenX: Math.round(Number(window.screenX || 0) + point.client_x),
      screenY: Math.round(Number(window.screenY || 0) + point.client_y),
      ...modifierFlags(modifiers)
    };
  }

  function activationEventForButton(button) {
    if (button === "right") {
      return "contextmenu";
    }
    if (button === "middle") {
      return "auxclick";
    }
    return "click";
  }

  function mouseButtonCode(button) {
    if (button === "middle") {
      return 1;
    }
    if (button === "right") {
      return 2;
    }
    return 0;
  }

  function mouseButtonBits(button) {
    if (button === "middle") {
      return 4;
    }
    if (button === "right") {
      return 2;
    }
    return 1;
  }

  function modifierFlags(modifiers) {
    const set = new Set(Array.isArray(modifiers) ? modifiers : []);
    return {
      altKey: set.has("alt"),
      ctrlKey: set.has("ctrl"),
      metaKey: set.has("meta"),
      shiftKey: set.has("shift")
    };
  }

  function normalizeMouseButtonLocal(value) {
    const normalized = String(value || "left").trim().toLowerCase();
    return ["left", "right", "middle"].includes(normalized) ? normalized : "left";
  }

  function normalizeClickModifiersLocal(value) {
    if (!Array.isArray(value)) {
      return [];
    }
    const seen = new Set();
    for (const item of value) {
      const normalized = String(item || "").trim().toLowerCase();
      if (normalized === "ctrl" || normalized === "control") {
        seen.add("ctrl");
      } else if (normalized === "shift") {
        seen.add("shift");
      } else if (normalized === "alt" || normalized === "option") {
        seen.add("alt");
      } else if (["meta", "super", "cmd", "command"].includes(normalized)) {
        seen.add("meta");
      }
    }
    return ["alt", "ctrl", "meta", "shift"].filter((modifier) => seen.has(modifier));
  }

  function normalizeClickPositionLocal(value) {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      return null;
    }
    const x = Number(value.x);
    const y = Number(value.y);
    if (!Number.isSafeInteger(x) || !Number.isSafeInteger(y) || x < 0 || y < 0) {
      return null;
    }
    return { x, y };
  }

  function performDispatchEvent(element, requestedType, requestedInit, events) {
    const eventType = stringOrEmpty(requestedType);
    if (!eventType) {
      throw actionError(ERROR_ACTION_UNSUPPORTED, "dispatch_event requires a non-empty event type");
    }
    const eventInit = plainObjectOrEmpty(requestedInit);
    const event = createDomEvent(eventType, eventInit);
    const defaultAllowed = element.dispatchEvent(event);
    events.push(eventType);
    return {
      event_type: eventType,
      constructor_used: event.constructor && event.constructor.name ? String(event.constructor.name) : "Event",
      default_allowed: Boolean(defaultAllowed),
      default_prevented: Boolean(event.defaultPrevented),
      bubbles: Boolean(event.bubbles),
      cancelable: Boolean(event.cancelable),
      composed: Boolean(event.composed),
      detail: "detail" in event ? event.detail : null
    };
  }

  function performClear(element, events) {
    const editable = editableKind(element);
    if (!editable) {
      throw actionError(
        ERROR_ACTION_UNSUPPORTED,
        `domAction clear supports editable input/textarea/contenteditable targets only; resolved ${tag(element)} role=${inferRole(element)}`
      );
    }
    if (element.disabled || element.readOnly) {
      throw actionError(ERROR_ELEMENT_NOT_ACTIONABLE, "resolved editable element is disabled or readonly");
    }
    try {
      if (typeof element.focus === "function") {
        element.focus({ preventScroll: true });
        events.push("focus");
      }
    } catch (_) {
      // A failed focus does not prove the DOM value cannot be cleared.
    }

    const beforeValue = elementTextValue(element);
    const beforeInput = dispatchInputLikeEvent(element, "beforeinput", null, "deleteContentBackward", true);
    events.push("beforeinput");
    if (!beforeInput) {
      throw actionError(ERROR_ACTION_UNSUPPORTED, "clear was cancelled by a beforeinput listener");
    }

    if (editable === "value") {
      setNativeValue(element, "");
      try {
        if (typeof element.setSelectionRange === "function") {
          element.setSelectionRange(0, 0);
        }
      } catch (_) {
        // Some input types expose value but reject text selection.
      }
    } else {
      element.textContent = "";
    }

    dispatchInputLikeEvent(element, "input", null, "deleteContentBackward", false);
    events.push("input");
    element.dispatchEvent(new Event("change", { bubbles: true }));
    events.push("change");

    const afterValue = elementTextValue(element);
    if (afterValue !== "") {
      throw actionError(
        ERROR_POSTCONDITION_FAILED,
        `clear postcondition failed: after value/text length ${afterValue.length} was not zero`
      );
    }
    return {
      before_value_len: beforeValue.length,
      after_value_len: afterValue.length,
      input_fired: true,
      change_fired: true
    };
  }

  function performFocus(element, events) {
    const beforeActive = activeElementSummary(element.ownerDocument, element);
    if (typeof element.focus !== "function") {
      throw actionError(ERROR_ACTION_UNSUPPORTED, `resolved ${tag(element)} element has no focus() method`);
    }
    try {
      element.focus({ preventScroll: true });
    } catch (_) {
      element.focus();
    }
    events.push("focus");
    const afterActive = activeElementSummary(element.ownerDocument, element);
    if (element.ownerDocument.activeElement !== element) {
      throw actionError(
        ERROR_POSTCONDITION_FAILED,
        `focus postcondition failed: activeElement is ${afterActive.tag_name || "none"}#${afterActive.id || ""}`
      );
    }
    return {
      before_active_element: beforeActive,
      after_active_element: afterActive,
      active_element_is_target: true
    };
  }

  function performBlur(element, events) {
    const beforeActive = activeElementSummary(element.ownerDocument, element);
    if (typeof element.blur !== "function") {
      throw actionError(ERROR_ACTION_UNSUPPORTED, `resolved ${tag(element)} element has no blur() method`);
    }
    element.blur();
    events.push("blur");
    const afterActive = activeElementSummary(element.ownerDocument, element);
    if (element.ownerDocument.activeElement === element) {
      throw actionError(ERROR_POSTCONDITION_FAILED, "blur postcondition failed: target remained document.activeElement");
    }
    return {
      before_active_element: beforeActive,
      after_active_element: afterActive,
      active_element_is_target: false
    };
  }

  function performSelectText(element, events) {
    const beforeSelection = selectionSummary(element);
    let readback;
    if (isSelectableValueControl(element)) {
      try {
        if (typeof element.focus === "function") {
          element.focus({ preventScroll: true });
          events.push("focus");
        }
      } catch (_) {
        // select() may still work for some controls.
      }
      try {
        element.select();
      } catch (error) {
        throw actionError(ERROR_ACTION_UNSUPPORTED, `selectText value-control select() failed: ${errorMessageLocal(error)}`);
      }
      element.dispatchEvent(new Event("select", { bubbles: true }));
      events.push("select");
      element.ownerDocument.dispatchEvent(new Event("selectionchange", { bubbles: true }));
      events.push("selectionchange");
      const value = elementTextValue(element);
      const start = typeof element.selectionStart === "number" ? element.selectionStart : null;
      const end = typeof element.selectionEnd === "number" ? element.selectionEnd : null;
      readback = {
        selection_mode: "value_control_select",
        text_len: value.length,
        selection_start: start,
        selection_end: end,
        selected_text: start === null || end === null ? "" : value.slice(start, end)
      };
      if (readback.selected_text !== value) {
        throw actionError(
          ERROR_POSTCONDITION_FAILED,
          `selectText postcondition failed: selected ${readback.selected_text.length} of ${value.length} chars`
        );
      }
    } else {
      const doc = element.ownerDocument;
      const selection = doc.defaultView?.getSelection?.();
      if (!selection) {
        throw actionError(ERROR_ACTION_UNSUPPORTED, "selectText requires document.getSelection()");
      }
      const range = doc.createRange();
      range.selectNodeContents(element);
      selection.removeAllRanges();
      selection.addRange(range);
      doc.dispatchEvent(new Event("selectionchange", { bubbles: true }));
      events.push("selectionchange");
      const expected = String(element.textContent || "");
      readback = {
        selection_mode: "dom_range_select_node_contents",
        text_len: expected.length,
        selection_start: null,
        selection_end: null,
        selected_text: String(selection.toString() || "")
      };
      if (readback.selected_text !== expected) {
        throw actionError(
          ERROR_POSTCONDITION_FAILED,
          `selectText postcondition failed: selected ${readback.selected_text.length} of ${expected.length} chars`
        );
      }
    }
    return {
      before_selection: beforeSelection,
      after_selection: selectionSummary(element),
      ...readback
    };
  }

  function performCheckState(element, desiredChecked, events) {
    const checkable = checkableKind(element);
    if (!checkable) {
      throw actionError(
        ERROR_ACTION_UNSUPPORTED,
        `domAction ${desiredChecked ? "check" : "uncheck"} supports native checkbox/radio inputs only; resolved ${tag(element)} role=${inferRole(element)}`
      );
    }
    if (checkable === "radio" && !desiredChecked) {
      throw actionError(ERROR_ACTION_UNSUPPORTED, "domAction uncheck does not support radio inputs; select another radio in the group");
    }
    const beforeChecked = Boolean(element.checked);
    if (beforeChecked === desiredChecked) {
      return {
        desired_checked: desiredChecked,
        before_checked: beforeChecked,
        after_checked: beforeChecked,
        changed: false,
        no_op: true,
        click_dispatched: false,
        checkable_kind: checkable
      };
    }
    scrollIntoViewIfPossible(element);
    if (typeof element.click !== "function") {
      throw actionError(ERROR_ACTION_UNSUPPORTED, `resolved ${tag(element)} element has no click() method`);
    }
    element.click();
    events.push("click");
    const afterChecked = Boolean(element.checked);
    if (afterChecked !== desiredChecked) {
      throw actionError(
        ERROR_POSTCONDITION_FAILED,
        `check state postcondition failed: desired=${desiredChecked} after=${afterChecked}`
      );
    }
    return {
      desired_checked: desiredChecked,
      before_checked: beforeChecked,
      after_checked: afterChecked,
      changed: true,
      no_op: false,
      click_dispatched: true,
      checkable_kind: checkable,
      name_attr: String(element.getAttribute("name") || ""),
      value_attr: String(element.getAttribute("value") || "")
    };
  }

  function createDomEvent(eventType, requestedInit) {
    const lower = eventType.toLowerCase();
    // Default synthetic dispatch_event events to bubble/cancel/compose so
    // framework-delegated listeners at the document root (React onInput, etc.)
    // and editors like Quill/Draft observe them. Caller-supplied values override
    // these defaults (#1347).
    const eventInit = { bubbles: true, cancelable: true, composed: true, ...requestedInit };
    if (Object.prototype.hasOwnProperty.call(eventInit, "detail") && typeof CustomEvent === "function") {
      return new CustomEvent(eventType, eventInit);
    }
    if (isPointerEventType(lower) && typeof PointerEvent === "function") {
      return new PointerEvent(eventType, eventInit);
    }
    if (isMouseEventType(lower) && typeof MouseEvent === "function") {
      return new MouseEvent(eventType, eventInit);
    }
    if (isKeyboardEventType(lower) && typeof KeyboardEvent === "function") {
      return new KeyboardEvent(eventType, eventInit);
    }
    if (isInputEventType(lower) && typeof InputEvent === "function") {
      return new InputEvent(eventType, eventInit);
    }
    if (isFocusEventType(lower) && typeof FocusEvent === "function") {
      return new FocusEvent(eventType, eventInit);
    }
    if (lower === "wheel" && typeof WheelEvent === "function") {
      return new WheelEvent(eventType, eventInit);
    }
    return new Event(eventType, eventInit);
  }

  function isPointerEventType(lower) {
    return ["pointerover", "pointerenter", "pointerdown", "pointermove", "pointerup", "pointercancel", "pointerout", "pointerleave", "gotpointercapture", "lostpointercapture"].includes(lower);
  }

  function isMouseEventType(lower) {
    return ["click", "dblclick", "mousedown", "mouseup", "mouseover", "mousemove", "mouseout", "mouseenter", "mouseleave", "contextmenu", "auxclick"].includes(lower);
  }

  function isKeyboardEventType(lower) {
    return ["keydown", "keypress", "keyup"].includes(lower);
  }

  function isInputEventType(lower) {
    return ["beforeinput", "input"].includes(lower);
  }

  function isFocusEventType(lower) {
    return ["focus", "blur", "focusin", "focusout"].includes(lower);
  }

  function performNativeSelect(element, loc, events) {
    if (tag(element) !== "select") {
      throw actionError(
        ERROR_ACTION_UNSUPPORTED,
        `domAction select supports native <select> only; resolved ${tag(element)} role=${inferRole(element)}`
      );
    }
    const specs = selectOptionSpecs(loc);
    if (specs.length === 0) {
      throw actionError(ERROR_ACTION_UNSUPPORTED, "domAction select requires option, value, option_label, option_index, or options[]");
    }
    if (specs.length > 1 && !element.multiple) {
      throw actionError(ERROR_ACTION_UNSUPPORTED, "domAction select received multiple option specs for a non-multiple <select>");
    }
    const options = Array.from(element.options || []);
    const resolved = specs.map((spec) => resolveSelectOption(options, spec));
    if (element.multiple) {
      for (const option of options) {
        option.selected = false;
      }
      for (const option of resolved) {
        option.selected = true;
      }
    } else {
      for (const option of options) {
        option.selected = false;
      }
      resolved[0].selected = true;
      element.selectedIndex = resolved[0].index;
    }
    element.dispatchEvent(new Event("input", { bubbles: true }));
    events.push("input");
    element.dispatchEvent(new Event("change", { bubbles: true }));
    events.push("change");
    const selectedOptions = Array.from(element.selectedOptions || []).map(optionReadback);
    const requested = specs.map(selectSpecReadback);
    for (const spec of specs) {
      const found = selectedOptions.some((option) => option.index === spec.resolvedIndex);
      if (!found) {
        throw actionError(
          ERROR_POSTCONDITION_FAILED,
          `select postcondition failed: option index ${spec.resolvedIndex} was not selected`
        );
      }
    }
    return {
      multiple: Boolean(element.multiple),
      requested_options: requested,
      selected_options: selectedOptions,
      selected_values: selectedOptions.map((option) => option.value),
      selected_labels: selectedOptions.map((option) => option.label),
      selected_indices: selectedOptions.map((option) => option.index),
      selected_value: String(element.value ?? ""),
      selected_index: element.selectedIndex
    };
  }

  function selectOptionSpecs(loc) {
    const specs = [];
    if (loc.option) {
      specs.push({ kind: "value_or_label", value: loc.option });
    }
    if (loc.value) {
      specs.push({ kind: "value_or_label", value: loc.value });
    }
    if (loc.optionLabel) {
      specs.push({ kind: "label", label: loc.optionLabel });
    }
    if (loc.optionIndex !== null) {
      specs.push({ kind: "index", index: loc.optionIndex });
    }
    for (const raw of loc.options) {
      const value = raw && raw.value !== null && raw.value !== undefined ? String(raw.value) : "";
      const label = raw && raw.label !== null && raw.label !== undefined ? String(raw.label) : "";
      const hasIndex = raw && Number.isSafeInteger(raw.index);
      const present = [value !== "", label !== "", hasIndex].filter(Boolean).length;
      if (present !== 1) {
        throw actionError(ERROR_ACTION_UNSUPPORTED, `select options[] entries must contain exactly one of value, label, or index; got ${JSON.stringify(raw)}`);
      }
      if (value !== "") {
        specs.push({ kind: "value", value });
      } else if (label !== "") {
        specs.push({ kind: "label", label });
      } else {
        specs.push({ kind: "index", index: raw.index });
      }
    }
    return specs;
  }

  function resolveSelectOption(options, spec) {
    let matches = [];
    if (spec.kind === "index") {
      if (!Number.isSafeInteger(spec.index) || spec.index < 0 || spec.index >= options.length) {
        throw actionError(ERROR_ELEMENT_NOT_FOUND, `no <option> matched index ${JSON.stringify(spec.index)}; option_count=${options.length}`);
      }
      const option = options[spec.index];
      spec.resolvedIndex = option.index;
      return option;
    }
    if (spec.kind === "value") {
      matches = options.filter((option) => String(option.value ?? "") === spec.value);
    } else if (spec.kind === "label") {
      matches = options.filter((option) => normalizeText(option.textContent) === normalizeText(spec.label));
    } else {
      const exactValue = options.filter((option) => String(option.value ?? "") === spec.value);
      const exactText = options.filter((option) => normalizeText(option.textContent) === normalizeText(spec.value));
      matches = exactValue.length > 0 ? exactValue : exactText;
    }
    if (matches.length === 0) {
      throw actionError(ERROR_ELEMENT_NOT_FOUND, `no <option> matched ${JSON.stringify(selectSpecReadback(spec))}`);
    }
    if (matches.length > 1) {
      throw actionError(ERROR_ELEMENT_AMBIGUOUS, `${matches.length} <option> elements matched ${JSON.stringify(selectSpecReadback(spec))}`);
    }
    spec.resolvedIndex = matches[0].index;
    return matches[0];
  }

  function optionReadback(option) {
    return {
      value: String(option.value ?? ""),
      label: trimForReadback(option.textContent, 160),
      index: option.index,
      disabled: Boolean(option.disabled)
    };
  }

  function selectSpecReadback(spec) {
    if (spec.kind === "index") {
      return { index: spec.index };
    }
    if (spec.kind === "label") {
      return { label: spec.label };
    }
    return { value: spec.value };
  }

  function performSubmit(element, events) {
    const form = tag(element) === "form" ? element : (element.form || element.closest?.("form"));
    if (!(form instanceof HTMLFormElement)) {
      throw actionError(ERROR_ELEMENT_NOT_FOUND, `resolved ${tag(element)} element is not a form and has no owning form`);
    }
    if (typeof form.requestSubmit !== "function") {
      throw actionError(ERROR_ACTION_UNSUPPORTED, "HTMLFormElement.requestSubmit() is unavailable");
    }
    let submitter = tag(element) === "form" ? null : element;
    if (submitter && !isSubmitter(submitter)) {
      submitter = null;
    }
    if (submitter) {
      form.requestSubmit(submitter);
    } else {
      form.requestSubmit();
    }
    events.push("submit");
    return {
      form_id: String(form.id || ""),
      submitter_tag: submitter ? tag(submitter) : null,
      submitter_id: submitter ? String(submitter.id || "") : null,
      submitter_name: submitter ? String(submitter.getAttribute("name") || "") : null
    };
  }

  function isSubmitter(element) {
    const lower = tag(element);
    if (lower === "button") {
      const type = String(element.getAttribute("type") || "submit").toLowerCase();
      return type === "submit" || type === "";
    }
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "").toLowerCase();
      return type === "submit" || type === "image";
    }
    return false;
  }

  function editableKind(element) {
    if (!(element instanceof Element)) {
      return null;
    }
    const lower = tag(element);
    if (lower === "textarea") {
      return "value";
    }
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "text").toLowerCase();
      const nonText = ["button", "submit", "reset", "checkbox", "radio", "range", "color", "file", "image", "hidden"];
      return nonText.includes(type) ? null : "value";
    }
    if (
      element.isContentEditable ||
      String(element.getAttribute("contenteditable") || "").toLowerCase() === "true" ||
      String(element.getAttribute("role") || "").toLowerCase() === "textbox"
    ) {
      return "contenteditable";
    }
    return null;
  }

  function isSelectableValueControl(element) {
    if (!(element instanceof Element)) {
      return false;
    }
    const lower = tag(element);
    if (!["input", "textarea"].includes(lower) || typeof element.select !== "function" || !("value" in element)) {
      return false;
    }
    if (lower === "textarea") {
      return true;
    }
    const type = String(element.getAttribute("type") || "text").toLowerCase();
    const nonSelectable = ["button", "submit", "reset", "checkbox", "radio", "range", "color", "file", "image", "hidden"];
    return !nonSelectable.includes(type);
  }

  function checkableKind(element) {
    if (!(element instanceof HTMLInputElement)) {
      return null;
    }
    const type = String(element.getAttribute("type") || "").toLowerCase();
    if (type === "checkbox" || type === "radio") {
      return type;
    }
    return null;
  }

  function elementTextValue(element) {
    if ("value" in element) {
      return String(element.value ?? "");
    }
    return String(element.innerText || element.textContent || "");
  }

  function setNativeValue(element, value) {
    const lower = tag(element);
    const proto =
      lower === "input"
        ? window.HTMLInputElement?.prototype
        : lower === "textarea"
          ? window.HTMLTextAreaElement?.prototype
          : null;
    const descriptor = proto ? Object.getOwnPropertyDescriptor(proto, "value") : null;
    if (descriptor && typeof descriptor.set === "function") {
      descriptor.set.call(element, value);
    } else {
      element.value = value;
    }
  }

  function dispatchInputLikeEvent(element, type, data, inputType, cancelable) {
    let event;
    try {
      event = new InputEvent(type, {
        bubbles: true,
        cancelable,
        data,
        inputType
      });
    } catch (_) {
      event = new Event(type, { bubbles: true, cancelable });
    }
    return element.dispatchEvent(event);
  }

  function activeElementSummary(doc, target) {
    const active = doc?.activeElement || null;
    if (!(active instanceof Element)) {
      return {
        has_active_element: false,
        tag_name: "",
        id: "",
        role: "",
        name_attr: "",
        accessible_name: "",
        is_target: false
      };
    }
    return {
      has_active_element: true,
      tag_name: tag(active),
      id: String(active.id || ""),
      role: inferRole(active),
      name_attr: String(active.getAttribute("name") || ""),
      accessible_name: trimForReadback(accessibleName(active), 160),
      is_target: active === target
    };
  }

  function selectionSummary(element) {
    const value = elementTextValue(element);
    if (typeof element.selectionStart === "number" && typeof element.selectionEnd === "number") {
      return {
        selection_mode: "value_control",
        selection_start: element.selectionStart,
        selection_end: element.selectionEnd,
        selected_text: value.slice(element.selectionStart, element.selectionEnd)
      };
    }
    const selection = element.ownerDocument?.defaultView?.getSelection?.();
    return {
      selection_mode: "dom_selection",
      selection_start: null,
      selection_end: null,
      selected_text: selection ? String(selection.toString() || "") : ""
    };
  }

  function accessibleName(element) {
    const labelledBy = stringOrEmpty(element.getAttribute("aria-labelledby"));
    if (labelledBy) {
      const text = labelledBy
        .split(/\s+/)
        .map((id) => document.getElementById(id)?.textContent || "")
        .join(" ")
        .trim();
      if (text) {
        return text;
      }
    }
    for (const attr of ["aria-label", "alt", "title", "placeholder", "value"]) {
      const value = stringOrEmpty(element.getAttribute(attr));
      if (value) {
        return value;
      }
    }
    if (element.id) {
      const label = document.querySelector(`label[for="${cssEscapeLocal(element.id)}"]`);
      if (label?.textContent?.trim()) {
        return label.textContent;
      }
    }
    const wrappingLabel = element.closest?.("label");
    if (wrappingLabel?.textContent?.trim()) {
      return wrappingLabel.textContent;
    }
    return element.textContent || "";
  }

  function inferRole(element) {
    const explicit = stringOrEmpty(element.getAttribute("role")).toLowerCase();
    if (explicit) {
      return explicit;
    }
    const lower = tag(element);
    if (lower === "a" && element.hasAttribute("href")) {
      return "link";
    }
    if (lower === "button") {
      return "button";
    }
    if (lower === "select") {
      return "combobox";
    }
    if (lower === "form") {
      return "form";
    }
    if (lower === "textarea") {
      return "textbox";
    }
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset", "image"].includes(type)) {
        return "button";
      }
      if (type === "checkbox") {
        return "checkbox";
      }
      if (type === "radio") {
        return "radio";
      }
      return "textbox";
    }
    return lower || "element";
  }

  function elementSummary(element) {
    if (!(element instanceof Element)) {
      return null;
    }
    const lower = tag(element);
    const value = "value" in element ? String(element.value ?? "") : "";
    return {
      tag_name: lower,
      role: inferRole(element),
      id: String(element.id || ""),
      name_attr: String(element.getAttribute("name") || ""),
      type_attr: String(element.getAttribute("type") || ""),
      accessible_name: trimForReadback(accessibleName(element), 160),
      text: trimForReadback(element.textContent, 160),
      value_len: value.length,
      checked: "checked" in element ? Boolean(element.checked) : null,
      selected_index: lower === "select" ? element.selectedIndex : null,
      visible: isElementVisible(element),
      enabled: isElementEnabled(element)
    };
  }

  function isElementEnabled(element) {
    if (Boolean(element.disabled)) {
      return false;
    }
    return String(element.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
  }

  function isElementVisible(element) {
    if (!(element instanceof Element) || !element.isConnected) {
      return false;
    }
    const style = window.getComputedStyle(element);
    if (!style || style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
      return false;
    }
    if (Number(style.opacity) === 0) {
      return false;
    }
    const rects = element.getClientRects();
    return rects && rects.length > 0 && Array.from(rects).some((rect) => rect.width > 0 && rect.height > 0);
  }

  function scrollIntoViewIfPossible(element) {
    try {
      element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    } catch (_) {
      try {
        element.scrollIntoView();
      } catch (_) {
        // A failed scroll is not by itself proof the element is unclickable.
      }
    }
  }

  function textMatches(actual, expectedNormalized) {
    const actualNormalized = normalizeText(actual);
    return actualNormalized === expectedNormalized || actualNormalized.includes(expectedNormalized);
  }

  function valueMatches(element, expected) {
    const expectedNormalized = normalizeText(expected);
    const values = [
      "value" in element ? String(element.value ?? "") : "",
      element.getAttribute("value") || "",
      element.getAttribute("data-value") || "",
      element.textContent || ""
    ];
    return values.some((value) => normalizeText(value) === expectedNormalized);
  }

  function readPageText(maxChars) {
    const source =
      document.body && typeof document.body.innerText === "string"
        ? document.body.innerText
        : document.documentElement && typeof document.documentElement.innerText === "string"
          ? document.documentElement.innerText
          : document.body && typeof document.body.textContent === "string"
            ? document.body.textContent
            : document.documentElement && typeof document.documentElement.textContent === "string"
              ? document.documentElement.textContent
              : "";
    let text = "";
    let textLen = 0;
    for (const ch of String(source || "")) {
      if (textLen < maxChars) {
        text += ch;
      }
      textLen += 1;
    }
    return {
      text,
      text_len: textLen,
      text_truncated: textLen > maxChars,
      max_chars: maxChars
    };
  }

  function locatorDebug(loc) {
    return JSON.stringify({
      selector: loc.selector || null,
      elementId: loc.elementId || null,
      role: loc.role || null,
      name: loc.name || null,
      value: loc.value || null,
      option: loc.option || null,
      optionLabel: loc.optionLabel || null,
      optionIndex: loc.optionIndex,
      options_count: Array.isArray(loc.options) ? loc.options.length : 0
    });
  }

  function normalizeText(value) {
    return String(value || "").replace(/\s+/g, " ").trim().toLowerCase();
  }

  function stringOrEmpty(value) {
    return value === null || value === undefined ? "" : String(value).trim();
  }

  function plainObjectOrEmpty(value) {
    if (value === null || value === undefined) {
      return {};
    }
    if (typeof value !== "object" || Array.isArray(value)) {
      return {};
    }
    const out = {};
    for (const [key, val] of Object.entries(value)) {
      out[key] = val;
    }
    return out;
  }

  function trimForReadback(value, maxChars) {
    const text = String(value || "").replace(/\s+/g, " ").trim();
    return [...text].slice(0, maxChars).join("");
  }

  function tag(element) {
    return String(element?.tagName || "").toLowerCase();
  }

  function cssEscapeLocal(value) {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return String(value).replace(/["\\]/g, "\\$&");
  }

  function actionError(code, detail) {
    const error = new Error(detail);
    error.code = code;
    return error;
  }

  function errorMessageLocal(error) {
    return error && error.message ? String(error.message) : String(error);
  }

  function fail(errorCode, errorDetail, extra = {}) {
    return {
      ok: false,
      error_code: errorCode,
      error_detail: errorDetail,
      ...extra
    };
  }
}

function performCoordinateClickInPage(request) {
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const ERROR_ELEMENT_NOT_ACTIONABLE = "CHROME_DOM_ELEMENT_NOT_ACTIONABLE";
  const ERROR_ACTION_UNSUPPORTED = "CHROME_DOM_ACTION_UNSUPPORTED";

  const x = Number(request?.x);
  const y = Number(request?.y);
  const coordinateSpace = String(request?.coordinateSpace || "screen").toLowerCase();
  const clicks = Number.isSafeInteger(request?.clicks) ? Math.min(Math.max(request.clicks, 1), 3) : 1;
  const button = normalizeMouseButtonLocal(request?.button);
  const modifiers = normalizeClickModifiersLocal(request?.modifiers);
  const maxPageTextChars = Number.isSafeInteger(request?.maxPageTextChars)
    ? Math.min(Math.max(request.maxPageTextChars, 0), 65536)
    : 4096;

  if (!Number.isSafeInteger(x) || !Number.isSafeInteger(y)) {
    return fail(ERROR_ACTION_UNSUPPORTED, `coordinateClick requires integer x/y, got (${JSON.stringify(request?.x)}, ${JSON.stringify(request?.y)})`);
  }
  if (!["screen", "window", "viewport"].includes(coordinateSpace)) {
    return fail(ERROR_ACTION_UNSUPPORTED, `unsupported coordinate space ${JSON.stringify(coordinateSpace)}`);
  }

  const beforeUrl = String(location.href || "");
  const beforePageText = readPageTextLocal(maxPageTextChars);
  const beforeActiveElement = readActiveElementLocal();
  const viewportPoint = toViewportPoint(x, y, coordinateSpace);
  const viewportReadback = {
    inner_width: Math.trunc(window.innerWidth || 0),
    inner_height: Math.trunc(window.innerHeight || 0),
    outer_width: Math.trunc(window.outerWidth || 0),
    outer_height: Math.trunc(window.outerHeight || 0),
    screen_x: Math.trunc(window.screenX || 0),
    screen_y: Math.trunc(window.screenY || 0),
    device_pixel_ratio: Number(window.devicePixelRatio || 1),
    visual_viewport_offset_left: Number(window.visualViewport?.offsetLeft || 0),
    visual_viewport_offset_top: Number(window.visualViewport?.offsetTop || 0),
    visual_viewport_scale: Number(window.visualViewport?.scale || 1)
  };

  if (
    viewportPoint.client_x < 0 ||
    viewportPoint.client_y < 0 ||
    viewportPoint.client_x >= window.innerWidth ||
    viewportPoint.client_y >= window.innerHeight
  ) {
    return fail(
      ERROR_ELEMENT_NOT_FOUND,
      `coordinate (${x}, ${y}) in ${coordinateSpace} space maps outside viewport (${viewportPoint.client_x}, ${viewportPoint.client_y}) for inner size ${window.innerWidth}x${window.innerHeight}`,
      {
        coordinate_space: coordinateSpace,
        input_coordinate: { x, y },
        viewport_coordinate: viewportPoint,
        viewport_readback: viewportReadback,
        before_active_element: beforeActiveElement,
        in_page_before_text: beforePageText
      }
    );
  }

  const element = document.elementFromPoint(viewportPoint.client_x, viewportPoint.client_y);
  if (!(element instanceof Element)) {
    return fail(ERROR_ELEMENT_NOT_FOUND, "document.elementFromPoint returned no element at the requested coordinate", {
      coordinate_space: coordinateSpace,
      input_coordinate: { x, y },
      viewport_coordinate: viewportPoint,
      viewport_readback: viewportReadback,
      before_active_element: beforeActiveElement,
      in_page_before_text: beforePageText
    });
  }

  const beforeElement = elementSummary(element);
  if (!isElementEnabled(element)) {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "coordinate resolved to a disabled or aria-disabled element", {
      coordinate_space: coordinateSpace,
      input_coordinate: { x, y },
      viewport_coordinate: viewportPoint,
      viewport_readback: viewportReadback,
      before_element: beforeElement,
      before_active_element: beforeActiveElement,
      in_page_before_text: beforePageText
    });
  }
  if (!isElementVisible(element)) {
    return fail(ERROR_ELEMENT_NOT_ACTIONABLE, "coordinate resolved to a non-visible element", {
      coordinate_space: coordinateSpace,
      input_coordinate: { x, y },
      viewport_coordinate: viewportPoint,
      viewport_readback: viewportReadback,
      before_element: beforeElement,
      before_active_element: beforeActiveElement,
      in_page_before_text: beforePageText
    });
  }

  const eventsDispatched = [];
  try {
    if (typeof element.focus === "function") {
      element.focus({ preventScroll: true });
      eventsDispatched.push("focus");
    }
  } catch (_) {
    // Some elements reject focus; the click event may still be actionable.
  }
  for (let index = 0; index < clicks; index += 1) {
    const detail = index + 1;
    dispatchPointerLikeEvent(element, "pointerdown", viewportPoint, button, modifiers, detail, true);
    eventsDispatched.push("pointerdown");
    dispatchMouseLikeEvent(element, "mousedown", viewportPoint, button, modifiers, detail, true);
    eventsDispatched.push("mousedown");
    dispatchPointerLikeEvent(element, "pointerup", viewportPoint, button, modifiers, detail, false);
    eventsDispatched.push("pointerup");
    dispatchMouseLikeEvent(element, "mouseup", viewportPoint, button, modifiers, detail, false);
    eventsDispatched.push("mouseup");
    const activationEvent = activationEventForButton(button);
    dispatchMouseLikeEvent(element, activationEvent, viewportPoint, button, modifiers, detail, false);
    eventsDispatched.push(activationEvent);
    if (button === "left" && detail === 2 && clicks >= 2) {
      dispatchMouseLikeEvent(element, "dblclick", viewportPoint, button, modifiers, detail, false);
      eventsDispatched.push("dblclick");
    }
  }

  const afterElement = element.isConnected ? elementSummary(element) : null;
  const afterActiveElement = readActiveElementLocal();
  const afterPageText = readPageTextLocal(maxPageTextChars);
  return {
    ok: true,
    action: "coordinate_click",
    coordinate_space: coordinateSpace,
    input_coordinate: { x, y },
    viewport_coordinate: viewportPoint,
    viewport_readback: viewportReadback,
    click_count: clicks,
    button,
    modifiers,
    before_element: beforeElement,
    after_element: afterElement,
    before_active_element: beforeActiveElement,
    after_active_element: afterActiveElement,
    events_dispatched: eventsDispatched,
    in_page_before_url: beforeUrl,
    in_page_after_url: String(location.href || ""),
    in_page_title: String(document.title || ""),
    in_page_ready_state: String(document.readyState || ""),
    in_page_before_text: beforePageText,
    in_page_after_text: afterPageText
  };

  function toViewportPoint(inputX, inputY, space) {
    if (space === "viewport") {
      return {
        client_x: inputX,
        client_y: inputY,
        conversion: "identity_viewport"
      };
    }
    const horizontalChrome = Math.max(0, (Number(window.outerWidth || 0) - Number(window.innerWidth || 0)) / 2);
    const verticalChrome = Math.max(0, Number(window.outerHeight || 0) - Number(window.innerHeight || 0) - horizontalChrome);
    if (space === "window") {
      return {
        client_x: Math.round(inputX - horizontalChrome),
        client_y: Math.round(inputY - verticalChrome),
        conversion: "window_outer_to_viewport",
        horizontal_chrome_px: horizontalChrome,
        vertical_chrome_px: verticalChrome
      };
    }
    return {
      client_x: Math.round(inputX - Number(window.screenX || 0) - horizontalChrome),
      client_y: Math.round(inputY - Number(window.screenY || 0) - verticalChrome),
      conversion: "screen_to_viewport",
      horizontal_chrome_px: horizontalChrome,
      vertical_chrome_px: verticalChrome
    };
  }

  function dispatchPointerLikeEvent(element, eventType, point, button, modifiers, detail, pressed) {
    if (typeof PointerEvent === "function") {
      element.dispatchEvent(new PointerEvent(eventType, pointerEventInit(point, button, modifiers, detail, pressed)));
      return;
    }
    dispatchMouseLikeEvent(element, eventType.replace(/^pointer/, "mouse"), point, button, modifiers, detail, pressed);
  }

  function dispatchMouseLikeEvent(element, eventType, point, button, modifiers, detail, pressed) {
    element.dispatchEvent(new MouseEvent(eventType, mouseEventInit(point, button, modifiers, detail, pressed)));
  }

  function pointerEventInit(point, button, modifiers, detail, pressed) {
    return {
      ...mouseEventInit(point, button, modifiers, detail, pressed),
      pointerId: 1,
      pointerType: "mouse",
      isPrimary: true,
      width: 1,
      height: 1,
      pressure: eventPressure(pressed)
    };
  }

  function mouseEventInit(point, button, modifiers, detail, pressed) {
    return {
      bubbles: true,
      cancelable: true,
      composed: true,
      view: window,
      detail,
      button: mouseButtonCode(button),
      buttons: pressed ? mouseButtonBits(button) : 0,
      clientX: point.client_x,
      clientY: point.client_y,
      screenX: coordinateSpace === "screen" ? x : Math.round(Number(window.screenX || 0) + point.client_x),
      screenY: coordinateSpace === "screen" ? y : Math.round(Number(window.screenY || 0) + point.client_y),
      ...modifierFlags(modifiers)
    };
  }

  function eventPressure(pressed) {
    return pressed ? 0.5 : 0;
  }

  function activationEventForButton(button) {
    if (button === "right") {
      return "contextmenu";
    }
    if (button === "middle") {
      return "auxclick";
    }
    return "click";
  }

  function mouseButtonCode(button) {
    if (button === "middle") {
      return 1;
    }
    if (button === "right") {
      return 2;
    }
    return 0;
  }

  function mouseButtonBits(button) {
    if (button === "middle") {
      return 4;
    }
    if (button === "right") {
      return 2;
    }
    return 1;
  }

  function modifierFlags(modifiers) {
    const set = new Set(Array.isArray(modifiers) ? modifiers : []);
    return {
      altKey: set.has("alt"),
      ctrlKey: set.has("ctrl"),
      metaKey: set.has("meta"),
      shiftKey: set.has("shift")
    };
  }

  function normalizeMouseButtonLocal(value) {
    const normalized = String(value || "left").trim().toLowerCase();
    return ["left", "right", "middle"].includes(normalized) ? normalized : "left";
  }

  function normalizeClickModifiersLocal(value) {
    if (!Array.isArray(value)) {
      return [];
    }
    const seen = new Set();
    for (const item of value) {
      const normalized = String(item || "").trim().toLowerCase();
      if (normalized === "ctrl" || normalized === "control") {
        seen.add("ctrl");
      } else if (normalized === "shift") {
        seen.add("shift");
      } else if (normalized === "alt" || normalized === "option") {
        seen.add("alt");
      } else if (["meta", "super", "cmd", "command"].includes(normalized)) {
        seen.add("meta");
      }
    }
    return ["alt", "ctrl", "meta", "shift"].filter((modifier) => seen.has(modifier));
  }

  function elementSummary(element) {
    if (!(element instanceof Element)) {
      return null;
    }
    const lower = tag(element);
    const value = "value" in element ? String(element.value ?? "") : "";
    const rect = element.getBoundingClientRect();
    return {
      tag_name: lower,
      role: inferRole(element),
      id: String(element.id || ""),
      name_attr: String(element.getAttribute("name") || ""),
      type_attr: String(element.getAttribute("type") || ""),
      accessible_name: trimForReadback(accessibleName(element), 160),
      text: trimForReadback(element.textContent, 160),
      value_len: value.length,
      checked: "checked" in element ? Boolean(element.checked) : null,
      selected_index: lower === "select" ? element.selectedIndex : null,
      visible: isElementVisible(element),
      enabled: isElementEnabled(element),
      bbox: {
        x: Math.round(rect.x),
        y: Math.round(rect.y),
        w: Math.round(rect.width),
        h: Math.round(rect.height)
      }
    };
  }

  function readActiveElementLocal() {
    return elementSummary(document.activeElement);
  }

  function readPageTextLocal(maxChars) {
    const source = document.body?.innerText || document.documentElement?.innerText || "";
    return trimForReadback(source, maxChars);
  }

  function isElementEnabled(element) {
    if (Boolean(element.disabled)) {
      return false;
    }
    return String(element.getAttribute("aria-disabled") || "").toLowerCase() !== "true";
  }

  function isElementVisible(element) {
    if (!(element instanceof Element) || !element.isConnected) {
      return false;
    }
    const style = window.getComputedStyle(element);
    if (!style || style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
      return false;
    }
    if (Number(style.opacity) === 0) {
      return false;
    }
    const rects = element.getClientRects();
    return rects && rects.length > 0 && Array.from(rects).some((rect) => rect.width > 0 && rect.height > 0);
  }

  function accessibleName(element) {
    const labelledBy = stringOrEmpty(element.getAttribute("aria-labelledby"));
    if (labelledBy) {
      const text = labelledBy
        .split(/\s+/)
        .map((id) => document.getElementById(id)?.textContent || "")
        .join(" ")
        .trim();
      if (text) {
        return text;
      }
    }
    for (const attr of ["aria-label", "alt", "title", "placeholder", "value"]) {
      const value = stringOrEmpty(element.getAttribute(attr));
      if (value) {
        return value;
      }
    }
    if (element.id) {
      const label = document.querySelector(`label[for="${cssEscapeLocal(element.id)}"]`);
      if (label?.textContent?.trim()) {
        return label.textContent;
      }
    }
    const wrappingLabel = element.closest?.("label");
    if (wrappingLabel?.textContent?.trim()) {
      return wrappingLabel.textContent;
    }
    return element.textContent || "";
  }

  function inferRole(element) {
    const explicit = stringOrEmpty(element.getAttribute("role")).toLowerCase();
    if (explicit) {
      return explicit;
    }
    const lower = tag(element);
    if (lower === "a" && element.hasAttribute("href")) {
      return "link";
    }
    if (lower === "button") {
      return "button";
    }
    if (lower === "select") {
      return "combobox";
    }
    if (lower === "form") {
      return "form";
    }
    if (lower === "textarea") {
      return "textbox";
    }
    if (lower === "input") {
      const type = String(element.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset", "image"].includes(type)) {
        return "button";
      }
      if (type === "checkbox") {
        return "checkbox";
      }
      if (type === "radio") {
        return "radio";
      }
      return "textbox";
    }
    return lower || "element";
  }

  function normalizeText(value) {
    return String(value || "").replace(/\s+/g, " ").trim().toLowerCase();
  }

  function stringOrEmpty(value) {
    return value === null || value === undefined ? "" : String(value).trim();
  }

  function trimForReadback(value, maxChars) {
    const text = String(value || "").replace(/\s+/g, " ").trim();
    return [...text].slice(0, maxChars).join("");
  }

  function tag(element) {
    return String(element?.tagName || "").toLowerCase();
  }

  function cssEscapeLocal(value) {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return String(value).replace(/["\\]/g, "\\$&");
  }

  function fail(errorCode, errorDetail, extra = {}) {
    return {
      ok: false,
      error_code: errorCode,
      error_detail: errorDetail,
      ...extra
    };
  }
}

function tabsNavigationMethod(action) {
  if (action === "navigate") {
    return "update";
  }
  if (action === "reload") {
    return "reload";
  }
  if (action === "back") {
    return "goBack";
  }
  if (action === "forward") {
    return "goForward";
  }
  return String(action);
}

function normalizeNavigateAction(action) {
  const normalized = String(action || "").toLowerCase();
  if (["navigate", "reload", "back", "forward"].includes(normalized)) {
    return normalized;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported navigation action ${String(action)}`);
}

function normalizeOptionalSessionId(value) {
  const sessionId = String(value || "").trim();
  return sessionId || null;
}

function markAgentNavigation(tabId, claim) {
  if (!Number.isInteger(tabId) || !claim?.sessionId) {
    return;
  }
  agentNavigationClaims.set(tabId, {
    action: String(claim.action || ""),
    requestedUrl: claim.requestedUrl === null ? null : String(claim.requestedUrl || ""),
    sessionId: claim.sessionId,
    at: Date.now()
  });
}

function matchingAgentNavigationClaim(tabId, url, status) {
  const claim = agentNavigationClaims.get(tabId);
  if (!claim) {
    return null;
  }
  const ageMs = Date.now() - claim.at;
  if (ageMs > AGENT_NAVIGATION_CLAIM_TTL_MS) {
    agentNavigationClaims.delete(tabId);
    return null;
  }
  const requestedUrl = claim.requestedUrl || "";
  const matchesUrl = !requestedUrl || !url || url === requestedUrl || url.startsWith(requestedUrl) || requestedUrl.startsWith(url);
  if (!matchesUrl) {
    return null;
  }
  if (status === "complete") {
    agentNavigationClaims.delete(tabId);
  }
  return claim;
}

async function performHtml5DragDropInPage(request) {
  const ERROR_SELECTOR_INVALID = "CHROME_DOM_SELECTOR_INVALID";
  const ERROR_ELEMENT_NOT_FOUND = "CHROME_DOM_ELEMENT_NOT_FOUND";
  const ERROR_ELEMENT_AMBIGUOUS = "CHROME_DOM_ELEMENT_AMBIGUOUS";
  const ERROR_ACTION_UNSUPPORTED = "CHROME_DOM_ACTION_UNSUPPORTED";

  function fail(error_code, error_detail, extra = {}) {
    return {
      ok: false,
      error_code,
      error_detail,
      ...extra
    };
  }

  function strictSelector(selector, label) {
    const value = String(selector || "").trim();
    if (!value) {
      throw { code: ERROR_SELECTOR_INVALID, message: `${label} selector must be non-empty` };
    }
    let matches;
    try {
      matches = Array.from(document.querySelectorAll(value));
    } catch (error) {
      throw { code: ERROR_SELECTOR_INVALID, message: `${label} selector ${JSON.stringify(value)} is invalid: ${error?.message || error}` };
    }
    if (matches.length === 0) {
      throw { code: ERROR_ELEMENT_NOT_FOUND, message: `${label} selector ${JSON.stringify(value)} matched no elements` };
    }
    if (matches.length > 1) {
      throw { code: ERROR_ELEMENT_AMBIGUOUS, message: `${label} selector ${JSON.stringify(value)} matched ${matches.length} elements` };
    }
    return matches[0];
  }

  function summary(element) {
    if (!(element instanceof Element)) {
      return null;
    }
    return {
      tag_name: String(element.tagName || "").toLowerCase(),
      id: String(element.id || ""),
      text: String(element.textContent || "").trim().slice(0, 200),
      class_name: String(element.className || "")
    };
  }

  function pageText() {
    return String(document.body?.innerText || document.documentElement?.innerText || "").slice(0, 4096);
  }

  function dispatchDragEvent(element, type, dataTransfer) {
    const event = new DragEvent(type, {
      bubbles: true,
      cancelable: true,
      composed: true,
      dataTransfer
    });
    return element.dispatchEvent(event);
  }

  try {
    const source = strictSelector(request?.sourceSelector, "source");
    const target = strictSelector(request?.targetSelector, "target");
    if (typeof DataTransfer !== "function" || typeof DragEvent !== "function") {
      return fail(ERROR_ACTION_UNSUPPORTED, "DataTransfer and DragEvent constructors are required for html5_drag");
    }
    const dataTransfer = new DataTransfer();
    const data = request?.data || {};
    const mimeType = String(data.mimeType || "text/plain");
    const hasRequestedText = typeof data.text === "string" && data.text.length > 0;
    const text = hasRequestedText ? String(data.text) : String(source.textContent || "");
    dataTransfer.setData(mimeType, text);

    const events = [];
    const dragstartDefaultAllowed = dispatchDragEvent(source, "dragstart", dataTransfer);
    events.push("dragstart");
    if (hasRequestedText) {
      dataTransfer.setData(mimeType, text);
    }
    const dragenterDefaultAllowed = dispatchDragEvent(target, "dragenter", dataTransfer);
    events.push("dragenter");
    const dragoverDefaultAllowed = dispatchDragEvent(target, "dragover", dataTransfer);
    events.push("dragover");
    const dropDefaultAllowed = dispatchDragEvent(target, "drop", dataTransfer);
    events.push("drop");
    const dragendDefaultAllowed = dispatchDragEvent(source, "dragend", dataTransfer);
    events.push("dragend");

    return {
      ok: true,
      action: "html5_drag",
      events_dispatched: events,
      source_selector: String(request.sourceSelector || ""),
      target_selector: String(request.targetSelector || ""),
      source_before: summary(source),
      target_before: summary(target),
      source_after: summary(source),
      target_after: summary(target),
      data_transfer_types: Array.from(dataTransfer.types || []),
      data_transfer_text: dataTransfer.getData(mimeType),
      data_transfer_reapplied_after_dragstart: hasRequestedText,
      dragstart_default_allowed: dragstartDefaultAllowed,
      dragenter_default_allowed: dragenterDefaultAllowed,
      dragover_default_allowed: dragoverDefaultAllowed,
      dragover_default_prevented: !dragoverDefaultAllowed,
      drop_default_allowed: dropDefaultAllowed,
      dragend_default_allowed: dragendDefaultAllowed,
      in_page_after_text: pageText()
    };
  } catch (error) {
    return fail(error?.code || ERROR_ACTION_UNSUPPORTED, error?.message || String(error || "html5_drag failed"));
  }
}

function ensurePageEventBuffer(tabId, state = {}) {
  let buffer = pageEventBuffers.get(tabId);
  if (buffer) {
    updatePageSnapshot(buffer, state);
    return { buffer, newlyArmed: false };
  }
  const now = Date.now();
  const targetId = state.target_id || targetIdForTabId(tabId);
  buffer = {
    tabId,
    targetId,
    capacity: MAX_PAGE_EVENT_BUFFER,
    entries: [],
    pages: new Map(),
    workers: new Map(),
    nextSeq: 0,
    dropped: 0,
    armedAtUnixMs: now
  };
  pageEventBuffers.set(tabId, buffer);
  updatePageSnapshot(buffer, state);
  return { buffer, newlyArmed: true };
}

function ensureNetworkEventBuffer(tabId, state = {}) {
  let buffer = networkEventBuffers.get(tabId);
  if (buffer) {
    return { buffer, newlyArmed: false };
  }
  const now = Date.now();
  buffer = {
    tabId,
    targetId: state.target_id || targetIdForTabId(tabId),
    capacity: MAX_NETWORK_EVENT_BUFFER,
    entries: [],
    requests: new Map(),
    nextSeq: 0,
    dropped: 0,
    armedAtUnixMs: now
  };
  networkEventBuffers.set(tabId, buffer);
  return { buffer, newlyArmed: true };
}

function networkBufferForTabId(tabId) {
  if (!Number.isInteger(tabId) || tabId < 0) {
    return null;
  }
  return networkEventBuffers.get(tabId) || null;
}

function recordWebRequestStarted(details) {
  const buffer = networkBufferForTabId(details?.tabId);
  if (!buffer) {
    return;
  }
  const entry = ensureNetworkEntry(buffer, details);
  entry.url = String(details.url || entry.url || "");
  entry.method = String(details.method || entry.method || "").toUpperCase() || null;
  entry.resource_type = webRequestResourceTypeToCdp(details.type);
  entry.web_request_type = String(details.type || "");
  touchNetworkEntry(buffer, entry);
}

function recordWebRequestHeadersReceived(details) {
  const buffer = networkBufferForTabId(details?.tabId);
  if (!buffer) {
    return;
  }
  const entry = ensureNetworkEntry(buffer, details);
  entry.response_received = true;
  entry.response_url = String(details.url || entry.response_url || entry.url || "");
  if (entry.url === null) {
    entry.url = entry.response_url;
  }
  entry.status = Number.isFinite(Number(details.statusCode)) ? Number(details.statusCode) : entry.status;
  entry.status_text = statusTextFromWebRequest(details.statusLine, entry.status);
  entry.protocol = protocolFromStatusLine(details.statusLine) || entry.protocol;
  entry.remote_ip_address = details.ip || entry.remote_ip_address || null;
  entry.response_headers = headersArrayToObject(details.responseHeaders);
  touchNetworkEntry(buffer, entry);
}

function recordWebRequestCompleted(details) {
  const buffer = networkBufferForTabId(details?.tabId);
  if (!buffer) {
    return;
  }
  const entry = ensureNetworkEntry(buffer, details);
  entry.response_received = true;
  entry.response_url = String(details.url || entry.response_url || entry.url || "");
  if (entry.url === null) {
    entry.url = entry.response_url;
  }
  entry.status = Number.isFinite(Number(details.statusCode)) ? Number(details.statusCode) : entry.status;
  entry.status_text = statusTextFromWebRequest(details.statusLine, entry.status);
  entry.protocol = protocolFromStatusLine(details.statusLine) || entry.protocol;
  entry.remote_ip_address = details.ip || entry.remote_ip_address || null;
  entry.loading_finished = true;
  entry.loading_failed = false;
  entry.failure_error_text = null;
  touchNetworkEntry(buffer, entry);
}

function recordWebRequestFailed(details) {
  const buffer = networkBufferForTabId(details?.tabId);
  if (!buffer) {
    return;
  }
  const entry = ensureNetworkEntry(buffer, details);
  entry.url = String(details.url || entry.url || "");
  entry.method = String(details.method || entry.method || "").toUpperCase() || entry.method;
  entry.resource_type = entry.resource_type || webRequestResourceTypeToCdp(details.type);
  entry.web_request_type = entry.web_request_type || String(details.type || "");
  entry.loading_finished = false;
  entry.loading_failed = true;
  entry.failure_error_text = details.error || "webRequest.onErrorOccurred";
  touchNetworkEntry(buffer, entry);
}

function ensureNetworkEntry(buffer, details) {
  const requestId = String(details?.requestId || `${buffer.tabId}:${buffer.nextSeq + 1}`);
  let entry = buffer.requests.get(requestId);
  if (entry) {
    return entry;
  }
  entry = {
    seq: 0,
    request_id: requestId,
    url: details?.url ? String(details.url) : null,
    method: details?.method ? String(details.method).toUpperCase() : null,
    resource_type: webRequestResourceTypeToCdp(details?.type),
    web_request_type: String(details?.type || ""),
    request_headers: null,
    response_received: false,
    response_url: null,
    status: null,
    status_text: null,
    response_headers: null,
    response_timing: null,
    protocol: null,
    remote_ip_address: null,
    remote_port: null,
    encoded_data_length: null,
    loading_finished: false,
    loading_failed: false,
    failure_error_text: null
  };
  buffer.requests.set(requestId, entry);
  pushNetworkEntry(buffer, entry);
  return entry;
}

function pushNetworkEntry(buffer, entry) {
  buffer.entries.push(entry);
  while (buffer.entries.length > buffer.capacity) {
    const dropped = buffer.entries.shift();
    buffer.dropped += 1;
    if (dropped && buffer.requests.get(dropped.request_id) === dropped) {
      buffer.requests.delete(dropped.request_id);
    }
  }
}

function touchNetworkEntry(buffer, entry) {
  buffer.nextSeq += 1;
  entry.seq = buffer.nextSeq;
  entry.observed_at_unix_ms = Date.now();
}

function webRequestResourceTypeToCdp(type) {
  const value = String(type || "").toLowerCase();
  switch (value) {
    case "main_frame":
    case "sub_frame":
      return "Document";
    case "stylesheet":
      return "Stylesheet";
    case "script":
      return "Script";
    case "image":
      return "Image";
    case "font":
      return "Font";
    case "object":
      return "Other";
    case "xmlhttprequest":
      return "XHR";
    case "ping":
      return "Ping";
    case "csp_report":
      return "CSPViolationReport";
    case "media":
      return "Media";
    case "websocket":
      return "WebSocket";
    case "webtransport":
      return "WebTransport";
    case "webbundle":
      return "WebBundle";
    default:
      return value ? value.replace(/(^|_)([a-z])/g, (_, _prefix, letter) => letter.toUpperCase()) : null;
  }
}

function headersArrayToObject(headers) {
  if (!Array.isArray(headers) || headers.length === 0) {
    return null;
  }
  const result = {};
  for (const header of headers) {
    const name = String(header?.name || "").trim();
    if (!name) {
      continue;
    }
    const value = String(header?.value || "");
    if (Object.prototype.hasOwnProperty.call(result, name)) {
      if (Array.isArray(result[name])) {
        result[name].push(value);
      } else {
        result[name] = [result[name], value];
      }
    } else {
      result[name] = value;
    }
  }
  return Object.keys(result).length === 0 ? null : result;
}

function protocolFromStatusLine(statusLine) {
  const line = String(statusLine || "").trim();
  const match = line.match(/^(HTTP\/[0-9.]+|h[0-9]+)\s+/i);
  return match ? match[1] : null;
}

function statusTextFromWebRequest(statusLine, status) {
  const line = String(statusLine || "").trim();
  if (!line) {
    return null;
  }
  const escapedStatus = String(status ?? "").replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const regex = new RegExp(`^(?:HTTP\\/[0-9.]+|h[0-9]+)\\s+${escapedStatus}\\s*(.*)$`, "i");
  const match = line.match(regex);
  if (match) {
    return match[1] || "";
  }
  return line;
}

async function ensureInPageNetworkRecorder(tabId) {
  if (!chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    if (!chrome.webRequest?.onBeforeRequest?.addListener) {
      throw bridgeError(
        ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
        "network waits require chrome.webRequest or chrome.scripting.executeScript"
      );
    }
    return { available: false, reason: "chrome.scripting unavailable" };
  }
  try {
    const injected = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      func: installSynapseNetworkRecorderInPage
    });
    const first = Array.isArray(injected) ? injected[0]?.result : null;
    return {
      available: Boolean(first?.ok),
      reason: first?.reason || "installed",
      installed_at_seq: Number(first?.next_seq || 0)
    };
  } catch (error) {
    if (!chrome.webRequest?.onBeforeRequest?.addListener) {
      throw bridgeError(
        ERROR_CHROME_SCRIPTING_EXECUTE_FAILED,
        `network wait recorder injection failed and chrome.webRequest is unavailable: ${errorMessage(error)}`
      );
    }
    return { available: false, reason: errorMessage(error) };
  }
}

async function mergeInPageNetworkEvents(tabId, buffer, recorder) {
  if (!recorder?.available || !chrome.scripting || typeof chrome.scripting.executeScript !== "function") {
    return;
  }
  let injected;
  try {
    injected = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      func: readSynapseNetworkRecorderInPage
    });
  } catch (error) {
    recorder.available = false;
    recorder.reason = errorMessage(error);
    return;
  }
  const entries = Array.isArray(injected) ? injected.flatMap((item) => item?.result?.entries || []) : [];
  for (const raw of entries) {
    mergeInPageNetworkEntry(buffer, raw);
  }
}

function mergeInPageNetworkEntry(buffer, raw) {
  if (!raw || typeof raw !== "object") {
    return;
  }
  const requestId = `page:${String(raw.request_id || "")}`;
  if (requestId === "page:") {
    return;
  }
  let entry = buffer.requests.get(requestId);
  if (!entry) {
    entry = ensureNetworkEntry(buffer, {
      requestId,
      url: raw.url,
      method: raw.method,
      type: String(raw.resource_type || "").toLowerCase() === "xhr" ? "xmlhttprequest" : "fetch"
    });
  }
  const rawSeq = Number(raw.seq || 0);
  if (Number(entry.page_recorder_seq || 0) >= rawSeq) {
    return;
  }
  entry.page_recorder_seq = rawSeq;
  entry.url = raw.url ? String(raw.url) : entry.url;
  entry.method = raw.method ? String(raw.method).toUpperCase() : entry.method;
  entry.resource_type = raw.resource_type ? String(raw.resource_type) : entry.resource_type;
  entry.response_received = Boolean(raw.response_received);
  entry.response_url = raw.response_url ? String(raw.response_url) : entry.response_url;
  entry.status = Number.isFinite(Number(raw.status)) ? Number(raw.status) : entry.status;
  entry.status_text = raw.status_text === undefined || raw.status_text === null ? entry.status_text : String(raw.status_text);
  entry.loading_finished = Boolean(raw.loading_finished);
  entry.loading_failed = Boolean(raw.loading_failed);
  entry.failure_error_text = raw.failure_error_text ? String(raw.failure_error_text) : null;
  entry.response_headers = null;
  entry.request_headers = null;
  entry.response_timing = null;
  touchNetworkEntry(buffer, entry);
}

function installSynapseNetworkRecorderInPage() {
  const global = globalThis;
  if (global.__synapseBridgeNetworkRecorder?.installed) {
    return {
      ok: true,
      reason: "already_installed",
      next_seq: global.__synapseBridgeNetworkRecorder.nextSeq || 0
    };
  }
  const recorder = {
    installed: true,
    nextId: 0,
    nextSeq: 0,
    maxEntries: 1000,
    entries: [],
    byId: Object.create(null)
  };
  const touch = (entry) => {
    recorder.nextSeq += 1;
    entry.seq = recorder.nextSeq;
    entry.observed_at_unix_ms = Date.now();
  };
  const remember = (entry) => {
    recorder.byId[entry.request_id] = entry;
    recorder.entries.push(entry);
    while (recorder.entries.length > recorder.maxEntries) {
      const dropped = recorder.entries.shift();
      if (dropped && recorder.byId[dropped.request_id] === dropped) {
        delete recorder.byId[dropped.request_id];
      }
    }
    touch(entry);
    return entry;
  };
  const requestUrl = (input) => {
    try {
      if (typeof Request !== "undefined" && input instanceof Request) {
        return input.url;
      }
    } catch (_) {
      // Request can be unavailable in unusual page contexts.
    }
    try {
      return new URL(String(input), String(global.location?.href || "about:blank")).href;
    } catch (_) {
      return String(input || "");
    }
  };
  const requestMethod = (input, init) => {
    const method = init?.method || (typeof Request !== "undefined" && input instanceof Request ? input.method : null) || "GET";
    return String(method).toUpperCase();
  };
  if (typeof global.fetch === "function") {
    const originalFetch = global.fetch;
    global.fetch = function synapseRecordedFetch(input, init) {
      const entry = remember({
        request_id: `fetch-${++recorder.nextId}`,
        url: requestUrl(input),
        method: requestMethod(input, init),
        resource_type: "Fetch",
        response_received: false,
        response_url: null,
        status: null,
        status_text: null,
        loading_finished: false,
        loading_failed: false,
        failure_error_text: null
      });
      const promise = Reflect.apply(originalFetch, this, arguments);
      return Promise.resolve(promise).then(
        (response) => {
          entry.response_received = true;
          entry.response_url = response?.url || entry.url;
          entry.status = Number(response?.status || 0);
          entry.status_text = response?.statusText || "";
          entry.loading_finished = true;
          entry.loading_failed = false;
          entry.failure_error_text = null;
          touch(entry);
          return response;
        },
        (error) => {
          entry.loading_finished = false;
          entry.loading_failed = true;
          entry.failure_error_text = String(error?.message || error);
          touch(entry);
          throw error;
        }
      );
    };
  }
  if (typeof global.XMLHttpRequest === "function") {
    const OriginalXHR = global.XMLHttpRequest;
    global.XMLHttpRequest = function SynapseRecordedXMLHttpRequest() {
      const xhr = new OriginalXHR();
      let entry = null;
      const originalOpen = xhr.open;
      const originalSend = xhr.send;
      xhr.open = function synapseRecordedXhrOpen(method, url) {
        entry = remember({
          request_id: `xhr-${++recorder.nextId}`,
          url: requestUrl(url),
          method: String(method || "GET").toUpperCase(),
          resource_type: "XHR",
          response_received: false,
          response_url: null,
          status: null,
          status_text: null,
          loading_finished: false,
          loading_failed: false,
          failure_error_text: null
        });
        return Reflect.apply(originalOpen, xhr, arguments);
      };
      xhr.send = function synapseRecordedXhrSend() {
        if (!entry) {
          entry = remember({
            request_id: `xhr-${++recorder.nextId}`,
            url: "",
            method: "GET",
            resource_type: "XHR",
            response_received: false,
            response_url: null,
            status: null,
            status_text: null,
            loading_finished: false,
            loading_failed: false,
            failure_error_text: null
          });
        }
        xhr.addEventListener("load", () => {
          entry.response_received = true;
          entry.response_url = xhr.responseURL || entry.url;
          entry.status = Number(xhr.status || 0);
          entry.status_text = xhr.statusText || "";
          entry.loading_finished = true;
          entry.loading_failed = false;
          entry.failure_error_text = null;
          touch(entry);
        });
        xhr.addEventListener("error", () => {
          entry.loading_finished = false;
          entry.loading_failed = true;
          entry.failure_error_text = "XMLHttpRequest error";
          touch(entry);
        });
        xhr.addEventListener("abort", () => {
          entry.loading_finished = false;
          entry.loading_failed = true;
          entry.failure_error_text = "XMLHttpRequest abort";
          touch(entry);
        });
        return Reflect.apply(originalSend, xhr, arguments);
      };
      return xhr;
    };
  }
  global.__synapseBridgeNetworkRecorder = recorder;
  return { ok: true, reason: "installed", next_seq: recorder.nextSeq };
}

function readSynapseNetworkRecorderInPage() {
  const recorder = globalThis.__synapseBridgeNetworkRecorder;
  if (!recorder?.installed) {
    return { ok: false, entries: [] };
  }
  return {
    ok: true,
    next_seq: recorder.nextSeq || 0,
    entries: Array.isArray(recorder.entries) ? recorder.entries.map((entry) => ({ ...entry })) : []
  };
}

function updatePageSnapshot(buffer, state = {}) {
  if (!buffer) {
    return;
  }
  const now = Date.now();
  const targetId = buffer.targetId || state.target_id || targetIdForTabId(buffer.tabId);
  const existing = buffer.pages.get(targetId);
  const snapshot = {
    target_id: targetId,
    target_type: "page",
    url: String(state.url || existing?.url || ""),
    title: String(state.title || existing?.title || ""),
    opener_id: existing?.opener_id || null,
    opener_frame_id: existing?.opener_frame_id || null,
    can_access_opener: Boolean(existing?.can_access_opener),
    browser_context_id: existing?.browser_context_id || null,
    subtype: existing?.subtype || null,
    attached: false,
    destroyed: false,
    first_seen_seq: Number.isSafeInteger(existing?.first_seen_seq) ? existing.first_seen_seq : buffer.nextSeq,
    last_seen_seq: buffer.nextSeq,
    first_seen_unix_ms: Number.isSafeInteger(existing?.first_seen_unix_ms) ? existing.first_seen_unix_ms : now,
    last_seen_unix_ms: now
  };
  buffer.pages.set(targetId, snapshot);
}

function pushPageEvent(tabId, entry) {
  const buffer = pageEventBuffers.get(tabId);
  if (!buffer || !entry || typeof entry !== "object") {
    return null;
  }
  const now = Date.now();
  const event = {
    seq: buffer.nextSeq,
    event_kind: String(entry.event_kind || ""),
    target_id: String(entry.target_id || buffer.targetId || targetIdForTabId(tabId)),
    target_type: entry.target_type == null ? null : String(entry.target_type),
    target_attached: entry.target_attached == null ? null : Boolean(entry.target_attached),
    page_target_id: entry.page_target_id == null ? null : String(entry.page_target_id),
    opener_id: entry.opener_id == null ? null : String(entry.opener_id),
    opener_frame_id: entry.opener_frame_id == null ? null : String(entry.opener_frame_id),
    can_access_opener: entry.can_access_opener == null ? null : Boolean(entry.can_access_opener),
    browser_context_id: entry.browser_context_id == null ? null : String(entry.browser_context_id),
    subtype: entry.subtype == null ? null : String(entry.subtype),
    worker_id: entry.worker_id == null ? null : String(entry.worker_id),
    worker_type: entry.worker_type == null ? null : String(entry.worker_type),
    worker_url: entry.worker_url == null ? null : String(entry.worker_url),
    frame_id: entry.frame_id == null ? null : String(entry.frame_id),
    parent_frame_id: entry.parent_frame_id == null ? null : String(entry.parent_frame_id),
    loader_id: entry.loader_id == null ? null : String(entry.loader_id),
    name: entry.name == null ? null : String(entry.name),
    url: entry.url == null ? null : String(entry.url),
    title: entry.title == null ? null : String(entry.title),
    navigation_type: entry.navigation_type == null ? null : String(entry.navigation_type),
    timestamp_s: Number.isFinite(Number(entry.timestamp_s)) ? Number(entry.timestamp_s) : null,
    observed_at_unix_ms: Number.isSafeInteger(entry.observed_at_unix_ms)
      ? entry.observed_at_unix_ms
      : now
  };
  if (!event.event_kind) {
    return null;
  }
  buffer.nextSeq += 1;
  if (event.page_target_id) {
    updateBufferedPageFromEvent(buffer, event);
  } else if (event.event_kind === "framenavigated" || event.event_kind === "load" || event.event_kind === "domcontentloaded") {
    updatePageSnapshot(buffer, {
      target_id: event.target_id,
      url: event.url || "",
      title: event.title || ""
    });
  }
  if (event.worker_id) {
    updateBufferedWorkerFromEvent(buffer, event);
  }
  while (buffer.entries.length >= buffer.capacity) {
    buffer.entries.shift();
    buffer.dropped += 1;
  }
  buffer.entries.push(event);
  return event;
}

function updateBufferedPageFromEvent(buffer, event) {
  const id = event.page_target_id;
  const existing = buffer.pages.get(id);
  const page = {
    target_id: id,
    target_type: event.target_type || existing?.target_type || "page",
    url: event.url || existing?.url || "",
    title: event.title || existing?.title || "",
    opener_id: event.opener_id || existing?.opener_id || null,
    opener_frame_id: event.opener_frame_id || existing?.opener_frame_id || null,
    can_access_opener: event.can_access_opener == null
      ? Boolean(existing?.can_access_opener)
      : Boolean(event.can_access_opener),
    browser_context_id: event.browser_context_id || existing?.browser_context_id || null,
    subtype: event.subtype || existing?.subtype || null,
    attached: event.target_attached == null ? Boolean(existing?.attached) : Boolean(event.target_attached),
    destroyed: event.event_kind === "page_destroyed" || Boolean(existing?.destroyed),
    first_seen_seq: Number.isSafeInteger(existing?.first_seen_seq) ? existing.first_seen_seq : event.seq,
    last_seen_seq: event.seq,
    first_seen_unix_ms: Number.isSafeInteger(existing?.first_seen_unix_ms) ? existing.first_seen_unix_ms : event.observed_at_unix_ms,
    last_seen_unix_ms: event.observed_at_unix_ms
  };
  if (event.event_kind === "page_attached") {
    page.attached = true;
  }
  if (event.event_kind === "page_destroyed") {
    page.attached = false;
  }
  buffer.pages.set(id, page);
}

function updateBufferedWorkerFromEvent(buffer, event) {
  const id = event.worker_id;
  const existing = buffer.workers.get(id);
  const worker = {
    worker_id: id,
    worker_type: event.worker_type || existing?.worker_type || "",
    url: event.worker_url || existing?.url || "",
    title: event.title || existing?.title || "",
    attached: event.event_kind !== "worker_destroyed",
    destroyed: event.event_kind === "worker_destroyed" || Boolean(existing?.destroyed),
    first_seen_seq: Number.isSafeInteger(existing?.first_seen_seq) ? existing.first_seen_seq : event.seq,
    last_seen_seq: event.seq,
    first_seen_unix_ms: Number.isSafeInteger(existing?.first_seen_unix_ms) ? existing.first_seen_unix_ms : event.observed_at_unix_ms,
    last_seen_unix_ms: event.observed_at_unix_ms
  };
  if (event.event_kind === "worker_destroyed") {
    worker.attached = false;
  }
  buffer.workers.set(id, worker);
}

function readPageEventBuffer(buffer, filters) {
  const entries = buffer.entries
    .filter((entry) => filters.sinceSeq == null || entry.seq >= filters.sinceSeq)
    .filter((entry) => !filters.eventKind || String(entry.event_kind).toLowerCase() === filters.eventKind)
    .filter((entry) => !filters.workerType || String(entry.worker_type || "").toLowerCase() === filters.workerType)
    .slice(0, filters.limit);
  const pages = Array.from(buffer.pages.values()).sort((left, right) =>
    String(left.target_id).localeCompare(String(right.target_id))
  );
  const workers = Array.from(buffer.workers.values()).sort((left, right) =>
    String(left.worker_id).localeCompare(String(right.worker_id))
  );
  return {
    entries,
    pages,
    workers,
    next_cursor: buffer.nextSeq,
    returned: entries.length,
    total_buffered: buffer.entries.length,
    dropped: buffer.dropped
  };
}

function updatePageEventTabState(tabId, tab, changeInfo = {}) {
  const buffer = pageEventBuffers.get(tabId);
  if (!tab) {
    return;
  }
  if (buffer) {
    updatePageSnapshot(buffer, tabPageStateFromTab(tab));
    if (changeInfo.title) {
      pushPageEvent(tabId, {
        event_kind: "page_info_changed",
        target_id: targetIdForTabId(tabId),
        page_target_id: targetIdForTabId(tabId),
        target_type: "page",
        target_attached: false,
        url: String(tab.pendingUrl || tab.url || ""),
        title: String(tab.title || ""),
        observed_at_unix_ms: Date.now()
      });
    }
  }
  updateRelatedPageEventTabState(tabId, tab, changeInfo);
}

function updateRelatedPageEventTabState(tabId, tab, changeInfo = {}) {
  const targetId = targetIdForTabId(tabId);
  if (!changeInfo?.url && !changeInfo?.title && changeInfo?.status !== "complete") {
    return;
  }
  for (const [ownerTabId, buffer] of pageEventBuffers.entries()) {
    if (ownerTabId === tabId || !buffer.pages.has(targetId)) {
      continue;
    }
    pushPageEvent(ownerTabId, {
      event_kind: "page_info_changed",
      target_id: buffer.targetId,
      page_target_id: targetId,
      target_type: "page",
      target_attached: false,
      url: String(tab.pendingUrl || tab.url || ""),
      title: String(tab.title || ""),
      observed_at_unix_ms: Date.now()
    });
  }
}

function recordTabCreatedForPageEvents(tab) {
  if (!tab || !Number.isInteger(tab.id) || !Number.isInteger(tab.openerTabId)) {
    return;
  }
  const openerBuffer = pageEventBuffers.get(tab.openerTabId);
  if (!openerBuffer) {
    return;
  }
  pushPageEvent(tab.openerTabId, {
    event_kind: "page_created",
    target_id: openerBuffer.targetId,
    target_type: "page",
    target_attached: false,
    page_target_id: targetIdForTabId(tab.id),
    opener_id: openerBuffer.targetId,
    can_access_opener: true,
    url: String(tab.pendingUrl || tab.url || ""),
    title: String(tab.title || ""),
    observed_at_unix_ms: Date.now()
  });
}

function recordTabRemovedForPageEvents(tabId) {
  for (const [ownerTabId, buffer] of pageEventBuffers.entries()) {
    const removedTargetId = targetIdForTabId(tabId);
    if (!buffer.pages.has(removedTargetId)) {
      continue;
    }
    pushPageEvent(ownerTabId, {
      event_kind: "page_destroyed",
      target_id: buffer.targetId,
      page_target_id: removedTargetId,
      observed_at_unix_ms: Date.now()
    });
  }
  pageEventBuffers.delete(tabId);
  networkEventBuffers.delete(tabId);
}

function recordWebNavigationPageEvent(eventKind, details, extra = {}) {
  if (!details || !Number.isInteger(details.tabId) || details.tabId < 0) {
    return;
  }
  const buffer = pageEventBuffers.get(details.tabId);
  if (!buffer) {
    return;
  }
  const frameId = Number.isSafeInteger(details.frameId) ? String(details.frameId) : null;
  const parentFrameId = Number.isSafeInteger(details.parentFrameId) && details.parentFrameId >= 0
    ? String(details.parentFrameId)
    : null;
  pushPageEvent(details.tabId, {
    event_kind: eventKind,
    target_id: buffer.targetId || targetIdForTabId(details.tabId),
    frame_id: frameId,
    parent_frame_id: parentFrameId,
    loader_id: details.documentId == null ? null : String(details.documentId),
    url: String(details.url || ""),
    navigation_type: extra.navigation_type || null,
    timestamp_s: Number.isFinite(details.timeStamp) ? details.timeStamp / 1000 : null,
    observed_at_unix_ms: Number.isFinite(details.timeStamp) ? Math.floor(details.timeStamp) : Date.now()
  });
}

function webNavigationTransition(details) {
  const transitionType = String(details?.transitionType || "");
  const qualifiers = Array.isArray(details?.transitionQualifiers)
    ? details.transitionQualifiers.map(String).sort()
    : [];
  if (!transitionType && qualifiers.length === 0) {
    return "";
  }
  return [transitionType, ...qualifiers].filter(Boolean).join("+");
}

async function postTabNavigationEvent(source, tab) {
  if (!tab || typeof tab.id !== "number") {
    return;
  }
  const url = String(tab.pendingUrl || tab.url || "");
  if (!url) {
    return;
  }
  const title = String(tab.title || "");
  const status = String(tab.status || "");
  const agentClaim = matchingAgentNavigationClaim(tab.id, url, status);
  if (agentClaim) {
    return;
  }
  const key = `${tab.id}\n${url}\n${title}\n${status}`;
  if (recentNavigationKeys.includes(key)) {
    return;
  }
  recentNavigationKeys.push(key);
  while (recentNavigationKeys.length > MAX_RECENT_NAVIGATION_KEYS) {
    recentNavigationKeys.shift();
  }
  await postDaemonMessage({
    type: "event",
    event: "tabNavigation",
    source,
    target_id: targetIdForTabId(tab.id),
    tab_id: tab.id,
    chrome_window_id: Number.isInteger(tab.windowId) ? tab.windowId : null,
    url,
    title,
    status,
    active: Boolean(tab.active),
    highlighted: Boolean(tab.highlighted),
    pinned: Boolean(tab.pinned),
    observed_at_unix_ms: Date.now()
  });
}

function requiredUrl(url) {
  const value = String(url ?? "");
  if (!value || value.trim() !== value || value.includes("\u0000")) {
    throw bridgeError(ERROR_ATTACH_FAILED, "Page.navigate requires a non-empty URL with no surrounding whitespace or NUL");
  }
  if (Array.from(value).length > 8192) {
    throw bridgeError(ERROR_ATTACH_FAILED, "Page.navigate URL must be at most 8192 Unicode scalar values");
  }
  return value;
}

function normalizeOpenUrl(url) {
  const value = String(url ?? "");
  if (!value) {
    return "";
  }
  if (value.trim() !== value || value.includes("\u0000")) {
    throw bridgeError(ERROR_ATTACH_FAILED, "chrome.tabs.create URL must have no surrounding whitespace or NUL");
  }
  if (Array.from(value).length > 8192) {
    throw bridgeError(ERROR_ATTACH_FAILED, "chrome.tabs.create URL must be at most 8192 Unicode scalar values");
  }
  return value;
}

function normalizeWaitTimeout(value) {
  if (value === undefined || value === null) {
    return 10000;
  }
  const number = Number(value);
  if (!Number.isInteger(number) || number < 1 || number > 30000) {
    throw bridgeError(ERROR_ATTACH_FAILED, "waitTimeoutMs must be an integer from 1 through 30000");
  }
  return number;
}

function normalizePollingInterval(value) {
  if (value === undefined || value === null) {
    return 100;
  }
  const number = Number(value);
  if (!Number.isInteger(number) || number < 10 || number > 5000) {
    throw bridgeError(ERROR_ATTACH_FAILED, "pollingIntervalMs must be an integer from 10 through 5000");
  }
  return number;
}

function normalizeWaitForTextState(value) {
  const state = String(value || "text_appears");
  if (state === "text_appears" || state === "text_gone" || state === "timeout") {
    return state;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported waitForText state ${JSON.stringify(state)}`);
}

function normalizeWaitForLoadStateState(value) {
  const state = String(value || "load").toLowerCase();
  if (state === "domcontentloaded" || state === "load" || state === "networkidle") {
    return state;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported waitForLoadState state ${JSON.stringify(state)}`);
}

function normalizeWaitForUrlMatchKind(value) {
  const matchKind = String(value || "exact").toLowerCase();
  if (matchKind === "exact" || matchKind === "glob" || matchKind === "regex") {
    return matchKind;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported waitForUrl matchKind ${JSON.stringify(matchKind)}`);
}

function normalizeNetworkWaitParams(params, requireResponse) {
  const url = params.url === undefined || params.url === null ? null : String(params.url);
  if (url !== null) {
    if (url.trim().length === 0) {
      throw bridgeError(ERROR_ATTACH_FAILED, "network wait url must be non-empty when supplied");
    }
    if (url.trim() !== url || url.includes("\u0000")) {
      throw bridgeError(ERROR_ATTACH_FAILED, "network wait url must have no surrounding whitespace or NUL");
    }
  }
  const matchKind = normalizeWaitForUrlMatchKind(params.matchKind);
  if (url === null && matchKind !== "exact") {
    throw bridgeError(ERROR_ATTACH_FAILED, "network wait matchKind requires url");
  }
  const method = params.method === undefined || params.method === null ? null : String(params.method).trim();
  if (method !== null && !/^[A-Za-z][A-Za-z0-9!#$%&'*+.^_`|~-]{0,63}$/.test(method)) {
    throw bridgeError(ERROR_ATTACH_FAILED, "network wait method must be a valid HTTP token");
  }
  const resourceType = params.resourceType === undefined || params.resourceType === null
    ? (params.resource_type === undefined || params.resource_type === null ? null : String(params.resource_type).trim())
    : String(params.resourceType).trim();
  if (resourceType !== null && !/^[A-Za-z][A-Za-z0-9_.:-]{0,63}$/.test(resourceType)) {
    throw bridgeError(ERROR_ATTACH_FAILED, "network wait resource_type must be a valid token");
  }
  let status = null;
  if (params.status !== undefined && params.status !== null) {
    status = Number(params.status);
    if (!Number.isInteger(status) || status < 0 || status > 999 || !requireResponse) {
      throw bridgeError(ERROR_ATTACH_FAILED, "network wait status must be an integer from 0 through 999 for response waits");
    }
  }
  return {
    url,
    matchKind,
    method: method ? method.toUpperCase() : null,
    status,
    resourceType: resourceType || null,
    timeoutMs: normalizeWaitTimeout(params.timeoutMs),
    pollingIntervalMs: normalizePollingInterval(params.pollingIntervalMs)
  };
}

function stringEqualsIgnoreCase(left, right) {
  return String(left || "").toLowerCase() === String(right || "").toLowerCase();
}

function normalizeWaitForSelectorState(value) {
  const state = String(value || "visible");
  if (state === "attached" || state === "visible" || state === "hidden" || state === "detached") {
    return state;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported waitForSelector state ${JSON.stringify(state)}`);
}

function normalizeDomActionAutoWaitTimeout(value, enabled) {
  if (!enabled) {
    return 2000;
  }
  if (value === undefined || value === null) {
    return 2000;
  }
  const number = Number(value);
  if (!Number.isInteger(number) || number < 50 || number > 30000) {
    throw bridgeError(ERROR_ATTACH_FAILED, "autoWaitTimeoutMs must be an integer from 50 through 30000");
  }
  return number;
}

function normalizeClockOperation(value) {
  const operation = String(value || "status");
  if (["status", "install", "setFixedTime", "fastForward", "pauseAt"].includes(operation)) {
    return operation;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported clock operation ${JSON.stringify(operation)}`);
}

function normalizePageEventsFilter(params) {
  const sinceSeq = optionalNonNegativeInteger(params.sinceSeq);
  const limit = normalizePageEventsLimit(params.limit);
  const eventKind = normalizeOptionalPageEventKind(params.eventKind);
  const workerType = normalizeOptionalWorkerType(params.workerType);
  return { sinceSeq, limit, eventKind, workerType };
}

function optionalNonNegativeInteger(value) {
  if (value === undefined || value === null || value === "") {
    return null;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0) {
    throw bridgeError(ERROR_ATTACH_FAILED, "sinceSeq must be a non-negative safe integer");
  }
  return number;
}

function normalizePageEventsLimit(value) {
  if (value === undefined || value === null) {
    return 100;
  }
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1 || number > MAX_PAGE_EVENT_BUFFER) {
    throw bridgeError(ERROR_ATTACH_FAILED, `limit must be an integer from 1 through ${MAX_PAGE_EVENT_BUFFER}`);
  }
  return number;
}

function normalizeOptionalPageEventKind(value) {
  const kind = String(value || "").trim().toLowerCase();
  if (!kind) {
    return null;
  }
  if ([
    "domcontentloaded",
    "load",
    "lifecycle",
    "framenavigated",
    "framestartednavigating",
    "framestoppedloading",
    "page_created",
    "page_attached",
    "page_destroyed",
    "page_info_changed",
    "worker_created",
    "worker_attached",
    "worker_destroyed",
    "worker_info_changed"
  ].includes(kind)) {
    return kind;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported page event kind ${JSON.stringify(value)}`);
}

function normalizeOptionalWorkerType(value) {
  const workerType = String(value || "").trim().toLowerCase();
  if (!workerType) {
    return null;
  }
  if (["worker", "service_worker", "shared_worker"].includes(workerType)) {
    return workerType;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported worker type ${JSON.stringify(value)}`);
}

function normalizeMaxContentBytes(value) {
  if (value === undefined || value === null) {
    return 2 * 1024 * 1024;
  }
  const number = Number(value);
  if (!Number.isInteger(number) || number < 0 || number > 2 * 1024 * 1024) {
    throw bridgeError(ERROR_ATTACH_FAILED, "maxBytes must be an integer from 0 through 2097152");
  }
  return number;
}

function normalizeCookieOperation(value) {
  const operation = String(value || "get").trim().toLowerCase();
  if (operation === "get" || operation === "set" || operation === "clear") {
    return operation;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported cookies operation ${JSON.stringify(value)}`);
}

function normalizeStorageOperation(value) {
  const operation = String(value || "get").trim().toLowerCase();
  if (["get", "set", "clear", "save_state", "load_state"].includes(operation)) {
    return operation;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported storage operation ${JSON.stringify(value)}`);
}

function normalizeStorageStore(value) {
  const store = String(value || "local").trim().toLowerCase();
  if (store === "local" || store === "session") {
    return store;
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported storage store ${JSON.stringify(value)}`);
}

function normalizeCookieUrl(rawUrl, fallbackUrl, operation) {
  const raw = rawUrl === undefined || rawUrl === null || String(rawUrl).trim() === ""
    ? String(fallbackUrl || "")
    : String(rawUrl);
  if (!raw) {
    if (operation === "get") {
      return "";
    }
    throw bridgeError(ERROR_ATTACH_FAILED, `cookies ${operation} requires url when the current tab has no HTTP(S) URL`);
  }
  let parsed;
  try {
    parsed = new URL(raw);
  } catch (_) {
    if (operation === "get") {
      return "";
    }
    throw bridgeError(ERROR_ATTACH_FAILED, `cookies ${operation} requires a valid HTTP(S) URL`);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    if (operation === "get") {
      return "";
    }
    throw bridgeError(ERROR_ATTACH_FAILED, `cookies ${operation} requires an http: or https: URL; got ${parsed.protocol}`);
  }
  return parsed.href;
}

function cookieGetAllDetails(params, url) {
  const details = {};
  if (url) {
    details.url = url;
  }
  if (params.name !== undefined && params.name !== null && String(params.name) !== "") {
    details.name = String(params.name);
  }
  if (params.domain !== undefined && params.domain !== null && String(params.domain) !== "") {
    details.domain = String(params.domain);
  }
  if (params.path !== undefined && params.path !== null && String(params.path) !== "") {
    details.path = String(params.path);
  }
  if (params.secure !== undefined && params.secure !== null) {
    details.secure = Boolean(params.secure);
  }
  if (params.session !== undefined && params.session !== null) {
    details.session = Boolean(params.session);
  }
  return details;
}

function cookieSetDetails(params, url) {
  if (!url) {
    throw bridgeError(ERROR_ATTACH_FAILED, "cookies set requires url");
  }
  const name = String(params.name || "").trim();
  if (!name) {
    throw bridgeError(ERROR_ATTACH_FAILED, "cookies set requires non-empty name");
  }
  const details = {
    url,
    name,
    value: String(params.value ?? "")
  };
  if (params.domain !== undefined && params.domain !== null && String(params.domain) !== "") {
    details.domain = String(params.domain);
  }
  if (params.path !== undefined && params.path !== null && String(params.path) !== "") {
    details.path = String(params.path);
  }
  if (params.secure !== undefined && params.secure !== null) {
    details.secure = Boolean(params.secure);
  }
  if (params.httpOnly !== undefined && params.httpOnly !== null) {
    details.httpOnly = Boolean(params.httpOnly);
  }
  const sameSite = normalizeCookieSameSite(params.sameSite);
  if (sameSite) {
    details.sameSite = sameSite;
  }
  const expirationDate = Number(params.expiresUnixSeconds ?? params.expirationDate ?? NaN);
  if (Number.isFinite(expirationDate)) {
    details.expirationDate = expirationDate;
  }
  return details;
}

function cookieSetDetailsFromState(cookie) {
  if (!cookie || typeof cookie !== "object") {
    throw new Error("storageState cookie must be an object");
  }
  const url = String(cookie.url || cookieUrlFromCookie(cookie));
  return cookieSetDetails({
    url,
    name: cookie.name,
    value: cookie.value,
    domain: cookie.domain,
    path: cookie.path,
    secure: cookie.secure,
    httpOnly: cookie.httpOnly,
    sameSite: cookie.sameSite,
    expiresUnixSeconds: cookie.expires === undefined || cookie.expires === null || Number(cookie.expires) < 0
      ? undefined
      : cookie.expires
  }, url);
}

function normalizeCookieSameSite(value) {
  const sameSite = String(value || "").trim().toLowerCase();
  if (!sameSite) {
    return null;
  }
  if (sameSite === "lax" || sameSite === "strict" || sameSite === "unspecified") {
    return sameSite;
  }
  if (sameSite === "none" || sameSite === "no_restriction" || sameSite === "no-restriction") {
    return "no_restriction";
  }
  throw bridgeError(ERROR_ATTACH_FAILED, `unsupported cookie sameSite ${JSON.stringify(value)}`);
}

function cookieUrlFromCookie(cookie) {
  const domain = String(cookie?.domain || "").replace(/^\./, "");
  if (!domain) {
    throw new Error("cookie has no domain and no url");
  }
  const path = String(cookie?.path || "/");
  const safePath = path.startsWith("/") ? path : `/${path}`;
  return `${cookie?.secure ? "https" : "http"}://${domain}${safePath}`;
}

function cookieRemovalUrl(cookie, fallbackUrl) {
  try {
    return cookieUrlFromCookie(cookie);
  } catch (_) {
    if (fallbackUrl) {
      return fallbackUrl;
    }
    throw _;
  }
}

function serializeCookie(cookie) {
  return {
    name: String(cookie?.name || ""),
    value: String(cookie?.value || ""),
    domain: String(cookie?.domain || ""),
    path: String(cookie?.path || ""),
    expires: Number.isFinite(Number(cookie?.expirationDate)) ? Number(cookie.expirationDate) : -1,
    httpOnly: Boolean(cookie?.httpOnly),
    secure: Boolean(cookie?.secure),
    sameSite: String(cookie?.sameSite || ""),
    session: Boolean(cookie?.session),
    hostOnly: Boolean(cookie?.hostOnly),
    storeId: cookie?.storeId == null ? null : String(cookie.storeId)
  };
}

function buildStorageStateFromFrameResults(frames, cookies) {
  const originsByName = new Map();
  let frameErrorCount = 0;
  for (const frame of frames) {
    const result = frame.result;
    if (!result || typeof result !== "object") {
      continue;
    }
    if (!result.ok) {
      frameErrorCount += 1;
      continue;
    }
    const origin = String(result.origin || "");
    if (!origin || origin === "null") {
      continue;
    }
    if (!originsByName.has(origin)) {
      originsByName.set(origin, {
        origin,
        localStorage: [],
        sessionStorage: []
      });
    }
    const entry = originsByName.get(origin);
    const localNames = new Set(entry.localStorage.map((item) => item.name));
    for (const item of Array.isArray(result.localStorage) ? result.localStorage : []) {
      const name = String(item?.name || "");
      if (name && !localNames.has(name)) {
        entry.localStorage.push({ name, value: String(item.value || "") });
        localNames.add(name);
      }
    }
    const sessionNames = new Set(entry.sessionStorage.map((item) => item.name));
    for (const item of Array.isArray(result.sessionStorage) ? result.sessionStorage : []) {
      const name = String(item?.name || "");
      if (name && !sessionNames.has(name)) {
        entry.sessionStorage.push({ name, value: String(item.value || "") });
        sessionNames.add(name);
      }
    }
  }
  const origins = Array.from(originsByName.values()).map((origin) => {
    origin.localStorage.sort((left, right) => left.name.localeCompare(right.name));
    origin.sessionStorage.sort((left, right) => left.name.localeCompare(right.name));
    return origin;
  }).sort((left, right) => left.origin.localeCompare(right.origin));
  return {
    ok: frameErrorCount === 0,
    origin_count: origins.length,
    frame_error_count: frameErrorCount,
    storage_state: {
      cookies: cookies.map(serializeCookie),
      origins
    }
  };
}

function normalizeReloadDelay(value) {
  if (value === undefined || value === null) {
    return 100;
  }
  const number = Number(value);
  if (!Number.isInteger(number) || number < 0 || number > 5000) {
    throw bridgeError(ERROR_ATTACH_FAILED, "reloadDelayMs must be an integer from 0 through 5000");
  }
  return number;
}

function normalizeEvaluateExpression(value) {
  if (typeof value !== "string" || value.length === 0) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "evaluateScript expression must be a non-empty string");
  }
  return value;
}

function normalizeEvaluateArgs(value) {
  if (value === undefined || value === null) {
    return [];
  }
  if (!Array.isArray(value)) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "evaluateScript args must be an array when supplied");
  }
  if (value.length > 64) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, `evaluateScript accepts at most 64 args; got ${value.length}`);
  }
  return value;
}

function normalizeEvaluateBoolean(value, defaultValue, fieldName) {
  if (value === undefined || value === null) {
    return defaultValue;
  }
  if (typeof value !== "boolean") {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `${fieldName} must be a boolean; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function buildEvaluateFunctionInvocation(expression, args) {
  return `(${expression})(${args.map((arg) => JSON.stringify(arg)).join(", ")})`;
}

function normalizeInitScriptOperation(value) {
  const operation = String(value || "add").trim().toLowerCase();
  if (operation === "add" || operation === "remove") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `initScript operation must be add or remove; got ${JSON.stringify(value)}`
  );
}

function normalizeInitScriptSource(value) {
  if (typeof value !== "string" || value.length === 0) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "initScript operation=add requires non-empty source");
  }
  return value;
}

function normalizeOptionalNonEmptyString(value, fieldName) {
  if (value === undefined || value === null) {
    return "";
  }
  if (typeof value !== "string" || value.length === 0) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, `${fieldName} must be a non-empty string when supplied`);
  }
  if (value.includes("\u0000")) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, `${fieldName} must not contain NUL`);
  }
  return value;
}

function normalizeBindingOperation(value) {
  const operation = String(value || "add").trim().toLowerCase();
  if (operation === "add" || operation === "read" || operation === "remove") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `exposeBinding operation must be add, read, or remove; got ${JSON.stringify(value)}`
  );
}

function normalizeBindingName(value) {
  if (typeof value !== "string" || value.trim() !== value || value.length === 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "exposeBinding name must be a non-empty JavaScript identifier without leading or trailing whitespace"
    );
  }
  if (Array.from(value).length > 128) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "exposeBinding name must be at most 128 Unicode scalar values"
    );
  }
  if (!/^[A-Za-z_$][0-9A-Za-z_$]*$/.test(value)) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      "exposeBinding name must be an ASCII JavaScript identifier, e.g. myBinding"
    );
  }
  return value;
}

function normalizeOptionalNonNegativeInteger(value, fieldName) {
  if (value === undefined || value === null) {
    return null;
  }
  if (!Number.isInteger(value) || value < 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `${fieldName} must be a non-negative integer when supplied; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function normalizeBindingMaxCalls(value) {
  if (value === undefined || value === null) {
    return 200;
  }
  if (!Number.isInteger(value) || value < 0) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `maxCalls must be a non-negative integer when supplied; got ${JSON.stringify(value)}`
    );
  }
  return Math.min(value, 5000);
}

function normalizeDialogOperation(value) {
  const operation = String(value || "status").trim().toLowerCase();
  if (operation === "status" || operation === "accept" || operation === "dismiss" || operation === "set_policy") {
    return operation;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `handleDialog operation must be status, accept, dismiss, or set_policy; got ${JSON.stringify(value)}`
  );
}

function normalizeDialogDefaultPolicy(value) {
  if (value === undefined || value === null || value === "") {
    return null;
  }
  const policy = String(value).trim().toLowerCase();
  if (policy === "accept" || policy === "dismiss" || policy === "manual") {
    return policy;
  }
  throw bridgeError(
    ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
    `handleDialog defaultPolicy must be accept, dismiss, or manual; got ${JSON.stringify(value)}`
  );
}

function normalizeDialogPromptText(value) {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string") {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "handleDialog promptText must be a string when supplied");
  }
  if (value.includes("\u0000")) {
    throw bridgeError(ERROR_CHROME_DOM_ACTION_UNSUPPORTED, "handleDialog promptText must not contain NUL");
  }
  if (Array.from(value).length > MAX_DIALOG_PROMPT_TEXT_CHARS) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `handleDialog promptText must be at most ${MAX_DIALOG_PROMPT_TEXT_CHARS} Unicode scalar values`
    );
  }
  return value;
}

function normalizeDialogLimit(value) {
  if (value === undefined || value === null) {
    return 20;
  }
  if (!Number.isInteger(value) || value < 1 || value > 100) {
    throw bridgeError(
      ERROR_CHROME_DOM_ACTION_UNSUPPORTED,
      `handleDialog limit must be an integer from 1 through 100; got ${JSON.stringify(value)}`
    );
  }
  return value;
}

function formatDebuggerExceptionDetails(details) {
  const exceptionText = details?.exception?.description || details?.exception?.value || details?.text || "unknown exception";
  const url = details?.url ? ` url=${details.url}` : "";
  const line = Number.isInteger(details?.lineNumber) ? ` line=${details.lineNumber + 1}` : "";
  const column = Number.isInteger(details?.columnNumber) ? ` column=${details.columnNumber + 1}` : "";
  return `${String(exceptionText)}${url}${line}${column}`;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function captureVisibleTabWithQuota(windowId, options, timeoutMs, context = {}) {
  const queuedAtMs = Date.now();
  let queueWaitMs = 0;
  let quotaWaitMs = 0;
  let effectiveTimeoutMs = 0;
  let captureStartedAtMs = 0;
  let captureElapsedMs = 0;
  const run = captureVisibleTabQueue.catch(() => undefined).then(async () => {
    queueWaitMs = Date.now() - queuedAtMs;
    quotaWaitMs = Math.max(
      0,
      lastCaptureVisibleTabAtMs + CAPTURE_VISIBLE_TAB_MIN_INTERVAL_MS - Date.now()
    );
    if (quotaWaitMs > 0) {
      await sleep(quotaWaitMs);
    }
    effectiveTimeoutMs = Math.floor(Number(timeoutMs) - queueWaitMs - quotaWaitMs);
    if (!Number.isFinite(effectiveTimeoutMs) || effectiveTimeoutMs <= 0) {
      throw bridgeError(
        ERROR_EXTENSION_TIMEOUT,
        "chrome.tabs.captureVisibleTab was not started because queue/quota wait exhausted the screenshot deadline; " +
          `${formatPageScreenshotCaptureContext(context)} timeout_ms=${timeoutMs} ` +
          `queue_wait_ms=${queueWaitMs} quota_wait_ms=${quotaWaitMs} queue_recovered=true`
      );
    }
    captureStartedAtMs = Date.now();
    try {
      return await withTimeout(
        chrome.tabs.captureVisibleTab(windowId, options),
        effectiveTimeoutMs,
        "chrome.tabs.captureVisibleTab"
      );
    } finally {
      captureElapsedMs = Date.now() - captureStartedAtMs;
      lastCaptureVisibleTabAtMs = Date.now();
    }
  });
  captureVisibleTabQueue = run.catch(() => undefined);
  try {
    return await run;
  } catch (error) {
    const detail = errorMessage(error);
    if (detail.includes("timed out after")) {
      throw bridgeError(
        ERROR_EXTENSION_TIMEOUT,
        "chrome.tabs.captureVisibleTab did not settle before the screenshot command deadline; " +
          `${formatPageScreenshotCaptureContext(context)} ` +
          `timeout_ms=${timeoutMs} effective_timeout_ms=${effectiveTimeoutMs} ` +
          `queue_wait_ms=${queueWaitMs} quota_wait_ms=${quotaWaitMs} ` +
          `capture_elapsed_ms=${captureElapsedMs || Math.max(0, Date.now() - captureStartedAtMs)} ` +
          `queue_recovered=true`
      );
    }
    throw error;
  }
}

function formatPageScreenshotCaptureContext(context) {
  return [
    `tab_id=${String(context.tabId ?? "unknown")}`,
    `window_id=${String(context.windowId ?? "unknown")}`,
    `target_id=${String(context.targetId ?? "unknown")}`,
    `tile=${String(context.tileIndex ?? "unknown")}/${String(context.tileCount ?? "unknown")}`,
    `requested_scroll_x=${String(context.requestedScrollX ?? "unknown")}`,
    `requested_scroll_y=${String(context.requestedScrollY ?? "unknown")}`
  ].join(" ");
}

async function postResponse(id, ok, result, error) {
  await postDaemonMessage({
    type: "response",
    id,
    ok,
    result,
    error
  });
}

async function postDaemonMessage(message) {
  if (!hostId) {
    console.warn("Synapse daemon bridge unavailable; message dropped");
    return;
  }
  try {
    await daemonFetchJson("/chrome-debugger/native/message", {
      method: "POST",
      body: {
        host_id: hostId,
        message
      }
    });
  } catch (error) {
    scheduleReconnect(
      `direct daemon message failed: ${errorMessage(error)}`,
      ERROR_DAEMON_UNAVAILABLE
    );
    throw error;
  }
}

async function daemonFetchJson(path, options = {}) {
  const init = {
    method: options.method || "GET",
    cache: "no-store",
    headers: {}
  };
  if (bridgeToken) {
    init.headers[BRIDGE_TOKEN_HEADER] = bridgeToken;
  }
  if (Object.prototype.hasOwnProperty.call(options, "body")) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(options.body);
  }
  const response = await fetch(`${DAEMON_BASE_URL}${path}`, init);
  const text = await response.text();
  let value = null;
  if (text) {
    try {
      value = JSON.parse(text);
    } catch (_) {
      throw new Error(
        `daemon ${init.method} ${path} returned non-JSON status=${response.status} body=${JSON.stringify(text.slice(0, 512))}`
      );
    }
  }
  if (!response.ok) {
    const error = new Error(
      `daemon ${init.method} ${path} failed status=${response.status} body=${JSON.stringify(value ?? text)}`
    );
    if (value?.code) {
      error.code = value.code;
    }
    error.status = response.status;
    error.responseBody = value ?? text;
    throw error;
  }
  return value;
}

function bridgeError(code, detail) {
  const error = new Error(detail);
  error.code = code;
  return error;
}

function errorPayload(error) {
  return {
    code: error?.code || ERROR_ATTACH_FAILED,
    detail: errorMessage(error)
  };
}

function errorMessage(error) {
  if (!error) {
    return "unknown error";
  }
  if (typeof error === "string") {
    return error;
  }
  return error.message || String(error);
}
