# Action guard axis and typed causal replay

Date: 2026-08-17

Status: implemented; production FSV pending

Issue: #1690

## Physical finding

Production panel `2_185_005` stored command-final execution outcomes correctly,
but two downstream consumers still collapsed different questions. Ward pooled
every boolean anchor when calibrating an out-of-distribution boundary, so an
internal execution failure was indistinguishable from an inadmissible request.
The oracle mistake report grouped only by action name and majority-voted all
historical outcomes. A deliberately broad corpus of invalid `act_run_shell`
requests therefore made novel, valid shell commands look predictably bad even
when their point-in-time preconditions were satisfied.

This is an axis-identity and causal-replay defect. It is not evidence for a
weaker Ward threshold or for treating an action name as a sufficient cause.

## Decision

Publish immutable action generation `2_185_006` and bind each decision to its
declared grounded axis.

- Preserve execution reliability on `reward` and
  `action_execution_reward`. Add the independent boolean label
  `action_guard_region`: success is inside the region; closed request, target,
  and policy rejections are outside; internal, timeout, cancellation, and
  runtime failures are unadjudicated on this axis.
- Persist Ward's optional calibration anchor selector in the profile and filter
  the adjudicated corpus by that exact kind. Action readiness refuses profiles
  not calibrated on `action_guard_region`.
- Add typed slot 123, `syn.action.admission_context.v1`. It encodes only the
  authenticated request and immutable `before` state, including executable
  resolution, working-directory state, and required-environment completeness.
  Exact request identity remains independently present in slot 118.
- Resolve shell executables before the trigger using the delivered child
  environment and Windows/Rust search semantics. Persist only a state, source
  class, path class, and exact path-byte digest. Relative programs are resolved
  to an absolute path before measurement and spawn; ambiguous Windows
  drive-relative paths refuse. Metadata errors are structured terminal failures;
  there is no assumed or current-state reconstruction during replay.
- Replace action-name majority voting with chronological, per-slot nearest
  neighbours fused by reciprocal rank. Typed slots are ranked separately and
  never flattened. Ties or absent evidence are `Insufficient`, never an
  implicit negative prediction.
- Preserve slot 123 and generation `2_185_006` as immutable history. Production
  calibration physically proved that v1 collapsed different good/bad scalar
  requests: their request-vector and request-atom bytes differed while slot 123
  was identical. Generation `2_185_007` therefore adds slot 124,
  `syn.action.admission_context.v2`, a 512-dimensional signed projection over
  separately namespaced bounded request-semantic and immutable-`before`
  semantic atoms. It is explicitly absent unless both causal sides were
  persisted before the trigger. It never reconstructs historical host state,
  imports terminal fields, weakens Ward's FAR, or uses exact request identity as
  the calibration boundary.
- Preserve slot 124 and generation `2_185_007` as immutable history. The next
  physical calibration isolated another exact collision: an accepted 256-byte
  idempotency key and a refused 257-byte key had different authenticated
  request digests but the same v2 causal vector. The writer had persisted only
  `idempotency_key_present`, so the boundary fact did not exist at measurement
  time. Generation `2_185_008` adds slot 125,
  `syn.action.admission_context.v3`. The command writer now seals
  `synapse.shell_admission_facts.v1` before authorization or execution: bounded
  non-secret facts for command shape, environment validity, prohibited-command
  predicates, timeout modes, idempotency boundary class, execution mode, and
  allow-shell policy state. Slot 125 separately namespaces that snapshot,
  authenticated request atoms, and immutable host preconditions. A historical
  row without the complete snapshot is `Absent`; terminal errors and current
  host state are never substitutes.

## Research basis

Microsoft documents the executable-name ambiguity and Windows search order for
process creation. Rust documents its own `Command` executable resolution and
the platform-specific search order applied before `CreateProcessW`. The stored
preflight therefore measures the same delivered environment used by the
trigger instead of consulting a later ambient host state.

Primary references:

- https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw
- https://doc.rust-lang.org/std/process/struct.Command.html
- https://www.hsph.harvard.edu/miguel-hernan/causal-inference-book/
- https://docs.cloud.google.com/bigquery/docs/reference/standard-sql/bigqueryml-syntax-feature-time
- https://docs.cloud.google.com/bigquery/docs/feature-serving
- https://learn.microsoft.com/azure/machine-learning/offline-retrieval-point-in-time-join-concepts
- https://airc.nist.gov/airmf-resources/airmf/5-sec-core/

