# Issue #1993: discarded-tab browser capture FSV (2026-08-04)

## Source of truth

- Chrome tab state from independent `chrome.tabs.query/get` bridge readback.
- PNG bytes read independently with `Get-Item`, `Get-FileHash`, and
  `System.Drawing.Bitmap` after the trigger.
- Prior active-tab state read again after capture to prove restoration.

## Diagnosis and research

Capture injected page setup before tab activation. A discarded tab has no live
document, so Chrome rejected injection and the bridge mislabeled the lifecycle
condition as missing host permission. Exa MCP and built-in web research used
Chrome's primary Tabs and Scripting documentation: discarded content is unloaded
and reloaded on activation; script injection requires a target document.

The bridge now exposes `discarded` and `frozen` in page state, activates an
unloaded/discarded target inside the serialized capture section, waits for
`discarded=false`, `frozen=false`, and `status=complete`, then injects. A race
that loses the document again returns a typed lifecycle timeout with exact state
and remediation.

## Happy path

- before: `chrome-tab:589710100`, GitHub Issues, `ready_state=unloaded`, inactive;
  artifact absent;
- trigger: viewport capture;
- after tab read: target `ready_state=complete`, inactive; prior tab
  `chrome-tab:589710106` active again;
- response: normal tabs backend, page region `1046x784`, DPR 1.25, physical PNG
  `1308x980`, 316,350 bytes, SHA-256
  `80f3d07455c4144632665ef42e98328336e82aa4b3e697373aca5bed08bdbacb`;
- independent file decode and hash matched all values.

## Boundary audit

1. Restricted `chrome://settings`: artifact absent before; capture returned
   `CHROME_SCRIPTING_EXECUTE_FAILED` with `Cannot access a chrome:// URL` (not a
   discarded-tab classification); artifact remained absent.
2. Empty clip: artifact absent before; `h=0` returned `TOOL_PARAMS_INVALID`;
   artifact remained absent.
3. Existing output with `overwrite=false`: before length/hash were 316,350 and
   the digest above; trigger returned `TOOL_PARAMS_INVALID`; after length/hash
   were byte-identical.

## Verification

- deployed extension service-worker SHA-256:
  `4aabbbd904030f2af16b4d8348ac1abc26c7e1a5f6c6d049fea94c6e6eb03450`;
- `scripts/lint.ps1`: all seven gates passed in both workspaces.
