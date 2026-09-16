use super::{
    AuthorityFinalizerDrainReadback, DEBUG_ONLY_TOOL_ROUTES, SessionTarget, SynapseService,
    explicit_action_target,
};

fn assert_canonical_hwnd_schema_fields(
    tool: &str,
    value: &serde_json::Value,
    path: &mut Vec<String>,
    found: &mut usize,
) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(properties) = object
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                for (field, schema) in properties {
                    if field == "hwnd" || field.ends_with("_hwnd") {
                        *found += 1;
                        let location = if path.is_empty() {
                            field.clone()
                        } else {
                            format!("{}.{}", path.join("."), field)
                        };
                        assert_eq!(
                            schema.get("minimum").and_then(serde_json::Value::as_u64),
                            Some(1),
                            "public tool {tool} HWND field {location} lacks minimum=1: {schema}"
                        );
                        assert_eq!(
                            schema.get("maximum").and_then(serde_json::Value::as_u64),
                            Some(u64::from(u32::MAX)),
                            "public tool {tool} HWND field {location} lacks maximum=u32::MAX: {schema}"
                        );
                    }
                }
            }
            for (key, child) in object {
                path.push(key.clone());
                assert_canonical_hwnd_schema_fields(tool, child, path, found);
                path.pop();
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                path.push(index.to_string());
                assert_canonical_hwnd_schema_fields(tool, child, path, found);
                path.pop();
            }
        }
        _ => {}
    }
}

#[test]
fn public_tool_schemas_bound_every_hwnd_to_canonical_user_handle_range() {
    let tools =
        crate::server::schema_sanitize::sanitize_tools(SynapseService::tool_router().list_all());
    let public_tools = tools
        .iter()
        .filter(|tool| super::tool_profiles::PUBLIC_TOOL_NAMES.contains(&tool.name.as_ref()))
        .collect::<Vec<_>>();
    assert_eq!(
        public_tools.len(),
        40,
        "canonical public tools/list must expose exactly the 40 facade schemas"
    );
    let mut actual_names = public_tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    let mut expected_names = super::tool_profiles::PUBLIC_TOOL_NAMES.to_vec();
    actual_names.sort_unstable();
    expected_names.sort_unstable();
    assert_eq!(
        actual_names, expected_names,
        "canonical public tools/list must contain each public facade exactly once"
    );
    let mut found = 0usize;
    for tool in public_tools {
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        assert_canonical_hwnd_schema_fields(
            tool.name.as_ref(),
            &schema,
            &mut Vec::new(),
            &mut found,
        );
    }
    assert!(
        found > 0,
        "public tools/list contained no HWND schema fields"
    );
    println!(
        "readback=public_tools_list public_tool_count=40 canonical_hwnd_schema_field_count={found}"
    );
}

/// #1595: `storage_put_probe_rows` is a synthetic-write supporting-test tool and must
/// be gated off the default agent surface exactly like its fault-injector
/// siblings — absent from the router unless `SYNAPSE_DEBUG_TOOLS` is set, and
/// present when it is. This regression drives the actual router
/// (`ToolRouter::has_route`), the source of truth for what a client can call;
/// manual FSV remains separate.
#[test]
fn debug_only_tool_routes_are_gated_off_the_default_surface() {
    // Sanity: the gated set is exactly the four synthetic diagnostics, with
    // the synthetic-write probe (#1595) included.
    println!("readback=DEBUG_ONLY_TOOL_ROUTES before=expected value=synthetic diagnostics");
    assert_eq!(
        DEBUG_ONLY_TOOL_ROUTES,
        &[
            "storage_put_probe_rows",
            "storage_pressure_sample",
            "action_diagnostic_rate_limit_override",
            "action_diagnostic_queue_full_setup",
        ],
    );

    // Default surface (debug disabled): every gated route is removed, so an
    // unbound client cannot call it even by exact name.
    let default_router = SynapseService::build_tool_router(false);
    for route in DEBUG_ONLY_TOOL_ROUTES {
        let present = default_router.has_route(route);
        println!("readback=has_route debug=false route={route} after=present:{present}");
        assert!(
            !present,
            "gated debug tool {route} must be absent from the default (non-debug) router surface",
        );
    }
    // The non-synthetic storage tools that share the `storage` facade stay on
    // the default surface — gating is scoped to the synthetic diagnostics only.
    assert!(
        default_router.has_route("storage_gc_once"),
        "real storage_gc_once must remain on the default surface",
    );
    assert!(
        default_router.has_route("storage_inspect"),
        "real storage_inspect must remain on the default surface",
    );

    // Debug surface (SYNAPSE_DEBUG_TOOLS set): every gated route is registered
    // so supporting tests can drive it. Manual FSV remains separate.
    let debug_router = SynapseService::build_tool_router(true);
    for route in DEBUG_ONLY_TOOL_ROUTES {
        let present = debug_router.has_route(route);
        println!("readback=has_route debug=true route={route} after=present:{present}");
        assert!(
            present,
            "gated debug tool {route} must be present when SYNAPSE_DEBUG_TOOLS is enabled",
        );
    }
    // The real storage tools are unaffected by the debug flag.
    assert!(debug_router.has_route("storage_gc_once"));
    assert!(debug_router.has_route("storage_inspect"));
}

