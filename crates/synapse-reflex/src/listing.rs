use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use synapse_core::{ReflexId, ReflexLifetime, ReflexState, ReflexStatus, StoredReflexAudit};
use synapse_storage::{cf, decode_json};

use crate::{
    DEFAULT_REFLEX_PRIORITY, REFLEX_FIRED_KIND, REFLEX_REGISTERED_KIND, ReflexError, ReflexResult,
    ReflexRuntime,
};

impl ReflexRuntime {
    /// Lists reflex statuses visible to MCP callers.
    ///
    /// By default, terminal cancelled/expired statuses are hidden. When
    /// `include_expired` is set, terminal rows from `CF_REFLEX_AUDIT` are
    /// merged back into the current runtime snapshot so cancelled/expired
    /// reflexes remain inspectable after a daemon restart.
    ///
    /// # Errors
    ///
    /// Returns a [`ReflexError`] when the audit column family cannot be scanned
    /// or an audit row cannot be decoded.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime", include_expired))]
    pub fn list(&self, include_expired: bool) -> ReflexResult<Vec<ReflexStatus>> {
        let mut statuses = self
            .statuses()
            .into_iter()
            .filter(|status| include_expired || is_non_terminal(status.state))
            .collect::<Vec<_>>();

        if include_expired {
            let existing = statuses
                .iter()
                .map(|status| status.id.clone())
                .collect::<HashSet<_>>();
            statuses.extend(
                self.terminal_statuses_from_audit()?
                    .into_iter()
                    .filter(|status| !existing.contains(&status.id)),
            );
        }

        Ok(statuses)
    }

