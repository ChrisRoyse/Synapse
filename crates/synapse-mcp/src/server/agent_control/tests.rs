//! Tests for the `agent_interrupt` / `agent_kill` verbs (#904).
//!
//! The deterministic helpers are unit-tested; the acceptance behaviour
//! (force-killing a real process tree to zero orphans, journaling, command
//! audit, double-kill idempotence, cooperative interrupt delivery) is verified
//! against a REAL spawned OS process through the real code path with the OS
//! process table and the storage column families as the sources of truth — no
//! mocks. The owning Windows job (KILL_ON_JOB_CLOSE) guarantees no orphan
//! survives even if an assertion fails, because the service drop closes it.
//! These checks are supporting real-process regression evidence only; manual
//! FSV remains separate.
//!
//! The real-process acceptance tests below are `#[ignore]`d: they spawn and
//! force-kill real `powershell.exe` victims and assert host-load-sensitive
//! budgets (a process is gone within the 5s kill-confirmation window, a fleet
//! kill returns inside 10s). On a saturated host (e.g. a live daemon plus a
//! concurrent build) `taskkill` confirmation can exceed those budgets, which
//! makes them flaky as a default gate — not because the kill path is wrong.
//! The deterministic logic (id validation, param defaults, tree-exit polling,
//! confirm-token gating, empty-fleet no-op, unknown/dead-session handling)
//! stays in the default gate; run the supporting real-process checks explicitly with
//! `cargo test -p synapse-mcp -- --ignored agent_control`.

use super::*;

fn synthetic_spawn_activity(
    sequence: u64,
    in_flight: u64,
    cleanup_incident: bool,
) -> super::super::m4_tools::AgentSpawnActivityReadback {
    super::super::m4_tools::AgentSpawnActivityReadback {
        sequence,
        in_flight,
        operator_panic_epoch: 7,
        operator_panic_safety_pending: true,
        cleanup_incident,
    }
}

#[test]
fn operator_panic_k2_requires_unchanged_spawn_sequence_and_zero_in_flight() {
    let baseline = synthetic_spawn_activity(10, 1, false);
    let quiescent_same_sequence = synthetic_spawn_activity(10, 0, false);
    assert!(operator_panic_spawn_activity_stable(
        &baseline,
        &quiescent_same_sequence,
        true
    ));

    let entered_during_sweep = synthetic_spawn_activity(11, 0, false);
    assert!(
        !operator_panic_spawn_activity_stable(&baseline, &entered_during_sweep, true),
        "a racing admission must force another fleet-stop round"
    );

    let still_in_flight = synthetic_spawn_activity(10, 1, false);
    assert!(!operator_panic_spawn_activity_stable(
        &baseline,
        &still_in_flight,
        true
    ));

    let cleanup_unverified = synthetic_spawn_activity(10, 0, true);
    assert!(!operator_panic_spawn_activity_stable(
        &baseline,
        &cleanup_unverified,
        true
    ));
    assert!(!operator_panic_spawn_activity_stable(
        &baseline,
        &quiescent_same_sequence,
        false
    ));

    let mutation_zero_before = super::super::operator_panic_boundary::McpMutationActivitySnapshot {
        sequence: 41,
        in_flight: 0,
    };
    let mutation_zero_after = mutation_zero_before;
    assert!(operator_panic_mcp_mutation_activity_stable(
        &mutation_zero_before,
        &mutation_zero_after
    ));
    assert!(!operator_panic_mcp_mutation_activity_stable(
        &mutation_zero_before,
        &super::super::operator_panic_boundary::McpMutationActivitySnapshot {
            sequence: 42,
            in_flight: 0,
        }
    ));
    assert!(!operator_panic_mcp_mutation_activity_stable(
        &mutation_zero_before,
        &super::super::operator_panic_boundary::McpMutationActivitySnapshot {
            sequence: 41,
            in_flight: 1,
        }
    ));
}

fn synthetic_extension_owner_readback(
    disable_sequence: u64,
    command_activity_sequence: u64,
    command_last_completed_sequence: u64,
) -> crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback {
    crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback {
        enabled: false,
        disable_sequence,
        command_activity_sequence,
        command_last_completed_sequence,
        command_in_flight_count: 1,
        mutation_handlers_started_count: 7,
        mutation_handlers_completed_count: 7,
        worker_boot_id: "worker-stable".to_owned(),
        browser_session_id: Some("browser-session-stable".to_owned()),
        ledger_browser_session_id: "browser-session-stable".to_owned(),
        browser_session_continuity_matched: true,
        stale_browser_session_owner_count: 0,
        storage_state_loaded: true,
        storage_state_load_error: None,
        persisted_state_revision: 9,
        persisted_in_flight_mutation: None,
        unresolved_debugger_command_timeouts: Vec::new(),
        unresolved_worker_restart_mutation_count: 0,
        owner_continuity_healthy: true,
        active_after: crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerCounts::default(),
        fully_drained: true,
    }
}

fn synthetic_extension_k2_readback() -> OperatorPanicChromeExtensionOwnersReadback {
    let disable_sequence = 4;
    OperatorPanicChromeExtensionOwnersReadback {
        disable: Some(synthetic_extension_owner_readback(disable_sequence, 17, 16)),
        disable_error: None,
        cleanup: Some(
            crate::chrome_debugger_bridge::ChromeDebuggerExtensionCleanupReadback {
                owner: synthetic_extension_owner_readback(disable_sequence, 18, 17),
                expected_disable_sequence: disable_sequence,
                opened_tabs_found: 0,
                opened_tabs_closed: 0,
                closed_opened_tabs: Vec::new(),
                remaining_opened_tabs: Vec::new(),
                init_scripts_found: 0,
                init_scripts_removed: 0,
                init_script_effects_found: 0,
                init_script_effects_cleared: 0,
                reloaded_init_script_effect_tabs: Vec::new(),
                bindings_found: 0,
                bindings_removed: 0,
                debugger_tabs_found: 0,
                debugger_tabs_detached: 0,
                detached_debugger_tabs: Vec::new(),
                dialog_policies_found: 0,
                dialog_policies_disabled: 0,
                file_chooser_interceptions_found: 0,
                file_chooser_interceptions_disabled: 0,
                clocks_found: 0,
                clocks_uninstalled: 0,
                failures: Vec::new(),
            },
        ),
        cleanup_error: None,
        cdp_target_owner_reconciliation: Some(
            super::super::m1_tools::OperatorPanicCdpTargetOwnerReconciliationReadback {
                successful_physical_closes: 0,
                targets: Vec::new(),
                remaining_extension_memory_owner_keys: Vec::new(),
                remaining_extension_persisted_owner_keys: Vec::new(),
                remaining_owner_readback_error: None,
                failures: Vec::new(),
                terminal: true,
            },
        ),
        cdp_target_owner_reconciliation_error: None,
        before_command_drain: Some(synthetic_extension_owner_readback(disable_sequence, 19, 18)),
        before_command_drain_error: None,
        after_command_drain: Some(synthetic_extension_owner_readback(disable_sequence, 20, 19)),
        after_command_drain_error: None,
        terminal: false,
    }
}

#[test]
fn operator_panic_extension_k2_rejects_worker_restart_and_unresolved_persisted_effects() {
    let clean = synthetic_extension_k2_readback();
    assert!(operator_panic_chrome_extension_terminal(&clean));

    let mut restarted = clean.clone();
    restarted
        .after_command_drain
        .as_mut()
        .expect("synthetic after readback")
        .worker_boot_id = "worker-restarted".to_owned();
    assert!(!operator_panic_chrome_extension_terminal(&restarted));

    let mut unresolved_restart = clean.clone();
    let after = unresolved_restart
        .after_command_drain
        .as_mut()
        .expect("synthetic after readback");
    after.unresolved_worker_restart_mutation_count = 1;
    after.owner_continuity_healthy = false;
    assert!(!operator_panic_chrome_extension_terminal(
        &unresolved_restart
    ));

    let mut persisted_in_flight = clean.clone();
    persisted_in_flight
        .after_command_drain
        .as_mut()
        .expect("synthetic after readback")
        .persisted_in_flight_mutation = Some(serde_json::json!({ "kind": "initScript" }));
    assert!(!operator_panic_chrome_extension_terminal(
        &persisted_in_flight
    ));

    let mut executed_init_effect = clean;
    executed_init_effect
        .after_command_drain
        .as_mut()
        .expect("synthetic after readback")
        .active_after
        .executed_init_script_effect_unresolved_count = 1;
    assert!(!operator_panic_chrome_extension_terminal(
        &executed_init_effect
    ));
}

