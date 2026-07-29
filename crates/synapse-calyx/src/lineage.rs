//! Vault lineage journal: the one durable record of vault identity that lives
//! *outside* the vault directory.
//!
//! # Why this exists
//!
//! Before this module, every marker that could prove a vault had ever existed —
//! `vault-identity.json`, the manifest, the ledger, the CF directories — lived
//! inside the vault directory itself. A `Remove-Item -Recurse -Force <vault>`
//! therefore removed the data *and* every witness to it, and the next open was
//! an ordinary, silent, successful open of an empty directory. That is exactly
//! what happened on 2026-07-27, losing ~1.56M sequences with no record and no
//! backup (issue #1875).
//!
//! The fix follows the pattern that mature durable systems already use: keep the
//! system identity somewhere the data directory's destruction cannot reach, and
//! compare it on every open.
//!
//! * `PostgreSQL` writes a 64-bit `system_identifier` into `pg_control` at
//!   `initdb`; standbys, `pg_basebackup`, pgBackRest and Patroni compare it
//!   against the identity recorded outside the data directory and fail loudly
//!   with "database system identifier differs" rather than replicating onto a
//!   re-initialised cluster.
//! * Kafka writes `cluster.id` into `meta.properties` in each log dir and the
//!   broker *refuses to start* when it does not match the cluster's recorded id
//!   ("The Cluster ID X doesn't match stored clusterId Y"), because a mismatch
//!   means the durable state was replaced underneath it.
//!
//! Both treat a replaced substrate as a fail-closed condition that needs an
//! explicit operator act to clear, never as a silent fresh start. This module
//! does the same for the Synapse vault.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SynapseCalyxError;

/// Schema version of the on-disk lineage journal.
pub const LINEAGE_SCHEMA_VERSION: u32 = 1;

/// Environment variable that acknowledges an observed vault replacement. Its
/// value must be the exact vault id of the *new* vault, so an acknowledgement
/// can never be a blanket "ignore all resets" switch.
pub const ACKNOWLEDGE_RESET_ENV: &str = "SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET";

const LINEAGE_SUFFIX: &str = ".lineage.json";

const RESET_REMEDIATION: &str = "the vault directory was replaced: its contents are NOT the vault \
                                 recorded in the lineage journal. Do not continue casually — the \
                                 previous vault's rows are unrecoverable without a backup \
                                 (see issue #1687 restore runbook). If the replacement is \
                                 intended, restore the backup first; to accept the new empty \
                                 vault permanently, set \
                                 SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET to the exact new vault id \
                                 printed in this error and reopen once";

const REGRESSION_REMEDIATION: &str = "the vault kept its identity but its durable sequence went backwards, which means committed \
     rows were removed or rolled back underneath the daemon. Stop writers and investigate before \
     reopening; to accept the shorter vault permanently, set \
     SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET to the exact vault id printed in this error";

const LINEAGE_IO_REMEDIATION: &str = "the vault lineage journal is the only record of vault identity that survives deleting the \
     vault directory; repair or restore the exact file named in this error rather than deleting \
     it";

/// Whether the caller is *creating* the vault on this open, or adopting one
/// that already existed.
///
/// This is deliberately supplied by the caller rather than inferred inside the
/// journal, because only the caller can know it. `PostgreSQL` follows the same
/// rule: the cluster's `system_identifier` is *produced by `initdb`* — at the
/// moment the cluster is created — and never reconstructed afterwards from the
/// data directory's contents. A vault whose identity was minted by this very
/// open, and which carries no durable sequences yet, has nothing before it that
/// the journal could have missed; a vault the journal is merely being attached
/// to does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultOpenGenesis {
    /// This open minted `vault-identity.json` for a vault that did not exist
    /// before. Combined with `latest_seq == 0`, the journal witnesses the
    /// vault's entire life.
    CreatedThisOpen,
    /// The vault directory already carried an identity before this open. The
    /// journal cannot attest anything that happened before it.
    PreExisting,
}

/// One contiguous life of a vault directory: opened, written to, and (if it was
/// replaced) superseded by the next generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultLineageGeneration {
    /// 1-based generation counter for this vault directory.
    pub generation: u64,
    /// Stable vault id (`vault-identity.json`) observed for this generation.
    pub vault_id: String,
    /// Reason this generation began.
    pub started_reason: String,
    pub first_observed_unix_ms: u64,
    pub last_observed_unix_ms: u64,
    /// Highest durable `latest_seq` ever observed for this generation. This is
    /// the number that proves a later empty vault is a loss and not a genesis.
    pub high_water_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_vault_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_high_water_seq: Option<u64>,
    /// Free-text operator provenance for generations reconstructed from
    /// evidence rather than observed live (issue #1875).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VaultLineageDisk {
    schema_version: u32,
    /// The vault directory this journal describes, so a journal can never be
    /// silently applied to a different vault.
    vault_dir: String,
    generations: Vec<VaultLineageGeneration>,
}

