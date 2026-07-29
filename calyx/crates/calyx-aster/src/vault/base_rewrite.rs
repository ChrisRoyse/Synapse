//! The only sound way to mutate an already-stored Base row.
//!
//! A Base row stores a 32-byte hash per slot plus an identity hash computed
//! over that slot map. `decode_constellation_base` cannot recover the slot
//! vectors those hashes cover — it returns `Absent` placeholders — so the
//! shape
//!
//! ```text
//! decode_constellation_base(bytes) -> mutate -> encode_constellation_base(cx)
//! ```
//!
//! silently replaces every real slot hash with the hash of a two-byte
//! placeholder, and recomputes the identity hash over those fabrications. The
//! row still decodes, still has the same length, and no decode-and-compare can
//! see it: issue #1888, found only because the #1878 migration compared bytes.
//!
//! `BaseRowRewrite` carries the stored slot hashes through the mutation and
//! fails closed if the caller changed slot membership, which is the one thing a
//! rewrite may never do — a constellation's `CxId` is content-addressed over
//! its inputs, so a different slot set is a different constellation and must be
//! written under a new id, not patched over an existing row.

use calyx_core::{CalyxError, Constellation, Result, SlotId};

use super::encode::{self, SlotHashEntry};

/// A Base row decoded together with its stored slot-hash map, ready to be
/// mutated and re-encoded without fabricating integrity records.
#[derive(Clone, Debug)]
pub struct BaseRowRewrite {
    constellation: Constellation,
    slot_hashes: Vec<SlotHashEntry>,
}

impl BaseRowRewrite {
    /// Decodes a stored Base row for in-place mutation.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (constellation, slot_hashes) =
            encode::decode_constellation_base_with_slot_hashes(bytes)?;
        Ok(Self {
            constellation,
            slot_hashes,
        })
    }

    pub const fn constellation(&self) -> &Constellation {
        &self.constellation
    }

    /// Mutable access for the fields a rewrite may legitimately change:
    /// anchors, scalars, metadata, flags and provenance.
    ///
    /// Slot membership is checked again at [`Self::encode`], so clearing or
    /// adding slots here is refused rather than silently written.
    pub const fn constellation_mut(&mut self) -> &mut Constellation {
        &mut self.constellation
    }

    /// Records the hash of the bytes now stored in `cf/slot_<id>`.
    ///
    /// This is what makes a lazily backfilled slot's Base row describe the
    /// vector actually present instead of the placeholder emitted at creation.
    pub fn set_slot_hash(&mut self, slot: SlotId, encoded_vector: &[u8]) -> Result<()> {
        let entry = self
            .slot_hashes
            .iter_mut()
            .find(|(candidate, _)| *candidate == slot)
            .ok_or_else(|| {
                CalyxError::stale_derived(format!(
                    "constellation {} Base row does not declare slot {slot}; a slot hash can only be recorded for a declared slot",
                    self.constellation.cx_id
                ))
            })?;
        entry.1 = encode::hash_slot_bytes(encoded_vector);
        Ok(())
    }

    /// Returns the stored hash the Base row records for `slot`.
    pub fn slot_hash(&self, slot: SlotId) -> Option<[u8; 32]> {
        self.slot_hashes
            .iter()
            .find(|(candidate, _)| *candidate == slot)
            .map(|(_, hash)| *hash)
    }

    pub fn slot_hashes(&self) -> &[SlotHashEntry] {
        &self.slot_hashes
    }

    /// Re-encodes the row, carrying the stored slot hashes and recomputing the
    /// identity hash over them.
    ///
    /// Fails closed when slot membership changed under the mutation, naming the
    /// constellation and both slot sets.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if !self
            .constellation
            .slots
            .keys()
            .copied()
            .eq(self.slot_hashes.iter().map(|(slot, _)| *slot))
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "Base row rewrite for constellation {} changed slot membership: stored slots [{}] but the rewrite carries [{}]; a different slot set is a different constellation and must be written under a new CxId, not patched over this row",
                self.constellation.cx_id,
                join_slots(self.slot_hashes.iter().map(|(slot, _)| *slot)),
                join_slots(self.constellation.slots.keys().copied()),
            )));
        }
        encode::encode_constellation_base_with_slot_hashes(&self.constellation, &self.slot_hashes)
    }
}

fn join_slots(slots: impl Iterator<Item = SlotId>) -> String {
    slots
        .map(|slot| slot.get().to_string())
        .collect::<Vec<_>>()
        .join(",")
}