#[tokio::test]
async fn operator_panic_closed_extension_tabs_reconcile_only_exact_m1_owner_rows()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let service = regression_service(temp.path());
    let extension_endpoint = "chrome-extension://test/chrome.tabs";
    let closed_target_id = "chrome-tab:42";
    let remaining_target_id = "chrome-tab:43";
    let closed_owner_key = service.register_cdp_target_owner(super::super::CdpTargetOwner {
        session_id: "closed-owner-session".to_owned(),
        window_hwnd: 0x4242,
        endpoint: extension_endpoint.to_owned(),
        chrome_window_id: Some(7),
        capture_window_hwnd: None,
        cdp_target_id: closed_target_id.to_owned(),
        requested_url: "about:blank".to_owned(),
        target_url: "about:blank".to_owned(),
        created_at_unix_ms: 42,
    })?;
    let remaining_owner_key = service.register_cdp_target_owner(super::super::CdpTargetOwner {
        session_id: "remaining-owner-session".to_owned(),
        window_hwnd: 0x4343,
        endpoint: extension_endpoint.to_owned(),
        chrome_window_id: Some(7),
        capture_window_hwnd: None,
        cdp_target_id: remaining_target_id.to_owned(),
        requested_url: "about:blank".to_owned(),
        target_url: "about:blank".to_owned(),
        created_at_unix_ms: 43,
    })?;

    let first = service
        .reconcile_operator_panic_extension_target_owners(1, &[(42, closed_target_id.to_owned())])
        .await;
    assert!(
        !first.terminal,
        "an extension owner row without a physical-close pair must remain visible and fail closed"
    );
    assert_eq!(first.targets.len(), 1);
    assert!(first.targets[0].terminal);
    assert!(first.targets[0].memory_owner_keys_after.is_empty());
    assert!(first.targets[0].persisted_owner_keys_after.is_empty());
    assert!(
        first
            .remaining_extension_memory_owner_keys
            .contains(&remaining_owner_key)
    );
    assert!(
        first
            .remaining_extension_persisted_owner_keys
            .contains(&remaining_owner_key)
    );
    assert!(
        !first
            .remaining_extension_memory_owner_keys
            .contains(&closed_owner_key)
    );
    assert!(
        service
            .read_persisted_cdp_target_owners_for_target_id(closed_target_id)?
            .is_empty(),
        "separate CF_SESSIONS readback must prove the physically closed target row absent"
    );
    assert_eq!(
        service
            .read_persisted_cdp_target_owners_for_target_id(remaining_target_id)?
            .len(),
        1,
        "a target without a physical-close pair must retain its persisted owner row"
    );

    let structurally_invalid = service
        .reconcile_operator_panic_extension_target_owners(
            1,
            &[(99, remaining_target_id.to_owned())],
        )
        .await;
    assert!(!structurally_invalid.terminal);
    assert!(!structurally_invalid.targets[0].failures.is_empty());
    assert_eq!(
        service
            .read_persisted_cdp_target_owners_for_target_id(remaining_target_id)?
            .len(),
        1,
        "a structurally inconsistent close pair must never delete an owner row"
    );

    let second = service
        .reconcile_operator_panic_extension_target_owners(
            1,
            &[(43, remaining_target_id.to_owned())],
        )
        .await;
    assert!(second.terminal);
    assert!(second.remaining_extension_memory_owner_keys.is_empty());
    assert!(second.remaining_extension_persisted_owner_keys.is_empty());
    Ok(())
}

#[test]
fn respawn_outer_spawn_guard_rejects_original_panic_after_exact_finalization() {
    let _serial = crate::test_support::lease_serial("respawn_outer_spawn_guard_serial");
    synapse_action::isolate_interrupt_epochs_for_test();
    let before = super::super::m4_tools::agent_spawn_activity_readback();
    let outer = super::super::m4_tools::AgentSpawnInFlightGuard::enter(
        "deterministic_respawn_outer_guard_test",
    )
    .expect("respawn outer guard should arm before the synthetic panic");
    let armed = super::super::m4_tools::agent_spawn_activity_readback();
    assert_eq!(armed.sequence, before.sequence.saturating_add(1));
    assert_eq!(armed.in_flight, before.in_flight.saturating_add(1));

    // Model the historical race precisely: panic fires while respawn is busy
    // killing its prior process, and K2 reaches exact finalization before the
    // replacement-spawn call. Pending is false again, but the entry epoch must
    // remain permanently poisoned for this respawn operation.
    let mut token = synapse_action::request_operator_panic_interrupt();
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut token
    ));
    let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
        synapse_action::complete_operator_panic_safety_generation(token)
            .unwrap_or_else(|detail| panic!("complete synthetic respawn panic: {detail}"))
    else {
        panic!("isolated synthetic respawn panic must own finalization");
    };
    assert!(synapse_action::finish_operator_panic_safety_finalization(
        finalization,
        true
    ));
    assert!(!synapse_action::operator_panic_safety_pending());

    let error = outer
        .ensure("respawn_immediately_before_replacement_spawn")
        .expect_err("the original panic epoch must reject a fresh replacement launch");
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("detail_code"))
            .and_then(Value::as_str),
        Some("AGENT_SPAWN_OPERATOR_PANIC_PREARMED")
    );
    drop(outer);
    let after = super::super::m4_tools::agent_spawn_activity_readback();
    assert_eq!(after.in_flight, before.in_flight);
}

#[test]
fn operator_panic_k2_accepts_exact_or_intermediate_newer_tagged_operator_lease() {
    let operator_lease = synapse_action::LeaseStatus {
        held: true,
        owner_session_id: Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID.to_owned()),
        acquired_at_ms_ago: Some(0),
        renewed_at_ms_ago: Some(0),
        ttl_ms: Some(synapse_action::OPERATOR_PREEMPT_LEASE_TTL_MS),
        expires_in_ms: Some(synapse_action::OPERATOR_PREEMPT_LEASE_TTL_MS),
    };

    assert!(operator_panic_k2_lease_retained(
        1,
        3,
        Some(1),
        &operator_lease
    ));
    assert!(
        operator_panic_k2_lease_retained(1, 3, Some(2), &operator_lease),
        "a third published panic must not invalidate the second K1 lease while the first K2 completes"
    );
    assert!(!operator_panic_k2_lease_retained(
        2,
        3,
        Some(1),
        &operator_lease
    ));
    assert!(!operator_panic_k2_lease_retained(
        1,
        3,
        Some(4),
        &operator_lease
    ));

    let mut agent_lease = operator_lease;
    agent_lease.owner_session_id = Some("agent-session".to_owned());
    assert!(!operator_panic_k2_lease_retained(
        1,
        3,
        Some(2),
        &agent_lease
    ));
}

use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command as StdCommand;
use std::time::Instant;

use synapse_storage::{Db, cf};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::m2::M2ServiceConfig;
use crate::m3::M3ServiceConfig;
use crate::m4::M4ServiceConfig;
use crate::safety::{DisableReport, OperatorHotkeyImmediateReport, ReleaseAllReport};
use crate::server::session_lifecycle::{
    SessionAuditSessionCleanupReport, SessionCdpCleanupReport, SessionClipboardCleanupReport,
    SessionContinuityCleanupReport, SessionInputCleanupReport, SessionProcessCleanupItem,
    SessionProcessCleanupReport, SessionProcessResource, SessionRegistryCleanupReport,
    SessionShellCleanupReport, SessionStoreCleanupReport, SessionSubscriptionCleanupReport,
    SessionTargetCleanupReport, SessionTeardownReport,
};
use crate::server::session_registry::SpawnedAgentRead;
use crate::server::target_claims::TargetClaimCleanupReport;

// ---------------------------------------------------------------------------
// Deterministic unit tests
// ---------------------------------------------------------------------------

#[test]
fn validate_lookup_id_trims_and_rejects_empty() {
    assert_eq!(
        validate_lookup_id("  agent-spawn-123  ", TOOL_AGENT_KILL).unwrap(),
        "agent-spawn-123"
    );
    let err = validate_lookup_id("   ", TOOL_AGENT_INTERRUPT).unwrap_err();
    assert!(
        err.message.contains("must be a non-empty"),
        "unexpected error: {}",
        err.message
    );
}

#[test]
fn kill_params_defaults_are_graceful() {
    let params: AgentKillParams =
        serde_json::from_value(json!({ "session_id": "s-1" })).expect("defaults parse");
    assert_eq!(params.grace_ms, DEFAULT_KILL_GRACE_MS);
    assert!(params.interrupt_first);
}

#[test]
fn kill_params_reject_unknown_fields() {
    let result: Result<AgentKillParams, _> =
        serde_json::from_value(json!({ "session_id": "s-1", "bogus": true }));
    assert!(
        result.is_err(),
        "deny_unknown_fields must reject extra keys"
    );
}

#[test]
fn interrupt_params_reject_unknown_fields() {
    let result: Result<AgentInterruptParams, _> =
        serde_json::from_value(json!({ "session_id": "s-1", "grace_ms": 10 }));
    assert!(
        result.is_err(),
        "agent_interrupt takes no grace_ms; unknown fields must be rejected"
    );
}

#[test]
fn process_readback_of_dead_pid_reports_no_live_processes() {
    // An impossible pid owns no live process tree — the OS process table (the
    // source of truth) backs this, so `live_process_ids` must be empty.
    let target = ResolvedAgent {
        session_id: "session-dead-pid".to_owned(),
        spawn_id: Some("agent-spawn-dead-pid".to_owned()),
        agent_kind: "local-model".to_owned(),
        lifecycle: "test".to_owned(),
        resolution_source: "test".to_owned(),
        dead: false,
        launcher_process_id: 0xFFFF_FFFE,
        agent_process_id: None,
        log_dir: String::new(),
        control: None,
    };
    let readback = process_readback_for_target(&target);
    assert!(
        readback.live_process_ids.is_empty(),
        "a non-existent pid must have zero live processes, got {:?}",
        readback.live_process_ids
    );
}

