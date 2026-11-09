use std::time::{Duration, Instant};

use regex::Regex;
use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{ForegroundContext, error_codes};

use crate::{
    m1::mcp_error,
    m2::postcondition::{ActPostcondition, default_verify_timeout_ms, hash_json},
};

const TOOL: &str = "act_focus_window";
const METHOD_SET_FOREGROUND: &str = "win32_set_foreground_window";
const METHOD_ALREADY_FOREGROUND: &str = "already_foreground";
const SOURCE_OF_TRUTH: &str = "win32_foreground_window";
const FOCUS_WINDOW_POLL_MS: u64 = 25;
const DEFAULT_FOCUS_STABLE_MS: u32 = 75;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActFocusWindowParams {
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub hwnd: Option<i64>,
    #[serde(default)]
    pub title_regex: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
    #[serde(default = "default_focus_stable_ms")]
    #[schemars(default = "default_focus_stable_ms", range(min = 0, max = 1000))]
    pub stable_ms: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActFocusWindowResponse {
    pub ok: bool,
    pub method: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
    pub target: FocusWindowTargetReadback,
    pub before_foreground: ForegroundContext,
    pub matched_window: ForegroundContext,
    pub after_foreground: ForegroundContext,
    pub changed: bool,
    pub focus_attempts: u32,
    pub postcondition: ActPostcondition,
    pub elapsed_ms: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusWindowTargetReadback {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
struct WindowSummary {
    hwnd: i64,
    pid: u32,
    process_name: String,
    window_title: String,
}

#[derive(Clone, Debug)]
enum RequestedFocusTarget {
    Hwnd(i64),
    TitleRegex { original: String, regex: Regex },
    Pid(u32),
}

struct FocusVerification {
    after: ForegroundContext,
    attempts: u32,
    method: &'static str,
}

struct FocusActivation {
    attempts: u32,
    method: &'static str,
}

const fn default_focus_stable_ms() -> u32 {
    DEFAULT_FOCUS_STABLE_MS
}

pub(crate) async fn act_focus_window_with_boundary(
    params: ActFocusWindowParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActFocusWindowResponse, ErrorData> {
    let started = Instant::now();
    validate_focus_window_params(&params)?;
    let requested = requested_target(&params)?;
    let target_readback = target_readback(&requested);

    let before = synapse_a11y::current_foreground_context()
        .map_err(|error| a11y_error_to_focus_mcp("before_foreground_read", error, None))?;
    let matched = resolve_focus_target(&requested)?;
    let activation = activate_focus_window_once(&before, &matched, boundary)?;
    let verification = verify_focus_window(&params, &matched, &target_readback, activation).await?;
    let before_signature = hash_json(&before)?;
    let after_signature = hash_json(&verification.after)?;
    let changed = before.hwnd != verification.after.hwnd;
    let matched_hwnd = matched.hwnd;

    tracing::info!(
        code = "M2_ACT_FOCUS_WINDOW_READBACK",
        hwnd = matched.hwnd,
        pid = matched.pid,
        title = %matched.window_title,
        before_hwnd = before.hwnd,
        after_hwnd = verification.after.hwnd,
        attempts = verification.attempts,
        stable_ms = params.stable_ms,
        changed,
        source_of_truth = SOURCE_OF_TRUTH,
        "readback=act_focus_window method={} before_hwnd=0x{:x} after_hwnd=0x{:x} target_hwnd=0x{:x} changed={}",
        verification.method,
        before.hwnd,
        verification.after.hwnd,
        matched.hwnd,
        changed
    );

    Ok(ActFocusWindowResponse {
        ok: true,
        method: verification.method.to_owned(),
        backend_tier_used: "foreground".to_owned(),
        required_foreground: true,
        source_of_truth: SOURCE_OF_TRUTH.to_owned(),
        target: target_readback,
        before_foreground: before,
        matched_window: matched,
        after_foreground: verification.after,
        changed,
        focus_attempts: verification.attempts,
        postcondition: postcondition_verified_state(
            before_signature,
            after_signature,
            changed,
            format!(
                "foreground readback matched requested hwnd 0x{:x} and stayed stable for {} ms",
                matched_hwnd, params.stable_ms
            ),
        ),
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

pub fn act_focus_window_request_details(params: &ActFocusWindowParams) -> Value {
    json!({
        "source_of_truth": SOURCE_OF_TRUTH,
        "target": {
            "hwnd": params.hwnd,
            "title_regex": params.title_regex,
            "pid": params.pid,
        },
        "verify_timeout_ms": params.verify_timeout_ms,
        "stable_ms": params.stable_ms,
    })
}

pub fn act_focus_window_target_hwnd(params: &ActFocusWindowParams) -> Result<i64, ErrorData> {
    validate_focus_window_params(params)?;
    let requested = requested_target(params)?;
    Ok(resolve_focus_target(&requested)?.hwnd)
}

fn validate_focus_window_params(params: &ActFocusWindowParams) -> Result<(), ErrorData> {
    if !(50..=5000).contains(&params.verify_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} verify_timeout_ms must be in 50..=5000, got {}",
                params.verify_timeout_ms
            ),
        ));
    }
    if params.stable_ms > 1000 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} stable_ms must be in 0..=1000, got {}",
                params.stable_ms
            ),
        ));
    }
    if params.stable_ms > params.verify_timeout_ms {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} stable_ms ({}) cannot exceed verify_timeout_ms ({})",
                params.stable_ms, params.verify_timeout_ms
            ),
        ));
    }
    Ok(())
}

