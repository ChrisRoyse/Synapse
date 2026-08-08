# Synapse Command Center Dashboard

Local-only browser dashboard for the Synapse daemon.

## Build

```powershell
bun install --frozen-lockfile
bun run build
```

The build writes committed, hashed static assets to `dashboard/dist/`. The Rust daemon embeds those
files and serves them on loopback under `/dashboard`; Bun, Vite, and Node-compatible tooling are
build-time only and are not part of the runtime.

## Local Checks

```powershell
bun run check
bun run build:storybook
```

These are structural build/charter checks only. They do not verify behavior and
do not replace manual Synapse Full State Verification. Visual and accessibility
acceptance is performed manually in the already-running Chrome session: read
the rendered dashboard state before the trigger, perform the real interaction,
then independently inspect the rendered state and daemon Source of Truth after
the trigger. Storybook remains a local component-inspection surface, including
its interactive accessibility panel; it is not an automated test runner.
