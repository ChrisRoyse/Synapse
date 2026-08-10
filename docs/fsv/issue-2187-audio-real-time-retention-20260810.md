# Issue #2187 FSV: loopback retention follows real time

Date: 2026-08-10 (America/Chicago)  
Installed commit: `73bfa076dd294190d8a6cc137bffbdbdc3a4f4fd`  
Installed PID: `15976`

## Root cause

The advertised 30-second tail was bounded by captured frame count, not time.
`WasapiLoopback` discarded every `BufferInfo` field except `silent`, and
`AudioRing` advanced only when WASAPI supplied a packet. During an empty capture
interval neither its cursor nor eviction advanced, so `tail(8)` meant the last
eight seconds *containing frames*. Old speech could remain eligible for STT for
arbitrarily long wall time. Separately, detector events had a 64-item count cap
but no age eviction, so quiet systems retained old events indefinitely.

The repair carries WASAPI device position, QPC timestamp, and flags into the
ring. Device-position gaps become provenance-marked zero frames, and a read
adds the elapsed no-packet interval as silence. Gaps at least as large as ring
capacity clear it in O(capacity), not O(gap). Detector events and activity
latches expire at the same configured real-time horizon. Timestamp errors,
device-position regression/overlap, and QPC regression remain structured
`AUDIO_TIMELINE_INVALID` failures; they are not silently restitched.

The first deployed implementation incorrectly made every
`DATA_DISCONTINUITY` fatal. Reality rejected that decision: real playback
produced a one-frame discontinuity (`expected_position=6034296`,
`device_position=6034297`), stopped audio with `AUDIO_TIMELINE_INVALID`, and
persisted degraded observation `18ca839f70d2756400000001`. Independent
`storage operation=row_read` decoded its 1,131-byte `StoredObservation` with
SHA-256
`sha256:b9f202e3b04cb5f669f76740b850428f11b84af73468e108b042d700f38839b7`.
Acceptance stopped, the research was corrected, and the implementation was
changed before redeployment. A discontinuity with monotonic device positions is
now the explicit observable signal for the exact intervening silence; invalid
or regressing coordinates still fail closed.

## Research

`scripts/check-research-lane.ps1` performed a real Exa MCP initialize,
tools/list, and tools/call and reported `exa_mcp live` (server 3.4.0). Exa and
the built-in web lane were then used independently after diagnosis.

- Microsoft documents the stream-relative device position, QPC timestamp,
  empty-buffer result, and buffer flags returned with each capture packet:
  <https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer>
- Capture clients drain all available packets; silent packets represent
  silence rather than sample bytes:
  <https://learn.microsoft.com/en-us/windows/win32/coreaudio/capturing-a-stream>
- Microsoft defines `DATA_DISCONTINUITY`, `SILENT`, and `TIMESTAMP_ERROR`:
  <https://learn.microsoft.com/en-us/windows/win32/api/audioclient/ne-audioclient-_audclnt_bufferflags>
- Microsoft's real-time audio guidance says to compare device positions and
  insert silence proportional to dropped frames; QPC deltas diagnose callback
  delay rather than forming an equality invariant with device-frame deltas:
  <https://blogs.windows.com/windowsdeveloper/2014/05/15/real-time-audio-in-windows-store-and-windows-phone-apps/>
- Loopback captures the render endpoint's system mix:
  <https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording>

This is timeline reconstruction from authoritative capture coordinates, not a
fallback or transcript filter.

## Sources of truth

1. Trigger: real Windows `System.Speech` WAV played synchronously through the
   render endpoint and captured by the installed WASAPI loopback process.
2. Returned state: public `observe include:["audio"]`, including live STT,
   detector events, audio diagnostics, and the exact persisted key.
3. Durable state: the physical `CF_OBSERVATIONS` count read separately before
   and after, followed by `storage operation=row_read` of the exact key.
4. Runtime state: an independent health request plus structured daemon log
   events `AUDIO_TIMELINE_DISCONTINUITY` / `AUDIO_TIMELINE_GAP`.
5. Whole-vault integrity: a separate full `audit operation=verify_chain` walk.

## Installed reality and hardware dispatch

