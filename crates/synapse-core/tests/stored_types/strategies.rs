use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use synapse_core::{
    Action, AudioContext, Backend, ElementId, EventSource, FocusedElement, ForegroundContext,
    HudReadings, Key, KeyCode, ObservationDiagnostics, PerceptionMode, Rect, ReflexState,
    SCHEMA_VERSION, SensorStatus, StoredAppContext, StoredAuditContext, StoredBackendPolicy,
    StoredEvent, StoredObservation, StoredProfileHistoryEntry, StoredRedaction, StoredReflexAudit,
    StoredReflexStep, StoredSession, UiaPattern, element_id,
};

use super::fixtures::fixed_time;

pub fn small_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,8}".prop_map(|value| value)
}

pub fn json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        (0_i64..10_000).prop_map(|value| serde_json::json!(value)),
        small_string().prop_map(serde_json::Value::String),
    ]
}

pub fn event_source_strategy() -> impl Strategy<Value = EventSource> {
    prop_oneof![
        Just(EventSource::System),
        Just(EventSource::ActionEmitter),
        Just(EventSource::PerceptionHud),
        Just(EventSource::Reflex),
    ]
}

pub fn perception_mode_strategy() -> impl Strategy<Value = PerceptionMode> {
    prop_oneof![
        Just(PerceptionMode::A11yOnly),
        Just(PerceptionMode::PixelOnly),
        Just(PerceptionMode::Hybrid),
        Just(PerceptionMode::Auto),
    ]
}

pub fn sensor_status_strategy() -> impl Strategy<Value = SensorStatus> {
    prop_oneof![
        Just(SensorStatus::Healthy),
        Just(SensorStatus::Disabled),
        Just(SensorStatus::Unavailable),
        (0.0_f32..250.0).prop_map(|last_p99_ms| SensorStatus::DegradedLatency { last_p99_ms }),
        small_string().prop_map(|reason_code| SensorStatus::DegradedSensorFailed { reason_code }),
    ]
}

pub fn instant_strategy() -> impl Strategy<Value = DateTime<Utc>> {
    (0_i64..86_400).prop_map(fixed_time)
}

pub fn element_id_strategy() -> impl Strategy<Value = ElementId> {
    (1_i64..10_000, 1_u32..10_000).prop_map(|(hwnd, runtime)| {
        let runtime_id_hex = format!("{runtime:x}");
        element_id(hwnd, &runtime_id_hex)
    })
}

pub fn redactions_strategy() -> impl Strategy<Value = Vec<StoredRedaction>> {
    prop::collection::vec(stored_redaction_strategy(), 0..3)
}

pub fn stored_redaction_strategy() -> impl Strategy<Value = StoredRedaction> {
    (small_string(), 0_u32..256, 0_u32..64).prop_map(|(kind, offset, len)| StoredRedaction {
        kind,
        offset,
        len,
    })
}

pub fn backend_strategy() -> impl Strategy<Value = Backend> {
    prop_oneof![
        Just(Backend::Software),
        Just(Backend::Hardware),
        Just(Backend::Vigem),
        Just(Backend::Auto),
    ]
}

pub fn stored_backend_policy_strategy() -> impl Strategy<Value = StoredBackendPolicy> {
    (
        backend_strategy(),
        backend_strategy(),
        backend_strategy(),
        backend_strategy(),
    )
        .prop_map(|(default, keyboard_default, mouse_default, pad_default)| {
            StoredBackendPolicy {
                default,
                keyboard_default,
                mouse_default,
                pad_default,
            }
        })
}

pub fn stored_app_context_strategy() -> impl Strategy<Value = StoredAppContext> {
    (
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
    )
        .prop_map(
            |(
                process_name,
                process_path,
                window_title,
                target_id,
                gameid,
                world_name,
                world_path,
                log_path,
            )| StoredAppContext {
                process_name,
                process_path,
                window_title,
                target_id,
                gameid,
                world_name,
                world_path,
                log_path,
            },
        )
}

pub fn stored_audit_context_strategy() -> impl Strategy<Value = StoredAuditContext> {
    (
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(0_u32..10),
        prop::option::of(stored_backend_policy_strategy()),
        prop::option::of(stored_app_context_strategy()),
    )
        .prop_map(
            |(
                session_id,
                profile_id,
                profile_version,
                profile_schema_version,
                backend_policy,
                app_context,
            )| StoredAuditContext {
                session_id,
                profile_id,
                profile_version,
                profile_schema_version,
                backend_policy,
                app_context,
            },
        )
}

