# Issue #2184 FSV: storage build-script watch set

Date: 2026-08-09  
Host toolchain: Rust/Cargo 1.97.1, Windows, 12 logical processors

## Root cause

`crates/synapse-storage/build.rs` emitted `cargo:rerun-if-changed` for the D1-deleted `tests/` and `benches/` paths. Cargo therefore considered the package dirty on every invocation. It also emitted a directory watch and another watch for every file it recursively scanned.

The defect was reproduced before editing with an unchanged `cargo check -p synapse-mcp -vv`: 32.01 seconds, with Cargo's exact reason `Dirty synapse-storage ... the file crates\synapse-storage\tests is missing`. A second unchanged call rebuilt the same cascade.

## Research and repair

The Exa MCP lane was live and used, and the built-in web lane independently read the Cargo Book. Cargo documents that a directory `rerun-if-changed` scans the whole directory, that a path which never exists can keep a build script dirty, and that Cargo automatically recompiles/reruns a changed `build.rs`: <https://doc.rust-lang.org/cargo/reference/build-scripts.html>.

The build script now:

- scans/watches only extant production inputs: `Cargo.toml`, `src/`, and `examples/`;
- scans its own `build.rs` for the policy token without emitting the redundant self-watch;
- emits one watch for each directory rather than one per descendant;
- continues to panic with the exact offending path on read failures or a forbidden codec token.

## Sources of truth

1. Cargo's `-vv` dirty/fresh classification and wall-clock time.
2. The physical build-script output file under `target\debug\build\synapse-storage-*\output`.
3. Exact source SHA-256 before/after the manual policy probe.

The physical output file contained exactly:

```text
cargo:rerun-if-changed=C:\code\synapse\crates\synapse-storage\Cargo.toml
cargo:rerun-if-changed=C:\code\synapse\crates\synapse-storage\src
cargo:rerun-if-changed=C:\code\synapse\crates\synapse-storage\examples
```

Independent filesystem reads proved `tests/` and `benches/` absent and `examples/` present. No individual-file watch was emitted.

## Execute and inspect

| Trigger | Storage state | MCP state | Wall time |
|---|---|---|---:|
| first check after the build-script edit | `Dirty` because `build.rs` changed; compiled | downstream rebuilt | 31.18 s |
| first unchanged check | `Fresh synapse-storage` | `Fresh synapse-mcp` | 0.95 s |
| second unchanged check | `Fresh synapse-storage` | `Fresh synapse-mcp` | 0.93 s |
| unchanged check after probe restoration rebuild | `Fresh synapse-storage` | `Fresh synapse-mcp` | 0.59 s |

## Boundary and edge audit

1. Missing retired surfaces: before and after, `tests_exists=false` and `benches_exists=false`; the physical emitted watch set contains neither, so absence is stable rather than a perpetual invalidation.
2. Extant optional surface: `examples_exists=true` and the physical output contains its one directory watch, preserving production policy coverage without descendant duplication.
3. Invalid source format/policy: `src/lib.rs` began at SHA-256 `79DF2C84BFDA24CE0232B389E6C557F6F6DA91BB76E5D25F8A814BCF87968BBA`. A manual exact forbidden-token comment changed it to `D150C812424440DA8B1A19FAAADDB876B3DDE4355E4BDC3E84FAC99C55C5238C`; the real check exited 101 in 0.56 s and named `forbidden binary storage codec token found in ...\src\lib.rs`. Removing the probe restored the exact original SHA-256, zero probe matches, and no diff in that file.

## Verdict

PASS. Cargo's independently reported state proves the edit loop is incremental again, while the real fail-closed policy scanner still rejects forbidden content in an actual production source surface.
