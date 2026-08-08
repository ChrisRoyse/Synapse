//! Bounded periodic observation of the machine's real process tree (#2097).
//!
//! # Why this exists
//!
//! `CF_PROCESS_HISTORY` is the sole input to the `syn-graphpos-process-v1`
//! process lane. Until now it had exactly one writer — `act_launch` — so every
//! row in it described a process the daemon itself had just created, and every
//! row's verified parent was therefore the daemon (`parent_is_observer: true`).
//! The lane could only ever derive a **star** centred on the daemon pid, with a
//! new hub appearing on each daemon restart. The panel was named for a spawn
//! tree and was measuring "what one tool launched".
//!
//! #2089 supplied the missing primitive: an exact, pid-reuse-guarded parentage
//! observation. What was missing was a **second writer** that observes processes
//! the daemon did not create. That is what this module is: a bounded periodic
//! read of the live process table, recorded as honest observations.
//!
//! # What a row here claims
//!
//! Nothing beyond what was physically read. Each row names:
//!
//! * the exact mechanism it was read with (`capture_source`),
//! * when it was read (`observed_at_unix_ms` / `ts_ns`),
//! * the process's image **name** and creation time,
//! * the full #2089 `parentage` observation, including every state in which
//!   parentage could *not* be established, with the reason — and the flat
//!   `parent_pid` the graph keys on only when the observation is edge-bearing.
//!
//! It claims nothing about *why* the process exists, and it does not record
//! command lines. The kernel snapshot does not expose them, and a process graph
//! needs identity and structure, not arguments. This is a strictly narrower
//! disclosure than the existing `act_launch` rows, which carry the command line
//! of processes the operator explicitly asked Synapse to start.
//!
//! # Row identity, and why re-observation does not mint rows
//!
//! A pid is not an identity — Windows recycles pids. The identity of a process
//! *occurrence* is `(pid, creation_time)`, and that pair is the row key. A
//! process seen on a hundred ticks therefore occupies one row on all hundred:
//! the key is the same, so a re-write is an update. On top of that the observer
//! keeps the identities it has already recorded and skips the write entirely
//! until [`ROW_REFRESH_INTERVAL_MS`] has passed, so a stable machine produces no
//! storage traffic at all between refreshes.
//!
//! # TTL
//!
//! Rows carry `schema_version: 1` and `ts_ns`, which is exactly what the
//! `CF_PROCESS_HISTORY` retention policy reads, so they age out on the same 6h
//! horizon as every other row in that column family. The refresh interval is
//! deliberately well inside that horizon: a process that is *still running* has
//! its row restamped before it can expire, and a process that has exited stops
//! being restamped and expires on schedule. The horizon a row is subject to is
//! written into the row itself so it can be checked without inferring it.
//!
//! # Bounds
//!
//! Every tick is bounded: at most [`MAX_ROWS_PER_TICK`] rows are written, in
//! chunks of [`WRITE_CHUNK_ROWS`] so the reflex-runtime lock is released between
//! batches, and the in-memory identity ledger is pruned against the live table
//! on every tick, so it cannot outgrow the number of live processes.

use std::{collections::BTreeMap, sync::Mutex};

use serde_json::json;
use synapse_action::process_parentage::{ProcessTopology, ProcessTopologyEntry};

/// The `row_kind` that distinguishes an observed process from a launched one.
///
/// Consumers that want "processes Synapse started" must filter on this rather
/// than assuming every `CF_PROCESS_HISTORY` row is a launch.
pub const PROCESS_OBSERVED_ROW_KIND: &str = "process_observed";
/// The writer name recorded in every row, so a row can say who wrote it.
pub const PROCESS_TOPOLOGY_WRITER: &str = "process_topology_observer";
/// Key prefix. Distinct from `process_history/v1/act_launch/` so the two writers
/// can never collide and either can be scanned alone.
pub const ROW_KEY_PREFIX: &str = "process_history/v1/process_topology/";

