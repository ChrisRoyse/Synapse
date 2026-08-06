# Issue #2014: coherent live Ledger verification

Date: 2026-08-05/06 (America/Chicago)

## Diagnosis

`AsterVault::verify_ledger_chain` read the filesystem head projection, the Ledger
CF, and raw commitments through separate live reads. A concurrent commit could
publish head `N` after the Ledger scan had pinned `N-1`; verification then
misreported the absent future row as physical corruption. Re-reading the exact
reported sequence showed it present, and successive failures moved with the
head (`326462`, then `326466`). This was a snapshot race, not data loss.

The fix acquires `durable.commit.lock`, pins one MVCC sequence, scans Ledger at
that sequence, and reads the matching head before releasing the lock. Ledger
verification and raw-commitment verification consume that same snapshot.

Boundary FSV also found #2015: a requested exclusive end beyond the pinned head
was misclassified as corruption. The vault now rejects bounds outside
`0 <= start <= end <= head` with
`CALYX_ASTER_LEDGER_VERIFY_RANGE_INVALID`; the MCP facade preserves the
substrate remediation.

## Research

Exa MCP was probed after diagnosis and was `live`; its real search call
succeeded. The built-in web lane was also used. Rust defines `Range` as
half-open (`start <= x < end`), making the durable head the maximum valid
exclusive end. OWASP recommends early syntactic and semantic validation,
including application-specific bounds.

- https://doc.rust-lang.org/std/ops/struct.Range.html
- https://cheatsheetseries.owasp.org/cheatsheets/Input_Validation_Cheat_Sheet.html

## Source of truth

- `%LOCALAPPDATA%\synapse\db-daemon` Ledger and RawCommitment column families
- durable Ledger head projection protected by `locks\durable.commit.lock`
- installed `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
- OS process table and TCP listener

Final installed daemon: PID `21984`, listener `127.0.0.1:7700`, SHA-256
`167BDCC01B774D567F780A465F8DFA7F5A099163B34BC61FC620ADC2062A6593`.

## Execute and inspect

The original reported gap was read independently through `audit verify_chain`
with `[326461..326463)` plus `read_seq=326462`:

```text
present=true self_verifies=true seq=326462
entry_hash=16e1dcf7beb47e6ea35bb37b1ec585da8cc5da41755ae0000c6442fff61cee10
payload_sha256=5c54882ae6947c912864c56bf7b742b56df75997f955bae557fa7f366663d794
verdict=intact raw_commitments_intact=true
```

Ten fresh MCP sessions then performed full physical chain walks while their own
usage writes advanced the vault. Heads were `326880, 326882, 326884, 326886,
326888, 326890, 326892, 326894, 326896, 326899`. Every observation had
`entry_count == head_height`, `verdict=intact`, and
`raw_commitments_intact=true`. Final tip:
`bbbaf4ec61f8eaec181e489897ca75762847d50319216e9c1a36cc15b31f82b5`.

## Boundary state audit

1. Future end: before `head=326870, intact`; trigger
   `[326620..999999999)` failed with
   `CALYX_ASTER_LEDGER_VERIFY_RANGE_INVALID` and the exact half-open-range
   remediation; after `head=326878, intact`, raw commitments intact.
2. Reversed range: `[100..99)` failed with
   `SYNAPSE_CALYX_LEDGER_VERIFY_RANGE_INVALID` and instructed
   `from_seq <= to_seq`; the subsequent full chain remained intact.
3. Empty maximum range: `[326870..326870)` returned `entry_count=0,
   verdict=intact`; independent `read_seq=326462` remained present and
   self-verifying; the subsequent full chain remained intact.

Supporting gates: `cargo check --workspace` and
`pwsh -File scripts/lint.ps1` passed in both workspaces. Commits:
`7db4c316`, `4fe9a5d3`, `a2fb95bc`.
