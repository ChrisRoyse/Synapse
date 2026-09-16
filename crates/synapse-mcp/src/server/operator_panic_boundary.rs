use super::{ErrorData, mcp_error};
use rmcp::model::ErrorCode;
use serde::Serialize;
use serde_json::json;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use synapse_core::error_codes;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct McpMutationActivitySnapshot {
    pub sequence: u64,
    pub in_flight: u64,
}

#[derive(Debug, Default)]
struct McpMutationActivity {
    sequence: u64,
    in_flight: u64,
}

#[derive(Debug, Default)]
struct McpMutationRequestState {
    reservation: Option<McpMutationReservation>,
    cancelled_before_mutation: Option<&'static str>,
}

fn mcp_mutation_activity() -> &'static Mutex<McpMutationActivity> {
    static ACTIVITY: OnceLock<Mutex<McpMutationActivity>> = OnceLock::new();
    ACTIVITY.get_or_init(|| Mutex::new(McpMutationActivity::default()))
}

pub(crate) fn mcp_mutation_activity_snapshot() -> Result<McpMutationActivitySnapshot, String> {
    mcp_mutation_activity()
        .lock()
        .map(|activity| McpMutationActivitySnapshot {
            sequence: activity.sequence,
            in_flight: activity.in_flight,
        })
        .map_err(|_error| "MCP mutation activity registry lock poisoned".to_owned())
}

#[derive(Debug)]
struct McpMutationReservation;

impl McpMutationReservation {
    fn acquire() -> Result<Self, ErrorData> {
        let mut activity = mcp_mutation_activity().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "MCP mutation activity registry lock poisoned while reserving physical work",
            )
        })?;
        let sequence = activity.sequence.checked_add(1).ok_or_else(|| {
            synapse_action::record_operator_panic_safety_incident();
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "MCP mutation activity sequence overflowed while reserving physical work",
            )
        })?;
        let in_flight = activity.in_flight.checked_add(1).ok_or_else(|| {
            synapse_action::record_operator_panic_safety_incident();
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "MCP mutation activity counter overflowed while reserving physical work",
            )
        })?;
        activity.sequence = sequence;
        activity.in_flight = in_flight;
        Ok(Self)
    }
}

impl Drop for McpMutationReservation {
    fn drop(&mut self) {
        let Ok(mut activity) = mcp_mutation_activity().lock() else {
            synapse_action::record_operator_panic_safety_incident();
            tracing::error!(
                code = error_codes::TOOL_INTERNAL_ERROR,
                detail_code = "MCP_MUTATION_ACTIVITY_RELEASE_LOCK_POISONED",
                "MCP mutation reservation could not publish terminal ownership"
            );
            return;
        };
        let Some(in_flight) = activity.in_flight.checked_sub(1) else {
            synapse_action::record_operator_panic_safety_incident();
            tracing::error!(
                code = error_codes::TOOL_INTERNAL_ERROR,
                detail_code = "MCP_MUTATION_ACTIVITY_UNDERFLOW",
                "MCP mutation reservation accounting underflowed"
            );
            return;
        };
        activity.in_flight = in_flight;
        let Some(sequence) = activity.sequence.checked_add(1) else {
            synapse_action::record_operator_panic_safety_incident();
            tracing::error!(
                code = error_codes::TOOL_INTERNAL_ERROR,
                detail_code = "MCP_MUTATION_ACTIVITY_SEQUENCE_OVERFLOW",
                "MCP mutation reservation terminal sequence overflowed"
            );
            return;
        };
        activity.sequence = sequence;
    }
}

/// The operator-panic epoch captured at the production MCP request boundary.
///
/// Every tool call receives a boundary, including read-only calls. That avoids
/// a fragile name classifier at the router and preserves the original epoch
/// through facades, batches, and nested service-method delegation. Only code
/// about to perform an irreversible mutation calls [`ensure_mcp_mutation`].
#[derive(Debug)]
pub(crate) struct McpOperatorPanicBoundary {
    tool: String,
    mcp_session_id: Option<String>,
    epoch_at_arm: u64,
    safety_pending_at_arm: bool,
    mutation_state: Mutex<McpMutationRequestState>,
}

