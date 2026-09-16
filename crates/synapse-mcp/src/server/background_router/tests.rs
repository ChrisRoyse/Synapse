//! Unit tests for background_router (split out of the module body per #1555).

use super::*;
use rmcp::schemars::schema_for;
use std::{collections::BTreeSet, num::NonZeroUsize, path::Path};
use tokio_util::sync::CancellationToken;

use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

fn read_action() -> TargetActParams {
    serde_json::from_value(json!({ "verb": "read" }))
        .expect("synthetic read action should deserialize")
}

fn act_error_field(error: &ErrorData, field: &str) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get(field))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn act_error_u64(error: &ErrorData, field: &str) -> Option<u64> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get(field))
        .and_then(Value::as_u64)
}

#[test]
fn stdio_act_invoke_owns_a_panic_authority_label_without_relaxing_raw_target_act() {
    assert_eq!(act_operator_panic_authority_label(None), "stdio:act:invoke");
    assert_eq!(
        act_operator_panic_authority_label(Some("http-session-7")),
        "http-session-7"
    );
}

fn complete_test_operator_panic(token: synapse_action::OperatorPanicSafetyToken, reason: &str) {
    let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
        synapse_action::complete_operator_panic_safety_generation(token)
            .unwrap_or_else(|detail| panic!("complete test operator panic: {detail}"))
    else {
        panic!("isolated test panic must own its exact finalization");
    };
    let generation = finalization.generation();
    let _cleared =
        synapse_action::force_clear_operator_panic_input_lease_generation(generation, reason);
    let lease_after = synapse_action::lease::status();
    assert!(
        !lease_after.held,
        "exact test panic lease must be unheld after finalizer clear: {lease_after:?}"
    );
    assert!(synapse_action::finish_operator_panic_safety_finalization(
        finalization,
        true
    ));
}

fn synthetic_foreground_context() -> synapse_core::ForegroundContext {
    synapse_core::ForegroundContext {
        hwnd: 0x2000,
        pid: 42,
        process_name: "synthetic-editor.exe".to_owned(),
        process_path: "C:\\synthetic\\synthetic-editor.exe".to_owned(),
        window_title: "Synthetic Editor".to_owned(),
        window_bounds: Rect {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    }
}

fn sanitized_tool_input_schema(tool_name: &str) -> Value {
    let tools = crate::server::schema_sanitize::sanitize_tools(
        crate::server::SynapseService::tool_router().list_all(),
    );
    let tool = tools
        .iter()
        .find(|tool| tool.name.as_ref() == tool_name)
        .unwrap_or_else(|| panic!("{tool_name} tool missing"));
    Value::Object((*tool.input_schema).clone())
}

fn act_schema_variant<'a>(schema: &'a Value, operation: &str) -> &'a Value {
    schema["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("act schema oneOf missing"))
        .iter()
        .find(|variant| variant["properties"]["operation"]["const"] == operation)
        .unwrap_or_else(|| panic!("act schema operation={operation} variant missing"))
}

fn schema_property_names(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema properties missing"))
        .keys()
        .cloned()
        .collect()
}

fn foreground_guard_test_service(path: &Path) -> SynapseService {
    SynapseService::try_with_m2_shutdown_reason_and_m3_config(
        CancellationToken::new(),
        "act_foreground_authority_guard_test",
        CancellationToken::new(),
        &M2ServiceConfig::default(),
        M3ServiceConfig::from_cli_parts(
            Some(path.join("db")),
            Some(path.to_path_buf()),
            false,
            "127.0.0.1:0".to_owned(),
            NonZeroUsize::new(4).unwrap_or_else(|| panic!("four is nonzero")),
            false,
            true,
            None,
            false,
            None,
        ),
        M4ServiceConfig::default(),
    )
    .unwrap_or_else(|error| panic!("construct authority-guard service: {error:#}"))
}

fn foreground_guard_with_new_authority(
    service: &SynapseService,
    session_id: &str,
) -> ActForegroundAuthorityGuard {
    let profile_before = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("snapshot initial profile: {error:?}"));
    let mut guard = ActForegroundAuthorityGuard::new(
        service,
        session_id,
        &profile_before,
        synapse_action::lease::status().owner_session_id,
        synapse_action::operator_panic_epoch(),
    )
    .unwrap_or_else(|error| panic!("arm authority guard: {error:?}"));
    let lease_status = match synapse_action::lease::try_acquire(
        session_id,
        synapse_action::lease::ttl_from_ms(30_000),
    ) {
        synapse_action::LeaseOutcome::Acquired(status) => status,
        outcome => panic!("fresh authority-guard lease must acquire: {outcome:?}"),
    };
    guard.acquire_outcome_was_new = true;
    service
        .persist_session_lease(session_id, &lease_status)
        .unwrap_or_else(|error| panic!("persist guard lease: {error:?}"));
    service
        .write_tool_profile_assignment(
            session_id,
            crate::server::tool_profiles::ToolProfileKind::BreakGlass,
            "forced_guard_elevation",
            Some("exercise drop cleanup".to_owned()),
            Some(session_id.to_owned()),
        )
        .unwrap_or_else(|error| panic!("persist elevated profile: {error:?}"));
    guard
}

fn assert_foreground_guard_restored_new_authority(
    service: &SynapseService,
    session_id: &str,
    expected_profile_value_sha256: &str,
) {
    let profile_after = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("read profile after guard cleanup: {error:?}"));
    assert_eq!(
        profile_after.profile,
        crate::server::tool_profiles::ToolProfileKind::NormalAgent
    );
    assert_ne!(
        profile_after
            .policy_row
            .as_ref()
            .map(|row| row.record.source.as_str()),
        Some("forced_guard_elevation")
    );
    assert_eq!(
        profile_after
            .policy_row
            .as_ref()
            .map(|row| row.value_sha256.as_str()),
        Some(expected_profile_value_sha256),
        "drop cleanup must restore the byte-exact pre-call profile assignment"
    );
    let lease_after = synapse_action::lease::status();
    assert_ne!(
        lease_after.owner_session_id.as_deref(),
        Some(session_id),
        "drop cleanup must revoke the newly acquired process-global lease"
    );
    let persisted_after = crate::server::session_continuity::snapshot_persisted_session_lease_row(
        &service.m3_state_handle(),
        session_id,
    )
    .unwrap_or_else(|error| panic!("read persisted lease after guard cleanup: {error}"));
    assert!(
        !persisted_after.row_exists(),
        "drop cleanup must restore the absent pre-call durable lease row"
    );
}

#[test]
fn act_facade_rejects_unknown_operation_enum() {
    let error = serde_json::from_value::<ActParams>(json!({
        "operation": "teleport",
        "action": { "verb": "read" }
    }))
    .expect_err("unknown act operation must fail schema deserialization");

    assert!(
        error.to_string().contains("unknown variant"),
        "unexpected act operation error: {error}"
    );
}

#[test]
fn act_facade_invoke_rejects_foreground_fields() {
    let params = ActParams {
        operation: ActOperation::Invoke,
        action: Some(read_action()),
        reason: Some("needs hardware foreground".to_owned()),
        ttl_ms: None,
    };

    let error =
        validate_act_invoke_params(&params).expect_err("invoke must reject foreground-only reason");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "operation").as_deref(),
        Some("invoke")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("reason")
    );
}

#[test]
fn act_facade_foreground_requires_non_empty_reason() {
    let params = ActParams {
        operation: ActOperation::Foreground,
        action: Some(read_action()),
        reason: Some("   ".to_owned()),
        ttl_ms: Some(30_000),
    };

    let error =
        validate_act_foreground_params(&params).expect_err("foreground must reject blank reason");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "operation").as_deref(),
        Some("foreground")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("reason")
    );
}

#[test]
fn act_facade_foreground_rejects_out_of_range_ttl() {
    let params = ActParams {
        operation: ActOperation::Foreground,
        action: Some(read_action()),
        reason: Some("needs audited hardware foreground".to_owned()),
        ttl_ms: Some(30_001),
    };

    let error = validate_act_foreground_params(&params)
        .expect_err("foreground must reject above-max ttl_ms");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "detail_code").as_deref(),
        Some("LEASE_TTL_OUT_OF_RANGE")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("ttl_ms")
    );
    assert_eq!(act_error_u64(&error, "ttl_ms"), Some(30_001));
    assert_eq!(
        act_error_u64(&error, "min_ttl_ms"),
        Some(synapse_action::MIN_LEASE_TTL_MS)
    );
    assert_eq!(
        act_error_u64(&error, "max_ttl_ms"),
        Some(synapse_action::MAX_LEASE_TTL_MS)
    );
}