#[tokio::test(start_paused = true)]
async fn wait_for_tree_exit_returns_immediately_for_empty_tree() {
    let (remaining, waited) = wait_for_tree_exit_async(&[], 5_000).await;
    assert!(remaining.is_empty(), "no pids means nothing remains alive");
    assert_eq!(
        waited, 0,
        "an already-empty tree must return without advancing the Tokio deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn wait_for_tree_exit_reports_survivors_after_grace() {
    // The current process is alive, so `owned_live_process_ids` reports it as a
    // survivor — proving the timeout path returns the still-live pid rather than
    // looping forever.
    let me = std::process::id();
    let (remaining, _waited) = wait_for_tree_exit_async(&[me], 150).await;
    assert_eq!(
        remaining,
        vec![me],
        "a live pid must be reported as a survivor after the grace window"
    );
}

#[tokio::test]
async fn operator_panic_empty_fleet_deletes_prior_lease_row_and_audits() -> anyhow::Result<()> {
    // Build the service FIRST: every `SynapseService` constructor installs the
    // per-thread process-global isolation override (input lease + agent-state
    // tracker) under `#[cfg(test)]` — the established pattern from #1574/#1585.
    // With that override installed before the first lease touch below, every
    // `try_acquire`/`status`/`force_preempt` in this test operates on this
    // thread's hermetic lease cell, so a parallel test mutating the real
    // process-global lease cannot clear/overwrite this test's seeded owner
    // (issue #1600).
    //
    // This replaces the pre-#1585 `lease_serial` band-aid, which serialized only
    // against other `lease_serial` callers and, worse, force-cleared the GLOBAL
    // cell before the override was installed — guarding a cell this test never
    // reads once the service is built.
    let temp = TempDir::new()?;
    let service = regression_service(temp.path());
    synapse_action::isolate_interrupt_epochs_for_test();
    let owner = "session-operator-panic-prior";
    let acquired = match synapse_action::lease::try_acquire(
        owner,
        synapse_action::input_lease_ttl_from_ms(5_000),
    ) {
        synapse_action::LeaseOutcome::Acquired(status)
        | synapse_action::LeaseOutcome::Renewed(status) => status,
        other => anyhow::bail!("owner lease acquire failed unexpectedly: {other:?}"),
    };
    // Preserve the exact successful-acquire readback as K1's synthetic
    // `lease_before`, then preempt immediately. Calling `status()` here made
    // this setup depend on scheduler wall time: a >5 s deschedule could expire
    // the test lease before the preemption even though no parallel writer had
    // touched its isolated cell (#1617). The behavior under test derives the
    // prior owner from `preempted_lease`, not from a second status read.
    let lease_before = acquired.clone();
    let mut operator_panic_token = synapse_action::request_operator_panic_interrupt();
    let operator_panic_generation = operator_panic_token.generation();
    let preempted_lease = synapse_action::force_preempt_input_lease_for_operator_panic(
        "operator_panic_test",
        operator_panic_generation,
    );
    let lease_after_preempt = synapse_action::lease::status();
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut operator_panic_token
    ));
    assert_eq!(lease_before.owner_session_id.as_deref(), Some(owner));
    assert_eq!(
        preempted_lease
            .as_ref()
            .and_then(|status| status.owner_session_id.as_deref()),
        Some(owner)
    );
    assert_eq!(
        lease_after_preempt.owner_session_id.as_deref(),
        Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID),
        "the isolated K1 cell must expose the operator handoff"
    );

    // The K2 trigger below still reads and deletes a real CF_SESSIONS row. Its
    // write occurs after the immutable K1 snapshot so storage contention cannot
    // retroactively change which live owner the hotkey actually preempted.
    service
        .persist_session_lease(owner, &acquired)
        .map_err(|error| anyhow::anyhow!("persist lease failed: {}", error.message))?;
    let immediate = OperatorHotkeyImmediateReport {
        hotkey: synapse_action::hotkey::DEFAULT_OPERATOR_HOTKEY,
        operator_panic_generation,
        lease_before,
        preempted_lease,
        lease_after_preempt,
        disable_report: DisableReport {
            result: "not_initialized",
            disabled_ids: Vec::new(),
            error_code: None,
            detail: None,
        },
        release_all_report: ReleaseAllReport {
            result: "ok",
            error_code: None,
            detail: None,
        },
        durable_browser_mutation_owners_after_disable:
            synapse_a11y::durable_browser_mutation_owners_disable_now(),
        release_all_elapsed_ms: 1,
        elapsed_ms: 1,
        within_budget: true,
        k1_safety_terminal: true,
    };

    let response_result = service.operator_panic_kill_all(immediate).await;
    let lease_after_k2 = synapse_action::lease::status();
    let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
        synapse_action::complete_operator_panic_safety_generation(operator_panic_token)
            .map_err(|detail| anyhow::anyhow!("complete synthetic panic: {detail}"))?
    else {
        anyhow::bail!("isolated synthetic panic must own finalization");
    };
    let _cleared = synapse_action::force_clear_operator_panic_input_lease_generation(
        operator_panic_generation,
        "operator_panic_empty_fleet_test_cleanup",
    );
    assert!(
        !synapse_action::finish_operator_panic_safety_finalization(finalization, true),
        "the deliberately unavailable extension readback must leave a sticky fail-closed incident"
    );
    let browser_owner_disabled = synapse_a11y::durable_browser_mutation_owners_readback();
    let browser_owner_reset = synapse_a11y::durable_browser_mutation_owners_enable_if_unchanged(
        browser_owner_disabled.disable_sequence,
    )
    .await;
    assert!(browser_owner_reset.enabled);
    let response = response_result
        .map_err(|error| anyhow::anyhow!("operator panic failed: {}", error.message))?;

    assert!(
        !response.all_stopped,
        "an empty process fleet must still fail closed when no real extension owner cleanup/readback was available"
    );
    assert!(
        !response.chrome_extension_mutation_owners.terminal,
        "K2 must not synthesize an empty extension-owner verdict without the real extension"
    );
    assert_eq!(
        response.prior_lease_owner_session_id.as_deref(),
        Some(owner)
    );
    let cleanup = response
        .prior_lease_row_cleanup
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing prior lease row cleanup readback"))?;
    assert!(cleanup.row_existed_before);
    assert!(cleanup.row_deleted);
    assert!(!cleanup.row_exists_after);
    assert_eq!(lease_after_k2.held, response.lease_after.held);
    assert_eq!(
        lease_after_k2.owner_session_id,
        response.lease_after.owner_session_id
    );
    assert_eq!(
        response
            .final_safety_sweep
            .as_ref()
            .and_then(|sweep| sweep.operator_lease_generation_after),
        Some(operator_panic_generation)
    );
    assert_eq!(
        lease_after_k2.owner_session_id.as_deref(),
        Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID),
        "K2 must retain the tagged operator lease for the unique finalizer"
    );

    let audit = service
        .command_audit_snapshot()
        .map_err(|error| anyhow::anyhow!("audit snapshot failed: {}", error.message))?;
    let operator_rows = audit
        .rows
        .iter()
        .filter(|row| row.tool == "operator_hotkey")
        .count();
    assert!(
        operator_rows >= 2,
        "operator panic must write intent+final command-audit rows, found {operator_rows}"
    );
    synapse_action::isolate_interrupt_epochs_for_test();
    Ok(())
}

#[test]
fn delivered_via_preserves_highest_ranked_successful_channel() {
    let mut delivered_via = None;
    let codex = ChannelAttempt {
        channel: "codex_app_server_turn_interrupt".to_owned(),
        rank: 1,
        status: "delivered".to_owned(),
        reason: "synthetic rank-1 delivery".to_owned(),
        message_id: Some("turn-1".to_owned()),
        row_key: Some("codex-control.json".to_owned()),
    };
    let mailbox = ChannelAttempt {
        channel: "mailbox_interrupt".to_owned(),
        rank: 3,
        status: "delivered".to_owned(),
        reason: "synthetic cooperative fallback delivery".to_owned(),
        message_id: Some("message-1".to_owned()),
        row_key: Some("agent-mailbox/v1/row".to_owned()),
    };

    record_first_delivered_channel(&mut delivered_via, &codex);
    record_first_delivered_channel(&mut delivered_via, &mailbox);

    assert_eq!(
        delivered_via.as_deref(),
        Some("codex_app_server_turn_interrupt"),
        "lower-ranked mailbox delivery must not overwrite the rank-1 Codex interrupt verdict"
    );
}

