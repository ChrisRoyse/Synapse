# FSV — #2221 Runtime.evaluate exception classification

Date: 2026-08-11 (America/Chicago)  
Implementation commit: `360e1a0a445e97afb7fba9ca49425b5bae7648f3` on `main`  
Acceptance comment: https://github.com/ChrisRoyse/Synapse/issues/2221#issuecomment-5253683327

## Source of Truth and diagnosis

This issue was discovered during #2174's real-browser verification. The source
of truth was the Chrome DevTools Protocol `Runtime.evaluate` response, not the
facade's return value. Evaluating invalid syntax produced an
`exceptionDetails` object, yet the extension mapped it to
`A11Y_CDP_ATTACH_FAILED`. A separate daemon terminal row proved the command had
already attached, executed, and returned one error terminal. The failure was a
JavaScript evaluation exception, not a debugger-attachment failure.

Acceptance reads four independent states after every trigger:

- the caller-visible typed error or value;
- the extension's durable `chrome.storage.local` terminal ACK/outbox and owner
  state through a separate `operator_panic_status` command;
- the append-only daemon JSONL terminal row on disk;
- the real target tab identity and `readyState` from Chrome.

## Repair

The cross-language error registry now defines
`BROWSER_EVALUATE_JAVASCRIPT_EXCEPTION`. `Runtime.evaluate.exceptionDetails`
is mapped only to that code. The extension carries bounded structured
diagnostics (`exception_type`, `exception_message`, `line_number`, and
`column_number`) in the terminal payload. The Rust bridge deserializes those
fields and logs them individually alongside the error code and detail.

Protocol/transport, debugger attachment, evaluation timeout, JavaScript
exception, and result-size failures remain separate classifications. There is
no message matching, attach-error fallback, swallowed exception, or synthetic
success result.

## Research after diagnosis

The configured Exa MCP lane was exercised with the query `Chrome DevTools
Protocol Runtime.evaluate exceptionDetails distinguish protocol error
JavaScript exception structured logging best practice`; its physical readback
reported `exa-search-server` 3.4.0 live. The built-in web lane independently
read:

- the official [Chrome DevTools Protocol Runtime domain](https://chromedevtools.github.io/devtools-protocol/v8/Runtime/),
  which defines `Runtime.evaluate.exceptionDetails` separately from protocol
  command failure and exposes exception metadata;
- [OpenTelemetry exception log conventions](https://opentelemetry.io/docs/specs/semconv/exceptions/exceptions-logs/),
  which support structured exception type/message fields rather than an opaque
  combined string;
- [MDN SyntaxError](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/SyntaxError),
  confirming syntax errors are JavaScript language exceptions.

This supports classification at the protocol boundary and structured logging
of the original exception evidence.

## Build and deployed identity

`node --check`, `cargo check -p synapse-mcp`, `cargo check --workspace`, and the
canonical `pwsh -File scripts/lint.ps1` all passed. The real installed daemon
reported build `360e1a0a445e`, PID 14120, and one healthy Chrome host. Its
installed/release executable SHA-256 was
`8CF698DAE1559712BA6D97568F9D2E26AAE98329BD04562AE284F073E676A30A`.
The live extension HELLO reported v13 and the expected declared build hash
`5878a1f4e4f4142104c2953aedc4f70eef960e3e625ab32ec32dad4269967b7d`.

## Manual Full State Verification

Before the first trigger, extension storage was loaded with no error, session
and owner continuity were healthy, the terminal outbox was empty, no mutation
was persisted, and daemon health reported one connected non-stale host with
zero queued/pending commands.

| Case | Trigger and expected result | Independent physical after-state |
|---|---|---|
| Invalid syntax | Evaluate `(() =>` on the real GitHub #2221 tab. Expect the JavaScript-exception code with SyntaxError details. | Caller received `BROWSER_EVALUATE_JAVASCRIPT_EXCEPTION`, `SyntaxError: Unexpected end of input`, line 1 column 7. Extension separately recorded command 4, terminal sequence 26, 339 bytes, SHA-256 `4a27dec4c83cb6767ad3961f55294db9a4a50b8ac8ed0c1112144ea29c909f5f`, then `outbox=[]` and no in-flight mutation. The daemon JSONL row matched those fields and separately logged `response_ok=false`, `exception_type=SyntaxError`, `exception_message=Unexpected end of input`, line 1 column 7. |
| Runtime throw | Evaluate `(()=>{throw new RangeError('issue-2221-runtime')})()`. Expect the same category but the original runtime class/message. | Caller received the dedicated code and `RangeError: issue-2221-runtime`. Extension ACKed command 6, sequence 28, 377 bytes, SHA-256 `40bcc5c07dcb42ef4937a022bf31977028d69da680a4dc359c97cdf9af7cc5a3`, with empty outbox and healthy continuity. The separate daemon row logged `exception_type=RangeError`, `exception_message=issue-2221-runtime`, line 1 column 7. |
| Happy scalar | Evaluate `21 * 2`. Expect number 42 and no error classification. | Runtime readback returned numeric 42 from tab 589710225 with `ready_state=complete`. Extension ACKed command 8, sequence 30, 690 bytes, SHA-256 `5fc0dae1be52a6616a4d2ac75325ef98fe1c26a51181ddefabc3f381814e8707`, then empty outbox. Daemon row matched and logged `response_ok=true`, empty error/exception fields, and `result_type=number`. |

After each trigger, a separate status command was itself ACKed at the next
terminal sequence, proving the same WebSocket remained usable. No disconnect,
reconnect, duplicate terminal, unresolved timeout, storage error, or owner
continuity fault was observed.

The acceptance comment was entered through the real GitHub form. Its dual field
readback matched 2,213 requested bytes with SHA-256
`480542ff1b0f31b20ff30b3a9a9a2570c3f93e1aa81a2784e7547844eb79c6ed`;
a separate page evaluation proved exact equality before submission. A fresh DOM
read found exactly one acceptance marker and the permalink above. After the
real `Close issue` control was clicked, a separate page read found zero close
buttons and exactly one `Reopen issue` button. The owned #2221 tab was then
closed through `chrome.tabs.remove`; an independent `chrome.tabs.get` absence
read returned no affected tab. The original #2174 and Gmail tabs were the only
remaining tabs, and #2174 was restored as the active tab without reading or
mutating Gmail content.

No automated test, mock value, fallback classifier, second browser, branch,
worktree, or alternate target directory was created or used for acceptance.
