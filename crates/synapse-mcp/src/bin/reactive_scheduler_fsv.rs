//! Manual FSV instrument for #1680's scheduled Reactive drift producer.

use std::{error::Error, path::PathBuf, sync::Arc, time::Duration};

use synapse_core::{Event, EventFilter, EventSource};
use synapse_reflex::EventBus;
use synapse_storage::{Db, cf};

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .init();
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: reactive_scheduler_fsv <new-vault-dir>")?;
    if vault_dir.exists() {
        return Err(format!(
            "SYNAPSE_FSV_PATH_EXISTS: {} already exists; remediation=pass a new disposable path",
            vault_dir.display()
        )
        .into());
    }
    let mode = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "positive".to_owned());
    let record_count = if mode == "insufficient" { 20 } else { 100 };
    let db = Arc::new(Db::open(&vault_dir, 1)?);
    synapse_storage::derived_state::register_derived_state_source(&db);

    let bus = EventBus::default();
    let subscription = bus.subscribe(
        EventFilter::All,
        vec!["calyx.reactive.drift".to_owned()],
        false,
    )?;
    if mode != "no-sink" {
        let delivery_bus = bus.clone();
        synapse_storage::derived_state::register_reactive_delivery_sink(move |finding| {
            let seq = finding
                .observed_seq
                .checked_mul(u64::from(u16::MAX) + 1)
                .and_then(|base| base.checked_add(u64::from(finding.slot)))
                .ok_or_else(|| "scheduled drift event sequence overflow".to_owned())?;
            let report = delivery_bus.publish(Event {
                seq,
                at: chrono::Utc::now(),
                source: EventSource::System,
                kind: "calyx.reactive.drift".to_owned(),
                data: serde_json::to_value(finding).map_err(|error| error.to_string())?,
                correlations: Vec::new(),
            });
            Ok(synapse_storage::derived_state::ReactiveDeliveryReadback {
                matched: report.matched as u64,
                queued: report.queued as u64,
                dropped: report.dropped,
            })
        });
    }

    println!(
        "BEFORE reactive_rows={} subscribed={} timeline_rows=0",
        db.calyx_cf_row_count("reactive")?,
        bus.subscriber_count()
    );
    for index in 0_u64..record_count {
        if index == record_count / 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ts_ns = 1_785_860_000_000_000_000_u64 + index * 1_000_000;
        let recent = index >= record_count / 2;
        let mut record = synapse_core::types::TimelineRecord::new(
            ts_ns,
            if recent {
                synapse_core::types::TimelineKind::Purge
            } else {
                synapse_core::types::TimelineKind::FocusChange
            },
            if recent {
                synapse_core::types::TimelineActor::Agent {
                    session_id: "fsv-reactive-agent".to_owned(),
                }
            } else {
                synapse_core::types::TimelineActor::Human
            },
        );
        record.app = Some(
            if recent {
                "fsv-omega-application-with-a-deliberately-long-identity"
            } else {
                "a"
            }
            .to_owned(),
        );
        record.payload = if recent {
            serde_json::json!({
                "index": index,
                "title": "scheduled reactive drift known-answer title with a long measured length",
                "url": "https://scheduled-reactive-drift.example.test/a/long/measured/path"
            })
        } else {
            serde_json::json!({"index": index})
        };
        let key = format!("fsv-reactive/{index:03}").into_bytes();
        let value = serde_json::to_vec(&record)?;
        db.put_batch(cf::CF_TIMELINE, vec![(key.clone(), value.clone())])?;
        let report = db.put_timeline_constellation(&key, &value, &record)?;
        if !report.inserted() {
            return Err(format!("timeline constellation {index} was not inserted").into());
        }
    }
    db.flush()?;
    std::thread::sleep(Duration::from_millis(5));

    let readback = synapse_storage::derived_state::run_derived_state_maintenance_once();
    let events = subscription.drain();
    println!(
        "AFTER attempts={} success={} failure={} failure_code={:?} failure_detail={:?} woven={:?} drift_rows={:?} matched={:?} queued={:?} dropped={:?} delivered_events={} reactive_rows={}",
        readback.attempts_total,
        readback.success_total,
        readback.failure_total,
        readback.last_failure_code,
        readback.last_failure_detail,
        readback.last_weave_records,
        readback.last_reactive_drift_rows,
        readback.last_reactive_notifications_matched,
        readback.last_reactive_notifications_queued,
        readback.last_reactive_notifications_dropped,
        events.len(),
        db.calyx_cf_row_count("reactive")?,
    );
    for event in events {
        println!(
            "EVENT seq={} kind={} data={}",
            event.seq,
            event.kind,
            serde_json::to_string(&event.data)?
        );
    }
    let rows_before_idle = db.calyx_cf_row_count("reactive")?;
    let idle = synapse_storage::derived_state::run_derived_state_maintenance_once();
    let idle_events = subscription.drain();
    println!(
        "IDLE mode={} reactive_before={} reactive_after={} events={} woven={:?} drift_rows={:?}",
        mode,
        rows_before_idle,
        db.calyx_cf_row_count("reactive")?,
        idle_events.len(),
        idle.last_weave_records,
        idle.last_reactive_drift_rows,
    );
    db.flush()?;
    println!("VAULT {}", vault_dir.display());
    Ok(())
}
