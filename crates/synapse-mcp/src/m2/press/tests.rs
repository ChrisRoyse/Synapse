use std::{collections::BTreeMap, sync::Arc, time::Duration};

use synapse_action::{ActionEmitter, RecordedInput, RecordingBackend};
use synapse_core::{
    Action, Backend, OcrBackend, PerceptionMode, Profile, ProfileBackends, ProfileCapture,
    ProfileCaptureTarget, ProfileDetection, ProfileOcr, ProfileUseScope, error_codes,
};
use tokio_util::sync::CancellationToken;

use super::{
    act_keymap_with_handle, act_press_with_handle, cdp_key_strokes,
    keys::{key, normalized_keys},
    live::execute_live_press_sequence,
    record::event_sequence,
    schema::{
        ActKeymapParams, ActPressParams, PressBackend, default_hold_ms, default_press_backend,
    },
};

#[tokio::test]
async fn recording_backend_readback_orders_chord_and_default_hold() {
    let (handle, _snapshot_handle, _emitter) = ActionEmitter::channel();
    let recording = Arc::new(RecordingBackend::new());
    let params = ActPressParams {
        keys: vec!["shift".to_owned(), "ctrl".to_owned(), "s".to_owned()],
        hold_ms: default_hold_ms(),
        backend: default_press_backend(),
        verify_delta: false,
        allow_foreground_change: false,
        expected_foreground_process_regex: None,
        expected_foreground_title_regex: None,
        verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
        window_hwnd: None,
        cdp_target_id: None,
        auto_wait: false,
        auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        auto_wait_element_id: None,
    };
    let before = recording.events();
    println!("readback=act_press_recording edge=ordered_chord before={before:?}");

    let response = act_press_with_handle(handle, Some(Arc::clone(&recording)), None, params)
        .await
        .unwrap_or_else(|error| panic!("act_press recording should succeed: {error}"));
    let after = recording.events();
    let sequence = event_sequence(&after);
    println!(
        "readback=act_press_recording edge=ordered_chord after={after:?} sequence={sequence} keys_pressed={}",
        response.keys_pressed
    );

    assert!(response.ok);
    assert_eq!(response.keys_pressed, 3);
    assert_eq!(
        sequence,
        "down:ctrl>down:shift>down:s>delay:33>up:s>up:shift>up:ctrl"
    );
}

#[tokio::test]
async fn keymap_alias_resolves_profile_binding_and_records_chord() {
    let (handle, _snapshot_handle, _emitter) = ActionEmitter::channel();
    let recording = Arc::new(RecordingBackend::new());
    let profile = profile_with_keymap([("spellbook", "ctrl+b")]);
    let params = ActKeymapParams {
        alias: " SpellBook ".to_owned(),
        hold_ms: default_hold_ms(),
        backend: default_press_backend(),
        window_hwnd: None,
        cdp_target_id: None,
    };
    let before = recording.events();
    println!("readback=act_keymap_recording edge=alias_chord before={before:?}");

    let response =
        act_keymap_with_handle(handle, Some(Arc::clone(&recording)), None, &profile, params)
            .await
            .unwrap_or_else(|error| panic!("act_keymap recording should succeed: {error}"));
    let after = recording.events();
    let sequence = event_sequence(&after);
    println!(
        "readback=act_keymap_recording edge=alias_chord after={after:?} sequence={sequence} alias={} binding={} resolved={:?}",
        response.alias, response.resolved_binding, response.resolved_keys
    );

    assert!(response.ok);
    assert_eq!(response.alias, "spellbook");
    assert_eq!(response.resolved_binding, "ctrl+b");
    assert_eq!(response.resolved_keys, ["ctrl".to_owned(), "b".to_owned()]);
    assert_eq!(response.keys_pressed, 2);
    assert_eq!(response.backend_used, "software");
    assert_eq!(sequence, "down:ctrl>down:b>delay:33>up:b>up:ctrl");
}

#[tokio::test]
async fn keymap_alias_missing_fails_closed() {
    let (handle, _snapshot_handle, _emitter) = ActionEmitter::channel();
    let recording = Arc::new(RecordingBackend::new());
    let profile = profile_with_keymap([("inventory", "i")]);
    let params = ActKeymapParams {
        alias: "target_nearest_npc".to_owned(),
        hold_ms: default_hold_ms(),
        backend: default_press_backend(),
        window_hwnd: None,
        cdp_target_id: None,
    };
    let before = recording.events();
    println!("readback=act_keymap_recording edge=missing_alias before={before:?}");

    let error =
        match act_keymap_with_handle(handle, Some(Arc::clone(&recording)), None, &profile, params)
            .await
        {
            Ok(response) => panic!("missing keymap alias should fail closed: {response:?}"),
            Err(error) => error,
        };
    let after = recording.events();
    println!("readback=act_keymap_recording edge=missing_alias after={after:?} error={error}");

    assert!(after.is_empty());
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&serde_json::json!(error_codes::PROFILE_KEYMAP_INVALID))
    );
}

