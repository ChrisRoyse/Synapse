//! `target_act` (#1005): a compact, high-level capability-preserving computer-use
//! router.
//!
//! The raw tool surface is large, and model priors make low-level primitive
//! selection brittle and foreground-prone. `target_act` gives agents one
//! intent-named verb that routes to the correct session-targeted primitive: a
//! background/target path when that satisfies the task, an agent logical
//! foreground/foreground-lane path when foreground-equivalent semantics are
//! required, and never an implicit fallback to the human OS foreground. It is a
//! thin dispatcher: each verb delegates to the existing tool method, inheriting
//! that tool's target resolution, action audit (#1006), and lease/foreground
//! guards (#999/#1004) - so a normal (leaseless) session can drive its owned
//! target but cannot seize the human foreground.

use super::browser_field::BrowserSetValueParams;
use super::m1_tools::{
    chrome_debugger_default_endpoint, chrome_debugger_endpoint, validate_cdp_target_id,
};
use super::{
    CdpTargetOwner, ErrorData, Json, Parameters, SessionTarget, SynapseService, tool, tool_router,
};
use crate::m1::{
    CaptureScreenshotParams, CdpActivateTabParams, CdpNavigateAction, CdpNavigateTabParams,
    CdpTargetInfoParams, ObserveParams, mcp_error,
};
use crate::m2::{
    ActClickParams, ActFocusWindowParams, ActPressParams, ActScrollParams, ActScrollPoint,
    ActSetFieldTextLocator, ActSetFieldTextParams, ActTypeParams, default_auto_wait_timeout_ms,
    default_verify_timeout_ms,
};
use crate::m4::{ActRunShellExecutionMode, ActRunShellParams};
use rmcp::schemars::JsonSchema;
use rmcp::{RoleServer, service::RequestContext};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use synapse_core::{AccessibleNode, ElementId, Point, Rect, UiaPattern, error_codes};

const DEFAULT_TARGET_ACT_SHELL_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_TARGET_ACT_FOCUS_STABLE_MS: u32 = 75;
const DEFAULT_TARGET_ACT_SAVE_TIMEOUT_MS: u32 = 2_000;
const DEFAULT_TARGET_ACT_CLEANUP_NOTEPAD_TABS_TIMEOUT_MS: u32 = 5_000;
const TARGET_ACT_SAVE_POLL_INTERVAL_MS: u64 = 50;
const TARGET_ACT_SAVE_SOURCE_OF_TRUTH: &str = "file.bytes";
const TARGET_ACT_CLEANUP_NOTEPAD_TABS_SOURCE_OF_TRUTH: &str = "hidden_desktop.notepad_tabs.uia";
const TARGET_ACT_STATUS_OK: &str = "ok";
const TARGET_ACT_STATUS_VERIFY_NEEDED: &str = "verify_needed";
const TARGET_ACT_STATUS_REFUSED: &str = "refused";
const TARGET_ACT_STATUS_ERROR: &str = "error";
const TARGET_ACT_KNOWN_VERBS: &str = "read, screenshot, navigate, set_field, insert_text, append_text, set_selection, click, dblclick, hover, tap, scroll, dispatch_event, clear, focus, blur, select_text, check, uncheck, type, key, press, select, submit, save, cleanup_notepad_tabs, run_shell, focus_window, set_window_bounds";

#[derive(Clone, Debug, JsonSchema)]
#[schemars(transparent)]
pub struct TargetActVerb(String);

impl<'de> Deserialize<'de> for TargetActVerb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self(raw.trim().to_ascii_lowercase()))
    }
}

impl TargetActVerb {
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetActParams {
    /// The high-level operation to perform on the session target.
    pub verb: TargetActVerb,
    /// `navigate`: destination URL.
    #[serde(default)]
    pub url: Option<String>,
    /// `screenshot`: output file path. `save`: existing document file path
    /// used as the physical Source of Truth after the target-scoped save.
    #[serde(default)]
    pub path: Option<String>,
    /// `set_field`: target element id (from observe/find), for the UIA/CDP-id
    /// background tiers. `click` can also use this as an observed element id.
    /// Browser DOM actions use observed CDP ids when possible and otherwise
    /// treat this as a page element id.
    #[serde(default)]
    pub element_id: Option<String>,
    /// `set_field` / browser DOM action: strict CSS selector routed to the safe
    /// normal-Chrome bridge (background, no foreground, no DOM/action debugger attach).
    #[serde(default)]
    pub selector: Option<String>,
    /// `set_field`: full replacement text (empty clears the field). `save`:
    /// optional expected file contents for the post-save file-byte readback.
    /// `insert_text` / `append_text`: text to type through the target-scoped
    /// keyboard path.
    #[serde(default)]
    pub text: Option<String>,
    /// `key` / `press`: raw key or chord, e.g. `Ctrl+End`, `Tab`, `Ctrl+Z`.
    /// Prefer this over `keys` when a single chord string is easier for the
    /// caller to produce.
    #[serde(default)]
    pub key: Option<String>,
    /// `key` / `press`: explicit raw key list, e.g. `["ctrl", "end"]`.
    #[serde(default)]
    pub keys: Vec<String>,
    /// `set_selection`: zero-based selection start offset.
    #[serde(default, alias = "start")]
    pub selection_start: Option<u32>,
    /// `set_selection`: zero-based selection end offset.
    #[serde(default, alias = "end")]
    pub selection_end: Option<u32>,
    /// `set_field`: native/UIA role to resolve at action time. Browser DOM
    /// action: accessible/ARIA role to resolve.
    #[serde(default)]
    pub role: Option<String>,
    /// `set_field`: native/UIA accessible name to resolve at action time.
    /// Browser DOM action: accessible name to resolve.
    #[serde(default)]
    pub name: Option<String>,
    /// `set_field`: native/UIA automation id to resolve at action time.
    #[serde(default)]
    pub automation_id: Option<String>,
    /// Browser DOM action: value match. For `select`, this is the option value
    /// when `option` is omitted. `cleanup_notepad_tabs`: modified-tab policy
    /// (`discard_modified` default, or `refuse_modified`).
    #[serde(default)]
    pub value: Option<String>,
    /// `select`: option text or option value.
    #[serde(default)]
    pub option: Option<String>,
    /// `select`: option label/text. Use when the select element itself is
    /// located separately and the option must be matched by label.
    #[serde(default, alias = "optionLabel", alias = "label")]
    pub option_label: Option<String>,
    /// `select`: zero-based option index.
    #[serde(default, alias = "optionIndex", alias = "index")]
    pub option_index: Option<i32>,
    /// `select`: one or more explicit option specs for single or multi-select.
    /// Each entry must contain exactly one of `value`, `label`, or `index`.
    #[serde(default)]
    pub options: Vec<TargetActSelectOption>,
    /// `dispatch_event`: DOM event type to dispatch on the matched element.
    #[serde(default, alias = "eventType")]
    pub event_type: Option<String>,
    /// `dispatch_event`: EventInit/CustomEventInit-style JSON object. `detail`
    /// creates a CustomEvent; common mouse/keyboard/input event types use their
    /// specialized constructors when available.
    #[serde(default, alias = "eventInit")]
    pub event_init: Option<Value>,
    /// `click`: click count for target element clicks. Defaults to 1; valid range is 1..=3.
    /// `dblclick` defaults to 2 and rejects any other count.
    #[serde(default, alias = "clickCount")]
    pub clicks: Option<u8>,
    /// `click` / `dblclick`: mouse button for browser/native click delivery.
    #[serde(default)]
    pub button: Option<TargetActMouseButton>,
    /// `click` / `dblclick`: modifier keys held while dispatching the click.
    #[serde(default)]
    pub modifiers: Vec<TargetActClickModifier>,
    /// `click` / `dblclick`: element-relative click position X in CSS pixels
    /// for browser DOM locator clicks. Use nested `position` for Playwright-style
    /// input, or `position_x`/`position_y` aliases for flat callers.
    #[serde(default, alias = "positionX", alias = "offset_x", alias = "offsetX")]
    pub position_x: Option<i32>,
    /// `click` / `dblclick`: element-relative click position Y in CSS pixels.
    #[serde(default, alias = "positionY", alias = "offset_y", alias = "offsetY")]
    pub position_y: Option<i32>,
    /// `click` / `dblclick`: Playwright-style element-relative position.
    #[serde(default)]
    pub position: Option<TargetActClickPosition>,
    /// `click` / `type`: coordinate X for target-owned coordinate fallback.
    /// Defaults to screen coordinates; set coordinate_space for viewport/window-relative input.
    #[serde(default)]
    pub x: Option<i32>,
    /// `click` / `type`: coordinate Y for target-owned coordinate fallback.
    /// Defaults to screen coordinates; set coordinate_space for viewport/window-relative input.
    #[serde(default)]
    pub y: Option<i32>,
    /// `set_window_bounds`: requested outer-window width in screen pixels. Omit to
    /// preserve the current width (move-only). Must be > 0 when supplied.
    #[serde(default)]
    pub width: Option<i32>,
    /// `set_window_bounds`: requested outer-window height in screen pixels. Omit to
    /// preserve the current height (move-only). Must be > 0 when supplied.
    #[serde(default)]
    pub height: Option<i32>,
    /// `click` / `type`: coordinate space for x/y. `screen` uses desktop pixels,
    /// `window` uses the target outer-window origin, and `viewport` uses page client pixels.
    #[serde(default)]
    pub coordinate_space: Option<TargetActCoordinateSpace>,
    /// Browser DOM action readback wait budget (ms). Defaults to the browser
    /// bridge command budget and is capped by the daemon command timeout.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// Browser DOM action / delegated CDP field action: opt in to polling
    /// actionability before dispatch. Default false preserves existing behavior.
    #[serde(default)]
    pub auto_wait: bool,
    /// Auto-wait timeout in milliseconds when auto_wait=true.
    #[serde(default = "default_auto_wait_timeout_ms")]
    #[schemars(default = "default_auto_wait_timeout_ms", range(min = 50, max = 30000))]
    pub auto_wait_timeout_ms: u32,
    /// `run_shell`: executable/program name (arguments go in `args`).
    #[serde(default)]
    pub command: Option<String>,
    /// `run_shell`: literal arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// `run_shell`: working directory.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// `run_shell`: inline wait budget (ms). Defaults to 30000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetActSelectOption {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub index: Option<i32>,
}

#[derive(Copy, Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TargetActMouseButton {
    Left,
    Right,
    Middle,
}

impl TargetActMouseButton {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, JsonSchema, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetActClickModifier {
    Ctrl,
    Shift,
    Alt,
    Meta,
}

impl<'de> Deserialize<'de> for TargetActClickModifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de;

        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Ok(Self::Ctrl),
            "shift" => Ok(Self::Shift),
            "alt" | "option" => Ok(Self::Alt),
            "meta" | "super" | "cmd" | "command" => Ok(Self::Meta),
            other => Err(de::Error::custom(format!(
                "unsupported click modifier {other:?}; expected ctrl, shift, alt, or meta"
            ))),
        }
    }
}

