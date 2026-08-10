# Issues #2199, #2200, #2202, and #2203 FSV — fail-closed reflex registration

Date: 2026-08-10 America/Chicago

## Defects and root causes

### #2199 — invalid audit timestamps silently became the Unix epoch

Reflex audit paths used `timestamp_nanos_opt().unwrap_or_default()`. Chrono
returns `None` for a `DateTime` outside the signed nanosecond range; the default
for the stored unsigned integer is zero. A conversion failure therefore became
a syntactically valid 1970 timestamp and entered the ordered audit keyspace.

### #2200 — registration exposed two independent publication boundaries

`ReflexRuntime::register` replaced the live scheduler and mutated the in-memory
definition list before it persisted the registration audit and Calyx
constellation. A subsequent storage, constellation, anchor, ledger, or flush
failure returned `Err` even though the new reflex could already tick. Trying to
compensate after activation could not retract actions already dispatched.

### #2202 — the new atomic ledger payload was rejected before publication

The first real happy-path run of #2200 failed closed with
`CALYX_LEDGER_SECRET_IN_PAYLOAD`. The digest fields were initially suspected,
then ruled out by reading Calyx's exact allowlist: `_sha256` fields explicitly
admit 64-character hex digests. The actual rejected token was the 42-character
unclassified schema string `synapse_reflex_registration_publication/v1`; the
ledger's secret-like no-whitespace threshold is 40 characters.

### #2203 — publication evidence mislabeled the complete row count

The final diff review found `source_row_count: physical_rows.len()` in the
ledger payload. The value included the complete publication batch, not only the
audit source CF, so the label was false even though the count was accurate.

## Independent research after diagnosis

The repository research probe reported Exa MCP live for both research passes:
`exa-search-server` v3.4.0 initialized, advertised `web_search_exa` and
`web_fetch_exa`, and completed real queries. The built-in web lane was exercised
independently. Primary sources read:

- AWS's transactional-outbox guidance classifies a database change plus a
  separately published event as a dual write and requires one transaction so a
  rolled-back operation cannot publish an event:
  <https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html>.
- Microsoft's transaction definition requires all operations to succeed or all
  effects to roll back:
  <https://learn.microsoft.com/en-us/windows/win32/ktm/what-is-a-transaction>.
- Chrono documents that `timestamp_nanos_opt` returns `None` when the value is
  outside its representable nanosecond range:
  <https://docs.rs/chrono/latest/chrono/struct.DateTime.html>.
- Rust documents explicit `Option` conversion through `ok_or`/`ok_or_else`,
  rather than substituting a valid default:
  <https://doc.rust-lang.org/stable/std/option/index.html>.
- OWASP says sensitive log values should be removed, masked, sanitized, hashed,
  or encrypted; the secret detector should not be weakened:
  <https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html>.
- OpenTelemetry requires a stable, low-cardinality event name that uniquely
  identifies the event structure and recommends namespaced naming:
  <https://opentelemetry.io/docs/specs/semconv/general/events/> and
  <https://opentelemetry.io/docs/specs/semconv/general/naming/>.

Applied rules: invalid time is a named error or an observable dropped-audit
health failure, never epoch; durable registration state has one Calyx
transaction; the candidate scheduler stays start-gated until that transaction
commits; and the ledger schema is the stable, secret-safe
`synapse.reflex.registration.v1`. There is no fallback, retry loop, secret-check
bypass, or compensating delete.

## Implementation

- Added checked `audit_timestamp::unix_ns`; cold callers receive
  `REFLEX_AUDIT_TIMESTAMP_INVALID`. Hot/offload callers increment a process-wide
  health counter and emit a structured error before omitting the invalid audit.
- One captured `DateTime<Utc>` now supplies both the status lifecycle timestamp
  and the registration audit nanoseconds.
- Added revision-guarded, atomic Calyx grounded-observation publication. One
  WAL/MVCC commit contains the source audit, global-order pointer, per-reflex
  state, registry, native constellation, active grounding anchor, provenance
  ledger entry, and raw commitment.
- The storage boundary separately reads every requested physical source row and
  the exact anchor after commit.
