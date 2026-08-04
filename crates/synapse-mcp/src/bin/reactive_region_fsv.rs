//! Manual FSV instrument for #1680 exact app first-use new-region delivery.

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
        .ok_or("usage: reactive_region_fsv <new-vault-dir>")?;
    let mode = std::env::args().nth(2);
    if mode.as_deref() == Some("inspect") {
        let vault = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_reactive_only(
            synapse_calyx::SynapseCalyxConfig::from_vault_dir(vault_dir.clone()),
        )?;
        let rows = vault.scan_reactive_latest()?;
        println!("READ_ONLY reactive_rows={}", rows.len());
        for (key, value) in rows {
            println!(
                "READ_ONLY_ROW key_hex={} value={}",
                hex(&key),
                String::from_utf8(value)?
            );
        }
        return Ok(());
    }
    if vault_dir.exists() && mode.as_deref() != Some("resume") {
        return Err(format!(
            "SYNAPSE_FSV_PATH_EXISTS: {} already exists; remediation=pass a new disposable path",
            vault_dir.display()
        )
        .into());
    }
    let db = Arc::new(Db::open(&vault_dir, 1)?);
    synapse_storage::derived_state::register_derived_state_source(&db);
    let bus = EventBus::default();
    let delivery_bus = bus.clone();
    synapse_storage::derived_state::register_region_delivery_sink(move |finding| {
        let report = delivery_bus.publish(Event {
            seq: finding.observed_seq,
            at: chrono::Utc::now(),
            source: EventSource::System,
            kind: "calyx.reactive.new_region".to_owned(),
            data: serde_json::to_value(finding).map_err(|error| error.to_string())?,
            correlations: Vec::new(),
        });
        Ok(synapse_storage::derived_state::ReactiveDeliveryReadback {
            matched: report.matched as u64,
            queued: report.queued as u64,
            dropped: report.dropped,
        })
    });

    if mode.as_deref() == Some("resume") {
        let subscription = bus.subscribe(
            EventFilter::All,
            vec!["calyx.reactive.new_region".to_owned()],
            false,
        )?;
        let cursor_before = db.region_delivery_cursor()?;
        let readback = synapse_storage::derived_state::run_derived_state_maintenance_once();
        let events = subscription.drain();
        println!(
            "RESUME reactive_rows={} cursor_before={} cursor_after={} events={} matched={} queued={} dropped={}",
            db.calyx_cf_row_count("reactive")?,
            cursor_before,
            db.region_delivery_cursor()?,
            events.len(),
            readback.last_region_notifications_matched,
            readback.last_region_notifications_queued,
            readback.last_region_notifications_dropped,
        );
        return Ok(());
    }

    println!(
        "BEFORE reactive_rows={}",
        db.calyx_cf_row_count("reactive")?
    );
    put_timeline(&db, 0, Some("fsv-first-app"))?;
    let after_first_write = db.calyx_cf_row_count("reactive")?;
    let no_listener = synapse_storage::derived_state::run_derived_state_maintenance_once();
    println!(
        "NO_LISTENER reactive_before=0 reactive_after={} matched={} queued={} watermark={}",
        after_first_write,
        no_listener.last_region_notifications_matched,
        no_listener.last_region_notifications_queued,
        no_listener.last_region_delivery_watermark,
    );

    let subscription = bus.subscribe(
        EventFilter::All,
        vec!["calyx.reactive.new_region".to_owned()],
        false,
    )?;
    let replay = synapse_storage::derived_state::run_derived_state_maintenance_once();
    print_events("REPLAY", &subscription.drain())?;
    println!(
        "REPLAY_STATE reactive_rows={} matched={} queued={} dropped={} watermark={}",
        db.calyx_cf_row_count("reactive")?,
        replay.last_region_notifications_matched,
        replay.last_region_notifications_queued,
        replay.last_region_notifications_dropped,
        replay.last_region_delivery_watermark,
    );

    let before_boundaries = db.calyx_cf_row_count("reactive")?;
    put_timeline(&db, 1, Some("fsv-first-app"))?;
    let after_repeat = db.calyx_cf_row_count("reactive")?;
    put_timeline(&db, 2, None)?;
    let after_missing = db.calyx_cf_row_count("reactive")?;
    put_timeline(&db, 3, Some("fsv-second-app"))?;
    let after_second = db.calyx_cf_row_count("reactive")?;
    let second = synapse_storage::derived_state::run_derived_state_maintenance_once();
    let second_events = subscription.drain();
    print_events("SECOND", &second_events)?;
    println!(
        "BOUNDARIES before={} repeat_after={} missing_after={} second_after={} delivered={} matched={} queued={} dropped={} watermark={}",
        before_boundaries,
        after_repeat,
        after_missing,
        after_second,
        second_events.len(),
        second.last_region_notifications_matched,
        second.last_region_notifications_queued,
        second.last_region_notifications_dropped,
        second.last_region_delivery_watermark,
    );
    db.flush()?;
    let rows = db.persisted_region_findings(0, 10)?;
    for row in rows {
        println!("ROW {}", serde_json::to_string(&row)?);
    }
    for app in ["fsv-first-app", "fsv-second-app"] {
        let series = db.read_recurrence_subject_series(
            synapse_storage::RecurrenceSubjectKind::AppUsage,
            app,
        )?;
        println!(
            "SERIES app={} frequency={} active_occurrences={} latest_seq={}",
            app,
            series.series.series.frequency,
            series.series.series.occurrences.len(),
            series.latest_seq,
        );
    }
    println!("VAULT {}", vault_dir.display());
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn put_timeline(db: &Db, index: u64, app: Option<&str>) -> Result<(), Box<dyn Error>> {
    let ts_ns = 1_785_861_000_000_000_000_u64 + index * 1_000_000;
    let mut record = synapse_core::types::TimelineRecord::new(
        ts_ns,
        synapse_core::types::TimelineKind::FocusChange,
        synapse_core::types::TimelineActor::Human,
    );
    record.app = app.map(str::to_owned);
    record.payload = serde_json::json!({"title": format!("region-fsv-{index}")});
    let key = format!("fsv-region/{index:03}").into_bytes();
    let value = serde_json::to_vec(&record)?;
    db.put_batch(cf::CF_TIMELINE, vec![(key.clone(), value.clone())])?;
    let report = db.put_timeline_constellation(&key, &value, &record)?;
    if !report.inserted() {
        return Err(format!("timeline constellation {index} was not inserted").into());
    }
    db.flush()?;
    std::thread::sleep(Duration::from_millis(2));
    Ok(())
}

fn print_events(label: &str, events: &[Event]) -> Result<(), Box<dyn Error>> {
    for event in events {
        println!(
            "{label}_EVENT seq={} kind={} data={}",
            event.seq,
            event.kind,
            serde_json::to_string(&event.data)?
        );
    }
    Ok(())
}
