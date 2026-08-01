//! Full State Verification for the every-published-generation search sweep
//! (issue #1938).
//!
//! # What is being proven, and against what
//!
//! The defect was that maintenance was addressed by the vault's **active-panel
//! pointer** instead of by the generations that actually exist, so every
//! non-active generation had no maintainer and rotted until its queries failed
//! closed. The fix claims three things, and each is checked against the bytes on
//! disk rather than against the sweep's own return value:
//!
//! 1. **Discovery is complete.** The set of generations the sweep considers is
//!    compared against an independent directory listing of
//!    `idx/search/panel_*/manifest.json` performed by this example, not by the
//!    code under test. A sweep that silently narrowed its own scope would still
//!    return `Ok` and would fail here.
//! 2. **A rebuild actually happened, for the right generations.** Every
//!    generation's `manifest.json` is SHA-256'd and its `base_seq` parsed
//!    *before* the sweep and *again after*. A generation the sweep reports as
//!    rebuilt must have a strictly greater `base_seq` and a different manifest
//!    hash on disk; a generation it reports as untouched must be byte-identical.
//!    This is the check that distinguishes a real rebuild from a returned claim.
//! 3. **The reported headroom is the real headroom.** `keys_to_bound` is
//!    recomputed here from the manifest's post-sweep `base_seq` through a
//!    separate status read, and must agree.
//!
//! # Boundary and edge cases exercised
//!
//! * **A published generation with no code-declared panel contract**
//!   (`panel_0001664001`, a superseded timeline version). It cannot be rebuilt
//!   by anything, so it must be reported as the declared terminal state
//!   `unmaintainable_no_contract` — and its manifest must be byte-identical
//!   afterwards, proving the sweep did not try and half-succeed.
//! * **A stray entry under the index root.** This example creates a file and an
//!   empty `panel_0009999999/` directory with no manifest, and requires both to
//!   appear in `unrecognized` rather than being silently dropped or counted as
//!   published. A sweep whose scope narrows silently is indistinguishable from
//!   one that covered everything.
//! * **An absent index root.** A vault that has never published a generation
//!   must enumerate to an empty set with no error — "nothing published yet" is a
//!   state, not a failure.
//! * **A generation already past the bound.** The manifest of one panel is
//!   rewritten to an artificially old `base_seq`, so its measured delta exceeds
//!   `MAX_RECONCILED_DELTA_KEYS`. The sweep must rebuild it (the naptime bound
//!   must NOT hold a dead generation dead), and the post-sweep query against it
//!   must succeed.
//!
//! # Usage
//!
//! ```text
//! cargo run -p synapse-storage --example search_generation_sweep_fsv -- <vault-parent-dir>
//! ```
//!
//! This example **writes** (it rebuilds generations and edits manifests on
//! purpose). Point it at a *copy* of a data directory, never at the live one.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use synapse_storage::Db;
use synapse_storage::search_sweep::{GenerationDisposition, SearchGenerationSweep};

const SCHEMA_VERSION: u32 = 1;

/// A superseded timeline panel version with a published generation on the live
/// vault and no entry in `syn_active_panel_contract`. The declared-terminal case.
const UNCONTRACTED_PANEL: u32 = 1_664_001;

/// The panel whose manifest this run ages past the reconciliation bound.
const AGED_PANEL: u32 = 1_776_006;

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

/// One generation's physical state, read straight off disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestFacts {
    sha256: String,
    base_seq: u64,
}

fn index_root(vault_dir: &Path) -> PathBuf {
    vault_dir.join("idx").join("search")
}

fn manifest_path(vault_dir: &Path, panel_version: u32) -> PathBuf {
    index_root(vault_dir)
        .join(format!("panel_{panel_version:010}"))
        .join("manifest.json")
}

