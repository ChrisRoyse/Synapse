# FSV — #2177 exact Chrome fullscreen bridge reload

Date: 2026-08-11 (America/Chicago)  
Implementation commit: `9081d95e798dcf45aa3a8c2ff987b7a7454ac925` on `main`  
Acceptance comment: https://github.com/ChrisRoyse/Synapse/issues/2177#issuecomment-5251892024

## Source of Truth

No installer or MCP return value was accepted by itself. The independent
Sources of Truth were:

- Chrome's real `chrome.windows` state and `chrome.tabs.query` table in the
  already-open, authenticated browser profile;
- the fixture renderer's separately evaluated `document.fullscreenElement`,
  `body.dataset.fullscreen`, control label, and document visibility;
- the exact Windows UI Automation tree rooted at Chrome HWND 393954, including
  `TabContainerImpl` cardinality, selected tab runtime ID, the fullscreen
  control/owner runtime IDs, control bounds, and InvokePattern availability;
- the replacement native host registration and its loaded extension/worker
  hashes;
- Calyx `CF_ACTION_LOG` rows read independently through `audit command_query`;
- the installed executable bytes, OS process table, listener table, and durable
  setup phase ledger;
- durable Synapse shell-job status/output for the synthetic UIA observer and
  operator-selection trigger.

The minimal fixture was one real HTML file served by a loopback Python HTTP
server at `127.0.0.1:43177`. It contained one button whose trusted invocation
calls `document.documentElement.requestFullscreen()` or
`document.exitFullscreen()`, plus a visible `fullscreen=true|false` state. The
fixture was SHA-256
`A703884A4F9F645FC2F026194453EFD569464B4817372486B1E72C97E9440677`.

## Root cause and repair

Chrome content fullscreen removes the browser tab strip from its exposed UIA
tree. The installer required exactly one `TabContainerImpl`, so it treated that
valid state as an undifferentiated maintenance failure. It had no evidence to
distinguish content fullscreen, browser/F11 fullscreen, a minimized window, or
a genuinely corrupt accessibility surface, and consequently had no safe way to
reload the extension or restore the user's state.

Two related transaction defects were also present in the unpublished first
repair:

1. cleanup reselected the prior tab before closing the owned right-edge
   maintenance tab; Chrome's deterministic neighbor selection after the close
   was then misclassified as a newer operator choice;
2. the bridge-wide preexisting tab count was compared with the target window's
   UIA tab count, which made a second Chrome window look like corruption.

The final design is one exact, fail-closed transaction:

1. capture the Chrome HWND, PID, process start time, executable hash, exact
   selected tab runtime ID, and initial UIA tab-container count;
2. when the tab strip is absent, accept content fullscreen only if exactly one
   visible, enabled UIA button supplies InvokePattern and its name, automation
   ID, class, owner runtime ID, control runtime ID, and positive bounds prove an
   Exit-fullscreen control;
3. invoke that exact control asynchronously, then freshly require one
   `TabContainerImpl` and the same control identity in its inverse Enter state;
4. create, navigate, and later close only the exact owned maintenance tab,
   carrying the whole fullscreen transaction through the Rust boundary;
5. compare per-window UIA counts only with per-window baselines and validate
   browser-wide counts independently against the bridge-wide baseline;
6. after the owned close, account for Chrome's documented adjacent-tab
   selection, restore the original selection only when no newer selection is
   observed, and preserve a real operator selection verbatim;
7. restore content fullscreen only when the same original document remains
   selected; otherwise report `operator_superseded` and leave both selection
   and window state untouched;
8. refuse browser/F11 fullscreen as
   `SYNAPSE_CHROME_MAINTENANCE_FULLSCREEN_MODE_UNPROVEN` before mutation because
   it has no exact inverse content control.

There is no Escape/F11 keystroke, coordinate click, process-name guess,
best-effort restoration, generic zero-tab fallback, or legacy success path.
The public response now carries typed fullscreen-control, fullscreen-
transaction, prior-selection, per-window-count, and bridge-wide-count evidence.

Manual FSV found one more real runtime defect before acceptance: the new
PowerShell helper named its result array `$matches`. PowerShell variable names
are case-insensitive, while every successful `-match`/`-notmatch` expression
writes the automatic `$Matches` hashtable in the same scope. The first real
reload therefore failed loudly with
`SYNAPSE_CHROME_BRIDGE_HOST_RELOAD_PROCESS_FAILED` and `A hash table can only be
added to another hash table`; independent reads proved Chrome remained
fullscreen and all three tabs remained present. The collection is now
`$controls`, with an in-source warning against reusing the automatic variable.
The complete manual matrix was then rerun; no retry or fallback hid the fault.

## Research after diagnosis

`scripts/check-research-lane.ps1` performed a real MCP `initialize`,
`tools/list`, and `tools/call`. The physical readback at
`%TEMP%\synapse-research-lane-readback.json` was 1,626 bytes, SHA-256
`AC9441EA7CEFD7B8E4CA6FA718057F9F9914BD6454C8DAD27423773ACC452440`,
and reported Exa MCP 3.4.0 live. Exa research was supplemented by the built-in
web lane. Primary sources read:

- [Chromium FullscreenController implementation](https://chromium.googlesource.com/chromium/src/+/main/chrome/browser/ui/exclusive_access/fullscreen_controller.cc)
  and [interface](https://chromium.googlesource.com/chromium/src/+/100d19e69298b212cc638fbb6b18b53b2b6af344/chrome/browser/ui/exclusive_access/fullscreen_controller.h),
  which distinguish browser fullscreen from tab/content fullscreen and make
  restoration dependent on the current exclusive-access state;
- [Microsoft UI Automation Invoke control pattern](https://learn.microsoft.com/en-us/dotnet/framework/ui-automation/implementing-the-ui-automation-invoke-control-pattern),
  which specifies that Invoke must return without blocking and that clients
  must independently observe completion;
- [Microsoft UI Automation caching for clients](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-cachingforclients),
  which explains the staleness boundary between cached properties and fresh
  element reads;
- [WHATWG Fullscreen Standard](https://fullscreen.spec.whatwg.org/), which
  defines the document fullscreen element and exit algorithm;
- [Chrome windows API](https://developer.chrome.com/docs/extensions/reference/api/windows),
  which exposes browser-window state independently of a document's Fullscreen
  API state.

These sources support explicit mode classification, an exact reversible
control, asynchronous invocation followed by fresh state reads, and
identity-bound restoration. They do not support treating every fullscreen
window as equivalent or recovering with an unverified key press.

## Build, lint, deployment, and host optimization

- both PowerShell 7 and Windows PowerShell 5 parsed the installer successfully;
- `cargo check --workspace`: passed;
- canonical `pwsh -File scripts/lint.ps1`: passed all fail-closed format and
  clippy gates in both the root and Calyx workspaces;
- after the `$Matches` correction, both parsers and the cheap canonical
  `scripts/lint.ps1 -SkipClippy` gates passed before the complete final gate.

Canonical `scripts\synapse-setup.ps1 -SourceDir C:\code\synapse` built from
the repository and completed all 11 durable phases. The setup ledger records
`outcome=completed`, 819.277 total seconds, 693.211 release-build seconds,
12 logical processors, `CARGO_BUILD_JOBS=12`, and
`CMAKE_BUILD_PARALLEL_LEVEL=12`. The release configuration used
`target-cpu=x86-64-v2`, `rust-lld`, and 16 codegen units. Hardware probing found
no NVIDIA PnP device, NVML runtime, `nvcc`, or `CUDA_PATH`; the optimized and
truthful path on this host is therefore the CPU path, not a nonfunctional CUDA
configuration.

Independent installed-state readback after setup:

```text
installed image: C:\Users\hotra\.cargo\bin\synapse-mcp.exe
release image:   C:\code\synapse\target\release\synapse-mcp.exe
both bytes:      257960265
both sha256:     0D5F832DD7E80A137E0BF4CFC33414D3AA74CD2DC7C237AA8B30C7BCE8628EB3
live process:    PID 12464, installed image
listener:        127.0.0.1:7700 owned by PID 12464
installer sha:   8AA11A4C295C074CA2AE8029CADC5E6B65752F9B95BF114411E5534691FF9EBC
```

The successful accepted triggers executed the final installer bytes above; the
installed and release daemon bytes are identical.

## Manual Full State Verification

### Happy path: exact content-fullscreen restoration

Before the trigger, `chrome.tabs.query` showed exactly three tabs with fixture
`chrome-tab:589710205` active and Chrome window state `fullscreen`. A separate
renderer read returned `fullscreenElement=HTML`, `bodyState=true`, and control
label `Exit full screen (f)`. The independently queried UIA tree had zero
`TabContainerImpl` controls and exactly one eligible button:

```text
control runtime id: 42.26478256.4.744.8.3
owner runtime id:   42.26478256.4.744.8.2
automation id:      fs
class:              ytp-fullscreen-button ytp-button
name:               Exit full screen (f)
bounds:             x=828 y=268 width=264 height=99
```

`browser_debugger reload_bridge` replaced host
`chrome-native-0-1786441897577` with
`chrome-native-0-1786442309958`. Its typed transaction recorded mode
`content_fullscreen_exact_uia_control`, all of `detected`, `exit_attempted`,
`exit_verified`, `restore_required`, `restore_attempted`, and `restored` true,
UIA container counts `0 -> 1 -> 0`, and outcome
`restored_content_fullscreen`. The exact owned tab closed `4 -> 3`, was absent,
and the replacement bridge independently found three preexisting tabs, zero
missing tabs, zero concurrent tabs, and no maintenance token.

A new and separate Chrome query still found exactly the same three tab IDs,
fixture active, and the window fullscreen. A new renderer read again returned
`HTML`, `true`, and `Exit full screen (f)`. A fresh UIA read again found zero
tab containers and the same fullscreen control runtime ID. `CF_ACTION_LOG`
contained an `ok` physical `cdp_bridge_reload` row and no corrupt or
noncanonical rows.

### Normal-window control case

Before the trigger, Chrome independently reported `state=normal`, the fixture
renderer reported `fullscreenElement=null`, `bodyState=false`, label
`Enter full screen (f)`, one exact tab container, and the same three tabs.
Reload replaced host `chrome-native-0-1786442309958` with
`chrome-native-0-1786442412449`. The transaction said mode `none`,
`detected=false`, `exit_attempted=false`, outcome `not_required`, and every
container count remained one. A separate post-trigger query reproduced the
normal window, null renderer fullscreen state, one container, exact original
selection, and exactly three tabs.

### Boundary and edge-case audit

| Case | Before and trigger | Expected | Independent after-state |
|---|---|---|---|
| Browser/F11 fullscreen is unproven | Target the exact Chrome HWND and send real F11 through the audited foreground lane. Chrome then said `state=fullscreen`; renderer remained `fullscreenElement=null`, `bodyState=false`, label `Enter full screen (f)`; UIA exposed zero tab containers and no Exit control. Trigger reload. | Fail before any maintenance mutation; never guess that F11 is a reversible content control. | `SYNAPSE_CHROME_MAINTENANCE_FULLSCREEN_MODE_UNPROVEN`, `exit_control_match_count=0`. Separate browser, renderer, and UIA reads were byte-for-byte equivalent in meaning: browser fullscreen, document not fullscreen, exact same three tabs. A second real F11 returned the window to `normal`. |
| Invalid target alias | Normal window, fixture active, exact three tabs; call the bridge-wide reload with a top-level CDP target alias. | Typed validation failure before mutation. | `TOOL_PARAMS_INVALID` explained that reload does not address one target and must drop target fields. Separate `chrome.tabs.query` still returned the same state and three tab IDs. |
| Newer operator selection during cleanup | Content fullscreen, fixture selected, exact three tabs. A durable UIA observer waited for exact maintenance tab `Extensions - Synapse Chrome Bridge`, then selected only the existing Gmail tab by SelectionItemPattern 6.5 seconds later. Before selection the exact maintenance runtime `42.393954.4.0.0.226586` was selected; afterward Gmail runtime `42.393954.4.0.0.10819` was selected. | Close only the owned maintenance tab, preserve Gmail, and do not restore fullscreen over a different selected document. | Transaction reports `prior_selection_restore.operator_superseded=true`, reason `preserved_newer_operator_selection`, before/after runtime `...10819`, and expected original `...221093`; fullscreen transaction reports `operator_superseded=true`, `restore_attempted=false`, `restored=false`, outcome `operator_superseded`. Independent Chrome query found Gmail solely active/highlighted, `state=normal`, the exact original three tab IDs, and no maintenance tab. Separate fixture renderer read found `fullscreenElement=null`, `bodyState=false`, `visibility=hidden`; exact UIA read found one `TabContainerImpl`, three Chrome tab items, and Gmail runtime `...10819` solely selected. |

The supersession trigger's durable shell job exited 0 with no stderr. Its own
physical log recorded `maintenance_exact_observed` at 11,915 ms, the first
Gmail selection at 18,720 ms with `selected_after=true`, and continued exact
readback through attempt 19. The reload independently completed, replaced host
`chrome-native-0-1786443009347` with
`chrome-native-0-1786443081941`, closed only runtime
`42.393954.4.0.0.226586`, and preserved all three preexisting tabs. The newest
independent `CF_ACTION_LOG` row is status `ok`, key
`18cab886de012ed400000078`, value SHA-256
`9ca55326a53da9331ba6a1b9dfec7da35e8d54750203e42f48f33acb9b4be687`;
the audit scan reported zero corrupt and zero noncanonical rows.

## Cleanup and final state

The exact synthetic tab `chrome-tab:589710205` was closed through the
ownership-checked Chrome operation, whose separate absence readback returned
zero rows. Durable loopback job `019ff03e-fd3e-7d72-bdfc-b87aabce10bd` was
canceled and reported an empty owned process tree. A separate OS read found PID
10396 absent and zero TCP connections on port 43177. The fixture file was
deleted with an independent `Test-Path=false` readback.

A final `chrome.tabs.query` returned only the two original tabs: the GitHub
issues tab `chrome-tab:589710129` and Gmail `chrome-tab:589710138`. Gmail was
the sole active/highlighted tab, Chrome state was `normal`, and Gmail document
contents were never read or mutated. A fresh GitHub document reload found the
acceptance comment permalink above, the implementation marker, zero `Close
issue` buttons, one `Reopen issue` button, and the physical timeline event
`ChrisRoyse closed this as completed`.

No automated test, mock data, fallback lane, second browser, branch, worktree,
alternate target directory, screenshot artifact, or temporary repository FSV
directory was created or used as acceptance evidence.
