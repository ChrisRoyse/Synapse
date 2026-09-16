use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use synapse_core::{
    AccessibleNode, Action, AudioContext, AudioEvent, Backend, ButtonAction, ClipboardSummary,
    DetectedEntity, DirectionEstimate, EventSource, EventSummary, FocusedElement,
    ForegroundContext, FsEvent, FsEventKind, HudReading, HudReadings, HudValue, Key, KeyCode,
    MouseButton, ObservationDiagnostics, PerceptionMode, Rect, ReflexState, SCHEMA_VERSION,
    SensorStatus, StoredAppContext, StoredAuditContext, StoredBackendPolicy, StoredEvent,
    StoredObservation, StoredProfileHistoryEntry, StoredRedaction, StoredReflexAudit,
    StoredReflexStep, StoredSession, UiaPattern, element_id, entity_id,
};

pub fn empty_event() -> StoredEvent {
    StoredEvent {
        schema_version: SCHEMA_VERSION,
        event_id: String::new(),
        ts_ns: 0,
        session_id: None,
        audit_context: None,
        source: EventSource::System,
        kind: String::new(),
        data: serde_json::Value::Null,
        window_id: None,
        element_id: None,
        redacted: false,
        redactions: Vec::new(),
    }
}

pub fn required_event() -> StoredEvent {
    StoredEvent {
        event_id: "event-1".to_owned(),
        ts_ns: 1,
        source: EventSource::ActionEmitter,
        kind: "action_completed".to_owned(),
        ..empty_event()
    }
}

pub fn full_event() -> StoredEvent {
    StoredEvent {
        schema_version: SCHEMA_VERSION,
        event_id: "event-full".to_owned(),
        ts_ns: 42,
        session_id: Some("session-full".to_owned()),
        audit_context: Some(full_audit_context()),
        source: EventSource::PerceptionHud,
        kind: "hud_value_changed".to_owned(),
        data: serde_json::json!({"field":"hp","new":20}),
        window_id: Some(0x1234),
        element_id: Some(element_id(0x1234, "0102")),
        redacted: true,
        redactions: vec![full_redaction()],
    }
}

pub fn empty_observation() -> StoredObservation {
    StoredObservation {
        schema_version: SCHEMA_VERSION,
        observation_id: String::new(),
        ts_ns: 0,
        session_id: None,
        mode: PerceptionMode::Auto,
        foreground: foreground(""),
        focused: None,
        elements: Vec::new(),
        entities: Vec::new(),
        hud: HudReadings::default(),
        audio: AudioContext::default(),
        recent_events: Vec::new(),
        clipboard_summary: None,
        fs_recent: Vec::new(),
        diagnostics: diagnostics(),
        reason: String::new(),
        redacted: false,
        redactions: Vec::new(),
    }
}

pub fn required_observation() -> StoredObservation {
    StoredObservation {
        observation_id: "obs-1".to_owned(),
        ts_ns: 1,
        reason: "1hz_sample".to_owned(),
        ..empty_observation()
    }
}

pub fn full_observation() -> StoredObservation {
    let at = fixed_time(10);
    let mut hud = HudReadings::default();
    hud.by_name.insert(
        "hp".to_owned(),
        HudReading {
            raw_text: "20".to_owned(),
            parsed: HudValue::Number(20.0),
            confidence: 0.97,
            stale_ms: 0,
        },
    );

    StoredObservation {
        schema_version: SCHEMA_VERSION,
        observation_id: "obs-full".to_owned(),
        ts_ns: 10,
        session_id: Some("session-full".to_owned()),
        mode: PerceptionMode::Hybrid,
        foreground: foreground("notepad.exe"),
        focused: Some(focused_element()),
        elements: vec![accessible_node()],
        entities: vec![DetectedEntity {
            entity_id: entity_id(7),
            track_id: 7,
            class_label: "cursor".to_owned(),
            bbox: Rect {
                x: 120,
                y: 130,
                w: 16,
                h: 24,
            },
            confidence: 0.75,
            first_seen_at: at,
            last_seen_at: at,
            velocity_px_per_s: Some((0.0, 0.0)),
        }],
        hud,
        audio: AudioContext {
            rms_db: -42.0,
            vad_speech_recent: false,
            recent_events: vec![AudioEvent {
                at,
                kind: "loud_transient".to_owned(),
                azimuth_deg: Some(15.0),
                confidence: 0.8,
            }],
            direction_estimate: Some(DirectionEstimate {
                azimuth_deg: 15.0,
                confidence: 0.8,
            }),
        },
        recent_events: vec![EventSummary {
            seq: 1,
            at,
            source: EventSource::PerceptionHud,
            kind: "hud_value_changed".to_owned(),
            data_excerpt: serde_json::json!({"field":"hp"}),
        }],
        clipboard_summary: Some(ClipboardSummary {
            formats: vec!["text/plain".to_owned()],
            text_len: Some(5),
            text_excerpt: Some("hello".to_owned()),
            redacted: false,
        }),
        fs_recent: vec![FsEvent {
            at,
            path: "C:\\Users\\owner\\note.txt".to_owned(),
            kind: FsEventKind::Modified,
            size_bytes: Some(5),
        }],
        diagnostics: diagnostics(),
        reason: "before_action".to_owned(),
        redacted: true,
        redactions: vec![full_redaction()],
    }
}

