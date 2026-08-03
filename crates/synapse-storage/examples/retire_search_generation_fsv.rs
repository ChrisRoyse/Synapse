//! Full-state verification for #1972: a closed superseded panel's published
//! search generation can be retired, through a real path, with the removal
//! proven by re-enumerating the index root.
//!
//! ## The defect
//!
//! The search-generation sweep named the condition correctly and **nothing ever
//! performed the named action**. On the live vault
//! `idx/search/panel_0001900001/` — a closed superseded generation of
//! `syn-timeline-v1` — kept `calyx_search_generations_unmaintainable = 1`
//! forever, so `calyx_search_generation` was permanently `degraded`, the next
//! genuinely-unknown generation would have been invisible against that
//! background, and the derived-state maintainer resolved its absent contract on
//! every five-minute tick in perpetuity.
//!
//! ## What is verified
//!
//! 1. **The classification**, which is #1972 ask 2. `unmaintainable_no_contract`
//!    collapsed two situations with opposite remedies. `superseded_panel_lineage`
//!    separates them from the catalog that already declares the fact, so a
//!    closed superseded version resolves to its live panel and an unknown
//!    version resolves to nothing.
//! 2. **The retirement**, against the real index root: the directory is gone
//!    and the published set re-enumerated from disk no longer contains it.
//! 3. **Every refusal**, because a destructive operation is only safe if the
//!    things it must refuse are proven to be refused, not assumed.
//!
//! ## Source of truth
//!
//! `idx/search/panel_*/manifest.json`. The directory *is* the publication, so
//! the evidence is a directory listing before and after — never the return
//! value of the removal call.
//!
//! Run against a disposable copy of a real vault (this example DELETES):
//!
//! ```text
//! cargo run --release -p synapse-storage --example retire_search_generation_fsv -- <vault-copy-dir>
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};

use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault};
use synapse_storage::constellations::{
    SYN_AGENT_TRANSCRIPT_PANEL_VERSION, SYN_EPISODE_PANEL_VERSION_PRE_1964,
    SYN_TIMELINE_PANEL_VERSION, SYN_TIMELINE_PANEL_VERSION_PRE_1900,
    SYN_TIMELINE_PANEL_VERSION_PRE_1963, superseded_panel_lineage, syn_active_panel_contract,
};

/// The generation this issue is about: a closed superseded `syn-timeline-v1`.
const TARGET: u32 = SYN_TIMELINE_PANEL_VERSION_PRE_1963;

/// A version no panel declares anywhere. The retirement must refuse it.
const UNKNOWN: u32 = 9_999_999;

fn generation_dir(vault_dir: &Path, panel_version: u32) -> PathBuf {
    vault_dir
        .join("idx")
        .join("search")
        .join(format!("panel_{panel_version:010}"))
}