impl TargetActClickModifier {
    const fn as_bridge_str(self) -> &'static str {
        match self {
            Self::Ctrl => "ctrl",
            Self::Shift => "shift",
            Self::Alt => "alt",
            Self::Meta => "meta",
        }
    }

    const fn as_act_click_str(self) -> &'static str {
        match self {
            Self::Ctrl => "ctrl",
            Self::Shift => "shift",
            Self::Alt => "alt",
            Self::Meta => "super",
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetActClickPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Copy, Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TargetActCoordinateSpace {
    Screen,
    Window,
    Viewport,
}

impl TargetActCoordinateSpace {
    const fn as_bridge_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Window => "window",
            Self::Viewport => "viewport",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct TargetActCoordinate {
    x: i32,
    y: i32,
    space: TargetActCoordinateSpace,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetActResponse {
    pub verb: String,
    /// True only when the delegated primitive completed and its own postcondition accepted.
    pub ok: bool,
    /// `ok`, `verify_needed`, `refused`, or `error`.
    pub status: String,
    /// The background primitive this verb routed to.
    pub delegated_tool: String,
    pub routing: String,
    /// The delegated tool's full response.
    pub result: Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActForegroundParams {
    /// Why the real OS-foreground route is needed. Required, non-empty; audited.
    pub reason: String,
    /// The target_act action to run under the temporary escalation.
    pub action: TargetActParams,
    /// Foreground input lease TTL in ms while the action runs (default 30000).
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActForegroundEscalation {
    pub reason: String,
    /// True when this call newly acquired the lease (and therefore released it).
    pub acquired_lease: bool,
    pub released_lease: bool,
    pub prior_profile: String,
    pub escalated_to: String,
    pub restored_profile: String,
    pub profile_restored: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActForegroundResponse {
    pub ok: bool,
    pub action: TargetActResponse,
    pub escalation: ActForegroundEscalation,
}

#[tool_router(router = background_router_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "High-level capability-preserving computer-use router (#1005/#1033/#1207/#1219/#1261/#1267/#1299/#1300). One verb, routed to the correct session-targeted primitive: background/target-scoped when sufficient, agent_logical_foreground/foreground_lane when foreground-equivalent semantics are required, and never implicit fallback to the human OS foreground. verb=read observes the target; verb=screenshot captures it; verb=navigate drives the owned browser target (Chrome bridge/CDP); verb=set_field replaces a web/UIA field's text by element id via target-capable tiers, by native/UIA role/name/automation_id resolved at action time, or by CSS selector through the safe normal-Chrome bridge; verb=insert_text replaces the current selection/caret text on an observed native editable element_id via exact native readback, or types text at the current caret after an optional target focus/click; verb=append_text appends to an observed native editable element_id via exact native readback, or moves the current caret to the end with Ctrl+End and types text; verb=set_selection sets an exact start/end selection on an observed web/native editable element; verb=click clicks a target element by observed element_id, selector/role/name DOM action, or x/y coordinate fallback on the owned target; verb=tap touch-taps a browser target element or viewport coordinate with Input.dispatchTouchEvent touchStart/touchEnd through raw CDP or the normal-profile Chrome bridge cdpInput lane, and never falls back to mouse click; verb=dispatch_event dispatches a caller-specified DOM event_type with event_init directly on a matched element through the session-owned normal Chrome bridge, bypassing actionability and reporting dispatchEvent's default_allowed result; verb=clear empties a matched editable element and fires input/change; verb=focus calls DOM.focus and verifies activeElement; verb=blur calls DOM.blur and verifies activeElement moved away; verb=select_text/selectText selects all text in the matched element and verifies the selection; verb=check/uncheck set a native checkbox/radio to the requested checked state, no-op if already there, and verify checked-property readback; verb=type optionally focuses x/y then types text into the session-owned browser active element or leased foreground target; verb=key presses a raw key/chord such as Ctrl+End or Tab; verb=press presses a named button/link in the session-owned tab, or a raw key/chord when key/keys is supplied; verb=select chooses native <select> option(s) by value, label, or zero-based index via option/value/option_label/option_index/options[] and fires input/change; verb=submit calls HTMLFormElement.requestSubmit() for a matched form/submitter; verb=save persists an already-owned Notepad target to an existing file path and verifies file bytes as the Source of Truth; verb=cleanup_notepad_tabs removes stale restored tabs from an owned hidden-desktop Notepad target while keeping the requested file tab; verb=run_shell runs a command in the session workspace; verb=focus_window intentionally activates the session target's top-level HWND only after the session is already break_glass/full_capability and holds the foreground input lease, so Codex clients can use an existing target_act schema when they cannot hot-add act_focus_window after tools/list_changed; verb=set_window_bounds moves/resizes the bound top-level window (native Window target, or the browser window behind a Cdp target) via background-safe SetWindowPos without activation, accepts x/y and/or width/height, and returns requested-vs-actual outer bounds (GetWindowRect readback) plus minimized state and size_satisfied so responsive-UI/layout FSV can drive a window through boundary sizes. Prefer this over raw act_* primitives: it inherits target resolution, action audit, lane/lease guards, and structured refusals, so a normal session can keep valid foreground-equivalent capability without seizing the human foreground. Mutating failures are returned as ok=false with status=verify_needed/refused/error and the original structured error in result; no optimistic success. Bind a target first with set_target (discover one with window_list/cdp_open_tab)."
    )]
    pub async fn target_act(
        &self,
        params: Parameters<TargetActParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetActResponse>, ErrorData> {
        let params = params.0;
        let verb = params.verb.as_str().to_owned();
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "target_act",
            verb = verb.as_str(),
            "tool.invocation kind=target_act"
        );

        let (delegated_tool, ok, status, result) = match verb.as_str() {
            "read" => {
                let session_id = target_act_session_id(&request_context, "read")?;
                let target = self.session_target(Some(&session_id))?;
                let request_details = json!({
                    "session_id": &session_id,
                    "verb": "read",
                    "requires_agent_logical_foreground": true,
                    "no_human_os_foreground_fallback": true,
                });
                match target_act_read_delegated_tool(target.as_ref()) {
                    Ok("cdp_target_info") => {
                        let response = self
                            .cdp_target_info(
                                Parameters(CdpTargetInfoParams {
                                    window_hwnd: None,
                                    cdp_target_id: None,
                                }),
                                request_context,
                            )
                            .await?;
                        (
                            "cdp_target_info",
                            true,
                            TARGET_ACT_STATUS_OK,
                            target_act_result(&response.0)?,
                        )
                    }
                    Ok("observe") => {
                        let response = self
                            .observe(Parameters(ObserveParams::default()), request_context)
                            .await?;
                        (
                            "observe",
                            true,
                            TARGET_ACT_STATUS_OK,
                            target_act_result(&response.0)?,
                        )
                    }
                    Ok(other) => {
                        return Err(mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            format!("target_act read resolved unknown delegated tool {other:?}"),
                        ));
                    }
                    Err(error) => {
                        self.audit_action_denied_with_details_for_session(
                            "target_act",
                            &error,
                            &request_details,
                            &session_id,
                        );
                        (
                            "observe",
                            false,
                            target_act_error_status(&error),
                            target_act_error_result("target_act", error),
                        )
                    }
                }
            }
            "screenshot" => {
                let path = require_param(params.path, "screenshot", "path")?;
                let session_id = target_act_session_id(&request_context, "screenshot")?;
                let target = self.session_target(Some(&session_id))?;
                let request_details = json!({
                    "session_id": &session_id,
                    "verb": "screenshot",
                    "path": &path,
                    "requires_agent_logical_foreground": true,
                    "no_human_os_foreground_fallback": true,
                });
                match target {
                    Some(SessionTarget::Cdp {
                        window_hwnd,
                        cdp_target_id,
                    }) => {
                        let activated = self
                            .cdp_activate_tab(
                                Parameters(CdpActivateTabParams {
                                    window_hwnd: Some(window_hwnd),
                                    cdp_target_id: Some(cdp_target_id),
                                    wait_timeout_ms: params.wait_timeout_ms,
                                }),
                                request_context.clone(),
                            )
                            .await?;
                        let response = self
                            .capture_screenshot(
                                Parameters(CaptureScreenshotParams {
                                    path,
                                    region: None,
                                    window_hwnd: Some(window_hwnd),
                                    overwrite: true,
                                    max_pixels: None,
                                    max_long_edge: None,
                                }),
                                request_context,
                            )
                            .await?;
                        let mut result = target_act_result(&response.0)?;
                        if let Some(object) = result.as_object_mut() {
                            object.insert(
                                "target_act_visual_route".to_owned(),
                                json!("cdp_activate_tab_then_passive_window_capture"),
                            );
                            object.insert(
                                "activated_target".to_owned(),
                                target_act_result(&activated.0)?,
                            );
                        }
                        ("capture_screenshot", true, TARGET_ACT_STATUS_OK, result)
                    }
                    Some(SessionTarget::Window { .. }) => target_act_delegate_response(
                        "capture_screenshot",
                        self.capture_screenshot(
                            Parameters(CaptureScreenshotParams {
                                path,
                                region: None,
                                window_hwnd: None,
                                overwrite: true,
                                max_pixels: None,
                                max_long_edge: None,
                            }),
                            request_context,
                        )
                        .await,
                    )?,
                    None => {
                        let error = mcp_error(
                            error_codes::TARGET_NOT_SET,
                            "target_act verb=screenshot requires this MCP session to have an agent_logical_foreground/foreground_lane target; refusing capture_screenshot's legacy human OS foreground fallback",
                        );
                        self.audit_action_denied_with_details_for_session(
                            "target_act",
                            &error,
                            &request_details,
                            &session_id,
                        );
                        (
                            "capture_screenshot",
                            false,
                            target_act_error_status(&error),
                            target_act_error_result("target_act", error),
                        )
                    }
                }
            }
            "navigate" => {
                let url = require_param(params.url, "navigate", "url")?;
                let response = self
                    .cdp_navigate_tab(
                        Parameters(CdpNavigateTabParams {
                            window_hwnd: None,
                            cdp_target_id: None,
                            action: CdpNavigateAction::Navigate,
                            url: Some(url),
                            wait_timeout_ms: None,
                            ignore_cache: None,
                        }),
                        request_context,
                    )
                    .await?;
                (
                    "cdp_navigate_tab",
                    true,
                    TARGET_ACT_STATUS_OK,
                    target_act_result(&response.0)?,
                )
            }
            "set_field" => {
                if target_act_coordinate(&params)?.is_some() {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "target_act verb=set_field does not accept x/y because set_field is a replacement operation; use verb=type with x/y for coordinate focus + keyboard text",
                    ));
                }
                match target_act_set_field_target(&params)? {
                    TargetActSetFieldTarget::Browser {
                        selector,
                        element_id,
                    } => {
                        // Background-safe web field replace in the user's normal Chrome
                        // via the safe bridge (no foreground, no DOM/action debugger attach,
                        // no UIA) — the #1000/#1005 path for forms perceived UIA-only.
                        let response = self
                            .browser_set_value(
                                Parameters(BrowserSetValueParams {
                                    text: params.text.unwrap_or_default(),
                                    selector,
                                    element_id,
                                    active_element: false,
                                    cdp_target_id: None,
                                    window_hwnd: None,
                                }),
                                request_context,
                            )
                            .await;
                        target_act_delegate_response("browser_set_value", response)?
                    }
                    TargetActSetFieldTarget::Native {
                        element_id,
                        locator,
                    } => {
                        let response = self
                            .act_set_field_text(
                                Parameters(ActSetFieldTextParams {
                                    element_id,
                                    locator,
                                    text: params.text.unwrap_or_default(),
                                    verify_timeout_ms: default_verify_timeout_ms(),
                                    auto_wait: params.auto_wait,
                                    auto_wait_timeout_ms: params.auto_wait_timeout_ms,
                                }),
                                request_context,
                            )
                            .await;
                        target_act_delegate_response("act_set_field_text", response)?
                    }
                }
            }
            "insert_text" => {
                target_act_insert_or_append_text(self, &params, &request_context, false).await?
            }
            "append_text" => {
                target_act_insert_or_append_text(self, &params, &request_context, true).await?
            }
            "set_selection" => target_act_set_selection(self, &params, &request_context).await?,
            action @ ("click" | "dblclick") => {
                if target_act_coordinate(&params)?.is_some() {
                    if target_act_has_any_locator(&params) {
                        return Err(mcp_error(
                            error_codes::TOOL_PARAMS_INVALID,
                            format!(
                                "target_act verb={action} accepts either x/y coordinates or an element/DOM locator, not both"
                            ),
                        ));
                    }
                    target_act_coordinate_click(self, action, &params, &request_context).await?
                } else if target_act_has_dom_locator(&params) {
                    let bridge_action: &'static str = if action == "dblclick" {
                        "dblclick"
                    } else {
                        "click"
                    };
                    target_act_dom_locator_pointer(
                        self,
                        bridge_action,
                        bridge_action,
                        &params,
                        &request_context,
                    )
                    .await?
                } else {
                    let element_id =
                        require_param(params.element_id.clone(), action, "element_id")?;
                    if let Some(element_id) = target_act_legacy_click_element_id(&element_id)? {
                        if target_act_click_position(&params)?.is_some() {
                            return Err(mcp_error(
                                error_codes::TOOL_PARAMS_INVALID,
                                format!(
                                    "target_act verb={action} position is supported for browser DOM locators, not observed native/UIA element_id clicks"
                                ),
                            ));
                        }
                        let clicks = target_act_click_count_for_action(action, params.clicks)?;
                        let click_params = target_act_click_params(
                            element_id,
                            clicks,
                            params.button,
                            &params.modifiers,
                        )?;
                        let response = self
                            .act_click(Parameters(click_params), request_context)
                            .await;
                        target_act_delegate_response("act_click", response)?
                    } else {
                        target_act_browser_dom_action(self, action, &params, &request_context)
                            .await?
                    }
                }
            }
            "tap" => target_act_touch_tap(self, &params, &request_context).await?,
            "hover" => target_act_hover(self, &params, &request_context).await?,
            "dispatch_event" | "dispatchevent" => {
                target_act_browser_dom_action(self, "dispatch_event", &params, &request_context)
                    .await?
            }
            "clear" => {
                target_act_browser_dom_primitive(self, "clear", &params, &request_context).await?
            }
            "focus" => {
                target_act_browser_dom_primitive(self, "focus", &params, &request_context).await?
            }
            "blur" => {
                target_act_browser_dom_primitive(self, "blur", &params, &request_context).await?
            }
            "select_text" | "selecttext" => {
                target_act_browser_dom_primitive(self, "select_text", &params, &request_context)
                    .await?
            }
            "check" => {
                target_act_browser_dom_action(self, "check", &params, &request_context).await?
            }
            "uncheck" => {
                target_act_browser_dom_action(self, "uncheck", &params, &request_context).await?
            }
            "type" => {
                if target_act_has_any_locator(&params) {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "target_act verb=type accepts text plus optional x/y coordinate; use verb=set_field/click for element, selector, role, name, value, or option locators",
                    ));
                }
                let text = require_param(params.text.clone(), "type", "text")?;
                let coordinate_result = if target_act_coordinate(&params)?.is_some() {
                    let (delegated_tool, ok, status, result) =
                        target_act_coordinate_click(self, "click", &params, &request_context)
                            .await?;
                    if !ok {
                        return Ok(Json(TargetActResponse {
                            verb: verb.as_str().to_owned(),
                            ok,
                            status: status.to_owned(),
                            delegated_tool: delegated_tool.to_owned(),
                            routing: target_act_routing_description(),
                            result,
                        }));
                    }
                    Some((delegated_tool, result))
                } else {
                    None
                };
                let type_params = target_act_type_params(text, params.wait_timeout_ms)?;
                let response = self
                    .act_type(Parameters(type_params), request_context)
                    .await;
                let (_delegated_tool, ok, status, type_result) =
                    target_act_delegate_response("act_type", response)?;
                if let Some((coordinate_tool, coordinate_result)) = coordinate_result {
                    (
                        "chrome_debugger_bridge.coordinateClick+act_type",
                        ok,
                        status,
                        json!({
                            "coordinate_delegated_tool": coordinate_tool,
                            "coordinate_click": coordinate_result,
                            "type": type_result,
                        }),
                    )
                } else {
                    ("act_type", ok, status, type_result)
                }
            }
            "key" | "key_chord" => target_act_key_press(self, &params, &request_context).await?,
            "press" => {
                if target_act_has_key_chord(&params) {
                    target_act_key_press(self, &params, &request_context).await?
                } else {
                    // press a named button/link = a real trusted click on the
                    // bridge target; keep the synthetic "press" DOM-action only as
                    // the raw-CDP/native fallback (#1348 headline #2).
                    target_act_dom_locator_pointer(
                        self,
                        "click",
                        "press",
                        &params,
                        &request_context,
                    )
                    .await?
                }
            }
            "select" => {
                target_act_browser_dom_action(self, "select", &params, &request_context).await?
            }
            "submit" => {
                target_act_browser_dom_action(self, "submit", &params, &request_context).await?
            }
            "save" => target_act_save(self, &params, &request_context).await?,
            "cleanup_notepad_tabs" => {
                target_act_cleanup_notepad_tabs(self, &params, &request_context).await?
            }
            "run_shell" => {
                let command = require_param(params.command, "run_shell", "command")?;
                let response = self
                    .act_run_shell(
                        Parameters(ActRunShellParams {
                            command,
                            args: params.args,
                            working_dir: params.working_dir,
                            env: BTreeMap::new(),
                            timeout_ms: params
                                .timeout_ms
                                .unwrap_or(DEFAULT_TARGET_ACT_SHELL_TIMEOUT_MS),
                            execution_mode: ActRunShellExecutionMode::Inline,
                            durable_timeout_ms: None,
                            idempotency_key: None,
                        }),
                        request_context,
                    )
                    .await?;
                (
                    "act_run_shell",
                    true,
                    TARGET_ACT_STATUS_OK,
                    target_act_result(&response.0)?,
                )
            }
            "focus_window" => {
                let session_id = target_act_session_id(&request_context, "focus_window")?;
                let request_details = json!({
                    "session_id": &session_id,
                    "verb": "focus_window",
                    "delegated_tool": "act_focus_window",
                    "requires_tool_profile": "break_glass_or_full_capability",
                    "requires_foreground_input_lease": true,
                    "target_source": "session_target",
                    "no_human_os_foreground_fallback": true,
                });
                if let Err(error) =
                    self.admit_tool_call_for_profile("act_focus_window", Some(&session_id))
                {
                    self.audit_action_denied_with_details_for_session(
                        "target_act",
                        &error,
                        &request_details,
                        &session_id,
                    );
                    (
                        "act_focus_window",
                        false,
                        target_act_error_status(&error),
                        target_act_error_result("act_focus_window", error),
                    )
                } else {
                    let target = self.session_target(Some(&session_id))?;
                    let target = match target {
                        Some(target) => target,
                        None => {
                            let error = target_act_focus_window_missing_target_error();
                            self.audit_action_denied_with_details_for_session(
                                "target_act",
                                &error,
                                &request_details,
                                &session_id,
                            );
                            return Ok(Json(TargetActResponse {
                                verb: verb.as_str().to_owned(),
                                ok: false,
                                status: target_act_error_status(&error).to_owned(),
                                delegated_tool: "act_focus_window".to_owned(),
                                routing: target_act_routing_description(),
                                result: target_act_error_result("act_focus_window", error),
                            }));
                        }
                    };
                    if let Err(error) =
                        target_act_focus_window_preflight(self, &session_id, &target)
                    {
                        self.audit_action_denied_with_details_for_session(
                            "target_act",
                            &error,
                            &request_details,
                            &session_id,
                        );
                        return Ok(Json(TargetActResponse {
                            verb: verb.as_str().to_owned(),
                            ok: false,
                            status: target_act_error_status(&error).to_owned(),
                            delegated_tool: "act_focus_window".to_owned(),
                            routing: target_act_routing_description(),
                            result: target_act_error_result("act_focus_window", error),
                        }));
                    }
                    let focus_params = match target_act_focus_window_params(Some(&target)) {
                        Ok(params) => params,
                        Err(error) => {
                            self.audit_action_denied_with_details_for_session(
                                "target_act",
                                &error,
                                &request_details,
                                &session_id,
                            );
                            return Ok(Json(TargetActResponse {
                                verb: verb.as_str().to_owned(),
                                ok: false,
                                status: target_act_error_status(&error).to_owned(),
                                delegated_tool: "act_focus_window".to_owned(),
                                routing: target_act_routing_description(),
                                result: target_act_error_result("act_focus_window", error),
                            }));
                        }
                    };
                    let response = self
                        .act_focus_window(Parameters(focus_params), request_context)
                        .await;
                    target_act_delegate_response("act_focus_window", response)?
                }
            }
            "scroll" => target_act_scroll(self, &params, &request_context).await?,
            "set_window_bounds" => target_act_set_window_bounds(self, &params, &request_context)?,
            other => return Err(target_act_unknown_verb_error(other)),
        };

        Ok(Json(TargetActResponse {
            verb: verb.as_str().to_owned(),
            ok,
            status: status.to_owned(),
            delegated_tool: delegated_tool.to_owned(),
            routing: target_act_routing_description(),
            result,
        }))
    }

    #[tool(
        description = "Run one target_act action under a single, fully-audited real-OS-foreground escalation (#1352). Given a non-empty `reason` and an `action` (a target_act params object incl. its `verb`), this atomically: acquires the foreground input lease (ttl_ms, default 30000), sets profile=break_glass, runs the action via target_act, then RESTORES the prior profile and releases the lease if it newly acquired it — so a session does not have to hand-stitch control_lease_acquire + tool_profile_set + the action + restore and guess the ordering. Returns the action result plus an escalation readback (acquired_lease, released_lease, prior/restored profile). The prior profile is restored even when the action fails. Use for actions that genuinely need the hardware foreground tier (e.g. Shift+selection, app accelerators); prefer plain target_act for background-capable work."
    )]
    pub async fn act_foreground(
        &self,
        params: Parameters<ActForegroundParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActForegroundResponse>, ErrorData> {
        let params = params.0;
        let reason = params.reason.trim().to_owned();
        if reason.is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_foreground requires a non-empty reason for the foreground escalation audit",
            ));
        }
        let session_id = target_act_session_id(&request_context, "act_foreground")?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_foreground",
            verb = params.action.verb.as_str(),
            "tool.invocation kind=act_foreground"
        );
        let before = self.tool_profile_snapshot(Some(&session_id))?;
        let prior_profile = before.profile;
        let already_held = synapse_action::lease::status().owner_session_id.as_deref()
            == Some(session_id.as_str());
        let ttl_ms = params.ttl_ms.unwrap_or(30_000);

        // 1) acquire/renew the foreground input lease.
        self.control_lease_acquire(
            Parameters(super::lease_tools::ControlLeaseAcquireParams { ttl_ms }),
            request_context.clone(),
        )
        .await?;
        let acquired = !already_held;

        // 2) escalate to break_glass (requires the lease we just took + confirm + reason).
        self.tool_profile_set(
            Parameters(super::tool_profiles::ToolProfileSetParams {
                profile: super::tool_profiles::ToolProfileKind::BreakGlass,
                reason: Some(reason.clone()),
                confirm_break_glass: true,
            }),
            request_context.clone(),
        )
        .await?;

        // 3) run the action — capture, do NOT early-return, so restore always runs.
        let action_result = self
            .target_act(Parameters(params.action), request_context.clone())
            .await;

        // 4) restore the prior profile (break_glass still requires the lease, which we
        //    still hold at this point).
        let prior_needs_confirm = matches!(
            prior_profile,
            super::tool_profiles::ToolProfileKind::BreakGlass
                | super::tool_profiles::ToolProfileKind::FullCapability
                | super::tool_profiles::ToolProfileKind::BrowserDebugger
        );
        let profile_restored = self
            .tool_profile_set(
                Parameters(super::tool_profiles::ToolProfileSetParams {
                    profile: prior_profile,
                    reason: Some(format!("restore after act_foreground: {reason}")),
                    confirm_break_glass: prior_needs_confirm,
                }),
                request_context.clone(),
            )
            .await
            .is_ok();

        // 5) release the lease only if this call newly acquired it.
        let released_lease = if acquired {
            self.control_lease_release(request_context.clone())
                .await
                .is_ok()
        } else {
            false
        };

        let action = action_result?.0;
        Ok(Json(ActForegroundResponse {
            ok: action.ok,
            escalation: ActForegroundEscalation {
                reason,
                acquired_lease: acquired,
                released_lease,
                prior_profile: prior_profile.as_str().to_owned(),
                escalated_to: "break_glass".to_owned(),
                restored_profile: prior_profile.as_str().to_owned(),
                profile_restored,
            },
            action,
        }))
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetActSaveResponse {
    ok: bool,
    method: String,
    backend_tier_used: String,
    required_foreground: bool,
    source_of_truth: String,
    path: String,
    target_hwnd: i64,
    target_process_name: String,
    target_window_title: String,
    before_len: u64,
    after_len: u64,
    before_sha256: String,
    after_sha256: String,
    changed: bool,
    expected_text_sha256: Option<String>,
    expected_text_matched: Option<bool>,
    attempts: Vec<TargetActSaveAttempt>,
    postcondition: crate::m2::postcondition::ActPostcondition,
    elapsed_ms: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetActSaveAttempt {
    method: String,
    command_source: String,
    sent: bool,
    detail: String,
    win32_result: Option<usize>,
}

struct TargetActFileSnapshot {
    len: u64,
    sha256: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetActCleanupNotepadTabsResponse {
    ok: bool,
    method: String,
    backend_tier_used: String,
    required_foreground: bool,
    source_of_truth: String,
    target_hwnd: i64,
    target_process_name: String,
    target_window_title_before: String,
    target_window_title_after: String,
    keep_path: String,
    keep_file_name: String,
    modified_stale_policy: String,
    desktop_names: Vec<String>,
    active_desktop_name: String,
    before_tabs: Vec<TargetActNotepadTabReadback>,
    closed_tabs: Vec<TargetActNotepadTabCloseAttempt>,
    after_tabs: Vec<TargetActNotepadTabReadback>,
    before_signature: String,
    after_signature: String,
    stale_tabs_before: u32,
    stale_tabs_after: u32,
    modified_stale_tabs_discarded: u32,
    postcondition: crate::m2::postcondition::ActPostcondition,
    elapsed_ms: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetActNotepadTabReadback {
    element_id: ElementId,
    name: String,
    document_name: String,
    modified: bool,
    keep: bool,
    bbox: Rect,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetActNotepadTabCloseAttempt {
    tab: TargetActNotepadTabReadback,
    selected: bool,
    scrolled_into_view: bool,
    stale_bbox_before: Rect,
    stale_bbox_after_select: Rect,
    close_method: String,
    close_button_element_id: Option<ElementId>,
    close_invoked: bool,
    discard_button_element_id: Option<ElementId>,
    discarded_modified: bool,
    tabs_after: Vec<TargetActNotepadTabReadback>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TargetActModifiedStalePolicy {
    DiscardModified,
    RefuseModified,
}

impl TargetActModifiedStalePolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DiscardModified => "discard_modified",
            Self::RefuseModified => "refuse_modified",
        }
    }
}

#[derive(Clone)]
struct TargetActNotepadSnapshot {
    desktop_names: Vec<String>,
    active_desktop_name: String,
    context: synapse_core::ForegroundContext,
    nodes: Vec<AccessibleNode>,
    tabs: Vec<TargetActNotepadTabReadback>,
}

#[derive(Clone)]
struct TargetActHiddenWindowSnapshot {
    hwnd: i64,
    nodes: Vec<AccessibleNode>,
}

async fn target_act_cleanup_notepad_tabs(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let result = target_act_cleanup_notepad_tabs_impl(service, params, request_context).await;
    let _ = service.audit_action_result_for_request("target_act", &result, request_context);
    match result {
        Ok(response) => Ok((
            "target_window_notepad_tab_cleanup",
            true,
            TARGET_ACT_STATUS_OK,
            target_act_result(&response)?,
        )),
        Err(error) => Ok((
            "target_window_notepad_tab_cleanup",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_window_notepad_tab_cleanup", error),
        )),
    }
}

async fn target_act_cleanup_notepad_tabs_impl(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<TargetActCleanupNotepadTabsResponse, ErrorData> {
    let started = Instant::now();
    target_act_validate_cleanup_notepad_tabs_params(params)?;
    let session_id = target_act_session_id(request_context, "cleanup_notepad_tabs")?;
    let keep_path = require_param(params.path.clone(), "cleanup_notepad_tabs", "path")?;
    let keep_path =
        canonical_existing_file_for_target_act_path("cleanup_notepad_tabs", Path::new(&keep_path))?;
    let keep_file_name = target_act_file_name(&keep_path, "cleanup_notepad_tabs")?;
    let modified_policy = target_act_modified_stale_policy(params.value.as_deref())?;
    let verify_timeout_ms = target_act_cleanup_notepad_tabs_verify_timeout(params.wait_timeout_ms)?;
    let target = service.session_target(Some(&session_id))?.ok_or_else(|| {
        mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act verb=cleanup_notepad_tabs requires this MCP session to have an owned hidden-desktop Notepad window target; call act_launch/set_target first",
        )
    })?;
    let hwnd = match target {
        SessionTarget::Window { hwnd } => hwnd,
        SessionTarget::Cdp { .. } => {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                "target_act verb=cleanup_notepad_tabs supports native Notepad window targets only",
            ));
        }
    };
    service.ensure_target_claim_allows_session("target_act", &session_id, &target)?;

    let mut before =
        target_act_hidden_notepad_snapshot(service, &session_id, hwnd, &keep_file_name)?;
    if !before
        .context
        .process_name
        .eq_ignore_ascii_case("notepad.exe")
    {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb=cleanup_notepad_tabs only supports Notepad hidden targets; target 0x{hwnd:x} is process {} title {:?}",
                before.context.process_name, before.context.window_title
            ),
        ));
    }
    if !target_act_notepad_title_matches_path(&before.context.window_title, &keep_path) {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb=cleanup_notepad_tabs refused: Notepad target title {:?} does not match keep file SoT {}",
                before.context.window_title,
                keep_path.display()
            ),
        ));
    }
    target_act_validate_notepad_keep_tab(&before.tabs, &keep_file_name)?;
    let before_signature = target_act_notepad_tabs_signature(&before.tabs);
    let before_tabs = before.tabs.clone();
    let target_window_title_before = before.context.window_title.clone();
    let stale_tabs_before = target_act_stale_notepad_tab_count(&before.tabs);
    let request_details = json!({
        "session_id": &session_id,
        "verb": "cleanup_notepad_tabs",
        "delegated_tool": "target_window_notepad_tab_cleanup",
        "target_hwnd": hwnd,
        "target_process_name": before.context.process_name,
        "target_window_title": before.context.window_title,
        "keep_path": keep_path.display().to_string(),
        "keep_file_name": keep_file_name,
        "desktop_names": before.desktop_names,
        "source_of_truth": TARGET_ACT_CLEANUP_NOTEPAD_TABS_SOURCE_OF_TRUTH,
        "before_signature": before_signature,
        "stale_tabs_before": stale_tabs_before,
        "modified_stale_policy": modified_policy.as_str(),
        "verify_timeout_ms": verify_timeout_ms,
        "requires_session_owned_hidden_desktop": true,
        "required_foreground": false,
        "no_human_os_foreground_fallback": true,
    });
    service.audit_action_started_with_details_for_request(
        "target_act",
        &request_details,
        request_context,
    )?;

    let deadline = Instant::now() + Duration::from_millis(u64::from(verify_timeout_ms));
    let mut attempts = Vec::new();
    loop {
        target_act_validate_notepad_keep_tab(&before.tabs, &keep_file_name)?;
        if modified_policy == TargetActModifiedStalePolicy::RefuseModified {
            if let Some(stale) = target_act_first_modified_stale_notepad_tab(&before.tabs) {
                return Err(target_act_refuse_modified_stale_tab_error(&stale));
            }
        }
        let Some(stale) = target_act_next_stale_notepad_tab(&before.tabs) else {
            break;
        };
        if attempts.len() > 64 {
            return Err(target_act_cleanup_notepad_tabs_postcondition_error(
                "tab cleanup iteration limit reached",
                &keep_path,
                &keep_file_name,
                &before_signature,
                &before,
                &before,
                &attempts,
                verify_timeout_ms,
            ));
        }
        if Instant::now() >= deadline {
            return Err(target_act_cleanup_notepad_tabs_postcondition_error(
                "tab cleanup timed out before all stale tabs were removed",
                &keep_path,
                &keep_file_name,
                &before_signature,
                &before,
                &before,
                &attempts,
                verify_timeout_ms,
            ));
        }
        let attempt = target_act_cleanup_close_notepad_tab(
            service,
            &session_id,
            hwnd,
            &keep_file_name,
            stale,
            modified_policy,
            deadline,
        )
        .await?;
        attempts.push(attempt);
        before = target_act_hidden_notepad_snapshot(service, &session_id, hwnd, &keep_file_name)?;
    }

    let after = target_act_hidden_notepad_snapshot(service, &session_id, hwnd, &keep_file_name)?;
    target_act_validate_notepad_keep_tab(&after.tabs, &keep_file_name)?;
    let stale_tabs_after = target_act_stale_notepad_tab_count(&after.tabs);
    if stale_tabs_after != 0 {
        return Err(target_act_cleanup_notepad_tabs_postcondition_error(
            "stale tabs remained after cleanup",
            &keep_path,
            &keep_file_name,
            &before_signature,
            &before,
            &after,
            &attempts,
            verify_timeout_ms,
        ));
    }
    let after_signature = target_act_notepad_tabs_signature(&after.tabs);
    let changed = before_signature != after_signature;
    let modified_stale_tabs_discarded = attempts
        .iter()
        .filter(|attempt| attempt.discarded_modified)
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    Ok(TargetActCleanupNotepadTabsResponse {
        ok: true,
        method: "uia_select_tab_then_invoke_close_tab".to_owned(),
        backend_tier_used: "uia_invoke_hidden_desktop_target".to_owned(),
        required_foreground: false,
        source_of_truth: TARGET_ACT_CLEANUP_NOTEPAD_TABS_SOURCE_OF_TRUTH.to_owned(),
        target_hwnd: hwnd,
        target_process_name: after.context.process_name.clone(),
        target_window_title_before,
        target_window_title_after: after.context.window_title.clone(),
        keep_path: keep_path.display().to_string(),
        keep_file_name,
        modified_stale_policy: modified_policy.as_str().to_owned(),
        desktop_names: after.desktop_names.clone(),
        active_desktop_name: after.active_desktop_name.clone(),
        before_tabs,
        closed_tabs: attempts,
        after_tabs: after.tabs,
        before_signature: before_signature.clone(),
        after_signature: after_signature.clone(),
        stale_tabs_before,
        stale_tabs_after,
        modified_stale_tabs_discarded,
        postcondition: crate::m2::postcondition::ActPostcondition {
            status: "verified_state".to_owned(),
            observed_delta: Some(changed),
            source_of_truth: Some(TARGET_ACT_CLEANUP_NOTEPAD_TABS_SOURCE_OF_TRUTH.to_owned()),
            before_signature: Some(before_signature.clone()),
            after_signature: Some(after_signature),
            detail: Some("target_act cleanup_notepad_tabs verified stale tab count is zero on the session-owned hidden Notepad target".to_owned()),
        },
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

async fn target_act_cleanup_close_notepad_tab(
    service: &SynapseService,
    session_id: &str,
    hwnd: i64,
    keep_file_name: &str,
    stale: TargetActNotepadTabReadback,
    modified_policy: TargetActModifiedStalePolicy,
    deadline: Instant,
) -> Result<TargetActNotepadTabCloseAttempt, ErrorData> {
    let stale_bbox_before = stale.bbox;
    let mut action_stale = stale.clone();
    let mut scrolled_into_view = false;
    if !target_act_rect_has_area(action_stale.bbox) {
        target_act_scroll_uia_element_into_view(
            "cleanup_notepad_tabs.scroll_stale_tab_into_view",
            &action_stale.element_id,
        )?;
        scrolled_into_view = true;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let scrolled =
            target_act_hidden_notepad_snapshot(service, session_id, hwnd, keep_file_name)?;
        action_stale =
            target_act_matching_notepad_tab(&scrolled.tabs, &stale).ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "target_act verb=cleanup_notepad_tabs scrolled stale hidden Notepad tab {:?}, but the tab was absent from the post-scroll SoT",
                        stale.name
                    ),
                )
            })?;
    }
    target_act_invoke_uia_element(
        "cleanup_notepad_tabs.select_stale_tab",
        &action_stale.element_id,
    )?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut selected =
        target_act_hidden_notepad_snapshot(service, session_id, hwnd, keep_file_name)?;
    let mut selected_stale =
        target_act_matching_notepad_tab(&selected.tabs, &action_stale).ok_or_else(|| {
            mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "target_act verb=cleanup_notepad_tabs selected stale tab {:?}, but the tab was absent from the selected hidden Notepad SoT",
                    stale.name
                ),
            )
        })?;
    if !target_act_rect_has_area(selected_stale.bbox) {
        target_act_scroll_uia_element_into_view(
            "cleanup_notepad_tabs.scroll_selected_stale_tab_into_view",
            &selected_stale.element_id,
        )?;
        scrolled_into_view = true;
        tokio::time::sleep(Duration::from_millis(100)).await;
        selected = target_act_hidden_notepad_snapshot(service, session_id, hwnd, keep_file_name)?;
        selected_stale =
            target_act_matching_notepad_tab(&selected.tabs, &action_stale).ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "target_act verb=cleanup_notepad_tabs scrolled selected stale tab {:?}, but the tab was absent from the post-scroll SoT",
                        stale.name
                    ),
                )
            })?;
    }
    if !target_act_notepad_title_matches_document(
        &selected.context.window_title,
        &stale.document_name,
    ) {
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "target_act verb=cleanup_notepad_tabs selected stale tab {:?}, but Notepad title readback stayed {:?}",
                stale.name, selected.context.window_title
            ),
        ));
    }
    let (close_method, close_button_element_id) = if let Some(close_button) =
        target_act_notepad_close_tab_button(&selected.nodes, &selected_stale)
    {
        let close_button_element_id = Some(close_button.element_id.clone());
        target_act_invoke_uia_element(
            "cleanup_notepad_tabs.invoke_close_tab",
            &close_button.element_id,
        )?;
        ("uia_close_tab_button".to_owned(), close_button_element_id)
    } else if let Some(close_menu_item) = target_act_notepad_close_tab_menu_item_for_hidden_desktop(
        service,
        session_id,
        hwnd,
        &selected.nodes,
    )
    .await?
    {
        let close_button_element_id = Some(close_menu_item.element_id.clone());
        target_act_invoke_uia_element(
            "cleanup_notepad_tabs.invoke_file_close_tab",
            &close_menu_item.element_id,
        )?;
        (
            "uia_file_menu_close_tab".to_owned(),
            close_button_element_id,
        )
    } else {
        let close_glyph = target_act_notepad_close_tab_glyph(&selected.nodes, &selected_stale);
        return Err(mcp_error(
            error_codes::ACTION_ELEMENT_NOT_RESOLVED,
            format!(
                "target_act verb=cleanup_notepad_tabs could not find a UIA Close Tab button or File > Close tab menu item for stale hidden Notepad tab {:?}; close_glyph_present={}",
                stale.name,
                close_glyph.is_some()
            ),
        ));
    };
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut discard_button_element_id = None;
    let mut discarded_modified = false;
    let mut after_close =
        target_act_hidden_notepad_snapshot(service, session_id, hwnd, keep_file_name)?;
    if target_act_notepad_tab_still_present(&after_close.tabs, &stale) && stale.modified {
        if modified_policy == TargetActModifiedStalePolicy::DiscardModified {
            let discard_button = target_act_notepad_discard_button_for_hidden_desktop(
                service,
                session_id,
                hwnd,
                &after_close.nodes,
            )
            .map_err(|error| {
                mcp_error(
                    error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                    format!(
                        "target_act verb=cleanup_notepad_tabs close of modified stale tab {:?} did not remove the tab and no discard button was visible in the hidden target/popup SoT: {error}",
                        stale.name
                    ),
                )
            })?;
            discard_button_element_id = Some(discard_button.element_id.clone());
            target_act_invoke_uia_element(
                "cleanup_notepad_tabs.discard_modified_stale_tab",
                &discard_button.element_id,
            )?;
            discarded_modified = true;
            tokio::time::sleep(Duration::from_millis(150)).await;
            after_close =
                target_act_hidden_notepad_snapshot(service, session_id, hwnd, keep_file_name)?;
        }
    }
    while target_act_notepad_tab_still_present(&after_close.tabs, &stale) {
        if Instant::now() >= deadline {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "target_act verb=cleanup_notepad_tabs stale tab {:?} remained after Close Tab",
                    stale.name
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        after_close =
            target_act_hidden_notepad_snapshot(service, session_id, hwnd, keep_file_name)?;
    }

    Ok(TargetActNotepadTabCloseAttempt {
        tab: stale,
        selected: true,
        scrolled_into_view,
        stale_bbox_before,
        stale_bbox_after_select: selected_stale.bbox,
        close_method,
        close_button_element_id,
        close_invoked: true,
        discard_button_element_id,
        discarded_modified,
        tabs_after: after_close.tabs,
    })
}

fn target_act_validate_cleanup_notepad_tabs_params(
    params: &TargetActParams,
) -> Result<(), ErrorData> {
    if params.url.as_ref().is_some_and(|value| !value.is_empty())
        || params
            .command
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        || !params.args.is_empty()
        || params
            .working_dir
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        || params.text.as_ref().is_some_and(|value| !value.is_empty())
        || params
            .option
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        || params.clicks.is_some()
        || params
            .element_id
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .selector
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .role
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .name
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || target_act_coordinate(params)?.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=cleanup_notepad_tabs accepts only path, optional value=discard_modified|refuse_modified, and optional wait_timeout_ms",
        ));
    }
    Ok(())
}

fn target_act_cleanup_notepad_tabs_verify_timeout(value: Option<u64>) -> Result<u32, ErrorData> {
    let timeout = value.unwrap_or(u64::from(
        DEFAULT_TARGET_ACT_CLEANUP_NOTEPAD_TABS_TIMEOUT_MS,
    ));
    if !(250..=30_000).contains(&timeout) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb=cleanup_notepad_tabs wait_timeout_ms must be 250..=30000, got {timeout}"
            ),
        ));
    }
    Ok(u32::try_from(timeout).unwrap_or(DEFAULT_TARGET_ACT_CLEANUP_NOTEPAD_TABS_TIMEOUT_MS))
}

fn target_act_modified_stale_policy(
    value: Option<&str>,
) -> Result<TargetActModifiedStalePolicy, ErrorData> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("discard_modified") => Ok(TargetActModifiedStalePolicy::DiscardModified),
        Some("refuse_modified") => Ok(TargetActModifiedStalePolicy::RefuseModified),
        Some(other) => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb=cleanup_notepad_tabs value must be discard_modified or refuse_modified, got {other:?}"
            ),
        )),
    }
}

fn target_act_hidden_notepad_snapshot(
    service: &SynapseService,
    session_id: &str,
    hwnd: i64,
    keep_file_name: &str,
) -> Result<TargetActNotepadSnapshot, ErrorData> {
    let Some(hidden_desktop) = service.session_hidden_desktop_readback(session_id)? else {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb=cleanup_notepad_tabs requires session-owned hidden desktop resources; session {session_id:?} has none"
            ),
        ));
    };
    let mut misses = Vec::new();
    for desktop_name in &hidden_desktop.desktop_names {
        match crate::desktop_worker::hidden_desktop_window_snapshot(desktop_name, hwnd, 16) {
            Ok(snapshot) => {
                let tabs = target_act_notepad_tabs_from_nodes(&snapshot.tree.nodes, keep_file_name);
                return Ok(TargetActNotepadSnapshot {
                    desktop_names: hidden_desktop.desktop_names.clone(),
                    active_desktop_name: desktop_name.clone(),
                    context: snapshot.context,
                    nodes: snapshot.tree.nodes,
                    tabs,
                });
            }
            Err(error) if target_act_hidden_worker_target_miss(&error) => {
                misses.push(desktop_name.clone());
            }
            Err(error) => return Err(error),
        }
    }
    Err(mcp_error(
        error_codes::TARGET_WINDOW_NOT_FOUND,
        format!(
            "target_act verb=cleanup_notepad_tabs target HWND 0x{hwnd:x} was not found on this session's hidden desktop(s): {misses:?}"
        ),
    ))
}

