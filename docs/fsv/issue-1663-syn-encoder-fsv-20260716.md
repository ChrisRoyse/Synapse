# Issue #1663 Syn* Encoder Family FSV

Date: 2026-07-16
Agent: Codex

## Root Cause

`calyx-registry` only exposed the generic PH17 algorithmic encoders plus GDELT-specific lenses. Structured Synapse fields from #1694 had no first-class, frozen `Syn*` encoder family, so routine structured values had to be forced through generic scalar/sparse paths or custom downstream code. That left these gaps:

- no stable content-addressed encoder identity for Synapse cyclic, scalar transform, categorical, record, rate, interruption, or cross-feature lenses;
- persisted `LensSpec` contract derivation and runtime reload could not recognize Syn encoder kinds;
- invalid numeric inputs such as NaN/Inf and invalid hash dimensions had no Syn-specific fail-closed boundary.

## Research Used

Best-practice references checked before implementation:

- scikit-learn `FeatureHasher`: stateless feature hashing, signed Murmurhash behavior, and finite numeric value requirements. Source: https://scikit-learn.org/stable/modules/generated/sklearn.feature_extraction.FeatureHasher.html
- Apache Spark `FeatureHasher`: feature hashing for mixed categorical/numeric features and power-of-two feature-count guidance. Source: https://spark.apache.org/docs/latest/ml-features.html#featurehasher
- scikit-learn `StandardScaler`: frozen mean/std z-score semantics. Source: https://scikit-learn.org/stable/modules/generated/sklearn.preprocessing.StandardScaler.html
- RFC 8785 JSON Canonicalization Scheme: deterministic object member ordering for JSON-derived hashes. Source: https://www.rfc-editor.org/rfc/rfc8785

## Implementation

Changed files:

- `calyx/crates/calyx-registry/src/runtime/algorithmic.rs`
- `calyx/crates/calyx-registry/src/runtime/algorithmic/syn.rs`
- `calyx/crates/calyx-registry/src/commission/algorithmic_manifest.rs`
- `calyx/crates/calyx-registry/src/persistence_contracts/static_contract.rs`
- `calyx/crates/calyx-registry/src/persistence_contracts/runtime.rs`

Added first-class `AlgorithmicEncoder::Syn*` variants and constructors for:

- cyclic time
- scalar raw/log1p/zscore/rank
- one-hot, signed hash, sparse text, token slots, multi-hot
- record vector
- binning, ordinal, frequency, target mean
- delta, rate
- cross features
- aggregation

Fail-closed behavior:

- numeric parsers reject empty, invalid UTF-8, non-numeric, NaN, and Inf inputs;
- hash-like sparse dimensions and token dimensions must be positive powers of two;
- rank and bin transforms reject values outside their frozen range;
- token slots reject empty token streams instead of fabricating a placeholder token;
- record vector and aggregation reject structured input with no numeric fields;
- manifest commissioning validates known Syn output shapes and power-of-two dimensions;
- persisted `LensSpec` contract derivation and runtime reload share the live `AlgorithmicLens` contract path.

## Structural Checks

No automated tests were created or run. Per D1, these are compile/lint checks only, not FSV:

- `cargo fmt --manifest-path calyx\Cargo.toml --all --check`
- `cargo check --manifest-path calyx\Cargo.toml -p calyx-registry`
- `cargo clippy --manifest-path calyx\Cargo.toml -p calyx-registry --all-targets`
- `cargo check --manifest-path calyx\Cargo.toml --workspace`
- `cargo clippy --manifest-path calyx\Cargo.toml --workspace --all-targets`

All completed successfully.

## Manual FSV

MCP precondition:

- Real Synapse MCP daemon health read through the configured MCP client.
- Daemon PID: `64508`
- Bind: `127.0.0.1:7700`
- Health: `ok`
- Tool surface count: `40`
- Tool surface SHA-256: `f762a7df57aac03adc41d80a8cb8c8ab72496f142491b60c55f39957b5f8a069`
- Required trigger tool present and called: `mcp__synapse.shell`

Source of Truth:

- Vault manifest/current pointer: `C:\Users\hotra\AppData\Local\synapse\manual-fsv\issue-1663-20260716-161205\vault\CURRENT`
- Persisted registry snapshot: `C:\Users\hotra\AppData\Local\synapse\manual-fsv\issue-1663-20260716-161205\vault\registry\registry-b594a27e017f3966.json`
- Measurement evidence: `C:\Users\hotra\AppData\Local\synapse\manual-fsv\issue-1663-20260716-161205\measurement-evidence.json`

Trigger:

- Real MCP tool call: `mcp__synapse.shell operation=start`
- Job id: `019f6cc7-1ca6-79f2-853f-39734847dfa5`
- Command: `cargo run --manifest-path C:\Users\hotra\AppData\Local\synapse\manual-fsv\issue-1663-20260716-161205\Cargo.toml`
- Exit code: `0`

Separate physical readback:

- `CURRENT`: `manifest-00000000000000000002.json`
- Registry path: `...\vault\registry\registry-b594a27e017f3966.json`
- Registry file length: `54203`
- Registry SHA-256: `857CA4D15CD0D8F4EBA34BAA0C561656F663EF301585B59EE72F94540485FFAE`
- Evidence SHA-256: `60801841CD8880471E6E569FEDF35F10BB6D64672A625FD16B7C40989F894FEA`
- Persisted registry rows: `19`
- Unique determinism values: `probe_verified`

Persisted registry row names:

- `syn_cross_fsv`
- `syn_scalar_rank_fsv`
- `syn_scalar_zscore_fsv`
- `syn_delta_fsv`
- `syn_record_vector_fsv`
- `syn_target_mean_fsv`
- `syn_rate_fsv`
- `syn_sparse_text_fsv`
- `syn_aggregation_fsv`
- `syn_scalar_log1p_fsv`
- `syn_bin_fsv`
- `syn_one_hot_fsv`
- `syn_multi_hot_fsv`
- `syn_token_slots_fsv`
- `syn_scalar_raw_fsv`
- `syn_hash_fsv`
- `syn_frequency_fsv`
- `syn_ordinal_fsv`
- `syn_cyclic_time_fsv`

Happy-path evidence:

- `syn_scalar_zscore_fsv`, input `5`, frozen mean `1`, std `2`: actual dense `[2.0]`.
- `syn_cyclic_time_fsv`, input `6`, period `24`: actual dense `[1.0, 6.123234262925839e-17]`, expected `[1.0, 0.0]` within tolerance.
- `syn_bin_fsv`, input max boundary `10`, 5 buckets over `[0,10]`: actual dense `[0.0, 0.0, 0.0, 0.0, 1.0]`.
- `syn_frequency_fsv`, frozen count/total `25/100`: actual dense `[0.25]`.
- `syn_record_vector_fsv`, input `{"a":3,"b":4}`: actual dense dim `4`, finite, unit-norm vector `[-0.800000011920929, 0.0, -0.6000000238418579, 0.0]`.
- `syn_hash_fsv`, input `category-a`: actual sparse dim `16`, one signed entry `{ idx: 3, val: 1.0 }`.

Edge-case evidence:

- Empty input: `syn_sparse_text_fsv`, input empty string. Before registry hash `b594a27e017f3966a0f00fa182202f10739427f19adbfbe8f152e7f76c4dddc2`; after same hash; actual sparse dim `16`, entries `[]`.
- Boundary input: `syn_bin_fsv`, input `10` at the frozen maximum. Before/after registry hash unchanged; last bucket set exactly.
- Structurally invalid numeric input: `syn_scalar_raw_fsv`, input `NaN`. Error `CALYX_LENS_NUMERICAL_INVARIANT`, message `syn scalar raw input is NaN or Inf`; before/after registry hash unchanged.
- Invalid format/config boundary: scratch registration of `syn_hash:3`. Error `CALYX_LENS_NUMERICAL_INVARIANT`, message `syn hash dim must be a power of two`; persisted vault registry hash unchanged.

Verdict: #1663 is manually FSV-accepted against the physical persisted vault registry snapshot and measurement evidence files.