#[test]
fn act_foreground_cleanup_postconditions_require_separate_profile_and_lease_truth() {
    use crate::server::tool_profiles::ToolProfileKind;

    let session_id = "session-cleanup-1379";
    assert_eq!(
        act_foreground_cleanup_postconditions(
            session_id,
            ToolProfileKind::NormalAgent,
            true,
            None,
            Some(ToolProfileKind::NormalAgent),
            true,
            None,
        ),
        (true, true),
        "restored profile plus no owned lease is the verified cleanup state"
    );
    assert_eq!(
        act_foreground_cleanup_postconditions(
            session_id,
            ToolProfileKind::NormalAgent,
            true,
            None,
            Some(ToolProfileKind::BreakGlass),
            true,
            None,
        ),
        (false, true),
        "a released lease cannot hide a still-elevated profile"
    );
    assert_eq!(
        act_foreground_cleanup_postconditions(
            session_id,
            ToolProfileKind::NormalAgent,
            true,
            None,
            Some(ToolProfileKind::NormalAgent),
            true,
            Some(session_id),
        ),
        (true, false),
        "a restored profile cannot hide a lease still owned by this session"
    );
    assert_eq!(
        act_foreground_cleanup_postconditions(
            session_id,
            ToolProfileKind::NormalAgent,
            true,
            None,
            None,
            false,
            None,
        ),
        (false, false),
        "missing readbacks fail closed"
    );
    assert_eq!(
        act_foreground_cleanup_postconditions(
            session_id,
            ToolProfileKind::BreakGlass,
            false,
            Some(session_id),
            Some(ToolProfileKind::BreakGlass),
            true,
            Some(session_id),
        ),
        (true, true),
        "the facade must preserve a lease the caller already held"
    );
    assert_eq!(
        act_foreground_cleanup_postconditions(
            session_id,
            ToolProfileKind::BreakGlass,
            false,
            Some(session_id),
            Some(ToolProfileKind::BreakGlass),
            true,
            None,
        ),
        (true, false),
        "losing a caller-owned renewed lease must fail the ordinary completion verdict"
    );
}

#[test]
fn act_foreground_cleanup_diagnostics_preserve_delegated_action_error() {
    let action_error = ErrorData::new(
        ErrorCode(-32099),
        "known delegated action failure",
        Some(json!({
            "code": error_codes::ACTION_POSTCONDITION_FAILED,
            "expected": "foreground_hwnd=42",
            "actual": "foreground_hwnd=7",
        })),
    );
    let action_result: Result<Json<TargetActResponse>, ErrorData> = Err(action_error);

    let diagnostics = act_foreground_action_diagnostics(Some(&action_result));

    assert_eq!(diagnostics["status"], "error");
    assert_eq!(
        diagnostics["error"]["code"],
        error_codes::ACTION_POSTCONDITION_FAILED
    );
    assert_eq!(
        diagnostics["error"]["message"],
        "known delegated action failure"
    );
    assert_eq!(
        diagnostics["error"]["data"]["expected"],
        "foreground_hwnd=42"
    );
    assert_eq!(diagnostics["error"]["data"]["actual"], "foreground_hwnd=7");
}

#[test]
fn act_foreground_authority_guard_drop_is_storage_free_and_fail_closed() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_guard_panic_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create authority-guard tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-panic-1621";
    let guard = foreground_guard_with_new_authority(&service, session_id);
    let prior_profile = guard.prior_profile.clone();

    drop(guard);

    let drain = service.drain_state_handle().snapshot();
    assert!(drain.draining, "an armed guard Drop must drain fail-closed");
    assert_eq!(
        drain.source,
        Some("act_foreground_guard_dropped_outside_supervised_cleanup")
    );
    let profile_after = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("read profile after invariant Drop: {error:?}"));
    assert_eq!(
        profile_after.profile,
        crate::server::tool_profiles::ToolProfileKind::BreakGlass,
        "Drop must not perform RocksDB/profile restoration"
    );
    let persisted_after = crate::server::session_continuity::snapshot_persisted_session_lease_row(
        &service.m3_state_handle(),
        session_id,
    )
    .unwrap_or_else(|error| panic!("read persisted row after invariant Drop: {error}"));
    assert!(
        persisted_after.row_exists(),
        "Drop must not mutate the persisted continuity row"
    );
    assert_eq!(
        synapse_action::lease::status().owner_session_id.as_deref(),
        Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID),
        "Drop must leave bounded in-memory operator preemption visible"
    );

    let _ = synapse_action::force_clear_input_lease("guard_drop_invariant_test_cleanup");
    service
        .delete_persisted_session_lease(session_id)
        .unwrap_or_else(|error| panic!("delete invariant-test lease row: {error:?}"));
    service
        .restore_tool_profile_assignment_exact(&prior_profile)
        .unwrap_or_else(|error| panic!("restore invariant-test profile row: {error:?}"));
}

