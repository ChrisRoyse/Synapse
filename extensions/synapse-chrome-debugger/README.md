# Synapse Chrome Bridge

This unpacked MV3 extension lets the Synapse daemon inspect and control the
user's normal Chrome profile through a direct localhost WebSocket from the
extension service worker to the Synapse daemon. The normal end-user bridge is
tabs-first: background tab open/close/navigation use `chrome.tabs` APIs, and the
extension requests `debugger` only for narrow target-scoped lanes:
`cdpInput` hover/tap/active-tab drag, `viewportEmulation`, `deviceEmulation`,
`geolocationEmulation`, `localeEmulation`, `mediaEmulation`, and
`networkConditions`. Inactive normal-profile tabs use a strict
selector-scoped synthetic MouseEvent drag path through `chrome.scripting` so
manual drag FSV stays background-safe. It also dispatches HTML5 DragEvent/DataTransfer
drops in page script. `pageScreenshot` uses typed page metrics/masks/scroll via
`chrome.scripting` plus queued `chrome.tabs.captureVisibleTab` tile stitching for
viewport, full-page, clip, and element screenshots without `Page.captureScreenshot`.
`pagePdf` uses a narrow `chrome.debugger` `Page.printToPDF` lane for PDF output
from the same already-open Chrome profile. Page-scope `browser_evaluate`,
`browser_add_init_script`, `browser_expose_binding`,
`browser_add_script_tag`, and `browser_add_style_tag` use narrow target-scoped
`chrome.debugger` `Runtime.evaluate`,
`Page.addScriptToEvaluateOnNewDocument`, and
`Runtime.addBinding`/`Runtime.bindingCalled` lanes against the same already-open
Chrome profile. `browser_handle_dialog` uses the same target-scoped bridge to
listen for `Page.javascriptDialogOpening`/`Page.javascriptDialogClosed` and call
`Page.handleJavaScriptDialog` for alert, confirm, prompt, and beforeunload
dialogs.
`browser_file_upload` uses `DOM.setFileInputFiles` for direct input assignment
and `Page.setInterceptFileChooserDialog`/`Page.fileChooserOpened` to capture
file chooser openings without showing the OS file picker, then assigns the
pending chooser's backing node.
`downloads` uses `chrome.downloads` to capture real download created/changed/
erased events, list profile downloads, wait for completion/interruption, and let
the daemon save or move completed files to caller-chosen paths with byte/hash
readback.
It does not require `nativeMessaging`. Element-scoped evaluation and deeper CDP
work still require raw CDP from a dedicated
Synapse-launched automation profile started with `--silent-debugger-extension-api`,
or fail closed before touching the normal browser. It requires `chrome.alarms` so Chrome can wake the MV3 service worker
after the daemon restarts or the worker is suspended, and
`chrome.management` so the bridge can disable external debugger/nativeMessaging
extensions that would otherwise create Chrome warning surfaces. If the daemon
is unavailable or the live Chrome profile is unsafe, the failure is
logged with the exact daemon error code and the bridge retries with bounded
backoff while remaining fail-closed to browser commands.

Stable extension ID: `leoocgnkjnplbfdbklajepahofecgfbk`

The service worker checks `chrome.runtime.id` against that stable ID before it
contacts the daemon. If Chrome loads the unpacked directory under any other ID,
the bridge stays dormant until the extension is reloaded correctly.

The bridge reports its extension version, protocol version, build ID, build
SHA-256, and command capabilities in the daemon `hello` readback. The daemon
uses that identity as the Source of Truth for extension/runtime skew. A loaded
worker that does not advertise a command needed by the daemon fails closed with
`CHROME_BRIDGE_EXTENSION_STALE` before the command is queued to Chrome.

