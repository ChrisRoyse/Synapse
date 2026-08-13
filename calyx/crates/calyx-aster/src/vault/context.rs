//! `VaultContext` — the PH60 tenant-isolation aggregate (T07).
//!
//! Every vault-scoped storage operation receives a `VaultContext`, which binds
//! together all four defense-in-depth layers for one vault:
//!
//! - [`VaultKey`] — per-vault AES-256-GCM key (HKDF-derived) for value crypto.
//! - [`KeyspaceGuard`] — per-vault CF-key prefix isolation.
//! - [`GrantStore`] — default-deny cross-vault grants + immutable audit.
//! - [`QuotaGuard`] — per-vault rate limits / backpressure.
//!
//! plus the probed [`ZfsEncryptionStatus`] (outermost crypto-at-rest layer),
//! recorded so the vault manifest can report it.

use crate::cf::ColumnFamily;
use crate::security::zfs::{
    ZfsEncryptionStatus, probe_zfs_encryption, probe_zfs_encryption_for_path,
};
use crate::vault::grant::GrantStore;
use crate::vault::key::{CALYX_DECRYPTION_FAILED, VaultKey};
use crate::vault::keyspace::KeyspaceGuard;
use crate::vault::quota::{QuotaConfig, QuotaGuard};
use calyx_core::{CalyxError, Result, Ts, VaultId};
use calyx_ledger::ActorId;
use rand::TryRngCore;
use rand::rngs::OsRng;
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// AES-GCM nonce length embedded at the front of every encrypted value.
const VALUE_NONCE_LEN: usize = 12;
/// AES-GCM authentication tag length appended by `VaultKey`.
const VALUE_TAG_LEN: usize = 16;
pub const CALYX_VAULT_KEY_SHREDDED: &str = "CALYX_VAULT_KEY_SHREDDED";
pub const CALYX_VAULT_NONCE_RANDOM_FAILED: &str = "CALYX_VAULT_NONCE_RANDOM_FAILED";

/// The single per-vault security aggregate threaded through every storage op.
#[derive(Debug)]
pub struct VaultContext {
    vault_id: VaultId,
    key: VaultKey,
    keyspace: KeyspaceGuard,
    grants: Arc<RwLock<GrantStore>>,
    quota: QuotaGuard,
    zfs_status: ZfsEncryptionStatus,
}

impl VaultContext {
    /// Builds the full PH60 stack for `vault_id`.
    ///
    /// Derives the vault key from `master_key` via HKDF, builds the keyspace
    /// guard, an empty grant store, the quota guard, and probes the ZFS dataset.
    ///
    /// # Errors
    /// [`CALYX_VAULT_KEY_MISSING`](crate::vault::key::CALYX_VAULT_KEY_MISSING)
    /// if `master_key` is empty (propagated from [`VaultKey::derive`]).
    pub fn new(
        vault_id: VaultId,
        master_key: &[u8],
        config: QuotaConfig,
        zfs_dataset: &str,
    ) -> Result<Self> {
        let key = VaultKey::derive(master_key, &vault_id)?;
        Ok(Self {
            vault_id,
            key,
            keyspace: KeyspaceGuard::new(vault_id),
            grants: Arc::new(RwLock::new(GrantStore::new())),
            quota: QuotaGuard::new(vault_id, config),
            zfs_status: probe_zfs_encryption(zfs_dataset),
        })
    }

    /// Builds a context and probes the dataset that actually hosts `vault_path`.
    pub fn new_for_path(
        vault_id: VaultId,
        master_key: &[u8],
        config: QuotaConfig,
        vault_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let key = VaultKey::derive(master_key, &vault_id)?;
        Ok(Self {
            vault_id,
            key,
            keyspace: KeyspaceGuard::new(vault_id),
            grants: Arc::new(RwLock::new(GrantStore::new())),
            quota: QuotaGuard::new(vault_id, config),
            zfs_status: probe_zfs_encryption_for_path(vault_path),
        })
    }

    /// The vault this context scopes to.
    pub fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// The probed ZFS encryption status (recorded in the vault manifest).
    pub fn zfs_status(&self) -> &ZfsEncryptionStatus {
        &self.zfs_status
    }

    /// The keyspace guard (a `Copy` codec) for direct prefix checks.
    pub fn keyspace(&self) -> KeyspaceGuard {
        self.keyspace
    }

    /// The quota guard.
    pub fn quota(&self) -> &QuotaGuard {
        &self.quota
    }

    /// Shared handle to the grant store (read for checks, write to add/revoke).
    pub fn grants(&self) -> &Arc<RwLock<GrantStore>> {
        &self.grants
    }

    /// Authorizes a cross-vault read from this vault into `dst` for `actor`.
    ///
    /// # Errors
    /// [`CALYX_VAULT_ACCESS_DENIED`](calyx_core::CalyxError::vault_access_denied)
    /// if no active grant exists; the denial is audited in the grant store.
    pub fn check_cross_vault_read(&self, dst: VaultId, actor: ActorId, now: Ts) -> Result<()> {
        self.grants_read()
            .check_grant(self.vault_id, dst, actor, now)
    }

