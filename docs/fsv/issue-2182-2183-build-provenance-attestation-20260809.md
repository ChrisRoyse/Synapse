# FSV: content-attested build provenance (#2182, #2183)

Date: 2026-08-09 America/Chicago (2026-08-10 UTC)

## Scope and first-principles diagnosis

- #2182: the build stamp described only `HEAD` plus
  `git status --untracked-files=no`. Once the build script emitted any
  `rerun-if-changed` instruction, Cargo watched only `HEAD` and the resolved
  ref. A source edit could therefore rebuild Rust while reusing a stale
  build-script output, and every non-ignored untracked input was excluded by
  construction. The live daemon consequently claimed `clean` and
  `build_matches_checkout=true` while four modified source inputs existed.
- #2183: runtime health initialized `status=ok`. Failure to resolve the current
  checkout populated an unavailable reason and overwrote `detail`, but never
  changed the status. It therefore published an unreadable checkout as `ok`.
  Later probes also overwrote earlier detail instead of aggregating failures.

The fix is a content-addressed v1 input attestation. It enumerates indexed plus
non-ignored untracked direct build inputs, hashes exact bytes in canonical path
order, captures the complete raw Git porcelain-status hash, and embeds counts
plus at most 16 changed path/status/kind/length/SHA-256 identities. It watches
every input and safe source/Git state boundary. Runtime validation aggregates
diagnostics; `error` dominates `degraded`; dirty bytes are an established
mismatch (`false`); unreadable/unknown state leaves the comparison absent.

## Independent research

Research occurred after diagnosis and before implementation.

- Exa lane: `pwsh -File scripts/check-research-lane.ps1` returned `live` for
  `exa-search-server` 3.4.0. Real MCP `web_search_exa` calls covered Cargo
  invalidation/untracked inputs and SLSA input-digest guidance.
- Built-in web lane, primary sources:
  - <https://doc.rust-lang.org/cargo/reference/build-scripts.html#change-detection>
  - <https://git-scm.com/docs/git-status>
  - <https://git-scm.com/docs/git-ls-files>
  - <https://slsa.dev/spec/v1.2/build-provenance>

Cargo documents that any emitted `rerun-if-changed` set becomes the closed
invalidation set and that watched directories are recursively scanned. Git
documents porcelain v1 as stable for scripts, `-z` as raw pathname-safe,
`--untracked-files=all` as the individual untracked inventory, and
`--no-optional-locks` for background callers. `ls-files --cached --others
--exclude-standard` supplies indexed plus non-ignored candidates. SLSA records
resolved build inputs by identity and digest.

## Sources of truth

No return value was accepted alone. The independent readbacks were:

1. live authenticated `health { detail: "full" }` from the installed daemon;
2. the installed executable path, OS process generation, length, and independent
   SHA-256;
3. physical source files independently read and hashed from disk;
4. raw `.git/HEAD`, loose ref, `packed-refs`, branch, and worktree state;
5. Cargo's physical build-script `output` file under the canonical `target/`
   tree, read after each real incremental build;
6. the Calyx vault health, physical lineage journal, vault identity, and
   durable sequence readbacks.

## Build, lint, deployment, and hardware

- `cargo check --workspace`: pass.
- `pwsh -File scripts/lint.ps1`: pass in both the root and separately rooted
  Calyx workspaces, including fmt, cargo-deny, Clippy with `-D warnings`, and
  the public API ratchet.
- Setup used the canonical `C:\code\synapse\target` only, with
  `CARGO_BUILD_JOBS=12` and `CMAKE_BUILD_PARALLEL_LEVEL=12` for the host's 12
  logical processors. The optimized release compiler accumulated thousands of
  CPU-seconds with about 4.4 GB working set during final code generation.
- No NVIDIA PnP device, driver/NVML, or `nvcc` exists. Setup therefore selected
  CPU intentionally. Live health reports `cpu_simd_path=avx2`; the fixed
  dot/cosine/L2/top-k probe matches the portable path bit-for-bit.
