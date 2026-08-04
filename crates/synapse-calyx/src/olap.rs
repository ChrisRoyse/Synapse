use calyx_aster::olap::OlapScanPlan;
pub use calyx_aster::olap::OlapScanResult;
use calyx_core::{PanelSlotId, SlotId};

use crate::{SynapseCalyxError, SynapseCalyxVault};

impl SynapseCalyxVault {
    pub fn olap_aggregate_slot(
        &self,
        panel_version: u32,
        slot_id: u32,
        value_column: usize,
        group_by_column: Option<usize>,
        max_rows: usize,
        max_groups: usize,
    ) -> Result<OlapScanResult, SynapseCalyxError> {
        let snapshot = self.vault.latest_seq();
        let slot_id = u16::try_from(slot_id).map_err(|_| {
            SynapseCalyxError::from_calyx(
                "validate native OLAP slot id",
                &calyx_core::CalyxError {
                    code: "CALYX_OLAP_INVALID_SLOT",
                    message: format!("slot id {slot_id} exceeds the 16-bit panel slot range"),
                    remediation: "pass a declared panel slot id in 0..=65535",
                },
            )
        })?;
        let panel_slot = PanelSlotId::new(panel_version, SlotId::new(slot_id));
        let output_dir = self
            .config
            .vault_dir
            .join("derived")
            .join("olap")
            .join(format!("panel-{panel_version}-slot-{slot_id}"));
        let mut plan = OlapScanPlan::new(value_column).with_limits(max_rows, max_groups);
        if let Some(column) = group_by_column {
            plan = plan.with_group_by(column);
        }
        self.vault
            .olap_scan_aggregate_slot_at(snapshot, panel_slot, output_dir, plan)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "materialize and scan native OLAP slot column",
                    &error,
                )
            })
    }
}