The supported setup path built with 12 Cargo/CMake jobs, performed isolated
candidate preflight, and installed the release image at
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`. The OS process table reports PID
15976 running that path with `--enable-audio` and explicit `READ_AUDIO` /
storage permissions. Independent file readback reports 257,759,561 bytes and
SHA-256
`2D5B46D6DDB858681A13D50B5CF5DD796640B7849BBCC54E83FAA023E6DC4333`.
Health reports build `73bfa076dd29`, `ok=true`, and audio `ok`.

The host is a 10-core/12-thread Intel Core i7-1355U with Intel Iris Xe and no
NVIDIA/CUDA device. Calyx's startup probe therefore selected the CPU backend's
`avx2` path under `math_backend=auto`; fixed dot/cosine/L2/top-k probes passed.
The daemon independently asserted interactive scheduling QoS with
`power_throttling_control_mask=1`, `power_throttling_state_mask=0`, and
`execution_speed_throttling_disabled=true`. This is the fastest valid backend
on the detected hardware, without pretending the Intel GPU is CUDA-capable.

Immediately after real playback health remained `ok` while exposing 26 observed
discontinuities, 13,627,129 cumulative gap frames, and the last exact gap
`expected_position=14043124 actual_position=14043125 gap_frames=1`. A long
endpoint position jump was processed with the bounded ring clear rather than a
loop proportional to the missing interval.

## Happy path: known real audio and physical row

Before the trigger, exact storage summary reported `CF_OBSERVATIONS=7` and
health audio `ok`. Windows played the known phrase:

```text
Audio clock repair one five seven.
UTF-8 SHA-256: 74C05977030D1D595CF128DA628EBF00A47312360AFA3A46A90170DB18D37670
WAV bytes: 131202
WAV SHA-256: 73CF2645454FE0F2B6386AF61FF49E146AAD021F87948C19AB95E1CB2B018A8F
played_at: 2026-08-10T18:43:45.8533713Z
```

The real eight-second observation returned:

```text
text=Audio clock repair 157.
latency_ms=401
model_id=whisper_tiny_int8
audio_status=healthy
persisted_key=18ca85e72e48af0c00000004
observation_id=observe-01786387430166277900-0000000004
```

It also returned four events at 18:43:46--18:43:48 (music started, speech
started, loud transient, speech ended), proving the live device triggered the
detector. A separate summary then reported `CF_OBSERVATIONS=8`. Independent
row read of the exact key decoded `StoredObservation`, matched the observation
id, reported `audio_transcription_present=true` and audio `healthy`, and read
1,417 physical bytes with SHA-256
`sha256:90f9f6f0885adb6603f87fe84aad40a8dc73340944dc0cabcc7e66f02f2a1849`.

## Real-time expiry

Before waiting, the known latest event was `2026-08-10T18:43:48.678019Z`, the
known returned transcription was `Audio clock repair 157.`, and the physical
row count was 8. No audio was played for a measured 32 seconds. A new real
eight-second observation at `2026-08-10T18:44:46.484354Z` then returned:

```text
transcription_text=""
contains_prior_phrase=false
recent_event_count=0
audio_status=healthy
persisted_key=18ca85f44b1ab38400000005
```

The separately read count advanced once to 9. Exact row read found a
1,076-byte `StoredObservation`, SHA-256
`sha256:990596dbfa47639992c729dfcad56e5412ae6a494cbfbfac32bc36d8edca466b`,
with transcription present and stored audio status healthy. Thus neither old
PCM nor count-capped old detector events survived the elapsed horizon.

## Boundary and edge audit

Every row count below came from a separate storage summary, not the operation's
return value.

| Case | State before / trigger | State after / independent evidence |
|---|---|---|
| Empty duration | rows 9; `transcribe_audio_seconds=0` | success, empty text, zero events, healthy; rows 10; key `18ca85fb7597f04800000006`; 1,075-byte row SHA `sha256:a2008d04c4ec812a4d1b9d8f501705d1a6590c384f2fd1fe1a7ac2ea968f9308`, transcription present |
| Exact maximum | rows 10; `transcribe_audio_seconds=30` after expiry | success, empty text, prior phrase absent, zero events, healthy; rows 11; key `18ca85fb861aad3400000007`; 1,076-byte row SHA `sha256:39155a3d89272f373373a4c74261535d774f8c28a1177273d347ee263f83cf08`, transcription present |
| Over maximum | rows 11; `transcribe_audio_seconds=30.001` | `TOOL_PARAMS_INVALID`: `audio seconds must be between 0 and 30; got 30.001`; rows remain exactly 11 (delta 0), proving rejection before persistence |

## Gates and whole-vault readback

- `cargo check --workspace`: pass on the corrected commit.
- Canonical `pwsh -File scripts/lint.ps1`: pass across both root and Calyx
  workspaces on the corrected commit.
- `git diff --check`: pass.
- Fresh `git fetch origin main`: local main is two tested commits ahead and
  zero behind `origin/main`; no branch or worktree was created.

The final public chain walk independently re-read and re-hashed the production
store: verdict `intact`, entries/head `343212/343212`, verified `[0..343212)`,
tip `6768edab95a5c3da3573168eb10e51af80e93099fd8f24e4a9cda22c6635ee69`.
Raw commitments were intact (`965533` total, `965530` sealed, 3 current tail
rows pending, 39,643 cohort seals). Vault generation remained 1 with zero
resets and the existing truthful lineage-seeded partial-history classification.

## Verdict

PASS. The installed release captured and transcribed known real output,
physically persisted it, evicted both samples and events by elapsed time, kept
valid Windows capture gaps observable without becoming unhealthy, rejected an
invalid duration before any write, and left the full durable chain intact.

