# Contributing to Synapse

Thanks for your interest in Synapse. This document covers how to propose changes
and, importantly, the **licensing terms for contributions** — Synapse is
dual-licensed (noncommercial + paid commercial), so contributions need to be
made under terms that keep that model possible.

## Licensing of contributions

By submitting a contribution (a pull request, patch, or any other work) to this
project, you agree that:

1. **You have the right to submit it.** The contribution is your original work,
   or you otherwise have the rights to submit it under these terms, and you sign
   off on it under the [Developer Certificate of Origin](https://developercertificate.org/)
   (use `git commit -s` to add a `Signed-off-by` line).
2. **You license it to the maintainer broadly.** You grant Chris Royse (the
   project maintainer / licensor) a perpetual, worldwide, non-exclusive,
   royalty-free, irrevocable copyright and patent license to use, reproduce,
   modify, distribute, and **sublicense** your contribution, **including the
   right to license it under the project's [PolyForm Noncommercial
   License](LICENSE.md) and under separate paid [commercial
   licenses](COMMERCIAL-LICENSE.md)**.
3. **You keep your rights too.** You retain copyright in your contribution and
   may use it elsewhere. This grant is in addition to your own rights, not a
   transfer of them.

If you cannot agree to these terms, please do not submit a contribution. If your
employer has rights to work you create, make sure you have permission to
contribute under these terms before doing so.

## Before you start

For anything larger than a small fix, **open an issue first** to discuss the
approach. This avoids wasted work on changes that don't fit the architecture or
the roadmap (see the README "What's left on the docket" section).

## Development workflow

1. Use the toolchain pinned in `rust-toolchain.toml` (currently Rust 1.97.1);
   rustup installs it automatically. The exact version is pinned so `cargo fmt`
   and `cargo clippy` are reproducible across contributors.
2. Build and check the workspace:
   ```bash
   cargo check --workspace
   cargo fmt --all --check
   cargo clippy --workspace --all-targets
   ```
   The absorbed Calyx code lives in its own nested workspace under `calyx/`.
   When touching it, run the Calyx workspace gates separately:
   ```bash
   cargo check --manifest-path calyx/Cargo.toml --workspace
   cargo fmt --manifest-path calyx/Cargo.toml --all
   cargo clippy --manifest-path calyx/Cargo.toml --workspace --all-targets
   ```
   The optional standalone Calyx CUDA compile probe on Windows needs `nvcc`
   plus an MSVC Hostx64 compiler directory supplied through `NVCC_CCBIN`.
   CUDA 13.x also requires `NVCC_APPEND_FLAGS` to include
   `-Xcompiler=/Zc:preprocessor` so dependency CUDA kernels compile with MSVC's
   conforming preprocessor. Configure those variables manually only for that
   explicit probe. Standard `scripts/synapse-setup.ps1` does not detect or
   publish NVCC variables, does not enable `calyx-cuda`, and rejects accelerator
   requests under the installed daemon's CPU-only/no-explicit-GPU contract.
   The standalone compile probe is:
   ```bash
   cargo check --manifest-path calyx/Cargo.toml --workspace --features "calyx-assay/cuda calyx-loom/cuda calyx-registry/cuda calyx-search/cuda calyx-sextant/cuda"
   ```
3. **Enable the local pre-push gate (once per clone):**
   ```bash
   git config core.hooksPath .githooks
   ```
   This repo uses no CI (GitHub Actions are forbidden), so `.githooks/pre-push`
   is the automated backstop: it runs root fmt/clippy for root Rust/Cargo
   changes and separate Calyx fmt/clippy with `--manifest-path calyx/Cargo.toml`
   for `calyx/` Rust/Cargo changes (it skips docs-only pushes). It is a fast
   compile+lint gate, never a behavioral acceptance gate. Bypass only for a
   genuine non-compiling emergency with
   `git push --no-verify`.
4. **Iterate with fast builds.** Use `cargo check` (~15 s) or `cargo build`
   (dev, ~45 s incremental) for the edit loop. Run `cargo build --release` ONLY
   to ship/run the real `synapse-mcp.exe` daemon — never as compile feedback.
   See [docs/BUILD-AND-MAINTENANCE.md](docs/BUILD-AND-MAINTENANCE.md) for the
   tuned profiles (rust-lld, thin-LTO release, `--profile release-max` for the
   max-runtime binary) and the worktree/`target` disk-hygiene tooling
   (`scripts/repo-maintenance.ps1`) — don't leave throwaway worktrees with their
   own multi-GB `target/` lying around.
4. Synapse is **Windows-native** for its real perception/action paths (Win32
   `SendInput`, UI Automation, and GDI `BitBlt` visible-surface capture; there is
   no WGC/DXGI capture backend). Behavior that touches those
   surfaces should be verified on Windows — the project uses manual Full State
   Verification (FSV) on the configured Windows host as the shipping gate (see the
   README "Agent Doctrine" section). Automated tests are not part of the
   acceptance surface.
5. Keep commits focused and write clear messages. Reference the issue number
   where applicable.
6. **Check your research lane before you rely on it.** The workflow expects
   independent best-practice research after a defect is diagnosed and before a
   fix is proposed. The optional Exa MCP lane has repeatedly turned out to be
   dead at the moment of use (issues #1864, #1856, #1833), costing each session a
   fresh re-diagnosis. Probe it up front instead:
   ```powershell
   pwsh -File scripts\check-research-lane.ps1
   ```
   It drives the configured server through a real stdio JSON-RPC
   `initialize`/`tools/list`/`tools/call` — registration is not service, so it
   issues a real query — and writes a structured verdict to
   `%TEMP%\synapse-research-lane-readback.json`. Launcher faults are reported
   separately from service verdicts, so a broken `npx` is never mistaken for an
   Exa outage.

   Exa is a **supplement, not a prerequisite**: ordinary web search/fetch against
   primary sources (Microsoft Learn for Win32 semantics, the Rust and Cargo
   books, RFCs, upstream project docs) satisfies the research requirement on its
   own. An Exa outage is not a reason to stop work. Record which lane you used.

## Pull requests

- Keep PRs scoped to one logical change.
- Include the reasoning and any verification you performed.
- Make sure `cargo fmt --check` and the build pass locally before requesting
  review.
- Sign off your commits (`git commit -s`) to assert the DCO and the contribution
  terms above.

## Code of conduct

This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). By participating,
you agree to uphold it.

## Reporting security issues

Do **not** open a public issue for security problems. See
[SECURITY.md](SECURITY.md).