fn empty_teardown_report_for_test() -> SessionTeardownReport {
    SessionTeardownReport {
        session_id: "session-teardown-summary-test".to_owned(),
        reason: "agent_kill".to_owned(),
        started_at_unix_ms: 1,
        finished_at_unix_ms: 2,
        already_terminated: false,
        marked_terminated: true,
        termination_marker_failed: false,
        termination_marker_error_message: None,
        input: SessionInputCleanupReport::default(),
        target: SessionTargetCleanupReport::default(),
        continuity: SessionContinuityCleanupReport::default(),
        audit_session: SessionAuditSessionCleanupReport::default(),
        clipboard: SessionClipboardCleanupReport::default(),
        cdp: SessionCdpCleanupReport::default(),
        target_claims: TargetClaimCleanupReport::default(),
        shell: SessionShellCleanupReport::default(),
        processes: SessionProcessCleanupReport::default(),
        subscriptions: SessionSubscriptionCleanupReport::default(),
        session_store: SessionStoreCleanupReport::default(),
        registry: SessionRegistryCleanupReport::default(),
        failure_count: 0,
    }
}

#[test]
fn teardown_failure_summary_absent_for_success_report() {
    let report = empty_teardown_report_for_test();

    assert!(
        summarize_teardown_failures(&report).is_none(),
        "successful teardown reports must not produce a failure summary"
    );
}

#[test]
fn teardown_failure_summary_names_failed_sections_and_resources() {
    let mut report = empty_teardown_report_for_test();
    report.target.failed = true;
    report.target.target_sessions_before = 1;
    report.target.target_sessions_after = 1;
    report.target.error_message = Some("target row still owned".to_owned());
    report.processes.owned_before = 1;
    report.processes.failed = 1;
    report.processes.items.push(SessionProcessCleanupItem {
        tool: "act_spawn_agent".to_owned(),
        pid: 12_345,
        resource_id: Some("agent-spawn-summary-test".to_owned()),
        launch_target: "codex".to_owned(),
        agent_cli: Some("codex".to_owned()),
        registered_at_unix_ms: 1,
        process_ids_before: vec![12_345],
        live_process_ids_before: vec![12_345],
        job_handle_dropped: true,
        natural_exit_wait_ms: 0,
        force_termination_status: Some("terminated".to_owned()),
        completion_status_path: None,
        completion_status_before_cleanup: None,
        completion_artifact_cleanup_status: Some("failed".to_owned()),
        completion_artifact_cleanup_error: Some("completion status file locked".to_owned()),
        desktop_name: None,
        desktop_close_attempted: false,
        desktop_close_succeeded: None,
        desktop_close_error: None,
        desktop_window_process_ids_before: Vec::new(),
        desktop_window_termination_attempted: false,
        desktop_window_termination_status: None,
        desktop_window_process_ids_after: Vec::new(),
        remaining_process_ids_after: Vec::new(),
    });
    report.failure_count = 2;

    let summary = summarize_teardown_failures(&report).expect("summary");
    let sections = summary
        .failed_sections
        .iter()
        .map(|section| section.section.as_str())
        .collect::<Vec<_>>();
    assert_eq!(sections, vec!["target", "processes"]);
    assert!(
        summary.failed_sections[0]
            .detail
            .contains("target row still owned"),
        "target detail must name the failed target cleanup: {:?}",
        summary.failed_sections[0]
    );
    assert!(
        summary.failed_sections[1]
            .detail
            .contains("agent-spawn-summary-test"),
        "process detail must name the failed spawn resource: {:?}",
        summary.failed_sections[1]
    );

    let error = format_teardown_failure_error(&report, &summary);
    assert!(error.contains("target"), "error: {error}");
    assert!(error.contains("processes"), "error: {error}");
    assert!(
        error.contains("teardown_failure_summary"),
        "error must point callers to the structured summary: {error}"
    );
    assert!(
        error.contains("teardown"),
        "error must point callers to the full teardown report: {error}"
    );
}

// ---------------------------------------------------------------------------
// Real-process supporting regression coverage (#904)
// ---------------------------------------------------------------------------

fn regression_service(path: &Path) -> SynapseService {
    SynapseService::try_with_m2_shutdown_reason_and_m3_config(
        CancellationToken::new(),
        "test",
        CancellationToken::new(),
        &M2ServiceConfig::default(),
        M3ServiceConfig::from_cli_parts(
            Some(path.join("db")),
            Some(path.to_path_buf()),
            false,
            "127.0.0.1:0".to_owned(),
            NonZeroUsize::new(4).expect("nonzero"),
            false,
            true,
            None,
            false,
            None,
        ),
        M4ServiceConfig::default(),
    )
    .expect("construct storage-backed service")
}

/// Spawns a benign long-lived process (a 600s sleep) and returns its pid.
/// Killed by the owning job at the latest when the service drops.
#[expect(
    clippy::zombie_processes,
    reason = "the victim's lifecycle is owned by the Windows job object \
              (KILL_ON_JOB_CLOSE / TerminateJobObject via the kill path under test), \
              not by the std::process::Child handle, which we keep only for its pid"
)]
fn spawn_victim() -> u32 {
    let child = StdCommand::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 600",
        ])
        .spawn()
        .expect("spawn victim process");
    child.id()
}

struct VictimGuard {
    pid: u32,
}

impl Drop for VictimGuard {
    fn drop(&mut self) {
        let _ = crate::m4::terminate_owned_process_tree(self.pid);
    }
}

/// Registers a spawned agent (registry row + owned process resource) exactly the
/// way act_spawn_agent does, keyed by the agent's own session id.
fn register_spawned_victim(
    service: &SynapseService,
    session_id: &str,
    spawn_id: &str,
    pid: u32,
    kind: &str,
) {
    register_spawned_victim_with_log_dir(
        service,
        session_id,
        spawn_id,
        pid,
        kind,
        Path::new("C:\\temp\\regression"),
    );
}

/// Like `register_spawned_victim` but with an explicit log dir, so a test can
/// place a real `spawn-manifest.json` for the resolver to read back.
fn register_spawned_victim_with_log_dir(
    service: &SynapseService,
    session_id: &str,
    spawn_id: &str,
    pid: u32,
    kind: &str,
    log_dir: &Path,
) {
    let now = unix_time_ms_now();
    {
        let mut registry = service
            .session_registry_ref()
            .lock()
            .expect("registry lock");
        registry.record_seen(session_id, Some("test".to_owned()), now);
        registry.record_spawned_agent(
            session_id,
            SpawnedAgentRead {
                spawn_id: spawn_id.to_owned(),
                cli: kind.to_owned(),
                launcher_process_id: pid,
                agent_process_id: Some(pid),
                started_by_session_id: Some("operator-regression".to_owned()),
                launched_at_unix_ms: now,
                launch_target: "powershell.exe".to_owned(),
                log_dir: log_dir.display().to_string(),
                template_id: None,
                template_version: None,
                control: None,
            },
            now,
        );
    }
    let job = crate::m4::assign_owned_process_job(pid, "act_spawn_agent", Some(spawn_id))
        .expect("assign owned job to victim");
    service
        .register_session_process_resource(
            SessionProcessResource::new(
                session_id.to_owned(),
                "act_spawn_agent",
                pid,
                Some(spawn_id.to_owned()),
                "powershell.exe".to_owned(),
                job,
            )
            .with_agent_cli(kind),
        )
        .expect("register session process resource");
}

fn journal_spawn_ready_only(
    service: &SynapseService,
    session_id: &str,
    spawn_id: &str,
    pid: u32,
    kind: &str,
    log_dir: &Path,
) {
    let db = service.agent_control_db().expect("open storage");
    let mut record = AgentEventRecord::new(
        crate::server::agent_events::unix_time_ns_now(),
        AgentEventKind::SpawnReady,
    );
    record.spawn_id = Some(spawn_id.to_owned());
    record.session_id = Some(session_id.to_owned());
    record.attributes.agent_name = Some(kind.to_owned());
    record.payload = json!({
        "launcher_process_id": pid,
        "agent_process_id": pid,
        "log_dir": log_dir.display().to_string(),
    });
    crate::server::agent_events::record_agent_event(&db, &record).expect("journal spawn_ready");
}

