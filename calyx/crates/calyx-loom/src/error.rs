//! Loom-local fail-closed error helpers.

use calyx_core::CalyxError;

pub const CALYX_LOOM_ZERO_NORM_VECTOR: &str = "CALYX_LOOM_ZERO_NORM_VECTOR";
pub const CALYX_LOOM_DIM_MISMATCH: &str = "CALYX_LOOM_DIM_MISMATCH";
pub const CALYX_LOOM_NON_FINITE_VECTOR: &str = "CALYX_LOOM_NON_FINITE_VECTOR";
pub const CALYX_LOOM_SLOT_MISSING: &str = "CALYX_LOOM_SLOT_MISSING";
pub const CALYX_LOOM_UNCALIBRATED_BLINDSPOT: &str = "CALYX_LOOM_UNCALIBRATED_BLINDSPOT";
pub const CALYX_LOOM_FORGE_UNAVAILABLE: &str = "CALYX_LOOM_FORGE_UNAVAILABLE";
pub const CALYX_LOOM_SERIES_READ_ERROR: &str = "CALYX_LOOM_SERIES_READ_ERROR";
pub const CALYX_LOOM_TEMPORAL_XTERM_CORRUPT: &str = "CALYX_LOOM_TEMPORAL_XTERM_CORRUPT";
pub const CALYX_LOOM_PANEL_SCOPE_REQUIRED: &str = "CALYX_LOOM_PANEL_SCOPE_REQUIRED";
pub const CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED: &str = "CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED";
pub const CALYX_RECURRENCE_CONTEXT_TOO_LARGE: &str = "CALYX_RECURRENCE_CONTEXT_TOO_LARGE";
pub const CALYX_RECURRENCE_INVALID_RETENTION: &str = "CALYX_RECURRENCE_INVALID_RETENTION";
/// The reactive trigger registry is at `max_triggers`; no new trigger admitted.
pub const CALYX_REACTIVE_REGISTRY_FULL: &str = "CALYX_REACTIVE_REGISTRY_FULL";
/// The reactive fired-event queue is at `max_queue_depth`; the oldest undelivered
/// event was discarded to make room (bounded by construction, A26).
pub const CALYX_REACTIVE_QUEUE_FULL: &str = "CALYX_REACTIVE_QUEUE_FULL";
/// A per-subscription drain buffer overflowed; retained events are still
/// available through the subscription report API.
pub const CALYX_REACTIVE_DRAIN_OVERFLOW: &str = "CALYX_REACTIVE_DRAIN_OVERFLOW";
/// The requested public subscription id is not registered.
pub const CALYX_REACTIVE_SUBSCRIPTION_NOT_FOUND: &str = "CALYX_REACTIVE_SUBSCRIPTION_NOT_FOUND";
/// A signal source cannot evaluate the requested trigger condition (e.g. a
/// recurrence-only source asked for a novelty/drift verdict). Fail closed rather
/// than silently treat the condition as not-firing.
pub const CALYX_REACTIVE_SIGNAL_UNAVAILABLE: &str = "CALYX_REACTIVE_SIGNAL_UNAVAILABLE";
/// A durable reactive CF row could not be encoded/decoded or its key was not
/// one of the canonical trigger audit/fired shapes.
pub const CALYX_REACTIVE_ROW_CORRUPT: &str = "CALYX_REACTIVE_ROW_CORRUPT";

pub fn loom_error(code: &'static str, message: impl Into<String>) -> CalyxError {
    let remediation = match code {
        // #2076: the old text — "supply non-zero slot vectors before weaving
        // agreements" — was addressed to a caller who has no such control. The
        // unattended weave runs over whatever the corpus holds, and a record
        // whose every measurable lens is exactly zero has no non-zero vector
        // for anyone to supply. Zero is frequently the *correct* measurement,
        // so the remediation now names the two things that are actually
        // actionable: exclude the directionless slot from the cosine lane
        // (which the bulk weave now does, with a counted skip), or re-measure
        // if zero is not what the source row says.
        CALYX_LOOM_ZERO_NORM_VECTOR => {
            "a zero-norm vector has no direction, so no cosine over it exists: exclude the named \
             slot from the cosine lane, or re-measure it if zero is not the source row's true value"
        }
        CALYX_LOOM_DIM_MISMATCH => "use slot vectors with matching dimensions for this xterm",
        CALYX_LOOM_NON_FINITE_VECTOR => "remove NaN or infinite values from slot vectors",
        CALYX_LOOM_SLOT_MISSING => "load the requested cx/slot vectors before computing xterms",
        CALYX_LOOM_UNCALIBRATED_BLINDSPOT => {
            "collect enough per-lens-pair blind-spot calibration samples"
        }
        CALYX_LOOM_FORGE_UNAVAILABLE => {
            "repair the selected Forge backend, its device runtime, or its explicit dispatch budget; Loom never retries on another backend"
        }
        CALYX_LOOM_SERIES_READ_ERROR => "repair the recurrence series before temporal xterm reads",
        CALYX_LOOM_TEMPORAL_XTERM_CORRUPT => {
            "rewrite the temporal_xterm row from recurrence series"
        }
        CALYX_LOOM_PANEL_SCOPE_REQUIRED => {
            "bind the operation to the exact panel version and panel-local slots"
        }
        CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED => {
            "rebuild Loom cross terms from authoritative panel-qualified Base rows"
        }
        CALYX_RECURRENCE_CONTEXT_TOO_LARGE => "store only a bounded recurrence context blob",
        CALYX_RECURRENCE_INVALID_RETENTION => "use a positive recurrence max_occurrences value",
        CALYX_REACTIVE_REGISTRY_FULL => "deregister a trigger or raise max_triggers",
        CALYX_REACTIVE_QUEUE_FULL => "drain TriggerFired events or raise max_queue_depth",
        CALYX_REACTIVE_DRAIN_OVERFLOW => "drain the subscription more often or raise max_drain_buf",
        CALYX_REACTIVE_SUBSCRIPTION_NOT_FOUND => "use a registered subscription id",
        CALYX_REACTIVE_SIGNAL_UNAVAILABLE => "wire a signal source that evaluates this condition",
        CALYX_REACTIVE_ROW_CORRUPT => "rebuild the reactive CF rows from the ledger/audit source",
        _ => "inspect Loom xterm inputs",
    };
    CalyxError {
        code,
        message: message.into(),
        remediation,
    }
}
