# FSV — issue #1867: install the durable shell-job keeper before `ResumeThread`

- **Date:** 2026-07-27 / 2026-07-28 UTC
- **Host:** Windows 11 Pro 10.0.26200, boot id `214c0f00-1815-11f0-803b-a0d365a198dd`
- **Branch:** canonical `main` (no worktree, no branch, no alternate target directory)
- **Verdict:** PASS

## 1. Defect

`spawn_shell_job_child` resumed the `CREATE_SUSPENDED` durable child *inside* the
spawner, before the caller persisted the durable `running` row and installed the
child-owned Job Object keeper handle. A child that reached terminal state in that
window could not receive a duplicated handle, so Windows failed
`DuplicateHandle` with `ERROR_ACCESS_DENIED (0x80070005)` and the durable job was
recorded as a control-plane failure instead of the normal terminal job it was.

## 2. Source of Truth

The durable status record on disk:

```
%LOCALAPPDATA%\synapse\shell-jobs\jobs\<job_id>\status.json
```

read independently with `Get-Content | ConvertFrom-Json`, plus:

- `stdout.log` bytes for the same job,
- the OS process table (`Get-CimInstance Win32_Process`) for exact pid + creation time,
- the daemon stderr log for the ordering of the structured event codes.

Tool return values are used only to trigger; every verdict below is a separate
read.

## 3. Synthetic input with a known expected output

`cmd.exe /c exit 7` — a child that always exits ~immediately with a value that
cannot occur by accident. A correct durable job must record `exit_code = 7`.

Control: `cmd.exe /c ping -n 30 127.0.0.1 >NUL & exit 3` — same call shape, only
the child's lifetime differs.

## 4. BEFORE — daemon pid 18444 (pre-fix image)

Six real `shell operation=start` calls through the wired MCP daemon. Six
identical failures:

```
MCP error -32099: act_run_shell_start could not establish restart-survivable
ownership for pid {22688,12068,13748,3872,14520,21840};
... DuplicateHandle into exact child failed: Access is denied. (0x80070005)
```

Disk readback:

| job | status | exit_code | error_code | containment |
|---|---|---|---|---|
| fsv1867-before-001..006 | `restart_survival_handoff_failed_reaped` | **NULL** | `TOOL_INTERNAL_ERROR` | `windows_named_job_object_child_keeper_pending` |
| fsv1867-before-control-long | `exit_nonzero` | 3 | – | `windows_named_job_object_armed_child_keeper_restart_survivable` |

The real result was observed and discarded — the persisted error message contains
`initial_reap=ExactChildReapReadback { reaped: true, exit_code: Some(7), ... }`
and `final_identity_state=Absent`, i.e. `ERROR_ACCESS_DENIED` on a process that
was already gone. 6/6 is deterministic, not flaky; the control proves the failure
is a pure function of the child winning the race.

## 5. Fix

- `spawn_shell_job_child` no longer resumes; it returns the child suspended,
  contained, and identity-bound.
- `start_authorized_shell_job_with_boundary` resumes through the new
  `resume_restart_survivable_shell_child`, **after** the durable `running` row,
  the keeper handle, and the kill-on-close readback are all committed.
- Resume failure after commit fails closed through the existing exact-owned
  `fail_shell_job_restart_survival_handoff` reap path, reason
  `contained_child_resume_after_restart_survival_commit_failed`.
- The #1866 reconciliation is preserved *and* guarded: if the child was
  externally terminated while suspended, the resume is skipped
  (`M4_ACT_RUN_SHELL_CONTAINED_CHILD_TERMINAL_BEFORE_RESUME`) rather than
  reported as a fault.

`PROC_THREAD_ATTRIBUTE_JOB_LIST` was deliberately not adopted: under
`CREATE_SUSPENDED` the child has executed no instruction when
`AssignProcessToJobObject` runs, so that step is already unraceable.

### Ordering safety, re-derived

`assign_owned_process_job` sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, reads it
back, assigns, and verifies membership with `IsProcessInJob` — all before the old
resume point, therefore before the new one. Daemon death anywhere in the enlarged
pre-resume window still reaps the child through the same authority. A suspended
child is reaped more cleanly: no descendants, no output, no mutations.

## 6. Deployed image identity

```
built/installed/live sha256 = 991CF7069944D46995B67586E4204EFCD64B5F692ECA76E2574CE4F50E0339D1
live pid                    = 11700 -> 3172 (supervisor gen2)
live exe                    = C:\Users\hotra\.cargo\bin\synapse-mcp.exe
```

New code markers present in the live image bytes: `True` for
`M4_ACT_RUN_SHELL_CONTAINED_CHILD_RESUMED`,
`M4_ACT_RUN_SHELL_CONTAINED_CHILD_TERMINAL_BEFORE_RESUME`,
`contained_child_resume_after_restart_survival_commit_failed`.

## 7. AFTER — happy path