fn journal_count_kind(db: &Db, session_id: &str, kind: AgentEventKind) -> usize {
    let (rows, _more) = db
        .scan_cf_from(cf::CF_AGENT_EVENTS, &[], 1_000_000)
        .expect("scan CF_AGENT_EVENTS");
    rows.iter()
        .filter_map(|(_key, value)| serde_json::from_slice::<AgentEventRecord>(value).ok())
        .filter(|record| record.kind == kind && record.session_id.as_deref() == Some(session_id))
        .count()
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: force-kills a real OS process within a host-load-sensitive 5s confirmation budget; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn agent_kill_terminates_real_process_tree_and_journals_killed() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-kill-1";
    let spawn = "agent-spawn-regression-kill-1";
    let pid = spawn_victim();
    register_spawned_victim(&service, session, spawn, pid, "local-model");

    // BEFORE: the process is alive in the OS process table (source of truth).
    assert!(
        crate::m4::process_exists(pid),
        "precondition: victim pid {pid} must be alive before the kill"
    );

    let response = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("agent_kill must succeed");

    // Readback 1: the tool reports the kill with zero orphans.
    assert!(response.killed, "agent_kill must report killed=true");
    assert!(
        response.orphan_process_ids.is_empty(),
        "no orphan processes may remain: {:?}",
        response.orphan_process_ids
    );
    assert!(!response.already_dead, "the victim was alive when killed");
    assert_eq!(response.session_id, session);
    assert_eq!(response.spawn_id.as_deref(), Some(spawn));
    let teardown_item = response
        .teardown
        .as_ref()
        .and_then(|report| report.processes.items.first())
        .expect("agent_kill must include process cleanup readback");
    assert_eq!(
        teardown_item.natural_exit_wait_ms, 0,
        "explicit kill must not spend the fixed act_spawn_agent completion grace"
    );

    // Readback 2: AFTER — the OS process table, read back independently, confirms the
    // pid is gone. This is the authoritative proof, not the return value.
    assert!(
        !crate::m4::process_exists(pid),
        "victim pid {pid} must be gone from the OS process table after the kill"
    );

    // Readback 3: the durable killed event is physically present in CF_AGENT_EVENTS.
    let db = service.agent_control_db().expect("open storage");
    assert_eq!(
        journal_count_kind(&db, session, AgentEventKind::Killed),
        1,
        "exactly one Killed journal row must exist for {session}"
    );
    assert!(
        response.journal_killed_event.is_some(),
        "the response must carry the killed journal readback"
    );

    // Readback 4: command audit rows for agent_kill are physically present in
    // CF_ACTION_LOG (intent + final).
    let audit = service.command_audit_snapshot().expect("audit snapshot");
    let kill_rows = audit
        .rows
        .iter()
        .filter(|row| row.tool == "agent_kill")
        .count();
    assert!(
        kill_rows >= 2,
        "expected intent+final agent_kill audit rows, found {kill_rows}"
    );
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: force-kills a real OS process within a host-load-sensitive 5s confirmation budget; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn agent_kill_resolves_restart_rebuilt_spawn_from_agent_state() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-kill-restart-rebuilt";
    let spawn = "agent-spawn-regression-kill-restart-rebuilt";
    let pid = spawn_victim();
    let _guard = VictimGuard { pid };
    let log_dir = temp.path().join(spawn);
    std::fs::create_dir_all(&log_dir).expect("create spawn log dir");
    std::fs::write(
        log_dir.join("completion-status.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": spawn,
            "cli": "local-model",
            "status": "running"
        }))
        .expect("encode running completion status"),
    )
    .expect("write running completion status");

    // Simulate a daemon restart: the durable journal rebuilt agent_state, but
    // the volatile session registry/process-resource job ledger has no spawned
    // metadata or job handle for the reconnected MCP session.
    journal_spawn_ready_only(&service, session, spawn, pid, "local-model", &log_dir);
    {
        let mut registry = service
            .session_registry_ref()
            .lock()
            .expect("registry lock");
        registry.record_seen(
            session,
            Some("tools/call:health".to_owned()),
            unix_time_ms_now(),
        );
    }
    assert!(
        crate::m4::process_exists(pid),
        "precondition: victim pid {pid} must be alive before the fallback kill"
    );

    let response = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("agent_kill must resolve the durable state row");

    assert_eq!(response.session_id, session);
    assert_eq!(response.spawn_id.as_deref(), Some(spawn));
    assert_eq!(response.resolution_source, "durable_agent_state");
    assert!(
        response
            .post_teardown_force_termination
            .as_ref()
            .is_some_and(|read| read.attempted),
        "restart fallback must perform an exact process-tree termination"
    );
    assert!(
        response
            .teardown
            .as_ref()
            .is_some_and(|report| report.processes.owned_before == 0),
        "restart simulation must have no live process-resource job ledger"
    );
    assert!(response.killed, "agent_kill must report killed=true");
    assert!(
        response.orphan_process_ids.is_empty(),
        "no orphan processes may remain: {:?}",
        response.orphan_process_ids
    );
    assert!(
        !crate::m4::process_exists(pid),
        "victim pid {pid} must be gone from the OS process table after fallback kill"
    );
    let completion_status: Value = serde_json::from_slice(
        &std::fs::read(log_dir.join("completion-status.json"))
            .expect("read completion status after kill"),
    )
    .expect("decode completion status after kill");
    assert_eq!(
        completion_status.get("status").and_then(Value::as_str),
        Some("agent_kill_forced_after_daemon_restart")
    );
    assert_eq!(
        response.completion_artifact_cleanup_status.as_deref(),
        Some("agent_kill_forced_after_daemon_restart")
    );
}

#[test]
fn restart_kill_completion_artifact_overwrites_wrapper_fallback_race() {
    let temp = TempDir::new().expect("temp dir");
    let spawn = "agent-spawn-regression-wrapper-fallback-race";
    let log_dir = temp.path().join(spawn);
    std::fs::create_dir_all(&log_dir).expect("create spawn log dir");
    std::fs::write(
        log_dir.join("final-message.txt"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": spawn,
            "status": "failed",
            "message": "wrapper fallback"
        }))
        .expect("encode wrapper final message"),
    )
    .expect("write wrapper final message");
    std::fs::write(
        log_dir.join("completion-status.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": spawn,
            "cli": "claude",
            "status": "failed",
            "exit_code": 1,
            "error_message": "spawned agent CLI exited with code 1",
            "final_message_source": "wrapper_fallback_json",
            "fallback_final_message_written": true
        }))
        .expect("encode wrapper completion status"),
    )
    .expect("write wrapper completion status");
    let target = ResolvedAgent {
        session_id: "session-regression-wrapper-fallback-race".to_owned(),
        spawn_id: Some(spawn.to_owned()),
        agent_kind: "claude".to_owned(),
        lifecycle: "agent_state:stuck".to_owned(),
        resolution_source: "durable_agent_state".to_owned(),
        dead: false,
        launcher_process_id: 1234,
        agent_process_id: Some(5678),
        log_dir: log_dir.display().to_string(),
        control: None,
    };
    let process_before = ProcessReadback {
        launcher_process_id: 1234,
        process_tree_ids: vec![1234, 5678],
        live_process_ids: vec![1234, 5678],
    };

    let status = write_agent_kill_restart_completion_artifact(&target, &process_before, &[], None)
        .expect("restart kill artifact rewrite succeeds");

    assert_eq!(status, "agent_kill_forced_after_daemon_restart");
    let completion_status: Value = serde_json::from_slice(
        &std::fs::read(log_dir.join("completion-status.json"))
            .expect("read rewritten completion status"),
    )
    .expect("decode rewritten completion status");
    assert_eq!(
        completion_status.get("status").and_then(Value::as_str),
        Some("agent_kill_forced_after_daemon_restart")
    );
    assert_eq!(
        completion_status
            .get("final_message_source")
            .and_then(Value::as_str),
        Some("agent_kill_restart_artifact_json")
    );
    let final_message: Value = serde_json::from_slice(
        &std::fs::read(log_dir.join("final-message.txt")).expect("read rewritten final message"),
    )
    .expect("decode rewritten final message");
    assert_eq!(
        final_message.get("status").and_then(Value::as_str),
        Some("agent_kill_forced_after_daemon_restart")
    );
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: force-kills a real OS process within a host-load-sensitive 5s confirmation budget; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn agent_kill_is_idempotent_double_kill_reports_already_dead() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-kill-2";
    let spawn = "agent-spawn-regression-kill-2";
    let pid = spawn_victim();
    register_spawned_victim(&service, session, spawn, pid, "local-model");

    let first = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("first kill succeeds");
    assert!(first.killed && !first.already_dead);
    assert!(!crate::m4::process_exists(pid));

    // Second kill: the agent is already dead — idempotent success, no new
    // Killed event (nothing was force-terminated this time).
    let second = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("second kill is idempotent, not an error");
    assert!(second.already_dead, "second kill must report already_dead");
    assert!(second.killed, "already-dead is still a successful kill");
    assert!(
        second.journal_killed_event.is_none(),
        "no force-kill happened, so no new Killed event"
    );
}

#[tokio::test]
async fn agent_kill_dead_unlinked_spawn_reports_already_dead() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let spawn = "agent-spawn-regression-dead-unlinked";
    let db = service.agent_control_db().expect("open storage");
    let mut exited = AgentEventRecord::new(
        crate::server::agent_events::unix_time_ns_now(),
        AgentEventKind::Exited,
    );
    exited.spawn_id = Some(spawn.to_owned());
    exited.reason_code = Some("local_model_registry_row_missing".to_owned());
    exited.end_state = Some(AgentEndState::Error);
    exited.attributes.agent_name = Some("local-model".to_owned());
    crate::server::agent_events::record_agent_event(&db, &exited)
        .expect("journal terminal unlinked spawn");

    let response = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: spawn.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("terminal unlinked spawn must be an idempotent kill success");

    assert_eq!(response.session_id, spawn);
    assert_eq!(response.spawn_id.as_deref(), Some(spawn));
    assert_eq!(response.resolution_source, "durable_agent_state");
    assert!(response.already_dead, "dead setup failure is already dead");
    assert!(response.killed, "already-dead is still a successful kill");
    assert!(response.process_before.live_process_ids.is_empty());
    assert_eq!(response.process_before.launcher_process_id, 0);
    assert!(
        response.journal_killed_event.is_none(),
        "nothing was force-terminated, so no Killed journal row is added"
    );
}