- Replacement schedulers are constructed behind an explicit atomic start gate.
  A pre-commit failure stops and joins the parked candidate; the old scheduler
  and runtime definition remain authoritative. A successful commit stops the old
  generation and then opens the candidate gate.
- The registration path no longer performs a second post-commit `Db::flush`
  that could falsely report the committed operation as failed.
- Final evidence-schema review found that the complete atomic batch count was
  initially called `source_row_count`. It is now precisely labeled
  `publication_row_count`; the counted rows and transaction are unchanged
  (#2203).

## Sources of Truth

1. Runtime: the scheduler's independently read status vector.
2. Logical durable state: real Calyx `CF_REFLEX_AUDIT`,
   `CF_REFLEX_AUDIT_ORDER`, per-reflex `CF_KV` projection rows, and registry.
3. Grounded physical state: Calyx constellation/anchor rows, provenance ledger,
   raw commitments, WAL/MVCC sequence, vault identity, and lineage journal.
4. Disk: the actual vault files and SHA-256 hashes, read after the process closed.

The manual verifier was a temporary binary calling production crate APIs against
real `Db::open` vaults. It contained no mock backend or test harness and was
deleted before lint.

## Manual boundary and edge-case audit

### Edge 1 — out-of-range time must not create an epoch row (#2199)

Scratch vault: `synapse-manual-fsv-2197-2199-20260810-0925`.

```text
before: CF_REFLEX_AUDIT=0, CF_REFLEX_AUDIT_ORDER=0
trigger: DateTime::<Utc>::MIN_UTC
cold result: REFLEX_AUDIT_TIMESTAMP_INVALID
hot result: no audit id; invalid timestamp health count incremented
after:  CF_REFLEX_AUDIT=0, CF_REFLEX_AUDIT_ORDER=0
```

A valid synthetic timestamp `1786350000123456789` then produced exactly one
source row and one ordered row. A second process reopened vault
`01KZNFJS9J24JW6G4A5XWB2WJH` and read the same audit id, status, and exact
timestamp. The durable high-water sequence was 20; the directory held 61 files
and 94,017 bytes. The scratch vault and sibling lineage journal were then
deleted and separately confirmed absent.

### Edge 2 — secret-like schema failure remains atomic (#2202)

The first #2200 vault began with source/order/runtime counts `0/0/0`. Triggering
a valid registration with the original 42-character schema returned:

```text
REFLEX_REGISTRATION_TRANSACTION_ROLLED_BACK
  scheduler_prepared=true
  scheduler_activated=false
  audit_committed=false
  CALYX_LEDGER_SECRET_IN_PAYLOAD
```

An immediate independent scan remained `source=0 order=0 runtime=0`. The fix
changed only the static discriminator to `synapse.reflex.registration.v1`; it did
not alter the ledger detector. The next real registration published source/order
`0/0 -> 1/1`, an active runtime status, a constellation, and an independently
read `reflex_registration_state=active` anchor.

### Edge 3 — physical revision conflict publishes nothing (#2200)

Authoritative vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-manual-fsv-2200-20260810-atomic3`,
vault id `01KZNJWB7X95CPY0F215NZAJ9D`.

Initial state was empty. The seed registration changed source/order/runtime
`0/0/0 -> 1/1/1` and used the same exact timestamp in both state domains:

```text
id = fsv-2200-seed-v2
status registered_at = 2026-08-10T10:18:20.353076100Z
audit ts_ns = 1786357100353076100
anchor cx_id = 6a99b75958c1ec0533755f4bdd2e5f6b
anchor = label:reflex_registration_state / active
source value SHA-256 = e91d792f3e6987916f634ed82f766eba60b0f1f2c185e191e04da9c200d81f50
```

The invalid trigger made 25 real Calyx writes to the exact registry row. Every
write used byte-distinct trailing JSON whitespace but decoded to the identical
registry object. This changed only the physical revision, the smallest data
needed to force the production compare-and-swap boundary. Registration of
`fsv-2200-conflict-v2` returned:

```text
CALYX_ASTER_GROUNDED_OBSERVATION_REVISION_CONFLICT
scheduler_prepared=true scheduler_activated=false audit_committed=false
prepared_scheduler_stopped=true
```

Immediate separate reads proved before and after were identical in meaning:

```text
source rows: 1 -> 1
order rows:  1 -> 1
registry:    [fsv-2200-seed-v2] -> [fsv-2200-seed-v2]
runtime:     [fsv-2200-seed-v2 active] -> same exact status/timestamp
failed id in source/order/registry/runtime/anchors: absent
```

### Happy path after the failed transaction

Without recreating the runtime, registering `fsv-2200-final-v2` succeeded:

```text
source rows: 1 -> 2
order rows:  1 -> 2
registry: [fsv-2200-final-v2, fsv-2200-seed-v2]
runtime: both active; seed timestamp unchanged
final status registered_at = 2026-08-10T10:18:20.499366100Z
final audit ts_ns = 1786357100499366100
final anchor cx_id = 5574dbcaa3980c64ae284a9c8468853d
final source value SHA-256 = 7e5e152b44c04f64bfa50131cba95c07affb3b59c10ace035ed03d8f0d898a9a
```

A new OS process reopened the vault and independently read source/order `2/2`,
the exact two registry ids, both source hashes, both timestamps, two
constellations, and two active anchors. Full physical verification reported:

```text
restore_success=true restore_chain_intact=true
ledger_intact=true ledger_verdict=intact
raw_commitments_intact=true covers_full_history=true
chain_origin=vault-genesis lineage_present=true
constellation_count=2 anchor_count=2 ledger_entry_count=6
ledger tip=1a996741e63b273901e393c05bb475cba4d098604d2f0fe09d79ddd4431bad19
failure_reasons=[]
```

Ledger entries 3 and 4 were independently decoded as `kind=grounding`, actor
`synapse-reflex-registration`, with subjects matching the two anchor CxIds;
every one of entries 0 through 5 reported `self_verifies=true`.

## Physical disk evidence

After all handles closed, the authoritative vault held 63 files and 128,836
bytes. SHA-256 associations:

```text
vault-identity.json = 73DC4D4882BBC41E9203A3563C0B9458DA36A1280F812A5E9D288A3738AD5933
CURRENT             = 463377015298C7902D62E5634EFEC6F17A3EBD1C274398299487822B0A988E98
lineage journal     = A6E4F73ED0D76C38D6C659A4BADE0CBB87C07E6BB1F11B2C7CA544563F0370E4
```

## Compile and lint

The temporary verifier was absent before both commands:

```text
cargo check --workspace -> PASS (28.81s after final evidence-schema edit)
pwsh -File scripts/lint.ps1 -> PASS (153.6s after final evidence-schema edit)
  zero-test / zero-harness doctrine -> PASS (1,360 Rust files)
  root + calyx format -> PASS
  root + calyx cargo-deny -> PASS
  root + calyx clippy --all-targets -D warnings -> PASS
  public Calyx API ratchet -> PASS (357 baseline, 357 current)
```

No automated tests, mock data, branch, worktree, fallback, or permanent FSV
surface was created.

## Production installed-daemon FSV

The supported setup path built and installed commit
`8819ee37e383a6c76078e4182559bb5088695f83` from a clean `main` checkout. The
shipping image was `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, length
257,312,073 bytes, SHA-256
`AB1585F3F45ED598A522A7813F243338BFB1D1A9588FF10E8F9870F467A8AF65`.
Authenticated health independently reported the same commit, image path,
length, clean build-input manifest, and exact source directory. The daemon used
the host's AVX2 implementation; NVML proved this host has no CUDA device, so
there was no faster GPU backend to select.

Temporary mutation grant:

```text
READ_EVENTS READ_REFLEX WRITE_REFLEX READ_PROFILE
READ_STORAGE WRITE_STORAGE READ_AUDIO
```

Before the trigger, production physical counts were
`CF_REFLEX_AUDIT=8, CF_REFLEX_AUDIT_ORDER=8`. Two inert, zero-action
`on_event` reflexes were registered through the public facade:

```text
id = 019feb4a-f6ed-7ce3-93e8-d3486c089ff9
priority = 931, exclusive = false
registered_at = 2026-08-10T10:49:46.477348Z
audit ts_ns = 1786358986477348000
audit id = 019feb4a-f6ee-7f41-be2a-148c696fe76d
source value bytes/SHA-256 = 780 / ac7cdcc92c541ef3e12d8cc0207918371242ee0be6d091ab44a14a06bf214b61
anchor CxID = c88af6b898e9d09263363bec99143503

id = 019feb4b-a287-75b2-9279-31be70e29690
priority = 932, exclusive = true
registered_at = 2026-08-10T10:50:30.407708900Z
audit ts_ns = 1786359030407708900
audit id = 019feb4b-a288-7ce3-ba32-4e9555bd53ff
source value bytes/SHA-256 = 779 / c7fb567074c44b05b180a64202253b19836d71b2c750c5fd32f36e592fd86750
anchor CxID = bf3d27c5a511d4ea14c84156fc712dd6
```

After the first registration, an independent list/history read and exact
physical anchor decode returned the same first timestamp and an active
`label:reflex_registration_state` anchor. After the second registration, the
first timestamp remained byte-for-byte unchanged and both independently read
histories held exactly one active event. Physical source/order counts were
`10/10`. The exact registered schema and `publication_row_count` field were
found in two physical Ledger SST files; `source_row_count` had zero matches in
the Ledger CF.

A whole-vault production verification then read the live SST/WAL bytes and
reported:

```text
vault id = 01KYJPGWATPD4XNMZY3ERGTKQW
verdict = verified, green = true
restore_success = true
scan_mode = full_chain
Ledger entries re-hashed = 342,260
chain_intact = true
raw_commitments_intact = true
tail_entries = 0
failure_reasons = []
tip = e598dde287dd00d5876ea6b8b5d687413226e20a389b7192f47923defdb1c6e5
```

### Real process boundary and newly isolated defect #2204

Setup changed only the declared permission contract and gracefully replaced
PID 4276 with PID 19024. Calyx reopened the same vault id at recovered sequence
1,306,320. Both exact audit histories and timestamps survived. However, the new
runtime reported `active_count=0` and omitted both ids while their durable audit
and anchor states remained active. Cancelling either id returned
`cancelled=false, reason=not_found` and did not invent terminal rows.

That contradiction is outside the fixed transactional-registration boundary
and is tracked fail-closed as #2204. The evidence rows were deliberately not
rewritten or deleted: doing so would hide the fact that restart recovery cannot
reconstruct an executable definition from the partial durable metadata.

### Final least privilege, denial boundary, and hardware readback

The daemon was left with exactly:

```text
READ_EVENTS READ_REFLEX READ_PROFILE READ_STORAGE WRITE_STORAGE READ_AUDIO
```

A third valid inert registration trigger then failed as
`SAFETY_PERMISSION_DENIED missing WRITE_REFLEX`. Separate before/after physical
counts stayed `CF_REFLEX_AUDIT=11, CF_REFLEX_AUDIT_ORDER=11`; an independent
global-history read identified the extra row as the pre-existing
`__scheduler__ / REFLEX_TICK_LATE` audit at `1786359232158204800` ns, not a
partial registration.

Audio was enabled but lazy before first use. A real one-second loopback
observation measured silence at -120 dB, completed the audio sensor in 182 ms,
reported `audio_status=healthy`, and persisted observation
`observe-01786359618782730300-0000000000`. A separate
`storage operation=row_read` decoded exact key
`18ca6c9bd6a5983c00000000`, 1,079 stored bytes, and SHA-256
`02458efe256cd0ec17312d47ff8bd5ba17e5a0d0fe4222ee11ac5b977edca965`.
Health then reported audio `ok`, the same vault id, and AVX2 CPU math.

The three scratch vaults and their three sibling lineage journals (382 KB)
were moved to the Windows Recycle Bin and separately confirmed absent from
`%TEMP%`. They remain recoverable until the Recycle Bin is emptied. The closing
issue comments record the final evidence-only commit, rebuilt installed-image
SHA-256, live PID, and authenticated provenance so the running binary exactly
matches the final `main` state without another tracked edit.
