//! Unit tests for chrome_debugger_bridge (split out of the module body per #1555).

use axum::http::{HeaderMap, HeaderValue, Uri, header};

use super::*;

const TEST_SERVICE_WORKER_SHA256: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";

fn test_popup_risk_suppression() -> Value {
    json!({
        "ok": true,
        "status": "clear",
        "management_available": true,
        "hazard_count": 0,
        "disabled_count": 0,
        "remaining_hazard_count": 0,
        "failure_count": 0,
        "remaining_hazards": [],
        "failures": []
    })
}

fn test_chrome_bridge_health_record() -> ChromeBridgeHealthRecord {
    ChromeBridgeHealthRecord {
        host_id: "chrome-native-test".to_owned(),
        origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
        extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
        extension_version: Some("0.1.0".to_owned()),
        extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
        extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
        extension_build_sha256: Some(EXPECTED_EXTENSION_DECLARED_BUILD_SHA256.to_owned()),
        extension_declared_build_sha256: Some(EXPECTED_EXTENSION_DECLARED_BUILD_SHA256.to_owned()),
        extension_service_worker_sha256: Some(TEST_SERVICE_WORKER_SHA256.to_owned()),
        extension_service_worker_sha256_status: Some("ok".to_owned()),
        extension_service_worker_sha256_source: Some(format!(
            "chrome-extension://{EXTENSION_ID}/service_worker.js"
        )),
        extension_service_worker_byte_length: Some(1234),
        extension_service_worker_sha256_error: None,
        expected_service_worker_sha256: Some(TEST_SERVICE_WORKER_SHA256.to_owned()),
        expected_service_worker_path: Some(
            r"C:\synapse-test\extension\service_worker.js".to_owned(),
        ),
        extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect(),
        extension_user_agent: Some("Chrome test".to_owned()),
        extension_debugger_api_available: Some(true),
        extension_popup_risk_suppression: Some(test_popup_risk_suppression()),
        pid: 42,
        parent_window: None,
        transport: Some("direct_http".to_owned()),
        registered_unix_ms: 1000,
        last_seen_unix_ms: 2000,
        last_disconnect_detail: None,
        last_detach_reason: None,
    }
}

fn test_host_record() -> HostRecord {
    HostRecord {
        origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
        extension_id: Some(EXTENSION_ID.to_owned()),
        extension_version: None,
        extension_protocol_version: None,
        extension_build_id: None,
        extension_build_sha256: None,
        extension_declared_build_sha256: None,
        extension_service_worker_sha256: None,
        extension_service_worker_sha256_status: None,
        extension_service_worker_sha256_source: None,
        extension_service_worker_byte_length: None,
        extension_service_worker_sha256_error: None,
        extension_capabilities: BTreeSet::new(),
        extension_user_agent: None,
        extension_debugger_api_available: None,
        extension_popup_risk_suppression: None,
        pid: 42,
        parent_window: None,
        transport: Some("direct_http".to_owned()),
        bridge_token_digest: [0; 32],
        registered_unix_ms: 1000,
        last_seen_unix_ms: 2000,
        last_disconnect_detail: None,
        last_detach_reason: None,
    }
}

// #1342: a downloads wait/save/move must give the daemon a response budget
// that outlives the caller's in-extension waitTimeoutMs; list keeps default.
#[test]
fn downloads_command_timeout_scales_with_wait_budget() {
    // Read-only list keeps the fixed default budget.
    assert_eq!(
        downloads_command_timeout(&json!({"operation": "list"})),
        COMMAND_TIMEOUT
    );
    // A long wait extends the budget past the 30s default (+5s margin).
    assert_eq!(
        downloads_command_timeout(&json!({"operation": "wait", "waitTimeoutMs": 300_000})),
        Duration::from_mins(5) + Duration::from_secs(5)
    );
    // save/move also wait for a completed match.
    assert_eq!(
        downloads_command_timeout(&json!({"operation": "save", "waitTimeoutMs": 120_000})),
        Duration::from_mins(2) + Duration::from_secs(5)
    );
    // A short wait budget never drops below the default floor.
    assert_eq!(
        downloads_command_timeout(&json!({"operation": "wait", "waitTimeoutMs": 1000})),
        COMMAND_TIMEOUT
    );
    // Absent waitTimeoutMs falls back to the extension's 30s default, so the
    // daemon budget is that default + the 5s margin (must outlive the in-page wait).
    assert_eq!(
        downloads_command_timeout(&json!({"operation": "wait"})),
        COMMAND_TIMEOUT + Duration::from_secs(5)
    );
}

#[test]
fn native_host_invocation_detects_chrome_origin_and_parent_window() {
    let invocation = native_host_invocation_from_args([
        OsString::from("chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/"),
        OsString::from("--parent-window=1234"),
    ])
    .expect("chrome native host origin should be detected");

    assert_eq!(
        invocation.origin,
        "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/"
    );
    assert_eq!(invocation.parent_window.as_deref(), Some("1234"));
}

#[test]
fn native_host_unknown_id_error_is_restart_recoverable() {
    let detail = r#"{"ok":false,"code":"A11Y_CDP_EXTENSION_UNAVAILABLE","detail":"unknown chrome debugger native host_id \"chrome-native-old\""}"#;
    let error = anyhow::anyhow!("Chrome debugger native poll failed status=400 detail={detail}");

    assert!(is_unknown_native_host_detail(detail));
    assert!(is_unknown_native_host_error(&error));
    assert!(!is_unknown_native_host_detail("bridge protocol mismatch"));
}

#[test]
fn extension_error_preserves_dom_action_codes() {
    for code in [
        error_codes::CHROME_SCRIPTING_EXECUTE_FAILED,
        error_codes::CHROME_DOM_SELECTOR_INVALID,
        error_codes::CHROME_DOM_ELEMENT_NOT_FOUND,
        error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS,
        error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE,
        error_codes::CHROME_DOM_ACTION_UNSUPPORTED,
        error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED,
        error_codes::ACTION_TARGET_INVALID,
        error_codes::BROWSER_NAVIGATION_FAILED,
    ] {
        let error = ChromeDebuggerBridgeError::extension(Some(code), "dom action failed");
        assert_eq!(error.code(), code);
        assert_eq!(error.detail(), "dom action failed");
    }
}