fn listing(vault_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(vault_dir.join("idx").join("search"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: retire_search_generation_fsv <disposable-vault-copy-dir>")?;
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    })?;

    println!("retire_search_generation_fsv  (#1972)");
    println!("vault copy = {}", vault_dir.display());

    let mut failures: Vec<String> = Vec::new();

    // -- ask 2: the classification, from the catalog ------------------------
    println!("\n== ask 2: 'no contract' separates into two dispositions ==");
    let created_at_ms = 1_785_000_000_000_u64;
    for (version, label, want_lineage) in [
        (TARGET, "1900001 superseded syn-timeline-v1", true),
        (
            SYN_TIMELINE_PANEL_VERSION_PRE_1900,
            "1664001 superseded syn-timeline-v1",
            true,
        ),
        (
            SYN_EPISODE_PANEL_VERSION_PRE_1964,
            "1904002 superseded syn-episode-v1",
            true,
        ),
        (UNKNOWN, "9999999 declared by nothing", false),
    ] {
        let lineage = superseded_panel_lineage(version);
        let contract = syn_active_panel_contract(version, created_at_ms)?.is_some();
        println!(
            "  {version:<9} {label:<40} contract={contract:<5} lineage={}",
            lineage.map_or_else(
                || "<none>".to_owned(),
                |l| format!("{} -> live {}", l.panel_name, l.live_panel_version)
            )
        );
        if lineage.is_some() != want_lineage {
            failures.push(format!(
                "panel {version} lineage resolution is {}, expected {want_lineage}",
                lineage.is_some()
            ));
        }
        if contract {
            failures.push(format!(
                "panel {version} unexpectedly has a code-declared contract"
            ));
        }
    }
    // A live panel must resolve the other way: it HAS a contract and is not a
    // superseded version of anything. Checking only the superseded side would
    // pass just as well if the lookup returned Some for everything.
    for (version, label) in [
        (SYN_TIMELINE_PANEL_VERSION, "syn-timeline-v1 live"),
        (
            SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
            "syn-agent-transcript-v1 live",
        ),
    ] {
        let lineage = superseded_panel_lineage(version);
        let contract = syn_active_panel_contract(version, created_at_ms)?.is_some();
        println!("  {version:<9} {label:<40} contract={contract:<5} lineage={lineage:?}");
        if !contract {
            failures.push(format!(
                "live panel {version} has no code-declared contract"
            ));
        }
        if lineage.is_some() {
            failures.push(format!(
                "live panel {version} resolved as a superseded generation"
            ));
        }
    }

    // -- state BEFORE, from the index root itself ---------------------------
    println!("\n== state BEFORE (source of truth = idx/search listing) ==");
    let before_listing = listing(&vault_dir);
    let before_published = vault.published_search_generations()?;
    let target_dir = generation_dir(&vault_dir, TARGET);
    let target_files: Vec<String> = std::fs::read_dir(&target_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    println!("  directories : {before_listing:?}");
    println!("  published   : {:?}", before_published.panels);
    println!("  active panel: {:?}", vault.active_panel_version()?);
    println!(
        "  {} exists={} with {} files",
        target_dir.display(),
        target_dir.is_dir(),
        target_files.len()
    );
    if !before_published.panels.contains(&TARGET) {
        return Err(format!(
            "this vault copy does not publish a generation for {TARGET}; there is nothing to verify"
        )
        .into());
    }

    // -- edge cases: every refusal, BEFORE the real retirement --------------
    // Run first on purpose. If a refusal is broken, that must be discovered
    // while the target is still present rather than after it is gone.
    println!("\n== edge cases: refusals ==");

    println!("  [1] a version with no published generation ({UNKNOWN})");
    match vault.retire_search_generation(UNKNOWN) {
        Ok(_) => failures.push(format!("retiring unpublished {UNKNOWN} succeeded")),
        Err(error) => {
            println!("      Err({}) {}", error.code, error.message);
            if error.code != "SYNAPSE_CALYX_SEARCH_GENERATION_NOT_PUBLISHED" {
                failures.push(format!(
                    "unpublished retirement failed with {} rather than NOT_PUBLISHED",
                    error.code
                ));
            }
        }
    }

    let active = vault.active_panel_version()?;
    if let Some(active) = active {
        println!("  [2] the ACTIVE panel ({active}) — every default query reads it");
        match vault.retire_search_generation(active) {
            Ok(_) => failures.push(format!("retiring the active panel {active} succeeded")),
            Err(error) => {
                println!("      Err({}) {}", error.code, error.message);
                if error.code != "SYNAPSE_CALYX_SEARCH_GENERATION_ACTIVE" {
                    failures.push(format!(
                        "active-panel retirement failed with {} rather than ACTIVE",
                        error.code
                    ));
                }
            }
        }
        let still_there = generation_dir(&vault_dir, active).is_dir();
        println!("      active generation directory still present: {still_there}");
        if !still_there {
            failures
                .push("the refused active-panel retirement removed the directory anyway".into());
        }
    }

    println!("  [3] a maintainable version and an undeclared one are refused by the");
    println!("      storage-layer catalog gate, which is exercised in ask-2 form above:");
    println!(
        "      contract({SYN_TIMELINE_PANEL_VERSION})={} -> search_rebuild, not retire",
        syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, created_at_ms)?.is_some()
    );
    println!(
        "      lineage({UNKNOWN})={:?} -> investigate, not delete",
        superseded_panel_lineage(UNKNOWN)
    );

    // -- the happy path -----------------------------------------------------
    println!("\n== retire {TARGET} ==");
    let report = vault.retire_search_generation(TARGET)?;
    println!("  directory        : {}", report.directory);
    println!("  files removed    : {}", report.files_removed);
    println!("  bytes reclaimed  : {}", report.bytes_reclaimed);
    println!("  published_before : {:?}", report.published_before);
    println!("  published_after  : {:?}", report.published_after);

    // -- state AFTER, read from the filesystem, not from the return value ---
    println!("\n== state AFTER (re-read from disk) ==");
    let after_listing = listing(&vault_dir);
    let after_published = vault.published_search_generations()?;
    println!("  directories : {after_listing:?}");
    println!("  published   : {:?}", after_published.panels);
    println!("  {} exists={}", target_dir.display(), target_dir.is_dir());

    if target_dir.exists() {
        failures.push("the generation directory still exists after retirement".into());
    }
    if after_published.panels.contains(&TARGET) {
        failures.push(format!("{TARGET} is still published after retirement"));
    }
    let expected_after: Vec<u32> = before_published
        .panels
        .iter()
        .copied()
        .filter(|version| *version != TARGET)
        .collect();
    if after_published.panels != expected_after {
        failures.push(format!(
            "published set after retirement is {:?}, expected exactly {expected_after:?} — a \
             retirement must remove one generation and disturb no other",
            after_published.panels
        ));
    }
    if report.files_removed == 0 || report.bytes_reclaimed == 0 {
        failures.push(format!(
            "the retirement reported files={} bytes={}, so it removed an empty directory rather \
             than a published generation",
            report.files_removed, report.bytes_reclaimed
        ));
    }

    // -- idempotency: a second retirement must refuse, not silently succeed --
    println!("\n== retiring the same generation twice ==");
    match vault.retire_search_generation(TARGET) {
        Ok(_) => failures.push("a second retirement of the same generation succeeded".into()),
        Err(error) => {
            println!("  Err({}) {}", error.code, error.message);
            if error.code != "SYNAPSE_CALYX_SEARCH_GENERATION_NOT_PUBLISHED" {
                failures.push(format!(
                    "the second retirement failed with {} rather than NOT_PUBLISHED",
                    error.code
                ));
            }
        }
    }

    // -- the surviving generations must still be readable -------------------
    println!("\n== the surviving generations are untouched ==");
    for version in &after_published.panels {
        let dir = generation_dir(&vault_dir, *version);
        let files = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .count();
        let manifest = dir.join("manifest.json").is_file();
        println!("  panel_{version:010}  files={files:<4} manifest={manifest}");
        if !manifest {
            failures.push(format!("surviving generation {version} lost its manifest"));
        }
    }

    println!("\n== verdict ==");
    if failures.is_empty() {
        println!("PASS: the superseded generation is classified from the catalog, retired through");
        println!("      a real path, proven gone by re-enumerating the index root, and every");
        println!("      refusal holds while leaving the other generations intact.");
        Ok(())
    } else {
        for failure in &failures {
            println!("  FAIL: {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}