    /// Returns persisted reflex audit rows in newest-first order.
    ///
    /// When `reflex_id` is present, the audit column family is read by the
    /// reflex audit key prefix. Without a `reflex_id`, rows are sorted globally
    /// by persisted timestamp and audit id before the limit is applied.
    ///
    /// # Errors
    ///
    /// Returns a [`ReflexError`] when the audit column family cannot be scanned
    /// or an audit row cannot be decoded.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime", reflex_id, limit))]
    pub fn history(
        &self,
        reflex_id: Option<&str>,
        limit: usize,
    ) -> ReflexResult<Vec<StoredReflexAudit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.db
            .flush()
            .map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("reflex audit flush before scan failed: {error}"),
            })?;

        let rows = reflex_id
            .map_or_else(
                || self.db.scan_cf(cf::CF_REFLEX_AUDIT),
                |reflex_id| {
                    self.db
                        .scan_cf_prefix(cf::CF_REFLEX_AUDIT, audit_key_prefix(reflex_id).as_bytes())
                },
            )
            .map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("reflex audit scan failed: {error}"),
            })?;

        let mut audits = rows
            .into_iter()
            .map(|(_key, value)| {
                decode_json::<StoredReflexAudit>(&value).map_err(|error| {
                    ReflexError::ParamsInvalid {
                        detail: format!("reflex audit decode failed: {error}"),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        audits.sort_by(|left, right| {
            right
                .ts_ns
                .cmp(&left.ts_ns)
                .then_with(|| right.audit_id.cmp(&left.audit_id))
                .then_with(|| right.reflex_id.cmp(&left.reflex_id))
        });
        audits.truncate(limit);
        Ok(audits)
    }

    pub(crate) fn terminal_statuses_from_audit(&self) -> ReflexResult<Vec<ReflexStatus>> {
        let rows =
            self.db
                .scan_cf(cf::CF_REFLEX_AUDIT)
                .map_err(|error| ReflexError::ParamsInvalid {
                    detail: format!("reflex audit scan failed: {error}"),
                })?;
        let mut audits = rows
            .into_iter()
            .map(|(_key, value)| {
                decode_json::<StoredReflexAudit>(&value).map_err(|error| {
                    ReflexError::ParamsInvalid {
                        detail: format!("reflex audit decode failed: {error}"),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        audits.sort_by_key(|audit| (audit.reflex_id.clone(), audit.ts_ns, audit.audit_id.clone()));

        let mut accumulators = BTreeMap::<String, AuditStatusAccumulator>::new();
        for audit in audits {
            accumulators
                .entry(audit.reflex_id.clone())
                .or_insert_with(|| AuditStatusAccumulator::new(audit.reflex_id.clone()))
                .record(audit);
        }

        Ok(accumulators
            .into_values()
            .filter_map(AuditStatusAccumulator::into_terminal_status)
            .collect())
    }

    pub(crate) fn terminal_status_from_audit(
        &self,
        reflex_id: &str,
    ) -> ReflexResult<Option<ReflexStatus>> {
        let rows = self
            .db
            .scan_cf_prefix(cf::CF_REFLEX_AUDIT, audit_key_prefix(reflex_id).as_bytes())
            .map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("reflex audit scan failed: {error}"),
            })?;
        let mut audits = rows
            .into_iter()
            .map(|(_key, value)| {
                decode_json::<StoredReflexAudit>(&value).map_err(|error| {
                    ReflexError::ParamsInvalid {
                        detail: format!("reflex audit decode failed: {error}"),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        audits.sort_by_key(|audit| (audit.ts_ns, audit.audit_id.clone()));

        let mut accumulator = AuditStatusAccumulator::new(reflex_id.to_owned());
        for audit in audits {
            accumulator.record(audit);
        }
        Ok(accumulator.into_terminal_status())
    }
}

const fn is_non_terminal(state: ReflexState) -> bool {
    !matches!(
        state,
        ReflexState::ActionDenied | ReflexState::Cancelled | ReflexState::Expired
    )
}

#[derive(Clone, Debug)]
struct AuditStatusAccumulator {
    reflex_id: ReflexId,
    registered_at: Option<DateTime<Utc>>,
    kind_summary: Option<String>,
    priority: Option<u32>,
    lifetime: Option<ReflexLifetime>,
    exclusive: Option<bool>,
    last_fired_at: Option<DateTime<Utc>>,
    fire_count: u64,
    terminal: Option<StoredReflexAudit>,
}

impl AuditStatusAccumulator {
    const fn new(reflex_id: String) -> Self {
        Self {
            reflex_id,
            registered_at: None,
            kind_summary: None,
            priority: None,
            lifetime: None,
            exclusive: None,
            last_fired_at: None,
            fire_count: 0,
            terminal: None,
        }
    }

    fn record(&mut self, audit: StoredReflexAudit) {
        let at = datetime_from_ts_ns(audit.ts_ns);
        let details_kind = audit
            .details
            .get("kind")
            .and_then(serde_json::Value::as_str);

        if details_kind == Some(REFLEX_REGISTERED_KIND) {
            self.registered_at = Some(at);
            self.update_common_fields(&audit);
        } else if details_kind == Some(REFLEX_FIRED_KIND) {
            self.last_fired_at = Some(at);
            self.fire_count = self.fire_count.saturating_add(1);
        }

        if matches!(
            audit.status,
            ReflexState::ActionDenied | ReflexState::Cancelled | ReflexState::Expired
        ) {
            self.update_common_fields(&audit);
            self.terminal = Some(audit);
        }
    }

    fn update_common_fields(&mut self, audit: &StoredReflexAudit) {
        let details = &audit.details;
        if let Some(kind_summary) = details
            .get("kind_summary")
            .and_then(serde_json::Value::as_str)
        {
            self.kind_summary = Some(kind_summary.to_owned());
        }
        if let Some(priority) = details
            .get("priority")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            self.priority = Some(priority);
        }
        if let Some(lifetime) = details
            .get("lifetime")
            .cloned()
            .and_then(|value| serde_json::from_value::<ReflexLifetime>(value).ok())
        {
            self.lifetime = Some(lifetime);
        }
        if let Some(exclusive) = details
            .get("exclusive")
            .and_then(serde_json::Value::as_bool)
        {
            self.exclusive = Some(exclusive);
        }
    }

    fn into_terminal_status(self) -> Option<ReflexStatus> {
        let terminal = self.terminal?;
        let terminal_at = datetime_from_ts_ns(terminal.ts_ns);
        Some(ReflexStatus {
            id: self.reflex_id,
            kind_summary: self.kind_summary.unwrap_or_else(|| "unknown".to_owned()),
            state: terminal.status,
            registered_at: self.registered_at.unwrap_or(terminal_at),
            last_fired_at: self.last_fired_at,
            fire_count: self.fire_count,
            priority: self.priority.unwrap_or(DEFAULT_REFLEX_PRIORITY),
            lifetime: self.lifetime.unwrap_or_default(),
            exclusive: self.exclusive.unwrap_or(false),
            last_error_code: terminal.error_code,
        })
    }
}

fn datetime_from_ts_ns(ts_ns: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_nanos(ts_ns))
}

fn audit_key_prefix(reflex_id: &str) -> String {
    format!("{reflex_id}:")
}
