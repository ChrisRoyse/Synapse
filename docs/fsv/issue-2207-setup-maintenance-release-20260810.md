# FSV — #2207: setup maintenance ownership and detached-parent release

Date: 2026-08-10  
Host: configured Windows production host  
Branch: `main` only  
Implementation commit installed for the final proof: `0cf6e8348344ec2404da6473b96ce5a7505b8ca8`

## Defect and root cause

The setup maintenance Source of Truth is
`%LOCALAPPDATA%\synapse\setup-maintenance.lock.json`, protected by the open
`FileStream` which owns the corresponding Windows file handle.

The old success path wrote the completion phase ledger, printed the phase report
and other success output, and only then released the maintenance handle. If the
detached caller disappeared while setup was in that reporting tail, Windows
closed the process handle but the last durable JSON remained `state=held`. The
record also had no per-acquisition identity, and release neither proved that the
record still described the current owner nor verified the durable released
bytes. A release failure was only a warning. Therefore a completed operation
could leave durable state that described an owner which no longer existed.

## Research performed after diagnosis

`pwsh -File scripts/check-research-lane.ps1` exercised the configured Exa MCP
server through real JSON-RPC initialization, `tools/list`, and `tools/call`.
The observed lane was live (`exa-search-server` 3.4.0). Exa was used as the
supplemental lane. The independent primary-source lane used:

- Microsoft PowerShell documentation: a `finally` block runs when control leaves
  a guarded block, including terminating errors, which supports keeping handle
  disposal in the unconditional outer cleanup path:
  <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_try_catch_finally?view=powershell-7.6>
- Microsoft Win32 documentation: each `CreateFile` opens a handle which the
  process must close, establishing the OS handle—not prose JSON—as the live
  exclusion primitive:
  <https://learn.microsoft.com/en-us/windows/win32/fileio/file-handles>
- .NET `FileStream.Flush(Boolean)` documentation: `Flush(true)` clears
  intermediate buffers to disk, which is the required durability boundary
  before setup may announce completion:
  <https://learn.microsoft.com/en-us/dotnet/api/system.io.filestream.flush>

The resulting contract is deliberately fail-closed: durable maintenance state
must be released and independently reread before setup publishes its completion
ledger or any success report.

## Implementation

The maintenance record is now `synapse_setup_maintenance_lock/v2` and each
acquisition receives a random GUID `lock_token`. Release:

1. reads the record through the still-owned stream;
2. requires `state=held`, the current PID and the exact token;
3. seeks to byte zero, truncates, writes the v2 released record and calls
   `Flush(true)`;
4. rereads the record and requires `state=released`, the same PID and token;
5. only then writes the completed phase ledger and emits success reporting.

The normal path throws the structured
`SYNAPSE_SETUP_MAINTENANCE_LOCK_RELEASE_FAILED` error on any failure. The trap
retains an explicitly best-effort release because it is already reporting a
different terminating failure, but the outer `finally` always disposes the exact
owned handle. There is no fallback state or inferred success.

## Compile and lint gates

- PowerShell parser: zero parse errors for `scripts/synapse-setup.ps1`.
- `cargo check --workspace`: passed.
- `pwsh -File scripts/lint.ps1`: passed the contract, toolchain, format and
  clippy gates in both the root and `calyx/` workspaces.
- The repository-mandated pre-push full gate also passed before the earlier
  reflex batch push; the setup-specific commit remained local until the real
  deployment proof below completed.

No automated test was used as acceptance. The following are real process, file,
installed-binary, HTTP/MCP and physical-vault readbacks.

## Manual boundary and edge-case audit

### 1. Invalid detached launcher argument shape — fail closed before mutation

Two initial detached launch attempts intentionally exercised argument-boundary
failure. `Start-Process` flattened the multi-token `AllowedPermissions` value;
the real setup received only `READ_EVENTS` while audio was enabled.

Before: the production daemon remained PID 552 and no new maintenance owner was
published. Trigger PIDs 12880 and 18424 both exited. After: stderr named
`SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID`, reported
`enable_audio=True read_audio_granted=False`, and gave the exact remediation.
The production daemon and its vault were unchanged. This is the expected
fail-closed boundary, not a successful #2207 trigger.

### 2. Real `-Stop` path — released state survives process exit

Before: a v2 held record named setup PID 16668 and the real production daemon
was running. Trigger: `synapse-setup.ps1 -Stop` against the installed service.
After independent reads: PID 16668 was absent, daemon count was zero, the
maintenance record was v2 `released`, its PID and token still matched that
acquisition, the installed sidecar was absent, and the lifecycle ledger recorded
a clean graceful shutdown. The released file SHA-256 began `D963…`.

### 3. Real `-Start` path — report tail cannot leave held state

