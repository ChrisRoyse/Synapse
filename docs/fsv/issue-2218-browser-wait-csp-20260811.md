# FSV — #2218 browser function waits under strict CSP

Date: 2026-08-11 (America/Chicago)  
Implementation commit: `ae9917c9efaac42e23bc7cc443544515e9096a86` on `main`  
Acceptance comment: pending publication

## Source of Truth

No command acknowledgement was accepted alone. The independent Sources of
Truth were:

- the real GitHub response's `Content-Security-Policy` header and the live page
  DOM/URL/title/readiness in Chrome target `chrome-tab:589710129`;
- Chrome `webNavigation` main-frame document IDs before and after every
  predicate poll;
- the Chrome extension's physical owner ledger, including debugger attachment
  count and unresolved debugger-command timeouts;
- the independently listed Chrome tab table, including active/highlighted state;
- `%LOCALAPPDATA%\synapse\db-daemon\daemon-tool-events.jsonl`, reopened through
  the audit facade with per-line byte lengths and SHA-256 hashes;
- installed daemon and extension bytes, the OS process table, the TCP listener,
  and authenticated daemon health/build provenance.

## Root cause and repair

`waitForFunction` injected `runWaitForFunctionProbeInPage` with
`chrome.scripting.executeScript({ world: "MAIN" })`, then compiled the caller's
predicate with indirect page-world `eval`. GitHub sends `default-src 'none'`
and `script-src github.githubassets.com` without `unsafe-eval`, so the browser
correctly rejected that compilation. The bridge collapsed the resulting page
policy failure into `CHROME_SCRIPTING_EXECUTE_FAILED`. Polling, timeout, and
target selection were never the cause.

The repaired path:

1. reads the exact main-frame document ID before attaching;
2. holds one bounded transient `chrome.debugger` attachment for the wait;
3. resolves the exact main frame with `Page.getFrameTree` and creates a named
   `Page.createIsolatedWorld` context;
4. compiles/runs the caller expression through CDP `Runtime.evaluate` in that
   isolated context, with `awaitPromise`, `returnByValue`, the caller's remaining
   deadline, breaks disabled, and CSP bypass disabled;
5. independently rereads the document ID after each poll and discards any
   result whose document changed while it was executing;
6. recreates the isolated context only after a measured navigation and reports
   initial/final document IDs plus `navigation_count`;
7. classifies syntax/runtime failures as `CHROME_WAIT_PREDICATE_INVALID`, keeps
   attach/transport/timeout distinct, and fails with
   `A11Y_CDP_EXTENSION_DETACHED` if transient detach cannot be proven;
8. derives the daemon response envelope from the caller wait budget plus
   explicit round-trip headroom, so the daemon cannot mask a longer wait with
   its old fixed 30-second timeout.

The obsolete page evaluator, including its indirect `eval`, was deleted. There
is no alternate scripting lane or CSP fallback.

## Research after diagnosis

The repository lane check performed real MCP `initialize`, `tools/list`, and
`tools/call`. `%TEMP%\synapse-research-lane-readback.json` was 1,626 bytes,
SHA-256
`B5213D24C886F509DD7BF84EB3AE1875CBE3D8D818CA06121F8226098306293A`,
and reported `exa-search-server` 3.4.0 live with `web_search_exa` and
`web_fetch_exa`. Exa official-source results were independently checked through
the built-in web lane. Primary sources read:

- [Chrome content-script isolated worlds](https://developer.chrome.com/docs/extensions/develop/concepts/content-scripts),
  which separate extension JavaScript variables from the host page while
  retaining DOM access;
- [Chrome `scripting` execution worlds](https://developer.chrome.com/docs/extensions/reference/api/scripting),
  which documents the `MAIN`/isolated execution boundary;
- [CDP Page `createIsolatedWorld`](https://chromedevtools.github.io/devtools-protocol/tot/Page/#method-createIsolatedWorld),
  used to bind the predicate to an explicit frame context;
- [CDP Runtime `evaluate`](https://chromedevtools.github.io/devtools-protocol/tot/Runtime/#method-evaluate),
  including context selection, promise awaiting, serialization, timeout, break,
  and CSP controls.

The resulting design makes the isolation boundary, document identity, deadline,
and cleanup state explicit and measurable rather than relying on page policy or
timing luck.

## Build, lint, and deployment

- `node --check extensions/synapse-chrome-debugger/service_worker.js`: passed.
- `cargo fmt --all --check`: passed.
- `cargo check --workspace`: passed in 107.3 seconds.
- `pwsh -File scripts/lint.ps1 -SkipClippy`: both workspace contracts, formats,
  lock graphs, dependency policy, no-test doctrine, and the 47-code authenticated
  Chrome registry passed.
- canonical `pwsh -File scripts/lint.ps1`: all gates passed in both workspaces,
  including `cargo clippy --workspace --all-targets -- -D warnings` in root and
  Calyx (complete captured run: 64.9 seconds with warm incremental state).

Canonical setup built the clean `ae9917c9` checkout. During the optimized main
crate, physical CPU samples measured 946–980% process utilization on the
10-core/12-thread i7-1355U and about 4.7 GiB working set. It used the configured
`x86-64-v2`/AVX2 CPU path, LLD, the repository release cache, and no alternate
target directory. This host has no NVIDIA/NVML/CUDA device, so no faster truthful
GPU path exists.

Independent installed-state readback:

```text
installed daemon: C:\Users\hotra\.cargo\bin\synapse-mcp.exe
release image:    C:\code\synapse\target\release\synapse-mcp.exe
both bytes=257861961
both sha256=B3D1423F7D566A09FC49BD22D6E677278074E44720446C563DF8D66C22D56C7B
process/listener: PID 3640, installed image, 127.0.0.1:7700
authenticated health: ok=true build=ae9917c9efaa build_tree_state=clean
installed active worker bytes=1017186
installed active worker sha256=C7380D10D9E5F179DE38ED072F6D19DD072211AADB3AEE46EDB5D93492B30C27
bridge build=synapse-chrome-bridge-2026-08-11-csp-independent-wait-v8
bridge host=chrome-native-0-1786434839690
host_count=1 queued_count=0 pending_count=0 extension_stale=false
```

The setup status read exposed an unrelated stale July 28 bridge-pending
checkpoint despite current successful activation. Its record was preserved and
the defect was filed and independently verified as
[#2220](https://github.com/ChrisRoyse/Synapse/issues/2220).

## Manual Full State Verification

### Strict-CSP baseline

Before the trigger, the background GitHub target was complete at issue #2218;
its body contained the issue title. A same-origin authenticated fetch of the
exact page returned HTTP 200 and this enforcement header:

```text
default-src 'none'; ...; script-src github.githubassets.com; ...
```

It contains no `unsafe-eval`. The old installed generation had physically
failed the same page with `CHROME_SCRIPTING_EXECUTE_FAILED`; that durable record
is listed below. The owner ledger before the new triggers contained zero
debugger attachments, zero overrides/init scripts, zero unresolved debugger
timeouts, healthy browser-session continuity, and no persisted in-flight
mutation. Gmail was the only active/highlighted tab.

### Happy path

Synthetic known predicate:

```javascript
() => ({
  issue: location.pathname.endsWith('/2218'),
  titleHasCsp: document.title.includes('strict page CSP'),
  ready: document.readyState
})
```

Expected value was exactly
`{"issue":true,"titleHasCsp":true,"ready":"complete"}`. The real
`browser_wait` returned that object after one poll / 14 ms using
`chrome.debugger.Runtime.evaluate(isolated-world ...)`.

```text
initial_document_id=6B0A56A1E306D581F893A66688F9A179
final_document_id=6B0A56A1E306D581F893A66688F9A179
navigation_count=0
```

A separate later `Runtime.evaluate` read reproduced all three values and the
exact `https://github.com/ChrisRoyse/Synapse/issues/2218` URL.

### Three boundary/edge cases

| Case | Before and trigger | Expected | Independent after-state |
|---|---|---|---|
| Permanently false / deadline | #2218 complete; `() => false`, 350 ms, 100 ms polling | typed timeout, never transport/attach failure | `BROWSER_WAIT_TIMEOUT`, 5 polls; initial/final ID both `6B0A...A179`, navigation 0; separate URL/title/DOM read still #2218 complete |
| Invalid JavaScript | same #2218 state; `() => {` | dedicated caller-code failure | `CHROME_WAIT_PREDICATE_INVALID`, `SyntaxError: Unexpected token ')' line=3 column=40`; separate URL/title/DOM read still #2218 complete |
| Navigation during polling | #2218 complete; first false predicate poll schedules #2216 after 1,000 ms, subsequent polls wait for #2216 + `readyState=complete` | discard old-document results, rebind, then accept only new document | success after 18 polls / 2,226 ms; initial `4DAC7E...92DF`, final `75492A...02A2`, navigation 1; separate DOM read found exact #2216 URL/title/body and complete state |

Two preliminary external-timer attempts at 400 and 1,500 ms navigated before
the next MCP call acquired its initial document. Both correctly returned stable
new-document IDs and navigation 0, so neither was counted as navigation-race
evidence. The accepted case schedules the trigger inside the first false poll,
which proves the transition occurred after initial identity capture.

After the edge audit, the GitHub target was navigated back to #2218. A separate
tab query found exactly the original two tabs. GitHub remained background and
Gmail remained the only active/highlighted tab.

### Durable evidence and cleanup

The physical lifecycle ledger at
`C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-events.jsonl`
contains:

```text
pre-fix: PID 7640 seq 71 line 25806 status=error
         error=CHROME_SCRIPTING_EXECUTE_FAILED
         raw_sha256=2bffce6f74b3e410edeb26351e3649795a8ff1a21b05893fde86954416137039
happy:   PID 3640 seq 14 line 26204 status=ok
         raw_sha256=aaef9f66a37265832622a91fb0345ac0df23ca17232d48045d1be8c1fb86e5a6
timeout: PID 3640 seq 16 line 26208 status=error error=BROWSER_WAIT_TIMEOUT
         raw_sha256=5fefd196620242ed3d80f53661e61abc50ca9727d35875f91e2bfa8573766843
invalid: PID 3640 seq 18 line 26212 status=error error=CHROME_WAIT_PREDICATE_INVALID
         raw_sha256=eb34077743aac4e86878d6d464306d7b709fd4bd0ca42e9240a49778efed6993
nav:     PID 3640 seq 26 line 26228 status=ok duration_ms=2597
         raw_sha256=8111653b94848238f1045217f9a2febe7ecd8d8dc1430c5f1bc8a08853a062d1
```

The audit scan read 26,239 physical lines, returned complete matching rows, and
reported zero oversized/corrupt omissions. A final independent owner read found
`debugger_attached_tab_count=0`, every override/init-script/binding count zero,
`unresolved_debugger_command_timeout_count=0`, no unresolved worker mutation,
and healthy continuity. The temporary GitHub search/issue tab used to file
#2220 was closed; a separate `chrome.tabs.query` proved it absent.

No automated test, mock data, fallback lane, second browser, worktree, branch,
alternate build target, screenshot artifact, or temporary FSV directory was
created or used as acceptance evidence.
