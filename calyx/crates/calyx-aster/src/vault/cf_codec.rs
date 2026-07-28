use crate::MAX_COMPACT_DURABLE_SLOT_ID;
use crate::cf::{ColumnFamily, SlotFamilyKind};
use calyx_core::{CalyxError, Result, SlotId};

/// Escape tag introducing an extended slot CF: `132 ‖ slot_id_be(2) ‖ kind`.
///
/// The compact one-byte encoding below has room for slots `0..=47` only (tags
/// `16..=63` quantized, `64..=111` raw), with static CFs occupying the rest.
/// That ceiling forced panels to reuse slot ids, which is what put seven panels
/// into one physical column family (issue #1776).
///
/// Rather than burn the last free single-byte tags on a slightly larger fixed
/// ceiling, extended slots use the same escape shape the keyspace tag has
/// always used (`ColumnFamily::keyspace_tag`), reaching the full `u16` slot
/// space. Every tag `0..=131` keeps its exact previous meaning, so WAL records
/// written before this change decode bit-identically, and slots `0..=47` still
/// encode to one byte — the escape is only reached by ids that previously could
/// not be written at all.
pub(crate) const SLOT_ESCAPE_TAG: u8 = 132;

const SLOT_KIND_QUANTIZED: u8 = 0;
const SLOT_KIND_RAW: u8 = 1;

/// Appends the durable WAL CF tag for `cf` to `out`.
///
/// Static CFs and slots `0..=47` emit exactly one byte, unchanged from every
/// previous build. Higher slots emit the four-byte escape form.
pub(crate) fn write_cf_tag(cf: ColumnFamily, out: &mut Vec<u8>) -> Result<()> {
    if let ColumnFamily::Slot { slot, kind } = cf
        && slot.get() > MAX_COMPACT_DURABLE_SLOT_ID
    {
        out.push(SLOT_ESCAPE_TAG);
        out.extend_from_slice(&slot.get().to_be_bytes());
        out.push(match kind {
            SlotFamilyKind::Quantized => SLOT_KIND_QUANTIZED,
            SlotFamilyKind::Raw => SLOT_KIND_RAW,
        });
        return Ok(());
    }
    out.push(compact_cf_tag(cf)?);
    Ok(())
}

/// Resolves the body of an extended slot CF tag, given the two id bytes and the
/// kind byte that follow [`SLOT_ESCAPE_TAG`].
pub(crate) fn read_escaped_slot(slot_id: u16, kind: u8) -> Result<ColumnFamily> {
    let kind = match kind {
        SLOT_KIND_QUANTIZED => SlotFamilyKind::Quantized,
        SLOT_KIND_RAW => SlotFamilyKind::Raw,
        other => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "extended slot CF tag has unknown slot kind {other}"
            )));
        }
    };
    if slot_id <= MAX_COMPACT_DURABLE_SLOT_ID {
        // A compactly-encodable slot must never appear in escape form, or one
        // CF would have two distinct durable encodings and byte-identical
        // records would stop being byte-identical.
        return Err(CalyxError::aster_corrupt_shard(format!(
            "extended slot CF tag carries slot {slot_id}, which must use the compact encoding \
             (0..={MAX_COMPACT_DURABLE_SLOT_ID})"
        )));
    }
    let slot = SlotId::new(slot_id);
    Ok(match kind {
        SlotFamilyKind::Quantized => ColumnFamily::slot(slot),
        SlotFamilyKind::Raw => ColumnFamily::slot_raw(slot),
    })
}