Future bridge updates reload through the background MCP tool
`cdp_bridge_reload`. That tool asks the loaded extension to run
`chrome.runtime.reload()`, then waits for a new authenticated bridge host
registration in daemon health. It does not open `chrome://extensions`, does not
activate Chrome, and does not use coordinates. If the currently loaded worker
predates the `reloadSelf` capability, Synapse cannot make that old worker run
new code; the correct behavior is a visible stale-worker error, not foreground
automation.
If daemon health reports `synapse_chrome_bridge_profile_installation
installed=false`, Chrome has no loaded extension host to receive `reloadSelf`;
run the installer from the interactive Windows desktop with the target Chrome
profile already open. The installer auto-loads this directory as an unpacked
extension in that active profile before retrying bridge health.

Install/verify the local bridge registration with:

```powershell
scripts\install-synapse-chrome-debugger.ps1
```

The verifier registers the bridge, removes stale Synapse-authored external
`debugger`/`nativeMessaging` blockers from earlier builds, then applies the
current reversible HKCU Chrome `ExtensionSettings` popup shield for external
extensions or native hosts that request `debugger`/`nativeMessaging`. It also
preserves a nativeMessaging-only self-shield for the stable Synapse extension
ID. The current bridge intentionally requests `debugger` for narrow
target-scoped `Runtime.evaluate`, `Page.addScriptToEvaluateOnNewDocument`,
`Runtime.addBinding`/`Runtime.bindingCalled`, `Page.handleJavaScriptDialog`,
`DOM.setFileInputFiles`, `Page.fileChooserOpened`, `cdpInput`,
`viewportEmulation`, `deviceEmulation`, and
`geolocationEmulation` / `localeEmulation` / `mediaEmulation` /
`networkConditions` lanes in the already-open
Chrome profile, and it still never requests `nativeMessaging` or creates helper
Chrome windows. The shield is identified by Synapse's
`blocked_install_message` marker and can be removed with the maintenance command
below. If the HKCU Chrome policy key is ACL-locked, the verifier reports
`SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED` with ACL readback. That policy
shield failure is not ignored: the loaded bridge uses `chrome.management` to
disable enabled external `debugger`/`nativeMessaging` extensions, and normal
commands fail closed if that suppression does not complete.

The verifier also opens the already-running Chrome profile's extensions page
and loads this directory as an unpacked extension when the active profile does
not already contain the expected stable extension row. It refuses to launch a
second Chrome profile as the repair path; open the intended authenticated
profile first, then run the installer. This setup script is the canonical
auto-install path for the Chrome bridge; it verifies required permissions,
including `downloads`, and prompts Chrome to accept new permissions when an
existing unpacked row needs repair. The extension registers with the
loopback daemon at `http://127.0.0.1:7700`,
then keeps an authenticated WebSocket open at `ws://127.0.0.1:7700` with a 20s
keepalive. Commands execute only after the daemon asks through the fixed
extension origin and daemon-issued bridge token. If registration, message post,
or WebSocket keepalive fails, the bridge closes the stale token, logs the code
and reconnect delay, and re-registers with bounded WebSocket reconnect. While
disconnected it keeps a 30s `chrome.alarms` wake registered so a suspended MV3
worker can re-register with the daemon without foreground Chrome automation.
The normal bridge does not call `runtime.connectNative()`, so Chrome does not
create a native-host `cmd.exe` wrapper on end-user systems.
The verifier also removes stale Synapse native-host registration from every
Chrome Windows lookup hive (`HKCU`/`HKLM`, 32-bit and 64-bit views) and returns
the before/after registry readback. If any Synapse native-host key remains, the
verifier fails closed with the exact hive/path/ACL evidence instead of
certifying the host.
The verifier also reads Chrome profile permissions for the live Synapse
extension ID and fails closed if an older load still has unexpected
`debugger` or `nativeMessaging` active. Granted-only stale permissions are
reported separately because Chromium can retain removed permissions in the
granted set after an update; they are profile debt, not proof that the running
bridge can call the API.

Registration is command-surface scoped. The daemon accepts the direct bridge
registration for the expected Synapse extension so the worker can report its
`chrome.management` suppression readback. If enabled external
`debugger`/`nativeMessaging` hazards remain, normal bridge commands fail closed
with `A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED` before queueing any browser work.

