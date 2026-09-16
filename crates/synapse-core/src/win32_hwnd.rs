//! Canonical Win32 `HWND` wire conversion.
//!
//! USER handles are interoperable 32-bit values even in 64-bit Windows
//! processes. Windows APIs may expose bit-31-set values either zero-extended or
//! sign-extended in pointer-sized storage. Synapse represents an HWND on its
//! JSON/storage boundaries as the unsigned low 32 bits in an `i64`; conversion
//! back to a native `HWND` sign-extends those same bits.
//!
//! ABI basis: <https://learn.microsoft.com/en-us/windows/win32/winprog64/interprocess-communication>

/// Highest canonical JSON/storage value for a Win32 USER handle.
pub const MAX_CANONICAL_HWND: i64 = u32::MAX as i64;

/// Converts a pointer-sized native HWND value to Synapse's unsigned low-32 wire
/// representation. This must only be used for Win32 USER handles, never kernel
/// `HANDLE` values.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Win32 USER handles are defined to interoperate through their low 32 bits"
)]
pub const fn hwnd_to_wire(native: isize) -> i64 {
    (native as u32) as i64
}

/// Compares two native HWND representations by their canonical USER-handle
/// bits.
///
/// Win64 APIs may return the same high-bit handle zero-extended or
/// sign-extended, so pointer-sized integer equality is not sufficient.
#[must_use]
pub const fn native_hwnds_equal(left: isize, right: isize) -> bool {
    hwnd_to_wire(left) == hwnd_to_wire(right)
}

/// Converts a canonical Synapse HWND wire value to the sign-extended native
/// representation expected at Win64 interoperability boundaries.
///
/// Returns `None` for zero, negative values, and values above `u32::MAX` so a
/// caller cannot accidentally turn malformed external data into an aliased
/// native handle by truncation.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the checked canonical range proves the wire value fits exactly in u32"
)]
pub const fn hwnd_from_wire(wire: i64) -> Option<isize> {
    if wire >= 1 && wire <= MAX_CANONICAL_HWND {
        let low_bits = wire as u32;
        Some(i32::from_ne_bytes(low_bits.to_ne_bytes()) as isize)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_bit_user_handles_round_trip_through_unsigned_low32_wire_values() {
        let cases = [
            (1_i64, 1_isize),
            (0x7fff_ffff_i64, 0x7fff_ffff_isize),
            (0x8000_0000_i64, i32::MIN as isize),
            (0xffff_ffff_i64, -1_isize),
        ];
        for (wire, native) in cases {
            assert_eq!(hwnd_from_wire(wire), Some(native));
            assert_eq!(hwnd_to_wire(native), wire);
        }
    }

    #[test]
    fn malformed_wire_values_never_truncate_into_native_hwnds() {
        for invalid in [i64::MIN, -1, 0, MAX_CANONICAL_HWND + 1, i64::MAX] {
            assert_eq!(hwnd_from_wire(invalid), None);
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn zero_and_sign_extended_native_forms_compare_as_the_same_user_handle() {
        assert!(native_hwnds_equal(0x8000_0000_isize, i32::MIN as isize));
        assert!(native_hwnds_equal(0xffff_ffff_isize, -1_isize));
        assert!(!native_hwnds_equal(0xffff_fffe_isize, -1_isize));
    }
}
