# Synapse durable webhook receiver contract

Synapse escalation egress accepts only canonical HTTPS
`synapse_receipt_v1` receivers. An
ordinary webhook endpoint, including the historical `synapse_echo_v1` shape,
is not sufficient: echoing an idempotency key does not prove that the receiver
durably deduplicates the remote side effect.

## Receiver transaction boundary

For each logical delivery, the receiver must use `X-Synapse-Delivery-Id` (also
sent as `Idempotency-Key`) as a unique key in durable storage. In one receiver
transaction it must either:

1. persist the delivery ID, `X-Synapse-Body-SHA256`, exact request body, and
   downstream work/outcome before acknowledging `committed`; or
2. make no durable mutation or downstream side effect before acknowledging
   `not_committed`.

A duplicate delivery ID with the same body digest returns its existing durable
receipt without repeating downstream work. The same delivery ID with a
different body digest is a protocol error and must not mutate receiver state.
The receiver must retain deduplication records for at least the maximum Synapse
escalation/outbox retention horizon.

## OPTIONS preflight

Synapse sends an `OPTIONS` request with:

- `X-Synapse-Idempotency-Protocol: synapse_receipt_v1`
- `X-Synapse-Delivery-Id: <stable delivery ID>`
- `Idempotency-Key: <the same delivery ID>`
- `X-Synapse-Body-SHA256: <lowercase SHA-256 hex>`

A ready receiver returns a successful status and echoes the first, second, and
fourth headers exactly, plus:

`X-Synapse-Receipt-State: durable_idempotency_ready`

Synapse performs no POST when this proof is absent or contradictory.
Each required response header must occur exactly once; duplicated header
values fail the contract.

## POST receipt

The POST carries the same identity headers and the exact JSON bytes whose
digest was preflighted. Every response that claims a conclusive outcome echoes
the protocol, delivery ID, and body digest exactly.

- A 2xx response must carry `X-Synapse-Receipt-State: committed` and means the
  receiver transaction is durably committed.
- A non-2xx response must carry
  `X-Synapse-Receipt-State: not_committed` and guarantees no receiver mutation
  or downstream side effect occurred.
- A missing, malformed, or contradictory receipt is an unknown terminal
  outcome. Synapse records it and does not retry.
- A transport failure after POST is unknown. Any numbered retry keeps the same
  delivery ID and exact body, so the receiver's durable uniqueness constraint
  remains the authority.

## Optional HMAC authentication

When a channel secret is configured, Synapse sends:

- `X-Synapse-Signature: v2=<lowercase HMAC-SHA256 hex>`
- `X-Synapse-Signature-Timestamp-Ms: <persisted attempt start, Unix ms>`
- `X-Synapse-Signature-Audience: <exact configured canonical HTTPS URL>`
- `X-Synapse-Signature-Max-Age-Ms: 300000`

The HMAC input is this byte sequence:

1. ASCII domain separator `synapse.webhook.signature.v2` followed by one NUL;
2. each component below, in order, encoded as an unsigned 64-bit big-endian
   byte length followed by the exact component bytes:
   - `POST`
   - the exact `X-Synapse-Signature-Audience` header bytes
   - `synapse_receipt_v1`
   - delivery ID
   - decimal signature timestamp in Unix milliseconds
   - decimal signature maximum age in milliseconds (`300000`)
   - lowercase body SHA-256 hex
   - exact request body bytes

Receivers must compare the HMAC in constant time, reject timestamps outside
their documented replay window, and still enforce the durable delivery-ID
constraint. Signature validity alone is not deduplication. Channel secrets must
contain 32–512 UTF-8 bytes. New receivers must use HTTPS, and the configured
URL must already equal the parser's canonical serialization. URL userinfo,
queries, and fragments are rejected; credentials belong in the secret field.
The receiver must compare the explicit audience to its configured public
endpoint rather than reconstructing it from proxy headers. HTTPS authenticates
the OPTIONS readiness response and POST receipt as well as protecting them in
transit.

## Manual verification Sources of Truth

Acceptance requires a real configured receiver, not a mock or an automated FSV
harness. For each manual trigger, inspect separately:

- Synapse Calyx `CF_KV` item, outbox, and audit rows;
- the receiver's physical receipt/deduplication table or durable file;
- the receiver's physical downstream side-effect record; and
- structured Synapse logs naming delivery ID, attempt, non-secret endpoint
  identity, committed Calyx sequence, and remediation.

The minimum cases are a successful commit, duplicate delivery replay, shutdown
or timeout after POST begins, safely retryable `not_committed`, malformed
receipt, reused delivery ID with a different body, invalid signature, and stale
signature timestamp.
