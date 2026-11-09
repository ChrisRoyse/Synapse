use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use rmcp::ErrorData;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_action::{
    ActionEmitterSnapshotHandle, ActionError, ActionHandle, ActionStateSnapshot,
    request_release_interrupt,
};
use synapse_core::{Action, error_codes};
use synapse_reflex::ReflexRuntime;

use crate::m1::mcp_error;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAllParams {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAllResponse {
    pub released_keys: u32,
    pub released_buttons: u32,
    pub neutralized_pads: u32,
}

pub async fn release_all_with_handles(
    handle: ActionHandle,
    snapshot_handle: ActionEmitterSnapshotHandle,
    reflex_runtime: Option<Arc<Mutex<ReflexRuntime>>>,
    _params: ReleaseAllParams,
) -> Result<ReleaseAllResponse, ErrorData> {
    let started = Instant::now();
    // Wake interrupt-aware in-flight software holds before awaiting an actor
    // snapshot; otherwise the snapshot request itself can wait behind the hold.
    request_release_interrupt();
    let reflex_report = disable_reflexes_for_release_all(reflex_runtime.as_ref());
    let before = snapshot_handle
        .snapshot()
        .await
        .map_err(|error| action_error_to_mcp(&error))?;
    let response = response_from_snapshot(&before)?;

    handle
        .execute(Action::ReleaseAll)
        .await
        .map_err(|error| action_error_to_mcp(&error))?;

    let after = snapshot_handle
        .snapshot()
        .await
        .map_err(|error| action_error_to_mcp(&error))?;
    ensure_drained(&after)?;

    tracing::info!(
        code = "M2_RELEASE_ALL_READBACK",
        kind = "release_all",
        released_keys = response.released_keys,
        released_buttons = response.released_buttons,
        neutralized_pads = response.neutralized_pads,
        reflex_result = reflex_report.result,
        disabled_reflexes = reflex_report.disabled_ids.len(),
        disabled_reflex_ids = ?reflex_report.disabled_ids,
        reflex_error_code = ?reflex_report.error_code,
        reflex_detail = ?reflex_report.detail,
        before_held_keys = ?before.held_keys,
        before_held_buttons = ?before.held_buttons,
        before_pad_state_len = before.pad_state.len(),
        after_held_keys = ?after.held_keys,
        after_held_buttons = ?after.held_buttons,
        after_pad_state_len = after.pad_state.len(),
        elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        "readback=action_emitter_state tool=release_all after_snapshot_readback"
    );

    if reflex_report.result == "error" {
        return Err(mcp_error(
            reflex_report
                .error_code
                .unwrap_or(error_codes::TOOL_INTERNAL_ERROR),
            reflex_report
                .detail
                .unwrap_or_else(|| "release_all could not disable active reflexes".to_owned()),
        ));
    }

    Ok(response)
}

#[derive(Debug)]
struct ReflexDisableReport {
    result: &'static str,
    disabled_ids: Vec<String>,
    error_code: Option<&'static str>,
    detail: Option<String>,
}

fn disable_reflexes_for_release_all(
    reflex_runtime: Option<&Arc<Mutex<ReflexRuntime>>>,
) -> ReflexDisableReport {
    let Some(runtime) = reflex_runtime else {
        return ReflexDisableReport {
            result: "not_initialized",
            disabled_ids: Vec::new(),
            error_code: None,
            detail: None,
        };
    };
    let mut runtime = match runtime.lock() {
        Ok(runtime) => runtime,
        Err(_err) => {
            return ReflexDisableReport {
                result: "error",
                disabled_ids: Vec::new(),
                error_code: Some(error_codes::TOOL_INTERNAL_ERROR),
                detail: Some("reflex runtime lock poisoned".to_owned()),
            };
        }
    };
    match runtime.disable_all_for_release_all() {
        Ok(disabled) => ReflexDisableReport {
            result: "ok",
            disabled_ids: disabled.into_iter().map(|status| status.id).collect(),
            error_code: None,
            detail: None,
        },
        Err(error) => ReflexDisableReport {
            result: "error",
            disabled_ids: Vec::new(),
            error_code: Some(error.code()),
            detail: Some(error.to_string()),
        },
    }
}

fn response_from_snapshot(snapshot: &ActionStateSnapshot) -> Result<ReleaseAllResponse, ErrorData> {
    Ok(ReleaseAllResponse {
        released_keys: count_to_u32(snapshot.held_keys.len(), "held_keys")?,
        released_buttons: count_to_u32(snapshot.held_buttons.len(), "held_buttons")?,
        neutralized_pads: count_to_u32(snapshot.pad_state.len(), "pad_state")?,
    })
}

fn ensure_drained(snapshot: &ActionStateSnapshot) -> Result<(), ErrorData> {
    if snapshot.held_keys.is_empty()
        && snapshot.held_buttons.is_empty()
        && snapshot.pad_state.is_empty()
        && snapshot.held_key_timer_count == 0
    {
        return Ok(());
    }

    Err(mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        format!("release_all did not drain held state: {snapshot:?}"),
    ))
}

fn count_to_u32(value: usize, field: &str) -> Result<u32, ErrorData> {
    u32::try_from(value).map_err(|_err| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("release_all {field} count exceeds u32::MAX"),
        )
    })
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}