## Consequences

Guard calibration, Goodhart validation, mistake closure, causal-map
publication, kernel construction, and readiness now consume explicit grounded
axes. Existing v1 rows retain their historical `legacy_unknown` measurement.
The v2 slot is instead `Absent` when an independently persisted request or
immutable precondition is unavailable; it is never rewritten from current
filesystem state. Panel, profile, assay, kernel, derived-state, and validation
artifacts must be rebuilt and physically read back for `2_185_008` before
readiness can pass.

Deployment also exposed an independent ledger-type ambiguity: action
validation and Forge promotion receipts legitimately use the broad
`EntryKind::Anneal`, but their payloads are not native `AnnealLedgerEntry`
payloads. The native Anneal reader now selects the exact
`Kernel("anneal\\0" + change_id)` subject namespace before decoding. This
preserves every append-only historical row, keeps malformed native rows fatal,
and prevents another subsystem sharing the stable wire kind from poisoning
Anneal health.

The first real `action_guard_region` calibration then refused with
`CALYX_GUARD_PROVISIONAL`: at least one known-bad slot vector had cosine `1.0`
to a trusted vector, so the only zero-accept threshold was greater than the
maximum legal cosine. This is evidence that the pre-trigger lens collapses
different causal contexts, not permission to weaken FAR. Ward's Synapse
adapter now retains each bad row's nearest-good identity through exact Forge
kNN and appends a bounded bad-cx-id to good-cx-id collision list to the
structured calibration error. The diagnostic is read-only, contains no action
payload, does not change score selection, and makes the physical rows needing
an immutable lens repair directly inspectable. Calibration-time corpus rows
also retain their canonical `input_ref.pointer` only long enough to render that
diagnostic, so each collision names the exact source CF/key that can be read
back. The pointer is excluded from the serialized serving profile and no raw
request or output enters the guard artifact.

The conformal design remains deliberately fail-closed. Finite-sample coverage
or risk control calibrates a supplied score; it does not make an
outcome-insensitive representation separable. Exact good/bad feature collisions
must be corrected at measurement time or treated as missing evidence, while
the real bad-case corpus and Clopper-Pearson requirements remain unchanged.

Manual production verification found a second, independent numeric defect
after a profile successfully calibrated. Calibration normalized the corpus and
ranked it with Forge's CUDA dot-product reduction, while `guard_verify` rescored
the selected vectors with `calyx_core::dense_cosine` on the CPU. The maximum
known-bad CUDA score was `0.9866821765899658`, so conformal calibration selected
the next representable threshold. The same physical bad record scored
`0.9866834282875061` through the serving reduction and was accepted. The tool's
return value had reported zero calibration bad accepts, but the separately read
serving verdict falsified that claim.

Ward now defines a versioned canonical scoring contract,
`calyx_core::dense_cosine:f32-sequential-v1`. Synapse constructs both
leave-one-out good scores and bad-to-good scores with that exact function and
reduction order; calibration is deliberately CPU-bound because it is outside
the serving hot path and a security threshold requires numeric identity more
than batched throughput. The profile persists both the scoring-engine identity
and its explicit deviation envelope (zero for the identical path). Legacy or
malformed profiles remain readable history but fail high-stakes use with
`CALYX_GUARD_CALIBRATION_SCORE_CONTRACT`. The MCP calibration result exposes
the backend, engine, and tolerance so the physical artifact and tool surface
state the same contract. A future scorer or reduction order must use a new
engine id and fresh calibration; it cannot silently reuse these thresholds.

NVIDIA documents that parallel reduction order and fused operations can produce
different valid floating-point answers, and NIST recommends quantifying and
documenting numerical reproducibility instead of assuming bitwise equivalence.
Primary references:

- https://proceedings.iclr.cc/paper_files/paper/2024/hash/f3549ef9b5ff520a7e41ff3cc306ab2b-Abstract-Conference.html
- https://proceedings.mlr.press/v267/zhang25dn.html
- https://docs.nvidia.com/cuda/archive/12.1.1/floating-point/index.html
- https://www.nist.gov/programs-projects/numerical-reproducibility
- https://www.nist.gov/srd/critical-evaluation-criteria
