# FSV — #2174 durable Chrome command terminals

Date: 2026-08-11 (America/Chicago)  
Implementation commit: `360e1a0a445e97afb7fba9ca49425b5bae7648f3` on `main`

## Source of Truth

No MCP or setup return value was accepted by itself. The independent Sources
of Truth were:

- the real `Runtime.evaluate` value read from the already-open authenticated
  Chrome window and its exact selected tab;
- the extension's durable `chrome.storage.local` owner ledger, read separately
  through `operator_panic_status`, including terminal sequence, payload SHA-256,
  byte length, outbox, in-flight mutation, session continuity, and load error;
- the daemon's append-only JSONL terminal events, read separately from disk,
  which record queue, delivery, terminal acceptance, error classification, and
  the result readback summary;
- the live extension HELLO identity and startup ledger emitted by the daemon;
- the installed/release executable bytes, Windows process table, listener
  table, authenticated `/health` payload, and durable setup phase ledger.

The smallest recurrence dataset was one deterministic JavaScript string of
2,902,214 `x` characters. It is one byte above neither limit but is larger than
Axum's former 2 MiB default, so it distinguishes the repaired transport from
the exact production failure without generating irrelevant data.

## Root cause

The command arrived over the persistent authenticated WebSocket, but its
terminal result took a second, unrelated HTTP POST to
`/chrome-debugger/native/message`. Axum's inherited 2 MiB JSON body limit
rejected the 2,902,214-character result with HTTP 413 before the bridge handler
ran. The extension interpreted that response-channel rejection as loss of the
otherwise healthy command channel, closed the WebSocket with code 3001,
cleared ownership without a daemon acknowledgement, and attempted a second
contradictory response. The daemon then surfaced only reconnect cleanup. The
architecture had split one command transaction across two transports with no
durable terminal ownership, acknowledgement, replay identity, or exact
deduplication.

Manual deployment exposed and corrected three deeper recovery defects before
acceptance:

1. a raw JSON-string comparison after `chrome.storage.local.set` failed because
   structured clone may reorder object keys; the verifier now compares
   recursively canonical object-key JSON plus SHA-256 and byte length;
2. `reload_bridge` asked a stale worker for its pre-reload snapshot, making the
   recovery control plane depend on the failed data plane; stale workers are
   now skipped with `CHROME_DEBUGGER_HOST_RELOAD_PRE_SNAPSHOT_SKIPPED`, while
   the installer-owned UIA lease remains the independent authority;
3. the first v4 ledger write physically completed before the old v2 row was
   removed. The next worker saw both rows and failed closed. Startup now accepts
   only the exact canonical v4 successor of v2, removes v2, and performs a
   separate exact readback. Any divergent dual-row state is still rejected.

The observed interrupted migration was reconciled exactly from v2 revision
2748 to v4 revision 2749, 898 bytes, SHA-256
`3496386b07e88718e413c0835c2c334ae9694dc38a10b77ac6eae769d64044ad`,
with a separate read proving the legacy v2 row absent.

## Repair

Protocol version 2 makes the response a terminal message on the same
authenticated WebSocket that delivered the command:

- after execution, the extension serializes the complete response once,
  records its command id, original host, browser-session id, monotonically
  increasing terminal sequence, byte length, and SHA-256 in the v4 durable
  outbox, then sends it;
- only an exact daemon ACK deletes that outbox row; reconnect replays the same
  bytes, and a contradictory ACK or replay fails closed;
- the daemon retains ownership after delivery even if the caller times out or
  the socket drops, accepts only the pending command's exact host/kind/session,
  and keeps a bounded receipt ledger so exact replay is ACKed without
  re-execution while contradiction is rejected;
- the HTTP message route is now a 1 MiB event-only lane. It cannot silently
  become a large command-result channel again;
- the WebSocket frame/message ceiling is 64 MiB and the terminal payload budget
  is 60 MiB. An executed response above 60 MiB is discarded and replaced by
  one small typed `A11Y_CDP_RESPONSE_TOO_LARGE` terminal containing actual and
  allowed byte counts;
