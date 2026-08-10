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
//! `build.rs` stamps the commit, ref, build time, source directory, and a
//! content-addressed inventory of every direct build input. That inventory
//! includes non-ignored untracked inputs and bounded changed-file identities,
//! so a source build cannot borrow a clean label from its commit. At health time
//! this module validates the entire embedded stamp, reads the current `HEAD` of
//! the same checkout straight out of `.git`, and compares without a subprocess.
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

use synapse_core::types::{BuildInputChange, SubsystemHealth};

/// Commit the running binary was built from, or `None` when it was built with
/// `SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1`.
const BUILD_COMMIT: Option<&str> = option_env!("SYNAPSE_BUILD_COMMIT");
const BUILD_REF: Option<&str> = option_env!("SYNAPSE_BUILD_REF");
const BUILD_TREE_STATE: Option<&str> = option_env!("SYNAPSE_BUILD_TREE_STATE");
const BUILD_INPUT_SCHEMA: Option<&str> = option_env!("SYNAPSE_BUILD_INPUT_SCHEMA");
const BUILD_INPUT_FILE_COUNT: Option<&str> = option_env!("SYNAPSE_BUILD_INPUT_FILE_COUNT");
const BUILD_INPUT_MANIFEST_SHA256: Option<&str> =
    option_env!("SYNAPSE_BUILD_INPUT_MANIFEST_SHA256");
const BUILD_GIT_STATUS_SHA256: Option<&str> = option_env!("SYNAPSE_BUILD_GIT_STATUS_SHA256");
const BUILD_CHANGED_INPUT_COUNT: Option<&str> = option_env!("SYNAPSE_BUILD_CHANGED_INPUT_COUNT");
const BUILD_CHANGED_INPUT_EXAMPLES_JSON: Option<&str> =
    option_env!("SYNAPSE_BUILD_CHANGED_INPUT_EXAMPLES_JSON");
const BUILD_CHANGED_INPUT_OMITTED: Option<&str> =
    option_env!("SYNAPSE_BUILD_CHANGED_INPUT_OMITTED");
const BUILD_INPUT_ATTESTATION_ERROR: Option<&str> =
    option_env!("SYNAPSE_BUILD_INPUT_ATTESTATION_ERROR");