/// Most rows one tick may write.
///
/// A full modern Windows desktop runs roughly 300-600 processes, so a normal
/// tick records the whole table in one pass. The bound is here to stop a
/// pathological table from turning one tick into an unbounded write, not to
/// ration ordinary operation: a tick that hits it fills what it can and the
/// remainder lands on the next tick, because the observer is a census and not a
/// deadline.
pub const MAX_ROWS_PER_TICK: usize = 1024;
/// Rows per storage call. The caller re-acquires its storage lock per chunk so a
/// census cannot hold it for the length of a whole batch.
pub const WRITE_CHUNK_ROWS: usize = 32;
/// How long a recorded identity is left alone before its row is restamped.
///
/// Comfortably inside the 6h `CF_PROCESS_HISTORY` TTL so a live process's row
/// cannot expire underneath the lane, and long enough that a stable machine
/// writes nothing on most ticks.
pub const ROW_REFRESH_INTERVAL_MS: u64 = 2 * 60 * 60 * 1000;

/// What one tick did. Returned so an FSV harness can drive a tick directly and
/// read the counts, instead of inferring them from logs.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessTopologyTickReport {
    /// Processes present in the kernel snapshot.
    pub processes_observed: usize,
    /// Of those, how many had an edge-bearing (`parent_verified` + guard-passing)
    /// parentage.
    pub edge_bearing: usize,
    /// Rows actually written this tick.
    pub rows_written: usize,
    /// Identities already recorded and still inside the refresh interval.
    pub rows_skipped_fresh: usize,
    /// Identities that would have been written but exceeded this tick's bound.
    pub rows_deferred_over_bound: usize,
    /// Identities dropped from the ledger because the process is gone.
    pub identities_retired: usize,
}

/// One planned row: its `CF_PROCESS_HISTORY` key and its encoded JSON value.
pub type PlannedRow = (Vec<u8>, Vec<u8>);

/// Per-identity ledger entry. This is the state that makes re-observation cheap
/// and honest: it remembers when an occurrence was first seen, how many ticks
/// have seen it, and when its row was last written.
#[derive(Clone, Copy, Debug)]
struct ObservedIdentity {
    first_observed_at_unix_ms: u64,
    observed_tick_count: u64,
    last_written_at_unix_ms: u64,
}

/// The observer's memory across ticks, keyed by `(pid, creation_time_100ns)`.
#[derive(Debug, Default)]
pub struct ProcessTopologyObserver {
    identities: Mutex<BTreeMap<(u32, u64), ObservedIdentity>>,
}

