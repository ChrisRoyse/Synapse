# Issue #2028 acquisition cleanup FSV — 2026-08-06

## Source of truth and trigger

The physical Source of Truth (SoT) is the filesystem beneath these two setup-owned acquisition roots:

- `C:\Users\hotra\AppData\Local\synapse\build-models`
- `C:\Users\hotra\AppData\Local\synapse\runtime\onnxruntime-gpu`

The manual trigger loaded `Remove-SynapseStaleAcquisitionArtifacts` and its dependencies by parsing their `FunctionDefinitionAst` nodes from the real `scripts/synapse-setup.ps1`. It did not use a copied helper, test, test harness, or mock. Every verdict below came from a separate `Test-Path`, `Get-ChildItem`, file-length, or SHA-256 read after the trigger.

The implementation follows PowerShell's documented guarantee that `finally` runs before control leaves the script, makes acquisition failures terminating with `-ErrorAction Stop`, and deletes only exact literal paths. References: [about Try/Catch/Finally](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_try_catch_finally?view=powershell-7.6), [Remove-Item](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.management/remove-item?view=powershell-7.6), and [Get-Process](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.management/get-process?view=powershell-7.6).

## Synthetic happy path

Root: `C:\Users\hotra\AppData\Local\synapse\fsv\issue-2028-happy`; synthetic dead owner PID: `2147483000`.

Before the trigger, separate filesystem reads found three candidates totaling 24 bytes: a five-byte `.download-2147483000` file, a seven-byte `package-2147483000.zip`, and an `extract-2147483000` directory containing three-byte and nine-byte files. File hashes were printed before deletion.

The trigger reported `candidate_count=3`, `candidate_bytes=24`, `reaped_count=3`, `reclaimed_bytes=24`, and `retained_live_count=0`. A separate recursive read found zero matching artifacts after the trigger.

The first run of this case failed closed with `SYNAPSE_ACQUISITION_TEMP_ANCESTOR_READ_FAILED` and left every byte in place. That exposed an incorrect reliance on `FileInfo.Parent`; the implementation now resolves the physical parent path explicitly. The complete case was then rerun successfully as described above.

## Boundary and edge cases

### Empty root

Before: zero candidates. Trigger: `candidate_count=0`, `reaped_count=0`, `reclaimed_bytes=0`. Separate after-read: zero candidates.

### Live owner

Root: `C:\Users\hotra\AppData\Local\synapse\fsv\issue-2028-live`; owner: the invoking PowerShell PID `59324`.

Before: `model.onnx.download-59324` existed, length 13, SHA-256 `170F5660FECE35DB218FECE184B25E99771DDB3E8852850ABA6F237624341FF4`. Trigger: `candidate_count=1`, `reaped_count=0`, `retained_live_count=1`. Separate after-read: the same path still existed with the same length and SHA-256. After PID 59324 exited, a new manual trigger classified it as dead, removed all 13 bytes, and a separate read proved absence.

### Structurally invalid candidate

Root: `C:\Users\hotra\AppData\Local\synapse\fsv\issue-2028-invalid`.

Before: an invalid directory named `bad.download-2147483000` contained a four-byte `proof.bin`, SHA-256 `425305E25DF9DF108E011164F7CA97522276CF1BC67B8AEC3A7139CD60FB9A81`. Trigger: failed loud with `SYNAPSE_ACQUISITION_TEMP_SHAPE_INVALID ... expected=file`. Separate after-read: the directory and file still existed with the same length and SHA-256. The exact production cleanup primitive was invoked afterward for host hygiene; its separate read proved absence.

## Real production orphan

Before the trigger, an independent recursive filesystem read found exactly one acquisition artifact:

```text
path=C:\Users\hotra\AppData\Local\synapse\build-models\rtdetr_v2_s_coco.onnx.download-19464
kind=file
bytes=81057510
owner_pid=19464
owner_live=false
sha256=583A236AC21C95A7FD94F284FC21485E42355BFEF82C27011BA78FBC09EE87E2
```

The production trigger reported:

```text
candidate_count=1
candidate_bytes=81057510
reaped_count=1
reclaimed_bytes=81057510
retained_live_count=0
```

After the trigger, a separate recursive enumeration of both production roots reported `SEPARATE_READBACK_AFTER_COUNT=0`; an exact `Test-Path` read reported `EXACT_ORPHAN_EXISTS=False`.

This proves both the historical leak and the reaper's real behavior against physical bytes. A complete setup run will separately verify the durable `setup-acquisition-cleanup.json` ledger as part of the final unattended-install FSV.