/// The compact single-byte tag. Slots above [`MAX_COMPACT_DURABLE_SLOT_ID`] have
/// no compact form and must go through [`write_cf_tag`].
fn compact_cf_tag(cf: ColumnFamily) -> Result<u8> {
    match cf {
        ColumnFamily::Base => Ok(0),
        ColumnFamily::Collections => Ok(117),
        ColumnFamily::Relational => Ok(118),
        ColumnFamily::Document => Ok(119),
        ColumnFamily::Kv => Ok(120),
        ColumnFamily::TimeSeries => Ok(121),
        ColumnFamily::Blob => Ok(122),
        ColumnFamily::Anchors => Ok(1),
        ColumnFamily::Ledger => Ok(2),
        ColumnFamily::XTerm => Ok(3),
        ColumnFamily::Scalars => Ok(4),
        ColumnFamily::Online => Ok(5),
        ColumnFamily::Assay => Ok(6),
        ColumnFamily::Recurrence => Ok(7),
        ColumnFamily::Reactive => Ok(126),
        ColumnFamily::TemporalXTerm => Ok(8),
        ColumnFamily::AnnealRollback => Ok(9),
        ColumnFamily::AnnealHealth => Ok(10),
        ColumnFamily::AnnealChecksums => Ok(11),
        ColumnFamily::Graph => Ok(12),
        ColumnFamily::AnnealMistakes => Ok(13),
        ColumnFamily::AnnealReplay => Ok(14),
        ColumnFamily::AnnealHeads => Ok(15),
        ColumnFamily::AnnealBandit => Ok(112),
        ColumnFamily::AnnealSoak => Ok(113),
        ColumnFamily::AnnealReport => Ok(114),
        ColumnFamily::AnnealGrowth => Ok(115),
        ColumnFamily::TimeIndex => Ok(116),
        ColumnFamily::IndexBtree => Ok(123),
        ColumnFamily::IndexInverted => Ok(124),
        ColumnFamily::AnnealOperators => Ok(125),
        ColumnFamily::Kernel => Ok(127),
        ColumnFamily::Guard => Ok(128),
        ColumnFamily::Leapable => Ok(129),
        ColumnFamily::Registry => Ok(130),
        ColumnFamily::RawCommitment => Ok(131),
        ColumnFamily::Slot { slot, kind } => {
            let slot_id = slot.get();
            if slot_id > MAX_COMPACT_DURABLE_SLOT_ID {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "slot id {slot_id} has no compact durable CF tag \
                     (0..={MAX_COMPACT_DURABLE_SLOT_ID}); it must be written through the extended \
                     escape form by write_cf_tag"
                )));
            }
            let base = match kind {
                SlotFamilyKind::Quantized => 16,
                SlotFamilyKind::Raw => 64,
            };
            Ok(base + slot_id as u8)
        }
    }
}

pub(crate) fn decode_cf(tag: u8) -> Result<ColumnFamily> {
    Ok(match tag {
        0 => ColumnFamily::Base,
        117 => ColumnFamily::Collections,
        118 => ColumnFamily::Relational,
        119 => ColumnFamily::Document,
        120 => ColumnFamily::Kv,
        121 => ColumnFamily::TimeSeries,
        122 => ColumnFamily::Blob,
        1 => ColumnFamily::Anchors,
        2 => ColumnFamily::Ledger,
        3 => ColumnFamily::XTerm,
        4 => ColumnFamily::Scalars,
        5 => ColumnFamily::Online,
        6 => ColumnFamily::Assay,
        7 => ColumnFamily::Recurrence,
        126 => ColumnFamily::Reactive,
        8 => ColumnFamily::TemporalXTerm,
        9 => ColumnFamily::AnnealRollback,
        10 => ColumnFamily::AnnealHealth,
        11 => ColumnFamily::AnnealChecksums,
        12 => ColumnFamily::Graph,
        13 => ColumnFamily::AnnealMistakes,
        14 => ColumnFamily::AnnealReplay,
        15 => ColumnFamily::AnnealHeads,
        112 => ColumnFamily::AnnealBandit,
        113 => ColumnFamily::AnnealSoak,
        114 => ColumnFamily::AnnealReport,
        115 => ColumnFamily::AnnealGrowth,
        116 => ColumnFamily::TimeIndex,
        123 => ColumnFamily::IndexBtree,
        124 => ColumnFamily::IndexInverted,
        125 => ColumnFamily::AnnealOperators,
        127 => ColumnFamily::Kernel,
        128 => ColumnFamily::Guard,
        129 => ColumnFamily::Leapable,
        130 => ColumnFamily::Registry,
        131 => ColumnFamily::RawCommitment,
        16..=63 => ColumnFamily::slot(SlotId::new((tag - 16) as u16)),
        64..=111 => ColumnFamily::slot_raw(SlotId::new((tag - 64) as u16)),
        SLOT_ESCAPE_TAG => {
            // The escape carries its slot id in the following three bytes, so a
            // caller holding only this byte cannot resolve it. Say so exactly
            // rather than guess: the single-byte readers (WAL indexed Base
            // lookup) only ever expect Base and must fail closed here.
            return Err(CalyxError::aster_corrupt_shard(
                "CF tag 132 introduces an extended slot CF and cannot be decoded from one byte; \
                 read it with read_cf_tag",
            ));
        }
        _ => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "unknown CF tag {tag}"
            )));
        }
    })
}
