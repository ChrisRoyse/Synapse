//! Authenticated row-value envelopes for optional durable at-rest encryption.

use crate::cf::ColumnFamily;
use crate::mvcc::is_tombstone_value;
use crate::vault::context::VaultContext;
use crate::vault::encode::WriteRow;
use calyx_core::{CalyxError, Result, VaultId};
use std::sync::{Arc, RwLock, RwLockReadGuard};

pub const CALYX_VAULT_VALUE_NOT_ENCRYPTED: &str = "CALYX_VAULT_VALUE_NOT_ENCRYPTED";
const ENVELOPE_MAGIC: &[u8; 4] = b"CXE1";
const AAD_DOMAIN: &[u8] = b"calyx-aster-row-value-v1";

pub type SharedVaultContext = Arc<RwLock<VaultContext>>;

pub fn seal_rows(context: Option<&SharedVaultContext>, rows: &[WriteRow]) -> Result<Vec<WriteRow>> {
    let Some(context) = context else {
        return Ok(rows.to_vec());
    };
    rows.iter()
        .map(|row| {
            Ok(WriteRow {
                cf: row.cf,
                key: row.key.clone(),
                value: seal_value(context, row.cf, &row.key, &row.value)?,
            })
        })
        .collect()
}

pub fn open_rows(
    context: Option<&SharedVaultContext>,
    rows: Vec<WriteRow>,
) -> Result<Vec<WriteRow>> {
    let Some(context) = context else {
        return Ok(rows);
    };
    rows.into_iter()
        .map(|row| {
            Ok(WriteRow {
                cf: row.cf,
                value: open_value(context, row.cf, &row.key, &row.value)?,
                key: row.key,
            })
        })
        .collect()
}

pub fn seal_value(
    context: &SharedVaultContext,
    cf: ColumnFamily,
    key: &[u8],
    value: &[u8],
) -> Result<Vec<u8>> {
    if is_tombstone_value(value) {
        return Ok(value.to_vec());
    }
    let context = read_context(context)?;
    let aad = row_aad(context.vault_id(), cf, key);
    let sealed = context.encrypt_value(value, &aad)?;
    let mut out = Vec::with_capacity(ENVELOPE_MAGIC.len() + sealed.len());
    out.extend_from_slice(ENVELOPE_MAGIC);
    out.extend_from_slice(&sealed);
    Ok(out)
}

pub fn open_value(
    context: &SharedVaultContext,
    cf: ColumnFamily,
    key: &[u8],
    value: &[u8],
) -> Result<Vec<u8>> {
    if is_tombstone_value(value) {
        return Ok(value.to_vec());
    }
    let Some(sealed) = value.strip_prefix(ENVELOPE_MAGIC) else {
        return Err(CalyxError {
            code: CALYX_VAULT_VALUE_NOT_ENCRYPTED,
            message: format!(
                "encrypted vault encountered plaintext {} value for key {}",
                cf.name(),
                hex_prefix(key)
            ),
            remediation: "recreate or migrate the vault with value encryption enabled before opening it with this key",
        });
    };
    let context = read_context(context)?;
    let aad = row_aad(context.vault_id(), cf, key);
    context.decrypt_value(sealed, &aad)
}

/// Opens one owned encrypted row value in its existing allocation.
///
/// Point reads that start from a borrowed value keep using [`open_value`]. SST
/// page readers, however, already own a freshly materialized ciphertext `Vec`;
/// decrypting that allocation in place prevents a full-value allocation pair
/// from being created and abandoned for every row of a long scan.
pub fn open_value_owned(
    context: &SharedVaultContext,
    cf: ColumnFamily,
    key: &[u8],
    mut value: Vec<u8>,
) -> Result<Vec<u8>> {
    if is_tombstone_value(&value) {
        return Ok(value);
    }
    if !value.starts_with(ENVELOPE_MAGIC) {
        return Err(CalyxError {
            code: CALYX_VAULT_VALUE_NOT_ENCRYPTED,
            message: format!(
                "encrypted vault encountered plaintext {} value for key {}",
                cf.name(),
                hex_prefix(key)
            ),
            remediation: "recreate or migrate the vault with value encryption enabled before opening it with this key",
        });
    }
    value.copy_within(ENVELOPE_MAGIC.len().., 0);
    value.truncate(value.len() - ENVELOPE_MAGIC.len());
    let context = read_context(context)?;
    let aad = row_aad(context.vault_id(), cf, key);
    context.decrypt_value_owned(value, &aad)
}

fn row_aad(vault_id: VaultId, cf: ColumnFamily, key: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 1 + 16 + cf.name().len() + 8 + key.len());
    aad.extend_from_slice(AAD_DOMAIN);
    aad.push(0);
    aad.extend_from_slice(&vault_id.as_ulid().to_bytes());
    aad.push(0);
    aad.extend_from_slice(cf.name().as_bytes());
    aad.push(0);
    aad.extend_from_slice(&(key.len() as u64).to_be_bytes());
    aad.extend_from_slice(key);
    aad
}

fn read_context(context: &SharedVaultContext) -> Result<RwLockReadGuard<'_, VaultContext>> {
    context.read().map_err(|_| CalyxError {
        code: "CALYX_VAULT_CONTEXT_LOCK_POISONED",
        message: "vault context lock was poisoned".to_string(),
        remediation: "drop the poisoned process and reopen the vault from durable bytes",
    })
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}
