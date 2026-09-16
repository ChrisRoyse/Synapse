#![allow(
    clippy::expect_used,
    clippy::float_cmp,
    reason = "unit tests intentionally assert exact schema-mapping values and failure paths"
)]

use serde_json::json;
use synapse_core::{
    Action, Backend, ComboInput, DataPredicate, EventFilter, EventSource, KeyCode,
    KeystrokeDynamics, KeystrokeNaturalParams, MouseButton, ReflexAimAxis, ReflexLifetime,
    StrokeTiming, VelocityProfile,
};
use synapse_reflex::{AimTrackTarget, ScheduledReflexDriver, SchedulerTrigger};

use super::{ReflexRegisterParams, register::scheduled_reflex_from_params};
use crate::m3::{permissions::Permission, reflex::required_permissions_register};

#[test]
fn demo_gate_shape_maps_to_event_filter_and_actions() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": {
            "kind": "element-appeared",
            "match": { "window_title_regex": "^Save As$" }
        },
        "then": {
            "steps": [
                {
                    "action": "act_type",
                    "params": {
                        "text": "m3-demo.txt",
                        "dynamics": "linear",
                        "linear_ms_per_char": 20
                    }
                },
                {
                    "action": "act_press",
                    "params": { "keys": ["enter"] }
                }
            ]
        },
        "lifetime": { "kind": "one_shot" }
    }))
    .expect("demo shape should deserialize");

    let reflex = scheduled_reflex_from_params(params).expect("demo shape should build a reflex");

    let SchedulerTrigger::OnEvent(EventFilter::And { args }) = reflex.trigger else {
        panic!("demo when should map to a compound on_event filter");
    };
    assert!(args.contains(&EventFilter::Kind {
        kind: "element-appeared".to_owned()
    }));
    assert!(args.contains(&EventFilter::Data {
        path: "/window_title".to_owned(),
        predicate: DataPredicate::Regex {
            pattern: "^Save As$".to_owned()
        }
    }));
    assert_eq!(reflex.lifetime, ReflexLifetime::OneShot);
    assert_eq!(reflex.then.len(), 2);
    match &reflex.then[0] {
        Action::TypeText { text, dynamics, .. } => {
            assert_eq!(text, "m3-demo.txt");
            assert_eq!(dynamics, &KeystrokeDynamics::Linear { ms_per_char: 20 });
        }
        other => panic!("first demo step should map to TypeText, got {other:?}"),
    }
    match &reflex.then[1] {
        Action::KeyPress { key, hold_ms, .. } => {
            assert_eq!(*hold_ms, 33);
            assert_eq!(
                &key.code,
                &KeyCode::Named {
                    value: "enter".to_owned()
                }
            );
        }
        other => panic!("second demo step should map to KeyPress, got {other:?}"),
    }
}

#[test]
fn demo_step_act_type_omitted_dynamics_resolves_to_natural_fast() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": { "op": "kind", "kind": "support-default-resolution" },
        "then": {
            "steps": [
                {
                    "action": "act_type",
                    "params": { "text": "abc" }
                }
            ]
        }
    }))
    .expect("default dynamics reflex shape should deserialize");

    let reflex =
        scheduled_reflex_from_params(params).expect("default dynamics reflex should build");

    assert_eq!(reflex.then.len(), 1);
    match &reflex.then[0] {
        Action::TypeText {
            text,
            dynamics,
            backend,
        } => {
            assert_eq!(text, "abc");
            assert_eq!(
                dynamics,
                &KeystrokeDynamics::Natural {
                    params: KeystrokeNaturalParams::FAST
                }
            );
            assert_eq!(*backend, Backend::Auto);
        }
        other => panic!("default act_type step should map to TypeText, got {other:?}"),
    }
}

#[test]
fn on_event_debounce_ms_maps_to_scheduler_debounce() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": { "op": "kind", "kind": "value-changed" },
        "then": { "kind": "action", "action": { "kind": "release_all" } },
        "debounce_ms": 250
    }))
    .expect("debounced reflex shape should deserialize");

    let reflex =
        scheduled_reflex_from_params(params).expect("debounced on_event should build a reflex");

    assert_eq!(reflex.debounce, std::time::Duration::from_millis(250));
}

