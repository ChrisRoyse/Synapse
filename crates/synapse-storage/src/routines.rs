//! `CF_ROUTINES` key codec (#848).
//!
//! Keys are the routine's stable deterministic id as UTF-8 bytes
//! (`rt1-` + 16 lowercase hex chars, 20 bytes total). Routines are a small,
//! wholesale-replaced derived CF: there is no time axis to iterate, so the
//! id IS the natural primary key, and lifecycle state (#849) can anchor on
//! the same id without an indirection table.
//!
//! Every producer and consumer must go through this module so a malformed
//! key is a structured error, never a silent skip.

use crate::{StorageError, StorageResult, cf};

/// Encoded key length: `rt1-` prefix plus 16 hex chars.
pub const ROUTINE_KEY_LEN: usize = 20;
/// Required id prefix.
pub const ROUTINE_ID_PREFIX: &str = "rt1-";

/// Encodes a `CF_ROUTINES` row key from a routine id.
///
/// # Errors
///
/// Returns [`StorageError::WriteFailed`] when the id is not a well-formed
/// `rt1-` + 16 lowercase hex id.
pub fn routine_key(routine_id: &str) -> StorageResult<Vec<u8>> {
    validate_routine_id(routine_id).map_err(|detail| StorageError::WriteFailed {
        cf_name: cf::CF_ROUTINES.to_owned(),
        detail,
    })?;
    Ok(routine_id.as_bytes().to_vec())
}

/// Decodes a `CF_ROUTINES` row key into the routine id.
///
/// # Errors
///
/// Returns [`StorageError::ReadFailed`] when the key is not a well-formed
/// routine id.
pub fn decode_routine_key(key: &[u8]) -> StorageResult<String> {
    decode_id_key(key, cf::CF_ROUTINES)
}

/// Encodes a `CF_ROUTINE_STATE` row key from a routine id (#849). State rows
/// share the routine id keyspace so lifecycle anchors on the stable id.
///
/// # Errors
///
/// Returns [`StorageError::WriteFailed`] when the id is not a well-formed
/// `rt1-` + 16 lowercase hex id.
pub fn routine_state_key(routine_id: &str) -> StorageResult<Vec<u8>> {
    validate_routine_id(routine_id).map_err(|detail| StorageError::WriteFailed {
        cf_name: cf::CF_ROUTINE_STATE.to_owned(),
        detail,
    })?;
    Ok(routine_id.as_bytes().to_vec())
}

/// Decodes a `CF_ROUTINE_STATE` row key into the routine id.
///
/// # Errors
///
/// Returns [`StorageError::ReadFailed`] when the key is not a well-formed
/// routine id.
pub fn decode_routine_state_key(key: &[u8]) -> StorageResult<String> {
    decode_id_key(key, cf::CF_ROUTINE_STATE)
}

fn decode_id_key(key: &[u8], cf_name: &str) -> StorageResult<String> {
    let text = std::str::from_utf8(key).map_err(|_e| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail: "ROUTINE_KEY_INVALID: key is not UTF-8".to_owned(),
    })?;
    validate_routine_id(text).map_err(|detail| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail,
    })?;
    Ok(text.to_owned())
}

fn validate_routine_id(routine_id: &str) -> Result<(), String> {
    if routine_id.len() != ROUTINE_KEY_LEN
        || !routine_id.starts_with(ROUTINE_ID_PREFIX)
        || !routine_id[ROUTINE_ID_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "ROUTINE_KEY_INVALID: expected {ROUTINE_ID_PREFIX:?} + 16 lowercase hex chars, \
             got {routine_id:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrips_for_valid_ids() {
        let id = "rt1-0123456789abcdef";
        let key = routine_key(id).expect("valid id encodes");
        assert_eq!(key.len(), ROUTINE_KEY_LEN);
        let decoded = decode_routine_key(&key).expect("roundtrip");
        assert_eq!(decoded, id);
    }

    #[test]
    fn state_key_roundtrips_and_errors_name_the_state_cf() {
        let id = "rt1-0123456789abcdef";
        let key = routine_state_key(id).expect("valid id encodes");
        assert_eq!(key, routine_key(id).expect("same keyspace"));
        let decoded = decode_routine_state_key(&key).expect("roundtrip");
        assert_eq!(decoded, id);
        let error = routine_state_key("rt1-NOPE").expect_err("invalid id");
        assert!(
            error.to_string().contains(cf::CF_ROUTINE_STATE),
            "state codec errors must name CF_ROUTINE_STATE: {error}"
        );
    }

    #[test]
    fn malformed_ids_are_structured_errors() {
        for bad in [
            "",
            "rt1-",
            "rt1-0123456789ABCDEF",  // uppercase hex
            "rt2-0123456789abcdef",  // wrong prefix
            "rt1-0123456789abcde",   // too short
            "rt1-0123456789abcdef0", // too long
            "rt1-0123456789abcdeg",  // non-hex
        ] {
            let error = routine_key(bad).expect_err(bad);
            assert!(
                error.to_string().contains("ROUTINE_KEY_INVALID"),
                "encode {bad:?}: {error}"
            );
        }
        let error = decode_routine_key(&[0xFF, 0xFE]).expect_err("non-utf8");
        assert!(
            error.to_string().contains("ROUTINE_KEY_INVALID"),
            "decode: {error}"
        );
    }
}
