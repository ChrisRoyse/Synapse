# Synapse Chrome Bridge

This unpacked MV3 extension gives Synapse background-safe access to the user's
already-authenticated Chrome profile. It uses a direct authenticated localhost
WebSocket to `synapse-mcp`; it never uses native messaging, OS input, a helper
window, or a debugger attachment.

The normal bridge is deliberately limited to Chrome APIs that cannot display
Chrome's layout-shifting debugger infobar:

- `chrome.tabs` for target-scoped tab listing, background creation, navigation,
  activation within an existing Chrome window, and close;
- `chrome.scripting` for typed DOM reads/actions, ARIA/assertion helpers, form
  state, content replacement, clock control, and synthetic page events;
- `chrome.cookies`, `chrome.downloads`, `chrome.storage`,
  `chrome.webNavigation`, and `chrome.webRequest` for their typed state;
- `chrome.alarms` for MV3 reconnect wakeups after worker suspension.

The manifest permanently forbids `debugger`, `nativeMessaging`, and
`management`. Synapse does not inspect, disable, or reconfigure unrelated
extensions.
Debugger-backed commands fail before any Chrome command is queued and explain
that the caller must use a session-owned raw-CDP target. Deep evaluation,
trusted input, screenshots, PDF, init scripts, bindings, dialogs, file chooser
interception, and emulation remain available on the dedicated non-default
automation profile launched by Synapse (`act_launch`). This separation keeps
capability without attaching DevTools to the human browser.

## Identity and transport

Stable extension ID: `leoocgnkjnplbfdbklajepahofecgfbk`.

The worker checks that ID before contacting the daemon. Every authenticated
hello reports the extension version, protocol version, build ID, declared build
hash, running-worker hash, deployed-file hash, capability set, reconnect alarm,
and durable owner state. The daemon compares those values with its compiled
expectations and the physical Chrome profile/file Source of Truth. Any mismatch
fails closed as `CHROME_BRIDGE_EXTENSION_STALE`.

Registration uses `http://127.0.0.1:7700`; commands use an authenticated
WebSocket at `ws://127.0.0.1:7700`. A host-local token is injected only into the
stable deployed worker. The bridge never calls `runtime.connectNative()` and
setup removes obsolete Synapse native-host registry entries.

## Background lifecycle

The stable deployment directory is
`%LOCALAPPDATA%\synapse\chrome-extension\active`. Once the unpacked extension is
installed, updates use the public `browser_debugger` operation `reload_bridge`:

1. require a connected authenticated worker advertising `reloadSelf`;
2. launch the repo installer as a hidden bounded process with
   `-SkipAutoInstall -OutputJson`;
3. verify the deployed bytes, token, stable extension path, and physical Chrome
   profile row;
4. send `reloadSelf`, whose only effect is a delayed `chrome.runtime.reload()`;
5. wait for a replacement authenticated worker; and
6. independently re-read the running/deployed hashes, profile row, capability
   set, durable owner state, and `debuggerApiAvailable=false`.

The path never focuses, restores, minimizes, unminimizes, navigates, clicks, or
types into a human Chrome window. A missing initial install is not disguised as
a reload: it fails with the exact absent profile/file condition.

Install or inspect the bridge with:

```powershell
scripts\install-synapse-chrome-debugger.ps1
```

Use `-SkipAutoInstall` for background deployment to an existing installation.
Legacy UI reload/cleanup switches fail immediately under
`SYNAPSE_CHROME_BACKGROUND_ONLY_POLICY`.

## Permission enforcement

Setup reads Chrome's physical Preferences/Secure Preferences rows. Active or
manifest `debugger`/`nativeMessaging` permission on the Synapse extension is a
blocking error; granted-only residue is reported separately because Chromium
can retain removed grants without exposing the runtime capability. The live
worker must also report `debuggerApiAvailable=false`.

External extensions and native hosts are separate warning surfaces. Setup
reports them only as diagnostics. They do not gate the debugger-free bridge,
and neither setup nor the extension changes their state. Deep browser work is
isolated at the profile boundary instead of trying to police the user's normal
profile.

To remove only Synapse-authored policy entries:

```powershell
scripts\install-synapse-chrome-debugger.ps1 -RemoveExternalDebuggerPolicyOnly
```

Operator- or administrator-authored policy entries are preserved.