#[test]
fn direct_http_bridge_token_authorizes_next_without_origin_only_after_register() {
    let registered = bridge()
        .register(NativeRegisterRequest {
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            pid: 0,
            parent_window: None,
            bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
            transport: Some("direct_http".to_owned()),
        })
        .expect("direct bridge register should issue a host token");
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7700"));
    headers.insert(
        BRIDGE_TOKEN_HEADER,
        HeaderValue::from_str(&registered.bridge_token)
            .expect("bridge token should be a valid header value"),
    );

    assert!(is_direct_http_extension_bridge_request(
        &headers,
        &Uri::from_static("/chrome-debugger/native/next?host_id=anything"),
    ));
    let ws_uri = format!(
        "/chrome-debugger/native/ws?host_id={}&bridge_token={}",
        registered.host_id, registered.bridge_token
    )
    .parse::<Uri>()
    .expect("websocket uri with token should parse");
    assert!(is_direct_http_extension_bridge_request(
        &HeaderMap::new(),
        &ws_uri
    ));
    assert!(!is_direct_http_extension_bridge_request(
        &headers,
        &Uri::from_static("/chrome-debugger/native/register"),
    ));
    assert!(!is_direct_http_extension_bridge_request(
        &headers,
        &Uri::from_static("/chrome-debugger/native/maintenance-pause"),
    ));
    assert!(!is_direct_http_extension_bridge_request(
        &headers,
        &Uri::from_static("/mcp"),
    ));
}

#[test]
fn direct_http_bridge_token_header_is_host_scoped_after_register() {
    let first = bridge()
        .register(NativeRegisterRequest {
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            pid: 1,
            parent_window: None,
            bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
            transport: Some("direct_http".to_owned()),
        })
        .expect("first direct bridge register should issue a host token");
    let second = bridge()
        .register(NativeRegisterRequest {
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            pid: 2,
            parent_window: None,
            bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
            transport: Some("direct_http".to_owned()),
        })
        .expect("second direct bridge register should issue a host token");
    let mut headers = HeaderMap::new();
    headers.insert(
        BRIDGE_TOKEN_HEADER,
        HeaderValue::from_str(&first.bridge_token).expect("bridge token header-safe"),
    );

    assert!(direct_http_bridge_token_header_matches_host(
        &headers,
        &first.host_id
    ));
    assert!(!direct_http_bridge_token_header_matches_host(
        &headers,
        &second.host_id
    ));
}

#[test]
fn extension_unavailable_maps_to_explicit_cdp_status() {
    let error = ChromeDebuggerBridgeError::unavailable();

    assert_eq!(error.code(), error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE);
    assert_eq!(error.cdp_status(), CdpStatus::ExtensionUnavailable);
    assert!(
        error
            .detail()
            .contains("install the bundled Synapse Chrome extension")
    );
    assert!(error.detail().contains("no_active_host_repair="));
    assert!(
        error
            .detail()
            .contains("do not launch a second Chrome process/profile")
    );
}

#[test]
fn chrome_bridge_health_reports_unavailable_without_active_host() {
    let health = chrome_bridge_health_from_snapshot(None, 0, 0, 0, &[], &[], &[]);

    assert_eq!(health.status, "unavailable");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=false"));
    assert!(detail.contains("reason=no_active_chrome_bridge_host"));
    assert!(detail.contains("expected_extension_id=leoocgnkjnplbfdbklajepahofecgfbk"));
    assert!(detail.contains("repair_guidance=no_active_host_repair="));
    assert!(detail.contains("already-open authenticated Chrome profile"));
    assert!(detail.contains("do not launch a second Chrome process/profile"));
    assert!(detail.contains("browser_debugger.reload_bridge"));
    assert!(detail.contains("scripts\\install-synapse-chrome-debugger.ps1"));
}

#[test]
fn chrome_bridge_health_reports_absent_profile_install_without_active_host() {
    let self_policy_shield = SynapseChromeSelfPolicyShieldStatus {
        present: true,
        detail: "synapse_chrome_self_policy_shield_present=true reason=test_present".to_owned(),
    };
    let profile_install_state = SynapseChromeProfileInstallState {
        detail: "synapse_chrome_bridge_profile_installation scanned=true installed=false profile_count=6 installed_profile_count=0 active_profile=\"Profile 5\" active_profile_installed=false reason=extension_id_absent_from_preferences_and_secure_preferences cdp_bridge_reload_can_install_absent_extension=false remediation=test".to_owned(),
        active_profile_extension_path: None,
        active_profile_service_worker_sha256: None,
        active_profile_service_worker_error: Some(
            "extension_id_absent_from_preferences_and_secure_preferences".to_owned(),
        ),
    };
    let health = chrome_bridge_health_from_snapshot_with_self_policy(
        None,
        0,
        0,
        0,
        &[],
        &[],
        &[],
        &self_policy_shield,
        &profile_install_state,
    );

    assert_eq!(health.status, "unavailable");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("reason=no_active_chrome_bridge_host"));
    assert!(detail.contains("synapse_chrome_bridge_profile_installation"));
    assert!(detail.contains("installed=false"));
    assert!(detail.contains("installed_profile_count=0"));
    assert!(detail.contains("active_profile_installed=false"));
    assert!(detail.contains("cdp_bridge_reload_can_install_absent_extension=false"));
}

#[test]
fn chrome_bridge_health_reports_external_popup_risk_unknown_without_active_host() {
    let health = chrome_bridge_health_from_snapshot(
        None,
        0,
        0,
        0,
        &["profile=Default extension_id=external active_api=debugger".to_owned()],
        &[],
        &[],
    );

    assert_eq!(health.status, "unavailable");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("reason=no_active_chrome_bridge_host"));
    assert!(detail.contains("external_chrome_popup_risk_warning=true"));
    assert!(
        detail.contains("external_chrome_popup_risk_scope=host_unavailable_no_live_management")
    );
    assert!(detail.contains("extension_id=external"));
    assert!(!detail.contains("external_chrome_popup_risk_blocking=true"));
    assert!(!detail.contains("external_chrome_popup_risk_scope=external_suppression_required"));
}

#[test]
fn chrome_bridge_health_reports_ready_active_host() {
    let mut host = test_chrome_bridge_health_record();
    host.parent_window = Some("1001".to_owned());

    let health = chrome_bridge_health_from_snapshot(Some(&host), 1, 2, 3, &[], &[], &[]);

    assert_eq!(health.status, "ok");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=true"));
    assert!(detail.contains("active_host_id=chrome-native-test"));
    assert!(
        detail.contains("endpoint=chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs")
    );
    assert!(detail.contains("queued_count=2"));
    assert!(detail.contains("pending_count=3"));
    assert!(detail.contains("extension_debugger_api_available=true"));
}