#[tokio::test]
async fn agent_kill_unknown_session_errors_structurally() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let error = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: "session-does-not-exist".to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect_err("unknown session must error");
    assert!(
        error.message.contains("AGENT_NOT_FOUND"),
        "unexpected error: {}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// agent_steer (#905) — deterministic param coverage + supporting real-process evidence
// ---------------------------------------------------------------------------

#[test]
fn steer_params_default_requests_receipt() {
    let params: AgentSteerParams =
        serde_json::from_value(json!({ "session_id": "s-1", "instruction": "tighten scope" }))
            .expect("defaults parse");
    assert!(
        params.request_receipt,
        "request_receipt defaults on so delivery becomes provable consumption"
    );
}

#[test]
fn steer_params_reject_unknown_fields() {
    let result: Result<AgentSteerParams, _> =
        serde_json::from_value(json!({ "session_id": "s-1", "instruction": "x", "grace_ms": 10 }));
    assert!(
        result.is_err(),
        "agent_steer takes no grace_ms; unknown fields must be rejected"
    );
}

#[tokio::test]
async fn steer_rejects_empty_and_oversized_instruction_before_resolution() {
    // Instruction validation precedes agent resolution, so these error paths
    // need no live process — they are deterministic default-gate coverage.
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());

    let empty = service
        .agent_steer_impl(
            AgentSteerParams {
                session_id: "session-steer-validation".to_owned(),
                instruction: "   ".to_owned(),
                request_receipt: true,
            },
            Some("operator-regression"),
        )
        .expect_err("empty instruction must error");
    assert!(
        empty.message.contains("AGENT_STEER_EMPTY"),
        "unexpected error: {}",
        empty.message
    );

    let huge = "x".repeat(MAX_STEER_INSTRUCTION_CHARS + 1);
    let oversized = service
        .agent_steer_impl(
            AgentSteerParams {
                session_id: "session-steer-validation".to_owned(),
                instruction: huge,
                request_receipt: true,
            },
            Some("operator-regression"),
        )
        .expect_err("oversized instruction must error");
    assert!(
        oversized.message.contains("AGENT_STEER_TOO_LONG"),
        "unexpected error: {}",
        oversized.message
    );
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: spawns a real OS process victim; host-load-sensitive; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn agent_steer_delivers_cooperative_mailbox_and_journals_message_sent() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-steer-1";
    let spawn = "agent-spawn-regression-steer-1";
    let pid = spawn_victim();
    register_spawned_victim(&service, session, spawn, pid, "local-model");

    let instruction = "Stop refactoring and write the failing test first.";
    let response = service
        .agent_steer_impl(
            AgentSteerParams {
                session_id: session.to_owned(),
                instruction: instruction.to_owned(),
                request_receipt: true,
            },
            Some("operator-regression"),
        )
        .expect("steer must deliver via the mailbox channel");

    // Readback 1: delivery is via the one wired channel; the other three are
    // honestly reported unavailable — never faked.
    assert!(response.delivered, "steer must be delivered");
    assert_eq!(response.delivered_via.as_deref(), Some("mailbox_steer"));
    assert_eq!(response.instruction_chars, instruction.chars().count());
    assert_eq!(
        response.channels.len(),
        4,
        "all four ranked channels reported"
    );
    let delivered: Vec<&str> = response
        .channels
        .iter()
        .filter(|c| c.status == "delivered")
        .map(|c| c.channel.as_str())
        .collect();
    assert_eq!(delivered, vec!["mailbox_steer"]);
    assert_eq!(
        response
            .channels
            .iter()
            .filter(|c| c.status == "unavailable")
            .count(),
        3,
        "codex/claude/pty channels are unavailable"
    );
    assert_eq!(
        response.receipt_box_session_id.as_deref(),
        Some("operator-regression"),
        "a receipt was requested, so the caller's receipt box is named"
    );

    // Readback 2: the durable steer mailbox row is physically present in CF_KV,
    // and its persisted payload carries the exact instruction (the SoT for what
    // was injected).
    let db = service.agent_control_db().expect("open storage");
    let mailbox_channel = response
        .channels
        .iter()
        .find(|c| c.channel == "mailbox_steer")
        .expect("mailbox channel present");
    let row_key = mailbox_channel
        .row_key
        .as_ref()
        .expect("delivered mailbox row has a key");
    let (rows, _more) = db
        .scan_cf_from(cf::CF_KV, row_key.as_bytes(), 1)
        .expect("scan CF_KV for the mailbox row");
    let (key, value) = rows
        .first()
        .expect("the durable steer mailbox row must exist");
    assert_eq!(
        key.as_slice(),
        row_key.as_bytes(),
        "row key matches at {row_key}"
    );
    let stored: Value = serde_json::from_slice(value).expect("steer row is JSON");
    let stored_instruction = stored
        .pointer("/payload/instruction")
        .and_then(Value::as_str)
        .expect("persisted steer row carries the instruction");
    assert_eq!(
        stored_instruction, instruction,
        "the persisted instruction must be byte-identical to what was sent"
    );

    // Readback 3: exactly one MessageSent journal row exists for the steer.
    assert_eq!(
        journal_count_kind(&db, session, AgentEventKind::MessageSent),
        1,
        "exactly one MessageSent journal row must exist"
    );

    // Clean up the still-live victim deterministically.
    let _ = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("cleanup kill");
    assert!(!crate::m4::process_exists(pid));
}

// ---------------------------------------------------------------------------
// agent_pause / agent_resume (#906) — deterministic param coverage + supporting regression evidence
// ---------------------------------------------------------------------------

#[test]
fn pause_params_reject_unknown_fields() {
    let result: Result<AgentPauseParams, _> =
        serde_json::from_value(json!({ "session_id": "s-1", "grace_ms": 10 }));
    assert!(
        result.is_err(),
        "agent_pause/agent_resume take only session_id; unknown fields must be rejected"
    );
}

#[tokio::test]
async fn pause_unknown_session_errors_structurally() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let error = service
        .agent_pause_impl(
            AgentPauseParams {
                session_id: "session-does-not-exist".to_owned(),
            },
            Some("operator-regression"),
        )
        .expect_err("unknown session must error");
    assert!(
        error.message.contains("AGENT_NOT_FOUND"),
        "unexpected error: {}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// agent_respawn (#906) — pure-logic units + validation coverage
// ---------------------------------------------------------------------------

#[test]
fn respawn_cli_serde_token_maps_stored_kinds() {
    // The manifest/registry store the `as_str` hyphen form; the spawn request
    // deserializes the snake_case serde token. The mapping must bridge them.
    assert_eq!(spawn_cli_serde_token("local-model"), Some("local_model"));
    assert_eq!(spawn_cli_serde_token("local_model"), Some("local_model"));
    assert_eq!(spawn_cli_serde_token("Codex"), Some("codex"));
    assert_eq!(spawn_cli_serde_token("claude"), Some("claude"));
    assert_eq!(spawn_cli_serde_token("nonsense"), None);
}

#[test]
fn respawn_final_message_is_trimmed_and_bounded() {
    let temp = TempDir::new().expect("temp dir");
    let log_dir = temp.path();
    // No file -> None.
    assert!(read_prior_final_message(&log_dir.display().to_string()).is_none());
    // Empty/whitespace file -> None.
    std::fs::write(log_dir.join("final-message.txt"), "   \n  ").expect("write");
    assert!(read_prior_final_message(&log_dir.display().to_string()).is_none());
    // Normal content is returned trimmed.
    std::fs::write(log_dir.join("final-message.txt"), "  done: shipped X  ").expect("write");
    assert_eq!(
        read_prior_final_message(&log_dir.display().to_string()).as_deref(),
        Some("done: shipped X")
    );
    // Oversized content is bounded to 4000 chars.
    std::fs::write(log_dir.join("final-message.txt"), "y".repeat(10_000)).expect("write");
    let bounded = read_prior_final_message(&log_dir.display().to_string()).expect("some");
    assert_eq!(bounded.chars().count(), 4_000);
}

#[test]
fn respawn_params_reject_unknown_fields() {
    let result: Result<AgentRespawnParams, _> =
        serde_json::from_value(json!({ "session_id": "s-1", "prompt": "go", "bogus": true }));
    assert!(
        result.is_err(),
        "deny_unknown_fields must reject extra keys"
    );
}

#[test]
fn respawn_plan_empty_prompt_errors_before_resolution() {
    // The plan validates the prompt before resolving, so this needs no agent.
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let error = service
        .agent_respawn_plan(&AgentRespawnParams {
            session_id: "session-anything".to_owned(),
            prompt: "   ".to_owned(),
            carry_context: true,
            grace_ms: 0,
        })
        .expect_err("empty prompt must error");
    assert!(
        error.message.contains("AGENT_RESPAWN_PROMPT_REQUIRED"),
        "unexpected error: {}",
        error.message
    );
}

#[test]
fn respawn_plan_unknown_session_errors_structurally() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let error = service
        .agent_respawn_plan(&AgentRespawnParams {
            session_id: "session-does-not-exist".to_owned(),
            prompt: "continue the task".to_owned(),
            carry_context: false,
            grace_ms: 0,
        })
        .expect_err("unknown session must error");
    assert!(
        error.message.contains("AGENT_NOT_FOUND"),
        "unexpected error: {}",
        error.message
    );
}

