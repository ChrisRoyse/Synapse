# Issues #2019 / #2020: action outcome replay and canonical reward axis

Date: 2026-08-06 (America/Chicago)

## Diagnosis and research

Source of truth is the production vault at
`%LOCALAPPDATA%\synapse\db-daemon`: authoritative action records are in
`CF_ACTION_LOG`, constellations in `Base`, outcome evidence in `Anchors`, and
mutation provenance in `Ledger`.

The first physical census found 631 active action constellations but only six
`reward` anchors. A real replay examined 325 source rows and still found only
six because the source is dual-schema:

- `action_audit`: terminal state is `status=ok|error|denied`;
- `command_audit`: terminal state is `phase=final,outcome=ok|error`;
- `preflight` and `phase=intent,outcome=pending` are nonterminal.

The sanitized production histogram contained 162 command intents, 157 final
successes, four final failures, six successful legacy actions, and three
preflights. The historical builder understood neither schema; the live-only
publication path manually attached only the legacy anchor.

The Exa MCP lane was probed before use: exa-search-server v3.4.0 initialized,
advertised two tools, and a real `web_search_exa` call returned content. The
built-in web lane independently read Microsoft's Event Sourcing guidance:
the event store is authoritative, projections must be reproducible by replay,
and consumers must be idempotent. Sources:

- https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing
- https://learn.microsoft.com/en-us/azure/architecture/serverless/event-hubs-functions/resilient-design

## Implementation

- One strict source adjudicator now handles both declared audit schemas.
- Unknown final command phases/outcomes return a repair-bearing error.
- Live writes and historical replay use the same constellation builder.
- Action panel generation `2020001` has exactly one native `Reward` axis.
- Legacy `2006001` is explicitly superseded. Its sacred evidence remains
  readable, but action anchors are not carried into the clean generation.
- Readiness validation, lens provenance, and the panel catalog use `2020001`.
- The action panel is declared outcome-bearing; timeline remains non-outcome.

Commits: `2e678b75`, `c9e3a900`, `43caec35`, `2fad54b1`.

## Build and installed image

`cargo check -p synapse-mcp` passed. The repository's only lint gate,
`pwsh -File scripts/lint.ps1`, passed all seven gates in both workspaces after
the final edit. The supported installer completed on its first build attempt.

Final installed daemon:

- PID: `20872`
- executable: `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`
- installed SHA-256: `05A49F3D3AC1EA32D6E9707F09846CBA37918D3A9F52917D637A7D05EB32BD1E`
- public tools: 40
- tool-surface SHA-256: `2770baa64b9410b86d0ec60343a1b1d6bf70df8db511039e7e07780031b67856`

The installed image reported the known current-Codex-process schema handoff;
fresh authenticated HTTP MCP sessions were used for all evidence below.

## Full State Verification

Before the clean-generation migration, `panel_coverage` read `2020001` with
zero active rows and zero anchors. After lifecycle replay and one explicit
cursor-exhaustive sweep, an independent census read:

```text
panel_version=2020001
active_version_records=319
source_cf_rows=295
uncovered_rows=0
coverage_fraction=1.0813559
grounded_records=161
anchor_kind_records={reward:161}
assay_measurable=true
outcome_bearing=true
all superseded generations closed=true
```

The source row count is lower because `CF_ACTION_LOG` has declared retention;
content-addressed active observations can outlive their TTL-managed source.

### Happy path and negative outcome

Independent exact `Anchors`-CF reads after replay:

```text
success source key 18c92480229edfec0000000b
  panel=2020001 cx_id=3ebf90e0915d341fb52ea71819be6c91
  anchor_count=1 kind=reward value=true
  source_value_sha256=c435215cf9dd1794ab1570afdda01d5e56518f919934c04f857015cc55493ee8

failure source key 18c9229934159eac0000000d
  panel=2020001 cx_id=166d1798634a357710a4e6a3a1aa1256
  anchor_count=1 kind=reward value=false
  source_value_sha256=fc2136e53dc33e21805a734bf9daa289aabbdf57d98accda6c4bc9c787c4c239
```

Exact-key replay reported each row current and one outcome seen. A second
independent read returned the same CxIds, hashes, values, and one physical
anchor each. This proves idempotency against stored state, not the return value.

### Three boundaries

1. Nonterminal preflight: before and after exact replay, source key
   `18c909972aac4b8800000000` had `anchor_count=0`; replay reported
   `outcome_absent_rows=1`.
2. Empty matching physical page: the first all-row page examined zero action
   rows, returned `more=true` with an advancing cursor, and changed no anchor
   counts. Following the cursor reached the source rows and `more=false`.
3. Invalid format: `key_hex=zz` returned `TOOL_PARAMS_INVALID`, naming byte
   offset zero and remediation. Independent panel reads before/after were
   identical: active `307`, grounded `155`, reward `155` at that observation.

## Remaining honest gates

`oracle_validate` now reaches the new action corpus but correctly refuses with
`SYNAPSE_CALYX_ACTION_VALIDATION_GUARD_ABSENT`. Only four real bad command cases
exist, below Ward's certifiable bad-case minimum; no synthetic or mock failures
were introduced. Issue #2017 remains open on that real calibration prerequisite.

The final exhaustive sweep returned `more=false` and `uncovered_rows=0`, but
`panel_coverage` still reports `backfill_owed=true`. That contradiction is filed
as #2021 and is not represented here as success.