#[test]
fn file_jsonl_tail_shape_maps_to_filesystem_event_filter_and_audit_then() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": {
            "kind": "file_jsonl_tail",
            "host": "aiwonder",
            "path": "/zfs/hot/calyx/metrics/anneal_j_series.jsonl",
            "predicate": {"json_path": "$.soak_status", "equals": "complete"},
            "min_lines": 100
        },
        "then": {"action": "audit/readback"}
    }))
    .expect("file_jsonl_tail issue shape should deserialize");

    let reflex =
        scheduled_reflex_from_params(params).expect("file_jsonl_tail issue shape should build");

    let SchedulerTrigger::OnEvent(EventFilter::And { args }) = reflex.trigger else {
        panic!("file_jsonl_tail should map to a compound on_event filter");
    };
    assert!(args.contains(&EventFilter::Source {
        source: EventSource::Filesystem
    }));
    assert!(args.contains(&EventFilter::Kind {
        kind: "file_jsonl_tail".to_owned()
    }));
    assert!(args.contains(&EventFilter::Data {
        path: "/host".to_owned(),
        predicate: DataPredicate::Eq {
            value: json!("aiwonder")
        }
    }));
    assert!(args.contains(&EventFilter::Data {
        path: "/path".to_owned(),
        predicate: DataPredicate::Eq {
            value: json!("/zfs/hot/calyx/metrics/anneal_j_series.jsonl")
        }
    }));
    assert!(args.contains(&EventFilter::Data {
        path: "/line_count".to_owned(),
        predicate: DataPredicate::Ge { value: json!(100) }
    }));
    assert!(args.contains(&EventFilter::Data {
        path: "/last_json/soak_status".to_owned(),
        predicate: DataPredicate::Eq {
            value: json!("complete")
        }
    }));
    assert!(reflex.then.is_empty());
}

#[test]
fn file_jsonl_tail_rejects_invalid_json_path() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": {
            "kind": "file_jsonl_tail",
            "host": "aiwonder",
            "path": "/zfs/hot/calyx/metrics/anneal_j_series.jsonl",
            "predicate": {"json_path": "$[*]", "equals": "complete"},
            "min_lines": 100
        },
        "then": {"action": "audit/readback"}
    }))
    .expect("invalid json_path shape should deserialize before validation");

    let error = scheduled_reflex_from_params(params)
        .expect_err("invalid file_jsonl_tail json_path should fail closed");
    assert!(error.to_string().contains("predicate.json_path"), "{error}");
}

#[test]
fn file_jsonl_tail_rejects_remote_relative_path() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": {
            "kind": "file_jsonl_tail",
            "host": "aiwonder",
            "path": "metrics/anneal_j_series.jsonl",
            "predicate": {"json_path": "$.soak_status", "equals": "complete"},
            "min_lines": 100
        },
        "then": {"action": "audit/readback"}
    }))
    .expect("relative path shape should deserialize before validation");

    let error = scheduled_reflex_from_params(params)
        .expect_err("remote relative file_jsonl_tail path should fail closed");
    assert!(error.to_string().contains("remote path"), "{error}");
}

#[test]
fn audit_readback_then_rejects_unknown_action() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": { "op": "kind", "kind": "support-invalid-audit-action" },
        "then": {"action": "audit/not-real"}
    }))
    .expect("unknown audit action shape should deserialize before validation");

    let error =
        scheduled_reflex_from_params(params).expect_err("unknown audit action should fail closed");
    assert!(error.to_string().contains("unsupported"), "{error}");
}

#[test]
fn aim_track_shape_maps_to_stateful_driver() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "aim_track",
        "target": { "kind": "screen", "point": { "x": 100, "y": 200 } },
        "axis": "x_only",
        "gain": 0.5,
        "deadzone_px": 2.0,
        "max_speed_px_per_tick": 40.0,
        "backend": "software"
    }))
    .expect("aim_track reflex shape should deserialize");

    let reflex = scheduled_reflex_from_params(params).expect("aim_track should build a reflex");

    let ScheduledReflexDriver::AimTrack(params) = reflex.driver else {
        panic!("aim_track should map to stateful driver");
    };
    assert_eq!(params.target, AimTrackTarget::Point(point(100, 200)));
    assert_eq!(params.axis, ReflexAimAxis::XOnly);
    assert_eq!(params.gain, 0.5);
    assert_eq!(params.deadzone_px, 2.0);
    assert_eq!(params.max_speed_px_per_tick, 40.0);
    assert_eq!(params.backend, Backend::Software);
}

#[test]
fn hold_move_shape_maps_key_string_and_duration() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "hold_move",
        "key": "w",
        "lifetime": { "kind": "duration", "ms": 1500 },
        "backend": "software"
    }))
    .expect("hold_move reflex shape should deserialize");

    let reflex = scheduled_reflex_from_params(params).expect("hold_move should build a reflex");

    let ScheduledReflexDriver::HoldMove(params) = reflex.driver else {
        panic!("hold_move should map to stateful driver");
    };
    assert_eq!(reflex.lifetime, ReflexLifetime::Duration { ms: 1500 });
    assert_eq!(params.keys.len(), 1);
    assert_eq!(
        params.keys[0].code,
        KeyCode::Named {
            value: "w".to_owned()
        }
    );
    assert_eq!(params.backend, Backend::Software);
}

