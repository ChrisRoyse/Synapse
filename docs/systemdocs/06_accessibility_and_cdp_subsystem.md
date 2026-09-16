# 06. Accessibility and CDP Subsystem

The `synapse-a11y` crate provides Synapse's two perception/action backends for live applications: **native accessibility** via Windows UI Automation (UIA) and the **Chrome DevTools Protocol** (CDP) for Chromium-family browsers. Native UI is reached through the OS accessibility tree; web content is reached through CDP.

**Source files covered:**

- `crates/synapse-a11y/Cargo.toml`
- `crates/synapse-a11y/src/lib.rs`
- `crates/synapse-a11y/src/ui_element.rs`
- `crates/synapse-a11y/src/ids.rs`
- `crates/synapse-a11y/src/error.rs`
- `crates/synapse-a11y/src/events.rs`
- `crates/synapse-a11y/src/window.rs`
- `crates/synapse-a11y/src/snapshot.rs`
- `crates/synapse-a11y/src/re_resolve.rs`
- `crates/synapse-a11y/src/cdp.rs`
- `crates/synapse-a11y/src/cdp_action.rs`
- `crates/synapse-a11y/src/cdp_actionability.rs`
- `crates/synapse-a11y/src/cdp_binding.rs`
- `crates/synapse-a11y/src/cdp_clock.rs`
- `crates/synapse-a11y/src/cdp_console.rs`
- `crates/synapse-a11y/src/cdp_dialog.rs`
- `crates/synapse-a11y/src/cdp_dom.rs`
- `crates/synapse-a11y/src/cdp_emulation.rs`
- `crates/synapse-a11y/src/cdp_files.rs`
- `crates/synapse-a11y/src/cdp_lifecycle.rs`
- `crates/synapse-a11y/src/cdp_network.rs`
- `crates/synapse-a11y/src/platform/mod.rs`
- `crates/synapse-a11y/src/platform/non_windows.rs`
- `crates/synapse-a11y/src/platform/windows/mod.rs`
- `crates/synapse-a11y/src/platform/windows/common.rs`
- `crates/synapse-a11y/src/platform/windows/events.rs`
- `crates/synapse-a11y/src/platform/windows/resolve.rs`
- `crates/synapse-a11y/src/platform/windows/snapshot.rs`
- `crates/synapse-a11y/src/platform/windows/window.rs`

---

## 1. Overview — Two Pillars

`crates/synapse-a11y/src/lib.rs` declares the module tree and re-exports the entire public surface. The crate has two distinct backends:

### Pillar A — Native Accessibility (Windows UI Automation)

Implemented under `crates/synapse-a11y/src/platform/`. The `platform` module is a `cfg` split:

- `cfg(windows)` → `platform/windows/*` — real UIA via the `uiautomation` crate plus raw `windows` Win32 calls.
- `cfg(not(windows))` → `platform/non_windows.rs` — every function returns `A11yError::not_available(...)` (`A11Y_NOT_AVAILABLE`). This lets the rest of the workspace compile and link on non-Windows hosts.

Native UIA provides: foreground/window enumeration, focused-element lookup, hit testing (`element_from_point`), accessible-tree snapshots, element re-resolution from a stable id, and semantic actions (click via control patterns, focus, set/read value, text selection, scrolling, expand/collapse state). It also installs `WinEvent` hooks for live accessibility events.

### Pillar B — Chrome DevTools Protocol (`cdp*`)

The `cdp.rs` + `cdp_*.rs` modules. All CDP behaviour that depends on `chromiumoxide` is `cfg(windows)` (see `Cargo.toml` — `chromiumoxide`, `tokio-tungstenite`, `base64`, `png`, `regex`, `serde_json` are Windows-only target dependencies). `cdp.rs` itself (probe + capability list + launched-port registry) is partly cross-platform; the attach/target functions are `cfg(windows)`.

CDP provides probing/attaching to a Chromium remote-debugging endpoint, target (tab) lifecycle, the page accessibility tree mapped into the same `AccessibleNode` model UIA uses, DOM-routed actions (click/type/scroll in viewport CSS coordinates), actionability predicates, and a large set of Playwright-style capture/emulation surfaces (console, network, dialogs, page lifecycle events, JS bindings, clock, viewport/device/geolocation/locale/media/network emulation).

The browser-facing MCP tools (`browser_*`, `cdp_*`) are thin wrappers over this crate. See [16_api_tools_reference.md](16_api_tools_reference.md).

> Background note from `cdp.rs`: since Chrome 136, `--remote-debugging-port` is ignored unless paired with a non-default `--user-data-dir`, so a normally-launched Chrome on the user's primary profile can never expose a debug port. This is why `act_launch` (#684) launches a dedicated automation profile and registers its port.

---

## 2. UI Element Model

### 2.1 `UIElement` (`crates/synapse-a11y/src/ui_element.rs`)

A platform-conditional re-export:

```rust
#[cfg(windows)]      pub use uiautomation::UIElement;   // real COM-backed element
#[cfg(not(windows))] pub struct UIElement;              // empty stub
```

The crate also re-exports `uiautomation` itself on Windows.

### 2.2 Element IDs (`crates/synapse-a11y/src/ids.rs`)

