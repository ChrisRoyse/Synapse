use super::{DEBUG_ONLY_TOOL_ROUTES, SessionTarget, SynapseService, explicit_action_target};

/// #1595: `storage_put_probe_rows` is a synthetic-write test/FSV tool and must
/// be gated off the default agent surface exactly like its fault-injector
/// siblings — absent from the router unless `SYNAPSE_DEBUG_TOOLS` is set, and
/// present when it is. FSV drives the actual router (`ToolRouter::has_route`),
/// the source of truth for what a client can call, not a return value.
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
    // so FSV/tests can drive it.
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
