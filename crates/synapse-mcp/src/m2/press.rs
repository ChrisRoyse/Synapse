use std::{sync::Arc, time::Instant};

use rmcp::ErrorData;
use synapse_action::{ActionError, ActionHandle, RecordingBackend};
use synapse_core::{Action, Backend, Key, KeyCode, Profile, error_codes};
use tokio_util::sync::CancellationToken;

use crate::m1::mcp_error;

const MAX_HOLD_MS: u32 = 30_000;

mod keys;
mod live;
mod postmessage;
mod record;
mod schema;
#[cfg(test)]
mod tests;

use schema::press_postcondition_not_requested;
pub use schema::{
    ActKeymapParams, ActKeymapResponse, ActPressParams, ActPressResponse, PressBackend,
};

pub(crate) use postmessage::HwndKeyboardTargetState;
pub(crate) use postmessage::clipboard_sequence_number;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedKeymapPress {
    pub alias: String,
    pub resolved_binding: String,
    pub resolved_keys: Vec<String>,
    pub press: ActPressParams,
}

#[cfg(test)]
pub async fn act_press_with_handle(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    connection_closed_cancel: Option<CancellationToken>,
    params: ActPressParams,
) -> Result<ActPressResponse, ErrorData> {
    let boundary = super::OperatorPanicActionBoundary::arm("act_press", "direct_call_entry")?;
    act_press_with_handle_and_boundary(
        handle,
        recording,
        connection_closed_cancel,
        params,
        boundary,
    )
    .await
}

pub(crate) async fn act_press_with_handle_and_boundary(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    connection_closed_cancel: Option<CancellationToken>,
    params: ActPressParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActPressResponse, ErrorData> {
    let started = Instant::now();
    let keys = keys::normalized_keys(&params.keys)?;
    let key_count = u32::try_from(keys.len()).map_err(|_err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_press keys length exceeds u32::MAX",
        )
    })?;
    let backend = params.backend.to_backend();
    validate_hold_ms(params.hold_ms)?;
    let action = press_action(keys.clone(), params.hold_ms, backend);

    boundary.ensure("immediately_before_press_dispatch")?;
    if let Some(recording) = recording {
        record::execute_recording(&recording, &action)?;
    } else {
        live::execute_live_press_sequence(
            handle,
            keys,
            params.hold_ms,
            backend,
            connection_closed_cancel,
            boundary,
        )
        .await?;
    }

    Ok(ActPressResponse {
        ok: true,
        keys_pressed: key_count,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        backend_used: backend_used_name(backend).to_owned(),
        backend_tier_used: "foreground".to_owned(),
        required_foreground: true,
        postcondition: press_postcondition_not_requested(),
    })
}

// Test-exercised keymap helper (see m2::press::tests); production keymap
// routing carries the request-captured boundary into the physical helper.
#[cfg(test)]
pub async fn act_keymap_with_handle(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    connection_closed_cancel: Option<CancellationToken>,
    profile: &Profile,
    params: ActKeymapParams,
) -> Result<ActKeymapResponse, ErrorData> {
    let resolved = resolve_keymap_press(profile, &params)?;
    let boundary = super::OperatorPanicActionBoundary::arm("act_keymap", "direct_call_entry")?;
    let response = act_press_with_handle_and_boundary(
        handle,
        recording,
        connection_closed_cancel,
        resolved.press.clone(),
        boundary,
    )
    .await?;

    Ok(act_keymap_response_from_press(&resolved, response))
}

pub fn action_from_press_params(params: &ActPressParams) -> Result<Action, ErrorData> {
    validate_hold_ms(params.hold_ms)?;
    let keys = keys::normalized_keys(&params.keys)?;
    Ok(press_action(
        keys,
        params.hold_ms,
        params.backend.to_backend(),
    ))
}