/// Reads every published generation's manifest facts by listing the directory
/// directly. Deliberately does NOT call the code under test.
fn read_manifests(vault_dir: &Path) -> Result<BTreeMap<u32, ManifestFacts>, Box<dyn Error>> {
    let mut facts = BTreeMap::new();
    let root = index_root(vault_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(facts);
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(version) = name
            .strip_prefix("panel_")
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            continue;
        };
        let path = manifest_path(vault_dir, version);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let parsed: serde_json::Value = serde_json::from_slice(&bytes)?;
        let base_seq = parsed
            .get("base_seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("manifest {} has no base_seq", path.display()))?;
        facts.insert(
            version,
            ManifestFacts {
                sha256: Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                base_seq,
            },
        );
    }
    Ok(facts)
}

fn disposition_of(sweep: &SearchGenerationSweep, panel_version: u32) -> Option<&'static str> {
    sweep
        .generations
        .iter()
        .find(|entry| entry.panel_version == panel_version)
        .map(|entry| entry.disposition.as_str())
}

fn rebuilt_by_sweep(sweep: &SearchGenerationSweep, panel_version: u32) -> bool {
    sweep
        .generations
        .iter()
        .find(|entry| entry.panel_version == panel_version)
        .is_some_and(|entry| match &entry.disposition {
            GenerationDisposition::Maintained(report) => report.after.is_some(),
            _ => false,
        })
}

