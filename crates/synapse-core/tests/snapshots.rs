use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use synapse_core::{
    AccessibleNode, AudioContext, Backend, DataPredicate, DetectedEntity, ElementId, Event,
    EventFilter, EventRef, EventSource, EventSummary, FocusedElement, ForegroundContext, Health,
    HudReading, HudReadings, HudValue, Observation, ObservationDiagnostics, PerceptionMode, Point,
    Rect, SensorStatus, Size, UiaPattern, element_id, entity_id,
};

#[test]
fn backend_json_shape() {
    insta::assert_json_snapshot!(Backend::Software);
    insta::assert_json_snapshot!(Backend::Vigem);
    insta::assert_json_snapshot!(Backend::Hardware);
    insta::assert_json_snapshot!(Backend::Auto);
}

#[test]
fn perception_mode_json_shape() {
    insta::assert_json_snapshot!(PerceptionMode::A11yOnly);
    insta::assert_json_snapshot!(PerceptionMode::PixelOnly);
    insta::assert_json_snapshot!(PerceptionMode::Hybrid);
    insta::assert_json_snapshot!(PerceptionMode::Auto);
}

#[test]
fn geometry_json_shape() {
    insta::assert_json_snapshot!(Point { x: -10, y: 20 });
    insta::assert_json_snapshot!(Rect {
        x: -10,
        y: 20,
        w: 30,
        h: 40,
    });
    insta::assert_json_snapshot!(Size { w: 1920, h: 1080 });
}

#[test]
fn health_json_shape() {
    insta::assert_json_snapshot!(Health {
        ok: true,
        version: "0.1.0".to_owned(),
        build: "dev".to_owned(),
        pid: 4321,
        uptime_s: 0,
        tool_count: 2,
        tool_surface_sha256: "0123456789abcdef".to_owned(),
        tool_names: vec!["act_run_shell".to_owned(), "health".to_owned()],
        subsystems: BTreeMap::new(),
    });
}

#[test]
fn observation_json_shape() {
    insta::assert_json_snapshot!(sample_observation());
}

#[test]
fn event_json_shape() {
    insta::assert_json_snapshot!(sample_event());
    insta::assert_json_snapshot!(sample_event().summary());
}

#[test]
fn event_filter_json_shape() {
    insta::assert_json_snapshot!(EventFilter::And {
        args: vec![
            EventFilter::Kind {
                kind: "hud-value-changed".to_owned(),
            },
            EventFilter::Data {
                path: "/field".to_owned(),
                predicate: DataPredicate::Eq {
                    value: serde_json::json!("hp"),
                },
            },
            EventFilter::Data {
                path: "/new".to_owned(),
                predicate: DataPredicate::Lt {
                    value: serde_json::json!(20),
                },
            },
        ],
    });
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::UNIX_EPOCH + std::time::Duration::from_hours(494_304))
}

fn sample_event() -> Event {
    Event {
        seq: 10,
        at: fixed_time(),
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
    }
}

#[allow(clippy::too_many_lines)]
fn sample_observation() -> Observation {
    let at = fixed_time();
    let focused_id = element_id(0x12ab, "0a1b2c3d");
    let parent_id = ElementId::parse("0x12ab:ff")
        .unwrap_or_else(|err| panic!("literal element id should parse: {err}"));
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

    Observation {
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
        audio: AudioContext::default(),
        recent_events: vec![EventSummary {
            seq: 10,
            at,
            source: EventSource::PerceptionHud,
            kind: "hud-value-changed".to_owned(),
            data_excerpt: serde_json::json!({"field": "hp", "new": 15}),
        }],
        clipboard_summary: None,
        fs_recent: Vec::new(),
        diagnostics: ObservationDiagnostics {
            assembled_in_ms: 4.5,
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
            size_bytes: 2048,
            size_estimate_tokens: 512,
        },
    }
}