fn target_act_hidden_worker_target_miss(error: &ErrorData) -> bool {
    matches!(
        target_act_error_code(error),
        Some(error_codes::TARGET_WINDOW_NOT_FOUND)
    )
}

fn target_act_notepad_tabs_from_nodes(
    nodes: &[AccessibleNode],
    keep_file_name: &str,
) -> Vec<TargetActNotepadTabReadback> {
    nodes
        .iter()
        .filter_map(|node| {
            let role = target_act_normalized_role(&node.role);
            if role != "tabitem" && role != "tab" {
                return None;
            }
            let (document_name, modified) = target_act_parse_notepad_tab_name(&node.name)?;
            let keep = target_act_names_equal(&document_name, keep_file_name);
            Some(TargetActNotepadTabReadback {
                element_id: node.element_id.clone(),
                name: node.name.trim().to_owned(),
                document_name,
                modified,
                keep,
                bbox: node.bbox,
            })
        })
        .collect()
}

fn target_act_parse_notepad_tab_name(name: &str) -> Option<(String, bool)> {
    let name = name.trim();
    for (suffix, modified) in [(". Modified.", true), (". Unmodified.", false)] {
        if let Some(document_name) = name.strip_suffix(suffix) {
            let document_name = document_name.trim().trim_start_matches('*').to_owned();
            if !document_name.is_empty() {
                return Some((document_name, modified));
            }
        }
    }
    None
}

fn target_act_validate_notepad_keep_tab(
    tabs: &[TargetActNotepadTabReadback],
    keep_file_name: &str,
) -> Result<(), ErrorData> {
    if tabs.is_empty() {
        return Ok(());
    }
    let keep_count = tabs.iter().filter(|tab| tab.keep).count();
    if keep_count == 1 {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "target_act verb=cleanup_notepad_tabs expected exactly one keep tab for {keep_file_name:?}, found {keep_count}; tabs={}",
            target_act_notepad_tab_names(tabs).join(" | ")
        ),
    ))
}

fn target_act_stale_notepad_tab_count(tabs: &[TargetActNotepadTabReadback]) -> u32 {
    tabs.iter()
        .filter(|tab| !tab.keep)
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn target_act_first_modified_stale_notepad_tab(
    tabs: &[TargetActNotepadTabReadback],
) -> Option<TargetActNotepadTabReadback> {
    tabs.iter().find(|tab| !tab.keep && tab.modified).cloned()
}

fn target_act_refuse_modified_stale_tab_error(stale: &TargetActNotepadTabReadback) -> ErrorData {
    mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "target_act verb=cleanup_notepad_tabs found modified stale hidden Notepad tab {:?} and value=refuse_modified was requested",
            stale.name
        ),
    )
}

fn target_act_next_stale_notepad_tab(
    tabs: &[TargetActNotepadTabReadback],
) -> Option<TargetActNotepadTabReadback> {
    tabs.iter()
        .filter(|tab| !tab.keep)
        .min_by_key(|tab| {
            (
                if target_act_rect_has_area(tab.bbox) {
                    0
                } else {
                    1
                },
                tab.bbox.x,
                tab.bbox.y,
                tab.name.clone(),
            )
        })
        .cloned()
}

fn target_act_notepad_tab_still_present(
    tabs: &[TargetActNotepadTabReadback],
    stale: &TargetActNotepadTabReadback,
) -> bool {
    tabs.iter().any(|tab| {
        tab.element_id == stale.element_id
            || (target_act_names_equal(&tab.document_name, &stale.document_name)
                && tab.modified == stale.modified)
    })
}

fn target_act_matching_notepad_tab(
    tabs: &[TargetActNotepadTabReadback],
    stale: &TargetActNotepadTabReadback,
) -> Option<TargetActNotepadTabReadback> {
    tabs.iter()
        .find(|tab| tab.element_id == stale.element_id)
        .or_else(|| {
            tabs.iter().find(|tab| {
                target_act_names_equal(&tab.document_name, &stale.document_name)
                    && tab.modified == stale.modified
            })
        })
        .cloned()
}

fn target_act_notepad_close_tab_button(
    nodes: &[AccessibleNode],
    stale: &TargetActNotepadTabReadback,
) -> Option<AccessibleNode> {
    nodes
        .iter()
        .filter(|node| {
            node.enabled
                && target_act_normalized_role(&node.role) == "button"
                && target_act_normalized_name(&node.name) == "closetab"
                && node.patterns.contains(&UiaPattern::Invoke)
        })
        .min_by_key(|node| target_act_rect_distance_score(node.bbox, stale))
        .cloned()
}

fn target_act_notepad_close_tab_glyph(
    nodes: &[AccessibleNode],
    stale: &TargetActNotepadTabReadback,
) -> Option<AccessibleNode> {
    nodes
        .iter()
        .filter(|node| {
            if !node.enabled || target_act_normalized_role(&node.role) != "text" {
                return false;
            }
            let bbox = node.bbox;
            if bbox.w <= 0 || bbox.h <= 0 || bbox.w > 40 || bbox.h > 40 {
                return false;
            }
            let (center_x, center_y) = target_act_rect_center(bbox);
            let left = i64::from(stale.bbox.x);
            let right = left + i64::from(stale.bbox.w);
            let top = i64::from(stale.bbox.y);
            let bottom = top + i64::from(stale.bbox.h);
            center_x > left + i64::from(stale.bbox.w) / 2
                && center_x <= right
                && center_y >= top
                && center_y <= bottom
                && !target_act_names_equal(&node.name, &stale.document_name)
        })
        .min_by_key(|node| target_act_rect_distance_score(node.bbox, stale))
        .cloned()
}

async fn target_act_notepad_close_tab_menu_item_for_hidden_desktop(
    service: &SynapseService,
    session_id: &str,
    target_hwnd: i64,
    target_nodes: &[AccessibleNode],
) -> Result<Option<AccessibleNode>, ErrorData> {
    let Some(file_menu) = target_act_notepad_file_menu_item(target_nodes) else {
        return Ok(None);
    };
    target_act_invoke_uia_element("cleanup_notepad_tabs.open_file_menu", &file_menu.element_id)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    if let Some(menu_item) = target_act_notepad_close_tab_menu_item(target_nodes) {
        return Ok(Some(menu_item));
    }
    let snapshots =
        target_act_hidden_desktop_auxiliary_snapshots(service, session_id, target_hwnd)?;
    for snapshot in snapshots {
        if let Some(menu_item) = target_act_notepad_close_tab_menu_item(&snapshot.nodes) {
            return Ok(Some(menu_item));
        }
    }
    Ok(None)
}

fn target_act_notepad_file_menu_item(nodes: &[AccessibleNode]) -> Option<AccessibleNode> {
    nodes
        .iter()
        .find(|node| {
            node.enabled
                && target_act_normalized_role(&node.role) == "menuitem"
                && target_act_normalized_name(&node.name) == "file"
                && node.patterns.contains(&UiaPattern::ExpandCollapse)
        })
        .cloned()
}

fn target_act_notepad_close_tab_menu_item(nodes: &[AccessibleNode]) -> Option<AccessibleNode> {
    nodes
        .iter()
        .find(|node| {
            if !node.enabled {
                return false;
            }
            let role = target_act_normalized_role(&node.role);
            if role != "menuitem" && role != "button" && role != "listitem" {
                return false;
            }
            target_act_normalized_name(&node.name).contains("closetab")
                && node.patterns.contains(&UiaPattern::Invoke)
        })
        .cloned()
}

fn target_act_notepad_discard_button(nodes: &[AccessibleNode]) -> Option<AccessibleNode> {
    nodes
        .iter()
        .find(|node| {
            if !node.enabled || target_act_normalized_role(&node.role) != "button" {
                return false;
            }
            let name = node.name.to_ascii_lowercase().replace('\u{2019}', "'");
            name.contains("discard")
                || name.contains("close without saving")
                || name.contains("do not save")
                || ((name.contains("don't") || name.contains("dont")) && name.contains("save"))
        })
        .cloned()
}

fn target_act_notepad_discard_button_for_hidden_desktop(
    service: &SynapseService,
    session_id: &str,
    target_hwnd: i64,
    target_nodes: &[AccessibleNode],
) -> Result<AccessibleNode, String> {
    if let Some(button) = target_act_notepad_discard_button(target_nodes) {
        return Ok(button);
    }
    let snapshots = target_act_hidden_desktop_auxiliary_snapshots(service, session_id, target_hwnd)
        .map_err(|error| error.to_string())?;
    let mut inspected = Vec::new();
    for snapshot in snapshots {
        inspected.push(format!("0x{:x}", snapshot.hwnd));
        if let Some(button) = target_act_notepad_discard_button(&snapshot.nodes) {
            return Ok(button);
        }
    }
    Err(format!(
        "inspected auxiliary hidden HWNDs [{}]",
        inspected.join(", ")
    ))
}

fn target_act_hidden_desktop_auxiliary_snapshots(
    service: &SynapseService,
    session_id: &str,
    target_hwnd: i64,
) -> Result<Vec<TargetActHiddenWindowSnapshot>, ErrorData> {
    let Some(hidden_desktop) = service.session_hidden_desktop_readback(session_id)? else {
        return Ok(Vec::new());
    };
    let mut snapshots = Vec::new();
    for desktop_name in &hidden_desktop.desktop_names {
        for hwnd in crate::desktop_worker::hidden_desktop_window_hwnds(desktop_name)? {
            if hwnd == target_hwnd {
                continue;
            }
            match crate::desktop_worker::hidden_desktop_window_snapshot(desktop_name, hwnd, 8) {
                Ok(snapshot) => snapshots.push(TargetActHiddenWindowSnapshot {
                    hwnd,
                    nodes: snapshot.tree.nodes,
                }),
                Err(error) if target_act_hidden_worker_target_miss(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(snapshots)
}

fn target_act_invoke_uia_element(
    operation: &'static str,
    element_id: &ElementId,
) -> Result<(), ErrorData> {
    synapse_action::invoke_element(element_id).map_err(|error| {
        mcp_error(
            error.code(),
            format!("target_act {operation} failed for element {element_id}: {error}"),
        )
    })
}

fn target_act_scroll_uia_element_into_view(
    operation: &'static str,
    element_id: &ElementId,
) -> Result<(), ErrorData> {
    synapse_a11y::scroll_element_into_view(element_id).map_err(|error| {
        mcp_error(
            error.code(),
            format!("target_act {operation} failed for element {element_id}: {error}"),
        )
    })
}

fn target_act_notepad_title_matches_document(title: &str, document_name: &str) -> bool {
    let lowered_title = title.to_ascii_lowercase();
    lowered_title.contains("notepad")
        && lowered_title.contains(
            document_name
                .trim()
                .trim_start_matches('*')
                .to_ascii_lowercase()
                .as_str(),
        )
}

fn target_act_notepad_tabs_signature(tabs: &[TargetActNotepadTabReadback]) -> String {
    let payload = tabs
        .iter()
        .map(|tab| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                tab.element_id,
                tab.document_name,
                tab.modified,
                tab.keep,
                tab.bbox.x,
                tab.bbox.y,
                tab.bbox.w,
                tab.bbox.h
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    crate::m2::postcondition::hex_encode(&Sha256::digest(payload.as_bytes()))
}

fn target_act_cleanup_notepad_tabs_postcondition_error(
    detail: &str,
    keep_path: &Path,
    keep_file_name: &str,
    before_signature: &str,
    before: &TargetActNotepadSnapshot,
    after: &TargetActNotepadSnapshot,
    attempts: &[TargetActNotepadTabCloseAttempt],
    verify_timeout_ms: u32,
) -> ErrorData {
    let after_signature = target_act_notepad_tabs_signature(&after.tabs);
    crate::m2::postcondition::postcondition_failed_error(
        "target_act",
        TARGET_ACT_CLEANUP_NOTEPAD_TABS_SOURCE_OF_TRUTH,
        detail,
        before_signature.to_owned(),
        after_signature,
        json!({
            "path": keep_path.display().to_string(),
            "keep_file_name": keep_file_name,
            "before_tabs": before.tabs,
            "after_tabs": after.tabs,
            "attempts": attempts,
            "verify_timeout_ms": verify_timeout_ms,
        }),
    )
}

fn target_act_notepad_tab_names(tabs: &[TargetActNotepadTabReadback]) -> Vec<String> {
    tabs.iter().map(|tab| tab.name.clone()).collect()
}

fn target_act_file_name(path: &Path, verb: &str) -> Result<String, ErrorData> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act verb={verb} path must have a UTF-8 file name: {}",
                    path.display()
                ),
            )
        })
}

fn target_act_names_equal(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn target_act_normalized_role(role: &str) -> String {
    role.chars()
        .filter(|character| !character.is_whitespace() && *character != '_' && *character != '-')
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn target_act_normalized_name(name: &str) -> String {
    name.chars()
        .filter(|character| {
            !character.is_whitespace()
                && *character != '_'
                && *character != '-'
                && *character != '.'
        })
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn target_act_rect_distance_score(rect: Rect, stale: &TargetActNotepadTabReadback) -> i64 {
    let (rect_x, rect_y) = target_act_rect_center(rect);
    let (tab_x, tab_y) = target_act_rect_center(stale.bbox);
    (rect_x - tab_x).abs() + (rect_y - tab_y).abs()
}

fn target_act_rect_has_area(rect: Rect) -> bool {
    rect.w > 0 && rect.h > 0
}

fn target_act_rect_center(rect: Rect) -> (i64, i64) {
    (
        i64::from(rect.x) + i64::from(rect.w) / 2,
        i64::from(rect.y) + i64::from(rect.h) / 2,
    )
}

async fn target_act_save(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let result = target_act_save_impl(service, params, request_context).await;
    let _ = service.audit_action_result_for_request("target_act", &result, request_context);
    match result {
        Ok(response) => Ok((
            "target_window_save",
            true,
            TARGET_ACT_STATUS_OK,
            target_act_result(&response)?,
        )),
        Err(error) => Ok((
            "target_window_save",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_window_save", error),
        )),
    }
}

async fn target_act_save_impl(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<TargetActSaveResponse, ErrorData> {
    let started = Instant::now();
    target_act_validate_save_params(params)?;
    let session_id = target_act_session_id(request_context, "save")?;
    let path = require_param(params.path.clone(), "save", "path")?;
    let expected_text = params.text.as_deref();
    let verify_timeout_ms = target_act_save_verify_timeout(params.wait_timeout_ms)?;
    let target = service.session_target(Some(&session_id))?.ok_or_else(|| {
        mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act verb=save requires this MCP session to have an owned window target; call act_launch/set_target first",
        )
    })?;
    let hwnd = match target {
        SessionTarget::Window { hwnd } => hwnd,
        SessionTarget::Cdp { .. } => {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                "target_act verb=save currently supports native window targets only; browser targets must use browser/CDP storage or download-specific tools",
            ));
        }
    };
    service.ensure_target_claim_allows_session("target_act", &session_id, &target)?;

    let context = synapse_a11y::foreground_context(hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!("target_act verb=save target HWND 0x{hwnd:x} context readback failed: {error}"),
        )
    })?;
    if !context.process_name.eq_ignore_ascii_case("notepad.exe") {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb=save only supports Notepad window targets for #1275; target 0x{hwnd:x} is process {} title {:?}",
                context.process_name, context.window_title
            ),
        ));
    }

    let path = canonical_existing_file_for_target_act_save(Path::new(&path))?;
    if !target_act_notepad_title_matches_path(&context.window_title, &path) {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb=save refused: Notepad target title {:?} does not match file SoT {}",
                context.window_title,
                path.display()
            ),
        ));
    }

    let before = target_act_file_snapshot(&path)?;
    let expected_sha256 = expected_text.map(crate::m2::postcondition::text_signature);
    let request_details = json!({
        "session_id": &session_id,
        "verb": "save",
        "delegated_tool": "target_window_save",
        "target_hwnd": hwnd,
        "target_process_name": context.process_name,
        "target_window_title": context.window_title,
        "path": path.display().to_string(),
        "source_of_truth": TARGET_ACT_SAVE_SOURCE_OF_TRUTH,
        "before_len": before.len,
        "before_sha256": before.sha256,
        "expected_text_sha256": expected_sha256,
        "verify_timeout_ms": verify_timeout_ms,
        "requires_agent_logical_foreground": true,
        "required_foreground": false,
        "no_human_os_foreground_fallback": true,
    });
    service.audit_action_started_with_details_for_request(
        "target_act",
        &request_details,
        request_context,
    )?;

    let mut attempts = Vec::new();
    for source in [
        NotepadSaveCommandSource::Menu,
        NotepadSaveCommandSource::Accelerator,
    ] {
        let attempt = send_notepad_save_command(hwnd, source)?;
        attempts.push(attempt);
        if let Some(after) =
            poll_target_act_save_file(&path, &before, expected_text, verify_timeout_ms).await?
        {
            return Ok(target_act_save_response(
                started,
                &path,
                hwnd,
                &context.process_name,
                &context.window_title,
                before,
                after,
                expected_sha256,
                expected_text,
                attempts,
            ));
        }
    }

    let after = target_act_file_snapshot(&path)?;
    Err(target_act_save_postcondition_error(
        &path,
        &before,
        &after,
        expected_text,
        verify_timeout_ms,
        &attempts,
    ))
}

fn target_act_validate_save_params(params: &TargetActParams) -> Result<(), ErrorData> {
    if params.url.as_ref().is_some_and(|value| !value.is_empty())
        || params
            .command
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        || !params.args.is_empty()
        || params
            .working_dir
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        || target_act_has_any_locator(params)
        || target_act_coordinate(params)?.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=save accepts only path, optional text as expected file contents, and optional wait_timeout_ms",
        ));
    }
    Ok(())
}

fn target_act_save_verify_timeout(value: Option<u64>) -> Result<u32, ErrorData> {
    let timeout = value.unwrap_or(u64::from(DEFAULT_TARGET_ACT_SAVE_TIMEOUT_MS));
    if !(50..=10_000).contains(&timeout) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb=save wait_timeout_ms must be 50..=10000, got {timeout}"),
        ));
    }
    Ok(u32::try_from(timeout).unwrap_or(DEFAULT_TARGET_ACT_SAVE_TIMEOUT_MS))
}

fn canonical_existing_file_for_target_act_save(path: &Path) -> Result<PathBuf, ErrorData> {
    canonical_existing_file_for_target_act_path("save", path)
}

fn canonical_existing_file_for_target_act_path(
    verb: &str,
    path: &Path,
) -> Result<PathBuf, ErrorData> {
    if path.as_os_str().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} path must not be empty"),
        ));
    }
    let metadata = fs::metadata(path).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} requires an existing file path Source of Truth; {} could not be read: {error}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} path must be an existing file, got {}",
                path.display()
            ),
        ));
    }
    path.canonicalize().map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} failed to canonicalize file SoT {}: {error}",
                path.display()
            ),
        )
    })
}

fn target_act_notepad_title_matches_path(title: &str, path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lowered_title = title.to_ascii_lowercase();
    lowered_title.contains("notepad")
        && lowered_title.contains(file_name.to_ascii_lowercase().as_str())
}

fn target_act_file_snapshot(path: &Path) -> Result<TargetActFileSnapshot, ErrorData> {
    let bytes = fs::read(path).map_err(|error| {
        mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "target_act verb=save file SoT read failed for {}: {error}",
                path.display()
            ),
        )
    })?;
    Ok(TargetActFileSnapshot {
        len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        sha256: crate::m2::postcondition::hex_encode(&Sha256::digest(&bytes)),
        bytes,
    })
}

async fn poll_target_act_save_file(
    path: &Path,
    before: &TargetActFileSnapshot,
    expected_text: Option<&str>,
    timeout_ms: u32,
) -> Result<Option<TargetActFileSnapshot>, ErrorData> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
    loop {
        let after = target_act_file_snapshot(path)?;
        if target_act_save_satisfied(before, &after, expected_text) {
            return Ok(Some(after));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(TARGET_ACT_SAVE_POLL_INTERVAL_MS)).await;
    }
}

fn target_act_save_satisfied(
    before: &TargetActFileSnapshot,
    after: &TargetActFileSnapshot,
    expected_text: Option<&str>,
) -> bool {
    if let Some(expected_text) = expected_text {
        return after.bytes == expected_text.as_bytes();
    }
    after.sha256 != before.sha256
}

fn target_act_save_response(
    started: Instant,
    path: &Path,
    hwnd: i64,
    process_name: &str,
    window_title: &str,
    before: TargetActFileSnapshot,
    after: TargetActFileSnapshot,
    expected_sha256: Option<String>,
    expected_text: Option<&str>,
    attempts: Vec<TargetActSaveAttempt>,
) -> TargetActSaveResponse {
    let changed = before.sha256 != after.sha256;
    let expected_text_matched = expected_text.map(|expected| after.bytes == expected.as_bytes());
    let detail = if expected_text_matched == Some(true) {
        "target_act save verified file bytes equal expected text"
    } else if changed {
        "target_act save verified file bytes changed"
    } else {
        "target_act save verified file bytes already matched expected text"
    };
    TargetActSaveResponse {
        ok: true,
        method: attempts
            .last()
            .map(|attempt| attempt.method.clone())
            .unwrap_or_else(|| "notepad_wm_command_save".to_owned()),
        backend_tier_used: "win32_wm_command".to_owned(),
        required_foreground: false,
        source_of_truth: TARGET_ACT_SAVE_SOURCE_OF_TRUTH.to_owned(),
        path: path.display().to_string(),
        target_hwnd: hwnd,
        target_process_name: process_name.to_owned(),
        target_window_title: window_title.to_owned(),
        before_len: before.len,
        after_len: after.len,
        before_sha256: before.sha256.clone(),
        after_sha256: after.sha256.clone(),
        changed,
        expected_text_sha256: expected_sha256,
        expected_text_matched,
        attempts,
        postcondition: crate::m2::postcondition::ActPostcondition {
            status: "verified_state".to_owned(),
            observed_delta: Some(changed),
            source_of_truth: Some(TARGET_ACT_SAVE_SOURCE_OF_TRUTH.to_owned()),
            before_signature: Some(before.sha256),
            after_signature: Some(after.sha256),
            detail: Some(detail.to_owned()),
        },
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    }
}

fn target_act_save_postcondition_error(
    path: &Path,
    before: &TargetActFileSnapshot,
    after: &TargetActFileSnapshot,
    expected_text: Option<&str>,
    verify_timeout_ms: u32,
    attempts: &[TargetActSaveAttempt],
) -> ErrorData {
    let expected_text_sha256 = expected_text.map(crate::m2::postcondition::text_signature);
    let expected_text_matched = expected_text.map(|expected| after.bytes == expected.as_bytes());
    let detail = if expected_text.is_some() {
        "file bytes did not equal expected text after target-scoped Notepad save"
    } else {
        "file bytes did not change after target-scoped Notepad save; pass text to verify an already-matching save"
    };
    crate::m2::postcondition::postcondition_failed_error(
        "target_act",
        TARGET_ACT_SAVE_SOURCE_OF_TRUTH,
        detail,
        before.sha256.clone(),
        after.sha256.clone(),
        json!({
            "path": path.display().to_string(),
            "before_len": before.len,
            "after_len": after.len,
            "expected_text_sha256": expected_text_sha256,
            "expected_text_matched": expected_text_matched,
            "verify_timeout_ms": verify_timeout_ms,
            "attempts": attempts,
        }),
    )
}

#[derive(Copy, Clone)]
enum NotepadSaveCommandSource {
    Menu,
    Accelerator,
}

impl NotepadSaveCommandSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Accelerator => "accelerator",
        }
    }

    const fn wparam(self) -> usize {
        const NOTEPAD_IDM_SAVE: usize = 3;
        match self {
            Self::Menu => NOTEPAD_IDM_SAVE,
            Self::Accelerator => (1_usize << 16) | NOTEPAD_IDM_SAVE,
        }
    }
}

#[cfg(windows)]
fn send_notepad_save_command(
    hwnd: i64,
    source: NotepadSaveCommandSource,
) -> Result<TargetActSaveAttempt, ErrorData> {
    use std::ffi::c_void;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::{
            IsWindow, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SendMessageTimeoutW, WM_COMMAND,
        },
    };

    let hwnd = HWND(hwnd as *mut c_void);
    if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            "target_act verb=save target HWND is not a live window",
        ));
    }
    let mut win32_result = 0_usize;
    let lresult = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COMMAND,
            WPARAM(source.wparam()),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            500,
            Some(&raw mut win32_result),
        )
    };
    let sent = lresult.0 != 0;
    Ok(TargetActSaveAttempt {
        method: format!("notepad_wm_command_save_{}", source.as_str()),
        command_source: source.as_str().to_owned(),
        sent,
        detail: if sent {
            "WM_COMMAND returned before timeout".to_owned()
        } else {
            format!(
                "SendMessageTimeoutW(WM_COMMAND save/{}) failed or timed out: {}",
                source.as_str(),
                windows::core::Error::from_thread()
            )
        },
        win32_result: sent.then_some(win32_result),
    })
}

#[cfg(not(windows))]
fn send_notepad_save_command(
    _hwnd: i64,
    _source: NotepadSaveCommandSource,
) -> Result<TargetActSaveAttempt, ErrorData> {
    Err(mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        "target_act verb=save Notepad WM_COMMAND route is only available on Windows",
    ))
}

fn target_act_session_id(
    request_context: &RequestContext<RoleServer>,
    verb: &str,
) -> Result<String, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} requires an MCP session id"),
        )
    })
}

async fn target_act_browser_dom_primitive(
    service: &SynapseService,
    action: &'static str,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    target_act_validate_dom_primitive_params(action, params)?;
    if let Some(raw_element_id) = params
        .element_id
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        match ElementId::parse(raw_element_id) {
            Ok(element_id) => {
                if synapse_a11y::cdp_backend_from_element_id(&element_id).is_some() {
                    return target_act_cdp_dom_primitive(
                        service,
                        action,
                        element_id,
                        request_context,
                    )
                    .await;
                }
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "target_act verb={action} supports browser DOM/CDP targets only; native/UIA element_id {raw_element_id:?} is not supported by this primitive"
                    ),
                ));
            }
            Err(error) if !target_act_click_element_id_can_be_dom_id(raw_element_id) => {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("target_act verb={action} element_id is invalid: {error}"),
                ));
            }
            Err(_) => {}
        }
    }
    target_act_browser_dom_action(service, action, params, request_context).await
}

fn target_act_validate_dom_primitive_params(
    action: &str,
    params: &TargetActParams,
) -> Result<(), ErrorData> {
    if target_act_coordinate(params)?.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={action} does not accept x/y coordinates"),
        ));
    }
    if target_act_has_key_chord(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} does not accept key(s); use verb=key for raw keyboard chords"
            ),
        ));
    }
    if params.clicks.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={action} does not accept clicks"),
        ));
    }
    if params.text.as_ref().is_some_and(|value| !value.is_empty()) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={action} does not accept text"),
        ));
    }
    if params
        .option
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || params
            .option_label
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params.option_index.is_some()
        || !params.options.is_empty()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={action} does not accept select option fields"),
        ));
    }
    if params
        .event_type
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || params.event_init.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={action} does not accept event_type/event_init"),
        ));
    }
    if params.selection_start.is_some() || params.selection_end.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} selects the whole element and does not accept selection_start/selection_end"
            ),
        ));
    }
    target_act_validate_dom_locator(action, params)
}

