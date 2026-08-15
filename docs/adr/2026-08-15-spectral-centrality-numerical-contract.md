# ADR: Spectral centrality numerical contract

## Status

Accepted and amended — 2026-08-15 (#2130).

## Context

The component-local shifted power iteration used `f32` for sparse accumulation,
normalization, Rayleigh recovery, and its successive-vector convergence check.
On a real Windows process forest, that change norm plateaued around `1e-6` for
more than 65,536 iterations. Its identity shift also used the component's
maximum weighted degree. That is a valid but arbitrarily loose spectral-radius
bound on irregular graphs: for a `k`-leaf star it is `k`, while the radius is
only `sqrt(k)`. The oversized identity shift creates an artificial near-unit
contraction ratio. The process observer therefore remained disabled because one
uncertified component failed the complete derived-state subpass.

This was not dominant-eigenvalue multiplicity inside a connected component. A
connected nonnegative adjacency block is irreducible and has a simple Perron
root. The positive identity shift also removes the bipartite `+rho`/`-rho`
modulus tie. The observed tolerance-scale plateau was the numerical signature
of single-precision roundoff in the iterative kernel and its stopping metric.

The first repair made the real process forest operational, but a later
1,184-node live agent component still stopped at residual `4.10248e-6` after
the frozen 256 steps even though its normalization scale (`4.93779`) was close
to its measured radius (`4.544397`). This exposed the remaining independent
limit: power iteration's rate is controlled by the leading-eigenvalue ratio.
Irreducibility makes the Perron root simple but supplies no minimum spectral
gap, so a fixed step budget cannot be the generic solver contract.

## Decision

- Keep connected-component decomposition and the component normalization
  `A_c / scale_c`, where `scale_c = ||A_c^8||_inf^(1/8)`.
- Keep persisted graph weights and public centrality scores as `f32`, but run
  bounded component-local operator products, orthogonalization, projection, and
  residual certification in `f64`.
- Replace shifted power iteration with a deterministic,
  full-reorthogonalized sparse Krylov/Rayleigh-Ritz solve. Select the largest
  algebraic Ritz value, which distinguishes the Perron root from the `-rho`
  partner on a bipartite component without an identity shift.
- Treat the caller's `max_iter` as a strict Krylov-basis ceiling. Basis and
  product storage are bounded by
  `O(component_nodes * min(component_nodes, max_iter))`; no dense component
  matrix is materialized.
- Certify convergence with normalized eigenpair residual
  `||A_c x - lambda*x||_2 / |lambda|` against a fresh sparse product, not the
  projected residual or a return value alone.
- Reject a zero iteration budget and non-finite or non-positive tolerances with
  named errors.
- Fail the complete structural-signature pass when any component is
  uncertified. A partial centrality has no representation in the frozen
  signature schema and must not be silently published.
- Enable the bounded process-topology observer by default at 120 seconds now
  that a real process forest is publishable.

## Consequences

The solver remains sparse, deterministic, bounded, and fail-closed. It removes
both the `f32` residual floor and power iteration's direct fixed-gap dependency
without widening persisted state or adding a fallback solver. A genuinely hard
component may still exhaust the bounded Krylov dimension; that remains a named
`CALYX_SPECTRAL_NOT_CONVERGED` error carrying the measured normalized residual
and exact component identity. Projected eigensolver failure remains separately
named `CALYX_SPECTRAL_JACOBI_NOT_CONVERGED`.

## Research basis

- [NetworkX eigenvector centrality](https://networkx.org/documentation/stable/reference/algorithms/generated/networkx.algorithms.centrality.eigenvector_centrality.html): a positive all-ones start and `A + I` power iteration target the unique positive eigenvector on a connected graph.
- [LAPACK symmetric eigenproblem error bounds](https://www.netlib.org/lapack/lug/node90.html): eigenpair accuracy is assessed through backward error; eigenvector forward sensitivity separately depends on the spectral gap.
- [SciPy `eigsh`](https://docs.scipy.org/doc/scipy/reference/generated/scipy.sparse.linalg.eigsh.html): symmetric sparse eigensolvers use a relative accuracy stopping criterion and fail explicitly when convergence is not obtained.
- [Nick Higham on spectral radius](https://nhigham.com/2024/01/12/what-is-the-spectral-radius-of-a-matrix/): every consistent matrix norm bounds spectral radius, and Gelfand's formula tightens the bound through roots of matrix-power norms.
- [Netlib power method](https://www.netlib.org/utk/people/JackDongarra/etemplates/node95.html): power-method convergence depends on the ratio of the two leading eigenvalue magnitudes.
- [SLEPc eigensolver manual](https://slepc.upv.es/release/documentation/manual/eps.html): production symmetric sparse solvers use Lanczos/Krylov-Schur-class methods and residual-based convergence; power iteration is classified as a basic method.
