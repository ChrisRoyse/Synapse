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

Research basis: caret defines near-zero variance using both frequency ratio and
percent unique; NIST's binomial rule of three shows why zero observations in a
bounded sample do not establish population absence.

## Sources of truth

1. Live typed source CF rows through `storage operation=corpus_histogram`.
2. Physical Calyx Base and slot CFs in the consistent backup at
   `%LOCALAPPDATA%\synapse\fsv\issues-1981-1982-final-20260803T1815Z\vault`.
3. The backup's `CURRENT` and `manifest-*.json` bytes for non-mutation proof.

## Independent source census

`CF_AGENT_EVENTS`: 9,084/9,084 decoded, zero failures, complete. `end_state` was
8,165 absent, 914 indeterminate, 3 success, 2 error.

`CF_AGENT_TRANSCRIPTS`: 50,973/50,973 decoded, zero failures, complete. `status`
was 50,964 parsed and 9 invalid. Frequency ratio is 5,662.67 and percent unique
is 0.00392365%, so it is near-zero variance, not constant. The invalid stratum is
operationally critical and remains protected from automatic parking.

## Physical vector readback

`cargo run -p synapse-calyx --example lane_distribution_fsv -- <vault>` read the
frozen physical vault at sequence 377,284. Panel 1665001 was a complete Base
census (9,073/9,073); panel 1921001 was explicitly sample-qualified
(20,000/101,946 Base revisions). Slot 36 reported
`CALYX_LENS_CONSTANT_BY_SAMPLE`, never `BY_CORPUS`. Every finding printed its
counts, ratio, percent unique, and census flag.

Slot 30 was present on 916 rows but had one vector value despite the independent
source census proving three non-absent categories. This cross-path disagreement
physically proves the eight-bucket hash collision; it is not source constancy.

## Boundary audit

1. Minimum: requested one row. Before: Base populations 9,073 and 101,946.
   After: one row observed from each, zero distribution findings, PASS.
2. Invalid format: `not-a-number`. Before and after `CURRENT` SHA-256 was
   `C83C6DAA39208D9A0733A5F56ADB5481A8863CA7BD7F12E03C03E29121A70309`;
   command exited 1 with `InvalidDigit`.
3. Invalid path: `<vault>\missing`. Before absent; after absent; command exited 1
   naming the exact non-directory path.
4. Maximum bounded operational read: requested 100,000. The intelligence safety
   ceiling bounded hydration at 20,000 and the verdict remained explicitly
   sample-qualified against the 101,946 Base-row population.

All `manifest-*.json` hashes matched before and after the read-only audit; the
last manifest remained
`E3854E252658C70A5CC1DA80104F093A27E8177468028193448E5DA8964E74EE`.