Background tab commands (`listTabs`, `openTab`, `closeTab`, `navigateTab`, `activateTab`,
`targetInfoPageText`, `pageVitals`, `pageContent`, `pageScreenshot`, `setContent`, `clock`, `pageEvents`, `domAction`, `setFieldValue`, `fileUpload`, and
`typeActiveElement`, `evaluateScript`, `initScript`, `exposeBinding`, and
`handleDialog`) use `chrome.windows.getAll`,
`chrome.tabs.query`, `chrome.tabs.create`, `chrome.tabs.remove`, `chrome.tabs.update`,
`chrome.tabs.reload`, `chrome.tabs.goBack`, `chrome.tabs.goForward`,
`chrome.tabs.captureVisibleTab`, and
`chrome.webNavigation` plus `chrome.scripting.executeScript`. `setContent` writes through a typed
precompiled `chrome.scripting.executeScript(document.open/write/close)` helper
in MAIN world so script tags in replacement HTML use normal page execution
semantics; if Chrome refuses script injection into an owned blank/internal page
such as a top-level `about:blank`, the bridge first moves that background tab
to a daemon-local seed URL on `http://127.0.0.1:7700` and then performs the
same document replacement/readback. When the daemon gives the normal bridge an OS
HWND hint, the extension cannot see HWNDs directly, so it maps the hint through
the daemon's passive window bounds/title readback before using `windowId`. If
that mapping does not identify exactly one Chrome window, `openTab` and
target-scoped readback fail before accepting the tab/window pair. It never
creates a helper Chrome window and never treats the most-recently-focused Chrome
window as a substitute for the requested HWND. `capturePageScreenshot` is still
not a normal-bridge capability; the daemon refuses that debugger-backed command
before queueing any Chrome command because Chrome's debugger infobar changes
viewport/layout and breaks coordinate truth. Use `browser_screenshot`, which
routes to `pageScreenshot`, for normal-profile page screenshots. `evaluateScript`
`exposeBinding`, `handleDialog`, and `fileUpload` are also normal-bridge capabilities through
narrow target-scoped `chrome.debugger` `Runtime.evaluate`,
`Runtime.addBinding`/`Runtime.bindingCalled`, and
`Page.javascriptDialogOpening`/`Page.handleJavaScriptDialog` plus
`DOM.setFileInputFiles`/`Page.fileChooserOpened` lanes for
session-owned `chrome-tab:*` targets. Use raw CDP in a dedicated silent
automation profile for element-scoped evaluation and broader DevTools-domain
work.
`pageScreenshot` temporarily activates only the requested tab inside its existing
Chrome window so `chrome.tabs.captureVisibleTab` captures the right page. On
Windows builds where Chrome refuses image readback for an unfocused Chrome
window, it focuses that Chrome window during capture, reports
`required_foreground=true`, restores the previous Chrome tab/window focus when
Chrome can report one, and always restores scroll position and masks. It queues
`captureVisibleTab` calls to stay under Chrome's per-second capture quota for
tiled full-page screenshots and back-to-back screenshot requests. Each tile
capture is bounded inside the extension and reports tab/window/tile timing on
timeout so a hung Chrome image readback cannot poison later screenshot requests.
It never launches a helper profile.
`pagePdf` attaches `chrome.debugger` only long enough to call `Page.printToPDF`,
then detaches and returns base64 PDF bytes to the daemon for file writing.
`clock` uses the same typed MAIN-world execution model for current-document
Date/timer control in an owned tab; future-document init scripts are available
through the normal bridge's narrow `Page.addScriptToEvaluateOnNewDocument` lane.
`pageEvents` keeps a per-tab normal-bridge ring buffer from `chrome.webNavigation`
and a typed MAIN-world worker shim for current-document worker creation/termination
readback; raw CDP still provides broader Target-domain worker/session detail.
`pageVitals` and `targetInfoPageText` read the page Performance Timeline for LCP
plus document visibility state. The tabs-first readback/action commands do not
call `chrome.debugger.getTargets` or `chrome.debugger.attach`; target IDs
returned by this path are synthetic `chrome-tab:<tabId>` IDs backed by
`chrome.tabs` readback. The narrow `cdpInput` command attaches only long enough
to dispatch hover/tap/active-tab mouse-drag input, then detaches; inactive-tab
mouse drag uses a strict in-page MouseEvent sequence. `viewportEmulation`
attaches only long enough to call `Emulation.setDeviceMetricsOverride` or
`Emulation.clearDeviceMetricsOverride` plus a zero-metrics disable command.
Set accepts explicit mobile viewport semantics for manual DPR/mobile FSV. Because
Chrome's normal-profile extension debugger path does not make
`deviceScaleFactor` visible as `window.devicePixelRatio` on this host, the
normal bridge installs a narrow MAIN-world DPR shim for page scripts and removes
it on reset. If Chrome's clear/zero reset leaves the emulated size in place,
the bridge reloads the background tab and, when reload still reports the stale
mobile dimensions, applies the captured native viewport metrics through the
same narrow debugger lane. It then reads `window.innerWidth`,
`window.innerHeight`, and `devicePixelRatio` back through a typed page script.
`deviceEmulation` extends that same narrow lane for Playwright-style device
descriptors: user agent, viewport/DPR/mobile, touch emulation, and max touch
points. Because normal-profile extension debugger readback can leave some
device fields invisible to page scripts on this host, it uses a MAIN-world
device shim for `devicePixelRatio`, `navigator.userAgent`,
`navigator.maxTouchPoints`, `ontouchstart`, and coarse pointer/hover media
queries, then clears the shim on reset.
`geolocationEmulation` applies target-scoped geolocation override/clear through
the debugger lane and uses a narrow MAIN-world shim for
`navigator.permissions.query({ name: "geolocation" })` plus
`navigator.geolocation` callback readback, then clears the shim on reset.
`localeEmulation` applies target-scoped locale/timezone override/clear through
the debugger lane and reads `Intl.DateTimeFormat`, `Intl.NumberFormat`, and
`Date` behavior back through a typed MAIN-world script.
`mediaEmulation` applies target-scoped CSS media type and media-feature
override/clear through the debugger lane and reads `matchMedia` screen/print,
prefers-color-scheme, and prefers-reduced-motion state back through a typed
MAIN-world script.
`networkConditions` applies target-scoped offline/latency/throughput state
through the debugger lane and uses a typed MAIN-world shim for
`navigator.onLine`, `fetch`, and Network Information API readback, then clears
that shim on reset.
The HTML5 drag path uses typed `chrome.scripting.executeScript` to create
`DragEvent` plus `DataTransfer` in page MAIN world. The daemon refuses other
attach-capable debugger commands before queueing them. External
`debugger`/`nativeMessaging` surfaces remain visible in health/diagnostics for
operator attribution and policy shielding, and they must be suppressed by
policy or `chrome.management` before popup-free normal-profile commands run.

