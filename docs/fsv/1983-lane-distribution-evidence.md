# Issue #1983: lane distribution evidence

Date: 2026-08-03. Research lanes: Exa MCP (`live`, v3.4.0) and built-in web.

## Root cause

The five-minute structural coverage pass hydrates at most 256 records per panel,
then reused those rows for a distributional verdict named `BY_CORPUS`. Exact
equality was the only distribution test and the verdict carried no prevalence
evidence. In addition, slot 30 is a hashed eight-bucket encoder: the three
non-absent `end_state` values in the physical corpus project to the same vector.

The fix qualifies every finding as sample or census, reports observed and
population rows, distinct values, frequency ratio, percent unique, and uses a
separate near-zero-variance code. No distribution finding authorizes a lifecycle
change until a grounded stratified assay has ruled out a rare critical carrier.

The encoding root cause is also removed. Declared finite categories now use
`syn_one_hot_index:<levels>`, which maps each validated integer index to its own
axis without hashing. Agent-event panel 1983001 carries the three-valued end
state in slot 30 and an independent two-valued presence signal in slot 114.
Transcript panel 1983002 carries the two declared parse statuses in slot 36.
Empty, negative, out-of-range, fractional, and non-numeric values fail with
`CALYX_LENS_NUMERICAL_INVARIANT`; none is coerced or mapped to an overflow bin.

Research basis: caret defines near-zero variance using both frequency ratio and
percent unique; NIST's binomial rule of three shows why zero observations in a
bounded sample do not establish population absence.

## Sources of truth

1. Live typed source CF rows through `storage operation=corpus_histogram`.
2. Physical Calyx Base and slot CFs in the consistent backup at
   `%LOCALAPPDATA%\synapse\fsv\issue-1965-agent-transcript-20260803T2115Z\vault`.
3. The backup's `CURRENT` and `manifest-*.json` bytes for non-mutation proof.
4. Live panel coverage and registry CF readbacks after backfill completion.

## Independent source census

`CF_AGENT_EVENTS`: 9,084/9,084 decoded, zero failures, complete. `end_state` was
8,165 absent, 914 indeterminate, 3 success, 2 error.

`CF_AGENT_TRANSCRIPTS`: 50,973/50,973 decoded, zero failures, complete. `status`
was 50,964 parsed and 9 invalid. Frequency ratio is 5,662.67 and percent unique
is 0.00392365%, so it is near-zero variance, not constant. The invalid stratum is
operationally critical and remains protected from automatic parking.

## Physical vector readback

`cargo run -p synapse-calyx --example lane_distribution_fsv -- <vault>` read the
frozen physical vault at sequence 338,456. Panel 1665001 was a complete Base
census (9,085/9,085); panel 1921001 was explicitly sample-qualified
(20,000/50,973 active Base records). Slot 36 reported
`CALYX_LENS_CONSTANT_BY_SAMPLE`, never `BY_CORPUS`. Every finding printed its
counts, ratio, percent unique, and census flag.

Slot 30 was present on 920 rows but had one vector value despite the independent
source census proving three non-absent categories. This cross-path disagreement
physically proves the eight-bucket hash collision; it is not source constancy.

The post-fix backup read at durable sequence 338,446 has 7,252 files and
1,586,225,754 bytes. Backup verification reported an intact ledger chain,
manifest SHA-256
`8e932c438c5c8f2270724367a49c7e3c71a709bfdd3ca674a2eedc5c83f84ba8`,
and ledger tip
`e26471f74aacf1cfd22ba54e61c2f40b11193ecf0377897ad755414230f8aada`.
Its complete panel-1983001 census measured 9,090/9,090 Base records: slot 30
contained 921 values and three distinct vectors. It therefore reported
`CALYX_LENS_NEAR_ZERO_VARIANCE_BY_CORPUS`, frequency ratio 305.333, percent
unique 0.3257%, and `lifecycle_action_allowed=false`. The former panel had one
distinct vector for the same three source categories.

The original #1983 backup above was later superseded to avoid retaining two
multi-gigabyte copies. The replacement backup independently passed restore
verification with an intact 296,193-entry ledger and re-ran this same probe at
sequence 463,273. It reproduced the old panel's one vector across 920 present
rows and measured panel 1983001 exactly: 9,093/9,093 Base rows, 922 slot-30
values, three distinct vectors, frequency ratio 305.667, percent unique 0.32538,
and `lifecycle_action_allowed=false`. Panel 1921001 remained explicitly
`CALYX_LENS_CONSTANT_BY_SAMPLE` at 20,000/50,973.

## Live migration readback

The installed daemon health endpoint independently reported build
`cd78ac90cc71...` running from `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`.
The Registry CF contains panel 1983001 at sequence 315,196 and panel 1983002 at
sequence 315,197. After the real `storage temporal_backfill` trigger completed:

- agent events: 9,091 active/current records out of 9,091 source rows,
  coverage 1.0, zero uncovered rows, and `backfill_owed=false`;
- transcripts: 50,973 active/current records out of 50,973 source rows,
  coverage 1.0, zero uncovered rows, and `backfill_owed=false`.

The transcript source CF census still independently reads 50,964 parsed and 9
invalid rows. The deterministic runtime probe read the produced bytes as
`0 -> [1,0]` and `1 -> [0,1]`; combined with complete current-panel coverage,
this establishes that both physical source strata are encoded without a hash
collision. The backup predates completion of that transcript backfill, so it is
not mislabeled as a complete transcript vector census.

`storage intelligence abundance` independently read panel 1983001 from the Base
and slot CFs and found both slots 30 and 114 dense and measurable. These are
separate signals: end-state presence no longer depends on the distribution of
the end-state category lane.

## Boundary audit

1. Minimum: requested one row. Before: Base populations 9,085, 50,973, and
   9,090. After: one row observed from each, zero distribution findings, PASS.
2. Invalid format: `not-a-number`. Before and after `CURRENT` SHA-256 was
   `8967FCBABD90B9CF1A9AD4D69993DEAE3C68401B3174B4DC2F4618A30F78A3B8`;
   command exited 1 with `InvalidDigit`.
3. Invalid path: `<vault>\missing`. Before absent; after absent; command exited 1
   naming the exact non-directory path.
4. Maximum bounded operational read: requested 100,000. The intelligence safety
   ceiling bounded transcript hydration at 20,000 and the verdict remained
   explicitly sample-qualified against the 50,973-record population, while the
   two smaller panels were exact censuses.

All `manifest-*.json` hashes matched before and after the read-only audit; the
last manifest remained
`CDF4B59BF66B6B658A131D63E4E7D2D6711BA2C1ACD10D4717E9E8C5FC42C740`.