#[tokio::test(flavor = "current_thread")]
async fn tracked_authority_transaction_completes_cleanup_after_caller_cancellation() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_guard_cancel_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create authority-guard tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-cancel-1621";
    let worker_service = service.clone();
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let task = service
        .spawn_cooperative_authority_transaction(move |_cancellation| async move {
            let _authority_gate = worker_service
                .lock_session_authority(session_id)
                .await
                .unwrap_or_else(|error| panic!("lock tracked authority transaction: {error:?}"));
            let mut guard = foreground_guard_with_new_authority(&worker_service, session_id);
            let expected_profile_value_sha256 = guard.prior_profile_value_sha256.clone();
            let _ = armed_tx.send(expected_profile_value_sha256);
            finish_rx
                .await
                .unwrap_or_else(|error| panic!("finish tracked transaction: {error}"));
            let cleanup = guard.cleanup_with_bounded_retries("caller_cancelled_join_handle");
            assert!(
                cleanup.is_some_and(|readback| readback.cleanup_ok),
                "tracked cleanup must prove its physical postconditions"
            );
        })
        .unwrap_or_else(|error| panic!("spawn tracked authority transaction: {error:?}"));
    let expected_profile_value_sha256 = armed_rx
        .await
        .unwrap_or_else(|error| panic!("guard task dropped before arming: {error}"));

    drop(task);
    finish_tx
        .send(())
        .unwrap_or_else(|()| panic!("detached transaction receiver disappeared"));
    service
        .drain_authority_finalizers()
        .await
        .unwrap_or_else(|error| panic!("drain detached authority transaction: {error}"));
    assert_foreground_guard_restored_new_authority(
        &service,
        session_id,
        &expected_profile_value_sha256,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn detached_tracked_act_transaction_writes_durable_final_audit() {
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create detached-audit tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let worker_service = service.clone();
    let session_id = "act-detached-audit-1621";
    let (intent_tx, intent_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let task = service
        .spawn_cooperative_authority_transaction(move |_cancellation| async move {
            let _authority_gate = worker_service
                .lock_session_authority(session_id)
                .await
                .unwrap_or_else(|error| panic!("lock detached audit transaction: {error:?}"));
            let mut audit = ActCommandAuditGuard::begin(
                &worker_service,
                "lease_status",
                Some(session_id.to_owned()),
                json!({ "operation": "lease_status" }),
                json!({ "source_of_truth": "synthetic_before" }),
            )
            .unwrap_or_else(|error| panic!("write detached audit intent: {error:?}"));
            let _ = intent_tx.send(());
            finish_rx
                .await
                .unwrap_or_else(|error| panic!("finish detached audit transaction: {error}"));
            audit
                .finalize(
                    json!({
                        "source_of_truth": "synthetic_after",
                        "actual": 4,
                        "expected": 4,
                    }),
                    "ok",
                    None,
                )
                .unwrap_or_else(|error| panic!("write detached audit final: {error:?}"));
        })
        .unwrap_or_else(|error| panic!("spawn detached audit transaction: {error:?}"));
    intent_rx
        .await
        .unwrap_or_else(|error| panic!("detached transaction did not write intent: {error}"));

    drop(task);
    finish_tx
        .send(())
        .unwrap_or_else(|()| panic!("detached audit receiver disappeared"));
    service
        .drain_authority_finalizers()
        .await
        .unwrap_or_else(|error| panic!("drain detached audit transaction: {error}"));

    let audit = service
        .command_audit_snapshot()
        .unwrap_or_else(|error| panic!("read CF_ACTION_LOG command audit: {error:?}"));
    let rows = audit
        .rows
        .iter()
        .filter(|row| {
            row.tool == "act"
                && row.verb == "lease_status"
                && row.actor_session_id.as_deref() == Some(session_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "intent and final rows must both persist");
    assert!(rows.iter().any(|row| row.phase == "intent"));
    let final_row = rows
        .iter()
        .find(|row| row.phase == "final")
        .unwrap_or_else(|| panic!("detached transaction final row missing"));
    assert_eq!(final_row.outcome, "ok");
    assert_eq!(
        final_row.after.as_ref().map(|after| &after["actual"]),
        Some(&json!(4))
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_cancellation_runs_real_foreground_cleanup_before_final_command_audit() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_shutdown_cancel_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create shutdown-cancel tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let worker_service = service.clone();
    let session_id = "act-foreground-shutdown-cancel-1631";
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
    let (boundary_finish_tx, boundary_finish_rx) = tokio::sync::oneshot::channel();
    let terminal_steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let worker_steps = std::sync::Arc::clone(&terminal_steps);
    let caller = service
        .spawn_cooperative_authority_transaction(move |cancellation| async move {
            let _authority_gate = worker_service
                .lock_session_authority(session_id)
                .await
                .unwrap_or_else(|error| {
                    panic!("lock shutdown-cancel authority transaction: {error:?}")
                });
            let mut audit = ActCommandAuditGuard::begin(
                &worker_service,
                "foreground",
                Some(session_id.to_owned()),
                json!({ "operation": "foreground" }),
                json!({ "source_of_truth": ACT_FOREGROUND_CLEANUP_SOURCE_OF_TRUTH }),
            )
            .unwrap_or_else(|error| panic!("write shutdown-cancel audit intent: {error:?}"));
            let mut guard = foreground_guard_with_new_authority(&worker_service, session_id);
            let expected_profile_value_sha256 = guard.prior_profile_value_sha256.clone();
            let _ = armed_tx.send((expected_profile_value_sha256, cancellation.clone()));

            // Model an already-polled profile/action boundary. Shutdown may
            // signal while it is pending, but the exact owner must await its
            // terminal result before touching profile/lease cleanup.
            boundary_finish_rx
                .await
                .unwrap_or_else(|error| panic!("finish in-flight authority boundary: {error}"));
            worker_steps
                .lock()
                .expect("record authority-boundary completion")
                .push("boundary_completed");
            assert!(
                cancellation.is_cancelled(),
                "shutdown signal must remain visible after the boundary completes"
            );
            let cancellation_error = cleanup_act_foreground_after_shutdown_cancellation(
                &mut guard,
                session_id,
                "causal_shutdown_test",
                None,
            );
            worker_steps
                .lock()
                .expect("record physical cleanup completion")
                .push("cleanup_completed");
            audit
                .finalize(
                    act_command_audit_error_after(ActOperation::Foreground),
                    "error",
                    Some(&cancellation_error),
                )
                .unwrap_or_else(|error| {
                    panic!("write shutdown-cancel terminal command audit: {error:?}")
                });
            worker_steps
                .lock()
                .expect("record terminal audit completion")
                .push("audit_completed");
        })
        .unwrap_or_else(|error| panic!("spawn shutdown-cancel authority owner: {error:?}"));
    let (expected_profile_value_sha256, cancellation) = armed_rx
        .await
        .unwrap_or_else(|error| panic!("shutdown-cancel authority owner did not arm: {error}"));

    drop(caller);
    let drain_service = service.clone();
    let drain = tokio::spawn(async move { drain_service.drain_authority_finalizers().await });
    cancellation.cancelled().await;

    let profile_while_boundary_in_flight = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("read in-flight profile SoT: {error:?}"));
    assert_eq!(
        profile_while_boundary_in_flight.profile,
        crate::server::tool_profiles::ToolProfileKind::BreakGlass,
        "cancellation must not drop the boundary future and run cleanup early"
    );
    assert_eq!(
        synapse_action::lease::status().owner_session_id.as_deref(),
        Some(session_id),
        "the exact owner must retain its lease until the boundary completes"
    );
    let persisted_while_boundary_in_flight =
        crate::server::session_continuity::snapshot_persisted_session_lease_row(
            &service.m3_state_handle(),
            session_id,
        )
        .unwrap_or_else(|error| panic!("read in-flight persisted lease SoT: {error}"));
    assert!(persisted_while_boundary_in_flight.row_exists());
    let audit_while_boundary_in_flight = service
        .command_audit_snapshot()
        .unwrap_or_else(|error| panic!("read in-flight command audit: {error:?}"));
    let in_flight_rows = audit_while_boundary_in_flight
        .rows
        .iter()
        .filter(|row| {
            row.tool == "act"
                && row.verb == "foreground"
                && row.actor_session_id.as_deref() == Some(session_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(in_flight_rows.len(), 1);
    assert_eq!(in_flight_rows[0].phase, "intent");
    assert!(
        terminal_steps
            .lock()
            .expect("read in-flight authority-step ledger")
            .is_empty(),
        "the in-flight boundary must precede cleanup and final audit"
    );

    boundary_finish_tx
        .send(())
        .unwrap_or_else(|()| panic!("in-flight authority boundary owner disappeared"));
    let drain = drain
        .await
        .unwrap_or_else(|error| panic!("join shutdown-cancel drain: {error}"))
        .unwrap_or_else(|error| panic!("drain shutdown-cancel authority owner: {error}"));

    assert_eq!(drain.registered_tasks_before, 1);
    assert_eq!(drain.cancellation_signals_sent, 1);
    assert_eq!(drain.abort_requests_sent, 0);
    assert!(drain.safe_to_unlock());
    assert_eq!(
        *terminal_steps
            .lock()
            .expect("read terminal authority-step ledger"),
        ["boundary_completed", "cleanup_completed", "audit_completed"],
        "the in-flight boundary must finish before cleanup, and cleanup before final audit"
    );
    assert_foreground_guard_restored_new_authority(
        &service,
        session_id,
        &expected_profile_value_sha256,
    );
    let audit = service
        .command_audit_snapshot()
        .unwrap_or_else(|error| panic!("read shutdown-cancel command audit: {error:?}"));
    let rows = audit
        .rows
        .iter()
        .filter(|row| {
            row.tool == "act"
                && row.verb == "foreground"
                && row.actor_session_id.as_deref() == Some(session_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "intent and terminal rows must both persist");
    assert!(rows.iter().any(|row| row.phase == "intent"));
    let final_row = rows
        .iter()
        .find(|row| row.phase == "final")
        .unwrap_or_else(|| panic!("shutdown-cancel final command audit missing"));
    assert_eq!(final_row.outcome, "error");
    assert_eq!(
        final_row.error_code.as_deref(),
        Some(error_codes::DAEMON_RESTARTING)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn same_session_authority_gate_serializes_without_wall_clock_assumptions() {
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create authority-gate tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "authority-gate-same-session-1379";
    let first = service
        .lock_session_authority(session_id)
        .await
        .unwrap_or_else(|error| panic!("lock first authority transaction: {error:?}"));
    let worker_service = service.clone();
    let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
    let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        let _second = worker_service
            .lock_session_authority_after_resolve(session_id, || {
                let _ = attempted_tx.send(());
            })
            .await
            .unwrap_or_else(|error| panic!("lock second authority transaction: {error:?}"));
        let _ = acquired_tx.send(());
    });
    attempted_rx
        .await
        .unwrap_or_else(|error| panic!("second authority transaction never attempted: {error}"));
    tokio::task::yield_now().await;
    assert!(
        matches!(
            acquired_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "second same-session transaction must remain queued while the first gate is held"
    );

    drop(first);
    acquired_rx.await.unwrap_or_else(|error| {
        panic!("second transaction did not acquire after release: {error}")
    });
    waiter
        .await
        .unwrap_or_else(|error| panic!("join authority-gate waiter: {error}"));
}

#[tokio::test(flavor = "current_thread")]
async fn same_session_act_invoke_cannot_observe_foreground_temporary_authority() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_invoke_foreground_authority_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create invoke-authority tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-invoke-foreground-authority-1379";
    let foreground_gate = service
        .lock_session_authority(session_id)
        .await
        .unwrap_or_else(|error| panic!("lock foreground authority transaction: {error:?}"));
    let mut foreground_guard = foreground_guard_with_new_authority(&service, session_id);
    let expected_profile_value_sha256 = foreground_guard.prior_profile_value_sha256.clone();

    let temporary_profile = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("read temporary foreground profile: {error:?}"));
    assert_eq!(
        temporary_profile.profile,
        crate::server::tool_profiles::ToolProfileKind::BreakGlass
    );
    assert_eq!(
        synapse_action::lease::status().owner_session_id.as_deref(),
        Some(session_id)
    );
    let temporary_persisted =
        crate::server::session_continuity::snapshot_persisted_session_lease_row(
            &service.m3_state_handle(),
            session_id,
        )
        .unwrap_or_else(|error| panic!("read temporary persisted lease row: {error}"));
    assert!(temporary_persisted.row_exists());

    let worker_service = service.clone();
    let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
    let (evaluated_tx, mut evaluated_rx) = tokio::sync::oneshot::channel();
    let invoke = tokio::spawn(async move {
        let _invoke_gate = worker_service
            .lock_act_authority_after_resolve(ActOperation::Invoke, Some(session_id), || {
                let _ = attempted_tx.send(());
            })
            .await
            .unwrap_or_else(|error| panic!("admit same-session act invoke: {error:?}"));
        let profile = worker_service
            .tool_profile_snapshot(Some(session_id))
            .unwrap_or_else(|error| panic!("invoke profile evaluation: {error:?}"));
        let lease_owner = synapse_action::lease::status().owner_session_id;
        let persisted = crate::server::session_continuity::snapshot_persisted_session_lease_row(
            &worker_service.m3_state_handle(),
            session_id,
        )
        .unwrap_or_else(|error| panic!("invoke persisted-row evaluation: {error}"));
        let _ = evaluated_tx.send((
            profile.profile,
            profile
                .policy_row
                .map(|row| row.value_sha256)
                .unwrap_or_default(),
            lease_owner,
            persisted.row_exists(),
        ));
    });
    attempted_rx
        .await
        .unwrap_or_else(|error| panic!("same-session invoke never reached admission: {error}"));
    assert!(
        matches!(
            evaluated_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "invoke must not evaluate profile, lease, target, or claim authority while foreground owns the keyed gate"
    );

    let cleanup = foreground_guard.cleanup_with_bounded_retries("invoke_waits_for_cleanup");
    assert!(
        cleanup.is_some_and(|readback| readback.cleanup_ok),
        "foreground cleanup must prove exact restoration before invoke admission"
    );
    assert_foreground_guard_restored_new_authority(
        &service,
        session_id,
        &expected_profile_value_sha256,
    );
    drop(foreground_gate);

    let (profile, profile_value_sha256, lease_owner, persisted_row_exists) = evaluated_rx
        .await
        .unwrap_or_else(|error| panic!("invoke did not evaluate restored authority: {error}"));
    assert_eq!(
        profile,
        crate::server::tool_profiles::ToolProfileKind::NormalAgent
    );
    assert_eq!(profile_value_sha256, expected_profile_value_sha256);
    assert_ne!(lease_owner.as_deref(), Some(session_id));
    assert!(!persisted_row_exists);
    invoke
        .await
        .unwrap_or_else(|error| panic!("join same-session invoke waiter: {error}"));
}

#[tokio::test(flavor = "current_thread")]
async fn same_session_target_mutation_waits_for_authority_transaction() {
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create target-authority tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "target-mutation-authority-1379";
    let target = SessionTarget::Window { hwnd: 0x2468 };
    let authority_gate = service
        .lock_session_authority(session_id)
        .await
        .unwrap_or_else(|error| panic!("lock target authority transaction: {error:?}"));
    service
        .set_session_target_authority_locked(session_id, target.clone())
        .unwrap_or_else(|error| panic!("set synthetic target SoT: {error:?}"));

    let worker_service = service.clone();
    let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
    let (mutated_tx, mut mutated_rx) = tokio::sync::oneshot::channel();
    let mutation = tokio::spawn(async move {
        let _mutation_gate = worker_service
            .lock_session_authority_for_tool_after_resolve("clear_target", session_id, || {
                let _ = attempted_tx.send(());
            })
            .await
            .unwrap_or_else(|error| panic!("admit clear_target mutation: {error:?}"));
        let previous = worker_service
            .clear_session_target_authority_locked(session_id)
            .unwrap_or_else(|error| panic!("clear target SoT: {error:?}"));
        let after = worker_service
            .session_target(Some(session_id))
            .unwrap_or_else(|error| panic!("read target SoT after clear: {error:?}"));
        let _ = mutated_tx.send((previous, after));
    });
    attempted_rx
        .await
        .unwrap_or_else(|error| panic!("target mutation never reached admission: {error}"));
    assert!(
        matches!(
            mutated_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "target mutation must remain queued while the same authority domain is owned"
    );
    assert_eq!(
        service
            .session_target(Some(session_id))
            .unwrap_or_else(|error| panic!("read target SoT while mutation queued: {error:?}")),
        Some(target.clone()),
        "queued mutation must not alter the target Source of Truth"
    );

    drop(authority_gate);
    let (previous, after) = mutated_rx
        .await
        .unwrap_or_else(|error| panic!("target mutation did not complete: {error}"));
    assert!(previous.is_some());
    assert!(after.is_none());
    mutation
        .await
        .unwrap_or_else(|error| panic!("join target mutation waiter: {error}"));
}

#[test]
fn act_foreground_authority_guard_retries_real_cleanup_after_mid_cleanup_panic() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_guard_cleanup_panic_epoch_serial");
    let temp =
        tempfile::tempdir().unwrap_or_else(|error| panic!("create cleanup-panic tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-cleanup-panic-1621";
    let mut guard = foreground_guard_with_new_authority(&service, session_id);
    let expected_profile_value_sha256 = guard.prior_profile_value_sha256.clone();
    guard.cleanup_panics_remaining = 1;

    let cleanup = guard.cleanup_with_bounded_retries("injected_mid_cleanup_panic");
    assert!(
        cleanup.is_some_and(|readback| readback.cleanup_ok),
        "bounded supervised cleanup must retry and prove restoration"
    );
    assert_foreground_guard_restored_new_authority(
        &service,
        session_id,
        &expected_profile_value_sha256,
    );
}

#[test]
fn act_foreground_authority_guard_preserves_real_preexisting_renewed_lease() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_guard_renew_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create authority-guard tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-renewed-1621";
    let profile_before = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("snapshot initial profile: {error:?}"));
    let acquired = match synapse_action::lease::try_acquire(
        session_id,
        synapse_action::lease::ttl_from_ms(5_000),
    ) {
        synapse_action::LeaseOutcome::Acquired(status) => status,
        outcome => panic!("preexisting lease must acquire: {outcome:?}"),
    };
    service
        .persist_session_lease(session_id, &acquired)
        .unwrap_or_else(|error| panic!("persist preexisting lease: {error:?}"));
    let mut guard = ActForegroundAuthorityGuard::new(
        &service,
        session_id,
        &profile_before,
        Some(session_id.to_owned()),
        synapse_action::operator_panic_epoch(),
    )
    .unwrap_or_else(|error| panic!("arm preexisting-lease guard: {error:?}"));
    let renewed = match synapse_action::lease::try_acquire(
        session_id,
        synapse_action::lease::ttl_from_ms(30_000),
    ) {
        synapse_action::LeaseOutcome::Renewed(status) => status,
        outcome => panic!("preexisting lease must renew: {outcome:?}"),
    };
    service
        .persist_session_lease(session_id, &renewed)
        .unwrap_or_else(|error| panic!("persist renewed lease: {error:?}"));
    guard
        .snapshot_expected_retained_persisted_lease_after_renewal()
        .unwrap_or_else(|error| panic!("snapshot exact renewed lease row: {error}"));
    let expected_renewed_row = guard.expected_retained_persisted_lease.clone();
    service
        .write_tool_profile_assignment(
            session_id,
            crate::server::tool_profiles::ToolProfileKind::BreakGlass,
            "forced_guard_elevation",
            Some("preserve caller lease".to_owned()),
            Some(session_id.to_owned()),
        )
        .unwrap_or_else(|error| panic!("persist elevated profile: {error:?}"));

    let cleanup = guard.cleanup_with_bounded_retries("preserve_preexisting_renewed_lease");
    assert!(
        cleanup.is_some_and(|readback| readback.cleanup_ok),
        "supervised cleanup must prove the renewed caller lease was preserved"
    );

    let profile_after = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("read restored profile: {error:?}"));
    assert_eq!(
        profile_after.profile,
        crate::server::tool_profiles::ToolProfileKind::NormalAgent
    );
    assert_eq!(
        synapse_action::lease::status().owner_session_id.as_deref(),
        Some(session_id),
        "drop cleanup must not release a caller-owned renewed lease"
    );
    let persisted_after = crate::server::session_continuity::snapshot_persisted_session_lease_row(
        &service.m3_state_handle(),
        session_id,
    )
    .unwrap_or_else(|error| panic!("read renewed persisted lease: {error}"));
    assert_eq!(
        persisted_after, expected_renewed_row,
        "renewed caller-owned lease must keep its exact continuity bytes"
    );

    assert!(synapse_action::lease::release_if_owner(session_id));
    service
        .delete_persisted_session_lease(session_id)
        .unwrap_or_else(|error| panic!("clean up preexisting lease row: {error:?}"));
}

#[test]
fn act_foreground_authority_guard_rejects_present_but_mismatched_renewed_lease_row() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_guard_mismatched_renew_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create mismatched-renew tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-mismatched-renewed-1621";
    let profile_before = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("snapshot initial profile: {error:?}"));
    let acquired = match synapse_action::lease::try_acquire(
        session_id,
        synapse_action::lease::ttl_from_ms(5_000),
    ) {
        synapse_action::LeaseOutcome::Acquired(status) => status,
        outcome => panic!("preexisting lease must acquire: {outcome:?}"),
    };
    service
        .persist_session_lease(session_id, &acquired)
        .unwrap_or_else(|error| panic!("persist preexisting lease: {error:?}"));
    let mut guard = ActForegroundAuthorityGuard::new(
        &service,
        session_id,
        &profile_before,
        Some(session_id.to_owned()),
        synapse_action::operator_panic_epoch(),
    )
    .unwrap_or_else(|error| panic!("arm preexisting-lease guard: {error:?}"));
    let renewed = match synapse_action::lease::try_acquire(
        session_id,
        synapse_action::lease::ttl_from_ms(30_000),
    ) {
        synapse_action::LeaseOutcome::Renewed(status) => status,
        outcome => panic!("preexisting lease must renew: {outcome:?}"),
    };
    service
        .persist_session_lease(session_id, &renewed)
        .unwrap_or_else(|error| panic!("persist renewed lease: {error:?}"));
    guard
        .snapshot_expected_retained_persisted_lease_after_renewal()
        .unwrap_or_else(|error| panic!("snapshot exact renewed lease row: {error}"));
    let expected_renewed_row = guard.expected_retained_persisted_lease.clone();

    let mut mismatched = renewed.clone();
    mismatched.ttl_ms = Some(29_999);
    service
        .persist_session_lease(session_id, &mismatched)
        .unwrap_or_else(|error| panic!("persist mismatched lease row: {error:?}"));
    let mismatched_row = crate::server::session_continuity::snapshot_persisted_session_lease_row(
        &service.m3_state_handle(),
        session_id,
    )
    .unwrap_or_else(|error| panic!("read mismatched persisted lease: {error}"));
    assert!(mismatched_row.row_exists());
    assert_ne!(mismatched_row, expected_renewed_row);

    let cleanup = guard.cleanup_now("mismatched_renewed_lease_row");
    assert!(!cleanup.cleanup_ok, "{cleanup:?}");
    assert!(!cleanup.persisted_lease_row_restored, "{cleanup:?}");
    assert!(
        cleanup
            .persisted_lease_restore_error
            .as_deref()
            .is_some_and(
                |error| error.contains("does not match its exact persisted continuity row")
            ),
        "{cleanup:?}"
    );

    crate::server::session_continuity::restore_persisted_session_lease_row(
        &service.m3_state_handle(),
        session_id,
        &expected_renewed_row,
    )
    .unwrap_or_else(|error| panic!("restore expected renewed row for test cleanup: {error}"));
    let recovered = guard.cleanup_with_bounded_retries("restore_expected_renewed_row");
    assert!(
        recovered.is_some_and(|readback| readback.cleanup_ok),
        "guard must disarm after the exact renewed row is restored"
    );
    assert!(synapse_action::lease::release_if_owner(session_id));
    service
        .delete_persisted_session_lease(session_id)
        .unwrap_or_else(|error| panic!("clean up mismatched-renew lease row: {error:?}"));
}

#[test]
fn act_foreground_authority_guard_never_resurrects_lease_after_operator_interrupt() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_guard_operator_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create operator-interrupt tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-operator-interrupt-1622";
    let profile_before = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("snapshot initial profile: {error:?}"));
    let acquired = match synapse_action::lease::try_acquire(
        session_id,
        synapse_action::lease::ttl_from_ms(30_000),
    ) {
        synapse_action::LeaseOutcome::Acquired(status) => status,
        outcome => panic!("preexisting operator-interrupt lease must acquire: {outcome:?}"),
    };
    service
        .persist_session_lease(session_id, &acquired)
        .unwrap_or_else(|error| panic!("persist preexisting lease: {error:?}"));
    let mut guard = ActForegroundAuthorityGuard::new(
        &service,
        session_id,
        &profile_before,
        Some(session_id.to_owned()),
        synapse_action::operator_panic_epoch(),
    )
    .unwrap_or_else(|error| panic!("arm operator-interrupt guard: {error:?}"));
    service
        .write_tool_profile_assignment(
            session_id,
            crate::server::tool_profiles::ToolProfileKind::BreakGlass,
            "forced_guard_elevation",
            Some("operator interrupt supersedes snapshot".to_owned()),
            Some(session_id.to_owned()),
        )
        .unwrap_or_else(|error| panic!("persist elevated profile: {error:?}"));

    let mut operator_panic_token = synapse_action::request_operator_panic_interrupt();
    let operator_panic_generation = operator_panic_token.generation();
    let preempted = synapse_action::force_preempt_input_lease_for_operator_panic(
        "operator_guard_test",
        operator_panic_generation,
    )
    .unwrap_or_else(|| panic!("operator preemption must observe the guarded session lease"));
    assert_eq!(preempted.owner_session_id.as_deref(), Some(session_id));
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut operator_panic_token
    ));
    service
        .delete_persisted_session_lease(session_id)
        .unwrap_or_else(|error| panic!("delete K2 session lease row: {error:?}"));

    let cleanup = guard.cleanup_with_bounded_retries("operator_interrupt");
    assert!(
        cleanup.is_some_and(|readback| readback.cleanup_ok),
        "supervised cleanup must prove operator authority was preserved"
    );

    let profile_after = service
        .tool_profile_snapshot(Some(session_id))
        .unwrap_or_else(|error| panic!("read restored profile: {error:?}"));
    assert_eq!(
        profile_after.profile,
        crate::server::tool_profiles::ToolProfileKind::NormalAgent
    );
    assert_eq!(
        synapse_action::lease::status().owner_session_id.as_deref(),
        Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID),
        "guard cleanup must preserve the operator's lease owner"
    );
    let persisted_after = crate::server::session_continuity::snapshot_persisted_session_lease_row(
        &service.m3_state_handle(),
        session_id,
    )
    .unwrap_or_else(|error| panic!("read operator-interrupted persisted lease: {error}"));
    assert!(
        !persisted_after.row_exists(),
        "guard cleanup must not resurrect the K2-deleted session lease row"
    );

    complete_test_operator_panic(operator_panic_token, "operator_guard_test_cleanup");
}

#[test]
fn act_foreground_completion_disarm_rearms_on_late_operator_epoch() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_completion_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create completion-epoch tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-completion-epoch-1622";
    let mut guard = foreground_guard_with_new_authority(&service, session_id);
    let expected_profile_value_sha256 = guard.prior_profile_value_sha256.clone();
    let expected_epoch = guard.operator_panic_epoch_at_arm;

    let mut operator_panic_token = synapse_action::request_operator_panic_interrupt();
    let operator_panic_generation = operator_panic_token.generation();
    let _preempted = synapse_action::force_preempt_input_lease_for_operator_panic(
        "completion_epoch_test",
        operator_panic_generation,
    );
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut operator_panic_token
    ));
    let observed_epoch = guard
        .disarm_if_operator_epoch_stable(expected_epoch)
        .expect_err("late operator panic must prevent ordinary completion disarm");
    assert_ne!(observed_epoch, expected_epoch);
    assert!(guard.armed, "epoch mismatch must rearm physical cleanup");

    let cleanup = guard.cleanup_with_bounded_retries("late_operator_epoch_at_completion");
    assert!(
        cleanup.is_some_and(|readback| {
            readback.cleanup_ok
                && readback.operator_panic_observed
                && readback.operator_epoch_stable_at_disarm
        }),
        "rearmed cleanup must handle the operator epoch and linearize a stable disarm"
    );
    assert_foreground_guard_restored_new_authority(
        &service,
        session_id,
        &expected_profile_value_sha256,
    );
    complete_test_operator_panic(operator_panic_token, "completion_epoch_test_cleanup");
}

