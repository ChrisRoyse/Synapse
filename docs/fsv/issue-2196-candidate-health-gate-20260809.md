# Issue #2196 FSV — fail-closed candidate health and provenance

Date: 2026-08-09 America/Chicago (UTC evidence crossed into 2026-08-10)

## Defect and root cause

`Test-SynapseCandidateDaemon` accepted every `health.ok=false` response after
checking only that the candidate PID matched and `tools/list` was non-empty. The
tolerance was introduced for the startup classification defect fixed by #1914,
but remained after that defect was removed. It therefore treated transport
reachability as semantic fitness and could replace the live daemon with a
candidate that explicitly reported an error.

The isolated candidate also had one real initialization requirement: its newly
created vault had an active panel but no search-generation manifest. Suppressing
that error would recreate the defect. The correct preparation is to build the
generation through the same audited public route used in production, then read
health and disk again.

## Independent research after diagnosis

Lane verification: `pwsh -File scripts/check-research-lane.ps1` reported Exa MCP
`live` (`exa-search-server` 3.4.0; real `tools/call` succeeded). Exa and the
built-in web lane were both used.

- Kubernetes distinguishes startup, readiness, and liveness; a failed readiness
  result does not receive traffic, while initialization gets its own startup
  state: <https://kubernetes.io/docs/concepts/workloads/pods/probes/>.
- Azure deployment gates do not proceed unless every gate succeeds in the same
  sampling interval before the deadline:
  <https://learn.microsoft.com/en-us/azure/devops/pipelines/release/approvals/gates?view=azure-devops>.
- Google Cloud deployment verification makes the rollout fail when verification
  fails: <https://docs.cloud.google.com/deploy/docs/verify-deployment>.

Applied rule: initialize an intentionally new isolated source of truth through
the real production operation, then require a fresh successful semantic-health
read. Do not reinterpret or warn past an error.

## Implementation

- Candidate health is serialized to a scoped evidence file, read back, and
  SHA-256 hashed before any acceptance decision.
- `build_provenance` must be `ok`, `clean`, match the named checkout, and report
  zero changed/omitted inputs. Known dirty provenance remains diagnostically
  `degraded` in general health but is unacceptable at the deployment boundary.
- The exact isolated `calyx_search_generation=error/state=absent` condition is
  initialized through public `storage operation=search_rebuild` only after the
  same MCP session acquires and reads back the foreground lease and enters the
  audited `break_glass` profile.
- Setup independently hashes the published manifest from disk, reads health
  again, restores `normal_agent`, releases and reads back the lease, and then
  requires `health.ok=true`.
- Any other error produces `SYNAPSE_CANDIDATE_HEALTH_UNHEALTHY` with every error
  subsystem/detail plus retained stdout, stderr, health, vault, and lifecycle
  evidence. Candidate cleanup happens before the exception escapes.

## Sources of Truth

1. Production: Task Scheduler supervisor JSON, OS process table/listener table,
   installed executable bytes, authenticated `/health`, vault identity and
   durable sequence.
2. Candidate: isolated vault files, search manifest bytes, authenticated health
   snapshots, exact candidate PID/listener, and `candidate-diagnostic.json`.
3. Source: Git `HEAD`, `.git` presence, working-tree status, and the immutable
   build-input attestation returned by the running candidate.

No return value below was accepted without a separate read of these stores.

## Happy path — clean candidate and real empty-vault bootstrap

Before trigger:

```text
live pid=16056 supervisor=19856 bind=127.0.0.1:7700
installed sha256=E9BB59C23D1BB0CC348EB857DF4BE37FAB5EA5F1FD1B2F1835FFE3E63C66E5B8
health pid=16056 build=84b6acbf8617 ok=true
vault_id=01KYJPGWATPD4XNMZY3ERGTKQW latest_seq=1305485
```

Trigger: canonical setup with `-SkipBuild -ForceRestart -SkipClientWiring`,
audio enabled, and the deployed six-permission set.

Candidate evidence:

```text
initial health sha256=CBEAD5DBA5C6E76FB805ED7D50B7AAD3E41F053E626BD0847EBE116B88CDBA0B
initial health ok=false; sole error=calyx_search_generation state=absent
lease owner session=815742da-0324-4dc8-a1f1-0b12d56add8d outcome=acquired
profile=break_glass (audited reason present)
panel_version=1963001
manifest sha256=06C4052DCFD4BA6A3D91CEBFFE1A6D39375623F14F26D4BE53C5DE5D9BCE2CF5
post-bootstrap health sha256=E5CB66D356FA1D4F79D15EB0738A2E61B18518DA107D3A186D3872436C2BA0BE
post-bootstrap health ok=true
lease outcome=released held=false; profile restored=normal_agent
candidate pid=21600 stopped; bind 59125 released; isolated directory removed
tool_count=40 surface=d1f09d4a25e3a9f6b623c62d1b46b58293c45804bc129bc91ed03b15cce90683
```