pub fn empty_reflex_audit() -> StoredReflexAudit {
    StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: String::new(),
        reflex_id: String::new(),
        ts_ns: 0,
        status: ReflexState::Active,
        event_id: None,
        audit_context: None,
        steps: Vec::new(),
        error_code: None,
        details: serde_json::Value::Null,
        redacted: false,
        redactions: Vec::new(),
    }
}

pub fn required_reflex_audit() -> StoredReflexAudit {
    StoredReflexAudit {
        audit_id: "audit-1".to_owned(),
        reflex_id: "reflex-1".to_owned(),
        ts_ns: 1,
        ..empty_reflex_audit()
    }
}

pub fn full_reflex_audit() -> StoredReflexAudit {
    StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: "audit-full".to_owned(),
        reflex_id: "reflex-full".to_owned(),
        ts_ns: 42,
        status: ReflexState::Starved,
        event_id: Some("event-full".to_owned()),
        audit_context: Some(full_audit_context()),
        steps: vec![full_reflex_step()],
        error_code: Some("REFLEX_STARVED".to_owned()),
        details: serde_json::json!({"lost_for_ms": 2000}),
        redacted: true,
        redactions: vec![full_redaction()],
    }
}

pub fn empty_session() -> StoredSession {
    StoredSession {
        schema_version: SCHEMA_VERSION,
        session_id: String::new(),
        started_at: fixed_time(0),
        ended_at: None,
        transport: String::new(),
        client: None,
        mode: PerceptionMode::Auto,
        active_profile: None,
        audit_context: None,
        profile_history: Vec::new(),
        redacted: false,
        redactions: Vec::new(),
    }
}

pub fn required_session() -> StoredSession {
    StoredSession {
        session_id: "session-1".to_owned(),
        transport: "stdio".to_owned(),
        ..empty_session()
    }
}

pub fn full_session() -> StoredSession {
    StoredSession {
        schema_version: SCHEMA_VERSION,
        session_id: "session-full".to_owned(),
        started_at: fixed_time(10),
        ended_at: Some(fixed_time(20)),
        transport: "http".to_owned(),
        client: Some("claude-desktop/0.4.2".to_owned()),
        mode: PerceptionMode::Hybrid,
        active_profile: Some("notepad".to_owned()),
        audit_context: Some(full_audit_context()),
        profile_history: vec![full_profile_history_entry()],
        redacted: true,
        redactions: vec![full_redaction()],
    }
}

pub fn empty_redaction() -> StoredRedaction {
    StoredRedaction {
        kind: String::new(),
        offset: 0,
        len: 0,
    }
}

pub fn required_redaction() -> StoredRedaction {
    StoredRedaction {
        kind: "email".to_owned(),
        offset: 0,
        len: 5,
    }
}

pub fn full_redaction() -> StoredRedaction {
    StoredRedaction {
        kind: "secret".to_owned(),
        offset: 12,
        len: 8,
    }
}

pub fn empty_reflex_step() -> StoredReflexStep {
    StoredReflexStep {
        index: 0,
        action: key_press_action("space"),
        status: String::new(),
        error_code: None,
    }
}

pub fn required_reflex_step() -> StoredReflexStep {
    StoredReflexStep {
        status: "queued".to_owned(),
        ..empty_reflex_step()
    }
}

pub fn full_reflex_step() -> StoredReflexStep {
    StoredReflexStep {
        index: 1,
        action: Action::MouseButton {
            button: MouseButton::Left,
            action: ButtonAction::Press,
            hold_ms: 16,
            backend: Backend::Software,
        },
        status: "completed".to_owned(),
        error_code: None,
    }
}

