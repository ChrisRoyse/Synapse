use std::time::Duration;

use proptest::prelude::*;
use synapse_core::{ElementId, element_id, error_codes};
use tokio::net::TcpListener;

use crate::*;

fn event(
    seq: u64,
    at_ms: u64,
    element: Option<ElementId>,
    kind: AccessibleEventKind,
) -> AccessibleEvent {
    AccessibleEvent {
        seq,
        at_ms,
        window_id: 0x1234,
        element_id: element,
        kind,
        name: None,
        value: None,
    }
}

#[test]
fn coalesce_empty_input_prints_before_after_state() {
    let before = Vec::<AccessibleEvent>::new();
    println!("readback=coalesced_events edge=empty before={before:?}");
    let after = coalesce_events(before, Duration::from_millis(50));
    println!("readback=coalesced_events edge=empty after={after:?}");
    assert!(after.is_empty());
}

#[test]
fn coalesce_same_key_within_window_keeps_latest_state() -> Result<(), Box<dyn std::error::Error>> {
    let id = ElementId::parse("0x1234:00000001")?;
    let mut first = event(1, 0, Some(id.clone()), AccessibleEventKind::NameChanged);
    first.name = Some("old".to_owned());
    let mut second = event(2, 49, Some(id), AccessibleEventKind::NameChanged);
    second.name = Some("new".to_owned());
    let before = vec![first, second];
    println!("readback=coalesced_events edge=within_50ms before={before:?}");
    let after = coalesce_events(before, Duration::from_millis(50));
    println!("readback=coalesced_events edge=within_50ms after={after:?}");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].name.as_deref(), Some("new"));
    assert_eq!(after[0].seq, 2);
    Ok(())
}

#[test]
fn coalesce_exact_boundary_does_not_merge() -> Result<(), Box<dyn std::error::Error>> {
    let id = ElementId::parse("0x1234:00000002")?;
    let before = vec![
        event(1, 0, Some(id.clone()), AccessibleEventKind::FocusChanged),
        event(2, 50, Some(id), AccessibleEventKind::FocusChanged),
    ];
    println!("readback=coalesced_events edge=exact_50ms before={before:?}");
    let after = coalesce_events(before, Duration::from_millis(50));
    println!("readback=coalesced_events edge=exact_50ms after={after:?}");
    assert_eq!(after.len(), 2);
    Ok(())
}

#[test]
fn value_debounce_rapid_typing_keeps_first_and_latest_state()
-> Result<(), Box<dyn std::error::Error>> {
    let id = ElementId::parse("0x1234:00000003")?;
    let before: Vec<_> = "abcdef"
        .chars()
        .enumerate()
        .map(|(index, character)| {
            let mut item = event(
                u64::try_from(index).unwrap_or(u64::MAX),
                u64::try_from(index * 20).unwrap_or(u64::MAX),
                Some(id.clone()),
                AccessibleEventKind::ValueChanged,
            );
            item.value = Some(character.to_string());
            item
        })
        .collect();
    println!("readback=debounced_events edge=rapid_typing before={before:?}");
    let after = debounce_value_changes(before, Duration::from_millis(200));
    println!("readback=debounced_events edge=rapid_typing after={after:?}");
    assert!(after.len() <= 2);
    assert_eq!(
        after.last().and_then(|item| item.value.as_deref()),
        Some("f")
    );
    Ok(())
}

#[test]
fn value_debounce_focus_loss_flushes_pending_state() -> Result<(), Box<dyn std::error::Error>> {
    let id = ElementId::parse("0x1234:00000004")?;
    let mut first = event(1, 0, Some(id.clone()), AccessibleEventKind::ValueChanged);
    first.value = Some("a".to_owned());
    let mut second = event(2, 20, Some(id.clone()), AccessibleEventKind::ValueChanged);
    second.value = Some("b".to_owned());
    let focus = event(3, 25, Some(id), AccessibleEventKind::FocusChanged);
    let before = vec![first, second, focus];
    println!("readback=debounced_events edge=focus_loss before={before:?}");
    let after = debounce_value_changes(before, Duration::from_millis(200));
    println!("readback=debounced_events edge=focus_loss after={after:?}");
    assert_eq!(after.len(), 3);
    assert_eq!(after[1].value.as_deref(), Some("b"));
    assert_eq!(after[2].kind, AccessibleEventKind::FocusChanged);
    Ok(())
}

