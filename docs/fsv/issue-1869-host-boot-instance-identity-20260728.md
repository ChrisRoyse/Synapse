# FSV — issue #1869: the host boot identity was install-scoped, so a reboot could never be detected

- **Date:** 2026-07-28 UTC
- **Host:** Windows 11 Pro 10.0.26200
- **Boot instance:** `winboot:214c0f00-1815-11f0-803b-a0d365a198dd:62`
- **Branch:** canonical `main` (no worktree, no branch, no alternate target directory)
- **Verdict:** PASS for every gate reachable without a reboot (see section 10)

## 1. Defect

`m4::read_host_boot_identity()` returned
`SYSTEM_BOOT_ENVIRONMENT_INFORMATION.BootIdentifier`. That GUID identifies the
**operating system installation**, not the boot instance. Because it never
changes, `classify_host_boot_relation` could only ever produce `same_boot`, and
`reconcile_one_orphaned_shell_job` would still publish
`windows_job_object_kill_on_last_handle_close` /
`supervisor_restart_children_reaped_by_job_object_close` after a real host
reboot. That is precisely the misattribution #1856 was filed to remove, so
#1856 gate 3 was unreachable while this held.

## 2. Source of Truth

Nothing below is taken from a tool return value.

| Fact | Physical Source of Truth |
| --- | --- |
| boot counter | `KUSER_SHARED_DATA.BootId`, read from the kernel page mapped read-only at `0x7FFE0000` |
| boot counter (2nd witness) | `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters\BootId` |
| boot counter (historical) | System event log `Microsoft-Windows-Kernel-Boot` id 20, field `LastBootId` |
| boot instant | `NtQuerySystemInformation(SystemTimeOfDayInformation).BootTime` |
| boot instant (2nd witness) | System event log `Microsoft-Windows-Kernel-General` id 12; `Win32_OperatingSystem.LastBootUpTime` |
| installation identity | `NtQuerySystemInformation(SystemBootEnvironmentInformation).BootIdentifier` |
| installation date | `Win32_OperatingSystem.InstallDate` |
| reboot event | System event log id 1074 |
| durable job classification | `%LOCALAPPDATA%\synapse\shell-jobs\jobs\<job_id>\status.json` |
| supervisor marker | `%LOCALAPPDATA%\synapse\shell-jobs\supervisor.json` |

## 3. Proof that the old identity was install-scoped

A real planned restart happened between two readings of the same value:

- Event 1074 at `2026-07-28T04:25:58Z` (restart initiated by `StartMenuExperienceHost.exe`).
- Kernel-General id 12: OS started `2026-07-28T04:26:55.5Z`.
- Event log service stopped `04:26:33Z`, started `04:27:15Z`.