    /// Encodes a storable, vault-prefixed CF key (`prefix ‖ cf_tag ‖ user_key`).
    pub fn encode_key(&self, cf: ColumnFamily, user_key: &[u8]) -> Vec<u8> {
        self.keyspace.encode_key(cf, user_key)
    }

    /// Decodes a raw CF key, verifying it belongs to this vault.
    ///
    /// # Errors
    /// [`CALYX_VAULT_KEYSPACE_MISMATCH`](crate::vault::keyspace::CALYX_VAULT_KEYSPACE_MISMATCH)
    /// for a foreign / short / malformed key.
    pub fn decode_key<'a>(&self, raw: &'a [u8]) -> Result<(ColumnFamily, &'a [u8])> {
        self.keyspace.decode_key(raw)
    }

    /// AES-256-GCM encrypts a value under this vault's key.
    ///
    /// Returns `nonce || ciphertext || tag`, with a fresh 96-bit nonce generated
    /// internally for every call. Callers never supply value nonces.
    pub fn encrypt_value(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if self.key.is_shredded_for_erasure() {
            return Err(CalyxError {
                code: CALYX_VAULT_KEY_SHREDDED,
                message: "vault key has been shredded for erasure".to_string(),
                remediation: "close the erased vault handle and reopen from durable bytes only when policy allows a new key context",
            });
        }
        let mut nonce = [0_u8; VALUE_NONCE_LEN];
        OsRng.try_fill_bytes(&mut nonce).map_err(|error| CalyxError {
            code: CALYX_VAULT_NONCE_RANDOM_FAILED,
            message: format!("failed to generate AES-GCM value nonce: {error}"),
            remediation: "verify the host random source is available before encrypting vault values",
        })?;
        let mut ciphertext = self.key.encrypt(&nonce, plaintext, aad)?;
        let mut sealed = Vec::with_capacity(VALUE_NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.append(&mut ciphertext);
        Ok(sealed)
    }

    /// AES-256-GCM decrypts a `nonce || ciphertext || tag` value (fails closed).
    pub fn decrypt_value(&self, sealed_value: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if sealed_value.len() < VALUE_NONCE_LEN + VALUE_TAG_LEN {
            return Err(CalyxError {
                code: CALYX_DECRYPTION_FAILED,
                message: format!(
                    "encrypted value is {} bytes, shorter than nonce plus GCM tag",
                    sealed_value.len()
                ),
                remediation: "read the complete encrypted value envelope before decrypting",
            });
        }
        let mut nonce = [0_u8; VALUE_NONCE_LEN];
        nonce.copy_from_slice(&sealed_value[..VALUE_NONCE_LEN]);
        let ciphertext = &sealed_value[VALUE_NONCE_LEN..];
        self.key.decrypt(&nonce, ciphertext, aad)
    }

    /// AES-256-GCM decrypts an owned `nonce || ciphertext || tag` value in its
    /// existing allocation.
    ///
    /// Sequential SST scans already paid for and exclusively own this buffer.
    /// Moving the ciphertext down over the nonce and decrypting in place keeps
    /// one value allocation live instead of churning two value-sized buffers
    /// per row. Authentication and failure behavior are identical to
    /// [`Self::decrypt_value`].
    pub fn decrypt_value_owned(&self, mut sealed_value: Vec<u8>, aad: &[u8]) -> Result<Vec<u8>> {
        if sealed_value.len() < VALUE_NONCE_LEN + VALUE_TAG_LEN {
            return Err(CalyxError {
                code: CALYX_DECRYPTION_FAILED,
                message: format!(
                    "encrypted value is {} bytes, shorter than nonce plus GCM tag",
                    sealed_value.len()
                ),
                remediation: "read the complete encrypted value envelope before decrypting",
            });
        }
        let mut nonce = [0_u8; VALUE_NONCE_LEN];
        nonce.copy_from_slice(&sealed_value[..VALUE_NONCE_LEN]);
        sealed_value.copy_within(VALUE_NONCE_LEN.., 0);
        sealed_value.truncate(sealed_value.len() - VALUE_NONCE_LEN);
        self.key.decrypt_in_place(&nonce, &mut sealed_value, aad)?;
        Ok(sealed_value)
    }

    /// Crypto-shreds the live vault key for lawful/user-requested erasure.
    pub fn shred_key_for_erasure(&mut self) {
        self.key.shred_for_erasure();
    }

    /// Returns true once the live key has been overwritten by the erasure sentinel.
    pub fn is_key_shredded_for_erasure(&self) -> bool {
        self.key.is_shredded_for_erasure()
    }

    /// Charges `cx_count` against this vault's ingest quota at `now_ns`.
    pub fn charge_ingest(&self, cx_count: u32, now_ns: u64) -> Result<()> {
        self.quota.charge_ingest(cx_count, now_ns)
    }

    fn grants_read(&self) -> RwLockReadGuard<'_, GrantStore> {
        self.grants
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write access to the grant store, recovering from lock poisoning.
    pub fn grants_write(&self) -> RwLockWriteGuard<'_, GrantStore> {
        self.grants
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
