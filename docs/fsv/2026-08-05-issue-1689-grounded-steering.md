# Issue #1689: grounded steering partial FSV

Date: 2026-08-05 (America/Chicago)

This record covers the implemented and physically verified steering surfaces.
The issue remains open because the model-A/model-B anchor-flip acceptance and a
calibrated live-agent quarantine still require eligible grounded production
evidence; provisional evidence is deliberately not promoted to proof.

## Source of truth and happy paths

- Model decision row:
  `steering/v1/decision/model/1785967323076169000/884cb61dacfeb878b423160d5c837d605191f4caace7418ed8a19d495300c46f`
- Override current row:
  `steering/v1/override/model/current/d04cfa632f23dbe4d442f574d1a705eb44b91a0ed83ff87cad77001113f675ab`
- Override history row:
  `steering/v1/override/model/history/1785967323152969900/884cb61dacfeb878b423160d5c837d605191f4caace7418ed8a19d495300c46f`
- Override bytes: length `423`, SHA-256
  `cddb5cb0e6d81d42ef8ebda692475cc1e0b7920f29ed39ebd3d654d62f998f37`
- Subsequent recommendation physically returned the selected
  `operator-selected-model` and the stored override without changing measured
  statistics.
- Tool decision row:
  `steering/v1/decision/tool/1785967323401865900/cac88202a438116012ff04c63b80b641308348124d7bc0394a49405e11377379`
  with SHA-256
  `b862974decab014f64d20d3cb98549197ed6675604aa6b714dfa226b7ac55b52`.
  With no eligible attempts it was correctly provisional and emitted no
  invented transfer-entropy result.

## Risky-call gate

A real background MCP Bash request attempted:

```text
Remove-Item -LiteralPath C:\fsv-1689-never-created -Force
```

The gate persisted Ward candidate constellation
`bf68c0d2c39fffadb75b3f81d095c3c2`, created pending approval
`apr1-019fd4106a8971d281e66adb647679b4`, and reported Oracle one-class evidence
and uncalibrated Ward evidence as provisional. The operator decline was written
to the approval item and audit row. Independent disk state confirmed
`C:\fsv-1689-never-created` did not exist. Reproduction of the Ward candidate
at Ledger sequence `326440` proved entry present, self-verifying, subject
matching, and `drift=none`; entry hash was
`33015ff5386bbda224ea40a6e75a7d65e618012eb1796190570f025ffbe3c7cc`.

The initial one-class Oracle response
`SYNAPSE_CALYX_ENSEMBLE_ANCHOR_NOT_BINARY` exposed a classification bug. It now
maps to provisional insufficient evidence, as do no anchored rows and no
co-present lenses. Structural failures still fail closed.

## Edge cases

Empty task class, `min_evidence=0`, and `min_evidence=100001` each returned
`TOOL_PARAMS_INVALID`. The physical `steering_tool_recommend` action-row count
was `1` before and `1` after every trigger, proving validation caused no
decision mutation.

## Research

Exa MCP was live and completed a real search. Built-in web research used NIST
AI RMF guidance: inability to measure risk is not evidence of either high or
low risk, uncertainty must be documented, and human intervention is appropriate
when the system cannot detect or correct errors.

- https://airc.nist.gov/airmf-resources/airmf/1-sec-risk/
- https://airc.nist.gov/airmf-resources/airmf/3-sec-characteristics/
- https://airc.nist.gov/airmf-resources/airmf/5-sec-core/

Supporting gates passed. Steering commits: `899e28dd`, `2181694b`.