- Dirty validation image: PID 11936 after the verified restart, installed at
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, SHA-256
  `8E53B8060EDD2116624C3347109AE188E0FDB1DC80F700FC0BE3D46802217212`.
- Full setup installed and verified the daemon, then intentionally failed
  `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` for this already-running Codex
  process. The documented `-SkipBuild -ForceRestart -SkipClientWiring` pass
  completed green and independently reverified the same installed image.

## Manual happy path: the formerly lying dirty build

Before the trigger, installed image
`BE740C2976B12ACCBB67BE5C93F5F8A15D38B6AF66228014340D84BD7CD60BD9`
ran as PID 7956. The physical worktree had four modified build inputs, but live
health reproduced the defect exactly:

- `build_tree_state=clean`
- `build_matches_checkout=true`
- `build_provenance.status=ok`

After building and deploying the corrected bytes from the identical dirty tree,
live full health reported:

- schema `synapse-build-inputs-v1`;
- 1,446 input files;
- manifest SHA-256
  `ec1fb6d603b3d1b8fb188bdd181138ea4e29aad4aabe55d9dd30f7d643d93a18`;
- raw Git-status SHA-256
  `0874dc3ad36cc78de06252d98ebe119e441c5e038bb19502e83fb6d18195d725`;
- `build_tree_state=dirty`, `build_matches_checkout=false`,
  `status=degraded`, changed count 4, omitted count 0;
- exact `SYNAPSE_BUILD_TREE_DIRTY` diagnostic and remediation.

Each embedded identity was separately read from disk. Length and SHA-256 agreed:

| Input | Bytes | SHA-256 |
|---|---:|---|
| `crates/synapse-core/src/types/health.rs` | 87,516 | `c4282cf072f23413cbb176b28b3586f818b25234abbebaa9b634536ab88a31c1` |
| `crates/synapse-mcp/Cargo.toml` | 5,465 | `80b5daa1f4b87713eae1ea0abf3ebbf772390c2bfb299c39013fda7bbb6d2cf6` |
| `crates/synapse-mcp/build.rs` | 20,654 | `4156bf8af3b552d1e7be7c2a9429f524122fc26171e0d5198122c3568bad2e82` |
| `crates/synapse-mcp/src/server/build_provenance.rs` | 22,274 | `1abaa91bc95549b2ef938e62a87f7d51b914d752a065f00847c33e9d19534343` |

## Boundary and edge-case audit

Every case printed physical state before, during, and after its real trigger.

### 1. Missing checkout metadata is an error, not `ok`

The exact `C:\code\synapse\.git` directory was verified inside the workspace,
moved to one same-parent backup name, queried, and restored in one `try/finally`.

- Before: dirty/degraded and established `false`.
- During: `status=error`, comparison absent,
  `build_checkout_unavailable_reason="C:\code\synapse is not a git checkout on
  this machine"`, with exact remediation.
- After: `.git` present, backup absent, and dirty/degraded/false restored.

### 2. Malformed `HEAD` is an error with the physical cause

`HEAD` was changed through `apply_patch` from symbolic `main` to the 26-byte
`SYNAPSE_FSV_MALFORMED_HEAD`, queried, and immediately restored.

- During: `status=error`, comparison absent, and reason
  `.git\HEAD does not hold a 40-hex object id (found 26 bytes)`.
- After: raw `HEAD=ref: refs/heads/main`, branch `main`, original commit, and
  dirty/degraded state restored.

### 3. Packed symbolic ref remains readable

The loose `refs/heads/main` was physically present before. `git pack-refs
--include refs/heads/main --prune` removed it and wrote the exact commit/ref row
to `packed-refs` while `HEAD` remained symbolic.

- During: loose ref absent, packed row physically present, checkout commit
  resolved exactly, no unavailable reason, dirty/degraded classification
  preserved.