#[test]
fn chrome_bridge_health_blocks_runtime_debugger_api_unavailable() {
    let mut host = test_chrome_bridge_health_record();
    host.extension_debugger_api_available = Some(false);
    host.parent_window = Some("1001".to_owned());

    let health = chrome_bridge_health_from_snapshot(Some(&host), 1, 0, 0, &[], &[], &[]);

    assert_eq!(health.status, "stale");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=false"));
    assert!(detail.contains("extension_debugger_api_available=false"));
    assert!(detail.contains("debugger_api_available=false expected=true"));
}

#[test]
fn chrome_bridge_health_blocks_external_popup_risk_until_suppressed() {
    let mut host = test_chrome_bridge_health_record();
    host.extension_popup_risk_suppression = None;

    let health = chrome_bridge_health_from_snapshot(
        Some(&host),
        1,
        0,
        0,
        &["profile=Default extension_id=external active_api=debugger".to_owned()],
        &[],
        &[],
    );

    assert_eq!(health.status, "unsafe_profile");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=false"));
    assert!(detail.contains("external_chrome_popup_risk_blocking=true"));
    assert!(detail.contains("external_chrome_popup_risk_scope=external_suppression_required"));
    assert!(detail.contains("extension_id=external"));
}

#[test]
fn chrome_bridge_health_allows_external_popup_risk_when_bridge_management_suppressed() {
    let mut host = test_chrome_bridge_health_record();
    host.extension_popup_risk_suppression = Some(json!({
        "ok": true,
        "status": "suppressed",
        "management_available": true,
        "hazard_count": 1,
        "disabled_count": 1,
        "remaining_hazard_count": 0,
        "failure_count": 0,
        "remaining_hazards": [],
        "failures": []
    }));

    let health = chrome_bridge_health_from_snapshot(
        Some(&host),
        1,
        0,
        0,
        &["profile=Default extension_id=external active_api=debugger".to_owned()],
        &[],
        &[],
    );

    assert_eq!(health.status, "ok");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=true"));
    assert!(detail.contains("external_chrome_popup_risk_warning=true"));
    assert!(detail.contains("external_chrome_popup_risk_scope=covered_by_live_bridge_management"));
    assert!(detail.contains("bridge_popup_risk_suppression=status=suppressed"));
}

#[test]
fn chrome_bridge_health_allows_physical_profile_risk_when_live_management_is_clear() {
    let host = test_chrome_bridge_health_record();

    let health = chrome_bridge_health_from_snapshot(
        Some(&host),
        1,
        0,
        0,
        &["profile=Profile 5 extension_id=fcoeoabgfenejglbffodgkkbkcdhcgfn active_api=debugger,nativeMessaging popup_risk=true".to_owned()],
        &[],
        &[],
    );

    assert_eq!(health.status, "ok");
    let detail = health.detail.as_deref().expect("health detail");
    println!(
        "readback=chrome_bridge_health edge=live_management_clear_over_physical_profile_risk detail={detail}"
    );
    assert!(detail.contains("tab_control_available=true"));
    assert!(detail.contains("external_chrome_popup_risk_warning=true"));
    assert!(detail.contains("external_chrome_popup_risk_scope=covered_by_live_bridge_management"));
    assert!(detail.contains("bridge_popup_risk_suppression=status=clear"));
    assert!(!detail.contains("external_chrome_popup_risk_blocking=true"));
}

#[test]
fn chrome_bridge_health_reports_external_layout_infobar_risk_as_warning() {
    let host = test_chrome_bridge_health_record();

    let health = chrome_bridge_health_from_snapshot(
        Some(&host),
        1,
        0,
        0,
        &[],
        &[],
        &["chrome_process pid=66452 parent_pid=100 parent_chain=100:node.exe name=chrome.exe reasons=headed_ms_playwright_mcp_layout_banner,remote_debugging_without_silent_debugger_extension_api user_data_dir=\"C:\\Users\\hotra\\AppData\\Local\\ms-playwright-mcp\\profile\" user_data_dir_state=dedicated_or_external owner_hint=ms_playwright_mcp_external repair_hint=stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags has_remote_debugging_pipe=false has_remote_debugging_port=true has_silent_debugger_extension_api=false has_ms_playwright_mcp_dir=true command_metadata_policy=safe_display_v1 command_line_len=256 command_line_sha256=sha256:abc123".to_owned()],
    );

    assert_eq!(health.status, "ok");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=true"));
    assert!(detail.contains("external_chrome_layout_infobar_risk_warning=true"));
    assert!(detail.contains("layout_risk_count=1"));
    assert!(detail.contains("headed_ms_playwright_mcp_layout_banner"));
    assert!(detail.contains("parent_chain=100:node.exe"));
    assert!(detail.contains("user_data_dir_state=dedicated_or_external"));
    assert!(detail.contains("owner_hint=ms_playwright_mcp_external"));
    assert!(detail.contains(
        "repair_hint=stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags"
    ));
    assert!(detail.contains("command_metadata_policy=safe_display_v1"));
    assert!(detail.contains("command_line_sha256=sha256:abc123"));
}

#[test]
fn chrome_layout_infobar_metadata_parses_and_classifies_owner() {
    let args = vec![
        "chrome.exe".to_owned(),
        "--remote-debugging-port=9222".to_owned(),
        "--user-data-dir=C:\\Temp\\ms-playwright-mcp\\profile".to_owned(),
    ];
    assert_eq!(
        process_switch_arg_value(&args, "--user-data-dir").as_deref(),
        Some("C:\\Temp\\ms-playwright-mcp\\profile")
    );
    assert_eq!(
        chrome_layout_infobar_owner_hint(
            "chrome.exe --remote-debugging-port=9222",
            Some("C:\\Temp\\ms-playwright-mcp\\profile"),
            "100:node.exe"
        ),
        "ms_playwright_mcp_external"
    );
    assert_eq!(
        chrome_layout_infobar_repair_hint("ms_playwright_mcp_external"),
        "stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags"
    );
    assert_eq!(
        chrome_layout_infobar_owner_hint(
            "chrome.exe --remote-debugging-pipe",
            Some("C:\\Users\\hotra\\AppData\\Local\\synapse\\synapse-cdp-profiles\\agent"),
            "200:synapse-mcp.exe"
        ),
        "synapse_owned_or_spawned"
    );
    assert_eq!(
        chrome_layout_infobar_repair_hint("synapse_owned_or_spawned"),
        "terminate_exact_synapse_owned_pid_tree_or_session_cleanup"
    );
    assert_eq!(
        chrome_layout_infobar_owner_hint(
            "chrome.exe --remote-debugging-port=9222",
            None,
            "300:powershell.exe"
        ),
        "unknown_external"
    );
    assert_eq!(
        chrome_layout_infobar_repair_hint("unknown_external"),
        "do_not_attach_or_target_until_owner_identified"
    );
    assert_eq!(quote_detail_value("a\"b"), "\"a\\\"b\"");
}

