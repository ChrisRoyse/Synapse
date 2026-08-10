# FSV: process terminal capture and filtered history (#2194, #2195)

Date: 2026-08-09 America/Chicago (2026-08-10 UTC)

## Scope and diagnosis

- #2194: `process launch` durably recorded only a `process_start` row. The
  Windows launcher neither redirected output nor retained the exact process
  handle, and it closed `hProcess` immediately. No component could therefore
  observe an exit code, exit FILETIME, stdout, or stderr.
- #2195: `process history` asked storage for the last `limit` raw rows and only
  then applied PID/name/command filters. A desired older match disappeared as
  soon as an unrelated newer row occupied that raw tail.

The fix retains an exact Windows process handle for terminal observation,
drains separately redirected stdout/stderr pipes concurrently, publishes a
bounded captured prefix as a verified content-addressed file, and writes linked
start/terminal rows. Filtered history now scans before limiting and refuses an
unproven incomplete result at its explicit 10,000-row bound.

## Independent research

Research was performed after diagnosis and before implementation.

- Exa lane: `pwsh -File scripts/check-research-lane.ps1` returned `live`.
  A real `web_search_exa` call through `exa-search-server` 3.4.0 returned the
  Microsoft child-I/O, `CreateProcess`, process-exit, anonymous-pipe, and handle
  inheritance documentation.
- Built-in web lane, primary sources:
  - <https://learn.microsoft.com/en-us/windows/win32/procthread/creating-a-child-process-with-redirected-input-and-output>
  - <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw>
  - <https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject>
  - <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess>
  - <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes>
  - <https://doc.rust-lang.org/std/process/struct.Child.html>
  - <https://www.postgresql.org/docs/current/queries-limit.html>

The resulting design uses a `STARTUPINFOEX` handle allowlist containing only
stdin/stdout/stderr, concurrent pipe draining, exact-handle wait/readback,
PID-plus-creation-FILETIME identity, pending then final durable terminal state,
and filter-before-limit query semantics with an explicit completeness verdict.

## Source of truth

The launch return value was only the trigger receipt. Acceptance read these
independent states:

1. the Windows process table, both through `process operation=list` and
   `Win32_Process`/`Get-Process`;
2. physical `CF_PROCESS_HISTORY` rows, decoded by a separate
   `process operation=history` request;
3. exact files under
   `%LOCALAPPDATA%\Synapse\process-output\sha256\<prefix>\<sha256>.bin`,
   read independently and hashed with `Get-FileHash`;
4. Calyx exact-scan storage census, vault sequence/identity, lineage journal,
   daemon process/image hash, health, and structured daemon logs.

## Build, lint, deployment, and hardware

- `cargo check --workspace`: pass.
- `pwsh -File scripts/lint.ps1`: pass in both workspaces, including config and
  toolchain agreement, fmt, cargo-deny, Clippy with `-D warnings`, and the public
  Calyx API ratchet.
- Setup built with `CARGO_BUILD_JOBS=12` and
  `CMAKE_BUILD_PARALLEL_LEVEL=12`, matching the Intel i7-1355U's 12 logical
  processors.
- The host has 32 GB RAM and Intel Iris Xe graphics, with no NVIDIA PnP device,
  NVML, or `nvcc`. Setup therefore compiled CPU math intentionally. Live health
  selected AVX2 and its fixed dot/cosine/L2/top-k probe agreed bit-for-bit with
  the portable path. CUDA was not silently attempted or emulated.
- First validation install: PID 17704, image
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, SHA-256
  `9EFED662C15317B27BC8230D7DF4CD2A55303890DFF5B21B7B06EE6C2987EFE1`.
  Setup then completed green with the documented `-SkipBuild -ForceRestart
  -SkipClientWiring` pass. The implementation and this FSV record are committed
  together and the same source is rebuilt once from that clean commit before
  issue closure so build provenance is not left dirty.

## Manual happy path

Synthetic child:

- stdout: `SYN2194_STDOUT_5B8D` (19 bytes)
- stderr: `SYN2194_STDERR_5B8D` (19 bytes)
- exit: 0 after a 1.2 second delay

Before:

- exact marker history returned zero rows;
- no target `powershell.exe` generation existed for the not-yet-known launch
  identity.

Trigger and live state:

- launch ID
  `3212e51fcc363e57669bec760920b1934c20c25d23a1a562c18078a58817416b`;
- PID 14804, creation FILETIME 134308000241382352;
- MCP and `Win32_Process` independently saw PID 14804 alive with parent 17704
  and the exact command line.

After:

- `Win32_Process` returned no PID 14804;
- `CF_PROCESS_HISTORY` returned two linked rows: `process_start/running` and
  `process_terminal/exit_zero`, both with the same launch ID, PID, and creation
  FILETIME; the terminal row had exit code 0;
- stdout file SHA-256
  `4b19aa69d42ebdfa61c245d675569cee968dc2d63e4cb8cd8b87efb5e46ab841`
  contained exactly `SYN2194_STDOUT_5B8D`;
- stderr file SHA-256
  `4303ac11a6c399cfd203494fc1fae5e1a2afb198a5ccd447811a0b06c2828e6d`
  contained exactly `SYN2194_STDERR_5B8D`;
- both files were 19 bytes, matched the history hashes, and had
  `state=complete`, `artifact_verified=true`, `truncated=false`.

## Boundary and edge-case audit

