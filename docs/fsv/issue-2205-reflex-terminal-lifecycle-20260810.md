# FSV — #2205: acknowledged durable reflex terminal lifecycle

Date: 2026-08-10

Host: configured Windows production host

Branch: `main` only; no worktree or secondary branch

## Accepted behavior

Scheduler-owned `Expired` and `ActionDenied` transitions now use a two-phase,
off-tick lifecycle protocol. The scheduler first makes the reflex
non-dispatchable while its public status remains active. A dedicated writer
atomically persists an exact terminal intent with an active desired-state row,
flushes it, and independently reads back both the desired row and prepare audit.
Only that acknowledgement permits the completion transaction. Completion
atomically publishes the exact terminal desired row, final source audit,
ordered projections, grounding, anchor, and ledger entry; flushes and reads the
exact bytes back; and only then publishes the terminal runtime status.

A process that opens a vault completes all prepared intents before constructing
or activating a scheduler. A pending or failed token stays non-dispatchable,
scheduler replacement is refused, and health reports pending/prepared/committed/
failed counts plus the exact last failure identity, phase, detail, and repair
instruction. Terminal audits are refused on the ordinary telemetry path.

The scheduler hot path performs no Calyx I/O and never waits for the writer.

## Diagnosis and research

The defect was diagnosed before solution research. The old scheduler mutated
control and public status to terminal, then used a nonblocking audit queue as the
only route to durable desired state. A process loss before that worker write left
an active executable row that startup was required to replay. An observation
queue therefore acted as lifecycle authority without an acknowledgement edge.

`scripts/check-research-lane.ps1` performed real MCP `initialize`, `tools/list`,
and `tools/call` operations and reported Exa MCP live. Exa results were checked
through the built-in web lane against primary sources:

- Akka persists durable effects before publishing actor state and recovers from
  its journal: <https://doc.akka.io/libraries/akka-core/current/typed/persistence.html>.
- Kafka's exactly-once design couples output and offset publication into one
  transaction: <https://kafka.apache.org/documentation/#semantics_eos>.
- Crossbeam documents bounded-channel nonblocking `try_send` behavior:
  <https://docs.rs/crossbeam-channel/latest/crossbeam_channel/struct.Sender.html#method.try_send>.

The exact diagnosis and research chronology is retained in the issue:
<https://github.com/ChrisRoyse/Synapse/issues/2205#issuecomment-5241405423>.

## Source of truth

Acceptance used separate physical reads rather than enqueue or registration
return values:

1. the exact `CF_KV` row under `reflex/desired/v1/<reflex-id>`;
2. exact source rows under `CF_REFLEX_AUDIT` and their SHA-256 digests;
3. the Calyx append-only ledger and raw commitment verification;
4. independently locked runtime status/control state and terminal queue counters;
5. the Windows process table at the forced-crash boundary.

The verifier was a temporary executable using an isolated real Calyx vault, the
real scheduler thread, real writer thread, and real action-permission gate. It
was removed after this evidence was captured; no test or harness was retained.

## Happy path: one-shot expiry

Before the trigger, a separate vault read found the desired row `active`, no
terminal intent, and one registration audit. Its SHA-256 was
`a36cf0232a755053dbb302980d4ab0ab3329c1cfbd98b8997858130829174f8b`.

After the real scheduler tick:

- runtime and durable state were both `expired`, with `fire_count=1`;
- desired-row SHA-256 was
  `1cce786f152a57e4a92f10e24b9228f04a9542681c55f10bc95c207ec8d0ec7c`;
- the terminal intent was absent and three exact audits existed;
- the latest audit was `reflex_lifetime_expired` / `expired`;
- queue state was pending `0`, prepared `1`, committed `1`, failed `0`;
- the independent chain verdict was `intact` across six entries, with raw
  commitments intact.

## Edge 1: real action-permission denial

Before the trigger, a separate vault read found `active`, no intent, one audit,
and desired-row SHA-256
`3cfceb304e0a76d7555a19901c17effd88438d24754074734ce719cb70b9e114`.
The real denial gate returned policy code `ISSUE_2205_DENIED`, reason
`manual_fsv`, profile `issue-2205`, and use scope `manual_fsv` for a real
`ReleaseAll` request.

After the trigger:

- runtime and durable state were both `action_denied`, with `fire_count=0`;
- desired-row SHA-256 was
  `99620278d18aa8ce7526c3e6c9f4c953c07933d20b18749987a2a01b58f8362c`;
- the terminal intent was absent and three exact audits existed;
- the latest audit was `reflex_action_permission_denied` /
  `action_denied`, retaining the policy fields and denied action step;
- queue state was pending `0`, prepared `1`, committed `1`, failed `0`;
- the chain was intact across seven entries and raw commitments were intact.

## Edge 2: process loss at the prepared boundary

Before firing, a physical read found `active`, no intent, one audit, and
desired-row SHA-256
`577d6286f56493202422a3b6d3bb284bec3e977db5942e14979122e2a0cce6e0`.

