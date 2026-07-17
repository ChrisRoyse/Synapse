# Issue #1718 FSV: Lightweight Syn* Lenses for `synapse-storage`

Date: 2026-07-17

## Root Cause

`synapse-storage` imported `calyx-registry` only for deterministic Syn* algorithmic lenses and two small measurement helpers. That made the storage crate's normal dependency graph include the registry runtime surface and its model/embedding transitive dependencies (`candle`, `fastembed`, `ort`, `tokenizers`, `hf-hub`) even though timeline/episode/agent storage constellation writes do not need those runtimes.

This was a dependency-boundary bug: storage needed a stable algorithmic measurement contract, not the full registry runtime package.

## Research

Best-practice checks used:

- Exa MCP search: `Rust Cargo official documentation optional dependencies features best practices avoid unnecessary transitive dependencies split crates`
- Web research: Cargo Book feature and resolver documentation.

Relevant Cargo guidance:

- Cargo features and optional dependencies are additive and feature-unified across dependency users. Cargo recommends inspecting resolved features with `cargo tree`, and for incompatible or separable functionality, splitting functionality into separate packages. Source: <https://doc.rust-lang.org/cargo/reference/features.html>
- Cargo's resolver unifies dependency features, and workspace builds unify features for selected workspace members. This makes relying on feature gates alone brittle when a lightweight consumer shares a heavy package with other consumers. Source: <https://doc.rust-lang.org/cargo/reference/resolver.html>

Chosen fix: split the deterministic Syn* algorithmic surface into a new lightweight Calyx crate instead of adding another feature combination to `calyx-registry`. This keeps the storage dependency boundary explicit and prevents future feature unification from pulling model runtimes into the storage hot path.

## Code Change

- Added `calyx/crates/calyx-lenses`.
- Moved the Syn* deterministic `AlgorithmicLens`, Syn* encoders, and `measure::{absent,input_hash}` helper surface into that crate.
- Preserved the frozen lens ID algorithm:
  - `algorithmic-runtime-v2`
  - `algorithmic-data-oblivious`
  - identical encoder debug text and dimensions
  - identical shape/norm fingerprinting
- Switched `crates/synapse-storage` from `calyx-registry` to `calyx-lenses`.
- No tests, benches, or FSV harnesses were added.

## Source Of Truth

Dependency SoT:

- `crates/synapse-storage/Cargo.toml`
- `Cargo.lock`
- `calyx/Cargo.lock`
- `cargo tree -p synapse-storage -e normal`

Runtime storage SoT:

- Native Calyx vault Base/Slot/Scalars rows under `C:\Users\hotra\AppData\Local\synapse\manual-fsv\issue-1718-calyx-lenses-20260717-v1`
- Read separately with `cargo run -p synapse-storage --example dump_cf -- --native-source --reveal-metadata ...`

MCP/daemon SoT:

- `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-run-current.json`
- `Get-Process synapse-mcp`
- `Get-NetTCPConnection -LocalPort 7700 -State Listen`
- Installed binary hash at `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`

## Dependency Verification

Before the fix, `cargo tree -p synapse-storage -e normal` showed:

```text
calyx-registry
ort
tokenizers
candle-core
candle-nn
candle-transformers
fastembed
hf-hub
```

After the fix:

```text
├── calyx-lenses v0.1.0 (C:\code\Synapse\calyx\crates\calyx-lenses)
```

Targeted query:

```powershell
cargo tree -p synapse-storage -e normal |
  Select-String -Pattern 'calyx-registry|calyx-lenses|candle|fastembed|\bort\b|tokenizers|hf-hub'
```

Result: only `calyx-lenses` appeared.

## Lens Contract Parity

One-off manual probe under ignored `target/manual/issue-1718-probe` compared old `calyx-registry::AlgorithmicLens` to new `calyx_lenses::AlgorithmicLens`. This probe was not committed and was not the verdict; it only produced deterministic inputs for manual inspection.

All seven timeline Syn* lenses had matching IDs, shapes, and serialized outputs:

```text
timeline.kind_onehot old_id=42d38c6af13142b61f0cbb29477ad149 new_id=42d38c6af13142b61f0cbb29477ad149 id_equal=true shape_equal=true output_equal=true
timeline.app_hash old_id=4c004c07c93636ee80bf0f02207c2826 new_id=4c004c07c93636ee80bf0f02207c2826 id_equal=true shape_equal=true output_equal=true
timeline.title_sparse old_id=bdaa8f6b098c26f8632b522ccc89e0d2 new_id=bdaa8f6b098c26f8632b522ccc89e0d2 id_equal=true shape_equal=true output_equal=true
timeline.hour_cyclic old_id=fe8ce58fd2c87e0a8910340e3fceab86 new_id=fe8ce58fd2c87e0a8910340e3fceab86 id_equal=true shape_equal=true output_equal=true
timeline.dow_cyclic old_id=4c3e81c8c7130c5f13f347278fe5f9fe new_id=4c3e81c8c7130c5f13f347278fe5f9fe id_equal=true shape_equal=true output_equal=true
timeline.actor_onehot old_id=52b299018ac9c16976b2d7aaa2b0f359 new_id=52b299018ac9c16976b2d7aaa2b0f359 id_equal=true shape_equal=true output_equal=true
timeline.event_time_rank old_id=0f71abb7d90176fd0aeb2591a6f10438 new_id=0f71abb7d90176fd0aeb2591a6f10438 id_equal=true shape_equal=true output_equal=true
```

## Manual FSV

### Happy Path

Input:

- source key: `issue-1718-happy`
- source key hex: `69737375652d313731382d6861707079`
- `TimelineKind::BrowserNav`
- app: `Issue1718App`
- title: `Issue 1718 deterministic title payload`
- timestamp: `1700000000123456789`

Before:

```text
db_exists=False source_cf=CF_TIMELINE source_key_hex=69737375652d313731382d6861707079 expected_match_count=0
```

Trigger:

```text
write case=happy outcome=ok source_key_hex=69737375652d313731382d6861707079 raw_sha256=4af5118482c334b150ba21bb59671c309d8c2cc93311aa929976652728acced6 cx_id=2e2e89c5cd6e6c8f2dcc24d27dfa22fc disposition=inserted inserted=true deduped=false slot_count=7 scalar_count=3 latest_seq=2
```

After readback:

```text
source_match index=0 cx_id=2e2e89c5cd6e6c8f2dcc24d27dfa22fc panel_version=1664001 slot_count=7 scalar_count=3
base present=true input_pointer=synapse://CF_TIMELINE/69737375652d313731382d6861707079 redacted=false slot_count=7 scalar_count=3 metadata_count=14
base_scalar name=raw_len_bytes value=248
base_scalar name=record_version value=1
base_scalar name=ts_unix_ms value=1700000000123
metadata key=synapse_raw_sha256 value=4af5118482c334b150ba21bb59671c309d8c2cc93311aa929976652728acced6
metadata key=timeline_app value=Issue1718App
metadata key=timeline_kind value=browser_nav
metadata key=timeline_title_excerpt value=Issue 1718 deterministic title payload
slot slot_id=1 present=true shape=Dense { dim: 32 }
slot slot_id=2 present=true shape=Sparse { dim: 1024, entry_count: 1 }
slot slot_id=3 present=true shape=Sparse { dim: 2048, entry_count: 5 }
slot slot_id=4 present=true shape=Dense { dim: 2 }
slot slot_id=5 present=true shape=Dense { dim: 2 }
slot slot_id=6 present=true shape=Dense { dim: 8 }
slot slot_id=7 present=true shape=Dense { dim: 1 }
scalar_cf_rows_for_cx=3
```

### Edge Case 1: Empty Optional Text/App

Input:

- source key: `issue-1718-empty`
- source key hex: `69737375652d313731382d656d707479`
- blank app, empty title, timestamp `0`

Before:

```text
native_source_result base_rows_scanned=1 match_count=0
```

Trigger:

```text
write case=empty outcome=ok source_key_hex=69737375652d313731382d656d707479 raw_sha256=0cbbcb8ec707ede000f56798f69886ff08abffe427329fc4d15881a6dde63c5f cx_id=eda3360b5512c78b2f991b52242ffbaa disposition=inserted inserted=true deduped=false slot_count=7 scalar_count=3 latest_seq=3
```

After readback:

```text
source_match index=0 cx_id=eda3360b5512c78b2f991b52242ffbaa panel_version=1664001 slot_count=7 scalar_count=3
base_scalar name=ts_unix_ms value=0
metadata key=timeline_actor value=human
metadata key=timeline_kind value=title_change
slot slot_id=2 present=true shape=Absent
slot slot_id=3 present=true shape=Sparse { dim: 2048, entry_count: 0 }
scalar_cf_rows_for_cx=3
```

Expected outcome met: optional app becomes explicit absent slot, empty title remains a valid sparse vector with zero entries, and no fallback or mock value is inserted.

