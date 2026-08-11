# Issue #2167 Full State Verification — truthful browser input provenance

Date: 2026-08-11 (America/Chicago)

## Outcome

Accepted against the installed production daemon and the user's already-open,
authenticated Chrome. Every successful input surface returned the same typed,
versioned `synapse.input_provenance.v1` contract. DOM dispatch, HTML activation,
Chrome debugger input, and Win32 `SendInput` were distinguishable without
calling any software lane physical input. Missing or competing legacy trust
fragments fail closed with `ACTION_INPUT_PROVENANCE_INVALID`.

Implementation commits:

- `7af87aa8e6ccda272dd63e66703cb12b0ad0033f` — typed contract and public-surface projections.
- `846c6824` — FSV-discovered correction: remove and centrally reject parallel legacy provenance fragments.

No automated tests, test harness, mock data, or CI were used. Acceptance was
manual, against physical browser/OS/vault state.

## Diagnosis and research

The root cause was not one wrong label. Input facts were independently inferred
at several response layers: `input_trust`, `real_trusted_input`,
`native_default_actions`, backend tier names, and free-form prose. Those
fragments could not represent multi-emission actions and could contradict one
another. In particular, `HTMLElement.click()` produces an untrusted click while
still running HTML activation behavior; a single `trusted/untrusted` scalar is
not a complete input-origin contract.

Research was performed after diagnosis through both required lanes:

- Exa MCP: `scripts/check-research-lane.ps1` returned `live`; the real
  `exa-search-server` 3.4.0 `web_search_exa` call succeeded. Readback:
  `%TEMP%\synapse-research-lane-readback.json`.
- Built-in web research, using primary sources:
  - WHATWG DOM event trust and `click()` legacy exception: <https://dom.spec.whatwg.org/>
  - WHATWG HTML activation and input semantics: <https://html.spec.whatwg.org/multipage/interaction.html>
  - Chrome DevTools Protocol Input domain: <https://chromedevtools.github.io/devtools-protocol/1-2/Input/>
  - W3C WebDriver virtual input sources: <https://www.w3.org/TR/webdriver/all/>
  - Microsoft `SendInput` semantics/UIPI: <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-sendinput>
  - Microsoft UI Automation Invoke semantics: <https://learn.microsoft.com/en-us/dotnet/api/system.windows.automation.invokepattern.invoke>

The implemented boundary follows those semantics:

- DOM dispatch: `isTrusted=false`; no user-agent input defaults.
- HTML activation method: `isTrusted=false` for the delivered click, while HTML
  activation behavior may produce its specified state/events.
- CDP / `chrome.debugger`: user-agent-created DOM events, expected
  `isTrusted=true`, but still software-originated (`physical_device_origin=false`).
- `SendInput`: software-originated OS input, target-gated by the actual Windows
  foreground and a per-emission fence.

## Build, lint, deployment, and runtime identity

The following gates passed before the accepted deployment:

```text
cargo check --workspace
  Finished dev profile; exit 0

pwsh -File scripts/lint.ps1
  LINT OK: every gate passed in BOTH workspaces (root and calyx).
```

Production deployment used only:

```text
pwsh -File scripts/synapse-setup.ps1 -SourceDir C:\code\synapse
```

Hardware/build readback:

```json
{
  "cargo_build_jobs": 12,
  "cmake_build_parallel_level": 12,
  "logical_cpus": 12,
  "cuda_kernels": false,
  "cuda_basis": "nvidia_pnp_devices=0; nvcc=not_found; CUDA_PATH=unset"
}
```

The installer completed its release build, bundled-model verification,
isolated candidate-vault bootstrap/search check, graceful daemon handoff, and
Chrome bridge reconnect. Its final exit was deliberately nonzero only because
the already-running Codex process retained its start-of-process tool schema
snapshot (`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE`). This is not used as the
installation verdict. Independent OS/disk reads proved the installed state:

```json
{
  "pid": 22940,
  "path": "C:\\Users\\hotra\\.cargo\\bin\\synapse-mcp.exe",
  "length": 258200905,
  "sha256": "CFBBFA9B03F9A9061ADE954762EB09B54E8255E2E56A3CB07216BCB5C00B6C9E"
}
```

The first browser read after deployment refused the stale worker. The
authenticated `browser_debugger.reload_bridge` path then independently read the
replacement runtime worker:

```json
{
  "before_host": "chrome-native-0-1786466579242",
  "after_host": "chrome-native-0-1786466628794",
  "runtime_service_worker_sha256": "7f02b74f6abc2c239b876b2e0da5478549c0570876693aff5e5e87f7582bd2ac",
  "expected_service_worker_sha256": "7f02b74f6abc2c239b876b2e0da5478549c0570876693aff5e5e87f7582bd2ac",
  "stale": false,
  "reconnected": true
}
```

## Sources of Truth

1. Browser outcome: the real page DOM (`#state`, `#text`, `#check`, and
   `#counter`) read separately through `browser_dom` / debugger evaluation after
   each trigger.
2. Browser target existence/ownership: the live `chrome.tabs` table read through
   `browser_tabs`.
3. OS delivery: `GetForegroundWindow`, process identity, `GetCursorPos`, live UIA
   focused element/value, and the page's trusted DOM event log.
4. Installed runtime: live OS process table plus the installed executable and
   runtime extension byte hashes.
5. Durable audit: physical `CF_ACTION_LOG` rows, Calyx `MANIFEST`/`CURRENT`,
   `daemon-tool-events.jsonl`, and an independently re-walked CF_LEDGER range.

Fixture: an in-memory HTTP response served on `127.0.0.1:18765` by exact helper
PID 5668 into a Synapse-owned background tab
`chrome-tab:589710231`. The page recorded event type, `isTrusted`, default
prevention, target, value/checked state, active element, URL hash, and counter.
No fixture file was written. The helper and tab were removed after verification.

## Baseline

Independent page read:

```json
{"state":"[]","text":"","checked":false,"counter":0,"title":"Synapse #2167 FSV","url":"http://127.0.0.1:18765/"}
```

Independent OS/vault read:

```json
{
  "foreground_hwnd": 394276,
  "foreground_pid": 3120,
  "chrome_hwnd": 393954,
  "chrome_owns_foreground": false,
  "cursor": [1341,700],
  "mouse_present": 1,
  "current": "manifest-00000000000000050932.json",
  "manifest_sha256": "BF0F6EDCCD2C20BD5C1505982E11041171FBD7C9F9D4F30C9FCA17F8B8F0150A",
  "daemon_tool_events_length": 7927559
}
```

## Happy paths

### 1. Synthetic DOM dispatch and HTML activation projection

Trigger: `act invoke`, click the resolved page button.

Typed response contained two real emissions:

```json
[
  {
    "delivery_origin":"dom_dispatch",
    "expected_dom_event_is_trusted":false,
    "physical_device_origin":"false",
    "browser_default_actions":"synthetic_dispatch_no_user_agent_input_defaults",
    "backend":"chrome.scripting.executeScript",
    "transport":"chrome_tabs_extension+chrome.scripting",
    "protocol_method":"dispatchEvent(PointerEvent/MouseEvent press sequence)",
    "foreground":{"required":false,"before":{"hwnd":394276},"after":{"hwnd":394276}}
  },
  {
    "delivery_origin":"html_activation_method",
    "expected_dom_event_is_trusted":false,
    "physical_device_origin":"false",
    "browser_default_actions":"synthetic_click_may_run_activation_behavior",
    "protocol_method":"HTMLElement.click"
  }
]
```

The delegated result contained none of the seven forbidden parallel legacy
fields. Independent DOM readback:

```json
{"event_count":5,"all_untrusted":true,"types":["pointerdown","mousedown","pointerup","mouseup","click"],"counter":1}
```

### 2. Chrome debugger protocol input

Trigger: `act invoke` touch tap at the known button center `(44,92)` in viewport
coordinates.

```json
{
  "delivery_origin":"chrome_debugger_protocol",
  "expected_dom_event_is_trusted":true,
  "physical_device_origin":"false",
  "browser_default_actions":"user_agent_input",
  "backend":"chrome.debugger.Input",
  "transport":"chrome_tabs_extension+chrome.debugger",
  "protocol_method":"Emulation.setTouchEmulationEnabled + Input.dispatchTouchEvent(touchStart,touchEnd)",
  "foreground":{"required":false,"before":{"hwnd":394276},"after":{"hwnd":394276}}
}
```

Independent DOM readback:

```json
{"event_count":5,"all_trusted":true,"types":["pointerdown","pointerup","mousedown","mouseup","click"],"counter":1}
```

This is the important cross-lane fact: the DOM events were trusted, while the
typed physical-origin field remained false.

### 3. HTML checkbox activation

Before:

```json
{"checked":false}
```

Trigger: `browser_form fill`, checkbox `#check` to `true`.

```json
{
  "delivery_origin":"html_activation_method",
  "expected_dom_event_is_trusted":false,
  "physical_device_origin":"false",
  "browser_default_actions":"html_activation_behavior",
  "protocol_method":"HTMLElement.click",
  "legacy_result_fields_present":[]
}
```

Independent after-state:

```json
{
  "checked":true,
  "event_tail":[
    {"type":"click","isTrusted":false,"checked":true},
    {"type":"input","isTrusted":true,"checked":true},
    {"type":"change","isTrusted":true,"checked":true}
  ]
}
```

This exact experiment exposed the first deployed build's contradiction: its
new typed record said HTML activation, but its legacy delegated scalar still
said no browser defaults. Acceptance stopped, commit `846c6824` removed that
parallel authority, the project was rebuilt/redeployed, and the entire matrix
was restarted from an empty page.

### 4. Form value replacement

Before value `""`; requested `"FSV-CORRECTED"`.

```json
{
  "status":"verified_state",
  "requested_len":13,
  "after_len":13,
  "independent_readback_len":13,
  "all_three_sha256_equal":true,
  "delivery_origin":"dom_dispatch",
  "expected_dom_event_is_trusted":false,
  "browser_default_actions":"scripted_mutation_plus_synthetic_notifications"
}
```

Separate `browser_dom inspect` read: `value="FSV-CORRECTED"`.

### 5. Real Windows foreground `SendInput`

The initial type attempt without a physical web-content click failed closed
with `ACTION_NO_OBSERVED_DELTA`; the DOM remained byte-for-byte unchanged even
though document/UIA focus appeared true. This was retained as an edge result,
not treated as delivery.

The accepted OS path explicitly established Windows keyboard focus with a real
foreground click on the independently observed UIA edit box, then typed one
known character after placing the caret at the far-right interior.

Pre-emission OS state:

```json
{"foreground_hwnd":393954,"foreground_pid":11340,"chrome_owns_foreground":true,"cursor":[1341,700]}
```

Both click and text returned:

```json
{
  "delivery_origin":"os_send_input",
  "physical_device_origin":"false",
  "browser_default_actions":"os_input_pipeline",
  "backend":"software",
  "transport":"synapse_action+win32_sendinput",
  "protocol_method":"SendInput",
  "foreground":{"required":true,"target_hwnd":393954,"before":{"target_owned":true},"after":{"target_owned":true},"per_emission_fence_verified":true}
}
```

Known state equation:

```text
before = FSV-CORREZCTED-OS
input  = Y at proven end caret
expect = FSV-CORREZCTED-OSY
actual = FSV-CORREZCTED-OSY
```

The independent page event tail was trusted `click`, `keydown`, `input`,
`keyup`; the input event carried the exact expected value. The foreground facade
also proved profile restoration and lease cleanup (`released_lease=true`,
`profile_restored=true`, `session_holds_lease_after=false`).

## Boundary and edge-case audit

### Edge 1 — empty replacement is a real clear

```json
{
  "before":"FSV-CORREZCTED-OSY",
  "request":"",
  "trigger":{"status":"verified_state","requested_len":0,"before_len":18,"after_len":0,"independent_len":0},
  "after":{"actual":"","expected":"","matches":true}
}
```

### Edge 2 — exact field maximum (64 characters)

```json
{
  "before":"",
  "request":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "trigger":{"status":"verified_state","requested_len":64,"after_len":64,"independent_len":64,"all_hashes_equal":true},
  "after":{"actual_len":64,"exact_match":true}
}
```

### Edge 3 — invalid empty DOM event type

Before and after were separately read from the page:

```json
{"activeId":"text","checked":true,"counter":1,"eventCount":32,"hash":"","value":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
```

Trigger refused before mutation:

```text
TOOL_PARAMS_INVALID: target_act verb=dispatch_event requires non-empty event_type
```

The complete before/after object was identical.

### Edge 4 — contradictory facade target aliases

