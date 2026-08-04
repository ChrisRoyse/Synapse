# Issue #1677: Ward verdict ledger and guarded find FSV (2026-08-04)

## Diagnosis and research

`find` hardcoded `GuardChoice::Off`; `calyx-search` read only
`Guard/profile\0default`, while Synapse correctly stores non-active profiles at
`profile\0panel\0<version>`. Ward verification also returned security verdicts
without appending them to the provenance Ledger. The existing FSV timestamp
fixture unintentionally rotated both labelled cohorts through an extra day,
making its claimed antipodal feature overlap in reality.

Research lanes: `scripts/check-research-lane.ps1` reported Exa MCP live and a
real Exa query completed. Built-in web research used primary sources. The design
follows calibrated OOD detection's held-out error-control requirement and NIST's
policy-enforcement principle that decisions must be explicit and auditable:

- Bates et al., *Calibrated Out-of-Distribution Detection with Conformal
  P-values*: https://proceedings.neurips.cc/paper_files/paper/2022/hash/0f659a20f08deb9d4ffc7e621bdc0a33-Abstract-Conference.html
- NIST SP 800-207, *Zero Trust Architecture*:
  https://csrc.nist.gov/pubs/sp/800/207/final

## Source of truth and trigger

Instrument:
`cargo run -p synapse-storage --example ward_declared_enum_adjudication_fsv -- <fresh-dir>`

Fresh vault:
`%TEMP%\synapse-ward-fsv-df7d41b7d3694acf871a8e2ecaeadf6a`

Sources of truth: physical Guard CF panel profile, immutable persisted-search
manifest, Base/slot constellation rows used by the query, and append-only Ledger
rows independently decoded and rehashed after each Ward verification.

Known corpus: 550 real persisted constellations, 400 good and 150 bad. Good hour
vectors occupy hours 8-11; bad vectors occupy hours 20-23. This is the minimum
fixture already justified by the exact one-sided binomial calibration bound.

## Physical evidence

```text
scanned=550 good=400 bad=150 unadjudicated=0 conflicting=0
Guard profile bytes=711 rows=2 tau=-0.707107 readback_calibrated=true
good_pass=true bad_refused=true
good Ledger seq=550 bad Ledger seq=551
physical_hashes_match=true self_verify=true
guarded find hits=20 verdicts=20 dropped=42
search manifest sha256=c7ba8c0957c6e8a18c89683c1868d5e19035ea852dec37cf2702b9da912bdbd1
ALL CLAIMS OK
```

The guarded query explicitly addressed panel 1963001 and therefore proved the
panel-keyed Guard namespace, rather than the active-panel compatibility mirror.

## Boundary audit

Each invalid trigger read the Ledger row count before and after:

```text
missing_record: before=552 refused=true after=552 unchanged=true
wrong_panel: before=552 refused=true after=552 unchanged=true
invalid_cx_format: before=552 refused=true after=552 unchanged=true
```

Structured codes were respectively
`SYNAPSE_CALYX_GUARD_QUERY_RECORD_MISSING`, `CALYX_GUARD_PROVISIONAL`, and
`SYNAPSE_CALYX_CX_ID_INVALID`. No invalid request released or persisted a
verdict.

Two failed reality passes preceded the successful run and were not hidden. The
first exposed the fixture's overlapping timestamp geometry. The second showed
that the reported guard-verdict count was measured before by-example self-match
removal (21 internal versus 20 returned); the count now derives from the final
returned hit set.
