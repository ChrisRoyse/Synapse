use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use synapse_core::{
    AccessibleNode, AudioContext, ClipboardSummary, DetectedEntity, Event, EventRef, EventSource,
    FocusedElement, ForegroundContext, FsEvent, FsEventKind, HudReading, HudReadings, HudValue,
    Observation, ObservationDiagnostics, PerceptionMode, Rect, SensorStatus, UiaPattern,
    element_id, entity_id,
};

pub fn fixed_time() -> Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339("2026-05-23T00:00:00Z").map(|value| value.with_timezone(&Utc))
}

pub fn sample_event() -> Result<Event, chrono::ParseError> {
    Ok(Event {
        seq: 10,
        at: fixed_time()?,
        source: EventSource::PerceptionHud,
        kind: "hud-value-changed".to_owned(),
        data: serde_json::json!({
            "field": "hp",
            "old": 25,
            "new": 15,
            "confidence": 0.98
        }),
        correlations: vec![EventRef {
            seq: 9,
            relation: "supersedes".to_owned(),
        }],
    })
}

#[allow(clippy::too_many_lines)]
pub fn sample_observation() -> Result<Observation, chrono::ParseError> {
    let at = fixed_time()?;
    let focused_id = element_id(0x12ab, "0a1b2c3d");
    let parent_id = element_id(0x12ab, "ff");
    let event = sample_event()?;
    let mut hud = HudReadings::default();
    hud.by_name.insert(
        "hp".to_owned(),
        HudReading {
            raw_text: "15".to_owned(),
            parsed: HudValue::Number(15.0),
            confidence: 0.98,
            stale_ms: 16,
        },
    );

    Ok(Observation {
        seq: 123,
        at,
        mode: PerceptionMode::Hybrid,
        foreground: ForegroundContext {
            hwnd: 0x12ab,
            pid: 4242,
            process_name: "notepad.exe".to_owned(),
            process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
            window_title: "Untitled - Notepad".to_owned(),
            window_bounds: Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
            },
            monitor_index: 0,
            dpi_scale: 1.25,
            profile_id: Some("windows.notepad".to_owned()),
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        },
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
        focused: Some(FocusedElement {
            element_id: focused_id.clone(),
            name: "Text Editor".to_owned(),
            role: "Edit".to_owned(),
            automation_id: Some("15".to_owned()),
            bbox: Rect {
                x: 10,
                y: 20,
                w: 600,
                h: 400,
            },
            enabled: true,
            patterns: vec![UiaPattern::Value, UiaPattern::Text],
            value: Some("hello".to_owned()),
            selected_text: None,
        }),
        elements: vec![AccessibleNode {
            element_id: focused_id,
            parent: Some(parent_id),
            name: "Text Editor".to_owned(),
            role: "Edit".to_owned(),
            automation_id: Some("15".to_owned()),
            value: None,
            bbox: Rect {
                x: 10,
                y: 20,
                w: 600,
                h: 400,
            },
            enabled: true,
            focused: true,
            patterns: vec![UiaPattern::Value, UiaPattern::Text],
            children_count: 0,
            depth: 1,
        }],
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
            rms_db: -45.0,
            vad_speech_recent: false,
            recent_events: Vec::new(),
            direction_estimate: None,
        },
        recent_events: vec![event.summary()],
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
        diagnostics: ObservationDiagnostics {
            assembled_in_ms: 4.5,
            sensor_latency_ms: BTreeMap::new(),
            a11y_enabled: true,
            pixel_enabled: true,
            audio_enabled: false,
            a11y_status: SensorStatus::Healthy,
            capture_status: SensorStatus::Healthy,
            detection_status: SensorStatus::DegradedSensorFailed {
                reason_code: "DETECTION_MODEL_NOT_LOADED".to_owned(),
            },
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
            size_bytes: 2048,
            size_estimate_tokens: 512,
        },
    })
}

#[allow(clippy::too_many_lines)]
pub fn observation_strategy() -> impl Strategy<Value = Observation> {
    (
        1_u64..1_000_000,
        "[a-z]{1,8}",
        "[a-z]{1,12}",
        0_u32..20_000,
        1_u64..10_000,
        0.0_f32..1.0,
    )
        .prop_map(|(seq, process, role, pid, track_id, confidence)| {
            let seconds = 1_700_000_000 + (seq % 86_400);
            let at = DateTime::<Utc>::from(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
            );
            let element = element_id(i64::from(pid), "aa");
            Observation {
                seq,
                at,
                mode: PerceptionMode::A11yOnly,
                foreground: ForegroundContext {
                    hwnd: i64::from(pid),
                    pid,
                    process_name: format!("{process}.exe"),
                    process_path: format!("C:\\Apps\\{process}.exe"),
                    window_title: process,
                    window_bounds: Rect {
                        x: 0,
                        y: 0,
                        w: 640,
                        h: 480,
                    },
                    monitor_index: 0,
                    dpi_scale: 1.0,
                    profile_id: None,
                    steam_appid: None,
                    is_fullscreen: false,
                    is_dwm_composed: true,
                },
                perceived_text_notice: None,
                suspected_injection: Vec::new(),
                focused: Some(FocusedElement {
                    element_id: element.clone(),
                    name: "Focused".to_owned(),
                    role: role.clone(),
                    automation_id: None,
                    bbox: Rect {
                        x: 1,
                        y: 2,
                        w: 3,
                        h: 4,
                    },
                    enabled: true,
                    patterns: vec![UiaPattern::Invoke],
                    value: None,
                    selected_text: None,
                }),
                elements: vec![AccessibleNode {
                    element_id: element,
                    parent: None,
                    name: "Focused".to_owned(),
                    role,
                    automation_id: None,
                    value: None,
                    bbox: Rect {
                        x: 1,
                        y: 2,
                        w: 3,
                        h: 4,
                    },
                    enabled: true,
                    focused: true,
                    patterns: vec![UiaPattern::Invoke],
                    children_count: 0,
                    depth: 0,
                }],
                entities: vec![DetectedEntity {
                    entity_id: entity_id(track_id),
                    track_id,
                    class_label: "target".to_owned(),
                    bbox: Rect {
                        x: 5,
                        y: 6,
                        w: 7,
                        h: 8,
                    },
                    confidence,
                    first_seen_at: at,
                    last_seen_at: at,
                    velocity_px_per_s: None,
                }],
                hud: HudReadings::default(),
                audio: AudioContext::default(),
                recent_events: Vec::new(),
                clipboard_summary: None,
                fs_recent: Vec::new(),
                diagnostics: ObservationDiagnostics {
                    assembled_in_ms: 1.0,
                    sensor_latency_ms: BTreeMap::new(),
                    a11y_enabled: true,
                    pixel_enabled: false,
                    audio_enabled: false,
                    a11y_status: SensorStatus::Healthy,
                    capture_status: SensorStatus::Disabled,
                    detection_status: SensorStatus::Disabled,
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
                    size_bytes: 512,
                    size_estimate_tokens: 128,
                },
            }
        })
}