pub fn rect_strategy() -> impl Strategy<Value = Rect> {
    (
        -10_000_i32..10_000,
        -10_000_i32..10_000,
        1_i32..2_000,
        1_i32..2_000,
    )
        .prop_map(|(x, y, w, h)| Rect { x, y, w, h })
}

pub fn foreground_strategy() -> impl Strategy<Value = ForegroundContext> {
    (
        1_i64..10_000,
        1_u32..50_000,
        small_string(),
        small_string(),
        rect_strategy(),
        0_u32..4,
        1.0_f32..3.0,
        prop::option::of(small_string()),
        any::<bool>(),
    )
        .prop_map(
            |(
                hwnd,
                pid,
                process_name,
                window_title,
                window_bounds,
                monitor_index,
                dpi_scale,
                profile_id,
                is_fullscreen,
            )| ForegroundContext {
                hwnd,
                pid,
                process_path: format!("C:\\Apps\\{process_name}.exe"),
                process_name,
                window_title,
                window_bounds,
                monitor_index,
                dpi_scale,
                profile_id,
                steam_appid: None,
                is_fullscreen,
                is_dwm_composed: true,
            },
        )
}

pub fn focused_element_strategy() -> impl Strategy<Value = FocusedElement> {
    (
        element_id_strategy(),
        small_string(),
        small_string(),
        prop::option::of(small_string()),
        rect_strategy(),
        any::<bool>(),
        prop::option::of(small_string()),
    )
        .prop_map(
            |(element_id, name, role, automation_id, bbox, enabled, value)| FocusedElement {
                element_id,
                name,
                role,
                automation_id,
                bbox,
                enabled,
                patterns: vec![UiaPattern::Value],
                value,
                selected_text: None,
            },
        )
}

pub fn diagnostics_strategy() -> impl Strategy<Value = ObservationDiagnostics> {
    (
        0.0_f32..50.0,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        sensor_status_strategy(),
        sensor_status_strategy(),
        sensor_status_strategy(),
        sensor_status_strategy(),
        0_u32..10_000,
        0_u32..10_000,
    )
        .prop_map(
            |(
                assembled_in_ms,
                a11y_enabled,
                pixel_enabled,
                audio_enabled,
                a11y_status,
                capture_status,
                detection_status,
                audio_status,
                size_bytes,
                size_estimate_tokens,
            )| ObservationDiagnostics {
                assembled_in_ms,
                sensor_latency_ms: BTreeMap::new(),
                a11y_enabled,
                pixel_enabled,
                audio_enabled,
                a11y_status,
                capture_status,
                detection_status,
                audio_status,
                is_minimized: false,
                capture_config: None,
                capture_runtime: None,
                input_backends: None,
                cdp: None,
                web_path: None,
                elements_truncated: false,
                elements_page: None,
                entities_truncated: false,
                size_bytes,
                size_estimate_tokens,
            },
        )
}

pub fn stored_event_strategy() -> impl Strategy<Value = StoredEvent> {
    (
        small_string(),
        0_u64..1_000_000,
        prop::option::of(small_string()),
        prop::option::of(stored_audit_context_strategy()),
        event_source_strategy(),
        small_string(),
        json_value_strategy(),
        prop::option::of(1_i64..10_000),
        prop::option::of(element_id_strategy()),
        any::<bool>(),
        redactions_strategy(),
    )
        .prop_map(
            |(
                event_id,
                ts_ns,
                session_id,
                audit_context,
                source,
                kind,
                data,
                window_id,
                element_id,
                redacted,
                redactions,
            )| StoredEvent {
                schema_version: SCHEMA_VERSION,
                event_id,
                ts_ns,
                session_id,
                audit_context,
                source,
                kind,
                data,
                window_id,
                element_id,
                redacted,
                redactions,
            },
        )
}