fn requested_target(params: &ActFocusWindowParams) -> Result<RequestedFocusTarget, ErrorData> {
    let title_regex_present = params
        .title_regex
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let target_count = usize::from(params.hwnd.is_some())
        + usize::from(title_regex_present)
        + usize::from(params.pid.is_some());
    if target_count != 1 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} requires exactly one target: hwnd, title_regex, or pid"),
        ));
    }
    if let Some(hwnd) = params.hwnd {
        let hwnd = crate::m1::validate_hwnd_shape(TOOL, "hwnd", hwnd)?;
        return Ok(RequestedFocusTarget::Hwnd(hwnd));
    }
    if let Some(pid) = params.pid {
        if pid == 0 {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{TOOL} pid must be non-zero"),
            ));
        }
        return Ok(RequestedFocusTarget::Pid(pid));
    }
    let original = params
        .title_regex
        .as_ref()
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let regex = Regex::new(&original).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} title_regex did not compile: {error}"),
        )
    })?;
    Ok(RequestedFocusTarget::TitleRegex { original, regex })
}

fn resolve_focus_target(target: &RequestedFocusTarget) -> Result<ForegroundContext, ErrorData> {
    match target {
        RequestedFocusTarget::Hwnd(hwnd) => resolve_hwnd(*hwnd, target),
        RequestedFocusTarget::Pid(pid) => {
            let windows = visible_windows_for_resolution(target)?;
            let matches = windows
                .iter()
                .filter(|context| context.pid == *pid)
                .cloned()
                .collect::<Vec<_>>();
            single_window_match(target, matches, &windows, "pid")
        }
        RequestedFocusTarget::TitleRegex { regex, .. } => {
            let windows = visible_windows_for_resolution(target)?;
            let matches = windows
                .iter()
                .filter(|context| regex.is_match(&context.window_title))
                .cloned()
                .collect::<Vec<_>>();
            single_window_match(target, matches, &windows, "title_regex")
        }
    }
}

fn resolve_hwnd(hwnd: i64, target: &RequestedFocusTarget) -> Result<ForegroundContext, ErrorData> {
    let visible = synapse_a11y::is_window_visible(hwnd).map_err(|error| {
        window_not_found_error("hwnd_invalid", target, Vec::new(), Some(error.to_string()))
    })?;
    let top_level = synapse_a11y::is_top_level_window(hwnd).map_err(|error| {
        window_not_found_error("hwnd_invalid", target, Vec::new(), Some(error.to_string()))
    })?;
    if !visible {
        return Err(window_not_found_error(
            "hwnd_not_visible",
            target,
            Vec::new(),
            None,
        ));
    }
    if !top_level {
        return Err(window_not_found_error(
            "hwnd_not_top_level",
            target,
            Vec::new(),
            None,
        ));
    }
    synapse_a11y::foreground_context(hwnd)
        .map_err(|error| a11y_error_to_focus_mcp("target_foreground_read", error, None))
}

fn visible_windows_for_resolution(
    target: &RequestedFocusTarget,
) -> Result<Vec<ForegroundContext>, ErrorData> {
    synapse_a11y::visible_top_level_window_contexts().map_err(|error| {
        let detail = error.to_string();
        tracing::error!(
            code = error.code(),
            tool = TOOL,
            target = ?target_readback(target),
            detail = %detail,
            "act_focus_window visible window enumeration failed"
        );
        window_not_found_error(
            "window_enumeration_failed",
            target,
            Vec::new(),
            Some(detail),
        )
    })
}