/// Copyable request-boundary state used when a public MCP tool delegates to a
/// supervised authority task. Tokio task-locals do not propagate across
/// `spawn`, so the caller must reserve mutation ownership in the routed request
/// and re-scope the spawned task with the same admission epoch.
#[derive(Clone, Debug)]
pub(crate) struct McpOperatorPanicBoundarySnapshot {
    tool: String,
    mcp_session_id: Option<String>,
    epoch_at_arm: u64,
    safety_pending_at_arm: bool,
}

impl McpOperatorPanicBoundary {
    pub(crate) fn capture(tool: &str, mcp_session_id: Option<&str>) -> Self {
        let operator_panic = synapse_action::operator_panic_safety_readback();
        Self {
            tool: tool.to_owned(),
            mcp_session_id: mcp_session_id.map(ToOwned::to_owned),
            epoch_at_arm: operator_panic.epoch,
            safety_pending_at_arm: operator_panic.pending,
            mutation_state: Mutex::new(McpMutationRequestState::default()),
        }
    }

    fn snapshot_for_child_authority_task(&self) -> McpOperatorPanicBoundarySnapshot {
        McpOperatorPanicBoundarySnapshot {
            tool: self.tool.clone(),
            mcp_session_id: self.mcp_session_id.clone(),
            epoch_at_arm: self.epoch_at_arm,
            safety_pending_at_arm: self.safety_pending_at_arm,
        }
    }

    fn ensure_mutation(&self, stage: &'static str) -> Result<(), ErrorData> {
        // Publish ownership before the epoch check. If K1/K2 has already
        // started, the check fails. If it starts afterward, K2 observes this
        // reservation and cannot finalize until the whole routed request drops
        // it. This closes the check→await→post-K2 completion race.
        let mut state = self.mutation_state.lock().map_err(|_error| {
            synapse_action::record_operator_panic_safety_incident();
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "MCP request mutation reservation lock poisoned",
            )
        })?;
        if let Some(reason) = state.cancelled_before_mutation {
            return Err(ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "{} was cancelled before physical mutation admission",
                    self.tool
                ),
                Some(json!({
                    "code": error_codes::DAEMON_RESTARTING,
                    "detail_code": "MCP_ROUTED_CALL_CANCELLED_BEFORE_MUTATION",
                    "tool": self.tool,
                    "mcp_session_id": self.mcp_session_id,
                    "stage": stage,
                    "reason": reason,
                    "source_of_truth": "McpOperatorPanicBoundary::mutation_state",
                })),
            ));
        }
        let newly_reserved = state.reservation.is_none();
        if newly_reserved {
            state.reservation = Some(McpMutationReservation::acquire()?);
        }
        if let Err(error) = self.ensure(stage) {
            if newly_reserved {
                let rejected_reservation = state.reservation.take();
                drop(state);
                drop(rejected_reservation);
            }
            return Err(error);
        }
        Ok(())
    }

    /// Atomically gives caller cancellation precedence only while no physical
    /// mutation reservation has been admitted. Once a reservation exists, its
    /// exact routed child remains the owner through cleanup and terminal audit.
    fn cancel_before_mutation(&self, reason: &'static str) -> Result<bool, ErrorData> {
        let mut state = self.mutation_state.lock().map_err(|_error| {
            synapse_action::record_operator_panic_safety_incident();
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "MCP request mutation reservation lock poisoned during caller cancellation",
            )
        })?;
        if state.reservation.is_some() {
            return Ok(false);
        }
        state.cancelled_before_mutation = Some(reason);
        Ok(true)
    }

    fn ensure(&self, stage: &'static str) -> Result<(), ErrorData> {
        let readback = synapse_action::operator_panic_safety_readback();
        if !self.safety_pending_at_arm && !readback.pending && readback.epoch == self.epoch_at_arm {
            return Ok(());
        }

        tracing::warn!(
            code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
            detail_code = "MCP_MUTATION_OPERATOR_PANIC_ADMISSION_CLOSED",
            tool = %self.tool,
            mcp_session_id = ?self.mcp_session_id,
            stage,
            epoch_at_arm = self.epoch_at_arm,
            safety_pending_at_arm = self.safety_pending_at_arm,
            epoch_after = readback.epoch,
            outstanding_generations = readback.outstanding_generations,
            outstanding_finalizations = readback.outstanding_finalizations,
            publications_in_flight = readback.publications_in_flight,
            accounting_incident = readback.accounting_incident,
            "physical operator panic superseded an MCP mutation"
        );
        Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "{} was superseded by the physical operator panic control",
                self.tool
            ),
            Some(json!({
                "code": error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                "detail_code": "MCP_MUTATION_OPERATOR_PANIC_ADMISSION_CLOSED",
                "tool": self.tool,
                "mcp_session_id": self.mcp_session_id,
                "stage": stage,
                "operator_panic_epoch_at_arm": self.epoch_at_arm,
                "operator_panic_safety_pending_at_arm": self.safety_pending_at_arm,
                "operator_panic": readback,
                "source_of_truth": "synapse_action::operator_panic_safety_readback",
                "remediation": "inspect the operator-panic K1/K2 audit and physical target readback; retry only as a fresh MCP request after safety finalization",
            })),
        ))
    }

    #[cfg(test)]
    pub(crate) const fn epoch_at_arm(&self) -> u64 {
        self.epoch_at_arm
    }
}

