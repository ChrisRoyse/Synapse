use std::collections::BTreeMap;

use proptest::{
    prelude::*,
    test_runner::{Config, TestRng, TestRunner},
};
use schemars::schema_for;
use synapse_core::{
    AccessibleQuery, AccessibleQueryScope, Backend, CdpStatus, DataPredicate, Detection,
    DetectionBatch, ElementId, EventFilter, EventSource, Health, Observation, PerceptionMode,
    Point, Rect, SCHEMA_VERSION, Size, SubsystemHealth, element_id, entity_id, new_reflex_id,
    new_session_id, new_subscription_id,
};

#[path = "types/support.rs"]
mod support;

use support::{fixed_time, observation_strategy, sample_event, sample_observation};

#[test]
fn backend_json_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let cases = [
        (Backend::Software, "\"software\""),
        (Backend::Vigem, "\"vigem\""),
        (Backend::Hardware, "\"hardware\""),
        (Backend::Auto, "\"auto\""),
    ];

    for (variant, json) in cases {
        assert_eq!(serde_json::to_string(&variant)?, json);
        assert_eq!(serde_json::from_str::<Backend>(json)?, variant);
    }

    assert!(serde_json::from_str::<Backend>("\"foo\"").is_err());
    assert!(serde_json::from_str::<Backend>("\"Software\"").is_err());
    assert!(serde_json::from_str::<Backend>("\"software \"").is_err());
    assert!(serde_json::from_str::<Backend>("null").is_err());
    Ok(())
}

#[test]
fn perception_mode_json_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let cases = [
        (PerceptionMode::A11yOnly, "\"a11y_only\""),
        (PerceptionMode::PixelOnly, "\"pixel_only\""),
        (PerceptionMode::Hybrid, "\"hybrid\""),
        (PerceptionMode::Auto, "\"auto\""),
    ];

    for (variant, json) in cases {
        assert_eq!(serde_json::to_string(&variant)?, json);
        assert_eq!(serde_json::from_str::<PerceptionMode>(json)?, variant);
    }

    assert!(serde_json::from_str::<PerceptionMode>("\"a11yOnly\"").is_err());
    assert!(serde_json::from_str::<PerceptionMode>("\"pixel\"").is_err());
    assert!(serde_json::from_str::<PerceptionMode>("\"\"").is_err());
    Ok(())
}

#[test]
fn cdp_status_json_uses_diagnostic_error_codes() -> Result<(), Box<dyn std::error::Error>> {
    let cases = [
        (CdpStatus::Ok, "\"ok\""),
        (CdpStatus::NotChromium, "\"not_chromium\""),
        (CdpStatus::Unreachable, "\"A11Y_CDP_UNREACHABLE\""),
        (
            CdpStatus::ExtensionUnavailable,
            "\"A11Y_CDP_EXTENSION_UNAVAILABLE\"",
        ),
        (CdpStatus::AttachFailed, "\"A11Y_CDP_ATTACH_FAILED\""),
    ];

    for (variant, json) in cases {
        assert_eq!(serde_json::to_string(&variant)?, json);
        assert_eq!(serde_json::from_str::<CdpStatus>(json)?, variant);
    }

    assert!(serde_json::from_str::<CdpStatus>("\"unreachable\"").is_err());
    assert!(serde_json::from_str::<CdpStatus>("\"attach_failed\"").is_err());
    Ok(())
}

