//! Per-vault key derivation and authenticated encryption (PH60 · T01).
//!
//! [`VaultKey`] is the innermost cryptographic layer of the defense-in-depth
//! tenant-isolation stack (key + keyspace + grant — PRD `30 §2`). It derives a
//! unique AES-256-GCM key per vault from host-provided key material using
//! HKDF-SHA-256, then provides authenticated encryption that **fails closed**
//! (a tampered tag or truncated ciphertext returns an error — never a silent
//! zero-fill or garbage plaintext, per axiom A16).
//!
//! Key material lives in [`zeroize::Zeroizing`], so the raw 32 bytes are wiped
//! from memory on drop and are never cloned into a static. `Clone` is
//! intentionally **not** derived: a secret should have exactly one owner.

use aes_gcm::aead::{Aead, AeadInPlace, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use calyx_core::{CalyxError, Result, VaultId};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

/// Host-provided key material was empty, so no vault key could be derived.
pub const CALYX_VAULT_KEY_MISSING: &str = "CALYX_VAULT_KEY_MISSING";
/// AES-256-GCM encryption failed inside the cipher.
pub const CALYX_ENCRYPTION_FAILED: &str = "CALYX_ENCRYPTION_FAILED";
/// AES-256-GCM decryption failed: tag mismatch, wrong AAD, or truncated input.
pub const CALYX_DECRYPTION_FAILED: &str = "CALYX_DECRYPTION_FAILED";

/// HKDF salt — domain-separates Calyx vault keys from any other HKDF use of the
/// same master material. Versioned so a future scheme can rotate without
/// colliding with `-v1` keys.
const VAULT_KEY_SALT: &[u8] = b"calyx-vault-key-v1";

/// AES-256 key length in bytes.
const KEY_LEN: usize = 32;
/// AES-GCM nonce length in bytes (96-bit, the GCM-standard size).
const NONCE_LEN: usize = 12;
/// AES-GCM authentication tag length in bytes (128-bit).
const TAG_LEN: usize = 16;

/// A per-vault AES-256-GCM key derived from host master material.
///
/// Holds 32 secret bytes in [`Zeroizing`], wiped on drop. Not `Clone`.
pub struct VaultKey {
    inner: Zeroizing<[u8; KEY_LEN]>,
}

impl VaultKey {
    /// Derives a per-vault key from host `master` material via HKDF-SHA-256.
    ///
    /// `ikm = master`, `salt = VAULT_KEY_SALT`, `info = vault_id` ULID bytes —
    /// so two vaults with the same master get distinct keys, and the same
    /// `(master, vault_id)` always derives the identical key (determinism that
    /// the golden-vector test pins).
    ///
    /// # Errors
    /// [`CALYX_VAULT_KEY_MISSING`] if `master` is empty.
    pub fn derive(master: &[u8], vault_id: &VaultId) -> Result<Self> {
        if master.is_empty() {
            return Err(vault_key_missing(
                "host-provided master key material is empty",
            ));
        }
        let info = vault_id.as_ulid().to_bytes();
        let hk = Hkdf::<Sha256>::new(Some(VAULT_KEY_SALT), master);
        let mut okm = Zeroizing::new([0_u8; KEY_LEN]);
        // HKDF-expand only fails when the requested length exceeds 255*HashLen
        // (255*32 = 8160 bytes). KEY_LEN is 32, so this branch is unreachable;
        // we still surface it as an error rather than unwrap, to stay fail-loud.
        hk.expand(&info, okm.as_mut_slice())
            .map_err(|err| CalyxError {
                code: CALYX_VAULT_KEY_MISSING,
                message: format!("HKDF-SHA-256 expand failed: {err}"),
                remediation: "report a bug: HKDF output length is fixed at 32 bytes",
            })?;
        Ok(Self { inner: okm })
    }

    /// Overwrites this live key with the all-zero erasure sentinel.
    pub(crate) fn shred_for_erasure(&mut self) {
        self.inner.as_mut_slice().zeroize();
    }

    /// Reports whether this live key has been overwritten by erasure.
    pub(crate) fn is_shredded_for_erasure(&self) -> bool {
        self.inner.iter().all(|byte| *byte == 0)
    }

    /// Zero-copy borrow of the inner bytes as an AES-256-GCM key reference.
    pub fn aes_gcm_key(&self) -> &Key<Aes256Gcm> {
        Key::<Aes256Gcm>::from_slice(self.inner.as_slice())
    }

    /// AES-256-GCM encrypts `plaintext` under `nonce`, binding `aad`.
    ///
    /// Returns `ciphertext || tag` (the 16-byte GCM tag is appended). This is a
    /// crate-internal primitive; public value encryption goes through
    /// `VaultContext::encrypt_value`, which generates and stores a fresh nonce.
    ///
    /// # Errors
    /// [`CALYX_ENCRYPTION_FAILED`] on any cipher error.
    pub(crate) fn encrypt(
        &self,
        nonce: &[u8; NONCE_LEN],
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(self.aes_gcm_key());
        cipher
            .encrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| encryption_failed("AES-256-GCM encryption failed"))
    }

    /// AES-256-GCM decrypts `ciphertext` (`= ct || tag`) under `nonce`, verifying
    /// `aad` and the authentication tag.
    ///
    /// Fails closed (A16): a flipped tag byte, wrong AAD, wrong nonce, or input
    /// shorter than the 16-byte tag all return an error — never garbage bytes.
    ///
    /// # Errors
    /// [`CALYX_DECRYPTION_FAILED`] on tag/AAD mismatch or truncated input.
    pub(crate) fn decrypt(
        &self,
        nonce: &[u8; NONCE_LEN],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>> {
        if ciphertext.len() < TAG_LEN {
            return Err(decryption_failed(format!(
                "ciphertext is {} bytes, shorter than the {TAG_LEN}-byte GCM tag",
                ciphertext.len()
            )));
        }
        let cipher = Aes256Gcm::new(self.aes_gcm_key());
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| decryption_failed("AES-256-GCM tag verification failed"))
    }

    /// AES-256-GCM decrypts an owned `ciphertext || tag` buffer in place.
    ///
    /// This is the allocation-stable counterpart to [`Self::decrypt`]. Durable
    /// SST reads already own their ciphertext allocation, so replacing its
    /// contents with plaintext avoids allocating and then abandoning a second
    /// value-sized buffer for every row in a long sequential scan.
    pub(crate) fn decrypt_in_place(
        &self,
        nonce: &[u8; NONCE_LEN],
        ciphertext_and_tag: &mut Vec<u8>,
        aad: &[u8],
    ) -> Result<()> {
        if ciphertext_and_tag.len() < TAG_LEN {
            return Err(decryption_failed(format!(
                "ciphertext is {} bytes, shorter than the {TAG_LEN}-byte GCM tag",
                ciphertext_and_tag.len()
            )));
        }
        let cipher = Aes256Gcm::new(self.aes_gcm_key());
        cipher
            .decrypt_in_place(Nonce::from_slice(nonce), aad, ciphertext_and_tag)
            .map_err(|_| decryption_failed("AES-256-GCM tag verification failed"))
    }
}

impl std::fmt::Debug for VaultKey {
    /// Never prints the secret bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultKey")
            .field("inner", &"<redacted>")
            .finish()
    }
}

fn vault_key_missing(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_VAULT_KEY_MISSING,
        message: message.into(),
        remediation: "provision non-empty master key material for the vault",
    }
}

fn encryption_failed(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ENCRYPTION_FAILED,
        message: message.into(),
        remediation: "verify the nonce is 12 bytes and plaintext is within size limits",
    }
}

fn decryption_failed(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_DECRYPTION_FAILED,
        message: message.into(),
        remediation: "verify the key, nonce, AAD, and that the ciphertext is intact",
    }
}
