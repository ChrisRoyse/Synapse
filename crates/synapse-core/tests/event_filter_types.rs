#![allow(clippy::missing_const_for_fn)]

use std::{collections::BTreeMap, fmt::Debug};

use chrono::{DateTime, Utc};
use proptest::{
    prelude::*,
    test_runner::{Config, TestRng, TestRunner},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use synapse_core::{
    DataPredicate, EVENT_FILTER_MAX_DEPTH, Event, EventFilter, EventFilterValidationError,
    EventSource,
};

#[test]
fn event_filter_variants_snapshot_with_readback() -> Result<(), Box<dyn std::error::Error>> {
    let filters = event_filter_variants()
        .into_iter()
        .map(|(name, filter)| round_trip("EventFilter", name, filter).map(|value| (name, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let predicates = data_predicate_variants()
        .into_iter()
        .map(|(name, predicate)| {
            round_trip("DataPredicate", name, predicate).map(|value| (name, value))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    insta::assert_json_snapshot!(
        "event_filter_variants",
        json!({
            "filters": filters,
            "data_predicates": predicates,
        })
    );
    Ok(())
}

#[test]
fn event_filter_validation_edges_have_readback() {
    let empty_and = EventFilter::And { args: Vec::new() };
    println!(
        "readback=event_filter_validation edge=empty_and before=depth:{}",
        empty_and.depth()
    );
    let empty_and_after = empty_and.validate();
    println!("readback=event_filter_validation edge=empty_and after={empty_and_after:?}");
    assert_eq!(empty_and_after, Err(EventFilterValidationError::EmptyAnd));

    let empty_or = EventFilter::Or { args: Vec::new() };
    println!(
        "readback=event_filter_validation edge=empty_or before=depth:{}",
        empty_or.depth()
    );
    let empty_or_after = empty_or.validate();
    println!("readback=event_filter_validation edge=empty_or after={empty_or_after:?}");
    assert_eq!(empty_or_after, Err(EventFilterValidationError::EmptyOr));

    let depth_8 = nested_not(EVENT_FILTER_MAX_DEPTH);
    println!(
        "readback=event_filter_validation edge=depth_8 before=depth:{}",
        depth_8.depth()
    );
    let depth_8_after = depth_8.validate();
    println!("readback=event_filter_validation edge=depth_8 after={depth_8_after:?}");
    assert_eq!(depth_8.depth(), EVENT_FILTER_MAX_DEPTH);
    assert_eq!(depth_8_after, Ok(()));

    let depth_9 = nested_not(EVENT_FILTER_MAX_DEPTH + 1);
    println!(
        "readback=event_filter_validation edge=depth_9 before=depth:{}",
        depth_9.depth()
    );
    let depth_9_after = depth_9.validate();
    println!("readback=event_filter_validation edge=depth_9 after={depth_9_after:?}");
    assert_eq!(
        depth_9_after,
        Err(EventFilterValidationError::DepthExceeded {
            depth: EVENT_FILTER_MAX_DEPTH + 1,
            max_depth: EVENT_FILTER_MAX_DEPTH,
        })
    );

    let invalid_path = EventFilter::Data {
        path: "field".to_owned(),
        predicate: DataPredicate::Exists,
    };
    println!("readback=event_filter_validation edge=invalid_path before=path:field");
    let invalid_path_after = invalid_path.validate();
    println!("readback=event_filter_validation edge=invalid_path after={invalid_path_after:?}");
    assert_eq!(
        invalid_path_after,
        Err(EventFilterValidationError::InvalidDataPath {
            path: "field".to_owned(),
            reason: "path must be empty or start with '/'".to_owned(),
        })
    );

    let invalid_regex = EventFilter::Data {
        path: "/field".to_owned(),
        predicate: DataPredicate::Regex {
            pattern: "[".to_owned(),
        },
    };
    println!("readback=event_filter_validation edge=invalid_regex before=pattern:[");
    let invalid_regex_after = invalid_regex.validate();
    println!("readback=event_filter_validation edge=invalid_regex after={invalid_regex_after:?}");
    assert!(matches!(
        invalid_regex_after,
        Err(EventFilterValidationError::InvalidRegex { .. })
    ));
}

#[test]
fn event_filter_trivially_always_true_detection_has_readback() {
    let all_filter = EventFilter::All;
    println!("readback=event_filter_trivial_true edge=all before={all_filter:?}");
    let all_after = all_filter.is_trivially_always_true();
    println!("readback=event_filter_trivial_true edge=all after={all_after}");
    assert!(all_after);

    let not_none = EventFilter::Not {
        arg: Box::new(EventFilter::None),
    };
    println!("readback=event_filter_trivial_true edge=not_none before={not_none:?}");
    let not_none_after = not_none.is_trivially_always_true();
    println!("readback=event_filter_trivial_true edge=not_none after={not_none_after}");
    assert!(not_none_after);

    let guarded_kind = EventFilter::And {
        args: vec![
            EventFilter::All,
            EventFilter::Kind {
                kind: "entity_appeared".to_owned(),
            },
        ],
    };
    println!("readback=event_filter_trivial_true edge=guarded_kind before={guarded_kind:?}");
    let guarded_kind_after = guarded_kind.is_trivially_always_true();
    println!("readback=event_filter_trivial_true edge=guarded_kind after={guarded_kind_after}");
    assert!(!guarded_kind_after);
}

#[test]
fn event_filter_predicate_matches_have_readback() {
    let event = sample_event();
    let low_hp = EventFilter::And {
        args: vec![
            EventFilter::Kind {
                kind: "hud-value-changed".to_owned(),
            },
            EventFilter::Source {
                source: EventSource::PerceptionHud,
            },
            EventFilter::Data {
                path: "/field".to_owned(),
                predicate: DataPredicate::Eq { value: json!("hp") },
            },
            EventFilter::Data {
                path: "/new".to_owned(),
                predicate: DataPredicate::Lt { value: json!(20) },
            },
        ],
    };
    println!(
        "readback=event_filter_match edge=low_hp before=event_kind:{} data:{}",
        event.kind, event.data
    );
    let matched = low_hp.matches(&event);
    println!("readback=event_filter_match edge=low_hp after=matched:{matched}");
    assert!(matched);

    let missing_path = EventFilter::Data {
        path: "/missing".to_owned(),
        predicate: DataPredicate::Exists,
    };
    println!("readback=event_filter_match edge=missing_path before=path:/missing");
    let missing_after = missing_path.matches(&event);
    println!("readback=event_filter_match edge=missing_path after=matched:{missing_after}");
    assert!(!missing_after);

    let invalid_regex = EventFilter::Data {
        path: "/field".to_owned(),
        predicate: DataPredicate::Regex {
            pattern: "[".to_owned(),
        },
    };
    println!("readback=event_filter_match edge=invalid_regex before=pattern:[");
    let invalid_regex_after = invalid_regex.matches(&event);
    println!("readback=event_filter_match edge=invalid_regex after=matched:{invalid_regex_after}");
    assert!(!invalid_regex_after);
}

#[test]
fn event_filter_double_not_proptest_is_equivalent() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config {
        cases: 1_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    println!("readback=event_filter_double_not_proptest before=cases:1000");
    runner.run(
        &(event_filter_strategy(), event_strategy()),
        |(filter, event)| {
            let double_not = EventFilter::Not {
                arg: Box::new(EventFilter::Not {
                    arg: Box::new(filter.clone()),
                }),
            };
            prop_assert_eq!(double_not.matches(&event), filter.matches(&event));
            Ok(())
        },
    )?;
    println!(
        "readback=event_filter_double_not_proptest after=cases:1000 result_value=all_equivalent"
    );
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn round_trip<T>(type_name: &str, edge: &str, value: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static,
{
    let before = serde_json::to_value(value.clone())?;
    println!("readback=json_event_filter type={type_name} edge={edge} before={before}");
    let parsed = serde_json::from_value::<T>(before)?;
    let after = serde_json::to_value(&parsed)?;
    println!(
        "readback=json_event_filter type={type_name} edge={edge} after={after} result_value={after}"
    );
    assert_eq!(parsed, value);
    Ok(parsed)
}

fn event_filter_variants() -> Vec<(&'static str, EventFilter)> {
    vec![
        ("all", EventFilter::All),
        ("none", EventFilter::None),
        (
            "kind",
            EventFilter::Kind {
                kind: "hud-value-changed".to_owned(),
            },
        ),
        (
            "source",
            EventFilter::Source {
                source: EventSource::PerceptionHud,
            },
        ),
        (
            "and",
            EventFilter::And {
                args: vec![
                    EventFilter::All,
                    EventFilter::Kind {
                        kind: "hud-value-changed".to_owned(),
                    },
                ],
            },
        ),
        (
            "or",
            EventFilter::Or {
                args: vec![
                    EventFilter::None,
                    EventFilter::Kind {
                        kind: "hud-value-changed".to_owned(),
                    },
                ],
            },
        ),
        (
            "not",
            EventFilter::Not {
                arg: Box::new(EventFilter::None),
            },
        ),
        (
            "data",
            EventFilter::Data {
                path: "/field".to_owned(),
                predicate: DataPredicate::Eq { value: json!("hp") },
            },
        ),
    ]
}

fn data_predicate_variants() -> Vec<(&'static str, DataPredicate)> {
    vec![
        ("eq", DataPredicate::Eq { value: json!("hp") }),
        (
            "ne",
            DataPredicate::Ne {
                value: json!("ammo"),
            },
        ),
        ("lt", DataPredicate::Lt { value: json!(20) }),
        ("le", DataPredicate::Le { value: json!(15) }),
        ("gt", DataPredicate::Gt { value: json!(5) }),
        ("ge", DataPredicate::Ge { value: json!(15) }),
        (
            "regex",
            DataPredicate::Regex {
                pattern: "^h.$".to_owned(),
            },
        ),
        (
            "in_set",
            DataPredicate::InSet {
                values: vec![json!("ammo"), json!("hp")],
            },
        ),
        ("exists", DataPredicate::Exists),
    ]
}

fn nested_not(depth: u32) -> EventFilter {
    let mut filter = EventFilter::All;
    for _ in 1..depth {
        filter = EventFilter::Not {
            arg: Box::new(filter),
        };
    }
    filter
}

fn sample_event() -> Event {
    Event {
        seq: 10,
        at: fixed_time(),
        source: EventSource::PerceptionHud,
        kind: "hud-value-changed".to_owned(),
        data: json!({
            "field": "hp",
            "old": 25,
            "new": 15,
            "confidence": 0.98,
            "flag": true,
        }),
        correlations: Vec::new(),
    }
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::UNIX_EPOCH + std::time::Duration::from_hours(494_304))
}

fn event_strategy() -> impl Strategy<Value = Event> {
    (
        event_source_strategy(),
        event_kind_strategy(),
        0_u64..1_000,
        0_i64..200,
        any::<bool>(),
    )
        .prop_map(|(source, kind, seq, new_value, flag)| Event {
            seq,
            at: fixed_time(),
            source,
            kind,
            data: json!({
                "field": if flag { "hp" } else { "ammo" },
                "new": new_value,
                "flag": flag,
            }),
            correlations: Vec::new(),
        })
}

fn event_filter_strategy() -> impl Strategy<Value = EventFilter> {
    leaf_filter_strategy().prop_recursive(4, 32, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..=3).prop_map(|args| EventFilter::And { args }),
            prop::collection::vec(inner.clone(), 1..=3).prop_map(|args| EventFilter::Or { args }),
            inner.prop_map(|arg| EventFilter::Not { arg: Box::new(arg) }),
        ]
    })
}

fn leaf_filter_strategy() -> impl Strategy<Value = EventFilter> {
    prop_oneof![
        Just(EventFilter::All),
        Just(EventFilter::None),
        event_kind_strategy().prop_map(|kind| EventFilter::Kind { kind }),
        event_source_strategy().prop_map(|source| EventFilter::Source { source }),
        data_filter_strategy(),
    ]
}

fn data_filter_strategy() -> impl Strategy<Value = EventFilter> {
    (path_strategy(), data_predicate_strategy())
        .prop_map(|(path, predicate)| EventFilter::Data { path, predicate })
}

fn data_predicate_strategy() -> impl Strategy<Value = DataPredicate> {
    prop_oneof![
        Just(DataPredicate::Exists),
        value_strategy().prop_map(|value| DataPredicate::Eq { value }),
        value_strategy().prop_map(|value| DataPredicate::Ne { value }),
        numeric_value_strategy().prop_map(|value| DataPredicate::Lt { value }),
        numeric_value_strategy().prop_map(|value| DataPredicate::Le { value }),
        numeric_value_strategy().prop_map(|value| DataPredicate::Gt { value }),
        numeric_value_strategy().prop_map(|value| DataPredicate::Ge { value }),
        Just(DataPredicate::Regex {
            pattern: "^h.$".to_owned(),
        }),
        prop::collection::vec(value_strategy(), 0..=4)
            .prop_map(|values| DataPredicate::InSet { values }),
    ]
}

fn value_strategy() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(json!("hp")),
        Just(json!("ammo")),
        numeric_value_strategy(),
        any::<bool>().prop_map(serde_json::Value::Bool),
    ]
}

fn numeric_value_strategy() -> impl Strategy<Value = serde_json::Value> {
    (0_i64..200).prop_map(|value| json!(value))
}

fn path_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("/field".to_owned()),
        Just("/new".to_owned()),
        Just("/flag".to_owned()),
        Just("/missing".to_owned()),
    ]
}

fn event_kind_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("hud-value-changed".to_owned()),
        Just("focus-changed".to_owned()),
        Just("reflex-fired".to_owned()),
    ]
}

fn event_source_strategy() -> impl Strategy<Value = EventSource> {
    prop_oneof![
        Just(EventSource::PerceptionHud),
        Just(EventSource::A11yUia),
        Just(EventSource::Reflex),
        Just(EventSource::System),
    ]
}
