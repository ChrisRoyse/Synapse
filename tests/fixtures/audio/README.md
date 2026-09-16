# Audio Fixtures

## hello_world_5s.wav

- Transcript: `Hello world. This is Synapse.`
- Format: WAV, PCM signed 16-bit little-endian, mono, 16 kHz, 5.000 seconds.
- Provenance: generated on the configured Windows host with `System.Speech.Synthesis` using the local `Microsoft David Desktop` en-US SAPI voice, then normalized and padded with ffmpeg to the fixture format.
- License: synthetic fixture generated for this repository; no external recording or third-party sample was sourced.
- SHA-256: `B811EDEDB0392928DC8673D91A3BE7FC37EC0BEC3E288C97EA928F949D96B6A6`

## loud_transient_1s.wav

- Format: WAV, PCM signed 16-bit little-endian, mono, 16 kHz, 1.000 seconds.
- Contents: silence except for one 10 ms 1 kHz sine burst from 0.500 s to 0.510 s at 0.95 linear amplitude.
- Provenance: generated on the configured Windows host with ffmpeg `aevalsrc` using the deterministic expression `if(between(t\,0.5\,0.51)\,0.95*sin(2*PI*1000*t)\,0)`.
- License: synthetic fixture generated for this repository; no external recording or third-party sample was sourced.
- SHA-256: `CDB3745482F4EA89533C3C9E1B14BFB944FDE58802808CA13064941602AF2EA0`

## pan_minus60_0_plus60.wav

- Format: WAV, PCM signed 16-bit little-endian, stereo, 16 kHz, 3.000 seconds.
- Contents: the same 440 Hz tone in three 1 s equal-power pan segments: -60 degrees, 0 degrees, +60 degrees.
- Provenance: generated on the configured Windows host with ffmpeg `aevalsrc`; segment gains are left/right `0.965925826/0.258819045`, `0.707106781/0.707106781`, then `0.258819045/0.965925826` at 0.6 tone amplitude.
- License: synthetic fixture generated for this repository; no external recording or third-party sample was sourced.
- SHA-256: `DDBB9B4AA3BA6599906B3F599E2F82F18CA0A2DE37167F866A3E486FB8DA343F`