#[cfg(windows)]
async fn target_act_cdp_dom_primitive(
    service: &SynapseService,
    action: &'static str,
    element_id: ElementId,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, action)?;
    let backend_node_id =
        synapse_a11y::cdp_backend_from_element_id(&element_id).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("target_act verb={action} element_id is not a CDP-backed web element"),
            )
        })?;
    let element_target_id =
        synapse_a11y::cdp_target_from_element_id(&element_id).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act verb={action} web element_id must include an embedded CDP target id; re-resolve it against the owned tab"
                ),
            )
        })?;
    let element_hwnd = element_id
        .parts()
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("target_act verb={action} element_id is invalid: {error}"),
            )
        })?
        .hwnd;
    let request_details = json!({
        "session_id": &session_id,
        "verb": action,
        "element_id": element_id.to_string(),
        "element_hwnd": element_hwnd,
        "backend_node_id": backend_node_id,
        "element_cdp_target_id": &element_target_id,
        "delegated_tool": "synapse_a11y.cdp_dom_primitive_node",
        "required_foreground": false,
    });
    let Some(target) = service.session_target(Some(&session_id))? else {
        let error = mcp_error(
            error_codes::TARGET_NOT_SET,
            format!(
                "target_act verb={action} requires this MCP session to own a CDP browser target; bind one with cdp_open_tab/set_target first"
            ),
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_dom_primitive_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    let (window_hwnd, cdp_target_id) = match &target {
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => (*window_hwnd, cdp_target_id.clone()),
        SessionTarget::Window { .. } => {
            let error = mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "target_act verb={action} with an observed web element_id requires a session-owned CDP target, not a native/window target"
                ),
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                "synapse_a11y.cdp_dom_primitive_node",
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
    };
    if window_hwnd != element_hwnd {
        let error = mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb={action} element belongs to browser HWND 0x{element_hwnd:x}, but this session target is HWND 0x{window_hwnd:x}"
            ),
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_dom_primitive_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_dom_primitive_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
        let error = mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act verb={action} requires a raw CDP debugging endpoint for window 0x{window_hwnd:x}; re-resolve by selector/role/name to use the normal Chrome bridge path"
            ),
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_dom_primitive_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    let routed_target_id = match target_act_validate_observed_cdp_element_target(
        &endpoint,
        window_hwnd,
        &cdp_target_id,
        &element_target_id,
        action,
    )
    .await
    {
        Ok(()) => element_target_id.clone(),
        Err(error) => {
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                "synapse_a11y.cdp_dom_primitive_node",
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
    };
    let request_details = json!({
        "session_id": &session_id,
        "verb": action,
        "element_id": element_id.to_string(),
        "element_hwnd": element_hwnd,
        "backend_node_id": backend_node_id,
        "session_cdp_target_id": &cdp_target_id,
        "element_cdp_target_id": &element_target_id,
        "dispatch_cdp_target_id": &routed_target_id,
        "delegated_tool": "synapse_a11y.cdp_dom_primitive_node",
        "required_foreground": false,
    });
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let result =
        synapse_a11y::cdp_dom_primitive_node(&endpoint, &routed_target_id, backend_node_id, action)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("target_act CDP {action} failed: {error}"),
                )
            });
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(readback) => {
            let mut value = target_act_result(&readback)?;
            if let Some(object) = value.as_object_mut() {
                object.insert("element_id".to_owned(), json!(element_id.to_string()));
                object.insert("backend_node_id".to_owned(), json!(backend_node_id));
                object.insert("target_id".to_owned(), json!(routed_target_id));
                object.insert("session_target_id".to_owned(), json!(cdp_target_id));
                object.insert("window_hwnd".to_owned(), json!(window_hwnd));
            }
            Ok((
                "synapse_a11y.cdp_dom_primitive_node",
                true,
                TARGET_ACT_STATUS_OK,
                value,
            ))
        }
        Err(error) => Ok((
            "synapse_a11y.cdp_dom_primitive_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("synapse_a11y.cdp_dom_primitive_node", error),
        )),
    }
}

#[cfg(not(windows))]
async fn target_act_cdp_dom_primitive(
    _service: &SynapseService,
    action: &'static str,
    _element_id: ElementId,
    _request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let error = mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "target_act verb={action} observed CDP element_id primitives require Windows CDP action support"
        ),
    );
    Ok((
        "synapse_a11y.cdp_dom_primitive_node",
        false,
        target_act_error_status(&error),
        target_act_error_result("synapse_a11y.cdp_dom_primitive_node", error),
    ))
}

async fn target_act_browser_dom_action(
    service: &SynapseService,
    action: &str,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, action)?;
    if target_act_coordinate(params)?.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} does not accept x/y; use verb=click or verb=type for coordinate fallback"
            ),
        ));
    }
    target_act_validate_dom_locator(action, params)?;
    crate::m2::validate_auto_wait_timeout(
        "target_act",
        params.auto_wait,
        params.auto_wait_timeout_ms,
    )?;
    let wait_timeout_ms = target_act_dom_wait_timeout(params.wait_timeout_ms)?;
    let click_count = target_act_dom_click_count(action, params.clicks)?;
    let click_position = target_act_click_position(params)?;
    let click_modifiers = target_act_click_modifiers_bridge_value(&params.modifiers)?;
    let request_details = json!({
        "session_id": &session_id,
        "verb": action,
        "selector_present": params.selector.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "element_id_present": params.element_id.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "role": params.role.as_deref(),
        "name_present": params.name.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "value_present": params.value.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "option_present": params.option.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "option_label_present": params.option_label.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "option_index": params.option_index,
        "options_count": params.options.len(),
        "event_type": params.event_type.as_deref(),
        "event_init_present": params.event_init.is_some(),
        "button": params.button.map(TargetActMouseButton::as_str),
        "modifiers": &click_modifiers,
        "position": click_position.map(|(x, y)| json!({ "x": x, "y": y })),
        "clicks": click_count,
        "wait_timeout_ms": wait_timeout_ms,
        "auto_wait": params.auto_wait,
        "auto_wait_timeout_ms": params.auto_wait_timeout_ms,
        "required_foreground": false,
    });
    let (window_hwnd, cdp_target_id) = match service.audit_cdp_target_resolution_result(
        "target_act",
        &session_id,
        &request_details,
        service.resolve_cdp_tab_mutation_target("target_act", &session_id, None, None),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            return Ok((
                "chrome_debugger_bridge.domAction",
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
    };
    let target = SessionTarget::Cdp {
        window_hwnd,
        cdp_target_id: cdp_target_id.clone(),
    };
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "chrome_debugger_bridge.domAction",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    let request_details = json!({
        "session_id": &session_id,
        "verb": action,
        "window_hwnd": window_hwnd,
        "cdp_target_id": &cdp_target_id,
        "selector_present": params.selector.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "element_id_present": params.element_id.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "role": params.role.as_deref(),
        "name_present": params.name.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "value_present": params.value.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "option_present": params.option.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "option_label_present": params.option_label.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "option_index": params.option_index,
        "options_count": params.options.len(),
        "event_type": params.event_type.as_deref(),
        "event_init_present": params.event_init.is_some(),
        "button": params.button.map(TargetActMouseButton::as_str),
        "modifiers": &click_modifiers,
        "position": click_position.map(|(x, y)| json!({ "x": x, "y": y })),
        "clicks": click_count,
        "wait_timeout_ms": wait_timeout_ms,
        "auto_wait": params.auto_wait,
        "auto_wait_timeout_ms": params.auto_wait_timeout_ms,
        "required_foreground": false,
    });
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let options_value = (!params.options.is_empty())
        .then(|| serde_json::to_value(&params.options))
        .transpose()
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("target_act select options encode failed: {error}"),
            )
        })?;
    let mut result = crate::chrome_debugger_bridge::dom_action(
        crate::chrome_debugger_bridge::ChromeDebuggerDomActionRequest {
            hwnd: window_hwnd,
            target_id: &cdp_target_id,
            action,
            selector: params.selector.as_deref(),
            element_id: params.element_id.as_deref(),
            role: params.role.as_deref(),
            name: params.name.as_deref(),
            value: params.value.as_deref(),
            option: params.option.as_deref(),
            option_label: params.option_label.as_deref(),
            option_index: params.option_index,
            options: options_value.as_ref(),
            event_type: params.event_type.as_deref(),
            event_init: params.event_init.as_ref(),
            clicks: click_count,
            button: params.button.map(TargetActMouseButton::as_str),
            modifiers: Some(&click_modifiers),
            position_x: click_position.map(|(x, _)| x),
            position_y: click_position.map(|(_, y)| y),
            wait_timeout_ms,
            auto_wait: params.auto_wait,
            auto_wait_timeout_ms: params.auto_wait_timeout_ms,
        },
    )
    .await
    .map_err(|error| mcp_error(error.code(), error.detail().to_owned()));
    if let Ok(value) = result.as_mut() {
        if let Err(error) =
            target_act_register_created_popup_tabs(service, &session_id, window_hwnd, value)
        {
            result = Err(error);
        }
    }
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(value) => Ok((
            "chrome_debugger_bridge.domAction",
            true,
            TARGET_ACT_STATUS_OK,
            value,
        )),
        Err(error) => Ok((
            "chrome_debugger_bridge.domAction",
            false,
            target_act_error_status(&error),
            target_act_error_result("chrome_debugger_bridge.domAction", error),
        )),
    }
}

fn target_act_register_created_popup_tabs(
    service: &SynapseService,
    session_id: &str,
    window_hwnd: i64,
    action_value: &mut Value,
) -> Result<(), ErrorData> {
    let tabs = action_value
        .get("created_popup_tabs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tabs.is_empty() {
        return Ok(());
    }

    let endpoint = action_value
        .get("extension_id")
        .and_then(Value::as_str)
        .filter(|extension_id| !extension_id.trim().is_empty())
        .map(chrome_debugger_endpoint)
        .unwrap_or_else(chrome_debugger_default_endpoint);
    let mut registered = Vec::with_capacity(tabs.len());
    for (index, tab) in tabs.iter().enumerate() {
        let target_id = tab
            .get("target_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "target_act DOM action created popup tab at index {index}, but the bridge did not return target_id"
                    ),
                )
            })?;
        validate_cdp_target_id(target_id)?;
        let target_url = tab
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let chrome_window_id = tab.get("chrome_window_id").and_then(Value::as_i64);
        let owner_key = service.register_cdp_target_owner(CdpTargetOwner {
            session_id: session_id.to_owned(),
            window_hwnd,
            endpoint: endpoint.clone(),
            chrome_window_id,
            capture_window_hwnd: None,
            cdp_target_id: target_id.to_owned(),
            requested_url: target_url.clone(),
            target_url,
            created_at_unix_ms: target_act_unix_ms_now(),
        })?;
        tracing::info!(
            code = "TARGET_ACT_POPUP_TAB_OWNER_REGISTERED",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %target_id,
            owner_key = %owner_key,
            chrome_window_id = chrome_window_id.unwrap_or_default(),
            "target_act registered bridge-created popup tab owner"
        );
        registered.push(json!({
            "target_id": target_id,
            "owner_key": owner_key,
            "endpoint": endpoint,
            "window_hwnd": window_hwnd,
            "chrome_window_id": chrome_window_id,
        }));
    }
    if let Some(object) = action_value.as_object_mut() {
        object.insert("created_popup_tab_owners".to_owned(), json!(registered));
    }
    Ok(())
}

fn target_act_unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn target_act_coordinate_click(
    service: &SynapseService,
    action: &str,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, "coordinate_click")?;
    let coordinate = target_act_coordinate(params)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act coordinate click requires both x and y",
        )
    })?;
    if target_act_click_position(params)?.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} position is for element-relative browser DOM clicks; coordinate clicks already use x/y"
            ),
        ));
    }
    let clicks = target_act_click_count_for_action(action, params.clicks)?;
    let wait_timeout_ms = target_act_dom_wait_timeout(params.wait_timeout_ms)?;
    let click_modifiers = target_act_click_modifiers_bridge_value(&params.modifiers)?;
    let request_details = json!({
        "session_id": &session_id,
        "verb": "coordinate_click",
        "action": action,
        "x": coordinate.x,
        "y": coordinate.y,
        "coordinate_space": coordinate.space.as_bridge_str(),
        "clicks": clicks,
        "button": params.button.map(TargetActMouseButton::as_str),
        "modifiers": &click_modifiers,
        "wait_timeout_ms": wait_timeout_ms,
        "requires_session_target": true,
        "no_human_os_foreground_fallback": true,
    });
    let Some(target) = service.session_target(Some(&session_id))? else {
        let error = mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act coordinate click requires this MCP session to own an agent_logical_foreground/foreground_lane target; bind a target with set_target or cdp_open_tab first",
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "target_act",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "target_act",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    match target {
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => {
            let request_details = json!({
                "session_id": &session_id,
                "verb": "coordinate_click",
                "delegated_tool": "chrome_debugger_bridge.coordinateClick",
                "window_hwnd": window_hwnd,
                "cdp_target_id": &cdp_target_id,
                "x": coordinate.x,
                "y": coordinate.y,
                "coordinate_space": coordinate.space.as_bridge_str(),
                "clicks": clicks,
                "button": params.button.map(TargetActMouseButton::as_str),
                "modifiers": &click_modifiers,
                "wait_timeout_ms": wait_timeout_ms,
                "required_foreground": false,
            });
            service.audit_action_started_with_details_for_session(
                "target_act",
                &request_details,
                &session_id,
            )?;
            let result = crate::chrome_debugger_bridge::coordinate_click(
                crate::chrome_debugger_bridge::ChromeDebuggerCoordinateClickRequest {
                    hwnd: window_hwnd,
                    target_id: &cdp_target_id,
                    x: coordinate.x,
                    y: coordinate.y,
                    coordinate_space: coordinate.space.as_bridge_str(),
                    clicks,
                    button: params.button.map(TargetActMouseButton::as_str),
                    modifiers: Some(&click_modifiers),
                    wait_timeout_ms,
                },
            )
            .await
            .map_err(|error| mcp_error(error.code(), error.detail().to_owned()));
            service.audit_action_result_for_session("target_act", &result, &session_id)?;
            match result {
                Ok(value) => Ok((
                    "chrome_debugger_bridge.coordinateClick",
                    true,
                    TARGET_ACT_STATUS_OK,
                    value,
                )),
                Err(error) => Ok((
                    "chrome_debugger_bridge.coordinateClick",
                    false,
                    target_act_error_status(&error),
                    target_act_error_result("chrome_debugger_bridge.coordinateClick", error),
                )),
            }
        }
        SessionTarget::Window { hwnd } => {
            let point = match target_act_window_coordinate_to_screen_point(hwnd, coordinate) {
                Ok(point) => point,
                Err(error) => {
                    service.audit_action_denied_with_details_for_session(
                        "target_act",
                        &error,
                        &request_details,
                        &session_id,
                    );
                    return Ok((
                        "act_click",
                        false,
                        target_act_error_status(&error),
                        target_act_error_result("act_click", error),
                    ));
                }
            };
            if let Err(error) = target_act_window_coordinate_foreground_preflight(hwnd, &session_id)
            {
                service.audit_action_denied_with_details_for_session(
                    "target_act",
                    &error,
                    &request_details,
                    &session_id,
                );
                return Ok((
                    "act_click",
                    false,
                    target_act_error_status(&error),
                    target_act_error_result("act_click", error),
                ));
            }
            let click_params =
                target_act_click_point_params(point, clicks, params.button, &params.modifiers)?;
            let response = service
                .act_click(Parameters(click_params), request_context.clone())
                .await;
            target_act_delegate_response("act_click", response)
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct TargetActTapElement {
    backend_node_id: i64,
    target_id: String,
    resolved_by: String,
    match_count: Option<usize>,
    returned_count: Option<usize>,
    element_id: Option<String>,
}

#[cfg(windows)]
async fn target_act_touch_tap(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, "tap")?;
    crate::m2::validate_auto_wait_timeout(
        "target_act",
        params.auto_wait,
        params.auto_wait_timeout_ms,
    )?;
    if params.clicks.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=tap does not accept clicks; a touch tap is always one touchStart/touchEnd sequence",
        ));
    }
    let coordinate = target_act_coordinate(params)?;
    if coordinate.is_some() && target_act_has_any_locator(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=tap accepts either viewport x/y coordinates or an element/DOM locator, not both",
        ));
    }
    let request_details = json!({
        "session_id": &session_id,
        "verb": "tap",
        "coordinate_present": coordinate.is_some(),
        "selector_present": params.selector.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "element_id_present": params.element_id.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "role": params.role.as_deref(),
        "name_present": params.name.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "requires_cdp_input": true,
        "delegated_tool": "synapse_a11y.cdp_touch_tap_or_chrome_debugger_bridge.cdpInput",
        "method": "Input.dispatchTouchEvent",
        "non_touch_fallback": "none; use verb=click explicitly for mouse behavior",
        "required_foreground": false,
    });
    let Some(target) = service.session_target(Some(&session_id))? else {
        let error = mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act verb=tap requires this MCP session to own a raw-CDP browser target; bind one with cdp_open_tab/set_target first",
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_touch_tap",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_touch_tap",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    let SessionTarget::Cdp {
        window_hwnd,
        cdp_target_id,
    } = target
    else {
        let error = mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            "target_act verb=tap supports raw-CDP browser targets only; native/window targets have no touch semantics, and the explicit non-touch fallback is verb=click",
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_touch_tap",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
        return target_act_bridge_cdp_input(
            service,
            "tap",
            window_hwnd,
            &cdp_target_id,
            coordinate,
            params,
            &request_details,
            &session_id,
        )
        .await;
    };

    let mut request_details = request_details;
    if let Some(object) = request_details.as_object_mut() {
        object.insert("window_hwnd".to_owned(), json!(window_hwnd));
        object.insert("cdp_target_id".to_owned(), json!(&cdp_target_id));
    }
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let result =
        target_act_touch_tap_dispatch(&endpoint, window_hwnd, &cdp_target_id, coordinate, params)
            .await;
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(result) => Ok((
            "synapse_a11y.cdp_touch_tap",
            true,
            TARGET_ACT_STATUS_OK,
            result,
        )),
        Err(error) => Ok((
            "synapse_a11y.cdp_touch_tap",
            false,
            target_act_error_status(&error),
            target_act_error_result("synapse_a11y.cdp_touch_tap", error),
        )),
    }
}

#[cfg(not(windows))]
async fn target_act_touch_tap(
    _service: &SynapseService,
    _params: &TargetActParams,
    _request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let error = mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        "target_act verb=tap requires Windows raw-CDP action support for Input.dispatchTouchEvent; use verb=click explicitly for non-touch mouse behavior",
    );
    Ok((
        "synapse_a11y.cdp_touch_tap",
        false,
        target_act_error_status(&error),
        target_act_error_result("target_act", error),
    ))
}

/// #1349: background-safe target-bound window move/resize. Resolves the bound
/// window HWND (native Window target, or the browser window behind a Cdp target),
/// drives SetWindowPos without activation, and reads the resulting outer rect
/// back so the caller gets requested-vs-actual bounds (min-size constraints) and
/// minimized state. No human-foreground fallback, no implicit target.
#[cfg(windows)]
fn target_act_set_window_bounds(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    const DELEGATE: &str = "synapse_a11y.set_window_bounds";
    let session_id = target_act_session_id(request_context, "set_window_bounds")?;
    if params.selector.is_some()
        || params.element_id.is_some()
        || params.role.is_some()
        || params.name.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_window_bounds operates on the bound window target; it does not accept selector/element_id/role/name",
        ));
    }
    if params.width.is_none() && params.height.is_none() && params.x.is_none() && params.y.is_none()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_window_bounds requires at least one of x/y (move) or width/height (resize)",
        ));
    }
    if params.width.is_some_and(|value| value <= 0) || params.height.is_some_and(|value| value <= 0)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_window_bounds width/height must be > 0 when supplied",
        ));
    }
    let request_details = json!({
        "session_id": &session_id,
        "verb": "set_window_bounds",
        "requested_x": params.x,
        "requested_y": params.y,
        "requested_width": params.width,
        "requested_height": params.height,
        "delegated_tool": DELEGATE,
        "method": "SetWindowPos(SWP_NOZORDER|SWP_NOACTIVATE)+GetWindowRect",
        "required_foreground": false,
    });
    let Some(target) = service.session_target(Some(&session_id))? else {
        let error = mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act verb=set_window_bounds requires a bound window target; bind one with window_list/set_target first",
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            DELEGATE,
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            DELEGATE,
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    let window_hwnd = match &target {
        SessionTarget::Window { hwnd } => *hwnd,
        SessionTarget::Cdp { window_hwnd, .. } => *window_hwnd,
    };
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let result = synapse_a11y::set_window_bounds(
        window_hwnd,
        params.x,
        params.y,
        params.width,
        params.height,
    )
    .map(|outcome| {
        let requested_w = params.width.unwrap_or(outcome.actual.w);
        let requested_h = params.height.unwrap_or(outcome.actual.h);
        let size_satisfied = outcome.actual.w == requested_w && outcome.actual.h == requested_h;
        json!({
            "target_hwnd": window_hwnd,
            "requested": {
                "x": params.x, "y": params.y,
                "width": params.width, "height": params.height,
            },
            "actual": {
                "x": outcome.actual.x, "y": outcome.actual.y,
                "width": outcome.actual.w, "height": outcome.actual.h,
            },
            "minimized": outcome.minimized,
            "size_satisfied": size_satisfied,
            "source_of_truth": "GetWindowRect readback after SetWindowPos",
        })
    })
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!("target_act set_window_bounds failed for HWND {window_hwnd:#x}: {error}"),
        )
    });
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(value) => Ok((DELEGATE, true, TARGET_ACT_STATUS_OK, value)),
        Err(error) => Ok((
            DELEGATE,
            false,
            target_act_error_status(&error),
            target_act_error_result(DELEGATE, error),
        )),
    }
}

#[cfg(not(windows))]
fn target_act_set_window_bounds(
    _service: &SynapseService,
    _params: &TargetActParams,
    _request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    Err(mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        "target_act verb=set_window_bounds requires Windows SetWindowPos support",
    ))
}

#[cfg(windows)]
async fn target_act_hover(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, "hover")?;
    crate::m2::validate_auto_wait_timeout(
        "target_act",
        params.auto_wait,
        params.auto_wait_timeout_ms,
    )?;
    if target_act_coordinate(params)?.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=hover targets an element center; use selector, role/name, or observed CDP element_id",
        ));
    }
    if params.clicks.is_some()
        || params.button.is_some()
        || !params.modifiers.is_empty()
        || target_act_has_click_position_input(params)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=hover does not accept click-only options clicks/clickCount, button, modifiers, or position",
        ));
    }
    let request_details = json!({
        "session_id": &session_id,
        "verb": "hover",
        "selector_present": params.selector.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "element_id_present": params.element_id.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "role": params.role.as_deref(),
        "name_present": params.name.as_ref().is_some_and(|value| !value.trim().is_empty()),
        "requires_cdp_input": true,
        "delegated_tool": "synapse_a11y.cdp_aim_node_or_chrome_debugger_bridge.cdpInput",
        "method": "Input.dispatchMouseEvent(mouseMoved)",
        "required_foreground": false,
    });
    let Some(target) = service.session_target(Some(&session_id))? else {
        let error = mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act verb=hover requires this MCP session to own a raw-CDP browser target; bind one with cdp_open_tab/set_target first",
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_aim_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_aim_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    let SessionTarget::Cdp {
        window_hwnd,
        cdp_target_id,
    } = target
    else {
        let error = mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            "target_act verb=hover supports raw-CDP browser targets only; native/window targets cannot set browser :hover state without foreground mouse input",
        );
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.cdp_aim_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    };
    let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
        return target_act_bridge_cdp_input(
            service,
            "hover",
            window_hwnd,
            &cdp_target_id,
            None,
            params,
            &request_details,
            &session_id,
        )
        .await;
    };

    let mut request_details = request_details;
    if let Some(object) = request_details.as_object_mut() {
        object.insert("window_hwnd".to_owned(), json!(window_hwnd));
        object.insert("cdp_target_id".to_owned(), json!(&cdp_target_id));
    }
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let result = target_act_hover_dispatch(&endpoint, window_hwnd, &cdp_target_id, params).await;
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(result) => Ok((
            "synapse_a11y.cdp_aim_node",
            true,
            TARGET_ACT_STATUS_OK,
            result,
        )),
        Err(error) => Ok((
            "synapse_a11y.cdp_aim_node",
            false,
            target_act_error_status(&error),
            target_act_error_result("synapse_a11y.cdp_aim_node", error),
        )),
    }
}

#[cfg(not(windows))]
async fn target_act_hover(
    _service: &SynapseService,
    _params: &TargetActParams,
    _request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let error = mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        "target_act verb=hover requires Windows raw-CDP action support for Input.dispatchMouseEvent(mouseMoved)",
    );
    Ok((
        "synapse_a11y.cdp_aim_node",
        false,
        target_act_error_status(&error),
        target_act_error_result("target_act", error),
    ))
}