#[test]
fn act_foreground_rejects_operator_panic_that_arrives_while_waiting_for_authority() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_prearm_operator_epoch_serial");
    let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("create prearm tempdir: {error}"));
    let _service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-prearm-1622";
    let armed_epoch = synapse_action::operator_panic_epoch();

    let mut operator_panic_token = synapse_action::request_operator_panic_interrupt();
    let operator_panic_generation = operator_panic_token.generation();
    let _preempted = synapse_action::force_preempt_input_lease_for_operator_panic(
        "prearm_epoch_test",
        operator_panic_generation,
    );
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut operator_panic_token
    ));
    let error = match ensure_act_operator_panic_not_observed(
        ActOperation::Foreground,
        session_id,
        armed_epoch,
        "after_authority_wait",
    ) {
        Err(error) => error,
        Ok(()) => panic!("operator panic after entry arm must invalidate the queued transaction"),
    };

    assert_eq!(
        act_error_field(&error, "detail_code").as_deref(),
        Some("ACT_FOREGROUND_OPERATOR_PANIC_PREARMED")
    );
    assert_eq!(
        act_error_u64(&error, "operator_panic_epoch_at_arm"),
        Some(armed_epoch)
    );
    assert!(
        act_error_u64(&error, "operator_panic_epoch_after")
            .is_some_and(|observed| observed != armed_epoch)
    );
    complete_test_operator_panic(operator_panic_token, "prearm_epoch_test_cleanup");
}

