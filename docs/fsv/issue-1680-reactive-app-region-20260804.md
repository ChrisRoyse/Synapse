# Issue #1680: exact app first-use new-region FSV (2026-08-04)

## Source of truth

The authoritative facts are the native Calyx recurrence series, the native
`Reactive` rows, the durable `Registry` delivery cursor, and the real bounded
`EventBus` subscription. The fixture uses the production timeline constellation
writer and production derived-state maintenance function in a fresh vault.

Vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-reactive-region-final-fsv-1785861259328`

## Root cause and design

Timeline ingestion already projected every non-empty app identity into a native
idempotent recurrence series, but discarded the first-occurrence fact. Therefore
no exact new-app region event existed. A corpus scan would be unbounded and a
process cache would forget history on restart.

The recurrence append now accepts caller-owned extension rows. While its durable
lock is held, it decides replay versus insertion, computes occurrence ID and
frequency, rewrites the Base frequency, appends Recurrence, and writes the
first-use Reactive row in one commit. The extension builder runs only for a new
occurrence; frequency one is the exact first-use predicate. The committed row is
then point-read before return.

Relay reads rows strictly after a durable Registry cursor. No matching listener
leaves the cursor unchanged. A matched, non-lossy enqueue advances and reads back
the cursor. A crash between enqueue and cursor commit can duplicate but cannot
lose an event; the stable sequence is the consumer deduplication identity.

Exa MCP v3.4.0 was live and returned 8,201 characters for the targeted
transactional-outbox/ordering/idempotency query. Built-in research used:

- https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html
- https://learn.microsoft.com/en-us/azure/architecture/databases/guide/transactional-out-box-cosmos

Both sources require state plus event to commit atomically, ordered relay from a
durable sequence, and idempotent handling of at-least-once duplicates.

## Manual execution and readback

Initial physical state: `Reactive=0`.

1. Ingested `fsv-first-app` with no subscriber. The atomic commit produced one
   Reactive row at `observed_seq=14`. Maintenance reported matched/queued `0/0`
   and kept cursor `0`.
2. Added a real `calyx.reactive.new_region` EventBus subscription and ran the
   same maintenance path. It delivered exactly one event with sequence `14`,
   frequency `1`, occurrence `0`, and the exact subject/trigger CxIds; cursor
   point-read was `14`.
3. Ingested the same app again. Reactive remained `1`.
4. Ingested a valid timeline row with no app. Reactive remained `1`.
5. Ingested `fsv-second-app`. Reactive became `2`; exactly one event was queued,
   none dropped, and cursor became `25`.

Independent recurrence reads:

- `fsv-first-app`: frequency `2`, active occurrences `2`;
- `fsv-second-app`: frequency `1`, active occurrences `1`.

The typed Reactive reader returned exactly two rows, both occurrence `0`,
frequency `1`, at sequences `14` and `25`.

## Restart and physical reopen

A separate process reopened the same vault, created a matching subscription, and
ran maintenance. Before/after Registry cursor was `25`; Reactive remained `2`;
events/matched/queued/dropped were all zero. A clean restart does not duplicate
already relayed events.

After that writer exited, `SynapseCalyxReadOnlyVault` opened only the native
Reactive family. It independently found exactly two rows:

- key `52524547494f4e31000000000000000e80a65178221b42420000000000000000`,
  exact JSON for `fsv-first-app`, sequence `14`;
- key `52524547494f4e310000000000000019664512f3436589d30000000000000000`,
  exact JSON for `fsv-second-app`, sequence `25`.

Thus producer rows and delivered notifications match one-for-one, repeat and
missing identities do not fire, pending delivery survives listener absence, and
the durable cursor survives process restart.

## Build and lint gate

`pwsh -File scripts/lint.ps1 -Fix` completed successfully after the manual
verification. All seven gates passed in both the root and Calyx workspaces,
including formatting, dependency policy, Clippy for all targets, and the Calyx
public-API ratchet (`370`, equal to its baseline).
