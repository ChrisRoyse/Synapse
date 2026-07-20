use calyx_core::{CxId, SlotVector};

use crate::error::{CALYX_SEXTANT_VECTOR_SHAPE, sextant_error};

pub fn dense(vector: &SlotVector) -> calyx_core::Result<&[f32]> {
    vector.as_dense().ok_or_else(|| {
        sextant_error(
            CALYX_SEXTANT_VECTOR_SHAPE,
            "dense index received non-dense vector",
        )
    })
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0;
    let mut an = 0.0;
    let mut bn = 0.0;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        an += x * x;
        bn += y * y;
    }
    if an == 0.0 || bn == 0.0 {
        0.0
    } else {
        dot / (an.sqrt() * bn.sqrt())
    }
}

pub fn top_k(mut scored: Vec<(CxId, f32)>, k: usize) -> Vec<(CxId, f32)> {
    scored.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
    });
    scored.truncate(k);
    scored
}

pub fn event_time_secs_from_ts(ts: u64) -> Option<i64> {
    let secs = if ts >= 10_000_000_000 { ts / 1_000 } else { ts };
    i64::try_from(secs).ok()
}

pub fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