/// Route a DOM-locator click/dblclick/press to the REAL trusted-input lane
/// (#1348 headline #1/#2). For the normal Chrome bridge target this dispatches
/// chrome.debugger `Input.dispatchMouseEvent` (isTrusted=true) instead of the
/// synthetic `performClick` that guarded handlers ignore. Raw-CDP and
/// native/window targets keep the existing path (raw-CDP real mouse click is a
/// tracked follow-up). `bridge_action` is the cdpInput action ("click"/
/// "dblclick"); `fallback_action` is the DOM-action verb used when the real lane
/// does not apply (so verb=press keeps its named-button fallback semantics).
#[cfg(windows)]
async fn target_act_dom_locator_pointer(
    service: &SynapseService,
    bridge_action: &'static str,
    fallback_action: &'static str,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, bridge_action)?;
    let Some(target) = service.session_target(Some(&session_id))? else {
        return target_act_browser_dom_action(service, fallback_action, params, request_context)
            .await;
    };
    let SessionTarget::Cdp {
        window_hwnd,
        cdp_target_id,
    } = &target
    else {
        return target_act_browser_dom_action(service, fallback_action, params, request_context)
            .await;
    };
    // Raw-CDP endpoint present → real trusted mouse click via
    // synapse_a11y::cdp_click_node (Input.dispatchMouseEvent mouseMoved →
    // mousePressed → mouseReleased), NOT the bridge synthetic path (which rejects
    // a non-bridge raw-CDP target outright). #1348: closes the last
    // synthetic/broken input lane — raw-CDP locator clicks are now real and
    // trusted, mirroring the verb=tap raw-CDP path. Bridge-only targets fall
    // through to the cdpInput lane below.
    #[cfg(windows)]
    if let Some(endpoint) = synapse_a11y::endpoint_for_window(*window_hwnd) {
        let raw_details = json!({
            "session_id": &session_id,
            "verb": bridge_action,
            "lane": "synapse_a11y.cdp_click_node",
            "real_trusted_input": true,
            "required_foreground": false,
            "window_hwnd": *window_hwnd,
            "cdp_target_id": cdp_target_id,
        });
        if let Err(error) =
            service.ensure_target_claim_allows_session("target_act", &session_id, &target)
        {
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &raw_details,
                &session_id,
            );
            return Ok((
                "synapse_a11y.cdp_click_node",
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
        service.audit_action_started_with_details_for_session(
            "target_act",
            &raw_details,
            &session_id,
        )?;
        let result = target_act_raw_cdp_click_dispatch(
            &endpoint,
            *window_hwnd,
            cdp_target_id,
            bridge_action,
            params,
        )
        .await;
        service.audit_action_result_for_session("target_act", &result, &session_id)?;
        return match result {
            Ok(result) => Ok((
                "synapse_a11y.cdp_click_node",
                true,
                TARGET_ACT_STATUS_OK,
                result,
            )),
            Err(error) => Ok((
                "synapse_a11y.cdp_click_node",
                false,
                target_act_error_status(&error),
                target_act_error_result("synapse_a11y.cdp_click_node", error),
            )),
        };
    }
    let request_details = json!({
        "session_id": &session_id,
        "verb": bridge_action,
        "fallback_verb": fallback_action,
        "lane": "chrome_debugger_bridge.cdpInput",
        "real_trusted_input": true,
        "required_foreground": false,
    });
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "chrome_debugger_bridge.cdpInput",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    target_act_bridge_cdp_input(
        service,
        bridge_action,
        *window_hwnd,
        cdp_target_id,
        None,
        params,
        &request_details,
        &session_id,
    )
    .await
}

#[cfg(not(windows))]
async fn target_act_dom_locator_pointer(
    service: &SynapseService,
    _bridge_action: &'static str,
    fallback_action: &'static str,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    target_act_browser_dom_action(service, fallback_action, params, request_context).await
}

#[cfg(windows)]
async fn target_act_bridge_cdp_input(
    service: &SynapseService,
    action: &'static str,
    window_hwnd: i64,
    cdp_target_id: &str,
    coordinate: Option<TargetActCoordinate>,
    params: &TargetActParams,
    request_details: &Value,
    session_id: &str,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    target_act_validate_bridge_cdp_input(action, coordinate, params)?;
    // Mouse-click options apply only to the real-mouse click/dblclick lane;
    // hover/tap/drag ignore them (left None).
    let is_click = matches!(action, "click" | "dblclick");
    let click_count = if is_click {
        Some(target_act_click_count_for_action(action, params.clicks)?)
    } else {
        None
    };
    let click_button = if is_click {
        params.button.map(TargetActMouseButton::as_str)
    } else {
        None
    };
    let click_modifiers = if is_click {
        Some(target_act_click_modifiers_bridge_value(&params.modifiers)?)
    } else {
        None
    };
    let mut request_details = request_details.clone();
    if let Some(object) = request_details.as_object_mut() {
        object.insert("window_hwnd".to_owned(), json!(window_hwnd));
        object.insert("cdp_target_id".to_owned(), json!(cdp_target_id));
        object.insert(
            "delegated_tool".to_owned(),
            json!("chrome_debugger_bridge.cdpInput"),
        );
        object.insert("bridge_debugger_lane".to_owned(), json!("chrome.debugger"));
        object.insert("required_foreground".to_owned(), json!(false));
    }
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        session_id,
    )?;
    let coordinate_space = coordinate.map(|value| value.space.as_bridge_str());
    let result = crate::chrome_debugger_bridge::cdp_input(
        crate::chrome_debugger_bridge::ChromeDebuggerCdpInputRequest {
            hwnd: window_hwnd,
            target_id: cdp_target_id,
            action,
            selector: params.selector.as_deref(),
            element_id: params.element_id.as_deref(),
            active_element: false,
            role: params.role.as_deref(),
            name: params.name.as_deref(),
            value: params.value.as_deref(),
            text: params.text.as_deref(),
            x: coordinate.map(|value| value.x),
            y: coordinate.map(|value| value.y),
            coordinate_space,
            source_selector: None,
            target_selector: None,
            drag_steps: None,
            drag_duration_ms: None,
            drag_data_mime_type: None,
            drag_data_text: None,
            button: click_button,
            modifiers: click_modifiers.as_ref(),
            clicks: click_count,
            wait_timeout_ms: target_act_dom_wait_timeout(params.wait_timeout_ms)?,
            auto_wait: params.auto_wait,
            auto_wait_timeout_ms: params.auto_wait_timeout_ms,
        },
    )
    .await
    .map_err(|error| mcp_error(error.code(), error.detail().to_owned()));
    service.audit_action_result_for_session("target_act", &result, session_id)?;
    match result {
        Ok(value) => Ok((
            "chrome_debugger_bridge.cdpInput",
            true,
            TARGET_ACT_STATUS_OK,
            value,
        )),
        Err(error) => Ok((
            "chrome_debugger_bridge.cdpInput",
            false,
            target_act_error_status(&error),
            target_act_error_result("chrome_debugger_bridge.cdpInput", error),
        )),
    }
}

#[cfg(windows)]
fn target_act_validate_bridge_cdp_input(
    action: &str,
    coordinate: Option<TargetActCoordinate>,
    params: &TargetActParams,
) -> Result<(), ErrorData> {
    if let Some(coordinate) = coordinate
        && coordinate.space != TargetActCoordinateSpace::Viewport
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} coordinate input must use coordinate_space=viewport because CDP Input consumes viewport CSS pixels; got {}",
                coordinate.space.as_bridge_str()
            ),
        ));
    }
    if coordinate.is_none() {
        target_act_validate_dom_locator(action, params)?;
    }
    if let Some(raw) = params
        .element_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if ElementId::parse(raw).is_ok() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "target_act verb={action} observed raw-CDP element_id {raw:?} requires a raw CDP endpoint; re-resolve the element through browser_locate for a chrome-tab:... bridge element id or use selector/role/name"
                ),
            ));
        }
        if !target_act_click_element_id_can_be_dom_id(raw) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act verb={action} element_id must be a chrome-tab:... bridge element id or plain DOM id for the normal Chrome bridge cdpInput lane"
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn target_act_hover_dispatch(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    params: &TargetActParams,
) -> Result<Value, ErrorData> {
    let element =
        target_act_tap_element(endpoint, window_hwnd, cdp_target_id, params, "hover").await?;
    let point = synapse_a11y::cdp_aim_node(
        endpoint,
        "",
        Some(&element.target_id),
        element.backend_node_id,
    )
    .await
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act hover Input.dispatchMouseEvent(mouseMoved) failed for backendNodeId {} in target {:?}: {error}",
                element.backend_node_id, element.target_id
            ),
        )
    })?;
    Ok(json!({
        "ok": true,
        "target_id": element.target_id,
        "point": point,
        "resolved_by": element.resolved_by,
        "backend_node_id": element.backend_node_id,
        "element_id": element.element_id,
        "match_count": element.match_count,
        "returned_count": element.returned_count,
        "window_hwnd": window_hwnd,
        "readback_backend": "raw_cdp",
        "method": "Input.dispatchMouseEvent(mouseMoved)",
        "scrolled_into_view_before_move": true
    }))
}

/// CDP `Input.dispatchMouseEvent` modifier bitmask: Alt=1, Ctrl=2, Meta=4,
/// Shift=8 (#1348 raw-CDP click).
fn target_act_click_modifiers_cdp_mask(modifiers: &[TargetActClickModifier]) -> i64 {
    let mut mask = 0;
    for modifier in modifiers {
        mask |= match modifier {
            TargetActClickModifier::Alt => 1,
            TargetActClickModifier::Ctrl => 2,
            TargetActClickModifier::Meta => 4,
            TargetActClickModifier::Shift => 8,
        };
    }
    mask
}

/// Real, TRUSTED raw-CDP mouse click/dblclick on a DOM-locator/element_id target
/// via `synapse_a11y::cdp_click_node` (Input.dispatchMouseEvent
/// mouseMoved→mousePressed→mouseReleased), mirroring `target_act_touch_tap_dispatch`
/// but with mouse instead of touch. isTrusted=true, so activation-guarded handlers
/// fire — the raw-CDP analogue of the bridge cdpInput real-click lane (#1348).
#[cfg(windows)]
async fn target_act_raw_cdp_click_dispatch(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    action: &'static str,
    params: &TargetActParams,
) -> Result<Value, ErrorData> {
    let element =
        target_act_tap_element(endpoint, window_hwnd, cdp_target_id, params, action).await?;
    let click_count = i64::from(target_act_click_count_for_action(action, params.clicks)?);
    let button = match params.button {
        Some(TargetActMouseButton::Right) => synapse_a11y::CdpMouseButton::Right,
        Some(TargetActMouseButton::Middle) => synapse_a11y::CdpMouseButton::Middle,
        Some(TargetActMouseButton::Left) | None => synapse_a11y::CdpMouseButton::Left,
    };
    let modifiers = target_act_click_modifiers_cdp_mask(&params.modifiers);
    let point = synapse_a11y::cdp_click_node(
        endpoint,
        "",
        Some(&element.target_id),
        element.backend_node_id,
        button,
        click_count,
        modifiers,
    )
    .await
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act {action} Input.dispatchMouseEvent failed for backendNodeId {} in target {:?}: {error}",
                element.backend_node_id, element.target_id
            ),
        )
    })?;
    Ok(json!({
        "ok": true,
        "action": action,
        "target_id": element.target_id,
        "point": point,
        "resolved_by": element.resolved_by,
        "backend_node_id": element.backend_node_id,
        "element_id": element.element_id,
        "match_count": element.match_count,
        "returned_count": element.returned_count,
        "window_hwnd": window_hwnd,
        "readback_backend": "raw_cdp",
        "method": "Input.dispatchMouseEvent(mouseMoved,mousePressed,mouseReleased)",
        "click_count": click_count,
        "real_trusted_input": true,
        "scrolled_into_view_before_click": true
    }))
}

#[cfg(windows)]
async fn target_act_touch_tap_dispatch(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    coordinate: Option<TargetActCoordinate>,
    params: &TargetActParams,
) -> Result<Value, ErrorData> {
    if let Some(coordinate) = coordinate {
        if coordinate.space != TargetActCoordinateSpace::Viewport {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act verb=tap coordinate input must use coordinate_space=viewport because Input.dispatchTouchEvent consumes viewport CSS pixels; got {}",
                    coordinate.space.as_bridge_str()
                ),
            ));
        }
        let point = synapse_a11y::CdpActionPoint {
            x: f64::from(coordinate.x),
            y: f64::from(coordinate.y),
        };
        let tap = synapse_a11y::cdp_touch_tap_target(endpoint, cdp_target_id, point)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "target_act tap Input.dispatchTouchEvent failed for target {cdp_target_id:?}: {error}"
                    ),
                )
            })?;
        let mut result = target_act_result(&tap)?;
        if let Some(object) = result.as_object_mut() {
            object.insert("ok".to_owned(), json!(true));
            object.insert("resolved_by".to_owned(), json!("viewport_coordinate"));
            object.insert("backend_node_id".to_owned(), Value::Null);
            object.insert("window_hwnd".to_owned(), json!(window_hwnd));
            object.insert("readback_backend".to_owned(), json!("raw_cdp"));
            object.insert("method".to_owned(), json!("Input.dispatchTouchEvent"));
        }
        return Ok(result);
    }

    let element =
        target_act_tap_element(endpoint, window_hwnd, cdp_target_id, params, "tap").await?;
    let tap = synapse_a11y::cdp_touch_tap_node(
        endpoint,
        "",
        Some(&element.target_id),
        element.backend_node_id,
    )
    .await
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act tap Input.dispatchTouchEvent failed for backendNodeId {} in target {:?}: {error}",
                element.backend_node_id, element.target_id
            ),
        )
    })?;
    let mut result = target_act_result(&tap)?;
    if let Some(object) = result.as_object_mut() {
        object.insert("ok".to_owned(), json!(true));
        object.insert("resolved_by".to_owned(), json!(element.resolved_by));
        object.insert("backend_node_id".to_owned(), json!(element.backend_node_id));
        object.insert("element_id".to_owned(), json!(element.element_id));
        object.insert("match_count".to_owned(), json!(element.match_count));
        object.insert("returned_count".to_owned(), json!(element.returned_count));
        object.insert("window_hwnd".to_owned(), json!(window_hwnd));
        object.insert("readback_backend".to_owned(), json!("raw_cdp"));
        object.insert("method".to_owned(), json!("Input.dispatchTouchEvent"));
    }
    Ok(result)
}

#[cfg(windows)]
async fn target_act_validate_observed_cdp_element_target(
    endpoint: &str,
    window_hwnd: i64,
    session_cdp_target_id: &str,
    element_cdp_target_id: &str,
    verb: &str,
) -> Result<(), ErrorData> {
    if target_act_cdp_target_matches_session_or_frame(
        session_cdp_target_id,
        element_cdp_target_id,
        &[],
    ) {
        return Ok(());
    }
    let frames = synapse_a11y::cdp_list_frames(endpoint, window_hwnd, session_cdp_target_id)
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "target_act verb={verb} could not validate CDP frame ownership for element target {element_cdp_target_id:?} under session target {session_cdp_target_id:?}: {error}"
                ),
            )
        })?;
    if target_act_cdp_target_matches_session_or_frame(
        session_cdp_target_id,
        element_cdp_target_id,
        &frames.frames,
    ) {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::ACTION_ELEMENT_NOT_RESOLVED,
        format!(
            "target_act verb={verb} element_id belongs to CDP target {element_cdp_target_id:?}, but that target is not the owned tab {session_cdp_target_id:?} or any currently attached frame target under it; re-resolve the element because its frame may be stale"
        ),
    ))
}

fn target_act_cdp_target_matches_session_or_frame(
    session_cdp_target_id: &str,
    element_cdp_target_id: &str,
    frames: &[synapse_a11y::CdpFrameTreeEntry],
) -> bool {
    element_cdp_target_id.eq_ignore_ascii_case(session_cdp_target_id)
        || frames.iter().any(|frame| {
            frame
                .cdp_target_id
                .eq_ignore_ascii_case(element_cdp_target_id)
        })
}

#[cfg(windows)]
async fn target_act_tap_element(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    params: &TargetActParams,
    verb: &'static str,
) -> Result<TargetActTapElement, ErrorData> {
    if let Some(raw) = params
        .element_id
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        let raw = raw.trim();
        if target_act_has_dom_locator(params) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("target_act verb={verb} accepts an element_id or a DOM locator, not both"),
            ));
        }
        if let Ok(element_id) = ElementId::parse(raw)
            && let Some(backend_node_id) = synapse_a11y::cdp_backend_from_element_id(&element_id)
        {
            let element_hwnd = element_id
                .parts()
                .map_err(|error| {
                    mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        format!("target_act verb={verb} element_id is invalid: {error}"),
                    )
                })?
                .hwnd;
            if element_hwnd != window_hwnd {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "target_act verb={verb} element belongs to browser HWND 0x{element_hwnd:x}, but this session target is HWND 0x{window_hwnd:x}"
                    ),
                ));
            }
            let target_id = synapse_a11y::cdp_target_from_element_id(&element_id)
                .unwrap_or_else(|| cdp_target_id.to_owned());
            target_act_validate_observed_cdp_element_target(
                endpoint,
                window_hwnd,
                cdp_target_id,
                &target_id,
                verb,
            )
            .await?;
            return Ok(TargetActTapElement {
                backend_node_id,
                target_id,
                resolved_by: "observed_cdp_element_id".to_owned(),
                match_count: None,
                returned_count: None,
                element_id: Some(element_id.to_string()),
            });
        }
        if target_act_click_element_id_can_be_dom_id(raw) {
            let selector = target_act_dom_id_selector(raw)?;
            return target_act_locate_tap_element(
                endpoint,
                window_hwnd,
                cdp_target_id,
                synapse_a11y::CdpLocateRequest {
                    engine: synapse_a11y::CdpLocateEngine::Css,
                    query: selector,
                    strict: true,
                    limit: 2,
                    ..Default::default()
                },
                "dom_id",
                verb,
            )
            .await;
        }
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} element_id must be an observed CDP web element id or a plain DOM id; use selector/role/name for DOM locator actions"
            ),
        ));
    }

    if let Some(selector) = params
        .selector
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return target_act_locate_tap_element(
            endpoint,
            window_hwnd,
            cdp_target_id,
            synapse_a11y::CdpLocateRequest {
                engine: synapse_a11y::CdpLocateEngine::Css,
                query: selector.to_owned(),
                strict: true,
                limit: 2,
                ..Default::default()
            },
            "selector",
            verb,
        )
        .await;
    }

    if let Some(role) = trimmed_non_empty_string(params.role.as_deref()) {
        return target_act_locate_tap_element(
            endpoint,
            window_hwnd,
            cdp_target_id,
            synapse_a11y::CdpLocateRequest {
                engine: synapse_a11y::CdpLocateEngine::Role,
                query: role,
                name: trimmed_non_empty_string(params.name.as_deref()),
                strict: true,
                limit: 2,
                ..Default::default()
            },
            "role",
            verb,
        )
        .await;
    }

    if let Some(name) = trimmed_non_empty_string(params.name.as_deref()) {
        return target_act_locate_tap_element(
            endpoint,
            window_hwnd,
            cdp_target_id,
            synapse_a11y::CdpLocateRequest {
                engine: synapse_a11y::CdpLocateEngine::Text,
                query: name,
                strict: true,
                limit: 2,
                ..Default::default()
            },
            "name_text",
            verb,
        )
        .await;
    }

    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!(
            "target_act verb={verb} requires an observed CDP element_id, selector, role, or name"
        ),
    ))
}

#[cfg(windows)]
async fn target_act_locate_tap_element(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    request: synapse_a11y::CdpLocateRequest,
    resolved_by: &'static str,
    verb: &'static str,
) -> Result<TargetActTapElement, ErrorData> {
    let located = synapse_a11y::cdp_locate(endpoint, cdp_target_id, request)
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("target_act {verb} raw CDP locator resolution failed: {error}"),
            )
        })?;
    let Some(backend_node_id) = located.backend_node_ids.first().copied() else {
        return Err(mcp_error(
            error_codes::ACTION_ELEMENT_NOT_RESOLVED,
            format!(
                "target_act verb={verb} found no element for {} query {:?} in target {:?}",
                located.engine, located.query, located.target_id
            ),
        ));
    };
    Ok(TargetActTapElement {
        backend_node_id,
        target_id: located.target_id.clone(),
        resolved_by: resolved_by.to_owned(),
        match_count: Some(located.match_count),
        returned_count: Some(located.returned_count),
        element_id: Some(
            synapse_a11y::cdp_element_id_for_target(
                window_hwnd,
                &located.target_id,
                backend_node_id,
            )
            .to_string(),
        ),
    })
}

fn target_act_dom_id_selector(value: &str) -> Result<String, ErrorData> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act DOM element_id must be non-empty visible text",
        ));
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("[id=\"{escaped}\"]"))
}

fn target_act_has_any_locator(params: &TargetActParams) -> bool {
    params
        .element_id
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || target_act_has_dom_locator(params)
}

fn target_act_set_field_locator(params: &TargetActParams) -> Option<ActSetFieldTextLocator> {
    let role = trimmed_non_empty_string(params.role.as_deref());
    let name = trimmed_non_empty_string(params.name.as_deref());
    let automation_id = trimmed_non_empty_string(params.automation_id.as_deref());
    (role.is_some() || name.is_some() || automation_id.is_some()).then_some(
        ActSetFieldTextLocator {
            window_hwnd: None,
            role,
            name,
            name_substring: None,
            automation_id,
        },
    )
}

#[derive(Debug)]
enum TargetActSetFieldTarget {
    Browser {
        selector: Option<String>,
        element_id: Option<String>,
    },
    Native {
        element_id: Option<ElementId>,
        locator: Option<ActSetFieldTextLocator>,
    },
}

fn target_act_set_field_target(
    params: &TargetActParams,
) -> Result<TargetActSetFieldTarget, ErrorData> {
    let selector = trimmed_non_empty_string(params.selector.as_deref());
    let raw_element_id = trimmed_non_empty_string(params.element_id.as_deref());
    let native_locator = target_act_set_field_locator(params);
    let locator_count = usize::from(selector.is_some())
        + usize::from(raw_element_id.is_some())
        + usize::from(native_locator.is_some());
    if locator_count != 1 {
        let message = if locator_count == 0 {
            "target_act verb=set_field requires element_id, selector, or a native/UIA locator (role/name/automation_id)"
        } else {
            "target_act verb=set_field accepts exactly one of element_id, selector, or native/UIA locator (role/name/automation_id)"
        };
        return Err(mcp_error(error_codes::TOOL_PARAMS_INVALID, message));
    }
    if let Some(selector) = selector {
        return Ok(TargetActSetFieldTarget::Browser {
            selector: Some(selector),
            element_id: None,
        });
    }
    if let Some(raw_element_id) = raw_element_id {
        match ElementId::parse(&raw_element_id) {
            Ok(element_id) => {
                return Ok(TargetActSetFieldTarget::Native {
                    element_id: Some(element_id),
                    locator: None,
                });
            }
            Err(_) if raw_element_id.starts_with("chrome-tab:") => {
                return Ok(TargetActSetFieldTarget::Browser {
                    selector: None,
                    element_id: Some(raw_element_id),
                });
            }
            Err(_) if target_act_click_element_id_can_be_dom_id(&raw_element_id) => {
                return Ok(TargetActSetFieldTarget::Browser {
                    selector: Some(target_act_dom_id_selector(&raw_element_id)?),
                    element_id: None,
                });
            }
            Err(error) => {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("target_act verb=set_field element_id is invalid: {error}"),
                ));
            }
        }
    }
    Ok(TargetActSetFieldTarget::Native {
        element_id: None,
        locator: native_locator,
    })
}

fn target_act_has_dom_locator(params: &TargetActParams) -> bool {
    params
        .selector
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || params
            .role
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .name
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .value
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .option
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
}

fn trimmed_non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// #1353: `verb=scroll` routes to the real `act_scroll` wheel primitive (visible
/// in break_glass), which the hidden-tool route guidance advertises. Wheel deltas
/// come in `args` as `[dx, dy]` (e.g. `["0","120"]`); an optional screen-space
/// `x`/`y` sets the wheel point, and omitting it scrolls the focused/claimed
/// target window — the native egui/wgpu viewport case.
async fn target_act_scroll(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let parse_delta = |raw: &str| -> Result<i32, ErrorData> {
        raw.trim().parse::<i32>().map_err(|_| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("target_act verb=scroll args must be integer deltas [dx, dy]; got {raw:?}"),
            )
        })
    };
    let dx = params
        .args
        .first()
        .map(|s| parse_delta(s))
        .transpose()?
        .unwrap_or(0);
    let dy = params
        .args
        .get(1)
        .map(|s| parse_delta(s))
        .transpose()?
        .unwrap_or(0);
    if dx == 0 && dy == 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=scroll requires wheel deltas in args as [dx, dy] (e.g. args=[\"0\",\"120\"] to scroll down)",
        ));
    }
    let at = match target_act_coordinate(params)? {
        Some(coordinate) => match coordinate.space {
            TargetActCoordinateSpace::Screen => Some(ActScrollPoint {
                x: coordinate.x,
                y: coordinate.y,
            }),
            other => {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "target_act verb=scroll supports coordinate_space=screen for the at-point (got {other:?}); omit x/y to scroll the focused target window"
                    ),
                ));
            }
        },
        None => None,
    };
    let scroll_params = ActScrollParams {
        dx,
        dy,
        at,
        target: None,
        smooth: false,
        verify_delta: false,
        verify_timeout_ms: default_verify_timeout_ms(),
    };
    target_act_delegate_response(
        "act_scroll",
        service
            .act_scroll(Parameters(scroll_params), request_context.clone())
            .await,
    )
}

fn target_act_coordinate(
    params: &TargetActParams,
) -> Result<Option<TargetActCoordinate>, ErrorData> {
    match (params.x, params.y) {
        (Some(x), Some(y)) => Ok(Some(TargetActCoordinate {
            x,
            y,
            space: params
                .coordinate_space
                .unwrap_or(TargetActCoordinateSpace::Screen),
        })),
        (None, None) => {
            if params.coordinate_space.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "target_act coordinate_space requires both x and y",
                ));
            }
            Ok(None)
        }
        _ => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act coordinate fallback requires both x and y; one coordinate was missing",
        )),
    }
}

fn target_act_is_click_like_dom_action(action: &str) -> bool {
    matches!(action, "click" | "press" | "dblclick")
}

fn target_act_dom_click_count(action: &str, clicks: Option<u8>) -> Result<Option<u8>, ErrorData> {
    if target_act_is_click_like_dom_action(action) {
        target_act_click_count_for_action(action, clicks).map(Some)
    } else {
        Ok(None)
    }
}

fn target_act_click_count_for_action(action: &str, clicks: Option<u8>) -> Result<u8, ErrorData> {
    let default_clicks = if action == "dblclick" { 2 } else { 1 };
    let clicks = clicks.unwrap_or(default_clicks);
    if !(1..=3).contains(&clicks) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={action} clicks must be in 1..=3, got {clicks}"),
        ));
    }
    if action == "dblclick" && clicks != 2 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb=dblclick requires clicks/clickCount to be omitted or 2, got {clicks}"
            ),
        ));
    }
    Ok(clicks)
}

fn target_act_has_click_position_input(params: &TargetActParams) -> bool {
    params.position.is_some() || params.position_x.is_some() || params.position_y.is_some()
}

fn target_act_click_position(params: &TargetActParams) -> Result<Option<(i32, i32)>, ErrorData> {
    let flat = match (params.position_x, params.position_y) {
        (Some(x), Some(y)) => Some((x, y)),
        (None, None) => None,
        _ => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "target_act click position requires both position_x and position_y when using flat position fields",
            ));
        }
    };
    let nested = params.position.map(|position| (position.x, position.y));
    if let (Some(flat), Some(nested)) = (flat, nested) {
        if flat != nested {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act click position is ambiguous: flat position {flat:?} conflicts with nested position {nested:?}"
                ),
            ));
        }
    }
    let position = nested.or(flat);
    if let Some((x, y)) = position {
        if x < 0 || y < 0 {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act click position must be non-negative element-relative CSS pixels, got x={x} y={y}"
                ),
            ));
        }
    }
    Ok(position)
}

fn target_act_click_modifiers_bridge_value(
    modifiers: &[TargetActClickModifier],
) -> Result<Value, ErrorData> {
    serde_json::to_value(
        modifiers
            .iter()
            .map(|modifier| modifier.as_bridge_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act click modifiers encode failed: {error}"),
        )
    })
}

fn target_act_click_modifiers_act_value(
    modifiers: &[TargetActClickModifier],
) -> Result<Value, ErrorData> {
    serde_json::to_value(
        modifiers
            .iter()
            .map(|modifier| modifier.as_act_click_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act click modifiers encode failed: {error}"),
        )
    })
}

fn target_act_legacy_click_element_id(value: &str) -> Result<Option<ElementId>, ErrorData> {
    target_act_parse_observed_element_id(value, "click")
}

fn target_act_parse_observed_element_id(
    value: &str,
    verb: &str,
) -> Result<Option<ElementId>, ErrorData> {
    match ElementId::parse(value) {
        Ok(element_id) => Ok(Some(element_id)),
        Err(_) if target_act_click_element_id_can_be_dom_id(value) => Ok(None),
        Err(error) => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} element_id is invalid: {error}"),
        )),
    }
}

fn target_act_click_element_id_can_be_dom_id(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.starts_with("0x") || value.starts_with("-0x") {
        return false;
    }
    if value.starts_with("chrome-tab:") {
        return true;
    }
    !value.contains(':')
}

