//! Lightweight constellation measurement helpers.

use calyx_core::{AbsentReason, SlotVector};

/// An absent slot vector for `reason`.
pub fn absent(reason: AbsentReason) -> SlotVector {
    SlotVector::Absent { reason }
}

/// Blake3 of the raw input bytes, used as the constellation `InputRef.hash`.
pub fn input_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}
