# Issue #2135 FSV: Whisper artifact and readiness recovery

Date: 2026-08-09  
Installed daemon build: `534913b2ee305ea3d3f3a8ae45b366e07c516b42`  
Installed PID: 20004

## Resolution lineage

The issue's original failure state—empty embedded slot, pre-pin ONNX residue, and absent ONNX Runtime Extensions DLL—was repaired by the end-to-end Whisper work in commit `8345dd15` and its setup/deployment follow-through. The fix generated and pinned the full-function graph, pinned and installed the Extensions shared library, packaged/materialized the model, and exposed readiness through audio health.

This closure audit does not rely on that historical claim. It independently re-read every physical artifact, loaded the installed model, transcribed new real audio, and reopened the durable result.

## Research

Exa MCP was live and used, and built-in web independently read Microsoft's primary documentation. ONNX Runtime Extensions requires an enhanced graph plus registration of the Extensions shared library in session options: <https://onnxruntime.ai/docs/extensions/>. Microsoft's Whisper end-to-end recipe composes audio decode, preprocessing, core Whisper/BeamSearch, and text postprocessing into the full-function graph used by this project: <https://github.com/microsoft/onnxruntime-extensions/blob/main/tutorials/whisper_e2e.py>.

## Physical artifact source of truth

| Artifact | Length | SHA-256 | Pin verdict |
|---|---:|---|---|
| `%LOCALAPPDATA%\synapse\models\whisper-tiny-int8.onnx` | 77,318,368 | `F43E21F9AAA360EBCF94270A83C900A8CE69D2F402040D30E05CFAA73074602F` | exact |
| `%LOCALAPPDATA%\synapse\build-models\whisper-tiny-int8.onnx` | 77,318,368 | `F43E21F9AAA360EBCF94270A83C900A8CE69D2F402040D30E05CFAA73074602F` | exact; no pre-pin residue |
| `%LOCALAPPDATA%\synapse\models\ort-extensions\onnxruntime_extensions.dll` | 3,333,664 | `0A0ACEA84AC7D90E5B6E81A6C37E19B8C356BD19809D44839EF945B3F2FEE353` | exact |

These values independently match `models/whisper-tiny-int8.pin.json` and the registry's `WHISPER_TINY_INT8_*` / `ORT_EXTENSIONS_WHISPER_*` constants. The old `147afac7...` artifact is present in neither named location.

## Execute and inspect

Before the new trigger, installed health reported audio `status=ok`, `stt_model_available=true`, `stt_model_loaded=true`, model policy `auto`, selected backend `Cpu`, and host-memory policy. CPU selection is separately justified by the positive CUDA-absence proof documented for #2137.

Windows `System.Speech` emitted `Whisper pinned artifact verification six three one.` with input SHA-256 `1CA279C60908ACE65F0B2BC830958F9644B1C2E48B317056F3CFC585FF9D2618`. The real installed loopback/STT path returned the expected new text `Whisper pinned artifact verification 631.`, model id `whisper_tiny_int8`, latency 309 ms, RMS -21.9828 dB, and speech start/end events.

The returned observation named physical key `18ca28b5e61cdad400000004`. A separate raw MCP storage session reopened that exact `CF_OBSERVATIONS` row and decoded `StoredObservation`:

- observation id `observe-01786284963920665300-0000000004`;
- `audio_transcription_present=true`;
- value length 1,635 bytes;
- value SHA-256 `sha256:d19004a9d98ea43c8096f66c817ec168cced81e4c98b2e57c825c2d5758c4618`.

## Boundary and edge coverage

The original end-to-end acceptance record `docs/fsv/2026-08-06-issue-2016-whisper-stt.md` independently counted physical vault rows before/after each case:

- duration 0: explicit empty transcript persisted;
- duration 31: `TOOL_PARAMS_INVALID`, no row written;
- language `fr`: `TOOL_PARAMS_INVALID`, no row written.

The later deployment record `docs/fsv/2026-08-06-issue-2038-audio-deployment.md` also proved setup fails closed for both mismatched audio/`READ_AUDIO` permission combinations and that the canonical supervisor retains the valid pair.

## Additional finding

The new response also retranscribed a phrase from approximately 19 minutes earlier despite health advertising a 30-second ring. That independent sample/event eviction defect is tracked as #2187 with exact timestamps and this row key. It does not contradict the narrower #2135 result: the pinned graph and custom-operator library now load and execute, but tail retention needs its own root-cause fix.

## Verdict

PASS for #2135. The exact pinned model and Extensions library physically exist, health announces readiness before a call, a new real utterance executed through the installed model, and the resulting transcription exists in the durable vault.
