use std::{collections::BTreeMap, hint::black_box};

use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use synapse_core::{
    AccessibleNode, AudioContext, DetectedEntity, FocusedElement, ForegroundContext, HudReadings,
    PerceptionMode, Rect, SensorStatus, UiaPattern, element_id, entity_id,
};
use synapse_perception::{ObservationAssembler, ObservationInput, ObserveInclude};

fn bench_observe_warm_hybrid(c: &mut Criterion) {
    let assembler = ObservationAssembler::new();
    let include = ObserveInclude::default();
    let input = synthetic_notepad_input(Some(PerceptionMode::Hybrid));
    c.bench_function("observe_warm_hybrid", |bench| {
        bench.iter(|| {
            let _ = black_box(assembler.assemble(black_box(include), black_box(input.clone())));
        });
    });
}

fn synthetic_notepad_input(mode_override: Option<PerceptionMode>) -> ObservationInput {
    let at = Utc::now();
    let mut latency = BTreeMap::new();
    latency.insert("a11y".to_owned(), 1.25);
    latency.insert("capture".to_owned(), 0.50);
    ObservationInput {
        foreground: ForegroundContext {
            hwnd: 0x1234,
            pid: 44,
            process_name: "notepad.exe".to_owned(),
            process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
            window_title: "manual-benchmark.txt - Notepad".to_owned(),
            window_bounds: Rect {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            },
            monitor_index: 0,
            dpi_scale: 1.0,
            profile_id: None,
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        },
        is_minimized: false,
        focused: Some(FocusedElement {
            element_id: element_id(0x1234, "0000002a00000001"),
            name: "Document".to_owned(),
            role: "Edit".to_owned(),
            automation_id: Some("15".to_owned()),
            bbox: Rect {
                x: 12,
                y: 80,
                w: 760,
                h: 480,
            },
            enabled: true,
            patterns: vec![UiaPattern::Text, UiaPattern::Value],
            value: Some("Synthetic Synapse text".to_owned()),
            selected_text: None,
        }),
        elements: vec![
            node(0, 0, "Notepad", "Window", false),
            node(1, 1, "Document", "Edit", true),
            node(2, 1, "File", "MenuItem", false),
            node(3, 1, "Edit", "MenuItem", false),
            node(4, 1, "View", "MenuItem", false),
            node(5, 1, "Status", "Text", false),
        ],
        entities: vec![DetectedEntity {
            entity_id: entity_id(9),
            track_id: 9,
            class_label: "cursor".to_owned(),
            bbox: Rect {
                x: 40,
                y: 90,
                w: 8,
                h: 20,
            },
            confidence: 0.80,
            first_seen_at: at,
            last_seen_at: at,
            velocity_px_per_s: None,
        }],
        hud: HudReadings::default(),
        audio: AudioContext::default(),
        recent_events: Vec::new(),
        clipboard_summary: None,
        fs_recent: Vec::new(),
        sensor_latency_ms: latency,
        a11y_status: SensorStatus::Healthy,
        capture_status: SensorStatus::Healthy,
        detection_status: SensorStatus::Disabled,
        audio_status: SensorStatus::Disabled,
        mode_override,
        capture_config: None,
        capture_runtime: None,
        input_backends: None,
        cdp: None,
        web_path: None,
    }
}

fn node(sequence: u32, depth: u32, name: &str, role: &str, focused: bool) -> AccessibleNode {
    let depth_i32 = i32::try_from(depth).unwrap_or(0);
    let sequence_i32 = i32::try_from(sequence).unwrap_or(0);
    AccessibleNode {
        element_id: element_id(0x1234, &format!("0000002a{sequence:08x}")),
        parent: (depth > 0).then(|| element_id(0x1234, "0000002a00000000")),
        name: name.to_owned(),
        role: role.to_owned(),
        automation_id: None,
        value: None,
        bbox: Rect {
            x: 10 + depth_i32,
            y: 20 + sequence_i32.saturating_mul(10),
            w: 100,
            h: 30,
        },
        enabled: true,
        focused,
        patterns: Vec::new(),
        children_count: 0,
        depth,
    }
}

criterion_group!(benches, bench_observe_warm_hybrid);
criterion_main!(benches);
