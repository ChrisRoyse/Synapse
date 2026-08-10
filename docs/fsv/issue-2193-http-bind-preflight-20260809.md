# Issue #2193 — HTTP bind preflight before daemon startup

Date: 2026-08-09 (America/Chicago)

Code commit verified: `46a715ecee23d9b26f3e46cd06c19ac8a6096a2c`

## Acceptance and Sources of Truth

The acceptance Sources of Truth were:

1. the installed `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`, its byte length
   and SHA-256, and the Windows process table;
2. the Windows TCP listener table and authenticated production `/health`;
3. the rejected child process's captured stderr and actual process exit code;
4. `%LOCALAPPDATA%\synapse\db-daemon\action_recovery.jsonl`, whose absence is
   the durable proof that no synthetic input was held or recovery event written;
5. `%LOCALAPPDATA%\synapse\db-daemon\daemon-run-current.json`, read and hashed
   independently before and after every rejected child;
6. the physical Calyx `MANIFEST`, `vault-identity.json`, `vault.pid`, and the
   sibling `db-daemon.lineage.json`;
7. setup's release-build log, build-target readback, and job-owned build-process
   invocation readback.

Every trigger was followed by separate process, listener, file, or HTTP reads.
Return values alone were not accepted. No automated test, mock, harness,
fallback, alternate Cargo target directory, worktree, or branch was used.

## Root cause

`Cli.bind` is a mode-shared `String`. In the old executable, `main::run` did all
of the following before dispatching `Mode::Http`:

1. initialized telemetry and the global metric registry;
2. asserted process QoS and initialized DPI awareness;
3. resolved M2/M3/M4 configuration;
4. configured and read the action crash-recovery ledger;
5. ran the real full-virtual-key-space startup release sweep;
6. spawned the synthetic-input watchdog OS thread.

Only then did `http::transport::serve` parse the string into `SocketAddr` and
enforce loopback policy. Its comment that validation occurred before side
effects was locally true only relative to the transport lock. It was false for
the executable as a whole.

The failure funnel had a second classification defect. `top_level_error_exit`
unconditionally called `daemon_lifecycle::record_top_level_error`; that helper
treated the expected absence of lifecycle state before `configure` as a ledger
failure. A malformed bind therefore acquired real process-global action state,
then printed both its primary parse error and the misleading secondary error
`daemon lifecycle ledger is not configured`.

## Independent research after diagnosis

`pwsh -File scripts/check-research-lane.ps1` drove a real Exa MCP 3.4.0
`initialize`, `tools/list`, and `tools/call`. The readback reported `live`, with
`web_search_exa` and `web_fetch_exa` listed and a real call successful. Exa was
used as the supplemental research lane. The built-in web lane independently
read the primary documentation:

- clap's `ValueParser` converts and validates raw values at the argument
  boundary: <https://docs.rs/clap/latest/clap/builder/struct.ValueParser.html>.
- clap derive maps typed fields through parser-driven conversion:
  <https://docs.rs/clap/latest/clap/_derive/>.
- Rust defines `FromStr` as the canonical typed conversion contract:
  <https://doc.rust-lang.org/std/str/trait.FromStr.html>.
- `SocketAddr` is the standard typed IP-and-port representation and implements
  parsing of IPv4 and bracketed IPv6 socket addresses:
  <https://doc.rust-lang.org/std/net/enum.SocketAddr.html>.

Globally changing `Cli.bind` to `SocketAddr` would make an HTTP-only argument
relevant to connect, Chrome native host, doctor, worker, and agent modes. The
robust application is therefore a mode-conditional typed preflight immediately
after CLI decoding, then typed propagation with no second parse.

## Implemented behavior

- `http::preflight_bind` is pure. It parses to `SocketAddr`, applies the
  non-loopback policy, and returns a typed `BindPreflightError` on refusal.
- The preflight runs immediately after `Cli::parse`, before telemetry, metrics,
  QoS, DPI, action recovery, OS input inspection/release, watchdog, storage,
  lifecycle, or lock acquisition.
- The decoded transport state carries the validated `SocketAddr`.
  `http::serve` and `http::transport::serve` accept only that type, so a second
  parse or divergent downstream policy is structurally unavailable.
- Invalid syntax emits `HTTP_BIND_ADDRESS_INVALID`, the rejected value, parser
  detail, expected `IP:PORT` format, example, and remediation, then exits 2.
- A forbidden non-loopback endpoint emits
  `HTTP_BIND_NON_LOOPBACK_REFUSED`, the exact endpoint, and explicit
  remediation, then exits 2.
- Rejections write one stderr line directly because telemetry has deliberately
  not initialized. This is not silent degradation: the complete structured
  operator diagnostic is available even when logging configuration itself is
  invalid.
