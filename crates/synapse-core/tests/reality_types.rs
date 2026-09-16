use chrono::{DateTime, Utc};
use schemars::schema_for;
use synapse_core::{
    EventRef, EventSource, ForbiddenRawDataKind, RealityAudit, RealityBaseline,
    RealityBaselineStatus, RealityDelta, RealityDeltaConflict, RealityDeltaValidationError,
    RealityDriftItem, RealityDriftStatus, RealitySourceSurface, RealityTargetKind,
    RealityTargetRef, RedactionPolicy, RedactionSummary, SourceRef,
};

fn fixed_time() -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::UNIX_EPOCH + std::time::Duration::from_hours(494_304))
}

fn source_ref(surface: RealitySourceSurface, path: &str) -> SourceRef {
    SourceRef {
        surface,
        path: path.to_owned(),
        offset: Some(12),
        hash: Some("sha256:source".to_owned()),
        summary: "redacted source pointer".to_owned(),
    }
}

fn redaction() -> RedactionSummary {
    RedactionSummary {
        policy: RedactionPolicy::DefaultPrivate,
        raw_private_fields_omitted: true,
        redacted_fields: vec!["chat.raw_body".to_owned(), "log.raw_body".to_owned()],
        forbidden_raw_kinds: vec![
            ForbiddenRawDataKind::RawChatBody,
            ForbiddenRawDataKind::RawLogBody,
            ForbiddenRawDataKind::HighCardinalityPrivateData,
        ],
    }
}

fn sample_baseline() -> RealityBaseline {
    RealityBaseline {
        epoch_id: "epoch-20260529T200000Z".to_owned(),
        baseline_seq: 100,
        generated_at: fixed_time(),
        profile_id: Some("runtime.live".to_owned()),
        source_surfaces: vec![
            RealitySourceSurface::Window,
            RealitySourceSurface::GameLog,
            RealitySourceSurface::Storage,
        ],
        source_refs: vec![
            source_ref(RealitySourceSurface::Window, "hwnd:0x12ab"),
            source_ref(RealitySourceSurface::GameLog, "game-log/synthetic.log"),
        ],
        compact_state_hash: "sha256:baseline".to_owned(),
        redaction: redaction(),
        size_bytes: 512,
        size_estimate_tokens: 96,
    }
}

fn sample_delta() -> RealityDelta {
    RealityDelta {
        epoch_id: "epoch-20260529T200000Z".to_owned(),
        seq: 101,
        previous_seq: 100,
        at: fixed_time(),
        source: EventSource::PerceptionHud,
        kind: "hud-field-changed".to_owned(),
        path: "/hud/mana".to_owned(),
        target: RealityTargetRef {
            kind: RealityTargetKind::HudField,
            entity_id: None,
            field: Some("mana".to_owned()),
        },
        before: serde_json::json!(12),
        after: serde_json::json!(15),
        confidence: 0.95,
        expected_previous_hash: Some("sha256:prev".to_owned()),
        source_refs: vec![source_ref(RealitySourceSurface::Hud, "screen:hud/mana")],
        correlations: vec![EventRef {
            seq: 90,
            relation: "observed_after".to_owned(),
        }],
        conflict: None,
        redaction: redaction(),
    }
}

fn sample_audit() -> RealityAudit {
    RealityAudit {
        audit_id: "audit-1".to_owned(),
        epoch_id: "epoch-20260529T200000Z".to_owned(),
        baseline_seq: 100,
        compared_seq_start: 100,
        compared_seq_end: 103,
        ran_at: fixed_time(),
        baseline_status: RealityBaselineStatus::Current,
        assumption_hash: "sha256:assumed".to_owned(),
        actual_hash: "sha256:actual".to_owned(),
        drift_status: RealityDriftStatus::MinorDrift,
        drift_items: vec![RealityDriftItem {
            path: "/hud/mana".to_owned(),
            assumed: serde_json::json!(15),
            actual: serde_json::json!(14),
            severity: RealityDriftStatus::MinorDrift,
            source_refs: vec![source_ref(RealitySourceSurface::Hud, "screen:hud/mana")],
        }],
        physical_source_refs: vec![source_ref(RealitySourceSurface::Storage, "CF_KV/reality")],
        rebase_required: false,
        rebase_reason: None,
        follow_up_refs: vec![EventRef {
            seq: 104,
            relation: "audit_after".to_owned(),
        }],
    }
}

