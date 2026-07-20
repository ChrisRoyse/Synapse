use super::{AsterVault, encode};
use crate::cf::{ColumnFamily, base_key, slot_key};
use calyx_core::{CalyxError, Clock, CxId, PanelSlotId, Result, Seq, SlotId, SlotVector};

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn put_slot_vector(
        &self,
        cx_id: CxId,
        panel_slot: PanelSlotId,
        vector: &SlotVector,
    ) -> Result<Seq> {
        self.ensure_base_declares_slot(cx_id, panel_slot)?;
        vector.validate_schema()?;
        let row = encode::WriteRow {
            cf: ColumnFamily::slot(panel_slot.slot_id()),
            key: slot_key(cx_id),
            value: encode::encode_slot_vector(vector)?,
        };
        self.commit_rows(&[row])
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

    fn ensure_base_declares_slot(&self, cx_id: CxId, panel_slot: PanelSlotId) -> Result<()> {
        let bytes = self
            .read_cf_at(self.latest_seq(), ColumnFamily::Base, &base_key(cx_id))?
            .ok_or_else(|| {
                CalyxError::stale_derived(format!(
                    "constellation {cx_id} missing for qualified slot write {panel_slot}"
                ))
            })?;
        let constellation = encode::decode_constellation_base(&bytes)?;
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
        Ok(())
    }
}
