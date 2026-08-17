# ADR: Measure sparse multivariate binary lanes through held-out predictions

Date: 2026-08-17  
Status: accepted for implementation; production FSV pending  
Issues: #1690

## Context

Production panel `2_185_004` physically stored 512-dimensional semantic action
atoms in slot 121. The first strict-client assay found 142 distinct vectors and
143 occupied `(vector, reward)` cells over 240 paired samples. Exact duplicate
classes made KSG's neighbour radius zero, while the Miller-Madow plug-in guard
correctly required 715 samples. Treating each complete vector as one categorical
identity discarded the shared hashed coordinates that the lens was designed to
make measurable.

Weakening the support guard, collapsing the vector to a hand-picked category,
or retrying a second estimator after the first refused would manufacture a
number rather than measure the lane. The valid quantity available now is a
held-out predictive lower bound. Barber and Agakov's
[variational information bound](https://proceedings.neurips.cc/paper_files/paper/2003/file/a6ea8471c120fe8cc35a2954c9b9c595-Paper.pdf)
and Poole et al.'s
[analysis of variational MI bounds](https://proceedings.mlr.press/v97/poole19a/poole19a.pdf)
establish the lower-bound framing. Independently, the data-processing inequality
binds the claim: a deterministic held-out prediction derived from `X` cannot
carry more information about `Y` than `X`.

## Decision

Extend `calyx-assay`'s declared MI instruments with `LogisticProbe`. Under
`Auto`, select it before measurement exactly when all of these physical facts
hold:

- exact duplicates make continuous KSG undefined at the requested `k`;
- the exact contingency table fails the plug-in estimator's declared
  five-samples-per-occupied-cell support bound;
- the input has more than one independently addressable coordinate; and
- the grounded outcome has exactly two levels.

The estimator uses Calyx's existing deterministic five-seed, fold-conditioned,
L2 logistic probe, convergence gate, planted-signal power calibration, and
held-out hard-prediction MI. Its refusal is terminal and typed; no other
estimator runs afterward. Non-binary, one-dimensional, underpowered, divergent,
or otherwise unsupported inputs retain their existing fail-closed behavior.

Expose the selected estimator plus input dimension, outcome cardinality,
distinct whole-row count, occupied joint cells, and duplicate multiplicity in
the assay readback. For synergy, if any of the pair/left/right terms selects the
logistic route, select `LogisticProbe` for all three terms before measurement so
their biases and held-out semantics match. A failure leaves the pair unmeasured.

## Consequences

- Exact request identity remains unchanged in slot 118 and semantic coordinates
  remain typed and separate in slot 121; no coarse replacement or side store is
  introduced.
- The reported bits are a conservative predictive lower bound, never an
  assertion that the probe recovered all of `I(X;Y)`.
- Slots and pairwise gains flow through the existing bits, sufficiency,
  redundancy, causal-map, kernel, Guard, oracle, readiness, and MCP storage
  intelligence surfaces without a special consumer path.
- Production acceptance still requires a repo-built daemon, strict client
  `tools/list`, real MCP trigger, and separate Assay/derived-state readback. A
  successful build is structural evidence only.
