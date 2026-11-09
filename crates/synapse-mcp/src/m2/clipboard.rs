use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rmcp::ErrorData;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;

use crate::m1::mcp_error;
use crate::m2::postcondition::{
    ActPostcondition, default_verify_timeout_ms, no_observed_delta_error,
    postcondition_failed_error, postcondition_not_requested, postcondition_observed_delta,
    text_signature,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClipboardParams {
    pub verb: ActClipboardVerb,
    pub text: Option<String>,
    #[serde(default = "default_clipboard_format")]
    #[schemars(default = "default_clipboard_format")]
    pub format: ActClipboardFormat,
    #[serde(default)]
    #[schemars(default)]
    pub verify_delta: bool,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActClipboardVerb {
    Read,
    Write,
    Clear,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActClipboardFormat {
    Text,
    Unicode,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClipboardResponse {
    pub ok: bool,
    pub verb: ActClipboardVerb,
    pub format: ActClipboardFormat,
    pub written: bool,
    pub cleared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_len: Option<usize>,
    pub backing: ActClipboardBacking,
    pub backend_tier_used: String,
    pub source_of_truth: String,
    pub os_clipboard_touched: bool,
    pub required_foreground: bool,
    pub lease_required: bool,
    pub elapsed_ms: u32,
    pub postcondition: ActPostcondition,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActClipboardBacking {
    SessionBuffer,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SessionClipboardBuffer {
    text: String,
    last_format: Option<ActClipboardFormat>,
    updated_at_unix_ms: u64,
}

pub(crate) type SharedSessionClipboardBuffers = Arc<Mutex<HashMap<String, SessionClipboardBuffer>>>;

pub(crate) fn new_session_clipboards() -> SharedSessionClipboardBuffers {
    Arc::new(Mutex::new(HashMap::new()))
}

pub(crate) fn act_clipboard_session_buffer(
    params: ActClipboardParams,
    session_id: &str,
    buffers: &SharedSessionClipboardBuffers,
) -> Result<ActClipboardResponse, ErrorData> {
    validate_params(&params)?;
    validate_session_clipboard_write_format(&params)?;
    let started = Instant::now();
    let mut guard = buffers.lock().map_err(|_err| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "session clipboard buffer lock poisoned",
        )
    })?;
    let before_text = if params.verify_delta && !matches!(params.verb, ActClipboardVerb::Read) {
        Some(session_clipboard_text(&guard, session_id))
    } else {
        None
    };
    let response = match params.verb {
        ActClipboardVerb::Read => {
            let text = session_clipboard_text(&guard, session_id);
            session_response(
                params.verb,
                params.format,
                false,
                false,
                Some(text.clone()),
                Some(text.chars().count()),
                started,
                postcondition_not_requested("act_clipboard", "session_clipboard_buffer"),
            )
        }
        ActClipboardVerb::Write => {
            let text = params
                .text
                .as_deref()
                .ok_or_else(missing_write_text_error)?;
            let buffer = guard.entry(session_id.to_owned()).or_default();
            buffer.text = text.to_owned();
            buffer.last_format = Some(params.format);
            buffer.updated_at_unix_ms = unix_time_ms_now();
            session_response(
                params.verb,
                params.format,
                true,
                false,
                None,
                Some(text.chars().count()),
                started,
                postcondition_not_requested("act_clipboard", "session_clipboard_buffer"),
            )
        }
        ActClipboardVerb::Clear => {
            guard.remove(session_id);
            session_response(
                params.verb,
                params.format,
                false,
                true,
                None,
                None,
                started,
                postcondition_not_requested("act_clipboard", "session_clipboard_buffer"),
            )
        }
    };
    let after_text = before_text
        .as_ref()
        .map(|_| session_clipboard_text(&guard, session_id));
    drop(guard);
    let response = if let (Some(before), Some(after)) = (before_text, after_text) {
        verify_clipboard_delta(response, &params, before, after, "session_clipboard_buffer")?
    } else {
        response
    };
    tracing::info!(
        code = "M2_ACT_CLIPBOARD_READBACK",
        kind = "act_clipboard",
        verb = response.verb.as_str(),
        format = response.format.as_str(),
        backing = "session_buffer",
        source_of_truth = %response.source_of_truth,
        os_clipboard_touched = response.os_clipboard_touched,
        lease_required = response.lease_required,
        written = response.written,
        cleared = response.cleared,
        text_len = response.text_len,
        "readback=session_clipboard_buffer tool=act_clipboard after_operation_readback"
    );
    Ok(response)
}

impl ActClipboardFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Unicode => "unicode",
        }
    }
}

impl ActClipboardVerb {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Clear => "clear",
        }
    }
}