#[test]
fn act_foreground_rejects_panic_published_before_k1_preemption_ack() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_pending_k1_epoch_serial");
    let temp =
        tempfile::tempdir().unwrap_or_else(|error| panic!("create pending-K1 tempdir: {error}"));
    let _service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-pending-k1-1622";

    let mut operator_panic_token = synapse_action::request_operator_panic_interrupt();
    let operator_panic_generation = operator_panic_token.generation();
    let epoch_captured_after_publication = synapse_action::operator_panic_epoch();
    assert!(synapse_action::operator_panic_safety_pending());

    let error = ensure_act_operator_panic_not_observed(
        ActOperation::Foreground,
        session_id,
        epoch_captured_after_publication,
        "entry_after_publication_before_k1",
    )
    .expect_err("pending K1 must reject even when entry captured the new panic epoch");
    assert_eq!(
        act_error_field(&error, "detail_code").as_deref(),
        Some("ACT_FOREGROUND_OPERATOR_PANIC_PREARMED")
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("operator_panic_safety_pending"))
            .and_then(Value::as_bool),
        Some(true)
    );
    let invoke_error = ensure_act_operator_panic_not_observed(
        ActOperation::Invoke,
        session_id,
        epoch_captured_after_publication,
        "invoke_after_publication_before_k1",
    )
    .expect_err("pending panic must also reject an invoke mutation");
    assert_eq!(
        act_error_field(&invoke_error, "detail_code").as_deref(),
        Some("ACT_INVOKE_OPERATOR_PANIC_PREARMED")
    );
    assert!(target_act_verb_requires_operator_panic_gate("focus_window"));
    assert!(target_act_verb_requires_operator_panic_gate("press"));
    assert!(!target_act_verb_requires_operator_panic_gate("read"));
    assert!(
        target_act_verb_requires_operator_panic_gate("screenshot"),
        "screenshot activates targets and writes bytes, so it is not a pure read"
    );

    let _preempted = synapse_action::force_preempt_input_lease_for_operator_panic(
        "pending_k1_epoch_test",
        operator_panic_generation,
    );
    assert_eq!(
        synapse_action::lease::status().owner_session_id.as_deref(),
        Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID)
    );
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut operator_panic_token
    ));
    assert!(
        synapse_action::operator_panic_safety_pending(),
        "K1 acknowledgement must not reopen admission before K2 terminal readback"
    );
    complete_test_operator_panic(operator_panic_token, "pending_k1_epoch_test_cleanup");
    assert!(!synapse_action::operator_panic_safety_pending());
}