#[tokio::test]
async fn live_press_sequence_leaves_actor_available_for_release_all_mid_hold() {
    let cancel = CancellationToken::new();
    let recording = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), recording.clone());
    let started_events = recording.events();
    println!(
        "readback=act_press_live_sequence edge=mid_hold_release before_events={started_events:?}"
    );

    let (press, before_release) =
        spawn_press_and_wait_for_held_key(handle.clone(), &snapshot_handle).await;
    println!(
        "readback=act_press_live_sequence edge=mid_hold_release before_release={before_release:?}"
    );

    handle
        .execute(Action::ReleaseAll)
        .await
        .unwrap_or_else(|error| panic!("release_all should execute during hold: {error}"));
    let after_release = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after release_all should succeed: {error}"));
    println!(
        "readback=act_press_live_sequence edge=mid_hold_release after_release={after_release:?}"
    );
    assert!(after_release.held_keys.is_empty());

    press
        .await
        .unwrap_or_else(|error| panic!("press task should join: {error}"))
        .unwrap_or_else(|error| panic!("press task should tolerate prior release_all: {error}"));
    let final_events = recording.events();
    println!(
        "readback=act_press_live_sequence edge=mid_hold_release after_events={final_events:?}"
    );
    assert!(
        final_events
            .iter()
            .any(|event| matches!(event, RecordedInput::ReleaseAll { .. }))
    );

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter should join: {error}"));
    assert!(final_snapshot.held_keys.is_empty());
}

