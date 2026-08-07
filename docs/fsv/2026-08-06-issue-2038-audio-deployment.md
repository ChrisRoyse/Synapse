# FSV: durable audio deployment contract (#2038)

Date: 2026-08-06 (America/Chicago)

## Sources of truth

- Durable launch intent: Task Scheduler task `SynapseMcpDaemon` and
  `%LOCALAPPDATA%\synapse\bin\synapse-daemon-supervisor.ps1`.
- Runtime: Win32 process table and authenticated `GET /health` for installed
  daemon PID 19124.
- Artifact identity: installed executable plus the pinned Whisper ONNX and ONNX
  Runtime Extensions files.
- Durable results: independently opened Calyx `CF_OBSERVATIONS` and physical
  vault WAL under `%LOCALAPPDATA%\synapse\db-daemon`.

## Diagnosis and fix

The Whisper and Extensions artifacts were already present, but the installed
daemon command line contained neither `--enable-audio` nor `READ_AUDIO`.
`scripts/synapse-setup.ps1` had no audio deployment parameter and therefore
could not persist the setting through its candidate, supervisor, drift, or
adoption contracts.

Setup now exposes `-EnableAudio`, passes `--enable-audio` through candidate and
supervised launches, verifies candidate audio health/model availability, and
includes the bit in live drift and persisted-supervisor identity checks. It
fails with `SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID` unless `-EnableAudio`
and `READ_AUDIO` are either both present or both absent.

Research used live Exa MCP v3.4.0 plus the built-in web lane. Primary references
included Microsoft Task Scheduler documentation, Microsoft Olive CPU/INT8
Whisper guidance, ONNX Runtime quantization/performance documentation, and the
upstream whisper.cpp project. The installed in-process 77 MB Whisper tiny INT8
graph was retained: on this i7-1355U it produced the exact known transcript in
547 ms, so adding a second native/Python runtime and IPC lifecycle had no
measured speed justification.

## Deployment readback

Setup completed all 12 phases in 2942.47 seconds, including candidate validation
(21.184 s), handoff (58.744 s), and installed health (68.497 s). The first
release link crashed as `rustc STATUS_ACCESS_VIOLATION` with zero source
diagnostics; setup classified it as a toolchain crash and its cached retry
completed successfully.

The physical supervisor contains:

```text
$ExpectedEnableAudio = 'True'
$ExpectedAllowedPermissions = 'READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO'
... --enable-audio --allowed-permissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO
```

Win32 independently reported installed PID 19124 with the same command line.
Authenticated health after initialization reported `status=ok`, `audio
loopback running`, `ring_buffer_seconds=30`, `stt_model_loaded=true`, and
`stt_model_available=true`.

## Manual happy path

Before the trigger, read-only `dump_cf` reported one row: the explicit
zero-duration initialization record. Windows System.Speech generated a real WAV
containing `The quick brown fox jumps over the lazy dog.` at
`%LOCALAPPDATA%\synapse\fsv\issue-2038\known-audio.wav`, length 148836 and
SHA-256 `4D86A12C1CC8542061574FEBA6BBBE10BE9C11E2A465538903E0293A6BA27C1D`.
`System.Media.SoundPlayer.PlaySync` played it through the default Windows render
endpoint for 3644 ms after WASAPI loopback was running.

The public `observe` transcription returned the exact expected sentence, model
`whisper_tiny_int8`, latency 547 ms, RMS -24.8833 dB, speech start/end events,
and a direction estimate. A separate read-only vault open then reported two
`CF_OBSERVATIONS` rows. The new row was 1516 bytes with SHA-256
`083405b9ac8d1da2fec48df0af7ec27a3f93c8bf53213f9e40c7cb3043b4479e`.
An independent binary search found the exact sentence in physical WAL
`wal\00000000000000000026.wal`.

## Boundary and edge audit

Each before/after row count came from a separate read-only Calyx open.

| Trigger | Before | Result | After |
|---|---:|---|---:|
| setup audio enabled without `READ_AUDIO` | 0 | `SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID`; live PID/arguments unchanged | 0 |
| setup `READ_AUDIO` without audio enabled | 0 | `SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID`; live PID/arguments unchanged | 0 |
| transcription duration 0, language `en` | 0 | explicit empty transcript, latency 0, persisted row hash `569c475e...` | 1 |
| transcription duration 31 | 2 | `TOOL_PARAMS_INVALID`, supported range 0..30 | 2 |
| transcription language `fr` | 2 | `TOOL_PARAMS_INVALID`, only `en` supported | 2 |

## Gates

- `cargo check --workspace`: passed.
- `pwsh -File scripts/lint.ps1`: all seven gates passed in both workspaces.
- PowerShell parser and `git diff --check`: passed.
