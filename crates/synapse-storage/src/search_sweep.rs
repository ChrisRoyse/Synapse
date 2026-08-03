//! Unattended maintenance of **every** published search generation (#1938).
//!
//! ## What was wrong
//!
//! `maintain_calyx_search_generation` maintained one generation: the one the
//! vault manifest publishes as active. Non-active generations had no maintainer
//! at all, and they do not sit still — every MCP tool call appends an
//! `mcp-usage` row, which advances the vault sequence, which grows every
//! generation's reconciliation delta. So the vault degraded any non-active
//! generation continuously, in proportion to how much the agent was used, until
//! it crossed `MAX_RECONCILED_DELTA_KEYS` and every query against it failed
//! closed with `CALYX_SEARCH_DELTA_REBASE_REQUIRED`.
//!
//! Measured on the live vault: the agent-transcript generation — the largest
//! corpus on the vault at 26,694 rows / 148 MB — was reachable for about two
//! hours after it was built, then required a break-glass rebuild to use again.
//! A rejected query also cost 183-255 ms against 26-34 ms for the same query on
//! a fresh generation, because the refusal scans the whole `Base` CF before it
//! refuses. A stale generation is both unusable *and* ~7x more expensive to ask.
//!
//! This was latent before #1668: only the active panel could hold a generation,
//! so there was no non-active generation to let rot. #1668 made non-active
//! panels queryable and rebuildable, which is what turned "queryable" into a
//! state something has to *keep*.
//!
//! ## The shape of the fix
//!
//! The set of generations owed maintenance is discovered **from disk** —
//! `idx/search/panel_*/manifest.json` is the publication — not from the active
//! panel pointer. That is the first-principles correction: "which generations
//! exist" and "which panel is active" are different questions, and the second
//! was being used to answer the first.
//!
//! Each generation is then maintained by exactly the same policy, bounds, and
//! post-build disk readback as the active one, because a freshness budget is a
//! property of the generation, not of what the manifest happens to point at.
//! This is the standard shape for incrementally-maintained retrieval indexes:
//! per-index maintenance debt with a per-index consolidation threshold, rather
//! than one global rebuild trigger (Postgres BM25 extensions carry
//! `auto_rebuild_threshold` per index; Lucene/Elasticsearch schedule merges per
//! segment on that segment's own superseded fraction).
//!
//! ## The declared expiry state (#1938 ask 2)
//!
//! Rebuilding a generation requires that panel's slot contract, and only
//! code-declared panel versions have one (`syn_active_panel_contract`). A
//! published generation for a version with no contract therefore **cannot** be
//! rebuilt by anything, and a query naming it already fails closed at the
//! contract lookup. That is a permanent, stable condition, so it is reported as
//! one: [`GenerationDisposition::UnmaintainableNoContract`], from the first
//! tick, with the action that resolves it. It is not left to be discovered as a
//! query that starts failing after an unpredictable amount of unrelated write
//! traffic — which is precisely the failure mode this module exists to remove.
//!
//! Nothing here deletes a generation directory. Reclaiming disk is a
//! destructive, operator-owned act; naming the condition is not.

use synapse_calyx::SearchGenerationMaintenanceReport;

/// What the sweep did about one published search generation.
#[derive(Clone, Debug)]
pub enum GenerationDisposition {
    /// The generation was evaluated against its freshness budget and the
    /// decision was carried out. The report says which of the five maintenance
    /// actions ran and what the on-disk state was afterwards.
    Maintained(Box<SearchGenerationMaintenanceReport>),
    /// A generation is published for a panel version that has no code-declared
    /// slot contract **and no place in any live panel's lineage**, so nothing
    /// can rebuild it, no query can measure through it, and nobody can say what
    /// it is. A declared terminal state, not a transient one — and the one that
    /// must be investigated before anything is deleted.
    UnmaintainableNoContract,
    /// A generation is published for a **closed superseded version of a panel
    /// that is still live** (#1972). It has no contract for the same reason
    /// every superseded version has none — the code declares the live layout —
    /// but unlike [`Self::UnmaintainableNoContract`] its disposition is known:
    /// the live generation of the same panel carries the corpus, so this
    /// directory is reclaimable through `storage operation=retire_search_generation`.
    ///
    /// Split out because the two used to share a bucket, and that bucket was a
    /// *permanent* floor under `calyx_search_generation`: the live vault has
    /// carried `unmaintainable=1` continuously for a superseded timeline
    /// generation, so an operator reading `degraded` learned nothing and the
    /// next genuinely-unknown generation would have been invisible against that
    /// background.
    RetirableSupersededGeneration {
        /// The live panel this version is a superseded generation of.
        panel_name: &'static str,
        /// The generation that superseded it and is maintained today.
        live_panel_version: u32,
    },
    /// Maintaining this one generation failed. Recorded per generation so a
    /// single bad generation cannot starve every other generation of
    /// maintenance, while still failing the pass as a whole.
    Failed { code: String, detail: String },
}

impl GenerationDisposition {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Maintained(_) => "maintained",
            Self::UnmaintainableNoContract => "unmaintainable_no_contract",
            Self::RetirableSupersededGeneration { .. } => "retirable_superseded_generation",
            Self::Failed { .. } => "failed",
        }
    }

    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Whether this generation is a known-reclaimable superseded one (#1972).
    #[must_use]
    pub const fn is_retirable(&self) -> bool {
        matches!(self, Self::RetirableSupersededGeneration { .. })
    }
}