#[test]
fn geometry_json_and_helpers_are_stable() -> Result<(), Box<dyn std::error::Error>> {
    let point = Point { x: -100, y: 25 };
    let rect = Rect {
        x: 0,
        y: 0,
        w: 10,
        h: 10,
    };
    let size = Size { w: 1920, h: 1080 };

    assert_eq!(
        serde_json::from_str::<Point>(&serde_json::to_string(&point)?)?,
        point
    );
    assert_eq!(
        serde_json::from_str::<Rect>(&serde_json::to_string(&rect)?)?,
        rect
    );
    assert_eq!(
        serde_json::from_str::<Size>(&serde_json::to_string(&size)?)?,
        size
    );

    assert!(rect.contains(Point { x: 5, y: 5 }));
    assert!(!rect.contains(Point { x: 10, y: 5 }));
    assert!(
        !Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 10
        }
        .contains(Point { x: 0, y: 0 })
    );
    assert!(
        Rect {
            x: -200,
            y: -100,
            w: 50,
            h: 50,
        }
        .contains(Point { x: -175, y: -75 })
    );
    let distance = Point { x: 0, y: 0 }.distance_to(Point { x: 3, y: 4 });
    assert!((distance - 5.0).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn id_helpers_have_expected_shapes() {
    let first = new_session_id();
    let second = new_session_id();
    let uuid_v7 =
        regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .unwrap_or_else(|err| panic!("regex should compile: {err}"));

    assert_ne!(first, second);
    assert!(uuid_v7.is_match(&first));
    assert!(uuid_v7.is_match(&new_reflex_id()));
    assert!(uuid_v7.is_match(&new_subscription_id()));
    assert_eq!(element_id(123, "abc"), "0x7b:abc");
    assert_eq!(element_id(-1, "ff"), "-0x1:ff");
    assert_eq!(entity_id(42), "track:42");
    assert_eq!(entity_id(u64::MAX), format!("track:{}", u64::MAX));

    let ids: Vec<_> = (0..100).map(|_| new_session_id()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
}

#[test]
fn element_id_parse_display_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let first = ElementId::parse("0x12ab:0a1b2c3d")?;
    let second = ElementId::parse("0x12ab:ffffffff")?;
    let first_parts = first.parts()?;

    assert_eq!(first.to_string(), "0x12ab:0a1b2c3d");
    assert_eq!(first_parts.hwnd, 0x12ab);
    assert_eq!(first_parts.runtime_id_hex, "0a1b2c3d");
    assert_ne!(first, second);
    assert_eq!(serde_json::to_string(&first)?, "\"0x12ab:0a1b2c3d\"");
    assert_eq!(
        serde_json::from_str::<ElementId>("\"0x12ab:0a1b2c3d\"")?,
        first
    );

    assert!(serde_json::from_str::<ElementId>("\"not-an-element\"").is_err());
    assert!(ElementId::parse("0x12ab").is_err());
    assert!(ElementId::parse("12ab:0a1b").is_err());
    assert!(ElementId::parse("0x12ab:").is_err());
    assert!(ElementId::parse("0x12ab:not-hex").is_err());
    Ok(())
}

#[test]
fn accessible_query_defaults_to_focused_subtree() -> Result<(), Box<dyn std::error::Error>> {
    let query = serde_json::from_value::<AccessibleQuery>(serde_json::json!({
        "role": "Button",
        "name_substring": "Save"
    }))?;

    assert_eq!(query.scope, AccessibleQueryScope::FocusedSubtree);
    assert_eq!(query.role.as_deref(), Some("Button"));
    assert_eq!(query.name_substring.as_deref(), Some("Save"));
    assert!(query.automation_id.is_none());
    assert!(
        serde_json::from_value::<AccessibleQuery>(serde_json::json!({
            "role": "Button",
            "unexpected": true
        }))
        .is_err()
    );
    Ok(())
}

#[test]
fn observation_json_round_trips_and_preserves_ids() -> Result<(), Box<dyn std::error::Error>> {
    let observation = sample_observation()?;
    let json = serde_json::to_string(&observation)?;
    let parsed = serde_json::from_str::<Observation>(&json)?;

    assert_eq!(parsed, observation);
    assert_eq!(
        parsed.focused.as_ref().map(|focused| &focused.element_id),
        Some(&element_id(0x12ab, "0a1b2c3d"))
    );
    assert_eq!(
        serde_json::from_str::<Observation>(
            r#"{"seq":1,"at":"2026-05-23T00:00:00Z","mode":"auto","extra":true}"#
        )
        .map(|value| value.seq)
        .ok(),
        None
    );
    Ok(())
}

#[test]
fn detection_batch_json_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let batch = DetectionBatch {
        model_id: "yolov10n-general".to_owned(),
        frame_seq: 42,
        inferred_at: fixed_time()?,
        items: vec![Detection {
            class_label: "enemy".to_owned(),
            bbox: Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            },
            confidence: 0.875,
            track_id: Some(7),
        }],
    };

    assert_eq!(
        serde_json::from_str::<DetectionBatch>(&serde_json::to_string(&batch)?)?,
        batch
    );
    Ok(())
}