#[test]
fn respawn_manifest_reads_working_dir_from_physical_manifest() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let spawn = "agent-spawn-regression-respawn-manifest";
    let working_dir = temp.path().join("prior-cwd");
    let log_dir = temp.path().join("respawn-log");
    std::fs::create_dir_all(&working_dir).expect("mkdir working dir");
    std::fs::create_dir_all(&log_dir).expect("mkdir log dir");
    std::fs::write(
        log_dir.join("spawn-manifest.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "spawn_id": spawn,
            "cli": "local-model",
            "kind": "local-model",
            "model": "gemma3",
            "model_ref": "gemma-local",
            "working_dir": working_dir.display().to_string(),
        }))
        .expect("encode manifest"),
    )
    .expect("write manifest");
    let target = ResolvedAgent {
        session_id: "session-regression-respawn-manifest".to_owned(),
        spawn_id: Some(spawn.to_owned()),
        agent_kind: "local-model".to_owned(),
        lifecycle: "test".to_owned(),
        resolution_source: "test".to_owned(),
        dead: true,
        launcher_process_id: 0,
        agent_process_id: None,
        log_dir: log_dir.display().to_string(),
        control: None,
    };

    let manifest = service
        .read_respawn_manifest(&target)
        .expect("manifest must read the durable cwd");
    let expected_working_dir = std::fs::canonicalize(&working_dir)
        .expect("canonical working dir")
        .display()
        .to_string();
    assert_eq!(manifest.agent_kind, "local-model");
    assert_eq!(manifest.model.as_deref(), Some("gemma3"));
    assert_eq!(manifest.model_ref.as_deref(), Some("gemma-local"));
    assert_eq!(
        manifest.working_dir.as_deref(),
        Some(expected_working_dir.as_str())
    );
}

#[test]
fn respawn_manifest_missing_working_dir_errors_loudly() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let spawn = "agent-spawn-regression-respawn-missing-cwd";
    let log_dir = temp.path().join("respawn-log");
    std::fs::create_dir_all(&log_dir).expect("mkdir log dir");
    std::fs::write(
        log_dir.join("spawn-manifest.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "spawn_id": spawn,
            "cli": "local-model",
            "kind": "local-model",
            "model": "gemma3",
        }))
        .expect("encode manifest"),
    )
    .expect("write manifest");
    let target = ResolvedAgent {
        session_id: "session-regression-respawn-missing-cwd".to_owned(),
        spawn_id: Some(spawn.to_owned()),
        agent_kind: "local-model".to_owned(),
        lifecycle: "test".to_owned(),
        resolution_source: "test".to_owned(),
        dead: true,
        launcher_process_id: 0,
        agent_process_id: None,
        log_dir: log_dir.display().to_string(),
        control: None,
    };

    let error = service
        .read_respawn_manifest(&target)
        .expect_err("missing cwd must fail before respawn can kill");
    assert!(
        error.message.contains("AGENT_RESPAWN_WORKING_DIR_MISSING"),
        "unexpected error: {}",
        error.message
    );
}

#[test]
fn respawn_manifest_invalid_working_dir_errors_before_spawn() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let spawn = "agent-spawn-regression-respawn-invalid-cwd";
    let missing_working_dir = temp.path().join("missing-cwd");
    let log_dir = temp.path().join("respawn-log");
    std::fs::create_dir_all(&log_dir).expect("mkdir log dir");
    std::fs::write(
        log_dir.join("spawn-manifest.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "spawn_id": spawn,
            "cli": "local-model",
            "kind": "local-model",
            "working_dir": missing_working_dir.display().to_string(),
        }))
        .expect("encode manifest"),
    )
    .expect("write manifest");
    let target = ResolvedAgent {
        session_id: "session-regression-respawn-invalid-cwd".to_owned(),
        spawn_id: Some(spawn.to_owned()),
        agent_kind: "local-model".to_owned(),
        lifecycle: "test".to_owned(),
        resolution_source: "test".to_owned(),
        dead: true,
        launcher_process_id: 0,
        agent_process_id: None,
        log_dir: log_dir.display().to_string(),
        control: None,
    };

    let error = service
        .read_respawn_manifest(&target)
        .expect_err("invalid cwd must fail before respawn can kill");
    assert!(
        error.message.contains("AGENT_RESPAWN_WORKING_DIR_INVALID"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: registers a real spawned victim to exercise manifest reconstruction; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn respawn_plan_reconstructs_identity_from_physical_manifest() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-respawn-1";
    let spawn = "agent-spawn-regression-respawn-1";
    let pid = spawn_victim();
    let _guard = VictimGuard { pid };

    // Register the victim with a log dir that holds a real spawn-manifest.json.
    let log_dir = temp.path().join("respawn-log");
    std::fs::create_dir_all(&log_dir).expect("mkdir log dir");
    std::fs::write(
        log_dir.join("spawn-manifest.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "spawn_id": spawn,
            "cli": "local-model",
            "kind": "local-model",
            "model": "gemma3",
            "model_ref": "gemma-local",
            "working_dir": temp.path().display().to_string(),
        }))
        .expect("encode manifest"),
    )
    .expect("write manifest");
    std::fs::write(log_dir.join("final-message.txt"), "halfway through step 3")
        .expect("write final message");
    register_spawned_victim_with_log_dir(&service, session, spawn, pid, "local-model", &log_dir);

    // No side effects: plan only reads the prior state.
    let plan = service
        .agent_respawn_plan(&AgentRespawnParams {
            session_id: session.to_owned(),
            prompt: "finish step 3 and write the test".to_owned(),
            carry_context: true,
            grace_ms: 0,
        })
        .expect("plan must reconstruct from the physical manifest");

    // The reconstructed identity must come from the manifest on disk.
    assert_eq!(plan.manifest.agent_kind, "local-model");
    assert_eq!(plan.manifest.model.as_deref(), Some("gemma3"));
    assert_eq!(plan.manifest.model_ref.as_deref(), Some("gemma-local"));
    let expected_working_dir = std::fs::canonicalize(temp.path())
        .expect("canonical temp dir")
        .display()
        .to_string();
    assert_eq!(
        plan.manifest.working_dir.as_deref(),
        Some(expected_working_dir.as_str())
    );
    assert_eq!(plan.request_value["cli"], json!("local_model"));
    assert_eq!(plan.request_value["model"], json!("gemma3"));
    assert_eq!(plan.request_value["model_ref"], json!("gemma-local"));
    // Continuity packet folds in the prior final message + the continued task.
    assert!(plan.carried_context);
    assert!(plan.effective_prompt.contains("Respawn continuity"));
    assert!(plan.effective_prompt.contains(spawn));
    assert!(plan.effective_prompt.contains("halfway through step 3"));
    assert!(
        plan.effective_prompt
            .contains("finish step 3 and write the test")
    );

    // Cleanup.
    let _ = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("cleanup kill");
    assert!(!crate::m4::process_exists(pid));
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: suspends/resumes a real OS process and reads the thread table back; host-load-sensitive; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn agent_pause_resume_freezes_real_process_tree_and_is_idempotent() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-pause-1";
    let spawn = "agent-spawn-regression-pause-1";
    let pid = spawn_victim();
    let _guard = VictimGuard { pid };
    register_spawned_victim(&service, session, spawn, pid, "local-model");

    // Baseline: the live process has running threads, none suspended.
    let baseline = crate::m4::process_tree_suspend_state(&[pid]);
    assert!(
        baseline.iter().any(|s| s.total_threads > 0),
        "victim must have live threads before pause: {baseline:?}"
    );
    assert!(
        baseline.iter().all(|s| s.suspended_threads == 0),
        "victim must not be suspended before pause: {baseline:?}"
    );

    // Pause: every thread must be suspended afterwards (physical SoT).
    let paused = service
        .agent_pause_impl(
            AgentPauseParams {
                session_id: session.to_owned(),
            },
            Some("operator-regression"),
        )
        .expect("pause must fully suspend the tree");
    assert!(paused.ok && paused.is_suspended_after && !paused.no_op);
    assert!(
        paused.journal_event.is_some(),
        "a state change must journal a StateChanged row"
    );
    assert!(
        paused
            .suspend
            .states_after
            .iter()
            .all(|s| s.fully_suspended && s.suspended_threads == s.total_threads),
        "every thread must be suspended: {:?}",
        paused.suspend.states_after
    );

    // Independent physical readback of the OS thread table confirms suspension.
    let observed = crate::m4::process_tree_suspend_state(&paused.suspend.live_process_ids);
    assert!(
        observed
            .iter()
            .all(|s| s.total_threads > 0 && s.fully_suspended),
        "independent thread-table read must show the tree frozen: {observed:?}"
    );

    // Pause again: idempotent no-op (must not stack a second suspend count).
    let repaused = service
        .agent_pause_impl(
            AgentPauseParams {
                session_id: session.to_owned(),
            },
            Some("operator-regression"),
        )
        .expect("second pause is a no-op");
    assert!(repaused.no_op && repaused.ok && repaused.is_suspended_after);
    assert!(
        repaused.journal_event.is_none(),
        "a no-op must not journal a StateChanged row"
    );

    // Resume: every thread must be running again (one resume suffices because
    // pause never stacked).
    let resumed = service
        .agent_resume_impl(
            AgentPauseParams {
                session_id: session.to_owned(),
            },
            Some("operator-regression"),
        )
        .expect("resume must fully thaw the tree");
    assert!(resumed.ok && !resumed.is_suspended_after && !resumed.no_op);
    let observed_running = crate::m4::process_tree_suspend_state(&resumed.suspend.live_process_ids);
    assert!(
        observed_running.iter().all(|s| s.suspended_threads == 0),
        "independent thread-table read must show the tree running: {observed_running:?}"
    );

    // Resume again: idempotent no-op.
    let reresumed = service
        .agent_resume_impl(
            AgentPauseParams {
                session_id: session.to_owned(),
            },
            Some("operator-regression"),
        )
        .expect("second resume is a no-op");
    assert!(reresumed.no_op && reresumed.ok);

    // Cleanup.
    let _ = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("cleanup kill");
    assert!(!crate::m4::process_exists(pid));
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: spawns a real OS process victim; host-load-sensitive; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn agent_interrupt_delivers_cooperative_mailbox_and_journals_interrupted() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let session = "session-regression-interrupt-1";
    let spawn = "agent-spawn-regression-interrupt-1";
    let pid = spawn_victim();
    register_spawned_victim(&service, session, spawn, pid, "local-model");

    let response = service
        .agent_interrupt_impl(
            AgentInterruptParams {
                session_id: session.to_owned(),
            },
            Some("operator-regression"),
        )
        .expect("interrupt must deliver via the mailbox channel");

    // Readback 1: delivery is via the one wired channel; the other three are honestly
    // reported unavailable — never faked.
    assert!(response.delivered, "interrupt must be delivered");
    assert_eq!(response.delivered_via.as_deref(), Some("mailbox_interrupt"));
    assert_eq!(
        response.channels.len(),
        4,
        "all four ranked channels reported"
    );
    let delivered: Vec<&str> = response
        .channels
        .iter()
        .filter(|c| c.status == "delivered")
        .map(|c| c.channel.as_str())
        .collect();
    assert_eq!(delivered, vec!["mailbox_interrupt"]);
    let unavailable = response
        .channels
        .iter()
        .filter(|c| c.status == "unavailable")
        .count();
    assert_eq!(unavailable, 3, "codex/claude/pty channels are unavailable");

    // Readback 2: the durable interrupt mailbox row is physically present in CF_KV.
    let db = service.agent_control_db().expect("open storage");
    let mailbox_channel = response
        .channels
        .iter()
        .find(|c| c.channel == "mailbox_interrupt")
        .expect("mailbox channel present");
    let row_key = mailbox_channel
        .row_key
        .as_ref()
        .expect("delivered mailbox row has a key");
    let (rows, _more) = db
        .scan_cf_from(cf::CF_KV, row_key.as_bytes(), 1)
        .expect("scan CF_KV for the mailbox row");
    assert!(
        rows.first().map(|(k, _)| k.as_slice()) == Some(row_key.as_bytes()),
        "the durable interrupt mailbox row must exist at {row_key}"
    );

    // Readback 3: the durable Interrupted journal row exists.
    assert_eq!(
        journal_count_kind(&db, session, AgentEventKind::Interrupted),
        1,
        "exactly one Interrupted journal row must exist"
    );

    // Clean up the still-live victim deterministically.
    let _ = service
        .agent_kill_impl(
            AgentKillParams {
                session_id: session.to_owned(),
                grace_ms: 0,
                interrupt_first: false,
            },
            Some("operator-regression"),
        )
        .await
        .expect("cleanup kill");
    assert!(!crate::m4::process_exists(pid));
}

