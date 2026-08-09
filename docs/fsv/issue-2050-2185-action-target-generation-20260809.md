# Issues #2050/#2185 FSV — dense action target lane and immutable generation repair

Date: 2026-08-09 (America/Chicago)

## Root cause

The action projector originally omitted the authoritative top-level session
target paths. The only target lane was also a sparse, one-cell whole-value hash,
which Ward cannot score as a dense cosine region.

Current main added dense, graded slot 117 under action generation `2050001`.
Manual FSV then found a second defect: a pre-merge local build had already
written a different physical slot map under the same generation id. The vault
contained 408 old rows without slot 117 and fresh current-binary rows with it.
One generation therefore named two layouts.

The repair moves the complete #2050 contract to new generation `2185001` and
declares contaminated `2050001` superseded. Storage, lens provenance, action
validation, and readiness all name `2185001`; no writer can append new rows to
the contaminated generation.

## Research

Both required lanes were used after diagnosis:

- Exa MCP was probed live with `scripts/check-research-lane.ps1`
  (`exa-search-server` 3.4.0) and used to retrieve primary calibration and
  evaluation material. NIST biometric evaluation guidance requires genuine and
  impostor transactions to remain attributable to the correct score population
  and requires anomalous/removed scores to be investigated and documented.
- Built-in web search independently retrieved NIST AI RMF 1.0, which requires
  realistic, deployment-representative test sets and documented methodology:
  https://nvlpubs.nist.gov/nistpubs/ai/NIST.AI.100-1.pdf
- Microsoft documents that HWND values can be destroyed and recycled, so a
  stale numeric handle is not durable identity evidence:
  https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-iswindow

This supports the implemented posture: real deployment-context evidence,
fixed representation contracts, and fail-closed liveness/generation handling;
no threshold relaxation or relabeling.

## Compile, lint, and deployed source

```text
cargo check --workspace: PASS
pwsh -File scripts/lint.ps1: PASS, every gate in both workspaces
installed PID: 21420
installed build: 5e629a6a2eff16aa7197b618fcef95b8d5b67045
build_profile: release
build_matches_checkout: true
build_tree_state: clean
tool_count: 40
daemon health ok: true
```

Setup built with 12 jobs on the 12-logical-CPU host. Health independently
reported CPU math `auto -> cpu` because no NVIDIA device exists, AVX2 selected,
and the AVX2 reduction probe bit-for-bit equal to portable dot/cosine/L2/top-k.

## Sources of truth

Return values were not acceptance evidence. The independent reads were:

1. Windows `EnumWindows` and `GetWindowRect` for HWND liveness and geometry.
2. Physical `CF_ACTION_LOG` rows via a separate `audit command_query` call.
3. Physical Calyx `Anchors` CF rows via separate `storage anchors` calls.
4. Hydrated Base/per-slot CF vectors via `hygiene grounding_gap`.
5. Full physical source/panel census via `storage panel_coverage`.
6. Installed process provenance and SIMD/backend probes via a separate health
   read.

## Before state

Exact current-source daemon `c12c60ad` over contaminated `2050001`:

```text
records_scanned=406
slots present in old rows: 48,49,50,51,52
slot 117 absent
```

After one fresh current-binary action:

```text
records_scanned=410
slot 117 records_present=2 grounded_records=1
```

That direct physical contrast proved two layouts under one generation and
caused #2185 to be filed. It was not hidden by a passing action return.

After deploying the repair, the new generation baseline was independently read
before any FSV trigger:

```text
panel_version=2185001
records_scanned=0
slot_coverage=[]
```

## Happy path — live target A

Before:

```text
HWND 395336, Notepad.exe
EnumWindows bounds x=53 y=35 w=1404 h=1015
session target readback {kind:window, window_hwnd:395336}
```

Trigger: real `act invoke/set_window_bounds` to those already-observed bounds,
which traverses the production Windows/audit path without changing UI geometry.

After, independently read:

```text
GetWindowRect x=53 y=35 w=1404 h=1015
CF_ACTION_LOG key=18ca1ea42554a1a800000002 status=ok
value_sha256=2e0a04bed6a6ada1c862d39d8b0e0d5df1a273cf46746bebc0344b40a078dc9e
Anchors cx_id=a8afeb2aa6ee1ca8dd36dbfbe79a8884
reward=true panel_version=2185001
```

## Edge 1 — no target

Before: `target clear`, followed by a separate target read showing no current
target.

Trigger: the same bounds action.

After, independently read:

```text
TARGET_NOT_SET (refused before delivery)
CF_ACTION_LOG key=18ca1ea41dcef88400000000 status=denied
value_sha256=119e267c038e3a0cab36a7ac5e9b34dc7565a227e2225fb78f2014d663df5325
Anchors cx_id=5c618adad043419238211d7e1004ebde
reward=false panel_version=2185001
```

Target slots remain absent for targetless records; no origin vector or fallback
to human foreground is manufactured.

## Edge 2 — invalid HWND

Before: current target was live HWND 395336.

Trigger: bind `{kind:window, window_hwnd:1}`.

After:

```text
TARGET_WINDOW_NOT_FOUND
CAPTURE_TARGET_INVALID: HWND is not a live window
separate target read: current HWND remains 395336
```

The invalid handle was rejected before session-state mutation.

## Edge 3 — distinct live target B

Before:

```text
HWND 264078, Notepad.exe
EnumWindows bounds x=320 y=200 w=1404 h=940
```

Trigger: bind target B and run the same no-op geometry action.

After, independently read:

```text
GetWindowRect x=320 y=200 w=1404 h=940
CF_ACTION_LOG key=18ca1ea5763abc0000000004 status=ok
value_sha256=a4c2d780f0c37aa03339e4b05f1ce8f38ea481efb15d4bae4dc44bf0c8a3470e
Anchors cx_id=e1e2b0085cbc43c290fe402a72c97913
reward=true panel_version=2185001
```

## Maintainer and final physical state

Before the real five-minute derived-state tick:

```text
active_version_records=11 source_cf_rows=30
coverage_fraction=0.3666667 backfill_owed=true uncovered_rows=19
```

No manual row insertion or test harness was used. The deployed maintainer ran:

```text
attempts_total=1 success_total=1 failure_total=0
last_tick_failed=false last_delta_changed_keys=174
```

Independent final census:

```text
active panel_version=2185001
active_version_records=30 source_cf_rows=30
coverage_fraction=1.0 backfill_owed=false uncovered_rows=0
coverage_deficient_panels=[] decode_failures=0
grounded_records=16 grounded_fraction=0.5333334
slot 49:  records_present=10 grounded_records=5 refused=0
slot 117: records_present=10 grounded_records=5 refused=0
2050001: closed=true records=410 grounded_records=206
```

The exact population agreement between slot 49 and slot 117 proves every
target-bearing row in the new generation carries both the exact and graded
representations. Targetless rows correctly carry neither. Contaminated history
is preserved and closed rather than rewritten or silently reinterpreted.

## Verdict

PASS for #2050 and #2185. The exact deployed release binary writes a dense,
graded target lane, target paths are retained on real successes, invalid and
absent targets fail/measure honestly, the new immutable generation is fully
backfilled by the real maintainer, and all terminal outcomes exist as physical
Anchors CF rows. No mock data, fallback, FAR/threshold change, or direct vault
write was used.
