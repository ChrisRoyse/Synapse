//! Stamps the daemon with content-addressed provenance for the source inputs
//! that actually influenced the binary (#1971, #2182, #2183).
//!
//! A commit id alone is insufficient: a source build may contain modified or
//! non-ignored untracked inputs that do not exist in that commit. This build
//! script therefore records a deterministic manifest digest over every direct
//! build input, the complete Git porcelain-status digest, and bounded identities
//! for changed inputs. It also registers every input and the safe source/Git
//! boundaries with Cargo so an incremental build cannot reuse a stale stamp.
//!
//! Provenance is fail-closed. `SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1` is the
//! explicit source-tarball escape hatch; binaries built that way report an
//! error in the `build_provenance` health subsystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const ATTESTATION_SCHEMA: &str = "synapse-build-inputs-v1";
const MAX_CHANGED_INPUT_EXAMPLES: usize = 16;

fn main() {
    println!("cargo::rerun-if-env-changed=SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE");

    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        panic!(
            "SYNAPSE_BUILD_MANIFEST_DIR_UNSET: cargo did not set CARGO_MANIFEST_DIR, so this build \
             script cannot locate the checkout to stamp provenance from"
        );
    };
    let manifest_dir = PathBuf::from(manifest_dir);
    // crates/synapse-mcp -> the workspace root that owns .git.
    let source_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&manifest_dir)
        .to_path_buf();
    verify_chrome_bridge_declared_identity(&source_dir).unwrap_or_else(|reason| {
        panic!(
            "SYNAPSE_CHROME_BRIDGE_DECLARED_IDENTITY_MISMATCH: {reason}; remediation=update the service-worker BRIDGE_BUILD_ID/BRIDGE_DECLARED_BUILD_SHA256 and daemon EXPECTED_EXTENSION_BUILD_ID/EXPECTED_EXTENSION_DECLARED_BUILD_SHA256 together"
        )
    });

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

    let mut failures = Vec::new();
    let head = match resolve_head(&source_dir) {
        Ok(head) => {
            for path in &head.rerun_paths {
                rerun_if_changed(path);
            }
            emit("SYNAPSE_BUILD_COMMIT", &head.commit);
            emit("SYNAPSE_BUILD_COMMIT_SHORT", &head.commit[..12]);
            emit("SYNAPSE_BUILD_REF", &head.reference);
            Some(head)
        }
        Err(reason) => {
            failures.push(format!("head={reason}"));
            None
        }
    };

    match attest_inputs(
        &source_dir,
        head.as_ref().map(|value| value.git_dir.as_path()),
    ) {
        Ok(attestation) => {
            for path in &attestation.rerun_paths {
                rerun_if_changed(path);
            }
            emit("SYNAPSE_BUILD_TREE_STATE", &attestation.tree_state);
            emit("SYNAPSE_BUILD_INPUT_SCHEMA", ATTESTATION_SCHEMA);
            emit(
                "SYNAPSE_BUILD_INPUT_FILE_COUNT",
                &attestation.file_count.to_string(),
            );
            emit(
                "SYNAPSE_BUILD_INPUT_MANIFEST_SHA256",
                &attestation.manifest_sha256,
            );
            emit(
                "SYNAPSE_BUILD_GIT_STATUS_SHA256",
                &attestation.git_status_sha256,
            );
            emit(
                "SYNAPSE_BUILD_CHANGED_INPUT_COUNT",
                &attestation.changed_input_count.to_string(),
            );
            emit(
                "SYNAPSE_BUILD_CHANGED_INPUT_EXAMPLES_JSON",
                &attestation.changed_input_examples_json,
            );
            emit(
                "SYNAPSE_BUILD_CHANGED_INPUT_OMITTED",
                &attestation.changed_input_omitted.to_string(),
            );
        }
        Err(reason) => {
            emit("SYNAPSE_BUILD_TREE_STATE", "unknown");
            failures.push(format!("input_attestation={reason}"));
        }
    }

    if failures.is_empty() {
        return;
    }

    let reason = failures.join(" | ");
    emit("SYNAPSE_BUILD_INPUT_ATTESTATION_ERROR", &reason);
    if std::env::var("SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE").as_deref() == Ok("1") {
        println!(
            "cargo::warning=synapse-mcp is being built with unknown provenance ({reason}); \
             the daemon will report build_provenance status=error"
        );
        return;
    }
    panic!(
        "SYNAPSE_BUILD_PROVENANCE_UNRESOLVED: the daemon's source provenance could not be \
         established: {reason}. Build from a readable git checkout after repairing the named \
         source, Git, or input error. Only a deliberate source-tarball build may set \
         SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1; that binary reports status=error at runtime."
    );
}