// ---------------------------------------------------------------------------
// fleet_stop (#907) — multi-agent real-process regression coverage
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: force-kills three real OS processes and asserts a host-load-sensitive 10s budget; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn fleet_stop_kill_terminates_every_live_agent() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    // Three real spawned agents.
    let agents = [
        ("session-fleet-a", "agent-spawn-fleet-a", spawn_victim()),
        ("session-fleet-b", "agent-spawn-fleet-b", spawn_victim()),
        ("session-fleet-c", "agent-spawn-fleet-c", spawn_victim()),
    ];
    for (session, spawn, pid) in &agents {
        register_spawned_victim(&service, session, spawn, *pid, "local-model");
        assert!(crate::m4::process_exists(*pid), "precondition: {pid} alive");
    }

    let started = Instant::now();
    let response = service
        .fleet_stop_impl(
            FleetStopParams {
                mode: "kill".to_owned(),
                confirm: "STOP-FLEET".to_owned(),
                agent_kinds: Vec::new(),
                grace_ms: 0,
            },
            Some("operator-regression"),
        )
        .await
        .expect("fleet_stop kill must succeed");
    let elapsed = started.elapsed();

    // Readback: the report claims all three stopped with zero survivors.
    assert_eq!(response.matched, 3, "all three live agents matched");
    assert_eq!(response.succeeded, 3);
    assert_eq!(response.failed, 0);
    assert!(response.all_stopped);
    assert!(
        elapsed < Duration::from_secs(10),
        "fleet_stop kill with grace_ms=0 must not serialize the fixed 30s spawn-completion grace; elapsed={elapsed:?}"
    );
    for outcome in &response.agents {
        assert!(
            outcome.ok,
            "{} not stopped: {}",
            outcome.session_id, outcome.reason
        );
        assert!(outcome.surviving_process_ids.is_empty());
    }

    // Readback: the OS process table, read back independently, confirms every pid is
    // gone — the authoritative proof.
    for (_session, _spawn, pid) in &agents {
        assert!(
            !crate::m4::process_exists(*pid),
            "fleet pid {pid} must be gone after fleet_stop kill"
        );
    }

    // Readback: a single fleet_stop audit pair plus the per-agent kill rows exist.
    let audit = service.command_audit_snapshot().expect("audit snapshot");
    let fleet_rows = audit.rows.iter().filter(|r| r.tool == "fleet_stop").count();
    assert!(
        fleet_rows >= 2,
        "expected intent+final fleet_stop rows, got {fleet_rows}"
    );
}

#[tokio::test]
async fn fleet_stop_requires_confirm_token() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let error = service
        .fleet_stop_impl(
            FleetStopParams {
                mode: "kill".to_owned(),
                confirm: "nope".to_owned(),
                agent_kinds: Vec::new(),
                grace_ms: 0,
            },
            Some("operator-regression"),
        )
        .await
        .expect_err("wrong confirm token must be refused");
    assert!(
        error.message.contains("FLEET_STOP_CONFIRM_REQUIRED"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn fleet_stop_empty_fleet_is_honest_noop() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let response = service
        .fleet_stop_impl(
            FleetStopParams {
                mode: "kill".to_owned(),
                confirm: "STOP-FLEET".to_owned(),
                agent_kinds: Vec::new(),
                grace_ms: 0,
            },
            Some("operator-regression"),
        )
        .await
        .expect("empty fleet is a no-op, not an error");
    assert_eq!(response.matched, 0);
    assert_eq!(response.succeeded, 0);
    assert_eq!(response.failed, 0);
    assert!(response.all_stopped, "vacuously all stopped on empty fleet");
}

#[tokio::test]
#[ignore = "supporting real-process regression evidence only; manual FSV remains separate: spawns/force-kills real OS process victims; host-load-sensitive; run with `cargo test -p synapse-mcp -- --ignored`"]
async fn fleet_stop_filters_by_agent_kind() {
    let temp = TempDir::new().expect("temp dir");
    let service = regression_service(temp.path());
    let codex_pid = spawn_victim();
    let claude_pid = spawn_victim();
    register_spawned_victim(
        &service,
        "session-codex",
        "agent-spawn-codex",
        codex_pid,
        "codex",
    );
    register_spawned_victim(
        &service,
        "session-claude",
        "agent-spawn-claude",
        claude_pid,
        "claude",
    );

    // Kill only the codex-kind agent.
    let response = service
        .fleet_stop_impl(
            FleetStopParams {
                mode: "kill".to_owned(),
                confirm: "STOP-FLEET".to_owned(),
                agent_kinds: vec!["codex".to_owned()],
                grace_ms: 0,
            },
            Some("operator-regression"),
        )
        .await
        .expect("filtered fleet_stop succeeds");

    assert_eq!(
        response.matched, 1,
        "only the codex agent matched the filter"
    );
    assert_eq!(response.agents[0].agent_kind, "codex");
    assert!(
        !crate::m4::process_exists(codex_pid),
        "codex agent must be killed"
    );
    assert!(
        crate::m4::process_exists(claude_pid),
        "the filtered-out claude agent must still be alive"
    );

    // Clean up the survivor deterministically.
    let _ = service
        .fleet_stop_impl(
            FleetStopParams {
                mode: "kill".to_owned(),
                confirm: "STOP-FLEET".to_owned(),
                agent_kinds: Vec::new(),
                grace_ms: 0,
            },
            Some("operator-regression"),
        )
        .await
        .expect("cleanup fleet_stop");
    assert!(!crate::m4::process_exists(claude_pid));
}
