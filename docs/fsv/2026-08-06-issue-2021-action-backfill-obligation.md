# Issue #2021: action backfill obligation Full State Verification

Date: 2026-08-06. Host: configured Windows end-user system. Branch: `main`.

## Source of truth

The authoritative trigger corpus is `CF_ACTION_LOG`. The resulting projection
and anchors are physical Calyx `Base` and `Anchors` CF rows. Acceptance used a
fresh authenticated HTTP MCP session for each pass, then a separate
`storage operation=panel_coverage` scan that decodes `Base`, enumerates physical
source keys, and checks `base_cf_rows == records_total + decode_failures`.
Return values from `temporal_backfill` were not accepted as proof.

## Root cause and repair

`anchors_stranded_on_superseded` treated every grounded superseded source
identity absent from the active generation as actionable carry-forward debt.
That was false in two independent cases:

1. A TTL-managed audit source had expired, so no replay was possible.
2. Action generation `2020001` deliberately replaced the earlier over-broad
   outcome adjudication. Twelve surviving nonterminal rows had invalid legacy
   anchors that must remain historical evidence, not be copied into the new
   outcome contract.

The census now intersects replay debt with the physical source-key census.
The panel catalog also declares whether superseded anchors retain compatible
semantics. The carry writer and coverage reader consume the same declaration;
`syn-action-v1` declares `carry_superseded_anchors=false`. Historical anchors
remain counted and sacred.

Research followed diagnosis. `scripts/check-research-lane.ps1` proved Exa MCP
v3.4.0 live with a real `web_search_exa` call. Built-in research used the
Microsoft Azure and AWS Event Sourcing primary guidance: immutable event
history is authoritative, projections are rebuildable, and replay consumers
must be idempotent. Sources:

- https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing
- https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/event-sourcing-pattern.html

## Execute and inspect

Installed daemon: PID `21404`,
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, SHA-256
`3F11F472EF5A1972E62FBEDD06C99F99A2781733F406FE5DFD2C421F943BEF41`.

Cursor-exhaustive real replay used two pages and independently read the vault:

```text
examined=163 already_current=163 inserted=0 more=false
terminal_anchored=83 nonterminal_absent=80
Base accounting_holds=true active=319 source_rows=163
grounded=161 reward=161 uncovered=0
superseded_grounded=167 superseded_orphaned=224
anchors_stranded_on_superseded=0 backfill_owed=false
```

The superseded counts prove the fix did not hide or delete history. The zero
stranded count is specifically the replayable, contract-compatible debt.

## Boundary and edge audit

1. `max_rows=1` was accepted. Before and after physical reads both showed
   `active=319`, `grounded=161`, `reward=161`, `stranded=0`, `uncovered=0`,
   `backfill_owed=false`, and complete Base accounting.
2. `max_rows=0` failed with `TOOL_PARAMS_INVALID` and remediation to use
   `1..=1000`; it states that no storage operation was attempted. Before and
   after action-panel state was identical.
3. `after_physical_hex=zz` failed at byte offset 0 with remediation to use the
   even-length `resume_after_physical_hex` returned by the preceding page.
   `key_hex=0` independently failed on odd length with exact-key remediation.
   Both before/after reads retained `active=319`, `grounded=161`, `reward=161`,
   `stranded=0`, `uncovered=0`, `backfill_owed=false`.

Unrelated `base_cf_rows` rose during verification because each real MCP call is
itself durably audited. The action-panel fields above, which are the relevant
source of truth, did not change.

## Gates

`cargo check --workspace` passed. `pwsh -File scripts/lint.ps1` passed all
seven gates in both workspaces. Setup built, validated, installed, restarted,
and independently identified the daemon and installed image shown above.