At the prepared boundary, Windows PID `10512` was independently observed alive
from image `target\debug\examples\issue_2205_verify.exe`. Public status and the
durable status were deliberately still `active`; the control was
non-dispatchable. The physical desired row contained intent
`019fec85-4e40-7722-87da-54735f2e909e`, whose frozen status and audit were both
`expired`. Its SHA-256 was
`b053480f2d55c589ba27c552ad08af630dc084a3634f6c8965658e21baf00c5c`.
Queue state was pending `1`, prepared `1`, committed `0`, failed `0`, and two
source audits physically existed.

A competing registration was refused before mutation with
`REFLEX_TERMINAL_LIFECYCLE_PENDING`, the exact pending reflex id, and a retry
instruction tied to the health counter. PID `10512` was then forcibly
terminated; a separate process-table read proved it absent.

A fresh process opened the same vault. Before scheduler activation it completed
the exact intent. Independent after-state was:

- active runtime count `0`, runtime rows `0`, recovered activations `0`;
- durable status `expired`, `fire_count=1`, no remaining intent;
- desired-row SHA-256
  `8980015346669336b7af770e738907cfe9bc9f4799f102ae47bceaf6619518b8`;
- exactly three audits, with the latest audit carrying the same intent id and
  SHA-256
  `b97f14e6871d5c37091bc2727fd65f66f8f76a06df4c319a4d3c55def004bfdf`;
- ledger verdict `intact` across seven entries and all 15 raw commitments
  intact.

The definition was not replayed and the terminal transition was not lost.

## Edge 3: malformed durable intent

An isolated desired row started `active` with SHA-256
`b7c767c110e39655ac3adbdee445d288c10d04703de09610054e1c6df2b58465`.
The trigger physically replaced only `terminal_intent` with `{}`, producing
SHA-256
`ce7ff4048c3ae10bbec54163c2646cff500049a506aab240c81b607558aa0564`.

A separate process failed startup with
`REFLEX_DURABLE_DECODE_FAILED`, named the exact reflex and value digest,
reported missing `intent_id`, and instructed the operator to preserve and
repair or explicitly migrate that exact row. A third process independently
read the row after the failure: `{}` and the same SHA-256 remained. Startup did
not skip, rewrite, or activate corrupt state.

## Failure found and corrected during FSV

The first happy-path run failed the terminal prepare transaction with
`REFLEX_GROUNDED_LIFECYCLE_KIND_INVALID`. Root cause was a duplicated lifecycle
allowlist in the reflex projection and storage transaction layers that knew
registration/cancellation/disable/final terminal kinds but not the new
`reflex_terminal_lifecycle_intent_prepared` fact. Both validators now admit
exactly that active prepare kind. The full manual matrix above was rerun from
fresh vaults after the correction; no failure was hidden or converted into a
passing check.

## Build, lint, deployment, and production readback

The temporary verifier was removed before acceptance gates. On the resulting
production-only tree:

- `cargo check --workspace` passed in 46.52 seconds;
- `pwsh -File scripts/lint.ps1` passed all seven canonical gates in both the
  root and Calyx workspaces in 183.8 seconds, including zero-test/zero-harness,
  formatting, dependency policy, and `clippy --all-targets -D warnings`.

The exact clean implementation commit is installed through
`scripts/synapse-setup.ps1` with the production audio and permission set. Setup
used all 12 logical CPUs (`CARGO_BUILD_JOBS=12` and
`CMAKE_BUILD_PARALLEL_LEVEL=12`), built the canonical checkout target, proved
that this no-NVIDIA host must use the CPU build, exercised a candidate on an
isolated real vault, gracefully drained the prior daemon, and completed all ten
phases in 984.388 seconds.

Independent reads after setup, rather than its return value, proved:

- Windows PID `8472` runs
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`;
- installed executable SHA-256 is
  `41EAF21CB6ADC26AC1F294D5697D63FA9894553AFBB73FEEAA693A3CEFF68391`;
- the served release build is exact clean commit
  `a372763420e0d0c4accffd4e05b1b896bd11bda5` from `refs/heads/main`;
- its 1,451-file build-input manifest SHA-256 is
  `629287fd69d4a5b1320ea36908e8fcabd07c3de6ffa37cd06ac02e8c920434ae`;
- a fresh MCP session initialized, listed all 40 real tools, and executed
  `routine operation=reflex_list`, which initialized the production reflex
  runtime through the public surface;
- a separate full health call reported reflex status `ok`, active count `0`,
  audit timestamp failures `0`, and terminal pending/prepared/committed/failed
  `0/0/0/0`;
- the same vault id `01KYJPGWATPD4XNMZY3ERGTKQW` was open at generation 1;
- Calyx selected AVX2, its fixed dot/cosine/L2/top-k probes matched the portable
  path bit-for-bit, and Windows execution-speed throttling was disabled.

Finally, a real public `audit operation=verify_chain` re-read and re-hashed the
production physical store:

- verdict `intact`, entries/head `342986/342986`, verified `[0..342986)`;
- raw commitments intact: `965212` total, `965209` sealed, three current
  checkpoint-tail rows pending, and `39590` cohort seals;
- tip hash
  `f84667dd52e16592f2a0987cd0e53f92a5c7939e2bb138be9b6c1ed7ee6be432`;
- vault generation `1`, reset count `0`, with the lineage-seeded origin and its
  historical coverage limitation reported truthfully.
