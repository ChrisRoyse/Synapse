//! Declared measurement provenance for the built-in Synapse panels (#1958).
//!
//! Anchor leakage is a **provenance** property, not a statistical one. Two
//! lenses with identical bits about an outcome — one that reads the label and
//! one that genuinely predicts it — are information-theoretically
//! indistinguishable, so no threshold, CI width or cardinality rule can separate
//! them. What separates them is where the lens's input came from, and that is
//! what this module declares.
//!
//! The tables live here, beside the assay that consumes them, rather than in
//! `synapse-storage` where the constellation builders are: `synapse-storage`
//! depends on this crate, not the reverse. `constellations::
//! assert_syn_lens_provenance_complete` closes the loop from the other side by
//! cross-checking both columns of [`SYN_SLOT_SOURCE_FIELDS`] against the
//! authoritative slot/lens catalog before any panel contract is built.

use std::collections::BTreeSet;

/// The record fields every built-in `syn-*` panel slot measures (#1958).
///
/// ## Why this exists
///
/// `detect_anchor_leakage` (#1953 ask 2) fires when a lens's marginal bits equal
/// `H(anchor)` at matching cardinality — a signature only a lens that **is** the
/// label can meet. A lens that merely *contains* the label, as one component of
/// a dense vector or as a field that determines it, meets neither condition and
/// passes cleanly. Measured on `syn-mcp-usage-v1 @ 1776006` against
/// `synapse:mcp_tool_call_outcome`, the statistical detector found slot 86 and
/// missed slots 87 and 93, both of which carry the answer.
///
/// **That gap cannot be closed by tuning the detector.** Two lenses with
/// identical bits about an outcome — one that reads the label and one that
/// genuinely predicts it — are information-theoretically indistinguishable. The
/// difference is not in the distribution; it is in where the lens's input came
/// from. Leakage is a **provenance** property, so it needs a provenance answer.
///
/// ## What a declaration means
///
/// One entry per slot: the panel-local record field paths that slot's
/// construction site reads, **transitively**. A `record_vector` lens declares
/// every field its numeric-record builder touches, because the label being one
/// component of a 128-dimension vector is exactly the case the statistical
/// detector cannot see.
///
/// An **empty** declaration is a positive statement — "this lens reads no record
/// field" — and is used only by lenses measured from a derived snapshot (graph
/// position, path hierarchy, recurrence subject) or from the source CF name
/// rather than the row. It is not the same as *undeclared*: a slot missing from
/// this table fails `constellations::assert_syn_lens_provenance_complete`,
/// which runs before any panel contract is built.
///
/// ## Over-declaring is the safe direction
///
/// A field listed here that the lens does not really read can only cause a
/// refusal a caller can lift with `excluded_slots`. A field omitted here is a
/// silent pass on a circular measurement, which is the failure this exists to
/// stop. When in doubt, list it.
pub const SYN_SLOT_SOURCE_FIELDS: &[(u16, u32, &str, &[&str])] = &[
    (1, 1900001, "syn.timeline.kind_onehot.v1", &["kind"]),
    (2, 1900001, "syn.timeline.app_hash.v1", &["app"]),
    (3, 1900001, "syn.timeline.title_sparse.v1", &["payload"]),
    (4, 1900001, "syn.timeline.hour_cyclic.v1", &["ts_ns"]),
    (5, 1900001, "syn.timeline.dow_cyclic.v1", &["ts_ns"]),
    (6, 1900001, "syn.timeline.actor_onehot.v1", &["actor"]),
    (7, 1900001, "syn.timeline.event_time_rank.v1", &["ts_ns"]),
    (103, 1900001, "syn.timeline.title_bm25.v1", &["payload"]),
    (8, 1904002, "syn.episode.app_hash.v1", &["app"]),
    (9, 1904002, "syn.episode.document_hash.v1", &["document"]),
    (10, 1904002, "syn.episode.url_host_hash.v1", &["url"]),
    (
        11,
        1904002,
        "syn.episode.title_sparse.v1",
        &["title_first", "title_last"],
    ),
    (
        108,
        1904002,
        "syn.episode.title_bm25.v1",
        &["title_first", "title_last"],
    ),
    (
        12,
        1904002,
        "syn.episode.start_hour_cyclic.v1",
        &["start_ts_ns"],
    ),
    (
        13,
        1904002,
        "syn.episode.start_dow_cyclic.v1",
        &["start_ts_ns"],
    ),
    (
        14,
        1904002,
        "syn.episode.duration_log1p.v1",
        &["duration_ms"],
    ),
    (
        15,
        1904002,
        "syn.episode.duration_rank.v1",
        &["duration_ms"],
    ),
    (
        16,
        1904002,
        "syn.episode.keystrokes_zscore.v1",
        &["keystroke_count"],
    ),
    (
        17,
        1904002,
        "syn.episode.clicks_zscore.v1",
        &["click_count"],
    ),
    (
        18,
        1904002,
        "syn.episode.row_count_zscore.v1",
        &["row_count"],
    ),
    (
        19,
        1904002,
        "syn.episode.started_boundary_onehot.v1",
        &["started_because"],
    ),
    (
        20,
        1904002,
        "syn.episode.ended_boundary_onehot.v1",
        &["ended_because"],
    ),
    (
        21,
        1904002,
        "syn.episode.interruption_ratio_raw.v1",
        &["duration_ms", "interrupted_ms"],
    ),
    (
        22,
        1904002,
        "syn.episode.record_vector.v1",
        &[
            "click_count",
            "distinct_title_count",
            "duration_ms",
            "end_ts_ns",
            "interrupted_ms",
            "interruption_count",
            "keystroke_count",
            "row_count",
            "start_ts_ns",
        ],
    ),
    (23, 1665001, "syn.agent_event.kind_onehot.v1", &["kind"]),
    (
        24,
        1665001,
        "syn.agent_event.operation_onehot.v1",
        &["attributes.operation_name"],
    ),
    (
        25,
        1665001,
        "syn.agent_event.provider_hash.v1",
        &["attributes.provider_name"],
    ),
    (
        26,
        1665001,
        "syn.agent_event.request_model_hash.v1",
        &["attributes.request_model"],
    ),
    (
        27,
        1665001,
        "syn.agent_event.response_model_hash.v1",
        &["attributes.response_model"],
    ),
    (
        28,
        1665001,
        "syn.agent_event.tool_hash.v1",
        &["attributes.tool_name"],
    ),
    (
        29,
        1665001,
        "syn.agent_event.error_onehot.v1",
        &["attributes.error_type"],
    ),
    (
        30,
        1665001,
        "syn.agent_event.end_state_onehot.v1",
        &["end_state"],
    ),
    (31, 1665001, "syn.agent_event.hour_cyclic.v1", &["ts_ns"]),
    (32, 1665001, "syn.agent_event.dow_cyclic.v1", &["ts_ns"]),
    (
        33,
        1665001,
        "syn.agent_event.usage_total_log1p.v1",
        &[
            "attributes.usage_cache_creation_input_tokens",
            "attributes.usage_cache_read_input_tokens",
            "attributes.usage_input_tokens",
            "attributes.usage_output_tokens",
        ],
    ),
    (
        34,
        1665001,
        "syn.agent_event.record_vector.v1",
        &[
            "attributes.error_type",
            "attributes.tool_name",
            "attributes.usage_cache_creation_input_tokens",
            "attributes.usage_cache_read_input_tokens",
            "attributes.usage_input_tokens",
            "attributes.usage_output_tokens",
            "end_state",
            "payload",
            "session_id",
            "spawn_id",
            "ts_ns",
        ],
    ),
    (
        35,
        1921001,
        "syn.agent_transcript.role_onehot.v1",
        &["role"],
    ),
    (
        36,
        1921001,
        "syn.agent_transcript.status_onehot.v1",
        &["status"],
    ),
    (
        37,
        1921001,
        "syn.agent_transcript.source_onehot.v1",
        &["source"],
    ),
    (
        38,
        1921001,
        "syn.agent_transcript.event_kind_hash.v1",
        &["event_kind"],
    ),
    (
        39,
        1921001,
        "syn.agent_transcript.model_hash.v1",
        &["model"],
    ),
    (
        40,
        1921001,
        "syn.agent_transcript.text_sparse.v1",
        &["content_summary", "parse_error", "source_error"],
    ),
    (
        107,
        1921001,
        "syn.agent_transcript.text_bm25.v1",
        &["content_summary", "parse_error", "source_error"],
    ),
    (
        109,
        1921001,
        "syn.agent_transcript.text_full_bm25.v1",
        &[
            "content_summary",
            "parse_error",
            "source_error",
            "tool_calls",
        ],
    ),
    (
        41,
        1921001,
        "syn.agent_transcript.tool_hash.v1",
        &["tool_calls"],
    ),
    (
        42,
        1921001,
        "syn.agent_transcript.line_rank.v1",
        &["line_no"],
    ),
    (
        43,
        1921001,
        "syn.agent_transcript.input_tokens_log1p.v1",
        &["usage"],
    ),
    (
        44,
        1921001,
        "syn.agent_transcript.output_tokens_log1p.v1",
        &["usage"],
    ),
    (
        45,
        1921001,
        "syn.agent_transcript.cache_read_log1p.v1",
        &["usage"],
    ),
    (
        46,
        1921001,
        "syn.agent_transcript.cache_creation_log1p.v1",
        &["usage"],
    ),
    (
        47,
        1921001,
        "syn.agent_transcript.record_vector.v1",
        &[
            "content_bytes",
            "content_truncated",
            "line_no",
            "raw_line_bytes",
            "tool_calls",
            "ts_ns",
            "turn_index",
            "usage",
        ],
    ),
    (
        48,
        1776001,
        "syn.action.kind_onehot.v1",
        &["outcome", "phase", "row_kind", "status", "tool", "verb"],
    ),
    (49, 1776001, "syn.action.target_hash.v1", &["pointer"]),
    (
        50,
        1776001,
        "syn.action.params_record_vector.v1",
        &[
            "error",
            "error_code",
            "payload_bytes",
            "payload_truncated",
            "pointer",
            "redacted",
            "schema_version",
            "seq",
            "ts_ns",
        ],
    ),
    (51, 1776001, "syn.action.hour_cyclic.v1", &["ts_ns"]),
    (52, 1776001, "syn.action.dow_cyclic.v1", &["ts_ns"]),
    (53, 1776002, "syn.reflex.reflex_hash.v1", &["reflex_id"]),
    (54, 1776002, "syn.reflex.outcome_onehot.v1", &["status"]),
    (55, 1776002, "syn.reflex.latency_ms_log1p.v1", &["details"]),
    (56, 1776002, "syn.reflex.step_count_log1p.v1", &["steps"]),
    (57, 1776002, "syn.reflex.hour_cyclic.v1", &["ts_ns"]),
    (58, 1776002, "syn.reflex.dow_cyclic.v1", &["ts_ns"]),
    (
        59,
        1776002,
        "syn.reflex.record_vector.v1",
        &[
            "details",
            "error_code",
            "event_id",
            "redacted",
            "schema_version",
            "steps",
            "ts_ns",
        ],
    ),
    (60, 1776003, "syn.process.process_hash.v1", &["pointer"]),
    (
        61,
        1776003,
        "syn.process.event_onehot.v1",
        &["event", "row_kind", "status"],
    ),
    (
        62,
        1776003,
        "syn.process.hour_cyclic.v1",
        &["launched_at_unix_ms", "ts_ns"],
    ),
    (
        63,
        1776003,
        "syn.process.dow_cyclic.v1",
        &["launched_at_unix_ms", "ts_ns"],
    ),
    (
        64,
        1776003,
        "syn.process.uptime_ms_log1p.v1",
        &["duration_ms", "uptime_ms"],
    ),
    (
        65,
        1776003,
        "syn.process.event_time_rank.v1",
        &["launched_at_unix_ms", "ts_ns"],
    ),
    (
        66,
        1776003,
        "syn.process.record_vector.v1",
        &[
            "cdp_debug_port",
            "desktop",
            "duration_ms",
            "hwnd",
            "launched_at_unix_ms",
            "pid",
            "reused_existing_window",
            "schema_version",
            "ts_ns",
            "uptime_ms",
            "window_owner_pid",
        ],
    ),
    (
        67,
        1776004,
        "syn.observation.app_hash.v1",
        &["foreground.process_name"],
    ),
    (
        68,
        1776004,
        "syn.observation.role_histogram.v1",
        &["elements", "focused"],
    ),
    (
        69,
        1776004,
        "syn.observation.entity_multi_hot.v1",
        &["entities"],
    ),
    (
        70,
        1776004,
        "syn.observation.hud_scalars.v1",
        &["hud.by_name", "hud.errors"],
    ),
    (
        71,
        1776004,
        "syn.observation.flags_multi_hot.v1",
        &[
            "diagnostics.a11y_status",
            "diagnostics.audio_status",
            "diagnostics.capture_status",
            "diagnostics.detection_status",
            "diagnostics.elements_truncated",
            "diagnostics.entities_truncated",
            "diagnostics.is_minimized",
            "foreground.is_dwm_composed",
            "foreground.is_fullscreen",
            "mode",
            "redacted",
        ],
    ),
    (72, 1776004, "syn.observation.hour_cyclic.v1", &["ts_ns"]),
    (73, 1776004, "syn.observation.dow_cyclic.v1", &["ts_ns"]),
    (
        74,
        1776004,
        "syn.observation.record_vector.v1",
        &[
            "audio.recent_events",
            "diagnostics.assembled_in_ms",
            "diagnostics.elements_truncated",
            "diagnostics.entities_truncated",
            "diagnostics.size_bytes",
            "diagnostics.size_estimate_tokens",
            "elements",
            "entities",
            "foreground.dpi_scale",
            "foreground.is_dwm_composed",
            "foreground.is_fullscreen",
            "foreground.monitor_index",
            "fs_recent",
            "hud.by_name",
            "hud.errors",
            "recent_events",
            "redacted",
            "schema_version",
            "ts_ns",
        ],
    ),
    (75, 1776005, "syn.outcome.source_cf_onehot.v1", &[]),
    (
        76,
        1776005,
        "syn.outcome.event_onehot.v1",
        &["action", "event", "kind"],
    ),
    (
        77,
        1776005,
        "syn.outcome.status_onehot.v1",
        &[
            "after_status",
            "code_count",
            "decision",
            "lifecycle",
            "matched",
            "outcome",
            "status",
        ],
    ),
    (
        78,
        1776005,
        "syn.outcome.target_hash.v1",
        &[
            "approval_id",
            "audit_id",
            "episode_id",
            "escalation_id",
            "event_id",
            "pointer",
            "routine_id",
            "session_id",
            "source",
            "spawn_id",
            "target",
        ],
    ),
    (
        79,
        1776005,
        "syn.outcome.hour_cyclic.v1",
        &[
            "at_unix_ms",
            "bound_at_unix_ms",
            "created_at_unix_ms",
            "read_at_unix_ms",
            "ts_ns",
            "updated_at_unix_ms",
        ],
    ),
    (
        80,
        1776005,
        "syn.outcome.dow_cyclic.v1",
        &[
            "at_unix_ms",
            "bound_at_unix_ms",
            "created_at_unix_ms",
            "read_at_unix_ms",
            "ts_ns",
            "updated_at_unix_ms",
        ],
    ),
    (
        81,
        1776005,
        "syn.outcome.record_vector.v1",
        &[
            "action",
            "after_status",
            "approval_id",
            "at_unix_ms",
            "audit_id",
            "bound_at_unix_ms",
            "code_count",
            "created_at_unix_ms",
            "decision",
            "episode_id",
            "escalation_id",
            "event",
            "event_id",
            "kind",
            "ladder_index",
            "lifecycle",
            "matched",
            "outcome",
            "pointer",
            "read_at_unix_ms",
            "routine_id",
            "session_id",
            "source",
            "spawn_id",
            "status",
            "target",
            "ts_ns",
            "updated_at_unix_ms",
        ],
    ),
    (82, 1776006, "syn.mcp_usage.tool_onehot.v1", &["tool"]),
    (
        83,
        1776006,
        "syn.mcp_usage.operation_onehot.v1",
        &["operation"],
    ),
    (84, 1776006, "syn.mcp_usage.route_hash.v1", &["route_id"]),
    (
        85,
        1776006,
        "syn.mcp_usage.param_shape_hash.v1",
        &["argument_shape_sha256"],
    ),
    (86, 1776006, "syn.mcp_usage.status_onehot.v1", &["status"]),
    (
        87,
        1776006,
        "syn.mcp_usage.error_onehot.v1",
        &["error_type"],
    ),
    (88, 1776006, "syn.mcp_usage.profile_hash.v1", &["profile"]),
    (
        89,
        1776006,
        "syn.mcp_usage.tool_surface_hash.v1",
        &["tool_surface_sha256"],
    ),
    (
        90,
        1776006,
        "syn.mcp_usage.session_sequence_rank.v1",
        &["session_sequence_position"],
    ),
    (
        91,
        1776006,
        "syn.mcp_usage.hour_cyclic.v1",
        &["finished_at_unix_ms", "started_at_unix_ms"],
    ),
    (
        92,
        1776006,
        "syn.mcp_usage.dow_cyclic.v1",
        &["finished_at_unix_ms", "started_at_unix_ms"],
    ),
    (
        93,
        1776006,
        "syn.mcp_usage.record_vector.v1",
        &[
            "argument_nested_path_count",
            "argument_top_level_key_count",
            "duration_ms",
            "error_type",
            "finished_at_unix_ms",
            "mcp_session_id_sha256",
            "operation",
            "profile",
            "response_content_count",
            "response_size_bytes",
            "route_id",
            "schema_version",
            "seq",
            "session_sequence_position",
            "steering_emitted",
            "tool",
            "tool_surface_sha256",
        ],
    ),
    (94, 1776007, "syn.recurrence_subject.kind_onehot.v1", &[]),
    (95, 1776007, "syn.recurrence_subject.identity_hash.v1", &[]),
    (96, 1685001, "syn.graphpos.signature.v1", &[]),
    (97, 1685001, "syn.graphpos.neighbor_histogram.v1", &[]),
    (98, 1685002, "syn.graphpos.signature.v1", &[]),
    (99, 1685002, "syn.graphpos.neighbor_histogram.v1", &[]),
    (100, 1685003, "syn.path_hierarchy.signature.v1", &[]),
    (101, 1685003, "syn.path_hierarchy.ancestor_set.v1", &[]),
    (102, 1685003, "syn.path_hierarchy.path_hash.v1", &[]),
];

