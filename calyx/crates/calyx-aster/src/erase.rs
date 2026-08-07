//! Lawful/user-requested erasure for Aster vault content (PH61 T01).

mod format;
mod ledger;
mod targets;

use crate::mvcc::tombstone_value;
use crate::vault::{AsterVault, VaultContext, encode};
use calyx_core::{CalyxError, Clock, CxId, Result, Ts, VaultId};
use calyx_ledger::{EntryKind, ErasureTombstone, SubjectId};
use format::hex;
use serde::{Deserialize, Serialize};
use targets::{affected_cfs, collect_targets};

/// Metadata key used by `EraseScope::Subject`.
///
/// Store `subject_metadata_value(subject)` in constellation metadata under this
/// key to make a subject-level erasure select that constellation.
pub const METADATA_SUBJECT_ID: &str = "subject_id";

/// One lawful erase target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EraseScope {
    Vault,
    Cx(CxId),
    Subject(SubjectId),
}

/// Result of an erase operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EraseResult {
    pub scope: EraseScope,
    /// Number of base constellations erased. Derived CF rows are tombstoned too.
    pub records_deleted: usize,
    pub shredded_at: Ts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<ErasureTombstone>,
}

/// Pluggable cleanup hook for derived data owned outside Aster's core CFs.
pub trait EraseHandler: Send + Sync {
    fn erase(&self, scope: &EraseScope, vault_id: VaultId) -> Result<()>;
}

/// Handler collection run during erasure.
#[derive(Default)]
pub struct EraseRegistry {
    handlers: Vec<Box<dyn EraseHandler>>,
}

impl EraseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_handler<H>(&mut self, handler: H)
    where
        H: EraseHandler + 'static,
    {
        self.handlers.push(Box::new(handler));
    }

    pub fn run_all(&self, scope: &EraseScope, vault_id: VaultId) -> Result<()> {
        for handler in &self.handlers {
            handler.erase(scope, vault_id)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for EraseRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EraseRegistry")
            .field("handler_count", &self.handlers.len())
            .finish()
    }
}

/// No-op derived-data eraser for crates that have no rows to remove yet.
#[derive(Debug, Default)]
pub struct NoopEraseHandler;