fn validate_params(params: &ActClipboardParams) -> Result<(), ErrorData> {
    match params.verb {
        ActClipboardVerb::Write => {
            if params.text.is_none() {
                return Err(missing_write_text_error());
            }
        }
        ActClipboardVerb::Read | ActClipboardVerb::Clear => {
            if params.text.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "act_clipboard text is only valid with verb=write",
                ));
            }
        }
    }
    Ok(())
}

fn validate_session_clipboard_write_format(params: &ActClipboardParams) -> Result<(), ErrorData> {
    if matches!(params.verb, ActClipboardVerb::Write)
        && matches!(params.format, ActClipboardFormat::Text)
        && let Some(text) = params.text.as_ref()
        && !text.is_ascii()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_clipboard format=text requires ASCII text; use format=unicode for Unicode text",
        ));
    }
    Ok(())
}

fn missing_write_text_error() -> ErrorData {
    mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        "act_clipboard verb=write requires text",
    )
}

fn verify_clipboard_delta(
    mut response: ActClipboardResponse,
    params: &ActClipboardParams,
    before: String,
    after: String,
    source_of_truth: &'static str,
) -> Result<ActClipboardResponse, ErrorData> {
    let before_signature = text_signature(&before);
    let after_signature = text_signature(&after);
    if before == after {
        return Err(no_observed_delta_error(
            "act_clipboard",
            source_of_truth,
            params.verify_timeout_ms,
            before_signature,
            after_signature,
            serde_json::json!({
                "verb": params.verb,
                "format": params.format,
                "before_len": before.chars().count(),
                "after_len": after.chars().count(),
            }),
        ));
    }
    if matches!(params.verb, ActClipboardVerb::Write)
        && after != params.text.as_deref().unwrap_or_default()
    {
        return Err(postcondition_failed_error(
            "act_clipboard",
            source_of_truth,
            "clipboard text changed but did not equal requested write text",
            before_signature,
            after_signature,
            serde_json::json!({
                "verb": params.verb,
                "format": params.format,
                "expected_len": params.text.as_ref().map(|text| text.chars().count()),
                "after_len": after.chars().count(),
            }),
        ));
    }
    if matches!(params.verb, ActClipboardVerb::Clear) && !after.is_empty() {
        return Err(postcondition_failed_error(
            "act_clipboard",
            source_of_truth,
            "clipboard text changed but was not empty after clear",
            before_signature,
            after_signature,
            serde_json::json!({
                "verb": params.verb,
                "format": params.format,
                "after_len": after.chars().count(),
            }),
        ));
    }
    response.postcondition = postcondition_observed_delta(
        "act_clipboard",
        source_of_truth,
        before_signature,
        after_signature,
        "observed clipboard text Source-of-Truth change",
    );
    Ok(response)
}

fn session_response(
    verb: ActClipboardVerb,
    format: ActClipboardFormat,
    written: bool,
    cleared: bool,
    text: Option<String>,
    text_len: Option<usize>,
    started: Instant,
    postcondition: ActPostcondition,
) -> ActClipboardResponse {
    ActClipboardResponse {
        ok: true,
        verb,
        format,
        written,
        cleared,
        text,
        text_len,
        backing: ActClipboardBacking::SessionBuffer,
        backend_tier_used: "session_buffer".to_owned(),
        source_of_truth: "session_clipboard_buffer".to_owned(),
        os_clipboard_touched: false,
        required_foreground: false,
        lease_required: false,
        elapsed_ms: elapsed_ms(started),
        postcondition,
    }
}

fn session_clipboard_text(
    buffers: &HashMap<String, SessionClipboardBuffer>,
    session_id: &str,
) -> String {
    buffers
        .get(session_id)
        .map_or_else(String::new, |buffer| buffer.text.clone())
}

const fn default_clipboard_format() -> ActClipboardFormat {
    ActClipboardFormat::Unicode
}

fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