pub const SYN_ANCHOR_DETERMINING_FIELDS: &[(&str, u32, &[&str])] = &[
    // `record.status`, and `error_type` which is `Some` exactly when the call
    // failed (`mcp_usage.rs`, `finish_tool_call`).
    (
        "synapse:mcp_tool_call_outcome",
        1776006,
        &["status", "error_type"],
    ),
    // NOTE (#1962): there is deliberately no entry for panel 1900001
    // (`syn-timeline-v1`). It used to carry
    // `("synapse:mcp_tool_call_outcome", 1900001, &[])`, which contradicted the
    // panel catalog's own `outcome_bearing: false` declaration for that panel —
    // a timeline row records that something was *seen*, not how it turned out,
    // and an MCP tool call's outcome is not a property of a focus change. The
    // declaration was an unfulfilled intent that made a correctly unanchored
    // panel look like a panel with a missing write. `validate_panel_slot_
    // declarations` now refuses any anchor declared on a panel the catalog
    // declares observation-shaped, so this cannot silently reappear.
    // `policy.enabled` on a policy snapshot row. No mcp-usage lens reads it.
    ("synapse:mcp_steering_enabled", 1776006, &[]),
    // `state` on a promotion-ledger row. No mcp-usage lens reads it.
    ("synapse:mcp_default_promotion_state", 1776006, &[]),
    // `!tool_call_error_present(record)` — `agent_events.rs` reads all three of
    // these, so all three determine the anchor.
    (
        "synapse:agent_tool_call_success",
        1665001,
        &["attributes.error_type", "end_state", "payload"],
    ),
    // The spawn's terminal outcome, which `record.end_state` records.
    ("synapse:agent_end_state", 1665001, &["end_state"]),
    // Written onto transcript rows from the spawn's outcome; no transcript field
    // determines it.
    ("synapse:agent_end_state", 1921001, &[]),
    // `episode_segment_outcome` is a function of exactly these three.
    (
        "synapse:episode_segmentation_outcome",
        1904002,
        &["interruption_count", "interrupted_ms", "ended_because"],
    ),
    // `readback.code_count > 0` on the audit row.
    ("synapse:verification_outcome", 1776005, &["code_count"]),
    // `approval_anchor_value(audit.after_status)`.
    ("synapse:approval_decision", 1776005, &["after_status"]),
    // The escalation `event` recorded on the audit row.
    ("synapse:escalation_event", 1776005, &["event"]),
    // `routine_transition_anchor_value(action)`, which the state row records as
    // its lifecycle.
    (
        "synapse:routine_transition",
        1776005,
        &["lifecycle", "action"],
    ),
];