Independent production read after handoff:

```text
live pid=22012 supervisor=16204
installed sha256=E9BB59C23D1BB0CC348EB857DF4BE37FAB5EA5F1FD1B2F1835FFE3E63C66E5B8
health pid=22012 ok=true
vault_id=01KYJPGWATPD4XNMZY3ERGTKQW latest_seq=1305530
```

The PID changed, the installed bytes and vault identity did not, and the durable
sequence advanced. This proves both handoff and preservation rather than relying
on setup's exit code.

## Edge 1 — required storage authority absent

Before: live PID 22012, same installed SHA-256, health `ok=true`, vault sequence
1305530. Trigger omitted only `WRITE_STORAGE` from the candidate permission set.

```text
candidate initial health sha256=8E11CD65138B4D67F224B8BC49BCB48F9956AAEC1FDF555F3E146F1DB579D19D
sole initial error=calyx_search_generation; state=absent
real tool error=SAFETY_PERMISSION_DENIED missing_permission=WRITE_STORAGE tool=storage
candidate pid=22116 process_has_exited=true OS process absent
candidate bind=127.0.0.1:64034 listener_count=0
diagnostic sha256=03663FC29799AD98362F9F408BD621CD4B117FE15BA4FF95DCE925C7C1878AA7
```

After: production PID and installed hash were unchanged, health was true, vault
ID was unchanged, and sequence was 1305534. No handoff occurred.

## Edge 2 — real dirty release input

Before: production PID 22012, installed clean SHA-256 above, health true, vault
sequence 1305534. Trigger used the canonical 12-job release build while the
implementation file was the only changed source input.

```text
candidate pid=11504 bind=127.0.0.1:49917
candidate image sha256=BEBB437469A1B27FD472D2021B77E8EF16EA17EC3DF42C16817F2EE04F618E3B
build_provenance status=degraded tree_state=dirty matches_checkout=false
changed_count=1 omitted=0
path=scripts/synapse-setup.ps1 status=" M" kind=tracked length=853511
input sha256=df368521f481a7081e2e856323965ce2dac2302d9a36af79da1ee9b31f44b090
failure=SYNAPSE_CANDIDATE_BUILD_PROVENANCE_UNACCEPTABLE
diagnostic sha256=BE75191FDE416BC25DF207BFA8C3A5C6DA783FD34A5124D6014E14FAAA13BB3A
candidate process_has_exited=true OS process absent listener_count=0
```

Independent after-read: production remained PID 22012 on the original installed
SHA-256 and build `84b6acbf8617`; health stayed true, vault ID stayed identical,
and durable sequence was 1305561. The 523,400,985-byte staging bundle was also
removed with `readback_exists=false`.

## Edge 3 — checkout becomes unreadable

The exact `C:\code\synapse\.git` directory was moved to one verified sibling
path, setup was invoked with the clean installed candidate, and restoration was
guaranteed by `finally`. This exercises a real runtime provenance error without
forging a binary or response.

Before: live PID 22012, health/provenance `true/ok`, matches checkout true,
installed SHA-256 unchanged, vault sequence 1305561.

```text
candidate pid=10444 bind=127.0.0.1:63177
candidate health sha256=03000478D45098F2F43D553130C5AE9C66AB0C380A6439AA183C92B767E4BA1A
build_provenance status=error
checkout reason=C:\code\synapse is not a git checkout on this machine
build_matches_checkout=<absent> (unknown was not collapsed to false)
failure=SYNAPSE_CANDIDATE_BUILD_PROVENANCE_UNACCEPTABLE
diagnostic sha256=364033A755DEF6461F1BC728A3D4388CF095F6BCDFE0954D6FD2DA3D232CBDE8
candidate process_has_exited=true OS process absent listener_count=0
```

After restoration: `.git` existed, the temporary sibling did not, `HEAD` was
`84b6acbf86173e74083b022868c826c569a7080f`, production remained PID 22012 on
the original executable and vault, and an independent health read returned
`ok=true`, provenance `ok`, and `build_matches_checkout=true`.

## Compile and lint

```text
cargo check --workspace -> PASS (dev profile completed in 62s)
pwsh -File scripts/lint.ps1 -> PASS
  root cargo fmt/clippy/deny: PASS
  calyx cargo fmt/clippy/deny: PASS
  zero-test doctrine and public API ratchet: PASS
```

## Acceptance rule for the committed production image

The installer intentionally rejects a dirty build, so the final shipping image
can only be produced after this file and the implementation are committed. The
post-commit setup run must again prove: clean provenance for that exact commit,
manifest hash agreement, health `ok=true`, installed bytes equal the canonical
release bytes, live PID/image identity, unchanged vault ID with non-regressing
sequence, and no candidate/staging residue. Exact final commit/image/PID values
are recorded in #2196's closing evidence so this tracked FSV record does not move
`HEAD` after the image it describes is built.
