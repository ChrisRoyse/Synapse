# Issue #2216 — stable-rustc frontend scope and Chrome bridge extraction FSV

Date: 2026-08-17 CDT  
Repository: `C:\code\Synapse`  
Branch: `main` only  
Final behavior commit under verification: `fb078fcbebda866bbec9d4d0bd18dbb48384ce02`

This record is manual Full State Verification. No automated test, benchmark,
FSV driver, alternate target directory, worktree, branch, CI job, or mock was
used. `cargo check`, Clippy, format, and repository policy checks below are
structural evidence only.

## Root cause and structural correction

The original process evidence showed two different phases inside the same
stable `rustc` invocation:

- frontend/type/monomorphization work used about 0.98 effective CPU core;
- LLVM/codegen later used about 6–13 cores;
- Cargo scheduled independent dependency crates concurrently before the
  critical path collapsed to the large `synapse-mcp` package.

`release.codegen-units = 16` was therefore not broken. It parallelized the LLVM
backend, but could not parallelize stable rustc's frontend. Increasing it would
target the already-parallel phase and trade away runtime optimization. Nightly
`-Z threads` or `RUSTC_BOOTSTRAP` would be an unstable workaround.

The stable fix was to extract the cohesive normal-Chrome bridge and shared
bearer-token contract into `synapse-chrome-bridge`. `synapse-mcp` and
`synapse-chrome-native-host` now depend on that one package instead of compiling
the 9.5 KLOC bridge source independently. Repository policy Gate 0c was moved to
the new canonical source seam.

The final real-host replay found two additional integration defects before
closure:

1. The canonical setup script parsed under Windows PowerShell 5.1 but used
   PowerShell 6.2/.NET 5+ runtime surfaces (`Sort-Object -Stable`, static
   `SHA256.HashData`, `CryptographicOperations`, `ConvertFrom-Json -Depth`, and
   `Path.GetRelativePath`). Commit `c5901473` replaced all eight incompatible
   uses and made official PSScriptAnalyzer Windows PowerShell 5.1 compatibility
   analysis part of structural Gate 0d.
2. `/chrome-debugger/native/register` treated a missing HTTP `Origin` as a
   browser-extension fetch. A real native-messaging process has no HTTP Origin
   and uses the daemon bearer token, while the direct extension fetch carries
   the pinned extension Origin and a derived bootstrap header. The middleware
   sent every native host to the wrong credential verifier and returned
   `401 HTTP_TOKEN_INVALID`. Commit `fb078fcb` requires the exact extension
   Origin for that special browser lane; origin-less native-host HTTP now reaches
   ordinary bearer authentication. Registration also rejects every caller
   origin except the pinned Synapse extension identity.

## Primary-source research

- Cargo timing guidance identifies critical paths and recommends splitting a
  large bottleneck package at real ownership seams:
  <https://doc.rust-lang.org/cargo/reference/timings.html>
- Cargo documents `codegen-units` as a code-generation parallelism/runtime
  tradeoff, not frontend parallelism:
  <https://doc.rust-lang.org/cargo/reference/profiles.html#codegen-units>
- Rust's stable parallel-front-end explanation distinguishes Cargo process
  parallelism and LLVM CGUs from the under-parallelized frontend:
  <https://blog.rust-lang.org/2023/11/09/parallel-rustc/>
- Microsoft documents `Sort-Object -Stable` as PowerShell 6.2+ and the
  Windows PowerShell 5.1/.NET Framework versus PowerShell 7/.NET runtime split:
  <https://learn.microsoft.com/powershell/module/microsoft.powershell.utility/sort-object>
  and
  <https://learn.microsoft.com/powershell/scripting/whats-new/differences-from-windows-powershell>
- Microsoft documents static `HashData` as a .NET 5+ API and recommends the
  instance hashing pattern on earlier runtimes:
  <https://learn.microsoft.com/dotnet/fundamentals/code-analysis/quality-rules/ca1850>
- Microsoft recommends PSScriptAnalyzer compatibility syntax/command/type
  analysis for cross-edition scripts:
  <https://learn.microsoft.com/powershell/module/microsoft.powershell.core/about/about_powershell_editions>
- Chrome documents that native hosts are separate stdio processes and receive
  the caller extension origin as argv, while extension code has its own network
  security origin for `fetch`:
  <https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging>
  and
  <https://developer.chrome.com/docs/extensions/develop/concepts/network-requests>

