# Browser and Web Perception

Synapse can see ordinary Windows UI through UI Automation, but modern browser
page content is different: Chrome, Edge, Brave, Vivaldi, Opera, and Chromium
usually expose only a shallow browser chrome tree through UIA. For page DOM
nodes, Synapse attaches to the browser's Chrome DevTools Protocol (CDP) endpoint
and reads `Accessibility.getFullAXTree` plus DOM box data.

## When CDP Attaches

`observe` and `find` probe the foreground Chromium-family process on every call.
The probe checks, in order:

1. A CDP port registered by `act_launch` for that browser process.
2. Ports listed in `SYNAPSE_CDP_PORTS`.
3. The conventional default port `9222`.

When the probe finds a reachable endpoint, the async browser path attaches CDP,
pulls the accessibility tree, resolves DOM bounds, and merges the resulting web
nodes into the normal element list. Those nodes are queryable through `find` and
actionable by `act_click`, `act_type`, and `act_stroke`.

When no endpoint is reachable, Synapse does not silently pretend the DOM was
observed. For the user's normal Chrome profile, Synapse can use the bundled
Chrome extension direct-localhost WebSocket bridge for background tab control
through `chrome.tabs`. DOM attach through the debugger API is intentionally
unavailable in the normal end-user bridge. If no CDP endpoint is available, the
UIA tree is still returned, but it is the browser shell, not the page DOM.
The normal-profile `chrome.tabs` bridge is also refused at runtime when the
current Chrome profile/process readback shows any external extension or native
host with `debugger` or `nativeMessaging`, because those surfaces can create the
same operator-visible debugger/native-host popup when tab events occur.
On unsafe hosts, the daemon refuses the direct bridge registration itself with
`A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED`, and the extension stays dormant until
Chrome or the extension restarts instead of repeatedly waking the normal Chrome
profile.

## Diagnostics

Browser observations carry diagnostics so agents can tell the difference between
"there is no button" and "the browser page was not reachable through CDP."

- `diagnostics.cdp.status = "ok"`: a CDP endpoint is reachable. After the DOM
  attach succeeds, `diagnostics.web_path = "cdp"` and page nodes should appear in
  `elements`. `diagnostics.cdp.checked_ports` and
  `diagnostics.cdp.checked_endpoints` show the loopback probes that led to the
  reachable endpoint.
- `diagnostics.cdp.status = "A11Y_CDP_UNREACHABLE"` with matching
  `reason_code = "A11Y_CDP_UNREACHABLE"`: the foreground process is Chromium
  family, but no probed debug port accepted a connection. The diagnostics
  include the exact localhost ports/endpoints checked and a detail string that
  distinguishes raw CDP HTTP attach from Chrome's newer auto-connect permission
  flow. The normal end-user Chrome bridge remains available for `chrome.tabs`
  background tab control, but it does not upgrade DOM observation through
  `chrome.debugger`.
- `diagnostics.cdp.status = "A11Y_CDP_EXTENSION_UNAVAILABLE"` with matching
  `reason_code = "A11Y_CDP_EXTENSION_UNAVAILABLE"`: raw CDP was unreachable and
  the normal-profile Chrome extension loopback WebSocket bridge is not
  connected. The detail field names the bundled extension directory, expected
  extension ID, and registration script. Synapse then attempts OCR over tiled
  browser content. If readable text is found,
  `diagnostics.web_path = "ocr"` and OCR text nodes appear in `elements`; if OCR
  has no usable text or capture is unavailable, `web_path` remains `uia_only`.
- CDP attach errors use the same diagnostics object with
  `diagnostics.cdp.status = "A11Y_CDP_ATTACH_FAILED"`, matching
  `reason_code = "A11Y_CDP_ATTACH_FAILED"`, and a detail string. Synapse may
  still recover readable text through OCR, but the CDP failure remains visible
  in diagnostics.
- Non-browser foreground windows leave `diagnostics.cdp` and `web_path` unset.

The intended strategy ladder is:

1. Raw CDP DOM and accessibility tree for Chromium page content when a real
   loopback debug endpoint exists.
2. Non-attach Chrome extension `chrome.tabs` navigation for normal-profile
   background tab open, close, navigate, reload, back, and forward over the
   direct localhost WebSocket bridge, only after the live profile/process
   Source of Truth is popup-free.