/// How the currently open vault relates to everything recorded before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxVaultLineage {
    pub lineage_path: PathBuf,
    /// 1-based generation of the vault that is open right now.
    pub generation: u64,
    /// Number of recorded replacements before this generation.
    pub reset_count: u64,
    /// `vault-genesis` (this journal was created together with the vault, at
    /// `latest_seq == 0`, so it saw every sequence the vault ever held),
    /// `lineage-seeded` (the journal was attached to a vault that already
    /// existed, so an unattested prefix precedes it), or `post-reset` (this
    /// chain begins after a recorded replacement).
    pub chain_origin: String,
    /// Durable sequence at which lineage tracking for this generation began.
    pub generation_origin_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_vault_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_high_water_seq: Option<u64>,
    /// True when this open created the journal, so nothing before it is
    /// attested by physical evidence.
    pub seeded_this_open: bool,
}

impl SynapseCalyxVaultLineage {
    /// True when the verified chain can be claimed to cover the vault's whole
    /// history: the journal was written at the vault's own genesis and no
    /// replacement has been recorded since.
    ///
    /// Before #1884 this was a constant `false`: no code path ever wrote
    /// `vault-genesis`, so the only reachable origins were `lineage-seeded`
    /// and `post-reset`. A permanently-negative provenance field trains
    /// operators to ignore it exactly when it starts telling the truth, which
    /// is the failure mode #1875 exists to prevent.
    #[must_use]
    pub fn chain_covers_full_history(&self) -> bool {
        self.chain_origin == "vault-genesis"
    }
}

/// Path of the lineage journal for `vault_dir`: a sibling file, so deleting the
/// vault directory does not delete its own witness.
#[must_use]
pub fn lineage_path(vault_dir: &Path) -> PathBuf {
    let name = vault_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("vault");
    let parent = vault_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    parent.join(format!("{name}{LINEAGE_SUFFIX}"))
}

fn canonical_dir_key(vault_dir: &Path) -> String {
    vault_dir
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
}

