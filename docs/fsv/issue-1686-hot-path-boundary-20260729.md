# FSV — issue #1686: Calyx hot-path boundary

Date: 2026-07-29 (UTC) · Host: configured Windows 11 host · Daemon: live, pid 18628, deployed from `7d0f16b0`

Instrument: `scripts/diagnostics/issue-1686-hot-path-boundary-fsv.ps1`.

Scope accepted here is the boundary-enforcement half: the reflex tick reads a frozen,
fingerprinted artifact and makes **zero** Calyx calls. New artifact kinds for armed-routine
constellations, next-occurrence windows and kernel hot sets are **not** in scope — they
depend on #1677 (Ward) and #1678 (Oracle), neither of which is a dependency of any Synapse
crate. The capture half is vacuously satisfied and was confirmed: `crates/synapse-capture`
depends only on crossbeam, serde, synapse-core, synapse-telemetry, thiserror, tracing and
windows — no Calyx or storage dependency at all.

## What was wrong

`enter_hot_context()` had **zero callers repo-wide**, so the cold assertion could never fire.
`LoweredArtifactHandle` and `lower_guard_thresholds` had zero callers outside re-exports, and
`%LOCALAPPDATA%\synapse\db-daemon\lowered\` did not exist — nothing had ever been published.
The mechanism was complete; the wiring was absent. The check was also `debug_assert!`, so a
release daemon would never have tripped it, making any release-run acceptance claim
worthless. It is now an always-on counter plus a structured log, with the hard assert kept in
debug.

## Two false starts, both caught by the instrument refusing to pass

**The first run was vacuous.** `tick_thread_tagged=false`, `hot_ticks_total=0`. The
scheduler is created *only* by `ReflexRuntime::register`, so with no reflex registered there
is no tick thread at all. Zero violations across a window where the hot path never ran proves
nothing, and `tick_thread_tagged=false` is indistinguishable from broken tagging. The
instrument asserts the tick count advanced *before* drawing any conclusion, so it failed
rather than reporting a green boundary.

Health now names that condition instead of leaving fields silently absent:
`scheduler_started`, `feed_unavailable_code`, `feed_unavailable_reason`.

**The trigger was harder to find than it should be.** `reflex_register` is an implementation
tool with no MCP surface at any profile — confirmed by checking `tools/list` at
`normal_agent` and again after acquiring the foreground lease and setting `break_glass`; both
show only the 40 public facade tools. The reachable path is the public **`routine` facade**
with `operation="reflex_register"`, and it requires `WRITE_REFLEX` in the daemon's
`--allowed-permissions` (a startup flag, not a runtime grant). The daemon was redeployed with
that grant for this run.

## Manual FSV — all checks passed

**At rest (no reflex registered).** The honest state, reported honestly:

```
scheduler_started      = false
feed_unavailable_code  = REFLEX_SCHEDULER_NOT_STARTED
feed_unavailable_reason= "no reflex is registered, so the scheduler thread does not exist:
  tick_thread_tagged=false and hot_ticks_total=0 mean there is nothing to tag, not that
  tagging is broken. Register one through the public facade `routine operation=reflex_register`…"
violations_total       = 0
violation_code         = <absent>
```

`violation_code` being absent at zero violations is itself a fix verified here: the first run
reported `violations_total=0` alongside a populated
`violation_code="SYNAPSE_CALYX_HOT_PATH_BOUNDARY_VIOLATION"` — a constant label that read as
an observation.

**The artifact, re-hashed independently:**

| Property | Value |
|---|---|
| publishes | 12 attempts, 12 success, 0 failure, 0 skipped |
| `Get-FileHash` of the artifact | `58261923fe0d099e2929640674f8dca77c01b2103426b3a6e048ea6419062a1f` |
| health `artifact_file_sha256` | identical |
| file hash vs `content_sha256` | **differ**, correctly — the content hash covers only the frozen payload, the file is the whole pretty-printed envelope |
| envelope `fingerprint.content_sha256` | `6b1f39ebef8493dbefeb5027d2eecf035bec01e9e2749ba57ac5907f4c2af5b8` = `publish_last_content_sha256` |
| envelope pins | a `source_ledger_seq` and the `vault_id` |

**Under a real tick load** (one reflex registered, matching nothing so it never fires; the
scheduler ticks at ~1 ms regardless):

| | before | after 60 s |
|---|---|---|
| `tick_thread_tagged` | **true** | true |
| `hot_ticks_total` | 134,639 | **194,988** (+60,349) |
| `artifact_hot_reads_total` | 473 | **60,823** (+60,350) |
| `violations_total` | 0 | **0** |
| `scheduler_started` | true | true |
| `feed_unavailable_code` | absent | absent |
| artifact content hash | `6b1f39eb…` | unchanged |

**60,349 real ticks, every one reading the frozen artifact, zero boundary violations.** The
hot-read count tracking the tick count one-for-one is the direct evidence that the tick is
served from the artifact rather than from Calyx.

The vault advanced 24 sequences during the window. That is other subsystems writing, and it
neither proves nor refutes the boundary claim — `violations_total == 0` is the direct
evidence, which is why the instrument says so inline rather than implying the seq delta means
something.

**Cleanup verified.** The reflex was cancelled and an independent `reflex_list` readback
confirms 0 active reflexes — the FSV left no state on the operator's daemon.

## Known follow-up

`lower_guard_thresholds` is still reachable only through a second, independently written
producer in `maintenance.rs`, because the only holder of `Arc<SynapseCalyxVault>` sits behind
a private `with_vault`. Two constructors for one fingerprinted artifact is a drift hazard —
filed as **#1885**.