#[test]
fn combo_timed_act_press_steps_map_to_one_shot_combo_driver() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "combo",
        "steps": [
            { "at_ms": 0, "action": "act_press", "params": { "keys": ["e"] } },
            { "at_ms": 200, "action": "act_press", "params": { "keys": ["space"] } }
        ],
        "backend": "software"
    }))
    .expect("combo reflex shape should deserialize");

    let reflex = scheduled_reflex_from_params(params).expect("combo should build a reflex");

    let ScheduledReflexDriver::Combo(combo) = reflex.driver else {
        panic!("combo should map to stateful combo driver");
    };
    assert_eq!(reflex.lifetime, ReflexLifetime::OneShot);
    assert!(reflex.then.is_empty());
    assert_eq!(combo.backend, Backend::Software);
    let steps = combo.steps;
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].at_ms, 0);
    assert_eq!(steps[1].at_ms, 200);
    assert!(matches!(
        steps[0].input,
        ComboInput::KeyPress { ref key, hold_ms: 33 }
            if key.code == KeyCode::Named { value: "e".to_owned() }
    ));
    assert!(matches!(
        steps[1].input,
        ComboInput::KeyPress { ref key, hold_ms: 33 }
            if key.code == KeyCode::Named { value: "space".to_owned() }
    ));
}

#[test]
fn path_follow_shape_maps_to_one_shot_stateful_driver() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "path_follow",
        "path": {
            "kind": "circle",
            "center": { "x": 100.0, "y": 120.0 },
            "radius": 10.0
        },
        "button": "left",
        "velocity_profile": "minimum_jerk",
        "duration_or_speed": { "kind": "duration_ms", "duration_ms": 12 },
        "backend": "software"
    }))
    .expect("path_follow reflex shape should deserialize");

    let reflex = scheduled_reflex_from_params(params).expect("path_follow should build a reflex");

    let ScheduledReflexDriver::PathFollow(path_follow) = reflex.driver else {
        panic!("path_follow should map to stateful path_follow driver");
    };
    assert_eq!(reflex.lifetime, ReflexLifetime::OneShot);
    assert!(reflex.then.is_empty());
    assert_eq!(path_follow.button, Some(MouseButton::Left));
    assert_eq!(path_follow.profile, VelocityProfile::MinimumJerk);
    assert_eq!(
        path_follow.timing,
        StrokeTiming::DurationMs { duration_ms: 12 }
    );
    assert_eq!(path_follow.backend, Backend::Software);
}

#[test]
fn register_permissions_do_not_add_removed_hardware_backend_gate() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": { "op": "kind", "kind": "support-permissions" },
        "then": {
            "steps": [
                {
                    "action": "act_type",
                    "params": { "text": "abc" }
                }
            ]
        },
        "backend": "hardware"
    }))
    .expect("permission reflex shape should deserialize");

    let permissions =
        required_permissions_register(&params).expect("permission calculation should pass");
    assert!(permissions.contains(&Permission::WriteReflex));
    assert!(permissions.contains(&Permission::InputKeyboard));
    assert!(!permissions.contains(&Permission::InputMouse));
    assert!(!permissions.contains(&Permission::InputPad));
}

#[test]
fn demo_gate_shape_rejects_invalid_regex() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": {
            "kind": "element-appeared",
            "match": { "window_title_regex": "[" }
        },
        "then": { "steps": [{ "action": "act_press", "params": { "keys": ["enter"] } }] }
    }))
    .expect("invalid regex still deserializes before validation");

    let error = scheduled_reflex_from_params(params)
        .expect_err("invalid window_title_regex should fail closed");
    assert!(
        error.to_string().contains("window_title_regex is invalid"),
        "{error}"
    );
}

#[test]
fn demo_gate_shape_rejects_unknown_action() {
    let params: ReflexRegisterParams = serde_json::from_value(json!({
        "kind": "on_event",
        "when": { "kind": "element-appeared" },
        "then": { "steps": [{ "action": "act_launch", "params": {} }] }
    }))
    .expect("unknown action still deserializes before validation");

    let error = scheduled_reflex_from_params(params)
        .expect_err("unsupported reflex step should fail closed");
    assert!(error.to_string().contains("unsupported"), "{error}");
}

const fn point(x: i32, y: i32) -> synapse_core::Point {
    synapse_core::Point { x, y }
}
