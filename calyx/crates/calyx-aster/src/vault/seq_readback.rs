use super::AsterVault;
use crate::cf::ColumnFamily;
use calyx_core::{Clock, Result, Seq};

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Returns the visible MVCC sequence for one CF/key at `snapshot`.
    pub fn seq_for_key_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Seq>> {
        let snapshot = self.snapshot_handle(snapshot)?;
        self.rows
            .seq_for_key_at(snapshot.snapshot(), cf, key, &self.clock)
    }

    /// Returns the visible MVCC sequence for one CF/key at the latest snapshot.
    pub fn seq_for_key(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Seq>> {
        self.seq_for_key_at(self.latest_seq(), cf, key)
    }
}