Before: daemon count was zero and the preceding stop record was already
released. Trigger: `synapse-setup.ps1 -Start`. After independent reads: setup
PID 20212 was absent, the record was v2 `released` with the same PID/token as
its held state, and production daemon PID 552 answered authenticated health from
the same vault. The released file SHA-256 began `0FA7…`.

### 4. Full detached-parent build/install — original reported failure mode

The final trigger used .NET `ProcessStartInfo.ArgumentList` so the permissions
were passed as one exact argument. A deliberately short-lived parent PID 22764
spawned:

```text
pwsh -NoProfile -File C:\code\synapse\scripts\synapse-setup.ps1
  -SourceDir C:\code\synapse -ForceRestart -EnableAudio
  -AllowedPermissions "READ_EVENTS READ_REFLEX WRITE_REFLEX READ_PROFILE READ_STORAGE WRITE_STORAGE READ_AUDIO"
  -ActiveIssue 2207 -SkipClientWiring
```

The child was PID 12644. The parent exited while the child was still building.
The pre-trigger durable read was:

```json
{
  "schema": "synapse_setup_maintenance_lock/v2",
  "state": "held",
  "lock_token": "5c32f7b8-4473-417b-984f-f3274a1e2569",
  "pid": 12644,
  "parent_pid": 22764,
  "started_at_utc": "2026-08-10T14:31:43.3924922Z",
  "lineage": "12644:pwsh.exe"
}
```

While the parent was absent, the real release build accumulated CPU work,
peaked near 10 GiB working set, validated an isolated candidate, gracefully
drained PID 552, installed the binary, and cold-started PID 9104. The setup
owner remained held through authenticated production health verification.

After the trigger, separate process and file reads at
`2026-08-10T14:48:17.4874634Z` proved both PID 22764 and PID 12644 absent. The
same maintenance file physically contained:

```json
{
  "schema": "synapse_setup_maintenance_lock/v2",
  "state": "released",
  "lock_token": "5c32f7b8-4473-417b-984f-f3274a1e2569",
  "pid": 12644,
  "parent_pid": 22764,
  "released_at_utc": "2026-08-10T14:47:46.2507982Z"
}
```

Its exact SHA-256 was
`022B6C282551D27A8E367EE221E56DB963059D323B3F4811AE1C35144B4A8A8C`.
The separately opened phase ledger then read `outcome=completed`, PID 12644,
start `2026-08-10T14:31:42.6033166Z`, end
`2026-08-10T14:47:46.2565058Z`, and 963.653 seconds total. Its exact SHA-256 was
`B16BB67E636AD520AC5B1FF97A563C0A873E279D6E12B72F48365CD2B232F789`.
The release timestamp precedes the completion-ledger timestamp, which is the
ordering #2207 required. Setup staging-directory count was zero.

The same ledger inspection exposed an unrelated ranking defect in
`slowest_phases`; it is tracked independently as #2208 rather than hidden in
this fix.

## Installed production reality

The independently hashed installed executable was:

```text
C:\Users\hotra\.cargo\bin\synapse-mcp.exe
SHA-256 298CFA5817B237E85361CFDBC3406578A680CEE10C4F2462DDB3EF1CA27FBCFE
```

OS process reality showed PID 9104 executing that exact installed path with
audio enabled and permissions
`READ_EVENTS,READ_REFLEX,WRITE_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO`.
Authenticated `/health` independently reported:

- `ok=true`, build `0cf6e8348344`;
- full build commit `0cf6e8348344ec2404da6473b96ce5a7505b8ca8`;
- `refs/heads/main`, clean build tree, `build_matches_checkout=true`;
- release profile and zero changed build inputs;
- Calyx storage open at the production vault, maintenance workers live;
- previous daemon PID 552 shutdown `clean` / `graceful`, ending after
  `calyx_vault_close`;
- Windows execution-speed throttling disabled;
- audio enabled with the STT model physically available.

The real public MCP client then created session
`931600ed-0a8b-4e6a-aeb1-cc5fb7fdf60c`. `tools/call health` succeeded from PID
9104; its response SHA-256 was
`E44675D71F3B2AD7D69E5A6FA3347B2F5C04B300C15CA98E7B946E9E6E696ABE`.

Finally, public `audit operation=verify_chain` re-read and rehashed the physical
Calyx Source of Truth. Response SHA-256 was
`5D6DD2D66D9A30C44757250606DDBA5BA175DF02A93D3CB8B9C8C9D94B69D35A`.
It reported:

```text
verdict=intact
head_height=342935
entries=342935
raw_commitments=965135
raw_sealed=965132
raw_pending=3
raw_seals=39570
tip_hash=7d3eae94f0717591dfb9debd5e94fba80ec3896d4a581bd8bf4b51d8e80f0d17
vault_generation=1
vault_reset_count=0
chain_origin=lineage-seeded
```

This proves the detached deployment released its durable maintenance identity,
published completion only afterward, installed the exact tested build, started
the intended process configuration, preserved a clean daemon/vault transition,
and left the physical provenance chain intact.