impl EraseHandler for NoopEraseHandler {
    fn erase(&self, _scope: &EraseScope, _vault_id: VaultId) -> Result<()> {
        Ok(())
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn erase(
        &self,
        scope: EraseScope,
        vault_ctx: &mut VaultContext,
        registry: &EraseRegistry,
    ) -> Result<EraseResult> {
        erase(self, scope, vault_ctx, registry)
    }

    pub fn erase_defer_key_shred(
        &self,
        scope: EraseScope,
        vault_ctx: &VaultContext,
        registry: &EraseRegistry,
    ) -> Result<EraseResult> {
        erase_inner(self, scope, vault_ctx.vault_id(), None, registry)
    }

    /// Lawfully erases `scope`'s content — CF-row tombstones, an append-only
    /// `EntryKind::Erase` Ledger entry, and physical purge of the tombstoned
    /// rows — without shredding the whole-vault crypto key.
    ///
    /// This is the per-record / per-subject erasure entrypoint used by the
    /// Synapse privacy surface: content becomes unrecoverable at the vault while
    /// the hash chain gains the erasure entry and stays verifiable. Whole-vault
    /// key crypto-shred is deliberately not performed here (it would destroy the
    /// entire vault's value crypto); it belongs to a full [`EraseScope::Vault`]
    /// flow that owns a mutable [`VaultContext`].
    ///
    /// # Errors
    ///
    /// Returns a structured error when the scope already carries an erasure
    /// tombstone, or when target collection, commit, ledger append, or physical
    /// purge fails. Fails closed before any partial visibility.
    pub fn erase_scope_ledger_stamped(
        &self,
        scope: EraseScope,
        registry: &EraseRegistry,
    ) -> Result<EraseResult> {
        erase_inner(self, scope, self.vault_id(), None, registry)
    }
}

// A25 ("never delete to compress") does NOT forbid this operation.
// Erasure is lawful/user-requested deletion, explicitly required by A33.
// No reviewer may reject a lawful erase citing A25.
pub fn erase<C>(
    vault: &AsterVault<C>,
    scope: EraseScope,
    vault_ctx: &mut VaultContext,
    registry: &EraseRegistry,
) -> Result<EraseResult>
where
    C: Clock,
{
    erase_inner(
        vault,
        scope,
        vault_ctx.vault_id(),
        Some(vault_ctx),
        registry,
    )
}

fn erase_inner<C>(
    vault: &AsterVault<C>,
    scope: EraseScope,
    context_vault_id: VaultId,
    mut vault_ctx: Option<&mut VaultContext>,
    registry: &EraseRegistry,
) -> Result<EraseResult>
where
    C: Clock,
{
    if context_vault_id != vault.vault_id() {
        return Err(CalyxError::vault_access_denied(
            "erase VaultContext belongs to another vault",
        ));
    }
    vault.with_native_compaction_guard("erase_scope_tombstone", || {
        vault.with_durable_commit_lock(|| {
            let snapshot = vault.latest_seq();
            let real_ledger = vault.has_real_ledger_hook();
            if real_ledger
                && let Some(tombstone) = ledger::existing_tombstone(vault, &scope, snapshot)?
            {
                if let Some(ctx) = vault_ctx.as_mut()
                    && (scope == EraseScope::Vault || tombstone.records_deleted > 0)
                {
                    vault.flush_locked()?;
                    ctx.shred_key_for_erasure();
                }
                return Err(CalyxError::erase_already_tombstoned(format!(
                    "erase scope already has ledger tombstone at seq {}",
                    tombstone.seq
                )));
            }
            let targets = collect_targets(vault, &scope, snapshot)?;
            registry.run_all(&scope, vault.vault_id())?;
            let rows_tombstoned = targets.rows.len();
            if scope != EraseScope::Vault && rows_tombstoned == 0 {
                return Ok(EraseResult {
                    scope,
                    records_deleted: targets.records_deleted,
                    shredded_at: vault.clock_now(),
                    tombstone: None,
                });
            }
            let affected = affected_cfs(&targets.rows);
            let row_tombstone = tombstone_value();
            let rows = targets
                .rows
                .iter()
                .map(|target| encode::WriteRow {
                    cf: target.cf,
                    key: target.key.clone(),
                    value: row_tombstone.clone(),
                })
                .collect::<Vec<_>>();
            let mut ledger_tombstone = None;
            if real_ledger {
                let tombstone = ledger::tombstone_for(
                    vault,
                    &scope,
                    targets.records_deleted,
                    vault.clock_now(),
                )?;
                let ledger_ref = vault.commit_erasure_rows_with_ledger_entry_locked(
                    rows,
                    EntryKind::Erase,
                    ledger::tombstone_subject(&tombstone),
                    tombstone.as_ledger_payload(),
                    tombstone.actor.clone(),
                )?;
                debug_assert_eq!(ledger_ref.seq, tombstone.seq);
                ledger_tombstone = Some(tombstone);
            } else {
                vault.commit_erasure_rows_locked(&rows)?;
            }
            if rows_tombstoned > 0 {
                vault.purge_tombstoned_cfs_locked(&affected)?;
            }
            if let Some(ctx) = vault_ctx.as_mut()
                && (scope == EraseScope::Vault || rows_tombstoned > 0)
            {
                vault.flush_locked()?;
                ctx.shred_key_for_erasure();
            }
            Ok(EraseResult {
                scope,
                records_deleted: targets.records_deleted,
                shredded_at: vault.clock_now(),
                tombstone: ledger_tombstone,
            })
        })
    })
}

/// Tombstones all visible Aster CF rows selected by `scope` through the normal
/// durable commit path. The committed tombstone is the WAL crash-safety record.
pub fn erase_cf_records<C>(
    vault: &AsterVault<C>,
    scope: &EraseScope,
    vault_ctx: &VaultContext,
) -> Result<usize>
where
    C: Clock,
{
    Ok(erase_cf_records_summary(vault, scope, vault_ctx, None)?.records_deleted)
}

fn erase_cf_records_summary<C>(
    vault: &AsterVault<C>,
    scope: &EraseScope,
    vault_ctx: &VaultContext,
    registry: Option<&EraseRegistry>,
) -> Result<EraseWriteSummary>
where
    C: Clock,
{
    if vault_ctx.vault_id() != vault.vault_id() {
        return Err(CalyxError::vault_access_denied(
            "erase VaultContext belongs to another vault",
        ));
    }
    vault.with_native_compaction_guard("erase_scope_purge", || {
        vault.with_durable_commit_lock(|| {
            let snapshot = vault.latest_seq();
            let targets = collect_targets(vault, scope, snapshot)?;
            if let Some(registry) = registry {
                registry.run_all(scope, vault.vault_id())?;
            }
            if targets.rows.is_empty() {
                return Ok(EraseWriteSummary {
                    records_deleted: targets.records_deleted,
                });
            }
            let tombstone = tombstone_value();
            let rows = targets
                .rows
                .iter()
                .map(|target| encode::WriteRow {
                    cf: target.cf,
                    key: target.key.clone(),
                    value: tombstone.clone(),
                })
                .collect::<Vec<_>>();
            vault.commit_erasure_rows_locked(&rows)?;
            vault.purge_tombstoned_cfs_locked(&affected_cfs(&targets.rows))?;
            Ok(EraseWriteSummary {
                records_deleted: targets.records_deleted,
            })
        })
    })
}

pub fn subject_metadata_value(subject: &SubjectId) -> String {
    match subject {
        SubjectId::Cx(id) => format!("cx:{id}"),
        SubjectId::Lens(id) => format!("lens:{id}"),
        SubjectId::Kernel(bytes) => format!("kernel:{}", hex(bytes)),
        SubjectId::Guard(bytes) => format!("guard:{}", hex(bytes)),
        SubjectId::Query(bytes) => format!("query:{}", hex(bytes)),
    }
}

#[derive(Debug)]
struct EraseWriteSummary {
    records_deleted: usize,
}