#[test]
fn explicit_action_target_prefers_window_then_cdp_and_rejects_orphan_cdp() {
    // #984B: explicit per-call routing maps to the right SessionTarget so
    // multi-window/multi-agent action routing is deterministic.
    println!("readback=explicit_action_target edge=none input=(None,None)");
    assert_eq!(
        explicit_action_target(None, None).expect("no override is allowed"),
        None,
        "no explicit params => fall back to the session target"
    );

    println!("readback=explicit_action_target edge=window input=(Some(0x1234),None)");
    assert_eq!(
        explicit_action_target(Some(0x1234), None).expect("window override is valid"),
        Some(SessionTarget::Window { hwnd: 0x1234 }),
    );

    println!(
        "readback=explicit_action_target edge=cdp input=(Some(0x1234),Some(\"ABC\")) plus whitespace trim"
    );
    assert_eq!(
        explicit_action_target(Some(0x1234), Some("  ABC123  ")).expect("cdp override is valid"),
        Some(SessionTarget::Cdp {
            window_hwnd: 0x1234,
            cdp_target_id: "ABC123".to_owned(),
        }),
        "cdp_target_id is trimmed and paired with the window hwnd"
    );

    // Whitespace-only cdp_target_id is treated as absent, not as a CDP target.
    println!("readback=explicit_action_target edge=blank_cdp input=(Some(7),Some(\"   \"))");
    assert_eq!(
        explicit_action_target(Some(7), Some("   ")).expect("blank cdp id ignored"),
        Some(SessionTarget::Window { hwnd: 7 }),
    );

    println!("readback=explicit_action_target edge=orphan_cdp input=(None,Some(\"ABC\"))");
    let err = explicit_action_target(None, Some("ABC"))
        .expect_err("cdp_target_id without window_hwnd must be rejected");
    let code = err
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str);
    assert_eq!(code, Some(synapse_core::error_codes::TOOL_PARAMS_INVALID));

    for hwnd in [-1, 0, i64::from(u32::MAX) + 1, i64::MAX] {
        let error = explicit_action_target(Some(hwnd), None)
            .expect_err("noncanonical explicit action HWND must fail before routing");
        let data = error.data.expect("HWND shape error must be structured");
        assert_eq!(data["actual_value"], hwnd);
        assert_eq!(data["accepted_range"], "1..=u32::MAX");
    }
    assert_eq!(
        explicit_action_target(Some(i64::from(u32::MAX)), None)
            .expect("maximum canonical HWND must remain routable"),
        Some(SessionTarget::Window {
            hwnd: i64::from(u32::MAX)
        })
    );
}