/// One lens whose declared source fields intersect an anchor's determining set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchorSourceCarrier {
    /// Physical panel slot.
    pub slot: u16,
    /// The slot's declared lens name.
    pub lens: &'static str,
    /// The fields it shares with the anchor. Never empty.
    pub shared_fields: Vec<&'static str>,
}

/// The structural verdict for one (anchor, panel) measurement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchorSourceProvenance {
    /// Whether this (anchor kind, panel version) pair is declared at all.
    ///
    /// `false` means the check did not run — not that it ran and found nothing.
    pub anchor_declared: bool,
    /// The anchor's declared determining fields, empty when undeclared.
    pub anchor_fields: &'static [&'static str],
    /// Slots carrying the label, ordered by slot id.
    pub carriers: Vec<AnchorSourceCarrier>,
}

/// Returns the declared source fields for one built-in slot.
#[must_use]
pub fn syn_slot_source_fields(slot: u16) -> Option<&'static [&'static str]> {
    SYN_SLOT_SOURCE_FIELDS
        .iter()
        .find(|(candidate, _, _, _)| *candidate == slot)
        .map(|(_, _, _, fields)| *fields)
}

/// Returns the declared lens name for one built-in slot.
#[must_use]
pub fn syn_slot_declared_lens(slot: u16) -> Option<&'static str> {
    SYN_SLOT_SOURCE_FIELDS
        .iter()
        .find(|(candidate, _, _, _)| *candidate == slot)
        .map(|(_, _, lens, _)| *lens)
}