impl ProcessTopologyObserver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe the process table once and persist what is new or stale.
    ///
    /// `write_chunk` receives at most [`WRITE_CHUNK_ROWS`] rows at a time and is
    /// expected to persist them to `CF_PROCESS_HISTORY`. It is a callback rather
    /// than a borrowed runtime so the caller decides its own lock discipline:
    /// the daemon re-acquires the reflex-runtime mutex per chunk instead of
    /// holding it across a whole census.
    ///
    /// # Errors
    ///
    /// Returns the exact failure text when the process table cannot be read, or
    /// a row fails to encode or persist. The observer never records a partial or
    /// assumed topology.
    pub fn run_once<W>(&self, write_chunk: W) -> Result<ProcessTopologyTickReport, String>
    where
        W: FnMut(Vec<PlannedRow>) -> Result<(), String>,
    {
        let topology = synapse_action::capture_process_topology()?;
        self.record_topology(&topology, write_chunk)
    }

    /// The half of [`Self::run_once`] that takes an already-captured topology.
    ///
    /// Exposed so an FSV harness can capture once, assert against the exact
    /// snapshot it captured, and then record that same snapshot — rather than
    /// asserting against a table that may have changed between two reads.
    ///
    /// # Errors
    ///
    /// Returns the exact failure text when a row fails to encode or persist.
    pub fn record_topology<W>(
        &self,
        topology: &ProcessTopology,
        write_chunk: W,
    ) -> Result<ProcessTopologyTickReport, String>
    where
        W: FnMut(Vec<PlannedRow>) -> Result<(), String>,
    {
        let (rows, mut report) = self.plan_rows(topology)?;
        self.write_rows(&rows, write_chunk, &mut report)?;
        Ok(report)
    }

    /// Decide which observations become rows this tick, and update the ledger.
    ///
    /// The ledger is pruned against `topology` first: an identity absent from
    /// the live table is a process that has exited, and keeping it would let the
    /// ledger grow without bound across a long daemon lifetime.
    fn plan_rows(
        &self,
        topology: &ProcessTopology,
    ) -> Result<(Vec<PlannedRow>, ProcessTopologyTickReport), String> {
        let now_ms = topology.observed_at_unix_ms;
        let mut identities = self
            .identities
            .lock()
            .map_err(|_error| "process topology identity ledger lock poisoned".to_owned())?;

        let live = topology
            .entries
            .iter()
            .map(|entry| (entry.pid, entry.start_time_100ns))
            .collect::<std::collections::BTreeSet<_>>();
        let before = identities.len();
        identities.retain(|identity, _state| live.contains(identity));
        let mut report = ProcessTopologyTickReport {
            processes_observed: topology.entries.len(),
            identities_retired: before.saturating_sub(identities.len()),
            ..ProcessTopologyTickReport::default()
        };

        // Newest processes first: recent structure is the part of the tree most
        // likely to be both changing and unrecorded, so it is what a bounded
        // tick should spend its budget on.
        let mut candidates = topology.entries.iter().collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .start_time_100ns
                .cmp(&left.start_time_100ns)
                .then_with(|| left.pid.cmp(&right.pid))
        });

        let mut rows = Vec::new();
        for entry in candidates {
            if entry.parentage.edge_parent_pid().is_some() {
                report.edge_bearing += 1;
            }
            let identity = (entry.pid, entry.start_time_100ns);
            let state = identities
                .entry(identity)
                .and_modify(|state| state.observed_tick_count += 1)
                .or_insert(ObservedIdentity {
                    first_observed_at_unix_ms: now_ms,
                    observed_tick_count: 1,
                    last_written_at_unix_ms: 0,
                });
            let due = state.last_written_at_unix_ms == 0
                || now_ms.saturating_sub(state.last_written_at_unix_ms) >= ROW_REFRESH_INTERVAL_MS;
            if !due {
                report.rows_skipped_fresh += 1;
                continue;
            }
            if rows.len() >= MAX_ROWS_PER_TICK {
                report.rows_deferred_over_bound += 1;
                continue;
            }
            rows.push((row_key(entry), observation_row(topology, entry, state)?));
            state.last_written_at_unix_ms = now_ms;
        }
        drop(identities);
        Ok((rows, report))
    }

    /// Persist the planned rows in bounded chunks.
    ///
    /// A write failure is fatal to the tick and the ledger is rolled back for
    /// every identity in the failed batch, so a row that was not stored is not
    /// remembered as stored — the next tick retries it rather than leaving a
    /// silent hole in the tree.
    fn write_rows<W>(
        &self,
        rows: &[PlannedRow],
        mut write_chunk: W,
        report: &mut ProcessTopologyTickReport,
    ) -> Result<(), String>
    where
        W: FnMut(Vec<PlannedRow>) -> Result<(), String>,
    {
        for chunk in rows.chunks(WRITE_CHUNK_ROWS) {
            let batch = chunk.to_vec();
            let keys = batch
                .iter()
                .map(|(key, _row)| key.clone())
                .collect::<Vec<_>>();
            match write_chunk(batch) {
                Ok(()) => report.rows_written += chunk.len(),
                Err(error) => {
                    self.forget(&keys);
                    return Err(format!(
                        "persist {} process topology rows to CF_PROCESS_HISTORY failed: {error}",
                        chunk.len()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Undo the "written" stamp for rows that did not reach storage.
    fn forget(&self, keys: &[Vec<u8>]) {
        let Ok(mut identities) = self.identities.lock() else {
            return;
        };
        for key in keys {
            if let Some(identity) = identity_from_key(key) {
                identities.remove(&identity);
            }
        }
    }
}

/// `process_history/v1/process_topology/<pid>/<creation_time_100ns>`, both
/// zero-padded so the key sorts numerically.
///
/// The creation time is part of the key on purpose: it is what makes the key an
/// identity rather than a pid, so a recycled pid gets its own row instead of
/// overwriting the record of the process that previously held that number.
#[must_use]
pub fn row_key(entry: &ProcessTopologyEntry) -> Vec<u8> {
    format!(
        "{ROW_KEY_PREFIX}{:010}/{:020}",
        entry.pid, entry.start_time_100ns
    )
    .into_bytes()
}

fn identity_from_key(key: &[u8]) -> Option<(u32, u64)> {
    let text = std::str::from_utf8(key).ok()?;
    let tail = text.strip_prefix(ROW_KEY_PREFIX)?;
    let (pid, start_time) = tail.split_once('/')?;
    Some((pid.parse().ok()?, start_time.parse().ok()?))
}

/// Build one observation row.
///
/// The shape satisfies the process lane's input contract (`derived_state.rs`
/// `process_parent_edge`): `pid`, the full `parentage` observation carrying the
/// #2089 reuse-guard fields, and a flat `parent_pid` **only** when the
/// observation is edge-bearing. Writing `parent_pid` on an unproven parentage
/// would be the one way this writer could fabricate an edge, so it is not
/// written at all rather than written as null.
fn observation_row(
    topology: &ProcessTopology,
    entry: &ProcessTopologyEntry,
    state: &ObservedIdentity,
) -> Result<Vec<u8>, String> {
    let observed_at_unix_ms = topology.observed_at_unix_ms;
    let ts_ns = observed_at_unix_ms
        .checked_mul(1_000_000)
        .ok_or_else(|| format!("observation timestamp {observed_at_unix_ms} overflows ns"))?;
    let parentage = serde_json::to_value(&entry.parentage)
        .map_err(|error| format!("encode parentage for pid {}: {error}", entry.pid))?;
    let ttl_hours = process_history_ttl_hours();
    let mut row = json!({
        "schema_version": 1,
        "row_kind": PROCESS_OBSERVED_ROW_KIND,
        "tool": PROCESS_TOPOLOGY_WRITER,
        "status": "observed",
        // Names the exact kernel mechanism the facts came from. A row that
        // cannot say how it was read is an assertion, not an observation.
        "capture_source": topology.capture_source,
        "observer_pid": topology.observer_pid,
        "pid": entry.pid,
        // Image name only — never a command line (see the module docs).
        "image_name": entry.image_name,
        "parent_image_name": entry.parentage.parent_image_name,
        "start_time_100ns": entry.start_time_100ns,
        "parentage": parentage,
        "observed_at_unix_ms": observed_at_unix_ms,
        "ts_ns": ts_ns,
        // Since this daemon started observing, not since the process started.
        "first_observed_at_unix_ms": state.first_observed_at_unix_ms,
        "observed_tick_count": state.observed_tick_count,
        "retention_cf": synapse_storage::cf::CF_PROCESS_HISTORY,
        "retention_ttl_hours": ttl_hours,
        "retention_expires_at_unix_ms": observed_at_unix_ms
            .saturating_add(ttl_hours.saturating_mul(60 * 60 * 1000)),
        "retention_refresh_interval_ms": ROW_REFRESH_INTERVAL_MS,
    });
    if let Some(parent_pid) = entry.parentage.edge_parent_pid() {
        let object = row
            .as_object_mut()
            .ok_or_else(|| "process topology row was not a JSON object".to_owned())?;
        object.insert("parent_pid".to_owned(), json!(parent_pid));
    }
    serde_json::to_vec(&row).map_err(|error| format!("encode process topology row: {error}"))
}

/// The TTL horizon these rows are actually subject to, read from the retention
/// policy rather than restated, so the row cannot disagree with the GC.
fn process_history_ttl_hours() -> u64 {
    synapse_core::retention::DEFAULTS
        .iter()
        .find(|entry| entry.cf == synapse_storage::cf::CF_PROCESS_HISTORY)
        .map_or(0, |entry| match entry.ttl {
            synapse_core::retention::RetentionTtl::Hours(hours) => hours,
            synapse_core::retention::RetentionTtl::Days(days) => days.saturating_mul(24),
            synapse_core::retention::RetentionTtl::None
            | synapse_core::retention::RetentionTtl::LruOnly => 0,
        })
}
