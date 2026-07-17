# Calyx Retention Parity (#1657)

## Root Cause

Synapse's Calyx backend stores all 17 Synapse column families through a direct
`ColumnFamily::Kv` envelope in `crates/synapse-storage/src/backend.rs`. That
route intentionally preserves the existing `Db` byte API, but it bypassed
`calyx-aster::layers::KvLayer`, so `synapse-core::retention::DEFAULTS` never
reached the Calyx value header.

Before #1657, every Calyx write encoded `expires_at_ms = 0`, and every Calyx
read rejected a non-zero expiry header with an error that said TTL parity still
belonged to #1657. The structural invariant was therefore false: Calyx rows
could not physically represent Synapse retention policy.

## Design

The Synapse-owned Calyx KV envelope is now:

```text
version(0x02) || expires_at_ms(u64 be) || written_at_ms(u64 be) || payload
```

Legacy `0x01` rows remain readable as:

```text
version(0x01) || expires_at_ms(u64 be) || payload
```

Legacy rows have `written_at_ms = 0`, so byte-cap enforcement treats them as the
oldest rows if they ever share a collection with v2 rows.

Write behavior:

- TTL defaults (`Hours`, `Days`) write `expires_at_ms = vault_clock + ttl`.
- `None` and `LruOnly` write `expires_at_ms = 0`.
- Every user row writes `written_at_ms = vault_clock`.
- TTL arithmetic and byte-cap arithmetic use checked math and return structured
  `StorageError::WriteFailed` on overflow or invalid defaults.
- A write batch preflight-scans each affected Calyx namespace after input
  encoding succeeds and before the commit is accepted, so a pre-existing
  malformed retention envelope fails the operation without partially committing
  the new rows.

Read behavior:

- Point reads and scans decode the envelope with the same vault clock.
- Expired rows are immediately invisible to Synapse callers.
- Malformed or unsupported envelopes fail closed with structured storage logs.

Byte-cap behavior:

- After a Calyx write batch, the backend scans only the affected CF namespace.
- Expired rows are tombstoned opportunistically.
- Live bytes use the same logical measurement as `cf_sizes`: `user_key.len() +
  payload.len()`.
- If a CF is over its soft cap, live rows are tombstoned oldest-first by
  `(written_at_ms, user_key)`.
- `CF_ROUTINE_STATE` is protected from automatic deletion because it stores
  operator-owned routine lifecycle decisions.
- `cache_evictions_total{reason="soft_cap"}` is incremented for cap evictions,
  and hard-cap reaches are logged with `STORAGE_CF_HARD_CAP_REACHED`.

## Research Inputs

- Redis documents the production pattern of passive expiry on access plus active
  cleanup for keys that may never be touched again:
  https://redis.io/docs/latest/commands/expire/
- LSM-style TTL implementations delete expired values during compaction and can
  return stale physical entries before maintenance runs.
- Caffeine separates size-based eviction from time-based expiration and performs
  expiration maintenance during writes and occasional reads:
  https://github.com/ben-manes/caffeine/wiki/Eviction

## Boundary With #1659

#1657 makes retention policy physically representable and enforceable on Calyx
write/read paths. The public `run_gc_once` / periodic GC task, report parity, and
maintenance-surface `GcReport` fields remain #1659.
