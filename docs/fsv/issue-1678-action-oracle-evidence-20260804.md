# Issue #1678: action Oracle evidence FSV (2026-08-04)

## Scope and source of truth

This verification covers the typed terminal-action evidence producer added as
one prerequisite for Oracle consequence and reverse queries. It does not close
#1678: the MCP query facades, sufficiency/readiness surface, and atomic source +
projection publication tracked by #2005 remain open.

Sources of truth:

- logical source: exact `CF_ACTION_LOG` rows in the Calyx KV collection;
- measured subject: the action recurrence-subject Base row;
- outcome evidence: exact rows in Calyx `Recurrence`, independently decoded
  after the daemon closed;
- durability: vault `latest_seq` before and after rejected writes.

Research followed diagnosis. The Exa MCP lane was `live` and returned a real
query. Primary references used were the OpenTelemetry event/context
specifications (structured outcome/state-change events and immutable propagated
context) and W3C PROV-O (explicit generated-by/informed-by provenance links):

- https://opentelemetry.io/docs/specs/semconv/general/events/
- https://opentelemetry.io/docs/specs/otel/context/
- https://www.w3.org/TR/prov-o/

## Real MCP trigger and independent readback

An isolated debug daemon used:

`C:\Users\hotra\AppData\Local\Temp\synapse-action-oracle-fsv-1785864836288\declared`

The real stdio sequence was `initialize`, `notifications/initialized`, then
`act_focus_window` for known-invalid HWND `1`. Win32 rejected the action with
`ACTION_WINDOW_NOT_FOUND`. After clean daemon shutdown, a separate process ran:

```text
cargo run -q -p synapse-storage --example action_oracle_readback_fsv -- <vault> act_focus_window
```

Physical readback:

```text
ACTION_LOG rows=2
ACTION_ROW ... status=started tool=act_focus_window ...
ACTION_ROW key_hex=18c8aa9c0b982c8000000001 ... status=error ... error_code=ACTION_WINDOW_NOT_FOUND ...
ORACLE_ACTION action=act_focus_window cx_id=fba4838f7283c849fc26f897d43f2682 latest_seq=19 frequency=1 active_occurrences=1
ORACLE_OCCURRENCE id=0 t_k=1785864839 context={"action_id":"act_focus_window","outcome_anchor":{"value":{"bool":false}},"source_action_audit_key_hex":"18c8aa9c0b982c8000000001"}
```

The occurrence points to the exact terminal audit key and encodes the observed
failure as `AnchorValue::Bool(false)`.

## Failure found and repaired

The first real attempt produced a 263-byte context against Calyx's enforced
256-byte maximum and failed with `CALYX_RECURRENCE_CONTEXT_TOO_LARGE`. The
source audit row remained visible and no success was claimed. The context was
reduced to the irreducible evidence contract: action id, typed outcome, and
exact source-row key. Status/error detail remains in the referenced source row.

## Boundary audit

Fresh vault:

`C:\Users\hotra\AppData\Local\Temp\synapse-action-oracle-edges-1785864949599`

```text
EDGE_EMPTY before_latest_seq=10
EDGE_EMPTY error_code=STORAGE_WRITE_FAILED after_latest_seq=10
EDGE_MAX before_latest_seq=10
EDGE_MAX after_latest_seq=12 frequency=1 active_occurrences=1
EDGE_CONFLICT before_latest_seq=14 frequency=1 active_occurrences=1
EDGE_CONFLICT error_code=CALYX_RECURRENCE_OCCURRENCE_CONFLICT after_latest_seq=14 frequency=1 active_occurrences=1
```

Thus an empty action mutates nothing, the documented 200-byte maximum persists
one active occurrence, and reuse of a stable occurrence identity with different
time/context fails closed without advancing durable state.
