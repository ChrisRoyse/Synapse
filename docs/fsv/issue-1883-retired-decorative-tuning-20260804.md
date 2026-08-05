# Issue #1883: decorative tuning removal FSV (2026-08-04)

## Diagnosis and decision

The original nine health knobs split into real algorithm parameters and
decorative configuration. Earlier work made guard FAR and fusion `k`
load-bearing and proved their behavior. Persisted index parameters, fusion slot
weights, and per-slot quantization are now also load-bearing.

Seven fields still had no honest global owner:

- `bit_floor_bits` and `correlation_ceiling` duplicated frozen panel-admission
  policy in `synapse-storage`;
- `guard_cold_start_tau` described a Ward builder Synapse does not call. Synapse
  requires a persisted calibrated guard profile and fails provisional without
  one;
- kernel recall is already an explicit `min_recall_ratio` on the real kernel
  rebuild request; the global `kernel_recall_gate` duplicated that owner;
- `kernel_fraction` and both `temporal_boost_*` values had no consumer.

All seven were removed from config, lowering, and health. Because the TOML
shape uses `#[serde(deny_unknown_fields)]`, stale keys fail startup parsing with
their exact names and remediation. They are not silently ignored.

## Research

Research followed diagnosis through both lanes. `scripts/check-research-lane.ps1`
proved Exa MCP v3.4.0 live with a real `tools/call`. Built-in web research used
primary sources:

- Serde documents `deny_unknown_fields` as the container-level mechanism for
  rejecting keys outside the declared schema:
  <https://serde.rs/attributes.html>
- Cargo's compatibility guidance classifies removal of public surface as a
  breaking change and recommends explicit deprecation/removal planning:
  <https://doc.rust-lang.org/stable/cargo/reference/semver.html>

Synapse/Calyx is pre-1.0 and the removed fields never changed runtime behavior.
Failing with a precise migration error preserves less false compatibility than
continuing to accept and echo values that the engine ignores.

## Source of truth and execution

Manual driver:
`crates/synapse-calyx/examples/retired_tuning_config_fsv.rs`.

Physical scratch state:
`%TEMP%\synapse-fsv-1883-d587103d8269479bb130175d268fc5dd\calyx.toml`.

The driver writes and independently hashes the real TOML file before and after
each parser trigger.

Happy path retained load-bearing configuration:

```text
[calyx]
fusion_k = 5
sha256 before=6163619162c06397d9934d2ecaf488a688deb141195e33bfbd7e9f6eff5d7f47
sha256 after =6163619162c06397d9934d2ecaf488a688deb141195e33bfbd7e9f6eff5d7f47
observed fusion_k=5
```

## Boundary audit

Each obsolete key was written individually. Every parse returned
`SYNAPSE_CALYX_CONFIG_PARSE_FAILED`, named the unknown key, listed the accepted
load-bearing fields, and preserved the file SHA-256 exactly:

```text
bit_floor_bits       6ab89ee7...c697bf
correlation_ceiling  d9881e2c...b2255
guard_cold_start_tau 6c443ec5...46980b
kernel_fraction      5f364909...84fccd
kernel_recall_gate   895b0655...f51b96
temporal_boost_min   21e6651d...a64d89
temporal_boost_max   a6d6013b...5f69b6
```

The final accepted tuning schema contains only fields with a named production
consumer. Health therefore cannot report an inert configured value.

## Installed-system verification

The first official deployment attempt failed closed and rolled back because the
production Anneal live pointer referenced an old tuning JSON artifact containing
the retired fields. The candidate log named the exact artifact and field; the
installed binary remained the prior SHA-256 after rollback.

Commit `23fba6f9` added a narrow durable migration at the Anneal artifact
boundary. It removes only the seven documented retired keys from a hash-verified
legacy JSON object, strictly deserializes every remaining field, validates the
tuning, writes canonical new content-addressed bytes, swaps the native
AnnealRollback live pointer, and reads it back. TOML config parsing remains
strict and does not use this migration.

The second `synapse-setup.ps1` transaction succeeded. Independent post-setup
readback:

```text
installed exe = C:\Users\hotra\.cargo\bin\synapse-mcp.exe
exe sha256    = E3C15810E0D12C88108A986AEE923E3217A4735236524EDA912EDD87E855D39D
process pid   = 22324
listener pid  = 22324 (127.0.0.1:7700)
/health ok    = true
calyx_vault   = ok
inert knobs   = 0
```

Authenticated `/health` contained 11 tuning rows and every row declared
`enforcement=load_bearing`. The response had no `calyx_bit_floor_bits` or
`calyx_correlation_ceiling` field.

The production structured log independently recorded the durable pointer
transition:

```text
code=SYNAPSE_CALYX_ANNEAL_TUNING_SCHEMA_MIGRATED
prior=8ed7f65833059660a0f15ebcf2afea1701bbd42bd9d63db3a1b5842230872a78
live =f27f4058998e32f90ba18253d0de8e51c506575309dd1c449c480e4a4b5a3634
```

The next log row reopened the migrated pointer and reported the retained
load-bearing values `fusion_k=60` and `index_ef_search=64`.