async fn spawn_press_and_wait_for_held_key(
    handle: synapse_action::ActionHandle,
    snapshot_handle: &synapse_action::ActionEmitterSnapshotHandle,
) -> (
    tokio::task::JoinHandle<Result<(), rmcp::ErrorData>>,
    synapse_action::ActionStateSnapshot,
) {
    for attempt in 0..5 {
        let press = tokio::spawn(execute_live_press_sequence(
            handle.clone(),
            vec![key("a")],
            500,
            Backend::Software,
            None,
            crate::m2::OperatorPanicActionBoundary::arm("act_press", "test_live_sequence")
                .expect("operator panic admission should be open for live press test"),
        ));
        if let Some(snapshot) = wait_for_held_key_or_press_done(snapshot_handle, "a", &press).await
        {
            return (press, snapshot);
        }
        press
            .await
            .unwrap_or_else(|error| panic!("interrupted press attempt should join: {error}"))
            .unwrap_or_else(|error| {
                panic!("interrupted press attempt should still release cleanly: {error}")
            });
        println!(
            "readback=act_press_live_sequence edge=mid_hold_release attempt={attempt} observed_external_release_before_held=true"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for held key a without external release interference");
}

fn profile_with_keymap<const N: usize>(entries: [(&str, &str); N]) -> Profile {
    let mut keymap = BTreeMap::new();
    for (alias, binding) in entries {
        keymap.insert(alias.to_owned(), binding.to_owned());
    }
    Profile {
        id: "test.profile".to_owned(),
        label: "Test Profile".to_owned(),
        version: "1.0.0".to_owned(),
        use_scope: ProfileUseScope::OperatorOwnedTest,
        matches: Vec::new(),
        mode: PerceptionMode::Auto,
        capture: ProfileCapture {
            target: ProfileCaptureTarget::ForegroundWindow,
            min_update_interval_ms: 16,
            cursor_visible: true,
        },
        detection: ProfileDetection {
            model_id: None,
            classes_of_interest: Vec::new(),
            confidence_threshold: 0.0,
            max_detections: 0,
        },
        ocr: ProfileOcr {
            default_backend: OcrBackend::Auto,
            regions: Vec::new(),
            parser_config: BTreeMap::new(),
        },
        hud: Vec::new(),
        keymap,
        backends: ProfileBackends {
            default: Backend::Software,
            keyboard_default: Backend::Software,
            mouse_default: Backend::Software,
            pad_default: Backend::Software,
        },
        metadata: BTreeMap::new(),
        event_extensions: Vec::new(),
    }
}

#[test]
fn defaults_are_issue_required_values() {
    assert_eq!(default_hold_ms(), 33);
    assert_eq!(default_press_backend(), PressBackend::Auto);
    let params: ActPressParams = serde_json::from_value(serde_json::json!({
        "keys": ["Enter"]
    }))
    .expect("minimal act_press params should deserialize");
    assert!(!params.auto_wait);
    assert_eq!(
        params.auto_wait_timeout_ms,
        crate::m2::default_auto_wait_timeout_ms()
    );
    assert!(params.auto_wait_element_id.is_none());
}

#[test]
fn normalized_keys_are_modifier_ordered() {
    let before = vec!["super".to_owned(), "s".to_owned(), "ctrl".to_owned()];
    println!("readback=act_press_keys edge=modifier_order before={before:?}");
    let after =
        normalized_keys(&before).unwrap_or_else(|error| panic!("keys should normalize: {error}"));
    let labels = after
        .iter()
        .map(|key| match &key.code {
            synapse_core::KeyCode::Named { value } => value.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>();
    println!("readback=act_press_keys edge=modifier_order after={labels:?}");
    assert_eq!(labels, ["ctrl", "super", "s"]);
}

#[test]
fn normalized_keys_accept_vs_code_terminal_backtick_shortcut() {
    let before = vec!["ctrl".to_owned(), "`".to_owned()];
    println!("readback=act_press_keys edge=backtick_shortcut before={before:?}");
    let after = normalized_keys(&before)
        .unwrap_or_else(|error| panic!("backtick should normalize: {error}"));
    let labels = after
        .iter()
        .map(|key| match &key.code {
            synapse_core::KeyCode::Named { value } => value.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>();
    println!("readback=act_press_keys edge=backtick_shortcut after={labels:?}");
    assert_eq!(labels, ["ctrl", "`"]);
}

#[test]
fn normalized_keys_accept_minecraft_lshift_alias() {
    let before = vec!["lshift".to_owned()];
    println!("readback=act_press_keys edge=lshift_alias before={before:?}");
    let after =
        normalized_keys(&before).unwrap_or_else(|error| panic!("lshift should normalize: {error}"));
    let labels = after
        .iter()
        .map(|key| match &key.code {
            synapse_core::KeyCode::Named { value } => value.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>();
    println!("readback=act_press_keys edge=lshift_alias after={labels:?}");
    assert_eq!(labels, ["shift"]);
}

#[test]
fn cdp_key_strokes_encode_ctrl_a_as_shortcut_without_text_payload() {
    let keys = normalized_keys(&["ctrl".to_owned(), "a".to_owned()])
        .unwrap_or_else(|error| panic!("ctrl+a should normalize: {error}"));
    let strokes = cdp_key_strokes(&keys)
        .unwrap_or_else(|error| panic!("ctrl+a should map to CDP key strokes: {error}"));

    println!("readback=act_press_cdp_mapping edge=ctrl_a strokes={strokes:?}");
    assert_eq!(strokes.len(), 2);
    assert_eq!(strokes[0].key, "Control");
    assert_eq!(strokes[0].modifier_bit, 2);
    assert_eq!(strokes[1].key, "a");
    assert_eq!(strokes[1].code, "KeyA");
    assert_eq!(strokes[1].windows_virtual_key_code, 65);
    assert_eq!(strokes[1].text, None);
}

#[test]
fn event_sequence_reads_recording_events() {
    let before = vec![
        RecordedInput::KeyDown { key: key("ctrl") },
        RecordedInput::DelayMs { ms: 33 },
        RecordedInput::KeyUp { key: key("ctrl") },
    ];
    let after = event_sequence(&before);
    println!("readback=act_press_recording edge=event_sequence before={before:?} after={after}");
    assert_eq!(after, "down:ctrl>delay:33>up:ctrl");
}

async fn wait_for_held_key_or_press_done(
    snapshot_handle: &synapse_action::ActionEmitterSnapshotHandle,
    key_name: &str,
    press: &tokio::task::JoinHandle<Result<(), rmcp::ErrorData>>,
) -> Option<synapse_action::ActionStateSnapshot> {
    for _ in 0..100 {
        let snapshot = snapshot_handle
            .snapshot()
            .await
            .unwrap_or_else(|error| panic!("snapshot should succeed: {error}"));
        if snapshot.held_keys.iter().any(|key| match &key.code {
            synapse_core::KeyCode::Named { value } => value == key_name,
            _ => false,
        }) {
            return Some(snapshot);
        }
        if press.is_finished() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    None
}