/// Returns every slot the named panel publishes, in slot order.
#[must_use]
pub fn syn_panel_slots(panel_version: u32) -> Vec<u16> {
    let mut slots: Vec<u16> = SYN_SLOT_SOURCE_FIELDS
        .iter()
        .filter(|(_, version, _, _)| *version == panel_version)
        .map(|(slot, _, _, _)| *slot)
        .collect();
    slots.sort_unstable();
    slots
}

/// Returns the fields declared to determine one anchor on one panel.
#[must_use]
pub fn syn_anchor_determining_fields(
    anchor_kind: &str,
    panel_version: u32,
) -> Option<&'static [&'static str]> {
    SYN_ANCHOR_DETERMINING_FIELDS
        .iter()
        .find(|(kind, version, _)| *kind == anchor_kind && *version == panel_version)
        .map(|(_, _, fields)| *fields)
}

/// Names every slot the panel publishes, minus `excluded`, whose declared source
/// fields intersect the anchor's declared determining fields.
///
/// ## Why the panel's own slot set, and not the slots that happen to have data
///
/// The first version of this took the slots the assay had gathered samples for.
/// That made the verdict a function of the corpus: a panel whose carrier slot
/// had no anchored rows yet came back clean, and would have started refusing
/// once rows arrived. A leakage check that switches on with traffic is not a
/// check. It is now a pure function of `(anchor_kind, panel_version, excluded)`,
/// so it returns the same answer on an empty vault and a full one.
///
/// Deterministic by construction: a set intersection over two static tables, so
/// it is independent of sample size and estimator. That is the whole difference
/// between this and `calyx_assay::sufficiency::detect_anchor_leakage`, which can
/// only reach the degenerate case where a lens *is* the label.
#[must_use]
pub fn syn_anchor_source_provenance(
    anchor_kind: &str,
    panel_version: u32,
    excluded: &BTreeSet<u16>,
) -> AnchorSourceProvenance {
    let Some(anchor_fields) = syn_anchor_determining_fields(anchor_kind, panel_version) else {
        return AnchorSourceProvenance {
            anchor_declared: false,
            anchor_fields: &[],
            carriers: Vec::new(),
        };
    };
    let mut carriers = Vec::new();
    for (slot, version, lens, fields) in SYN_SLOT_SOURCE_FIELDS {
        if *version != panel_version || excluded.contains(slot) {
            continue;
        }
        let shared: Vec<&'static str> = fields
            .iter()
            .filter(|field| anchor_fields.contains(*field))
            .copied()
            .collect();
        if shared.is_empty() {
            continue;
        }
        carriers.push(AnchorSourceCarrier {
            slot: *slot,
            lens,
            shared_fields: shared,
        });
    }
    carriers.sort_by_key(|carrier| carrier.slot);
    AnchorSourceProvenance {
        anchor_declared: true,
        anchor_fields,
        carriers,
    }
}