fn verify_chrome_bridge_declared_identity(source_dir: &Path) -> Result<(), String> {
    let worker_path = source_dir
        .join("extensions")
        .join("synapse-chrome-debugger")
        .join("service_worker.js");
    let daemon_path = source_dir
        .join("crates")
        .join("synapse-chrome-bridge")
        .join("src")
        .join("lib.rs");
    rerun_if_changed(&worker_path);
    rerun_if_changed(&daemon_path);
    let worker = std::fs::read_to_string(&worker_path).map_err(|error| {
        format!(
            "read extension identity source {} failed: {error}",
            worker_path.display()
        )
    })?;
    let daemon = std::fs::read_to_string(&daemon_path).map_err(|error| {
        format!(
            "read daemon identity source {} failed: {error}",
            daemon_path.display()
        )
    })?;
    let worker_build = extract_string_constant(&worker, "BRIDGE_BUILD_ID")?;
    let daemon_build = extract_string_constant(&daemon, "EXPECTED_EXTENSION_BUILD_ID")?;
    let worker_declared = extract_string_constant(&worker, "BRIDGE_DECLARED_BUILD_SHA256")?;
    let daemon_declared =
        extract_string_constant(&daemon, "EXPECTED_EXTENSION_DECLARED_BUILD_SHA256")?;
    if worker_build != daemon_build {
        return Err(format!(
            "build_id extension={worker_build:?} daemon={daemon_build:?}"
        ));
    }
    if worker_declared != daemon_declared {
        return Err(format!(
            "declared_sha256 extension={worker_declared:?} daemon={daemon_declared:?}"
        ));
    }
    if worker_declared.len() != 64
        || !worker_declared
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "declared_sha256 is not exactly 64 lowercase hexadecimal characters: {worker_declared:?}"
        ));
    }
    let computed = sha256_hex(worker_build.as_bytes());
    if worker_declared != computed {
        return Err(format!(
            "declared_sha256 does not commit to build_id: build_id={worker_build:?} declared={worker_declared:?} computed={computed:?}"
        ));
    }
    Ok(())
}

fn extract_string_constant<'a>(source: &'a str, name: &str) -> Result<&'a str, String> {
    let marker = format!("const {name}");
    let matches = source.match_indices(&marker).collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "constant {name} must have exactly one declaration; found {}",
            matches.len()
        ));
    }
    let declaration = &source[matches[0].0 + marker.len()..];
    let assignment = declaration
        .find('=')
        .map(|offset| &declaration[offset + 1..])
        .ok_or_else(|| format!("constant {name} has no assignment"))?;
    let opening = assignment
        .find('"')
        .ok_or_else(|| format!("constant {name} has no opening string quote"))?;
    let value = &assignment[opening + 1..];
    let closing = value
        .find('"')
        .ok_or_else(|| format!("constant {name} has no closing string quote"))?;
    Ok(&value[..closing])
}

struct InputAttestation {
    tree_state: String,
    file_count: usize,
    manifest_sha256: String,
    git_status_sha256: String,
    changed_input_count: usize,
    changed_input_examples_json: String,
    changed_input_omitted: usize,
    rerun_paths: Vec<PathBuf>,
}

struct InputChange {
    path: String,
    status: String,
    kind: &'static str,
    length: Option<u64>,
    sha256: Option<String>,
}