const BUILD_UNIX_MS: Option<&str> = option_env!("SYNAPSE_BUILD_UNIX_MS");
const BUILD_PROFILE: Option<&str> = option_env!("SYNAPSE_BUILD_PROFILE");
const BUILD_SOURCE_DIR: Option<&str> = option_env!("SYNAPSE_BUILD_SOURCE_DIR");
const INPUT_SCHEMA_V1: &str = "synapse-build-inputs-v1";

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
    let input_file_count = BUILD_INPUT_FILE_COUNT.and_then(|value| value.parse().ok());
    let changed_input_count = BUILD_CHANGED_INPUT_COUNT.and_then(|value| value.parse().ok());
    let changed_input_omitted = BUILD_CHANGED_INPUT_OMITTED.and_then(|value| value.parse().ok());
    let changed_input_examples = BUILD_CHANGED_INPUT_EXAMPLES_JSON
        .and_then(|value| serde_json::from_str::<Vec<BuildInputChange>>(value).ok());
    let mut health = SubsystemHealth {
        status: "ok".to_owned(),
        build_commit: BUILD_COMMIT.map(str::to_owned),
        build_ref: BUILD_REF.map(str::to_owned),
        build_tree_state: BUILD_TREE_STATE.map(str::to_owned),
        build_input_schema: BUILD_INPUT_SCHEMA.map(str::to_owned),
        build_input_file_count: input_file_count,
        build_input_manifest_sha256: BUILD_INPUT_MANIFEST_SHA256.map(str::to_owned),
        build_git_status_sha256: BUILD_GIT_STATUS_SHA256.map(str::to_owned),
        build_changed_input_count: changed_input_count,
        build_changed_input_examples: changed_input_examples.clone(),
        build_changed_input_omitted: changed_input_omitted,
        build_input_attestation_error: BUILD_INPUT_ATTESTATION_ERROR.map(str::to_owned),
        build_unix_ms: BUILD_UNIX_MS.and_then(|value| value.parse().ok()),
        build_profile: BUILD_PROFILE.map(str::to_owned),
        build_source_dir: BUILD_SOURCE_DIR.map(str::to_owned),
        ..SubsystemHealth::default()
    };
    let mut severity = 0_u8;
    let mut diagnostics = Vec::new();

    match running_executable() {
        Ok((path, len, modified_unix_ms)) => {
            health.build_exe_path = Some(path.display().to_string());
            health.build_exe_len = Some(len);
            health.build_exe_modified_unix_ms = modified_unix_ms;
        }
        Err(reason) => {
            record_problem(
                &mut severity,
                &mut diagnostics,
                2,
                format!(
                    "SYNAPSE_BUILD_EXECUTABLE_UNREADABLE: the running executable's own path or \
                     metadata could not be read: {reason}; remediation=verify the installed \
                     executable still exists and the daemon account can read it, then redeploy"
                ),
            );
        }
    }

    if let Some(reason) = BUILD_INPUT_ATTESTATION_ERROR {
        record_problem(
            &mut severity,
            &mut diagnostics,
            2,
            format!(
                "SYNAPSE_BUILD_INPUT_ATTESTATION_UNKNOWN: the build explicitly proceeded without \
                 complete source provenance: {reason}; remediation=repair the named build/Git/input \
                 failure and rebuild without SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE"
            ),
        );
    }

    validate_stamp(&mut severity, &mut diagnostics, &changed_input_examples);

    let commit_valid = BUILD_COMMIT.is_some_and(is_object_id);
    let attestation_valid = BUILD_INPUT_ATTESTATION_ERROR.is_none()
        && BUILD_INPUT_SCHEMA == Some(INPUT_SCHEMA_V1)
        && BUILD_TREE_STATE.is_some_and(|value| matches!(value, "clean" | "dirty"))
        && input_file_count.is_some()
        && BUILD_INPUT_MANIFEST_SHA256.is_some_and(is_sha256)
        && BUILD_GIT_STATUS_SHA256.is_some_and(is_sha256)
        && changed_input_count.is_some()
        && changed_input_examples.is_some()
        && changed_input_omitted.is_some();

    match (BUILD_COMMIT, BUILD_SOURCE_DIR) {
        (Some(build_commit), Some(source_dir)) if commit_valid => {
            match resolve_head(Path::new(source_dir)) {
                Ok(checkout_commit) => {
                    health.build_checkout_commit = Some(checkout_commit.clone());
                    if checkout_commit != build_commit {
                        health.build_matches_checkout = Some(false);
                        record_problem(
                            &mut severity,
                            &mut diagnostics,
                            1,
                            format!(
                                "SYNAPSE_BUILD_CHECKOUT_DRIFT: the running daemon was built from \
                                 {build_commit} but the checkout at {source_dir} is now on \
                                 {checkout_commit}; remediation=redeploy with \
                                 scripts/synapse-setup.ps1 -SourceDir {source_dir}"
                            ),
                        );
                    } else if attestation_valid && BUILD_TREE_STATE == Some("clean") {
                        health.build_matches_checkout = Some(true);
                    } else if attestation_valid && BUILD_TREE_STATE == Some("dirty") {
                        // Established false: the bytes contain inputs not identified by the
                        // commit, even though HEAD itself has not moved.
                        health.build_matches_checkout = Some(false);
                        record_problem(
                            &mut severity,
                            &mut diagnostics,
                            1,
                            format!(
                                "SYNAPSE_BUILD_TREE_DIRTY: the daemon was built from {build_commit} \
                                 with {} changed build input(s) in {source_dir}, so its bytes are not \
                                 reproducible from that commit; remediation=commit or revert every \
                                 reported input and redeploy",
                                changed_input_count.unwrap_or(0)
                            ),
                        );
                    }
                    // Invalid/unknown attestation deliberately leaves the comparison absent.
                }
                Err(reason) => {
                    health.build_checkout_unavailable_reason = Some(reason.clone());
                    record_problem(
                        &mut severity,
                        &mut diagnostics,
                        2,
                        format!(
                            "SYNAPSE_BUILD_CHECKOUT_UNREADABLE: the daemon names its own commit \
                             ({build_commit}) but the checkout at {source_dir} could not be read, so \
                             drift is not established either way: {reason}; remediation=restore a \
                             readable .git HEAD/ref/packed-refs source or redeploy from a readable \
                             checkout"
                        ),
                    );
                }
            }
        }
        _ => {
            // Missing/invalid commit or source diagnostics are emitted by validate_stamp.
            // The comparison remains absent because no mismatch was established.
        }
    }

    health.status = match severity {
        0 => "ok",
        1 => "degraded",
        _ => "error",
    }
    .to_owned();
    health.detail = (!diagnostics.is_empty()).then(|| diagnostics.join(" | "));
    health
}