#[test]
fn generic_release_interrupt_does_not_invalidate_foreground_authority_epoch() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_foreground_generic_release_epoch_serial");
    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create generic-release tempdir: {error}"));
    let _service = foreground_guard_test_service(temp.path());
    let session_id = "act-foreground-generic-release-1622";
    let armed_epoch = synapse_action::operator_panic_epoch();

    synapse_action::request_release_interrupt();

    ensure_act_operator_panic_not_observed(
        ActOperation::Foreground,
        session_id,
        armed_epoch,
        "after_generic_release",
    )
    .unwrap_or_else(|error| {
        panic!("generic software release must not impersonate operator panic: {error:?}")
    });
}

#[tokio::test(flavor = "current_thread")]
async fn target_act_rechecks_operator_panic_after_preparation_before_physical_boundaries() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("target_act_physical_boundary_epoch_serial");
    synapse_action::isolate_interrupt_epochs_for_test();

    for stage in [
        "screenshot_after_activation_before_file_capture",
        "run_shell_before_durable_process_launch",
    ] {
        let boundary = TargetActOperatorPanicBoundary {
            session_id: format!("target-act-boundary-{stage}"),
            operator_panic_epoch_at_arm: synapse_action::operator_panic_epoch(),
        };
        let request_boundary =
            crate::server::operator_panic_boundary::McpOperatorPanicBoundary::capture(
                "target_act",
                Some(&boundary.session_id),
            );
        let (error, mutation_ran, mut token) =
            crate::server::operator_panic_boundary::MCP_OPERATOR_PANIC_BOUNDARY
                .scope(request_boundary, async move {
                    TARGET_ACT_OPERATOR_PANIC_BOUNDARY
                        .scope(Some(boundary), async move {
                            let preparation_complete = true;
                            tokio::task::yield_now().await;
                            let token = synapse_action::request_operator_panic_interrupt();
                            let boundary_result = ensure_target_act_operator_panic_boundary(stage);
                            let mutation_ran = preparation_complete && boundary_result.is_ok();
                            (
                                boundary_result
                                    .expect_err("panic must close the physical boundary"),
                                mutation_ran,
                                token,
                            )
                        })
                        .await
                })
                .await;

        assert!(
            !mutation_ran,
            "stage {stage} must not cross its mutation boundary"
        );
        assert_eq!(
            act_error_field(&error, "detail_code").as_deref(),
            Some("MCP_MUTATION_OPERATOR_PANIC_ADMISSION_CLOSED")
        );
        assert!(synapse_action::acknowledge_operator_panic_preemption(
            &mut token
        ));
        match synapse_action::complete_operator_panic_safety_generation(token)
            .unwrap_or_else(|error| panic!("complete test panic generation: {error}"))
        {
            synapse_action::OperatorPanicSafetyCompletion::Pending => {
                panic!("isolated test panic generation unexpectedly remained pending")
            }
            synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) => assert!(
                synapse_action::finish_operator_panic_safety_finalization(finalization, true)
            ),
        }
        assert!(!synapse_action::operator_panic_safety_pending());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn act_facade_mcp_boundary_snapshot_survives_supervised_task_spawn() {
    let _operator_epoch_serial =
        crate::test_support::lease_serial("act_facade_mcp_boundary_snapshot_spawn_serial");
    synapse_action::isolate_interrupt_epochs_for_test();

    let session_id = "act-boundary-child-1621";
    let request_boundary =
        crate::server::operator_panic_boundary::McpOperatorPanicBoundary::capture(
            "act",
            Some(session_id),
        );
    let target_boundary = TargetActOperatorPanicBoundary {
        session_id: session_id.to_owned(),
        operator_panic_epoch_at_arm: synapse_action::operator_panic_epoch(),
    };

    let result = crate::server::operator_panic_boundary::MCP_OPERATOR_PANIC_BOUNDARY
        .scope(request_boundary, async move {
            let snapshot =
                crate::server::operator_panic_boundary::reserve_and_snapshot_current_mcp_boundary(
                    "act_before_authority_transaction_spawn",
                )
                .unwrap_or_else(|error| {
                    panic!("facade must reserve and snapshot routed MCP boundary: {error:?}")
                });
            tokio::spawn(async move {
                crate::server::operator_panic_boundary::scope_mcp_boundary_snapshot(
                    Some(snapshot),
                    async move {
                        TARGET_ACT_OPERATOR_PANIC_BOUNDARY
                            .scope(Some(target_boundary), async move {
                                ensure_target_act_operator_panic_boundary(
                                    "focus_window_before_activation",
                                )
                                .map_err(|error| error.message.to_string())
                            })
                            .await
                    },
                )
                .await
            })
            .await
            .unwrap_or_else(|error| panic!("spawned authority task join failed: {error}"))
        })
        .await;

    result.unwrap_or_else(|error| {
        panic!("spawned authority task must inherit the MCP mutation boundary: {error}")
    });
}