pub fn stored_observation_strategy() -> impl Strategy<Value = StoredObservation> {
    (
        small_string(),
        0_u64..1_000_000,
        prop::option::of(small_string()),
        perception_mode_strategy(),
        foreground_strategy(),
        prop::option::of(focused_element_strategy()),
        diagnostics_strategy(),
        small_string(),
        any::<bool>(),
        redactions_strategy(),
    )
        .prop_map(
            |(
                observation_id,
                ts_ns,
                session_id,
                mode,
                foreground,
                focused,
                diagnostics,
                reason,
                redacted,
                redactions,
            )| StoredObservation {
                schema_version: SCHEMA_VERSION,
                observation_id,
                ts_ns,
                session_id,
                mode,
                foreground,
                focused,
                elements: Vec::new(),
                entities: Vec::new(),
                hud: HudReadings::default(),
                audio: AudioContext::default(),
                recent_events: Vec::new(),
                clipboard_summary: None,
                fs_recent: Vec::new(),
                diagnostics,
                reason,
                redacted,
                redactions,
            },
        )
}

pub fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        (small_string(), 0_u32..250).prop_map(|(value, hold_ms)| Action::KeyPress {
            key: Key {
                code: KeyCode::Named { value },
                use_scancode: false,
            },
            hold_ms,
            backend: Backend::Software,
        }),
        (-500.0_f32..500.0, -500.0_f32..500.0).prop_map(|(dx, dy)| Action::MouseMoveRelative {
            dx,
            dy,
            backend: Backend::Software,
        }),
    ]
}

pub fn stored_reflex_step_strategy() -> impl Strategy<Value = StoredReflexStep> {
    (
        0_u32..32,
        action_strategy(),
        small_string(),
        prop::option::of(small_string()),
    )
        .prop_map(|(index, action, status, error_code)| StoredReflexStep {
            index,
            action,
            status,
            error_code,
        })
}

pub fn reflex_state_strategy() -> impl Strategy<Value = ReflexState> {
    prop_oneof![
        Just(ReflexState::Active),
        Just(ReflexState::ActionDenied),
        Just(ReflexState::Paused),
        Just(ReflexState::Cancelled),
        Just(ReflexState::Expired),
        Just(ReflexState::Disabled),
        Just(ReflexState::Starved),
    ]
}

pub fn stored_reflex_audit_strategy() -> impl Strategy<Value = StoredReflexAudit> {
    (
        small_string(),
        small_string(),
        0_u64..1_000_000,
        reflex_state_strategy(),
        prop::option::of(small_string()),
        prop::option::of(stored_audit_context_strategy()),
        prop::collection::vec(stored_reflex_step_strategy(), 0..3),
        prop::option::of(small_string()),
        json_value_strategy(),
        any::<bool>(),
        redactions_strategy(),
    )
        .prop_map(
            |(
                audit_id,
                reflex_id,
                ts_ns,
                status,
                event_id,
                audit_context,
                steps,
                error_code,
                details,
                redacted,
                redactions,
            )| StoredReflexAudit {
                schema_version: SCHEMA_VERSION,
                audit_id,
                reflex_id,
                ts_ns,
                status,
                event_id,
                audit_context,
                steps,
                error_code,
                details,
                redacted,
                redactions,
            },
        )
}

pub fn stored_profile_history_entry_strategy() -> impl Strategy<Value = StoredProfileHistoryEntry> {
    (
        small_string(),
        prop::option::of(small_string()),
        prop::option::of(0_u32..10),
        instant_strategy(),
        small_string(),
    )
        .prop_map(
            |(profile_id, profile_version, profile_schema_version, activated_at, reason)| {
                StoredProfileHistoryEntry {
                    profile_id,
                    profile_version,
                    profile_schema_version,
                    activated_at,
                    reason,
                }
            },
        )
}

pub fn stored_session_strategy() -> impl Strategy<Value = StoredSession> {
    (
        small_string(),
        instant_strategy(),
        prop::option::of(instant_strategy()),
        small_string(),
        prop::option::of(small_string()),
        perception_mode_strategy(),
        prop::option::of(small_string()),
        prop::option::of(stored_audit_context_strategy()),
        prop::collection::vec(stored_profile_history_entry_strategy(), 0..3),
        any::<bool>(),
        redactions_strategy(),
    )
        .prop_map(
            |(
                session_id,
                started_at,
                ended_at,
                transport,
                client,
                mode,
                active_profile,
                audit_context,
                profile_history,
                redacted,
                redactions,
            )| StoredSession {
                schema_version: SCHEMA_VERSION,
                session_id,
                started_at,
                ended_at,
                transport,
                client,
                mode,
                active_profile,
                audit_context,
                profile_history,
                redacted,
                redactions,
            },
        )
}