3. OCR/capture over tiled browser content when CDP is down or attach fails.
4. Explicit `uia_only` for browser chrome/native UI when neither DOM nor OCR
   produced page content.

## Launching a Browser for DOM Access

Use Synapse `act_launch` for browser sessions that need DOM perception. For a
Chromium-family target, `act_launch` injects a remote-debugging port and a
dedicated automation profile unless the caller already supplied
`--remote-debugging-port` or `--user-data-dir`.

That dedicated profile is intentional. Chrome 136 and newer ignore
`--remote-debugging-port` on the default user profile unless a non-default
`--user-data-dir` is also supplied. A normally launched primary-profile Chrome
can therefore be impossible for Synapse to inspect through CDP.

Chrome 144 and newer also have a user-consented auto-connect flow at
`chrome://inspect/#remote-debugging`. That flow is not the same thing as a raw
`http://127.0.0.1:<port>/json/version` endpoint and cannot be discovered by
adding ports to `SYNAPSE_CDP_PORTS`. It requires a client that speaks Chrome's
auto-connect permission protocol, or a native browser bridge such as the
Synapse Chrome extension path tracked separately.

By default the automation profile is temporary and per launch. Set
`SYNAPSE_CDP_USER_DATA_DIR` when a stable automation profile is needed, for
example to keep a login session across runs. Do not point it at the user's
primary browser profile.

## Already-Running User Browsers

Prefer the already-running authenticated browser when the workflow depends on
that user's cookies, tabs, extensions, or site state. Synapse attaches to that
session when the browser exposes a reachable CDP endpoint; it does not need a
separate automation profile in that case.

Synapse cannot turn a loopback CDP socket on from outside an already-running
Chrome/Edge process that was launched without remote debugging or without the
browser's own local remote-debugging consent. For an existing authenticated
Chrome session, the supported attach path is:

1. Read `observe.diagnostics.cdp`.
2. If it is `ok`, use `diagnostics.cdp.endpoint` and require
   `diagnostics.web_path = "cdp"` after attach before assuming DOM access.
3. If it is `A11Y_CDP_UNREACHABLE`, read
   `diagnostics.cdp.checked_ports`, `checked_endpoints`, and `detail`; those are
   the actual endpoints Synapse checked.
4. If the existing browser has a known remote-debugging port not listed there,
   set `SYNAPSE_CDP_PORTS` to include that port and restart only the Synapse
   daemon, not the browser.
5. If Chrome's own UI supports enabling remote debugging for the running
   browser, treat that as the Chrome 144+ auto-connect permission flow. Do not
   expect it to make `/json/version` reachable through `SYNAPSE_CDP_PORTS`.
6. For the user's normal Chrome profile, install the bundled extension at
   `extensions/synapse-chrome-debugger` and verify the direct localhost bridge
   with `scripts\install-synapse-chrome-debugger.ps1`. The stable extension ID
   is `leoocgnkjnplbfdbklajepahofecgfbk`. The normal bridge must not request
   `nativeMessaging`; Chrome can launch native hosts through a visible
   `cmd.exe` intermediary on Windows. The extension uses a daemon-issued bridge
   token and a 20s WebSocket keepalive while connected. It must not request
   `chrome.alarms` or any other recurring wakeup permission. If registration,
   message post, or WebSocket keepalive fails, or if the daemon refuses
   registration because the live Chrome profile/process SoT is unsafe, the
   service worker logs the exact error and remains dormant until Chrome or the
   extension restarts.
   If Chrome loads the unpacked directory under any extension ID other than
   `leoocgnkjnplbfdbklajepahofecgfbk`, the service worker remains dormant before
   making any daemon HTTP request.
   The bridge hello reports extension version, protocol version, build ID,
   build SHA-256, user agent, and command capabilities. Daemon health treats
   missing or mismatched identity as extension skew and command preflight fails
   closed with `CHROME_BRIDGE_EXTENSION_STALE`.
   The verifier removes stale Synapse native-host registration from all Windows
   Chrome lookup hives (`HKCU`/`HKLM`, 32-bit and 64-bit views) and fails closed
   with per-key readback/ACL evidence if any Synapse native-host registration
   remains. It also reads Chrome profile permissions for the live Synapse
   extension ID and fails closed if an older load still has `alarms`,
   `debugger`, or `nativeMessaging` active.