#[test]
fn act_facade_lease_acquire_rejects_out_of_range_ttl() {
    let params = ActParams {
        operation: ActOperation::LeaseAcquire,
        action: None,
        reason: None,
        ttl_ms: Some(99),
    };

    let error = validate_act_lease_acquire_params(&params)
        .expect_err("lease_acquire must reject below-min ttl_ms");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "detail_code").as_deref(),
        Some("LEASE_TTL_OUT_OF_RANGE")
    );
    assert_eq!(
        act_error_field(&error, "tool").as_deref(),
        Some("act operation=lease_acquire")
    );
    assert_eq!(act_error_u64(&error, "ttl_ms"), Some(99));
}

#[test]
fn act_facade_operation_schema_is_closed_enum() {
    let schema = sanitized_tool_input_schema("act");
    let operation_schema = schema
        .pointer("/properties/operation")
        .unwrap_or_else(|| panic!("act schema must include operation: {schema}"));
    let enum_schema = operation_schema
        .pointer("/enum")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("ActOperation enum schema missing: {operation_schema}"));
    let enum_values = enum_schema
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    assert!(
        enum_values.contains("invoke")
            && enum_values.contains("foreground")
            && enum_values.contains("lease_acquire")
            && enum_values.contains("lease_status")
            && enum_values.contains("lease_release"),
        "act operation schema must enumerate invoke/foreground/lease operations: {operation_schema}"
    );
}

#[test]
fn act_facade_public_schema_is_operation_specific() {
    let schema = sanitized_tool_input_schema("act");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], Value::Bool(false));
    assert_eq!(
        schema["properties"]["ttl_ms"]["minimum"],
        synapse_action::MIN_LEASE_TTL_MS
    );
    assert_eq!(
        schema["properties"]["ttl_ms"]["maximum"],
        synapse_action::MAX_LEASE_TTL_MS
    );

    let variants = schema["oneOf"]
        .as_array()
        .expect("act schema oneOf present");
    assert_eq!(variants.len(), 5);
    for variant in variants {
        assert_eq!(variant["type"], "object");
        assert_eq!(variant["additionalProperties"], Value::Bool(false));
    }

    let invoke_fields = schema_property_names(act_schema_variant(&schema, "invoke"));
    assert_eq!(
        invoke_fields,
        BTreeSet::from(["action".to_owned(), "operation".to_owned()])
    );

    let foreground = act_schema_variant(&schema, "foreground");
    let foreground_fields = schema_property_names(foreground);
    assert_eq!(
        foreground_fields,
        BTreeSet::from([
            "action".to_owned(),
            "operation".to_owned(),
            "reason".to_owned(),
            "ttl_ms".to_owned()
        ])
    );
    assert_eq!(foreground["properties"]["reason"]["type"], "string");
    assert_eq!(foreground["properties"]["reason"]["minLength"], 1);

    let lease_acquire_fields = schema_property_names(act_schema_variant(&schema, "lease_acquire"));
    assert_eq!(
        lease_acquire_fields,
        BTreeSet::from(["operation".to_owned(), "ttl_ms".to_owned()])
    );
    assert!(!lease_acquire_fields.contains("action"));
    assert!(!lease_acquire_fields.contains("reason"));

    let lease_status_fields = schema_property_names(act_schema_variant(&schema, "lease_status"));
    assert_eq!(
        lease_status_fields,
        BTreeSet::from(["operation".to_owned()])
    );

    let lease_release_fields = schema_property_names(act_schema_variant(&schema, "lease_release"));
    assert_eq!(
        lease_release_fields,
        BTreeSet::from(["operation".to_owned()])
    );
}

#[test]
fn act_facade_invoke_requires_action() {
    let params = ActParams {
        operation: ActOperation::Invoke,
        action: None,
        reason: None,
        ttl_ms: None,
    };

    let error =
        validate_act_invoke_params(&params).expect_err("invoke must require action payload");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "operation").as_deref(),
        Some("invoke")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("action")
    );
}

#[test]
fn act_facade_lease_status_rejects_action_and_ttl() {
    let with_action = ActParams {
        operation: ActOperation::LeaseStatus,
        action: Some(read_action()),
        reason: None,
        ttl_ms: None,
    };
    let action_error = validate_act_lease_read_params(&with_action, ActOperation::LeaseStatus)
        .expect_err("lease_status must not accept target actions");
    assert_eq!(
        act_error_field(&action_error, "source_id").as_deref(),
        Some("action")
    );

    let with_ttl = ActParams {
        operation: ActOperation::LeaseStatus,
        action: None,
        reason: None,
        ttl_ms: Some(1_000),
    };
    let ttl_error = validate_act_lease_read_params(&with_ttl, ActOperation::LeaseStatus)
        .expect_err("lease_status must not accept ttl_ms");
    assert_eq!(
        act_error_field(&ttl_error, "source_id").as_deref(),
        Some("ttl_ms")
    );
}

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
    let locator = target_act_set_field_locator(&params).expect("role/name/automation_id locator");

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
    .expect("cleanup_notepad_tabs params should deserialize through the open target_act schema");

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
    let close_tab =
        target_act_test_accessible_node(12, "Close tab Ctrl+W", "MenuItem", &[UiaPattern::Invoke]);
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
    let params = target_act_focus_window_params(Some(&SessionTarget::Window { hwnd: 0x250a08 }))
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
fn target_act_focus_window_profile_gate_accepts_only_foreground_capability() {
    use crate::server::tool_profiles::ToolProfileKind;

    for profile in [ToolProfileKind::BreakGlass, ToolProfileKind::FullCapability] {
        target_act_focus_window_profile_preflight("session-1379", profile)
            .expect("foreground-capable facade profile must delegate focus");
    }

    for profile in [
        ToolProfileKind::NormalAgent,
        ToolProfileKind::BrowserControl,
        ToolProfileKind::BrowserDebugger,
    ] {
        let error = target_act_focus_window_profile_preflight("session-1379", profile)
            .expect_err("non-foreground profile must fail closed");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PROFILE_POLICY_DENIED)
        );
        assert_eq!(
            act_error_field(&error, "profile").as_deref(),
            Some(profile.as_str())
        );
        assert_eq!(act_error_field(&error, "tool").as_deref(), Some("act"));
        assert_eq!(
            act_error_field(&error, "operation").as_deref(),
            Some("focus_window")
        );
        assert_eq!(
            act_error_field(&error, "session_id").as_deref(),
            Some("session-1379")
        );
        assert_eq!(
            act_error_field(&error, "remediation").as_deref(),
            Some(TARGET_ACT_FOREGROUND_ROUTE_REMEDIATION)
        );
    }
}

#[test]
fn http_profile_admission_denies_raw_target_act_for_every_profile() {
    use crate::server::tool_profiles::ToolProfileKind;

    let temp = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("create target-act policy tempdir: {error}"));
    let service = foreground_guard_test_service(temp.path());
    let session_id = "raw-target-act-policy-1379";
    for profile in [
        ToolProfileKind::NormalAgent,
        ToolProfileKind::BrowserControl,
        ToolProfileKind::BrowserDebugger,
        ToolProfileKind::BreakGlass,
        ToolProfileKind::FullCapability,
    ] {
        service
            .write_tool_profile_assignment(
                session_id,
                profile,
                "raw_target_act_policy_test",
                Some("prove facade-only HTTP admission".to_owned()),
                Some(session_id.to_owned()),
            )
            .unwrap_or_else(|error| panic!("persist {profile:?} policy row: {error:?}"));
        let error = service
            .admit_tool_call_for_profile("target_act", Some(session_id))
            .expect_err("implementation-only target_act must remain HTTP policy denied");
        assert_eq!(
            act_error_field(&error, "code").as_deref(),
            Some(error_codes::TOOL_PROFILE_POLICY_DENIED)
        );
        assert_eq!(
            act_error_field(&error, "profile").as_deref(),
            Some(profile.as_str())
        );
        assert_eq!(
            act_error_field(&error, "tool").as_deref(),
            Some("target_act")
        );
    }
}

