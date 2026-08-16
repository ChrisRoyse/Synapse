# Action readiness is bound to one immutable panel generation

- **Status:** accepted
- **Date:** 2026-08-16
- **Issue:** #2253
- **Supersedes:** readiness row `oracle-readiness/v2/synapse.action`

## Context

Action panel `2_185_002` added the point-in-time request-cause lens. The held-out
validation and future measurement paths were re-armed, but the persisted
readiness row was not self-describing: schema v2 stored neither its semantic
domain nor its frozen panel version. The read path checked only `schema_version
== 2`, so health served the retained `2_185_001` report as though it described
the current panel. Its Guard, kernel, corpus, and validation remediations all
physically named the superseded generation.

This is unsafe independently of the old verdict's sign. A negative verdict is
misleading operational state, while a retained positive verdict could authorize
autonomy after a later slot-layout change.

Primary guidance supports explicit immutable feature identity. Feast's feature
view versioning snapshots definition changes, version-qualifies serving, and
rejects definition/version conflicts. BigQuery's feature-serving contract binds
training and inference to point-in-time feature values to prevent leakage.

- <https://docs.feast.dev/reference/alpha-feature-view-versioning>
- <https://github.com/feast-dev/feast/blob/master/docs/adr/rfc-feature-view-versioning.md>
- <https://docs.cloud.google.com/bigquery/docs/feature-serving>

## Decision

1. Readiness generation v3 is stored only at
   `AnnealReport/oracle-readiness/v3/synapse.action`.
2. Every row stores exact `domain` and `panel_version` fields alongside its
   schema version and report.
3. Measurement writes `domain=synapse.action` and the compile-time frozen
   `ACTION_PANEL_VERSION`, then independently reads the row through the same
   strict scope gate.
4. Reads require schema, domain, and panel equality. A mismatch is
   `SYNAPSE_CALYX_READINESS_SCOPE_MISMATCH`; no field is inferred and no older
   generation is consulted.
5. The v2 row remains immutable historical evidence at its original key. Its
   absence from the current read path is intentional, not deletion or migration.

## Consequences

After any future panel bump, current readiness is physically absent until it is
remeasured under a correspondingly bumped readiness generation. Health and
arming consumers can no longer confuse an inherited verdict with evidence about
the current slot layout. Guard, kernel, held-out validation, and corpus evidence
must all be rebuilt for the exact current panel before readiness can pass.