fn single_window_match(
    target: &RequestedFocusTarget,
    matches: Vec<ForegroundContext>,
    observed_windows: &[ForegroundContext],
    reason_prefix: &'static str,
) -> Result<ForegroundContext, ErrorData> {
    match matches.len() {
        0 => Err(window_not_found_error(
            reason_prefix,
            target,
            window_summaries(observed_windows),
            None,
        )),
        1 => matches.into_iter().next().ok_or_else(|| {
            window_not_found_error(
                reason_prefix,
                target,
                window_summaries(observed_windows),
                None,
            )
        }),
        _ => Err(window_ambiguous_error(target, &matches)),
    }
}

async fn verify_focus_window(
    params: &ActFocusWindowParams,
    matched: &ForegroundContext,
    target: &FocusWindowTargetReadback,
    activation: FocusActivation,
) -> Result<FocusVerification, ErrorData> {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(u64::from(params.verify_timeout_ms));
    let stable_for = Duration::from_millis(u64::from(params.stable_ms));
    let mut stable_since: Option<Instant> = None;
    let mut last_error: Option<String> = None;
    let mut last_foreground: Option<ForegroundContext> = None;

    loop {
        let now = Instant::now();
        match synapse_a11y::current_foreground_context() {
            Ok(context) if context.hwnd == matched.hwnd => {
                let stable_since = *stable_since.get_or_insert(now);
                last_foreground = Some(context.clone());
                if now.duration_since(stable_since) >= stable_for {
                    return Ok(FocusVerification {
                        after: context,
                        attempts: activation.attempts,
                        method: activation.method,
                    });
                }
            }
            Ok(context) => {
                stable_since = None;
                last_error = Some(format!(
                    "foreground readback hwnd 0x{:x} pid {} title {:?}, expected hwnd 0x{:x}",
                    context.hwnd, context.pid, context.window_title, matched.hwnd
                ));
                last_foreground = Some(context);
            }
            Err(error) => {
                stable_since = None;
                last_error = Some(format!("foreground readback failed: {error}"));
            }
        }

        if Instant::now() >= deadline {
            break;
        }

        tokio::time::sleep(Duration::from_millis(FOCUS_WINDOW_POLL_MS)).await;
    }

    Err(focus_failed_error(
        params,
        target,
        matched,
        last_foreground,
        last_error,
        activation.attempts,
    ))
}

fn activate_focus_window_once(
    before: &ForegroundContext,
    matched: &ForegroundContext,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<FocusActivation, ErrorData> {
    let plan = focus_activation_plan(before.hwnd, matched.hwnd);
    if plan.attempts == 0 {
        return Ok(plan);
    }

    boundary.ensure("immediately_before_focus_window_with_intent")?;
    synapse_a11y::focus_window_with_intent(
        matched.hwnd,
        synapse_a11y::ForegroundActivationIntent::OperatorRequested { caller: TOOL },
    )
    .map_err(|error| {
        a11y_error_to_focus_mcp(
            "set_foreground_window",
            error,
            Some(json!({
                "before_foreground": before,
                "matched_window": matched,
                "attempts": plan.attempts,
                "method": plan.method,
            })),
        )
    })?;

    Ok(plan)
}

fn focus_activation_plan(before_hwnd: i64, matched_hwnd: i64) -> FocusActivation {
    if before_hwnd == matched_hwnd {
        FocusActivation {
            attempts: 0,
            method: METHOD_ALREADY_FOREGROUND,
        }
    } else {
        FocusActivation {
            attempts: 1,
            method: METHOD_SET_FOREGROUND,
        }
    }
}

fn postcondition_verified_state(
    before_signature: String,
    after_signature: String,
    changed: bool,
    detail: impl Into<String>,
) -> ActPostcondition {
    ActPostcondition {
        status: "verified_state".to_owned(),
        observed_delta: Some(changed),
        source_of_truth: Some(SOURCE_OF_TRUTH.to_owned()),
        before_signature: Some(before_signature),
        after_signature: Some(after_signature),
        detail: Some(format!("{TOOL} {}", detail.into())),
    }
}

fn focus_failed_error(
    params: &ActFocusWindowParams,
    target: &FocusWindowTargetReadback,
    matched: &ForegroundContext,
    last_foreground: Option<ForegroundContext>,
    last_error: Option<String>,
    attempts: u32,
) -> ErrorData {
    tracing::error!(
        code = error_codes::ACTION_FOCUS_WINDOW_FAILED,
        tool = TOOL,
        source_of_truth = SOURCE_OF_TRUTH,
        target_hwnd = matched.hwnd,
        target_pid = matched.pid,
        target_title = %matched.window_title,
        attempts,
        timeout_ms = params.verify_timeout_ms,
        stable_ms = params.stable_ms,
        last_error = ?last_error,
        "act_focus_window could not verify requested foreground window"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{TOOL} could not verify hwnd 0x{:x} as foreground within {} ms",
            matched.hwnd, params.verify_timeout_ms
        ),
        Some(json!({
            "code": error_codes::ACTION_FOCUS_WINDOW_FAILED,
            "tool": TOOL,
            "source_of_truth": SOURCE_OF_TRUTH,
            "reason": "foreground_not_stable",
            "target": target,
            "matched_window": matched,
            "last_foreground": last_foreground,
            "last_error": last_error,
            "attempts": attempts,
            "verify_timeout_ms": params.verify_timeout_ms,
            "stable_ms": params.stable_ms,
        })),
    )
}

