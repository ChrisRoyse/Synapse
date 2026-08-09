# Issue #2137 FSV: STT backend decision and health readback

Date: 2026-08-09  
Host: Windows, Intel Core i7-1355U (10 cores / 12 logical processors), Intel Iris Xe, no NVIDIA device  
Verified installed build: `af8cb64e02d39d52898f6d50a1cb85581ad2647b` from `refs/heads/main`

## Decision and root cause

DirectML was removed from the model, audio, and MCP feature graphs. The production ONNX Runtime GPU 1.27.1 binary refused DirectML registration with `Specified provider is not supported`. An isolated trial using Microsoft's DirectML-specific 1.24.4 native package registered the provider, but the real pinned Whisper encoder failed its first convolution with HRESULT `0x80070057` (`The parameter is incorrect`). Consequently, advertising or automatically choosing DirectML could not truthfully provide working STT on this machine.

The supported fail-closed policy is now CUDA, after positive device/admission proof, then CPU only after positive CUDA absence proof. Health exposes policy, loaded state, selected backend, device-memory policy, reservation, and exact fallback code/detail. Unsupported pinned values, provider construction failures, poisoned state locks, and indeterminate device state are errors rather than silent CPU degradation.

## Independent research

Both required lanes were used after diagnosis. `scripts/check-research-lane.ps1` reported `exa_mcp live`; Exa supplemented built-in web research. Primary references:

- ONNX Runtime's DirectML provider documentation requires sequential execution and disabled memory patterns, notes operator-support differences, and identifies DirectML as sustained engineering: <https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html>
- Microsoft's package split places DirectML in `Microsoft.ML.OnnxRuntime.DirectML`, rather than the ordinary GPU runtime used by this application: <https://www.nuget.org/packages/Microsoft.ML.OnnxRuntime.DirectML>

These constraints matched the measured runtime/package incompatibility and supported dropping an unverified provider instead of retaining a nonfunctional feature flag.

## Sources of truth

1. Installed process and build: `/health`, Windows process table, and `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`.
2. Backend selection: the live audio subsystem health object after the model was loaded.
3. Durable result: exact physical `CF_OBSERVATIONS` rows reopened independently through storage `row_read`, not the `observe` return value.
4. Failed-input non-mutation: independently repeated Calyx vault census and `CF_OBSERVATIONS` live/raw row counts.

## Execute and inspect

Canonical setup deployed with audio and `READ_AUDIO`; setup exited 0. Its independent health check found PID 8948 at the installed path, binary SHA-256 `3A06C1B97B7EC20C8917355CFD9E7BEE86BDF4BDA16B58682E47FC058A24C1AF`, and `build_matches_checkout=true`.

Before model load, health reported audio `initializing`, `stt_model_loaded=false`. A zero-second trigger returned the defined empty boundary and persisted row key `18ca25b627884d0800000000`: text empty, confidence source `not_applicable`, latency 0 ms, RMS -120 dB.

For the happy path, Windows `System.Speech` emitted the known phrase `Synapse hardware verification number four two seven.`; its UTF-8 SHA-256 was `6AF08A06C724B1512B0AC65895270D6924D51F47F5A3FD0DA8FFD63949A57163`. The real loopback/Whisper path returned `Synapse hardware verification number 427` (twice because the prior failed facade attempt's still-real playback remained in the 30-second loopback tail), latency 270 ms, and RMS -13.9524 dB.

The durable result was then read separately from the vault:

| Field | Physical readback |
|---|---|
| CF/key | `CF_OBSERVATIONS` / `18ca25beb4ab4de000000001` |
| Observation id | `observe-01786281703210962400-0000000001` |
| Decoded type | `StoredObservation` |
| Audio transcription present | `true` |
| Value bytes | 7,284 |
| Value SHA-256 | `sha256:ae93bd70a6936f6d1b83b88282bbd74bd259041f3302ddb040481f9c69c2188a` |

After load, live health reported:

- policy `auto`, selected backend `Cpu`, device memory `host_memory`, model loaded `true`;
- fallback code `SYNAPSE_STT_AUTO_CPU_NO_CUDA_PROVIDER`;
- detail beginning `CUDA device absence proved before STT provider selection`, with the exact NVML DLL/driver evidence and preceding admission result;
- Calyx math backend `cpu`, SIMD path `avx2`, fixed-vector probe `ok` with bit-for-bit portable-path agreement;
- process execution-speed throttling disabled.

## Boundary and edge-case audit

| Case | State before | Trigger | State after / physical proof |
|---|---|---|---|
| Empty duration | model unloaded; audio initializing | real `observe`, 0 seconds, language `en` | empty/not-applicable result persisted at key `18ca25b627884d0800000000`; model remained unloaded |
| Invalid format | vault census: `CF_OBSERVATIONS` live 5, raw 12 | real `observe`, 1 second, language `fr` | `TOOL_PARAMS_INVALID: language must be \"en\"`; repeated census remained live 5/raw 12, proving no observation row was written |
| Unsupported DirectML runtime | isolated candidate used real installed executable, pinned Whisper graph, separate vault/port, and DirectML 1.24.4 native DLLs | real Windows speech entered live loopback transcription | provider registered, then real encoder convolution failed with HRESULT `0x80070057`; candidate process stopped and all package/candidate artifacts were independently confirmed absent |
| Missing permission | installed audio model untouched | real transcription request before `READ_AUDIO` was enabled | `SAFETY_PERMISSION_DENIED` naming missing `READ_AUDIO`; no model load; canonical setup was then rerun with the permission explicitly granted |

An additional defect discovered during verification—explicit `include:["audio"]` incorrectly dispatching detection and returning `DETECTION_NO_FRAME`—was recorded separately as #2186. It was not hidden or absorbed into this backend decision.

## Verdict

PASS. The installed production daemon matches `main`; the real supported STT path runs on the fastest truthful backend available on this hardware (CPU with host memory), health explains exactly why CUDA was not selected, and the expected transcription exists in the physical vault. DirectML was removed because real provider and graph execution evidence proved it nonfunctional, not because of an assumed hardware rule.
