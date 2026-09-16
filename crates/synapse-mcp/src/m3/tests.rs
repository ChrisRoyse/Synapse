use super::{M3ServiceConfig, M3State, audio_event_sink, m3_tool_stubs};
use crate::http::sse::SseState;
use std::path::PathBuf;
use synapse_audio::{
    detectors::{DetectorProcessor, SharedDetectorState},
    ring::AudioFormat,
};
use synapse_core::{EventFilter, EventSource};
use synapse_reflex::DEFAULT_MAX_SUBSCRIPTIONS_NONZERO;
use tokio_util::sync::CancellationToken;

#[test]
fn m3_state_from_config_reads_cli_shape() -> anyhow::Result<()> {
    println!(
        "readback=m3_state scenario=from_config before_db=db before_profile=profiles before_reflex=true"
    );
    let state = M3State::from_config_with_shutdown_reason_and_sse_state(
        M3ServiceConfig {
            db_path: Some(PathBuf::from("db")),
            profile_dir: Some(PathBuf::from("profiles")),
            reflex_disabled: true,
            bind: "127.0.0.1:7701".to_owned(),
            bearer_token: Some("token".to_owned()),
            max_subscriptions: DEFAULT_MAX_SUBSCRIPTIONS_NONZERO,
            enable_audio: false,
            allow_unknown_profile: false,
            allowed_permissions: None,
            reflex_force_degraded: false,
            storage_pressure_free_bytes_sample: None,
        },
        CancellationToken::new(),
        "sigint",
        Some(CancellationToken::new()),
        SseState::from_env(),
    )?;
    println!(
        "readback=m3_state scenario=from_config after_db={:?} after_profile={:?} after_reflex_disabled={} after_bind={} after_token_present={} after_connection_token={}",
        state.db_path,
        state.profile_dir,
        state.reflex_disabled,
        state.bind,
        state.bearer_token.is_some(),
        state.connection_closed_cancel.is_some()
    );
    assert!(state.reflex_disabled);
    assert!(state.scaffold_ready());
    Ok(())
}

#[test]
fn m3_state_rejects_invalid_reflex_disabled() {
    let before_reflex = Some("yes");
    println!("readback=m3_state scenario=invalid_reflex before={before_reflex:?}");
    let after = M3State::from_parts_with_sse_state(
        None,
        None,
        before_reflex,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        CancellationToken::new(),
        "shutdown",
        None,
        SseState::from_env(),
    );
    println!("readback=m3_state scenario=invalid_reflex after={after:?}");
    assert!(after.is_err());
}

#[test]
fn m3_tool_stub_names_are_stable() {
    let expected = [
        "subscribe",
        "subscribe_cancel",
        "reflex_register",
        "reflex_cancel",
        "reflex_list",
        "reflex_history",
        "profile_list",
        "profile_activate",
        "profile_authoring_generate",
        "profile_authoring_list",
        "profile_authoring_inspect",
        "profile_authoring_decide",
        "profile_authoring_export",
        "routine_automate",
        "profile_quality_refresh",
        "profile_registry_query",
        "profile_registry_install",
        "profile_registry_disable",
        "profile_registry_export",
        "profile_registry_import",
        "profile_registry_rollback",
        "audit_intelligence_query",
        "audit_export_bundle",
        "replay_record",
        "demo_record_start",
        "demo_record_stop",
        "audio_tail",
        "audio_transcribe",
        "approval_request",
        "approval_list",
        "approval_decide",
        "hygiene_scan_text",
        "hygiene_scan_storage",
        "hygiene_flags",
        "local_model_register",
        "local_model_list",
        "local_model_update",
        "local_model_remove",
        "local_model_probe",
        "storage_inspect",
        "storage_put_probe_rows",
        "storage_gc_once",
        "storage_pressure_sample",
        "timeline_search",
        "timeline_purge",
        "episode_segment",
        "episode_list",
        "episode_get",
        "routine_mine",
        "routine_list",
        "routine_inspect",
        "routine_update",
        "armed_routine_tick",
        "timeline_pause",
        "timeline_resume",
        "timeline_exclusions",
        "intent_current",
        "intent_detect_tick",
    ];
    println!("readback=m3_tool_stubs before=expected:{expected:?}");
    let actual = m3_tool_stubs().map(|stub| stub.name);
    println!("readback=m3_tool_stubs after=actual:{actual:?}");
    assert_eq!(actual, expected);
}

#[test]
fn audio_event_sink_publishes_detector_events_to_shared_bus() -> anyhow::Result<()> {
    let sse_state = SseState::from_env();
    let event_bus = sse_state.event_bus();
    let subscriber = event_bus.subscribe(
        EventFilter::Source {
            source: EventSource::PerceptionAudio,
        },
        Vec::new(),
        false,
    )?;
    println!(
        "readback=audio_event_sink scenario=detector_bridge before_subscriber_len={}",
        subscriber.len()
    );

    let mut detector = DetectorProcessor::new(
        SharedDetectorState::default(),
        audio_event_sink(event_bus.clone()),
    );
    let format = AudioFormat {
        sample_rate_hz: 48_000,
        channels: 2,
    };
    detector.process(&vec![0.0; 480 * 2], format);
    detector.process(&vec![0.9; 480 * 2], format);

    let events = subscriber.drain();
    let kinds = events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    println!(
        "readback=audio_event_sink scenario=detector_bridge after_subscriber_len={} after_kinds={:?}",
        events.len(),
        kinds
    );

    assert!(
        events
            .iter()
            .all(|event| event.source == EventSource::PerceptionAudio)
    );
    assert!(kinds.contains(&"loud_transient"));
    assert!(kinds.contains(&"speech_started"));
    Ok(())
}
