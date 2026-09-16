//! At-rest secret protection via Windows DPAPI (CurrentUser scope).
//!
//! Cloud-model API keys are encrypted with `CryptProtectData` before they ever
//! touch RocksDB and decrypted with `CryptUnprotectData` only in-process, only
//! when a spawn or probe needs to authenticate. CurrentUser scope binds the
//! ciphertext to the Windows account the daemon runs as: another local user, or
//! the same database directory copied to a different machine, cannot decrypt
//! it. There is no plaintext at rest and no separate key-management surface.
//!
//! A fixed secondary entropy value namespaces Synapse secrets so a blob minted
//! for one purpose cannot be transplanted into another `CryptUnprotectData`
//! caller on the same account.

use anyhow::{Context, Result, bail};

/// Secondary entropy mixed into every protect/unprotect call. Not itself a
/// secret — it namespaces Synapse's DPAPI blobs to this subsystem so the
/// ciphertext is meaningless to any other DPAPI consumer on the account.
const ENTROPY: &[u8] = b"synapse/local-model-api-key/v1";

/// Encrypts `plaintext` with DPAPI (CurrentUser). Returns opaque ciphertext
/// suitable for at-rest storage. The plaintext is never written anywhere by
/// this function.
#[cfg(windows)]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    // SAFETY: the blobs borrow `plaintext`/`ENTROPY` for the duration of the
    // call only; DPAPI does not retain the pointers. `out_blob` is populated
    // with a LocalAlloc'd buffer that we copy out of and then `LocalFree`.
    unsafe {
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(plaintext.len()).context("plaintext too large for DPAPI")?,
            pbData: plaintext.as_ptr().cast_mut(),
        };
        let entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(ENTROPY.len()).context("entropy too large")?,
            pbData: ENTROPY.as_ptr().cast_mut(),
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        let result = CryptProtectData(
            &in_blob,
            windows::core::PCWSTR::null(),
            Some(&entropy_blob),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        );
        match result {
            Ok(()) => {
                let bytes =
                    std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
                let _ = LocalFree(Some(HLOCAL(out_blob.pbData.cast())));
                Ok(bytes)
            }
            Err(error) => bail!("CryptProtectData failed: {error}"),
        }
    }
}

/// Decrypts DPAPI ciphertext produced by [`protect`] on this account. Fails if
/// the blob was produced by a different user, on a different machine, or with
/// different entropy (i.e. tampered or foreign data) — there is no silent
/// fallback to returning the raw bytes.
#[cfg(windows)]
pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};

    // SAFETY: see `protect`. `out_blob` owns a LocalAlloc'd buffer we copy and
    // free.
    unsafe {
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(ciphertext.len()).context("ciphertext too large for DPAPI")?,
            pbData: ciphertext.as_ptr().cast_mut(),
        };
        let entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(ENTROPY.len()).context("entropy too large")?,
            pbData: ENTROPY.as_ptr().cast_mut(),
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        let result = CryptUnprotectData(
            &in_blob,
            None,
            Some(&entropy_blob),
            None,
            None,
            0,
            &mut out_blob,
        );
        match result {
            Ok(()) => {
                let bytes =
                    std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
                let _ = LocalFree(Some(HLOCAL(out_blob.pbData.cast())));
                Ok(bytes)
            }
            Err(error) => bail!("CryptUnprotectData failed: {error}"),
        }
    }
}

/// Non-Windows builds have no DPAPI. Secret protection is a Windows-only
/// capability; refuse loudly rather than persist a plaintext key.
#[cfg(not(windows))]
pub fn protect(_plaintext: &[u8]) -> Result<Vec<u8>> {
    bail!("secure secret storage requires Windows DPAPI; this platform is unsupported")
}

#[cfg(not(windows))]
pub fn unprotect(_ciphertext: &[u8]) -> Result<Vec<u8>> {
    bail!("secure secret storage requires Windows DPAPI; this platform is unsupported")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn protect_then_unprotect_roundtrips_secret() {
        let secret = b"sk-deepseek-live-EXAMPLE-0123456789abcdef";
        let ciphertext = protect(secret).expect("protect");
        // Ciphertext must not contain the plaintext anywhere.
        assert!(
            ciphertext
                .windows(secret.len())
                .all(|window| window != secret),
            "DPAPI ciphertext must not contain the plaintext"
        );
        let recovered = unprotect(&ciphertext).expect("unprotect");
        assert_eq!(recovered, secret);
    }

    #[test]
    fn unprotect_rejects_foreign_bytes() {
        // Random bytes were never DPAPI-protected for this account/entropy.
        let garbage = vec![0xA5_u8; 64];
        assert!(
            unprotect(&garbage).is_err(),
            "unprotecting non-DPAPI bytes must fail, not return garbage"
        );
    }

    #[test]
    fn ciphertext_differs_each_call_but_both_decrypt() {
        // DPAPI mixes randomness, so two ciphertexts of the same plaintext
        // differ yet both decrypt — proves we are not just echoing input.
        let secret = b"identical-plaintext";
        let a = protect(secret).expect("protect a");
        let b = protect(secret).expect("protect b");
        assert_ne!(a, b, "DPAPI output should be salted per call");
        assert_eq!(unprotect(&a).expect("a"), secret);
        assert_eq!(unprotect(&b).expect("b"), secret);
    }
}
