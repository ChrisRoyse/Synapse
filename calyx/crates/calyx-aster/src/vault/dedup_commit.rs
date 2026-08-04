use super::base_rewrite::BaseRowRewrite;
use super::{AsterVault, encode, ledger_hook};
use crate::cf::{ColumnFamily, base_key};
use calyx_core::{CalyxError, Clock, Constellation, CxId, Result, Seq};

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub(crate) fn commit_recurrence_batch_locked(
        &self,
        recurrence_rows: Vec<(Vec<u8>, Vec<u8>)>,
        updated_base: Option<BaseRowRewrite>,
        additional_rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>,
    ) -> Result<Seq> {
        let mut rows = Vec::new();
        if let Some(rewrite) = updated_base.as_ref() {
            let cx = rewrite.constellation();
            if cx.vault_id != self.vault_id {
                return Err(CalyxError::vault_access_denied(
                    "recurrence base update belongs to another vault",
                ));
            }
            rows.push(encode::WriteRow {
                cf: ColumnFamily::Base,
                key: base_key(cx.cx_id),
                // Carries the stored slot hashes; re-encoding from the decoded
                // constellation clobbered them on every occurrence (#1888).
                value: rewrite.encode()?,
            });
        }
        rows.extend(
            recurrence_rows
                .into_iter()
                .map(|(key, value)| encode::WriteRow {
                    cf: ColumnFamily::Recurrence,
                    key,
                    value,
                }),
        );
        rows.extend(
            additional_rows
                .into_iter()
                .map(|(cf, key, value)| encode::WriteRow { cf, key, value }),
        );
        self.commit_rows_locked(&rows)
    }

    pub(crate) fn commit_online_rows<I>(&self, rows: I) -> Result<Seq>
    where
        I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    {
        let rows = rows
            .into_iter()
            .map(|(key, value)| encode::WriteRow {
                cf: ColumnFamily::Online,
                key,
                value,
            })
            .collect::<Vec<_>>();
        self.commit_rows(&rows)
    }

    pub(crate) fn commit_dedup_ingest_locked(
        &self,
        mut constellation: Option<Constellation>,
        updated_base: Option<BaseRowRewrite>,
        online_rows: Vec<(Vec<u8>, Vec<u8>)>,
        recurrence_rows: Vec<(Vec<u8>, Vec<u8>)>,
        subject: CxId,
        ledger_payload: Vec<u8>,
    ) -> Result<Seq> {
        let mut rows = Vec::new();
        let mut hook_guard = match &self.ledger_hook {
            Some(hook) => Some(ledger_hook::lock_hook(hook)?),
            None => None,
        };
        let staged_ledger = if let Some(hook) = hook_guard.as_deref() {
            let staged =
                ledger_hook::stage_ingest_payload(hook, &mut rows, subject, ledger_payload)?;
            if let Some(cx) = constellation.as_mut() {
                cx.provenance = staged
                    .first()
                    .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                    .ledger_ref();
            }
            Some(staged)
        } else {
            let ledger_ref =
                self.stage_raw_ingest_ledger_locked(&mut rows, subject, ledger_payload)?;
            if let Some(cx) = constellation.as_mut() {
                cx.provenance = ledger_ref;
            }
            None
        };
        if let Some(cx) = constellation.as_ref() {
            self.stage_constellation_rows(&mut rows, cx)?;
        }
        if let Some(rewrite) = updated_base.as_ref() {
            let cx = rewrite.constellation();
            if cx.vault_id != self.vault_id {
                return Err(CalyxError::vault_access_denied(
                    "dedup recurrence base update belongs to another vault",
                ));
            }
            rows.push(encode::WriteRow {
                cf: ColumnFamily::Base,
                key: base_key(cx.cx_id),
                value: rewrite.encode()?,
            });
        }
        for (key, value) in online_rows {
            rows.push(encode::WriteRow {
                cf: ColumnFamily::Online,
                key,
                value,
            });
        }
        for (key, value) in recurrence_rows {
            rows.push(encode::WriteRow {
                cf: ColumnFamily::Recurrence,
                key,
                value,
            });
        }
        let seq = self.commit_rows_locked(&rows)?;
        if let (Some(hook), Some(staged)) = (hook_guard.take(), staged_ledger.as_ref()) {
            self.commit_persistent_ledger_staged_locked(hook, staged, "commit_dedup_ingest")?;
        }
        Ok(seq)
    }

    pub(crate) fn commit_constellation_recurrence_rows_locked(
        &self,
        mut constellation: Constellation,
        updated_base: BaseRowRewrite,
        recurrence_rows: Vec<(Vec<u8>, Vec<u8>)>,
        additional_rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>,
        ledger_payload: Vec<u8>,
    ) -> Result<Seq> {
        if constellation.vault_id != self.vault_id {
            return Err(CalyxError::vault_access_denied(
                "atomic recurrence constellation belongs to another vault",
            ));
        }
        constellation.validate_schema()?;
        let mut rows = additional_rows
            .into_iter()
            .map(|(cf, key, value)| encode::WriteRow { cf, key, value })
            .collect::<Vec<_>>();
        let mut hook_guard = match &self.ledger_hook {
            Some(hook) => Some(ledger_hook::lock_hook(hook)?),
            None => None,
        };
        let staged_ledger = if let Some(hook) = hook_guard.as_deref() {
            let staged = ledger_hook::stage_ingest_payload(
                hook,
                &mut rows,
                constellation.cx_id,
                ledger_payload,
            )?;
            constellation.provenance = staged
                .first()
                .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ingest rows"))?
                .ledger_ref();
            Some(staged)
        } else {
            constellation.provenance = self.stage_raw_ingest_ledger_locked(
                &mut rows,
                constellation.cx_id,
                ledger_payload,
            )?;
            None
        };
        self.stage_constellation_rows(&mut rows, &constellation)?;
        rows.push(encode::WriteRow {
            cf: ColumnFamily::Base,
            key: base_key(updated_base.constellation().cx_id),
            value: updated_base.encode()?,
        });
        rows.extend(
            recurrence_rows
                .into_iter()
                .map(|(key, value)| encode::WriteRow {
                    cf: ColumnFamily::Recurrence,
                    key,
                    value,
                }),
        );
        let seq = self.commit_rows_locked(&rows)?;
        if let (Some(hook), Some(staged)) = (hook_guard.take(), staged_ledger.as_ref()) {
            self.commit_persistent_ledger_staged_locked(
                hook,
                staged,
                "commit_constellation_recurrence_rows",
            )?;
        }
        Ok(seq)
    }

    pub(crate) fn commit_dedup_undo_locked(
        &self,
        restored: Vec<Constellation>,
        updated_bases: Vec<Constellation>,
        recurrence_rows: Vec<(Vec<u8>, Vec<u8>)>,
        subject: CxId,
        ledger_payload: Vec<u8>,
    ) -> Result<Seq> {
        let mut rows = Vec::new();
        let mut hook_guard = match &self.ledger_hook {
            Some(hook) => Some(ledger_hook::lock_hook(hook)?),
            None => None,
        };
        let staged_ledger = if let Some(hook) = hook_guard.as_deref() {
            Some(ledger_hook::stage_ingest_payload(
                hook,
                &mut rows,
                subject,
                ledger_payload,
            )?)
        } else {
            self.stage_raw_ingest_ledger_locked(&mut rows, subject, ledger_payload)?;
            None
        };
        for cx in &restored {
            if cx.vault_id != self.vault_id {
                return Err(CalyxError::vault_access_denied(
                    "dedup undo restore belongs to another vault",
                ));
            }
            self.stage_constellation_rows(&mut rows, cx)?;
        }
        for cx in &updated_bases {
            if cx.vault_id != self.vault_id {
                return Err(CalyxError::vault_access_denied(
                    "dedup undo base update belongs to another vault",
                ));
            }
            rows.push(encode::WriteRow {
                cf: ColumnFamily::Base,
                key: base_key(cx.cx_id),
                value: encode::encode_constellation_base(cx)?,
            });
        }
        for (key, value) in recurrence_rows {
            rows.push(encode::WriteRow {
                cf: ColumnFamily::Recurrence,
                key,
                value,
            });
        }
        let rows = latest_rows(rows);
        let seq = self.commit_rows_locked(&rows)?;
        if let (Some(hook), Some(staged)) = (hook_guard.take(), staged_ledger.as_ref()) {
            self.commit_persistent_ledger_staged_locked(hook, staged, "commit_dedup_undo")?;
        }
        Ok(seq)
    }
}

fn latest_rows(rows: Vec<encode::WriteRow>) -> Vec<encode::WriteRow> {
    let mut latest = Vec::<encode::WriteRow>::new();
    for row in rows {
        if let Some(index) = latest
            .iter()
            .position(|existing| existing.cf == row.cf && existing.key == row.key)
        {
            latest[index] = row;
        } else {
            latest.push(row);
        }
    }
    latest
}
