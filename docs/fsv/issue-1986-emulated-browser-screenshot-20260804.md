# Issue #1986: emulated browser screenshot FSV (2026-08-04)

## Source of truth

- Page state: the owned Chrome tab's MAIN-world `innerWidth`, `innerHeight`, and
  `devicePixelRatio`, read independently with `browser_debugger.evaluate`.
- Artifact state: PNG bytes under `%LOCALAPPDATA%\synapse\fsv`, read separately
  with `Get-Item`, `Get-FileHash`, and `System.Drawing.Bitmap` after capture.
- Runtime state: the live OS process table and installed
  `%USERPROFILE%\.cargo\bin\synapse-mcp.exe` bytes.

## Diagnosis and research

The extension paired emulated MAIN-world CSS metrics with
`chrome.tabs.captureVisibleTab` pixels from the desktop window compositor. The
stitcher inferred a scale from those unrelated surfaces and had no CSS/DPR
postcondition. Exa MCP was live (`exa-search-server` 3.4.0, real tool call) and
the built-in web lane was also used after diagnosis. Chrome's primary docs say
that `captureVisibleTab` captures the visible area of the active tab; CDP defines
`Page.captureScreenshot` clips in DIP with `captureBeyondViewport`; and
`Emulation.setDeviceMetricsOverride` owns the emulated layout metrics.

The fix uses MAIN-world metrics, selects CDP page-surface capture only while a
Synapse-owned viewport/device override exists, activates the requested tab in
the already serialized capture/restore section, and refuses any bitmap before
write unless both pixel/CSS axes agree with the declared DPR. Ordinary tabs keep
the debugger-free `captureVisibleTab` tier.

## Happy paths

Emulated target `chrome-tab:589710115`, Example Domain:

- before: evaluator read `{w:360,h:800,dpr:2}`;
- trigger: viewport screenshot;
- response: backend `chrome_debugger_page_surface`, page region `360x800`, DPR
  `2`, native/written bitmap `720x1600`, 72,273 bytes, SHA-256
  `091cecfe30fd6825d80dc1eb7eb97e9cf01f8d57ea3d8d21955d48774c08d640`;
- independent after-read: PNG physically existed at 72,273 bytes, decoded as
  `720x1600`, and independently hashed to the same digest.

Normal target `chrome-tab:589710117`, Example Domain:

- page region `1046x784`, DPR `1.25`;
- response backend `chrome_tabs_extension`, bitmap `1308x980`, 53,779 bytes,
  SHA-256 `e39e185fe1b0b8075926da4708fa157de1cddb2df8d0942557a7bcf5f8f6ac4f`;
- independent PNG decode/hash matched; `ceil(1046*1.25)=1308` and
  `784*1.25=980`.

## Boundary audit

1. Invalid viewport width: before evaluator read `{360,800,2}`; setting width
   zero returned `TOOL_PARAMS_INVALID`; after evaluator still read
   `{360,800,2}`.
2. Empty clip: artifact absent before; `w=0` returned `TOOL_PARAMS_INVALID`;
   artifact remained absent after.
3. Existing output with `overwrite=false`: before file was 37,408 bytes with
   SHA-256 `13ec5a15e620698940c20b1b109c91d1386608f38951682710787fd445537a40`;
   trigger returned `TOOL_PARAMS_INVALID`; independent after-read had identical
   length and hash.

An intermediate DPR mismatch was also proven fail-closed: requested CSS
`360x800`/DPR2 versus returned `450x1000`/scale1.25 produced
`ACTION_POSTCONDITION_FAILED` before write.

## Deployment and gates

- `cargo check --workspace`: passed.
- `scripts/lint.ps1`: all seven gates passed in both workspaces.
- installed daemon PID 14292, executable length 185,649,769, SHA-256
  `9CAFF818B0925A20EE96417401BDBD5899BDCC67EF34CC9C2BCC87F9D153EFEB`.
- setup completed build, candidate validation, handoff, daemon/bridge readback;
  its final nonzero status was the intentional
  `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` current-client restart guard.

Related defects discovered and separately tracked: #1992 (viewport reset
baseline across extension reload) and #1993 (discarded tab capture error
classification).