The lifecycle command `reloadSelf` is limited to self-reload. It validates the
expected extension ID and expected build ID, acknowledges the request to the
daemon, then schedules `chrome.runtime.reload()`. The daemon accepts the reload
only after a separate post-reconnect host readback reports the expected build
and the full required capability set.

Attach-capable DOM commands (`snapshot`, `clickNode`, `typeNode`, and
`nodeValue`) are unavailable in the normal end-user install. The normal service
worker rejects them immediately. The bridge's only `chrome.debugger` use is the
target-scoped `cdpInput` hover/tap/active-tab mouse-drag lane and the
`viewportEmulation` / `deviceEmulation` / `geolocationEmulation` /
`localeEmulation` / `mediaEmulation` / `networkConditions` metrics lanes,
page-scope `Runtime.evaluate`, init-script mutation, binding capture, dialog
handling, and inactive-tab synthetic MouseEvent drag fallback; DOM attach and debugger-backed
`Page.captureScreenshot` require raw CDP on a dedicated Synapse-launched
automation profile.

The install verifier also reads whether the live Chrome profile contains an
external extension or native-messaging wrapper process with `debugger` or
`nativeMessaging`. Those are separate browser surfaces that can produce an
end-user popup/window even though Synapse's bridge uses only tabs plus
scripting. By default, setup writes a Synapse-marked HKCU `ExtensionSettings`
`blocked_permissions` shield for those hazards. Use
`-PreserveExternalDebuggerExtensions` only as an explicit emergency opt-out;
deep CDP work still belongs in a dedicated Synapse-launched automation profile
started with `--silent-debugger-extension-api`. If the policy key is ACL-locked,
setup reports the denied write with ACL evidence instead of assuming the shield
exists. Granted-only stale Synapse nativeMessaging rows remain advisory when the
loaded bridge is current, `debuggerApiAvailable=true`, and `cdpInput`,
`viewportEmulation`, `deviceEmulation`, `geolocationEmulation`,
`localeEmulation`, `mediaEmulation`, and `networkConditions` are
advertised; active/manifest Synapse `nativeMessaging` hazards still fail closed.
External hazards rely on the loaded bridge's `chrome.management`
suppression readback. If Chrome rejects that suppression, normal-profile commands
fail closed with exact extension IDs.