- After: the loose main ref and symbolic main `HEAD` were restored.

### 4. Detached `HEAD` remains readable

`HEAD` was changed through `apply_patch` to the exact 40-hex commit, queried,
and restored without creating a branch or worktree.

- During: the physical 40-hex value equaled `build_checkout_commit`, no
  unavailable reason existed, and dirty/degraded/false remained correct.
- After: raw `HEAD=ref: refs/heads/main` and branch `main`.

### 5. Non-ignored untracked input invalidates and is attested

The smallest synthetic input was a one-line, 41-byte
`scripts/synapse-provenance-fsv-edge.ps1`.

- Before: file absent; physical build output reported 1,446 files / 4 changed.
- During: Git reported `??`; an actual `cargo check -p synapse-mcp` reran the
  build script; output reported 1,447 / 5 and carried
  `kind=untracked`, `status=??`, and SHA-256
  `65ec6dcb8052a305c93ab319a27ea70577e55704506a540d941239bc3e647ec1`.
  A separate disk hash matched.
- After deletion and another actual build: file absent, Git row absent, and
  output restored to 1,446 / 4 with no synthetic identity.

### 6. Harmless tracked input edit invalidates

A one-line documentation-only edit was made inside the tracked build selector
`scripts/check-research-lane.ps1`.

- During: Git reported ` M`; input count remained 1,446; changed count became
  5; the exact path, length 16,935, and disk-matching SHA-256
  `4870a6f10ee3db1814e6faddf577c482db1cdf9a3d61d368c825547273515be5`
  appeared in the physical build output.
- After: the edit was reversed byte-for-byte and `git diff` for that file was
  empty.

### 7. Evidence list bound is exact

Four real changes already existed, so 13 one-line untracked inputs are the
smallest dataset that crosses the 16-entry cap.

- During: physical files=13; Git rows=13; input count=1,459; changed count=17;
  examples=16; omitted=1. The 16th identity was boundary file 12 and boundary
  file 13 was not embedded.
- After all 13 deletions and another build: physical files=0, Git rows=0,
  input count=1,446, changed count=4, examples=4, omitted=0.

### 8. Build with absent Git cannot claim clean

The real `.git` directory was moved to a verified same-parent backup. With the
documented explicit source-tarball opt-in only, a real Cargo build completed.

- Embedded tree state was `unknown`.
- No commit was embedded.
- The exact aggregate error named both `C:\code\synapse is not a git checkout`
  and Git exit 128 `fatal: not a git repository` for input enumeration.
- The directory was restored in `finally`; the backup was absent; branch
  `main`, original commit, and the four expected production modifications were
  independently read afterward.

## Durable-system readback and cleanup

- Calyx vault remained open with ID `01KYJPGWATPD4XNMZY3ERGTKQW`.
- Live latest sequence was 1,305,205; last recovered sequence 1,305,180.
- The sibling lineage journal remained generation 1 with the same vault ID and
  high-water sequence 1,305,180.
- AVX2 math probe status was `ok`; row-guard over-budget and starvation totals
  were zero.
- No temporary FSV input file, Git backup, branch, worktree, alternate target,
  or target directory remained. The checkout ended on symbolic `main`.

## Final clean immutable-build protocol

This FSV record and the implementation are committed together before the final
release build. Setup then rebuilds from that clean immutable commit and the
installed daemon is independently required to report all of the following
before either issue closes: commit equals checkout, tree state `clean`, changed
count/omitted count zero, empty examples, manifest/status digests present,
`build_matches_checkout=true`, `status=ok`, installed-image hash equal to the
running OS image, unchanged vault identity/lineage, and green AVX2 math probe.

The exact post-commit values are recorded in the closing GitHub issue evidence
comments because they cannot exist until after this immutable FSV record is
committed. Any failed assertion leaves both issues open.

Verdict for dirty detection and all boundary cases: PASS. Final issue verdict
remains pending until the clean-commit deployment readback described above.
