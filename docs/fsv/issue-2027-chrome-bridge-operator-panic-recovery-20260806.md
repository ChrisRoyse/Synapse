# Issue #2027: Chrome bridge operator-panic recovery FSV

Date: 2026-08-06 (America/Chicago)

## Result

PASS. A Chrome extension worker replacement can no longer make an empty owner
ledger permanently ineligible for re-enable, and the public 40-tool surface now
exposes generation-guarded operator-panic status and recovery through `act`.

## Diagnosis and research

The defect had two coupled causes:

1. `operatorPanicActiveOwners()` included the nullable diagnostic
   `resolved_prior_session_debugger_command_timeouts` in an object later treated
   as an all-numeric owner-count map. `Object.values(...).every(count => count ===
   0)` is false for `null`, so an otherwise empty durable ledger could never be
   enabled. The diagnostic now remains in readback but outside `active_after`.
2. Status/enable existed only as internal Chrome commands. No operation in the
   public 40-tool facade could inspect or recover a stranded latch. `act` now
   exposes `operator_panic_status` and `operator_panic_recover`; recovery is an
   explicit compare-and-swap over the process panic epoch plus both gate disable
   generations, and refuses pending, unhealthy, non-empty, or stale state.

The Exa lane was verified live first with
`scripts/check-research-lane.ps1` (`exa-search-server` 3.4.0), then used for real
queries on MV3 update lifecycle and safety-interlock recovery. The accepted
built-in web lane independently read the primary Chrome documentation:

- https://developer.chrome.com/docs/extensions/reference/api/storage
  (`storage.session` is cleared on extension update/reload; `storage.local` is
  durable extension state).
- https://developer.chrome.com/docs/extensions/reference/api/runtime
  (`runtime.onInstalled` reports update lifecycle, including unpacked reload).
- https://developer.chrome.com/docs/extensions/develop/concepts/service-workers/lifecycle
  (MV3 workers are restartable and global in-memory state is not durable).

These facts support durable owner state plus explicit fresh readback and exact
generation guards; they do not support clearing the latch on update or silently
assuming an empty ledger.

## Sources of truth

- Process-global wave: `synapse_action::operator_panic_safety_readback()`.
- Daemon browser gate: `synapse_a11y` durable browser-owner registry readback.
- Extension gate: live worker plus its `chrome.storage.local` owner ledger.
- Physical audit: Calyx vault `CF_ACTION_LOG`, queried independently through
  `audit operation=command_query`.
- Deployment: Windows process/socket table, installed executable bytes, daemon
  `/health`, Chrome host identity, loaded service-worker bytes/build ID.

## Deployment readback

- Setup command: `pwsh -File scripts/synapse-setup.ps1 -SourceDir C:\code\synapse -ForceRestart`
- Setup exit: 0.
- Live PID: 22316.
- Installed executable SHA-256:
  `7C9E3BA0761D80C1CF541F13D6BEAFACD94BFB895CB1B7113CD77D074C3F26AE`.
- Public tool count: 40.
- Tool-surface SHA-256:
  `72d4068d45a2a0602776e9185e205edaf2c52d90e025a76a12f6e509975f8846`.
- Loaded bridge: `stale=false`, build
  `synapse-chrome-bridge-2026-08-06-operator-panic-recovery-v1`.

Setup initially caught a mixed identity: the newly loaded worker had the new
build ID while the daemon still expected the old constant. The expected ID was
updated and the full integrity-coupled build/deploy repeated before acceptance.

## Happy path 1: recover the real update-stranded latch

This was not fabricated state. A real OS panic hotkey plus bridge reload on the
previous build left the extension's durable local ledger disabled across daemon
replacement.

Before trigger (`act operation=operator_panic_status`):

```text
process: pending=false epoch=0 accounting_incident=false
browser: enabled=true  disable_sequence=0 all owner counts=0 healthy=true
extension: enabled=false disable_sequence=1 persisted_state_revision=281
extension active_after: every count=0, continuity=true, storage loaded=true
```

Trigger: `act operation=operator_panic_recover` with reason and exact generations
`extension=1`, `browser=0`, `epoch=0`.

Independent after read:

```text
process: pending=false epoch=0 accounting_incident=false
browser: enabled=true  disable_sequence=0 all owner counts=0 healthy=true
extension: enabled=true disable_sequence=1 persisted_state_revision=282
extension active_after: every count=0, continuity=true, storage loaded=true
```

## Happy path 2: real OS panic trigger and automatic terminal recovery

Before the physical Win32 `Ctrl+Alt+Shift+P` key events:

```text
process pending=false epoch=0
browser enabled=true disable_sequence=0
extension enabled=true disable_sequence=1 persisted revision=282
```

Immediate independent read after the hotkey:

```text
process epoch=1 pending=false accounting_incident=false
browser enabled=false disable_sequence=1 all owner counts=0
extension enabled=false disable_sequence=2 fully_drained=true revision=284
```

The next two-second poll independently read:

```text
process epoch=1 pending=false accounting_incident=false outstanding=0
browser enabled=true disable_sequence=1 all owner counts=0
extension enabled=true disable_sequence=2 all owner counts=0 revision=285
```

Thus the real trigger produced observable K1 disable and K2 cleanup/re-enable,
and the final state was read separately rather than inferred from the hotkey.

## Boundary and edge cases

All cases began and ended with extension `enabled=true/sequence=2/revision=285`,
browser `enabled=true/sequence=1`, process `pending=false/epoch=1`, and zero
owners. Each result was followed by a separate status read.

1. Missing/empty recovery reason: refused (`TOOL_PARAMS_INVALID`); no generation,
   enable bit, owner count, or revision changed.
2. Stale extension generation `999999`: refused
   (`ACT_OPERATOR_PANIC_RECOVERY_REFUSED`); all state unchanged.
3. Invalid epoch format `"invalid"`: schema/deserialization refusal
   (`TOOL_PARAMS_INVALID`); all state unchanged.

## Physical vault evidence

`audit operation=command_query`, filtered to tool `act`, separately scanned the
physical `CF_ACTION_LOG`: `scanned_rows=35`, `returned_count=30`,
`corrupt_row_count=0`, `noncanonical_key_count=0`. Evidence included:

```text
stale generation final row:
  key_hex=18c94d8700f0d8200000003b
  outcome=error error_code=ACT_OPERATOR_PANIC_RECOVERY_REFUSED
  value_sha256=sha256:3327d216d5b6e1ce600d4bd9f6250a7a01437128e2839790117f61050d56aa7c
invalid format final row:
  key_hex=18c94d86fa9db5b000000035
  outcome=error error_code=TOOL_PARAMS_INVALID
  value_sha256=sha256:039fb0b68a11164bba04dac3d084bd6ba87638df3ee99995177b21d46ddb0f39
latest status final row:
  key_hex=18c94d87071b59c800000041
  outcome=ok verb=operator_panic_status
  value_sha256=sha256:7271642395ff6e8746804328e9af26952110c0c81f83513e00907649c746f008
```

An additional real `browser_tabs` mutation created a tab and a separate list
observed it; later independent enumeration proved its exact target absent.
The close return disagreed with physical state, so unrelated issue #2032 records
that defect. Setup post-handoff readiness timing observed during deployment is
tracked separately as #2031.

## Gates

- `node --check extensions/synapse-chrome-debugger/service_worker.js`: pass.
- `cargo check --workspace`: pass during the edit loop.
- `pwsh -File scripts/lint.ps1`: pass for both root and Calyx workspaces.
- Real setup/deployment: pass.
- Manual FSV above: pass.