impl McpOperatorPanicBoundarySnapshot {
    fn into_boundary(self) -> McpOperatorPanicBoundary {
        McpOperatorPanicBoundary {
            tool: self.tool,
            mcp_session_id: self.mcp_session_id,
            epoch_at_arm: self.epoch_at_arm,
            safety_pending_at_arm: self.safety_pending_at_arm,
            mutation_state: Mutex::new(McpMutationRequestState::default()),
        }
    }
}

tokio::task_local! {
    pub(crate) static MCP_OPERATOR_PANIC_BOUNDARY: McpOperatorPanicBoundary;
    pub(crate) static MCP_REQUEST_CANCELLATION: CancellationToken;
}

/// Recheck the routed-call authority supervisor cancellation token from inside
/// long physical-mutation waits. Missing task-local state is allowed for
/// direct module tests and non-MCP helper entry points; production routed calls
/// install it in `handler::call_tool` next to the operator-panic boundary.
pub(crate) fn ensure_mcp_request_not_cancelled(stage: &'static str) -> Result<(), ErrorData> {
    MCP_REQUEST_CANCELLATION
        .try_with(|cancellation| {
            if cancellation.is_cancelled() {
                Err(mcp_error(
                    error_codes::DAEMON_RESTARTING,
                    format!("MCP routed-call authority cancellation reached physical wait {stage}"),
                ))
            } else {
                Ok(())
            }
        })
        .unwrap_or(Ok(()))
}

/// Whether the current task is executing beneath the production MCP router
/// guard. Direct helper entry points use this to inherit the outer epoch when
/// they are delegated to by a facade or batch, while remaining usable in
/// explicitly unscoped module tests.
pub(crate) fn is_mcp_request_guarded() -> bool {
    MCP_OPERATOR_PANIC_BOUNDARY.try_with(|_| ()).is_ok()
}

/// Recheck the request's original operator-panic epoch at the last reversible
/// point before an externally visible mutation.
///
/// Missing task-local state is a production wiring failure and therefore
/// fails closed. Unit tests may exercise the owned boundary directly instead
/// of bypassing this invariant.
pub(crate) fn ensure_mcp_mutation(stage: &'static str) -> Result<(), ErrorData> {
    MCP_OPERATOR_PANIC_BOUNDARY
        .try_with(|boundary| boundary.ensure_mutation(stage))
        .unwrap_or_else(|_| {
            Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP mutation reached physical boundary {stage} outside its operator-panic request guard"
                ),
            ))
        })
}

