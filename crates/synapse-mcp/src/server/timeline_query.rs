//! `timeline_get` and `timeline_stats` MCP tools (#842).
//!
//! Read-only companions to `timeline_search`/`timeline_purge` (#841/#843): a
//! raw ordered slice retrieval for the dashboard day-view and agents, and a
//! recorder/storage status report. They live in their own router (wired in
//! `server.rs`) rather than the `m3_tool_router` so this surface can land
//! without editing the shared M3 tool table — the read logic itself reuses the
//! single CF_TIMELINE scan implementation in [`crate::m3::timeline`].
//!
//! Both gate on the same `ReadStorage` M3 permission as `timeline_search` and
//! derive every number from the authoritative `CF_TIMELINE` rows + the live
//! recorder control gate, never a parallel cache.

use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};
use crate::m3::episodes::{
    EpisodeGetParams, EpisodeGetResponse, EpisodeListParams, EpisodeListResponse, get_episode,
    list_episodes, required_permissions_get as episode_required_permissions_get,
    required_permissions_list as episode_required_permissions_list,
};
use crate::m3::routines::{
    RoutineInspectParams, RoutineInspectResponse, RoutineListParams, RoutineListResponse,
    RoutineUpdateParams, RoutineUpdateResponse, inspect_routine, list_routines,
    required_permissions_inspect as routine_required_permissions_inspect,
    required_permissions_list as routine_required_permissions_list,
    required_permissions_update as routine_required_permissions_update, update_routine,
};
use crate::m3::storage::{
    StorageGcOnceParams, StorageGcOnceResponse, required_permissions_gc, run_storage_gc_once,
};
use crate::m3::timeline::{
    RecorderStatus, TimelineGetParams, TimelineGetResponse, TimelinePurgeParams,
    TimelinePurgeResponse, TimelineSearchParams, TimelineSearchResponse, TimelineStatsParams,
    TimelineStatsResponse, get_timeline, purge_timeline,
    required_permissions as timeline_required_permissions_search,
    required_permissions_get as timeline_required_permissions_get, required_permissions_purge,
    required_permissions_stats, search_timeline, timeline_stats_data,
};
use crate::m3::timeline_control::{
    TimelinePauseParams, TimelinePauseResponse, TimelineResumeParams, TimelineResumeResponse,
    pause_timeline, recorder_control_handle, required_permissions_pause,
    required_permissions_resume, resume_timeline,
};
use crate::server::timeline_digest::{
    TimelineDigestParams, TimelineDigestResponse, build_digest,
    required_permissions as timeline_digest_required_permissions,
};

#[tool_router(router = timeline_query_tool_router, vis = "pub(super)")]
impl SynapseService {
    pub(crate) fn timeline_stats_snapshot(&self) -> Result<TimelineStatsResponse, ErrorData> {
        let params = TimelineStatsParams::default();
        self.require_m3_permissions("timeline_stats", &required_permissions_stats(&params))?;
        let control = recorder_control_handle(&self.m3_state_handle())?;
        let recorder = RecorderStatus::from_control(&control);
        let runtime = self.reflex_runtime()?;
        timeline_stats_data(&runtime, recorder, &params)
    }

    pub(crate) fn dashboard_timeline_get(
        &self,
        params: TimelineGetParams,
    ) -> Result<TimelineGetResponse, ErrorData> {
        self.require_m3_permissions("timeline_get", &timeline_required_permissions_get(&params))?;
        let runtime = self.reflex_runtime()?;
        get_timeline(&runtime, &params)
    }

    pub(crate) fn dashboard_timeline_search(
        &self,
        params: TimelineSearchParams,
    ) -> Result<TimelineSearchResponse, ErrorData> {
        self.require_m3_permissions(
            "timeline_search",
            &timeline_required_permissions_search(&params),
        )?;
        let runtime = self.reflex_runtime()?;
        search_timeline(&runtime, &params)
    }

    pub(crate) fn dashboard_timeline_digest(
        &self,
        params: TimelineDigestParams,
    ) -> Result<TimelineDigestResponse, ErrorData> {
        self.require_m3_permissions(
            "timeline_digest",
            &timeline_digest_required_permissions(&params),
        )?;
        let runtime = self.reflex_runtime()?;
        build_digest(&runtime, &params)
    }

    pub(crate) fn dashboard_episode_list(
        &self,
        params: EpisodeListParams,
    ) -> Result<EpisodeListResponse, ErrorData> {
        self.require_m3_permissions("episode_list", &episode_required_permissions_list(&params))?;
        let runtime = self.reflex_runtime()?;
        list_episodes(&runtime, &params)
    }

    pub(crate) fn dashboard_episode_get(
        &self,
        params: EpisodeGetParams,
    ) -> Result<EpisodeGetResponse, ErrorData> {
        self.require_m3_permissions("episode_get", &episode_required_permissions_get(&params))?;
        let runtime = self.reflex_runtime()?;
        get_episode(&runtime, &params)
    }

    pub(crate) fn dashboard_routine_list(
        &self,
        params: RoutineListParams,
    ) -> Result<RoutineListResponse, ErrorData> {
        self.require_m3_permissions("routine_list", &routine_required_permissions_list(&params))?;
        let db = self.m3_storage()?;
        list_routines(&db, &params)
    }

