# Issue #2050 FSV — action target identity lane

Date: 2026-08-09 (America/Chicago)

## Scope and root cause

The action projector omitted the authoritative session target paths, and the
remaining whole-value `syn_hash` lane was sparse. Ward admits dense slots only,
so successful target-bound actions could not contribute a usable target identity
score. The repair reads the two authoritative target paths and gives slot 49 a
dense `syn_one_hot` representation under the new immutable action generation
`2050001`. The prior generation `2020001` remains named and closed.

No outcome/status/error field enters slot 49. Target identity and action outcome
remain independent facts; a failure downstream of a valid target is not
manufactured into an out-of-region target.

## Research lanes used after diagnosis

- Exa MCP (`exa-search-server` 3.4.0) was probed live with
  `scripts/check-research-lane.ps1`, then used to retrieve primary calibration
  and evaluation material. NIST biometric evaluation guidance requires genuine
  and impostor transactions to be attributed to the correct score populations
  and outliers/removed scores to be investigated and documented.
- Built-in web search was independently exercised. NIST AI RMF 1.0 requires
  clearly defined, realistic test sets representative of expected use and
  documented test methodology:
  https://nvlpubs.nist.gov/nistpubs/ai/NIST.AI.100-1.pdf
- Microsoft documents that an HWND can be destroyed and recycled, so a stale
  numeric handle cannot be treated as durable identity evidence:
  https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-iswindow

These sources support the implemented posture: fixed representation contract,
real deployment-context evidence, and fail-closed liveness validation rather
than threshold relaxation or synthetic relabeling.

## Build and lint

Source checkout: `main` at `0fa5dfae3efc3e1ccb4f915e84ac47d43b3b5c1c`,
including prerequisite commit `17d7d77c`.

- `cargo check --workspace`: PASS (38.53 s)
- `pwsh -File scripts/lint.ps1`: PASS, all seven gates in both workspaces
  (shared lint contract, toolchain agreement, lock graph, fmt, deny, clippy,
  and public-API ratchet)

## Source of truth and baseline

The sources of truth were not tool return values:

1. Windows `EnumWindows`/`GetWindowRect` readback for target liveness and
   geometry.
2. Physical `CF_ACTION_LOG` rows, read separately with `audit command_query`.
3. Physical Calyx `Anchors` CF rows, read separately with `storage anchors`.
4. Hydrated Calyx per-slot CF vectors and Base rows, read separately with
   `hygiene grounding_gap`.

Initial `storage panel_coverage` physical census:

```text
syn-action-v1 active panel_version=2050001
active_version_records=391
grounded_records=196
superseded 2020001 records=654 closed=true
decode_failures=0 coverage_deficient_panels=[]
```

Initial slot-49 census after the no-target trigger was independently observed as
`records_present=23`, `grounded_records=13`, `records_slot_refused=0`.

## Happy path — live target A

Before:

```text
HWND 395336, Notepad.exe
bounds x=53 y=35 w=1404 h=1015
session target explicitly read back as {kind:window, window_hwnd:395336}
slot 49 records_present=23 grounded_records=13
```

Trigger: `act invoke` with `set_window_bounds` to the already-observed geometry,
making the operation side-effect-free while still traversing the real Windows
action and audit path.

After, independently read:

```text
GetWindowRect: x=53 y=35 w=1404 h=1015
CF_ACTION_LOG key=18ca1abf3a8fd6fc00000002 status=ok
value_sha256=e29f2f45c5351bbc8e49a753a32031b21caaccd53332502e4d33986d0dda9cb1
Anchors CF cx_id=f6252bc2814e89be7846fb33c4c9fa76
reward bool_value=true, panel_version=2050001
slot 49 records_present=25 grounded_records=14 records_slot_refused=0
```

The two slot rows are the started and terminal action records. The grounded
terminal row is physically anchored true.

## Edge 1 — no target

Before: `target clear` followed by a separate `target get` showed no current
target.

Trigger: the same bounds action.

After, independently read:

```text
structured failure: TARGET_NOT_SET
CF_ACTION_LOG key=18ca1ab1e9140d5000000000 status=denied
value_sha256=8a00c4818d32d8afb7ccd2e7b64b4860b65a7f7a107a649e787758ba122f253c
Anchors CF cx_id=58cbf17dba2fefebfbf3738370567873
reward bool_value=false, panel_version=2050001
```

The overall action panel advanced, while target slot 49 did not advance for the
targetless row. This proves `Absent(NotApplicable)` rather than an origin vector
or fabricated target identity.

## Edge 2 — invalid/stale HWND

Before: current session target was live HWND `395336`.

Trigger: bind `{kind:window, window_hwnd:1}`.

After:

```text
TARGET_WINDOW_NOT_FOUND
CAPTURE_TARGET_INVALID: HWND is not a live window
separate target get: current HWND remains 395336
```

The invalid handle was rejected before mutation. There was no fallback to the
human foreground and no stale target was published.

## Edge 3 — distinct live target B

Before:

```text
HWND 264078, Notepad.exe
bounds x=320 y=200 w=1404 h=940
slot 49 records_present=25 grounded_records=14
```

Trigger: bind target B and run the same side-effect-free bounds action.

After, independently read:

```text
EnumWindows/GetWindowRect: x=320 y=200 w=1404 h=940
CF_ACTION_LOG key=18ca1acc29d0acbc00000004 status=ok
value_sha256=3204df8995bddc10ebb37cb327a78bdc5fd9a75c846c9be3cb9a818bcf07a04a
Anchors CF cx_id=720e04f77c0dc74bd0f92e0c137837c5
reward bool_value=true, panel_version=2050001
slot 49 records_present=27 grounded_records=15 records_slot_refused=0
```

## Verdict

PASS. The installed daemon publishes the immutable repaired generation, real
successful session targets populate Ward-compatible slot 49, targetless actions
remain absent, invalid HWNDs fail loudly without changing session state, two
distinct live targets traverse the lane, and terminal outcomes exist as
independently read physical Anchors CF rows. No threshold, FAR, corpus minimum,
fallback, or mock data was introduced.
