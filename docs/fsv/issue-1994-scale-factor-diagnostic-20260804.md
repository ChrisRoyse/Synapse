# Issue #1994: scale-factor diagnostic FSV (2026-08-04)

Source of truth is the live daemon error envelope plus independent MAIN-world
viewport state.

- before: `{w:1046,h:784,dpr:1.25}`;
- trigger: set viewport with `device_scale_factor=0`;
- result: `TOOL_PARAMS_INVALID` with
  `device_scale_factor must be finite, greater than 0, and at most 1000`;
- after: `{w:1046,h:784,dpr:1.25}` (no mutation).

Installed daemon PID 9472, SHA-256
`A9B8429E6B5374479CF2EEBCE0B59E48AF8007704C3C5C4430BE2B9BC177CFE7`.
The full two-workspace lint gate passed with the implementation.
