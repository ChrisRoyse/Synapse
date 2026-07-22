use crate::MAX_DURABLE_SLOT_ID;
use crate::cf::{ColumnFamily, SlotFamilyKind};
use calyx_core::{CalyxError, Result, SlotId};

/// Highest slot id encodable by the legacy durable one-byte WAL CF tag.
///
/// Tags `16..=63` are quantized slots and `64..=111` are raw slot sidecars.
/// Static CFs occupy higher tags, so out-of-range slots must fail before
/// durable write; otherwise slot ids alias unrelated static CF tags.
pub(crate) fn cf_tag(cf: ColumnFamily) -> Result<u8> {
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
        ColumnFamily::Slot { slot, kind } => {
            let slot_id = slot.get();
            if slot_id > MAX_DURABLE_SLOT_ID {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "slot id {slot_id} exceeds durable CF tag maximum {MAX_DURABLE_SLOT_ID}; \
                     allocate panel slots within 0..={MAX_DURABLE_SLOT_ID} or extend the WAL CF tag codec before writing"
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
        16..=63 => ColumnFamily::slot(SlotId::new((tag - 16) as u16)),
        64..=111 => ColumnFamily::slot_raw(SlotId::new((tag - 64) as u16)),
        _ => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "unknown CF tag {tag}"
            )));
        }
    })
}