fn validate_stamp(
    severity: &mut u8,
    diagnostics: &mut Vec<String>,
    examples: &Option<Vec<BuildInputChange>>,
) {
    match BUILD_COMMIT {
        Some(value) if is_object_id(value) => {}
        Some(value) => record_problem(
            severity,
            diagnostics,
            2,
            format!(
                "SYNAPSE_BUILD_COMMIT_INVALID: embedded commit must be 40 hexadecimal characters \
                 (found {}); remediation=rebuild from a readable Git checkout",
                value.len()
            ),
        ),
        None => record_problem(
            severity,
            diagnostics,
            2,
            "SYNAPSE_BUILD_PROVENANCE_UNKNOWN: this binary cannot name the commit it was compiled \
             from; remediation=rebuild from a Git checkout without \
             SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE"
                .to_owned(),
        ),
    }
    require_text(
        BUILD_REF,
        "SYNAPSE_BUILD_REF_UNSTAMPED",
        "the build's Git ref is absent",
        severity,
        diagnostics,
    );
    require_text(
        BUILD_SOURCE_DIR,
        "SYNAPSE_BUILD_SOURCE_DIR_UNSTAMPED",
        "the binary does not name the checkout it came from",
        severity,
        diagnostics,
    );
    require_text(
        BUILD_PROFILE,
        "SYNAPSE_BUILD_PROFILE_UNSTAMPED",
        "the Cargo build profile is absent",
        severity,
        diagnostics,
    );
    require_u64(
        BUILD_UNIX_MS,
        "SYNAPSE_BUILD_TIME_INVALID",
        "build timestamp",
        severity,
        diagnostics,
    );

    if BUILD_INPUT_SCHEMA != Some(INPUT_SCHEMA_V1) {
        record_problem(
            severity,
            diagnostics,
            2,
            format!(
                "SYNAPSE_BUILD_INPUT_SCHEMA_INVALID: expected={INPUT_SCHEMA_V1} actual={}; \
                 remediation=rebuild with the current build script",
                BUILD_INPUT_SCHEMA.unwrap_or("<absent>")
            ),
        );
    }
    match BUILD_TREE_STATE {
        Some("clean" | "dirty") => {}
        Some(value) => record_problem(
            severity,
            diagnostics,
            2,
            format!(
                "SYNAPSE_BUILD_TREE_STATE_INVALID: expected=clean|dirty actual={value}; \
                 remediation=rebuild after repairing input attestation"
            ),
        ),
        None => record_problem(
            severity,
            diagnostics,
            2,
            "SYNAPSE_BUILD_TREE_STATE_UNSTAMPED: build input state is absent; \
             remediation=rebuild after repairing input attestation"
                .to_owned(),
        ),
    }
    require_u64(
        BUILD_INPUT_FILE_COUNT,
        "SYNAPSE_BUILD_INPUT_COUNT_INVALID",
        "build input file count",
        severity,
        diagnostics,
    );
    require_sha256(
        BUILD_INPUT_MANIFEST_SHA256,
        "SYNAPSE_BUILD_INPUT_MANIFEST_INVALID",
        severity,
        diagnostics,
    );
    require_sha256(
        BUILD_GIT_STATUS_SHA256,
        "SYNAPSE_BUILD_GIT_STATUS_DIGEST_INVALID",
        severity,
        diagnostics,
    );
    require_u64(
        BUILD_CHANGED_INPUT_COUNT,
        "SYNAPSE_BUILD_CHANGED_INPUT_COUNT_INVALID",
        "changed input count",
        severity,
        diagnostics,
    );
    require_u64(
        BUILD_CHANGED_INPUT_OMITTED,
        "SYNAPSE_BUILD_CHANGED_INPUT_OMITTED_INVALID",
        "changed input omitted count",
        severity,
        diagnostics,
    );

    if BUILD_CHANGED_INPUT_EXAMPLES_JSON.is_some() && examples.is_none() {
        record_problem(
            severity,
            diagnostics,
            2,
            "SYNAPSE_BUILD_CHANGED_INPUT_EXAMPLES_INVALID: embedded JSON could not be decoded; \
             remediation=rebuild with the current build script"
                .to_owned(),
        );
    } else if BUILD_CHANGED_INPUT_EXAMPLES_JSON.is_none() {
        record_problem(
            severity,
            diagnostics,
            2,
            "SYNAPSE_BUILD_CHANGED_INPUT_EXAMPLES_UNSTAMPED: bounded change identities are absent; \
             remediation=rebuild with the current build script"
                .to_owned(),
        );
    }

    if let (Some(count), Some(omitted), Some(examples)) = (
        BUILD_CHANGED_INPUT_COUNT.and_then(|value| value.parse::<u64>().ok()),
        BUILD_CHANGED_INPUT_OMITTED.and_then(|value| value.parse::<u64>().ok()),
        examples,
    ) {
        let example_count = u64::try_from(examples.len()).unwrap_or(u64::MAX);
        if count != example_count.saturating_add(omitted) || examples.len() > 16 {
            record_problem(
                severity,
                diagnostics,
                2,
                format!(
                    "SYNAPSE_BUILD_CHANGED_INPUT_SUMMARY_INCONSISTENT: count={count} \
                     examples={example_count} omitted={omitted}; remediation=rebuild with the \
                     current build script"
                ),
            );
        }
        if (BUILD_TREE_STATE == Some("clean")) != (count == 0) {
            record_problem(
                severity,
                diagnostics,
                2,
                format!(
                    "SYNAPSE_BUILD_TREE_STATE_INCONSISTENT: state={} changed_input_count={count}; \
                     remediation=rebuild after repairing the input inventory",
                    BUILD_TREE_STATE.unwrap_or("<absent>")
                ),
            );
        }
        for example in examples {
            if example
                .sha256
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
            {
                record_problem(
                    severity,
                    diagnostics,
                    2,
                    format!(
                        "SYNAPSE_BUILD_CHANGED_INPUT_DIGEST_INVALID: path={} digest is not SHA-256; \
                         remediation=rebuild with the current build script",
                        example.path
                    ),
                );
            }
        }
    }
}