| Function | Signature | Purpose |
|---|---|---|
| `runtime_id_hex` | `fn runtime_id_hex(runtime_id: &[i32]) -> String` | Encodes a UIA RuntimeId (array of `i32`) as lowercase hex, 8 hex chars per element (via `cast_unsigned`). Forms the `runtime_id_hex` portion of a composite `ElementId`. |

The composite `ElementId` (defined in `synapse-core`) carries `<hwnd_hex>:<runtime_id_hex>`. Two sentinel prefixes in the runtime-id portion encode origin:

- UIA RuntimeId → real hex from `runtime_id_hex`.
- Fallback (RuntimeId unavailable) → `ffffffff` prefix + FNV-1a 64-bit hash of (hwnd, pid, control type, class name, automation id, name, rect). See `fallback_runtime_id_hex` in `platform/windows/common.rs`.
- CDP web node → `cdcd` prefix (see §5.7).

### 2.3 Resolution and Re-Resolution (`crates/synapse-a11y/src/re_resolve.rs`)

`re_resolve(id: &ElementId) -> A11yResult<UIElement>` re-resolves a composite id back to a live UIA element by locating the RuntimeId under the id's HWND. Returns `A11Y_ELEMENT_STALE` when the runtime id cannot be found, `OBSERVE_INTERNAL` for invalid ids. This module also defines the readback structs returned by element actions (all run on the UIA worker thread):

| Type | Kind | Notes |
|---|---|---|
| `ElementClickAction` | enum | Result of a semantic click: `Invoked`, `Toggled{before_state,after_state}`, `Selected{was_selected,is_selected}`, `Expanded{...}`, `Collapsed{...}`, `LegacyDefaultAction{default_action}`. |
| `ElementValueSetReadback` | struct | `method`, `before_value`, `after_value`, `expected_after_value`, `is_password`, `before/after_password_len`. |
| `ElementValueReadback` | struct | `method`, `value`, `is_readonly`, `is_password`, `password_len`. |
| `ElementTextSelectionReadback` | struct | selection range readback (`requested/before/after` start/end, `text_len`). |
| `ElementTextInsertReadback` | struct | UTF-16 length deltas for replace/append text ops. |
| `ElementMetadataReadback` | struct | `name`, `role`, `automation_id`, `bbox: Rect`, `enabled`, `keyboard_focusable`, `patterns: Vec<UiaPattern>`, `value`. |
| `ElementScrollStateReadback` | struct | `bbox`, scroll percents, view sizes, scrollable flags. |
| `ElementScrollReadback` | struct | `method`, `before`/`after` scroll state, `requested_dy/dx`, `scroll_call_count`. |
| `ExpandState` | enum | Read-only mirror of `uiautomation::types::ExpandCollapseState`: `Collapsed`, `Expanded`, `PartiallyExpanded`, `LeafNode`. |

Public element accessors (each wraps `platform::*`):