    pub(crate) fn dashboard_routine_inspect(
        &self,
        params: RoutineInspectParams,
    ) -> Result<RoutineInspectResponse, ErrorData> {
        self.require_m3_permissions(
            "routine_inspect",
            &routine_required_permissions_inspect(&params),
        )?;
        let db = self.m3_storage()?;
        inspect_routine(&db, &params)
    }

    pub(crate) fn dashboard_routine_update(
        &self,
        params: RoutineUpdateParams,
    ) -> Result<RoutineUpdateResponse, ErrorData> {
        self.require_m3_permissions(
            "routine_update",
            &routine_required_permissions_update(&params),
        )?;
        let db = self.m3_storage()?;
        update_routine(&db, &params, "dashboard")
    }

    pub(crate) fn dashboard_timeline_pause(
        &self,
        params: TimelinePauseParams,
    ) -> Result<TimelinePauseResponse, ErrorData> {
        self.require_m3_permissions("timeline_pause", &required_permissions_pause(&params))?;
        pause_timeline(&self.m3_state_handle(), &params, "dashboard")
    }

    pub(crate) fn dashboard_timeline_resume(
        &self,
        params: TimelineResumeParams,
    ) -> Result<TimelineResumeResponse, ErrorData> {
        self.require_m3_permissions("timeline_resume", &required_permissions_resume(&params))?;
        resume_timeline(&self.m3_state_handle(), &params, "dashboard")
    }

    /// Hard-deletes operator timeline rows from the dashboard storage manager.
    ///
    /// Reuses the exact [`purge_timeline`] tool logic (same filter machinery,
    /// scan budget, hard-delete + range compaction, and counts-only audit row)
    /// so the dashboard can never diverge from the `timeline_purge` MCP tool.
    /// The audit row records `dashboard` as the actor.
    pub(crate) fn dashboard_timeline_purge(
        &self,
        params: TimelinePurgeParams,
    ) -> Result<TimelinePurgeResponse, ErrorData> {
        self.require_m3_permissions("timeline_purge", &required_permissions_purge(&params))?;
        let runtime = self.reflex_runtime()?;
        purge_timeline(&runtime, &params, "dashboard")
    }

    /// Runs one row-cap storage GC pass from the dashboard storage manager.
    ///
    /// Reuses the [`run_storage_gc_once`] tool logic: when `before_rows >
    /// soft_cap_rows` the oldest rows in the column family are evicted down to
    /// `soft_cap_rows` (keep-newest-N), with the same `AUDIT_RETENTION` age mode
    /// the MCP tool exposes. Returns exact before/after row counts.
    pub(crate) fn dashboard_storage_gc(
        &self,
        params: StorageGcOnceParams,
    ) -> Result<StorageGcOnceResponse, ErrorData> {
        self.require_m3_permissions("storage_gc_once", &required_permissions_gc(&params))?;
        let runtime = self.reflex_runtime()?;
        run_storage_gc_once(&runtime, &params)
    }

    #[tool(
        description = "Retrieve raw operator timeline rows (CF_TIMELINE) in ascending time order for a time range and optional kinds/actor — the day-view feed for the dashboard and agents. Pages via an opaque cursor that is the physical storage key, so paging is stable under concurrent writes. Read-only; no text/app search (use timeline_search for that)."
    )]
    pub async fn timeline_get(
        &self,
        params: Parameters<TimelineGetParams>,
    ) -> Result<Json<TimelineGetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_get",
            start_ts_ns = params.0.start_ts_ns,
            end_ts_ns = params.0.end_ts_ns,
            limit = params.0.limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=timeline_get"
        );
        self.require_m3_permissions(
            "timeline_get",
            &timeline_required_permissions_get(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        get_timeline(&runtime, &params.0).map(Json)
    }

    #[tool(
        description = "Report operator timeline recorder + storage status: recorder paused/feed/exclusion state (read from the same control gate the recorder consults), exact CF_TIMELINE row counts by kind and by UTC day, oldest/newest row timestamps, and on-disk footprint, over an optional time window. Counts are derived by a budget-guarded scan; scan_complete is false (never silently) if the budget paused before the whole window was read."
    )]
    pub async fn timeline_stats(
        &self,
        params: Parameters<TimelineStatsParams>,
    ) -> Result<Json<TimelineStatsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_stats",
            start_ts_ns = params.0.start_ts_ns,
            end_ts_ns = params.0.end_ts_ns,
            "tool.invocation kind=timeline_stats"
        );
        self.require_m3_permissions("timeline_stats", &required_permissions_stats(&params.0))?;
        // Recorder state is read from the shared control gate (the exact gate the
        // recorder write-path consults) plus the feed-enable config, so the
        // reported pause/feed/exclusion state can never diverge from reality.
        let control = recorder_control_handle(&self.m3_state_handle())?;
        let recorder = RecorderStatus::from_control(&control);
        let runtime = self.reflex_runtime()?;
        timeline_stats_data(&runtime, recorder, &params.0).map(Json)
    }
}