/// Evaluates the open vault against the lineage journal, fails closed on an
/// unacknowledged replacement or sequence regression, and records the observed
/// state back to disk.
///
/// `now_unix_ms` is supplied by the caller so the vault's configured clock — not
/// an ambient wall clock — stamps the journal.
///
/// `genesis` records whether this open *created* the vault. It decides, once
/// and permanently, whether the journal may later claim to cover the vault's
/// full history; nothing observable after the fact can recover that answer,
/// which is why it is recorded at creation rather than inferred (#1884).
///
/// # Errors
///
/// Fails closed when the journal cannot be read/written/parsed, when it belongs
/// to a different vault directory, or when a replacement or regression is
/// observed without an exact-id acknowledgement.
#[allow(clippy::too_many_lines)]
pub fn evaluate_and_record(
    vault_dir: &Path,
    vault_id: &str,
    latest_seq: u64,
    now_unix_ms: u64,
    acknowledgement: Option<&str>,
    genesis: VaultOpenGenesis,
) -> Result<SynapseCalyxVaultLineage, SynapseCalyxError> {
    let path = lineage_path(vault_dir);
    let Some(mut disk) = read_lineage(&path)? else {
        // Genesis is claimed only on the conjunction of two physical facts:
        // this open minted the vault identity, AND the vault holds no durable
        // sequences. Either alone is insufficient — an identity file restored
        // beside populated column families would mint an id over real history.
        let genesis_proven = genesis == VaultOpenGenesis::CreatedThisOpen && latest_seq == 0;
        let started_reason = if genesis_proven {
            "vault-genesis"
        } else {
            "lineage-seeded"
        };
        let generation = VaultLineageGeneration {
            generation: 1,
            vault_id: vault_id.to_owned(),
            started_reason: started_reason.to_owned(),
            first_observed_unix_ms: now_unix_ms,
            last_observed_unix_ms: now_unix_ms,
            high_water_seq: latest_seq,
            predecessor_vault_id: None,
            predecessor_high_water_seq: None,
            note: None,
        };
        let disk = VaultLineageDisk {
            schema_version: LINEAGE_SCHEMA_VERSION,
            vault_dir: vault_dir.to_string_lossy().into_owned(),
            generations: vec![generation],
        };
        write_lineage(&path, &disk)?;
        if genesis_proven {
            tracing::info!(
                code = "SYNAPSE_CALYX_VAULT_LINEAGE_GENESIS_RECORDED",
                vault_dir = %vault_dir.display(),
                lineage_path = %path.display(),
                vault_id,
                latest_seq,
                "recorded the vault lineage journal at this vault's own genesis; the journal \
                 attests every sequence this vault will ever hold"
            );
        } else {
            tracing::warn!(
                code = "SYNAPSE_CALYX_VAULT_LINEAGE_SEEDED",
                vault_dir = %vault_dir.display(),
                lineage_path = %path.display(),
                vault_id,
                latest_seq,
                identity_created_this_open = genesis == VaultOpenGenesis::CreatedThisOpen,
                "seeded the vault lineage journal over a pre-existing vault; nothing before this \
                 open is attested by it"
            );
        }
        return Ok(SynapseCalyxVaultLineage {
            lineage_path: path,
            generation: 1,
            reset_count: 0,
            chain_origin: started_reason.to_owned(),
            generation_origin_seq: latest_seq,
            predecessor_vault_id: None,
            predecessor_high_water_seq: None,
            seeded_this_open: true,
        });
    };

    if canonical_dir_key(Path::new(&disk.vault_dir)) != canonical_dir_key(vault_dir) {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_LINEAGE_PATH_MISMATCH",
            format!(
                "vault lineage journal {} records vault_dir={} but the open vault is {}",
                path.display(),
                disk.vault_dir,
                vault_dir.display()
            ),
            LINEAGE_IO_REMEDIATION,
        ));
    }

    let Some(active) = disk.generations.last().cloned() else {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_LINEAGE_EMPTY",
            format!(
                "vault lineage journal {} contains no generations",
                path.display()
            ),
            LINEAGE_IO_REMEDIATION,
        ));
    };

    let acknowledged = acknowledgement.is_some_and(|ack| ack.trim() == vault_id);

    if active.vault_id == vault_id {
        if latest_seq < active.high_water_seq {
            tracing::error!(
                code = "SYNAPSE_CALYX_VAULT_SEQ_REGRESSION",
                vault_dir = %vault_dir.display(),
                lineage_path = %path.display(),
                vault_id,
                latest_seq,
                recorded_high_water_seq = active.high_water_seq,
                lost_seq_count = active.high_water_seq - latest_seq,
                acknowledged,
                "durable vault sequence went backwards: committed rows were removed underneath the daemon"
            );
            if !acknowledged {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_VAULT_SEQ_REGRESSION",
                    format!(
                        "vault {vault_id} at {} opened at latest_seq={latest_seq} but the lineage \
                         journal {} records high_water_seq={} for this same vault id: {} \
                         sequences are missing",
                        vault_dir.display(),
                        path.display(),
                        active.high_water_seq,
                        active.high_water_seq - latest_seq
                    ),
                    REGRESSION_REMEDIATION,
                ));
            }
            let index = disk.generations.len() - 1;
            disk.generations[index].note = Some(format!(
                "operator-acknowledged sequence regression from high_water_seq={} to \
                 latest_seq={latest_seq}",
                active.high_water_seq
            ));
            disk.generations[index].high_water_seq = latest_seq;
        }
        let index = disk.generations.len() - 1;
        disk.generations[index].last_observed_unix_ms = now_unix_ms;
        if latest_seq > disk.generations[index].high_water_seq {
            disk.generations[index].high_water_seq = latest_seq;
        }
        write_lineage(&path, &disk)?;
        let reset_count = disk.generations.len() as u64 - 1;
        return Ok(SynapseCalyxVaultLineage {
            lineage_path: path,
            generation: active.generation,
            reset_count,
            chain_origin: chain_origin_label(&active),
            generation_origin_seq: 0,
            predecessor_vault_id: active.predecessor_vault_id.clone(),
            predecessor_high_water_seq: active.predecessor_high_water_seq,
            seeded_this_open: false,
        });
    }

    // Identity changed: the directory now holds a different vault than the one
    // the journal last saw. This is the #1875 condition.
    tracing::error!(
        code = "SYNAPSE_CALYX_VAULT_RESET_DETECTED",
        vault_dir = %vault_dir.display(),
        lineage_path = %path.display(),
        observed_vault_id = vault_id,
        observed_latest_seq = latest_seq,
        recorded_vault_id = %active.vault_id,
        recorded_high_water_seq = active.high_water_seq,
        recorded_generation = active.generation,
        recorded_last_observed_unix_ms = active.last_observed_unix_ms,
        acknowledged,
        "vault directory holds a different vault than the lineage journal recorded: the previous \
         vault was replaced"
    );
    if !acknowledged {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED",
            format!(
                "vault directory {} now holds vault_id={vault_id} (latest_seq={latest_seq}) but \
                 the lineage journal {} records vault_id={} at generation {} with \
                 high_water_seq={}; approximately {} durable sequences are unaccounted for",
                vault_dir.display(),
                path.display(),
                active.vault_id,
                active.generation,
                active.high_water_seq,
                active.high_water_seq.saturating_sub(latest_seq)
            ),
            RESET_REMEDIATION,
        ));
    }

    let generation = VaultLineageGeneration {
        generation: active.generation + 1,
        vault_id: vault_id.to_owned(),
        started_reason: "reset-acknowledged".to_owned(),
        first_observed_unix_ms: now_unix_ms,
        last_observed_unix_ms: now_unix_ms,
        high_water_seq: latest_seq,
        predecessor_vault_id: Some(active.vault_id.clone()),
        predecessor_high_water_seq: Some(active.high_water_seq),
        note: None,
    };
    disk.generations.push(generation.clone());
    write_lineage(&path, &disk)?;
    tracing::warn!(
        code = "SYNAPSE_CALYX_VAULT_RESET_RECORDED",
        vault_dir = %vault_dir.display(),
        lineage_path = %path.display(),
        vault_id,
        generation = generation.generation,
        predecessor_vault_id = %active.vault_id,
        predecessor_high_water_seq = active.high_water_seq,
        "recorded an operator-acknowledged vault replacement in the lineage journal"
    );
    let reset_count = disk.generations.len() as u64 - 1;
    Ok(SynapseCalyxVaultLineage {
        lineage_path: path,
        generation: generation.generation,
        reset_count,
        chain_origin: "post-reset".to_owned(),
        generation_origin_seq: latest_seq,
        predecessor_vault_id: generation.predecessor_vault_id,
        predecessor_high_water_seq: generation.predecessor_high_water_seq,
        seeded_this_open: false,
    })
}

