# ADR: Calyx Intelligence Integration Doctrine

## Status

Accepted — 2026-07-23.

## Context

Synapse uses Calyx as its association-native durable intelligence substrate,
not merely as a replacement key/value backend. The integration must preserve
Calyx's typed constellation model and expose the capabilities derived from it.

## Decision

Build Calyx capabilities in this order:

1. decompose each domain into irreducible, independently measurable atoms;
2. compute every base association among those atoms;
3. differentiate associations against grounded anchors in bits;
4. distill the minimal generating kernel;
5. compose retrieval, guard, prediction, completion, forecasting, provenance,
   and reversible self-optimization from that grounded kernel.

Embed latent inputs such as prose, images, audio, video, and code. Encode
explicit structured values deterministically and retain their exact scalar or
metadata representation. Hybrid records carry both families as separate typed
slots. Slots are never flattened, and absent slots are never zero vectors.

Production storage readers that claim coherent multi-page state require full
MVCC recovery. Latest-only recovery is limited to explicit inspection or
one-time fail-closed adoption work and cannot serve coherent production reads.

Model backend selection defaults to `auto`: prefer a compatible GPU only after
capability and memory admission succeeds; otherwise select the embedded
lightweight CPU model. Explicit CPU or GPU overrides remain configurable and
fail loudly when unavailable. A single operation never silently changes its
selected backend. All core runtime models ship inside `synapse-mcp.exe`; first
use extracts and cryptographically verifies embedded bytes and never downloads
them.

Every shipped behavior is accepted only through manual Full State Verification:
the real strict MCP trigger, a separately read physical Source of Truth, known
synthetic inputs, happy path, and at least three boundary/error cases. Return
values, compilation, lint, and logs alone are not behavioral proof.

## Consequences

Full MVCC recovery costs more startup time and memory, but it is required for
coherent snapshots. Exhaustive association construction costs more than sampled
or hand-picked relationships, but grounded meaning depends on the complete
measurable association web. GPU execution maximizes capability where available;
the bundled INT8 CPU path preserves core operation on CPU-only hosts with
explicitly reduced capability.

GitHub Issues remain the execution-state surface. This ADR defines direction;
individual issue closure still requires physical evidence.
