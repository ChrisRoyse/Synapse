# FSV — #2168 truthful browser-navigation correlation

Date: 2026-08-11 (America/Chicago)  
Implementation commit: `a83c566faedaf39ad3005594f203a2728039c290` on `main`  
Acceptance comment: https://github.com/ChrisRoyse/Synapse/issues/2168#issuecomment-5251143665

## Source of Truth

No command return value was accepted by itself. The independent Sources of
Truth were:

- the live DOM, URL, title, readiness, and main-frame `documentId` in the real
  already-open Chrome profile;
- every physical browser-navigation row in Calyx `CF_TIMELINE`, reopened
  independently through `timeline operation=search`, including its sequence,
  key, event ID, claim ID/status, actor, transition metadata, and document ID;
- the synthetic loopback HTTP server's real socket and request log;
- Chrome's separately queried tab table and exact-target presence/absence;
- the installed daemon/worker bytes, OS process table, setup state, listener,
  and authenticated daemon/bridge health.

The fixture was a real in-memory `ThreadingHTTPServer` at
`127.0.0.1:43168`, owned by durable shell job
`019ff002-73ab-7d21-a9f5-af5d7babe786` / PID 7632. Its minimal known pages
were `/` = `ROOT`, `/a` = `ABA`, `/ab` = `AB`, `/redirect` = an HTTP 302 to
`/final`, `/final` = `FINAL`, an in-document History API transition to
`/final/spa` = `SPAFINAL`, and `/slow` = a 32-second response. A separate
`Invoke-WebRequest /a` read HTTP 200, 124 response bytes, and SHA-256
`B5456B0BD941866D76C67FF4EF09A73774015F938820890E46471F1C9EC6598C`.

## Root cause and repair

The extension held only one mutable navigation claim per tab. The matcher
treated an empty URL as a wildcard and considered either URL a match when one
was a prefix of the other. Once such a claim matched, `postTabNavigationEvent`
returned without persisting the physical event. Separately, command readback
accepted any URL or document change. Thus a claim for `/a` could suppress or
claim a real `/ab` navigation and the command could report success for the
wrong destination.

The repaired design:

1. creates a stable claim before mutation whose ID is the daemon's exact Chrome
   command ID and records action, agent session, requested URL, before URL,
   before document ID, and a bounded 30-second lifetime;
2. uses canonical URL equality, never prefix/wildcard matching;
3. requires exact action-specific evidence: the requested URL or a measured
   redirect chain for navigate/open, `transitionType=reload` and a new document
   for reload, and `forward_back` plus a new document for history traversal;
4. serializes `webNavigation` delivery so asynchronous HTTP posting cannot
   reorder browser lifecycle events;
5. persists every `tabs.onUpdated` and main-frame `webNavigation` event with a
   worker UUID plus monotonic event sequence, even when it is unmatched;
6. attributes `Agent` only when the exact claim and nonempty agent session are
   both present; contradictory state fails loudly instead of guessing;
7. exposes explicit completed, failed, download, expired, superseded, and
   target-closed claim terminals, retaining a terminal-delivery failure for
   diagnosis if persistence itself fails;
8. requires the exact correlated committed document for navigation command
   success and refuses commands before mutation when the current main-frame
   document ID is unavailable;
9. carries the new correlation/document fields through the daemon response and
   M3 timeline record instead of discarding them.

There is no suppression lane, legacy prefix fallback, or URL-change success
shortcut left.

## Research after diagnosis

`scripts/check-research-lane.ps1` performed a real MCP `initialize`,
`tools/list`, and `tools/call`. The physical readback at
`%TEMP%\synapse-research-lane-readback.json` was 1,626 bytes, SHA-256
`017DC2E1996EEBA2A45BD5193EDC2AE0A119EEE6A615489664D1A65405BA2239`,
and reported Exa MCP 3.4.0 live. Exa official-source search was supplemented by
the built-in web lane. Primary sources read:

