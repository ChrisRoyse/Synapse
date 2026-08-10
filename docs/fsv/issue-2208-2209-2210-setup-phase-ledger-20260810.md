# FSV — #2208 / #2209 / #2210: truthful, durable setup phase ledgers

Date: 2026-08-10
Host: configured Windows production host
Branch: `main` only

## Sources of Truth

- Ledger result: the separately reopened
  `%LOCALAPPDATA%\synapse\logs\setup-phase-timings.json`, or the deliberate
  absence of that file at a synthetic target when publication fails.
- Ledger-write failure: the structured
  `SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED` message retained in script scope and
  emitted by the real failure caller/trap.
- Mutation isolation: the OS process table, installed executable hash and
  production maintenance-record hash before and after each whole-script edge.

Return values were recorded, but were not accepted as evidence without those
separate reads.

## Defects and first-principles diagnosis

### #2208

The real #2207 completion ledger measured phases of 834.819, 51.729, 24.056,
23.620, 7.820 seconds, but `slowest_phases` physically held phases 1-5 in
insertion order: 5.120, 834.819, 5.414, 24.056, 2.304 seconds.

`Stop-SynapseSetupPhaseTimer` stored each row as `[ordered]@{}`, a
`System.Collections.Specialized.OrderedDictionary`. The writer asked
`Sort-Object -Property elapsed_seconds` for an object property which those
dictionaries did not expose. Every sort key was therefore null and the original
order survived. The smallest exact reproduction—three rows with values 5, 834
and 51—returned 5, 834, 51.

### #2209

The invalid numeric boundary correctly caused the writer to return null and
publish no corrupt JSON, but `catch { return $null }` discarded the ErrorRecord.
An empty `LogDir` also returned null without constructing an error. The normal
caller could only emit a generic warning, the `Die` caller emitted nothing, and
raw PowerShell/.NET failures caught by the top-level trap did not call the phase
writer at all. Filesystem denial, bad internal data and invalid path state were
therefore indistinguishable—or completely silent.

### #2210

The writer called `WriteAllText` directly on the authoritative ledger. That
truncates the destination before all replacement bytes are known durable, does
not call `Flush(true)`, and provided no post-write byte or JSON identity
readback. Process loss or an I/O exception could therefore leave partial bytes
while the function claimed no publication; success proved only an API return,
not the physical Source of Truth.

## Research performed after diagnosis

All three issues used an issue-specific query through the real Exa MCP server, then
an independent built-in web lane which opened the primary Microsoft sources:

- [`Sort-Object`](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/sort-object?view=powershell-7.6)
- [`about_Hash_Tables`](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_hash_tables?view=powershell-7.6)
- [`about_PSCustomObject`](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_pscustomobject?view=powershell-7.6)
- [`about_Error_Handling`](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_error_handling?view=powershell-7.6)
- [`about_Try_Catch_Finally`](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_try_catch_finally?view=powershell-7.6)
- [Microsoft's exception guidance](https://learn.microsoft.com/en-us/powershell/scripting/learn/deep-dives/everything-about-exceptions?view=powershell-7.6)
- [`.NET FileStream.Flush(Boolean)`](https://learn.microsoft.com/en-us/dotnet/api/system.io.filestream.flush?view=net-10.0)
- [`.NET File.Replace`](https://learn.microsoft.com/en-us/dotnet/api/system.io.file.replace?view=net-10.0)
- [Win32 moving and replacing files](https://learn.microsoft.com/en-us/windows/win32/fileio/moving-and-replacing-files)

Microsoft documents that `[ordered]` creates an OrderedDictionary, while a
`PSCustomObject` exposes named members and preserves literal member order;
`Sort-Object` supports calculated numeric properties and `-Stable` preserves
input order when keys tie. Microsoft also documents that `$_`/`$PSItem` in a
catch is the ErrorRecord and that recording a secondary error must preserve the
original failure rather than replacing its useful origin.
Microsoft further documents that `Flush(true)` clears intermediate file
buffers to disk, `File.Replace` installs a same-volume replacement while
creating a recovery backup, and Win32 replacement preserves the destination's
attributes.

## Implementation contract

- Phase rows are `[pscustomobject][ordered]@{...}`: real schema properties with
  deterministic JSON member order.
- Ranking uses an explicit `[double]` calculated expression, descending, with
  `-Stable`, then selects at most five rows.
- Every write failure retains a structured
  `SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED` value naming outcome, log directory,
  resolved target (or `<unresolved>`), sanitized exception detail and repair
  action.
- Empty `LogDir` is an explicit failure, not a silent return.
- Completed, `Die`, and raw top-level trap paths emit that retained diagnostic.
- The top-level trap attempts the failed ledger immediately but never replaces
  its primary error if the diagnostic itself fails.
- Publication writes exact UTF-8 bytes to a same-directory GUID temporary file
  opened `CreateNew`/`WriteThrough`, calls `Flush(true)`, closes it, then uses
  `File.Move` for create or `File.Replace` with a GUID recovery backup.
- The published path is independently reopened. Exact length, SHA-256, fixed-
  time byte equality, JSON schema, outcome and PID must all match before success.
- The recovery backup is removed only after that readback; every GUID temp and
  backup is independently checked absent. Failure diagnostics state the exact
  phase and whether publication/readback completed, so they never lie about
  which file remains authoritative.
- No fallback ledger, fabricated duration or partial-success claim exists.

## Manual synthetic state matrix using the checked-in writer

The PowerShell parser loaded the exact function definitions from the checked-in
`scripts/synapse-setup.ps1` AST in-process; no copied implementation, test
harness or mock writer was used. Every case printed physical state before,
called that writer, then separately reopened the target and enumerated all
GUID-named temporary/recovery artifacts.

| Case | Before | Physical after | Exact evidence |
|---|---|---|---|
| create / empty phases | target absent; no artifacts | valid completed ledger, zero phases, zero slowest rows; returned exact path; no artifacts | SHA-256 `CA394DEFAAF0782911488CF2F08FA0C13EC721D8905B4D154B2A78A61AE4ADED` |
| replace + 11 rows + boundary tie | prior body `{"prior":true}`; SHA-256 `25930DBC97628788CBB0B877909FC6FFE51D0B66188D03AE6C9D07643B57BF01` | 3,432-byte valid ledger; returned exact path; no temp/backup | SHA-256 `03EB2AE6867308C59527FFC4EF6036B4673897247F641B17C1EAFEDDFD17BCCA`; actual/expected/independent `[2,6,4,10,9]` |
| invalid numeric preserves prior | body `{"sentinel":"KEEP-ME"}`; SHA-256 `AB7ED76396CBC002C22218FCBD44AF1EF5E6CD0287519C4A111B5BE4B4FBAF60` | return null; exact body/hash unchanged; no artifacts | `phase=not_started published=False readback_validated=False`; value named |
| target path is a directory | directory exists; no artifacts | same directory exists; return null; GUID temp absent | `phase=create published=False`; exact `File.Move` collision named |
| empty `LogDir` | no target resolvable | return null; no fallback | `target/temp/recovery_backup=<unresolved>` |
| `Die` with invalid row | target absent | target/artifacts absent; secondary warning emitted; primary error thrown | both structured ledger error and `FATAL: SYNTHETIC_PRIMARY_FAILURE` observed |

The tie case used these measured values:

```text
1=5.000, 2=834.819, 3=5.414, 4=24.056, 5=2.304,
6=51.729, 7=1.474, 8=6.408, 9=7.820,
10=23.620, 11=7.820
```

Rows 9 and 11 tie at the fifth-place boundary; stable input order selected row
9 exactly as specified.

The invalid row preserved the prior authoritative bytes and retained:

```text
SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED outcome=completed
phase=not_started published=False readback_validated=False
target_state=[file length=23 sha256=AB7ED76396CBC002C22218FCBD44AF1EF5E6CD0287519C4A111B5BE4B4FBAF60 exact_expected=False]
detail=[Cannot convert value "not-a-number" to type "System.Double" ...]
remediation=an exact new ledger was not observed; inspect the reported target state and named recovery paths before deciding which prior bytes are authoritative; no fallback path was used
```

The empty target left no sentinel and retained:

```text
SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED outcome=completed log_dir=[]
phase=not_started published=False readback_validated=False
target=[<unresolved>] target_state=[<unresolved>]
temp=[<unresolved>] recovery_backup=[<unresolved>]
detail=[LogDir is empty; no phase-ledger target can be resolved]
```

Calling the real `Die` function with the invalid row emitted both:

```text
[synapse-setup] WARNING: SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED outcome=failed ...
CAUGHT=[synapse-setup] FATAL: SYNTHETIC_PRIMARY_FAILURE
```

No ledger file existed afterward. This proves the secondary error is visible
without masking the primary one.

## Whole-script integration: real measured ledger

Before the trigger:

- production daemon PID: 9104;
- installed executable SHA-256:
  `298CFA5817B237E85361CFDBC3406578A680CEE10C4F2462DDB3EF1CA27FBCFE`;
- production maintenance-record SHA-256:
  `68E9429912DA871112A2C3A5CB795C7737BE8D9B9F567B3C59897A518BB830F0`;
- preceding failure-ledger SHA-256:
  `1792164C89EE70C3C823E2E8AD5CD568C9BAC3AC4F7893412832F5783338B27F`;
- phase temp/recovery artifact count: zero.

The real `-SkipBuild` setup trigger intentionally met the strict provenance
boundary: the installed daemon was built from `0cf6e834…`, while `HEAD` had
advanced to the documentation commit `d552b40c…`. Candidate validation failed
closed with `SYNAPSE_CANDIDATE_BUILD_PROVENANCE_UNACCEPTABLE` before production
handoff. This is an expected rejection, not a claimed happy-path deployment.

The actual failure ledger was atomically replaced and independently reopened at
`2026-08-10T15:17:37.2325639Z`:

```text
outcome=failed
pid=752 (absent after trigger)
length=3,638 bytes
phase 1 Preflight                         2.484 s
phase 2 Bearer token + data dirs          9.389 s
phase 3 Validating candidate daemon       6.087 s
slowest_phases=[2,3,1]
independent recomputation=[2,3,1]
exact_match=true
temp/recovery artifact count=0
```

The physical ledger SHA-256 was
`3ECCCE882FDCEFBD2CCFC4002E223B3774A2BCC81FBDCC465FC56BDCD24AD6A7`.
The v2 lock separately read `state=failed`, PID 752, exact token
`747b0d80-a32c-40af-b3fe-dc56d97f2d25`; its SHA-256 was
`E958BA34433B3F6509AA9783F21F7A002EB3B66B33FFB4A07C4A8C9010181365`.
Production remained PID 9104 with the exact same installed executable hash and
clean live vault.

## Whole-script integration: invalid target and trap reporting

To exercise the raw trap without mutating the production lock, the real script
ran `-Start -LogDir ''` with an isolated maintenance path under `%TEMP%`.

Before: isolated record absent, production PID 9104 live. Trigger: real setup
PID 17044 acquired the isolated v2 record, then PowerShell rejected the empty
`LogDir` at the first staging call. Output contained the exact structured
ledger warning and the original binding failure:

```text
[synapse-setup] WARNING: SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED
  outcome=failed phase=not_started published=False readback_validated=False
  log_dir=[] target=[<unresolved>] target_state=[<unresolved>]
  temp=[<unresolved>]
  recovery_backup=[<unresolved>]
  detail=[LogDir is empty; no phase-ledger target can be resolved]
  remediation=an exact new ledger was not observed; inspect the reported target state and named recovery paths before deciding which prior bytes are authoritative; no fallback path was used
Cannot bind argument to parameter 'LogDir' because it is an empty string.
```

After independent reads:

- PID 17044 absent;
- no phase ledger at an invented/fallback location;
- isolated lock `state=failed`, PID 17044, token
  `a518c784-c1af-4114-9d83-24ef6c89d1b5`;
- isolated lock SHA-256
  `F7DD2A4C0A0BCF76D6AE44E250FF02BBD0E5B254E268F74AEA173A1C466CB69A`;
- production maintenance record still the exact
  `E958BA34…` file from the preceding real setup rejection;
- production daemon still PID 9104, build `0cf6e834…`, with the exact installed
  executable SHA-256 above.

## Gates

- PowerShell AST parser: zero errors after the final edit.
- `cargo check --workspace`: passed on the final source.
- `pwsh -File scripts/lint.ps1`: all gates passed in both the root and excluded
  Calyx workspaces after the final edit, including formatting, cargo-deny and
  clippy with warnings denied.
- Acceptance used no automated tests or mock data.

The evidence proves both directions against physical state: correctly formed
durations publish an independently reproducible ranking, while invalid data or
an invalid target publishes no false ledger, emits a precise diagnostic, keeps
the original failure authoritative, releases ownership and leaves production
untouched.

After recording the evidence, the four exact final synthetic roots (3 files /
4,240 bytes; 4 files / 5,341 bytes; an empty root; and 1 file / 473 bytes) plus
this run's retained candidate diagnostic (80 files / 264,241 bytes) were
recursively removed. Independent path reads reported all five absent; no vault,
build cache or unrelated candidate was removed. Earlier #2208/#2209 intermediate
evidence roots and their 263,047-byte rejected candidate had already been
removed after their evidence was incorporated above.
The final catch-state matrix and isolated whole-script readback added one last
exact root (4 files / 5,022 bytes); it too was removed and independently read
absent after the final source was exercised.
