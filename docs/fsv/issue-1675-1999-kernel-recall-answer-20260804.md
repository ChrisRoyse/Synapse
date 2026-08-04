# FSV: grounding kernel recall, persistence, answer path, and schedule (#1675, #1999)

Date: 2026-08-04. Host: the configured Windows end-user system. Branch: `main`.

## Diagnosis and research

`build_domain_kernel_inputs` measured kernel-only recall into a local report but
persisted the original `Kernel`, whose `recall` remained the pipeline default.
It also stopped after measuring a too-small MFVS kernel instead of using the
existing exact-top-k recall-support refinement. Consequently health could read a
different recall from the artifact than the build gate had enforced.

The Exa MCP lane was checked immediately before research and was `live`
(`exa-search-server` 3.4.0, real `web_search_exa` call succeeded). Exa and the
built-in web lane were both used. Faiss documents the relevant discipline:
measure approximate recall against an exact flat-index ground truth and tune the
candidate/search population to the required recall target:

- https://github.com/facebookresearch/faiss/wiki/Indexing-1M-vectors
- https://github.com/facebookresearch/faiss/wiki/Guidelines-to-choose-an-index

The fix applies that rule directly: measure against exact full-corpus top-k,
add the missing support records when the initial kernel misses the gate,
remeasure, copy the final recall into the persisted artifact, and seal the
completed identity from the final physical contract. Scheduled construction is
bounded to 2,000 records and a 24-hour cadence inside the existing blocking
derived-state maintainer; all three requested panels are independently attempted.

## Source of truth and known-answer FSV

Source of truth: native Calyx `Base`, `slot_1`, and `Kernel` CF bytes in a new
vault. Trigger:

```text
cargo run -p synapse-calyx --example kernel_answer_physical_fsv -- %TEMP%\synapse-fsv-1675
```

Independent read-only reopen after checkpoint reported:

```text
BEFORE Base=0 Kernel=0
BUILT corpus=120 members=77 recall=1 gate=0.95 anchored=77 Kernel_rows=2
ANSWER query=16750000000000000000000000000000 hops=1 grounded=true recall=1
HOP 0 physical_weight=0.99951607 physical_score=0.99951607 MATCH
READBACK snapshot=123 Base=120 Kernel=2 persisted_recall=1 matches=true
PASS
```

The reader separately proved both hop CxIds existed in `Base`, decoded both
vectors from physical `slot_1` rows, recomputed cosine and `weight * 0.9^hop`,
and matched the returned total.

## Boundary audit

All refusals printed filesystem state before and after:

```text
missing argument: exit=1, usage error, filesystem unchanged
existing empty directory: entries 0 -> 0, SYNAPSE_FSV_SCRATCH_CREATE_FAILED
existing sentinel file: SHA256 2B7847...543 unchanged, exit=1
```

The live API also refused an out-of-range 600,000 ms control lease with
`LEASE_TTL_OUT_OF_RANGE`, and the timeline/agent-event scheduled targets refused
with `SYNAPSE_CALYX_KERNEL_NO_DOMAIN` because their current panels contain no
grounded outcome domain. Neither refusal wrote a fake kernel.

## Live daemon FSV

Setup installed the repo-built release binary at
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`; PID 4592 owned the
`127.0.0.1:7700` listener and authenticated `/health` returned `ok=true`.
Through a real MCP session and audited break-glass lease:

```text
hygiene kernel_rebuild panel=1964001 slot=113 max_records=256
  corpus=171 anchored=171 members=112 recall=1.0 gate=0.95
  kernel_id=fae8c1b4e11cbaa08ee3f63268f9e015 Kernel_rows=3

independent hygiene kernel read
  artifact_bytes=11974 recall=1.0 pass_mode=passed trust=anchored
  grounded_fraction=1.0 unanchored=0 n_queries_tested=18

storage intelligence kernel_answer (real tools/call)
  grounded=true recall=1.0 total_score=1.0
  query and anchor CxId=0164248e9eb246703c8a8b77e34fa5dc
```

The profile was restored to `normal_agent`; lease readback was
`was_held=true released=true`.

At the first five-minute derived-state tick, health independently reported
`attempts=1` and the scheduled kernel failure code naming the two unanchored
panels. A subsequent physical artifact read showed the episode artifact's
`built_at_millis` advanced to `1785837048617`, retained recall 1.0, and remained
anchored; `Kernel` held four rows. This proves the scheduler continued past the
first refusal, persisted the eligible target, and surfaced the ineligible ones.

## Gates

`cargo check --workspace` passed. `scripts/lint.ps1` passed all seven gates in
both workspaces, including format, deny, Clippy all-targets, and public-API
ratchet.
