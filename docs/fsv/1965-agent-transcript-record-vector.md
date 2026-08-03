# Issue #1965: agent record-vector migration tranche

Date: 2026-08-03. Commit: `829a1368`. Research lanes: Exa MCP (live,
`exa-search-server` v3.4.0, real query succeeded) and built-in web.

## Root cause and research

`RECORD_VECTOR_MAGNITUDE_GRANDFATHERED` used moving current-version constants.
The #1983 panel bumps therefore silently grandfathered two new generations and
propagated the record vectors #1964 had already measured as constant. The gate's
stated invariant, "no new generation can use this encoder", was false.

The exemptions now contain literal historical versions. Agent-event generation
1965001 omits the constant slot 34. Transcript generation 1965002 replaces raw-
magnitude slot 47 with slot 110, a `syn_record_vector_unit_fields` lens over
log-bounded counts, bytes, tokens and cost plus periodic day/week position. The
absolute timestamp is absent. Every component is in `[0,1]` and the encoder
refuses an out-of-scale component.

Research basis:

- scikit-learn's `VarianceThreshold` removes zero-variance features by default:
  <https://scikit-learn.org/stable/modules/feature_selection.html#removing-features-with-low-variance>
- Microsoft documents migrations as explicit incremental model/schema changes
  that preserve existing data, and recommends inspecting and validating the
  migration before production application:
  <https://learn.microsoft.com/en-us/ef/core/managing-schemas/migrations/>
- MLflow registers each changed model as a distinct immutable model version and
  uses a mutable alias to select the deployed version:
  <https://mlflow.org/docs/latest/ml/model-registry/workflow/>

The inference applied here is: preserve old slot bytes under their historical
panel versions, publish a new panel version, and omit a measured constant feature
rather than mutate or rename it in place.

## Candidate source-of-truth probe

`transcript_record_vector_fsv` opened the verified #1983 vault and decoded all
50,973 authoritative `CF_AGENT_TRANSCRIPTS` rows. Every row measured through the
production builder with zero decode or measurement failures. A deterministic
2,999-vector stride sample produced:

```text
nearest_neighbor_cosine distinct=1336 modal_share=0.006669
range=[0.879258,1.000000]
```

The superseded stored vector had one distinct nearest-neighbour value and modal
share 1.0. The candidate therefore graded the real corpus before publication.

## Live trigger and independent coverage readback

Setup installed `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`; `/health` separately
reported PID 18448, build `829a1368b14c`, clean `main`, release profile, and
`build_matches_checkout=true`.

Real `storage temporal_backfill` calls produced:

- agent events: 10 pages, 9,097 examined, 9,093 inserted, 4 already current;
- transcripts: 51 pages, 50,973 examined and inserted.

A separate panel-coverage census after the triggers read:

- panel 1965001: 9,097/9,097, coverage 1.0, no debt, zero uncovered;
- panel 1965002: 50,973/50,973, coverage 1.0, no debt, zero uncovered;
- transcript anchors: 13,250 grounded records, with 140
  `synapse:agent_end_state` and 13,137 `synapse:agent_tool_call_success` records;
- every transcript superseded generation was closed and zero transcript anchors
  remained stranded.

`storage intelligence abundance` independently read Base/XTerm/Graph CF state.
Panel 1965001 contained 12 slots and no slot 34. Panel 1965002 contained slot
110 as dense and measurable.

## Physical bytes

The completed consistent backup is
`%LOCALAPPDATA%\synapse\fsv\issue-1965-agent-transcript-20260803T2115Z\vault`.
The daemon's completion event recorded 48,671 copied files and 2,739,388,091
bytes at durable sequence 463,177. Independent `restore_verify` read 234,859
constellations, 50,540 anchors and 296,193 ledger entries; the chain was intact
at tip `ac88c30992155a53689a3a306ce471bfb82f5e25b11bcbe441b77d4b098f90dd`.

The physical-only FSV reopened Base and slot 110 CFs from that backup:

```text
agent panel=1965001 base_rows=9098 rows_declaring_retired_slot_34=0
transcript panel=1965002 base_rows=50973 rows_declaring_slot_110=50973
slot_110_cf_rows=50973
stored slot 110: sampled=2999 distinct=1298 modal_share=0.009003
range=[0.895882,1.000000]
```

The extra agent row arrived after the live coverage census and before the pinned
backup; it was written directly at the current panel and also omitted slot 34.

## Backup timeout finding

The public backup call exceeded the client's 300-second deadline while the
server continued copying. An immediate verifier raced the worker and correctly
reported a head mismatch (296179 vs 296193). Daemon logs later recorded internal
completion and successful verification; the independent retry then passed with
the counts above. The failed intermediate observation is retained as evidence,
not presented as corruption or success. Publication/lifetime remediation is
tracked in #1987.

## Boundary audit

Before all three read-only edges, backup `CURRENT` SHA-256 was
`D14AE29E49466D305E1305432C3F763E481E0D47AD3F5F34C00D43354CE1CBCC`
and pinned manifest `manifest-00000000000000030329.json` SHA-256 was
`346216B87D89C9D42BD1EF6E3C6FDDDEF1BC7BBF743C9D715E8062E64458D829`.

1. Empty invocation exited 1 with the exact usage string.
2. A missing vault path was absent before, exited 1 naming that path, and
   remained absent afterward.
3. Unknown mode `--unknown` exited 1 and named the only accepted mode.

Afterward both hashes were byte-identical to their before values. The maximum
operational boundary was the complete 50,973-row source measurement and complete
50,973-row stored-slot census above, not a synthetic oversized corpus.

The repository's only lint gate, `scripts/lint.ps1`, passed all seven gates in
both workspaces before deployment. The final FSV-only reader extension is linted
again in the evidence commit.
