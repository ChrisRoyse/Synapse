use super::*;

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Opens a durable vault with an injected clock.
    pub fn open_with_clock(
        vault_dir: impl AsRef<Path>,
        vault_id: VaultId,
        vault_salt: impl Into<Vec<u8>>,
        options: VaultOptions,
        clock: C,
    ) -> Result<Self> {
        DurableVault::validate_options(&options)?;
        let vault_root = vault_dir.as_ref().to_path_buf();
        let mut recovery = DurableVault::recover_batches(vault_dir.as_ref(), &options)?;
        let ledger_hook = if options.restore_ledger_hook && options.value_crypto.is_some() {
            Some(ledger_hook::recover_hook(
                &recovery,
                options.ledger_checkpoint.clone(),
            )?)
        } else if options.restore_ledger_hook {
            Some(ledger_hook::recover_hook_from_vault_dir(
                vault_dir.as_ref(),
                &recovery,
                options.ledger_checkpoint.clone(),
                options.tiering_policy.as_ref(),
            )?)
        } else {
            None
        };
        let recovery_report = VaultRecoveryReport {
            last_recovered_seq: recovery.last_recovered_seq,
            torn_tail: recovery.torn_tail.clone(),
        };
        let mut selected_cfs = options.selected_cfs.clone();
        if recovery.migrate_derived_content_model {
            let required = durable::recovery_readback::persistent_search_cfs(
                vault_dir.as_ref(),
                options.tiering_policy.as_ref(),
            )?;
            if let Some(selected) = selected_cfs.as_mut() {
                selected.extend(required);
                selected.sort();
                selected.dedup();
            }
        }
        let router_started_at = std::time::Instant::now();
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_OPEN_START",
            vault_dir = %vault_dir.as_ref().display(),
            selected_cfs = ?selected_cfs,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            router_latest_readback = recovery.router_latest_readback,
            "opening Calyx CF router"
        );
        let router = match &selected_cfs {
            Some(cfs) => CfRouter::open_selected_cfs_with_tiering_crypto_and_lookup_policy(
                vault_dir.as_ref(),
                options.memtable_byte_cap,
                cfs.iter().copied(),
                options.tiering_policy.clone(),
                options.value_crypto.clone(),
                options.eager_router_lookup_on_open,
            ),
            None => CfRouter::open_with_tiering_crypto_and_lookup_policy(
                vault_dir.as_ref(),
                options.memtable_byte_cap,
                options.tiering_policy.clone(),
                options.value_crypto.clone(),
                options.eager_router_lookup_on_open,
            ),
        };
        let router = match router {
            Ok(router) => router,
            Err(error) => {
                tracing::error!(
                    code = "CALYX_ASTER_ROUTER_OPEN_FAILED",
                    vault_dir = %vault_dir.as_ref().display(),
                    selected_cfs = ?selected_cfs,
                    eager_router_lookup_on_open = options.eager_router_lookup_on_open,
                    elapsed_ms = router_started_at.elapsed().as_millis(),
                    error = %error,
                    "Calyx CF router open failed"
                );
                return Err(error);
            }
        };
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_OPEN_DONE",
            vault_dir = %vault_dir.as_ref().display(),
            selected_cfs = ?selected_cfs,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            elapsed_ms = router_started_at.elapsed().as_millis(),
            "opened Calyx CF router"
        );
        if recovery.migrate_derived_content_model {
            recovery.derived_content_floor_seq =
                router.prove_persistent_search_content_watermark(recovery.wal_replay_floor_seq)?;
        }
        let rows = VersionedCfStore::new_with_router_and_policy(
            recovery.last_recovered_seq,
            router,
            recovery.router_latest_readback,
            options.eager_router_lookup_on_open,
        );
        // Derived-content watermark (issue #1100): the manifest floor vouches
        // for checkpointed seqs; replayed batches below re-derive the rest
        // from their CFs.
        rows.advance_derived_content_seq_to_at_least(recovery.derived_content_floor_seq);
        // WAL-tail batches have no durable-batch SSTs yet; write-capable
        // handles must re-stage them so no later manifest advance can strand
        // them behind the WAL replay floor (issue #1132).
        let wal_tail_batches: Vec<(u64, Vec<encode::WriteRow>)> = if options.read_only {
            Vec::new()
        } else {
            recovery
                .batches
                .iter()
                .filter(|batch| batch.seq > recovery.wal_replay_floor_seq)
                .map(|batch| (batch.seq, batch.rows.clone()))
                .collect()
        };
        for batch in recovery.batches {
            let rows_at_seq = batch
                .rows
                .into_iter()
                .map(|row| (row.cf, row.key, row.value));
            rows.restore_batch(batch.seq, rows_at_seq)?;
        }
        rows.set_start_seq(recovery.last_recovered_seq)?;
        if !recovery.router_latest_readback {
            // Full-restore contract (issue #1132): every row physically held
            // in Router-class SSTs must be visible to the restored MVCC state,
            // otherwise snapshot reads on this handle silently miss it.
            let violations = durable::router_coverage::router_only_rows(
                vault_dir.as_ref(),
                options.tiering_policy.as_ref(),
                |cf, key| rows.has_any_version(cf, key),
            )?;
            if !violations.is_empty() {
                return Err(durable::router_coverage::router_only_rows_error(
                    &violations,
                ));
            }
        }
        let mut durable_options = options.clone();
        durable_options.temporal_policy = recovery.temporal_policy;
        durable_options.dedup_policy = recovery.dedup_policy;
        durable_options.retention_horizon = recovery.retention_horizon.clone();
        let dedup_policy = durable_options.dedup_policy.clone().unwrap_or_default();
        let retention_horizon = durable_options.retention_horizon.clone();
        let durable = if options.read_only {
            None
        } else {
            let durable = DurableVault::open_after(
                vault_dir.as_ref(),
                &durable_options,
                recovery.wal_replay_floor_seq,
                recovery.derived_content_floor_seq,
            )?;
            durable.stage_recovered_wal_batches(wal_tail_batches)?;
            Some(durable)
        };
        // Data residency (PRD 30 §4): a caller-supplied pin is enforced against
        // tiering and persisted (conflict-checked, immutable); on reopen the
        // on-disk pin is authoritative and re-enforced against tiering.
        if let Some(pin) = &options.residency {
            if let Some(tiering) = &options.tiering_policy {
                pin.enforce_tier_roots(&tiering.tier_roots())?;
            }
            pin.persist(&vault_root)?;
        }
        let residency = crate::residency::Residency::load(&vault_root)?;
        if options.residency.is_none()
            && let (Some(pin), Some(tiering)) = (&residency, &options.tiering_policy)
        {
            pin.enforce_tier_roots(&tiering.tier_roots())?;
        }
        Ok(Self {
            vault_id,
            vault_salt: vault_salt.into(),
            clock,
            rows,
            durable,
            dedup_policy,
            retention_horizon: Mutex::new(retention_horizon),
            ledger_hook,
            read_only: options.read_only,
            recurrence_write_lock: Mutex::new(()),
            recovery_report,
            residency,
        })
    }
}