7. Use the non-attach `chrome.tabs` bridge for normal-profile Chrome tab
   navigation (`cdp_open_tab`, `cdp_close_tab`, and extension-backed
   `cdp_navigate_tab`). This path uses `chrome.tabs.create`,
   `chrome.tabs.remove`, `chrome.tabs.update`, `chrome.tabs.reload`,
   `chrome.tabs.goBack`, and `chrome.tabs.goForward`; it must not call
   `chrome.debugger.getTargets` or `chrome.debugger.attach`. Its target IDs are
   synthetic `chrome-tab:<tabId>` IDs backed by `chrome.tabs` readback.
8. Use `cdp_bridge_reload` for background bridge lifecycle updates after the
   loaded worker advertises `reloadSelf`. The tool asks the extension to run
   `chrome.runtime.reload()`, then accepts success only after daemon health sees
   a new authenticated bridge host with the expected build/capabilities. It must
   not open `chrome://extensions` or activate Chrome. A worker that predates
   `reloadSelf` is a fail-closed stale-worker condition, not permission to click
   the foreground extension UI.
9. The normal end-user extension is structurally tabs-only: it does not request
   `debugger`, does not call `chrome.debugger`, and rejects attach-capable
   commands before any browser debugger startup. The daemon also refuses those
   commands before queueing anything to Chrome. Chrome intentionally shows a
   "`started debugging this browser`" warning UI when an extension calls
   `chrome.debugger.attach` without `--silent-debugger-extension-api`; Synapse's
   normal bridge therefore cannot use that API. DOM attach for a normal Chrome
   profile requires raw CDP on a dedicated Synapse-launched automation profile.
10. The verifier observes (for diagnostics only) whether the live Chrome profile
   has another active extension with the `debugger` permission, or whether Chrome
   is running an external native-messaging wrapper process. Those surfaces can
   display a user-visible debugger/native-host popup even though Synapse is not
   the caller. Synapse NEVER disables those extensions and NEVER modifies the
   Chrome `ExtensionSettings` policy: that policy governs the user's own
   extensions, and blocking an installed extension's permissions disables it,
   which is unacceptable end-user behavior. Popup-free background automation is
   achieved entirely on Synapse's own side — the bundled bridge is tabs-only, and
   deep CDP runs in a dedicated Synapse-launched automation profile started with
   `--silent-debugger-extension-api`.
   Windows setup (`scripts\synapse-setup.ps1`) and the standalone verifier
   (`scripts\install-synapse-chrome-debugger.ps1`) also perform a one-way
   self-heal: they remove any `debugger`/`nativeMessaging` blockers a prior
   Synapse version wrote into `ExtensionSettings` (identified by Synapse's
   `blocked_install_message` marker) from HKCU and HKLM, restoring the user's
   extensions on previously-affected machines. Admin- or user-authored
   `ExtensionSettings` entries are left untouched. Run
   `scripts\install-synapse-chrome-debugger.ps1 -RemoveExternalDebuggerPolicyOnly`
   to perform only this self-heal.
   Runtime `observe` diagnostics also include a live
   `external_chrome_popup_risk` profile/process summary when Synapse refuses a
   normal-profile attach-capable command, so remaining popups are attributed to
   the exact external browser surface instead of to Synapse's tabs-only bridge.
11. If the current browser session still exposes no endpoint or extension bridge,
   fail closed with
   `web_path = "uia_only"` or `ocr`; do not claim DOM/control readback. Relaunch
   is a user/session decision because it changes the authenticated browser
   process.

Do not treat a fresh automation profile as equivalent to the user's primary
profile when cookies/session state matter. Also do not silently fall back to
coordinate-only web automation and claim DOM access; `web_path = uia_only` means
the DOM was not read.

## Optional UIA Renderer Accessibility

CDP is the preferred browser DOM path. When you specifically need the Chromium
renderer accessibility tree through UIA, opt in to
`--force-renderer-accessibility` for a Synapse-launched browser:

- Per launch: set `force_renderer_accessibility = true` on `act_launch`.
- Per host/session: set `SYNAPSE_FORCE_RENDERER_ACCESSIBILITY` to `1`, `true`,
  `yes`, or `on`.
- Per launch override: set `force_renderer_accessibility = false` to ignore the
  environment opt-in for that call.

This has a cost: Chromium builds and maintains a fuller renderer accessibility
tree. Keep it off by default and enable it only for browser sessions where the
UIA fallback matters.

## Agent Workflow

For browser work in an existing authenticated Chrome session, prefer this loop:

1. Bring the existing Chrome tab/window to foreground using ordinary user-level
   navigation; do not launch a second Chrome when session state matters.
2. Call `observe` with `include = ["focused", "elements", "diagnostics"]`.
3. Require `diagnostics.cdp.status = "ok"` and `diagnostics.web_path = "cdp"`
   before assuming page DOM nodes are present.
4. Call `find` with a role/name query, such as `role = "button"` and
   `name_substring = "Apply"`.
5. Use the returned CDP-backed `element_id` with `act_click`, `act_type`, or
   `act_stroke`. For normal Chrome bridge `chrome-tab:*` targets owned by the
   MCP session, `act_type` can route through `typeActiveElement` and mutate the
   inactive tab's current DOM active element with `chrome.scripting`, then read
   the active-element value back without taking foreground. If CDP is unavailable
   in the user's existing Chrome but UIA still exposes a verified Chromium
   editable target, `act_type.into_element` may use a leased foreground
   click/type fallback only while that Chrome HWND is already foreground; it
   refuses before typing if the target or focus readback does not match. For
   fragile browser controls, set
   `act_click.verify_delta = true` so Synapse fails closed with
   `ACTION_NO_OBSERVED_DELTA` when no focused/UI/pixel state change is observed.
6. Read the separate source of truth after the action: page text/DOM state,
   visible UI state, downloaded file bytes, server-side record, or the Synapse
   audit row that should have changed.

For a disposable automation browser where login/session state does not matter,
`act_launch` remains the simplest way to create a dedicated CDP-enabled profile.

If `observe` reports `A11Y_CDP_UNREACHABLE` and `web_path = "ocr"`, text can be
searched but DOM-only controls still need a CDP-backed browser launch. If
`web_path = "uia_only"`, relaunch through `act_launch` or provide a real debug
port through `SYNAPSE_CDP_PORTS`. Do not keep retrying `find` against the
collapsed UIA tree and treat missing page buttons as absent.

## Recovering Truncated Observations

Large browser, Electron, and IDE trees can exceed the default element budget.
When `diagnostics.elements_truncated = true`, read
`diagnostics.elements_page`:

- `total`: element count available after the requested depth filter.
- `offset`: first element index returned in this response.
- `limit`: maximum elements requested for this response.
- `next_offset`: pass this as `element_offset` on the next `observe` call to
  fetch the next page. If it is absent, this page is the end of the current
  result set.

To expand one UIA subtree instead of paging the whole foreground tree, call
`observe` with `subtree_root = "<element_id>"` and a larger `depth`. This
re-snapshots that element as the root. Use this for native/UIA trees; CDP-backed
web nodes should generally be recovered by paging or by a targeted `find`.

Example sequence:

```text
act_launch(target="chrome.exe", args=["https://example.test/form"], wait_for_window_title_regex="Example")
observe(include=["focused","elements","diagnostics"], max_elements=200)
find(role="button", name_substring="Apply", limit=5)
act_click(target={ element_id="<cdp-backed element id from find>" })
observe(include=["focused","elements","diagnostics"], max_elements=200)
```

For UIA-only browser content, `act_click` element targets prefer semantic UIA
patterns (`InvokePattern`, then `TogglePattern`) before coordinate fallback.
When the fallback target is an enabled keyboard-focusable edit/document/text
surface or exposes `ValuePattern`/`TextPattern`, Synapse bypasses HWND
PostMessage and uses a leased foreground bbox-center click so the real caret
and focus state are placed for later text entry. Only set
`use_invoke_pattern = false` when the operator explicitly wants a raw coordinate
click and the post-click foreground/readback will be verified.

The final `observe` is not the verdict by itself. Full State Verification still
requires a separate read of the real outcome produced by the web action.