/// Reserve the currently routed MCP request before transferring mutation-capable
/// work to a supervised child task, then snapshot the original admission epoch
/// so the child can re-install equivalent task-local mutation checks.
pub(crate) fn reserve_and_snapshot_current_mcp_boundary(
    stage: &'static str,
) -> Result<McpOperatorPanicBoundarySnapshot, ErrorData> {
    ensure_mcp_mutation(stage)?;
    MCP_OPERATOR_PANIC_BOUNDARY
        .try_with(McpOperatorPanicBoundary::snapshot_for_child_authority_task)
        .map_err(|_| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP mutation boundary snapshot reached {stage} outside its operator-panic request guard"
                ),
            )
        })
}

/// Run `future` beneath a cloned MCP admission boundary captured by
/// [`reserve_and_snapshot_current_mcp_boundary`]. `None` is used only for
/// routes proven not to cross a physical mutation boundary.
pub(crate) async fn scope_mcp_boundary_snapshot<F, T>(
    snapshot: Option<McpOperatorPanicBoundarySnapshot>,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    match snapshot {
        Some(snapshot) => {
            MCP_OPERATOR_PANIC_BOUNDARY
                .scope(snapshot.into_boundary(), future)
                .await
        }
        None => future.await,
    }
}

/// Attempt to cancel the current routed call before its first physical
/// mutation. `Ok(false)` means mutation ownership is already armed and the
/// routed child must continue to a terminal result. Any state-read error also
/// requires continuing rather than dropping an owner whose status is unknown.
pub(crate) fn cancel_mcp_request_before_mutation(reason: &'static str) -> Result<bool, ErrorData> {
    MCP_OPERATOR_PANIC_BOUNDARY
        .try_with(|boundary| boundary.cancel_before_mutation(reason))
        .unwrap_or_else(|_| {
            Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "MCP routed-call cancellation reached {reason} outside its operator-panic request guard"
                ),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_epoch_rejects_a_fully_finalized_later_panic_wave() {
        synapse_action::isolate_interrupt_epochs_for_test();
        let boundary = McpOperatorPanicBoundary::capture("browser_evaluate", Some("session-test"));
        let epoch_at_arm = boundary.epoch_at_arm();

        let mut token = synapse_action::request_operator_panic_interrupt();
        assert!(synapse_action::acknowledge_operator_panic_preemption(
            &mut token
        ));
        let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
            synapse_action::complete_operator_panic_safety_generation(token)
                .unwrap_or_else(|detail| panic!("complete test panic: {detail}"))
        else {
            panic!("isolated generation must own finalization");
        };
        assert!(synapse_action::finish_operator_panic_safety_finalization(
            finalization,
            true
        ));
        assert!(!synapse_action::operator_panic_safety_pending());
        assert_ne!(synapse_action::operator_panic_epoch(), epoch_at_arm);

        let error = boundary
            .ensure("before_runtime_evaluate")
            .expect_err("a completed intervening panic must supersede the old request");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::SAFETY_OPERATOR_HOTKEY_FIRED))
        );
    }

    #[test]
    fn mutation_without_request_boundary_fails_closed() {
        let error = ensure_mcp_mutation("before_external_write")
            .expect_err("missing production request guard must fail closed");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::TOOL_INTERNAL_ERROR))
        );
    }

    #[test]
    fn mutation_captured_while_pending_stays_closed_after_finalization() {
        synapse_action::isolate_interrupt_epochs_for_test();
        let mut token = synapse_action::request_operator_panic_interrupt();
        let boundary = McpOperatorPanicBoundary::capture("browser_evaluate", Some("session-test"));
        assert!(boundary.safety_pending_at_arm);

        assert!(synapse_action::acknowledge_operator_panic_preemption(
            &mut token
        ));
        let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
            synapse_action::complete_operator_panic_safety_generation(token)
                .unwrap_or_else(|detail| panic!("complete test panic: {detail}"))
        else {
            panic!("isolated generation must own finalization");
        };
        assert!(synapse_action::finish_operator_panic_safety_finalization(
            finalization,
            true
        ));
        assert!(!synapse_action::operator_panic_safety_pending());
        assert_eq!(
            synapse_action::operator_panic_epoch(),
            boundary.epoch_at_arm()
        );

        boundary
            .ensure("before_runtime_evaluate")
            .expect_err("a mutation request admitted during K1/K2 must remain superseded");
    }
}
