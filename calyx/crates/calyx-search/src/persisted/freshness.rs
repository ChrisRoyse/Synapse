//! Fresh-policy freshness gate for persisted search indexes (issue #1100).

use super::{PersistedSearchIndexes, marker};
use crate::error::CliResult;
use calyx_core::CalyxError;

impl PersistedSearchIndexes {
    /// Fresh-policy gate (issue #1100): the manifest must cover every commit
    /// that changed derived-search inputs up to the pin, i.e.
    /// `derived_content_seq <= manifest.base_seq <= pinned_seq`. Content-neutral
    /// commits (idempotency-ledger appends, time-index sentinels) advance the
    /// pinned vault seq but not `derived_content_seq`, so a replay-only batch
    /// no longer breaks Fresh search. `derived_content_seq` must already be
    /// clamped to `pinned_seq` (the MVCC snapshot pin does this fail-closed).
    pub fn ensure_fresh_at_snapshot(&self, pinned_seq: u64, derived_content_seq: u64) -> CliResult {
        if derived_content_seq > pinned_seq {
            return Err(CalyxError::stale_derived(format!(
                "derived content seq {derived_content_seq} exceeds pinned vault seq {pinned_seq}; snapshot watermark was not clamped at pin time — refusing to reason about freshness{}",
                marker::marker_error_context(&self.vault_dir, self.manifest.panel_version)
            ))
            .into());
        }
        if self.manifest.base_seq > pinned_seq {
            return Err(CalyxError::stale_derived(format!(
                "persistent search manifest base seq {} is ahead of pinned vault seq {pinned_seq}; the manifest was built after this snapshot — rebuild the vault search indexes or retry against the latest vault seq{}",
                self.manifest.base_seq,
                marker::marker_error_context(&self.vault_dir, self.manifest.panel_version)
            ))
            .into());
        }
        if self.manifest.base_seq < derived_content_seq {
            return Err(CalyxError::stale_derived(format!(
                "persistent search manifest base seq {} is behind derived content seq {derived_content_seq} (pinned vault seq {pinned_seq}); a commit after the manifest was built changed search inputs; rebuild the vault search indexes before search{}",
                self.manifest.base_seq,
                marker::marker_error_context(&self.vault_dir, self.manifest.panel_version)
            ))
            .into());
        }
        Ok(())
    }
}
