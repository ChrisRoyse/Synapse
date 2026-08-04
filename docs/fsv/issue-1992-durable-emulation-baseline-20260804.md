# Issue #1992: durable emulation baseline FSV (2026-08-04)

## Source of truth

The pre/post state is the owned Chrome tab's MAIN-world `innerWidth`,
`innerHeight`, and `devicePixelRatio`, independently read through
`browser_debugger.evaluate`. Worker replacement is proven by the authenticated
bridge host id and deployed service-worker SHA.

## Root cause and research

The durable override journal loaded successfully, but its rows were never
rehydrated into any of the six in-memory owner maps. After an MV3 worker reload,
the next viewport `set` saw an empty map, measured the already-emulated page as a
new baseline, and overwrote the genuine durable baseline during owner merge.

Exa MCP and built-in web research used Chrome's primary MV3 lifecycle and
migration documentation. Both require persistent storage rather than global
variables because service-worker globals are lost on termination.

The fix hydrates viewport, device, geolocation, locale, media, and network owner
maps from the durable journal after browser-session continuity is proven. A row
with no baseline/origin or a live/durable conflict fails closed and disables
mutation admission instead of inventing state.

## Happy path

Target `chrome-tab:589710125`, Example Domain:

1. native before read: `{w:1046,h:784,dpr:1.25}`;
2. set viewport: read `{w:360,h:800,dpr:2}`;
3. reload extension: authenticated replacement host
   `chrome-native-0-1785820262306`; after-reload read stayed `{360,800,2}`;
4. repeat set (the historical overwrite trigger): read stayed `{360,800,2}`;
5. reset: method readback explicitly restored preserved baseline
   `width=1046,height=784,deviceScaleFactor=1.25`;
6. independent after read: `{w:1046,h:784,dpr:1.25}`.

## Boundary audit

1. Reset with no active owner was idempotent: before/after remained
   `{1046,784,1.25}`.
2. Combined viewport+device set returned `TOOL_PARAMS_INVALID`; after read kept
   native viewport and the real Chrome UA (not the requested `FSV-UA`).
3. Scale 1001 returned `TOOL_PARAMS_INVALID`; after read remained
   `{1046,784,1.25}`.

## Verification

- deployed service-worker SHA-256:
  `76f5b220c8704b09dc00f46df733a502fe7801c2bebb56bcf8c93d82ddc58f94`;
- installed daemon PID 9472, SHA-256
  `A9B8429E6B5374479CF2EEBCE0B59E48AF8007704C3C5C4430BE2B9BC177CFE7`;
- `cargo check --workspace` and all seven `scripts/lint.ps1` gates passed.