#[test]
fn chrome_bridge_health_blocks_stale_active_self_permission() {
    let host = test_chrome_bridge_health_record();

    let self_rows = [format!(
        "profile=Default pref=Preferences extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api=nativeMessaging manifest_api=nativeMessaging granted_hazard_api=nativeMessaging synapse_self_popup_risk=true risk_basis=active_or_manifest_hazard_without_disable_reason state=1 active_bit=true disable_reasons=[]"
    )];
    let health = chrome_bridge_health_from_snapshot(Some(&host), 1, 0, 0, &[], &self_rows, &[]);

    assert_eq!(health.status, "unsafe_profile");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=false"));
    assert!(detail.contains("synapse_chrome_bridge_permission_blocking=true"));
    assert!(detail.contains("active_synapse_bridge_native_messaging_permission"));
}

#[test]
fn chrome_bridge_health_warns_on_self_granted_only_residue_without_policy_shield() {
    let host = test_chrome_bridge_health_record();

    let self_rows = [format!(
        "profile=Default pref=Secure Preferences extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api=alarms,tabs,debugger manifest_api=alarms,tabs,debugger granted_hazard_api=nativeMessaging synapse_self_popup_risk=true risk_basis=granted_only_stale state=<absent> active_bit=<absent> disable_reasons=[]"
    )];
    let missing_policy_shield = SynapseChromeSelfPolicyShieldStatus {
        present: false,
        detail: "synapse_chrome_self_policy_shield_present=false reason=test_missing".to_owned(),
    };
    let profile_install_state = SynapseChromeProfileInstallState::test_installed();
    let health = chrome_bridge_health_from_snapshot_with_self_policy(
        Some(&host),
        1,
        0,
        0,
        &[],
        &self_rows,
        &[],
        &missing_policy_shield,
        &profile_install_state,
    );

    assert_eq!(health.status, "ok");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=true"));
    assert!(detail.contains("synapse_chrome_bridge_permission_warning=true"));
    assert!(detail.contains("granted_only_stale_permissions_without_policy_shield"));
    assert!(detail.contains("synapse_chrome_self_policy_shield_present=false"));
    assert!(detail.contains("extension_debugger_api_available=true"));
}

#[test]
fn chrome_bridge_health_warns_on_self_granted_only_residue_with_policy_shield() {
    let host = test_chrome_bridge_health_record();

    let self_rows = [format!(
        "profile=Default pref=Secure Preferences extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api=alarms,tabs,debugger manifest_api=alarms,tabs,debugger granted_hazard_api=nativeMessaging synapse_self_popup_risk=true risk_basis=granted_only_stale state=<absent> active_bit=<absent> disable_reasons=[]"
    )];
    let present_policy_shield = SynapseChromeSelfPolicyShieldStatus {
        present: true,
        detail: "synapse_chrome_self_policy_shield_present=true reason=test_present".to_owned(),
    };
    let profile_install_state = SynapseChromeProfileInstallState::test_installed();
    let health = chrome_bridge_health_from_snapshot_with_self_policy(
        Some(&host),
        1,
        0,
        0,
        &[],
        &self_rows,
        &[],
        &present_policy_shield,
        &profile_install_state,
    );

    assert_eq!(health.status, "ok");
    let detail = health.detail.as_deref().expect("health detail");
    assert!(detail.contains("tab_control_available=true"));
    assert!(detail.contains("synapse_chrome_bridge_permission_warning=true"));
    assert!(detail.contains("granted_only_stale_permissions_with_policy_shield"));
    assert!(detail.contains("synapse_chrome_self_policy_shield_present=true"));
}

#[test]
fn direct_http_bridge_refuses_new_commands_without_capability_readback() {
    let profile_install_state = SynapseChromeProfileInstallState::test_installed();
    let mut host = test_host_record();

    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state)
            .expect("missing identity");
    assert!(reason.contains("build_id=not_seen_yet"));
    assert!(reason.contains("service_worker_sha256=not_seen_yet"));
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "reloadSelf", &profile_install_state)
            .expect("stale reason");
    assert!(reason.contains("missing_capability=reloadSelf"));

    host.extension_capabilities = ["openTab", "closeTab", "targetInfo"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let reason = bridge_command_stale_reason_with_profile_state(
        &host,
        "targetInfoPageText",
        &profile_install_state,
    )
    .expect("missing identity still fails before capability fallback");
    assert!(reason.contains("build_id=not_seen_yet"));
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "reloadSelf", &profile_install_state)
            .expect("missing capability");
    assert!(reason.contains("missing_capability=reloadSelf"));

    host.extension_build_id = Some("old-build".to_owned());
    host.extension_declared_build_sha256 = Some("old-sha".to_owned());
    host.extension_service_worker_sha256 = Some("old-worker-sha".to_owned());
    host.extension_service_worker_sha256_status = Some("ok".to_owned());
    host.extension_capabilities = REQUIRED_DIRECT_HTTP_CAPABILITIES
        .iter()
        .map(|capability| (*capability).to_owned())
        .collect();
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state)
            .expect("stale build blocked");
    assert!(reason.contains("build_id=old-build"));
    assert!(reason.contains("service_worker_sha256=old-worker-sha"));
    assert_eq!(
        bridge_command_stale_reason_with_profile_state(&host, "reloadSelf", &profile_install_state,),
        None
    );

    host.extension_build_id = Some(EXPECTED_EXTENSION_BUILD_ID.to_owned());
    host.extension_declared_build_sha256 =
        Some(EXPECTED_EXTENSION_DECLARED_BUILD_SHA256.to_owned());
    host.extension_service_worker_sha256 = Some(TEST_SERVICE_WORKER_SHA256.to_owned());
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state)
            .expect("missing runtime debugger API readback is unsafe");
    assert!(reason.contains("debugger_api_available=not_seen_yet"));

    host.extension_debugger_api_available = Some(true);
    assert_eq!(
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state,),
        None
    );

    host.extension_debugger_api_available = Some(false);
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state)
            .expect("runtime debugger API availability false is unsafe");
    assert!(reason.contains("debugger_api_available=false"));

    host.extension_debugger_api_available = Some(true);
    host.extension_capabilities.clear();
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state)
            .expect("exact identity without capability readback is still unsafe");
    assert!(reason.contains("capabilities_not_advertised"));
    assert!(reason.contains("required=alarmReconnect"));

    host.extension_capabilities = ["openTab", "closeTab", "targetInfo"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let reason = bridge_command_stale_reason_with_profile_state(
        &host,
        "targetInfoPageText",
        &profile_install_state,
    )
    .expect("missing targetInfoPageText capability");
    assert!(reason.contains("missing_capability=targetInfoPageText"));

    host.extension_capabilities = REQUIRED_DIRECT_HTTP_CAPABILITIES
        .iter()
        .map(|capability| (*capability).to_owned())
        .collect();
    assert_eq!(
        bridge_command_stale_reason_with_profile_state(&host, "reloadSelf", &profile_install_state,),
        None
    );
    assert_eq!(
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state,),
        None
    );
}

