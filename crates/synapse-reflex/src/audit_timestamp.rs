use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicU64, Ordering};
use synapse_core::error_codes;

use crate::{ReflexError, ReflexResult};

const REFLEX_AUDIT_TIMESTAMP_INVALID_METRIC: &str = "reflex_audit_timestamp_invalid_total";
static REFLEX_AUDIT_TIMESTAMP_INVALID_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn invalid_total() -> u64 {
    REFLEX_AUDIT_TIMESTAMP_INVALID_TOTAL.load(Ordering::Acquire)
}

pub fn unix_ns(timestamp: &DateTime<Utc>, event_kind: &'static str) -> ReflexResult<u64> {
    let signed_ns = timestamp.timestamp_nanos_opt().ok_or_else(|| {
        invalid_timestamp(
            timestamp,
            event_kind,
            "Chrono DateTime is outside the i64 nanosecond timestamp range",
        )
    })?;
    u64::try_from(signed_ns).map_err(|_error| {
        invalid_timestamp(
            timestamp,
            event_kind,
            "timestamp predates the unsigned Unix epoch supported by StoredReflexAudit.ts_ns",
        )
    })
}

pub fn now_unix_ns(event_kind: &'static str) -> ReflexResult<u64> {
    unix_ns(&Utc::now(), event_kind)
}

/// Checked timestamp conversion for scheduler/offload paths that cannot return
/// an audit-construction error to their caller. Failure is observable and the
/// caller omits the invalid audit instead of publishing a fabricated epoch row.
pub fn try_unix_ns(timestamp: &DateTime<Utc>, event_kind: &'static str) -> Option<u64> {
    match unix_ns(timestamp, event_kind) {
        Ok(value) => Some(value),
        Err(error) => {
            REFLEX_AUDIT_TIMESTAMP_INVALID_TOTAL.fetch_add(1, Ordering::AcqRel);
            metrics::counter!(
                REFLEX_AUDIT_TIMESTAMP_INVALID_METRIC,
                "event_kind" => event_kind
            )
            .increment(1);
            tracing::error!(
                code = error_codes::REFLEX_AUDIT_TIMESTAMP_INVALID,
                component = "reflex_audit_timestamp",
                event_kind,
                detail = %error,
                "refused to construct a reflex audit with an invalid timestamp"
            );
            None
        }
    }
}

pub fn try_now_unix_ns(event_kind: &'static str) -> Option<u64> {
    try_unix_ns(&Utc::now(), event_kind)
}

fn invalid_timestamp(
    timestamp: &DateTime<Utc>,
    event_kind: &'static str,
    detail: &'static str,
) -> ReflexError {
    ReflexError::AuditTimestampInvalid {
        event_kind,
        timestamp: format!(
            "unix_seconds={} subsec_nanos={}",
            timestamp.timestamp(),
            timestamp.timestamp_subsec_nanos()
        ),
        detail: format!(
            "{detail}; remediation=preserve the triggering event and repair the system clock/timestamp source before retrying; no audit row was written"
        ),
    }
}
