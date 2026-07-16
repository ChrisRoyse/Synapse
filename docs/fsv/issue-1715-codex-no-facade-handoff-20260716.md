# Issue #1715 FSV - Codex No-Facade Handoff

## Root Cause

The live failure was not the Synapse daemon, Codex durable config, bearer token,
or existing Chrome bridge. A fresh production Codex process in
`C:\code\Calyx-Dev` loaded the configured Synapse MCP server and called real
`mcp__synapse.health`. The failing boundary is session-local: an already-running
Codex process can have no Synapse MCP namespace in its model tool registry, and
that absent namespace cannot call `mcp__synapse.setup` to generate the existing
setup restart handoff.

Fix: add `scripts\synapse-codex-doctor.ps1`, a shell-runnable doctor for sessions
that can still run commands but cannot call Synapse. It fails closed on broken
project/config/token/snapshot/socket/daemon/fresh-client proof, strips bearer
tokens from persisted artifacts, and writes a same-agent restart handoff only
after a fresh production Codex process proves a real Synapse MCP health call.

## Research Basis

- OpenAI Codex manual, fetched 2026-07-16 with the repo's `openai-docs` skill,
  plus the public config reference at
  <https://learn.chatgpt.com/docs/config-file/config-reference>:
  Codex MCP servers are configured in `config.toml`; Streamable HTTP servers use
  `url`, bearer-token env vars are supported, `required=true` fails startup when
  the server cannot initialize, and `default_tools_approval_mode="approve"`
  removes the old prompt gate.
- Model Context Protocol debugging guide at
  <https://modelcontextprotocol.io/docs/tools/debugging>:
  validate client logs, server process, standalone operation, protocol/capability
  negotiation, and restart the MCP client after config/server changes.
- MCP Streamable HTTP spec at <https://modelcontextprotocol.io/specification>:
  a single HTTP endpoint plus JSON-RPC session negotiation is the durable
  transport boundary; bypassing the client is diagnostic only.

## Source Of Truth

- Codex config: `C:\Users\hotra\.codex\config.toml`.
- Bearer token: `C:\Users\hotra\AppData\Roaming\synapse\token.txt`.
- Codex start snapshot: `C:\Users\hotra\AppData\Roaming\synapse\codex-tool-surface.json`.
- Daemon process/socket: `synapse-mcp.exe` PID `64508`, bind `127.0.0.1:7700`.
- Real client proof: nested `codex exec --json` event log with
  `mcp_tool_call server=synapse tool=health`.
- Handoff/report files:
  - `C:\Users\hotra\AppData\Local\synapse\codex-no-facade-doctor\run-47096-20260716T212847516Z\doctor-report.json`
  - `C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs\codex-no-facade-handoff-27984-20260716T212912013Z.json`

## Happy Path

Before trigger:

- `codex mcp get synapse` showed enabled Streamable HTTP at
  `http://127.0.0.1:7700/mcp`, bearer env `SYNAPSE_BEARER_TOKEN`,
  `required=true`, and `default_tools_approval_mode="approve"`.
- Real `mcp__synapse.health` from this session reported `ok=true`, PID `64508`,
  `tool_count=40`, and tool surface
  `f762a7df57aac03adc41d80a8cb8c8ab72496f142491b60c55f39957b5f8a069`.
- Real `mcp__synapse.browser_tabs operation=list` reached the already-open
  Chrome profile through the normal bridge and listed 13 tabs.

Trigger:

```powershell
pwsh -NoProfile -File scripts\synapse-codex-doctor.ps1 `
  -ProjectDir C:\code\Calyx-Dev `
  -SourceDir C:\code\Synapse `
  -ActiveIssue 1715 `
  -ObservedSynapseFacadeAbsent `
  -FreshProbeTimeoutSec 240
```

After trigger, separate physical readback:

- Doctor report status: `handoff_written`.
- Handoff reason: `SYNAPSE_CODEX_CURRENT_PROCESS_MCP_ABSENT`.
- Stale Codex PID named in handoff: `27984`.
- Fresh Codex probe: `ok=true`.
- JSONL proof: `mcp_health_call_observed_in_jsonl=true`.
- Fresh probe health result: PID `64508`, `tool_count=40`, tool surface
  `f762a7df57aac03adc41d80a8cb8c8ab72496f142491b60c55f39957b5f8a069`.
- Handoff/report bearer-token search: both `false`.

Artifact hashes:

- Script SHA-256:
  `247C48D05F8DFABA2A98448AAEDB98C3BE43DB2E92BDD7D49101D77246EE1AB4`.
- Happy report SHA-256:
  `96F893F4E6EC1CA37AC310C3135D532EE277F6AA32322E2111888FCB8861FFC6`.
- Handoff SHA-256:
  `A6335B70857CFE8B4C5800458D908AE6D921A611D8DEA413D948FD2C5C54CA9B`.

## Edge Cases

Each edge used an isolated report root, printed before/after counts, and was
read back from the physical JSON report file with a separate operation.

| Edge | Before | After | Expected | Actual |
| --- | ---: | ---: | --- | --- |
| Missing project `C:\path\does-not-exist` | 0 report dirs | 1 report dir | fail, no handoff | `SYNAPSE_CODEX_PROJECT_PATH_MISSING`, `project.exists=false`, `handoff_present=false` |
| Missing config `C:\path\missing-config.toml` | 0 report dirs | 1 report dir | fail, no handoff | `SYNAPSE_CODEX_CONFIG_MISSING`, `codex_config.exists=false`, `handoff_present=false` |
| Missing token `C:\path\missing-token.txt` | 0 report dirs | 1 report dir | fail, no handoff | `SYNAPSE_CODEX_TOKEN_MISSING`, `token.exists=false`, `handoff_present=false` |

Edge report SHA-256:

- Bad project:
  `BB799A0F1F78B1EA45FD6E2DC3598A24AA921AA66CCEFB37C0C95A869F0AD2BC`.
- Missing config:
  `18BDD72E2D5FE866C92C7BAEB7C1D8E3FD391F9E421C474810605CA05F308506`.
- Missing token:
  `4777883C980E0A69F83686F3798C73313DA0B5BF25915DFAF241A3B3B129019A`.

## Structural Checks

- PowerShell parser validation for `scripts\synapse-codex-doctor.ps1`: pass.
- `git diff --check`: pass.

No automated tests or FSV harnesses were added.
