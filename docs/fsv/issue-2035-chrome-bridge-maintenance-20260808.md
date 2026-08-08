# Issue #2035 — Chrome bridge maintenance FSV (2026-08-08)

## Scope

This report records manual Full State Verification of the concurrency-safe Chrome
maintenance-tab changes shipped in commit `7f8278d5`. The trigger was the real
wired production MCP client. No test, harness, helper driver, alternate browser,
or direct storage mutation was used as the behavioral trigger.

## Source of Truth before the trigger

- MCP session: `2e60e771-ba89-46cd-b20c-35fc9ca78117`.
- Strict public tool surface: 40 tools; SHA-256
  `510eee457f36741a1bd693e537fb16cb8fd90fa2e4383c0562cbb310b767b6bf`.
- Daemon: PID `48496`, build `162b3a2e1dcc`, sole listener
  `127.0.0.1:7700`.
- Browser: one Chrome root process, PID `31384`, HWND `328518`; no second
  Chrome profile or root process.
- `browser_tabs operation=list`: 13 physical tab IDs, one active
  (`chrome-tab:600768012`). The ordered ID set SHA-256 was
  `959694a25c17bc307766fcf75d73c009554663c8fd771dd70503bdb38ad63ad0`.
- Chrome bridge: one clean authenticated host
  `chrome-native-0-1786206297426`; `extension_stale=false`; expected build and
  service-worker hashes matched.
- Session profile row: `normal_agent` in
  `CF_SESSIONS mcp/tool-profile/v1/<session_id>`.

## Trigger

The profile was set to `browser_debugger` with explicit confirmation and reason,
then separately read back from `CF_SESSIONS`. The real MCP trigger was:

```text
browser_debugger {
  operation: reload_bridge,
  reload_bridge: { wait_timeout_ms: 30000 }
}
```

The command used
`chrome_extensions_exact_reload_or_load_unpacked_control`. The installer exited
zero in 8,437 ms with reason `existing_ready_extension_ui_reload_invoked`.
The UI readback reported the exact Reload button present and the extension enable
toggle on before and after. The profile row remained installed and ready. The
daemon separately observed replacement host
`chrome-native-0-1786207114581` after 7 ms.

## Separate physical readback after the trigger

- A new `browser_tabs operation=list` returned the exact same ordered 13 tab
  IDs and the same active tab. No operation-owned maintenance/Extensions tab
  remained.
- A separate Windows UI Automation read of HWND `328518` found exactly one
  `TabContainerImpl`, 13 child `TabItem` runtime IDs, and exactly one selected
  item. The ordered runtime-ID set SHA-256 was
  `4cc767d6f373ecba27238cc1e2092c288040b92084a1dc5be2cd956b7d69ad04`.
- A separate health read found one host, zero pending/queued bridge commands,
  `tab_control_available=true`, `extension_stale=false`, and service-worker
  hash status `ok`; its active host was exactly the replacement identity.
- The Chrome root process remained PID `31384`; daemon/listener remained PID
  `48496`. Only reload-owned Chrome child workers changed.
- The physical event ledger
  `%LOCALAPPDATA%\synapse\db-daemon\daemon-tool-events.jsonl` recorded seq 17,
  route `browser_debugger.reload_bridge`, profile `browser_debugger`,
  `status=ok`, duration 8,757 ms.
- The session profile was restored and separately read back as `normal_agent`.

## Manual boundary and edge audit

Before and after every rejected operation, independent health, `browser_tabs`,
and foreground reads printed the same replacement host and the same 13-tab set.

| Case | Expected and actual result | Physical after-state |
|---|---|---|
| Wrong profile (`normal_agent`) | `TOOL_PROFILE_POLICY_DENIED`; event seq 34, error in 38 ms | host and all 13 tab IDs unchanged |
| Structurally invalid whole-bridge target (`cdp_target_id` supplied) | `TOOL_PARAMS_INVALID`; event seq 39, error in 25 ms | host and all 13 tab IDs unchanged |
| Empty/boundary-invalid wait (`wait_timeout_ms=0`) | `TOOL_PARAMS_INVALID`, accepted range `1..=30000`; event seq 43, error in 24 ms | host and all 13 tab IDs unchanged |
| Maximum valid wait (`30000`) | successful real reload described above | replacement host current; baseline tabs exact |

The FSV also retained the earlier real failure shapes documented on #2035:
background-provider `chrome_tab_container_count=0`, simultaneous operator typing
against global key chords, and exact runtime-id cleanup.

## Research and follow-ups

Microsoft documents that UI Automation cache state is valid only until the UI
changes and that clients must obtain an updated snapshot. It also documents
`IUIAutomationValuePattern::SetValue` as the exact-element value-setting control
pattern. The shipped foreground-first reacquisition and exact
`ValuePattern.SetValue`/runtime-id flow follows those constraints:

- <https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-cachingforclients>
- <https://learn.microsoft.com/en-us/windows/win32/api/uiautomationclient/nf-uiautomationclient-iuiautomationvaluepattern-setvalue>

The run exposed two separate gaps, tracked rather than hidden:

- #2158: maintenance must restore or explicitly yield the pre-operation OS
  foreground without stealing a later human foreground.
- #2159: the public/durable reload evidence must expose a bounded, redacted
  projection of the already-verified maintenance lease and cleanup result.

Neither gap changes the #2035 verdict: exact tab creation, navigation, reload,
cleanup, baseline preservation, and fail-closed validation all ran against the
configured physical browser.
