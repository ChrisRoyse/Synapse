//! Reactive trigger/subscription engine (PH72 · T02).
//!
//! A bounded, audited subsystem that evaluates [`TriggerDef`] conditions
//! immediately after an ingest completes. Three condition variants are
//! supported — [`TriggerCondition::NewRegion`] (Ward novelty against the
//! configured panel at calibrated τ), [`TriggerCondition::EventRecurs`] (a
//! recurrence series crosses an occurrence threshold), and
//! [`TriggerCondition::DriftDetected`] (agreement-graph cosine drift exceeds a
//! threshold). On match a [`TriggerFired`] event is enqueued.
//!
//! Everything is bounded by construction (A26): the registry caps at
//! `max_triggers` ([`CALYX_REACTIVE_REGISTRY_FULL`]), the fired-event queue caps
//! at `max_queue_depth` (oldest discarded + [`CALYX_REACTIVE_QUEUE_FULL`] on
//! overflow), and the audit log is a ring capped at `max_audit_entries`. Every
//! evaluation — match or no-match — appends one [`AuditEntry`] carrying the
//! Ledger reference of the ingest that triggered it (A15), so the audit log is
//! the immutable source of truth for what the engine decided and why.

use std::collections::VecDeque;

use calyx_core::{CxId, LedgerRef, PanelSlotId, Result, Ts};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CALYX_REACTIVE_QUEUE_FULL, CALYX_REACTIVE_REGISTRY_FULL, loom_error};

mod durable;
mod engine;
mod signals;
mod subscription;

pub use durable::{
    ReactiveRowKey, ReactiveRowKind, decode_audit_entry, decode_trigger_fired, reactive_audit_key,
    reactive_audit_prefix, reactive_fired_key, reactive_row_key,
};
pub use engine::ReactiveEngine;
pub use signals::{
    AgreementDriftSignals, AgreementDriftTracker, ReactiveSignalSet, RecurrenceSignals,
    WardNoveltySignals,
};
pub use subscription::{
    DEFAULT_MAX_DRAIN_BUF, DEFAULT_MAX_SUBSCRIPTIONS, SubscriptionDelta, SubscriptionHandle,
    SubscriptionId, SubscriptionStore,
};

/// Stable identifier for a registered trigger (UUID v7 — time-ordered).
pub type TriggerId = Uuid;

/// Default hard cap on registered triggers (A26).
pub const DEFAULT_MAX_TRIGGERS: usize = 1024;
/// Default hard cap on undelivered [`TriggerFired`] events (A26).
pub const DEFAULT_MAX_QUEUE_DEPTH: usize = 4096;
/// Default hard cap on retained [`AuditEntry`] rows (A26).
pub const DEFAULT_MAX_AUDIT_ENTRIES: usize = 65536;

/// The condition a trigger evaluates after each ingest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerCondition {
    /// Fires when the constellation reads as a *new region* — the configured
    /// panel's Gτ guard reports novelty at the calibrated τ (or `tau_override`).
    NewRegion {
        /// Optional τ override; `None` uses the panel's calibrated threshold.
        tau_override: Option<f32>,
    },
    /// Fires when the recurrence series for `series` crosses `min_occurrences`
    /// occurrences (the count both incremented this ingest and reached the bar).
    EventRecurs {
        /// The recurrence series, keyed by its constellation id.
        series: CxId,
        /// Occurrence count at or above which the trigger fires.
        min_occurrences: u32,
    },
    /// Fires when the absolute cosine drift for `slot` since the previous
    /// evaluation meets or exceeds `drift_threshold`.
    DriftDetected {
        /// The panel slot whose agreement cosine is watched.
        slot: PanelSlotId,
        /// Absolute `|Δcosine|` at or above which the trigger fires.
        drift_threshold: f32,
    },
}

/// A registered trigger definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerDef {
    /// Stable id assigned at registration.
    pub id: TriggerId,
    /// The condition evaluated after each ingest.
    pub condition: TriggerCondition,
    /// Wall-clock registration time (engine clock, Unix ms).
    pub created_at: Ts,
    /// Optional owning tenant (free-form; `None` for system triggers).
    pub owner: Option<String>,
}

/// An emitted match: a trigger whose condition held for a specific ingest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerFired {
    /// The trigger that fired.
    pub trigger_id: TriggerId,
    /// The constellation ingested when the condition held.
    pub cx_id: CxId,
    /// Engine-clock time of the fire (Unix ms).
    pub fired_at: Ts,
    /// Ledger reference of the ingest that caused the fire (A15).
    pub ledger_ref: LedgerRef,
    /// Snapshot of the condition at fire time (the def may later change).
    pub condition_snapshot: TriggerCondition,
}

/// One immutable audit record: the verdict of a single trigger evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique id for this evaluation (UUID v7).
    pub eval_id: Uuid,
    /// The trigger evaluated.
    pub trigger_id: TriggerId,
    /// The constellation the evaluation ran against.
    pub cx_id: CxId,
    /// Whether the condition matched (and a [`TriggerFired`] was produced).
    pub matched: bool,
    /// Engine-clock time of the evaluation (Unix ms).
    pub ts: Ts,
    /// Ledger reference of the ingest that drove the evaluation.
    pub ledger_ref: LedgerRef,
    /// Set when the entry records a bounded-resource warning (e.g.
    /// [`CALYX_REACTIVE_QUEUE_FULL`]); `None` for ordinary verdicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A FIFO queue of fired events with a hard depth cap. On overflow the oldest
