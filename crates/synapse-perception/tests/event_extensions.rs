#![allow(clippy::missing_const_for_fn)]

use chrono::{DateTime, Utc};
use serde_json::json;
use synapse_core::{DataPredicate, Event, EventExtension, EventFilter, EventSource, error_codes};
use synapse_perception::{evaluate_event_extensions, validate_event_extensions};
use synapse_reflex::EventBus;

#[test]
fn event_extensions_emit_creeper_event_on_bus() -> Result<(), Box<dyn std::error::Error>> {
    let extensions = vec![creeper_extension()];
    let trigger = creeper_event(42, "creeper", 100);
    let bus = EventBus::default();
    let subscriber = bus.subscribe(
        EventFilter::Kind {
            kind: "creeper-imminent".to_owned(),
        },
        Vec::new(),
        false,
    )?;

    println!(
        "readback=event_extensions edge=creeper_happy before=trigger:{} bus_len:{}",
        trigger.data,
        subscriber.len()
    );
    let derived = evaluate_event_extensions(&extensions, &trigger, 1_000)?;
    println!(
        "readback=event_extensions edge=creeper_happy after_derived={}",
        serde_json::to_string(&derived)?
    );
    for event in derived {
        let report = bus.publish(event);
        println!("readback=event_extensions edge=creeper_happy publish_report={report:?}");
    }
    let drained = subscriber.drain();
    println!(
        "readback=event_extensions edge=creeper_happy bus_after={}",
        serde_json::to_string(&drained)?
    );

    assert_eq!(drained.len(), 1);
    let event = drained.first().ok_or("missing drained event")?;
    assert_eq!(event.seq, 1_000);
    assert_eq!(event.source, EventSource::Perception);
    assert_eq!(event.kind, "creeper-imminent");
    assert_eq!(event.data["extension_name"], "creeper_nearby");
    assert_eq!(event.data["trigger_seq"], 42);
    assert_eq!(event.data["trigger_kind"], "entity_appeared");
    assert_eq!(event.correlations.len(), 1);
    let correlation = event.correlations.first().ok_or("missing correlation")?;
    assert_eq!(correlation.seq, 42);
    assert_eq!(correlation.relation, "event_extension_trigger");
    Ok(())
}

#[test]
fn event_extensions_do_not_emit_for_nonmatching_class() -> Result<(), Box<dyn std::error::Error>> {
    let extensions = vec![creeper_extension()];
    let trigger = creeper_event(43, "zombie", 100);
    println!(
        "readback=event_extensions edge=nonmatching_class before=trigger:{}",
        trigger.data
    );
    let derived = evaluate_event_extensions(&extensions, &trigger, 1_001)?;
    println!(
        "readback=event_extensions edge=nonmatching_class after={}",
        serde_json::to_string(&derived)?
    );
    assert!(derived.is_empty());
    Ok(())
}

#[test]
fn event_extensions_do_not_emit_at_bbox_threshold() -> Result<(), Box<dyn std::error::Error>> {
    let extensions = vec![creeper_extension()];
    let trigger = creeper_event(44, "creeper", 80);
    println!(
        "readback=event_extensions edge=bbox_threshold before=trigger:{}",
        trigger.data
    );
    let derived = evaluate_event_extensions(&extensions, &trigger, 1_002)?;
    println!(
        "readback=event_extensions edge=bbox_threshold after={}",
        serde_json::to_string(&derived)?
    );
    assert!(derived.is_empty());

    let trigger = creeper_event(46, "creeper", 79);
    println!(
        "readback=event_extensions edge=bbox_below_threshold before=trigger:{}",
        trigger.data
    );
    let derived = evaluate_event_extensions(&extensions, &trigger, 1_003)?;
    println!(
        "readback=event_extensions edge=bbox_below_threshold after={}",
        serde_json::to_string(&derived)?
    );
    assert!(derived.is_empty());
    Ok(())
}

#[test]
fn event_extensions_reject_always_true_filters() {
    let extensions = vec![EventExtension {
        name: "always_true".to_owned(),
        from_filter: EventFilter::All,
        emits_kind: "always.true".to_owned(),
    }];
    println!(
        "readback=event_extensions edge=always_true before=extensions:{}",
        extensions.len()
    );
    let error = validate_event_extensions(&extensions).err();
    println!("readback=event_extensions edge=always_true after={error:?}");
    let Some(error) = error else {
        panic!("always-true event extension registered successfully but should fail");
    };
    assert_eq!(error.code(), error_codes::PROFILE_PARSE_ERROR);
    assert!(
        error
            .to_string()
            .contains("from_filter must not be trivially always true")
    );
}

#[test]
fn event_extensions_reject_sequence_overflow() {
    let extensions = vec![
        EventExtension {
            name: "first".to_owned(),
            from_filter: EventFilter::Kind {
                kind: "entity_appeared".to_owned(),
            },
            emits_kind: "first.synthetic".to_owned(),
        },
        EventExtension {
            name: "second".to_owned(),
            from_filter: EventFilter::Kind {
                kind: "entity_appeared".to_owned(),
            },
            emits_kind: "second.synthetic".to_owned(),
        },
    ];
    let trigger = creeper_event(45, "creeper", 100);
    println!(
        "readback=event_extensions edge=seq_overflow before=first_seq:{} trigger_seq:{}",
        u64::MAX,
        trigger.seq
    );
    let error = evaluate_event_extensions(&extensions, &trigger, u64::MAX).err();
    println!("readback=event_extensions edge=seq_overflow after={error:?}");
    let Some(error) = error else {
        panic!("sequence overflow was not rejected");
    };
    assert_eq!(error.code(), error_codes::PROFILE_PARSE_ERROR);
    assert!(error.to_string().contains("sequence assignment overflowed"));
}

fn creeper_extension() -> EventExtension {
    EventExtension {
        name: "creeper_nearby".to_owned(),
        from_filter: EventFilter::And {
            args: vec![
                EventFilter::Source {
                    source: EventSource::PerceptionDetection,
                },
                EventFilter::Kind {
                    kind: "entity_appeared".to_owned(),
                },
                EventFilter::Data {
                    path: "/profile_id".to_owned(),
                    predicate: DataPredicate::Eq {
                        value: json!("minecraft.java"),
                    },
                },
                EventFilter::Data {
                    path: "/class_label".to_owned(),
                    predicate: DataPredicate::Eq {
                        value: json!("creeper"),
                    },
                },
                EventFilter::Data {
                    path: "/bbox/w".to_owned(),
                    predicate: DataPredicate::Gt { value: json!(80) },
                },
            ],
        },
        emits_kind: "creeper-imminent".to_owned(),
    }
}

fn creeper_event(seq: u64, class_label: &str, width: i64) -> Event {
    Event {
        seq,
        at: fixed_time(),
        source: EventSource::PerceptionDetection,
        kind: "entity_appeared".to_owned(),
        data: json!({
            "profile_id": "minecraft.java",
            "class_label": class_label,
            "bbox": {
                "x": 10,
                "y": 20,
                "w": width,
                "h": 60,
            },
        }),
        correlations: Vec::new(),
    }
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::UNIX_EPOCH + std::time::Duration::from_hours(494_329))
}