pub fn empty_profile_history_entry() -> StoredProfileHistoryEntry {
    StoredProfileHistoryEntry {
        profile_id: String::new(),
        profile_version: None,
        profile_schema_version: None,
        activated_at: fixed_time(0),
        reason: String::new(),
    }
}

pub fn required_profile_history_entry() -> StoredProfileHistoryEntry {
    StoredProfileHistoryEntry {
        profile_id: "notepad".to_owned(),
        profile_version: Some("1.0.0".to_owned()),
        profile_schema_version: Some(SCHEMA_VERSION),
        activated_at: fixed_time(1),
        reason: "manual".to_owned(),
    }
}

pub fn full_profile_history_entry() -> StoredProfileHistoryEntry {
    StoredProfileHistoryEntry {
        profile_id: "vscode".to_owned(),
        profile_version: Some("2.1.0".to_owned()),
        profile_schema_version: Some(SCHEMA_VERSION),
        activated_at: fixed_time(2),
        reason: "window_match".to_owned(),
    }
}

pub fn full_audit_context() -> StoredAuditContext {
    StoredAuditContext {
        session_id: Some("session-full".to_owned()),
        profile_id: Some("notepad".to_owned()),
        profile_version: Some("1.0.0".to_owned()),
        profile_schema_version: Some(SCHEMA_VERSION),
        backend_policy: Some(StoredBackendPolicy {
            default: Backend::Software,
            keyboard_default: Backend::Software,
            mouse_default: Backend::Software,
            pad_default: Backend::Vigem,
        }),
        app_context: Some(StoredAppContext {
            process_name: Some("notepad.exe".to_owned()),
            process_path: Some("C:\\Windows\\System32\\notepad.exe".to_owned()),
            window_title: Some("Untitled - Notepad".to_owned()),
            target_id: Some("windows.notepad".to_owned()),
            gameid: None,
            world_name: None,
            world_path: None,
            log_path: None,
        }),
    }
}

pub fn foreground(process_name: &str) -> ForegroundContext {
    ForegroundContext {
        hwnd: 0x1234,
        pid: 42,
        process_name: process_name.to_owned(),
        process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
        window_title: "Untitled - Notepad".to_owned(),
        window_bounds: Rect {
            x: 10,
            y: 20,
            w: 800,
            h: 600,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: Some("notepad".to_owned()),
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    }
}

pub fn focused_element() -> FocusedElement {
    FocusedElement {
        element_id: element_id(0x1234, "0102"),
        name: "Editor".to_owned(),
        role: "document".to_owned(),
        automation_id: Some("15".to_owned()),
        bbox: Rect {
            x: 20,
            y: 30,
            w: 700,
            h: 500,
        },
        enabled: true,
        patterns: vec![UiaPattern::Value, UiaPattern::Text],
        value: Some("hello".to_owned()),
        selected_text: None,
    }
}

pub fn accessible_node() -> AccessibleNode {
    AccessibleNode {
        element_id: element_id(0x1234, "0102"),
        parent: None,
        name: "Editor".to_owned(),
        role: "document".to_owned(),
        automation_id: Some("15".to_owned()),
        value: None,
        bbox: Rect {
            x: 20,
            y: 30,
            w: 700,
            h: 500,
        },
        enabled: true,
        focused: true,
        patterns: vec![UiaPattern::Value],
        children_count: 0,
        depth: 0,
    }
}

pub fn diagnostics() -> ObservationDiagnostics {
    ObservationDiagnostics {
        assembled_in_ms: 1.5,
        sensor_latency_ms: BTreeMap::new(),
        a11y_enabled: true,
        pixel_enabled: true,
        audio_enabled: false,
        a11y_status: SensorStatus::Healthy,
        capture_status: SensorStatus::Healthy,
        detection_status: SensorStatus::Unavailable,
        audio_status: SensorStatus::Disabled,
        is_minimized: false,
        capture_config: None,
        capture_runtime: None,
        input_backends: None,
        cdp: None,
        web_path: None,
        elements_truncated: false,
        elements_page: None,
        entities_truncated: false,
        size_bytes: 256,
        size_estimate_tokens: 64,
    }
}

pub fn key_press_action(key: &str) -> Action {
    Action::KeyPress {
        key: Key {
            code: KeyCode::Named {
                value: key.to_owned(),
            },
            use_scancode: false,
        },
        hold_ms: 30,
        backend: Backend::Software,
    }
}

pub fn fixed_time(offset_seconds: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::SystemTime::UNIX_EPOCH) + Duration::seconds(offset_seconds)
}