fn target_act_validate_dom_locator(
    action: &str,
    params: &TargetActParams,
) -> Result<(), ErrorData> {
    let has_element_id = params
        .element_id
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_selector = params
        .selector
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_semantic = params
        .role
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || params
            .name
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .value
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty());
    if !(has_element_id || has_selector || has_semantic) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} requires element_id, selector, or a semantic locator (role/name/value)"
            ),
        ));
    }
    if action == "select" {
        target_act_validate_select_options(params)?;
    }
    if action == "select" && !target_act_has_select_option_spec(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=select requires option, value, option_label, option_index, or options[]",
        ));
    }
    if action == "dispatch_event"
        && !params
            .event_type
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=dispatch_event requires non-empty event_type",
        ));
    }
    // Accept event_init as either a JSON object or a JSON-encoded object string
    // (some MCP transports stringify nested object params); the in-page bridge's
    // normalizeEventInit parses the string back into an EventInit object (#1347).
    if action == "dispatch_event"
        && params
            .event_init
            .as_ref()
            .is_some_and(|value| !value.is_object() && !value.is_string())
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=dispatch_event event_init must be a JSON object or JSON-object string when supplied",
        ));
    }
    if target_act_is_click_like_dom_action(action) {
        let _ = target_act_click_count_for_action(action, params.clicks)?;
        let _ = target_act_click_position(params)?;
    } else if params.clicks.is_some()
        || params.button.is_some()
        || !params.modifiers.is_empty()
        || target_act_has_click_position_input(params)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={action} does not accept click-only options clicks/clickCount, button, modifiers, or position"
            ),
        ));
    }
    Ok(())
}

fn target_act_has_select_option_spec(params: &TargetActParams) -> bool {
    params
        .option
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || params
            .value
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params
            .option_label
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        || params.option_index.is_some()
        || !params.options.is_empty()
}

fn target_act_validate_select_options(params: &TargetActParams) -> Result<(), ErrorData> {
    if let Some(index) = params.option_index
        && index < 0
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb=select option_index must be >= 0, got {index}"),
        ));
    }
    for (entry_index, option) in params.options.iter().enumerate() {
        let has_value = option
            .value
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_label = option
            .label
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_index = option.index.is_some();
        let present = usize::from(has_value) + usize::from(has_label) + usize::from(has_index);
        if present != 1 {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act verb=select options[{entry_index}] must contain exactly one of value, label, or index"
                ),
            ));
        }
        if let Some(index) = option.index
            && index < 0
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "target_act verb=select options[{entry_index}].index must be >= 0, got {index}"
                ),
            ));
        }
    }
    Ok(())
}

fn target_act_dom_wait_timeout(value: Option<u64>) -> Result<u64, ErrorData> {
    let wait_timeout_ms = value.unwrap_or(default_verify_timeout_ms().into());
    if wait_timeout_ms == 0 || wait_timeout_ms > 30_000 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act DOM wait_timeout_ms must be 1..=30000, got {wait_timeout_ms}"),
        ));
    }
    Ok(wait_timeout_ms)
}

/*
 * Legacy element-id click helpers below are kept for observed native/UIA/OCR
 * element ids. Browser DOM selector/name/role actions route through the normal
 * Chrome bridge instead.
 */

fn require_param(value: Option<String>, verb: &str, field: &str) -> Result<String, ErrorData> {
    value.filter(|value| !value.is_empty()).ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} requires a non-empty `{field}`"),
        )
    })
}

fn target_act_unknown_verb_error(verb: &str) -> ErrorData {
    mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("target_act verb must be one of {TARGET_ACT_KNOWN_VERBS}; got {verb:?}"),
    )
}

fn target_act_focus_window_params(
    target: Option<&SessionTarget>,
) -> Result<ActFocusWindowParams, ErrorData> {
    let hwnd = match target {
        Some(SessionTarget::Window { hwnd }) => *hwnd,
        Some(SessionTarget::Cdp { window_hwnd, .. }) => *window_hwnd,
        None => {
            return Err(target_act_focus_window_missing_target_error());
        }
    };
    Ok(ActFocusWindowParams {
        hwnd: Some(hwnd),
        title_regex: None,
        pid: None,
        verify_timeout_ms: default_verify_timeout_ms(),
        stable_ms: DEFAULT_TARGET_ACT_FOCUS_STABLE_MS,
    })
}

fn target_act_focus_window_missing_target_error() -> ErrorData {
    mcp_error(
        error_codes::TARGET_NOT_SET,
        "target_act verb=focus_window requires this MCP session to have an agent_logical_foreground/foreground_lane target; call window_list then set_target for the exact HWND first",
    )
}

fn target_act_focus_window_preflight(
    service: &SynapseService,
    session_id: &str,
    target: &SessionTarget,
) -> Result<(), ErrorData> {
    if service
        .target_claim_for_session(session_id, target)?
        .is_none()
    {
        return Err(mcp_error(
            error_codes::TARGET_CLAIM_NOT_FOUND,
            "target_act verb=focus_window requires this MCP session to own a live target_claim for the session target before deliberate real-foreground activation",
        ));
    }
    let lease = synapse_action::lease::status();
    match lease.owner_session_id.as_deref() {
        Some(owner) if owner == session_id => Ok(()),
        Some(_owner) => Err(mcp_error(
            error_codes::ACTION_FOREGROUND_LEASE_BUSY,
            "target_act verb=focus_window requires this MCP session to own the foreground input lease; another live session owns it",
        )),
        None => Err(mcp_error(
            error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
            "target_act verb=focus_window requires this MCP session to own the foreground input lease before deliberate real-foreground activation",
        )),
    }
}

fn target_act_read_delegated_tool(
    target: Option<&SessionTarget>,
) -> Result<&'static str, ErrorData> {
    match target {
        Some(SessionTarget::Cdp { .. }) => Ok("cdp_target_info"),
        Some(SessionTarget::Window { .. }) => Ok("observe"),
        None => Err(mcp_error(
            error_codes::TARGET_NOT_SET,
            "target_act verb=read requires this MCP session to have an agent_logical_foreground/foreground_lane target; refusing observe's legacy human OS foreground fallback",
        )),
    }
}

fn target_act_routing_description() -> String {
    "capability-preserving; delegated to the session-targeted primitive, inheriting action audit plus lane/lease/foreground guards and refusing implicit human OS foreground use before input".to_owned()
}

fn target_act_result<T: Serialize>(value: &T) -> Result<Value, ErrorData> {
    serde_json::to_value(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act failed to encode delegated tool result: {error}"),
        )
    })
}

fn target_act_delegate_response<T: Serialize>(
    delegated_tool: &'static str,
    result: Result<Json<T>, ErrorData>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    match result {
        Ok(response) => Ok((
            delegated_tool,
            true,
            TARGET_ACT_STATUS_OK,
            target_act_result(&response.0)?,
        )),
        Err(error) => {
            let status = target_act_error_status(&error);
            Ok((
                delegated_tool,
                false,
                status,
                target_act_error_result(delegated_tool, error),
            ))
        }
    }
}

async fn target_act_key_press(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    if target_act_coordinate(params)?.is_some() || target_act_has_any_locator(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=key/press with key(s) targets the current focused control and does not accept x/y or element/DOM locators; focus/click first, or use insert_text/append_text",
        ));
    }
    let verb = params.verb.as_str();
    let keys = target_act_key_chord_keys(params, verb)?;
    let press_params = target_act_press_params(keys, params.wait_timeout_ms, verb)?;
    let response = service
        .act_press(Parameters(press_params), request_context.clone())
        .await;
    target_act_delegate_response("act_press", response)
}

async fn target_act_insert_or_append_text(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
    append: bool,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    if target_act_has_key_chord(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=insert_text/append_text does not accept key(s); use verb=key for raw chords",
        ));
    }
    let verb = if append { "append_text" } else { "insert_text" };
    if target_act_coordinate(params)?.is_some() && target_act_has_any_locator(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} accepts either x/y coordinates or an element/DOM locator, not both"
            ),
        ));
    }
    let text = require_param(params.text.clone(), verb, "text")?;
    if let Some(element_id) = target_act_native_text_element_id(params, verb)? {
        let verify_timeout_ms = target_act_verify_timeout(params.wait_timeout_ms, verb)?;
        return target_act_insert_or_append_text_native(
            service,
            element_id,
            text,
            append,
            verify_timeout_ms,
            request_context,
        )
        .await;
    }
    let mut steps = Vec::new();

    if let Some(focus_step) =
        target_act_focus_for_text_insert(service, params, request_context, verb).await?
    {
        let (tool, ok, status, result) = focus_step;
        steps.push(json!({
            "step": "focus",
            "delegated_tool": tool,
            "ok": ok,
            "status": status,
            "result": result,
        }));
        if !ok {
            return Ok((
                "target_act.text_focus+act_type",
                false,
                status,
                json!({
                    "ok": false,
                    "mode": verb,
                    "failed_step": "focus",
                    "steps": steps,
                }),
            ));
        }
    }

    if append {
        let press_params =
            target_act_press_params(vec!["ctrl".to_owned(), "end".to_owned()], None, verb)?;
        let key_response = service
            .act_press(Parameters(press_params), request_context.clone())
            .await;
        let (tool, ok, status, result) = target_act_delegate_response("act_press", key_response)?;
        steps.push(json!({
            "step": "move_caret_to_end",
            "delegated_tool": tool,
            "ok": ok,
            "status": status,
            "result": result,
        }));
        if !ok {
            return Ok((
                "target_act.text_focus+act_press+act_type",
                false,
                status,
                json!({
                    "ok": false,
                    "mode": verb,
                    "failed_step": "move_caret_to_end",
                    "steps": steps,
                }),
            ));
        }
    }

    let type_params = target_act_type_params(text, params.wait_timeout_ms)?;
    let type_response = service
        .act_type(Parameters(type_params), request_context.clone())
        .await;
    let (tool, ok, status, result) = target_act_delegate_response("act_type", type_response)?;
    steps.push(json!({
        "step": "type",
        "delegated_tool": tool,
        "ok": ok,
        "status": status,
        "result": result,
    }));

    Ok((
        if append {
            "target_act.text_focus+act_press+act_type"
        } else {
            "target_act.text_focus+act_type"
        },
        ok,
        status,
        json!({
            "ok": ok,
            "mode": verb,
            "steps": steps,
        }),
    ))
}

fn target_act_native_text_element_id(
    params: &TargetActParams,
    verb: &str,
) -> Result<Option<ElementId>, ErrorData> {
    let Some(raw_element_id) = params
        .element_id
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(element_id) = target_act_parse_observed_element_id(raw_element_id, verb)? else {
        return Ok(None);
    };
    if synapse_a11y::cdp_backend_from_element_id(&element_id).is_some() {
        return Ok(None);
    }
    if target_act_has_dom_locator(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} accepts an observed native element_id or a DOM locator, not both"
            ),
        ));
    }
    Ok(Some(element_id))
}

async fn target_act_insert_or_append_text_native(
    service: &SynapseService,
    element_id: ElementId,
    text: String,
    append: bool,
    verify_timeout_ms: u32,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let verb = if append { "append_text" } else { "insert_text" };
    let delegated_tool = if append {
        "synapse_a11y.append_element_text"
    } else {
        "synapse_a11y.replace_element_text_selection"
    };
    let session_id = target_act_session_id(request_context, verb)?;
    let (element_hwnd, root_hwnd, target) = target_act_native_element_target(&element_id, verb)?;
    let request_details = json!({
        "session_id": &session_id,
        "verb": verb,
        "delegated_tool": delegated_tool,
        "element_id": element_id.to_string(),
        "element_hwnd": element_hwnd,
        "window_hwnd": root_hwnd,
        "text_len": text.chars().count(),
        "text_utf16_len": text.encode_utf16().count(),
        "append": append,
        "verify_timeout_ms": verify_timeout_ms,
        "required_foreground": false,
    });
    match service.session_target(Some(&session_id))? {
        Some(current) if current == target => {}
        Some(current) => {
            let error = mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "target_act verb={verb} element belongs to HWND 0x{root_hwnd:x}, but this session target is {current:?}"
                ),
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                delegated_tool,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
        None => {
            let error = mcp_error(
                error_codes::TARGET_NOT_SET,
                format!(
                    "target_act verb={verb} requires this MCP session to have an owned window target; call set_target for the exact HWND first"
                ),
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                delegated_tool,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
    }
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            delegated_tool,
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let result = if append {
        synapse_a11y::append_element_text(&element_id, &text)
    } else {
        synapse_a11y::replace_element_text_selection(&element_id, &text)
    }
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!("target_act native {verb} failed: {error}"),
        )
    });
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(readback) => Ok((
            delegated_tool,
            true,
            TARGET_ACT_STATUS_OK,
            target_act_result(&readback)?,
        )),
        Err(error) => Ok((
            delegated_tool,
            false,
            target_act_error_status(&error),
            target_act_error_result(delegated_tool, error),
        )),
    }
}

async fn target_act_focus_for_text_insert(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
    verb: &str,
) -> Result<Option<(&'static str, bool, &'static str, Value)>, ErrorData> {
    if target_act_coordinate(params)?.is_some() {
        return target_act_coordinate_click(service, "click", params, request_context)
            .await
            .map(Some);
    }

    if let Some(raw_element_id) = params
        .element_id
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        if let Some(element_id) = target_act_legacy_click_element_id(raw_element_id)? {
            let click_params = target_act_click_params(element_id, 1, None, &[])?;
            let response = service
                .act_click(Parameters(click_params), request_context.clone())
                .await;
            return target_act_delegate_response("act_click", response).map(Some);
        }
        return target_act_browser_dom_action(service, "click", params, request_context)
            .await
            .map(Some);
    }

    if target_act_has_dom_locator(params) {
        return target_act_browser_dom_action(service, "click", params, request_context)
            .await
            .map(Some);
    }

    tracing::info!(
        code = "TARGET_ACT_TEXT_INSERT_CURRENT_FOCUS",
        verb,
        "target_act text insertion will use the current focused control without a focus step"
    );
    Ok(None)
}

const TARGET_ACT_SET_SELECTION_JS: &str = r#"(el, start, end) => {
    if (!Number.isInteger(start) || !Number.isInteger(end) || start < 0 || end < start) {
        throw new Error(`invalid selection range ${start}..${end}`);
    }
    const selectedText = () => {
        const selection = el.ownerDocument.defaultView.getSelection();
        return selection ? selection.toString() : '';
    };
    if (typeof el.setSelectionRange === 'function' && 'value' in el) {
        if (el.disabled || el.readOnly) {
            throw new Error('target value control is disabled or readonly');
        }
        const value = String(el.value ?? '');
        if (end > value.length) {
            throw new Error(`selection range ${start}..${end} exceeds value length ${value.length}`);
        }
        const beforeStart = el.selectionStart ?? 0;
        const beforeEnd = el.selectionEnd ?? beforeStart;
        el.focus();
        el.setSelectionRange(start, end);
        el.dispatchEvent(new Event('select', { bubbles: true }));
        return {
            method: 'dom_set_selection_range',
            text_len: value.length,
            requested_start: start,
            requested_end: end,
            before_start: beforeStart,
            before_end: beforeEnd,
            after_start: el.selectionStart ?? start,
            after_end: el.selectionEnd ?? end,
            selected_text: value.slice(start, end)
        };
    }
    if (!el.isContentEditable) {
        throw new Error('target is not an editable value control or contenteditable element');
    }
    const doc = el.ownerDocument;
    const walker = doc.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    const textNodes = [];
    let textLen = 0;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const len = node.nodeValue.length;
        textNodes.push({ node, start: textLen, end: textLen + len });
        textLen += len;
    }
    if (end > textLen) {
        throw new Error(`selection range ${start}..${end} exceeds textContent length ${textLen}`);
    }
    const boundary = (offset) => {
        for (const item of textNodes) {
            if (offset <= item.end) {
                return { node: item.node, offset: Math.max(0, offset - item.start) };
            }
        }
        return { node: el, offset: el.childNodes.length };
    };
    const selectionOffsets = () => {
        const selection = doc.defaultView.getSelection();
        if (!selection || selection.rangeCount === 0) {
            return { start: 0, end: 0 };
        }
        const range = selection.getRangeAt(0);
        const preStart = doc.createRange();
        preStart.selectNodeContents(el);
        preStart.setEnd(range.startContainer, range.startOffset);
        const preEnd = doc.createRange();
        preEnd.selectNodeContents(el);
        preEnd.setEnd(range.endContainer, range.endOffset);
        return { start: preStart.toString().length, end: preEnd.toString().length };
    };
    const before = selectionOffsets();
    const startBoundary = boundary(start);
    const endBoundary = boundary(end);
    const range = doc.createRange();
    range.setStart(startBoundary.node, startBoundary.offset);
    range.setEnd(endBoundary.node, endBoundary.offset);
    el.focus();
    const selection = doc.defaultView.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    doc.dispatchEvent(new Event('selectionchange', { bubbles: true }));
    const after = selectionOffsets();
    return {
        method: 'contenteditable_dom_range',
        text_len: textLen,
        requested_start: start,
        requested_end: end,
        before_start: before.start,
        before_end: before.end,
        after_start: after.start,
        after_end: after.end,
        selected_text: selectedText()
    };
}"#;

async fn target_act_set_selection(
    service: &SynapseService,
    params: &TargetActParams,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    if target_act_coordinate(params)?.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_selection does not accept x/y coordinates; pass an observed element_id",
        ));
    }
    if target_act_has_dom_locator(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_selection currently accepts an observed Synapse element_id only, not selector/role/name/value/option locators",
        ));
    }
    if target_act_has_key_chord(params) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_selection does not accept key(s); use verb=key for raw keyboard selection chords",
        ));
    }
    if params.text.as_ref().is_some_and(|value| !value.is_empty()) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_selection does not accept text; use insert_text or append_text after setting the selection",
        ));
    }
    let element_id = require_param(params.element_id.clone(), "set_selection", "element_id")?;
    let element_id = ElementId::parse(&element_id).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb=set_selection element_id is invalid: {error}"),
        )
    })?;
    let (start, end) = target_act_selection_range(params)?;
    if synapse_a11y::cdp_backend_from_element_id(&element_id).is_some() {
        target_act_set_selection_web(service, element_id, start, end, request_context).await
    } else {
        target_act_set_selection_native(service, element_id, start, end, request_context).await
    }
}

async fn target_act_set_selection_web(
    service: &SynapseService,
    element_id: ElementId,
    start: u32,
    end: u32,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    #[cfg(windows)]
    {
        const DELEGATED_TOOL: &str = "synapse_a11y.cdp_evaluate_on_element";
        let session_id = target_act_session_id(request_context, "set_selection")?;
        let backend_node_id =
            synapse_a11y::cdp_backend_from_element_id(&element_id).ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "target_act verb=set_selection element_id is not a CDP-backed web element",
                )
            })?;
        let element_target_id =
            synapse_a11y::cdp_target_from_element_id(&element_id).ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "target_act verb=set_selection web element_id must include an embedded CDP target id; re-resolve it with find/observe against the owned tab",
                )
            })?;
        let element_hwnd = element_id
            .parts()
            .map_err(|error| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("target_act verb=set_selection element_id is invalid: {error}"),
                )
            })?
            .hwnd;
        let request_details = json!({
            "session_id": &session_id,
            "verb": "set_selection",
            "element_id": element_id.to_string(),
            "element_hwnd": element_hwnd,
            "backend_node_id": backend_node_id,
            "element_cdp_target_id": &element_target_id,
            "selection_start": start,
            "selection_end": end,
            "delegated_tool": DELEGATED_TOOL,
            "required_foreground": false,
        });
        let Some(target) = service.session_target(Some(&session_id))? else {
            let error = mcp_error(
                error_codes::TARGET_NOT_SET,
                "target_act verb=set_selection requires this MCP session to own a CDP browser target; bind one with cdp_open_tab/set_target first",
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                DELEGATED_TOOL,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        };
        let (window_hwnd, session_cdp_target_id) = match &target {
            SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id,
            } => (*window_hwnd, cdp_target_id.clone()),
            SessionTarget::Window { .. } => {
                let error = mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    "target_act verb=set_selection with an observed web element_id requires a session-owned CDP target, not a native/window target",
                );
                service.audit_action_denied_with_details_for_session(
                    "target_act",
                    &error,
                    &request_details,
                    &session_id,
                );
                return Ok((
                    DELEGATED_TOOL,
                    false,
                    target_act_error_status(&error),
                    target_act_error_result("target_act", error),
                ));
            }
        };
        if window_hwnd != element_hwnd {
            let error = mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "target_act verb=set_selection element belongs to browser HWND 0x{element_hwnd:x}, but this session target is HWND 0x{window_hwnd:x}"
                ),
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                DELEGATED_TOOL,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
        if let Err(error) =
            service.ensure_target_claim_allows_session("target_act", &session_id, &target)
        {
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                DELEGATED_TOOL,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            let error = mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "target_act verb=set_selection requires a raw CDP debugging endpoint for window 0x{window_hwnd:x}"
                ),
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                DELEGATED_TOOL,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        };
        if let Err(error) = target_act_validate_observed_cdp_element_target(
            &endpoint,
            window_hwnd,
            &session_cdp_target_id,
            &element_target_id,
            "set_selection",
        )
        .await
        {
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                DELEGATED_TOOL,
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
        let request_details = json!({
            "session_id": &session_id,
            "verb": "set_selection",
            "element_id": element_id.to_string(),
            "window_hwnd": window_hwnd,
            "backend_node_id": backend_node_id,
            "session_cdp_target_id": &session_cdp_target_id,
            "element_cdp_target_id": &element_target_id,
            "dispatch_cdp_target_id": &element_target_id,
            "selection_start": start,
            "selection_end": end,
            "delegated_tool": DELEGATED_TOOL,
            "required_foreground": false,
        });
        service.audit_action_started_with_details_for_session(
            "target_act",
            &request_details,
            &session_id,
        )?;
        let result = synapse_a11y::cdp_evaluate_on_element(
            &endpoint,
            &element_target_id,
            backend_node_id,
            TARGET_ACT_SET_SELECTION_JS,
            &[json!(start), json!(end)],
            false,
            true,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("target_act web set_selection failed: {error}"),
            )
        });
        service.audit_action_result_for_session("target_act", &result, &session_id)?;
        match result {
            Ok(readback) => {
                let mut value = target_act_result(&readback)?;
                if let Some(object) = value.as_object_mut() {
                    object.insert("element_id".to_owned(), json!(element_id.to_string()));
                    object.insert("backend_node_id".to_owned(), json!(backend_node_id));
                    object.insert("session_target_id".to_owned(), json!(session_cdp_target_id));
                    object.insert("target_id".to_owned(), json!(element_target_id));
                    object.insert("window_hwnd".to_owned(), json!(window_hwnd));
                }
                Ok((DELEGATED_TOOL, true, TARGET_ACT_STATUS_OK, value))
            }
            Err(error) => Ok((
                DELEGATED_TOOL,
                false,
                target_act_error_status(&error),
                target_act_error_result(DELEGATED_TOOL, error),
            )),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (service, element_id, start, end, request_context);
        let error = mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            "target_act verb=set_selection observed CDP element_id support requires Windows CDP action support",
        );
        Ok((
            "synapse_a11y.cdp_evaluate_on_element",
            false,
            target_act_error_status(&error),
            target_act_error_result("synapse_a11y.cdp_evaluate_on_element", error),
        ))
    }
}

fn target_act_native_element_target(
    element_id: &ElementId,
    verb: &str,
) -> Result<(i64, i64, SessionTarget), ErrorData> {
    let element_hwnd = element_id
        .parts()
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("target_act verb={verb} element_id is invalid: {error}"),
            )
        })?
        .hwnd;
    let root_hwnd = synapse_a11y::top_level_root_hwnd(element_hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act verb={verb} element HWND 0x{element_hwnd:x} root readback failed: {error}"
            ),
        )
    })?;
    Ok((
        element_hwnd,
        root_hwnd,
        SessionTarget::Window { hwnd: root_hwnd },
    ))
}

async fn target_act_set_selection_native(
    service: &SynapseService,
    element_id: ElementId,
    start: u32,
    end: u32,
    request_context: &RequestContext<RoleServer>,
) -> Result<(&'static str, bool, &'static str, Value), ErrorData> {
    let session_id = target_act_session_id(request_context, "set_selection")?;
    let (element_hwnd, root_hwnd, target) =
        target_act_native_element_target(&element_id, "set_selection")?;
    let request_details = json!({
        "session_id": &session_id,
        "verb": "set_selection",
        "element_id": element_id.to_string(),
        "element_hwnd": element_hwnd,
        "window_hwnd": root_hwnd,
        "selection_start": start,
        "selection_end": end,
        "required_foreground": false,
    });
    match service.session_target(Some(&session_id))? {
        Some(current) if current == target => {}
        Some(current) => {
            let error = mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "target_act verb=set_selection element belongs to HWND 0x{root_hwnd:x}, but this session target is {current:?}"
                ),
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                "synapse_a11y.set_element_text_selection",
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
        None => {
            let error = mcp_error(
                error_codes::TARGET_NOT_SET,
                "target_act verb=set_selection requires this MCP session to have an owned window target; call set_target for the exact HWND first",
            );
            service.audit_action_denied_with_details_for_session(
                "target_act",
                &error,
                &request_details,
                &session_id,
            );
            return Ok((
                "synapse_a11y.set_element_text_selection",
                false,
                target_act_error_status(&error),
                target_act_error_result("target_act", error),
            ));
        }
    }
    if let Err(error) =
        service.ensure_target_claim_allows_session("target_act", &session_id, &target)
    {
        service.audit_action_denied_with_details_for_session(
            "target_act",
            &error,
            &request_details,
            &session_id,
        );
        return Ok((
            "synapse_a11y.set_element_text_selection",
            false,
            target_act_error_status(&error),
            target_act_error_result("target_act", error),
        ));
    }
    service.audit_action_started_with_details_for_session(
        "target_act",
        &request_details,
        &session_id,
    )?;
    let result =
        synapse_a11y::set_element_text_selection(&element_id, start, end).map_err(|error| {
            mcp_error(
                error.code(),
                format!("target_act native set_selection failed: {error}"),
            )
        });
    service.audit_action_result_for_session("target_act", &result, &session_id)?;
    match result {
        Ok(readback) => Ok((
            "synapse_a11y.set_element_text_selection",
            true,
            TARGET_ACT_STATUS_OK,
            target_act_result(&readback)?,
        )),
        Err(error) => Ok((
            "synapse_a11y.set_element_text_selection",
            false,
            target_act_error_status(&error),
            target_act_error_result("synapse_a11y.set_element_text_selection", error),
        )),
    }
}

fn target_act_click_params(
    element_id: ElementId,
    clicks: u8,
    button: Option<TargetActMouseButton>,
    modifiers: &[TargetActClickModifier],
) -> Result<ActClickParams, ErrorData> {
    let modifiers = target_act_click_modifiers_act_value(modifiers)?;
    serde_json::from_value(json!({
        "target": {
            "element_id": element_id.to_string()
        },
        "button": button.map(TargetActMouseButton::as_str).unwrap_or("left"),
        "clicks": clicks,
        "modifiers": modifiers,
        "verify_delta": true,
        "verify_timeout_ms": default_verify_timeout_ms()
    }))
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act failed to construct act_click params: {error}"),
        )
    })
}

fn target_act_click_point_params(
    point: Point,
    clicks: u8,
    button: Option<TargetActMouseButton>,
    modifiers: &[TargetActClickModifier],
) -> Result<ActClickParams, ErrorData> {
    let modifiers = target_act_click_modifiers_act_value(modifiers)?;
    serde_json::from_value(json!({
        "target": {
            "x": point.x,
            "y": point.y
        },
        "button": button.map(TargetActMouseButton::as_str).unwrap_or("left"),
        "clicks": clicks,
        "modifiers": modifiers,
        "verify_delta": true,
        "verify_timeout_ms": default_verify_timeout_ms()
    }))
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act failed to construct coordinate act_click params: {error}"),
        )
    })
}

