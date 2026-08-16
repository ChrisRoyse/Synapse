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

1. A session-scoped `tools/list` acquires the same session-authority lock used by profile mutations, rejects terminated sessions, computes the exact profile-filtered sanitized surface, and persists a `client_tools_list` binding before returning it. When a client reinitializes an unchanged Streamable-HTTP server but reuses its already validated catalog, the first `tools/call` instead persists a `server_first_tool_call` binding over that same exact sanitized surface before admission. The MCP discovery specification defines `tools/list`, but does not require a fresh list request after every new transport session; making that stronger condition an admission rule caused a complete outage in the production Codex client.
2. The authoritative row lives in `CF_SESSIONS` under `mcp/tool-surface-attestation/v1/<session_id>`. Schema v2 binds session id, transport, initialized client name/version, negotiated protocol, agent kind, binding source, request/binding timestamps, sorted tool names, tool count, and the canonical SHA-256 of the exact served tools.
3. `tools/list` fails closed if the session registry is absent or not live, the client/protocol identity is incomplete, the surface cannot be fingerprinted, the row cannot be written, or immediate independent byte readback disagrees with the written record and live surface.
4. Every registered session-scoped `tools/call` first reloads the binding and compares count, sorted names, and SHA-256 to the exact surface currently allowed by that session's durable profile. An honestly absent row is initialized from the immutable schema-sanitized server surface and immediately read back before the call proceeds. Malformed or mismatched existing state is a typed error and is never overwritten; no host snapshot or inferred surface substitutes for it.
5. Stale refusals finish the denied call lifecycle, emit `notifications/tools/list_changed`, and then run full session teardown. A Streamable HTTP tool response may already be committed as SSE before asynchronous teardown completes, so the server does not claim it can retroactively replace that in-flight response. The HTTP middleware instead intercepts the next request carrying the terminated session id before RMCP dispatch and returns HTTP 404 with the exact stale root-cause code. This is the MCP 2025-11-25 recovery boundary: the specification permits server termination at any time and requires a client receiving 404 for its session id to initialize a new session. The server never admits a known-stale call or overwrites corrupt evidence.
6. The process-local terminated-session authority stores the first termination reason with the session id, rather than only membership. Ordinary lifecycle termination remains `session_terminated`; tool-surface termination remains distinguishable as `MCP_TOOL_SURFACE_ATTESTATION_MISSING` or `MCP_TOOL_SURFACE_ATTESTATION_STALE` through the transport 404.
7. The registry's current request must be `tools/call:<name>`. This proves the bound surface and an actual call were observed on the same initialized session before caller-current status is reported; `binding_source` separately says whether the client listed or the server bound on first use.
8. `profile status` reports the caller attestation as the effective verdict. Host snapshot, restart handoff, and live stale-PID findings remain visible in separate `host_status` and `host_diagnostic_code` fields and cannot mask a current caller.
9. Profile surface changes continue to emit `notifications/tools/list_changed`. A client that attempts a call before rediscovery receives the stale refusal and forced session reinitialization.
10. Session teardown deletes the attestation and separately reads its absence. Cleanup failure is part of the session-continuity cleanup report and prevents a clean teardown verdict.
11. Unscoped stdio/admin calls retain their existing behavior because they have no Streamable HTTP session identity. This is an explicit protocol boundary, not a fallback for failed session attestation.
12. Readers accept the previously shipped schema-v1 bytes without rewriting them: the legacy `listed_at_unix_ms` field is decoded as `bound_at_unix_ms`, and its absent binding source is deterministically interpreted as `client_tools_list`. Validation still requires schema v1's exact client-list semantics. All new writes remain schema v2. This is a read migration, not permissive normalization of malformed state.

## Consequences

- A live old Codex process can still be called out as a host hygiene concern while a different, exact caller is correctly proven current.
- A client that retains an unchanged catalog across transport reinitialization remains usable: the server binds its exact current sanitized surface on first use and physically reads the row back before admission.
- A client that ignores a profile-change notification receives `MCP_TOOL_SURFACE_ATTESTATION_STALE` rather than executing against an undiscovered schema.
- A client that ignores a real surface-change notification cannot execute against a known-stale binding: the first unsafe call expires that session and the next protocol cycle must initialize again.
- The physical row and its byte digest provide an independently readable Source of Truth for manual Full State Verification.
- Moving to MCP 2026-07-28 is a schema-attestation migration and must not reuse these session semantics by name only.

## References

- MCP 2025-11-25, Tools: <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>
- MCP 2025-11-25, Transports: <https://modelcontextprotocol.io/specification/2025-11-25/basic/transports>
- MCP 2026-07-28 changelog: <https://modelcontextprotocol.io/specification/2026-07-28/changelog>
