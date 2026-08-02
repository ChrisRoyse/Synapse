#!/usr/bin/env python3
"""Calyx facade reachability gate (issue #1944 ask 3).

#1944 established the failure mode: `calyx_assay::ensemble_card` had no caller
anywhere in Synapse, and accumulated three separate defects (#1942, #1943, and
a whole-pass abort on one degenerate lens) at full rate while reporting none of
them. An unreached code path is a defect reservoir with no observation.

It was found by accident. The sweep that followed found 116 more names in the
same state. This script is the durable form of that sweep: it recomputes the
unreferenced set on demand and **fails when the set grows**, so a newly
unreached facade item cannot appear silently. Every currently-unreached name
must carry a one-line reason in the allow-list beside this file.

What it measures, stated precisely so the number is not over-read:

  declared   = names re-exported at a calyx crate root (`pub use ...` in
               `calyx/crates/*/src/lib.rs`). This is each crate's *declared*
               API surface, not its internal helpers.
  referenced = every identifier that occurs anywhere in `crates/**/*.rs`
               (the Synapse workspace).

`referenced` counts an occurrence in a comment or a string as a reference, on
purpose. That makes the unreferenced set a strict **lower bound on deadness**:
a name on the list is one Synapse never even mentions. It does not catch a
facade item reached only through a trait method or a re-export chain, so the
true unreached surface is at least this large. The gate is therefore sound in
the direction that matters -- it never invents deadness -- and conservative in
the other.

Exit codes: 0 clean, 1 the unreferenced set grew, 2 usage/IO error.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ALLOW_FILE_NAME = "calyx-facade-reach-allow.json"

# `pub use a::b::{C, D as E, f};` / `pub use a::B;` including multi-line forms.
PUB_USE = re.compile(r"^\s*pub use\s+(?P<body>[^;]+);", re.MULTILINE)
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def declared_names(lib_rs: Path) -> set[str]:
    """Names a crate re-exports at its root."""
    try:
        text = lib_rs.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:  # pragma: no cover - surfaced by the caller
        raise SystemExit(f"CALYX_FACADE_REACH_READ_FAILED {lib_rs}: {exc}") from exc

    names: set[str] = set()
    for match in PUB_USE.finditer(text):
        body = match.group("body")
        # Take the leaf of each path segment group, honouring `as` aliases.
        for chunk in re.split(r"[{},]", body):
            chunk = chunk.strip()
            if not chunk or chunk == "*":
                continue
            if " as " in chunk:
                chunk = chunk.split(" as ")[-1].strip()
            leaf = chunk.split("::")[-1].strip()
            if not leaf or leaf == "*":
                continue
            if not IDENT.fullmatch(leaf):
                continue
            if leaf in {"self", "crate", "super", "pub", "use"}:
                continue
            # SCREAMING_SNAKE constants are excluded. #1944 is about *code
            # paths* that accumulate defects while unobserved; a `CALYX_*`
            # error-code string cannot accumulate a defect, and including them
            # buries the types and functions that can under an order of
            # magnitude of inert names.
            if leaf.isupper() or (leaf.upper() == leaf and "_" in leaf):
                continue
            names.add(leaf)
    return names


def referenced_identifiers(roots: list[Path]) -> set[str]:
    """Every identifier occurring anywhere in the Synapse workspace sources."""
    seen: set[str] = set()
    for root in roots:
        for path in root.rglob("*.rs"):
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            seen.update(IDENT.findall(text))
    return seen


def collect(repo: Path) -> dict[str, set[str]]:
    """Maps each calyx crate name to its unreferenced declared names."""
    calyx_crates = sorted((repo / "calyx" / "crates").glob("*/src/lib.rs"))
    if not calyx_crates:
        raise SystemExit(
            "CALYX_FACADE_REACH_NO_CRATES: found no calyx/crates/*/src/lib.rs "
            f"under {repo}. This gate fails closed rather than reporting a "
            "clean sweep it did not perform."
        )
    referenced = referenced_identifiers([repo / "crates"])
    if not referenced:
        raise SystemExit(
            "CALYX_FACADE_REACH_NO_SOURCES: found no .rs files under "
            f"{repo / 'crates'}. Refusing to report every facade name as "
            "unreached on the basis of an empty scan."
        )

    result: dict[str, set[str]] = {}
    for lib_rs in calyx_crates:
        crate = lib_rs.parent.parent.name
        result[crate] = declared_names(lib_rs) - referenced
    return result


def load_allow(path: Path) -> dict[str, str]:
    if not path.exists():
        return {}
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise SystemExit(f"CALYX_FACADE_REACH_ALLOW_UNREADABLE {path}: {exc}") from exc
    entries = raw.get("unreferenced", {})
    if not isinstance(entries, dict):
        raise SystemExit(
            f"CALYX_FACADE_REACH_ALLOW_MALFORMED {path}: 'unreferenced' must be "
            "an object mapping 'crate::Name' to a one-line reason"
        )
    blank = [k for k, v in entries.items() if not isinstance(v, str) or not v.strip()]
    if blank:
        raise SystemExit(
            "CALYX_FACADE_REACH_ALLOW_UNREASONED: these entries carry no "
            f"reason, which defeats the point of the list: {sorted(blank)}"
        )
    return entries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parent.parent))
    parser.add_argument(
        "--write-baseline",
        action="store_true",
        help="Rewrite the allow-list from the current sweep, preserving every "
        "existing reason and stamping new entries TODO. Review the diff.",
    )
    parser.add_argument("--report", action="store_true", help="Print the full table.")
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    allow_path = Path(__file__).resolve().parent / ALLOW_FILE_NAME
    unreferenced = collect(repo)

    current = {
        f"{crate}::{name}"
        for crate, names in unreferenced.items()
        for name in names
    }

    if args.write_baseline:
        existing = load_allow(allow_path)
        merged = {
            key: existing.get(key, "TODO: state why this is not wired yet, or wire it")
            for key in sorted(current)
        }
        allow_path.write_text(
            json.dumps(
                {
                    "_doc": (
                        "Calyx facade names with no reference anywhere in "
                        "crates/. Each needs a one-line reason (#1944 ask 3). "
                        "Regenerate with scripts/calyx_facade_reach.py "
                        "--write-baseline, but prefer wiring the path over "
                        "adding a line here."
                    ),
                    "unreferenced": merged,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        print(f"   wrote {allow_path} with {len(merged)} entries")
        return 0

    allow = load_allow(allow_path)
    allowed = set(allow)
    grew = sorted(current - allowed)
    shrank = sorted(allowed - current)

    total_declared = sum(
        len(declared_names(p)) for p in (repo / "calyx" / "crates").glob("*/src/lib.rs")
    )
    print(
        f"   calyx facade names declared={total_declared} "
        f"unreferenced={len(current)} allow-listed={len(allowed)}"
    )

    if args.report:
        for crate in sorted(unreferenced):
            names = sorted(unreferenced[crate])
            if names:
                print(f"     {crate}: {len(names)} -> {', '.join(names)}")

    if shrank:
        print(
            f"   REPORT  {len(shrank)} allow-listed name(s) are now referenced "
            "or no longer declared; delete these lines to keep the list exact:"
        )
        for key in shrank:
            print(f"             {key}")

    if grew:
        print(
            f"   FAIL    CALYX_FACADE_REACH_GREW: {len(grew)} calyx facade "
            "name(s) became unreachable from crates/ without a recorded reason."
        )
        for key in grew:
            print(f"             {key}")
        print(
            "   remediation : wire the path from crates/, or add it to "
            f"scripts/{ALLOW_FILE_NAME} with a one-line reason "
            "(e.g. \"CUDA-only, see #1906\"). An unreached path accumulates "
            "defects and reports none of them -- that is #1944."
        )
        return 1

    print("   OK   no calyx facade name became unreachable without a reason")
    return 0


if __name__ == "__main__":
    sys.exit(main())