Each case printed state before and after its real trigger.

### 1. Nonzero exit is not success

- Before: exact dynamic marker had zero history rows.
- Trigger: PID 8676 emitted `NZ_OUT`/`NZ_ERR` and exited 23.
- After OS: PID absent.
- After CF: `status=exit_nonzero`, `exit_code=23`.
- Independent disk reads found the exact six-byte strings with hashes
  `6b06c825...556fa` and `b152e2bd...dee59`; both matched the observed and
  captured history hashes.

### 2. Maximum capture boundary and truncation

- An 80-byte stdout stream and 96-byte stderr stream were captured with a
  64-byte limit.
- CF recorded observed lengths 80/96, captured lengths 64/64, and
  `truncated=true` for both.
- Whole-observed hashes independently matched
  `d9b1f3e2...d7a60` and `170e6572...86412`.
- The two files were exactly the expected 64 `A` and 64 `B` bytes with
  captured hashes `d53eda7a...081f6` and `c422e707...d3ae8`.
- `max_bytes_per_stream=16777216` (the documented maximum) was separately
  accepted and durably captured `MAXBOUND` as eight bytes.

### 3. Empty streams and concurrent same-hash publication

- Before: the dynamic marker had zero history rows.
- Trigger: a process produced no stdout or stderr and exited 0.
- After: both stream records were complete, zero bytes, untruncated, and
  independently verified against the same physical empty artifact:
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
- This exercises the simultaneous stdout/stderr publication race without a
  mock or an artificial publisher call.

### 4. Invalid limits and invalid format fail before mutation

For capture limits 0 and 16,777,217 and for mode `pretend_success`:

- before state: zero matching history rows, zero matching native target rows,
  eight artifact files totaling 186 bytes;
- each request failed with `TOOL_PARAMS_INVALID`; numeric cases named
  `launch_output_capture_limit_invalid` and the allowed 1..=16,777,216 range;
- after state: zero matching history rows, zero matching native target rows,
  and the same eight files/186 bytes. `state_unchanged=true` in all cases.

### 5. Explicitly non-terminal fire-and-forget

- Omitted output returned `mode=fire_and_forget` and
  `terminal_observation=not_requested`, with no terminal history key.
- PID 23548 was independently live under daemon PID 17704 and later absent.
- History held exactly one `process_start/status=started` row and made no exit
  claim despite the synthetic child exiting 7.
- Artifact state remained eight files/186 bytes before and after.

### 6. Filter-before-limit chronology (#2195)

- Before: dynamic A marker returned zero history rows.
- A: PID 7976 / launch ID `1c3f2a9e...f6f4` completed and had a terminal row.
- B: unrelated PID 16472 / launch ID `ef7201dd...edb5` completed afterward.
- Unfiltered `limit=1` returned B's terminal row, proving B occupied the raw
  newest tail position.
- Filtered A `limit=1` scanned 17 CF rows and returned A's earlier terminal row
  with `returned_count=1`, `complete=true`, `cf_exhausted=true`.
- The explicit assertion that raw PID was B while filtered PID was A and row
  kind was `process_terminal` printed `proof=true`.

### 7. Daemon shutdown while capture is pending

- Before: marker returned zero history rows.
- Trigger: PID 6972 wrote `PRE_SHUT_OUT` and `PRE_SHUT_ERR`, then began a
  120-second sleep. `Win32_Process` proved it alive under old daemon PID 17704;
  history contained only `process_start/status=running`.
- A real `synapse-setup.ps1 -SkipBuild -ForceRestart -SkipClientWiring` drain
  saw six active sessions and completed green.
- After OS: old daemon 17704 and child 6972 were absent; new daemon PID 13680
  ran the same verified image.
- After CF: the exact terminal key held
  `status=daemon_shutdown_forced`,
  `termination_cause=daemon_shutdown_forced`, and the original PID, creation
  FILETIME, and launch ID.
- Independent files contained exactly the pre-shutdown 12-byte strings with
  SHA-256 `f62feb16...4335` and `c9039d04...5671`; `POST_SHUT_OUT` was absent.
- The combined OS/CF/file assertion printed `proof=true`.

## Final physical readback and fault audit

- Health: `ok=true`, 40 tools, surface hash
  `98a1cbcb999c1cc0a70bf99277fe33aac1e0fb8b42e6c7667e7540958b441d30`.
- Calyx vault ID: `01KYJPGWATPD4XNMZY3ERGTKQW`.
- Health latest sequence: 1,304,930; last recovered sequence: 1,304,900.
- Exact storage summary after restart: `CF_PROCESS_HISTORY=19`, 47,122 logical
  bytes; metrics mode `calyx_exact_scan_sizes_counts`; pressure `Normal`.
- Lineage journal remained generation 1 with the same vault ID and high-water
  sequence 1,304,900.
- Artifact directory: 12 files, 220 bytes, zero `.tmp` files.
- Process leak sweep: no `powershell.exe` command containing an FSV marker.
- Structured terminal logs existed for happy, nonzero, truncated, empty,
  maximum-boundary, A/B chronology, and forced-shutdown launches.
- Error-log search over the full deployment/FSV interval returned zero
  error-level events. No handle-close, output-publication, terminal-persist, or
  panic-safety fault was present.

Verdict: PASS. The real OS process generations, physical Calyx rows, content
files, vault lineage, and logs agree. No return value is used as sole evidence.
