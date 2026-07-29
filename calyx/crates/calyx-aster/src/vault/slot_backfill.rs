use super::base_rewrite::BaseRowRewrite;
use super::{AsterVault, encode};
use crate::cf::{ColumnFamily, base_key, slot_key};
use calyx_core::{CalyxError, Clock, CxId, PanelSlotId, Result, Seq, SlotId, SlotVector};

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Writes a slot vector and the Base row's integrity record for it in one
    /// atomic batch.
    ///
    /// The Base slot hash **is** the integrity record for the vector, so a
    /// write that leaves it pinned to the `Absent` placeholder the panel
    /// emitted at creation is not a completed write: every later verifier
    /// checks the vector against a hash of a placeholder and passes regardless
    /// of what the slot CF holds (issue #1888).
    pub fn put_slot_vector(
        &self,
        cx_id: CxId,
        panel_slot: PanelSlotId,
        vector: &SlotVector,
    ) -> Result<Seq> {
        vector.validate_schema()?;
        let encoded = encode::encode_slot_vector(vector)?;
        self.with_durable_commit_lock(|| {
            let mut rewrite = self.base_rewrite_declaring_slot(cx_id, panel_slot)?;
            rewrite.set_slot_hash(panel_slot.slot_id(), &encoded)?;
            let rows = [
                encode::WriteRow {
                    cf: ColumnFamily::slot(panel_slot.slot_id()),
                    key: slot_key(cx_id),
                    value: encoded.clone(),
                },
                encode::WriteRow {
                    cf: ColumnFamily::Base,
                    key: base_key(cx_id),
                    value: rewrite.encode()?,
                },
            ];
            self.commit_rows_locked(&rows)
        })
    }

    pub fn read_slot_vector_at(
        &self,
        snapshot: Seq,
        cx_id: CxId,
        slot_id: SlotId,
    ) -> Result<Option<SlotVector>> {
        self.read_cf_at(snapshot, ColumnFamily::slot(slot_id), &slot_key(cx_id))?
            .map(|bytes| encode::decode_slot_vector(&bytes))
            .transpose()
    }

    fn base_rewrite_declaring_slot(
        &self,
        cx_id: CxId,
        panel_slot: PanelSlotId,
    ) -> Result<BaseRowRewrite> {
        let bytes = self
            .read_cf_at(self.latest_seq(), ColumnFamily::Base, &base_key(cx_id))?
            .ok_or_else(|| {
                CalyxError::stale_derived(format!(
                    "constellation {cx_id} missing for qualified slot write {panel_slot}"
                ))
            })?;
        let rewrite = BaseRowRewrite::decode(&bytes)?;
        let constellation = rewrite.constellation();
        if constellation.cx_id != cx_id {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "Base row key {cx_id} contains constellation {} during qualified slot write",
                constellation.cx_id
            )));
        }
        if constellation.panel_version != panel_slot.panel_version() {
            return Err(CalyxError::stale_derived(format!(
                "qualified slot write {panel_slot} cannot attach to constellation {cx_id} from panel {}",
                constellation.panel_version
            )));
        }
        if !constellation.slots.contains_key(&panel_slot.slot_id()) {
            return Err(CalyxError::stale_derived(format!(
                "qualified slot write {panel_slot} is absent from constellation {cx_id} Base membership; re-measure the authoritative input into a new panel generation and CxId"
            )));
        }
        Ok(rewrite)
    }
}