Both Exa MCP and the native web research tool were used after the physical
diagnosis. The implementation follows the primary sources above.

## Build timing and artifact Sources of Truth

Earlier targeted-clean timing evidence retained by the issue:

- `target/cargo-timings/cargo-timing-20260816T111328343Z-b32ce8cb5f515534.html`
  SHA-256
  `F34B4AED82AB37296EACF490CB1EA158F475F71D82DE6840067A5A4F32E39712`:
  total 6m12s; extracted bridge 16.2s (7.2s frontend / 8.9s codegen),
  main binary 280.3s, native-host binary 2.0s.
- Representative MCP-only edit:
  `target/cargo-timings/cargo-timing-20260816T112012022Z-b32ce8cb5f515534.html`
  SHA-256
  `D5BA626DE63DD026F9BC47DD6D8A1672DA4E92760431185EC23FFCC240E30A70`:
  total 4m15s; main binary 254.6s; native host 1.4s; independently compiled
  bridge 0.0s/fresh.

Final canonical shipping trigger:

```text
pwsh -NoProfile -File scripts\synapse-setup.ps1 \
  -SourceDir C:\code\Synapse -ForceRestart
```

Separate post-trigger reads:

- phase ledger:
  `%LOCALAPPDATA%\synapse\logs\setup-phase-timings.json`, SHA-256
  `128902D80576BC33A20DFD3F85B6AAA70D28C8B4FE12344EEBE88FC70C6D0B94`;
  10 phases, build 286.702s, exact canonical target, 32 logical CPUs;
- build target ledger:
  `%LOCALAPPDATA%\synapse\logs\setup-build-target.json`, SHA-256
  `4516CD4A4944A0F734F17616E97ED2E52EBF9BB505E0B3A9B0A1F73994A1C821`;
- raw `synapse-mcp.exe` build artifact:
  `CF127799394768CDD42C45ECE0980D00866977D3AF0DBC6A8DC82687ECCC8D47`;
- installed model-bundled daemon:
  `B6450E11FB1E3F8FB6989D0D1267C8DDFAFB7D7EF8BC46337BE08FA97AB7AA8D`;
- target and installed `synapse-chrome-native-host.exe` both:
  `16CD766084A1BD0FCF155A3F3F06CB168A577A549D001B691ADAAB6D9D7794C1`.

Setup returned the expected, separate Chrome lifecycle refusal
`SYNAPSE_CHROME_BACKGROUND_RELOAD_HOST_UNAVAILABLE` without touching Chrome.
The checkpoint at
`%LOCALAPPDATA%\synapse\setup-chrome-bridge-pending.json` was SHA-256
`534331E5C1057A3AA3BF4BADBAD40F075D5D09B52988E927E7F9A52128815B9E`
and physically bound daemon PID 51316, exact installed binary/token/setup
script hashes, and state `pending`. That natural-lifecycle work remains tracked
by #2220/#2223/#2225 and is not a failed #2216 artifact handoff.

## Strict production MCP precondition

After handoff, the real wired `mcp__synapse` client performed schema-validated
tool discovery and tool calls:

- daemon/listener: PID 51316 at `127.0.0.1:7700`;
- health build: `fb078fcbebda`;
- `build_matches_checkout=true`, clean tree and zero changed build inputs;
- 40 sanitized tools;
- tool surface SHA-256
  `ecfc7ae5004a8e40b6df0987bd2dc18ecce160ad8cd86654f7287c974610bcfb`;
- calling session:
  `ef5a5d00-8357-4dd0-a9d0-f11ea2b8562f`;
- client: `codex-mcp-client` 0.147.0, protocol `2025-06-18`, binding source
  `client_tools_list`, `matches_live_tool_surface=true`.

This satisfies the strict-client precondition. A hand-written HTTP caller was
not substituted for the MCP client.

## Windows PowerShell 5.1 repair state verification

Source of Truth:
`%LOCALAPPDATA%\synapse\logs\setup-phase-timings.json`, the checkpoint above,
the setup-repair run directory, PID/socket state, and the strict setup tool.

The real strict `setup operation=repair` launched inbox Windows PowerShell 5.1
as PID 8264. It reached only the expected Chrome-host-unavailable boundary.
Separate readback found:

- no `Sort-Object -Stable`, `SHA256.HashData`, parser, or phase-ledger warning;
- a newly written 2,024-byte phase ledger, SHA-256
  `FD50B91257414BA2D5A232192FEBB85E7360CC4C573CF42812EEC95625887B5A`;
- schema v1, PID 8264, one measured resume phase (10.821s);
- checkpoint identity unchanged;
- child absent and daemon listener unchanged.

Three strict MCP rejection cases were read before and after and produced no
repair run, phase-ledger change, checkpoint change, or listener change:

1. normal profile repair -> `TOOL_PROFILE_POLICY_DENIED`;
2. conflicting `status` and `repair` blocks -> `TOOL_PARAMS_INVALID`;
3. empty repair reason -> `TOOL_PARAMS_INVALID`.

## Native-host manual FSV

Sources of Truth:

- installed and target executable bytes/hashes above;
- daemon in-memory Chrome bridge row exposed through strict health;
- durable daemon log
  `%LOCALAPPDATA%\synapse\logs\synapse.log.2026-08-17-06`;
- host telemetry
  `%LOCALAPPDATA%\synapse\issue-2216-native-host-fb078fcb\synapse.log.2026-08-17-06`;
- startup error
  `%APPDATA%\synapse\chrome-debugger\native-host-startup-error.log`;
- exact child PID/process table and PID 51316 listener.

Before the happy trigger, host count was 0, no native-host process existed,
and startup-error SHA-256 was
`A11B90D741926019C741B9B4BA3D77E81659E85AA10E37384E72B171C411948D`.
The installed executable was launched with the pinned Chrome origin and stdin
was closed to simulate port EOF.

After the trigger:

- child PID 50056 exited 0 and no longer existed;
- host telemetry SHA-256 was initially
  `16D0D7FC305F5F04CD2FA03C50407B0FE2AB5AB0F146D9A79475ED9AD50C34A8`;
- it contained registered/started/exited events for
  `chrome-native-50056-1786949103855`;
- daemon log lines 18047–18048 independently contained the
  `native_messaging` registration and `stdin EOF from Chrome native messaging
  port` disconnect;
- strict health independently changed host count `0 -> 1`, with no active host;
- the old startup error did not change.

Manual edge cases, each with printed before/after state:

1. **Empty argv:** PID 46064 exited 1; startup-error changed to SHA-256
   `723F9D49CB8C543EF1AA9B5EBAEEDC5FE57085C1926D03C4CD6281EE6CF16305`
   and exact code `SYNAPSE_CHROME_NATIVE_HOST_INVOCATION_INVALID`; no daemon or
   telemetry mutation.
2. **Forged extension origin:** PID 51184 exited 1; startup-error changed to
   SHA-256
   `62780513C06CA6116D0BA814FE4729DA426FACFF100677A945074B9177782A16`
   with HTTP 400 and exact expected/actual origin plus remediation; daemon host
   count remained 1.
3. **Invalid log level:** PID 29448 exited 1; startup-error changed to SHA-256
   `3564069CD95F00A8F2B6C8927C25FEDEC0271C01645A020637D8DBCDCD428607`
   and listed the accepted levels; no daemon or telemetry mutation.

The browser-extension credential lane was then triggered separately without
touching Chrome. A request carrying the exact extension Origin and derived
register header returned HTTP 200 and created
`chrome-native-0-1786949242088`; an authenticated disconnect returned HTTP 200.
Daemon log lines 18868–18869 independently persisted transport `direct_http`
and the disconnect detail. Strict health read host count 2 with no active host.
No bearer or bridge credential value was printed or stored in this evidence.

Human Chrome PID 18816 retained creation time
`2026-08-16T20:37:35.9638801-05:00` across every trigger. No Chrome process was
focused, activated, stopped, restarted, or otherwise mutated.

## Structural checks (not FSV)

- `cargo fmt --all --check` — passed;
- `cargo check -p synapse-chrome-bridge -p synapse-mcp` — passed;
- `cargo clippy -p synapse-chrome-bridge -p synapse-mcp --all-targets -- -D warnings`
  — passed;
- `pwsh -NoProfile -File scripts\lint.ps1 -PolicyOnly` — passed Gates 0,
  0b, 0c, and 0d; Gate 0 confirmed zero test/bench/FSV-driver surfaces;
- `git diff --check` — passed before each commit.

The physical artifact, process, strict-client, daemon-row, durable-log, repair
ledger, and boundary evidence jointly satisfy #2216.
