//! Publishes the running binary's provenance, and the drift between it and the
//! checkout it was built from (issue #1971 finding 1).
//!
//! ## Why this exists
//!
//! Deploying `84dd8c9b` — a perf change touching no panel code — took recall to
//! zero. The direct cause was three un-migrated call sites, but the reason it
//! was *invisible until a deploy* was that the running daemon predated two
//! merged commits and nothing said so. `health.build` read
//! `option_env!("VERGEN_GIT_SHA")` and fell back to `"dev"`; no build script
//! ever set that variable, so `"dev"` was the only value that field had ever
//! held. Every subsystem reported `ok` throughout, correctly — each was fine
//! *for the code it was running*. The delta accumulated silently and the first
//! deploy applied all of it at once.
//!
//! ## What is published
//!
//! `build.rs` stamps the commit, ref, tree state, build time and source
//! directory into the binary. At health time this module reads that stamp, then
//! reads the *current* `HEAD` of that same checkout straight out of `.git` and
//! compares. Drift between `main` and the running daemon becomes a boolean an
//! operator can read at a glance instead of a fact discovered by deploying.
//!
//! Resolving `HEAD` is two small file reads and no subprocess, so it is cheap
//! enough to do on every health call. It deliberately does not shell out to
//! `git`: `health` must never be able to block on a process spawn.
//!
//! ## Fail-closed
//!
//! A binary that cannot name its own commit reports `error`, not `unknown`
//! dressed up as fine. A checkout that cannot be read reports the reason
//! verbatim and leaves `build_matches_checkout` absent — absent means "not
//! established", which is different from `false`, and collapsing the two is the
//! defect this module exists to remove.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use synapse_core::types::SubsystemHealth;

/// Commit the running binary was built from, or `None` when it was built with
/// `SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1`.
const BUILD_COMMIT: Option<&str> = option_env!("SYNAPSE_BUILD_COMMIT");
const BUILD_REF: Option<&str> = option_env!("SYNAPSE_BUILD_REF");
const BUILD_TREE_STATE: Option<&str> = option_env!("SYNAPSE_BUILD_TREE_STATE");
const BUILD_UNIX_MS: Option<&str> = option_env!("SYNAPSE_BUILD_UNIX_MS");
const BUILD_PROFILE: Option<&str> = option_env!("SYNAPSE_BUILD_PROFILE");
const BUILD_SOURCE_DIR: Option<&str> = option_env!("SYNAPSE_BUILD_SOURCE_DIR");

/// The 12-hex short commit published in `health.build`.
///
/// Returns the sentinel `unknown-provenance` rather than `dev` when the stamp is
/// missing. `dev` reads as a deliberate development build; `unknown-provenance`
/// reads as what it is, and the `build_provenance` subsystem reports `error`
/// alongside it.
#[must_use]
pub fn short_commit() -> String {
    option_env!("SYNAPSE_BUILD_COMMIT_SHORT")
        .unwrap_or("unknown-provenance")
        .to_owned()
}

