use std::{error::Error, sync::Arc};

use serde_json::json;
use synapse_action::ActionHandle;
use synapse_core::{
    Action, EventFilter, ReflexLifetime, ReflexState, StoredReflexAudit, error_codes,
};
use synapse_storage::{Db, cf, decode_json};
use tempfile::tempdir;
use tokio::sync::mpsc;

use crate::{
    EventBus, REFLEX_CANCELLED_KIND, REFLEX_DISABLED_KIND, REFLEX_LIFETIME_EXPIRED_KIND,
    REFLEX_REGISTERED_KIND, ReflexCancelOutcome, ReflexRuntime, ScheduledReflex, write_audit,
};

const TEST_SCHEMA_VERSION: u32 = 7;

#[test]
fn spawn_retains_runtime_inputs_and_action_handle() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let (action_handle, mut action_rx) = ActionHandle::channel();
    assert!(matches!(
        action_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    let runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    runtime.action_handle().try_execute(Action::ReleaseAll)?;
    let (queued_action, _ack, _operator_panic_epoch_at_enqueue) = action_rx.try_recv()?;

    assert_eq!(runtime.schema_version(), TEST_SCHEMA_VERSION);
    assert_eq!(queued_action, Action::ReleaseAll);
    Ok(())
}

#[test]
fn cancel_registered_reflex_marks_status_and_writes_audit() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let (action_handle, _action_rx) = ActionHandle::channel();
    let mut runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    let reflex = ScheduledReflex::on_event(
        "reflex-runtime-cancel",
        EventFilter::Kind {
            kind: "support-cancel".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    let registered = runtime.register(&reflex)?;
    assert_eq!(registered.state, ReflexState::Active);

    let outcome = runtime.cancel("reflex-runtime-cancel")?;
    let ReflexCancelOutcome::Cancelled { status } = outcome else {
        panic!("registered reflex should cancel");
    };
    assert_eq!(status.state, ReflexState::Cancelled);
    assert_eq!(
        runtime
            .statuses()
            .into_iter()
            .find(|status| status.id == "reflex-runtime-cancel")
            .map(|status| status.state),
        Some(ReflexState::Cancelled)
    );
    assert!(runtime.list(false)?.is_empty());
    let visible_with_expired = runtime.list(true)?;
    assert_eq!(visible_with_expired.len(), 1);
    assert_eq!(visible_with_expired[0].state, ReflexState::Cancelled);

    let audits = db
        .scan_cf(cf::CF_REFLEX_AUDIT)?
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    let kinds = audits
        .iter()
        .map(|audit| audit.details["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&Some(REFLEX_REGISTERED_KIND)));
    assert!(kinds.contains(&Some(REFLEX_CANCELLED_KIND)));
    assert!(
        audits
            .iter()
            .any(|audit| audit.status == ReflexState::Cancelled)
    );
    drop(runtime);

    let (action_handle, _action_rx) = ActionHandle::channel();
    let restarted = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    let restored = restarted.list(true)?;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, "reflex-runtime-cancel");
    assert_eq!(restored[0].state, ReflexState::Cancelled);
    assert_eq!(restored[0].kind_summary, "on_event:1 actions");
    Ok(())
}

#[test]
fn cancel_expired_reflex_restored_from_audit_reports_already_expired() -> Result<(), Box<dyn Error>>
{
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let reflex_id = "reflex-runtime-expired-from-audit";
    write_audit(
        &db,
        &stored_reflex_audit(
            reflex_id,
            "audit-registered",
            100,
            ReflexState::Active,
            REFLEX_REGISTERED_KIND,
        ),
    )?;
    write_audit(
        &db,
        &stored_reflex_audit(
            reflex_id,
            "audit-expired",
            200,
            ReflexState::Expired,
            REFLEX_LIFETIME_EXPIRED_KIND,
        ),
    )?;
    db.flush()?;

    let (action_handle, _action_rx) = ActionHandle::channel();
    let mut runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    assert!(runtime.statuses().is_empty());
    assert_eq!(
        runtime
            .list(true)?
            .into_iter()
            .find(|status| status.id == reflex_id)
            .map(|status| status.state),
        Some(ReflexState::Expired)
    );

    let outcome = runtime.cancel(reflex_id)?;
    let ReflexCancelOutcome::AlreadyExpired { status } = outcome else {
        panic!("expired audit-only reflex should report already expired");
    };
    assert_eq!(status.id, reflex_id);
    assert_eq!(status.state, ReflexState::Expired);

    let audits = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
    assert_eq!(audits.len(), 2);
    Ok(())
}

#[test]
fn cancelled_reflex_does_not_resurrect_on_later_registration() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let (action_handle, _action_rx) = ActionHandle::channel();
    let mut runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    let cancelled_reflex = ScheduledReflex::on_event(
        "reflex-runtime-cancelled-first",
        EventFilter::Kind {
            kind: "support-cancelled-first".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    let active_reflex = ScheduledReflex::on_event(
        "reflex-runtime-active-second",
        EventFilter::Kind {
            kind: "support-active-second".to_owned(),
        },
        vec![Action::ReleaseAll],
    );

    runtime.register(&cancelled_reflex)?;
    assert!(matches!(
        runtime.cancel("reflex-runtime-cancelled-first")?,
        ReflexCancelOutcome::Cancelled { .. }
    ));
    runtime.register(&active_reflex)?;

    let active_statuses = runtime.list(false)?;
    assert_eq!(active_statuses.len(), 1);
    assert_eq!(active_statuses[0].id, "reflex-runtime-active-second");
    assert_eq!(active_statuses[0].state, ReflexState::Active);

    let visible_with_expired = runtime.list(true)?;
    assert!(visible_with_expired.iter().any(|status| {
        status.id == "reflex-runtime-cancelled-first" && status.state == ReflexState::Cancelled
    }));
    assert!(visible_with_expired.iter().any(|status| {
        status.id == "reflex-runtime-active-second" && status.state == ReflexState::Active
    }));
    assert_eq!(
        runtime
            .statuses()
            .into_iter()
            .find(|status| status.id == "reflex-runtime-cancelled-first")
            .map(|status| status.state),
        None
    );
    Ok(())
}

fn stored_reflex_audit(
    reflex_id: &str,
    audit_id: &str,
    ts_ns: u64,
    status: ReflexState,
    kind: &str,
) -> StoredReflexAudit {
    StoredReflexAudit {
        schema_version: TEST_SCHEMA_VERSION,
        audit_id: audit_id.to_owned(),
        reflex_id: reflex_id.to_owned(),
        ts_ns,
        status,
        event_id: None,
        audit_context: None,
        steps: Vec::new(),
        error_code: None,
        details: json!({
            "kind": kind,
            "kind_summary": "combo:1 steps",
            "priority": 100,
            "lifetime": ReflexLifetime::OneShot,
            "exclusive": false,
        }),
        redacted: false,
        redactions: Vec::new(),
    }
}

#[test]
fn duplicate_active_reflex_definition_is_rejected() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let (action_handle, _action_rx) = ActionHandle::channel();
    let mut runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    let first = ScheduledReflex::on_event(
        "reflex-runtime-duplicate-a",
        EventFilter::Kind {
            kind: "support-duplicate".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    let duplicate = ScheduledReflex::on_event(
        "reflex-runtime-duplicate-b",
        EventFilter::Kind {
            kind: "support-duplicate".to_owned(),
        },
        vec![Action::ReleaseAll],
    );

    runtime.register(&first)?;
    let Err(error) = runtime.register(&duplicate) else {
        panic!("duplicate active reflex definition must be rejected");
    };

    assert_eq!(error.code(), error_codes::REFLEX_PARAMS_INVALID);
    assert_eq!(runtime.list(false)?.len(), 1);
    let audits = db
        .scan_cf(cf::CF_REFLEX_AUDIT)?
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        audits
            .iter()
            .filter(|audit| audit.details["kind"].as_str() == Some(REFLEX_REGISTERED_KIND))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn repeated_on_event_registrations_do_not_leak_scheduler_subscribers() -> Result<(), Box<dyn Error>>
{
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let (action_handle, _action_rx) = ActionHandle::channel();
    let bus = EventBus::default();
    let mut runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, bus.clone())?;

    for index in 0..32 {
        let reflex = ScheduledReflex::on_event(
            format!("reflex-runtime-many-{index}"),
            EventFilter::Kind {
                kind: format!("support-many-{index}"),
            },
            vec![Action::ReleaseAll],
        );
        runtime.register(&reflex)?;
        assert_eq!(
            bus.subscriber_count(),
            1,
            "scheduler restart should leave exactly one live internal subscriber"
        );
    }

    assert_eq!(runtime.list(false)?.len(), 32);
    Ok(())
}

#[test]
fn disable_all_by_operator_marks_statuses_and_writes_audit() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), TEST_SCHEMA_VERSION)?);
    let (action_handle, _action_rx) = ActionHandle::channel();
    let mut runtime = ReflexRuntime::spawn(Arc::clone(&db), action_handle, EventBus::default())?;
    let first = ScheduledReflex::on_event(
        "reflex-runtime-disable-a",
        EventFilter::Kind {
            kind: "support-disable-a".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    let second = ScheduledReflex::on_event(
        "reflex-runtime-disable-b",
        EventFilter::Kind {
            kind: "support-disable-b".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    runtime.register(&first)?;
    runtime.register(&second)?;

    let disabled = runtime.disable_all_by_operator()?;
    assert_eq!(disabled.len(), 2);
    assert!(
        disabled
            .iter()
            .all(|status| status.state == ReflexState::Disabled)
    );
    assert!(
        runtime
            .list(false)?
            .iter()
            .all(|status| status.state == ReflexState::Disabled)
    );
    assert!(runtime.disable_all_by_operator()?.is_empty());

    let audits = db
        .scan_cf(cf::CF_REFLEX_AUDIT)?
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    let disabled_audits = audits
        .iter()
        .filter(|audit| audit.details["kind"].as_str() == Some(REFLEX_DISABLED_KIND))
        .collect::<Vec<_>>();
    assert_eq!(disabled_audits.len(), 2);
    assert!(disabled_audits.iter().all(|audit| {
        audit.status == ReflexState::Disabled
            && audit.error_code.as_deref()
                == Some(synapse_core::error_codes::REFLEX_DISABLED_BY_OPERATOR)
    }));
    Ok(())
}