fn target_act_type_params(
    text: String,
    wait_timeout_ms: Option<u64>,
) -> Result<ActTypeParams, ErrorData> {
    let verify_timeout_ms = target_act_type_verify_timeout(wait_timeout_ms)?;
    serde_json::from_value(json!({
        "text": text,
        "verify_delta": true,
        "verify_timeout_ms": verify_timeout_ms
    }))
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act failed to construct act_type params: {error}"),
        )
    })
}

fn target_act_press_params(
    keys: Vec<String>,
    wait_timeout_ms: Option<u64>,
    verb: &str,
) -> Result<ActPressParams, ErrorData> {
    let verify_timeout_ms = target_act_verify_timeout(wait_timeout_ms, verb)?;
    serde_json::from_value(json!({
        "keys": keys,
        "verify_delta": true,
        "verify_timeout_ms": verify_timeout_ms
    }))
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("target_act failed to construct act_press params: {error}"),
        )
    })
}

fn target_act_type_verify_timeout(value: Option<u64>) -> Result<u32, ErrorData> {
    target_act_verify_timeout(value, "type")
}

fn target_act_verify_timeout(value: Option<u64>, verb: &str) -> Result<u32, ErrorData> {
    let wait_timeout_ms = value.unwrap_or_else(|| u64::from(default_verify_timeout_ms()));
    if !(50..=5000).contains(&wait_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "target_act verb={verb} wait_timeout_ms must be 50..=5000, got {wait_timeout_ms}"
            ),
        ));
    }
    Ok(wait_timeout_ms as u32)
}

fn target_act_has_key_chord(params: &TargetActParams) -> bool {
    params
        .key
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || !params.keys.is_empty()
}

fn target_act_key_chord_keys(
    params: &TargetActParams,
    verb: &str,
) -> Result<Vec<String>, ErrorData> {
    let has_key = params
        .key
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_keys = !params.keys.is_empty();
    match (has_key, has_keys) {
        (true, true) => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} accepts either key or keys, not both"),
        )),
        (true, false) => {
            target_act_parse_key_chord(params.key.as_deref().unwrap_or_default(), verb)
        }
        (false, true) => Ok(params.keys.clone()),
        (false, false) => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} requires key or keys"),
        )),
    }
}

fn target_act_parse_key_chord(raw: &str, verb: &str) -> Result<Vec<String>, ErrorData> {
    let keys = raw
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb={verb} key chord must contain at least one key"),
        ));
    }
    Ok(keys)
}

fn target_act_selection_range(params: &TargetActParams) -> Result<(u32, u32), ErrorData> {
    let start = params.selection_start.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_selection requires selection_start (alias: start)",
        )
    })?;
    let end = params.selection_end.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act verb=set_selection requires selection_end (alias: end)",
        )
    })?;
    if end < start {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("target_act verb=set_selection requires end >= start, got {start}..{end}"),
        ));
    }
    Ok((start, end))
}

fn target_act_window_coordinate_to_screen_point(
    hwnd: i64,
    coordinate: TargetActCoordinate,
) -> Result<Point, ErrorData> {
    if coordinate.space == TargetActCoordinateSpace::Viewport {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target_act native/window coordinate click does not support coordinate_space=viewport; use screen or window coordinates, or bind a Chrome CDP target for viewport coordinates",
        ));
    }
    let context = synapse_a11y::foreground_context(hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act coordinate click target window HWND 0x{hwnd:x} bounds readback failed: {error}"
            ),
        )
    })?;
    let bounds = context.window_bounds;
    let point = match coordinate.space {
        TargetActCoordinateSpace::Screen => Point {
            x: coordinate.x,
            y: coordinate.y,
        },
        TargetActCoordinateSpace::Window => Point {
            x: bounds.x.saturating_add(coordinate.x),
            y: bounds.y.saturating_add(coordinate.y),
        },
        TargetActCoordinateSpace::Viewport => unreachable!("viewport rejected above"),
    };
    if !target_act_rect_contains_point(bounds, point) {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "target_act coordinate click point ({}, {}) is outside target window 0x{hwnd:x} bounds ({}, {}, {}x{})",
                point.x, point.y, bounds.x, bounds.y, bounds.w, bounds.h
            ),
        ));
    }
    Ok(point)
}

fn target_act_window_coordinate_foreground_preflight(
    hwnd: i64,
    session_id: &str,
) -> Result<(), ErrorData> {
    let target_root = synapse_a11y::top_level_root_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act coordinate click target HWND 0x{hwnd:x} root readback failed: {error}"
            ),
        )
    })?;
    let foreground = synapse_a11y::current_foreground_context().map_err(|error| {
        mcp_error(
            error.code(),
            format!("target_act coordinate click foreground readback failed: {error}"),
        )
    })?;
    let foreground_root = synapse_a11y::top_level_root_hwnd(foreground.hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "target_act coordinate click foreground HWND 0x{:x} root readback failed: {error}",
                foreground.hwnd
            ),
        )
    })?;
    if foreground_root == target_root {
        return Ok(());
    }

    // #1351: verb=focus_window may have verified the target as foreground, but the
    // MCP round-trip gap let another window (often the human) reclaim it, so this
    // pre-click readback no longer matches and the click was refused — asking for
    // a focus step that was already done. If THIS session holds the foreground
    // input lease, it is authorized to re-assert the claimed target's foreground
    // (exactly what verb=focus_window does) and self-heal the race once, rather
    // than refuse. Without the lease we fail loud with a precise, evidence-bearing
    // foreground-lost diagnostic (both readbacks) instead of a focus re-request.
    let holds_lease =
        synapse_action::lease::status().owner_session_id.as_deref() == Some(session_id);
    if holds_lease {
        if let Err(error) = synapse_a11y::focus_window_with_intent(
            target_root,
            synapse_a11y::ForegroundActivationIntent::LeaseContextRestore {
                caller: "target_act",
            },
        ) {
            return Err(target_act_foreground_lost_error(
                target_root,
                foreground_root,
                &foreground,
                true,
                Some(format!("lease-held re-focus failed: {error}")),
            ));
        }
        // Confirm the re-assert took before clicking.
        let after = synapse_a11y::current_foreground_context().ok();
        let after_root = after
            .as_ref()
            .and_then(|context| synapse_a11y::top_level_root_hwnd(context.hwnd).ok());
        if after_root == Some(target_root) {
            tracing::info!(
                code = "TARGET_ACT_COORDINATE_FOREGROUND_SELF_HEALED",
                target_root,
                prior_foreground_root = foreground_root,
                session_id,
                "readback=foreground self-healed lease-held re-focus before native coordinate click"
            );
            return Ok(());
        }
        let detail = after.map(|context| {
            format!(
                "lease-held re-focus did not stick; foreground is still 0x{:x} process={} title={:?}",
                context.hwnd, context.process_name, context.window_title
            )
        });
        return Err(target_act_foreground_lost_error(
            target_root,
            foreground_root,
            &foreground,
            true,
            detail,
        ));
    }
    Err(target_act_foreground_lost_error(
        target_root,
        foreground_root,
        &foreground,
        false,
        None,
    ))
}

/// #1351: precise foreground-lost diagnostic for a native coordinate click whose
/// pre-click foreground readback no longer matches the target. Carries both
/// readbacks and whether the session held the lease, so the caller can tell a
/// human-reclaimed-foreground race apart from a missing focus/lease step.
fn target_act_foreground_lost_error(
    target_root: i64,
    foreground_root: i64,
    foreground: &synapse_core::ForegroundContext,
    holds_lease: bool,
    extra: Option<String>,
) -> ErrorData {
    let remediation = if holds_lease {
        "this session holds the foreground input lease but could not re-assert the target's foreground (the human or another window is actively holding it); retry, or wait for the human to release foreground"
    } else {
        "acquire the foreground input lease (control_lease_acquire) so target_act can re-assert the claimed target's foreground automatically, or call verb=focus_window immediately before the click"
    };
    let message = format!(
        "target_act native/window coordinate click: the target 0x{target_root:x} is not the OS foreground at click time; foreground moved to root=0x{foreground_root:x} process={} title={:?}. {remediation}.{}",
        foreground.process_name,
        foreground.window_title,
        extra
            .as_ref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default()
    );
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::FOREGROUND_ACTIVATION_REFUSED,
            "reason": "foreground_moved_before_click",
            "target_root_hwnd": target_root,
            "foreground_root_hwnd": foreground_root,
            "foreground_hwnd": foreground.hwnd,
            "foreground_process": foreground.process_name,
            "foreground_title": foreground.window_title,
            "session_holds_foreground_lease": holds_lease,
            "self_heal_attempted": holds_lease,
        })),
    )
}

fn target_act_rect_contains_point(rect: Rect, point: Point) -> bool {
    point.x >= rect.x
        && point.y >= rect.y
        && point.x < rect.x.saturating_add(rect.w)
        && point.y < rect.y.saturating_add(rect.h)
}

fn target_act_error_result(delegated_tool: &'static str, error: ErrorData) -> Value {
    let code = target_act_error_code(&error)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    json!({
        "error": {
            "delegated_tool": delegated_tool,
            "code": code,
            "message": error.message.to_string(),
            "data": error.data,
        }
    })
}

fn target_act_error_status(error: &ErrorData) -> &'static str {
    match target_act_error_code(error) {
        Some(
            error_codes::ACTION_NO_OBSERVED_DELTA
            | error_codes::ACTION_FOREGROUND_LOST
            | error_codes::ACTION_POSTCONDITION_FAILED
            | error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED
            | error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE,
        ) => TARGET_ACT_STATUS_VERIFY_NEEDED,
        Some(
            error_codes::ACTION_ELEMENT_NOT_RESOLVED
            | error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
            | error_codes::ACTION_ELEMENT_VALUE_READ_ONLY
            | error_codes::ACTION_FOREGROUND_LEASE_BUSY
            | error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD
            | error_codes::ACTION_TARGET_INVALID
            | error_codes::A11Y_ELEMENT_STALE
            | error_codes::CHROME_DOM_ACTION_UNSUPPORTED
            | error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS
            | error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE
            | error_codes::CHROME_DOM_ELEMENT_NOT_FOUND
            | error_codes::CHROME_DOM_SELECTOR_INVALID
            | error_codes::FOREGROUND_ACTIVATION_REFUSED
            | error_codes::TARGET_CO_OWNED
            | error_codes::TARGET_CLAIM_NOT_FOUND
            | error_codes::TARGET_NOT_SET
            | error_codes::TARGET_WINDOW_NOT_FOUND
            | error_codes::TOOL_PROFILE_POLICY_DENIED
            | error_codes::TOOL_PARAMS_INVALID
            | error_codes::TRANSIENT_ELEMENT_EXPIRED,
        ) => TARGET_ACT_STATUS_REFUSED,
        _ => TARGET_ACT_STATUS_ERROR,
    }
}

