use proptest::prelude::*;
use synapse_core::{
    Backend, DataPredicate, EventExtension, EventFilter, EventSource, HudExtractor, HudFieldSpec,
    HudParser, HudRegion, OcrBackend, PerceptionMode, Profile, ProfileBackends, ProfileCapture,
    ProfileCaptureTarget, ProfileDetection, ProfileMatch, ProfileOcr, ProfileUseScope, WindowEdge,
};

pub fn small_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,8}".prop_map(|value| value)
}

pub fn backend_strategy() -> impl Strategy<Value = Backend> {
    prop_oneof![
        Just(Backend::Software),
        Just(Backend::Vigem),
        Just(Backend::Hardware),
        Just(Backend::Auto),
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

pub fn profile_use_scope_strategy() -> impl Strategy<Value = ProfileUseScope> {
    prop_oneof![
        Just(ProfileUseScope::Productivity),
        Just(ProfileUseScope::SinglePlayer),
        Just(ProfileUseScope::OperatorOwnedTest),
        Just(ProfileUseScope::SanctionedResearch),
        Just(ProfileUseScope::Unknown),
    ]
}

pub fn ocr_backend_strategy() -> impl Strategy<Value = OcrBackend> {
    prop_oneof![
        Just(OcrBackend::Winrt),
        Just(OcrBackend::Crnn),
        Just(OcrBackend::Auto),
    ]
}

pub fn window_edge_strategy() -> impl Strategy<Value = WindowEdge> {
    prop_oneof![
        Just(WindowEdge::TopLeft),
        Just(WindowEdge::TopRight),
        Just(WindowEdge::BottomLeft),
        Just(WindowEdge::BottomCenter),
        Just(WindowEdge::BottomRight),
        Just(WindowEdge::Center),
    ]
}

pub fn profile_capture_target_strategy() -> impl Strategy<Value = ProfileCaptureTarget> {
    prop_oneof![
        Just(ProfileCaptureTarget::ForegroundWindow),
        Just(ProfileCaptureTarget::PrimaryMonitor),
        (0_u32..4).prop_map(|index| ProfileCaptureTarget::MonitorIndex { index }),
    ]
}

pub fn profile_capture_strategy() -> impl Strategy<Value = ProfileCapture> {
    (profile_capture_target_strategy(), 0_u32..250, any::<bool>()).prop_map(
        |(target, min_update_interval_ms, cursor_visible)| ProfileCapture {
            target,
            min_update_interval_ms,
            cursor_visible,
        },
    )
}

pub fn profile_match_strategy() -> impl Strategy<Value = ProfileMatch> {
    (
        prop::option::of(small_string()),
        prop::option::of(small_string()),
        prop::option::of(1_u32..1_000_000),
        prop::option::of(small_string()),
        prop::collection::vec(small_string(), 0..4),
    )
        .prop_map(
            |(exe, title_regex, steam_appid, window_class, process_args)| ProfileMatch {
                exe,
                title_regex,
                steam_appid,
                window_class,
                process_args,
            },
        )
}

pub fn profile_detection_strategy() -> impl Strategy<Value = ProfileDetection> {
    (
        prop::option::of(small_string()),
        prop::collection::vec(small_string(), 0..5),
        0.0_f32..1.0,
        0_u32..64,
    )
        .prop_map(
            |(model_id, classes_of_interest, confidence_threshold, max_detections)| {
                ProfileDetection {
                    model_id,
                    classes_of_interest,
                    confidence_threshold,
                    max_detections,
                }
            },
        )
}

pub fn hud_region_strategy() -> impl Strategy<Value = HudRegion> {
    prop_oneof![
        (-100_i32..100, -100_i32..100, 1_i32..400, 1_i32..400)
            .prop_map(|(x, y, w, h)| HudRegion::Absolute { x, y, w, h }),
        (0.0_f32..1.0, 0.0_f32..1.0, 0.01_f32..1.0, 0.01_f32..1.0)
            .prop_map(|(x, y, w, h)| HudRegion::FractionOfWindow { x, y, w, h }),
        (
            window_edge_strategy(),
            -500_i32..500,
            -500_i32..500,
            1_i32..400,
            1_i32..400,
        )
            .prop_map(
                |(edge, x_offset, y_offset, w, h)| HudRegion::AnchoredToEdge {
                    edge,
                    x_offset,
                    y_offset,
                    w,
                    h,
                }
            ),
    ]
}

pub fn hud_extractor_strategy() -> impl Strategy<Value = HudExtractor> {
    prop_oneof![
        Just(HudExtractor::WinrtOcr),
        small_string().prop_map(|model_id| HudExtractor::Crnn { model_id }),
        prop::collection::vec(small_string(), 0..4)
            .prop_map(|templates| HudExtractor::TemplateMatch { templates }),
        (
            prop::collection::vec((-50_i32..50, -50_i32..50), 0..4),
            small_string(),
        )
            .prop_map(|(sample_points, mapping)| HudExtractor::ColorRatio {
                sample_points,
                mapping,
            }),
    ]
}

pub fn hud_parser_strategy() -> impl Strategy<Value = HudParser> {
    prop_oneof![
        Just(HudParser::Number),
        (0_u32..30, 30_u32..100, prop::option::of(0_u32..30)).prop_map(
            |(min, max, default_on_no_text)| HudParser::BoundedInteger {
                min,
                max,
                default_on_no_text,
            }
        ),
        Just(HudParser::FractionNumerator),
        Just(HudParser::FractionDenominator),
        (small_string(), 0_u32..5).prop_map(|(pattern, group)| HudParser::Regex { pattern, group }),
        prop::collection::btree_map(small_string(), small_string(), 0..4)
            .prop_map(|mapping| HudParser::Enum { mapping }),
    ]
}

pub fn profile_ocr_strategy() -> impl Strategy<Value = ProfileOcr> {
    (
        ocr_backend_strategy(),
        prop::collection::vec(hud_region_strategy(), 0..3),
        prop::collection::btree_map(small_string(), small_string(), 0..3),
    )
        .prop_map(|(default_backend, regions, parser_config)| ProfileOcr {
            default_backend,
            regions,
            parser_config,
        })
}

pub fn hud_field_spec_strategy() -> impl Strategy<Value = HudFieldSpec> {
    (
        small_string(),
        hud_region_strategy(),
        hud_extractor_strategy(),
        hud_parser_strategy(),
        0.0_f32..1.0,
    )
        .prop_map(
            |(name, region, extractor, parser, confidence_threshold)| HudFieldSpec {
                name,
                region,
                extractor,
                parser,
                confidence_threshold,
            },
        )
}

pub fn profile_backends_strategy() -> impl Strategy<Value = ProfileBackends> {
    (
        backend_strategy(),
        backend_strategy(),
        backend_strategy(),
        backend_strategy(),
    )
        .prop_map(
            |(default, keyboard_default, mouse_default, pad_default)| ProfileBackends {
                default,
                keyboard_default,
                mouse_default,
                pad_default,
            },
        )
}

pub fn event_filter_strategy() -> impl Strategy<Value = EventFilter> {
    prop_oneof![
        Just(EventFilter::All),
        Just(EventFilter::None),
        small_string().prop_map(|kind| EventFilter::Kind { kind }),
        Just(EventFilter::Source {
            source: EventSource::PerceptionHud,
        }),
        small_string().prop_map(|kind| EventFilter::Not {
            arg: Box::new(EventFilter::Kind { kind }),
        }),
        (small_string(), small_string()).prop_map(|(path, value)| EventFilter::Data {
            path: format!("/{path}"),
            predicate: DataPredicate::Eq {
                value: serde_json::json!(value),
            },
        }),
    ]
}

pub fn event_extension_strategy() -> impl Strategy<Value = EventExtension> {
    (small_string(), event_filter_strategy(), small_string()).prop_map(
        |(name, from_filter, emits_kind)| EventExtension {
            name,
            from_filter,
            emits_kind,
        },
    )
}

pub fn profile_identity_strategy() -> impl Strategy<Value = (String, String, String)> {
    (small_string(), small_string()).prop_map(|(id, label)| (id, label, "1.0.0".to_owned()))
}

pub fn profile_strategy() -> impl Strategy<Value = Profile> {
    (
        profile_identity_strategy(),
        profile_use_scope_strategy(),
        prop::collection::vec(profile_match_strategy(), 0..3),
        perception_mode_strategy(),
        profile_capture_strategy(),
        profile_detection_strategy(),
        profile_ocr_strategy(),
        prop::collection::vec(hud_field_spec_strategy(), 0..3),
        prop::collection::btree_map(small_string(), small_string(), 0..4),
        profile_backends_strategy(),
        prop::collection::btree_map(small_string(), small_string(), 0..4),
        prop::collection::vec(event_extension_strategy(), 0..3),
    )
        .prop_map(
            |(
                (id, label, version),
                use_scope,
                matches,
                mode,
                capture,
                detection,
                ocr,
                hud,
                keymap,
                backends,
                metadata,
                event_extensions,
            )| Profile {
                id,
                label,
                version,
                use_scope,
                matches,
                mode,
                capture,
                detection,
                ocr,
                hud,
                keymap,
                backends,
                metadata,
                event_extensions,
            },
        )
}
