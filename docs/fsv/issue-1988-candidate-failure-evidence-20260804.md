# Issue #1988 - candidate failure evidence survives setup

Date: 2026-08-04

## Source of truth

The source of truth is the retained candidate directory under
`%LOCALAPPDATA%\synapse\logs\setup-candidates`, especially its independently
read `candidate-diagnostic.json`, redirected stdout/stderr, and isolated daemon
lifecycle ledger. The setup exception is not acceptance evidence.

## Diagnosis and research

`Test-SynapseCandidateDaemon` started the child without redirected streams,
never refreshed or read its process exit metadata, and unconditionally removed
the isolated candidate root in `finally`. The reported process-dead state could
therefore not distinguish structured startup refusal, panic, Windows exception,
or external termination.

Research was performed after diagnosis. Exa MCP was proved live by
`scripts/check-research-lane.ps1` and queried for PowerShell process stream,
exit-code, and diagnostic-retention practices. The built-in research lane read
the primary Microsoft documentation:

- `Start-Process` supports `-PassThru`, `-RedirectStandardOutput`, and
  `-RedirectStandardError`:
  <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.management/start-process>.
- A retained `System.Diagnostics.Process` handle preserves exit code and exit
  time after termination, and `WaitForExit()` completes redirected output event
  processing:
  <https://learn.microsoft.com/en-us/dotnet/api/system.diagnostics.process.hasexited>.
- Windows Error Reporting likewise treats error codes, application logs, and
  bounded diagnostic artifacts as the inputs required to classify failures:
  <https://learn.microsoft.com/en-us/windows/win32/wer/using-wer>.

Commit `f5cda5da` redirects both streams, reads signed and hexadecimal exit
status after `WaitForExit()`, inventories every retained file by relative path,
length, and SHA-256, writes a versioned diagnostic manifest, and retains the
isolated lifecycle bytes on failure. Successful candidates are still deleted.
Retention keeps the newest five directories only when the directory has the
strict candidate name, is not a reparse point, and contains a diagnostic
manifest. Unverified directories are ineligible for deletion.

## Happy path

The real installed packaged daemon was run through setup with `-SkipBuild` and
the normal permission set. Candidate PID 14256 answered authenticated health,
exposed 40 tools with surface SHA-256
`42cf32e464c1492594589f4fdfa3d7a0c1a6309fcf043e88b0cdd4cc00d1802e`,
and shut down gracefully. Independent filesystem readback proved its exact
directory
`candidate-20260804T000953229Z-7668` was absent afterward. The prior failure
bundle remained, and the live daemon remained PID 2272 at the installed path.
Setup then reached its expected current-Codex schema-stale handoff rather than a
candidate error.

## Failure path

The real packaged daemon was run with the deliberately invalid startup grant
`NOT_A_REAL_PERMISSION`. This is a real configuration parse failure, not a mock
binary or test double. Before the trigger there were zero candidate directories
and the live daemon was PID 2272.

Setup returned `SYNAPSE_CANDIDATE_HEALTH_FAILED` with candidate PID 15720,
`alive=False`, signed exit code `1`, hexadecimal exit code `0x00000001`, and
paths plus hashes for stdout and stderr. Independent readback found:

- diagnostic schema: `synapse_candidate_failure/v1`
- diagnostic SHA-256:
  `67094F5A88EDD73FD7126211D1D6AA9BD9357632438A5009E88508F57C99F1C6`
- eight retained evidence files
- empty stdout SHA-256:
  `E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855`
- stderr SHA-256:
  `D5FE0AC5E063976DFD8F4F79DB114DC09AAD25E0DBA32E64AF5386619D16C970`
- lifecycle exit ledger SHA-256:
  `FEBC528F1FE4960B5FFB4DBACF83F2A059B105C0B92A4465EC52DF1747689AD5`

Every retained file's independently computed length and SHA-256 matched the
manifest. The last stderr line classified the root failure exactly:
`parse M3 permission grants: unknown M3 permission "NOT_A_REAL_PERMISSION"`.
The old live daemon was still PID 2272.

A later repetition retained
`candidate-20260804T001209985Z-2108` with manifest SHA-256
`3750B2F898977AEDC38BEEC376477ECBD68857626EE499F1997527315D68EB4B`,
stderr SHA-256
`46A4B350AF79359D2AF8DB18DAB226FBC7CD172B6DE712D34109FCB6BF9B4103`,
and exit-ledger SHA-256
`8F3CDB3FD9A47F7F750902B86FF9E8CB15B792949AA048CAB2960F6D9D944E23`.

## Boundary audit

1. Invalid startup format: the real invalid permission above exited 1 before
   binding. Before: no candidate evidence. After: eight hash-verified files and
   exact structured cause; live PID unchanged.
2. Retention maximum: five timestamped copies of the real verified bundle were
   added, producing six verified directories before another real failure. After
   the trigger, exactly five verified bundles remained and the two oldest paths
   were physically absent. The new real failure bundle was present and valid.
3. Unverified lookalike: a strict-name directory without
   `candidate-diagnostic.json` contained `do-not-delete.txt`, SHA-256
   `B557383DD842F08EB80D54DB9078B315DABF8FAB67727689D0E9A5F8E5B675B0`.
   Before another retention trigger it existed with that hash. Afterward it
   still existed with the identical hash; verified bundles remained capped at
   five. Retention therefore cannot treat an unknown directory as setup-owned
   evidence.

`scripts/lint.ps1` passed every gate, including fmt, deny, and clippy in both
workspaces.