Same six-call sweep, same synthetic input:

| job | status | exit_code | error_code | containment |
|---|---|---|---|---|
| fsv1867-after-001..006 | `exit_nonzero` | **7** | – | `windows_named_job_object_armed_child_keeper_restart_survivable` |

6/6 → 6/6. `M4_ACT_RUN_SHELL_CHILD_TERMINAL_BEFORE_RESTART_SURVIVAL` count: **0**.
`M4_ACT_RUN_SHELL_RESTART_SURVIVAL_HANDOFF_FAILED` count: **0**.

Event ordering for `fsv1867-after-001` (pid 22536) — the structural proof:

```
03:28:49.153  M4_ACT_RUN_SHELL_JOB_KEEPER_INSTALLED
03:28:49.178  M4_ACT_RUN_SHELL_RESTART_SURVIVAL_ARMED
03:28:49.394  M4_ACT_RUN_SHELL_CONTAINED_CHILD_RESUMED
03:28:49.395  M4_ACT_RUN_SHELL_JOB_STARTED
03:28:49.635  M4_ACT_RUN_SHELL_JOB_LOCAL_TERMINAL_STATUS_PERSISTED
```

The child's entire executable lifetime begins 241 ms *after* containment was
committed. The race is not reconciled — it cannot occur.

## 8. Edge cases

### Edge 1 — success boundary + proof the child actually executed

`cmd.exe /c echo SYNAPSE_FSV_1867_MARKER_42 & exit 0`, job
`fsv1867-edge-exit0-marker`:

```
status      = ok
exit_code   = 0
containment = windows_named_job_object_armed_child_keeper_restart_survivable
stdout.log  length = 29 bytes
            sha256 = 26973F0BD965727936E95A7A5DA7ED4649BEA49950591E001D671A6A96AFA9BC
            content = [SYNAPSE_FSV_1867_MARKER_42 ]
```

A child left suspended would have produced a zero-byte `stdout.log`. The bytes
on disk prove `ResumeThread` ran.

### Edge 2 — restart survival regression (the guarantee the keeper exists for)

Job `fsv1867-restart-survival`, `cmd.exe /c ping -n 240 127.0.0.1 >NUL & exit 11`.

Before: child pid 14324, creation `2026-07-27T22:30:12.4451010-05:00`,
`local_process_identity.start_time = 134296830124451014`, daemon pid 11700,
supervisor incarnation `279c6cce2aa64459bc4cd50ee91f7faf`.

Action: `Stop-Process -Id 11700 -Force` (exact same-boot daemon termination).

Immediately after, with the daemon gone:

```
daemon 11700 alive = False
child  14324 alive = True
child creation     = 2026-07-27T22:30:12.4451010-05:00   (unchanged)
```

After supervisor relaunch (gen2, child pid 3172):

```
M4_SHELL_JOB_SUPERVISOR_RESTART_CHILD_ADOPTED
    job_id=fsv1867-restart-survival pid=14324
    child_start_time=134296830124451014
M4_SHELL_JOB_SUPERVISOR_RESTART_RECONCILED
    incarnation_id=dcb1c6deb4f44c56af344df27b2c1b06 supervisor_pid=3172
    supervisor_identity_provable=true
```

Durable record after adoption: `status=running`, `pid=14324`,
`identity start=134296830124451014` (unchanged), `supervisor.pid=3172`,
`supervisor.incarnation_id=dcb1c6deb4f44c56af344df27b2c1b06` (changed).

Deferring the resume did not weaken restart survival.

### Edge 3 — cancel of a live durable job

Job `fsv1867-edge-cancel`, `cmd.exe /c ping -n 300 127.0.0.1 >NUL`:

```
before_status = running
job object readback: active_before=2; arm=Ok(()); terminate=Ok(());
                     active_after=Ok(0); close=Ok(()); verified=true
after: status=cancelled  exit_code=1  running=false
       remaining_process_ids=[]
```

`active_before=2` is independent proof the resumed child really executed: it had
already created its `ping.exe` descendant inside the job object.

## 9. Residual

The inline `act_run_shell` path is untouched — it uses a separate spawner
(`m4/mod.rs` ~23752) with its own resume and its own cleanup-on-failure
guarantees. Verified by `grep -n 'spawn_shell_job_child('`, which shows the
durable start as the only caller.

## 10. Research lane

Exa is HTTP 402 on this host (#1864); the independent lane was the built-in web
path. It corroborates the Raymond Chen guidance cited in the issue: the
documented pattern is create-suspended → assign to job → resume, because a
process can exit or spawn descendants before the assignment otherwise completes.

- <https://learn.microsoft.com/en-us/windows/desktop/api/jobapi2/nf-jobapi2-assignprocesstojobobject>
- <https://learn.microsoft.com/en-us/windows/win32/procthread/suspending-thread-execution>
- <https://devblogs.microsoft.com/oldnewthing/20230209-00/?p=107812>