#[test]
fn build_id_skew_reason_distinguishes_matching_worker_bytes() {
    let reason = bridge_build_id_stale_reason(
        Some("synapse-chrome-bridge-future"),
        EXPECTED_EXTENSION_BUILD_ID,
        Some(TEST_SERVICE_WORKER_SHA256),
        Some(TEST_SERVICE_WORKER_SHA256),
        Some("ok"),
    )
    .expect("build id mismatch");

    assert!(reason.contains("build_id_skew=daemon_expected_build_mismatch"));
    assert!(reason.contains("loaded_build_id=synapse-chrome-bridge-future"));
    assert!(reason.contains("service_worker_sha256_matches_expected=true"));
    assert!(reason.contains("repair=restart_or_reinstall_repo_built_daemon"));

    let stale_worker_reason = bridge_build_id_stale_reason(
        Some("synapse-chrome-bridge-future"),
        EXPECTED_EXTENSION_BUILD_ID,
        Some("different-worker-sha"),
        Some(TEST_SERVICE_WORKER_SHA256),
        Some("mismatch"),
    )
    .expect("build id mismatch");
    assert!(stale_worker_reason.contains("build_id=synapse-chrome-bridge-future"));
    assert!(!stale_worker_reason.contains("build_id_skew="));
}

#[test]
fn stale_bridge_error_names_setup_repair_surface() {
    let profile_install_state = SynapseChromeProfileInstallState::test_installed();
    let mut host = test_host_record();
    host.extension_build_id = Some("synapse-chrome-bridge-future".to_owned());
    host.extension_service_worker_sha256 = Some(TEST_SERVICE_WORKER_SHA256.to_owned());
    host.extension_service_worker_sha256_status = Some("ok".to_owned());
    let reason =
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state)
            .expect("stale reason");

    let error = ChromeDebuggerBridgeError::stale("openTab", "host-test", &host, &reason);

    assert_eq!(error.code(), error_codes::CHROME_BRIDGE_EXTENSION_STALE);
    assert!(
        error
            .detail()
            .contains("build_id_skew=daemon_expected_build_mismatch")
    );
    assert!(
        error
            .detail()
            .contains("mcp_setup_repair=call public MCP tool setup")
    );
    assert!(error.detail().contains("setup_script_path="));
    assert!(error.detail().contains("synapse-setup.ps1"));
}

#[test]
fn maintenance_pause_validation_is_bounded() {
    assert_eq!(
        validate_maintenance_reconnect_pause_ms(None).expect("default accepted"),
        DEFAULT_MAINTENANCE_RECONNECT_PAUSE_MS
    );
    assert_eq!(
        validate_maintenance_reconnect_pause_ms(Some(MIN_MAINTENANCE_RECONNECT_PAUSE_MS))
            .expect("min accepted"),
        MIN_MAINTENANCE_RECONNECT_PAUSE_MS
    );
    assert_eq!(
        validate_maintenance_reconnect_pause_ms(Some(MAX_MAINTENANCE_RECONNECT_PAUSE_MS))
            .expect("max accepted"),
        MAX_MAINTENANCE_RECONNECT_PAUSE_MS
    );
    for invalid in [
        MIN_MAINTENANCE_RECONNECT_PAUSE_MS - 1,
        MAX_MAINTENANCE_RECONNECT_PAUSE_MS + 1,
    ] {
        let error = validate_maintenance_reconnect_pause_ms(Some(invalid))
            .expect_err("invalid maintenance pause rejected");
        assert_eq!(error.code(), error_codes::TOOL_PARAMS_INVALID);
        assert!(error.detail().contains("pause_ms must be"));
        assert!(error.detail().contains(&format!("got {invalid}")));
    }

    assert_eq!(
        validate_maintenance_reconnect_pause_reason(" issue1410 setup ").expect("reason trimmed"),
        "issue1410 setup"
    );
    let error = validate_maintenance_reconnect_pause_reason("").expect_err("empty reason rejected");
    assert_eq!(error.code(), error_codes::TOOL_PARAMS_INVALID);
    assert!(error.detail().contains("reason must be 1..=256 chars"));
}

#[test]
fn maintenance_pause_only_requires_pause_capability() {
    let profile_install_state = SynapseChromeProfileInstallState::test_installed();
    let mut host = test_host_record();

    let reason = bridge_command_stale_reason_with_profile_state(
        &host,
        MAINTENANCE_RECONNECT_PAUSE_COMMAND,
        &profile_install_state,
    )
    .expect("missing maintenance capability");
    assert!(reason.contains("missing_capability=maintenancePauseReconnect"));

    host.extension_capabilities = [MAINTENANCE_RECONNECT_PAUSE_COMMAND]
        .into_iter()
        .map(str::to_owned)
        .collect();
    host.extension_build_id = Some("old-build".to_owned());
    host.extension_declared_build_sha256 = Some("old-sha".to_owned());
    host.extension_service_worker_sha256 = Some("old-worker-sha".to_owned());
    host.extension_service_worker_sha256_status = Some("mismatch".to_owned());
    host.extension_debugger_api_available = Some(false);

    assert_eq!(
        bridge_command_stale_reason_with_profile_state(
            &host,
            MAINTENANCE_RECONNECT_PAUSE_COMMAND,
            &profile_install_state,
        ),
        None
    );

    assert!(
        bridge_command_stale_reason_with_profile_state(&host, "openTab", &profile_install_state,)
            .is_some()
    );
}

