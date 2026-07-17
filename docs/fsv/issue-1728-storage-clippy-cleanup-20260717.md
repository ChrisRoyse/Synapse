# Issue 1728 storage clippy cleanup FSV - 2026-07-17

## Research

- Exa and web research used official Clippy documentation:
  - https://rust-lang.github.io/rust-clippy/master/
  - https://doc.rust-lang.org/clippy/lint_configuration.html
- Applied conclusion: fix local clarity warnings where behavior stays obvious, and use narrow `allow(..., reason=...)` only for intentional structural cases such as one-to-one record-field builders and metric conversions that require `f64`.

## Changes

- `crates/synapse-storage/src/constellations.rs`
  - Added `# Errors` rustdoc to public constellation builders.
  - Added exact helper functions for intentional `u64 -> f64` conversions.
  - Replaced avoidable clones and option matches.
  - Made pure helpers `const`/`#[must_use]` where appropriate.
  - Added narrow, reasoned clippy allowances for intentionally long field-to-slot/metadata mapping functions.
- `crates/synapse-storage/examples/dump_cf.rs`
  - Fixed machine-applicable warnings.
  - Added narrow, reasoned clippy allowances for long CLI/readback functions whose output is intended to remain one complete physical readback.

## Source Of Truth

- Structural warning SoT: local `cargo clippy -p synapse-storage --all-targets` output.
- Real storage readback SoT:
  - `%LOCALAPPDATA%\synapse\db-daemon`
  - `cargo run -p synapse-storage --example dump_cf -- <db> <cf>`
  - live `mcp__synapse.storage(operation=summary)`
  - live daemon process table and `/health`

## Manual FSV

Before:

- Real DB path existed: `C:\Users\hotra\AppData\Local\synapse\db-daemon`.
- Expected counts from live MCP summary:
  - `CF_MODEL_CACHE=0`
  - `CF_PROFILES=2`
- Edited file hashes before readback:
  - `constellations.rs`: `36C060DFEE06729CFA95FAD05FF6D5AEE2CF9F1FDFE303CA70996A17FDA17C7C`
  - `dump_cf.rs`: `B2F937DCC38F926799AC0E756FD0ACE1DFDF82E0B402FE936D658F4A00881E57`

Happy path / non-empty CF:

- Trigger: `cargo run -p synapse-storage --example dump_cf -- %LOCALAPPDATA%\synapse\db-daemon CF_PROFILES`
- Expected: read-only output with 2 metadata-only rows.
- Actual:
  - `mode=read_only`
  - `row_count=2`
  - two rows printed with key/value lengths and SHA-256 hashes, raw material omitted.

Edge 1 / empty CF:

- Trigger: `cargo run -p synapse-storage --example dump_cf -- %LOCALAPPDATA%\synapse\db-daemon CF_MODEL_CACHE`
- Expected: read-only output with `row_count=0`.
- Actual:
  - `mode=read_only`
  - `row_count=0`

Edge 2 / invalid CF:

- Trigger: `cargo run -p synapse-storage --example dump_cf -- %LOCALAPPDATA%\synapse\db-daemon CF_NOT_REAL`
- Expected: fail closed with a schema error.
- Actual:
  - exit code `1`
  - `ReadFailed { cf_name: "CF_NOT_REAL", detail: "column family name is not part of the Synapse storage schema" }`

After:

- Live daemon process table still showed one installed daemon:
  - PID `62524`
  - path `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
  - bind/db command line for `127.0.0.1:7700` and `%LOCALAPPDATA%\synapse\db-daemon`
- Direct `/health` readback:
  - `ok=true`
  - `storage=ok`
  - `reflex=ok`
  - `chrome=ok`
- Live `mcp__synapse.storage(operation=summary)`:
  - `readback_source_of_truth=calyx summary cf_count=17 pressure=Normal`
  - `CF_MODEL_CACHE=0`
  - `CF_PROFILES=2`
  - `pressure=Normal`
  - `missing_cf_row_count_estimates=[]`
  - `missing_cf_size_estimates=[]`

## Structural Checks

- `cargo clippy -p synapse-storage --all-targets`: passed with no storage clippy lint warnings; only CUDA build-script discovery warnings remained.
