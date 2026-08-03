# FSV: anchor lineage, probe cost, and unattended repair (#1981-#1984)

Date: 2026-08-03. Host: the configured Windows end-user system.

## Source of truth

The trigger ran only against a sacred-data backup produced by the live daemon's
`storage backup` operation:

- vault: `%LOCALAPPDATA%\synapse\fsv\issues-1981-1982-final-20260803T1815Z\vault`
- durable sequence before trigger: `313030`
- files: `1961`
- backup manifest SHA-256:
  `45bbe8c41fb8edf8ca7ab92e9a0e74ab582fe719fbfc097d60e7af67790addd9`

The backup surface independently hashed every copied file. The post-trigger
read used a newly opened `Db` and folded every physical Base row; it did not use
the trigger's return value.

## Diagnosis and research

The old carry reconstructed a superseded `CxId` from a mutable source row's
current bytes. That ID frequently never existed. It then performed one active
and two historical Anchors-prefix scans for each of 50,973 transcript rows.

The Exa MCP lane was probed before research and reported `live` (server 3.4.0).
Exa and the built-in web lane were used after diagnosis. Primary upstream
ordered-store guidance confirms that ordered iterators provide a consistent
point-in-time range view and that batched/targeted reads avoid repeated lookup
overhead. Sources: [RocksDB overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview),
[basic operations](https://github.com/facebook/rocksdb/wiki/Basic-Operations), and
[MultiGet performance](https://github.com/facebook/rocksdb/wiki/MultiGet-Performance).

The implemented shape is one cold physical Base census indexed by stable
`(source_cf, source_key)` identity. Rows with no historical grounded evidence
perform no Anchors read. Only the grounded subset checks the exact typed active
anchor key. Unknown source identity is counted separately and never classified
as repaired.

## Full-corpus trigger and readback

Pre-trigger physical census:

```text
CF_AGENT_TRANSCRIPTS rows=50973
Base rows=114498
transcript exact stranded source identities=113
```

First complete trigger (51 pages):

```text
examined=50973
inserted current source revisions=50973
anchors_carried_forward=140
elapsed_ms=305413
```

The 140 writes exceed the 113 identity debt because 27 source keys had a
grounded older active revision while their current byte revision still needed
the anchor. That is append-only revision history, not duplicate identity.

Independent post-trigger reopen and full Base decode:

```text
Base rows=165471 decoded=165471 failures=0 atomic=true
syn-agent-transcript-v1 active grounded=26387
syn-agent-transcript-v1 superseded grounded=140
syn-agent-transcript-v1 exact stranded=0 unknown_identity=0
```

The same census continues to report genuinely unrepairable debt instead of
hiding it: `syn-outcome-v1=6` and `syn-mcp-usage-v1=135`; neither panel declares
a source backfill path. The maintainer classifies this as
`anchor_debt_unbackfillable`.

## Performance and idempotency

The original implementation took `720942 ms` and issued 101,946 logical
historical probes while carrying nothing because the reconstructed IDs were
wrong. The indexed implementation's first full materialization took
`305413 ms`, a 57.6% reduction while physically carrying 140 anchors.

A second complete sweep over the same unchanged physical vault proved the
current-revision and anchor paths are idempotent:

```text
before Base=165471 stranded=0
examined=50973 inserted=0 already_current=50973 carried=0
elapsed_ms=124137
after Base=165471 stranded=0
```

An identical anchor installed by historical carry is exact-read before the
fresh outcome writer. Exact equality is accepted without a second ledger
entry; conflicting evidence fails closed and cannot overwrite an immutable
grounded outcome.

## Boundary audit

1. Maximum observed corpus: all 50,973 physical transcript rows were examined;
   page accounting was 51,023 candidates (one lookahead per non-final page),
   50,973 examined, and no omissions.
2. Nonexistent vault: before `exists=False`; command exited 1 with `vault
   directory does not exist`; after `exists=False`.
3. Invalid extra argument: before manifest SHA-256
   `E3854E252658C70A5CC1DA80104F093A27E8177468028193448E5DA8964E74EE`;
   command exited 1 with `exactly one vault directory is required`; after hash
   was byte-identical.

The unattended driver now treats exact anchor debt independently from coverage
debt, retains its physical page cursor across ticks, prioritizes anchor debt,
names the firing reason, reports panels with no repair path, and latches
`STORAGE_DERIVED_STATE_ANCHOR_DEBT_NO_PROGRESS` after a completed sweep fails
to reduce the next physical census. It refuses another automatic sweep until
the debt or panel generation changes.

## Gates

`cargo check --workspace` passed. `pwsh -File scripts/lint.ps1` passed all seven
gates in both workspaces, including formatting, dependency policy, root and
Calyx clippy, and the public API ratchet.

## Installed daemon readback

Commit `5e93e29c` was deployed through `scripts/synapse-setup.ps1`. Candidate
validation passed, the installed executable SHA-256 was
`0FCCE6F8508F22540CC22596316747B424985E795E9A62748D247A11203F5670`, and the
OS/health readback named installed PID `12424`, build `5e93e29c6e31`, 40 public
tools, and overall health `ok` before the maintenance trigger. CPU math selected
AVX2; the host has no NVIDIA/CUDA device, and all row-guard over-budget and
starvation counters were zero.

The first unattended tick selected the transcript panel, processed 43 pages in
its 60-second budget, reported `budget_exhausted`, inserted zero rows, and kept
its cursor. The next tick resumed rather than restarting. No manual backfill was
invoked. A separate public `storage panel_coverage` read then reported:

```text
Base rows=114525 records_total=114525 decode_failures=0 accounting_holds=true
syn-agent-transcript-v1 active_version_records=50973
anchors_stranded_on_superseded=0
anchors_stranding_identity_unknown=0
backfill_owed=false
```

The same installed read still named `syn-outcome-v1=6` and
`syn-mcp-usage-v1=135` as stranded and unbackfillable. This is the expected
fail-closed state until #1965 supplies those panels with a source path.
