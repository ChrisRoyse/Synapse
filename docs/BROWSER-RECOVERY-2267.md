# Browser recovery verification — 2026-10-28

Issue: #2267. PR: #2268. Operator requested replacement of the old daemon,
manual validation, and merge. No automated tests, FSV drivers, or CI were run.

## Installation and source of truth

- Verified old PID 26572, executable `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`,
  HTTP bind `127.0.0.1:7700`; authenticated `/shutdown` completed gracefully.
- Preserved the old executable under
  `%LOCALAPPDATA%\synapse\daemon-backups\2267-before-install\synapse-mcp.exe`.
- Installed the repo-built diagnostic patch, SHA256
  `B69536625D4060566241D63C15B092725CB4BB8D7EF13E1C27F7EEAABE809879`, PID 30620.
  RocksDB replayed 260,054 pre-existing WAL files before serving HTTP, then
  removed obsolete journals itself. No database reset or manual WAL deletion.
- Live diagnostics exposed 19 prior-browser-session owners: one opened-tab row
  and 18 unresolved debugger timeouts, no in-flight mutation. Revision 42431,
  gate disabled, stop generation 0. Ordinary reload could not fix this state.
- Extended maintenance recovery to archive and reconcile only opened-tab and
  timeout records whose targets are absent in independent Chrome tab/debugger
  readbacks. Other owner kinds, live IDs, unreadable state, in-flight mutations,
  changed revisions, or newer stop generations remain refusals.
- Deployed the extension through `install-synapse-chrome-debugger.ps1
  -ReloadExistingExtensionViaUi` in the existing Profile 5. Unrelated extensions
  and browser processes were preserved. Installed worker SHA256:
  `CEF355C9D5FB9B6D44433D974950A37F5F438D8D9C6D923C443C86A02447DEC2`.
- Installed the recovery implementation, SHA256
  `5C09EEE59FBBBA65A90DE8D45A46F33DB034312F11CE3D14B84F59E3A24217CD`, PID 24396.
  Separate OS process/socket reads confirmed its executable and bind. Authenticated
  health and the real wired MCP client succeeded with all 40 public tools and
  surface hash `089c19716a4935b813d569ad7838af69c2cc5f6b67b7f5629bcc817a433f3149`.

## Manual recovery trigger and separate readback

Wired MCP session: `2bbf1613-2ed9-4401-bae0-4750fda10e1a`.
Before: `setup doctor` read revision 42433, disabled generation 0, stale owners
19, current browser session `711d68f2-77b4-4ee9-bc41-ea1c673633e7`, ledger session
`28e5df68-566a-4f82-9ad3-dff092f524ee`. All six current tabs had different IDs.

Acquired the foreground lease, selected `break_glass` with explicit operator
intent, then called real `setup operation=repair`. The extension verified absence,
archived the old ledger, persisted reconciliation, and the daemon separately read
the drained ledger before requesting generation-checked enable. Browser recovery
returned without launching another daemon installer.

Separate `setup doctor`: revision 42435, enabled generation 0, matching session
IDs, zero stale owners, zero timeout records, no in-flight mutation, healthy
continuity. Worker identity stayed `a3d8477f-880c-411e-8665-e68d6af80c94`.
Read-only inspection of Chrome's Profile 5 extension WAL found archive key
`synapseOperatorPanicDurableOwnerLedgerV2:recovery:a3d8477f-880c-411e-8665-e68d6af80c94:42433`.
Restored `normal_agent`; separate lease release reported held=false/released=true.

## Manual browser checks

Used a local synthetic HTML page on `127.0.0.1:18867` with an editable input,
disabled input, Apply button, and output. Every trigger used the wired MCP tools;
DOM inspections and tab listings were separate calls.

| Case | Before | Trigger | Independent after |
| --- | --- | --- | --- |
| Create | Six original tabs | `browser_tabs new` | Seven tabs; owned ID 600778756, expected title/URL |
| Field happy path | `before-2267` | `browser_form set_value` to `after-2267` | Live input property `after-2267` |
| Button | Output `not-applied` | `act invoke` click Apply | Output `after-2267` |
| Empty | Input `after-2267` | Set empty string | Live input property empty |
| Boundary | Input empty, maxlength 32 | Set `12345678901234567890123456789012` | Exact 32-character property |
| Invalid structure | 32-character property | Both selector and active_element | TOOL_PARAMS_INVALID; property unchanged |
| Disabled field | `must-stay`, enabled=false | Set `must-not-apply` | Refused; `must-stay`, enabled=false |
| Navigate | Original fixture URL | Navigate to `/?verified=2267` | Content read returned reset fixture HTML and new URL; local server recorded exact request |
| Close | Seven tabs | Close owned ID 600778756 | Original six IDs only; original active tab restored |

Final original tab IDs: 600778631, 600778739, 600778745, 600778749 (active),
600778752, 600778755. Fixture server PID 30444 stopped; separate socket read
found no port 18867 listener. No terminal/IDE/other-agent process was stopped.

After browser cleanup, `setup doctor` read revision 42489, enabled generation 0,
healthy continuity, every owner count zero, and no unresolved timeouts. Additional
repair edge cases were manually triggered from this state: empty reason,
normal-agent maintenance attempt, and conflicting doctor/repair payloads. Each
was refused; separate doctor calls after each retained revision 42489, enabled
generation 0, and zero stale owners.

These observations cover configured-host recovery and the listed browser paths.
Concurrent new-panic races and recovery with a live reused historical tab ID were
reviewed in code, not physically injected or claimed as manually exercised.

## Structural checks

`cargo check -p synapse-mcp --bin synapse-mcp`, `cargo fmt --all --check`,
`node --check extensions/synapse-chrome-debugger/service_worker.js`, and
`git diff --check` passed. Strict `cargo clippy --workspace --all-targets --
-D warnings` passed without warnings after separating the enable-command result
from the independent enabled-state readback in the condition. This is structural
evidence, separate from the manual observations above.