- [Chrome `webNavigation`](https://developer.chrome.com/docs/extensions/reference/api/webNavigation),
  which documents lifecycle ordering, stable per-document `documentId`, frame
  identity, and navigation transition metadata;
- [Chrome `tabs`](https://developer.chrome.com/docs/extensions/reference/api/tabs),
  which documents that tab URL/readiness updates are separate observations and
  may be absent or partial at different lifecycle stages.

These sources support correlating immutable document and transition facts while
retaining tab updates as observations; neither supports inferring causal
identity from string prefixes or suppressing one event stream in favor of the
other.

## Build, lint, and deployment

- `node --check extensions/synapse-chrome-debugger/service_worker.js`: passed.
- `cargo fmt --all --check`: passed.
- `cargo check --workspace`: passed.
- canonical `pwsh -File scripts/lint.ps1`: all seven fail-closed gates passed,
  including root and Calyx formats and
  `cargo clippy --workspace --all-targets -- -D warnings` in both workspaces.

Canonical `scripts\synapse-setup.ps1 -SourceDir C:\code\synapse` used the
repository target, `CARGO_BUILD_JOBS=12`, and
`CMAKE_BUILD_PARALLEL_LEVEL=12` on the host's 12 logical processors. The host
has no NVIDIA/NVML/CUDA device, so the truthful optimized path is CPU. Setup
built, installed, started, health-checked, and reloaded bridge v9. It then
returned the intentional structured restart-required error
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE`, because this long-lived Codex
process predates the changed `browser_nav` output schema. It did not disguise
that incompatibility as deployment success; the independently read installed
state was live:

```text
installed daemon: C:\Users\hotra\.cargo\bin\synapse-mcp.exe
release image:    C:\code\synapse\target\release\synapse-mcp.exe
both bytes=258004809
both sha256=5ebd8106194804802d5ffa5874002acac2baf80d37170ff452b054618823312d
process/listener: PID 18376, installed image, 127.0.0.1:7700
authenticated health: ok=true build=a83c566faeda tool_count=40
surface sha256=21799f167deaafa3aa7f28618bf0092a3094bb4937c6ebd5a343dfe801a48427
active worker bytes=1034304
active worker sha256=47c1be0df9903127d74d517d73d103348b02e95bbc61a38b72041c5b83e5ea4e
bridge build=synapse-chrome-bridge-2026-08-11-truthful-navigation-events-v9
bridge declared build sha256=fd72131dc2ca91f5bcfb825aa91207f109e7e398e9b2258dc429238b0c7ea272
bridge host_count=1 pending_count=0 queued_count=0 extension_stale=false
```

The released and installed executable hashes match. A later setup-status read
independently found PID 18376, bind `127.0.0.1:7700`, the installed autostart
task running, and the current physical daemon-run file.

## Manual Full State Verification

### Happy path and physical event retention

Before the trigger, owned background tab `chrome-tab:589710199` independently
read `title=nav-root`, `body=ROOT`, and `data-state=root`. Gmail was the only
active/highlighted tab.

Navigating to `/a` created claim `chrome-cdp-18376-9`. Its initial document was
`55EE905C160AA2F41BC9F122CDC455E4`; its exact committed/final document was
`0AA30017928B08A2BFF81DB2CFD4065E`. A separate DOM read found
`title=nav-a`, `body=ABA`, and `data-state=a`.

An independent `CF_TIMELINE` search returned eight distinct physical rows,
sequences 40–48: claim created, exact before event, pending tab update, exact
commit, committed tab update, DOMContentLoaded, completed, and terminal claim
completed. Every browser event had a distinct worker event ID and retained the
same exact claim.

Further real action/source-of-truth pairs were:

| Trigger | Browser command evidence | Independent physical result |
|---|---|---|
| Reload `/ab` | claim `chrome-cdp-18376-20`; document `762DC9...3B18` to `8BF2CD...A463` | CF sequences 94–100 retain created/before/commit/DOMContentLoaded/completed/terminal; commit says `transition_type=reload` |
| Back | claim `chrome-cdp-18376-21`; document `8BF2CD...A463` to `A236D1...558D` | DOM is `nav-a`; CF sequence 105 says `agent_back_history_transition_committed_document`, `transition_qualifiers=[forward_back]` |
| Forward | claim `chrome-cdp-18376-22`; document `A236D1...558D` to `760EA0...EEF4` | separate DOM is `nav-ab`; command and persisted lifecycle both completed |
| HTTP redirect | claim `chrome-cdp-18376-23`; document `760EA0...EEF4` to `5ED0A0...0328` | DOM is `nav-final`; CF sequence 121 says `agent_explicit_redirect_chain_committed_document`, qualifier `server_redirect`; terminal is sequence 126 |
| History API | real click on `#spa`, same document `5ED0A0...0328` | separate DOM is `/final/spa`, `nav-spa`, `SPAFINAL`; CF key `18cab445945bbe8800000081`, sequence 129, is retained as `webNavigation.onHistoryStateUpdated`, `initiator=page_or_operator`, `claim_id=null` |

### Boundary and edge-case audit

| Case | Before and trigger | Expected | Independent after-state |
|---|---|---|---|
| Prefix collision plus interleaving | DOM `nav-a`; claim `/a?delay=1` while a page timer navigates to `/ab` | `/ab` must not satisfy or be suppressed by the `/a` claim | command `chrome-cdp-18376-17` fails `A11Y_CDP_EXTENSION_TIMEOUT` and names last URL/document `/ab` / `762DC9...3B18`; DOM is `nav-ab` / `AB`; CF sequences 83–92 retain the claim, canceled `/a` event, unmatched `/ab` before/update/commit/lifecycle; sequence 88 verdict is `unmatched_commit_not_in_agent_url_or_redirect_chain` |
| Claim expiry | wait beyond the same incomplete claim's 30-second TTL | explicit durable terminal, never silent deletion | CF key `18cab42e537504a40000005d`, sequence 93: `claim_status=expired`, verdict `claim_ttl_expired_without_terminal_navigation` |
| Target closes mid-navigation | from `/final/spa`, claim `/slow`, observe timeout, then close exact owned tab | command cannot report success; claim must terminate and tab must be physically absent | claim `chrome-cdp-18376-30` times out with last `/slow`; separate tab list has no `589710199`; CF key `18cab4553c3207e800000086`, sequence 134, is `target_closed` / `claim_target_closed_before_terminal_navigation`; sequence 135 retains the aborted physical error event |
| Fresh tab after close | open `/` after exact absence | no stale target or claim may transfer | new tab is `chrome-tab:589710201`, not 199; new claim is `chrome-cdp-18376-34`, not 30; new committed document is `590B982A09051CADEA0DF59F5F3A53BF`; DOM is exactly `ROOT`; CF sequences 136–145 contain only claim 34 and its own lifecycle |
| Invalid format before mutation | fresh tab DOM `ROOT`; navigate with leading whitespace | typed validation failure and zero browser/storage mutation | `TOOL_PARAMS_INVALID` names leading/trailing whitespace and remediation; a later DOM read is still URL `/`, `nav-root`, `ROOT`, `data-state=root`; a later CF search still returns the identical ten rows ending at sequence 145—no claim/event was created |

Several external-timer attempts fired before their next MCP call acquired its
initial document due orchestration latency; they correctly completed from the
already-changed starting state and were not counted as collision evidence. The
accepted collision trigger scheduled the page navigation and issued
`browser_nav` in one orchestrated call, so the `/ab` transition occurred after
claim creation while `/a?delay=1` was deliberately held by the real server.

The fixture's two `ConnectionAbortedError` messages are the expected Python
server-side observation of Chrome canceling the deliberately interrupted
`/a?delay=1` and `/slow` responses. Chrome's corresponding
`webNavigation.onErrorOccurred` rows were retained; this was the test trigger,
not a product or fixture fallback.

### Cleanup and final state

The exact synthetic tabs 589710199, 589710201, and the temporary issue-inspection
tab were closed through ownership-checked Chrome operations. A separate final
`chrome.tabs.query` returned only the original issue #2168 tab plus Gmail;
Gmail remained the sole active/highlighted tab. The durable fixture job was
canceled, its owned process tree reported empty, and a separate OS read found
PID 7632 absent and no listener at port 43168.

No automated test, mock data, fallback lane, second browser, branch, worktree,
alternate target directory, screenshot artifact, or temporary repository FSV
directory was created or used as acceptance evidence.