#[test]
fn event_filter_predicates_cover_each_variant() -> Result<(), Box<dyn std::error::Error>> {
    let event = sample_event()?;

    assert!(EventFilter::All.matches(&event));
    assert!(!EventFilter::None.matches(&event));
    assert!(
        EventFilter::Kind {
            kind: "hud-value-changed".to_owned()
        }
        .matches(&event)
    );
    assert!(
        EventFilter::Source {
            source: EventSource::PerceptionHud
        }
        .matches(&event)
    );
    assert!(
        EventFilter::And {
            args: vec![
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
        }
        .matches(&event)
    );
    assert!(
        EventFilter::Or {
            args: vec![
                EventFilter::Kind {
                    kind: "focus-changed".to_owned(),
                },
                EventFilter::Kind {
                    kind: "hud-value-changed".to_owned(),
                },
            ],
        }
        .matches(&event)
    );
    assert!(
        EventFilter::Not {
            arg: Box::new(EventFilter::Kind {
                kind: "focus-changed".to_owned(),
            }),
        }
        .matches(&event)
    );

    assert!(DataPredicate::Exists.matches(event.data.pointer("/field")));
    assert!(
        DataPredicate::Ne {
            value: serde_json::json!("ammo")
        }
        .matches(event.data.pointer("/field"))
    );
    assert!(
        DataPredicate::Le {
            value: serde_json::json!(19)
        }
        .matches(event.data.pointer("/new"))
    );
    assert!(
        DataPredicate::Gt {
            value: serde_json::json!(5)
        }
        .matches(event.data.pointer("/new"))
    );
    assert!(
        DataPredicate::Ge {
            value: serde_json::json!(15)
        }
        .matches(event.data.pointer("/new"))
    );
    assert!(
        DataPredicate::Regex {
            pattern: "^h.$".to_owned()
        }
        .matches(event.data.pointer("/field"))
    );
    assert!(
        DataPredicate::InSet {
            values: vec![serde_json::json!("ammo"), serde_json::json!("hp")]
        }
        .matches(event.data.pointer("/field"))
    );
    assert!(
        !DataPredicate::Regex {
            pattern: "[".to_owned()
        }
        .matches(event.data.pointer("/field"))
    );
    Ok(())
}

#[test]
fn observation_proptest_json_round_trip_is_deterministic() -> Result<(), Box<dyn std::error::Error>>
{
    let config = Config {
        cases: 1_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    runner.run(&observation_strategy(), |observation| {
        let json = serde_json::to_string(&observation)?;
        let parsed = serde_json::from_str::<Observation>(&json)?;
        prop_assert!(!parsed.elements.is_empty());
        prop_assert_eq!(parsed, observation);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn schema_version_is_root_reexported_u32() {
    let version: u32 = SCHEMA_VERSION;
    assert_eq!(version, 1);
}

#[test]
fn health_json_shape_and_schema_are_stable() -> Result<(), Box<dyn std::error::Error>> {
    let health = Health {
        ok: true,
        version: "0.1.0".to_owned(),
        build: "dev".to_owned(),
        pid: 4321,
        uptime_s: 0,
        tool_count: 2,
        tool_surface_sha256: "0123456789abcdef".to_owned(),
        tool_names: vec!["act_run_shell".to_owned(), "health".to_owned()],
        subsystems: BTreeMap::new(),
    };
    let expected = r#"{"ok":true,"version":"0.1.0","build":"dev","pid":4321,"uptime_s":0,"tool_count":2,"tool_surface_sha256":"0123456789abcdef","tool_names":["act_run_shell","health"],"subsystems":{}}"#;
    assert_eq!(serde_json::to_string(&health)?, expected);

    let value = serde_json::to_value(&health)?;
    assert_eq!(value["subsystems"], serde_json::json!({}));

    let schema = serde_json::to_value(schema_for!(Health))?;
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["subsystems"].is_object());

    let subsystem = SubsystemHealth {
        status: "healthy".to_owned(),
        detail: None,
        active_profile_id: None,
        ..SubsystemHealth::default()
    };
    assert_eq!(
        serde_json::to_value(subsystem)?,
        serde_json::json!({"status":"healthy","detail":null})
    );
    Ok(())
}