#[test]
fn runtime_id_hex_round_trips_through_composite_element_id()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = [42, -1, 0x1234_abcd_u32.cast_signed()];
    let runtime_hex = runtime_id_hex(&runtime);
    let id = element_id(0x12ab, &runtime_hex);
    println!("readback=element_id edge=runtime_hex before={runtime:?}");
    println!("readback=element_id edge=runtime_hex after={id}");
    let parts = id.parts()?;
    assert_eq!(parts.hwnd, 0x12ab);
    assert_eq!(parts.runtime_id_hex, "0000002affffffff1234abcd");
    Ok(())
}

#[test]
fn non_windows_uia_reports_not_available() {
    #[cfg(not(windows))]
    {
        let before = "focused_window";
        println!("readback=a11y_error edge=non_windows before={before}");
        let after = focused_window();
        println!("readback=a11y_error edge=non_windows after={after:?}");
        assert_eq!(
            after.err().map(|err| err.code()),
            Some(error_codes::A11Y_NOT_AVAILABLE)
        );
    }
}

#[test]
fn implicit_focus_window_refuses_before_touching_os_focus() {
    let hwnd = 0x1234_i64;
    println!("readback=foreground_activation_guard edge=implicit before_hwnd=0x{hwnd:x}");
    let after = focus_window(hwnd);
    println!("readback=foreground_activation_guard edge=implicit after={after:?}");
    assert_eq!(
        after.err().map(|err| err.code()),
        Some(error_codes::FOREGROUND_ACTIVATION_REFUSED)
    );
}

#[test]
fn foreground_activation_intent_records_caller_and_reason() {
    let intent = ForegroundActivationIntent::OperatorRequested {
        caller: "supporting_regression_test",
    };
    println!("readback=foreground_activation_intent before={intent:?}");
    println!(
        "readback=foreground_activation_intent after_caller={} after_reason={}",
        intent.caller(),
        intent.reason()
    );
    assert_eq!(intent.caller(), "supporting_regression_test");
    assert_eq!(intent.reason(), "operator_requested");
}

#[tokio::test]
async fn cdp_probe_non_chromium_is_explicitly_not_chromium() {
    let before = ("notepad.exe", Vec::<u16>::new());
    println!("readback=cdp_diagnostics edge=non_chromium before={before:?}");
    let after = probe_chromium_cdp("notepad.exe", &[], Duration::from_millis(10)).await;
    println!("readback=cdp_diagnostics edge=non_chromium after={after:?}");
    assert_eq!(after.status, CdpStatus::NotChromium);
    assert!(after.reason_code.is_none());
}

#[tokio::test]
async fn cdp_probe_chromium_without_port_surfaces_unreachable_code() {
    let before = ("chrome.exe", Vec::<u16>::new());
    println!("readback=cdp_diagnostics edge=no_debug_port before={before:?}");
    let after = probe_chromium_cdp("chrome.exe", &[], Duration::from_millis(10)).await;
    println!("readback=cdp_diagnostics edge=no_debug_port after={after:?}");
    assert_eq!(after.status, CdpStatus::Unreachable);
    assert_eq!(
        after.reason_code.as_deref(),
        Some(error_codes::A11Y_CDP_UNREACHABLE)
    );
    assert!(
        after
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("chrome://inspect/#remote-debugging"))
    );
}

#[tokio::test]
async fn cdp_probe_reachable_debug_port_surfaces_capabilities()
-> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    println!("readback=cdp_diagnostics edge=reachable_port before=port:{port}");
    let after = probe_chromium_cdp("msedge.exe", &[port], Duration::from_secs(1)).await;
    println!("readback=cdp_diagnostics edge=reachable_port after={after:?}");
    assert_eq!(after.status, CdpStatus::Ok);
    assert_eq!(
        after.endpoint.as_deref(),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    assert_eq!(after.capabilities, cdp_capabilities());
    Ok(())
}