pub(crate) async fn act_press_cdp_target(
    endpoint: &str,
    cdp_target_id: &str,
    params: ActPressParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActPressResponse, ErrorData> {
    let started = Instant::now();
    let keys = keys::normalized_keys(&params.keys)?;
    let key_count = u32::try_from(keys.len()).map_err(|_err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_press keys length exceeds u32::MAX",
        )
    })?;
    validate_hold_ms(params.hold_ms)?;
    let strokes = cdp_key_strokes(&keys)?;
    boundary.ensure("immediately_before_cdp_press_key_sequence")?;
    synapse_a11y::cdp_press_key_sequence(endpoint, cdp_target_id, strokes, params.hold_ms)
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "act_press CDP Input.dispatchKeyEvent failed for target {cdp_target_id:?}: {error}"
                ),
            )
        })?;
    Ok(ActPressResponse {
        ok: true,
        keys_pressed: key_count,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        backend_used: backend_used_name(params.backend.to_backend()).to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        postcondition: press_postcondition_not_requested(),
    })
}

pub(crate) async fn act_press_postmessage_target(
    root_hwnd: i64,
    params: ActPressParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActPressResponse, ErrorData> {
    let started = Instant::now();
    let keys = keys::normalized_keys(&params.keys)?;
    let key_count = u32::try_from(keys.len()).map_err(|_err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_press keys length exceeds u32::MAX",
        )
    })?;
    validate_hold_ms(params.hold_ms)?;
    postmessage::post_key_sequence(root_hwnd, &keys, params.hold_ms, boundary).await?;
    Ok(ActPressResponse {
        ok: true,
        keys_pressed: key_count,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        backend_used: backend_used_name(params.backend.to_backend()).to_owned(),
        backend_tier_used: "postmessage".to_owned(),
        required_foreground: false,
        postcondition: press_postcondition_not_requested(),
    })
}

pub(crate) fn hwnd_keyboard_target_state(
    root_hwnd: i64,
) -> Result<HwndKeyboardTargetState, ErrorData> {
    postmessage::hwnd_keyboard_target_state(root_hwnd)
}

pub(crate) fn act_press_normalized_labels(
    params: &ActPressParams,
) -> Result<Vec<String>, ErrorData> {
    Ok(keys::normalized_keys(&params.keys)?
        .iter()
        .map(key_label)
        .collect())
}

pub(crate) fn resolve_keymap_press(
    profile: &Profile,
    params: &ActKeymapParams,
) -> Result<ResolvedKeymapPress, ErrorData> {
    let alias = normalized_alias(&params.alias)?;
    let resolved_binding = profile.keymap.get(&alias).cloned().ok_or_else(|| {
        mcp_error(
            error_codes::PROFILE_KEYMAP_INVALID,
            format!(
                "profile {} keymap alias {alias:?} was not found",
                profile.id
            ),
        )
    })?;
    let resolved_keys = split_key_binding(&resolved_binding)?;
    let press = ActPressParams {
        keys: resolved_keys.clone(),
        hold_ms: params.hold_ms,
        backend: params.backend,
        verify_delta: false,
        allow_foreground_change: false,
        expected_foreground_process_regex: None,
        expected_foreground_title_regex: None,
        verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
        window_hwnd: params.window_hwnd,
        cdp_target_id: params.cdp_target_id.clone(),
        auto_wait: false,
        auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        auto_wait_element_id: None,
    };
    Ok(ResolvedKeymapPress {
        alias,
        resolved_binding,
        resolved_keys,
        press,
    })
}

pub(crate) fn act_keymap_response_from_press(
    resolved: &ResolvedKeymapPress,
    response: ActPressResponse,
) -> ActKeymapResponse {
    ActKeymapResponse {
        ok: response.ok,
        alias: resolved.alias.clone(),
        resolved_binding: resolved.resolved_binding.clone(),
        resolved_keys: resolved.resolved_keys.clone(),
        hold_ms: resolved.press.hold_ms,
        keys_pressed: response.keys_pressed,
        elapsed_ms: response.elapsed_ms,
        backend_used: response.backend_used,
        backend_tier_used: response.backend_tier_used,
        required_foreground: response.required_foreground,
    }
}

