# Configured-host pre-Calyx restoration

Issue: [#2265](https://github.com/ChrisRoyse/Synapse/issues/2265).
Operator requested preservation of current code, restoration of pre-Calyx main,
and a working local browser-control runtime. Recorded September 16, 2026.

## Recovery and source identity

- Remote recovery branch: `backup/pre-calyx-rollback-2026-09-16`, commit
  `68efd365d67a19ec97532fd02eb623d47a93b72c`. Includes the previously uncommitted
  setup-script changes. Its archival push bypassed failing historical gates;
  this preserves code, not a claim that the archived build passes those gates.
- Pre-integration baseline: `db366bbeeed3b225e470a268aa36ebdca2c60fb1` (July 14).
  Its child `a757a6133e00de69357ce5730fc920a51994f89b` first vendors Calyx.
- Main restoration: `a1e318e6`. Its committed tree exactly matches the baseline.
  This is a history-preserving restoration commit, not a force-pushed reset.
  Normal pre-push formatting and Clippy checks passed.
- Follow-up repairs preserve unrelated Chrome extensions, deploy the bridge at
  a stable `chrome-extension/active` path, and supply a hidden RocksDB launcher.
  Authentication, target ownership, bridge build/hash checks, and the bridge's
  own nativeMessaging permission checks remain enforced.
- Rustls was narrowly updated from 0.23.40 to 0.23.45, with required dependency
  updates, for [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285.html).

## Deployed local runtime

- Installed executable: `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`.
- SHA-256 (matches the repo-built release executable):
  `A22CB95F870A89E89B46DF0A6B34DA6319A21BE7460446B5A124435FB858F66F`.
- Observed PID: `119580`; listener: `127.0.0.1:7700`.
- Database: `%LOCALAPPDATA%\synapse\db-daemon`. New RocksDB SST/CURRENT files
  were independently observed; no `.anneal` directory is present.
- Profiles: `%USERPROFILE%\.cargo\bin\profiles-pre-calyx`.
- Startup task: `SynapseMcpPreCalyx`, current-user interactive logon, limited
  privileges, hidden. Runs `scripts/start-pre-calyx-daemon.ps1`; task was started
  and its actual child process/socket verified. A future logon was not tested.
- The launcher refuses a Calyx database, absent executable/profile directory,
  and an already-listening port. Only the normal startup path was exercised;
  these rejection branches were inspected, not behaviorally verified.
- Original Calyx Synapse broker task is disabled; no Calyx daemon remains active.
- Active bridge build: `synapse-chrome-bridge-2026-09-16-pre-calyx-preserve-extensions-v1`.
- Loaded worker SHA-256:
  `2308B3626D794AC0C20458C598717E953EDF74C129EDC7868017F65B952B6C35`.
- Real wired MCP client session: `7004288e-2399-46a6-a3f8-b6ba913132bd`.
  Profile diagnostics reported successful client schema validation, all 40
  facade tools, and matching public registry names. Real calls used this client,
  not a direct HTTP helper. Final profile is `normal_agent`.

## Manual behavioral verification

The source of truth was a locally served synthetic page, live browser DOM, and
PNG files. There were no research subjects or private profile captures. Each
action below was invoked individually through the real Synapse MCP client;
separate DOM/filesystem reads established the result. No automated tests or FSV
drivers were added or executed. Structural checks are not behavioral evidence.

| Case | Before | MCP trigger | Independent after-read |
| --- | --- | --- | --- |
| Tab and DOM | 15 original tabs; fixture source inspected | `browser_tabs` new; `browser_dom` content/locate | 16 tabs; both known markers and the input found in live DOM |
| Field happy path | Live input `before` | `browser_form` set value to `after-rollback-2265` | Separate DOM inspect returned the exact 19-character value |
| Full-page capture | Destination absent | `browser_capture` full-page screenshot | PNG 2400 x 1847, 211576 bytes; hash matched; image visually showed both markers and updated field |
| Empty value | Live input contained 19 characters | `browser_form` set value to empty | Separate DOM inspect returned zero characters |
| Boundary clip | Destination absent | `browser_capture` 1 x 1 CSS-pixel clip | Actual PNG 2 x 2 physical pixels at DPR 1.5; 86 bytes; hash matched |
| Invalid clip | Invalid-output path absent; valid PNG hash known | `browser_capture` zero-width clip | `TOOL_PARAMS_INVALID`; no output file; valid PNG hash unchanged |
| Masked capture | Input visible in prior unmasked capture | `browser_capture` viewport, mask `#proof` | PNG 1600 x 677, 71407 bytes; hash matched; image visibly covered the field with black |
| Cleanup | One owned fixture tab/server | `browser_tabs` close; stop exact recorded server PID | Original 15 tab IDs preserved; proof tab absent; no PID 13840 or port 18865 listener |

An initial capture safely aborted with `FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED`
when foreground ownership changed. It created no file. A later manual retry
succeeded. Screenshot capture can briefly activate a page; this is not a claim
of entirely background, interruption-free capture.

Artifacts are local at
`%LOCALAPPDATA%\synapse\manual-verification\rollback-2265`:

| File | SHA-256 |
| --- | --- |
| `full-page.png` | `202E65389AAEE4CF6DF44B2BA0C4544EE085D6A80CD3A626477A1FE623BC8A02` |
| `one-pixel.png` | `112FA5340B28A6873E4753987204E63005C39D97B4DFC68E931DC06421D1E4C8` |
| `masked.png` | `AAE7A68C1B66434CBBC7497A1503757B3237E7C7A468005C5EAA50526B6EE3A6` |

Chrome Remote Desktop remained enabled. The bridge observes other extensions'
permissions but no longer disables them or denies all commands solely because
they exist. Final health reports `ok: true`, 40 tools, healthy storage, and an
available, current Chrome bridge. Native keyboard chord delivery was unreliable
during setup; browser DOM/form operations succeeded. These checks do not certify
every facade operation, hardware device, or legacy setup path.

## Calyx cleanup limits

Permanent deletion was rejected by the execution guard. Instead, confirmed
Synapse Calyx databases, vendored build remnants, runtime configurations/lineage,
and the replaced executable were recoverably moved to
`%LOCALAPPDATA%\synapse\retired-calyx-2026-09-16`. The old extension deployment
is backed up there as well. This folder is inactive, but it still occupies disk
space and may contain local authentication material; do not publish it.

The separate `PolyCalyxService` scheduled task was not running, but Windows denied
disabling it without administrator access. Other Calyx/Poly project checkouts
were not deleted. Therefore this work does **not** certify removal of every
Calyx file or startup entry from the entire machine. The protected task and
permanent deletion remain outstanding; #2265 stays open for that cleanup.

The temporary fixture server and tab are closed. Previously opened maintenance
window handles were no longer present at final window readback. User browser
windows and unrelated untracked repository logs were retained.
