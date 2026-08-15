# ADR: Spectral centrality numerical contract

## Status

Accepted — 2026-08-15 (#2130).

## Context

The component-local shifted power iteration used `f32` for sparse accumulation,
normalization, Rayleigh recovery, and its successive-vector convergence check.
On a real Windows process forest, that change norm plateaued around `1e-6` for
more than 65,536 iterations. The process observer therefore remained disabled
because one uncertified component failed the complete derived-state subpass.

This was not dominant-eigenvalue multiplicity inside a connected component. A
connected nonnegative adjacency block is irreducible and has a simple Perron
root. The positive identity shift also removes the bipartite `+rho`/`-rho`
modulus tie. The observed tolerance-scale plateau was the numerical signature
of single-precision roundoff in the iterative kernel and its stopping metric.

## Decision

- Keep connected-component decomposition and the scale-commensurate operator
  `B = I + A_c / scale_c`.
- Keep persisted graph weights and public centrality scores as `f32`, but run
  bounded component-local accumulation and normalization in `f64`.
- Certify convergence with normalized eigenpair residual
  `||Bx - mu*x||_2 / |mu|`, not successive-vector distance.
- Reject a zero iteration budget and non-finite or non-positive tolerances with
  named errors.
- Fail the complete structural-signature pass when any component is
  uncertified. A partial centrality has no representation in the frozen
  signature schema and must not be silently published.
- Enable the bounded process-topology observer by default at 120 seconds now
  that a real process forest is publishable.

## Consequences

The solver remains sparse, deterministic, bounded, and fail-closed. It removes
the `f32` residual floor without widening persisted state or adding a fallback
solver. A genuinely small spectral gap may still exhaust the frozen iteration
budget; that remains a named `CALYX_SPECTRAL_NOT_CONVERGED` error carrying the
measured normalized residual and exact component identity.

## Research basis

- [NetworkX eigenvector centrality](https://networkx.org/documentation/stable/reference/algorithms/generated/networkx.algorithms.centrality.eigenvector_centrality.html): a positive all-ones start and `A + I` power iteration target the unique positive eigenvector on a connected graph.
- [LAPACK symmetric eigenproblem error bounds](https://www.netlib.org/lapack/lug/node90.html): eigenpair accuracy is assessed through backward error; eigenvector forward sensitivity separately depends on the spectral gap.
- [SciPy `eigsh`](https://docs.scipy.org/doc/scipy/reference/generated/scipy.sparse.linalg.eigsh.html): symmetric sparse eigensolvers use a relative accuracy stopping criterion and fail explicitly when convergence is not obtained.