| Function | Signature |
|---|---|
| `element_bounding_rect` | `(id: &ElementId) -> A11yResult<Rect>` |
| `click_element_action` | `(id: &ElementId) -> A11yResult<ElementClickAction>` |
| `focus_element` | `(id: &ElementId) -> A11yResult<()>` |
| `set_element_value` | `(id: &ElementId, value: &str) -> A11yResult<ElementValueSetReadback>` |
| `element_value` | `(id: &ElementId) -> A11yResult<ElementValueReadback>` |
| `set_element_text_selection` | `(id: &ElementId, start: u32, end: u32) -> A11yResult<ElementTextSelectionReadback>` |
| `replace_element_text_selection` | `(id: &ElementId, text: &str) -> A11yResult<ElementTextInsertReadback>` |
| `append_element_text` | `(id: &ElementId, text: &str) -> A11yResult<ElementTextInsertReadback>` |
| `element_metadata` | `(id: &ElementId) -> A11yResult<ElementMetadataReadback>` |
| `scroll_element_into_view` | `(id: &ElementId) -> A11yResult<()>` (UIA `ScrollItemPattern.ScrollIntoView`, #882) |
| `scroll_element` | `(id: &ElementId, dy: i32, dx: i32) -> A11yResult<ElementScrollReadback>` |
| `element_scroll_state` | `(id: &ElementId) -> A11yResult<ElementScrollStateReadback>` |
| `expand_state_of` | `(element: &UIElement) -> A11yResult<ExpandState>` |
| `expand_state_of_id` | `(id: &ElementId) -> A11yResult<ExpandState>` |

`click_element_action` attempts a semantic click using exposed UIA patterns and returns `ACTION_ELEMENT_PATTERN_UNSUPPORTED` rather than synthesizing a coordinate fallback — the caller chooses the next delivery tier.

### 2.4 Snapshot Tree (`crates/synapse-a11y/src/snapshot.rs`)

Captures UIA subtrees as `synapse_core::AccessibleSubtree` (tree of `AccessibleNode`).

| Function | Signature |
|---|---|
| `snapshot` | `(root: &UIElement, depth: u32) -> A11yResult<AccessibleSubtree>` |
| `snapshot_focused_window` | `(depth: u32) -> A11yResult<AccessibleSubtree>` |
| `snapshot_window_from_hwnd` | `(hwnd: i64, depth: u32) -> A11yResult<AccessibleSubtree>` |
| `snapshot_window_for_process` | `(pid: u32, depth: u32) -> A11yResult<AccessibleSubtree>` |
| `snapshot_element` | `(id: &ElementId, depth: u32) -> A11yResult<AccessibleSubtree>` |
| `focused_element_node` | `() -> A11yResult<AccessibleNode>` |
| `focused_element_node_in_window` | `(hwnd: i64) -> A11yResult<Option<AccessibleNode>>` |
| `element_node_from_point` | `(point: Point) -> A11yResult<AccessibleNode>` |
| `find_by_name_and_pattern` | `(root: &UIElement, name: &str, pattern: UiaPattern, scope: ElementSearchScope) -> A11yResult<Option<AccessibleNode>>` |
| `find_by_name_and_pattern_in_window` | `(hwnd: i64, name: impl Into<String>, pattern: UiaPattern, scope: ElementSearchScope) -> A11yResult<Option<AccessibleNode>>` |
| `chromium_renderer_accessibility_nodes_from_window` | `(hwnd: i64, depth: u32, max_nodes: usize) -> A11yResult<Vec<AccessibleNode>>` |

`ElementSearchScope` enum: `Children`, `Descendants`, `Subtree` (maps to UIA `TreeScope`). `chromium_renderer_accessibility_nodes_from_window` supplements UIA raw child-walk gaps in the Chromium renderer tree even after `--force-renderer-accessibility`.

`UiaPattern` (from `synapse-core`): `Invoke`, `Toggle`, `Value`, `Selection`, `SelectionItem`, `ExpandCollapse`, `LegacyIAccessible`, `Scroll`, `ScrollItem`, `Text`, `Window`, `Transform`, `RangeValue`.

---

## 3. Window Enumeration (`crates/synapse-a11y/src/window.rs`)

`ForegroundActivationIntent` enum gates foreground activation:

- `OperatorRequested { caller: &'static str }` → reason `"operator_requested"`.
- `LeaseContextRestore { caller: &'static str }` → reason `"lease_context_restore"`.

`focus_window(hwnd)` **always** returns `FOREGROUND_ACTIVATION_REFUSED` — implicit activation is disabled; callers must prove intent and use `focus_window_with_intent`.

| Function | Signature | Notes |
|---|---|---|
| `focused_window` | `() -> A11yResult<UIElement>` | Foreground HWND as UIA element; `A11Y_NO_FOREGROUND` if none. |
| `current_foreground_context` | `() -> A11yResult<ForegroundContext>` | Process/title metadata for current foreground. |
| `window_from_hwnd` | `(hwnd: i64) -> A11yResult<UIElement>` | Top-level UIA window for an HWND. |
| `millis_since_last_input` | `() -> A11yResult<u64>` | Ms since session last saw input via `GetLastInputInfo`. Synthetic `SendInput` resets it too (measures "session receiving input", not "human present"; #838). |
| `focus_window` | `(hwnd: i64) -> A11yResult<()>` | Always `FOREGROUND_ACTIVATION_REFUSED`. |
| `focus_window_with_intent` | `(hwnd: i64, intent: ForegroundActivationIntent) -> A11yResult<()>` | Activates with validated intent. |
| `is_window_minimized` | `(hwnd: i64) -> A11yResult<bool>` | Iconic state. |
| `is_window_visible` | `(hwnd: i64) -> A11yResult<bool>` | |
| `is_top_level_window` | `(hwnd: i64) -> A11yResult<bool>` | Whether HWND is its own top-level root. |
| `top_level_root_hwnd` | `(hwnd: i64) -> A11yResult<i64>` | Top-level root HWND for an HWND. |
| `close_window` | `(hwnd: i64) -> A11yResult<()>` | Posts `WM_CLOSE`. |
| `window_for_process` | `(pid: u32) -> A11yResult<UIElement>` | Top-level UIA window for a pid. |
| `top_level_window_hwnd_by_name` | `(name: impl Into<String>) -> A11yResult<Option<i64>>` | Resolves window name → HWND without returning COM elements. |
| `foreground_context` | `(hwnd: i64) -> A11yResult<ForegroundContext>` | Process, title, bounds, display metadata. |
| `visible_top_level_window_contexts` | `() -> A11yResult<Vec<ForegroundContext>>` | All visible top-level windows; vanished windows skipped. |
| `focused_element` | `() -> A11yResult<UIElement>` | Currently focused element, cached basic props. |
| `element_from_point` | `(point: Point) -> A11yResult<UIElement>` | UIA hit test at a screen point. |

---

## 4. CDP Core — Probe, Attach, Targets (`crates/synapse-a11y/src/cdp.rs`)

Diagnostic types (`CdpDiagnostics`, `CdpStatus`, `CdpCapability`) live in `synapse-core` and are re-exported here; this module owns behaviour.

| Constant | Value |
|---|---|
| `DEFAULT_CDP_PORT` | `9222` |
| `CDP_PORTS_ENV` | `SYNAPSE_CDP_PORTS` (comma-separated port override) |

`CdpCapability` (advertised by `cdp_capabilities()`): `DomSnapshot`, `AccessibilityFullAxTree`, `DomQuerySelector`, `PageCaptureScreenshot`, `PageFrameTree`, `FlatIframeSessions`, `PiercedShadowDom`.

**Launched-port registry** — `act_launch` (#684) registers the ephemeral debug port it opened keyed by browser pid, so a later `observe`/`find` finds it without manual flags:

| Function | Signature |
|---|---|
| `register_launched_port` | `(pid: u32, port: u16)` |
| `forget_launched_port` | `(pid: u32)` |
| `launched_port_for_pid` | `(pid: u32) -> Option<u16>` |
| `candidate_ports_for_pid` | `(pid: u32) -> Vec<u16>` (registered port first, then env/default, de-duped) |

**Probe / attach / targets:**

| Function | Signature | Notes |
|---|---|---|
| `cdp_capabilities` | `() -> Vec<CdpCapability>` | cross-platform |
| `is_chromium_family` | `(process_name: &str) -> bool` | matches chrome/chromium/msedge/brave/vivaldi/opera | cross-platform |
| `probe_chromium_cdp_blocking` | `(process_name, ports: &[u16], connect_timeout: Duration) -> CdpDiagnostics` | sync probe used in perception path; connection-refused returns in microseconds | cross-platform |
| `probe_chromium_cdp` | `async (process_name, ports, connect_timeout) -> CdpDiagnostics` | async variant | cross-platform |
| `endpoint_for_window` | `(hwnd: i64) -> Option<String>` | `cfg(windows)`; resolves reachable `http://127.0.0.1:<port>` for a browser window (#686) |
| `attach_chromiumoxide` | `async (endpoint: &str) -> A11yResult<CdpAttachment>` | `cfg(windows)`; connects a `chromiumoxide::Browser`+`Handler` |
| `cdp_list_targets` | `async (endpoint: &str) -> A11yResult<Vec<CdpTargetSummary>>` | `Target.getTargets` |
| `cdp_open_background_tab` | `async (endpoint, url) -> A11yResult<CdpOpenTabResult>` | `Target.createTarget(background=true)` then waits for presence |
| `cdp_close_target` | `async (endpoint, target_id) -> A11yResult<CdpCloseTabResult>` | `Target.closeTarget`, waits for absence |
| `cdp_activate_target` | `async (endpoint, target_id) -> A11yResult<CdpActivateTabResult>` | `Target.activateTarget`; CDP-level tab select, does not seize OS foreground (#1189) |

`cfg(windows)` types: `CdpAttachment { browser, handler, endpoint }`, `CdpTargetSummary { target_id, target_type, title, url, attached }`, `CdpOpenTabResult`, `CdpCloseTabResult`, `CdpActivateTabResult`. Tab-present/absent waits poll `Target.getTargets` 30× at 100 ms (3 s budget).

---

## 5. CDP Modules

All CDP action/capture modules are `cfg(windows)` (depend on `chromiumoxide`). The bridging MCP tools are documented in [16_api_tools_reference.md](16_api_tools_reference.md).

### 5.1 `cdp_dom.rs` — Accessibility/DOM Tree → `AccessibleNode` (#685)

Pulls `Accessibility.getFullAXTree` (+ `DOM.getBoxModel` for bounds) and maps each AX node into the same `AccessibleNode` model the UIA path uses, so `find(role=..., name_substring=...)` works on a web page identically to native UI. The pure mapper (`build_accessible_nodes`) is unit-tested; `fetch_dom_snapshot` is the live I/O wrapper.

| Function | Signature |
|---|---|
| `cdp_element_id` | `(hwnd: i64, backend_node_id: i64) -> ElementId` |
| `cdp_element_id_for_target` | `(hwnd: i64, target_id: &str, backend_node_id: i64) -> ElementId` |
| `cdp_backend_from_element_id` | `(id: &ElementId) -> Option<i64>` |
| `cdp_target_from_element_id` | `(id: &ElementId) -> Option<String>` |
| `build_accessible_nodes` | `(...) -> ...` (pure mapper) |
| `build_accessible_nodes_for_target` | `(...)` |
| `rect_from_quad` | `(quad: &[f64]) -> Option<Rect>` |
| `fetch_dom_snapshot` | `async (...) -> A11yResult<CdpDomSnapshot>` |
| `cdp_list_frames` | `async (...) -> A11yResult<CdpFrameListResult>` |

Types: `CdpDomNode` (backend_node_id, parent_backend_node_id, role, name, value, frame_id, bbox, child_count, enabled, focused), `CdpDomSnapshot`, `CdpFrameTreeEntry`, `CdpFrameListResult`. **Web-node bbox** is the CSS-pixel rectangle in page-layout coords (not screen pixels); actions re-resolve the live box model at click time.

### 5.2 `cdp_action.rs` — DOM-Routed Actions (#686)

When an action targets a web node (element id carrying `CDP_RUNTIME_PREFIX`), routing comes here instead of UIA/`SendInput`. Attaches CDP, locates the owning page, scrolls into view, resolves the live box model, and dispatches via `Input.dispatchMouseEvent` / `Input.dispatchTouchEvent` / `Input.insertText` in **viewport CSS coordinates** (sidesteps DPI/scroll/origin mapping).

Selected public async functions: `cdp_click_node`, `cdp_touch_tap_node`, `cdp_type_node`, `cdp_dom_primitive_node`, `cdp_set_node_text`, `cdp_press_key_sequence`, `cdp_mouse_stroke_target`, `cdp_touch_tap_target`, `cdp_active_element_state`, `cdp_page_text_target`, `cdp_evaluate_expression`, `cdp_evaluate_on_element`, `cdp_add_init_script_target`, `cdp_remove_init_script_target`, `cdp_locate`, `cdp_wait_for_load_state`, `cdp_wait_for_url`, `cdp_navigate_page_target`, `cdp_set_document_content_target`, `cdp_scroll_into_view_node`, `cdp_scroll_node`, `cdp_node_value`, `cdp_node_scroll_state`, `cdp_node_viewport_center`, `cdp_aim_node`, `cdp_capture_node_bgra`, `cdp_capture_page_bgra`.

Key types: `CdpActionPoint`, `CdpMouseStrokePoint`/`CdpMouseStrokeResult`, `CdpTouchTapResult`, `CdpWheelDelta`, `CdpKeyStroke`, `CdpScrollState`, `CdpScrollIntoView*`, `CdpActiveElementState`, `CdpPageNavigationAction`/`Result`, `CdpPageState`, `CdpLoadState`/`CdpLoadStateWaitResult`, `CdpUrlMatchKind`/`CdpUrlWaitResult`, `CdpPageTextState`, `CdpEvaluateResult`, `CdpInitScriptResult`, `CdpLocateEngine`/`CdpLayoutRelation`/`CdpLocateRequest`/`CdpLocateResult`, `CdpSetDocumentContentResult`, `CdpMouseButton`, `CdpSetNodeTextReadback`, `CdpDomPrimitiveResult`, `CdpNodeBitmap`. Re-exported at crate root: `CdpMouseStrokePoint`, `cdp_mouse_stroke_target`.

### 5.3 `cdp_actionability.rs` — Actionability Predicates (#1122)

Mirrors Playwright-style preflight checks before DOM actions: live attachment, visibility, box stability, enabled/editable state, and whether the action point receives pointer events.

| Function | Signature |
|---|---|
| `cdp_actionability` | `async (...) -> A11yResult<CdpActionabilityResult>` |
| `visible_from` | `(...) -> ...` (pure) |
| `stable_from` | `(...) -> ...` (pure) |
| `box_models_equal` | `(...) -> ...` (pure) |

Types: `CdpActionabilityPoint`, `CdpActionabilityRect` (`center()`, `has_area()`), `CdpActionabilityBoxModel` (`action_point()`), `CdpActionabilityDomState`, `CdpActionabilityFailure`, `CdpActionabilityHitTest`, `CdpActionabilityResult`.

### 5.4 `cdp_binding.rs` — Runtime Binding Capture (#1069)

`Runtime.bindingCalled` is a live event. Keeps one long-lived CDP session per target, exposes binding names via `Runtime.addBinding`, and buffers payloads for later MCP reads.

| Function | Signature |
|---|---|
| `binding_capture_add` | `async (...) -> A11yResult<...>` |
| `binding_capture_read` | `(...) -> CdpBindingReadResult` |
| `binding_capture_remove` | `async (...) -> ...` |

Types: `CdpBindingCall`, `CdpBindingCaptureStatus`, `CdpBindingReadFilter`, `CdpBindingReadResult`.

### 5.5 `cdp_clock.rs` — Controllable Browser Clock (#1198)

Installs a small init-script Date/timer shim into one page target and drives it with `Runtime.evaluate`, providing `setFixedTime`, `fastForward`, `pauseAt`-style controls.

| Function | Signature |
|---|---|
| `cdp_clock` | `async (...) -> A11yResult<CdpClockResult>` |

`CdpClockOperation` enum + `CdpClockReadback`, `CdpClockResult`.

### 5.6 `cdp_console.rs` — Console + Page-Error Capture (#1091/#1092/#1093)

Persistent per-target listener (not connect-on-demand) because Chrome does not replay `Runtime.consoleAPICalled` history on `Runtime.enable` — events fire live, once. One long-lived `chromiumoxide` client per armed target with a bounded ring buffer.

| Function | Signature |
|---|---|
| `console_capture_ensure` | `async (endpoint: &str, target_id: &str, capacity: usize) -> A11yResult<ConsoleCaptureStatus>` |
| `console_capture_read` | `(...) -> ConsoleReadResult` (cursor-based, non-consuming) |
| `console_capture_stop` | `(target_id: &str) -> bool` |
| `console_capture_active_count` | `() -> usize` |

Types: `ConsoleEntry`, `ConsoleReadResult`, `ConsoleCaptureStatus`, `ConsoleReadFilter<'a>`.

### 5.7 `cdp_dialog.rs` — JavaScript Dialog Capture (#1097)

`Page.javascriptDialogOpening` is live. Keeps a long-lived connection per armed target, records dialog open/close state, and immediately applies a configured default policy so an unhandled dialog cannot silently block page execution. The MCP `browser_handle_dialog` tool uses these raw-CDP helpers for raw targets and the normal Chrome bridge's narrow `chrome.debugger` `Page.javascriptDialogOpening`/`Page.handleJavaScriptDialog` lane for session-owned `chrome-tab:*` targets in the already-open profile.

| Function | Signature |
|---|---|
| `dialog_capture_ensure` | `async (...) -> A11yResult<...>` |
| `cdp_dialog_capture_start` | `async (...) -> ...` |
| `dialog_capture_set_default_policy` | `(...) -> ...` |
| `dialog_handle_pending` | `async (...) -> A11yResult<CdpDialogHandleResult>` |
| `cdp_dialog_handle_pending` | `async (...) -> ...` |
| `dialog_capture_read` | `(...) -> CdpDialogReadResult` |
| `dialog_capture_status` | `(target_id: &str) -> Option<CdpDialogCaptureStatus>` |
| `dialog_capture_stop` | `(target_id: &str) -> bool` |
| `dialog_capture_active_count` | `() -> usize` |

Enums: `CdpDialogDefaultPolicy`, `CdpDialogAutoAction`, `CdpDialogHandleAction`. Structs: `CdpDialogEntry`, `CdpDialogCaptureStatus`, `CdpDialogReadFilter`, `CdpDialogReadResult`, `CdpDialogHandleResult`.

### 5.8 `cdp_files.rs` — File Input / Chooser Helpers (#1101-#1103)

Wraps the CDP file-upload primitives used by both raw-CDP automation and the normal Chrome bridge contract:

| Function | Signature |
|---|---|
| `cdp_set_file_input_files_params_for_backend_node` | `(backend_node_id, files) -> A11yResult<SetFileInputFilesParams>` |
| `cdp_set_file_input_files_by_backend_node` | `async (page, backend_node_id, files) -> A11yResult<()>` |
| `cdp_intercept_file_chooser_params` | `(enabled, cancel) -> A11yResult<SetInterceptFileChooserDialogParams>` |
| `cdp_set_intercept_file_chooser` | `async (page, enabled, cancel) -> A11yResult<()>` |
| `cdp_file_chooser_entry_from_event` | `(EventFileChooserOpened, seq, opened_at_unix_ms) -> CdpFileChooserEntry` |

`DOM.setFileInputFiles` assigns local file paths to a file input node by `backendNodeId`. `Page.setInterceptFileChooserDialog` prevents the native OS picker and emits `Page.fileChooserOpened` with mode and backing node metadata. The normal-profile MCP `browser_file_upload` tool uses the extension bridge's narrow `chrome.debugger` lane for the same primitives on session-owned `chrome-tab:*` targets.

### 5.9 `cdp_emulation.rs` — Browser Emulation (#1173–#1178)

Target-scoped raw-CDP overrides. Each capability has an apply + reset pair returning a readback:

| Function | Domain |
|---|---|
| `cdp_set_viewport_size` / `cdp_set_viewport_size_with_mobile` / `cdp_reset_viewport_size` | `Emulation.setDeviceMetricsOverride` (viewport) |
| `cdp_apply_device_descriptor` / `cdp_reset_device_descriptor` | device emulation |
| `cdp_set_geolocation_override` / `cdp_reset_geolocation_override` | `Emulation.setGeolocationOverride` |
| `cdp_set_locale_timezone_override` / `cdp_reset_locale_timezone_override` | locale + timezone |
| `cdp_set_media_override` / `cdp_reset_media_override` | media type / features |
| `cdp_set_network_conditions` / `cdp_reset_network_conditions` | `Network.emulateNetworkConditions` |

Override/readback/result struct triples per capability: `CdpViewportOverride/Readback/Result`, `CdpDeviceDescriptor/...`, `CdpGeolocationOverride/...` (+ coordinate/error readbacks), `CdpLocaleTimezoneOverride/...`, `CdpMediaOverride/...`, `CdpNetworkConditionsOverride/...`.

### 5.10 `cdp_lifecycle.rs` — Page Lifecycle + Worker Targets (#1199/#1200)

Persistent capture of `Page.lifecycleEvent` and worker target events over raw CDP.

| Function | Signature |
|---|---|
| `lifecycle_capture_ensure` | `async (...) -> A11yResult<...>` |
| `lifecycle_capture_read` | `(...) -> CdpPageEventsReadResult` |

Types: `CdpPageEventEntry`, `CdpPageEventsCaptureStatus`, `CdpPageEventsReadFilter<'a>`, `CdpPageEventsReadResult`, `CdpPageTargetSnapshot`, `CdpWorkerSnapshot`.

### 5.11 `cdp_network.rs` — Network Capture + Interception (#1080)

Mirrors `cdp_console`'s persistent-listener model (CDP does not replay Network events after `Network.enable`): one long-lived connection per armed target, a live event pump, and a bounded ring buffer read by cursor without consuming. Also implements request override and Playwright-style `Fetch`-based routing.

| Function | Signature |
|---|---|
| `network_capture_ensure` | `async (endpoint, target_id, capacity: usize) -> A11yResult<CdpNetworkCaptureStatus>` |
| `cdp_network_capture_start` | `async (...) -> ...` |
| `network_capture_read` | `(...) -> CdpNetworkReadResult` |
| `network_web_socket_read` | `(...) -> CdpWebSocketReadResult` |
| `network_response_body` | `async (...) -> A11yResult<CdpNetworkResponseBody>` |
| `network_request_post_data` | `async (...) -> A11yResult<CdpNetworkRequestPostData>` |
| `network_overrides_apply` | `async (...) -> ...` |
| `network_overrides_status` | `(target_id: &str) -> Option<CdpNetworkOverrideStatus>` |
| `network_overrides_clear` | `async (...) -> ...` |
| `fetch_interception_ensure` | `async (...) -> ...` |
| `fetch_interception_status` | `(target_id: &str) -> Option<CdpFetchInterceptionStatus>` |
| `fetch_route_rules` | `(target_id: &str) -> Option<Vec<CdpFetchRouteRule>>` |
| `fetch_route_add` | `(...) -> ...` |
| `fetch_route_remove` | `(target_id, route_id) -> A11yResult<bool>` |
| `fetch_route_clear` | `(target_id) -> A11yResult<usize>` |
| `fetch_interception_stop` | `async (target_id) -> A11yResult<bool>` |
| `fetch_interception_active_count` | `() -> usize` |
| `network_capture_stop` | `(target_id: &str) -> bool` |
| `network_capture_clear` | `(target_id: &str) -> bool` |
| `network_capture_active_count` | `() -> usize` |

Types include `CdpNetworkEntry`, `CdpNetworkResponseSnapshot`, `CdpNetworkCaptureStatus`, `CdpNetworkReadFilter<'a>`/`Result`, `CdpWebSocketFrame`/`Entry`/`ReadFilter`/`Result`, `CdpNetworkResponseBody`, `CdpNetworkRequestPostData`, `CdpNetworkOverrideConfig`/`Status`, and Fetch routing types `CdpFetchInterceptionStage`, `CdpFetchInterceptionPattern`/`Status`, `CdpFetchRouteMatchKind`, `CdpFetchRouteFulfill`/`Abort`/`Continue`, `CdpFetchRouteAction`, `CdpFetchRouteRule`.

---

## 6. Platform Layer

### 6.1 Split (`platform/mod.rs`)

```rust
#[cfg(windows)]      mod windows;      pub use windows::*;
#[cfg(not(windows))] mod non_windows;  pub use non_windows::*;
```

`platform/non_windows.rs` provides a complete stub of every platform function; all return `A11yError::not_available(...)` (`A11Y_NOT_AVAILABLE`). `WinEventSubscription`, `subscribe_win_events`, and `uia_worker_readback` are also stubbed.

### 6.2 Windows Module Map (`platform/windows/mod.rs`)

| Submodule | Responsibility |
|---|---|
| `common.rs` | UIA worker thread, cache-request construction, property/pattern readers, RuntimeId hex + FNV fallback, COM apartment readback, error mapping. |
| `events.rs` | `WinEvent` hook thread + accessibility event marshalling. |
| `resolve.rs` | Element re-resolution and all element actions (click, focus, value, text, scroll, expand). |
| `snapshot.rs` | Tree snapshots, focused/point node lookup, name+pattern search, Chromium renderer supplement. |
| `window.rs` | Foreground/window enumeration, focus/close/visibility, idle input timer, foreground context. |

### 6.3 UIA Worker Model (`platform/windows/common.rs`)

All UIA calls run on a single dedicated worker thread (`synapse-a11y-uia-mta`) initialized with `CoInitializeEx(COINIT_MULTITHREADED)` (MTA) and `UIAutomation::new_direct()`. Jobs are submitted via `with_automation` / `with_automation_operation` over an mpsc channel. Key properties:

- Default job timeout `DEFAULT_UIA_WORKER_JOB_TIMEOUT = 10 s`; busy-poll interval `10 ms`.
- A bounded deadline per job. On timeout the worker is marked `timed_out` and **retired** (`A11Y_UIA_WORKER_RETIRED`) so the next request starts a fresh worker; the original job thread stays alive (does not block the daemon). Errors carry `phase=worker_busy` / `worker_unavailable` / `worker_reply_recv`, mapped to `A11Y_UIA_WORKER_TIMEOUT`.
- A `job_lock` serializes jobs; a second concurrent request returns busy rather than queueing.

Cache requests (`create_cache_request`) pre-cache a fixed property set including RuntimeId, BoundingRectangle, ProcessId, ControlType, LocalizedControlType, Name, HasKeyboardFocus, IsKeyboardFocusable, IsEnabled, AutomationId, ClassName, IsControlElement, IsContentElement, IsPassword, NativeWindowHandle, ValueValue, RangeValueValue, and the `Is*PatternAvailable` flags for every supported pattern. `TreeView` selects `Control` (control-view condition) vs `Raw` (true condition). `cached_value` prefers `ValuePattern` string then `RangeValuePattern` number.

`UiaWorkerReadback { thread_id, apartment: ComApartmentKind, owned_window_count }` exposes worker state; `is_mta_windowless()` asserts the worker is MTA with no owned HWNDs. `ComApartmentKind`: `Sta`, `Mta`, `Neutral`, `MainSta`, `Unknown`, `Unsupported` (read via `CoGetApartmentType`).

### 6.4 UIA APIs Used

From the `uiautomation` crate: `UIAutomation`, `UIElement`, `UICacheRequest`, `ControlType`, `ElementMode`, `TreeScope`, `UIProperty`, `Value`/`Variant`. From the `windows` Win32 crate: `Com` (`CoInitializeEx`, `CoUninitialize`, `CoGetApartmentType`, apartment constants), `Threading::GetCurrentThreadId`, `UI::WindowsAndMessaging::EnumThreadWindows`, and (events) `UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK}` plus message-loop APIs.

---

## 7. Events (`crates/synapse-a11y/src/events.rs` + `platform/windows/events.rs`)

Live accessibility events delivered over an `UnboundedSender<AccessibleEvent>` (`AccessibleEventSender`).

`AccessibleEvent` fields: `seq: u64`, `at_ms: u64`, `window_id: i64`, `element_id: Option<ElementId>`, `kind: AccessibleEventKind`, `name: Option<String>`, `value: Option<String>`.

`AccessibleEventKind` ↔ Win32 `WinEvent` mapping (`platform/windows/events.rs`):

| Kind | WinEvent constant |
|---|---|
| `ForegroundChanged` | `EVENT_SYSTEM_FOREGROUND` |
| `FocusChanged` | `EVENT_OBJECT_FOCUS` |
| `ValueChanged` | `EVENT_OBJECT_VALUECHANGE` |
| `NameChanged` | `EVENT_OBJECT_NAMECHANGE` |
| `ElementAppeared` | `EVENT_OBJECT_CREATE` |
| `ElementDisappeared` | `EVENT_OBJECT_DESTROY` |
| `SelectionChanged` | `EVENT_OBJECT_SELECTION` |
| `MenuStart` | `EVENT_SYSTEM_MENUSTART` |
| `MenuEnd` | `EVENT_SYSTEM_MENUEND` |
| `Alert` | `EVENT_SYSTEM_ALERT` |

Hooks are installed with `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS` over the 10 event ids above. `WinEventHookReadback { thread_id, apartment, hook_count, event_ids }` reports the hook thread state.

Helpers:

| Function | Signature | Purpose |
|---|---|---|
| `subscribe_win_events` | `(sender: AccessibleEventSender) -> A11yResult<WinEventSubscription>` | Starts the hook thread; marshals events into `sender`. |
| `uia_worker_readback` | `() -> A11yResult<UiaWorkerReadback>` | Live readback from the UIA worker. |
| `coalesce_events` | `(events: I, window: Duration) -> Vec<AccessibleEvent>` | Drops a same-key event arriving within `window` of its predecessor. |
| `debounce_value_changes` | `(events: I, window: Duration) -> Vec<AccessibleEvent>` | Debounces `ValueChanged` by key/window; `FocusChanged` flushes pending; output sorted by `(at_ms, seq)`. |

Same-key is `(window_id, kind, hash(element_id))` (`EventKey`).

---

## 8. Error Types (`crates/synapse-a11y/src/error.rs`)

`A11yResult<T> = Result<T, A11yError>`. `A11yError::code()` maps each variant to a `synapse_core::error_codes` string.

| Variant | Code |
|---|---|
| `NotAvailable { detail }` | `A11Y_NOT_AVAILABLE` |
| `NoForeground { detail }` | `A11Y_NO_FOREGROUND` |
| `ElementStale { detail }` | `A11Y_ELEMENT_STALE` |
| `ElementPatternUnsupported { detail }` | `ACTION_ELEMENT_PATTERN_UNSUPPORTED` |
| `ElementValueUnsupported { detail }` | `ACTION_ELEMENT_PATTERN_UNSUPPORTED` |
| `ElementValueReadOnly { detail }` | `ACTION_TARGET_INVALID` |
| `ElementNotEnabled { detail }` | `ACTION_TARGET_INVALID` |
| `CdpUnreachable { detail }` | `A11Y_CDP_UNREACHABLE` |
| `CdpAttachFailed { detail }` | `A11Y_CDP_ATTACH_FAILED` |
| `CdpAxtreeFailed { detail }` | `A11Y_CDP_AXTREE_FAILED` |
| `BrowserWaitTimeout { detail }` | `BROWSER_WAIT_TIMEOUT` |
| `UiaWorkerTimeout { detail }` | `A11Y_UIA_WORKER_TIMEOUT` |
| `ForegroundActivationRefused { detail }` | `FOREGROUND_ACTIVATION_REFUSED` |
| `InvalidElementId { detail }` | `OBSERVE_INTERNAL` |
| `Internal { detail }` | `OBSERVE_INTERNAL` |

Convenience constructors: `not_available`, `internal`, `foreground_activation_refused`, `uia_worker_timeout`.

---

## 9. Cross-References

- MCP tools that wrap this crate (`browser_*`, `cdp_*`, `observe`, `find`, `act_*`): [16_api_tools_reference.md](16_api_tools_reference.md).
- `AccessibleNode`, `AccessibleSubtree`, `UiaPattern`, `ElementId`, `ForegroundContext`, `CdpDiagnostics`/`CdpStatus`/`CdpCapability`, `error_codes` are defined in `synapse-core`.