### Edge Case 2: Maximum Inclusive Recency Rank

Input:

- source key: `issue-1718-boundary`
- source key hex: `69737375652d313731382d626f756e64617279`
- timestamp: `4102444800000000000`
- expected scalar ms: `4102444800000`

Before:

```text
native_source_result base_rows_scanned=2 match_count=0
```

Trigger:

```text
write case=boundary outcome=ok source_key_hex=69737375652d313731382d626f756e64617279 raw_sha256=e989807b2cc24d09a87b5ddc2399d54a492203addc3cc14c04252f47d9b0d704 cx_id=ba66b30fdeb00de56eedab5e68d06610 disposition=inserted inserted=true deduped=false slot_count=7 scalar_count=3 latest_seq=4
```

After readback:

```text
source_match index=0 cx_id=ba66b30fdeb00de56eedab5e68d06610 panel_version=1664001 slot_count=7 scalar_count=3
base_scalar name=ts_unix_ms value=4102444800000
metadata key=synapse_ts_ns value=4102444800000000000
metadata key=timeline_actor value=agent
metadata key=timeline_agent_session_id value=issue-1718-agent-session
slot slot_id=7 present=true shape=Dense { dim: 1 }
scalar_cf_rows_for_cx=3
```

Expected outcome met: the upper bound is accepted and physically persisted.

### Edge Case 3: Invalid Timestamp Beyond Frozen Rank

Input:

- source key: `issue-1718-invalid`
- source key hex: `69737375652d313731382d696e76616c6964`
- timestamp: `(4102444800000 + 1) ms`

Before:

```text
native_source_result base_rows_scanned=3 match_count=0
```

Trigger:

```text
write case=invalid outcome=expected_error source_key_hex=69737375652d313731382d696e76616c6964 detail=storage write failed in calyx_constellation: Calyx Syn* lens measurement failed: syn-timeline-v1: 0f71abb7d90176fd0aeb2591a6f10438: CALYX_LENS_NUMERICAL_INVARIANT: syn scalar rank input 4102444800001 outside frozen range [0, 4102444800000]
```

After readback:

```text
native_source_result base_rows_scanned=3 match_count=0
```

Expected outcome met: the storage path fails loudly with a structured Calyx numerical invariant error and no native Base row is written.

## Installed Daemon Readback

The repo-built release daemon was installed and started, but the setup command intentionally returned non-zero because this live Codex process was started with an older tool schema. This is a current-process restart precondition, not a daemon health failure.

Installed daemon evidence:

```text
installed sha256=94D1766A086B1ABA46F0EE4562F99D8837FC0F13D5C7EC9413839CF36EB9B269
daemon pid=64200
bind=127.0.0.1:7700
daemon-run-current db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
```

`setup operation=status` readback:

```text
pid=64200
bind=127.0.0.1:7700
token_env_present=true
codex_mcp_config_mentions_synapse=true
codex_mcp_config_mentions_bearer_env=true
```

The attempted real MCP replay mutation failed closed before writing:

```text
tool replay requires permission WRITE_REPLAY
code=SAFETY_PERMISSION_DENIED
```

Profile readback confirmed the active session grants are read-only for storage:

```text
effective_grant_names=["READ_EVENTS","READ_REFLEX","READ_PROFILE","READ_STORAGE"]
config_source=fail-closed read-only default
```

No hidden/debug MCP tool was used to bypass this permission boundary.

## Structural Checks

Structural checks passed:

```text
cargo fmt --all --check
cargo fmt --manifest-path calyx\Cargo.toml --all --check
git diff --check
cargo check -p synapse-storage
cargo check --manifest-path calyx\Cargo.toml -p calyx-lenses
cargo check --workspace
cargo check --manifest-path calyx\Cargo.toml --workspace
cargo clippy --workspace --all-targets
cargo clippy --manifest-path calyx\Cargo.toml --workspace --all-targets
```

These are compile/lint checks only. Manual FSV evidence is the separate dependency graph and Calyx Base/Slot/Scalars readback above.

## Outcome

#1718 is fixed:

- `synapse-storage` no longer depends on `calyx-registry`.
- The heavy model/runtime dependency stack is absent from the `synapse-storage` normal dependency tree.
- Syn* frozen lens IDs and outputs match the previous registry implementation for the timeline contract.
- Real native Calyx Base/Slot/Scalars rows persist correctly for happy path and edge cases.
- Invalid out-of-range input fails loudly and leaves no persisted native row.
