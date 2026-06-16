# Synapse Chrome Bridge

This unpacked MV3 extension lets the Synapse daemon inspect and control the
user's normal Chrome profile through a direct localhost WebSocket from the
extension service worker to the Synapse daemon. The normal end-user bridge is
tabs-first: background tab open/close/navigation use `chrome.tabs` APIs and the
extension does not require the `debugger` or `nativeMessaging` permissions.
It also does not require `chrome.alarms` or any recurring wakeup permission. If
the daemon is unavailable or the live Chrome profile is unsafe, the failure is
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

Install/verify the local bridge registration with:

```powershell
scripts\install-synapse-chrome-debugger.ps1
```

Synapse never modifies the Chrome `ExtensionSettings` policy and never disables
the user's other extensions. The verifier registers the bridge and performs a
one-way self-heal: it removes any `debugger`/`nativeMessaging` blockers that an
earlier Synapse version wrote into `ExtensionSettings` (identified by Synapse's
`blocked_install_message` marker), so running the latest build restores
extensions on previously-affected machines.

Then load this directory as an unpacked extension from `chrome://extensions`.
The extension registers with the loopback daemon at `http://127.0.0.1:7700`,
then keeps an authenticated WebSocket open at `ws://127.0.0.1:7700` with a 20s
keepalive. Commands execute only after the daemon asks through the fixed
extension origin and daemon-issued bridge token. If registration, message post,
or WebSocket keepalive fails, the bridge closes the stale token, logs the code
and reconnect delay, and re-registers with bounded WebSocket reconnect. While
disconnected it performs only a low-frequency `chrome.runtime.getPlatformInfo()`
heartbeat to keep the MV3 worker available for reconnect; it does not request
`chrome.alarms`.
The normal bridge does not call `runtime.connectNative()`, so Chrome does not
create a native-host `cmd.exe` wrapper on end-user systems.
The verifier also removes stale Synapse native-host registration from every
Chrome Windows lookup hive (`HKCU`/`HKLM`, 32-bit and 64-bit views) and returns
the before/after registry readback. If any Synapse native-host key remains, the
verifier fails closed with the exact hive/path/ACL evidence instead of
certifying the host.
The verifier also reads Chrome profile permissions for the live Synapse
extension ID and fails closed if an older load still has `alarms`, `debugger`,
or `nativeMessaging` active.

Registration is also fail-closed. If the daemon sees any live Chrome
profile/process Source of Truth that is not popup-free, it refuses the direct
bridge registration with `A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED` before
accepting a Chrome-hosted command channel. The service worker treats that exact
error as an unsafe-profile condition, logs it with
`A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED`, and retries with the same bounded
backoff. This keeps the failure visible while preventing any browser command
from queueing on an unsafe end-user Chrome profile.

Background tab commands (`openTab`, `closeTab`, `navigateTab`, and
`typeActiveElement`) use
`chrome.tabs.create`, `chrome.tabs.remove`, `chrome.tabs.update`,
`chrome.tabs.reload`, `chrome.tabs.goBack`, `chrome.tabs.goForward`, and
`chrome.scripting.executeScript` against the selected tab's active element. They
do not call `chrome.debugger.getTargets` or `chrome.debugger.attach`; target IDs
returned by this path are synthetic `chrome-tab:<tabId>` IDs backed by
`chrome.tabs` readback. The daemon refuses these normal-profile commands before
queueing them whenever the live Chrome profile/process Source of Truth still
contains any external `debugger` or `nativeMessaging` surface, because even a tab
event can wake another extension's debugger/native-host popup on an unsafe host.

The lifecycle command `reloadSelf` is limited to self-reload. It validates the
expected extension ID and expected build ID, acknowledges the request to the
daemon, then schedules `chrome.runtime.reload()`. The daemon accepts the reload
only after a separate post-reconnect host readback reports the expected build
and the full required capability set.

Attach-capable commands (`snapshot`, `clickNode`, `typeNode`, and `nodeValue`)
are unavailable in the normal end-user install. The normal service worker
rejects them immediately and contains no `chrome.debugger` API calls, so a
stale daemon command or stale permission grant cannot surface Chrome's
"started debugging this browser" warning from the Synapse bridge. DOM attach
requires raw CDP on a dedicated Synapse-launched automation profile.

The install verifier also observes (for diagnostics only) whether the live
Chrome profile contains an active external extension with the `debugger`
permission, or a live external native-messaging wrapper process. Those are
separate browser surfaces that can produce an end-user popup/window even though
Synapse's bridge is tabs-only. The verifier names the extension ID, profile, and
process SoT, but Synapse never disables those extensions or modifies Chrome
policy; deep CDP work runs in a dedicated Synapse-launched automation profile
started with `--silent-debugger-extension-api` instead.

Runtime Chrome observations follow the same rule. If raw CDP is unavailable and
Synapse refuses a normal-profile attach-capable command, the diagnostic detail
includes any external Chrome `debugger` or `nativeMessaging` profile/process
surface found at that moment. A remaining end-user debugger/native-host popup is
therefore attributed to a concrete extension or process instead of being
reported as an ambiguous Synapse bridge failure. Background normal-profile tab
commands follow the same runtime guard; they are available only after the
external profile/process readback is popup-free. If registration itself is
refused, normal-profile tab commands are unavailable; use raw CDP on a dedicated
Synapse-launched automation profile started with `--silent-debugger-extension-api`.

The full Windows setup script never modifies the Chrome `ExtensionSettings`
policy:

```powershell
scripts\synapse-setup.ps1
```

Setup registers the bridge and runs the same one-way self-heal described above,
removing any `debugger`/`nativeMessaging` blockers a prior Synapse version wrote
into `ExtensionSettings` so the user's extensions are restored. It does not write
any blocking policy, never shows a UAC prompt to modify Chrome policy, and never
disables the user's extensions.

To heal an affected machine without a full setup run, invoke the standalone
verifier's maintenance entry point:

```powershell
scripts\install-synapse-chrome-debugger.ps1 -RemoveExternalDebuggerPolicyOnly
```

That removes only Synapse-authored blockers (matched by the
`blocked_install_message` marker) from HKCU and HKLM and reports a per-hive
result; admin- or user-authored `ExtensionSettings` entries are left untouched.
Popup-free background automation is achieved entirely on Synapse's own side: the
bundled bridge is tabs-only over localhost WebSocket (no `debugger`/
`nativeMessaging` permission), and deep CDP runs in a dedicated Synapse-launched
automation profile started with `--silent-debugger-extension-api`.