// === Blocking probe (the path used by the synchronous perception pipeline) ===
//
// These exercise the *real* `probe_chromium_cdp_blocking` used by observe/find
// against real loopback sockets — no mocks — so the #683 diagnostics contract is
// locked at its source (issue #691, supporting evidence; not an FSV harness).

#[test]
fn blocking_cdp_probe_non_chromium_is_explicitly_not_chromium() {
    let before = ("notepad.exe", Vec::<u16>::new());
    println!("readback=cdp_blocking edge=non_chromium before={before:?}");
    let after = probe_chromium_cdp_blocking("notepad.exe", &[], Duration::from_millis(50));
    println!("readback=cdp_blocking edge=non_chromium after={after:?}");
    assert_eq!(after.status, CdpStatus::NotChromium);
    assert!(after.reason_code.is_none());
    assert!(after.capabilities.is_empty());
}

#[test]
fn blocking_cdp_probe_chromium_without_port_surfaces_unreachable_code() {
    // A free, unbound port: connecting must be refused, yielding Unreachable.
    let before = ("chrome.exe", vec![1u16]);
    println!("readback=cdp_blocking edge=no_debug_port before={before:?}");
    let after = probe_chromium_cdp_blocking("chrome.exe", &[1], Duration::from_millis(50));
    println!("readback=cdp_blocking edge=no_debug_port after={after:?}");
    assert_eq!(after.status, CdpStatus::Unreachable);
    assert_eq!(
        after.reason_code.as_deref(),
        Some(error_codes::A11Y_CDP_UNREACHABLE)
    );
    assert!(
        after
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("SYNAPSE_CDP_PORTS"))
    );
}

#[test]
fn blocking_cdp_probe_reachable_debug_port_surfaces_capabilities()
-> Result<(), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    println!("readback=cdp_blocking edge=reachable_port before=port:{port}");
    let after = probe_chromium_cdp_blocking("chrome.exe", &[port], Duration::from_secs(1));
    println!("readback=cdp_blocking edge=reachable_port after={after:?}");
    assert_eq!(after.status, CdpStatus::Ok);
    assert_eq!(
        after.endpoint.as_deref(),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    assert_eq!(after.capabilities, cdp_capabilities());
    Ok(())
}

#[test]
fn launched_port_registry_is_consulted_first_then_defaults() {
    // Use a high, unlikely-collision pid for test isolation.
    let pid = 4_294_967_000_u32;
    forget_launched_port(pid);
    let before = candidate_ports_for_pid(pid);
    println!("readback=cdp_port_registry edge=unregistered before={before:?}");
    assert_eq!(before, vec![DEFAULT_CDP_PORT]);

    register_launched_port(pid, 51_999);
    let after = candidate_ports_for_pid(pid);
    println!("readback=cdp_port_registry edge=registered after={after:?}");
    assert_eq!(after.first().copied(), Some(51_999));
    assert!(after.contains(&DEFAULT_CDP_PORT));

    forget_launched_port(pid);
    let cleared = candidate_ports_for_pid(pid);
    println!("readback=cdp_port_registry edge=forgotten after={cleared:?}");
    assert_eq!(cleared, vec![DEFAULT_CDP_PORT]);
}

