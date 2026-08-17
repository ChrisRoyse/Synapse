# Build Speed And Disk Hygiene

This repo is tuned for fast iterative builds on the configured Windows dev host
and to prevent git worktree plus Cargo `target/` buildup from consuming the
system disk again.

## Build Settings

| Change | Where | Effect |
| --- | --- | --- |
| `rust-lld` linker | `.cargo/config.toml` | Replaces slow MSVC `link.exe` for faster Windows linking. Missing linker state fails loudly. |
| Dev incremental compilation | `Cargo.toml [profile.dev]` | Rebuilds only changed codegen units during the edit loop. |
| Dependency debuginfo off | `Cargo.toml [profile.dev.package."*"]` | Avoids large dependency debug artifacts while keeping workspace panic line tables. |
| `jobs = 32` | user Cargo config | Uses the configured host's logical cores for local builds. |
| Cohesive crate seams | workspace crates | Lets Cargo schedule independent stable-rustc front ends and prevents unrelated main-crate edits from recompiling large subsystems. The Chrome bridge and native-host runtime live in `synapse-chrome-bridge`; both daemon binaries consume that one compiled artifact. |

Use the fast local edit loop:

```powershell
cargo check
cargo build
```

Use `cargo build --release` only when shipping or running the optimized daemon,
not as compile feedback during edits.

The local structural gate also requires Microsoft's `PSScriptAnalyzer` 1.25.0
or newer. Gate 0d analyzes the complete shipping setup script against the
Windows PowerShell 5.1 Desktop compatibility profile, in addition to parsing
the exact ASCII bytes under both Windows PowerShell 5.1 and PowerShell 7. A
missing analyzer or any incompatible command parameter, syntax, or .NET API is
a hard failure:

```powershell
Install-Module PSScriptAnalyzer -RequiredVersion 1.25.0 -Scope CurrentUser
pwsh -NoProfile -File .\scripts\lint.ps1 -PolicyOnly
```

Do not suppress compatibility findings. `scripts\synapse-setup.ps1` is
deliberately launched by the MCP setup facade through the inbox Windows
PowerShell 5.1 executable, so PowerShell 7-only APIs are production failures.

`codegen-units = 16` parallelizes LLVM backend work; it does not make stable
rustc's front end parallel. If a release timing report shows one large crate
occupying the critical path, first locate a real high-cohesion ownership seam
and extract that subsystem into one shared workspace crate. Do not duplicate the
source with `#[path]`, create a second target directory, enable unstable rustc
flags, or raise codegen units as a substitute for an architectural seam. Use
`cargo build --release --timings` only for an explicit build investigation and
read the generated timing artifact plus the built binary as the Sources of
Truth.

## CUDA Build Environment

The absorbed Calyx workspace has optional CUDA feature builds. On Windows with
CUDA 13.x, `nvcc` must be able to find MSVC `cl.exe`, and CUDA dependency
kernels need MSVC's conforming preprocessor. `scripts\synapse-setup.ps1`
repairs this configured-host state when CUDA is installed:

- `NVCC_CCBIN` -> Visual Studio `VC\Tools\MSVC\...\bin\Hostx64\x64`
- `NVCC_APPEND_FLAGS` includes `-Xcompiler=/Zc:preprocessor`

The CUDA compile check is:

```powershell
cargo check --manifest-path calyx\Cargo.toml --workspace --features "calyx-assay/cuda calyx-loom/cuda calyx-registry/cuda calyx-search/cuda calyx-sextant/cuda"
```

## Defender Exclusions

Defender real-time scanning of Cargo output can slow local builds. The helper
self-elevates and adds the repo build-output exclusions:

```powershell
pwsh -File .\scripts\add-defender-exclusions.ps1
```

## Chrome Policy Popup Shield

Synapse tries to write a reversible Chrome `ExtensionSettings` popup shield at
`HKCU:\Software\Policies\Google\Chrome\ExtensionSettings`. That policy blocks
debugger/nativeMessaging permissions for hazards that can surface Chrome popups
during background automation.

The intended ACL on a hardened configured host is admin-only for writes. It is
valid for the key owner to be `BUILTIN\Administrators` or `SYSTEM`, with the
medium-integrity user token limited to `ReadKey`. Do not weaken that ACL to make
normal setup write Chrome managed policy.

The policy shield is defense-in-depth, not the runtime enforcement boundary.
Runtime safety is the installed Synapse Chrome Bridge `chrome.management`
suppression readback plus daemon fail-closed command gates.

To apply the policy shield, use an elevated PowerShell from the repo:

```powershell
pwsh -File .\scripts\synapse-setup.ps1 -SourceDir C:\code\Synapse -ForceRestart
```

Then verify `mcp__synapse.health` reports
`synapse_chrome_self_policy_shield_present=true`. Without elevation, setup may
warn with `SYNAPSE_CHROME_POLICY_POPUP_SHIELD_WRITE_DENIED_NONBLOCKING`; the
required readback is that `chrome_bridge.status=ok`, live management suppression
has `remaining_hazard_count=0` and `failure_count=0`, and browser commands fail
closed if that suppression changes.

## Repo Maintenance

`scripts/repo-maintenance.ps1` is dry-run by default. It scans git repos under
`-Root` (default `C:\code`) and:

- prunes merged or remote-gone worktrees without touching active, dirty, or
  unmerged work;
- deletes local branches whose remote branch is gone;
- runs `cargo sweep` for stale build artifacts.

```powershell
pwsh -File .\scripts\repo-maintenance.ps1
pwsh -File .\scripts\repo-maintenance.ps1 -Apply
```