fn target_act_error_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::schemars::schema_for;

    #[test]
    fn target_act_verb_click_deserializes() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "element_id": "0x2a:0000000000000001",
            "clicks": 2
        }))
        .expect("click params should deserialize");

        assert_eq!(params.verb.as_str(), "click");
        assert_eq!(params.clicks, Some(2));
    }

    #[test]
    fn target_act_set_field_accepts_selector() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_field",
            "selector": "input[name=\"q\"]",
            "text": "hello"
        }))
        .expect("set_field selector params should deserialize");

        assert_eq!(params.verb.as_str(), "set_field");
        assert_eq!(params.selector.as_deref(), Some("input[name=\"q\"]"));
        assert_eq!(params.text.as_deref(), Some("hello"));
        assert!(params.element_id.is_none());
    }

    #[test]
    fn target_act_set_field_accepts_native_locator() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_field",
            "role": "document",
            "name": "Message Body",
            "automation_id": "compose-body",
            "text": "hello"
        }))
        .expect("set_field native locator params should deserialize");
        let locator =
            target_act_set_field_locator(&params).expect("role/name/automation_id locator");

        assert_eq!(params.verb.as_str(), "set_field");
        assert_eq!(locator.role.as_deref(), Some("document"));
        assert_eq!(locator.name.as_deref(), Some("Message Body"));
        assert_eq!(locator.automation_id.as_deref(), Some("compose-body"));
        assert!(locator.name_substring.is_none());
    }

    #[test]
    fn target_act_set_field_bridge_element_id_routes_to_browser_bridge() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_field",
            "element_id": "chrome-tab:589708698:frame:0:path:0.1.1",
            "text": "hello"
        }))
        .expect("set_field bridge element_id params should deserialize");

        match target_act_set_field_target(&params).expect("bridge element_id routes") {
            TargetActSetFieldTarget::Browser {
                selector,
                element_id,
            } => {
                assert!(selector.is_none());
                assert_eq!(
                    element_id.as_deref(),
                    Some("chrome-tab:589708698:frame:0:path:0.1.1")
                );
            }
            TargetActSetFieldTarget::Native { .. } => {
                panic!("bridge element_id must not route to native/UIA")
            }
        }
    }

    #[test]
    fn target_act_set_field_native_element_id_routes_to_native_text() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_field",
            "element_id": "0x2a:0000000000000001",
            "text": "hello"
        }))
        .expect("set_field native element_id params should deserialize");

        match target_act_set_field_target(&params).expect("native element_id routes") {
            TargetActSetFieldTarget::Native {
                element_id,
                locator,
            } => {
                assert_eq!(
                    element_id.as_ref().map(ElementId::as_str),
                    Some("0x2a:0000000000000001")
                );
                assert!(locator.is_none());
            }
            TargetActSetFieldTarget::Browser { .. } => {
                panic!("native element_id must not route to browser bridge")
            }
        }
    }

    #[test]
    fn target_act_set_field_plain_dom_element_id_routes_to_selector() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_field",
            "element_id": "compose-body",
            "text": "hello"
        }))
        .expect("set_field plain DOM id params should deserialize");

        match target_act_set_field_target(&params).expect("plain DOM id routes") {
            TargetActSetFieldTarget::Browser {
                selector,
                element_id,
            } => {
                assert_eq!(selector.as_deref(), Some("[id=\"compose-body\"]"));
                assert!(element_id.is_none());
            }
            TargetActSetFieldTarget::Native { .. } => {
                panic!("plain DOM id must not route to native/UIA")
            }
        }
    }

    #[test]
    fn target_act_set_field_rejects_mixed_locators() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_field",
            "selector": "textarea",
            "element_id": "chrome-tab:589708698:frame:0:path:0.1.1",
            "text": "hello"
        }))
        .expect("set_field mixed params should deserialize");

        let error = target_act_set_field_target(&params).expect_err("mixed locators must fail");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert!(error.message.contains("exactly one"));
    }

    #[test]
    fn target_act_verb_schema_is_forward_compatible_string() {
        let schema = serde_json::to_value(schema_for!(TargetActParams))
            .unwrap_or_else(|error| panic!("target_act params schema should serialize: {error}"));
        let verb_schema = schema
            .pointer("/properties/verb")
            .unwrap_or_else(|| panic!("target_act schema must include verb: {schema}"));

        assert!(
            verb_schema
                .pointer("/type")
                .and_then(Value::as_str)
                .is_some_and(|value| value == "string"),
            "target_act verb schema must be an open string: {verb_schema}"
        );
        assert!(
            verb_schema.pointer("/enum").is_none(),
            "target_act verb schema must not be a closed enum: {verb_schema}"
        );
    }

    #[test]
    fn target_act_unknown_verb_is_runtime_validation_error() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "future_dashboard_action"
        }))
        .expect("future target_act verb should deserialize so clients do not stale on schema");
        let error = target_act_unknown_verb_error(params.verb.as_str());

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert!(
            error.message.contains("future_dashboard_action"),
            "unknown verb error should name the rejected verb: {}",
            error.message
        );
    }

    #[test]
    fn target_act_focus_window_is_forward_compatible_verb() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "focus_window"
        }))
        .expect("focus_window should use the existing open-string target_act schema");

        assert_eq!(params.verb.as_str(), "focus_window");
        assert!(params.path.is_none());
        assert!(params.element_id.is_none());
    }

    #[test]
    fn target_act_save_accepts_file_source_of_truth_params() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "save",
            "path": "C:\\Temp\\issue1275-save.txt",
            "text": "Issue1275 expected persisted text",
            "wait_timeout_ms": 750
        }))
        .expect("save should use the existing open-string target_act schema");

        assert_eq!(params.verb.as_str(), "save");
        assert_eq!(params.path.as_deref(), Some("C:\\Temp\\issue1275-save.txt"));
        assert_eq!(
            params.text.as_deref(),
            Some("Issue1275 expected persisted text")
        );
        assert_eq!(params.wait_timeout_ms, Some(750));
        target_act_validate_save_params(&params).expect("save params should validate");
    }

    #[test]
    fn target_act_save_rejects_locator_command_and_coordinate_mixes() {
        for params in [
            json!({
                "verb": "save",
                "path": "C:\\Temp\\issue1275-save.txt",
                "selector": "#document"
            }),
            json!({
                "verb": "save",
                "path": "C:\\Temp\\issue1275-save.txt",
                "command": "powershell.exe"
            }),
            json!({
                "verb": "save",
                "path": "C:\\Temp\\issue1275-save.txt",
                "x": 12,
                "y": 34
            }),
        ] {
            let params: TargetActParams =
                serde_json::from_value(params).expect("synthetic save params deserialize");
            let error = target_act_validate_save_params(&params)
                .expect_err("save must reject unrelated action parameters");

            assert_eq!(
                target_act_error_code(&error),
                Some(error_codes::TOOL_PARAMS_INVALID)
            );
        }
    }

    #[test]
    fn target_act_save_timeout_is_bounded() {
        assert_eq!(
            target_act_save_verify_timeout(None).expect("default timeout should validate"),
            DEFAULT_TARGET_ACT_SAVE_TIMEOUT_MS
        );
        assert_eq!(
            target_act_save_verify_timeout(Some(50)).expect("lower bound should validate"),
            50
        );
        assert_eq!(
            target_act_save_verify_timeout(Some(10_000)).expect("upper bound should validate"),
            10_000
        );

        for value in [49, 10_001] {
            let error = target_act_save_verify_timeout(Some(value))
                .expect_err("out-of-range save timeout must fail closed");
            assert_eq!(
                target_act_error_code(&error),
                Some(error_codes::TOOL_PARAMS_INVALID)
            );
        }
    }

    #[test]
    fn target_act_save_matches_notepad_title_to_file_source_of_truth() {
        let path = Path::new("C:\\Temp\\issue1275-save.txt");

        assert!(target_act_notepad_title_matches_path(
            "issue1275-save.txt - Notepad",
            path
        ));
        assert!(target_act_notepad_title_matches_path(
            "*issue1275-save.txt - Notepad",
            path
        ));
        assert!(!target_act_notepad_title_matches_path(
            "other.txt - Notepad",
            path
        ));
        assert!(!target_act_notepad_title_matches_path(
            "issue1275-save.txt - WordPad",
            path
        ));
    }

    #[test]
    fn target_act_save_satisfied_requires_expected_bytes_or_file_delta() {
        let before = target_act_test_snapshot(b"old");
        let same = target_act_test_snapshot(b"old");
        let expected = target_act_test_snapshot(b"expected");
        let changed = target_act_test_snapshot(b"changed");

        assert!(
            target_act_save_satisfied(&before, &expected, Some("expected")),
            "expected text should accept exact file bytes even if delta semantics are separate"
        );
        assert!(
            !target_act_save_satisfied(&before, &changed, Some("expected")),
            "wrong file bytes must not satisfy an expected-text save"
        );
        assert!(
            !target_act_save_satisfied(&before, &same, None),
            "without expected text, unchanged file bytes are not enough"
        );
        assert!(
            target_act_save_satisfied(&before, &changed, None),
            "without expected text, any file-byte delta verifies the save side effect"
        );
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_accepts_existing_schema_fields() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "cleanup_notepad_tabs",
            "path": "C:\\Temp\\issue1276-keep.txt",
            "value": "discard_modified",
            "wait_timeout_ms": 750
        }))
        .expect(
            "cleanup_notepad_tabs params should deserialize through the open target_act schema",
        );

        assert_eq!(params.verb.as_str(), "cleanup_notepad_tabs");
        assert_eq!(params.path.as_deref(), Some("C:\\Temp\\issue1276-keep.txt"));
        assert_eq!(params.value.as_deref(), Some("discard_modified"));
        assert_eq!(params.wait_timeout_ms, Some(750));
        target_act_validate_cleanup_notepad_tabs_params(&params)
            .expect("cleanup_notepad_tabs should accept path/value/timeout only");
        assert_eq!(
            target_act_modified_stale_policy(params.value.as_deref())
                .expect("discard policy should validate"),
            TargetActModifiedStalePolicy::DiscardModified
        );
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_rejects_unrelated_action_params() {
        for params in [
            json!({
                "verb": "cleanup_notepad_tabs",
                "path": "C:\\Temp\\issue1276-keep.txt",
                "element_id": "0x2a:0000000000000001"
            }),
            json!({
                "verb": "cleanup_notepad_tabs",
                "path": "C:\\Temp\\issue1276-keep.txt",
                "command": "powershell.exe"
            }),
            json!({
                "verb": "cleanup_notepad_tabs",
                "path": "C:\\Temp\\issue1276-keep.txt",
                "x": 12,
                "y": 34
            }),
        ] {
            let params: TargetActParams = serde_json::from_value(params)
                .expect("synthetic cleanup_notepad_tabs params deserialize");
            let error = target_act_validate_cleanup_notepad_tabs_params(&params)
                .expect_err("cleanup_notepad_tabs must reject unrelated parameters");

            assert_eq!(
                target_act_error_code(&error),
                Some(error_codes::TOOL_PARAMS_INVALID)
            );
        }
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_policy_is_bounded() {
        assert_eq!(
            target_act_modified_stale_policy(None).expect("default policy should validate"),
            TargetActModifiedStalePolicy::DiscardModified
        );
        assert_eq!(
            target_act_modified_stale_policy(Some("refuse_modified"))
                .expect("refuse policy should validate"),
            TargetActModifiedStalePolicy::RefuseModified
        );
        let error = target_act_modified_stale_policy(Some("save_modified"))
            .expect_err("unknown policy must fail closed");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_parses_keep_and_stale_tabs() {
        let nodes = vec![
            target_act_test_accessible_node(
                1,
                "issue1276-keep.txt. Unmodified.",
                "tab item",
                &[UiaPattern::SelectionItem],
            ),
            target_act_test_accessible_node(
                2,
                "old-agent-tab.txt. Modified.",
                "TabItem",
                &[UiaPattern::SelectionItem],
            ),
            target_act_test_accessible_node(3, "Close Tab", "Button", &[UiaPattern::Invoke]),
        ];
        let tabs = target_act_notepad_tabs_from_nodes(&nodes, "issue1276-keep.txt");

        assert_eq!(tabs.len(), 2);
        assert!(tabs[0].keep);
        assert!(!tabs[0].modified);
        assert!(!tabs[1].keep);
        assert!(tabs[1].modified);
        assert_eq!(target_act_stale_notepad_tab_count(&tabs), 1);
        target_act_validate_notepad_keep_tab(&tabs, "issue1276-keep.txt")
            .expect("single keep tab should validate");
        assert!(
            target_act_notepad_close_tab_button(&nodes, &tabs[1]).is_some(),
            "Close Tab invoke button should be discoverable"
        );
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_chooses_nearest_close_tab_button() {
        let mut stale = target_act_test_accessible_node(
            10,
            "old-agent-tab.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        stale.bbox = Rect {
            x: 100,
            y: 20,
            w: 120,
            h: 30,
        };
        let mut far_close =
            target_act_test_accessible_node(11, "Close Tab", "Button", &[UiaPattern::Invoke]);
        far_close.bbox = Rect {
            x: 900,
            y: 20,
            w: 28,
            h: 30,
        };
        let mut near_close =
            target_act_test_accessible_node(12, "Close Tab", "Button", &[UiaPattern::Invoke]);
        near_close.bbox = Rect {
            x: 205,
            y: 20,
            w: 28,
            h: 30,
        };
        let nodes = vec![far_close.clone(), stale, near_close.clone()];
        let tabs = target_act_notepad_tabs_from_nodes(&nodes, "issue1276-keep.txt");

        let close_button = target_act_notepad_close_tab_button(&nodes, &tabs[0])
            .expect("nearest close tab button should resolve");

        assert_eq!(close_button.element_id, near_close.element_id);
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_finds_close_glyph_inside_tab() {
        let mut stale = target_act_test_accessible_node(
            10,
            "old-agent-tab.txt. Modified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        stale.bbox = Rect {
            x: 100,
            y: 20,
            w: 150,
            h: 48,
        };
        let mut title =
            target_act_test_accessible_node(11, "old-agent-tab.txt", "Text", &[UiaPattern::Text]);
        title.bbox = Rect {
            x: 112,
            y: 31,
            w: 90,
            h: 23,
        };
        let mut glyph = target_act_test_accessible_node(12, "x", "Text", &[UiaPattern::Text]);
        glyph.bbox = Rect {
            x: 214,
            y: 34,
            w: 18,
            h: 18,
        };
        let nodes = vec![stale.clone(), title, glyph.clone()];
        let tabs = target_act_notepad_tabs_from_nodes(&nodes, "issue1276-keep.txt");

        let close_glyph = target_act_notepad_close_tab_glyph(&nodes, &tabs[0])
            .expect("small right-side text glyph inside selected tab should be close target");

        assert_eq!(close_glyph.element_id, glyph.element_id);
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_finds_file_close_tab_menu_item() {
        let file_menu =
            target_act_test_accessible_node(10, "File", "MenuItem", &[UiaPattern::ExpandCollapse]);
        let close_window =
            target_act_test_accessible_node(11, "Close", "Button", &[UiaPattern::Invoke]);
        let close_tab = target_act_test_accessible_node(
            12,
            "Close tab Ctrl+W",
            "MenuItem",
            &[UiaPattern::Invoke],
        );
        let nodes = vec![close_window, file_menu.clone(), close_tab.clone()];

        let found_file = target_act_notepad_file_menu_item(&nodes)
            .expect("File menu item should resolve by name and ExpandCollapse");
        let found_close_tab = target_act_notepad_close_tab_menu_item(&nodes)
            .expect("Close tab menu item should resolve by name and InvokePattern");

        assert_eq!(found_file.element_id, file_menu.element_id);
        assert_eq!(found_close_tab.element_id, close_tab.element_id);
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_prefers_visible_stale_tab() {
        let mut offscreen = target_act_test_accessible_node(
            10,
            "offscreen-old.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        offscreen.bbox = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let mut visible = target_act_test_accessible_node(
            11,
            "visible-old.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        visible.bbox = Rect {
            x: 200,
            y: 20,
            w: 120,
            h: 48,
        };
        let keep = target_act_test_accessible_node(
            12,
            "issue1276-keep.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        let tabs =
            target_act_notepad_tabs_from_nodes(&[offscreen, keep, visible], "issue1276-keep.txt");

        let next = target_act_next_stale_notepad_tab(&tabs)
            .expect("visible stale tab should be selected first");

        assert_eq!(next.document_name, "visible-old.txt");
        assert!(target_act_rect_has_area(next.bbox));
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_refuse_policy_detects_any_modified_stale_tab() {
        let keep = target_act_test_accessible_node(
            10,
            "issue1276-keep.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        let mut visible_unmodified = target_act_test_accessible_node(
            11,
            "visible-old.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        visible_unmodified.bbox = Rect {
            x: 200,
            y: 20,
            w: 120,
            h: 48,
        };
        let mut offscreen_modified = target_act_test_accessible_node(
            12,
            "offscreen-old.txt. Modified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        offscreen_modified.bbox = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let tabs = target_act_notepad_tabs_from_nodes(
            &[keep, visible_unmodified, offscreen_modified],
            "issue1276-keep.txt",
        );

        let next = target_act_next_stale_notepad_tab(&tabs)
            .expect("visible unmodified tab should be selected for discard policy");
        let modified = target_act_first_modified_stale_notepad_tab(&tabs)
            .expect("refuse policy must detect modified stale tab before closing any stale tab");

        assert_eq!(next.document_name, "visible-old.txt");
        assert_eq!(modified.document_name, "offscreen-old.txt");
        assert!(modified.modified);
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_matches_refreshed_tab_by_document() {
        let mut original = target_act_test_accessible_node(
            10,
            "old-agent-tab.txt. Modified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        original.bbox = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let original_tabs = target_act_notepad_tabs_from_nodes(&[original], "issue1276-keep.txt");
        let mut refreshed = target_act_test_accessible_node(
            44,
            "old-agent-tab.txt. Modified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        );
        refreshed.bbox = Rect {
            x: 350,
            y: 242,
            w: 126,
            h: 48,
        };
        let refreshed_tabs = target_act_notepad_tabs_from_nodes(&[refreshed], "issue1276-keep.txt");

        let matched = target_act_matching_notepad_tab(&refreshed_tabs, &original_tabs[0])
            .expect("refreshed tab should match by document name and modified state");

        assert_eq!(matched.document_name, "old-agent-tab.txt");
        assert!(target_act_rect_has_area(matched.bbox));
    }

    #[test]
    fn target_act_cleanup_notepad_tabs_requires_one_keep_tab_when_tabs_exist() {
        let nodes = vec![
            target_act_test_accessible_node(
                1,
                "one.txt. Unmodified.",
                "TabItem",
                &[UiaPattern::SelectionItem],
            ),
            target_act_test_accessible_node(
                2,
                "two.txt. Unmodified.",
                "TabItem",
                &[UiaPattern::SelectionItem],
            ),
        ];
        let tabs = target_act_notepad_tabs_from_nodes(&nodes, "missing.txt");
        let error = target_act_validate_notepad_keep_tab(&tabs, "missing.txt")
            .expect_err("missing keep tab must fail closed");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::ACTION_TARGET_INVALID)
        );
    }

    #[test]
    fn target_act_focus_window_uses_window_session_target() {
        let params =
            target_act_focus_window_params(Some(&SessionTarget::Window { hwnd: 0x250a08 }))
                .expect("window target should produce focus params");

        assert_eq!(params.hwnd, Some(0x250a08));
        assert!(params.title_regex.is_none());
        assert!(params.pid.is_none());
        assert_eq!(params.stable_ms, DEFAULT_TARGET_ACT_FOCUS_STABLE_MS);
    }

    #[test]
    fn target_act_focus_window_uses_cdp_parent_window() {
        let params = target_act_focus_window_params(Some(&SessionTarget::Cdp {
            window_hwnd: 0x250a08,
            cdp_target_id: "chrome-tab:42".to_owned(),
        }))
        .expect("cdp target should focus its containing browser HWND");

        assert_eq!(params.hwnd, Some(0x250a08));
        assert!(params.title_regex.is_none());
        assert!(params.pid.is_none());
    }

    #[test]
    fn target_act_focus_window_requires_session_target() {
        let error = target_act_focus_window_params(None).expect_err("missing target should refuse");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TARGET_NOT_SET)
        );
        assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
    }

    #[test]
    fn target_act_read_routes_cdp_targets_to_target_info() {
        let target = SessionTarget::Cdp {
            window_hwnd: 0x1234,
            cdp_target_id: "chrome-tab:42".to_owned(),
        };

        assert_eq!(
            target_act_read_delegated_tool(Some(&target))
                .expect("cdp target routes to target info"),
            "cdp_target_info"
        );
    }

    #[test]
    fn target_act_read_routes_window_targets_to_observe() {
        let target = SessionTarget::Window { hwnd: 0x1234 };

        assert_eq!(
            target_act_read_delegated_tool(Some(&target)).expect("window target routes to observe"),
            "observe"
        );
    }

    #[test]
    fn target_act_read_without_target_refuses_foreground_fallback() {
        let error =
            target_act_read_delegated_tool(None).expect_err("missing target should fail closed");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TARGET_NOT_SET)
        );
        assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
    }

    #[test]
    fn target_act_click_count_rejects_out_of_range() {
        let error =
            target_act_click_count_for_action("click", Some(4)).expect_err("clicks=4 should fail");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_dblclick_defaults_and_rejects_wrong_count() {
        assert_eq!(
            target_act_click_count_for_action("dblclick", None).expect("default dblclick"),
            2
        );
        assert_eq!(
            target_act_click_count_for_action("dblclick", Some(2)).expect("explicit dblclick"),
            2
        );
        let error = target_act_click_count_for_action("dblclick", Some(1))
            .expect_err("dblclick clickCount=1 should fail");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_coordinate_click_deserializes() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "x": 42,
            "y": 77,
            "coordinate_space": "viewport",
            "clicks": 3
        }))
        .expect("coordinate click params should deserialize");
        let coordinate = target_act_coordinate(&params)
            .expect("coordinate should validate")
            .expect("coordinate should be present");

        assert_eq!(params.verb.as_str(), "click");
        assert_eq!(coordinate.x, 42);
        assert_eq!(coordinate.y, 77);
        assert_eq!(coordinate.space, TargetActCoordinateSpace::Viewport);
        assert_eq!(coordinate.space.as_bridge_str(), "viewport");
        assert_eq!(
            target_act_click_count_for_action("click", params.clicks).unwrap(),
            3
        );
    }

    #[test]
    fn target_act_coordinate_defaults_to_screen_space() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "x": 12,
            "y": 34
        }))
        .expect("coordinate click params should deserialize");
        let coordinate = target_act_coordinate(&params)
            .expect("coordinate should validate")
            .expect("coordinate should be present");

        assert_eq!(coordinate.space, TargetActCoordinateSpace::Screen);
        assert_eq!(coordinate.space.as_bridge_str(), "screen");
    }

    #[test]
    fn target_act_coordinate_requires_x_y_pair() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "x": 42
        }))
        .expect("partial coordinate params should deserialize");
        let error = target_act_coordinate(&params).expect_err("missing y must fail closed");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_coordinate_space_requires_coordinates() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "coordinate_space": "viewport"
        }))
        .expect("coordinate-space-only params should deserialize");
        let error = target_act_coordinate(&params)
            .expect_err("coordinate_space without coordinates must fail closed");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_coordinate_rejects_locator_mix_before_routing() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "selector": "#submit",
            "x": 42,
            "y": 77
        }))
        .expect("mixed coordinate and selector params should deserialize");

        assert!(
            target_act_coordinate(&params)
                .expect("coordinate pair should validate")
                .is_some()
        );
        assert!(
            target_act_has_any_locator(&params),
            "mixed selector/coordinate input must be detected before routing"
        );
    }

    #[test]
    fn target_act_tap_viewport_coordinate_deserializes() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "tap",
            "x": 52,
            "y": 191,
            "coordinate_space": "viewport"
        }))
        .expect("tap coordinate params should deserialize");
        let coordinate = target_act_coordinate(&params)
            .expect("coordinate should validate")
            .expect("coordinate should be present");

        assert_eq!(params.verb.as_str(), "tap");
        assert_eq!(coordinate.x, 52);
        assert_eq!(coordinate.y, 191);
        assert_eq!(coordinate.space, TargetActCoordinateSpace::Viewport);
    }

    #[test]
    fn target_act_bridge_cdp_input_accepts_bridge_element_id() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "hover",
            "element_id": "chrome-tab:589708699:frame:0:path:0.1.1",
            "auto_wait": true
        }))
        .expect("bridge cdp input params should deserialize");

        target_act_validate_bridge_cdp_input("hover", None, &params)
            .expect("chrome-tab bridge element id should be valid for cdpInput");
    }

    #[test]
    fn target_act_bridge_cdp_input_rejects_raw_cdp_element_id() {
        let raw_cdp_id = synapse_a11y::cdp_element_id(0x2a, 42).to_string();
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "hover",
            "element_id": raw_cdp_id
        }))
        .expect("raw cdp params should deserialize");

        let error = target_act_validate_bridge_cdp_input("hover", None, &params)
            .expect_err("raw cdp element id should require a raw endpoint");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::ACTION_TARGET_INVALID)
        );
    }

    #[test]
    fn target_act_tap_dom_id_selector_escapes_css_string() {
        let selector = target_act_dom_id_selector(r#"apply"now\button"#)
            .expect("visible DOM id should become a selector");

        assert_eq!(selector, r#"[id="apply\"now\\button"]"#);
    }

    #[test]
    fn target_act_hover_params_deserialize() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "hover",
            "role": "button",
            "name": "Account menu"
        }))
        .expect("hover params should deserialize");

        assert_eq!(params.verb.as_str(), "hover");
        assert_eq!(params.role.as_deref(), Some("button"));
        assert_eq!(params.name.as_deref(), Some("Account menu"));
        assert!(
            target_act_coordinate(&params)
                .expect("hover should not contain coordinates")
                .is_none()
        );
    }

    #[test]
    fn target_act_dom_verbs_deserialize_and_validate() {
        let click_with_options: TargetActParams = serde_json::from_value(json!({
            "verb": "click",
            "selector": "#canvas",
            "clickCount": 2,
            "button": "right",
            "modifiers": ["Shift", "control", "meta"],
            "position": { "x": 12, "y": 8 }
        }))
        .expect("click options params should deserialize");
        assert_eq!(click_with_options.verb.as_str(), "click");
        assert_eq!(click_with_options.clicks, Some(2));
        assert_eq!(click_with_options.button, Some(TargetActMouseButton::Right));
        assert_eq!(
            click_with_options.modifiers,
            vec![
                TargetActClickModifier::Shift,
                TargetActClickModifier::Ctrl,
                TargetActClickModifier::Meta
            ]
        );
        assert_eq!(
            target_act_click_position(&click_with_options).expect("click position"),
            Some((12, 8))
        );
        target_act_validate_dom_locator("click", &click_with_options)
            .expect("click options locator should validate");

        let dblclick: TargetActParams = serde_json::from_value(json!({
            "verb": "dblclick",
            "selector": "#apply",
            "offsetX": 3,
            "offsetY": 4
        }))
        .expect("dblclick params should deserialize");
        assert_eq!(dblclick.verb.as_str(), "dblclick");
        assert_eq!(
            target_act_dom_click_count("dblclick", dblclick.clicks).expect("dblclick count"),
            Some(2)
        );
        assert_eq!(
            target_act_click_position(&dblclick).expect("dblclick position"),
            Some((3, 4))
        );
        target_act_validate_dom_locator("dblclick", &dblclick)
            .expect("dblclick locator should validate");

        let press: TargetActParams = serde_json::from_value(json!({
            "verb": "press",
            "role": "button",
            "name": "Create token"
        }))
        .expect("press params should deserialize");
        assert_eq!(press.verb.as_str(), "press");
        target_act_validate_dom_locator("press", &press).expect("press locator should validate");

        let select: TargetActParams = serde_json::from_value(json!({
            "verb": "select",
            "selector": "#scope",
            "option": "Workers KV Storage"
        }))
        .expect("select params should deserialize");
        assert_eq!(select.verb.as_str(), "select");
        target_act_validate_dom_locator("select", &select).expect("select locator should validate");

        let select_by_label: TargetActParams = serde_json::from_value(json!({
            "verb": "select",
            "selector": "#scope",
            "option_label": "Workers KV Storage"
        }))
        .expect("select by label params should deserialize");
        assert_eq!(
            select_by_label.option_label.as_deref(),
            Some("Workers KV Storage")
        );
        target_act_validate_dom_locator("select", &select_by_label)
            .expect("select label should validate");

        let select_by_index: TargetActParams = serde_json::from_value(json!({
            "verb": "select",
            "selector": "#scope",
            "option_index": 2
        }))
        .expect("select by index params should deserialize");
        assert_eq!(select_by_index.option_index, Some(2));
        target_act_validate_dom_locator("select", &select_by_index)
            .expect("select index should validate");

        let select_many: TargetActParams = serde_json::from_value(json!({
            "verb": "select",
            "selector": "#scope",
            "options": [
                { "value": "read" },
                { "label": "Write" },
                { "index": 3 }
            ]
        }))
        .expect("multi-select params should deserialize");
        assert_eq!(select_many.options.len(), 3);
        target_act_validate_dom_locator("select", &select_many)
            .expect("multi-select options should validate");

        let bad_select_option: TargetActParams = serde_json::from_value(json!({
            "verb": "select",
            "selector": "#scope",
            "options": [
                { "value": "read", "label": "Read" }
            ]
        }))
        .expect("bad select option shape should deserialize");
        let error = target_act_validate_dom_locator("select", &bad_select_option)
            .expect_err("ambiguous select option spec should be rejected");
        assert!(
            error
                .message
                .contains("exactly one of value, label, or index"),
            "select validation should reject ambiguous option specs: {error:?}"
        );

        let submit: TargetActParams = serde_json::from_value(json!({
            "verb": "submit",
            "selector": "form#token"
        }))
        .expect("submit params should deserialize");
        assert_eq!(submit.verb.as_str(), "submit");
        target_act_validate_dom_locator("submit", &submit).expect("submit locator should validate");

        let dispatch_event: TargetActParams = serde_json::from_value(json!({
            "verb": "dispatch_event",
            "selector": "#token",
            "event_type": "synapse-ready",
            "event_init": {
                "bubbles": true,
                "cancelable": true,
                "detail": {
                    "ok": true
                }
            }
        }))
        .expect("dispatch_event params should deserialize");
        assert_eq!(dispatch_event.verb.as_str(), "dispatch_event");
        assert_eq!(dispatch_event.event_type.as_deref(), Some("synapse-ready"));
        assert_eq!(
            dispatch_event
                .event_init
                .as_ref()
                .and_then(|value| value.get("detail"))
                .and_then(|value| value.get("ok"))
                .and_then(Value::as_bool),
            Some(true)
        );
        target_act_validate_dom_locator("dispatch_event", &dispatch_event)
            .expect("dispatch_event locator should validate");

        for verb in ["check", "uncheck"] {
            let params: TargetActParams = serde_json::from_value(json!({
                "verb": verb,
                "role": "checkbox",
                "name": "Accept terms"
            }))
            .expect("check state params should deserialize");
            assert_eq!(params.verb.as_str(), verb);
            target_act_validate_dom_locator(verb, &params)
                .expect("check state locator should validate");
        }

        for verb in ["clear", "focus", "blur", "select_text", "selectText"] {
            let params: TargetActParams = serde_json::from_value(json!({
                "verb": verb,
                "selector": "#token"
            }))
            .expect("primitive params should deserialize");
            let expected = if verb == "selectText" {
                "selecttext"
            } else {
                verb
            };
            assert_eq!(params.verb.as_str(), expected);
            let action = if verb == "selectText" {
                "select_text"
            } else {
                verb
            };
            target_act_validate_dom_primitive_params(action, &params)
                .expect("primitive locator should validate");
        }

        let clear_with_text: TargetActParams = serde_json::from_value(json!({
            "verb": "clear",
            "selector": "#token",
            "text": "ignored"
        }))
        .expect("clear params should deserialize");
        let error = target_act_validate_dom_primitive_params("clear", &clear_with_text)
            .expect_err("clear should reject unused text");
        assert!(
            error.message.contains("does not accept text"),
            "clear validation should reject ignored text: {error:?}"
        );
    }

    #[test]
    fn target_act_key_chord_deserializes_and_constructs_press_request() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "key",
            "key": "Ctrl+End",
            "wait_timeout_ms": 750
        }))
        .expect("key params should deserialize");

        let keys = target_act_key_chord_keys(&params, "key").expect("key chord should parse");
        assert_eq!(keys, vec!["Ctrl", "End"]);
        let press =
            target_act_press_params(keys, params.wait_timeout_ms, "key").expect("press params");
        assert_eq!(press.keys, vec!["Ctrl", "End"]);
        assert!(press.verify_delta);
        assert_eq!(press.verify_timeout_ms, 750);
    }

    #[test]
    fn target_act_key_chord_rejects_key_and_keys_together() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "key",
            "key": "Ctrl+Z",
            "keys": ["ctrl", "z"]
        }))
        .expect("key params should deserialize");

        let error = target_act_key_chord_keys(&params, "key")
            .expect_err("key and keys together should fail closed");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_insert_and_append_params_deserialize() {
        let insert: TargetActParams = serde_json::from_value(json!({
            "verb": "insert_text",
            "element_id": "0x2a:0000000000000001",
            "text": "insert me"
        }))
        .expect("insert_text params should deserialize");
        assert_eq!(insert.verb.as_str(), "insert_text");
        assert_eq!(insert.text.as_deref(), Some("insert me"));

        let append: TargetActParams = serde_json::from_value(json!({
            "verb": "append_text",
            "text": "append me"
        }))
        .expect("append_text current-focus params should deserialize");
        assert_eq!(append.verb.as_str(), "append_text");
        assert!(!target_act_has_any_locator(&append));
    }

    #[test]
    fn target_act_insert_native_element_id_routes_to_native_text() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "insert_text",
            "element_id": "0x2a:0000000000000001",
            "text": "insert me"
        }))
        .expect("insert_text params should deserialize");

        let element_id = target_act_native_text_element_id(&params, "insert_text")
            .expect("native element routing should validate")
            .expect("native element id should use native text route");

        assert_eq!(element_id.as_str(), "0x2a:0000000000000001");
    }

    #[test]
    fn target_act_insert_cdp_element_id_uses_existing_focus_type_path() {
        let cdp_id = synapse_a11y::cdp_element_id(0x2a, 42);
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "insert_text",
            "element_id": cdp_id.to_string(),
            "text": "insert me"
        }))
        .expect("insert_text params should deserialize");

        assert!(
            target_act_native_text_element_id(&params, "insert_text")
                .expect("cdp element routing should validate")
                .is_none()
        );
    }

    #[test]
    fn target_act_insert_native_element_id_rejects_dom_locator_mix() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "insert_text",
            "element_id": "0x2a:0000000000000001",
            "selector": "#editor",
            "text": "insert me"
        }))
        .expect("insert_text params should deserialize");

        let error = target_act_native_text_element_id(&params, "insert_text")
            .expect_err("native element id plus selector must fail closed");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_set_selection_params_deserialize_with_aliases() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_selection",
            "element_id": "0x2a:0000000000000001",
            "start": 3,
            "end": 8
        }))
        .expect("set_selection params should deserialize");

        assert_eq!(params.verb.as_str(), "set_selection");
        assert_eq!(target_act_selection_range(&params).expect("range"), (3, 8));
    }

    #[test]
    fn target_act_set_selection_rejects_reversed_range() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "set_selection",
            "element_id": "0x2a:0000000000000001",
            "selection_start": 9,
            "selection_end": 2
        }))
        .expect("set_selection params should deserialize");

        let error = target_act_selection_range(&params)
            .expect_err("set_selection must reject end before start");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_select_requires_option_or_value() {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": "select",
            "selector": "#scope"
        }))
        .expect("synthetic select params should deserialize");
        let error = target_act_validate_dom_locator("select", &params)
            .expect_err("select must require an option/value");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_type_params_constructs_act_type_request() {
        let params = target_act_type_params("issue-1267".to_owned(), Some(750))
            .expect("target_act type params should construct act_type params");

        assert_eq!(params.text, "issue-1267");
        assert_eq!(params.verify_timeout_ms, 750);
        assert!(params.verify_delta);
        assert!(params.into_element.is_none());
    }

    #[test]
    fn target_act_type_wait_timeout_is_bounded() {
        let error = target_act_type_params("issue-1267".to_owned(), Some(30_000))
            .expect_err("type wait timeout must be bounded");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_rect_contains_point_uses_exclusive_bottom_right() {
        let rect = Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };

        assert!(target_act_rect_contains_point(rect, Point { x: 10, y: 20 }));
        assert!(target_act_rect_contains_point(rect, Point { x: 39, y: 59 }));
        assert!(!target_act_rect_contains_point(
            rect,
            Point { x: 40, y: 59 }
        ));
        assert!(!target_act_rect_contains_point(
            rect,
            Point { x: 39, y: 60 }
        ));
    }

    #[test]
    fn target_act_click_plain_element_id_routes_to_dom() {
        let routed = target_act_legacy_click_element_id("create-token-button")
            .expect("plain page id should be accepted as DOM id");

        assert!(
            routed.is_none(),
            "plain page element ids should route through the browser DOM bridge"
        );
    }

    #[test]
    fn target_act_click_bridge_element_id_routes_to_dom() {
        let routed =
            target_act_legacy_click_element_id("chrome-tab:589708698:frame:4970:path:0.1.1")
                .expect("normal bridge element id should be accepted as a DOM id");

        assert!(
            routed.is_none(),
            "normal bridge element ids must route through chrome_debugger_bridge.domAction"
        );
    }

    #[test]
    fn target_act_click_native_shaped_element_id_stays_legacy() {
        let routed = target_act_legacy_click_element_id("0x2a:0000000000000001")
            .expect("valid native/UIA id should parse");

        assert_eq!(
            routed
                .expect("native/UIA id should stay on legacy click path")
                .as_str(),
            "0x2a:0000000000000001"
        );
    }

    #[test]
    fn target_act_click_malformed_native_id_fails_closed() {
        let error = target_act_legacy_click_element_id("0xnotvalid:bad")
            .expect_err("malformed native-looking id should not fall back to DOM");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn target_act_cdp_target_match_accepts_owned_root_case_insensitive() {
        assert!(target_act_cdp_target_matches_session_or_frame(
            "ABC123",
            "abc123",
            &[]
        ));
    }

    #[test]
    fn target_act_cdp_target_match_accepts_owned_oopif_child() {
        let frames = vec![
            target_act_test_frame_entry("main-frame", None, "root-target", 0, false),
            target_act_test_frame_entry(
                "child-frame",
                Some("main-frame"),
                "iframe-target",
                1,
                true,
            ),
        ];

        assert!(target_act_cdp_target_matches_session_or_frame(
            "root-target",
            "IFRAME-TARGET",
            &frames
        ));
    }

    #[test]
    fn target_act_cdp_target_match_rejects_unrelated_or_stale_child() {
        let frames = vec![
            target_act_test_frame_entry("main-frame", None, "root-target", 0, false),
            target_act_test_frame_entry(
                "child-frame",
                Some("main-frame"),
                "iframe-target",
                1,
                true,
            ),
        ];

        assert!(!target_act_cdp_target_matches_session_or_frame(
            "root-target",
            "stale-frame-target",
            &frames
        ));
    }

    #[test]
    fn target_act_dom_error_codes_classify() {
        for code in [
            error_codes::CHROME_DOM_ACTION_UNSUPPORTED,
            error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS,
            error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE,
            error_codes::CHROME_DOM_ELEMENT_NOT_FOUND,
            error_codes::CHROME_DOM_SELECTOR_INVALID,
        ] {
            let error = mcp_error(code, "synthetic DOM refusal");
            assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
        }

        let postcondition = mcp_error(
            error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED,
            "synthetic DOM readback mismatch",
        );
        assert_eq!(
            target_act_error_status(&postcondition),
            TARGET_ACT_STATUS_VERIFY_NEEDED
        );
    }

    #[test]
    fn target_act_errors_classify_verify_needed() {
        for code in [
            error_codes::ACTION_NO_OBSERVED_DELTA,
            error_codes::ACTION_FOREGROUND_LOST,
            error_codes::ACTION_POSTCONDITION_FAILED,
            error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE,
        ] {
            let error = mcp_error(code, "synthetic delegated postcondition failure");
            assert_eq!(
                target_act_error_status(&error),
                TARGET_ACT_STATUS_VERIFY_NEEDED
            );
        }
    }

    #[test]
    fn target_act_errors_classify_refusal() {
        for code in [
            error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
            error_codes::ACTION_ELEMENT_VALUE_READ_ONLY,
            error_codes::ACTION_FOREGROUND_LEASE_BUSY,
            error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
            error_codes::FOREGROUND_ACTIVATION_REFUSED,
            error_codes::TARGET_CLAIM_NOT_FOUND,
            error_codes::TARGET_NOT_SET,
        ] {
            let error = mcp_error(code, "synthetic delegated refusal");
            assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
        }
    }

    #[test]
    fn target_act_error_result_preserves_delegated_data() {
        let error = mcp_error(error_codes::ACTION_POSTCONDITION_FAILED, "mismatch");
        let result = target_act_error_result("act_set_field_text", error);

        assert_eq!(
            result.pointer("/error/code").and_then(Value::as_str),
            Some(error_codes::ACTION_POSTCONDITION_FAILED)
        );
        assert_eq!(
            result
                .pointer("/error/delegated_tool")
                .and_then(Value::as_str),
            Some("act_set_field_text")
        );
        assert_eq!(
            result.pointer("/error/data/code").and_then(Value::as_str),
            Some(error_codes::ACTION_POSTCONDITION_FAILED)
        );
    }

    fn target_act_test_snapshot(bytes: &[u8]) -> TargetActFileSnapshot {
        TargetActFileSnapshot {
            len: u64::try_from(bytes.len()).expect("synthetic bytes length should fit u64"),
            sha256: crate::m2::postcondition::hex_encode(&Sha256::digest(bytes)),
            bytes: bytes.to_vec(),
        }
    }

    fn target_act_test_accessible_node(
        sequence: u32,
        name: &str,
        role: &str,
        patterns: &[UiaPattern],
    ) -> AccessibleNode {
        AccessibleNode {
            element_id: synapse_core::element_id(0x2a, &format!("0000002a{sequence:08x}")),
            parent: None,
            name: name.to_owned(),
            role: role.to_owned(),
            automation_id: None,
            value: None,
            bbox: Rect {
                x: i32::try_from(sequence).unwrap_or(0) * 10,
                y: 20,
                w: 100,
                h: 30,
            },
            enabled: true,
            focused: false,
            patterns: patterns.to_vec(),
            children_count: 0,
            depth: 1,
        }
    }

    fn target_act_test_frame_entry(
        frame_id: &str,
        parent_frame_id: Option<&str>,
        cdp_target_id: &str,
        depth: u32,
        is_out_of_process: bool,
    ) -> synapse_a11y::CdpFrameTreeEntry {
        synapse_a11y::CdpFrameTreeEntry {
            frame_id: frame_id.to_owned(),
            parent_frame_id: parent_frame_id.map(ToOwned::to_owned),
            cdp_target_id: cdp_target_id.to_owned(),
            target_type: if is_out_of_process { "iframe" } else { "page" }.to_owned(),
            target_attached: Some(true),
            url: format!("https://example.test/{frame_id}"),
            name: None,
            origin: "https://example.test".to_owned(),
            security_origin: Some("https://example.test".to_owned()),
            loader_id: Some(format!("loader-{frame_id}")),
            depth,
            sibling_index: 0,
            child_count: 0,
            is_out_of_process,
            frame_element_id: None,
            frame_element_backend_node_id: None,
            frame_element_cdp_target_id: None,
            frame_element_source: if parent_frame_id.is_some() {
                "DOM.Node.frameId".to_owned()
            } else {
                "main_frame".to_owned()
            },
        }
    }
}
