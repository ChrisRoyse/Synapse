//! Fresh-policy freshness gate for persisted search indexes (issue #1100).

use super::{PersistedSearchIndexes, marker};
use crate::error::CliResult;
use calyx_core::CalyxError;

impl PersistedSearchIndexes {
    /// Fresh-policy gate (issue #1100): the manifest must cover every commit
    /// that changed derived-search inputs up to the pin, i.e.
    /// `panel_content_seq <= manifest.base_seq <= pinned_seq`. Writes for
    /// other panels and content-neutral commits advance the vault sequence but
    /// not this exact panel's watermark (#1841). `panel_content_seq` is pinned
    /// atomically with and clamped to `pinned_seq` by the MVCC store.
    pub fn ensure_fresh_at_snapshot(&self, pinned_seq: u64, panel_content_seq: u64) -> CliResult {
        if panel_content_seq > pinned_seq {
            return Err(CalyxError::stale_derived(format!(
                "panel {} content seq {panel_content_seq} exceeds pinned vault seq {pinned_seq}; snapshot watermark was not clamped at pin time — refusing to reason about freshness{}",
                self.manifest.panel_version,
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
        if self.manifest.base_seq < panel_content_seq {
            return Err(CalyxError::stale_derived(format!(
                "persistent search manifest base seq {} is behind panel {} content seq {panel_content_seq} (pinned vault seq {pinned_seq}); a commit after the manifest was built changed this panel's search inputs; rebuild the exact panel search indexes before search{}",
                self.manifest.base_seq,
                self.manifest.panel_version,
                marker::marker_error_context(&self.vault_dir, self.manifest.panel_version)
            ))
            .into());
        }
        Ok(())
    }
}