#[test]
fn target_act_read_routes_cdp_targets_to_target_info() {
    let target = SessionTarget::Cdp {
        window_hwnd: 0x1234,
        cdp_target_id: "chrome-tab:42".to_owned(),
    };

    assert_eq!(
        target_act_read_delegated_tool(Some(&target)).expect("cdp target routes to target info"),
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
fn target_act_coordinate_space_accepts_nested_position() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "coordinate_space": "window",
        "position": {
            "x": 42,
            "y": 77
        }
    }))
    .expect("nested coordinate position params should deserialize");
    let coordinate = target_act_coordinate(&params)
        .expect("nested coordinate position should validate")
        .expect("coordinate should be present");

    assert_eq!(coordinate.x, 42);
    assert_eq!(coordinate.y, 77);
    assert_eq!(coordinate.space, TargetActCoordinateSpace::Window);
    assert!(target_act_coordinate_uses_nested_position(&params));
    assert_eq!(
        target_act_click_position(&params).expect("DOM click position remains readable"),
        Some((42, 77))
    );
}

#[test]
fn target_act_coordinate_space_rejects_ambiguous_position_sources() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "coordinate_space": "window",
        "x": 42,
        "y": 77,
        "position": {
            "x": 42,
            "y": 77
        }
    }))
    .expect("ambiguous coordinate params should deserialize");
    let error =
        target_act_coordinate(&params).expect_err("mixed x/y and nested position must fail closed");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
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
    let press = target_act_press_params(keys, params.wait_timeout_ms, "key").expect("press params");
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
fn target_act_key_rejects_printable_text_sequence() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "key",
        "keys": ["V", "F", "X"]
    }))
    .expect("key params should deserialize");

    let keys = target_act_key_chord_keys(&params, "key").expect("keys should parse");
    let error = target_act_press_params(keys, params.wait_timeout_ms, "key")
        .expect_err("printable text sequence must fail before act_press dispatch");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert!(
        error
            .message
            .contains("simultaneous chord route, not ordered text input"),
        "error should explain the root cause and route guidance: {error:?}"
    );
}

#[test]
fn target_act_key_allows_modifier_plus_single_printable_key() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "key",
        "key": "Ctrl+F"
    }))
    .expect("key params should deserialize");

    let keys = target_act_key_chord_keys(&params, "key").expect("key chord should parse");
    let press = target_act_press_params(keys, params.wait_timeout_ms, "key").expect("press params");

    assert_eq!(press.keys, vec!["Ctrl", "F"]);
    assert!(press.verify_delta);
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
    let params = target_act_type_params("issue-1267".to_owned(), Some(750), Some(0x1234))
        .expect("target_act type params should construct act_type params");

    assert_eq!(params.text, "issue-1267");
    assert_eq!(params.verify_timeout_ms, 750);
    assert!(params.verify_delta);
    assert_eq!(params.verify_target_window_hwnd, Some(0x1234));
    assert!(params.into_element.is_none());
}

#[test]
fn target_act_type_wait_timeout_is_bounded() {
    let error = target_act_type_params("issue-1267".to_owned(), Some(30_000), None)
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
    let routed = target_act_legacy_click_element_id("chrome-tab:589708698:frame:4970:path:0.1.1")
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
        target_act_test_frame_entry("child-frame", Some("main-frame"), "iframe-target", 1, true),
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
        target_act_test_frame_entry("child-frame", Some("main-frame"), "iframe-target", 1, true),
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
fn target_act_delivery_state_distinguishes_verification_failures() {
    assert_eq!(
        target_act_delivery_state(true, TARGET_ACT_STATUS_OK),
        "delivered_and_verified"
    );
    assert_eq!(
        target_act_delivery_state(false, TARGET_ACT_STATUS_VERIFY_NEEDED),
        "delivered_unverified"
    );
    assert_eq!(
        target_act_delivery_state(false, TARGET_ACT_STATUS_REFUSED),
        "refused_before_delivery"
    );
    assert_eq!(
        target_act_delivery_state(false, TARGET_ACT_STATUS_ERROR),
        "failed_before_delivery"
    );
    assert_eq!(
        target_act_delivery_state_from_result(
            true,
            TARGET_ACT_STATUS_OK,
            &json!({
                "postcondition": {
                    "source_of_truth": "target_window_ui_or_pixels",
                    "observed_delta": true,
                }
            }),
        ),
        "delivered_and_pixel_verified"
    );
}

#[test]
fn target_act_foreground_lost_guidance_uses_public_act_facade() {
    let foreground = synthetic_foreground_context();
    let error = target_act_foreground_lost_error(0x1000, 0x2000, &foreground, false, None);
    let message = error.message.to_string();

    assert!(message.contains("act operation=foreground"));
    assert!(message.contains("same action payload"));
    assert!(!message.contains("control_lease_acquire"));
    assert!(!message.contains("verb=focus_window immediately"));
    assert_eq!(
        act_error_field(&error, "recommended_public_tool").as_deref(),
        Some("act")
    );
    assert_eq!(
        act_error_field(&error, "recommended_public_operation").as_deref(),
        Some("foreground")
    );
    assert_eq!(
        act_error_field(&error, "recommended_public_route").as_deref(),
        Some(TARGET_ACT_FOREGROUND_ROUTE_REMEDIATION)
    );
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

#[test]
fn target_act_secret_safe_result_redacts_page_text_and_values() {
    let result = target_act_secret_safe_result(
        "chrome_debugger_bridge.domAction",
        json!({
            "target_id": "target-1470",
            "tab_id": 1470,
            "action": "click",
            "before_page": {
                "url": "https://example.test/secrets",
                "title": "API keys",
                "ready_state": "complete"
            },
            "after_page": {
                "url": "https://example.test/secrets",
                "title": "API keys",
                "ready_state": "complete"
            },
            "before_page_text": {"text": "existing-key-secret-1470"},
            "after_page_text": {"text": "new-one-time-secret-1470"},
            "action_readback": {
                "selected_text": "copied-secret-1470",
                "after_value": "input-secret-1470",
                "value_len": 17,
                "checked": true
            }
        }),
    )
    .expect("secret-safe result should sanitize");

    let encoded = serde_json::to_string(&result).expect("serialize sanitized result");
    for raw in [
        "existing-key-secret-1470",
        "new-one-time-secret-1470",
        "copied-secret-1470",
        "input-secret-1470",
    ] {
        assert!(
            !encoded.contains(raw),
            "secret-safe result leaked raw field {raw}"
        );
    }
    assert_eq!(
        result.get("secret_safe").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result.pointer("/before_page/url").and_then(Value::as_str),
        Some("https://example.test/secrets")
    );
    assert_eq!(
        result
            .pointer("/after_page_text/redaction_policy")
            .and_then(Value::as_str),
        Some(TARGET_ACT_SECRET_SAFE_REDACTION_POLICY)
    );
    assert_eq!(
        result
            .pointer("/action_readback/value_len")
            .and_then(Value::as_u64),
        Some(17)
    );
}

#[test]
fn target_act_secret_safe_error_redacts_message_and_data() {
    let error = ErrorData::new(
        ErrorCode(-32099),
        "domAction frame_results contained new-one-time-secret-1470".to_owned(),
        Some(json!({
            "code": error_codes::CHROME_DOM_ELEMENT_NOT_FOUND,
            "error_detail": "button near existing-key-secret-1470 was not found",
            "frame_results": [{
                "result": {
                    "in_page_before_text": "existing-key-secret-1470"
                }
            }]
        })),
    );

    let result =
        target_act_secret_safe_error_result_ref("chrome_debugger_bridge.domAction", &error);
    let encoded = serde_json::to_string(&result).expect("serialize sanitized error");
    for raw in [
        "new-one-time-secret-1470",
        "existing-key-secret-1470",
        "button near existing-key-secret-1470 was not found",
    ] {
        assert!(
            !encoded.contains(raw),
            "secret-safe error leaked raw field {raw}"
        );
    }
    assert_eq!(
        result.pointer("/error/code").and_then(Value::as_str),
        Some(error_codes::CHROME_DOM_ELEMENT_NOT_FOUND)
    );
    assert_eq!(
        result
            .pointer("/error/message_redacted")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result
            .pointer("/error/data/error_detail/redaction_policy")
            .and_then(Value::as_str),
        Some(TARGET_ACT_SECRET_SAFE_REDACTION_POLICY)
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
