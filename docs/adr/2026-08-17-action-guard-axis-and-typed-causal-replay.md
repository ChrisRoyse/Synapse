# Action guard axis and typed causal replay

Date: 2026-08-17

Status: implemented; production guard separability repair in progress

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

## Consequences

Guard calibration, Goodhart validation, mistake closure, causal-map
publication, kernel construction, and readiness now consume explicit grounded
axes. Existing rows without executable preflight remain visibly
`legacy_unknown`; they are never rewritten from current filesystem state.
Panel, profile, assay, kernel, derived-state, and validation artifacts must be
rebuilt and physically read back for `2_185_006` before readiness can pass.

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
Primary references:

- https://proceedings.iclr.cc/paper_files/paper/2024/hash/f3549ef9b5ff520a7e41ff3cc306ab2b-Abstract-Conference.html
- https://proceedings.mlr.press/v267/zhang25dn.html
