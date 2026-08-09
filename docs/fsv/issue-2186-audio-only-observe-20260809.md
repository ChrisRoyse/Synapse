# Issue #2186 FSV: audio-only observe does not dispatch detection

Date: 2026-08-09  
Installed build: `534913b2ee305ea3d3f3a8ae45b366e07c516b42`  
Installed PID: 20004

## Root cause and repair

`observe_include` correctly classified explicit `include:["audio"]` as global-only and created an intentional zero-window `ObservationInput`. The handler nevertheless took the shared detection runtime and spawned `populate_detection_from_state` for every request. Detection then rejected the deliberately empty foreground rectangle with `DETECTION_NO_FRAME`.

Detection is now dispatched only when `include.entities` is true. Audio, clipboard, and filesystem requests neither take the detector nor spawn its blocking task. Entity observations retain the existing fail-closed detector path.

## Research

`scripts/check-research-lane.ps1` had established `exa_mcp live`. Exa and the built-in web lane were both used after diagnosis. Tokio's official `spawn_blocking` documentation states that blocking work consumes the blocking pool and cannot be aborted once started, reinforcing that an unrequested detector must not be spawned: <https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html>.

## Sources of truth

- Installed executable and `/health` build provenance.
- Real request response diagnostics for routing state.
- Physical `CF_OBSERVATIONS` row reopened independently through storage `row_read`.

Setup deployed the exact clean `main` commit through `scripts/synapse-setup.ps1`. Health separately reported `build_matches_checkout=true`, executable `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, build `534913b2ee30`, and PID 20004.

## Happy path and state readback

Windows `System.Speech` emitted `Explicit audio route verification eight one nine.` (UTF-8 SHA-256 `A0F7A9DF1E857932A9F4FF7865EFA1F0259A6512797A1F37872BF29851F1F351`). The formerly failing exact request used `include:["audio"]`, `transcribe_audio_seconds:8`, and language `en`.

It succeeded with:

- transcription `Explicit Audio Root Verification 819`;
- latency 537 ms and RMS -23.7001 dB;
- foreground HWND/PID/bounds all zero, proving it remained global-only;
- `detection_status=disabled`, proving the detector was not dispatched;
- persisted key `18ca27ac06c7577400000001`.

Independent physical reopen of that key decoded `StoredObservation` with observation id `observe-01786283822007408500-0000000001`, `audio_transcription_present=true`, value length 1,511 bytes, and SHA-256 `sha256:f0db85b5545dd1dfa776ed47782eaf46741c451d2eb56e35711218e4a75b590d`.

## Boundary and edge cases

| Case | Before/trigger | After / independent state |
|---|---|---|
| Empty audio tail | newly restarted audio runtime, explicit `include:["audio"]` plus 8-second transcription | success, empty/not-applicable transcription, zero-window foreground, detector disabled; physical row key `18ca27a1cb4ce88000000000` persisted |
| Clipboard-only global route | explicit `include:["clipboard"]` | success, zero-window foreground, redacted content digest, detector disabled; physical row key `18ca27afdab4b08800000002` persisted |
| Entity route remains detection-bearing | explicit `include:["entities"]` against the real VS Code foreground | real HWND 394276 and positive 1938x1158 bounds; detector path returned structured `DETECTION_NOT_CONFIGURED` (active profile intentionally has no detector), not disabled; physical row key `18ca27b0bc8050a800000003` persisted |

The pre-fix trigger was also physically reproduced on installed `af8cb64e`: the same explicit audio request failed with `DETECTION_NO_FRAME` and zero bounds. This establishes the before/after contrast on the real daemon rather than from a return-value assumption.

## Verification gates

- `cargo check --workspace`: pass.
- `scripts/lint.ps1`: all seven gates passed in both root and calyx workspaces.

## Verdict

PASS. The exact previously failing call now transcribes real loopback audio, its durable row exists and decodes correctly, global-only sibling routes remain window-free, and entity requests still retain the detector contract.