fn chain_origin_label(active: &VaultLineageGeneration) -> String {
    match active.started_reason.as_str() {
        "lineage-seeded" => "lineage-seeded".to_owned(),
        "vault-genesis" => "vault-genesis".to_owned(),
        _ => "post-reset".to_owned(),
    }
}

fn read_lineage(path: &Path) -> Result<Option<VaultLineageDisk>, SynapseCalyxError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_VAULT_LINEAGE_READ_FAILED",
                "read vault lineage journal",
                path,
                &error,
                LINEAGE_IO_REMEDIATION,
            ));
        }
    };
    let disk = serde_json::from_str::<VaultLineageDisk>(&raw).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_LINEAGE_INVALID",
            format!("parse vault lineage journal {}: {error}", path.display()),
            LINEAGE_IO_REMEDIATION,
        )
    })?;
    if disk.schema_version != LINEAGE_SCHEMA_VERSION {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_LINEAGE_SCHEMA_UNSUPPORTED",
            format!(
                "vault lineage journal {} schema_version={} expected={LINEAGE_SCHEMA_VERSION}",
                path.display(),
                disk.schema_version
            ),
            LINEAGE_IO_REMEDIATION,
        ));
    }
    Ok(Some(disk))
}

fn write_lineage(path: &Path, disk: &VaultLineageDisk) -> Result<(), SynapseCalyxError> {
    let encoded = serde_json::to_vec_pretty(disk).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_LINEAGE_ENCODE_FAILED",
            format!("encode vault lineage journal {}: {error}", path.display()),
            LINEAGE_IO_REMEDIATION,
        )
    })?;
    calyx_aster::durable_fs::write_atomic_replace(path, &encoded, "vault lineage journal").map_err(
        |error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_VAULT_LINEAGE_WRITE_FAILED",
                format!(
                    "publish vault lineage journal {} failed with {}: {}",
                    path.display(),
                    error.code,
                    error.message
                ),
                LINEAGE_IO_REMEDIATION,
            )
        },
    )
}

/// Reads the acknowledgement environment variable, trimmed, if it is set to a
/// non-empty value.
#[must_use]
pub fn acknowledgement_from_env() -> Option<String> {
    std::env::var(ACKNOWLEDGE_RESET_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