#[allow(
    clippy::too_many_lines,
    reason = "one verification run over one vault; the before/act/after triple has to stay in one scope or the 'after' read stops being a read of the same state the 'before' captured"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let parent = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: search_generation_sweep_fsv <dir-containing-db-daemon>")?;
    let vault_dir = parent.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!(
            "no db-daemon directory under {}; point this at a COPY of a Synapse data dir",
            parent.display()
        )
        .into());
    }

    println!("search_generation_sweep_fsv");
    println!("  vault_dir = {}", vault_dir.display());
    println!();
    let mut failures: Vec<String> = Vec::new();

    // -----------------------------------------------------------------------
    // EDGE CASE 1: entries under the index root that are not published
    // generations must be REPORTED, never silently dropped.
    // -----------------------------------------------------------------------
    let stray_file = index_root(&vault_dir).join("stray-artifact.tmp");
    std::fs::write(&stray_file, b"not a generation")?;
    let stray_dir = index_root(&vault_dir).join("panel_0009999999");
    std::fs::create_dir_all(&stray_dir)?;
    println!("== EDGE CASE SETUP ==");
    println!("  planted stray file      = {}", stray_file.display());
    println!(
        "  planted manifest-less dir = {} (a directory is not a publication)",
        stray_dir.display()
    );

    // -----------------------------------------------------------------------
    // EDGE CASE 2: age one generation's manifest PAST the reconciliation bound,
    // so the sweep must rebuild it rather than defer it by the naptime bound.
    // -----------------------------------------------------------------------
    let aged_path = manifest_path(&vault_dir, AGED_PANEL);
    let aged_before_bytes = std::fs::read(&aged_path)?;
    let mut aged_json: serde_json::Value = serde_json::from_slice(&aged_before_bytes)?;
    let aged_original_seq = aged_json["base_seq"].as_u64().unwrap_or_default();
    aged_json["base_seq"] = serde_json::Value::from(1_u64);
    std::fs::write(&aged_path, serde_json::to_vec_pretty(&aged_json)?)?;
    // The manifest's *mtime* is the generation's age, and the naptime bound
    // rejects a refresh inside SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS.
    // Rewriting the file above reset that mtime to now, so without this the
    // sweep would correctly answer `deferred_by_interval` and the case would
    // prove nothing about whether a stale non-active generation gets rebuilt.
    let aged_backdated = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(24 * 60 * 60))
        .ok_or("cannot backdate the aged manifest")?;
    std::fs::File::options()
        .write(true)
        .open(&aged_path)?
        .set_modified(aged_backdated)?;
    println!(
        "  aged panel {AGED_PANEL} manifest base_seq {aged_original_seq} -> 1 and mtime -> 24h ago \
         (a stale NON-ACTIVE generation, past its refresh trigger and outside the naptime bound)"
    );
    println!();

    // -----------------------------------------------------------------------
    // BEFORE: the Source of Truth, read independently of the code under test
    // -----------------------------------------------------------------------
    let before = read_manifests(&vault_dir)?;
    println!("== SOURCE OF TRUTH: manifests on disk BEFORE the sweep ==");
    for (version, facts) in &before {
        println!(
            "  panel {version:>8}  base_seq={:<10} sha256={}",
            facts.base_seq,
            &facts.sha256[..16]
        );
    }
    println!();

    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;
    let vault_seq_before = db.calyx_vault_status()?.latest_seq.unwrap_or_default();
    println!("  vault latest_seq = {vault_seq_before}");
    println!();

    // -----------------------------------------------------------------------
    // ACT
    // -----------------------------------------------------------------------
    println!("== ACT: one unattended sweep over every published generation ==");
    let sweep = db.maintain_calyx_search_generation()?;
    println!("  {}", sweep.summary_line());
    println!();

    // -----------------------------------------------------------------------
    // AFTER: re-read the same bytes
    // -----------------------------------------------------------------------
    let after = read_manifests(&vault_dir)?;
    println!("== SOURCE OF TRUTH: manifests on disk AFTER the sweep ==");
    for (version, facts) in &after {
        let changed = before.get(version) != Some(facts);
        println!(
            "  panel {version:>8}  base_seq={:<10} sha256={}  changed_on_disk={changed}",
            facts.base_seq,
            &facts.sha256[..16]
        );
    }
    println!();

    // --- CHECK 1: discovery is complete -------------------------------------
    let discovered: Vec<u32> = sweep
        .generations
        .iter()
        .map(|entry| entry.panel_version)
        .collect();
    let expected: Vec<u32> = before.keys().copied().collect();
    let discovery_ok = expected.iter().all(|version| discovered.contains(version));
    println!("== CHECK 1: every generation on disk was considered ==");
    println!("  on disk    = {expected:?}");
    println!("  considered = {discovered:?}");
    println!(
        "  {} discovery covers the physical set",
        verdict(discovery_ok)
    );
    if !discovery_ok {
        failures.push(format!(
            "the sweep considered {discovered:?} but {expected:?} are published on disk"
        ));
    }

    // --- CHECK 2: the stray entries were reported ---------------------------
    let stray_file_reported = sweep
        .unrecognized_index_entries
        .iter()
        .any(|entry| entry.contains("stray-artifact.tmp"));
    let stray_dir_reported = sweep
        .unrecognized_index_entries
        .iter()
        .any(|entry| entry.contains("panel_0009999999"));
    let stray_dir_not_published = !discovered.contains(&9_999_999);
    println!("== CHECK 2: unrecognized index entries are reported, not dropped ==");
    println!("  unrecognized = {:?}", sweep.unrecognized_index_entries);
    println!(
        "  {} stray file reported | {} manifest-less dir reported | {} manifest-less dir NOT \
         counted as published",
        verdict(stray_file_reported),
        verdict(stray_dir_reported),
        verdict(stray_dir_not_published)
    );
    if !stray_file_reported {
        failures.push("a stray file under the index root was not reported".to_owned());
    }
    if !stray_dir_reported {
        failures.push("a manifest-less panel directory was not reported".to_owned());
    }
    if !stray_dir_not_published {
        failures.push(
            "a manifest-less panel directory was counted as a published generation".to_owned(),
        );
    }

    // --- CHECK 3: the uncontracted generation is a declared terminal state --
    let uncontracted = disposition_of(&sweep, UNCONTRACTED_PANEL);
    let uncontracted_ok = uncontracted == Some("unmaintainable_no_contract");
    let uncontracted_untouched = before.get(&UNCONTRACTED_PANEL) == after.get(&UNCONTRACTED_PANEL);
    println!("== CHECK 3: a generation with no panel contract is DECLARED, not attempted ==");
    println!("  panel {UNCONTRACTED_PANEL} disposition = {uncontracted:?}");
    println!(
        "  {} declared terminal | {} manifest byte-identical afterwards",
        verdict(uncontracted_ok),
        verdict(uncontracted_untouched)
    );
    if !uncontracted_ok {
        failures.push(format!(
            "panel {UNCONTRACTED_PANEL} has no contract but was reported as {uncontracted:?}"
        ));
    }
    if !uncontracted_untouched {
        failures.push(format!(
            "panel {UNCONTRACTED_PANEL} manifest changed on disk despite being unmaintainable"
        ));
    }

    // --- CHECK 4: a generation past the bound was actually REBUILT ----------
    let aged_after = after
        .get(&AGED_PANEL)
        .ok_or("the aged panel's manifest vanished")?;
    let aged_rebuilt_claim = rebuilt_by_sweep(&sweep, AGED_PANEL);
    let aged_rebuilt_on_disk = aged_after.base_seq > 1;
    println!(
        "== CHECK 4: a stale NON-ACTIVE generation is rebuilt (this is the whole #1938 claim) =="
    );
    println!(
        "  panel {AGED_PANEL} base_seq on disk: 1 (aged) -> {}",
        aged_after.base_seq
    );
    println!(
        "  {} sweep claims a rebuild | {} disk proves a rebuild (base_seq advanced past the \
         planted value)",
        verdict(aged_rebuilt_claim),
        verdict(aged_rebuilt_on_disk)
    );
    if !aged_rebuilt_claim {
        failures.push(format!(
            "panel {AGED_PANEL} was past the bound but the sweep did not rebuild it"
        ));
    }
    if !aged_rebuilt_on_disk {
        failures.push(format!(
            "the sweep claimed a rebuild of panel {AGED_PANEL} but its manifest base_seq on disk \
             is still {}",
            aged_after.base_seq
        ));
    }

    // --- CHECK 5: claim vs bytes, for EVERY generation ----------------------
    println!("== CHECK 5: every claimed rebuild changed the bytes, every non-rebuild did not ==");
    for entry in &sweep.generations {
        let version = entry.panel_version;
        let (Some(pre), Some(post)) = (before.get(&version), after.get(&version)) else {
            continue;
        };
        let claimed = rebuilt_by_sweep(&sweep, version);
        let changed = pre != post;
        let agrees = claimed == changed;
        println!(
            "  panel {version:>8}  claimed_rebuild={claimed:<5} bytes_changed={changed:<5} {}",
            verdict(agrees)
        );
        if !agrees {
            failures.push(format!(
                "panel {version}: sweep claimed rebuild={claimed} but the manifest bytes \
                 changed={changed}"
            ));
        }
    }

    // --- CHECK 6: reported headroom matches an independent status read ------
    println!("== CHECK 6: reported keys_to_bound agrees with a fresh status read ==");
    for entry in &sweep.generations {
        let Some(reported) = entry.keys_to_bound() else {
            continue;
        };
        let status = db.calyx_search_generation_status_for_panel(entry.panel_version, true)?;
        let independent = status
            .max_reconciled_delta_keys
            .saturating_sub(status.delta_changed_keys.unwrap_or(u64::MAX));
        // The vault does not advance under this single-process run, so the two
        // measurements are over the same state and must agree exactly.
        let agrees = reported == independent;
        println!(
            "  panel {:>8}  sweep={reported:<8} independent={independent:<8} {}",
            entry.panel_version,
            verdict(agrees)
        );
        if !agrees {
            failures.push(format!(
                "panel {}: sweep reported keys_to_bound={reported} but an independent status read \
                 measured {independent}",
                entry.panel_version
            ));
        }
    }

    // --- CHECK 7: every maintained generation actually ANSWERS a query ------
    println!("== CHECK 7: each maintained generation serves a real query afterwards ==");
    for entry in &sweep.generations {
        if !matches!(entry.disposition, GenerationDisposition::Maintained(_)) {
            continue;
        }
        let params = synapse_calyx::SynapseCalyxFindParams {
            panel_version: Some(entry.panel_version),
            query: synapse_calyx::SynapseCalyxFindQuery::ByText {
                text: "synapse".to_owned(),
            },
            k: 3,
            fusion: synapse_calyx::SynapseCalyxFindFusion::Rrf,
            filter: None,
            explain: false,
            temporal: None,
        };
        match db.find_similar(&params) {
            Ok(report) => {
                println!(
                    "  panel {:>8}  OK   hits={} consulted={:?}",
                    entry.panel_version,
                    report.hits.len(),
                    report.consulted_slots
                );
            }
            // A structured-only panel (mcp-usage) declares no text-queryable
            // lens, so a text query can never produce a vector there however
            // fresh the generation is. That refusal is decided *after* the
            // generation is opened and its lanes enumerated, so it still proves
            // what this check is for — the generation loaded and reconciled.
            // Any staleness, rebase or integrity code does not, and fails.
            Err(error) if format!("{error}").contains("FIND_NO_INDEXABLE_QUERY") => {
                println!(
                    "  panel {:>8}  OK   generation opened; panel declares no text-queryable lens \
                     (structured-only), so the query shape cannot be answered here",
                    entry.panel_version
                );
            }
            Err(error) => {
                println!("  panel {:>8}  FAIL {error}", entry.panel_version);
                failures.push(format!(
                    "panel {} was reported maintained but a query against it failed: {error}",
                    entry.panel_version
                ));
            }
        }
    }
    println!();

    // -----------------------------------------------------------------------
    // EDGE CASE 3: a vault that has never published a generation
    // -----------------------------------------------------------------------
    println!("== EDGE CASE 3: an absent index root enumerates empty, and is not an error ==");
    let moved_root = index_root(&vault_dir).with_file_name("search.moved-for-fsv");
    std::fs::rename(index_root(&vault_dir), &moved_root)?;
    let empty_sweep = db.maintain_calyx_search_generation();
    // The sweep legitimately re-created the index root: with no generation on
    // disk the active panel's case is InitialBuild. Discard that build before
    // restoring, so the restore is a rename onto an absent path.
    let _ = std::fs::remove_dir_all(index_root(&vault_dir));
    let restored = std::fs::rename(&moved_root, index_root(&vault_dir));
    match empty_sweep {
        Ok(empty) => {
            // The active panel still has a contract, so it is still swept — it
            // just has no generation on disk any more, which is the
            // InitialBuild case. What must NOT happen is an error, and what must
            // NOT appear is any generation that no longer exists.
            let stale_reported = empty
                .generations
                .iter()
                .any(|entry| Some(entry.panel_version) != empty.active_panel_version);
            println!(
                "  {} enumerating an absent index root is not an error (generations={} \
                 active_panel={:?})",
                verdict(true),
                empty.generations.len(),
                empty.active_panel_version
            );
            println!(
                "  {} no vanished generation is still reported as published",
                verdict(!stale_reported)
            );
            if stale_reported {
                failures.push(
                    "the sweep reported a generation that no longer exists on disk".to_owned(),
                );
            }
        }
        Err(error) => {
            println!("  FAIL an absent index root errored: {error}");
            failures.push(format!(
                "an absent index root must not error, but did: {error}"
            ));
        }
    }
    restored?;
    let _ = std::fs::remove_file(&stray_file);
    let _ = std::fs::remove_dir(&stray_dir);
    println!();

    if failures.is_empty() {
        println!(
            "search_generation_sweep_fsv: PASS — every check held against the physical vault."
        );
        Ok(())
    } else {
        println!(
            "search_generation_sweep_fsv: FAIL — {} check(s):",
            failures.len()
        );
        for failure in &failures {
            println!("  - {failure}");
        }
        Err(format!("{} FSV check(s) failed", failures.len()).into())
    }
}
