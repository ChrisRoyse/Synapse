# Issue #1975: rustc ThinLTO crash classification and recovery

Date: 2026-08-04

## Sources of truth

- `%LOCALAPPDATA%\synapse\logs\setup-build-failures`: immutable failed-build
  log/diagnostic pairs. The newest remains 2026-08-03 12:09:50Z; no post-fix
  toolchain crash was archived.
- `setup-build-invocation.json` and `setup-build.log`: the actual cargo child
  result and compiler output from the latest deployment.
- the installed executable, Windows process table, and live `/health` payload.

Research used Exa MCP v3.4.0 and built-in web. Rust's upstream issue #125765
records flaky Windows release aborts with `lto="thin"`; Rust 1.97.1's standard
library documentation states that `RUST_MIN_STACK` controls the default spawned
thread stack unless a builder overrides it.

## Real deployment

`scripts\synapse-setup.ps1` ran the shipping build with:

```text
RUST_MIN_STACK=67108864
retry budget=2 extra attempts / 3 maximum
attempt 1 of 3
memory before: commit 65.79%, available physical 18.26 GB
memory after : commit 66.15%, available physical 18.24 GB
```

The cargo job completed without retry:

```text
started  2026-08-04T06:12:13Z
finished 2026-08-04T06:29:31Z
exit_code=0 failure=""
```

Candidate validation and handoff installed a 185,672,809-byte executable. An
independent disk read found identical hashes in target and installed paths:

```text
0A3D3F110E85BFE8A84F8DBFD7409AC2EE58D45D48923BA81921AE3C3D0B22A1
```

The Windows process table independently reported PID 4352 executing
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`. Live health reported PID 4352,
`ok=true`, commit `2641b9c53ea9`, `build_matches_checkout=true`, and the same
185,672,809-byte installed image.

Setup intentionally returned nonzero only after the successful handoff because
the current Codex process held stale health/storage schemas. The structured code
was `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE`, not a build classification.

## Accumulated evidence and boundaries

Before the fix, four of seven relevant builds crashed rustc with
`STATUS_ACCESS_VIOLATION`. Post-fix durable evidence now includes the originally
recorded clean deployment, four distinct Aug 4 release binaries that reached
isolated candidate validation, and this deployment: at least six release
compiles with no new crash archive.

The shipped classifier was previously exercised against all four physical crash
logs and distinguishes real source diagnostics, empty logs, locked output, link
errors, tool crashes, and a tool crash accompanied by a source diagnostic. Only
a named OS tool crash with zero source diagnostics consumes the bounded retry
budget; every operator-repairable failure remains fatal on attempt one.