Runtime Chrome observations follow the same rule. If raw CDP is unavailable and
Synapse refuses an unsupported normal-profile attach-capable command, the diagnostic detail
includes any external Chrome `debugger` or `nativeMessaging` profile/process
surface found at that moment. Health/setup also report visible automation Chrome
processes whose flags are known to show layout-shifting browser banners, such as
headed Playwright MCP launches with `--disable-blink-features=AutomationControlled`
or remote debugging without `--silent-debugger-extension-api`. A remaining
end-user debugger/native-host/banner popup is therefore attributed to a concrete
extension or process instead of being reported as an ambiguous Synapse bridge
failure. Background normal-profile tab, typed DOM commands, and the target-scoped
`cdpInput` hover/tap/drag, `viewportEmulation`, `deviceEmulation`,
`geolocationEmulation` / `localeEmulation` / `mediaEmulation` /
`networkConditions` lanes require those warnings to be cleared or
suppressed before they run. Use raw CDP on a dedicated Synapse-launched
automation profile started with `--silent-debugger-extension-api` for full
attach-capable CDP work outside the bridge's narrow input lane.

The full Windows setup script runs the same verifier and applies the same
reversible HKCU popup shield by default:

```powershell
scripts\synapse-setup.ps1
```

Setup registers the bridge, removes stale Synapse-authored blockers from prior
builds, writes the Synapse extension ID nativeMessaging-only self-shield, and
tries to write current Synapse-authored `blocked_permissions` entries for
detected external debugger/nativeMessaging hazards. Those entries are reversible through
the maintenance command below and are the supported first way to suppress
popups from other extensions/native hosts; the runtime `chrome.management`
fallback is the second way, and fail-closed is the only remaining behavior when
both are unavailable.

To heal an affected machine without a full setup run, invoke the standalone
verifier's maintenance entry point:

```powershell
scripts\install-synapse-chrome-debugger.ps1 -RemoveExternalDebuggerPolicyOnly
```

That removes Synapse-authored blockers (matched by the
`blocked_install_message` marker) from HKCU and HKLM, including stale self
entries that blocked `debugger`, then reports a per-hive result; admin- or user-authored
`ExtensionSettings` entries are preserved.
Background automation is achieved on Synapse's own side with the bundled bridge
over localhost WebSocket, no `nativeMessaging` permission, no helper Chrome
windows, tabs/scripting for DOM work, and narrow `chrome.debugger` lanes for
page-scope `Runtime.evaluate`, init-script mutation,
`Runtime.addBinding`/`Runtime.bindingCalled` binding capture,
`Page.handleJavaScriptDialog` dialog handling,
`cdpInput` hover/tap/active-tab mouse-drag, `viewportEmulation`,
`deviceEmulation`, `geolocationEmulation`, `localeEmulation`, and
`mediaEmulation` / `networkConditions` plus
inactive-tab synthetic mouse drag and HTML5 DataTransfer dispatch. Deeper DOM/action CDP still runs in a dedicated
Synapse-launched automation profile started with `--silent-debugger-extension-api`.