Top-level target `chrome-tab:589710231` plus nested target
`chrome-tab:589710129` failed before mutation with `TOOL_PARAMS_INVALID`, naming
both targets and the repair. Separate DOM readback proved the 64-character field
value unchanged.

### Edge 5 — stale/closed target

Before live `chrome.tabs` read:

```json
{"target_present":true,"target":"chrome-tab:589710231","tab_count":3}
```

After the owned fixture tab was closed, using its old element/target identity
failed with `ACTION_TARGET_INVALID`: the target was neither active nor owned.
Separate `chrome.tabs` read:

```json
{"target_present":false,"remaining_tabs":["chrome-tab:589710129","chrome-tab:589710138"]}
```

Closing the last fixture tab made Chrome select the adjacent Gmail tab by its
own tab-close behavior. Synapse immediately restored the GitHub tab through
background `chrome.tabs.update`, and final readback was GitHub active, Gmail
inactive, Chrome not OS foreground. No Gmail DOM/content action occurred.

## Durable evidence after execution

Selected physical `CF_ACTION_LOG` readbacks:

```text
act foreground SendInput final OK
  key=18cace65ff3297880000006b
  sha256=269b3f21804c017bb7881aca1850f8774973c001f4ab6909275b245066ff29a5

act_type SendInput final OK
  key=18cace65fa6971400000004c
  sha256=df2a8e8d43b5f26242caed297b75fd3cdf0ddff38d0911cebb73e593564cb3e1

browser_set_value empty final OK
  key=18cace7500f87fc400000056
  sha256=f9546c71b787b28d0d3f466c7384d0370064f6530f7a6b193f2e95ca1c39fb25

browser_set_value max-64 final OK
  key=18cace77cb85eacc0000005c
  sha256=dbd3e8f4b2103675b125015f4e65e625b430d550098eb1269468e60b3156a837

browser_fill_form checkbox final OK
  key=18cace040fc0c43800000021
  sha256=6f4b8bece6da89d3864f57bf7068cbbad34b7ce5720791f321a5c78c7fbb8e29

invalid event final ERROR/TOOL_PARAMS_INVALID
  key=18cace7d313024280000007d
  sha256=9e4976efe6ec4221210dc25620646928be2a504436f1d92a28fb8d6bd7884ac4

unfocused OS type final ERROR/ACTION_NO_OBSERVED_DELTA
  key=18cace3fa7b81ef40000003a
  sha256=19919bb928ca9cc7ef5d2336f0c3c8f561ff1098d7df7d89e75a1d780eac64d3
```

Incremental physical chain verification:

```json
{
  "verdict":"intact",
  "intact":true,
  "verified_from_seq":351232,
  "verified_to_seq":351332,
  "entry_count":100,
  "tip_hash":"99705049454546b9f6486a82d76975b7fae55dc1b9cb0b36df9c90ddcd42d810",
  "read_seq":351331,
  "read_entry_hash":"a1b46f2655070762343a6a9d37cb8477dcaa03a04b55791322cab4286576f43f",
  "read_entry_self_verifies":true,
  "raw_commitments_intact":true,
  "raw_commitment_pending_count":0,
  "vault_generation":1,
  "vault_reset_count":0
}
```

Final physical vault/runtime read:

```json
{
  "current":"manifest-00000000000000050953.json",
  "manifest_seq":50953,
  "durable_seq":1324306,
  "manifest_sha256":"D3582D0EE08245F1E0322560F528725D2013FC973981EDA365A1AC83B06E257F",
  "daemon_tool_events_length":8074177,
  "daemon_pid":22940,
  "daemon_sha256":"CFBBFA9B03F9A9061ADE954762EB09B54E8255E2E56A3CB07216BCB5C00B6C9E"
}
```

## Cleanup and final operator state

The fixture tab was absent from the final Chrome table. Exact helper PID 5668
was stopped only after verifying its path and HTTP response; independent after
read showed process absent, port 18765 not listening, and HTTP unreachable.

```json
{
  "foreground_hwnd":394276,
  "foreground_pid":3120,
  "foreground_restored":true,
  "cursor":[1341,700],
  "cursor_restored":true,
  "github_tab_active":true,
  "gmail_tab_active":false,
  "chrome_os_foreground":false,
  "fixture_helper_exists":false,
  "fixture_port_listening":false
}
```

