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
bun run test:coverage
bun run build:storybook
bun run test:visual
bun run test:a11y
```

These are local supporting gates only. They do not replace manual Synapse FSV.

`test:visual` starts local Storybook and snapshots every case in
`dashboard/tests/storybook-cases.ts` across dark/light and comfortable/compact
globals. Screenshots target the Storybook root and allow at most
`maxDiffPixelRatio: 0.002` or `maxDiffPixels: 96`; update baselines only with an
explicit `bun run test:visual:update`.

The reproducible runner image is pinned to:

```text
mcr.microsoft.com/playwright:v1.60.0-noble
```

Storybook is pinned to `9.1.20`. The current `@storybook/test-runner` package
advertises Storybook 10 peers, so this workspace uses Playwright Test directly
against Storybook iframe URLs instead of adding a mismatched runner dependency.
