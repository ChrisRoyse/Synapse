# Issue #2191 FSV: public profile-registry query route

Date: 2026-08-10 (America/Chicago)  
Installed commit: `73bfa076dd294190d8a6cc137bffbdbdc3a4f4fd`  
Installed PID: `15976`

## Root cause and repair

The registry query engine itself already had typed search, exact inspect, and
report implementations plus `READ_PROFILE + READ_STORAGE` checks. The public
contract made it unreachable. `ProfileOperation` omitted the operation,
`ProfileParams` and `ProfileResponse` had no nested registry types, the facade
contract omitted it, and denial routing fell through to unrelated act/elevation
advice. Every scoped profile deliberately advertises the same 40 facade names,
so elevation could not reveal the hidden raw tool.

The existing public `profile` facade now exposes exactly
`operation=registry_query` with one required matching `registry_query` payload
and a typed response. It calls the existing engine and permission check rather
than duplicating storage logic. Operation/payload mismatch, omission, unknown
fields, and invalid limits fail closed. The hidden raw route now names the
working facade invocation. No 41st public tool, elevation requirement,
fallback, or untyped catch-all was added.

## Research

The live research-lane preflight reported Exa MCP 3.4.0 usable via a real
initialize, tools/list, and tools/call. Exa and built-in web research followed
diagnosis.

- MCP defines `tools/list` plus each tool's JSON input schema as the discovery
  contract: <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>
- MCP server concepts likewise describe discovery as tool name, description,
  and schema: <https://modelcontextprotocol.io/docs/learn/server-concepts>
- Microsoft's API guidance calls for explicit validated contracts, actionable
  errors, and non-secret read surfaces:
  <https://github.com/microsoft/api-guidelines/blob/vNext/azure/Guidelines.md>
- JSON Schema object/keyword semantics support the strict typed payload instead
  of an open bag: <https://json-schema.org/understanding-json-schema/keywords>

## Sources of truth

1. Public discovery: a fresh MCP `tools/list` response and the `profile` input
   schema/contract exposed by the installed daemon.
2. Registry state: the production Calyx vault at
   `C:\Users\hotra\AppData\Local\synapse\db-daemon`, specifically
   `CF_PROFILES profile_registry/v1/*` and `CF_KV
   profile_registry/v1/head/*`.
3. Session policy: each exact physical `CF_SESSIONS
   mcp/tool-profile/v1/<session_id>` row returned with byte length and SHA-256.
4. Whole-vault integrity: a separate full public ledger verification.

Before deployment, installed PID 8472 rejected the real
`profile(operation=registry_query)` input as an unknown operation, and
`profile operation=status` listed only status, set, grant, and revoke. That is
the reproduced pre-fix state.

The corrected installed executable is the clean release image at
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, 257,759,561 bytes, SHA-256
`2D5B46D6DDB858681A13D50B5CF5DD796640B7849BBCC54E83FAA023E6DC4333`.
Health independently reports build `73bfa076dd29`, PID 15976, and `ok=true`.

## Normal-agent happy path and independent storage read

A fresh normal-agent session advertised exactly 40 public tools. Its live
profile contract includes registry query with source of truth
`CF_PROFILES profile_registry/v1/* + CF_KV profile_registry/v1/head/*` and typed
readback metadata.

Independent storage summary before the calls reported Calyx backend and
`CF_PROFILES=0`. This production vault presently has no installed registry
package, so the honest expected state is an empty search and an exact absent
point read; no row was fabricated by bypassing registry write governance.

The public facade produced:

- search: `CF_PROFILES`, prefix `profile_registry/v1/`, limit 5,
  `include_disabled=true`, `total_matched=0`, rows `[]`;
- inspect source `fsv-known-absent-2191`: exact row key
  `profile_registry/v1/source/fsv-known-absent-2191`, `found=false`, row null;
- report: `registry_rows_scanned=0`, `CF_PROFILES=0`, and real bounded evidence
  from the other physical sources, including the newest `CF_ACTION_LOG` row;
- report source metadata named the vault path, every CF, prefix, plain key, and
  key hex used by search/inspect/report.

A separate storage summary after all reads still reported `CF_PROFILES=0`
(delta 0). Thus the read-only facade reached the real query engine and did not
mutate its source of truth.

Calling hidden `profile_registry_query` in a separate normal-agent session was
still correctly denied with `TOOL_PROFILE_POLICY_DENIED`. Its denial named the
exact preferred route:

```text
profile operation=registry_query registry_query={view=search|inspect|report,...}
```

The denial independently returned its physical CF_SESSIONS policy row: 551
bytes, SHA-256
`sha256:a8b27537b98a86447fe020ce2a51bbb2dae88b1346e92009470a55dd8153c7ba`,
40 allowed public tools, and the normal-agent profile.

## Explicit full-capability repeat and cleanup

Session `5af3344b-7132-4419-a8ee-bc542e753a64` began as `normal_agent` with 40
visible tools and policy-row SHA-256
`sha256:b0cf3a1a7e4484b45b33fc971aa200b09ec938b8cb0302f3b403472a93b1eea1`.
The real public act facade acquired the 30-second input lease with
`held=true`, `is_owner=true`. An explicit, reasoned set changed the durable
profile row to `full_capability`, still 40 tools, SHA-256
`sha256:b27f374f6d9d2c0613f5fd98800ea6969765caee5c6f6807523976d7d7b881e5`.

The same public registry inspect succeeded under that profile and returned the
same exact absent CF_PROFILES key. Cleanup then restored `normal_agent`,
released the lease (`held=false`, outcome `released`), and independently read
the restored policy row: 40 tools, SHA-256
`sha256:4741968eb133181f177a209f06897406b283ef09d41b2fb65bed4b595f16eb3a`.
This proves the route is profile-independent as intended and that no elevated
session state leaked after FSV.

## Boundary and edge audit

Physical `CF_PROFILES` count was read as 0 before and after the whole matrix.

| Case | Before / trigger | After |
|---|---|---|
| Missing matching payload | `operation=registry_query` with no nested object | `TOOL_PARAMS_INVALID`: requires `registry_query`; remediation names exact views and absent-row semantics; CF row delta 0 |
| Above maximum | search with `limit=1001` | `TOOL_PARAMS_INVALID`: `limit must be 1..=1000; got 1001`; CF row delta 0 |
| Payload/operation mismatch | `operation=status` plus `registry_query` | `TOOL_PARAMS_INVALID`: payload valid only with matching operation and exact remediation; CF row delta 0 |

## Gates and whole-vault readback

- `cargo check --workspace`: pass.
- Canonical `pwsh -File scripts/lint.ps1`: pass across both root and Calyx
  workspaces.
- `git diff --check`: pass.
- Fresh fetch: local main is two tested commits ahead and zero behind
  `origin/main`; no branch or worktree exists.

The final full public chain walk reported `intact`, entries/head
`343212/343212`, verified `[0..343212)`, tip
`6768edab95a5c3da3573168eb10e51af80e93099fd8f24e4a9cda22c6635ee69`.
Raw commitments were intact (`965533` total, `965530` sealed, 3 current tail
rows pending, 39,643 cohort seals); generation remained 1 with zero resets.

## Verdict

PASS. A fresh 40-tool normal-agent client can discover and execute all three
registry read views through the public facade; exact missing-row semantics,
bounded real report reads, raw-route remediation, explicit profile elevation,
cleanup, invalid inputs, physical CF counts, and full-vault integrity all agree.