#[test]
fn reload_self_uses_loaded_build_id_as_command_guard() {
    let mut snapshot = ChromeBridgeHostSnapshot {
        host_id: "chrome-native-stale".to_owned(),
        origin: format!("chrome-extension://{EXTENSION_ID}/"),
        extension_id: Some(EXTENSION_ID.to_owned()),
        extension_version: Some("0.1.0".to_owned()),
        extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
        extension_build_id: Some("synapse-chrome-bridge-older-but-reload-capable".to_owned()),
        extension_build_sha256: Some("old-sha".to_owned()),
        extension_declared_build_sha256: Some("old-sha".to_owned()),
        extension_service_worker_sha256: Some("old-worker-sha".to_owned()),
        extension_service_worker_sha256_status: Some("ok".to_owned()),
        extension_service_worker_sha256_source: Some(format!(
            "chrome-extension://{EXTENSION_ID}/service_worker.js"
        )),
        extension_service_worker_byte_length: Some(1234),
        extension_service_worker_sha256_error: None,
        expected_service_worker_sha256: Some(TEST_SERVICE_WORKER_SHA256.to_owned()),
        expected_service_worker_path: Some(
            r"C:\synapse-test\extension\service_worker.js".to_owned(),
        ),
        extension_capabilities: vec!["reloadSelf".to_owned()],
        extension_user_agent: Some("Chrome test".to_owned()),
        extension_debugger_api_available: Some(false),
        extension_popup_risk_suppression: None,
        pid: 0,
        parent_window: None,
        transport: Some("direct_http".to_owned()),
        registered_unix_ms: 1000,
        last_seen_unix_ms: 2000,
        last_disconnect_detail: None,
        last_detach_reason: None,
        extension_stale: true,
        extension_stale_reasons: vec!["build_id=old expected=new".to_owned()],
    };

    assert_eq!(
        reload_self_expected_loaded_build_id(&snapshot),
        Some("synapse-chrome-bridge-older-but-reload-capable")
    );

    snapshot.extension_build_id = Some(String::new());
    assert_eq!(reload_self_expected_loaded_build_id(&snapshot), None);

    snapshot.extension_build_id = None;
    assert_eq!(reload_self_expected_loaded_build_id(&snapshot), None);
}

#[test]
fn reload_wait_timeout_validation_is_bounded() {
    assert_eq!(
        validate_reload_wait_timeout(None).expect("default accepted"),
        DEFAULT_RELOAD_WAIT_TIMEOUT_MS
    );
    assert_eq!(validate_reload_wait_timeout(Some(1)).expect("min"), 1);
    assert_eq!(
        validate_reload_wait_timeout(Some(MAX_RELOAD_WAIT_TIMEOUT_MS)).expect("max"),
        MAX_RELOAD_WAIT_TIMEOUT_MS
    );
    for invalid in [0, MAX_RELOAD_WAIT_TIMEOUT_MS + 1] {
        let error =
            validate_reload_wait_timeout(Some(invalid)).expect_err("invalid timeout rejected");
        assert_eq!(error.code(), error_codes::TOOL_PARAMS_INVALID);
        assert!(error.detail().contains("wait_timeout_ms must be 1..="));
        assert!(error.detail().contains(&format!("got {invalid}")));
        assert!(error.detail().contains("before bridge reload command"));
    }
}

#[test]
fn normal_bridge_attach_disabled_is_local_refusal() {
    let error = ChromeDebuggerBridgeError::normal_bridge_attach_disabled(1234, "snapshot");

    assert_eq!(
        error.code(),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );
    assert_eq!(error.cdp_status(), CdpStatus::AttachFailed);
    assert!(
        error
            .detail()
            .contains("before queueing any Chrome command")
    );
    assert!(
        error
            .detail()
            .contains("explicit browser_debugger-profile lanes")
    );
    assert!(error.detail().contains("viewportEmulation"));
    assert!(
        error
            .detail()
            .contains("dedicated raw-CDP automation profile")
    );
    assert!(
        error
            .detail()
            .contains("scripts\\install-synapse-chrome-debugger.ps1")
    );
    assert!(error.detail().contains("raw CDP"));
}

#[test]
fn external_popup_risk_warning_blocks_until_suppressed() {
    let risks = vec![
        "profile=Profile 5 pref=Secure Preferences extension_id=fcoeoabgfenejglbffodgkkbkcdhcgfn name=\"Claude\" active_api=debugger,nativeMessaging".to_owned(),
        "native_messaging_process pid=26616 name=cmd.exe extension_id=fcoeoabgfenejglbffodgkkbkcdhcgfn".to_owned(),
    ];

    let warning = external_chrome_popup_risk_warning(&risks, false);

    assert!(warning.contains("external_chrome_popup_risk_blocking=true"));
    assert!(warning.contains("external_suppression_required"));
    assert!(warning.contains("risk_count=2"));
    assert!(warning.contains("fcoeoabgfenejglbffodgkkbkcdhcgfn"));
    assert!(warning.contains("fail closed"));

    let suppressed_warning = external_chrome_popup_risk_warning(&risks, true);
    assert!(suppressed_warning.contains("external_chrome_popup_risk_warning=true"));
    assert!(suppressed_warning.contains("covered_by_live_bridge_management"));
    assert!(suppressed_warning.contains("remaining_hazard_count=0"));
}

#[test]
fn chrome_extension_runtime_state_treats_disabled_permission_rows_as_not_enabled() {
    let setting = json!({
        "state": 0,
        "active_bit": false,
        "disable_reasons": [65536],
        "active_permissions": {
            "api": ["downloads", "nativeMessaging"]
        }
    });

    let runtime_state = chrome_extension_runtime_state(&setting);

    assert_eq!(runtime_state.state, Some(0));
    assert_eq!(runtime_state.active_bit, Some(false));
    assert_eq!(runtime_state.disable_reasons, vec![65536]);
    assert!(!runtime_state.runtime_enabled);
}

#[test]
fn chrome_extension_runtime_state_requires_enabled_state_even_with_active_permissions() {
    let setting = json!({
        "active_bit": false,
        "active_permissions": {
            "api": ["debugger", "nativeMessaging"]
        }
    });

    let runtime_state = chrome_extension_runtime_state(&setting);

    assert_eq!(runtime_state.state, None);
    assert_eq!(runtime_state.active_bit, Some(false));
    assert!(runtime_state.disable_reasons.is_empty());
    assert!(!runtime_state.runtime_enabled);
}

#[test]
fn chrome_extension_runtime_state_treats_enabled_state_as_runtime_enabled() {
    let setting = json!({
        "state": 1,
        "active_bit": false,
        "active_permissions": {
            "api": ["debugger", "nativeMessaging"]
        }
    });

    let runtime_state = chrome_extension_runtime_state(&setting);

    assert_eq!(runtime_state.state, Some(1));
    assert_eq!(runtime_state.active_bit, Some(false));
    assert!(runtime_state.disable_reasons.is_empty());
    assert!(runtime_state.runtime_enabled);
}