- `record_top_level_error` returns `Recorded` or `NotConfigured`. Expected
  pre-configuration absence is quiet; poisoned locks and durable ledger-write
  failures remain errors and still produce the secondary lifecycle diagnostic.

The first committed candidate placed preflight before action/storage but after
telemetry. Manual FSV rejected it because the malformed bind emitted 23 metric
registration events before the correct error. Acceptance stopped, the boundary
was moved ahead of telemetry, compile/lint/deployment were rerun, and none of
that first candidate's evidence is counted below.

## Compile and lint

The exact final committed source passed:

```text
cargo check --workspace
Finished dev profile; exit 0

pwsh -File scripts/lint.ps1
Gate 0 zero-test/zero-harness doctrine — OK
Gate 0b production surface invariants — OK
Gate 1 shared lint contract — OK
Gate 2 toolchain 1.97.1 agreement — OK
Gate 3 lock graph — OK
Gate 4 fmt, root and calyx — OK
Gate 5 cargo deny, root and calyx — OK
Gate 6 clippy --workspace --all-targets -D warnings, root and calyx — OK
Gate 7 public Calyx API ratchet — OK
LINT OK
```

## Real optimized build and deployment

The production setup path built the canonical checkout target with
`CARGO_BUILD_JOBS=12` and `CMAKE_BUILD_PARALLEL_LEVEL=12`, matching all 12
logical CPUs. The job-owned invocation exited 0 after 11m01s, left no child or
build-tool process behind, and produced:

```text
target artifact bytes=77293568
target artifact sha256=1B12D61F982795265B131764DE1736694285CFD9B852E037F3E62B17ECE76571
installed bundled bytes=256660809
installed bundled sha256=EA3969B3A690F28434C4B360FD1DDD6878791B27BEBED06552D5C250BA4EE462
setup build log bytes=215
setup build log sha256=06773057F26619F22CD464E71AD9A68B53F2FD91F1CC2B2A2124059691B29DA0
```

The only build warning was:

```text
calyx-forge: cuda feature not enabled, skipping kernel compilation
```

Setup's hardware readback independently proved
`nvidia_pnp_device_count=0`, `nvcc_path=null`, and `cuda_path_env=null`. CPU was
therefore the capable path, not a fallback hiding failed CUDA. Live health
reported `cpu_simd_path=avx2`; the fixed-vector dot, cosine, L2, and top-k probe
passed, and Windows execution-speed throttling was disabled. No alternate
build-target footprint existed.

The full setup installed and started the candidate, then intentionally failed
closed only because this already-running Codex process started with tool-surface
hash `72d406...8846` while the live daemon publishes `80c43d...c573`. It wrote
the required handoff. A supported `-SkipBuild -SkipClientWiring` setup invocation
then revalidated the same installed hash, performed another real graceful
handoff, preserved audio and the existing permission contract, independently
reopened the production vault, and exited 0.

## Full State Verification

### Final before state

Immediately before the issue-specific triggers:

```text
installed_exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed_bytes=256660809
installed_sha256=EA3969B3A690F28434C4B360FD1DDD6878791B27BEBED06552D5C250BA4EE462
production_pid=8112
listener=127.0.0.1:7700 owner_pid=8112
health_ok=true
build_commit=46a715ecee23d9b26f3e46cd06c19ac8a6096a2c
build_tree_state=clean
build_matches_checkout=true
action_recovery_exists=false
lifecycle_run_id=1786320789065-8112-019fe9041e497c50a1e8f5570ab2dae6
lifecycle_bytes=942
lifecycle_sha256=21FA6E7D32F48C34F39C5B2A402A49801D00ECCFCF06797B86C7B5E468EA34ED
vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
manifest_seq=49619
durable_seq=1304520
lineage_high_water_seq=1304499
```

The live command line contained the preserved capability contract:

```text
--mode http --bind 127.0.0.1:7700
--enable-audio
--allowed-permissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO
```

### Happy path — valid loopback HTTP startup

The production setup started the real installed executable on the valid
loopback endpoint. A separate Windows listener read found exactly one listener
at `127.0.0.1:7700`, owned by PID 8112. Authenticated `/health` independently
reported:

```text
ok=true
pid=8112
build=46a715ecee23
http.status=ok
http.bind_addr=127.0.0.1:7700
action.status=ok
synthetic_watchdog_started=true
synthetic_holds_tracked_keys=0
synthetic_holds_tracked_buttons=0
storage.status=ok
storage.db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
calyx_vault.status=ok
calyx_vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
calyx_vault_last_recovered_seq=1304499
chrome_bridge.status=ok
process_qos.status=ok
execution_speed_throttling_disabled=true
```

