# Issue #1668: durable panel-lifecycle Registry foundation (2026-08-04)

This is a foundation checkpoint, not issue acceptance. The MCP facade,
new-record dual measurement, and background backfill worker remain outstanding.

## Diagnosis and research

`calyx-registry::SwapController` and its queue existed but had no caller. Its
state was in memory, while `BackfillScheduler` persisted to an unrelated JSON
file. Synapse panels other than the default are code-declared contracts; using
`persist_vault_panel_state` for a mutation would replace the manifest's serving
default rather than update a per-panel catalog.

The Exa lane was probed first with `scripts/check-research-lane.ps1`: server
`exa-search-server` v3.4.0 initialized, advertised two tools, and a real search
returned content (`exa_mcp=live`). Exa and the built-in web lane were then used.
The primary-source design basis was CockroachDB's online schema-change model:
multiple schema versions, durable observable background jobs, small resumable
backfill batches, and resource-pressure pausing. See:

- <https://www.cockroachlabs.com/docs/stable/online-schema-changes>
- <https://github.com/cockroachdb/cockroach/blob/master/docs/RFCS/20151014_online_schema_change.md>
- <https://www.cockroachlabs.com/docs/stable/show-jobs>

The resulting ownership rule is: the manifest remains the serving default;
each logical panel owns one revision-guarded Registry-CF lifecycle row.

## Source of truth and trigger

The physical source of truth was Aster `Registry` CF in a fresh real vault:

`%TEMP%\synapse-panel-lifecycle-fsv-0a1eac1983584416a2a560b21a4d4524`

Trigger:

```powershell
cargo run -p synapse-calyx --example panel_lifecycle_registry_fsv -- <fresh-vault-dir>
```

The example separately scanned `Registry` before and after every action. It did
not use a returned success value as proof.

## Physical evidence

Before add: `Registry rows=0`.

After adding one frozen deterministic byte-feature lens with three historical
candidate ids:

```text
Registry rows=2
panel generation=11
slot=0
queued=3
committed seq=3
lifecycle-row bytes=1782
sha256=7fb4dd9527d318ebd3972c4081789f08a55a693bbb9889194dfa0928f1e46194
```

The second row is the vault-global generation allocator. The returned lifecycle
hash was recomputed independently from the scanned Registry value and matched.

Park and retire retained the slot in the panel:

```text
park:   generation=12 slot_count=1 state=parked
        sha256=b10319d907eb8b3eb2778c0f8f8b8fa57014ef5ec4ca1c0625fa6c8124ca7353
retire: generation=13 slot_count=1 state=retired historical_slot_present=true
        sha256=89caf067df080e4197ec405d7cf251bf176dd8b24a13ff4b95307cbcd04f7be6
```

## Boundary audit

Every boundary printed the full Registry row count before and after and compared
the complete ordered row set:

| Boundary | Result | Physical state |
|---|---|---|
| identical add replay | `existing_identical=true` | 2 rows -> 2 rows; byte-identical |
| panel name containing NUL | `SYNAPSE_CALYX_PANEL_LIFECYCLE_INVALID` | 2 -> 2; unchanged |
| external-command runtime | `SYNAPSE_CALYX_PANEL_LIFECYCLE_RUNTIME_UNSUPPORTED` | 2 -> 2; unchanged |
| reactivate retired slot | `SYNAPSE_CALYX_LENS_FROZEN_VIOLATION` | 2 -> 2; unchanged; historical slot retained |

The last boundary initially exposed an allocator side effect before transition
validation. Validation was moved ahead of allocation, then the entire fresh-vault
sequence was repeated and passed.

## Supporting gates

`cargo check --workspace` passed after the physical FSV. The full
`scripts/lint.ps1` result is recorded with the commit that contains this file.