#[test]
fn chrome_extension_runtime_state_treats_absent_state_as_stale_not_enabled() {
    let setting = json!({
        "active_permissions": {
            "api": ["nativeMessaging"]
        }
    });

    let runtime_state = chrome_extension_runtime_state(&setting);

    assert_eq!(runtime_state.state, None);
    assert_eq!(runtime_state.active_bit, None);
    assert!(runtime_state.disable_reasons.is_empty());
    assert!(!runtime_state.runtime_enabled);
}

#[test]
fn external_popup_risk_treats_active_manifest_absent_state_as_enabled_risk() {
    let setting = json!({
        "active_bit": false,
        "active_permissions": {
            "api": ["debugger", "nativeMessaging"]
        },
        "granted_permissions": {
            "api": ["debugger", "nativeMessaging"]
        },
        "manifest": {
            "permissions": ["debugger", "nativeMessaging"]
        }
    });
    let runtime_state = chrome_extension_runtime_state(&setting);
    let active_or_manifest_hazards = hazard_api_permissions(
        active_api_permissions(&setting)
            .iter()
            .chain(manifest_api_permissions(&setting).iter())
            .map(String::as_str),
    );
    let granted_hazards =
        hazard_api_permissions(granted_api_permissions(&setting).iter().map(String::as_str));

    assert_eq!(runtime_state.state, None);
    assert_eq!(runtime_state.active_bit, Some(false));
    assert!(runtime_state.disable_reasons.is_empty());
    assert!(!runtime_state.runtime_enabled);
    assert_eq!(
        active_or_manifest_hazards,
        vec!["debugger".to_owned(), "nativeMessaging".to_owned()]
    );
    assert!(external_popup_risk_enabled(
        &runtime_state,
        !active_or_manifest_hazards.is_empty(),
        !granted_hazards.is_empty()
    ));
}

#[test]
fn external_popup_risk_ignores_absent_state_granted_only_residue() {
    let setting = json!({
        "active_permissions": {
            "api": ["alarms", "tabs"]
        },
        "granted_permissions": {
            "api": ["debugger", "nativeMessaging"]
        },
        "manifest": {
            "permissions": ["alarms", "tabs"]
        }
    });
    let runtime_state = chrome_extension_runtime_state(&setting);
    let active_or_manifest_hazards = hazard_api_permissions(
        active_api_permissions(&setting)
            .iter()
            .chain(manifest_api_permissions(&setting).iter())
            .map(String::as_str),
    );
    let granted_hazards =
        hazard_api_permissions(granted_api_permissions(&setting).iter().map(String::as_str));

    assert_eq!(runtime_state.state, None);
    assert!(active_or_manifest_hazards.is_empty());
    assert_eq!(
        granted_hazards,
        vec!["debugger".to_owned(), "nativeMessaging".to_owned()]
    );
    assert!(!external_popup_risk_enabled(
        &runtime_state,
        !active_or_manifest_hazards.is_empty(),
        !granted_hazards.is_empty()
    ));
}

#[test]
fn external_popup_risk_respects_disable_reasons() {
    let setting = json!({
        "disable_reasons": [65536],
        "active_permissions": {
            "api": ["nativeMessaging"]
        },
        "manifest": {
            "permissions": ["nativeMessaging"]
        }
    });
    let runtime_state = chrome_extension_runtime_state(&setting);
    let active_or_manifest_hazards = hazard_api_permissions(
        active_api_permissions(&setting)
            .iter()
            .chain(manifest_api_permissions(&setting).iter())
            .map(String::as_str),
    );

    assert_eq!(runtime_state.disable_reasons, vec![65536]);
    assert!(!external_popup_risk_enabled(
        &runtime_state,
        !active_or_manifest_hazards.is_empty(),
        false
    ));
}

#[test]
fn external_popup_risk_formatter_caps_noisy_readback() {
    let risks = (0..10)
        .map(|index| format!("risk-{index}"))
        .collect::<Vec<_>>();

    let formatted = format_external_chrome_popup_risks(&risks);

    assert!(formatted.contains("risk-0"));
    assert!(formatted.contains("risk-7"));
    assert!(!formatted.contains("risk-8 |"));
    assert!(formatted.ends_with("+2 more"));
}

// ---- #1558 aria_snapshot credential redaction (host defense-in-depth) ----

fn aria_test_node(
    name: &str,
    value: Option<&str>,
    input_type: Option<&str>,
    autocomplete: Option<&str>,
) -> ChromeDebuggerAriaSnapshotNode {
    ChromeDebuggerAriaSnapshotNode {
        element_id: "chrome-tab:1:frame:0:path:0.1".to_owned(),
        parent_element_id: None,
        depth: 1,
        role: "textbox".to_owned(),
        name: name.to_owned(),
        value: value.map(str::to_owned),
        enabled: true,
        focused: false,
        children_count: 0,
        input_type: input_type.map(str::to_owned),
        autocomplete: autocomplete.map(str::to_owned),
        redacted: false,
        value_length: None,
        value_hash: None,
    }
}