| when | reported `current_host_boot_id` |
| --- | --- |
| `2026-07-27T20:07:06Z` (pre-reboot, recorded in #1856) | `214c0f00-1815-11f0-803b-a0d365a198dd` |
| `2026-07-28T04:33Z` (post-reboot, live daemon pid 2932) | `214c0f00-1815-11f0-803b-a0d365a198dd` |

Byte-identical. Reading `ntdll!NtQuerySystemInformation` class 90 directly —
outside Synapse, so no daemon cache is involved — returned the same GUID
(`ntstatus=0x00000000 returned=32`). Decoding it as a UUIDv1 gives an embedded
timestamp of `2025-04-13T03:12:28Z`; `Win32_OperatingSystem.InstallDate` is
`2025-04-12 22:16:16` local = `2025-04-13T03:16Z`. It is minted at OS install.

## 4. Proof that `KUSER_SHARED_DATA.BootId` is the per-boot value

Rather than assume, the boot counter's behaviour was read out of 24 consecutive
boots already recorded in the Windows System event log
(`Microsoft-Windows-Kernel-Boot` id 20, field `LastBootId`):

```
38  2026-03-06 20:35:03Z     47  2026-05-01 07:30:15Z     56  2026-06-10 15:38:45Z
39  2026-03-13 10:16:33Z     48  2026-05-06 18:40:58Z     57  2026-06-16 14:53:18Z
40  2026-03-13 10:18:14Z     49  2026-06-07 12:32:17Z     58  2026-07-15 07:31:32Z
41  2026-04-18 15:25:42Z     50  2026-06-07 12:34:21Z     59  2026-07-15 07:33:10Z
42  2026-04-18 16:29:37Z     51  2026-06-07 12:35:15Z     60  2026-07-15 07:34:01Z
43  2026-04-22 10:00:47Z     52  2026-06-08 18:35:45Z     61  2026-07-28 04:26:55Z
44  2026-04-22 10:02:38Z     53  2026-06-10 09:48:50Z
45  2026-04-22 10:03:59Z     54  2026-06-10 09:50:29Z     live shared page : 62
46  2026-04-23 21:38:13Z     55  2026-06-10 09:51:19Z     live registry    : 62
```

`+1` per boot with no gaps; each event timestamp equals the corresponding
Kernel-General id 12 boot instant; the live value is `61 + 1`. Three independent
surfaces agree.

## 5. Fix

Identity becomes `winboot:<installation guid>:<boot counter>`.

- The counter discriminates boots within an installation.
- The installation GUID prevents a reinstall — whose counter restarts near zero —
  from colliding with boot records written by the previous installation.
- `SYSTEM_TIMEOFDAY_INFORMATION.BootTime` is **excluded** from the identity and
  carried as corroborating evidence only, because the kernel re-bases
  `KeBootTime` when the system clock is set. Including it would let one boot
  present two identities and produce a false `boot_changed` — the #1856 error
  inverted.
- `BootId` is read through the `windows` crate's metadata-derived
  `KUSER_SHARED_DATA`, guarded by
  `const { assert!(offset_of!(KUSER_SHARED_DATA, BootId) == 0x2C4) }`. A future
  layout change fails the build rather than silently reading a neighbouring
  field — which is exactly how the install-scoped GUID shipped as a boot
  identity.

`setup operation=host_transition` now publishes `host_boot_identity_evidence`
with every witness and an explicit agreement verdict, so a `same_boot` /
`boot_changed` classification can be grounded against the event log instead of
trusted.

## 6. Second defect, caught by watching the first deploy

Changing the encoding invalidates every previously written record. The
first deployed build was observed doing exactly that:

```json
{"code":"M4_SHELL_JOB_SUPERVISOR_RESTART_RECONCILED",
 "host_boot_id":"winboot:214c0f00-1815-11f0-803b-a0d365a198dd:62",
 "prior_host_boot_id":"Some(\"214c0f00-1815-11f0-803b-a0d365a198dd\")",
 "marker_host_boot_relation":"boot_changed",
 "prior_supervisor_state":"absent_clean_shutdown_marker"}
```

No reboot occurred — the host stayed in boot 62 and only the daemon was
replaced. `classify_host_boot_relation` now returns
`prior_boot_identity_incomparable_encoding` when either side is in an encoding
this build cannot interpret. It proves neither a reboot nor its absence, and the
downstream arms already degrade such a record to
`unknown_without_same_boot_job_object_evidence` /
`supervisor_restart_child_absent_containment_unknown` rather than naming a
termination mechanism. A surviving exact child creation identity still upgrades
the relation to `same_boot_proven_by_surviving_exact_child`, because that is
physical proof the host did not reboot.

## 7. Synthetic inputs with known expected outputs

Deployed through `scripts\synapse-setup.ps1` (exit 0, Chrome bridge OK):
daemon pid 16836, `sha256=183FCF43D651CE711718BCC7BDE1921B353CBB7868CB514DF245F616342FD101`.

### 7.1 Three durable-job fixtures differing in exactly one field

Three records were written under
`%LOCALAPPDATA%\synapse\shell-jobs\jobs\<id>\status.json`, identical apart from
`supervisor.host_boot_id`. Every process identity in them is real: for each
fixture a `powershell -Command Start-Sleep 60` child was started, its pid and
`GetProcessTimes` creation FILETIME captured from the kernel, then killed and
confirmed absent — so "child absent" is a physical fact, not an invented pid.

| fixture | recorded `supervisor.host_boot_id` | child pid / creation FILETIME |
| --- | --- | --- |
| `fsv1869-boot-changed` | `winboot:214c0f00-…:61` | 19796 / 134297083770978897 |
| `fsv1869-same-boot` | `winboot:214c0f00-…:62` | 3784 / 134297083780099961 |
| `fsv1869-legacy-format` | `214c0f00-1815-11f0-803b-a0d365a198dd` | 7660 / 134297083788562120 |

BEFORE, read off disk: all three `status=running`, `supervisor_reconciliation`
absent.

**Trigger:** the real daemon was killed (pid 16836) and the supervisor relaunched
it (pid 5076, health confirmed). Startup reconciliation is the code under test.

### 7.2 Edge cases 4 and 1/2 through the wired MCP transport

- Gate 1: a real durable job `fsv1869-live-gate1` (`cmd.exe /c ping -n 600 … & exit 5`, pid 10808) started through the `shell` facade, then `action=preflight`.
- Gate 2: same after cancelling it (pid 10808 confirmed absent).
- Edge case 4: an authorized preflight whose `host_boot_id` was rewritten to `…:61`, i.e. an authorization minted on a previous boot, then replayed through `action=execute` with the exact confirmation string. A second live durable job (`fsv1869-live-guard2`, pid 13916) was armed first so the safety-digest check would also refuse — two independent guards, so the test could not reach a real shutdown.

## 8. Evidence of success

### 8.1 Classification, read back from `status.json` after the restart

| fixture | `host_boot_relation` | `children_terminated_by` | `cause` |
| --- | --- | --- | --- |
| `fsv1869-boot-changed` | `boot_changed` | `host_reboot_or_power_transition_not_job_object_attributed` | `host_reboot_controller_loss` |
| `fsv1869-same-boot` | `same_boot` | `windows_job_object_kill_on_last_handle_close` | `supervisor_restart_children_reaped_by_job_object_close` |
| `fsv1869-legacy-format` | `prior_boot_identity_incomparable_encoding` | `unknown_without_same_boot_job_object_evidence` | `supervisor_restart_child_absent_containment_unknown` |

All nine values match the prediction. Every record shows
`reconciling_host_boot_id = winboot:214c0f00-…:62`, `child_identity_state = absent`,
`observation_source = daemon_startup_supervisor_restart_reconciliation`, and
`exit_code_trustworthy = false`.

This is the #1856 gate 3 behaviour, and it is the behaviour that was
**impossible** before this fix: with an install-scoped identity, the first row
could only ever have been classified as the second.

The `boot_changed` row also proves the discrimination is real rather than a
constant — the `same_boot` row still names the Job Object mechanism.

### 8.2 The supervisor marker no longer claims a phantom reboot

```json
{"code":"M4_SHELL_JOB_SUPERVISOR_RESTART_RECONCILED",
 "host_boot_id":"winboot:214c0f00-1815-11f0-803b-a0d365a198dd:62",
 "prior_host_boot_id":"Some(\"winboot:214c0f00-1815-11f0-803b-a0d365a198dd:62\")",
 "marker_host_boot_relation":"same_boot",
 "live_status_jobs":3,"interrupted_jobs":3,"adopted_jobs":0,
 "retained_live_child_jobs":0,"write_failures":0,
 "interrupted_job_ids_sample":["fsv1869-boot-changed","fsv1869-legacy-format","fsv1869-same-boot"]}
```

Same daemon-restart-within-one-boot scenario that produced `boot_changed` in
section 6; now correctly `same_boot`.

### 8.3 Gate 1 — refusal persisted on disk

`%LOCALAPPDATA%\synapse\host-transitions\preflights\pf-1785234903629-….json`:

```json
{"schema":"synapse_host_transition_preflight/v1",
 "transition":"restart","reason":"issue 1869 boot identity FSV",
 "host_boot_id":"winboot:214c0f00-1815-11f0-803b-a0d365a198dd:62",
 "authorized":false,
 "durable_job_ids":["fsv1869-live-gate1"],
 "consumed_unix_ms":null}
```

The refusal names the exact job and is itself auditable. Gate 2 after
cancellation: `authorized: true`, `pf-1785234938901-…`.

### 8.4 Edge case 4 — a prior-boot authorization cannot be replayed

```
MCP error -32099
message     : host boot identity changed after preflight:
              preflight=winboot:214c0f00-…:61 current=winboot:214c0f00-…:62
detail_code : HOST_TRANSITION_PREFLIGHT_BOOT_CHANGED
```

Refused at the boot-identity check, ahead of the digest backstop, naming both
identities. Independent readback proves the refusal had no side effect at all:

```
boot_time_now      = 2026-07-27 23:26:55 (unchanged)
boot_counter_now   = 62 (unchanged)
pending_intent     = False
intents_dir        = <absent>          <- no intent was persisted
preflight_consumed = <null>            <- authorization not burned
newest_event_1074  = 2026-07-28 04:25:58Z (the earlier user restart, unchanged)
```

### 8.5 Live identity evidence surface

`setup operation=host_transition action=status`:

```
current_host_boot_id           : winboot:214c0f00-1815-11f0-803b-a0d365a198dd:62
boot_counter                   : 62      (KUSER_SHARED_DATA)
boot_counter_registry_value    : 62      (PrefetchParameters)
boot_counter_witness_agreement : agree
boot_time_utc                  : 2026-07-28T04:26:55Z  == Kernel-General id 12
installation_identity          : 214c0f00-1815-11f0-803b-a0d365a198dd
```

## 10. What is NOT proven here

The counter has not been observed advancing on this host across a boot with the
fixed code in place. It is proven to advance by the 24-boot event-log series in
section 4, and the classifier is proven to act correctly on a changed counter in
section 8.1, but the end-to-end crossing — arm a live durable job, execute the
planned restart, and read `…:63` plus a real `host_reboot_controller_loss` after
boot — requires a real reboot and is tracked as #1856 gates 3/4/5.

## 9. Research lane

Exa: `registered_but_unusable`, `EXA_API_CREDITS_EXHAUSTED_402` — the state
already recorded in AGENTS.md/#1864, so not re-diagnosed. Research went through
the built-in web lane:

- Geoff Chappell, *KUSER_SHARED_DATA* — `BootId` at x64 offset `0x2C4`, Windows 10 and higher.
- Geoff Chappell, *The Boot Status Data Log* — the `bootstat.dat` store winload seeds `BootId` from.
- NtDoc, *SYSTEM_BOOT_ENVIRONMENT_INFORMATION* — structure shape (`BootIdentifier`, `FirmwareType`, `BootFlags`); notably it does **not** document the field as per-boot, and this host proves it is not.
- systemd `/proc/sys/kernel/random/boot_id` and journald `_BOOT_ID` — the cross-platform precedent for a per-boot identifier distinct from installation identity.

