use calyx_aster::collection::{
    CALYX_COLLECTION_NOT_FOUND, Collection, CollectionMode, DedupPolicy, RetentionPolicy,
    TemporalPolicy, TenantId, TxnPolicy, create_collection, get_collection,
};
use calyx_aster::layers::{RollupWindow, TimeSeriesLayer};
use serde::{Deserialize, Serialize};

use crate::{SynapseCalyxError, SynapseCalyxVault};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxRollupWindow {
    Minute,
    Hour,
    Day,
}

impl From<SynapseCalyxRollupWindow> for RollupWindow {
    fn from(value: SynapseCalyxRollupWindow) -> Self {
        match value {
            SynapseCalyxRollupWindow::Minute => Self::OneMinute,
            SynapseCalyxRollupWindow::Hour => Self::OneHour,
            SynapseCalyxRollupWindow::Day => Self::OneDay,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRollupValue {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl SynapseCalyxVault {
    /// Ensures an immutable native `TimeSeries` collection exists with the exact descriptor.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the descriptor conflicts or create and
    /// independent readback cannot establish the required collection.
    pub fn ensure_timeseries_collection(&self, name: &str) -> Result<(), SynapseCalyxError> {
        let expected = timeseries_collection(name);
        match get_collection(&self.vault, name) {
            Ok(stored) if stored == expected => Ok(()),
            Ok(stored) => Err(SynapseCalyxError::from_calyx(
                "verify native TimeSeries collection descriptor",
                &calyx_core::CalyxError {
                    code: "CALYX_TIMESERIES_COLLECTION_CONFLICT",
                    message: format!(
                        "stored collection descriptor for {name:?} differs from the required native TimeSeries descriptor: stored={stored:?} required={expected:?}"
                    ),
                    remediation: "use a new generation-qualified collection name; collection descriptors are immutable",
                },
            )),
            Err(error) if error.code == CALYX_COLLECTION_NOT_FOUND => {
                create_collection(&self.vault, expected).map_err(|error| {
                    SynapseCalyxError::from_calyx("create native TimeSeries collection", &error)
                })?;
                let stored = get_collection(&self.vault, name).map_err(|error| {
                    SynapseCalyxError::from_calyx(
                        "read native TimeSeries collection after create",
                        &error,
                    )
                })?;
                if stored != timeseries_collection(name) {
                    return Err(SynapseCalyxError::from_calyx(
                        "verify native TimeSeries collection after create",
                        &calyx_core::CalyxError {
                            code: "CALYX_TIMESERIES_COLLECTION_READBACK_MISMATCH",
                            message: format!(
                                "collection {name:?} differs immediately after durable create"
                            ),
                            remediation: "inspect the physical Collections CF row and refuse TimeSeries writes",
                        },
                    ));
                }
                Ok(())
            }
            Err(error) => Err(SynapseCalyxError::from_calyx(
                "read native TimeSeries collection",
                &error,
            )),
        }
    }

    /// Writes one point to a native `TimeSeries` collection.
    ///
    /// # Errors
    ///
    /// Returns a structured error when collection verification or the durable point write fails.
    pub fn timeseries_write(
        &self,
        collection_name: &str,
        series: u64,
        timestamp_ns: u64,
        value: f64,
    ) -> Result<u64, SynapseCalyxError> {
        self.ensure_timeseries_collection(collection_name)?;
        TimeSeriesLayer::new(&self.vault)
            .ts_write(
                &timeseries_collection(collection_name),
                series,
                timestamp_ns,
                value,
            )
            .map_err(|error| SynapseCalyxError::from_calyx("write native TimeSeries point", &error))
    }

    /// Reads one native `TimeSeries` rollup bucket.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the collection descriptor is invalid or the rollup read fails.
    pub fn timeseries_rollup(
        &self,
        collection_name: &str,
        series: u64,
        window: SynapseCalyxRollupWindow,
        timestamp_ns: u64,
    ) -> Result<Option<SynapseCalyxRollupValue>, SynapseCalyxError> {
        let collection = require_timeseries_collection(self, collection_name)?;
        TimeSeriesLayer::new(&self.vault)
            .ts_rollup(&collection, series, window.into(), timestamp_ns)
            .map(|value| {
                value.map(|value| SynapseCalyxRollupValue {
                    count: value.count,
                    sum: value.sum,
                    min: value.min,
                    max: value.max,
                })
            })
            .map_err(|error| SynapseCalyxError::from_calyx("read native TimeSeries rollup", &error))
    }

    /// Reads a bounded timestamp range from one native `TimeSeries` series.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the collection descriptor is invalid or the range read fails.
    pub fn timeseries_range(
        &self,
        collection_name: &str,
        series: u64,
        start_timestamp_ns: u64,
        end_timestamp_ns: u64,
    ) -> Result<Vec<(u64, f64)>, SynapseCalyxError> {
        let collection = require_timeseries_collection(self, collection_name)?;
        TimeSeriesLayer::new(&self.vault)
            .ts_range(&collection, series, start_timestamp_ns, end_timestamp_ns)
            .map_err(|error| SynapseCalyxError::from_calyx("read native TimeSeries range", &error))
    }
}

fn require_timeseries_collection(
    vault: &SynapseCalyxVault,
    name: &str,
) -> Result<Collection, SynapseCalyxError> {
    let stored = get_collection(&vault.vault, name).map_err(|error| {
        SynapseCalyxError::from_calyx("read required native TimeSeries collection", &error)
    })?;
    let expected = timeseries_collection(name);
    if stored != expected {
        return Err(SynapseCalyxError::from_calyx(
            "verify required native TimeSeries collection",
            &calyx_core::CalyxError {
                code: "CALYX_TIMESERIES_COLLECTION_CONFLICT",
                message: format!(
                    "stored collection descriptor for {name:?} differs from the required native TimeSeries descriptor"
                ),
                remediation: "use the exact immutable collection generation that owns these series",
            },
        ));
    }
    Ok(stored)
}

fn timeseries_collection(name: &str) -> Collection {
    Collection {
        name: name.to_owned(),
        mode: CollectionMode::TimeSeries,
        schema: None,
        panel: None,
        indexes: Vec::new(),
        dedup: DedupPolicy::Off,
        temporal: TemporalPolicy::default(),
        retention: RetentionPolicy::Forever,
        txn_policy: TxnPolicy::default(),
        tenant: TenantId::default(),
    }
}