/// undelivered event is discarded (ring semantics) so memory is bounded (A26).
#[derive(Clone, Debug)]
pub struct BoundedQueue<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> BoundedQueue<T> {
    /// Creates a queue with hard capacity `capacity` (min 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Pushes `item`. On overflow discards and returns the oldest item so the
    /// caller can record the loss; otherwise returns `None`.
    pub fn push(&mut self, item: T) -> Option<T> {
        let dropped = if self.items.len() >= self.capacity {
            self.items.pop_front()
        } else {
            None
        };
        self.items.push_back(item);
        dropped
    }

    /// Removes and returns the oldest queued item, if any.
    pub fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    /// Number of queued items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when no items are queued.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Hard capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Read-only view of the queued items, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }
}

/// An append-only ring of [`AuditEntry`] rows capped at `max_entries`. When full,
/// the oldest entry is overwritten — the audit window is bounded (A26) but always
/// retains the most recent decisions, including the latest overflow warning.
#[derive(Clone, Debug)]
pub struct AuditLog {
    entries: VecDeque<AuditEntry>,
    max_entries: usize,
}

impl AuditLog {
    /// Creates an audit log retaining at most `max_entries` rows (min 1).
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: max_entries.max(1),
        }
    }

    /// Appends `entry`, evicting the oldest row if at capacity.
    pub fn append(&mut self, entry: AuditEntry) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Number of retained rows.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no rows are retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most recent audit row, if any.
    pub fn last(&self) -> Option<&AuditEntry> {
        self.entries.back()
    }

    /// Read-only view of retained rows, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = &AuditEntry> {
        self.entries.iter()
    }
}

/// A bounded set of registered triggers keyed by [`TriggerId`].
#[derive(Clone, Debug)]
pub struct TriggerRegistry {
    defs: Vec<TriggerDef>,
    max_triggers: usize,
}

impl TriggerRegistry {
    /// Creates a registry capped at `max_triggers` (min 1).
    pub fn new(max_triggers: usize) -> Self {
        Self {
            defs: Vec::new(),
            max_triggers: max_triggers.max(1),
        }
    }

    /// Registers `def`, failing closed with [`CALYX_REACTIVE_REGISTRY_FULL`] when
    /// the registry is already at `max_triggers`. Existing triggers are untouched
    /// on rejection.
    pub fn register(&mut self, def: TriggerDef) -> Result<TriggerId> {
        if self.defs.len() >= self.max_triggers {
            return Err(loom_error(
                CALYX_REACTIVE_REGISTRY_FULL,
                format!(
                    "trigger registry full at {} entries; cannot admit {}",
                    self.max_triggers, def.id
                ),
            ));
        }
        let id = def.id;
        self.defs.push(def);
        Ok(id)
    }

    /// Removes the trigger with `id`, returning whether it existed.
    pub fn deregister(&mut self, id: TriggerId) -> bool {
        let before = self.defs.len();
        self.defs.retain(|def| def.id != id);
        self.defs.len() != before
    }

    /// Snapshot of all registered triggers, in registration order.
    pub fn list(&self) -> Vec<TriggerDef> {
        self.defs.clone()
    }

    /// Number of registered triggers.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// True when no triggers are registered.
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Hard capacity.
    pub fn capacity(&self) -> usize {
        self.max_triggers
    }

    pub(crate) fn defs(&self) -> &[TriggerDef] {
        &self.defs
    }
}

/// The verdict a [`ReactiveSignals`] source returns for a novelty evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoveltyVerdict {
    /// The constellation is a new region (fire `NewRegion`).
    NewRegion,
    /// The constellation grounded against an existing region (no fire).
    Grounded,
}

/// The data source the engine queries to evaluate trigger conditions against the
/// post-ingest state. Abstracted so the engine never hard-depends on Ward, the
/// recurrence store, or the agreement graph directly — each condition is backed
/// by an injectable source.
///
/// Implementors must **fail closed**: a source that cannot evaluate a condition
/// returns an error (e.g. [`crate::CALYX_REACTIVE_SIGNAL_UNAVAILABLE`]) rather
/// than silently reporting "no match".
pub trait ReactiveSignals {
    /// Novelty verdict for `cx_id` against the configured panel at the calibrated
    /// τ (or `tau_override` when set). Errors propagate (fail closed).
    fn novelty(&self, cx_id: CxId, tau_override: Option<f32>) -> Result<NoveltyVerdict>;

    /// Current occurrence count for the recurrence series keyed by `series`.
    fn occurrence_count(&self, series: CxId) -> Result<u64>;

    /// Absolute cosine drift `|Δcosine|` for `slot` since the previous
    /// evaluation. Errors propagate (fail closed).
    fn slot_drift(&self, slot: PanelSlotId) -> Result<f32>;
}

pub(crate) fn queue_full_error(dropped: &TriggerFired) -> calyx_core::CalyxError {
    loom_error(
        CALYX_REACTIVE_QUEUE_FULL,
        format!(
            "reactive queue full; discarded oldest fired event for trigger {}",
            dropped.trigger_id
        ),
    )
}