fn normalized_alias(alias: &str) -> Result<String, ErrorData> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_keymap alias must not be empty",
        ));
    }
    Ok(alias.to_ascii_lowercase())
}

fn split_key_binding(binding: &str) -> Result<Vec<String>, ErrorData> {
    let keys = binding
        .split('+')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let normalized = keys::normalized_keys(&keys)?;
    Ok(normalized.iter().map(key_label).collect())
}

fn key_label(key: &Key) -> String {
    match &key.code {
        KeyCode::Named { value } => value.clone(),
        KeyCode::Symbol { value } => value.to_string(),
        KeyCode::HidCode { value } => format!("hid:{value}"),
    }
}

fn cdp_key_strokes(keys: &[Key]) -> Result<Vec<synapse_a11y::CdpKeyStroke>, ErrorData> {
    let labels = keys.iter().map(key_label).collect::<Vec<_>>();
    let shift_down = labels.iter().any(|label| label == "shift");
    let text_allowed = !labels
        .iter()
        .any(|label| matches!(label.as_str(), "ctrl" | "alt" | "super"));
    labels
        .iter()
        .map(|label| cdp_key_stroke(label, text_allowed, shift_down))
        .collect()
}

fn cdp_key_stroke(
    label: &str,
    text_allowed: bool,
    shift_down: bool,
) -> Result<synapse_a11y::CdpKeyStroke, ErrorData> {
    let stroke = match label {
        "ctrl" => cdp_named_stroke("Control", "ControlLeft", 17, None, None, 2, Some(1)),
        "shift" => cdp_named_stroke("Shift", "ShiftLeft", 16, None, None, 8, Some(1)),
        "alt" => cdp_named_stroke("Alt", "AltLeft", 18, None, None, 1, Some(1)),
        "super" => cdp_named_stroke("Meta", "MetaLeft", 91, None, None, 4, Some(1)),
        "backspace" => cdp_named_stroke("Backspace", "Backspace", 8, None, None, 0, None),
        "tab" => cdp_named_stroke("Tab", "Tab", 9, None, None, 0, None),
        "enter" => cdp_named_stroke("Enter", "Enter", 13, None, None, 0, None),
        "esc" => cdp_named_stroke("Escape", "Escape", 27, None, None, 0, None),
        "space" => cdp_named_stroke(
            " ",
            "Space",
            32,
            text_allowed.then(|| " ".to_owned()),
            Some(" ".to_owned()),
            0,
            None,
        ),
        "pageup" => cdp_named_stroke("PageUp", "PageUp", 33, None, None, 0, None),
        "pagedown" => cdp_named_stroke("PageDown", "PageDown", 34, None, None, 0, None),
        "end" => cdp_named_stroke("End", "End", 35, None, None, 0, None),
        "home" => cdp_named_stroke("Home", "Home", 36, None, None, 0, None),
        "left" => cdp_named_stroke("ArrowLeft", "ArrowLeft", 37, None, None, 0, None),
        "up" => cdp_named_stroke("ArrowUp", "ArrowUp", 38, None, None, 0, None),
        "right" => cdp_named_stroke("ArrowRight", "ArrowRight", 39, None, None, 0, None),
        "down" => cdp_named_stroke("ArrowDown", "ArrowDown", 40, None, None, 0, None),
        "insert" => cdp_named_stroke("Insert", "Insert", 45, None, None, 0, None),
        "delete" => cdp_named_stroke("Delete", "Delete", 46, None, None, 0, None),
        "`" => cdp_named_stroke(
            "`",
            "Backquote",
            192,
            text_allowed.then(|| "`".to_owned()),
            Some("`".to_owned()),
            0,
            None,
        ),
        label if label.len() == 1 && label.as_bytes()[0].is_ascii_alphabetic() => {
            let lower = char::from(label.as_bytes()[0].to_ascii_lowercase());
            let upper = char::from(label.as_bytes()[0].to_ascii_uppercase());
            let key_text = if shift_down {
                upper.to_string()
            } else {
                lower.to_string()
            };
            let vk = i64::from(label.as_bytes()[0].to_ascii_uppercase());
            cdp_named_stroke(
                key_text.as_str(),
                format!("Key{upper}"),
                vk,
                text_allowed.then(|| key_text.clone()),
                Some(lower.to_string()),
                0,
                None,
            )
        }
        label if label.len() == 1 && label.as_bytes()[0].is_ascii_digit() => {
            let digit = char::from(label.as_bytes()[0]);
            cdp_named_stroke(
                digit.to_string(),
                format!("Digit{digit}"),
                i64::from(label.as_bytes()[0]),
                text_allowed.then(|| digit.to_string()),
                Some(digit.to_string()),
                0,
                None,
            )
        }
        label if label.starts_with('f') => {
            let number = label[1..].parse::<i64>().map_err(|error| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("act_press unsupported function key {label:?}: {error}"),
                )
            })?;
            if !(1..=24).contains(&number) {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("act_press unsupported function key {label:?}"),
                ));
            }
            let name = format!("F{number}");
            cdp_named_stroke(name.clone(), name, 111 + number, None, None, 0, None)
        }
        _ => {
            return Err(action_error_to_mcp(&ActionError::UnsupportedKey {
                detail: format!("act_press unsupported CDP key {label:?}"),
            }));
        }
    };
    Ok(stroke)
}

