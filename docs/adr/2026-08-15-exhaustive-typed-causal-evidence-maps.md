# Exhaustive typed causal-evidence maps

- **Status:** accepted
- **Date:** 2026-08-15
- **Issue:** #2249
- **Supersedes:** the implicit assumption that one transfer-entropy pair is the system's causal model

## Context

Calyx already carries Gaussian PC-stable skeleton discovery, partial-correlation networks, transfer entropy, linear Granger causality, convergent cross mapping, signed cross-correlation, temporal cross-K, and multivariate Hawkes branching. Synapse exposed only a single transfer-entropy comparison. When the caller omitted stream names, that path chose the two most frequent values; its shared loader stopped at a 20,000-row panel-membership prefix, ignored the public time-window fields, and did not disclose incompleteness. A successful observational estimate was reported as `grounded=true`, which confused estimator stability with identification of a structural effect.

Primary sources constrain the interpretation:

- PC structure recovery relies on Gaussian/Markov/faithfulness/causal-sufficiency/sparsity assumptions, and the Calyx implementation returns a skeleton rather than oriented effects.
- Transfer entropy and Granger are equivalent for Gaussian variables, so their agreement is not independent confirmation.
- Hawkes branching matrices describe event triggering under the point-process model; stationarity interpretation requires a subcritical spectral radius.
- Exhaustive p-value families require explicit multiplicity control.
- Observational effect claims still require consistency, exchangeability, positivity, and a declared estimand/interference model.

## Decision

1. `storage operation=intelligence sub-operation=causal_map` is the production agent surface. No new top-level MCP tool is added.
2. The request is defined by a panel, metadata `group_key`, and inclusive/exclusive source-event-time scope. Omitting both stream values means every `C(n,2)` pair; supplying both means exactly one pair; supplying one fails closed.
3. The temporal loader must read the complete requested scope. Encountering record `max_records + 1` is `SYNAPSE_CALYX_TEMPORAL_SCOPE_EXCEEDS_MAX_RECORDS`, never a partial estimate. Missing group values and excessive stream cardinality also fail rather than sample.
4. The artifact keeps estimator lanes separate, including their assumptions, samples, and typed failures. There is no combined causal score and no estimator fallback.
5. Benjamini-Hochberg q-values are persisted for each applicable p-value family. Transfer entropy and CCM retain their native confidence/convergence evidence rather than receiving invented p-values.
6. Every artifact is `observational_predictive` and `structural_effect_identified=false` unless a future version accepts and verifies a persisted intervention/identification contract. Estimator agreement alone cannot change that class.
7. The complete artifact is content-addressed under the native Calyx Graph CF (`GCMP1...`). The write is flushed and separately read byte-for-byte before the response exposes its key, SHA-256, byte count, Graph row count, and readback verdict.

## Consequences

- Large or heterogeneous calls must narrow their physical source-event scope instead of receiving a biased prefix.
- A lane can be unresolved because its assumptions or quorum are not met while the map remains a complete account of what was and was not measurable. The persisted typed error is evidence, not a silent substitute.
- Consumers can use predictive arrows for explanation and hypothesis generation, but structural control requires a separately identified causal contract.
- Existing periodicity, drift, hazard, and targeted causality calls inherit the source-event window and complete-scope refusal, removing the same silent-truncation class from the shared loader.

## References

- Kalisch and Bühlmann, “Estimating High-Dimensional Directed Acyclic Graphs with the PC-Algorithm,” JMLR 8 (2007).
- Barnett, Barrett, and Seth, “Granger Causality and Transfer Entropy Are Equivalent for Gaussian Variables,” Physical Review Letters 103 (2009).
- Xu, Farajtabar, and Zha, “Learning Granger Causality for Hawkes Processes,” ICML/PMLR 48 (2016).
- Embrechts and Kirchner, “Hawkes Graphs,” Theory of Probability and Its Applications 62 (2018; preprint 2017).
- Benjamini and Hochberg, “Controlling the False Discovery Rate,” JRSS B 57 (1995).
- Hernán and Robins, *Causal Inference: What If* (living edition).