/// Attests every non-ignored direct build input in a canonical path order.
fn attest_inputs(source_dir: &Path, git_dir: Option<&Path>) -> Result<InputAttestation, String> {
    let inventory_output = git_output(
        source_dir,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        "enumerate tracked and non-ignored untracked files",
    )?;
    let tracked_output = git_output(
        source_dir,
        &["ls-files", "--cached", "--deduplicate", "-z"],
        "enumerate tracked files",
    )?;
    let status_output = git_output(
        source_dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
            "--ignore-submodules=none",
            "--no-renames",
        ],
        "read stable working-tree status",
    )?;

    let inventory = parse_nul_paths(&inventory_output.stdout, "git ls-files inventory")?;
    let tracked = parse_nul_paths(&tracked_output.stdout, "git ls-files tracked set")?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let statuses = parse_porcelain_status(&status_output.stdout)?;

    let candidates = inventory
        .into_iter()
        .filter(|path| is_build_input(path))
        .collect::<BTreeSet<_>>();
    if candidates.is_empty() {
        return Err(
            "SYNAPSE_BUILD_INPUT_INVENTORY_EMPTY: Git returned no build-input candidates"
                .to_owned(),
        );
    }

    let mut manifest = Sha256::new();
    manifest.update(ATTESTATION_SCHEMA.as_bytes());
    let mut changes = Vec::new();
    let mut rerun_paths = Vec::with_capacity(candidates.len() + 32);

    for relative in &candidates {
        let absolute = source_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        rerun_paths.push(absolute.clone());

        let is_tracked = tracked.contains(relative);
        let status = statuses
            .get(relative)
            .cloned()
            .unwrap_or_else(|| "  ".to_owned());
        if !is_tracked && status != "??" {
            return Err(format!(
                "SYNAPSE_BUILD_INPUT_STATUS_MISSING: non-indexed candidate {relative} did not \
                 have the expected ?? Git status (found {status:?})"
            ));
        }

        let (kind, length, digest) = match std::fs::read(&absolute) {
            Ok(bytes) => {
                let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let kind = if is_tracked { "tracked" } else { "untracked" };
                (kind, Some(length), Some(sha256_hex(&bytes)))
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && is_tracked
                    && status.as_bytes().contains(&b'D') =>
            {
                ("missing_tracked", None, None)
            }
            Err(error) => {
                return Err(format!(
                    "SYNAPSE_BUILD_INPUT_READ_FAILED: path={relative} absolute={} error={error}",
                    absolute.display()
                ));
            }
        };

        hash_manifest_field(&mut manifest, relative.as_bytes());
        hash_manifest_field(&mut manifest, kind.as_bytes());
        hash_manifest_field(
            &mut manifest,
            length
                .map_or_else(
                    || b"missing".to_vec(),
                    |value| value.to_string().into_bytes(),
                )
                .as_slice(),
        );
        hash_manifest_field(
            &mut manifest,
            digest.as_deref().unwrap_or("missing").as_bytes(),
        );

        if status != "  " {
            changes.push(InputChange {
                path: relative.clone(),
                status,
                kind,
                length,
                sha256: digest,
            });
        }
    }

    // A tracked ignore rule changes which untracked inputs are admitted.
    for relative in tracked.iter().filter(|path| {
        path.rsplit('/').next() == Some(".gitignore") || path.as_str() == ".gitattributes"
    }) {
        rerun_paths.push(source_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)));
    }
    add_safe_source_boundaries(source_dir, &mut rerun_paths);
    if let Some(git_dir) = git_dir {
        add_git_boundaries(source_dir, git_dir, &mut rerun_paths)?;
    }
    rerun_paths.sort();
    rerun_paths.dedup();

    let changed_input_count = changes.len();
    let changed_input_omitted = changed_input_count.saturating_sub(MAX_CHANGED_INPUT_EXAMPLES);
    let examples = changes
        .into_iter()
        .take(MAX_CHANGED_INPUT_EXAMPLES)
        .map(|change| {
            serde_json::json!({
                "path": change.path,
                "status": change.status,
                "kind": change.kind,
                "length": change.length,
                "sha256": change.sha256,
            })
        })
        .collect::<Vec<_>>();
    let changed_input_examples_json = serde_json::to_string(&examples)
        .map_err(|error| format!("SYNAPSE_BUILD_INPUT_EXAMPLES_ENCODE_FAILED: {error}"))?;

    Ok(InputAttestation {
        tree_state: if changed_input_count == 0 {
            "clean".to_owned()
        } else {
            "dirty".to_owned()
        },
        file_count: candidates.len(),
        manifest_sha256: hex_digest(manifest.finalize().as_slice()),
        git_status_sha256: sha256_hex(&status_output.stdout),
        changed_input_count,
        changed_input_examples_json,
        changed_input_omitted,
        rerun_paths,
    })
}

/// Closed definition of files whose bytes can affect shipped Synapse artifacts.
/// Documentation and ignored runtime state are intentionally outside it.
fn is_build_input(path: &str) -> bool {
    if [".claude/", ".github/", ".vscode/", "docs/", "STATE/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(
        name,
        "Cargo.toml" | "Cargo.lock" | "rust-toolchain.toml" | "clippy.toml"
    ) || path.starts_with(".cargo/")
    {
        return true;
    }

    matches!(
        name.rsplit_once('.').map(|(_, extension)| extension),
        Some("rs" | "cu" | "cuh" | "h" | "c" | "cpp" | "html" | "js" | "css" | "json" | "ps1")
    )
}