/// Builds the `build_provenance` health subsystem.
#[must_use]
pub fn health_subsystem() -> SubsystemHealth {
    let mut health = SubsystemHealth {
        status: "ok".to_owned(),
        build_commit: BUILD_COMMIT.map(str::to_owned),
        build_ref: BUILD_REF.map(str::to_owned),
        build_tree_state: BUILD_TREE_STATE.map(str::to_owned),
        build_unix_ms: BUILD_UNIX_MS.and_then(|value| value.parse().ok()),
        build_profile: BUILD_PROFILE.map(str::to_owned),
        build_source_dir: BUILD_SOURCE_DIR.map(str::to_owned),
        ..SubsystemHealth::default()
    };

    match running_executable() {
        Ok((path, len, modified_unix_ms)) => {
            health.build_exe_path = Some(path.display().to_string());
            health.build_exe_len = Some(len);
            health.build_exe_modified_unix_ms = modified_unix_ms;
        }
        Err(reason) => {
            // Not fatal to the reading, but it must be said rather than
            // silently omitted.
            health.detail = Some(format!(
                "the running executable's own path could not be read: {reason}"
            ));
        }
    }

    let Some(build_commit) = BUILD_COMMIT else {
        health.status = "error".to_owned();
        health.detail = Some(
            "SYNAPSE_BUILD_PROVENANCE_UNKNOWN: this binary was built with \
             SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1, so it cannot name the commit it was \
             compiled from and drift against the checkout cannot be measured; remediation=rebuild \
             from a git checkout with scripts/synapse-setup.ps1 -SourceDir <checkout>"
                .to_owned(),
        );
        return health;
    };

    let Some(source_dir) = BUILD_SOURCE_DIR else {
        health.status = "error".to_owned();
        health.detail = Some(
            "SYNAPSE_BUILD_SOURCE_DIR_UNSTAMPED: the binary names its commit but not the checkout \
             it came from, so drift cannot be measured; remediation=rebuild"
                .to_owned(),
        );
        return health;
    };

    match resolve_head(Path::new(source_dir)) {
        Ok(checkout_commit) => {
            let matches = checkout_commit == build_commit;
            health.build_checkout_commit = Some(checkout_commit.clone());
            health.build_matches_checkout = Some(matches);
            if !matches {
                health.status = "degraded".to_owned();
                health.detail = Some(format!(
                    "SYNAPSE_BUILD_CHECKOUT_DRIFT: the running daemon was built from {build_commit} \
                     but the checkout at {source_dir} is now on {checkout_commit}, so the daemon is \
                     serving bytes that predate the source tree and every subsystem below reports \
                     health for the *older* code; remediation=redeploy with \
                     scripts/synapse-setup.ps1 -SourceDir {source_dir}, and expect the whole \
                     accumulated delta to apply at once"
                ));
            } else if BUILD_TREE_STATE == Some("dirty") {
                health.status = "degraded".to_owned();
                health.detail = Some(format!(
                    "SYNAPSE_BUILD_TREE_DIRTY: the running daemon was built from {build_commit} \
                     with uncommitted changes in {source_dir}, so its bytes are not reproducible \
                     from that commit and the checkout no longer shows what is running; \
                     remediation=commit the working tree and redeploy"
                ));
            }
        }
        Err(reason) => {
            // Absent, not `false`. "Could not be established" and "established
            // as different" are different facts and must not collapse.
            health.build_checkout_unavailable_reason = Some(reason.clone());
            health.detail = Some(format!(
                "SYNAPSE_BUILD_CHECKOUT_UNREADABLE: the daemon names its own commit \
                 ({build_commit}) but the checkout it was built from could not be read, so drift \
                 is not established either way: {reason}"
            ));
        }
    }

    health
}

fn running_executable() -> Result<(PathBuf, u64, Option<u64>), String> {
    let path = std::env::current_exe().map_err(|error| error.to_string())?;
    let metadata =
        std::fs::metadata(&path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
    Ok((path, metadata.len(), modified))
}

/// Resolves a checkout's current `HEAD` to a 40-hex commit id by reading `.git`.
///
/// Mirrors the resolution `build.rs` performs at compile time so the two values
/// being compared are produced the same way. Never spawns a process.
fn resolve_head(source_dir: &Path) -> Result<String, String> {
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
        return Err(format!(
            "{} is not a git checkout on this machine",
            source_dir.display()
        ));
    };

    let head_path = git_dir.join("HEAD");
    let head = std::fs::read_to_string(&head_path)
        .map_err(|error| format!("read {}: {error}", head_path.display()))?;
    let head = head.trim();

    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return validate_object_id(head, &head_path);
    };

    let ref_path = git_dir.join(reference);
    if ref_path.is_file() {
        let contents = std::fs::read_to_string(&ref_path)
            .map_err(|error| format!("read {}: {error}", ref_path.display()))?;
        return validate_object_id(contents.trim(), &ref_path);
    }

    let packed_path = git_dir.join("packed-refs");
    let packed = std::fs::read_to_string(&packed_path).map_err(|error| {
        format!(
            "ref {reference} has no loose file at {} and {} could not be read: {error}",
            ref_path.display(),
            packed_path.display()
        )
    })?;
    for line in packed.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let Some((object_id, name)) = line.split_once(' ') else {
            continue;
        };
        if name.trim() == reference {
            return validate_object_id(object_id, &packed_path);
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
