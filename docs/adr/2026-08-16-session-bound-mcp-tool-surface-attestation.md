# Session-bound MCP tool-surface attestation

- **Status:** accepted
- **Date:** 2026-08-16
- **Issue:** #2252
- **Supersedes:** treating the newest host-wide Codex restart handoff as the effective schema verdict for every caller

## Context

Synapse already recorded a host snapshot of the installed MCP tool surface and restart handoffs for Codex processes that were alive when that surface changed. That state answers whether a named host process may be stale, but it does not answer whether the exact MCP session making a later tool call successfully loaded and validated the daemon's current `tools/list` response. A newer caller could therefore be reported stale solely because an older, unrelated Codex PID remained alive.

The production transport currently implements the MCP 2025-11-25 Streamable HTTP contract. The server assigns `Mcp-Session-Id` during initialization, and the client sends that identifier on subsequent requests. Tool discovery is also a protocol operation: servers expose tools through `tools/list`, clients validate the returned schemas, and servers can request rediscovery through `notifications/tools/list_changed`. The session id is consequently the narrowest available identity for binding a served schema surface to later calls by that same client session.

The MCP 2026-07-28 revision removes protocol-level sessions. This decision deliberately does not claim to cover that future boundary; migration to that revision requires a request- or transport-instance attestation design rather than silently weakening this one.

## Decision

1. A session-scoped `tools/list` acquires the same session-authority lock used by profile mutations, rejects terminated sessions, computes the exact profile-filtered sanitized surface, and persists an attestation before returning it.
2. The authoritative row lives in `CF_SESSIONS` under `mcp/tool-surface-attestation/v1/<session_id>`. It binds session id, transport, initialized client name/version, negotiated protocol, agent kind, request/list timestamps, sorted tool names, tool count, and the canonical SHA-256 of the exact served tools.
3. `tools/list` fails closed if the session registry is absent or not live, the client/protocol identity is incomplete, the surface cannot be fingerprinted, the row cannot be written, or immediate independent byte readback disagrees with the written record and live surface.
4. Every registered session-scoped `tools/call` first reloads the attestation and compares count, sorted names, and SHA-256 to the exact surface currently allowed by that session's durable profile. Missing, malformed, or mismatched state is a typed error; no host snapshot or inferred default substitutes for it.
5. The registry's current request must be a later `tools/call:<name>`. This proves the attested `tools/list` and a subsequent call were observed on the same initialized session before caller-current status is reported.
6. `profile status` reports the caller attestation as the effective verdict. Host snapshot, restart handoff, and live stale-PID findings remain visible in separate `host_status` and `host_diagnostic_code` fields and cannot mask a current caller.
7. Profile surface changes continue to emit `notifications/tools/list_changed`. The next call is refused until the same session has fetched and attested the new surface.
8. Session teardown deletes the attestation and separately reads its absence. Cleanup failure is part of the session-continuity cleanup report and prevents a clean teardown verdict.
9. Unscoped stdio/admin calls retain their existing behavior because they have no Streamable HTTP session identity. This is an explicit protocol boundary, not a fallback for failed session attestation.

## Consequences

- A live old Codex process can still be called out as a host hygiene concern while a different, exact caller is correctly proven current.
- A direct tool caller that skips discovery cannot use a session-scoped Synapse tool; it receives `MCP_TOOL_SURFACE_ATTESTATION_MISSING`.
- A client that ignores a profile-change notification receives `MCP_TOOL_SURFACE_ATTESTATION_STALE` rather than executing against an undiscovered schema.
- The physical row and its byte digest provide an independently readable Source of Truth for manual Full State Verification.
- Moving to MCP 2026-07-28 is a schema-attestation migration and must not reuse these session semantics by name only.

## References

- MCP 2025-11-25, Tools: <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>
- MCP 2025-11-25, Transports: <https://modelcontextprotocol.io/specification/2025-11-25/basic/transports>
- MCP 2026-07-28 changelog: <https://modelcontextprotocol.io/specification/2026-07-28/changelog>
