use rmcp::ErrorData;
use std::time::Duration;

use synapse_action::ActionHandle;
use synapse_core::{Action, Backend, Key};
use tokio_util::sync::CancellationToken;

use super::action_error_to_mcp;

pub(in crate::m2::press) async fn execute_live_press_sequence(
    handle: ActionHandle,
    keys: Vec<Key>,
    hold_ms: u32,
    backend: Backend,
    connection_closed_cancel: Option<CancellationToken>,
    boundary: crate::m2::OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let mut pressed = Vec::with_capacity(keys.len());
    for key in &keys {
        if let Err(error) = boundary.ensure("immediately_before_live_press_key_down") {
            release_pressed_keys(&handle, &pressed, backend).await;
            return Err(error);
        }
        if let Err(error) = handle
            .execute(Action::KeyDown {
                key: key.clone(),
                backend,
            })
            .await
        {
            release_pressed_keys(&handle, &pressed, backend).await;
            return Err(action_error_to_mcp(&error));
        }
        pressed.push(key.clone());
        maybe_force_panic_after_keydown();
    }

    if let Err(error) = boundary.ensure("after_live_press_key_downs_before_hold") {
        release_pressed_keys(&handle, &pressed, backend).await;
        return Err(error);
    }
    let hold_end = wait_for_hold_end(hold_ms, connection_closed_cancel).await;
    let boundary_error = boundary
        .ensure("after_live_press_hold_before_release")
        .err();

    let mut first_error = None;
    for key in pressed.iter().rev() {
        if let Err(error) = handle
            .execute(Action::KeyUp {
                key: key.clone(),
                backend,
            })
            .await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }

    if let Some(error) = boundary_error {
        if let Some(release_error) = first_error.as_ref() {
            tracing::error!(
                code = synapse_core::error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                detail_code = "LIVE_PRESS_KEY_RELEASE_AFTER_OPERATOR_PANIC_FAILED",
                detail = %release_error,
                "operator panic superseded held live keys and best-effort key-up cleanup failed"
            );
        }
        return Err(error);
    }
    if let Some(error) = first_error {
        return Err(action_error_to_mcp(&error));
    }
    match hold_end {
        HoldEnd::Elapsed => {}
        HoldEnd::ConnectionClosed => {
            tracing::warn!(
                code = "M2_ACT_PRESS_CONNECTION_CLOSED_RELEASE",
                released_keys = pressed.len(),
                "readback=connection_closed edge=act_press after=pressed_keys_released"
            );
        }
        HoldEnd::OperatorRelease => {
            tracing::warn!(
                code = synapse_core::error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                released_keys = pressed.len(),
                "readback=operator_hotkey edge=act_press after=pressed_keys_released"
            );
        }
    }
    Ok(())
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum HoldEnd {
    Elapsed,
    ConnectionClosed,
    OperatorRelease,
}

async fn wait_for_hold_end(
    hold_ms: u32,
    connection_closed_cancel: Option<CancellationToken>,
) -> HoldEnd {
    let release_epoch = synapse_action::operator_release_epoch();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(hold_ms));
    loop {
        if synapse_action::operator_release_requested_since(release_epoch) {
            return HoldEnd::OperatorRelease;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return HoldEnd::Elapsed;
        }
        let tick = (deadline - now).min(Duration::from_millis(1));
        if let Some(cancel) = &connection_closed_cancel {
            tokio::select! {
                () = tokio::time::sleep(tick) => {}
                () = cancel.cancelled() => return HoldEnd::ConnectionClosed,
            }
        } else {
            tokio::time::sleep(tick).await;
        }
    }
}

#[cfg(debug_assertions)]
fn maybe_force_panic_after_keydown() {
    if std::env::var("SYNAPSE_MCP_FORCE_PANIC_DURING_ACT").as_deref()
        == Ok("act_press_after_keydown")
    {
        tracing::warn!(
            code = "M2_ACT_PRESS_FORCE_PANIC_AFTER_KEYDOWN",
            "readback=act_press edge=force_panic after=keydown"
        );
        tokio::task::block_in_place(|| panic!("forced panic during act_press after keydown"));
    }
}

#[cfg(not(debug_assertions))]
const fn maybe_force_panic_after_keydown() {}

async fn release_pressed_keys(handle: &ActionHandle, pressed: &[Key], backend: Backend) {
    for key in pressed.iter().rev() {
        let _ = handle
            .execute(Action::KeyUp {
                key: key.clone(),
                backend,
            })
            .await;
    }
}
