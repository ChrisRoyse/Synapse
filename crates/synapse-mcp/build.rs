//! Stamps the daemon binary with the provenance of the tree it was built from
//! (issue #1971 finding 1).
//!
//! Before this existed, `health.build` read `option_env!("VERGEN_GIT_SHA")` and
//! fell back to the literal string `"dev"`. No build script ever set that
//! variable, so the fallback was the only value the field had ever taken: the
//! daemon published a provenance field that carried no provenance, and it looked
//! deliberate. A daemon running bytes many commits behind `main` reported
//! exactly what a current one did, so the drift was only discoverable by
//! deploying — which is how a #1968 perf change ended up shipping an unrelated
//! backlog and taking recall to zero.
//!
//! This script resolves the commit by reading `.git` directly rather than
//! shelling out to `git`, so it costs no subprocess and works when `git` is not
//! on PATH. It **fails the build** when it cannot determine provenance: a
//! silently-unknown build stamp is the defect being fixed, so reintroducing one
//! as a fallback is not available. Set
//! `SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1` to build outside a git checkout
//! (a source tarball); the resulting binary reports `status=error` on its
//! `build_provenance` health subsystem rather than pretending to be a known
//! build.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        panic!(
            "SYNAPSE_BUILD_MANIFEST_DIR_UNSET: cargo did not set CARGO_MANIFEST_DIR, so this build \
             script cannot locate the checkout to stamp provenance from"
        );
    };
    let manifest_dir = PathBuf::from(manifest_dir);
    // crates/synapse-mcp -> the workspace root that owns .git
    let source_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&manifest_dir)
        .to_path_buf();

    let build_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    emit("SYNAPSE_BUILD_UNIX_MS", &build_unix_ms.to_string());
    emit(
        "SYNAPSE_BUILD_SOURCE_DIR",
        &source_dir.display().to_string(),
    );
    emit(
        "SYNAPSE_BUILD_PROFILE",
        &std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned()),
    );

    match resolve_head(&source_dir) {
        Ok(head) => {
            for path in head.rerun_paths {
                println!("cargo::rerun-if-changed={}", path.display());
            }
            emit("SYNAPSE_BUILD_COMMIT", &head.commit);
            emit("SYNAPSE_BUILD_COMMIT_SHORT", &head.commit[..12]);
            emit("SYNAPSE_BUILD_REF", &head.reference);
            // A build taken from a dirty tree is not reproducible from its
            // commit, and this repo has a known way to produce one: editing
            // source while `synapse-setup` is mid-deploy changes the bytes that
            // get installed. Reported as `unknown` (never as `clean`) when `git`
            // is unavailable, because a clean-looking unknown is the same lie
            // this whole change exists to remove.
            emit("SYNAPSE_BUILD_TREE_STATE", &tree_state(&source_dir));
        }
        Err(reason) => {
            if std::env::var("SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE").as_deref() == Ok("1") {
                println!(
                    "cargo::warning=synapse-mcp is being built with unknown git provenance \
                     ({reason}); the daemon will report build_provenance status=error"
                );
                return;
            }
            panic!(
                "SYNAPSE_BUILD_PROVENANCE_UNRESOLVED: cannot determine the commit this \
                 synapse-mcp binary is being built from: {reason}. A daemon that cannot name its \
                 own commit makes binary/checkout drift undetectable, which is issue #1971. \
                 Build from a git checkout, or set SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1 to \
                 accept a binary that reports its provenance as an error at runtime."
            );
        }
    }
}

/// `clean` | `dirty` | `unknown`, from `git status --porcelain`.
fn tree_state(source_dir: &Path) -> String {
    match std::process::Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
    {
        Ok(output) if output.status.success() => {
            if output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
                "dirty".to_owned()
            } else {
                "clean".to_owned()
            }
        }
        _ => "unknown".to_owned(),
    }
}

struct Head {
    commit: String,
    reference: String,
    rerun_paths: Vec<PathBuf>,
}

/// Resolves `.git/HEAD` to a full 40-hex commit id without spawning `git`.
///
/// Handles the three shapes that occur in practice: a symbolic ref to a loose
/// ref file, a symbolic ref resolved only through `packed-refs`, and a detached
/// HEAD holding a raw object id. A worktree or submodule `.git` *file* is
/// followed to its real git directory.
fn resolve_head(source_dir: &Path) -> Result<Head, String> {
    let git_path = source_dir.join(".git");
    let git_dir = if git_path.is_file() {
        let contents = std::fs::read_to_string(&git_path)
            .map_err(|error| format!("read {}: {error}", git_path.display()))?;
        let target = contents
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:"))
            .ok_or_else(|| format!("{} has no gitdir: line", git_path.display()))?
            .trim();
        let target = PathBuf::from(target);
        if target.is_absolute() {
            target
        } else {
            source_dir.join(target)
        }
    } else if git_path.is_dir() {
        git_path
    } else {
        return Err(format!("{} is not a git checkout", source_dir.display()));
    };

    let head_path = git_dir.join("HEAD");
    let head = std::fs::read_to_string(&head_path)
        .map_err(|error| format!("read {}: {error}", head_path.display()))?;
    let head = head.trim();
    let mut rerun_paths = vec![head_path.clone()];

    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        // Detached HEAD: the object id is the file's whole contents.
        let commit = validate_object_id(head, &head_path)?;
        return Ok(Head {
            commit,
            reference: "HEAD".to_owned(),
            rerun_paths,
        });
    };

    let ref_path = git_dir.join(reference);
    if ref_path.is_file() {
        let contents = std::fs::read_to_string(&ref_path)
            .map_err(|error| format!("read {}: {error}", ref_path.display()))?;
        rerun_paths.push(ref_path.clone());
        let commit = validate_object_id(contents.trim(), &ref_path)?;
        return Ok(Head {
            commit,
            reference: reference.to_owned(),
            rerun_paths,
        });
    }

    // A ref that has been packed has no loose file; `packed-refs` is the only
    // remaining source of truth and is not optional.
    let packed_path = git_dir.join("packed-refs");
    let packed = std::fs::read_to_string(&packed_path).map_err(|error| {
        format!(
            "ref {reference} has no loose file at {} and {} could not be read: {error}",
            ref_path.display(),
            packed_path.display()
        )
    })?;
    rerun_paths.push(packed_path.clone());
    for line in packed.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let Some((object_id, name)) = line.split_once(' ') else {
            continue;
        };
        if name.trim() == reference {
            let commit = validate_object_id(object_id, &packed_path)?;
            return Ok(Head {
                commit,
                reference: reference.to_owned(),
                rerun_paths,
            });
        }
    }
    Err(format!(
        "ref {reference} appears in neither {} nor {}",
        ref_path.display(),
        packed_path.display()
    ))
}

fn validate_object_id(candidate: &str, source: &Path) -> Result<String, String> {
    let candidate = candidate.trim();
    if candidate.len() == 40 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(candidate.to_ascii_lowercase())
    } else {
        Err(format!(
            "{} does not hold a 40-hex object id (found {} bytes)",
            source.display(),
            candidate.len()
        ))
    }
}

fn emit(key: &str, value: &str) {
    println!("cargo::rustc-env={key}={value}");
}
