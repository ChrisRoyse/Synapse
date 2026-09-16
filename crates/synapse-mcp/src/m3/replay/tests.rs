use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use rmcp::ErrorData;
use serde_json::json;
use synapse_core::{Event, EventSource, ForegroundContext, Observation, Rect, SensorStatus};
use synapse_perception::ObservationInput;
use tokio::time::sleep;

use crate::{http::sse::SseState, m1::M1State, m3::permissions::replay_root};

use super::{ReplayRecordParams, record_replay};

#[tokio::test]
async fn events_target_records_published_bus_events() -> anyhow::Result<()> {
    let path = replay_test_path("events");
    let _ = std::fs::remove_file(&path);
    let sse_state = SseState::from_env();
    let publisher = sse_state.event_bus();
    let params = ReplayRecordParams {
        target: "events".to_owned(),
        format: "jsonl".to_owned(),
        duration_ms: 250,
        path: Some(path.display().to_string()),
    };
    let m1_state = Arc::new(Mutex::new(M1State::default()));
    let event = Event {
        seq: 324_001,
        at: Utc::now(),
        source: EventSource::System,
        kind: "support.replay_record".to_owned(),
        data: json!({"known": "event-target"}),
        correlations: Vec::new(),
    };

    let (response, report) =
        tokio::join!(record_replay(m1_state, sse_state, &params), async move {
            sleep(Duration::from_millis(50)).await;
            publisher.publish(event)
        });
    let response = response.map_err(|error| anyhow::anyhow!("record_replay failed: {error:?}"))?;
    assert_eq!(report.matched, 1);
    assert_eq!(report.queued, 1);
    assert_eq!(response.records_written, 1);

    let replay_text = std::fs::read_to_string(&path)?;
    let events = replay_text
        .lines()
        .map(serde_json::from_str::<Event>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 324_001);
    assert_eq!(events[0].data["known"], "event-target");
    std::fs::remove_file(&path)?;
    Ok(())
}

#[tokio::test]
async fn observations_target_skips_transient_no_perception_and_continues() -> anyhow::Result<()> {
    let path = replay_test_path("transient-observations");
    let _ = std::fs::remove_file(&path);
    let sse_state = SseState::from_env();
    let params = ReplayRecordParams {
        target: "observations".to_owned(),
        format: "jsonl".to_owned(),
        duration_ms: 600,
        path: Some(path.display().to_string()),
    };
    let state = M1State {
        synthetic: Some(observation_input()),
        force_no_perception: true,
        ..M1State::default()
    };
    let m1_state = Arc::new(Mutex::new(state));
    let toggled = Arc::clone(&m1_state);
    let restore_perception = tokio::spawn(async move {
        sleep(Duration::from_millis(40)).await;
        toggled
            .lock()
            .map_err(|_| anyhow::anyhow!("test M1 state lock poisoned"))?
            .force_no_perception = false;
        anyhow::Ok(())
    });

    let response = record_replay(m1_state, sse_state, &params)
        .await
        .map_err(|error| anyhow::anyhow!("record_replay failed: {error:?}"))?;
    restore_perception.await??;

    assert!(response.records_written >= 1);
    assert!(response.observations_skipped >= 1);

    let replay_text = std::fs::read_to_string(&path)?;
    let observations = replay_text
        .lines()
        .map(serde_json::from_str::<Observation>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(!observations.is_empty());
    assert_eq!(observations[0].foreground.process_name, "notepad.exe");
    std::fs::remove_file(&path)?;
    Ok(())
}

#[tokio::test]
async fn observations_target_errors_when_every_sample_is_unavailable() -> anyhow::Result<()> {
    let path = replay_test_path("unavailable-observations");
    let _ = std::fs::remove_file(&path);
    let sse_state = SseState::from_env();
    let params = ReplayRecordParams {
        target: "observations".to_owned(),
        format: "jsonl".to_owned(),
        duration_ms: 60,
        path: Some(path.display().to_string()),
    };
    let state = M1State {
        synthetic: Some(observation_input()),
        force_no_perception: true,
        ..M1State::default()
    };
    let m1_state = Arc::new(Mutex::new(state));

    let error = match record_replay(m1_state, sse_state, &params).await {
        Ok(response) => {
            anyhow::bail!("sustained no-perception replay unexpectedly succeeded: {response:?}")
        }
        Err(error) => error,
    };
    assert_eq!(
        error_data_code(&error),
        Some(synapse_core::error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE)
    );
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn observation_input() -> ObservationInput {
    let mut input = ObservationInput::new(ForegroundContext {
        hwnd: 100,
        pid: 200,
        process_name: "notepad.exe".to_owned(),
        process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
        window_title: "transient.txt - Notepad".to_owned(),
        window_bounds: Rect {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    });
    input.a11y_status = SensorStatus::Healthy;
    input
}

fn error_data_code(error: &ErrorData) -> Option<&str> {
    error.data.as_ref()?.get("code")?.as_str()
}

fn replay_test_path(prefix: &str) -> PathBuf {
    replay_root().join(format!(
        "{prefix}-{}.jsonl",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ))
}
