use std::collections::BTreeMap;

use synapse_core::{
    Backend, DataPredicate, EventExtension, EventFilter, HudExtractor, HudFieldSpec, HudParser,
    HudRegion, OcrBackend, PerceptionMode, Profile, ProfileBackends, ProfileCapture,
    ProfileCaptureTarget, ProfileDetection, ProfileMatch, ProfileOcr, ProfileUseScope, WindowEdge,
};

pub fn empty_profile(id: &str) -> Profile {
    Profile {
        id: id.to_owned(),
        label: "Empty Profile".to_owned(),
        version: "1.0.0".to_owned(),
        use_scope: ProfileUseScope::Unknown,
        matches: Vec::new(),
        mode: PerceptionMode::Auto,
        capture: foreground_capture(),
        detection: disabled_detection(),
        ocr: empty_ocr(),
        hud: Vec::new(),
        keymap: BTreeMap::new(),
        backends: software_backends(),
        metadata: BTreeMap::new(),
        event_extensions: Vec::new(),
    }
}

pub fn required_profile(id: &str) -> Profile {
    let mut profile = empty_profile(id);
    "Required Profile".clone_into(&mut profile.label);
    profile.matches = vec![exe_profile_match("notepad.exe")];
    profile.mode = PerceptionMode::A11yOnly;
    profile
}

pub fn full_profile() -> Profile {
    let mut keymap = BTreeMap::new();
    keymap.insert("attack".to_owned(), "lmb".to_owned());
    keymap.insert("inventory".to_owned(), "e".to_owned());
    let mut metadata = BTreeMap::new();
    metadata.insert("benchmark_id".to_owned(), "minecraft.java".to_owned());

    Profile {
        id: "minecraft.java".to_owned(),
        label: "Minecraft Java Edition".to_owned(),
        version: "1.0.0".to_owned(),
        use_scope: ProfileUseScope::SinglePlayer,
        matches: vec![full_profile_match()],
        mode: PerceptionMode::PixelOnly,
        capture: monitor_index_capture(1),
        detection: full_detection(),
        ocr: full_ocr(),
        hud: vec![full_hud_field()],
        keymap,
        backends: mixed_backends(),
        metadata,
        event_extensions: vec![full_event_extension()],
    }
}

pub fn empty_profile_match() -> ProfileMatch {
    ProfileMatch {
        exe: None,
        title_regex: None,
        steam_appid: None,
        window_class: None,
        process_args: Vec::new(),
    }
}

pub fn exe_profile_match(exe: &str) -> ProfileMatch {
    ProfileMatch {
        exe: Some(exe.to_owned()),
        ..empty_profile_match()
    }
}

pub fn full_profile_match() -> ProfileMatch {
    ProfileMatch {
        exe: Some("javaw.exe".to_owned()),
        title_regex: Some("Minecraft\\* [0-9]".to_owned()),
        steam_appid: Some(12_345),
        window_class: Some("LWJGL".to_owned()),
        process_args: vec!["--demo".to_owned()],
    }
}

pub fn foreground_capture() -> ProfileCapture {
    ProfileCapture {
        target: ProfileCaptureTarget::ForegroundWindow,
        min_update_interval_ms: 100,
        cursor_visible: true,
    }
}

pub fn primary_monitor_capture() -> ProfileCapture {
    ProfileCapture {
        target: ProfileCaptureTarget::PrimaryMonitor,
        min_update_interval_ms: 33,
        cursor_visible: true,
    }
}

pub fn monitor_index_capture(index: u32) -> ProfileCapture {
    ProfileCapture {
        target: ProfileCaptureTarget::MonitorIndex { index },
        min_update_interval_ms: 16,
        cursor_visible: false,
    }
}

pub fn disabled_detection() -> ProfileDetection {
    ProfileDetection {
        model_id: None,
        classes_of_interest: Vec::new(),
        confidence_threshold: 0.0,
        max_detections: 0,
    }
}