#[test]
fn reality_schemas_round_trip_and_use_snake_case_enums() -> Result<(), Box<dyn std::error::Error>> {
    let baseline = sample_baseline();
    let delta = sample_delta();
    let audit = sample_audit();

    println!(
        "readback=reality_schema edge=happy_path before=baseline:{}",
        serde_json::to_value(&baseline)?
    );
    assert_eq!(
        serde_json::from_str::<RealityBaseline>(&serde_json::to_string(&baseline)?)?,
        baseline
    );
    println!(
        "readback=reality_schema edge=happy_path after=baseline:{}",
        serde_json::to_value(&baseline)?
    );
    println!(
        "readback=reality_schema edge=happy_path before=delta:{}",
        serde_json::to_value(&delta)?
    );
    assert_eq!(
        serde_json::from_str::<RealityDelta>(&serde_json::to_string(&delta)?)?,
        delta
    );
    println!(
        "readback=reality_schema edge=happy_path after=delta:{}",
        serde_json::to_value(&delta)?
    );
    println!(
        "readback=reality_schema edge=happy_path before=audit:{}",
        serde_json::to_value(&audit)?
    );
    assert_eq!(
        serde_json::from_str::<RealityAudit>(&serde_json::to_string(&audit)?)?,
        audit
    );
    println!(
        "readback=reality_schema edge=happy_path after=audit:{}",
        serde_json::to_value(&audit)?
    );

    let delta_value = serde_json::to_value(&delta)?;
    assert_eq!(delta_value["source"], "perception_hud");
    assert_eq!(delta_value["target"]["kind"], "hud_field");
    assert_eq!(delta_value["redaction"]["policy"], "default_private");
    assert_eq!(
        delta_value["redaction"]["forbidden_raw_kinds"],
        serde_json::json!([
            "raw_chat_body",
            "raw_log_body",
            "high_cardinality_private_data"
        ])
    );

    Ok(())
}

#[test]
fn reality_schemas_deny_unknown_top_level_fields() -> Result<(), Box<dyn std::error::Error>> {
    let mut delta_value = serde_json::to_value(sample_delta())?;
    println!("readback=reality_schema edge=unknown_delta before={delta_value}");
    delta_value["unexpected"] = serde_json::json!(true);
    let delta_after = serde_json::from_value::<RealityDelta>(delta_value).is_err();
    println!("readback=reality_schema edge=unknown_delta after=is_err:{delta_after}");
    assert!(delta_after);

    let mut audit_value = serde_json::to_value(sample_audit())?;
    println!("readback=reality_schema edge=unknown_audit before={audit_value}");
    audit_value["unexpected"] = serde_json::json!("nope");
    let audit_after = serde_json::from_value::<RealityAudit>(audit_value).is_err();
    println!("readback=reality_schema edge=unknown_audit after=is_err:{audit_after}");
    assert!(audit_after);

    let schema = serde_json::to_value(schema_for!(RealityDelta))?;
    println!(
        "readback=reality_schema edge=json_schema after=additionalProperties:{}",
        schema["additionalProperties"]
    );
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    Ok(())
}

#[test]
fn reality_delta_validation_rejects_out_of_order_or_bad_confidence() {
    let mut delta = sample_delta();
    println!(
        "readback=reality_delta_validation edge=ordered before=seq:{} previous_seq:{} confidence:{}",
        delta.seq, delta.previous_seq, delta.confidence
    );
    let ordered_after = delta.validate_append_order();
    println!("readback=reality_delta_validation edge=ordered after={ordered_after:?}");
    assert_eq!(ordered_after, Ok(()));

    delta.seq = 100;
    delta.previous_seq = 100;
    println!(
        "readback=reality_delta_validation edge=out_of_order before=seq:{} previous_seq:{}",
        delta.seq, delta.previous_seq
    );
    let out_of_order_after = delta.validate_append_order();
    println!("readback=reality_delta_validation edge=out_of_order after={out_of_order_after:?}");
    assert_eq!(
        out_of_order_after,
        Err(RealityDeltaValidationError::OutOfOrderSeq {
            seq: 100,
            previous_seq: 100
        })
    );

    delta.seq = 101;
    delta.confidence = 1.01;
    println!(
        "readback=reality_delta_validation edge=bad_confidence before=confidence:{}",
        delta.confidence
    );
    let bad_confidence_after = delta.validate_append_order();
    println!(
        "readback=reality_delta_validation edge=bad_confidence after={bad_confidence_after:?}"
    );
    assert_eq!(
        bad_confidence_after,
        Err(RealityDeltaValidationError::ConfidenceOutOfRange { confidence: 1.01 })
    );
}