fn require_text(
    value: Option<&str>,
    code: &str,
    description: &str,
    severity: &mut u8,
    diagnostics: &mut Vec<String>,
) {
    if !value.is_some_and(|value| !value.trim().is_empty()) {
        record_problem(
            severity,
            diagnostics,
            2,
            format!("{code}: {description}; remediation=rebuild from a readable Git checkout"),
        );
    }
}

fn require_u64(
    value: Option<&str>,
    code: &str,
    description: &str,
    severity: &mut u8,
    diagnostics: &mut Vec<String>,
) {
    if value.and_then(|value| value.parse::<u64>().ok()).is_none() {
        record_problem(
            severity,
            diagnostics,
            2,
            format!(
                "{code}: {description} is absent or not an unsigned integer; remediation=rebuild"
            ),
        );
    }
}

fn require_sha256(
    value: Option<&str>,
    code: &str,
    severity: &mut u8,
    diagnostics: &mut Vec<String>,
) {
    if !value.is_some_and(is_sha256) {
        record_problem(
            severity,
            diagnostics,
            2,
            format!("{code}: expected a 64-hex SHA-256 digest; remediation=rebuild"),
        );
    }
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn record_problem(severity: &mut u8, diagnostics: &mut Vec<String>, level: u8, diagnostic: String) {
    *severity = (*severity).max(level);
    diagnostics.push(diagnostic);
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