fn cdp_named_stroke(
    key: impl Into<String>,
    code: impl Into<String>,
    vk: i64,
    text: Option<String>,
    unmodified_text: Option<String>,
    modifier_bit: i64,
    location: Option<i64>,
) -> synapse_a11y::CdpKeyStroke {
    synapse_a11y::CdpKeyStroke {
        key: key.into(),
        code: code.into(),
        windows_virtual_key_code: vk,
        native_virtual_key_code: vk,
        key_identifier: key_identifier(vk),
        text,
        unmodified_text,
        modifier_bit,
        location,
    }
}

fn key_identifier(vk: i64) -> Option<String> {
    (matches!(vk, 0x30..=0x5A)).then(|| format!("U+{vk:04X}"))
}

fn validate_hold_ms(hold_ms: u32) -> Result<(), ErrorData> {
    if hold_ms == 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_press hold_ms must be at least 1",
        ));
    }
    if hold_ms > MAX_HOLD_MS {
        return Err(action_error_to_mcp(&ActionError::HoldExceededMax {
            detail: format!("act_press hold_ms {hold_ms} exceeds max {MAX_HOLD_MS}"),
        }));
    }
    Ok(())
}

/// Select-all chord (Ctrl+A) for composite tools that replace field content
/// (#882). `hold_ms` is the chord hold duration.
pub(crate) fn select_all_chord_action(hold_ms: u32, backend: Backend) -> Result<Action, ErrorData> {
    let keys = keys::normalized_keys(&["ctrl".to_owned(), "a".to_owned()])?;
    Ok(press_action(keys, hold_ms, backend))
}

/// Single Delete key press for composite tools that clear field content (#882).
pub(crate) fn delete_key_action(hold_ms: u32, backend: Backend) -> Result<Action, ErrorData> {
    let keys = keys::normalized_keys(&["delete".to_owned()])?;
    Ok(press_action(keys, hold_ms, backend))
}

fn press_action(keys: Vec<synapse_core::Key>, hold_ms: u32, backend: Backend) -> Action {
    if let [key] = keys.as_slice() {
        return Action::KeyPress {
            key: key.clone(),
            hold_ms,
            backend,
        };
    }
    Action::KeyChord {
        keys,
        hold_ms,
        backend,
    }
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    crate::m2::action_error_to_mcp(error)
}

const fn backend_used_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Auto | Backend::Software => "software",
        Backend::Vigem => "vigem",
        Backend::Hardware => "hardware",
    }
}
