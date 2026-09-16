//! Regression test for the CDP diagnostics contract that epic #682 depends on
//! (issue #691). This is *supporting evidence* per AGENTS.md D1 — NOT an FSV
//! harness, and deliberately not named `*_fsv`. Manual FSV against a real Chrome
//! remains the shipping gate; this guards the silent-fallthrough regression
//! (#683) from recurring at the `assemble()` boundary where `observe`/`find`
//! build their diagnostics.
//!
//! What it locks:
//! * Chromium foreground, no debug port →
//!   `diagnostics.cdp.status = A11Y_CDP_UNREACHABLE` with matching
//!   `reason_code`, and a named `web_path` (never a silent absent field).
//! * Chromium foreground, CDP attached → `diagnostics.cdp.status = ok` and DOM
//!   elements present in the observation.

use std::collections::BTreeMap;

use synapse_core::{
    AccessibleNode, CdpCapability, CdpDiagnostics, CdpStatus, ForegroundContext, Rect,
    SensorStatus, WebPerceptionPath, element_id, error_codes,
};
use synapse_perception::{ObservationInput, ObserveInclude, assemble};

fn chromium_foreground() -> ForegroundContext {
    ForegroundContext {
        hwnd: 0x2200,
        pid: 7777,
        process_name: "chrome.exe".to_owned(),
        process_path: "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe".to_owned(),
        window_title: "Apply to YC | Y Combinator - Google Chrome".to_owned(),
        window_bounds: Rect {
            x: 0,
            y: 0,
            w: 1600,
            h: 900,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: Some("chrome".to_owned()),
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    }
}

/// The collapsed UIA-only tree a normally-launched Chrome yields: window → pane
/// → region, with zero document/link/button nodes. This is exactly the trap the
/// epic documents.
fn collapsed_uia_nodes() -> Vec<AccessibleNode> {
    ["Window", "Pane", "Region"]
        .iter()
        .enumerate()
        .map(|(index, role)| AccessibleNode {
            element_id: element_id(0x2200, &format!("0000220000{index:06x}")),
            parent: (index > 0).then(|| element_id(0x2200, "0000220000000000")),
            name: String::new(),
            role: (*role).to_owned(),
            automation_id: None,
            value: None,
            bbox: Rect {
                x: 0,
                y: 0,
                w: 1600,
                h: 900,
            },
            enabled: true,
            focused: index == 0,
            patterns: Vec::new(),
            children_count: u32::from(index < 2),
            depth: u32::try_from(index).unwrap_or(0),
        })
        .collect()
}

fn base_input() -> ObservationInput {
    let mut input = ObservationInput::new(chromium_foreground());
    input.elements = collapsed_uia_nodes();
    input.a11y_status = SensorStatus::Healthy;
    input.capture_status = SensorStatus::Healthy;
    input.sensor_latency_ms = BTreeMap::from([("a11y".to_owned(), 1.0)]);
    input
}

#[test]
fn no_debug_port_surfaces_unreachable_status_and_named_web_path() {
    let mut input = base_input();
    // Outcome the synchronous probe produces for a no-port Chrome.
    input.cdp = Some(CdpDiagnostics::unreachable(
        "chrome.exe",
        error_codes::A11Y_CDP_UNREACHABLE,
    ));
    input.web_path = Some(WebPerceptionPath::UiaOnly);

    println!(
        "readback=cdp_contract edge=no_debug_port before=elements:{} cdp:{:?} web_path:{:?}",
        input.elements.len(),
        input.cdp.as_ref().map(|cdp| cdp.status),
        input.web_path,
    );

    let observation = assemble(ObserveInclude::default(), input).expect("assemble succeeds");
    let cdp = observation
        .diagnostics
        .cdp
        .as_ref()
        .expect("Chromium foreground must carry cdp diagnostics — no silent fallthrough");

    println!(
        "readback=cdp_contract edge=no_debug_port after=cdp.status:{:?} reason:{:?} web_path:{:?}",
        cdp.status, cdp.reason_code, observation.diagnostics.web_path
    );

    assert_eq!(cdp.status, CdpStatus::Unreachable);
    assert_eq!(
        cdp.reason_code.as_deref(),
        Some(error_codes::A11Y_CDP_UNREACHABLE)
    );
    assert_eq!(
        observation.diagnostics.web_path,
        Some(WebPerceptionPath::UiaOnly),
        "no-port Chromium must name its web_path, not leave it absent"
    );
}

#[test]
fn attached_cdp_surfaces_ok_status_and_dom_elements() {
    let mut input = base_input();
    // Simulate a successful attach: DOM nodes folded into elements, status ok.
    let dom_button = AccessibleNode {
        element_id: element_id(0x2200, "00002201cdp00001"),
        parent: Some(element_id(0x2200, "0000220000000000")),
        name: "Apply".to_owned(),
        role: "button".to_owned(),
        automation_id: Some("cdp:backendNodeId=42".to_owned()),
        value: None,
        bbox: Rect {
            x: 720,
            y: 480,
            w: 120,
            h: 40,
        },
        enabled: true,
        focused: false,
        patterns: Vec::new(),
        children_count: 0,
        depth: 2,
    };
    input.elements.push(dom_button);
    input.cdp = Some(CdpDiagnostics {
        process_name: "chrome.exe".to_owned(),
        status: CdpStatus::Ok,
        endpoint: Some("http://127.0.0.1:9222".to_owned()),
        checked_ports: vec![9222],
        checked_endpoints: vec!["http://127.0.0.1:9222".to_owned()],
        reason_code: None,
        detail: None,
        capabilities: vec![CdpCapability::AccessibilityFullAxTree],
        attached_node_count: Some(1),
        selected_target_id: Some("target-1".to_owned()),
        selected_session_id: Some("session-1".to_owned()),
        target_selection_reason: Some("url_hint".to_owned()),
        target_candidate_count: Some(1),
        frame_tree_frame_count: Some(1),
        attached_frame_target_count: Some(0),
        blocked_frame_targets: Vec::new(),
        frame_snapshot_errors: Vec::new(),
    });
    input.web_path = Some(WebPerceptionPath::Cdp);

    println!(
        "readback=cdp_contract edge=attached before=elements:{} web_path:{:?}",
        input.elements.len(),
        input.web_path
    );

    let observation = assemble(ObserveInclude::default(), input).expect("assemble succeeds");
    let cdp = observation
        .diagnostics
        .cdp
        .as_ref()
        .expect("attached Chromium foreground carries cdp diagnostics");

    let dom_present = observation
        .elements
        .iter()
        .any(|node| node.role == "button" && node.name == "Apply");

    println!(
        "readback=cdp_contract edge=attached after=cdp.status:{:?} web_path:{:?} dom_button_present:{dom_button_present} target:{:?} session:{:?} reason:{:?} candidates:{:?}",
        cdp.status,
        observation.diagnostics.web_path,
        cdp.selected_target_id,
        cdp.selected_session_id,
        cdp.target_selection_reason,
        cdp.target_candidate_count,
        dom_button_present = dom_present
    );

    assert_eq!(cdp.status, CdpStatus::Ok);
    assert_eq!(cdp.selected_target_id.as_deref(), Some("target-1"));
    assert_eq!(cdp.selected_session_id.as_deref(), Some("session-1"));
    assert_eq!(cdp.target_selection_reason.as_deref(), Some("url_hint"));
    assert_eq!(cdp.target_candidate_count, Some(1));
    assert_eq!(
        observation.diagnostics.web_path,
        Some(WebPerceptionPath::Cdp)
    );
    assert!(
        dom_present,
        "attached CDP path must expose DOM elements (the Apply button) as queryable nodes"
    );
}
