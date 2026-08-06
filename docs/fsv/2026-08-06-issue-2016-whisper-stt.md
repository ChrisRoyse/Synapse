# FSV: Whisper end-to-end speech-to-text (#2016, #2023-#2026)

Date: 2026-08-06

## Sources of truth

- Artifact: `%LOCALAPPDATA%\synapse\models\whisper-tiny-int8.onnx`, length
  `77318368`, SHA-256 `F43E21F9AAA360EBCF94270A83C900A8CE69D2F402040D30E05CFAA73074602F`.
- Custom operators: `%LOCALAPPDATA%\synapse\models\ort-extensions\onnxruntime_extensions.dll`,
  length `3333664`, SHA-256
  `0A0ACEA84AC7D90E5B6E81A6C37E19B8C356BD19809D44839EF945B3F2FEE353`.
- Runtime: installed daemon PID 7876 and its public HTTP MCP `tools/list` schema.
- Durable result: Calyx `CF_OBSERVATIONS` rows under
  `%LOCALAPPDATA%\synapse\db-daemon`; the exact transcript was also found in
  physical WAL `wal\00000000000000000025.wal`.

## Diagnosis and research

The old direct Transformers export used 4.44.2, although Microsoft's last
maintained Olive Whisper example pins Transformers below 4.43 because later
exports decode empty text. It also quantized after BeamSearch insertion, which
corrupted fused contrib-domain types. The replacement follows Olive v0.8.0
commit `6ab9d8bb9284a89ccc87c63ca323c0f3386f6426`: optimize and INT8-quantize
component graphs before inserting BeamSearch.

Exa MCP v3.4.0 was probed live through real `initialize`, `tools/list`, and
`tools/call`. Built-in research used primary documentation: Microsoft Olive,
the [MCP tools specification](https://modelcontextprotocol.io/specification/2025-11-25/server/tools),
[ONNX Runtime custom operators](https://onnxruntime.ai/docs/reference/operators/add-custom-op.html),
[Microsoft's Extensions integration guide](https://github.com/microsoft/onnxruntime/blob/main/docs/onnxruntime_extensions.md),
[Hugging Face Whisper](https://huggingface.co/docs/transformers/v4.48.0/en/model_doc/whisper),
and [SLSA artifact verification](https://slsa.dev/spec/v1.2/verifying-artifacts).

## Happy path

Before the final trigger, read-only `dump_cf` reported 2 historical
`CF_OBSERVATIONS` rows. The builder independently generated a Windows
System.Speech WAV with SHA-256
`4D86A12C1CC8542061574FEBA6BBBE10BE9C11E2A465538903E0293A6BA27C1D`
whose known text is `The quick brown fox jumps over the lazy dog.`. The WAV was
played through the real default Windows output after WASAPI loopback was live.

The public `observe` call returned speech-start/end events, RMS `-26.5883 dB`,
Whisper model id `whisper_tiny_int8`, inference latency `731 ms`, and exactly:

```text
The quick brown fox jumps over the lazy dog.
```

A separate read-only `dump_cf` process then reported 8 rows and row 7 length
`1525`, value SHA-256
`62c753da5999d926329cc9dc6055a85c07772156887f2e0c91ee3eab129edc68`.
An independent binary search found the exact sentence in the physical WAL.

## Boundary audit

Each before/after count came from a separate read-only open of the real Calyx
vault, not the MCP return value.

| Trigger | Before | Result | After |
|---|---:|---|---:|
| `seconds=0`, language `en` | 8 | success, explicit `text=""` | 9 |
| `seconds=31`, language `en` | 9 | `TOOL_PARAMS_INVALID`, range is 0..30 | 9 |
| `seconds=1`, language `fr` | 9 | `TOOL_PARAMS_INVALID`, only `en` accepted | 9 |

The final physical row (zero-duration edge) has value length `1476` and SHA-256
`9cb0d1b8bfdfc0360a4fe0975b46abdbaf0ccd544f874f3fafaedab609a70959`.
Invalid requests created no observation rows.

## Gates

- `cargo check --workspace`: passed.
- `pwsh -File scripts/lint.ps1 -Fix`: all seven gates passed in both workspaces.
- Setup verified model pin, registry digest/length, Extensions pin, physical
  Extensions bytes, embedded bundle bytes, and installed daemon health before
  handoff. Its final `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` is expected:
  this already-running Codex cached the pre-change `observe` schema; an
  independent HTTP MCP session verified the current 40-tool schema contains
  both transcription inputs and the transcription output.