pub fn minimal_detection() -> ProfileDetection {
    ProfileDetection {
        model_id: Some("none".to_owned()),
        classes_of_interest: Vec::new(),
        confidence_threshold: 0.0,
        max_detections: 0,
    }
}

pub fn full_detection() -> ProfileDetection {
    ProfileDetection {
        model_id: Some("yolov10n_general".to_owned()),
        classes_of_interest: vec!["player".to_owned(), "creeper".to_owned()],
        confidence_threshold: 0.45,
        max_detections: 32,
    }
}

pub fn empty_ocr() -> ProfileOcr {
    ProfileOcr {
        default_backend: OcrBackend::Auto,
        regions: Vec::new(),
        parser_config: BTreeMap::new(),
    }
}

pub fn winrt_ocr() -> ProfileOcr {
    ProfileOcr {
        default_backend: OcrBackend::Winrt,
        regions: Vec::new(),
        parser_config: BTreeMap::new(),
    }
}

pub fn full_ocr() -> ProfileOcr {
    let mut parser_config = BTreeMap::new();
    parser_config.insert("language".to_owned(), "en".to_owned());
    parser_config.insert("normalize_whitespace".to_owned(), "true".to_owned());

    ProfileOcr {
        default_backend: OcrBackend::Crnn,
        regions: vec![anchored_region()],
        parser_config,
    }
}

pub fn minimal_hud_field(name: &str) -> HudFieldSpec {
    HudFieldSpec {
        name: name.to_owned(),
        region: HudRegion::Absolute {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        },
        extractor: HudExtractor::WinrtOcr,
        parser: HudParser::Number,
        confidence_threshold: 0.85,
    }
}

pub fn full_hud_field() -> HudFieldSpec {
    HudFieldSpec {
        name: "hp_hearts".to_owned(),
        region: anchored_region(),
        extractor: full_extractor(),
        parser: full_parser(),
        confidence_threshold: 0.85,
    }
}

pub fn anchored_region() -> HudRegion {
    HudRegion::AnchoredToEdge {
        edge: WindowEdge::BottomLeft,
        x_offset: 220,
        y_offset: -50,
        w: 180,
        h: 18,
    }
}

pub fn full_extractor() -> HudExtractor {
    HudExtractor::TemplateMatch {
        templates: vec![
            "hearts/full.png".to_owned(),
            "hearts/half.png".to_owned(),
            "hearts/empty.png".to_owned(),
        ],
    }
}

pub fn full_parser() -> HudParser {
    HudParser::Regex {
        pattern: r"([0-9]+)/[0-9]+".to_owned(),
        group: 1,
    }
}

pub fn software_backends() -> ProfileBackends {
    ProfileBackends {
        default: Backend::Software,
        keyboard_default: Backend::Software,
        mouse_default: Backend::Software,
        pad_default: Backend::Software,
    }
}

pub fn mixed_backends() -> ProfileBackends {
    ProfileBackends {
        default: Backend::Auto,
        keyboard_default: Backend::Software,
        mouse_default: Backend::Hardware,
        pad_default: Backend::Vigem,
    }
}

pub fn minimal_event_extension(name: &str) -> EventExtension {
    EventExtension {
        name: name.to_owned(),
        from_filter: EventFilter::All,
        emits_kind: format!("{name}-event"),
    }
}

pub fn full_event_extension() -> EventExtension {
    EventExtension {
        name: "creeper_nearby".to_owned(),
        from_filter: EventFilter::And {
            args: vec![
                EventFilter::Kind {
                    kind: "entity-appeared".to_owned(),
                },
                EventFilter::Data {
                    path: "/class_label".to_owned(),
                    predicate: DataPredicate::Eq {
                        value: serde_json::json!("creeper"),
                    },
                },
                EventFilter::Data {
                    path: "/bbox/w".to_owned(),
                    predicate: DataPredicate::Gt {
                        value: serde_json::json!(80),
                    },
                },
            ],
        },
        emits_kind: "creeper-imminent".to_owned(),
    }
}
