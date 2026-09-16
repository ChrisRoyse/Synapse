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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use tokio_util::sync::CancellationToken;

    use serde_json::Value;
    use synapse_action::{
        ActionBackend, ActionEmitter, ActionEmitterSnapshotHandle, ActionError,
        ActionStateSnapshot, EmitState, RecordingBackend, operator_release_epoch,
        operator_release_requested_since,
    };
    use synapse_core::{
        Action, Backend, ButtonAction, GamepadController, GamepadReport, Key, KeyCode, MouseButton,
        PadButton, ReflexButtonTarget, ReflexState, StoredReflexAudit,
    };
    use synapse_reflex::{EventBus, HoldButtonParams, ReflexRuntime, ScheduledReflex};
    use synapse_storage::{Db, cf, decode_json};
    use tempfile::TempDir;

    use super::{ReleaseAllParams, release_all_with_handles};

    #[tokio::test]
    async fn release_all_counts_and_drains_actor_state() {
        let _operator_epoch_serial =
            crate::test_support::lease_serial("release_all_counts_epoch_serial");
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let keys = [key("ctrl"), key("shift"), key("alt")];
        for key in &keys {
            handle
                .execute(synapse_core::Action::KeyDown {
                    key: key.clone(),
                    backend: Backend::Software,
                })
                .await
                .unwrap_or_else(|error| panic!("prime key should succeed: {error}"));
        }
        handle
            .execute(synapse_core::Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Down,
                hold_ms: 0,
                backend: Backend::Software,
            })
            .await
            .unwrap_or_else(|error| panic!("prime mouse button should succeed: {error}"));
        let report = GamepadReport {
            controller: GamepadController::X360,
            buttons: vec![PadButton::A],
            thumb_l: (0.5, -0.5),
            thumb_r: (0.0, 0.0),
            lt: 0.25,
            rt: 0.0,
        };
        handle
            .execute(synapse_core::Action::PadReport { pad: 1, report })
            .await
            .unwrap_or_else(|error| panic!("prime pad should succeed: {error}"));

        let before = snapshot_handle
            .snapshot()
            .await
            .unwrap_or_else(|error| panic!("snapshot before release_all should succeed: {error}"));
        println!("readback=action_emitter_state tool=release_all edge=happy before={before:?}");
        assert_eq!(before.held_keys.len(), 3);
        assert_eq!(before.held_buttons, vec![MouseButton::Left]);
        assert_eq!(before.pad_state.len(), 1);

        let response = release_all_with_handles(
            handle.clone(),
            snapshot_handle.clone(),
            None,
            ReleaseAllParams {},
        )
        .await
        .unwrap_or_else(|error| panic!("release_all should succeed: {error}"));
        assert_eq!(response.released_keys, 3);
        assert_eq!(response.released_buttons, 1);
        assert_eq!(response.neutralized_pads, 1);

        let after = snapshot_handle
            .snapshot()
            .await
            .unwrap_or_else(|error| panic!("snapshot after release_all should succeed: {error}"));
        println!(
            "readback=action_emitter_state tool=release_all edge=happy after={after:?} response={response:?}"
        );
        assert!(after.held_keys.is_empty());
        assert!(after.held_buttons.is_empty());
        assert!(after.pad_state.is_empty());
        assert_eq!(after.held_key_timer_count, 0);

        cancel.cancel();
        let final_snapshot = join
            .await
            .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
        assert!(final_snapshot.held_keys.is_empty());
        assert!(final_snapshot.held_buttons.is_empty());
        assert!(final_snapshot.pad_state.is_empty());
    }

    #[tokio::test]
    async fn release_all_disables_reflexes_before_draining_actor_state() {
        let _operator_epoch_serial =
            crate::test_support::lease_serial("release_all_reflex_epoch_serial");
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir should exist: {error}"));
        let db = Arc::new(
            Db::open(&temp.path().join("db"), synapse_core::SCHEMA_VERSION)
                .unwrap_or_else(|error| panic!("test db should open: {error}")),
        );
        let runtime = Arc::new(Mutex::new(
            ReflexRuntime::spawn(Arc::clone(&db), handle.clone(), EventBus::default())
                .unwrap_or_else(|error| panic!("reflex runtime should spawn: {error}")),
        ));
        let reflex = ScheduledReflex::hold_button(
            "release-all-held-button",
            HoldButtonParams::new(ReflexButtonTarget::Mouse {
                button: MouseButton::Left,
            }),
        );
        runtime
            .lock()
            .unwrap_or_else(|error| panic!("reflex runtime lock should not poison: {error}"))
            .register(&reflex)
            .unwrap_or_else(|error| panic!("hold_button reflex should register: {error}"));

        let before = wait_for_snapshot(&snapshot_handle, |snapshot| {
            snapshot.held_buttons == vec![MouseButton::Left]
        })
        .await;
        println!(
            "readback=action_emitter_state tool=release_all edge=active_reflex before={before:?}"
        );

        let response = release_all_with_handles(
            handle.clone(),
            snapshot_handle.clone(),
            Some(Arc::clone(&runtime)),
            ReleaseAllParams {},
        )
        .await
        .unwrap_or_else(|error| panic!("release_all should disable reflex and drain: {error}"));
        assert_eq!(response.released_buttons, 1);

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let after = snapshot_handle
            .snapshot()
            .await
            .unwrap_or_else(|error| panic!("snapshot after release_all should succeed: {error}"));
        let reflexes = runtime
            .lock()
            .unwrap_or_else(|error| panic!("reflex runtime lock should not poison: {error}"))
            .list(false)
            .unwrap_or_else(|error| panic!("reflex list should succeed: {error}"));
        println!(
            "readback=action_emitter_state tool=release_all edge=active_reflex after={after:?} response={response:?} reflexes={reflexes:?}"
        );
        assert!(after.held_keys.is_empty());
        assert!(after.held_buttons.is_empty());
        assert!(after.pad_state.is_empty());
        assert_eq!(after.held_key_timer_count, 0);
        assert_eq!(
            reflexes
                .iter()
                .find(|status| status.id == "release-all-held-button")
                .map(|status| status.state),
            Some(ReflexState::Disabled)
        );
        let audits = db
            .scan_cf(cf::CF_REFLEX_AUDIT)
            .unwrap_or_else(|error| panic!("reflex audit scan should succeed: {error}"))
            .into_iter()
            .map(|(_key, value)| {
                decode_json::<StoredReflexAudit>(&value)
                    .unwrap_or_else(|error| panic!("reflex audit row should decode: {error}"))
            })
            .collect::<Vec<_>>();
        let disabled_reason = audits
            .iter()
            .filter(|audit| audit.reflex_id == "release-all-held-button")
            .filter(|audit| audit.status == ReflexState::Disabled)
            .filter_map(|audit| reason(&audit.details))
            .next_back();
        assert_eq!(disabled_reason.as_deref(), Some("release_all"));

        cancel.cancel();
        let final_snapshot = join
            .await
            .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
        assert!(final_snapshot.held_buttons.is_empty());
    }

    #[tokio::test]
    async fn release_all_interrupts_in_flight_hold_before_snapshot_read() {
        let _operator_epoch_serial =
            crate::test_support::lease_serial("release_all_interrupt_epoch_serial");
        let cancel = CancellationToken::new();
        let backend = Arc::new(InterruptibleHoldBackend::default());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend.clone());

        let press = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .execute(Action::KeyPress {
                        key: key("a"),
                        hold_ms: 1_000,
                        backend: Backend::Software,
                    })
                    .await
            }
        });
        // Wait on the backend's causal readiness event. The timeout is only a
        // deadlock diagnostic; readiness state—not elapsed time—is the verdict.
        tokio::time::timeout(Duration::from_secs(10), backend.hold_ready.notified())
            .await
            .unwrap_or_else(|_| panic!("interruptible hold backend did not publish readiness"));
        assert!(backend.hold_started.load(Ordering::Acquire));

        let response = release_all_with_handles(
            handle.clone(),
            snapshot_handle.clone(),
            None,
            ReleaseAllParams {},
        )
        .await
        .unwrap_or_else(|error| panic!("release_all should interrupt the in-flight hold: {error}"));
        press
            .await
            .unwrap_or_else(|error| panic!("press task should join: {error}"))
            .unwrap_or_else(|error| panic!("interrupted press should release cleanly: {error}"));
        let after = snapshot_handle
            .snapshot()
            .await
            .unwrap_or_else(|error| panic!("snapshot after interrupt should succeed: {error}"));

        println!(
            "readback=action_emitter_state tool=release_all edge=in_flight_hold_interrupt release_observed={} response={response:?} after={after:?}",
            backend.release_observed.load(Ordering::Acquire)
        );
        // `release_observed` is set only when the backend sees the operator
        // release epoch change after its readiness snapshot. It directly proves
        // interruption; a test-side stopwatch only measures scheduler load.
        assert!(backend.release_observed.load(Ordering::Acquire));
        assert!(after.held_keys.is_empty());
        assert!(after.held_buttons.is_empty());
        assert!(after.pad_state.is_empty());

        cancel.cancel();
        let final_snapshot = join
            .await
            .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
        assert!(final_snapshot.held_keys.is_empty());
    }

    fn key(value: &str) -> Key {
        Key {
            code: KeyCode::Named {
                value: value.to_owned(),
            },
            use_scancode: false,
        }
    }

    async fn wait_for_snapshot(
        snapshot_handle: &ActionEmitterSnapshotHandle,
        predicate: impl Fn(&ActionStateSnapshot) -> bool,
    ) -> ActionStateSnapshot {
        for _attempt in 0..50 {
            let snapshot = snapshot_handle
                .snapshot()
                .await
                .unwrap_or_else(|error| panic!("snapshot should succeed: {error}"));
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("action emitter snapshot did not reach expected state");
    }

    fn reason(details: &Value) -> Option<String> {
        details
            .get("reason")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    #[derive(Default)]
    struct InterruptibleHoldBackend {
        hold_started: AtomicBool,
        hold_ready: tokio::sync::Notify,
        release_observed: AtomicBool,
    }

    impl ActionBackend for InterruptibleHoldBackend {
        fn execute(&self, action: &Action, _state: &mut EmitState) -> Result<(), ActionError> {
            match action {
                Action::KeyPress { .. } => {
                    let epoch = operator_release_epoch();
                    // Publish readiness only after the exact epoch baseline is
                    // captured. The test's release_all trigger may run as soon
                    // as hold_started becomes visible; publishing first would
                    // allow that release to land before this snapshot and make
                    // a real interruption look unobserved.
                    self.hold_started.store(true, Ordering::Release);
                    self.hold_ready.notify_one();
                    // This backend models an in-flight hold that cannot expire
                    // before the test's release trigger. The 30 s deadline is
                    // only a fail-loud deadlock backstop; an epoch transition is
                    // the sole successful completion condition.
                    let deadlock_deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        // The epoch is the causal verdict. Check it before the
                        // synthetic hold deadline so scheduler starvation after
                        // `hold_started` cannot hide a release that already
                        // happened while this backend was descheduled.
                        if operator_release_requested_since(epoch) {
                            self.release_observed.store(true, Ordering::Release);
                            break;
                        }
                        if Instant::now() >= deadlock_deadline {
                            return Err(ActionError::BackendUnavailable {
                                detail: "interruptible hold backend did not observe the operator release epoch within the 30 s deadlock backstop".to_owned(),
                            });
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(())
                }
                Action::ReleaseAll => Ok(()),
                _ => Ok(()),
            }
        }
    }
}
