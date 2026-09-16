use serde_json::{Value, json};
use synapse_core::retention::{DEFAULTS, RetentionTtl};
use synapse_core::types::{TIMELINE_RECORD_VERSION, TimelineActor, TimelineKind, TimelineRecord};

/// The storage TTL compaction filter byte-scans values for the first
/// `"ts_ns"` occurrence. The envelope must therefore always serialize the
/// real timestamp before any payload content that could carry its own
/// `ts_ns` field.
#[test]
fn serialized_envelope_ts_ns_precedes_payload_imposters() {
    let mut record = TimelineRecord::new(
        1_234_567_890,
        TimelineKind::BrowserNav,
        TimelineActor::Human,
    );
    record.payload = json!({ "ts_ns": 1, "url": "https://example.test" });
    let bytes = serde_json::to_vec(&record).expect("serialize");
    let text = String::from_utf8(bytes).expect("utf8");
    let first = text.find("\"ts_ns\"").expect("ts_ns field present");
    let envelope_pos = text
        .find("\"ts_ns\":1234567890")
        .expect("envelope ts_ns serialized");
    println!(
        "regression_state=timeline_serde case=ts_ns_order first_ts_ns_at={first} envelope_ts_ns_at={envelope_pos} observed={text}"
    );
    assert_eq!(
        first, envelope_pos,
        "first ts_ns occurrence must be the envelope timestamp (TTL filter contract)"
    );
}

#[test]
fn record_roundtrips_with_agent_actor_and_kind_casing() {
    let mut record = TimelineRecord::new(
        42,
        TimelineKind::InteractionSummary,
        TimelineActor::Agent {
            session_id: "session-abc".to_owned(),
        },
    );
    record.app = Some("notepad.exe".to_owned());
    record.payload = json!({ "keystrokes": 17, "clicks": 3 });

    let encoded = serde_json::to_value(&record).expect("serialize");
    println!("regression_state=timeline_serde case=roundtrip observed={encoded}");
    assert_eq!(encoded["kind"], Value::from("interaction_summary"));
    assert_eq!(encoded["actor"]["actor"], Value::from("agent"));
    assert_eq!(encoded["actor"]["session_id"], Value::from("session-abc"));
    assert_eq!(
        encoded["record_version"],
        Value::from(TIMELINE_RECORD_VERSION)
    );

    let decoded: TimelineRecord = serde_json::from_value(encoded).expect("deserialize");
    assert_eq!(decoded, record);
}

#[test]
fn unknown_envelope_fields_and_kinds_are_decode_errors_not_silent_skips() {
    let unknown_field = serde_json::from_value::<TimelineRecord>(json!({
        "record_version": 1,
        "ts_ns": 1,
        "kind": "focus_change",
        "actor": { "actor": "human" },
        "surprise": true
    }));
    let unknown_kind = serde_json::from_value::<TimelineRecord>(json!({
        "record_version": 1,
        "ts_ns": 1,
        "kind": "keylogger_dump",
        "actor": { "actor": "human" }
    }));
    println!(
        "regression_state=timeline_serde case=reject unknown_field_err={:?} unknown_kind_err={:?}",
        unknown_field.as_ref().err().map(ToString::to_string),
        unknown_kind.as_ref().err().map(ToString::to_string)
    );
    assert!(unknown_field.is_err(), "deny_unknown_fields must hold");
    assert!(unknown_kind.is_err(), "unknown kinds must fail decode");
}

#[test]
fn retention_defaults_cover_timeline_with_90_day_ttl() {
    let default = DEFAULTS
        .iter()
        .find(|default| default.cf == "CF_TIMELINE")
        .expect("CF_TIMELINE retention default registered");
    println!(
        "regression_state=retention_defaults cf=CF_TIMELINE observed=ttl:{:?},soft:{},hard:{}",
        default.ttl, default.soft_cap_mb, default.hard_cap_mb
    );
    assert_eq!(default.ttl, RetentionTtl::Days(90));
    assert!(default.soft_cap_mb < default.hard_cap_mb);
}