fn add_safe_source_boundaries(source_dir: &Path, paths: &mut Vec<PathBuf>) {
    // Directories are intentionally limited to trees without multi-GB ignored
    // build products. Cargo recursively scans a watched directory by mtime.
    for relative in [
        ".cargo",
        "crates",
        "calyx/crates",
        "dashboard/dist",
        "scripts",
        "extensions",
        "models",
    ] {
        let path = source_dir.join(relative);
        if path.is_dir() {
            paths.push(path);
        }
    }
}

fn add_git_boundaries(
    source_dir: &Path,
    git_dir: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for relative in ["index", "config", "packed-refs", "info/exclude"] {
        let path = git_dir.join(relative);
        if path.exists() {
            paths.push(path);
        }
    }
    let git_marker = source_dir.join(".git");
    if git_marker.is_file() {
        paths.push(git_marker);
    }

    match git_config_path(source_dir, "core.excludesFile")? {
        Some(path) if path.exists() => paths.push(path),
        _ => {}
    }
    Ok(())
}

fn git_config_path(source_dir: &Path, key: &str) -> Result<Option<PathBuf>, String> {
    let output = std::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(source_dir)
        .args(["config", "--path", "--get", key])
        .output()
        .map_err(|error| {
            format!("SYNAPSE_BUILD_GIT_LAUNCH_FAILED: git config --get {key}: {error}")
        })?;
    if output.status.success() {
        let value = String::from_utf8(output.stdout).map_err(|error| {
            format!("SYNAPSE_BUILD_GIT_CONFIG_NON_UTF8: key={key} error={error}")
        })?;
        let value = value.trim();
        return Ok((!value.is_empty()).then(|| PathBuf::from(value)));
    }
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(None);
    }
    Err(format!(
        "SYNAPSE_BUILD_GIT_CONFIG_FAILED: key={key} exit={:?} stderr={}",
        output.status.code(),
        bounded_stderr(&output.stderr)
    ))
}

fn git_output(source_dir: &Path, args: &[&str], operation: &str) -> Result<Output, String> {
    let output = std::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(source_dir)
        .args(args)
        .output()
        .map_err(|error| {
            format!(
                "SYNAPSE_BUILD_GIT_LAUNCH_FAILED: operation={operation} executable=git error={error}"
            )
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "SYNAPSE_BUILD_GIT_COMMAND_FAILED: operation={operation} exit={:?} stderr={}",
            output.status.code(),
            bounded_stderr(&output.stderr)
        ))
    }
}

fn parse_nul_paths(bytes: &[u8], operation: &str) -> Result<Vec<String>, String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            String::from_utf8(entry.to_vec()).map_err(|error| {
                format!("SYNAPSE_BUILD_GIT_PATH_NON_UTF8: operation={operation} error={error}")
            })
        })
        .collect()
}

fn parse_porcelain_status(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut statuses = BTreeMap::new();
    for entry in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(format!(
                "SYNAPSE_BUILD_GIT_STATUS_MALFORMED: expected XY<space>path, found {} bytes",
                entry.len()
            ));
        }
        let status = String::from_utf8(entry[..2].to_vec())
            .map_err(|error| format!("SYNAPSE_BUILD_GIT_STATUS_NON_UTF8: status error={error}"))?;
        let path = String::from_utf8(entry[3..].to_vec())
            .map_err(|error| format!("SYNAPSE_BUILD_GIT_STATUS_NON_UTF8: path error={error}"))?;
        if statuses.insert(path.clone(), status).is_some() {
            return Err(format!(
                "SYNAPSE_BUILD_GIT_STATUS_DUPLICATE: path={path} appeared more than once"
            ));
        }
    }
    Ok(statuses)
}

fn hash_manifest_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize().as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn bounded_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(1024)
        .collect::<String>()
        .replace(['\r', '\n'], " ")
}

struct Head {
    commit: String,
    reference: String,
    git_dir: PathBuf,
    rerun_paths: Vec<PathBuf>,
}

/// Resolves symbolic loose refs, packed refs, and detached HEAD without a
/// subprocess. A worktree/submodule `.git` file is followed to its Git dir.
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
        let commit = validate_object_id(head, &head_path)?;
        return Ok(Head {
            commit,
            reference: "HEAD".to_owned(),
            git_dir,
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
            git_dir,
            rerun_paths,
        });
    }

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
                git_dir,
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

fn rerun_if_changed(path: &Path) {
    println!("cargo::rerun-if-changed={}", path.display());
}

fn emit(key: &str, value: &str) {
    let value = value.replace(['\r', '\n'], " ");
    println!("cargo::rustc-env={key}={value}");
}