#[test]
fn health_payload_reports_m3_subsystems_initializing_or_disabled() {
    let service = SynapseService::new();
    let payload = service.health_payload();
    assert!(payload.ok);
    assert_eq!(payload.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(payload.build, "dev");
    assert_eq!(payload.subsystems["storage"].status, "initializing");
    assert_eq!(payload.subsystems["reflex"].status, "initializing");
    assert_eq!(payload.subsystems["profiles"].status, "initializing");
    assert!(!payload.subsystems.contains_key("hid_host"));
    assert_eq!(payload.subsystems["action"].status, "ok");
    assert_eq!(payload.subsystems["audio"].status, "disabled");
    assert_eq!(payload.subsystems["http"].status, "disabled");
}

#[test]
fn uptime_uses_monotonic_elapsed() {
    let service = SynapseService::new();
    let first = service.health_payload().uptime_s;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = service.health_payload().uptime_s;
    assert!(second >= first);
}

#[tokio::test(flavor = "current_thread")]
async fn authority_finalizer_drain_reports_poison_after_closing_and_emptying_supervisor() {
    let service = SynapseService::new();
    let admission_closed = std::sync::Arc::clone(&service.authority_finalizers.admission_closed);
    let poison = std::thread::spawn(move || {
        let _guard = admission_closed
            .lock()
            .expect("synthetic authority admission lock starts healthy");
        panic!("synthetic authority admission poison");
    });
    assert!(poison.join().is_err(), "synthetic poison thread must panic");

    let error = service
        .drain_authority_finalizers()
        .await
        .expect_err("poisoned admission must make the shutdown verdict fail");

    assert!(error.readback.admission_closed);
    assert!(error.readback.tracker_closed);
    assert_eq!(error.readback.registered_tasks_after, 0);
    assert_eq!(error.readback.tracked_tasks_after, 0);
    assert!(error.readback.safe_to_unlock());
    let spawn_error = match service.spawn_authority_transaction(async {}) {
        Ok(_caller) => panic!("poisoned, closed admission must reject later transactions"),
        Err(error) => error,
    };
    assert!(spawn_error.message.contains("poisoned"));
}

#[test]
fn authority_lifetime_unlock_requires_closed_admission_and_tracker() {
    let all_quiescent = AuthorityFinalizerDrainReadback {
        admission_closed: true,
        tracker_closed: true,
        registered_tasks_before: 0,
        cancellation_signals_sent: 0,
        abort_requests_sent: 0,
        registered_tasks_after: 0,
        tracked_tasks_after: 0,
    };
    assert!(all_quiescent.safe_to_unlock());

    let mut admission_open = all_quiescent.clone();
    admission_open.admission_closed = false;
    assert!(
        !admission_open.safe_to_unlock(),
        "an empty but admission-open registry must retain lifetime locks"
    );

    let mut tracker_open = all_quiescent;
    tracker_open.tracker_closed = false;
    assert!(
        !tracker_open.safe_to_unlock(),
        "an empty but tracker-open supervisor must retain lifetime locks"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn authority_completion_proves_exact_join_and_registry_reap() {
    let service = SynapseService::new();
    let completion = service
        .spawn_authority_transaction(async { "terminal" })
        .unwrap_or_else(|error| panic!("spawn supervised authority transaction: {error:?}"));

    assert_eq!(
        completion
            .await
            .unwrap_or_else(|error| panic!("join supervised authority transaction: {error}")),
        "terminal"
    );
    assert_eq!(
        service
            .authority_finalizers
            .transactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        0,
        "completion acknowledgement must follow exact JoinHandle join and registry removal"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn authority_finalizer_drain_causally_cancels_and_reaps_detached_transaction() {
    struct RollbackProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for RollbackProbe {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    let service = SynapseService::new();
    let rollback_complete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let transaction_rollback = std::sync::Arc::clone(&rollback_complete);
    let (armed_sender, armed_receiver) = tokio::sync::oneshot::channel();
    let caller = service
        .spawn_authority_transaction(async move {
            let _rollback = RollbackProbe(transaction_rollback);
            let _armed = armed_sender.send(());
            std::future::pending::<()>().await;
        })
        .unwrap_or_else(|error| panic!("spawn supervised authority transaction: {error:?}"));

    armed_receiver
        .await
        .unwrap_or_else(|error| panic!("authority transaction did not reach armed state: {error}"));
    drop(caller);

    let readback = service
        .drain_authority_finalizers()
        .await
        .unwrap_or_else(|error| panic!("cancel and reap authority transaction: {error}"));

    assert!(
        rollback_complete.load(std::sync::atomic::Ordering::Acquire),
        "drain may not return until dropping the transaction ran its rollback guard"
    );
    assert_eq!(readback.registered_tasks_before, 1);
    assert_eq!(readback.cancellation_signals_sent, 1);
    assert_eq!(readback.abort_requests_sent, 0);
    assert_eq!(readback.registered_tasks_after, 0);
    assert_eq!(readback.tracked_tasks_after, 0);
    assert!(readback.safe_to_unlock());
    assert_eq!(
        service
            .authority_finalizers
            .transactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        0,
        "independent transaction-registry readback must remain empty after drain"
    );
    assert_eq!(
        service.authority_finalizers.tasks.len(),
        0,
        "independent task-tracker readback must remain empty after drain"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn authority_finalizer_drain_waits_for_cooperative_cleanup_and_terminal_audit() {
    let service = SynapseService::new();
    let terminal_steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let worker_steps = std::sync::Arc::clone(&terminal_steps);
    let (armed_sender, armed_receiver) = tokio::sync::oneshot::channel();
    let caller = service
        .spawn_cooperative_authority_transaction(move |cancellation| async move {
            let _armed = armed_sender.send(());
            cancellation.cancelled().await;
            worker_steps
                .lock()
                .expect("cooperative cleanup step ledger remains healthy")
                .push("physical_cleanup_readback");
            tokio::task::yield_now().await;
            worker_steps
                .lock()
                .expect("cooperative audit step ledger remains healthy")
                .push("terminal_command_audit");
        })
        .unwrap_or_else(|error| panic!("spawn cooperative authority transaction: {error:?}"));

    armed_receiver
        .await
        .unwrap_or_else(|error| panic!("cooperative authority owner did not arm: {error}"));
    drop(caller);
    let readback = service
        .drain_authority_finalizers()
        .await
        .unwrap_or_else(|error| panic!("drain cooperative authority owner: {error}"));

    assert_eq!(
        *terminal_steps
            .lock()
            .expect("read cooperative terminal-step ledger"),
        ["physical_cleanup_readback", "terminal_command_audit"],
        "drain must retain the exact async owner through cleanup and final audit"
    );
    assert_eq!(readback.registered_tasks_before, 1);
    assert_eq!(readback.cancellation_signals_sent, 1);
    assert_eq!(readback.abort_requests_sent, 0);
    assert_eq!(readback.registered_tasks_after, 0);
    assert_eq!(readback.tracked_tasks_after, 0);
    assert!(readback.safe_to_unlock());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn authority_finalizer_drain_retains_late_cooperative_owner_and_lifetime_lock_verdict() {
    let service = SynapseService::new();
    let release = tokio_util::sync::CancellationToken::new();
    let worker_release = release.clone();
    let (armed_sender, armed_receiver) = tokio::sync::oneshot::channel();
    let (cancellation_sender, cancellation_receiver) = tokio::sync::oneshot::channel();
    let caller = service
        .spawn_cooperative_authority_transaction(move |cancellation| async move {
            let _armed = armed_sender.send(());
            cancellation.cancelled().await;
            let _observed = cancellation_sender.send(());
            worker_release.cancelled().await;
        })
        .unwrap_or_else(|error| panic!("spawn retained cooperative owner: {error:?}"));
    armed_receiver
        .await
        .unwrap_or_else(|error| panic!("retained cooperative owner did not arm: {error}"));
    drop(caller);

    let drain_service = service.clone();
    let drain = tokio::spawn(async move { drain_service.drain_authority_finalizers().await });
    cancellation_receiver
        .await
        .unwrap_or_else(|error| panic!("cooperative owner did not observe shutdown: {error}"));
    tokio::time::advance(super::AUTHORITY_CANCELLATION_GRACE + std::time::Duration::from_millis(1))
        .await;
    let failure = drain
        .await
        .unwrap_or_else(|error| panic!("join retained-owner drain: {error}"))
        .expect_err("a late cooperative owner must fail the bounded drain");

    assert_eq!(failure.readback.abort_requests_sent, 0);
    assert_eq!(failure.readback.registered_tasks_after, 1);
    assert_eq!(failure.readback.tracked_tasks_after, 1);
    assert!(
        !failure.readback.safe_to_unlock(),
        "live cooperative authority must retain daemon lifetime locks"
    );
    assert!(
        failure
            .to_string()
            .contains("retained 1 storage-backed owner"),
        "drain must identify the retained exact owner: {failure}"
    );

    release.cancel();
    service.authority_finalizers.tasks.wait().await;
    assert_eq!(
        service
            .authority_finalizers
            .transactions
            .lock()
            .expect("read reaped retained-owner registry")
            .len(),
        0
    );
}

#[tokio::test(flavor = "current_thread")]
async fn authority_finalizer_drain_aggregates_panics_after_exact_join_and_empty_readback() {
    let service = SynapseService::new();
    let caller = service
        .spawn_authority_transaction(async move {
            panic!("synthetic authority transaction panic");
        })
        .unwrap_or_else(|error| panic!("spawn panicking authority transaction: {error:?}"));
    let join_error = caller
        .await
        .expect_err("caught authority transaction panic must reach its caller as an error");
    assert!(join_error.is_panic());
    assert!(
        join_error
            .to_string()
            .contains("synthetic authority transaction panic")
    );

    let drain_error = service
        .drain_authority_finalizers()
        .await
        .expect_err("a supervised transaction panic must fail the shutdown verdict");

    assert!(
        drain_error
            .to_string()
            .contains("synthetic authority transaction panic"),
        "drain must aggregate the exact reaped task failure: {drain_error}"
    );
    assert_eq!(drain_error.readback.registered_tasks_after, 0);
    assert_eq!(drain_error.readback.tracked_tasks_after, 0);
    assert!(drain_error.readback.safe_to_unlock());
}
