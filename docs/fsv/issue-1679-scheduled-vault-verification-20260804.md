# Issue #1679 scheduled vault verification FSV (2026-08-04)

## Source of Truth

- Disposable physical vault: `C:\Users\hotra\AppData\Local\Temp\synapse-vault-verify-fsv-1785851350041\vault`
- Surviving lineage journal: sibling `vault.lineage.json`
- Runtime evidence: `daemon-fixed.out.log` and `daemon-fixed.err.log` in the same FSV directory
- Trigger: repo-built `target\debug\synapse-mcp.exe --mode http`, PID 3848, loopback port 7791

The verifier independently scans SST/WAL bytes, the physical Ledger CF hash
chain, raw-commitment seals, and the lineage journal. The daemon log is evidence
of the scheduled trigger; the vault files and lineage readback are the state.

## Research

`scripts/check-research-lane.ps1` issued a real Exa MCP query and reported
`exa_mcp live` (server 3.4.0, 8,180 returned characters). Built-in research used
NIST SP 800-92 for robust log-management processes, NIST SP 800-88 for verified
sanitization, and Microsoft Task Scheduler/background-job guidance for delayed,
power-conscious, idempotent periodic integrity work. The implementation uses
the existing daemon lifecycle rather than adding an OS task.

## Before, trigger, after

1. Invalid vault-path pairing: before there was no process or vault state;
   starting with different `--db` and `--calyx-vault-dir` paths exited with
   `SYNAPSE_CALYX_VAULT_PATH_CONFLICT`. After: no daemon remained.
2. Negative control: the first scheduled scan over a real fresh vault verified
   chain `[0..10)`, raw commitments, and lineage, but alarmed solely because
   `anchor_count=0`. This diagnosed #2000: restore validity incorrectly depended
   on corpus population.
3. After the root fix, the same vault and scheduler produced repeated
   `VAULT_VERIFY_PERIODIC_OK` events. The first physical verdict was vault ID
   `01KZ6GJPP669XY2SQ8Q89QWD7F`, range `[0..12)`, head 12, tip
   `f555f4727f9202a2be77a80a80570691acd49b40fcb5e0a65112eea26c288af2`.
   A later independent pass observed `[0..14)`, head 14, tip
   `22b1c2da92672e5945d6d067121d8af45bf4e517a2d9c544ff194967389c768f`.
4. Direct filesystem read found 72 vault files totaling 157,131 bytes. The
   sibling lineage JSON independently named the same vault ID, generation 1,
   and a durable high-water sequence of 28.

## Boundary audit

- Fresh/empty corpus: zero Base/Anchor rows now remains observable but does not
  imply corrupt storage. Chain, WAL, and lineage integrity remain mandatory.
- Invalid cadence: `SYNAPSE_VAULT_VERIFY_INTERVAL_SECS=banana` exited 1 during
  HTTP startup, naming the variable/value, unsigned-integer contract, parse
  cause, and `vault_verifier` phase. No listener was left serving.
- Path mismatch: divergent sole-store/vault paths failed closed before bind.
- Shutdown: the valid verifier was owned by the HTTP background-task collection;
  stopping PID 3848 released the executable and vault before the rebuild/edge run.

No production vault row was modified or erased during this verification.