`scripts/install-maintenance-task.ps1` registers or removes the weekly
non-elevated Scheduled Task:

```powershell
pwsh -File .\scripts\install-maintenance-task.ps1
pwsh -File .\scripts\install-maintenance-task.ps1 -Remove
Get-ScheduledTask -TaskName SynapseRepoMaintenance
```

## Line Endings

`.gitattributes` is the repository Source of Truth for line endings. It pins
source, docs, manifests, PowerShell, and POSIX hook scripts to LF in the working
tree even when a Windows checkout has `core.autocrlf=true`. Windows-only batch
and solution formats are explicit CRLF exceptions. Binary assets are marked
`binary` so Git never normalizes their bytes.

When adding a new generated artifact or binary format, add an explicit
`.gitattributes` rule in the same change. When adding a new text format, either
let the repo default apply or add an explicit `text eol=lf` rule if the format is
important enough to audit directly.

Manual readback commands for the policy:

```powershell
git -c core.autocrlf=true diff --check
git check-attr text eol -- .githooks/pre-push scripts/synapse-setup.ps1 Cargo.toml README.md tests/fixtures/audio/hello_world_5s.wav
git ls-files --eol .githooks/pre-push scripts/synapse-setup.ps1 Cargo.toml README.md tests/fixtures/audio/hello_world_5s.wav
```

If an attribute change intentionally normalizes tracked text, stage it with:

```powershell
git add --renormalize .
```

## Root Cause

Parallel issue work created many throwaway git worktrees, each with its own
multi-GB Cargo `target/`. Git does not automatically remove worktrees, and Cargo
does not garbage-collect old `target/` artifacts. Scheduled worktree pruning plus
`cargo sweep` keeps the checkout set and build artifacts bounded.

## MCP Helper Process Hygiene

Some external stdio MCP servers are launched as helper process trees under the
client that requested them. For the configured Exa launcher, the expected live
tree is:

```text
codex.exe or claude.exe
  -> cmd.exe ... C:\Users\hotra\.codex\bin\exa-mcp-server.cmd
      -> node.exe ... exa-mcp-server\smithery\stdio\index.cjs
```

Classify these helpers from the process table before cleanup:

- Live transport: wrapper `cmd.exe` parent is a live `codex.exe` or
  `claude.exe`, and the Exa `node.exe` parent is that wrapper PID. Leave it
  running; it belongs to an active MCP client.
- Owned probe: wrapper PID was spawned and recorded by the current operation.
  Close the MCP client first, then verify the wrapper and child PIDs are gone.
- Orphaned helper: wrapper/node command lines exactly match the Exa launcher,
  the parent client PID is absent, and no active client owns the tree. Cleanup
  may target only those exact helper PIDs after before/after process readback.

Never kill broad `cmd.exe`, terminal, IDE, WSL, Codex, or Claude process sets to
clean MCP helpers. If ownership cannot be proven, print the process Source of
Truth and leave the process running.

## Never Measure Latency While A Build Is Running

This is a single-machine project, and the agent's own compiler is the heaviest
process on the box. Any latency number collected while `cargo build`,
`cargo check`, `cargo clippy` or `scripts/lint.ps1` is running is measuring the
compiler, not the daemon.

The host is a 2 P-core + 8 E-core i7-1355U. On a hybrid part under load the
scheduler can park a thread on an E-core, or off-core entirely, for a long time.
The effect is not marginal:

- A `read_latest` — a point read of **one key** — was measured holding the
  vault-wide row-table read guard for **743 ms** (#1955). It cannot do 743 ms of
  work. It was descheduled.
- The same scan, same code, same 200,000 rows, measured quiet and loaded:
  `held_us=39,278 cpu_us=31,250` versus `held_us=284,392 cpu_us=125,000` — a
  284 ms hold of which 159 ms was not running.
- Worst commit stall moved **5.6x** and guard-event rate **14x** on one
  unchanged binary, purely from build load (#1950).

That is larger than most changes being measured, and it moves in the direction
that flatters a change: a loaded "before" against a quiet "after" attributes the
machine's idle CPU to the diff. #1952 ask 3 was abandoned for exactly this
reason — there was no clean pre-deploy baseline and no way to recover one.

### What to do instead

1. **Batch every code change, then deploy once, then measure.** A deploy after
   touching `synapse-calyx` is a full release rebuild; interleaving builds with
   measurement windows costs more than it saves.
2. **Read the self-identifying fields rather than trusting the wall clock.**
   `CALYX_ASTER_ROW_READ_GUARD_SLOW` carries `cpu_us` and `starved`, and
   `health` carries `calyx_row_guard_starved_total`. A window with a non-zero
   starved count measured the machine; discard it rather than reasoning about
   it. This is why those fields exist — a contaminated sample now says so
   instead of relying on a reader noticing that a point read took 743 ms.
3. **Distinguish "descheduled" from "blocked".** A thread waiting on a lock
   consumes no CPU by definition, so a `cpu_us` near zero across a *wait* means
   nothing and a starvation flag derived from it would be true unconditionally.
   Only a region where a thread is supposed to be *running* — a scan holding a
   guard, or a commit's locked region — can be checked this way.

### Related standing rules

- Never measure latency within **2 minutes of a daemon restart**; the vault is
  still recovering and the numbers describe recovery, not steady state.
- Prefer a **frozen vault copy** for any before/after that compares *values*
  rather than structure. The live vault is a moving corpus, so a before/after
  taken minutes apart measures drift plus the change with no way to separate
  them. Structural counts are the exception and stay valid live.