/// One published generation's maintenance outcome, with enough state attached
/// that `health` never has to re-measure a corpus to report it.
#[derive(Clone, Debug)]
pub struct PanelGenerationMaintenance {
    pub panel_version: u32,
    /// Whether this is the panel the vault manifest currently publishes as
    /// active. Reported because it is the *only* thing that used to distinguish
    /// a maintained generation from an abandoned one, and an operator reading
    /// the sweep should be able to see that it no longer does.
    pub is_active_panel: bool,
    pub disposition: GenerationDisposition,
}

impl PanelGenerationMaintenance {
    /// Measured changed keys after the pass, falling back to the pre-pass
    /// measurement when nothing was built. `None` means unmeasured, which is
    /// never the same as zero.
    #[must_use]
    pub fn delta_changed_keys(&self) -> Option<u64> {
        let GenerationDisposition::Maintained(report) = &self.disposition else {
            return None;
        };
        report
            .after
            .as_ref()
            .map_or(report.before.delta_changed_keys, |after| {
                after.delta_changed_keys
            })
    }

    /// How many more changed keys this generation can absorb before queries
    /// against it fail closed.
    ///
    /// This is the number that predicts the failure, so it is the number
    /// `health` reports. Before this, the only per-generation fact published was
    /// for the active panel, so "this panel is about to stop answering" was
    /// visible exactly once: after the first query that failed.
    ///
    /// `None` when the delta is unmeasured. Zero means the bound is already
    /// crossed and every query against this generation is failing now.
    #[must_use]
    pub fn keys_to_bound(&self) -> Option<u64> {
        let GenerationDisposition::Maintained(report) = &self.disposition else {
            return None;
        };
        let limit = report.before.max_reconciled_delta_keys;
        self.delta_changed_keys()
            .map(|keys| limit.saturating_sub(keys))
    }

    /// One line of the health detail: everything an operator needs to decide
    /// whether to act, per generation.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let disposition = self.disposition.as_str();
        let active = if self.is_active_panel { " active" } else { "" };
        match &self.disposition {
            GenerationDisposition::Maintained(report) => {
                let state = report
                    .after
                    .as_ref()
                    .map_or(report.before.state.as_str(), |after| after.state.as_str());
                let built_at = report
                    .after
                    .as_ref()
                    .map_or(report.before.built_at_seq, |after| after.built_at_seq);
                format!(
                    "panel {}{active} {disposition} action={} state={state} built_at_seq={:?} \
                     delta_changed_keys={:?} keys_to_bound={:?} limit={} elapsed_ms={}",
                    self.panel_version,
                    report.action.as_str(),
                    built_at,
                    self.delta_changed_keys(),
                    self.keys_to_bound(),
                    report.before.max_reconciled_delta_keys,
                    report.elapsed_ms,
                )
            }
            GenerationDisposition::UnmaintainableNoContract => format!(
                "panel {}{active} {disposition}: a search generation is published for a panel \
                 version with no code-declared slot contract and no place in any live panel's \
                 declared lineage, so no rebuild can reconstruct it, no query can measure \
                 through it, and nothing establishes what it is; action=investigate what wrote \
                 idx/search/panel_{:010} and declare the panel's contract before deleting \
                 anything",
                self.panel_version, self.panel_version,
            ),
            GenerationDisposition::RetirableSupersededGeneration {
                panel_name,
                live_panel_version,
            } => format!(
                "panel {}{active} {disposition}: a search generation is published for a closed \
                 superseded version of {panel_name}, whose live generation {live_panel_version} \
                 carries the corpus; no query can reach this one and no rebuild can reconstruct \
                 it; action=storage operation=retire_search_generation panel_version={}",
                self.panel_version, self.panel_version,
            ),
            GenerationDisposition::Failed { code, detail } => {
                format!(
                    "panel {}{active} {disposition} code={code} detail={detail}",
                    self.panel_version
                )
            }
        }
    }
}

/// One unattended pass over every published search generation.
#[derive(Clone, Debug, Default)]
pub struct SearchGenerationSweep {
    /// The index root the generations were discovered under.
    pub index_root: String,
    /// The panel the vault manifest publishes as active, when one is published.
    pub active_panel_version: Option<u32>,
    /// One entry per generation considered, ascending by panel version.
    pub generations: Vec<PanelGenerationMaintenance>,
    /// Entries under the index root that are not published generations, carried
    /// through so the sweep's scope is auditable rather than assumed complete.
    pub unrecognized_index_entries: Vec<String>,
    pub elapsed_ms: u64,
}

impl SearchGenerationSweep {
    #[must_use]
    pub fn any_failed(&self) -> bool {
        self.generations
            .iter()
            .any(|entry| entry.disposition.is_failure())
    }

    /// The entry for the vault's active panel, which is what the pre-#1938
    /// single-generation health field reports.
    #[must_use]
    pub fn active_generation(&self) -> Option<&PanelGenerationMaintenance> {
        self.generations.iter().find(|entry| entry.is_active_panel)
    }

    /// The smallest remaining headroom across every maintained generation, and
    /// the panel that holds it. This is the sweep's headline number: the vault
    /// is as close to a failing query as its closest generation.
    #[must_use]
    pub fn closest_to_bound(&self) -> Option<(u32, u64)> {
        self.generations
            .iter()
            .filter_map(|entry| {
                entry
                    .keys_to_bound()
                    .map(|keys| (entry.panel_version, keys))
            })
            .min_by_key(|(_, keys)| *keys)
    }

    #[must_use]
    pub fn summary_line(&self) -> String {
        let generations = self
            .generations
            .iter()
            .map(PanelGenerationMaintenance::summary_line)
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "index_root={} active_panel={:?} generations={} unrecognized={:?} elapsed_ms={} [{}]",
            self.index_root,
            self.active_panel_version,
            self.generations.len(),
            self.unrecognized_index_entries,
            self.elapsed_ms,
            generations,
        )
    }
}