- terminal acceptance logs response status and structured error diagnostics so
  transport, protocol, execution, and JavaScript failures remain distinct.

There is no response POST fallback, best-effort cleanup success, unacknowledged
outbox deletion, arbitrary replay, or silent oversized-result truncation.

## Research after diagnosis

`scripts/check-research-lane.ps1` drove the configured Exa server through real
stdio JSON-RPC initialize, tools/list, and tools/call operations. The final
readback at `%TEMP%\synapse-research-lane-readback.json` was 1,719 bytes,
SHA-256 `00725F4FFA3F56E1EAF49E7FB1DC08C26199DC180458DEF890D3B481D2A60E14`,
and reported `exa-search-server` 3.4.0 `live`. Exa queries covered durable
outboxes/ACKs/idempotent replay, MV3 service-worker persistence, recovery-plane
independence, and crash-consistent migration.

The built-in web lane independently read these primary sources:

- [Axum DefaultBodyLimit](https://docs.rs/axum/latest/axum/extract/struct.DefaultBodyLimit.html)
  and [WebSocketUpgrade](https://docs.rs/axum/latest/axum/extract/struct.WebSocketUpgrade.html),
  which document the 2 MiB default and explicit WebSocket message/frame limits;
- [Chrome MV3 WebSocket guidance](https://developer.chrome.com/docs/extensions/how-to/web-platform/websockets)
  and [chrome.storage](https://developer.chrome.com/docs/extensions/reference/api/storage),
  which establish service-worker lifetime behavior and durable extension state;
- [MDN WebSocket.send](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket/send),
  which makes send/buffering state observable but does not make delivery an ACK;
- [AWS Making retries safe with idempotent APIs](https://aws.amazon.com/builders-library/making-retries-safe-with-idempotent-APIs/),
  supporting caller-provided identity and exact duplicate handling;
- [AWS static stability](https://aws.amazon.com/builders-library/static-stability-using-availability-zones/)
  and [Google SRE emergency response](https://sre.google/sre-book/emergency-response/),
  supporting a recovery path that does not rely on the unhealthy component;
- [SQLite atomic commit](https://www.sqlite.org/atomiccommit.html), supporting
  recovery by inspecting durable pre/post states rather than assuming which
  write ran last.

Those sources led directly to one transport transaction, durable ownership,
exact ACK/dedup identity, explicit resource bounds, and crash reconciliation
from physical state.

## Build, deployment, and host optimization

- `node --check extensions/synapse-chrome-debugger/service_worker.js`: passed;
- `cargo check -p synapse-mcp`: passed;
- `cargo check --workspace`: passed;
- `pwsh -File scripts/lint.ps1`: passed the root and Calyx format/Clippy gates.

The canonical setup built with `CARGO_BUILD_JOBS=12` and
`CMAKE_BUILD_PARALLEL_LEVEL=12`, matching all 12 logical CPUs. Hardware probes
found no NVIDIA PnP device, `nvcc`, or CUDA path, so Calyx intentionally uses
the verified AVX2 CPU backend rather than pretending CUDA is available.

Setup completed build, candidate validation, Chrome preflight, installed-image
handoff, bridge reload, profile deployment, task registration, listener health,
and client wiring. It then failed closed at the already-running Codex process
boundary with `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE`; the durable handoff
names the four browser facade schemas changed since this Codex process started.
That failure did not roll back the installed daemon. Independent installed
state after the setup process exited was:

```text
source/build commit: 360e1a0a445e97afb7fba9ca49425b5bae7648f3
installed image:     C:\Users\hotra\.cargo\bin\synapse-mcp.exe
release image:       C:\code\synapse\target\release\synapse-mcp.exe
both bytes:          258054473
both sha256:         8CF698DAE1559712BA6D97568F9D2E26AAE98329BD04562AE284F073E676A30A
live process:        PID 14120, installed image
listener:            127.0.0.1:7700 owned by PID 14120
health:              ok=true build=360e1a0a445e pid=14120 bridge=ok host_count=1
math:                cpu/avx2, fixed probe agrees bit-for-bit
```

The live v13 HELLO reported declared build SHA-256
`5878a1f4e4f4142104c2953aedc4f70eef960e3e625ab32ec32dad4269967b7d`.
The installed worker was 1,059,327 bytes with SHA-256
`52A129B029E8137CBECD805EB0DF6B197DA2E98DD7B6A49EC731D935AD350A23`;
the daemon independently loaded the same hash from the real
`chrome-extension://.../service_worker.js` URL. Its startup ledger had storage
loaded, session continuity matched, owner continuity healthy, no in-flight
mutation, and zero terminal outbox entries.

## Manual Full State Verification

### Original 2.9 MB recurrence

Before the trigger the bridge had one healthy host, no queued/pending daemon
commands, storage loaded, no in-flight mutation, and an empty extension outbox.
The trigger evaluated `'x'.repeat(2902214)` on the real #2174 GitHub tab.

The result was independently inspected as a string of exactly 2,902,214
characters, first and last character `x`, every character `x`, result type
`string`, returned by value, and page state `complete`. The separately read
extension ledger then contained exactly this ACK:

```text
command_id=chrome-cdp-14120-11 terminal_sequence=33
payload_bytes=2902915
payload_sha256=156ab933c9bcb459ba721677821d57e3a0fc4416511e620b8baba3f2134bd77e
outbox=[] persisted_in_flight_mutation=null owner_continuity_healthy=true
```

The separate daemon JSONL row matched all four identifiers and reported
`response_ok=true`, `result_type=string`, target tab 589710129. No disconnect
event exists between delivery, terminal acceptance, and the following status
read. The host id remained `chrome-native-0-1786453212436`.

### Boundary and edge-case audit

| Case | Before and trigger | Expected | Independent after-state |
|---|---|---|---|
| Empty JavaScript value | Healthy host, empty outbox, no in-flight mutation. Evaluate `void 0` on the #2174 tab. | One successful terminal that preserves JavaScript `undefined`. | Caller read `result_type=undefined`, explicit `value=null`. Extension separately recorded command 15, sequence 37, 706 bytes, SHA-256 `9d89d4a3fd65955d0ec04e366eec995d6a56ac98c9b965303a4de0a9686d8e86`, then `outbox=[]`. Daemon row matched and said `response_ok=true`, `result_type=undefined`. |
| Maximum boundary exceeded after real execution | Healthy same host and worker, empty outbox. Evaluate `'x'.repeat(60 * 1024 * 1024)` by value. | Execute once, discard the over-budget result, emit one bounded typed terminal; do not disconnect or retry. | Caller received `A11Y_CDP_RESPONSE_TOO_LARGE` with `actual_bytes=62915261`, `limit_bytes=62914560`, `result_discarded=true`. Extension ACK was command 13, sequence 35, only 350 bytes, SHA-256 `e9659c4937aea8fdd76e2cc812b8d2018a504487bce47f4fc9a2e3baf56c05f0`; outbox empty, no in-flight mutation/timeouts. Daemon row matched, `response_ok=false`, same typed code and counts. Host and worker ids did not change and no disconnect was logged. |
| Invalid target identity | Ledger completed through command 17/sequence 39 with empty outbox. Evaluate on nonexistent `chrome-tab:999999999`. | Reject before delivery; never invent a target or send a bridge command. | Caller received `ACTION_TARGET_INVALID`. The immediately following independent extension read still named command 17/sequence 39 as the prior ACK; its own status command became the next activity. Outbox and in-flight state remained empty and no evaluate terminal for the nonexistent tab exists. |

## Final state

The final extension ledger remained loaded and healthy with no terminal outbox,
no persisted in-flight mutation, no unresolved debugger timeout, and the same
browser session/worker identity. The daemon remained PID 14120 on the installed
image, with one healthy Chrome host and no queue. The owned #2221 issue tab was
closed with an independent absence read. The already-open Chrome window then
contained only the two original tabs, #2174 and Gmail, with #2174 independently
verified active. Gmail content was never read or mutated.

No automated test, mock data, fallback response lane, second browser, branch,
worktree, alternate target directory, or temporary repository evidence tree
was created or used as acceptance evidence.