fn a11y_error_to_focus_mcp(
    operation: &'static str,
    error: synapse_a11y::A11yError,
    prior_readback: Option<Value>,
) -> ErrorData {
    let code = error.code();
    tracing::error!(
        code,
        tool = TOOL,
        operation,
        source_of_truth = SOURCE_OF_TRUTH,
        detail = %error,
        "act_focus_window a11y operation failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("{TOOL} {operation} failed: {error}"),
        Some(json!({
            "code": code,
            "tool": TOOL,
            "operation": operation,
            "source_of_truth": SOURCE_OF_TRUTH,
            "detail": error.to_string(),
            "prior_readback": prior_readback,
        })),
    )
}

fn window_not_found_error(
    reason: &'static str,
    target: &RequestedFocusTarget,
    observed_windows: Vec<WindowSummary>,
    read_error: Option<String>,
) -> ErrorData {
    tracing::error!(
        code = error_codes::ACTION_WINDOW_NOT_FOUND,
        tool = TOOL,
        reason,
        target = ?target_readback(target),
        observed_count = observed_windows.len(),
        read_error = ?read_error,
        "act_focus_window target window not found"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("{TOOL} did not find a unique visible target window: {reason}"),
        Some(json!({
            "code": error_codes::ACTION_WINDOW_NOT_FOUND,
            "tool": TOOL,
            "source_of_truth": SOURCE_OF_TRUTH,
            "reason": reason,
            "target": target_readback(target),
            "observed_windows": observed_windows,
            "read_error": read_error,
        })),
    )
}

fn window_ambiguous_error(
    target: &RequestedFocusTarget,
    matches: &[ForegroundContext],
) -> ErrorData {
    let candidates = window_summaries(matches);
    tracing::error!(
        code = error_codes::ACTION_WINDOW_AMBIGUOUS,
        tool = TOOL,
        target = ?target_readback(target),
        candidates = ?candidates,
        "act_focus_window target matched multiple visible windows"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("{TOOL} target matched {} visible windows", matches.len()),
        Some(json!({
            "code": error_codes::ACTION_WINDOW_AMBIGUOUS,
            "tool": TOOL,
            "source_of_truth": SOURCE_OF_TRUTH,
            "reason": "multiple_visible_matches",
            "target": target_readback(target),
            "candidate_count": matches.len(),
            "candidates": candidates,
        })),
    )
}

fn target_readback(target: &RequestedFocusTarget) -> FocusWindowTargetReadback {
    match target {
        RequestedFocusTarget::Hwnd(hwnd) => FocusWindowTargetReadback {
            kind: "hwnd".to_owned(),
            hwnd: Some(*hwnd),
            title_regex: None,
            pid: None,
        },
        RequestedFocusTarget::TitleRegex { original, .. } => FocusWindowTargetReadback {
            kind: "title_regex".to_owned(),
            hwnd: None,
            title_regex: Some(original.clone()),
            pid: None,
        },
        RequestedFocusTarget::Pid(pid) => FocusWindowTargetReadback {
            kind: "pid".to_owned(),
            hwnd: None,
            title_regex: None,
            pid: Some(*pid),
        },
    }
}

fn window_summaries(contexts: &[ForegroundContext]) -> Vec<WindowSummary> {
    contexts
        .iter()
        .take(12)
        .map(|context| WindowSummary {
            hwnd: context.hwnd,
            pid: context.pid,
            process_name: context.process_name.clone(),
            window_title: context.window_title.clone(),
        })
        .collect()
}