proptest! {
    #[test]
    fn coalescing_never_outputs_same_key_inside_window(times in proptest::collection::vec(0_u64..500, 1..80)) {
        let id = element_id(0x1234, "00000005");
        let mut sorted = times;
        sorted.sort_unstable();
        let input: Vec<_> = sorted
            .iter()
            .enumerate()
            .map(|(index, at_ms)| event(u64::try_from(index).unwrap_or(u64::MAX), *at_ms, Some(id.clone()), AccessibleEventKind::NameChanged))
            .collect();
        let output = coalesce_events(input, Duration::from_millis(50));
        for pair in output.windows(2) {
            prop_assert!(
                pair[1].at_ms.saturating_sub(pair[0].at_ms) >= 50,
                "readback=coalesced_events edge=proptest after={output:?}"
            );
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_win_event_hook_apartment_readback_is_sta() -> Result<(), Box<dyn std::error::Error>> {
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    println!("readback=winevent_hook edge=apartment before=unsubscribed");
    let subscription = subscribe_win_events(sender)?;
    let readback = subscription.readback().clone();
    println!("readback=winevent_hook edge=apartment after={readback:?}");
    assert!(readback.apartment.is_sta_family());
    assert_eq!(readback.hook_count, 10);
    assert_eq!(readback.event_ids.len(), 10);
    drop(subscription);
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_uia_worker_readback_is_mta_windowless() -> Result<(), Box<dyn std::error::Error>> {
    println!("readback=uia_worker edge=threading before=uninitialized_or_existing");
    let readback = uia_worker_readback()?;
    println!("readback=uia_worker edge=threading after={readback:?}");
    assert!(readback.is_mta_windowless());
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_direct_uielement_api_fails_closed() {
    println!("readback=uia_direct_api edge=focused_window before=call");
    let after = focused_window();
    println!("readback=uia_direct_api edge=focused_window after={after:?}");
    let Err(error) = after else {
        panic!("direct UIElement API unexpectedly returned a COM element");
    };
    assert_eq!(error.code(), error_codes::OBSERVE_INTERNAL);
    assert!(error.to_string().contains("direct UIElement"));
}

#[cfg(windows)]
#[test]
fn windows_foreground_context_invalid_hwnd_reports_no_foreground() {
    let before = 0_i64;
    println!("readback=foreground_context edge=invalid_hwnd before=0x{before:x}");
    let after = foreground_context(before);
    println!("readback=foreground_context edge=invalid_hwnd after={after:?}");
    assert_eq!(
        after.err().map(|error| error.code()),
        Some(error_codes::A11Y_NO_FOREGROUND)
    );
}

#[cfg(windows)]
#[test]
fn windows_foreground_snapshot_round_trips_element_id() -> Result<(), Box<dyn std::error::Error>> {
    println!("readback=uia_snapshot edge=depth2 before=focused_window_resolved");
    let tree = match snapshot_focused_window(2) {
        Ok(tree) => tree,
        // No focused window on this host right now (locked screen, focus on the
        // desktop itself, unattended run): there is nothing to snapshot, which is
        // a valid environment state rather than a test failure. This unit test
        // exercises the real UIA snapshot/round-trip whenever a foreground window
        // IS present and skips otherwise — the same opportunistic approach the
        // change-driven capture tests use. (It cannot use the synthetic-foreground
        // fixture: that lives in the MCP layer, not the raw a11y UIA path.)
        Err(error) if error.code() == error_codes::A11Y_NO_FOREGROUND => {
            println!("readback=uia_snapshot edge=depth2 after=no_foreground_window_skip");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    println!(
        "readback=uia_snapshot edge=depth2 after=root:{} nodes:{} max_depth:{}",
        tree.root,
        tree.nodes.len(),
        tree.max_depth
    );
    assert!(!tree.nodes.is_empty());
    let round_trip = snapshot_element(&tree.root, 0)?;
    println!(
        "readback=uia_snapshot edge=round_trip after=root:{} nodes:{}",
        round_trip.root,
        round_trip.nodes.len()
    );
    assert_eq!(round_trip.root, tree.root);
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_millis_since_last_input_reads_real_session_idle()
-> Result<(), Box<dyn std::error::Error>> {
    println!("readback=last_input edge=real_session before=GetLastInputInfo");
    let first = millis_since_last_input()?;
    let second = millis_since_last_input()?;
    println!("readback=last_input edge=real_session after=first:{first}ms second:{second}ms");
    // Two consecutive reads without injected input: the second read is the
    // same instant or later on the session tick clock (modulo an input event
    // landing between them, which resets both toward zero).
    assert!(
        second >= first || second < 1_000,
        "second read ({second}ms) must advance from first ({first}ms) unless real input reset it"
    );
    Ok(())
}