#[test]
fn reality_edge_states_are_representable() -> Result<(), Box<dyn std::error::Error>> {
    let no_op = RealityDelta {
        kind: "noop".to_owned(),
        path: "/noop".to_owned(),
        before: serde_json::json!({"stable": true}),
        after: serde_json::json!({"stable": true}),
        source_refs: Vec::new(),
        correlations: Vec::new(),
        expected_previous_hash: None,
        ..sample_delta()
    };
    println!(
        "readback=reality_edge edge=noop before=before:{} after:{}",
        no_op.before, no_op.after
    );
    let no_op_after = no_op.validate_append_order();
    println!("readback=reality_edge edge=noop after={no_op_after:?}");
    assert_eq!(no_op_after, Ok(()));

    let conflict = RealityDelta {
        conflict: Some(RealityDeltaConflict {
            expected_previous_hash: Some("sha256:expected".to_owned()),
            actual_previous_hash: Some("sha256:actual".to_owned()),
            detail: "previous HUD value differed from the delta cursor".to_owned(),
            source_refs: vec![source_ref(RealitySourceSurface::Hud, "screen:hud/mana")],
        }),
        ..sample_delta()
    };
    let conflict_after = serde_json::to_value(conflict)?["conflict"].clone();
    println!("readback=reality_edge edge=conflict after={conflict_after}");
    assert!(conflict_after.is_object());

    let stale_audit = RealityAudit {
        baseline_status: RealityBaselineStatus::Stale,
        drift_status: RealityDriftStatus::RebaseRequired,
        rebase_required: true,
        rebase_reason: Some("baseline hash no longer matches full physical audit".to_owned()),
        ..sample_audit()
    };
    println!(
        "readback=reality_edge edge=stale_baseline before=status:{}",
        serde_json::to_value(RealityBaselineStatus::Current)?
    );
    let stale_after = serde_json::to_value(stale_audit)?["baseline_status"].clone();
    println!("readback=reality_edge edge=stale_baseline after=status:{stale_after}");
    assert_eq!(stale_after, "stale");

    let source_unavailable = RealityAudit {
        baseline_status: RealityBaselineStatus::SourceUnavailable,
        drift_status: RealityDriftStatus::SourceUnavailable,
        drift_items: vec![RealityDriftItem {
            path: "/eq/log".to_owned(),
            assumed: serde_json::json!("offset:2048"),
            actual: serde_json::Value::Null,
            severity: RealityDriftStatus::SourceUnavailable,
            source_refs: vec![source_ref(
                RealitySourceSurface::GameLog,
                "game-log/missing.log",
            )],
        }],
        rebase_required: true,
        rebase_reason: Some("game log source was unavailable".to_owned()),
        ..sample_audit()
    };
    println!(
        "readback=reality_edge edge=source_unavailable before=status:{}",
        serde_json::to_value(RealityDriftStatus::InSync)?
    );
    let source_unavailable_after =
        serde_json::to_value(source_unavailable)?["drift_status"].clone();
    println!(
        "readback=reality_edge edge=source_unavailable after=status:{source_unavailable_after}"
    );
    assert_eq!(source_unavailable_after, "source_unavailable");
    Ok(())
}

#[test]
fn redaction_default_forbids_private_raw_payloads() {
    let redaction = RedactionSummary::default_private();
    assert_eq!(redaction.policy, RedactionPolicy::DefaultPrivate);
    assert!(redaction.raw_private_fields_omitted);
    assert!(
        redaction
            .forbidden_raw_kinds
            .contains(&ForbiddenRawDataKind::RawChatBody)
    );
    assert!(
        redaction
            .forbidden_raw_kinds
            .contains(&ForbiddenRawDataKind::RawLogBody)
    );
    assert!(
        redaction
            .forbidden_raw_kinds
            .contains(&ForbiddenRawDataKind::HighCardinalityPrivateData)
    );
}