The setup path also launched the same installed hash as a real isolated daemon
on `127.0.0.1:60495`, obtained its authenticated health and 40-public-tool
surface, gracefully shut it down, separately proved the listener was released,
and removed its isolated artifacts before the production handoff.

### Edge 1 — malformed socket address

Before the trigger, the action ledger was absent and lifecycle bytes were the
942-byte SHA-256 above. Trigger:

```text
synapse-mcp.exe --mode http --bind not-a-socket-address --db <production-db> --log-level info
```

Actual exit was 2. Actual stderr contained exactly one line:

```text
synapse-mcp error: code=HTTP_BIND_ADDRESS_INVALID bind="not-a-socket-address" detail="invalid socket address syntax" expected_format="IP:PORT" example="127.0.0.1:7700" remediation="provide an IP socket address such as 127.0.0.1:7700"
```

Separate after reads proved:

```text
output_lines=1
telemetry_or_metric_lines=0
action_or_synthetic_input_lines=0
storage_calyx_or_single_instance_lines=0
secondary_lifecycle_error_lines=0
action_recovery_exists=false -> false
lifecycle_bytes=942 -> 942
lifecycle_sha256=21FA6...34ED -> 21FA6...34ED
production_pid=8112 -> 8112
production_command_line=identical
listener_owner_pid=8112 -> 8112
extra_synapse_processes=0
```

### Edge 2 — valid syntax, forbidden non-loopback endpoint

Trigger used the maximum TCP port to cover the socket boundary:

```text
synapse-mcp.exe --mode http --bind 0.0.0.0:65535 --db <production-db> --log-level info
```

Actual exit was 2. Actual stderr contained exactly one line:

```text
synapse-mcp error: code=HTTP_BIND_NON_LOOPBACK_REFUSED bind=0.0.0.0:65535 remediation="bind to a loopback address, or explicitly pass --allow-non-loopback after securing the network boundary"
```

The independent after audit again found zero telemetry/action/storage/lifecycle
lines, no action ledger, identical lifecycle bytes/hash, unchanged production
PID/command/listener, and no rejected child process.

### Edge 3 — unrelated pre-lifecycle failure after a valid maximum-port bind

This trigger proves two facts at once: `127.0.0.1:65535` passes the bind
preflight, and an independent failure before lifecycle configuration no longer
manufactures a secondary ledger error.

```text
synapse-mcp.exe --mode http --bind 127.0.0.1:65535 --db <production-db> --log-level definitely-invalid
```

Actual exit was 1. Actual stderr contained exactly one line:

```text
synapse-mcp error: invalid log level definitely-invalid: error parsing level filter: expected one of "off", "error", "warn", "info", "debug", "trace", or a number 0-5
```

The output contained neither `synapse-mcp lifecycle error` nor
`daemon lifecycle ledger is not configured`. Separate after reads again proved
the action ledger absent, lifecycle bytes/hash identical, production
PID/command/listener unchanged, and rejected child gone.

### Final state — independent physical reads

After all triggers:

```text
installed_bytes=256660809
installed_sha256=EA3969B3A690F28434C4B360FD1DDD6878791B27BEBED06552D5C250BA4EE462
production_pid=8112
listener=127.0.0.1:7700 owner_pid=8112
health_ok=true
build_commit=46a715ecee23d9b26f3e46cd06c19ac8a6096a2c
build_tree_state=clean
action_recovery_exists=false
lifecycle_bytes=942
lifecycle_sha256=21FA6E7D32F48C34F39C5B2A402A49801D00ECCFCF06797B86C7B5E468EA34ED
vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
vault_latest_seq=1304523
manifest_seq=49620
manifest_durable_seq=1304523
manifest_derived_content_seq=1304522
```

The durable sequence increase from 1304520 to 1304523 is physically present in
the production `MANIFEST` and corresponds to normal authenticated health/usage
activity from the live daemon. The rejected children could not reach storage;
their production lifecycle file remained byte-for-byte identical across every
trigger.

The real AVX2 math probe after the triggers contained the expected values:

```text
dot=[1.0, 0.0, 2.0]
cosine=[1.0, 0.0, 1.0]
l2_squared=[0.0, 2.0, 1.0]
topk=[(1,1.5), (3,1.5), (0,0.25)]
```

## Cleanup

Both setup invocations removed their candidate and staging directories. Setup's
staging sweep ended at zero entries and zero bytes. The manual rejected-child
checks wrote no issue-owned file or database. No alternate build target,
temporary vault, mock data, worktree, branch, browser process, or test artifact
was created.