fn aria_test_result(
    nodes: Vec<ChromeDebuggerAriaSnapshotNode>,
) -> ChromeDebuggerAriaSnapshotResult {
    // Render a snapshot string that embeds each node's raw value the same way
    // the browser-side renderer does, so we can prove the scrub removes secrets.
    let snapshot = nodes
        .iter()
        .map(|node| {
            let mut line = format!("- {} \"{}\"", node.role, node.name);
            if let Some(value) = node.value.as_deref() {
                line.push_str(&format!(": \"{value}\""));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    ChromeDebuggerAriaSnapshotResult {
        target_id: "target-1".to_owned(),
        tab_id: 1,
        chrome_window_id: None,
        url: "https://example.test/login".to_owned(),
        title: "Login".to_owned(),
        ready_state: "complete".to_owned(),
        root_element_id: None,
        snapshot,
        node_count: nodes.len(),
        total_ax_nodes: nodes.len() as u32,
        nodes,
        max_nodes: 500,
        max_depth: 32,
        truncated_by_max_nodes: false,
        truncated_by_depth: false,
        frame_tree_frame_count: 1,
        attached_frame_target_count: 0,
        blocked_frame_targets: Vec::new(),
        frame_snapshot_errors: Vec::new(),
        readback_backend: "test".to_owned(),
        backend_tier_used: "test".to_owned(),
        required_foreground: false,
        target_candidate_count: 1,
        target_selection_reason: "test".to_owned(),
        extension_id: None,
    }
}

#[test]
fn aria_redaction_strips_password_field_value_and_snapshot() {
    const SENTINEL: &str = "S3cr3t-SENTINEL-A";
    let mut result = aria_test_result(vec![aria_test_node(
        "Password",
        Some(SENTINEL),
        Some("password"),
        None,
    )]);
    let before = result.nodes[0].value.clone();
    let redacted_count = redact_aria_snapshot(&mut result);
    let node = &result.nodes[0];
    println!(
        "readback=password before={before:?} after={:?} redacted_count={redacted_count}",
        node.value
    );
    assert_eq!(redacted_count, 1);
    assert!(node.redacted);
    assert!(node.value.is_none());
    assert_eq!(node.value_length, Some(SENTINEL.chars().count()));
    assert!(node.value_hash.is_some());
    // role/name preserved as safe evidence.
    assert_eq!(node.role, "textbox");
    assert_eq!(node.name, "Password");
    // Sentinel absent from node debug rendering AND from the snapshot string.
    assert!(!format!("{node:?}").contains(SENTINEL));
    assert!(!result.snapshot.contains(SENTINEL));
    assert!(result.snapshot.contains(ARIA_SNAPSHOT_REDACTED_MARKER));
}

#[test]
fn aria_redaction_strips_one_time_code_autocomplete() {
    const SENTINEL: &str = "123456-SENTINEL";
    let mut result = aria_test_result(vec![aria_test_node(
        "Verification code",
        Some(SENTINEL),
        Some("text"),
        Some("one-time-code"),
    )]);
    let redacted_count = redact_aria_snapshot(&mut result);
    let node = &result.nodes[0];
    println!(
        "readback=one-time-code after={:?} redacted_count={redacted_count}",
        node.value
    );
    assert_eq!(redacted_count, 1);
    assert!(node.redacted);
    assert!(node.value.is_none());
    assert_eq!(node.value_length, Some(SENTINEL.chars().count()));
    assert!(node.value_hash.is_some());
    assert!(!result.snapshot.contains(SENTINEL));
}

#[test]
fn aria_redaction_strips_hidden_input_token() {
    const SENTINEL: &str = "hidden-token-SENTINEL-xyz";
    let mut result = aria_test_result(vec![aria_test_node(
        "csrf_field",
        Some(SENTINEL),
        Some("hidden"),
        None,
    )]);
    let redacted_count = redact_aria_snapshot(&mut result);
    let node = &result.nodes[0];
    println!(
        "readback=hidden after={:?} redacted_count={redacted_count}",
        node.value
    );
    assert_eq!(redacted_count, 1);
    assert!(node.redacted);
    assert!(node.value.is_none());
    assert!(!result.snapshot.contains(SENTINEL));
}

#[test]
fn aria_redaction_preserves_ordinary_textbox() {
    const VISIBLE: &str = "hello world";
    let mut result = aria_test_result(vec![aria_test_node(
        "Search",
        Some(VISIBLE),
        Some("text"),
        Some("off"),
    )]);
    let redacted_count = redact_aria_snapshot(&mut result);
    let node = &result.nodes[0];
    println!(
        "readback=search after={:?} redacted_count={redacted_count}",
        node.value
    );
    assert_eq!(redacted_count, 0);
    assert!(!node.redacted);
    assert_eq!(node.value.as_deref(), Some(VISIBLE));
    assert!(node.value_hash.is_none());
    assert!(result.snapshot.contains(VISIBLE));
}

#[test]
fn aria_redaction_fails_closed_on_ambiguous_high_entropy_value() {
    const SENTINEL: &str = "sk_live_SENTINEL_0xDEADBEEFCAFE";
    // No input_type, no autocomplete, benign-looking name: metadata is unknown,
    // so a token-like value must fail closed and be redacted.
    let mut result = aria_test_result(vec![aria_test_node("field", Some(SENTINEL), None, None)]);
    let redacted_count = redact_aria_snapshot(&mut result);
    let node = &result.nodes[0];
    println!(
        "readback=ambiguous after={:?} redacted_count={redacted_count}",
        node.value
    );
    assert_eq!(redacted_count, 1);
    assert!(node.redacted);
    assert!(node.value.is_none());
    assert_eq!(node.value_length, Some(SENTINEL.chars().count()));
    assert!(node.value_hash.is_some());
    assert!(!result.snapshot.contains(SENTINEL));
}

#[test]
fn aria_redaction_scrubs_all_sentinels_from_snapshot_string() {
    const PW: &str = "S3cr3t-SENTINEL-A";
    const OTP: &str = "123456-SENTINEL";
    const HIDDEN: &str = "hidden-token-SENTINEL-xyz";
    const AMBIG: &str = "sk_live_SENTINEL_0xDEADBEEFCAFE";
    const VISIBLE: &str = "hello world";
    let mut result = aria_test_result(vec![
        aria_test_node("Password", Some(PW), Some("password"), None),
        aria_test_node("Code", Some(OTP), Some("text"), Some("one-time-code")),
        aria_test_node("csrf", Some(HIDDEN), Some("hidden"), None),
        aria_test_node("field", Some(AMBIG), None, None),
        aria_test_node("Search", Some(VISIBLE), Some("text"), Some("off")),
    ]);
    let redacted_count = redact_aria_snapshot(&mut result);
    println!(
        "readback=snapshot-scrub redacted_count={redacted_count} snapshot={:?}",
        result.snapshot
    );
    assert_eq!(redacted_count, 4);
    for sentinel in [PW, OTP, HIDDEN, AMBIG] {
        assert!(
            !result.snapshot.contains(sentinel),
            "sentinel {sentinel} survived in snapshot"
        );
        for node in &result.nodes {
            assert_ne!(node.value.as_deref(), Some(sentinel));
        }
    }
    // The ordinary visible value is preserved.
    assert!(result.snapshot.contains(VISIBLE));
}

#[test]
fn aria_redaction_honors_browser_supplied_redacted_flag() {
    // A stale/older browser may already have redacted (value stripped) but the
    // host must still count it and keep the evidence fields intact.
    let mut node = aria_test_node("Password", None, Some("password"), None);
    node.redacted = true;
    node.value_length = Some(12);
    node.value_hash = Some("sha256:deadbeefdeadbeef".to_owned());
    let mut result = aria_test_result(vec![node]);
    let redacted_count = redact_aria_snapshot(&mut result);
    let node = &result.nodes[0];
    println!(
        "readback=browser-redacted after={:?} redacted_count={redacted_count}",
        node.value
    );
    assert_eq!(redacted_count, 1);
    assert!(node.redacted);
    assert!(node.value.is_none());
    assert_eq!(node.value_length, Some(12));
    assert_eq!(node.value_hash.as_deref(), Some("sha256:deadbeefdeadbeef"));
}
