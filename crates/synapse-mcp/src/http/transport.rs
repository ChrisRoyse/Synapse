use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt::Write as _,
    io,
    net::SocketAddr,
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::{SessionState, SessionStore, SessionStoreError, local::LocalSessionManager},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
#[cfg(test)]
use synapse_action::ActionHandle;
use synapse_action::ActionStateSnapshot;
use synapse_core::{EventFilter, EventSource, Health};
use synapse_storage::{Db, cf};
use tokio::{
    net::TcpListener,
    sync::{broadcast, watch},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;

use crate::{
    http::auth::{self, HttpAuth},
    http::session,
    http::sse::{self, SseState},
    m2::M2ServiceConfig,
    m3::M3ServiceConfig,
    m4::M4ServiceConfig,
    server::{
        SynapseService,
        terminal_capture::capture::{
            LiveTerminalSession, TerminalCaptureEvent, TerminalCaptureEventKind,
            TerminalCaptureStatus, terminal_capture_session,
        },
    },
};

type McpHttpService = StreamableHttpService<SynapseService, LocalSessionManager>;
const STALE_SESSION_INPUT_CLEANUP_INTERVAL: Duration = Duration::from_millis(250);
const M2_EMITTER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_RESPONSE_GRACE_TIMEOUT: Duration = Duration::from_secs(2);
const DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES: usize = 256 * 1024;
const DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES: usize = 64 * 1024;
const DASHBOARD_CONTEXT_BODY_LIMIT_BYTES: usize = 256 * 1024;
const DASHBOARD_SPAWN_FAN_OUT_MAX: u32 = 5;
const DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS: u64 = 3_000;

#[derive(Clone)]
struct HttpState {
    bind_addr: SocketAddr,
    health_service: Arc<SynapseService>,
    session_manager: Arc<LocalSessionManager>,
    shutdown_cancel: CancellationToken,
    drain_state: crate::server::drain::DaemonDrainState,
    sse_state: SseState,
    /// Journal handle for the push-telemetry ingress (#899); the same DB the
    /// MCP session store writes through.
    agent_events_db: Arc<Db>,
}

struct HttpRouterRuntime {
    app: Router,
    session_manager: Arc<LocalSessionManager>,
    session_lifecycle: crate::server::session_lifecycle::SessionLifecycleState,
    drain_state: crate::server::drain::DaemonDrainState,
}

#[derive(Debug, Serialize)]
struct DaemonShutdownInputCleanupReport {
    reason: &'static str,
    active_sessions_before: usize,
    live_spawn_sessions_before: usize,
    shutdown_sessions_before: usize,
    cleaned_sessions: usize,
    orphan_lease_owner_cleanup:
        Option<crate::server::session_lifecycle::SessionShutdownInputCleanupReport>,
    final_lease_held: bool,
    final_lease_owner_session_id: Option<String>,
    final_lease_is_operator: bool,
    lease_still_held_after_cleanup: bool,
    failure_count: usize,
    session_reports: Vec<crate::server::session_lifecycle::SessionShutdownInputCleanupReport>,
}

pub(super) async fn serve(
    bind: &str,
    allow_non_loopback: bool,
    m2_config: &M2ServiceConfig,
    m3_config: M3ServiceConfig,
    m4_config: M4ServiceConfig,
) -> anyhow::Result<ExitCode> {
    synapse_action::install_panic_hook();

    // Validate the bind address first — a pure argument check with no side
    // effects. Doing this before acquiring the single-instance lock means a
    // misconfigured non-loopback bind always fails with HTTP_BIND_NON_LOOPBACK_
    // REFUSED (exit 2), even when another daemon already holds the DB lock
    // (which would otherwise short-circuit to exit 3 and mask the real problem).
    let addr = bind
        .parse::<SocketAddr>()
        .with_context(|| format!("parse HTTP bind address {bind}"))?;
    if !addr.ip().is_loopback() && !allow_non_loopback {
        tracing::error!(
            code = synapse_core::error_codes::HTTP_BIND_NON_LOOPBACK_REFUSED,
            bind = %addr,
            "refusing non-loopback HTTP bind without --allow-non-loopback"
        );
        return Ok(ExitCode::from(2));
    }

    // Single-instance guard: at most one daemon may own a given RocksDB path.
    // Acquired before binding the port or opening storage so a duplicate launch
    // fails fast with a clear, holder-naming error instead of a cryptic RocksDB
    // LOCK failure surfacing later inside a tool call.
    let db_path = m3_config
        .db_path
        .clone()
        .unwrap_or_else(crate::m3::default_db_path);
    let _single_instance = match crate::single_instance::SingleInstanceGuard::acquire(&db_path) {
        Ok(guard) => {
            tracing::info!(
                code = "MCP_DAEMON_SINGLE_INSTANCE_ACQUIRED",
                lock_path = %guard.lock_path().display(),
                db_path = %db_path.display(),
                pid = std::process::id(),
                "daemon single-instance lock acquired"
            );
            guard
        }
        Err(crate::single_instance::SingleInstanceError::AlreadyRunning {
            lock_path,
            holder_pid,
        }) => {
            tracing::error!(
                code = "MCP_DAEMON_ALREADY_RUNNING",
                lock_path = %lock_path.display(),
                holder_pid = holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                db_path = %db_path.display(),
                "refusing to start: another synapse-mcp daemon already owns this DB path"
            );
            return Ok(ExitCode::from(3));
        }
        Err(err @ crate::single_instance::SingleInstanceError::Io { .. }) => {
            return Err(anyhow::Error::new(err)).context("acquire daemon single-instance lock");
        }
    };

    let lifecycle_paths =
        crate::daemon_lifecycle::configure(crate::daemon_lifecycle::DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some(addr.to_string()),
            db_path: db_path.clone(),
        })
        .context("configure daemon lifecycle ledger")?;
    crate::daemon_lifecycle::install_panic_hook();
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_READY",
        run_current_path = %lifecycle_paths.run_current_path,
        tool_last_path = %lifecycle_paths.tool_last_path,
        tool_events_path = %lifecycle_paths.tool_events_path,
        exit_events_path = %lifecycle_paths.exit_events_path,
        "daemon lifecycle ledger ready"
    );

    if !addr.ip().is_loopback() {
        tracing::warn!(
            code = "MCP_HTTP_NON_LOOPBACK_BIND_ALLOWED",
            bind = %addr,
            "non-loopback HTTP bind allowed by explicit operator flag"
        );
    }
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP MCP transport to {addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("read HTTP listener address")?;
    let shutdown_cancel = CancellationToken::new();
    let connection_closed_cancel = CancellationToken::new();
    let sse_state = SseState::with_max_subscriptions(m3_config.max_subscriptions);
    let service = http_service(
        shutdown_cancel.clone(),
        connection_closed_cancel.clone(),
        sse_state.clone(),
        m2_config,
        m3_config,
        m4_config,
    )
    .context("initialize shared HTTP service state")?;

    // Eager storage open: validate RocksDB at startup rather than lazily on the
    // first reflex tool call, so a lock/schema fault fails fast with a clear
    // error and the daemon refuses to start half-broken (instead of every tool
    // call failing later). The handle is cached and reused by the reflex
    // runtime, so there is no open-then-reopen race.
    {
        let m3_handle = service.m3_state_handle();
        let open_result = {
            let mut state = m3_handle.lock().map_err(|_poisoned| {
                anyhow::anyhow!("m3 service state lock poisoned during startup storage open")
            })?;
            state.ensure_storage()
        };
        if let Err(error) = open_result {
            let detail = error.to_string();
            if detail.to_lowercase().contains("lock") {
                tracing::error!(
                    code = "STORAGE_LOCK_CONTENDED",
                    db_path = %db_path.display(),
                    detail = %detail,
                    "refusing to start: RocksDB storage lock is held by another process; run `synapse-mcp doctor` to find and stop the holder, or point this daemon at a different --db path"
                );
            } else {
                tracing::error!(
                    code = "STORAGE_OPEN_FAILED",
                    db_path = %db_path.display(),
                    detail = %detail,
                    "refusing to start: storage open failed at daemon startup"
                );
            }
            crate::daemon_lifecycle::record_startup_exit(
                "startup_storage_open_failed",
                serde_json::json!({
                    "db_path": db_path.display().to_string(),
                    "detail": detail,
                }),
            )
            .context("record daemon lifecycle startup storage-open failure")?;
            return Ok(ExitCode::from(4));
        }
        tracing::info!(
            code = "MCP_DAEMON_STORAGE_OPENED",
            db_path = %db_path.display(),
            "daemon storage opened eagerly at startup"
        );
    }

    // Always-on activity recorder (#837): started eagerly so the operator
    // timeline records whenever the daemon runs, before any tool call can
    // lazily start a recorder-less WinEvent bridge. A recorder that cannot
    // record is a startup failure, not a degraded mode.
    let m3_state_for_recorder = service.m3_state_handle();
    {
        let recorder_result = {
            let mut state = m3_state_for_recorder.lock().map_err(|_poisoned| {
                anyhow::anyhow!("m3 service state lock poisoned during activity recorder startup")
            })?;
            state.ensure_activity_recorder(sse_state.event_bus())
        };
        if let Err(error) = recorder_result {
            let detail = format!("{error:#}");
            tracing::error!(
                code = "TIMELINE_RECORDER_START_FAILED",
                db_path = %db_path.display(),
                detail = %detail,
                "refusing to start: activity recorder failed at daemon startup"
            );
            crate::daemon_lifecycle::record_startup_exit(
                "startup_activity_recorder_failed",
                serde_json::json!({
                    "db_path": db_path.display().to_string(),
                    "detail": detail,
                }),
            )
            .context("record daemon lifecycle startup activity-recorder failure")?;
            return Ok(ExitCode::from(4));
        }
        tracing::info!(
            code = "MCP_DAEMON_ACTIVITY_RECORDER_STARTED",
            "activity recorder started eagerly at startup"
        );
    }

    // Periodic routine miner (#848): keeps CF_ROUTINES tracking the episode
    // store without manual routine_mine calls. A misconfigured schedule is a
    // startup failure, not a silently substituted default.
    let _routine_miner_task = crate::m3::routine_miner_job::spawn_periodic_miner(
        service.m3_state_handle(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic routine miner")?;

    // Periodic intent detector (#855): re-evaluates the live intent snapshot on
    // a fixed interval and publishes intent-detected/confirmed/abandoned events
    // on the shared bus for reflexes and the suggestion engine. A misconfigured
    // schedule or detection floor is a startup failure, not a silent default.
    let _intent_detector_task = crate::m3::intent_events::spawn_intent_detector(
        service.m3_state_handle(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic intent detector")?;

    // Periodic armed routine runner (#862): evaluates persisted arm rows and
    // executes due installed automations through the same background-first
    // plan executor as manual suggestion acceptance. Misconfigured schedules
    // fail startup rather than silently substituting defaults.
    let _armed_routine_task = crate::server::suggestions::spawn_periodic_armed_routine_runner(
        service.clone(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic armed routine runner")?;

    // Periodic transcript ingester (#900): tails spawned-agent stdout JSONL
    // into CF_AGENT_TRANSCRIPTS. Same contract as the miner: a misconfigured
    // schedule is a startup failure, not a silently substituted default.
    let _transcript_ingest_task =
        crate::server::agent_transcripts::spawn_periodic_transcript_ingest(
            service.m3_state_handle(),
            shutdown_cancel.clone(),
        )
        .context("spawn periodic transcript ingester")?;

    // Ambient agent discovery (#fleet-ambient): tails the persisted Claude
    // session transcripts under ~/.claude/projects so agents the operator
    // launched themselves (not via act_spawn_agent) are registered into the
    // same journal → state-machine → dashboard read path. A misconfigured
    // schedule or an unresolvable home anchor is a startup failure, never a
    // watcher that silently observes nothing.
    let _ambient_ingest_task = crate::server::ambient_agents::spawn_periodic_ambient_ingest(
        service.m3_state_handle(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic ambient agent discovery")?;

    let _operator_hotkey_guard = crate::safety::install_operator_hotkey(service.clone())
        .context("install operator panic hotkey")?;
    let m2_emitter_done = service.m2_emitter_done_receiver();
    let runtime = router(&shutdown_cancel, local_addr, sse_state, service)
        .context("build HTTP MCP router")?;

    tracing::info!(
        code = "MCP_HTTP_STARTED",
        bind = %local_addr,
        "starting streamable HTTP MCP transport"
    );

    let mut server_task = spawn_server(listener, runtime.app, shutdown_cancel.clone());
    let m2_done_after_server_stop = m2_emitter_done.clone();
    let m2_done_after_signal = m2_emitter_done;
    let code = tokio::select! {
        result = &mut server_task => {
            result.context("join HTTP MCP transport")?
                .context("serve HTTP MCP transport")?;
            if shutdown_cancel.is_cancelled() {
                connection_closed_cancel.cancel();
                let cleanup = cleanup_active_session_inputs_for_shutdown(
                    &runtime.session_lifecycle,
                    &runtime.session_manager,
                    "http_endpoint",
                ).await;
                tracing::info!(
                    code = "MCP_HTTP_SHUTDOWN_INPUT_CLEANUP",
                    cleanup = ?cleanup,
                    "readback=session_input_ownership edge=http_endpoint_shutdown after_cleanup"
                );
                wait_for_m2_emitter_done(m2_done_after_server_stop, "http_endpoint").await;
            }
            ExitCode::SUCCESS
        }
        signal = wait_for_shutdown_signal("http") => {
            signal?;
            let drain = runtime.drain_state.mark_draining("signal");
            tracing::info!(code = "MCP_SHUTDOWN_GRACEFUL", "HTTP shutdown signal received");
            time::sleep(DRAIN_RESPONSE_GRACE_TIMEOUT).await;
            shutdown_cancel.cancel();
            connection_closed_cancel.cancel();
            wait_for_server_stop(&mut server_task).await?;
            let cleanup = cleanup_active_session_inputs_for_shutdown(
                &runtime.session_lifecycle,
                &runtime.session_manager,
                "signal",
            ).await;
            tracing::info!(
                code = "MCP_HTTP_SHUTDOWN_INPUT_CLEANUP",
                drain = ?drain,
                cleanup = ?cleanup,
                "readback=session_input_ownership edge=signal_shutdown after_cleanup"
            );
            wait_for_m2_emitter_done(m2_done_after_signal, "signal").await;
            ExitCode::SUCCESS
        }
    };
    // Stop the activity recorder last so the timeline closes with a
    // session_end row covering the whole daemon lifetime (#837).
    let recorder = m3_state_for_recorder
        .lock()
        .ok()
        .and_then(|mut state| state.activity_recorder.take());
    if let Some(recorder) = recorder {
        recorder.shutdown().await;
        let (rows_written, write_failures) = recorder.readback();
        tracing::info!(
            code = "MCP_DAEMON_ACTIVITY_RECORDER_STOPPED",
            rows_written,
            write_failures,
            "activity recorder stopped at daemon shutdown"
        );
    }
    crate::daemon_lifecycle::record_graceful_exit("http_service_completed")
        .context("record daemon lifecycle graceful HTTP service completion")?;
    Ok(code)
}

fn router(
    shutdown_cancel: &CancellationToken,
    bind_addr: SocketAddr,
    sse_state: SseState,
    service: SynapseService,
) -> anyhow::Result<HttpRouterRuntime> {
    let auth = Arc::new(HttpAuth::load(bind_addr).context("load HTTP bearer token")?);
    tracing::info!(
        code = "MCP_HTTP_AUTH_CONFIGURED",
        source = auth.source_label(),
        "HTTP bearer token configured"
    );
    let health_service = Arc::new(service.clone());
    let drain_state = service.drain_state_handle();
    let session_registry = service.session_registry_handle();
    let terminated_sessions = service.terminated_sessions_handle();
    let session_lifecycle = service
        .session_lifecycle_state()
        .map_err(|error| anyhow::anyhow!("initialize session lifecycle state: {error:?}"))?;
    let (mcp_service, session_manager) = streamable_service(shutdown_cancel, service)
        .context("initialize HTTP MCP session state")?;
    let agent_events_db =
        session_store_db(&health_service).context("open storage for agent-event ingress")?;
    // #898: install the live-event sink, rebuild agent states from the
    // journal, and start the heartbeat/process-probe liveness sweep.
    crate::server::agent_events::install_session_registry_activity_sink(Arc::clone(
        &session_registry,
    ));
    crate::server::agent_state::install_event_bus(sse_state.event_bus());
    let liveness_config = crate::server::agent_state::load_liveness_config()
        .map_err(|detail| anyhow::anyhow!("agent liveness configuration invalid: {detail}"))?;
    crate::server::agent_state::rebuild_from_journal(&agent_events_db)
        .context("rebuild agent state tracker from CF_AGENT_EVENTS")?;
    let _agent_liveness_task = spawn_agent_liveness_sweep(
        Arc::clone(&agent_events_db),
        liveness_config,
        shutdown_cancel.child_token(),
    );
    // #948: start the AFK escalation delivery worker (Tier 0 on-PC toast +
    // operator-supplied Tier 1 webhook ladder). Installs the wake signal that
    // `agent_state::emit_transitions` pulses on each live attention transition.
    let _escalation_worker = crate::server::escalation::spawn_worker(
        Arc::clone(&agent_events_db),
        shutdown_cancel.child_token(),
    );
    let session_request = session::SessionCleanupState::request_state(
        Arc::clone(&session_registry),
        terminated_sessions,
    );
    let session_cleanup =
        session::SessionCleanupState::new(Arc::clone(&session_manager), session_lifecycle.clone());
    let _stale_cleanup_task = spawn_stale_session_input_cleanup(
        Arc::clone(&session_manager),
        session_lifecycle.clone(),
        shutdown_cancel.child_token(),
    );
    let state = HttpState {
        bind_addr,
        health_service,
        session_manager: Arc::clone(&session_manager),
        shutdown_cancel: shutdown_cancel.clone(),
        drain_state: drain_state.clone(),
        sse_state,
        agent_events_db,
    };
    let protected_routes = Router::new()
        .route("/health", get(health))
        .route("/shutdown", post(shutdown))
        .route("/events", get(events).post(publish_event))
        .route("/events/stats", get(event_stats))
        .route(
            "/agent-events",
            post(agent_events_ingest).layer(DefaultBodyLimit::max(
                crate::server::agent_event_ingress::MAX_AGENT_EVENT_INGRESS_BODY_BYTES,
            )),
        )
        .route("/agent-events/stats", get(agent_events_ingress_stats))
        .route(
            "/codex-app-server/request",
            post(codex_app_server_request).layer(DefaultBodyLimit::max(
                crate::server::codex_app_server_bridge::MAX_CODEX_APP_SERVER_REQUEST_BODY_BYTES,
            )),
        )
        .route(
            "/agent-transcripts/stats",
            get(agent_transcripts_ingest_stats),
        )
        .route(
            "/chrome-debugger/native/register",
            post(crate::chrome_debugger_bridge::http_register),
        )
        .route(
            "/chrome-debugger/native/message",
            post(crate::chrome_debugger_bridge::http_message),
        )
        .route(
            "/chrome-debugger/native/next",
            get(crate::chrome_debugger_bridge::http_next),
        )
        .route(
            "/chrome-debugger/native/ws",
            get(crate::chrome_debugger_bridge::http_ws),
        )
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            session_request,
            session::require_mcp_session,
        ))
        .layer(middleware::from_fn_with_state(
            session_cleanup,
            session::release_held_inputs_on_delete,
        ))
        .layer(middleware::from_fn_with_state(
            auth,
            auth::require_http_security,
        ));
    let dashboard_routes = Router::new()
        .route("/dashboard", get(dashboard_index))
        .route("/dashboard/assets/{asset}", get(dashboard_asset))
        .route("/dashboard/state.json", get(dashboard_state))
        .route("/dashboard/tray-state.json", get(dashboard_tray_state))
        .route(
            "/dashboard/events/subscribe",
            post(dashboard_events_subscribe)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route("/dashboard/events", get(dashboard_events))
        .route(
            "/dashboard/agent-terminal/{spawn_id}/ws",
            get(dashboard_agent_terminal_ws),
        )
        .route("/dashboard/audit/query", get(dashboard_audit_query))
        .route(
            "/dashboard/saved-views",
            get(dashboard_saved_views)
                .post(dashboard_saved_view_upsert)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/saved-views/{view_id}",
            delete(dashboard_saved_view_delete),
        )
        .route(
            "/dashboard/local-model-spawn",
            post(dashboard_local_model_spawn).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/spawn-agent",
            post(dashboard_spawn_agent).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/agent-kill",
            post(dashboard_agent_kill)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/tasks/create",
            post(dashboard_task_create).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/tasks/update",
            post(dashboard_task_update).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/tasks/cancel",
            post(dashboard_task_cancel)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/tasks/dispatch-once",
            post(dashboard_task_dispatch_once)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/pause",
            post(dashboard_timeline_pause)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/resume",
            post(dashboard_timeline_resume)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/get",
            post(dashboard_timeline_get)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/search",
            post(dashboard_timeline_search)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/digest",
            post(dashboard_timeline_digest)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/episodes/list",
            post(dashboard_episode_list)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/episodes/get",
            post(dashboard_episode_get)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/routines/list",
            post(dashboard_routine_list)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/routines/inspect",
            post(dashboard_routine_inspect)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/routines/update",
            post(dashboard_routine_update)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/storage/timeline-purge",
            post(dashboard_storage_timeline_purge)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/storage/gc",
            post(dashboard_storage_gc)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/control-lease/force-release",
            post(dashboard_control_lease_force_release)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/control-lease/handoff",
            post(dashboard_control_lease_handoff)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/target-claims/prune",
            post(dashboard_target_claims_prune)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/templates",
            get(dashboard_template_list)
                .post(dashboard_template_upsert)
                .layer(DefaultBodyLimit::max(
                    DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
                )),
        )
        .route(
            "/dashboard/templates/{template_id}",
            delete(dashboard_template_delete),
        )
        .route("/dashboard/models", get(dashboard_model_list))
        .route(
            "/dashboard/api-model/register",
            post(dashboard_api_model_register).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/api-model/update",
            post(dashboard_api_model_update).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/api-model/remove",
            post(dashboard_api_model_remove),
        )
        .route(
            "/dashboard/approval/decide",
            post(dashboard_approval_decide)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/context/inject",
            post(dashboard_context_inject)
                .layer(DefaultBodyLimit::max(DASHBOARD_CONTEXT_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/context/plan",
            post(dashboard_context_plan)
                .layer(DefaultBodyLimit::max(DASHBOARD_CONTEXT_BODY_LIMIT_BYTES)),
        )
        .route("/approval/activate", get(approval_activate));
    let app = Router::new()
        .merge(dashboard_routes)
        .merge(protected_routes)
        .with_state(state);
    Ok(HttpRouterRuntime {
        app,
        session_manager,
        session_lifecycle,
        drain_state,
    })
}

fn streamable_service(
    shutdown_cancel: &CancellationToken,
    service: SynapseService,
) -> anyhow::Result<(McpHttpService, Arc<LocalSessionManager>)> {
    let session_config = session::load_session_config().context("load HTTP session config")?;
    {
        let session_registry = service.session_registry_handle();
        let mut registry = session_registry.lock().map_err(|_poisoned| {
            anyhow::anyhow!("session registry lock poisoned during HTTP session setup")
        })?;
        registry.set_stale_after(session_config.keep_alive);
    }
    let session_store = Arc::new(SynapseMcpSessionStore::new(
        session_store_db(&service)?,
        session_config.keep_alive,
        service.session_registry_handle(),
    ));
    let mut config = StreamableHttpServerConfig::default()
        .with_cancellation_token(shutdown_cancel.child_token());
    config.session_store = Some(session_store);
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config = session_config;
    let session_manager = Arc::new(session_manager);
    let service = StreamableHttpService::new(
        move || Ok(service.clone()),
        Arc::clone(&session_manager),
        config,
    );
    Ok((service, session_manager))
}

fn spawn_stale_session_input_cleanup(
    session_manager: Arc<LocalSessionManager>,
    session_lifecycle: crate::server::session_lifecycle::SessionLifecycleState,
    shutdown_cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(STALE_SESSION_INPUT_CLEANUP_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown_cancel.cancelled() => {
                    tracing::debug!(
                        code = "MCP_HTTP_SESSION_INPUT_STALE_CLEANUP_STOPPED",
                        "stopping stale HTTP session held-input cleanup"
                    );
                    break;
                }
                _ = interval.tick() => {
                    cleanup_stale_session_resources_once(
                        &session_lifecycle,
                        &session_manager,
                    ).await;
                }
            }
        }
    })
}

/// #898 liveness sweep: periodically cross-checks heartbeat silence with the
/// process table so stuck and dead agents surface within one sweep interval.
fn spawn_agent_liveness_sweep(
    agent_events_db: Arc<Db>,
    config: crate::server::agent_state::LivenessConfig,
    shutdown_cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(config.sweep_interval_ms));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        tracing::info!(
            code = "AGENT_LIVENESS_SWEEP_STARTED",
            sweep_interval_ms = config.sweep_interval_ms,
            stuck_after_ms = config.stuck_after_ms,
            runaway_identical_calls = config.runaway_identical_calls,
            "agent liveness sweep running"
        );
        loop {
            tokio::select! {
                _ = shutdown_cancel.cancelled() => {
                    tracing::debug!(
                        code = "AGENT_LIVENESS_SWEEP_STOPPED",
                        "stopping agent liveness sweep"
                    );
                    break;
                }
                _ = interval.tick() => {
                    let db = Arc::clone(&agent_events_db);
                    // Process probes are blocking Win32 calls; keep them off
                    // the async reactor.
                    let sweep = tokio::task::spawn_blocking(move || {
                        crate::server::agent_state::liveness_sweep_once(
                            &db,
                            crate::server::session_registry::unix_time_ms_now(),
                        )
                    })
                    .await;
                    match sweep {
                        Ok(transition_count) if transition_count > 0 => {
                            tracing::info!(
                                code = "AGENT_LIVENESS_SWEEP_TRANSITIONS",
                                transition_count,
                                "liveness sweep emitted state transitions"
                            );
                        }
                        Ok(_quiet) => {}
                        Err(join_error) => {
                            tracing::error!(
                                code = "AGENT_LIVENESS_SWEEP_FAILED",
                                detail = %join_error,
                                "liveness sweep task panicked"
                            );
                        }
                    }
                }
            }
        }
    })
}

async fn cleanup_stale_session_resources_once(
    session_lifecycle: &crate::server::session_lifecycle::SessionLifecycleState,
    session_manager: &LocalSessionManager,
) {
    let active_sessions = active_http_session_ids(session_manager).await;
    session_lifecycle.cleanup_expired_lease_inputs_once().await;
    let stale_sessions = session_lifecycle.stale_session_candidates(&active_sessions);
    for (session_id, reason) in stale_sessions {
        match session_lifecycle
            .teardown_session(&session_id, reason)
            .await
        {
            Ok(report) => {
                tracing::info!(
                    code = "MCP_HTTP_SESSION_STALE_LIFECYCLE_CLEANUP",
                    session_id = %session_id,
                    reason,
                    active_session_count = active_sessions.len(),
                    report = ?report,
                    "readback=session_lifecycle edge=http_session_gone after_cleanup"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    session_id = %session_id,
                    reason,
                    active_session_count = active_sessions.len(),
                    detail = %error.message,
                    data = ?error.data,
                    "HTTP MCP stale-session lifecycle cleanup failed"
                );
            }
        }
    }
}

#[cfg(test)]
async fn cleanup_stale_session_inputs_once(
    action_handle: &ActionHandle,
    session_manager: &LocalSessionManager,
    cdp_target_owners: &crate::server::SharedCdpTargetOwners,
) {
    let active_sessions = active_http_session_ids(session_manager).await;
    cleanup_expired_lease_inputs_once(action_handle).await;
    cleanup_stale_session_cdp_targets_once(cdp_target_owners, &active_sessions).await;

    let snapshot = match action_handle.session_inputs_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(
                code = error.code(),
                detail = %error.detail(),
                "HTTP MCP stale-session cleanup could not read held-input ownership"
            );
            return;
        }
    };
    for session in snapshot.sessions {
        if active_sessions.contains(&session.session_id) {
            continue;
        }
        release_stale_session_inputs_and_lease(action_handle, &session.session_id).await;
    }

    cleanup_stale_session_lease_once(action_handle, &active_sessions).await;
}

#[cfg(test)]
async fn cleanup_stale_session_cdp_targets_once(
    cdp_target_owners: &crate::server::SharedCdpTargetOwners,
    active_sessions: &BTreeSet<String>,
) {
    let stale_sessions = match cdp_target_owners.lock() {
        Ok(owners) => owners
            .values()
            .filter_map(|owner| {
                (!active_sessions.contains(&owner.session_id)).then(|| owner.session_id.clone())
            })
            .collect::<BTreeSet<_>>(),
        Err(_error) => {
            tracing::error!(
                code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "HTTP MCP stale-session cleanup could not lock CDP target ownership registry"
            );
            return;
        }
    };
    for session_id in stale_sessions {
        let (owned_before, target_ids) = match cdp_target_owners.lock() {
            Ok(mut owners) => {
                let stale_owner_keys = owners
                    .iter()
                    .filter_map(|(owner_key, owner)| {
                        (owner.session_id == session_id).then(|| owner_key.clone())
                    })
                    .collect::<Vec<_>>();
                let target_ids = stale_owner_keys
                    .iter()
                    .filter_map(|owner_key| {
                        owners
                            .get(owner_key)
                            .map(|owner| owner.cdp_target_id.clone())
                    })
                    .collect::<Vec<_>>();
                for owner_key in &stale_owner_keys {
                    owners.remove(owner_key);
                }
                (target_ids.len(), target_ids)
            }
            Err(_error) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "HTTP MCP test stale-session cleanup could not lock CDP target ownership registry"
                );
                continue;
            }
        };
        tracing::info!(
            code = "MCP_HTTP_SESSION_CDP_TARGET_STALE_CLEANUP",
            session_id = %session_id,
            active_session_count = active_sessions.len(),
            cdp_cleanup_reason = "http_stale",
            cdp_owned_before = owned_before,
            cdp_closed = 0,
            cdp_failed = 0,
            cdp_target_ids = ?target_ids,
            "readback=cdp_target_ownership edge=http_session_gone after_cleanup"
        );
    }
}

async fn active_http_session_ids(session_manager: &LocalSessionManager) -> BTreeSet<String> {
    session_manager
        .sessions
        .read()
        .await
        .keys()
        .map(|session_id| session_id.as_ref().to_owned())
        .collect()
}

async fn cleanup_active_session_inputs_for_shutdown(
    session_lifecycle: &crate::server::session_lifecycle::SessionLifecycleState,
    session_manager: &LocalSessionManager,
    reason: &'static str,
) -> DaemonShutdownInputCleanupReport {
    let active_sessions = active_http_session_ids(session_manager).await;
    let live_spawn_sessions = session_lifecycle.live_spawned_session_ids_for_shutdown();
    let shutdown_sessions = shutdown_cleanup_session_ids(&active_sessions, &live_spawn_sessions);
    let mut session_reports = Vec::with_capacity(shutdown_sessions.len());
    for session_id in &shutdown_sessions {
        session_reports.push(
            session_lifecycle
                .release_session_inputs_for_daemon_shutdown(session_id, reason)
                .await,
        );
    }
    let mut orphan_lease_owner_cleanup = None;
    let mut final_lease = synapse_action::lease::status();
    if let Some(owner_session_id) = final_lease.owner_session_id.clone()
        && owner_session_id != synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID
        && !shutdown_sessions.contains(&owner_session_id)
    {
        orphan_lease_owner_cleanup = Some(
            session_lifecycle
                .release_session_inputs_for_daemon_shutdown(&owner_session_id, reason)
                .await,
        );
        final_lease = synapse_action::lease::status();
    }
    let final_lease_is_operator = final_lease.owner_session_id.as_deref()
        == Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID);
    let lease_still_held_after_cleanup = final_lease.held && !final_lease_is_operator;
    let mut failure_count = session_reports
        .iter()
        .filter(|report| report.failed)
        .count();
    if orphan_lease_owner_cleanup
        .as_ref()
        .is_some_and(|report| report.failed)
    {
        failure_count += 1;
    }
    if lease_still_held_after_cleanup {
        failure_count += 1;
    }
    DaemonShutdownInputCleanupReport {
        reason,
        active_sessions_before: active_sessions.len(),
        live_spawn_sessions_before: live_spawn_sessions.len(),
        shutdown_sessions_before: shutdown_sessions.len(),
        cleaned_sessions: session_reports.len(),
        orphan_lease_owner_cleanup,
        final_lease_held: final_lease.held,
        final_lease_owner_session_id: final_lease.owner_session_id,
        final_lease_is_operator,
        lease_still_held_after_cleanup,
        failure_count,
        session_reports,
    }
}

fn shutdown_cleanup_session_ids(
    active_sessions: &BTreeSet<String>,
    live_spawn_sessions: &BTreeSet<String>,
) -> BTreeSet<String> {
    active_sessions
        .iter()
        .cloned()
        .chain(live_spawn_sessions.iter().cloned())
        .collect()
}

#[cfg(test)]
async fn cleanup_expired_lease_inputs_once(action_handle: &ActionHandle) {
    let _lease_status_readback = synapse_action::lease::status();
    let pending = synapse_action::lease::expired_cleanup_snapshot();
    for expired in pending {
        let Some(session_id) = expired.owner_session_id.clone() else {
            continue;
        };
        release_expired_session_inputs_and_lease(action_handle, &session_id, &expired).await;
    }
}

#[cfg(test)]
async fn cleanup_stale_session_lease_once(
    action_handle: &ActionHandle,
    active_sessions: &BTreeSet<String>,
) {
    let status = synapse_action::lease::status();
    let Some(owner_session_id) = status.owner_session_id.clone() else {
        return;
    };
    if owner_session_id == synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID {
        return;
    }
    if active_sessions.contains(&owner_session_id) {
        return;
    }
    let before_ownership = action_handle.session_inputs_snapshot();
    let result = action_handle
        .release_session_inputs_and_lease(&owner_session_id)
        .await;
    let after_ownership = action_handle.session_inputs_snapshot();
    let after_lease = synapse_action::lease::status();
    match result {
        Ok(summary) => {
            tracing::info!(
                code = "MCP_HTTP_SESSION_LEASE_STALE_CLEANUP",
                session_id = %owner_session_id,
                input_lease_released = summary.lease_released,
                expired_lease_cleanup_completed = summary.expired_lease_cleanup_completed,
                before_lease = ?status,
                after_lease = ?after_lease,
                before_ownership = ?before_ownership,
                after_ownership = ?after_ownership,
                active_session_count = active_sessions.len(),
                "readback=input_lease edge=http_session_gone after_cleanup"
            );
        }
        Err(error) => {
            tracing::error!(
                code = error.code(),
                session_id = %owner_session_id,
                detail = %error.detail(),
                before_lease = ?status,
                after_lease = ?after_lease,
                before_ownership = ?before_ownership,
                after_ownership = ?after_ownership,
                active_session_count = active_sessions.len(),
                "HTTP MCP stale-session lease cleanup failed"
            );
        }
    }
}

#[cfg(test)]
async fn release_stale_session_inputs_and_lease(action_handle: &ActionHandle, session_id: &str) {
    let before = action_handle.session_inputs_snapshot();
    let before_lease = synapse_action::lease::status();
    let result = action_handle
        .release_session_inputs_and_lease(session_id)
        .await;
    let after = action_handle.session_inputs_snapshot();
    let after_lease = synapse_action::lease::status();
    match result {
        Ok(summary) => {
            tracing::info!(
                code = "MCP_HTTP_SESSION_INPUT_STALE_CLEANUP",
                session_id,
                released_keys = summary.input_summary.released_keys,
                released_buttons = summary.input_summary.released_buttons,
                neutralized_pads = summary.input_summary.neutralized_pads,
                retained_shared_inputs = summary.input_summary.retained_shared_inputs,
                input_lease_released = summary.lease_released,
                expired_lease_cleanup_completed = summary.expired_lease_cleanup_completed,
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "readback=session_input_ownership edge=http_session_gone after_cleanup"
            );
        }
        Err(error) => {
            tracing::error!(
                code = error.code(),
                session_id,
                detail = %error.detail(),
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "HTTP MCP stale-session cleanup failed while releasing owned inputs"
            );
        }
    }
}

#[cfg(test)]
async fn release_expired_session_inputs_and_lease(
    action_handle: &ActionHandle,
    session_id: &str,
    expired: &synapse_action::LeaseStatus,
) {
    let before = action_handle.session_inputs_snapshot();
    let before_lease = synapse_action::lease::status();
    let result = action_handle
        .release_session_inputs_and_lease(session_id)
        .await;
    let after = action_handle.session_inputs_snapshot();
    let after_lease = synapse_action::lease::status();
    match result {
        Ok(summary) => {
            tracing::warn!(
                code = "MCP_HTTP_SESSION_LEASE_EXPIRED_INPUT_CLEANUP",
                session_id,
                released_keys = summary.input_summary.released_keys,
                released_buttons = summary.input_summary.released_buttons,
                neutralized_pads = summary.input_summary.neutralized_pads,
                retained_shared_inputs = summary.input_summary.retained_shared_inputs,
                input_lease_released = summary.lease_released,
                expired_lease_cleanup_completed = summary.expired_lease_cleanup_completed,
                expired = ?expired,
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "readback=session_input_ownership edge=input_lease_expired after_cleanup"
            );
        }
        Err(error) => {
            tracing::error!(
                code = error.code(),
                session_id,
                detail = %error.detail(),
                expired = ?expired,
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "HTTP MCP expired-lease cleanup failed while releasing owned inputs"
            );
        }
    }
}

fn session_store_db(service: &SynapseService) -> anyhow::Result<Arc<Db>> {
    let m3_handle = service.m3_state_handle();
    let mut state = m3_handle.lock().map_err(|_poisoned| {
        anyhow::anyhow!("m3 service state lock poisoned during session-store setup")
    })?;
    state
        .ensure_storage()
        .context("open storage for HTTP MCP session store")
}

#[derive(Clone)]
struct SynapseMcpSessionStore {
    db: Arc<Db>,
    ttl: Option<Duration>,
    session_registry: crate::server::session_registry::SharedSessionRegistry,
}

impl SynapseMcpSessionStore {
    fn new(
        db: Arc<Db>,
        ttl: Option<Duration>,
        session_registry: crate::server::session_registry::SharedSessionRegistry,
    ) -> Self {
        Self {
            db,
            ttl,
            session_registry,
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PersistedMcpSessionState {
    stored_at_unix_ms: u64,
    state: SessionState,
}

#[async_trait::async_trait]
impl SessionStore for SynapseMcpSessionStore {
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        let key = mcp_session_store_key(session_id);
        let rows = self
            .db
            .scan_cf_prefix(cf::CF_KV, &key)
            .map_err(session_store_error)?;
        let Some((_key, value)) = rows.into_iter().find(|(row_key, _value)| row_key == &key) else {
            return Ok(None);
        };
        let now_ms = unix_time_ms()?;
        let persisted = match synapse_storage::decode_json::<PersistedMcpSessionState>(&value) {
            Ok(persisted) => persisted,
            Err(wrapper_error) => {
                if synapse_storage::decode_json::<SessionState>(&value).is_ok() {
                    self.db
                        .delete_batch(cf::CF_KV, [key])
                        .map_err(session_store_error)?;
                    delete_session_continuity_rows(&self.db, session_id)?;
                    tracing::warn!(
                        code = "MCP_HTTP_SESSION_STORE_LEGACY_STALE_DELETE",
                        session_id,
                        detail = %wrapper_error,
                        "deleted legacy MCP HTTP session state without persistent TTL metadata"
                    );
                    return Ok(None);
                }
                tracing::error!(
                    code = "MCP_HTTP_SESSION_STORE_DECODE_FAILED",
                    session_id,
                    detail = %wrapper_error,
                    "failed to decode persisted MCP HTTP session state"
                );
                return Err(session_store_error(wrapper_error));
            }
        };
        if session_store_expired(persisted.stored_at_unix_ms, now_ms, self.ttl) {
            self.db
                .delete_batch(cf::CF_KV, [key])
                .map_err(session_store_error)?;
            delete_session_continuity_rows(&self.db, session_id)?;
            tracing::warn!(
                code = "MCP_HTTP_SESSION_STORE_EXPIRED",
                session_id,
                stored_at_unix_ms = persisted.stored_at_unix_ms,
                now_unix_ms = now_ms,
                ttl_ms = self.ttl.map(duration_millis_u64),
                "deleted expired MCP HTTP session state from CF_KV"
            );
            return Ok(None);
        }
        tracing::info!(
            code = "MCP_HTTP_SESSION_STORE_LOAD",
            session_id,
            stored_at_unix_ms = persisted.stored_at_unix_ms,
            "loaded MCP HTTP session state from CF_KV"
        );
        let newly_visible = record_registry_initialized(
            &self.session_registry,
            session_id,
            &persisted.state,
            persisted.stored_at_unix_ms,
        )
        .map_err(session_store_error)?;
        if newly_visible {
            journal_session_live_event(&self.db, session_id, &persisted.state, "session_restored")
                .map_err(session_store_error)?;
        }
        Ok(Some(persisted.state))
    }

    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        let key = mcp_session_store_key(session_id);
        let stored_at_unix_ms = unix_time_ms()?;
        let persisted = PersistedMcpSessionState {
            stored_at_unix_ms,
            state: state.clone(),
        };
        let encoded = synapse_storage::encode_json(&persisted).map_err(session_store_error)?;
        self.db
            .put_batch_pressure_bypass(cf::CF_KV, [(key, encoded)])
            .map_err(session_store_error)?;
        tracing::info!(
            code = "MCP_HTTP_SESSION_STORE_WRITE",
            session_id,
            stored_at_unix_ms,
            ttl_ms = self.ttl.map(duration_millis_u64),
            "persisted MCP HTTP session state to CF_KV"
        );
        let newly_visible = record_registry_initialized(
            &self.session_registry,
            session_id,
            state,
            stored_at_unix_ms,
        )
        .map_err(session_store_error)?;
        if newly_visible {
            journal_session_live_event(&self.db, session_id, state, "session_initialized")
                .map_err(session_store_error)?;
        }
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let key = mcp_session_store_key(session_id);
        self.db
            .delete_batch(cf::CF_KV, [key])
            .map_err(session_store_error)?;
        delete_session_continuity_rows(&self.db, session_id)?;
        tracing::info!(
            code = "MCP_HTTP_SESSION_STORE_DELETE",
            session_id,
            "deleted MCP HTTP session state from CF_KV"
        );
        let transitioned = record_registry_closed(&self.session_registry, session_id)
            .map_err(session_store_error)?;
        if transitioned {
            journal_session_exited_event(&self.db, session_id, "http_session_store_deleted")
                .map_err(session_store_error)?;
        }
        Ok(())
    }
}

fn record_registry_initialized(
    session_registry: &crate::server::session_registry::SharedSessionRegistry,
    session_id: &str,
    state: &SessionState,
    now_unix_ms: u64,
) -> Result<bool, synapse_storage::StorageError> {
    let mut registry =
        session_registry
            .lock()
            .map_err(|_error| synapse_storage::StorageError::WriteFailed {
                cf_name: cf::CF_KV.to_owned(),
                detail: "session registry lock poisoned during session store".to_owned(),
            })?;
    Ok(registry.record_initialized(session_id, state, "http", now_unix_ms))
}

fn record_registry_closed(
    session_registry: &crate::server::session_registry::SharedSessionRegistry,
    session_id: &str,
) -> Result<bool, synapse_storage::StorageError> {
    let mut registry =
        session_registry
            .lock()
            .map_err(|_error| synapse_storage::StorageError::WriteFailed {
                cf_name: cf::CF_KV.to_owned(),
                detail: "session registry lock poisoned during session delete".to_owned(),
            })?;
    Ok(registry.record_closed(
        session_id,
        crate::server::session_registry::unix_time_ms_now(),
    ))
}

/// Journals a `state_changed → live` agent event for a session that just
/// became visible through the HTTP session store (#897).
fn journal_session_live_event(
    db: &Db,
    session_id: &str,
    state: &SessionState,
    reason_code: &str,
) -> Result<(), synapse_storage::StorageError> {
    use crate::server::{agent_events, session_registry};
    let client_name = state.initialize_params.client_info.name.clone();
    let agent_kind = session_registry::infer_agent_kind(&client_name);
    let mut record = synapse_core::AgentEventRecord::new(
        agent_events::unix_time_ns_now(),
        synapse_core::AgentEventKind::StateChanged,
    );
    record.session_id = Some(session_id.to_owned());
    record.reason_code = Some(reason_code.to_owned());
    record.state_to = Some("live".to_owned());
    record.attributes.operation_name = Some(synapse_core::GenAiOperationName::InvokeAgent);
    record.attributes.conversation_id = Some(session_id.to_owned());
    record.attributes.agent_name = Some(client_name);
    record.attributes.provider_name = agent_events::provider_for_agent_kind(&agent_kind);
    agent_events::record_agent_event(db, &record).map(|_readback| ())
}

/// Journals a terminal `exited` agent event (durable flush) for a session
/// the HTTP session store just deleted (#897). The outcome of the agent's
/// work is unknown at this layer, so the end state is `indeterminate`.
fn journal_session_exited_event(
    db: &Db,
    session_id: &str,
    reason_code: &str,
) -> Result<(), synapse_storage::StorageError> {
    use crate::server::agent_events;
    let mut record = synapse_core::AgentEventRecord::new(
        agent_events::unix_time_ns_now(),
        synapse_core::AgentEventKind::Exited,
    );
    record.session_id = Some(session_id.to_owned());
    record.reason_code = Some(reason_code.to_owned());
    record.end_state = Some(synapse_core::AgentEndState::Indeterminate);
    record.attributes.conversation_id = Some(session_id.to_owned());
    agent_events::record_agent_event_durable(db, &record).map(|_readback| ())
}

fn mcp_session_store_key(session_id: &str) -> Vec<u8> {
    crate::server::session_lifecycle::mcp_session_store_key(session_id)
}

fn delete_session_continuity_rows(db: &Db, session_id: &str) -> Result<(), SessionStoreError> {
    let readback =
        crate::server::session_continuity::delete_persisted_session_continuity_rows_from_db(
            db, session_id,
        )
        .map_err(|error| -> SessionStoreError { Box::new(io::Error::other(error)) })?;
    tracing::info!(
        code = "MCP_HTTP_SESSION_CONTINUITY_DELETE",
        session_id,
        readback = ?readback,
        "readback=CF_SESSIONS after=http_session_store_deleted_continuity"
    );
    Ok(())
}

fn session_store_error(error: synapse_storage::StorageError) -> SessionStoreError {
    Box::new(error)
}

fn unix_time_ms() -> Result<u64, SessionStoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| -> SessionStoreError { Box::new(error) })?;
    Ok(duration_millis_u64(elapsed))
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn session_store_expired(stored_at_unix_ms: u64, now_unix_ms: u64, ttl: Option<Duration>) -> bool {
    let Some(ttl) = ttl else {
        return false;
    };
    now_unix_ms.saturating_sub(stored_at_unix_ms) > duration_millis_u64(ttl)
}

fn http_service(
    shutdown_cancel: CancellationToken,
    connection_closed_cancel: CancellationToken,
    sse_state: SseState,
    m2_config: &M2ServiceConfig,
    m3_config: M3ServiceConfig,
    m4_config: M4ServiceConfig,
) -> io::Result<SynapseService> {
    SynapseService::try_with_m2_shutdown_reason_and_sse_state_and_m3_config(
        shutdown_cancel,
        "http",
        connection_closed_cancel,
        sse_state,
        m2_config,
        m3_config,
        m4_config,
    )
    .map_err(|error| io::Error::other(format!("{error:#}")))
}

#[derive(Serialize)]
struct DashboardStateResponse {
    schema_version: u32,
    generated_at_unix_ms: u64,
    bind_addr: String,
    token_policy: &'static str,
    timings: DashboardStateTimings,
    dashboard_assets: DashboardPanel,
    auth: DashboardPanel,
    daemon: DashboardPanel,
    sessions: DashboardPanel,
    lease: DashboardPanel,
    storage: DashboardPanel,
    target_claims: DashboardPanel,
    timeline: DashboardPanel,
    demo_recording: DashboardPanel,
    events: DashboardPanel,
    hidden_desktops: DashboardPanel,
    cdp_attachments: DashboardPanel,
    shell_jobs: DashboardPanel,
    command_audit: DashboardPanel,
    tasks: DashboardPanel,
    approvals: DashboardPanel,
    suggestions: DashboardPanel,
    armed_runs: DashboardPanel,
    agent_transcripts: DashboardPanel,
    context: DashboardPanel,
    hygiene: DashboardPanel,
    local_models: DashboardPanel,
}

#[derive(Serialize)]
struct DashboardTrayStateResponse {
    schema_version: u32,
    generated_at_unix_ms: u64,
    bind_addr: String,
    token_policy: &'static str,
    source_of_truth: &'static str,
    timings: DashboardStateTimings,
    sessions: DashboardPanel,
    lease: DashboardPanel,
    timeline: DashboardPanel,
    approvals: DashboardPanel,
    demo_recording: DashboardPanel,
}

#[derive(Serialize)]
struct DashboardStateTimings {
    source_of_truth: &'static str,
    total_elapsed_ms: u64,
    segments: Vec<DashboardStateTiming>,
}

#[derive(Serialize)]
struct DashboardStateTiming {
    segment: &'static str,
    elapsed_ms: u64,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DashboardEventScope {
    Fleet,
    Agent,
    Tasks,
    System,
    Audit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardEventSubscribeRequest {
    scope: DashboardEventScope,
    #[serde(default)]
    snapshot_first: bool,
}

#[derive(Serialize)]
struct DashboardEventSubscribeResponse {
    ok: bool,
    source_of_truth: &'static str,
    scope: DashboardEventScope,
    subscription_id: String,
    event_url: String,
    replay_contract: &'static str,
}

#[derive(Serialize)]
struct DashboardPanel {
    status: &'static str,
    source: &'static str,
    data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DashboardAssetSurface {
    schema_version: u32,
    source_of_truth: &'static str,
    js_file: &'static str,
    css_file: &'static str,
}

impl DashboardPanel {
    fn ok(source: &'static str, data: impl Serialize) -> Self {
        Self {
            status: "ok",
            source,
            data: serde_json::to_value(data).unwrap_or_else(|error| {
                serde_json::json!({
                    "serialization_error": error.to_string(),
                })
            }),
            error: None,
        }
    }

    fn unavailable(source: &'static str, data: impl Serialize) -> Self {
        Self {
            status: "unavailable",
            source,
            data: serde_json::to_value(data).unwrap_or_else(|error| {
                serde_json::json!({
                    "serialization_error": error.to_string(),
                })
            }),
            error: None,
        }
    }

    fn error(source: &'static str, error: impl ToString) -> Self {
        Self {
            status: "error",
            source,
            data: serde_json::json!({}),
            error: Some(error.to_string()),
        }
    }
}

fn dashboard_event_subscription(
    scope: DashboardEventScope,
) -> (EventFilter, Vec<String>, &'static str) {
    let approval_kinds = || {
        vec![
            crate::server::APPROVAL_REQUEST_EVENT_KIND.to_owned(),
            crate::server::APPROVAL_DECISION_EVENT_KIND.to_owned(),
            crate::server::APPROVAL_TIMEOUT_EVENT_KIND.to_owned(),
        ]
    };
    let fleet_attention_kinds = || {
        let mut kinds = vec![
            crate::server::agent_state::AGENT_STATE_EVENT_KIND.to_owned(),
            "workspace.put".to_owned(),
        ];
        kinds.extend(approval_kinds());
        kinds
    };
    match scope {
        DashboardEventScope::Fleet => (
            EventFilter::All,
            fleet_attention_kinds(),
            "agent state + workspace + approval events",
        ),
        DashboardEventScope::Agent => (
            EventFilter::All,
            vec![
                crate::server::agent_state::AGENT_STATE_EVENT_KIND.to_owned(),
                "profile-changed".to_owned(),
                "scope-transitioned".to_owned(),
                "workspace.put".to_owned(),
            ],
            "agent state + profile/workspace events",
        ),
        DashboardEventScope::Tasks => (
            EventFilter::All,
            fleet_attention_kinds(),
            "task/attention/approval state events",
        ),
        DashboardEventScope::System => (
            EventFilter::Or {
                args: vec![
                    EventFilter::Source {
                        source: EventSource::System,
                    },
                    EventFilter::Source {
                        source: EventSource::Filesystem,
                    },
                    EventFilter::Source {
                        source: EventSource::Process,
                    },
                    EventFilter::Source {
                        source: EventSource::Clipboard,
                    },
                    EventFilter::Source {
                        source: EventSource::PerceptionAudio,
                    },
                ],
            },
            Vec::new(),
            "system/process/filesystem/audio events",
        ),
        DashboardEventScope::Audit => (
            EventFilter::Or {
                args: vec![
                    EventFilter::Source {
                        source: EventSource::ActionEmitter,
                    },
                    EventFilter::Source {
                        source: EventSource::Reflex,
                    },
                    EventFilter::Source {
                        source: EventSource::System,
                    },
                ],
            },
            Vec::new(),
            "command/audit-relevant system events",
        ),
    }
}

fn dashboard_event_url(subscription_id: &str) -> String {
    format!("/dashboard/events?subscription_id={subscription_id}")
}

#[derive(Serialize)]
struct DashboardStorageSummary {
    schema_version: u32,
    pressure_level: crate::m3::storage::StoragePressureLevel,
    pressure_transition_codes: Vec<String>,
    metrics_mode: String,
    cf_sizes: BTreeMap<String, u64>,
    cf_row_counts: BTreeMap<String, u64>,
    audit_retention_policy_count: usize,
    missing_cf_size_estimates: Vec<String>,
    missing_cf_row_count_estimates: Vec<String>,
}

#[derive(Serialize)]
struct DashboardEventSurface {
    source_of_truth: &'static str,
    active_subscription_count: usize,
    owner_session_ids: Vec<String>,
    owner_read_error: Option<String>,
    agent_event_ingress: serde_json::Value,
    agent_transcript_ingest: serde_json::Value,
}

#[derive(Serialize)]
struct DashboardHiddenDesktopSurface {
    source_of_truth: &'static str,
    row_count: usize,
    rows: Vec<crate::server::session_lifecycle::SessionHiddenDesktopReadback>,
}

#[derive(Serialize)]
struct DashboardCdpAttachmentSurface {
    source_of_truth: &'static str,
    row_count: usize,
    rows: Vec<crate::server::session_lifecycle::SessionCdpTargetOwnerReadback>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTimelinePauseRequest {
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTimelineQueryRequest {
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    end_ts_ns: Option<String>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardEpisodeListRequest {
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    end_ts_ns: Option<String>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    min_duration_ms: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardEpisodeGetRequest {
    episode_id: String,
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    refs_limit: Option<u32>,
    #[serde(default)]
    refs_cursor: Option<String>,
}

/// Dashboard Approvals-inbox decision (#927). Resolves one pending approval —
/// including the `agent_permission` rows a blocked `approval_gate` call is
/// waiting on, so deciding here resumes the agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardApprovalDecideRequest {
    approval_id: String,
    /// "approve"/"accept", "deny"/"decline"/"reject", or "snooze".
    decision: String,
    #[serde(default)]
    note: Option<String>,
    /// Approve-with-edits (#1030): full-replacement tool input, JSON object as a
    /// string. Honored only with an approve decision on an `allow.edit` item.
    #[serde(default)]
    edited_args: Option<String>,
    /// Respond (#1030): the operator's answer to a needs-input / agent_question
    /// item. Honored only with an approve decision on an `allow.respond` item.
    #[serde(default)]
    response: Option<String>,
}

#[derive(Serialize)]
struct DashboardApprovalDecideResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    decision: crate::m3::approvals::ApprovalDecideResponse,
}

#[derive(Serialize)]
struct DashboardTimelineControlResponse<T>
where
    T: Serialize,
{
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    readback: T,
}

/// Dashboard storage-manager request to purge operator timeline recordings by
/// time period / kind / app / actor. Maps to the [`timeline_purge`] tool params
/// (`flag_ids`/`text` are intentionally not exposed here — flag-id deletes flow
/// through the hygiene surface, free-text purges through `timeline_search`).
/// `dry_run` previews the matched count without deleting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTimelinePurgeRequest {
    /// Epoch-nanosecond bound as a decimal string — ns values exceed JS
    /// `Number.MAX_SAFE_INTEGER`, so the client serializes them as strings to
    /// avoid silent precision loss.
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    end_ts_ns: Option<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    cursor: Option<String>,
}

/// Dashboard storage-manager request to trim a column family to its newest rows
/// (`soft_cap_rows` = keep-newest-N) or run the `AUDIT_RETENTION` age sweep.
/// Maps directly to the [`storage_gc_once`] tool params.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardStorageGcRequest {
    cf_name: String,
    soft_cap_rows: u64,
    hard_cap_rows: u64,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    now_ns: Option<u64>,
    #[serde(default)]
    max_age_ns: Option<u64>,
    #[serde(default)]
    dedupe_window_ns: Option<u64>,
    #[serde(default)]
    profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardControlLeaseForceReleaseRequest {
    owner_session_id: String,
    confirmed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardControlLeaseHandoffRequest {
    from_session_id: String,
    to_session_id: String,
    #[serde(default)]
    ttl_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardControlResponse<T>
where
    T: Serialize,
{
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    readback: T,
}

#[derive(Serialize)]
struct DashboardDeferredSurface {
    tool: &'static str,
    available: bool,
    rows: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct DashboardApprovalSurface {
    tool: &'static str,
    available: bool,
    rows: Vec<crate::m3::approvals::ApprovalQueueItem>,
}

#[derive(Serialize)]
struct DashboardLocalModelSurface {
    tool: &'static str,
    available: bool,
    enabled_count: usize,
    unhealthy_count: usize,
    rows: Vec<crate::m3::local_models::LocalModelRegistryRow>,
}

#[derive(Serialize)]
struct DashboardTaskSurface {
    tool: &'static str,
    available: bool,
    source_of_truth: &'static str,
    row_count: usize,
    tasks: Vec<crate::server::agent_tasks::AgentTask>,
    reconciled_orphans: Vec<String>,
    next: crate::server::agent_tasks::TaskNextResponse,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardLocalModelSpawnRequest {
    model_ref: String,
    prompt: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    wait_timeout_ms: Option<u64>,
    #[serde(default)]
    hold_open_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardSpawnAgentRequest {
    #[serde(default)]
    fan_out: Option<u32>,
    #[serde(default)]
    template_id: Option<String>,
    #[serde(default)]
    template_version: Option<u32>,
    #[serde(default)]
    template_params: BTreeMap<String, String>,
    #[serde(default)]
    cli: Option<crate::m4::ActSpawnAgentCli>,
    #[serde(default)]
    kind: Option<crate::m4::ActSpawnAgentCli>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_ref: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    target: Option<crate::m4::ActSpawnAgentTarget>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    wait_timeout_ms: Option<u64>,
    #[serde(default)]
    hold_open_ms: Option<u64>,
    /// Route the spawned agent's risky tool calls through the Approvals inbox
    /// (#927). Defaults true; the dashboard may send false for trusted spawns.
    #[serde(default)]
    require_approval_gate: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTaskDispatchOnceRequest {
    #[serde(default)]
    concurrency_cap: Option<usize>,
    #[serde(default)]
    wait_timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardLocalModelSpawnResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    spawn: crate::m4::ActSpawnAgentResponse,
}

#[derive(Serialize)]
struct DashboardSpawnAgentResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    requested_count: u32,
    succeeded_count: usize,
    failed_count: usize,
    attempts: Vec<DashboardSpawnAgentAttempt>,
}

#[derive(Serialize)]
struct DashboardSpawnAgentAttempt {
    index: u32,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    spawn: Option<crate::m4::ActSpawnAgentResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardAgentKillRequest {
    session_id: String,
    #[serde(default)]
    grace_ms: Option<u64>,
    #[serde(default)]
    interrupt_first: Option<bool>,
}

#[derive(Serialize)]
struct DashboardAgentKillResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    kill: crate::server::agent_control::AgentKillResponse,
}

#[derive(Serialize)]
struct DashboardTemplateListResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    list: crate::server::agent_templates::AgentTemplateListResponse,
}

#[derive(Serialize)]
struct DashboardTemplateUpsertResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    put: crate::server::agent_templates::AgentTemplatePutResponse,
}

#[derive(Serialize)]
struct DashboardTemplateDeleteResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    delete: crate::server::agent_templates::AgentTemplateDeleteResponse,
}

/// Browser-facing request to register an OpenAI-compatible cloud API model
/// (DeepSeek first) into the local-model registry. `api_shape` is fixed to
/// `open_ai_chat_completions` and `allow_non_loopback` to `true` server-side —
/// these are the only valid settings for a remote https provider, so the UI
/// never has to (and cannot) get them wrong. The secret is never sent here:
/// only the `api_key_env_var` *name* the daemon already has in its environment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardApiModelRegisterRequest {
    name: String,
    base_url: String,
    model_id: String,
    #[serde(default)]
    runtime_preset: crate::m3::local_models::LocalModelRuntimePreset,
    api_key_env_var: String,
    /// Optional plaintext API key entered by the operator. DPAPI-encrypted at
    /// rest by the daemon; never stored or returned in plaintext.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    max_tools: Option<u32>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    probe_timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardApiModelRegisterResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    register: crate::m3::local_models::LocalModelRegisterResponse,
}

#[derive(Serialize)]
struct DashboardModelListResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    list: crate::m3::local_models::LocalModelListResponse,
}

/// Browser-facing request to remove a model-registry row (and its stored key).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardModelRemoveRequest {
    name: String,
}

#[derive(Serialize)]
struct DashboardModelRemoveResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    remove: crate::m3::local_models::LocalModelRemoveResponse,
}

#[derive(Serialize)]
struct DashboardModelUpdateResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    update: crate::m3::local_models::LocalModelUpdateResponse,
}

#[derive(Serialize)]
struct DashboardTranscriptSurface {
    source_of_truth: &'static str,
    row_count: usize,
    rows: Vec<crate::server::AgentTranscriptSnapshotRow>,
}

#[derive(Serialize)]
struct DashboardContextSurface {
    source_of_truth: &'static str,
    workspace: DashboardContextWorkspaceSurface,
    inboxes: Vec<DashboardContextInboxSurface>,
}

#[derive(Serialize)]
struct DashboardContextWorkspaceSurface {
    tool: &'static str,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    list: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DashboardContextInboxSurface {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    spawn_id: Option<String>,
    agent_kind: String,
    lifecycle: String,
    source_of_truth: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    inbox: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardContextInjectRequest {
    session_id: String,
    channel: String,
    packet: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    workspace_key: Option<String>,
    #[serde(default)]
    request_receipt: bool,
}

#[derive(Serialize)]
struct DashboardContextInjectResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    channel: String,
    payload_sha256: String,
    readback: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardContextPlanRequest {
    session_id: String,
    plan: serde_json::Value,
    #[serde(default)]
    expected_version: Option<u64>,
    #[serde(default)]
    notify_agent: Option<bool>,
}

#[derive(Serialize)]
struct DashboardContextPlanResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    key: String,
    payload_sha256: String,
    workspace_put: serde_json::Value,
    notification: DashboardContextPlanNotification,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum DashboardContextPlanNotification {
    Skipped,
    Delivered {
        readback: serde_json::Value,
    },
    Failed {
        error_code: String,
        message: String,
        data: Option<serde_json::Value>,
    },
}

#[derive(Serialize)]
struct DashboardHygieneSurface {
    tool: &'static str,
    available: bool,
    source_of_truth: &'static str,
    report: crate::m3::hygiene::HygieneReportResponse,
}

const DASHBOARD_SAVED_VIEW_SCHEMA_VERSION: u32 = 1;
const DASHBOARD_SAVED_VIEW_PREFIX: &str = "dashboard-saved-view/v1/view/";
const DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH: &str = "CF_KV dashboard-saved-view/v1";
const DASHBOARD_SAVED_VIEW_MAX_NAME_CHARS: usize = 80;
const DASHBOARD_SAVED_VIEW_MAX_ID_CHARS: usize = 96;
const DASHBOARD_SAVED_VIEW_MAX_ROUTE_CHARS: usize = 32;
const DASHBOARD_SAVED_VIEW_MAX_FILTER_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardSavedViewRow {
    schema_version: u32,
    view_id: String,
    row_key: String,
    name: String,
    route: String,
    filters: serde_json::Value,
    created_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardSavedViewUpsertRequest {
    #[serde(default)]
    view_id: Option<String>,
    name: String,
    route: String,
    filters: serde_json::Value,
}

#[derive(Serialize)]
struct DashboardSavedViewsResponse {
    ok: bool,
    source_of_truth: &'static str,
    views: Vec<DashboardSavedViewRow>,
    corrupt_row_count: usize,
}

#[derive(Serialize)]
struct DashboardSavedViewUpsertResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    row_key: String,
    view: DashboardSavedViewRow,
}

#[derive(Serialize)]
struct DashboardSavedViewDeleteResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    deleted_row_key: String,
}

#[derive(Debug, Deserialize)]
struct DashboardAuditQueryRequest {
    limit: Option<usize>,
    scan_limit: Option<usize>,
    cursor: Option<String>,
    start_key_hex: Option<String>,
    start_ts_ns: Option<u64>,
    end_ts_ns: Option<u64>,
    session_id: Option<String>,
    tool: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
    row_kind: Option<String>,
}

async fn dashboard_index(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    with_dashboard_security_headers(Html(DASHBOARD_HTML).into_response())
}

async fn dashboard_asset(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(asset): Path<String>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match asset.as_str() {
        DASHBOARD_CSS_FILE => dashboard_asset_response("text/css; charset=utf-8", DASHBOARD_CSS),
        DASHBOARD_JS_FILE => {
            dashboard_asset_response("application/javascript; charset=utf-8", DASHBOARD_JS)
        }
        _ => with_dashboard_security_headers(
            (StatusCode::NOT_FOUND, "DASHBOARD_ASSET_NOT_FOUND").into_response(),
        ),
    }
}

async fn dashboard_saved_views(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match dashboard_saved_view_rows(&state.agent_events_db) {
        Ok((views, corrupt_row_count)) => with_dashboard_security_headers(
            Json(DashboardSavedViewsResponse {
                ok: true,
                source_of_truth: DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
                views,
                corrupt_row_count,
            })
            .into_response(),
        ),
        Err(response) => with_dashboard_security_headers(response),
    }
}

async fn dashboard_saved_view_upsert(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardSavedViewUpsertRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match dashboard_save_view_row(&state.agent_events_db, request) {
        Ok(view) => with_dashboard_security_headers(
            Json(DashboardSavedViewUpsertResponse {
                ok: true,
                trigger: "dashboard.saved_view_upsert",
                source_of_truth: DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
                row_key: view.row_key.clone(),
                view,
            })
            .into_response(),
        ),
        Err(response) => with_dashboard_security_headers(response),
    }
}

async fn dashboard_saved_view_delete(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(view_id): Path<String>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let view_id = match dashboard_validate_saved_view_id(&view_id) {
        Ok(view_id) => view_id,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let row_key = dashboard_saved_view_row_key(&view_id);
    match state
        .agent_events_db
        .delete_batch(cf::CF_KV, [row_key.as_bytes().to_vec()])
    {
        Ok(()) => with_dashboard_security_headers(
            Json(DashboardSavedViewDeleteResponse {
                ok: true,
                trigger: "dashboard.saved_view_delete",
                source_of_truth: DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
                deleted_row_key: row_key,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_storage_error_response(
            "dashboard saved view delete failed",
            error,
        )),
    }
}

#[derive(Debug, Deserialize)]
struct DashboardTerminalWsQuery {
    mode: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashboardTerminalMode {
    Observer,
    Controller,
}

impl DashboardTerminalMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Controller => "controller",
        }
    }
}

const TERMINAL_WS_COMMAND_INPUT: u8 = b'0';
const TERMINAL_WS_COMMAND_RESIZE: u8 = b'1';
const TERMINAL_WS_COMMAND_PAUSE: u8 = b'2';
const TERMINAL_WS_COMMAND_RESUME: u8 = b'3';
const TERMINAL_WS_COMMAND_AUTH_INIT: u8 = b'{';
const TERMINAL_WS_SERVER_OUTPUT: u8 = b'0';
const TERMINAL_WS_SERVER_TITLE: u8 = b'1';
const TERMINAL_WS_SERVER_PREFS: u8 = b'2';
const TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX: usize = 64 * 1024 * 1024;

async fn dashboard_agent_terminal_ws(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(spawn_id): Path<String>,
    Query(query): Query<DashboardTerminalWsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    if !dashboard_valid_agent_spawn_id(&spawn_id) {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "TERMINAL_SPAWN_ID_INVALID",
            "terminal WebSocket requires a valid agent-spawn id",
            Some(serde_json::json!({ "spawn_id": spawn_id })),
        ));
    }
    let mode = match dashboard_terminal_mode(query.mode.as_deref()) {
        Ok(mode) => mode,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let Some(session) = terminal_capture_session(&spawn_id) else {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::CONFLICT,
            "TERMINAL_SESSION_NOT_LIVE",
            "terminal WebSocket attach requires a currently running owned-PTY agent spawn",
            Some(serde_json::json!({
                "spawn_id": spawn_id,
                "structured_error": "dead_or_missing_terminal_session",
            })),
        ));
    };
    let snapshot = match session.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TERMINAL_SNAPSHOT_FAILED",
                "terminal WebSocket failed to read the current shadow screen",
                Some(serde_json::json!({
                    "spawn_id": spawn_id,
                    "source_error": error.to_string(),
                })),
            ));
        }
    };
    if !matches!(snapshot.status, TerminalCaptureStatus::Running) {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::CONFLICT,
            "TERMINAL_SESSION_FINISHED",
            "terminal WebSocket attach requires a running PTY session",
            Some(serde_json::json!({
                "spawn_id": spawn_id,
                "status": format!("{:?}", snapshot.status),
            })),
        ));
    }

    let connection_id = uuid::Uuid::new_v4().to_string();
    tracing::info!(
        code = "DASHBOARD_TERMINAL_WS_ATTACH",
        spawn_id,
        connection_id,
        mode = mode.as_str(),
        process_id = snapshot.process_id,
        "dashboard terminal WebSocket attach accepted"
    );
    ws.on_upgrade(move |socket| {
        dashboard_agent_terminal_ws_loop(socket, session, spawn_id, connection_id, mode)
    })
}

async fn dashboard_agent_terminal_ws_loop(
    socket: WebSocket,
    session: Arc<LiveTerminalSession>,
    spawn_id: String,
    connection_id: String,
    mode: DashboardTerminalMode,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = session.subscribe();
    let snapshot = match session.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = terminal_ws_send_prefs(
                &mut sender,
                serde_json::json!({
                    "event": "error",
                    "code": "TERMINAL_SNAPSHOT_FAILED",
                    "source_error": error.to_string(),
                }),
            )
            .await;
            return;
        }
    };
    let snapshot_seq = snapshot.seq;
    let mut paused = false;
    let mut paused_frames: VecDeque<Vec<u8>> = VecDeque::new();
    let mut paused_bytes = 0usize;

    if terminal_ws_send_prefs(
        &mut sender,
        serde_json::json!({
            "event": "attach",
            "protocol": "ttyd-compatible-1-byte-command",
            "mode": mode.as_str(),
            "spawn_id": spawn_id,
            "connection_id": connection_id,
            "process_id": snapshot.process_id,
            "cols": snapshot.cols,
            "rows": snapshot.rows,
            "snapshot_seq": snapshot_seq,
        }),
    )
    .await
    .is_err()
    {
        return;
    }
    if !snapshot.title.is_empty()
        && terminal_ws_send_frame(
            &mut sender,
            TERMINAL_WS_SERVER_TITLE,
            snapshot.title.as_bytes(),
        )
        .await
        .is_err()
    {
        return;
    }
    if terminal_ws_send_frame(
        &mut sender,
        TERMINAL_WS_SERVER_OUTPUT,
        &terminal_snapshot_dump(&snapshot.screen_text),
    )
    .await
    .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(message)) => {
                        match terminal_ws_client_payload(message) {
                            Some(payload) => {
                                if terminal_ws_handle_client_payload(
                                    &session,
                                    &mut sender,
                                    &connection_id,
                                    mode,
                                    &mut paused,
                                    &mut paused_frames,
                                    &mut paused_bytes,
                                    payload,
                                ).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(
                            code = "DASHBOARD_TERMINAL_WS_RECEIVE_FAILED",
                            spawn_id,
                            connection_id,
                            error = %error,
                            "dashboard terminal WebSocket receive failed"
                        );
                        break;
                    }
                    None => break,
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if event.seq <= snapshot_seq {
                            continue;
                        }
                        if terminal_ws_deliver_event(
                            &mut sender,
                            event,
                            paused,
                            &mut paused_frames,
                            &mut paused_bytes,
                        ).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        let _ = terminal_ws_send_prefs(
                            &mut sender,
                            serde_json::json!({
                                "event": "stream_lagged",
                                "dropped_events": dropped,
                            }),
                        ).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = terminal_ws_send_prefs(
                            &mut sender,
                            serde_json::json!({ "event": "closed" }),
                        ).await;
                        break;
                    }
                }
            }
        }
    }
    tracing::info!(
        code = "DASHBOARD_TERMINAL_WS_DETACHED",
        spawn_id,
        connection_id,
        mode = mode.as_str(),
        "dashboard terminal WebSocket detached"
    );
}

async fn terminal_ws_handle_client_payload(
    session: &LiveTerminalSession,
    sender: &mut SplitSink<WebSocket, Message>,
    connection_id: &str,
    mode: DashboardTerminalMode,
    paused: &mut bool,
    paused_frames: &mut VecDeque<Vec<u8>>,
    paused_bytes: &mut usize,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let Some((&command, body)) = payload.split_first() else {
        return Ok(());
    };
    match command {
        TERMINAL_WS_COMMAND_INPUT => {
            if mode != DashboardTerminalMode::Controller {
                let _ = session.audit_rejected_input(connection_id, body, "observer_mode");
                terminal_ws_send_prefs(
                    sender,
                    serde_json::json!({
                        "event": "input_rejected",
                        "reason": "observer_mode",
                    }),
                )
                .await
                .map_err(|_| ())?;
                return Ok(());
            }
            if let Err(error) = session.write_controller_input(connection_id, body) {
                terminal_ws_send_prefs(
                    sender,
                    serde_json::json!({
                        "event": "input_error",
                        "reason": error.to_string(),
                    }),
                )
                .await
                .map_err(|_| ())?;
            }
        }
        TERMINAL_WS_COMMAND_RESIZE => {
            if mode != DashboardTerminalMode::Controller {
                terminal_ws_send_prefs(
                    sender,
                    serde_json::json!({
                        "event": "resize_rejected",
                        "reason": "observer_mode",
                    }),
                )
                .await
                .map_err(|_| ())?;
                return Ok(());
            }
            match terminal_ws_parse_resize(body) {
                Ok((cols, rows)) => {
                    if let Err(error) = session.resize(connection_id, cols, rows) {
                        terminal_ws_send_prefs(
                            sender,
                            serde_json::json!({
                                "event": "resize_error",
                                "reason": error.to_string(),
                            }),
                        )
                        .await
                        .map_err(|_| ())?;
                    }
                }
                Err(error) => {
                    terminal_ws_send_prefs(
                        sender,
                        serde_json::json!({
                            "event": "resize_error",
                            "reason": error,
                        }),
                    )
                    .await
                    .map_err(|_| ())?;
                }
            }
        }
        TERMINAL_WS_COMMAND_PAUSE => {
            *paused = true;
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "paused",
                    "buffered_bytes": paused_bytes,
                }),
            )
            .await
            .map_err(|_| ())?;
        }
        TERMINAL_WS_COMMAND_RESUME => {
            *paused = false;
            while let Some(frame) = paused_frames.pop_front() {
                *paused_bytes = paused_bytes.saturating_sub(frame.len());
                sender
                    .send(Message::Binary(frame.into()))
                    .await
                    .map_err(|_| ())?;
            }
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "resumed",
                    "buffered_bytes": paused_bytes,
                }),
            )
            .await
            .map_err(|_| ())?;
        }
        TERMINAL_WS_COMMAND_AUTH_INIT => {
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "auth",
                    "status": "local_only_loopback",
                    "mode": mode.as_str(),
                }),
            )
            .await
            .map_err(|_| ())?;
        }
        _ => {
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "unknown_command",
                    "command": command,
                }),
            )
            .await
            .map_err(|_| ())?;
        }
    }
    Ok(())
}

async fn terminal_ws_deliver_event(
    sender: &mut SplitSink<WebSocket, Message>,
    event: TerminalCaptureEvent,
    paused: bool,
    paused_frames: &mut VecDeque<Vec<u8>>,
    paused_bytes: &mut usize,
) -> Result<(), ()> {
    let frame = match event.kind {
        TerminalCaptureEventKind::Output(bytes) => {
            terminal_ws_frame(TERMINAL_WS_SERVER_OUTPUT, &bytes)
        }
        TerminalCaptureEventKind::Title(title) => {
            terminal_ws_frame(TERMINAL_WS_SERVER_TITLE, title.as_bytes())
        }
        TerminalCaptureEventKind::Prefs(value) => {
            let bytes = serde_json::to_vec(&value).map_err(|_| ())?;
            terminal_ws_frame(TERMINAL_WS_SERVER_PREFS, &bytes)
        }
        TerminalCaptureEventKind::Exit(exit_code) => {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "event": "exit",
                "exit_code": exit_code,
            }))
            .map_err(|_| ())?;
            terminal_ws_frame(TERMINAL_WS_SERVER_PREFS, &bytes)
        }
    };
    if paused {
        terminal_ws_buffer_paused_frame(paused_frames, paused_bytes, frame)
    } else {
        sender
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|_| ())
    }
}

fn terminal_ws_buffer_paused_frame(
    paused_frames: &mut VecDeque<Vec<u8>>,
    paused_bytes: &mut usize,
    frame: Vec<u8>,
) -> Result<(), ()> {
    let new_total = paused_bytes.saturating_add(frame.len());
    if new_total > TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX {
        return Err(());
    }
    *paused_bytes = new_total;
    paused_frames.push_back(frame);
    Ok(())
}

fn terminal_ws_client_payload(message: Message) -> Option<Vec<u8>> {
    match message {
        Message::Binary(bytes) => Some(bytes.to_vec()),
        Message::Text(text) => Some(text.as_bytes().to_vec()),
        Message::Ping(_) | Message::Pong(_) => Some(Vec::new()),
        Message::Close(_) => None,
    }
}

async fn terminal_ws_send_prefs(
    sender: &mut SplitSink<WebSocket, Message>,
    value: serde_json::Value,
) -> Result<(), axum::Error> {
    let bytes =
        serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"event\":\"encode_error\"}".to_vec());
    terminal_ws_send_frame(sender, TERMINAL_WS_SERVER_PREFS, &bytes).await
}

async fn terminal_ws_send_frame(
    sender: &mut SplitSink<WebSocket, Message>,
    code: u8,
    payload: &[u8],
) -> Result<(), axum::Error> {
    sender
        .send(Message::Binary(terminal_ws_frame(code, payload).into()))
        .await
}

fn terminal_ws_frame(code: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(code);
    frame.extend_from_slice(payload);
    frame
}

fn terminal_snapshot_dump(screen_text: &str) -> Vec<u8> {
    let mut dump = Vec::from(&b"\x1b[H\x1b[2J"[..]);
    if !screen_text.is_empty() {
        dump.extend_from_slice(screen_text.replace('\n', "\r\n").as_bytes());
    }
    dump
}

fn terminal_ws_parse_resize(payload: &[u8]) -> Result<(u16, u16), String> {
    let text = std::str::from_utf8(payload)
        .map_err(|error| format!("resize payload must be UTF-8: {error}"))?
        .trim();
    if text.is_empty() {
        return Err("resize payload is empty".to_owned());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let cols = value
            .get("cols")
            .or_else(|| value.get("columns"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "resize JSON requires cols/columns".to_owned())?;
        let rows = value
            .get("rows")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "resize JSON requires rows".to_owned())?;
        return terminal_ws_validate_size(cols, rows);
    }
    let parts: Vec<&str> = text
        .split(['x', 'X', ',', ' '])
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 2 {
        return Err("resize payload must be COLSxROWS or JSON".to_owned());
    }
    let cols = parts[0]
        .parse::<u64>()
        .map_err(|error| format!("resize cols invalid: {error}"))?;
    let rows = parts[1]
        .parse::<u64>()
        .map_err(|error| format!("resize rows invalid: {error}"))?;
    terminal_ws_validate_size(cols, rows)
}

fn terminal_ws_validate_size(cols: u64, rows: u64) -> Result<(u16, u16), String> {
    if !(1..=500).contains(&cols) || !(1..=500).contains(&rows) {
        return Err("resize dimensions must be in 1..=500".to_owned());
    }
    Ok((cols as u16, rows as u16))
}

fn dashboard_terminal_mode(value: Option<&str>) -> Result<DashboardTerminalMode, Response> {
    match value
        .unwrap_or("observer")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "observer" | "observe" | "read" => Ok(DashboardTerminalMode::Observer),
        "controller" | "control" | "write" => Ok(DashboardTerminalMode::Controller),
        other => Err(with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "TERMINAL_MODE_INVALID",
            "terminal WebSocket mode must be observer or controller",
            Some(serde_json::json!({ "mode": other })),
        ))),
    }
}

fn dashboard_valid_agent_spawn_id(spawn_id: &str) -> bool {
    spawn_id.starts_with("agent-spawn-")
        && spawn_id.len() <= 128
        && spawn_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

async fn dashboard_tray_state(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let state_started = Instant::now();
    let mut timing_segments = Vec::new();
    let sessions = dashboard_timed_state_segment(&mut timing_segments, "sessions", || match state
        .health_service
        .session_list_impl(false)
    {
        Ok(sessions) => DashboardPanel::ok(
            "session_list dashboard primary agent feed + acknowledged escalation suppression",
            dashboard_primary_session_list_data(
                &sessions,
                state.health_service.acked_open_attention_anchors_snapshot(),
            ),
        ),
        Err(error) => DashboardPanel::error("session_list", format!("{error:?}")),
    });
    let lease = dashboard_timed_state_segment(&mut timing_segments, "lease", || {
        DashboardPanel::ok("control_lease_status", synapse_action::lease::status())
    });
    let timeline = dashboard_timed_state_segment(&mut timing_segments, "timeline", || match state
        .health_service
        .timeline_stats_snapshot()
    {
        Ok(snapshot) => DashboardPanel::ok("timeline_stats", snapshot),
        Err(error) => DashboardPanel::error("timeline_stats", format!("{error:?}")),
    });
    let approvals =
        dashboard_timed_state_segment(&mut timing_segments, "approvals", || {
            match state.health_service.approval_queue_snapshot(None) {
                Ok(rows) => DashboardPanel::ok(
                    "approval_list",
                    DashboardApprovalSurface {
                        tool: "approval_list",
                        available: true,
                        rows,
                    },
                ),
                Err(error) => DashboardPanel::error("approval_list", format!("{error:?}")),
            }
        });
    let demo_recording =
        dashboard_timed_state_segment(&mut timing_segments, "demo_recording", || {
            match state.health_service.demo_record_status_snapshot() {
                Ok(snapshot) => DashboardPanel::ok("demo_record_status", snapshot),
                Err(error) => DashboardPanel::error("demo_record_status", format!("{error:?}")),
            }
        });
    let timings = DashboardStateTimings {
        source_of_truth: "daemon Instant wall-clock around dashboard_tray_state segments",
        total_elapsed_ms: dashboard_elapsed_ms(state_started.elapsed()),
        segments: timing_segments,
    };
    let response = DashboardTrayStateResponse {
        schema_version: 1,
        generated_at_unix_ms: dashboard_unix_time_ms(),
        bind_addr: state.bind_addr.to_string(),
        token_policy: "dashboard responses never include bearer tokens",
        source_of_truth: "same dashboard/MCP snapshot methods as /dashboard/state.json, limited to tray companion panels",
        timings,
        sessions,
        lease,
        timeline,
        approvals,
        demo_recording,
    };
    with_dashboard_security_headers(Json(response).into_response())
}

async fn dashboard_state(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let state_started = Instant::now();
    let mut timing_segments = Vec::new();
    let active_sessions_started = Instant::now();
    let active_sessions = state.session_manager.sessions.read().await.len();
    dashboard_push_state_timing(
        &mut timing_segments,
        "active_sessions",
        active_sessions_started,
    );
    let health = dashboard_timed_state_segment(&mut timing_segments, "health", || {
        state
            .health_service
            .health_payload_with_http_sessions(Some(active_sessions))
    });
    let sessions = dashboard_timed_state_segment(&mut timing_segments, "sessions", || match state
        .health_service
        .session_list_impl(false)
    {
        Ok(sessions) => DashboardPanel::ok(
            "session_list dashboard primary agent feed + acknowledged escalation suppression",
            dashboard_primary_session_list_data(
                &sessions,
                state.health_service.acked_open_attention_anchors_snapshot(),
            ),
        ),
        Err(error) => DashboardPanel::error("session_list", format!("{error:?}")),
    });
    let lease = dashboard_timed_state_segment(&mut timing_segments, "lease", || {
        DashboardPanel::ok("control_lease_status", synapse_action::lease::status())
    });
    let storage = dashboard_timed_state_segment(&mut timing_segments, "storage", || {
        match state.health_service.storage_summary_snapshot() {
            Ok(snapshot) => DashboardPanel::ok(
                "storage_summary",
                DashboardStorageSummary {
                    schema_version: snapshot.schema_version,
                    pressure_level: snapshot.pressure_level,
                    pressure_transition_codes: snapshot.pressure_transition_codes,
                    metrics_mode: snapshot.metrics_mode,
                    cf_sizes: snapshot.cf_sizes,
                    cf_row_counts: snapshot.cf_row_counts,
                    audit_retention_policy_count: snapshot.audit_retention_policy_count,
                    missing_cf_size_estimates: snapshot.missing_cf_size_estimates,
                    missing_cf_row_count_estimates: snapshot.missing_cf_row_count_estimates,
                },
            ),
            Err(error) => DashboardPanel::error("storage_summary", format!("{error:?}")),
        }
    });
    let target_claims =
        dashboard_timed_state_segment(&mut timing_segments, "target_claims", || {
            match state.health_service.target_claim_status_snapshot() {
                Ok(snapshot) => DashboardPanel::ok("target_claim_status", snapshot),
                Err(error) => DashboardPanel::error("target_claim_status", format!("{error:?}")),
            }
        });
    let timeline = dashboard_timed_state_segment(&mut timing_segments, "timeline", || match state
        .health_service
        .timeline_stats_snapshot()
    {
        Ok(snapshot) => DashboardPanel::ok("timeline_stats", snapshot),
        Err(error) => DashboardPanel::error("timeline_stats", format!("{error:?}")),
    });
    let demo_recording =
        dashboard_timed_state_segment(&mut timing_segments, "demo_recording", || {
            match state.health_service.demo_record_status_snapshot() {
                Ok(snapshot) => DashboardPanel::ok("demo_record_status", snapshot),
                Err(error) => DashboardPanel::error("demo_record_status", format!("{error:?}")),
            }
        });
    let events = dashboard_timed_state_segment(&mut timing_segments, "events", || {
        dashboard_events_panel(&state)
    });
    let hidden_desktops =
        dashboard_timed_state_segment(&mut timing_segments, "hidden_desktops", || {
            dashboard_hidden_desktops_panel(&state)
        });
    let cdp_attachments =
        dashboard_timed_state_segment(&mut timing_segments, "cdp_attachments", || {
            dashboard_cdp_attachments_panel(&state)
        });
    let shell_jobs = dashboard_timed_state_segment(&mut timing_segments, "shell_jobs", || {
        dashboard_shell_jobs_panel()
    });
    let tool_names = dashboard_timed_state_segment(&mut timing_segments, "tool_names", || {
        health
            .tool_names
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
    });
    let dashboard_assets =
        dashboard_timed_state_segment(&mut timing_segments, "dashboard_assets", || {
            DashboardPanel::ok(
                "embedded dashboard dist assets",
                DashboardAssetSurface {
                    schema_version: 1,
                    source_of_truth: "synapse-mcp embedded dashboard dist asset constants",
                    js_file: DASHBOARD_JS_FILE,
                    css_file: DASHBOARD_CSS_FILE,
                },
            )
        });
    let auth = dashboard_timed_state_segment(&mut timing_segments, "auth", || {
        DashboardPanel::ok(
            "local-only trust model (loopback bind + Host guard; no app-layer auth)",
            serde_json::json!({
                "mode": "local_only",
                "authenticated": true,
                "source": "loopback bind + Host header guard",
            }),
        )
    });
    let daemon = dashboard_timed_state_segment(&mut timing_segments, "daemon", || {
        DashboardPanel::ok("health", &health)
    });
    let command_audit =
        dashboard_timed_state_segment(&mut timing_segments, "command_audit", || {
            command_audit_panel(&state)
        });
    let tasks = dashboard_timed_state_segment(&mut timing_segments, "tasks", || {
        tasks_panel(&state, &tool_names)
    });
    let approvals = dashboard_timed_state_segment(&mut timing_segments, "approvals", || {
        approval_panel(&state, &tool_names, None)
    });
    let suggestions = dashboard_timed_state_segment(&mut timing_segments, "suggestions", || {
        approval_panel(
            &state,
            &tool_names,
            Some(crate::m3::approvals::ApprovalKind::Suggestion),
        )
    });
    let armed_runs = dashboard_timed_state_segment(&mut timing_segments, "armed_runs", || {
        approval_panel(
            &state,
            &tool_names,
            Some(crate::m3::approvals::ApprovalKind::ArmedRunReview),
        )
    });
    let agent_transcripts =
        dashboard_timed_state_segment(&mut timing_segments, "agent_transcripts", || {
            agent_transcript_panel(&state)
        });
    let context = dashboard_timed_state_segment(&mut timing_segments, "context", || {
        context_panel(&state, &tool_names)
    });
    let hygiene = dashboard_timed_state_segment(&mut timing_segments, "hygiene", || {
        hygiene_panel(&state, &tool_names)
    });
    let local_models = dashboard_timed_state_segment(&mut timing_segments, "local_models", || {
        local_model_panel(&state, &tool_names)
    });
    let timings = DashboardStateTimings {
        source_of_truth: "daemon Instant wall-clock around dashboard_state segments",
        total_elapsed_ms: dashboard_elapsed_ms(state_started.elapsed()),
        segments: timing_segments,
    };
    let response = DashboardStateResponse {
        schema_version: 1,
        generated_at_unix_ms: dashboard_unix_time_ms(),
        bind_addr: state.bind_addr.to_string(),
        token_policy: "dashboard responses never include bearer tokens",
        timings,
        dashboard_assets,
        auth,
        daemon,
        sessions,
        lease,
        storage,
        target_claims,
        timeline,
        demo_recording,
        events,
        hidden_desktops,
        cdp_attachments,
        shell_jobs,
        command_audit,
        tasks,
        approvals,
        suggestions,
        armed_runs,
        agent_transcripts,
        context,
        hygiene,
        local_models,
    };
    with_dashboard_security_headers(Json(response).into_response())
}

fn dashboard_timed_state_segment<T>(
    timings: &mut Vec<DashboardStateTiming>,
    segment: &'static str,
    action: impl FnOnce() -> T,
) -> T {
    let started = Instant::now();
    let result = action();
    dashboard_push_state_timing(timings, segment, started);
    result
}

fn dashboard_push_state_timing(
    timings: &mut Vec<DashboardStateTiming>,
    segment: &'static str,
    started: Instant,
) {
    timings.push(DashboardStateTiming {
        segment,
        elapsed_ms: dashboard_elapsed_ms(started.elapsed()),
    });
}

fn dashboard_elapsed_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn dashboard_events_subscribe(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardEventSubscribeRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (filter, kinds, source_of_truth) = dashboard_event_subscription(request.scope);
    let subscription_id =
        match state
            .sse_state
            .subscribe(filter, kinds, request.snapshot_first, None)
        {
            Ok(subscription_id) => subscription_id,
            Err(error) => {
                return with_dashboard_security_headers(dashboard_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error.code(),
                    &error.message(),
                    Some(serde_json::json!({ "scope": request.scope })),
                ));
            }
        };
    with_dashboard_security_headers(
        Json(DashboardEventSubscribeResponse {
            ok: true,
            source_of_truth,
            scope: request.scope,
            event_url: dashboard_event_url(&subscription_id),
            subscription_id,
            replay_contract: "browser EventSource reconnect sends Last-Event-ID to /events with the stable subscription_id query parameter",
        })
        .into_response(),
    )
}

async fn dashboard_events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<sse::EventsQuery>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    with_dashboard_security_headers(state.sse_state.open(&headers, query))
}

async fn dashboard_audit_query(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(request): Query<DashboardAuditQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::server::command_audit::CommandAuditQueryParams {
        limit: request.limit,
        scan_limit: request.scan_limit,
        start_key_hex: request.cursor.or(request.start_key_hex),
        start_ts_ns: request.start_ts_ns,
        end_ts_ns: request.end_ts_ns,
        session_id: request.session_id,
        tool: request.tool,
        status: request.status,
        error_code: request.error_code,
        row_kind: request.row_kind,
    };
    match state.health_service.command_audit_query(params) {
        Ok(query) => with_dashboard_security_headers(
            Json(serde_json::json!({
                "ok": true,
                "source_of_truth": "CF_ACTION_LOG bounded scan",
                "query": query,
            }))
            .into_response(),
        ),
        Err(error) => {
            let code = dashboard_error_code(&error);
            let status = if code == synapse_core::error_codes::TOOL_PARAMS_INVALID {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            with_dashboard_security_headers(dashboard_error_response(
                status,
                &code,
                &error.message,
                error.data.clone(),
            ))
        }
    }
}

fn dashboard_saved_view_rows(db: &Db) -> Result<(Vec<DashboardSavedViewRow>, usize), Response> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, DASHBOARD_SAVED_VIEW_PREFIX.as_bytes())
        .map_err(|error| {
            dashboard_storage_error_response("dashboard saved views read failed", error)
        })?;
    let mut views = Vec::new();
    let mut corrupt_row_count = 0;
    for (key, value) in rows {
        match serde_json::from_slice::<DashboardSavedViewRow>(&value) {
            Ok(row) if key == row.row_key.as_bytes() => views.push(row),
            Ok(_row) => corrupt_row_count += 1,
            Err(error) => {
                corrupt_row_count += 1;
                tracing::warn!(
                    code = synapse_core::error_codes::STORAGE_CORRUPTED,
                    row_key = %String::from_utf8_lossy(&key),
                    detail = %error,
                    "dashboard saved view row decode failed"
                );
            }
        }
    }
    views.sort_by(|left, right| {
        right
            .updated_unix_ms
            .cmp(&left.updated_unix_ms)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok((views, corrupt_row_count))
}

fn dashboard_save_view_row(
    db: &Db,
    request: DashboardSavedViewUpsertRequest,
) -> Result<DashboardSavedViewRow, Response> {
    let now = dashboard_unix_time_ms();
    let name = dashboard_validate_saved_view_name(&request.name)?;
    let route = dashboard_validate_saved_view_route(&request.route)?;
    let filters = dashboard_validate_saved_view_filters(request.filters)?;
    let view_id = match request.view_id {
        Some(value) => dashboard_validate_saved_view_id(&value)?,
        None => dashboard_saved_view_id_from_name(&name, now),
    };
    let row_key = dashboard_saved_view_row_key(&view_id);
    let created_unix_ms = dashboard_read_saved_view_by_key(db, &row_key)?
        .map(|row| row.created_unix_ms)
        .unwrap_or(now);
    let row = DashboardSavedViewRow {
        schema_version: DASHBOARD_SAVED_VIEW_SCHEMA_VERSION,
        view_id,
        row_key: row_key.clone(),
        name,
        route,
        filters,
        created_unix_ms,
        updated_unix_ms: now,
    };
    let encoded = serde_json::to_vec(&row).map_err(|error| {
        tracing::error!(
            code = synapse_core::error_codes::STORAGE_WRITE_FAILED,
            row_key,
            detail = %error,
            "dashboard saved view row encode failed"
        );
        dashboard_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            synapse_core::error_codes::STORAGE_WRITE_FAILED,
            "dashboard saved view row encode failed",
            None,
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
        .map_err(|error| {
            dashboard_storage_error_response("dashboard saved view write failed", error)
        })?;
    tracing::info!(
        code = "DASHBOARD_SAVED_VIEW_WRITTEN",
        row_key,
        source_of_truth = DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
        "dashboard saved view row written"
    );
    Ok(row)
}

fn dashboard_read_saved_view_by_key(
    db: &Db,
    row_key: &str,
) -> Result<Option<DashboardSavedViewRow>, Response> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| {
            dashboard_storage_error_response("dashboard saved view read failed", error)
        })?;
    let Some((_key, value)) = rows
        .into_iter()
        .find(|(key, _value)| key == row_key.as_bytes())
    else {
        return Ok(None);
    };
    let row = serde_json::from_slice::<DashboardSavedViewRow>(&value).map_err(|error| {
        tracing::error!(
            code = synapse_core::error_codes::STORAGE_CORRUPTED,
            row_key,
            detail = %error,
            "dashboard saved view row corrupt"
        );
        dashboard_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "dashboard saved view row corrupt",
            None,
        )
    })?;
    Ok(Some(row))
}

fn dashboard_validate_saved_view_name(value: &str) -> Result<String, Response> {
    let trimmed = value.trim();
    let char_count = trimmed.chars().count();
    if trimmed.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view requires name",
            None,
        ));
    }
    if char_count > DASHBOARD_SAVED_VIEW_MAX_NAME_CHARS {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view name is too long",
            None,
        ));
    }
    Ok(trimmed.to_owned())
}

fn dashboard_validate_saved_view_id(value: &str) -> Result<String, Response> {
    let trimmed = value.trim();
    let char_count = trimmed.chars().count();
    if trimmed.is_empty()
        || char_count > DASHBOARD_SAVED_VIEW_MAX_ID_CHARS
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view id must be ascii alphanumeric plus ._-",
            None,
        ));
    }
    Ok(trimmed.to_owned())
}

fn dashboard_validate_saved_view_route(value: &str) -> Result<String, Response> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > DASHBOARD_SAVED_VIEW_MAX_ROUTE_CHARS {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view route is invalid",
            None,
        ));
    }
    match trimmed {
        "fleet" | "agent" | "tasks" | "approvals" | "analytics" | "timeline" | "system"
        | "audit" => Ok(trimmed.to_owned()),
        _ => Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view route is not supported",
            None,
        )),
    }
}

fn dashboard_validate_saved_view_filters(
    value: serde_json::Value,
) -> Result<serde_json::Value, Response> {
    if !value.is_object() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view filters must be an object",
            None,
        ));
    }
    let encoded = serde_json::to_vec(&value).map_err(|error| {
        tracing::error!(
            code = synapse_core::error_codes::TOOL_PARAMS_INVALID,
            detail = %error,
            "dashboard saved view filters encode failed"
        );
        dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view filters are invalid",
            None,
        )
    })?;
    if encoded.len() > DASHBOARD_SAVED_VIEW_MAX_FILTER_BYTES {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view filters are too large",
            None,
        ));
    }
    Ok(value)
}

fn dashboard_saved_view_id_from_name(name: &str, now_unix_ms: u64) -> String {
    let mut slug = String::with_capacity(name.len().min(40));
    let mut previous_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("view");
    }
    format!("{slug}-{now_unix_ms}")
}

fn dashboard_saved_view_row_key(view_id: &str) -> String {
    format!("{DASHBOARD_SAVED_VIEW_PREFIX}{view_id}")
}

async fn dashboard_local_model_spawn(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardLocalModelSpawnRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let mut params = match dashboard_local_model_spawn_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    // Anchor the spawned agent's MCP endpoint to THIS daemon's bind address so
    // it phones home to the daemon that launched it (not the hardcoded default).
    params.mcp_url = crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr);
    match state
        .health_service
        .dashboard_spawn_local_model_agent(params)
        .await
    {
        Ok(spawn) => with_dashboard_security_headers(
            Json(DashboardLocalModelSpawnResponse {
                ok: true,
                trigger: "dashboard.local_model_spawn",
                source_of_truth:
                    "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                spawn,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_spawn_agent(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardSpawnAgentRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (fan_out, mut spawn_request) = match dashboard_spawn_agent_request_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    // Anchor browser-triggered spawns to this daemon, just like the legacy
    // local-model-only endpoint. The browser cannot override the MCP endpoint.
    spawn_request.mcp_url = crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr);
    let mut attempts = Vec::with_capacity(fan_out as usize);
    for index in 1..=fan_out {
        let attempt = match state
            .health_service
            .dashboard_spawn_agent_request(spawn_request.clone())
            .await
        {
            Ok(spawn) => DashboardSpawnAgentAttempt {
                index,
                status: "ok",
                spawn: Some(spawn),
                error_code: None,
                message: None,
                data: None,
            },
            Err(error) => DashboardSpawnAgentAttempt {
                index,
                status: "error",
                spawn: None,
                error_code: Some(dashboard_error_code(&error)),
                message: Some(error.message.to_string()),
                data: error.data.clone(),
            },
        };
        attempts.push(attempt);
    }
    let succeeded_count = attempts
        .iter()
        .filter(|attempt| attempt.status == "ok")
        .count();
    let failed_count = attempts.len().saturating_sub(succeeded_count);
    with_dashboard_security_headers(
        Json(DashboardSpawnAgentResponse {
            ok: true,
            trigger: "dashboard.spawn_agent",
            source_of_truth:
                "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
            requested_count: fan_out,
            succeeded_count,
            failed_count,
            attempts,
        })
        .into_response(),
    )
}

async fn dashboard_approval_decide(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardApprovalDecideRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let approval_id = request.approval_id.trim();
    if approval_id.is_empty() {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "APPROVAL_DECIDE_ID_EMPTY",
            "approval_id must be a non-empty approval id",
            None,
        ));
    }
    let decision = match request.decision.trim().to_ascii_lowercase().as_str() {
        "approve" | "accept" | "allow" => crate::m3::approvals::ApprovalDecision::Accept,
        "deny" | "decline" | "reject" => crate::m3::approvals::ApprovalDecision::Decline,
        "snooze" => crate::m3::approvals::ApprovalDecision::Snooze,
        other => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                "APPROVAL_DECIDE_DECISION_INVALID",
                &format!(
                    "decision {other:?} is not one of approve|accept|allow|deny|decline|reject|snooze"
                ),
                None,
            ));
        }
    };
    let note = request
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty());
    // edited_args is JSON object text — must NOT be trimmed of internal content;
    // only drop an entirely-empty field. The approvals layer validates the JSON.
    let edited_args = request
        .edited_args
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let response = request
        .response
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match state.health_service.approval_decide_from_dashboard(
        approval_id,
        decision,
        note,
        edited_args,
        response,
        "dashboard_inbox",
    ) {
        Ok(decision) => with_dashboard_security_headers(
            Json(DashboardApprovalDecideResponse {
                ok: true,
                trigger: "dashboard.approval_decide",
                source_of_truth:
                    "CF_KV approval queue rows + approval audit rows + command audit; blocked approval_gate resumed",
                decision,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_context_inject(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardContextInjectRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let session_id = match dashboard_context_resolve_live_session_id(&state, &request.session_id) {
        Ok(session_id) => session_id,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let packet = request.packet.trim();
    if packet.is_empty() {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "CONTEXT_PACKET_EMPTY",
            "packet must be non-empty",
            None,
        ));
    }
    let channel = request.channel.trim().to_ascii_lowercase();
    let now_unix_ms = dashboard_unix_time_ms();
    let payload = serde_json::json!({
        "schema_version": 1,
        "source": "dashboard_context",
        "target_session_id": session_id,
        "requested_session_id": request.session_id.trim(),
        "channel": channel,
        "packet": packet,
        "created_at_unix_ms": now_unix_ms,
    });
    let payload_sha256 = dashboard_payload_sha256(&payload);
    let result = match channel.as_str() {
        "steer" => state.health_service.dashboard_agent_steer(
            session_id.clone(),
            packet.to_owned(),
            request.request_receipt,
        ),
        "mailbox" => {
            let kind = request
                .kind
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("context_packet")
                .to_owned();
            state.health_service.dashboard_agent_send(
                session_id.clone(),
                kind,
                payload.clone(),
                request.request_receipt,
            )
        }
        "workspace" => {
            let key = request
                .workspace_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("context/{session_id}/{now_unix_ms}"));
            state
                .health_service
                .dashboard_workspace_put(key, None, payload.clone())
        }
        other => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                "CONTEXT_CHANNEL_INVALID",
                &format!("channel {other:?} is not one of steer|mailbox|workspace"),
                None,
            ));
        }
    };

    match result {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardContextInjectResponse {
                ok: true,
                trigger: "dashboard.context_inject",
                source_of_truth:
                    "agent_steer/agent_send/workspace_put command audit rows + tool-specific durable readback",
                channel,
                payload_sha256,
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_context_plan(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardContextPlanRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let session_id = match dashboard_context_resolve_live_session_id(&state, &request.session_id) {
        Ok(session_id) => session_id,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let key = format!("plan/{session_id}");
    let payload = serde_json::json!({
        "schema_version": 1,
        "source": "dashboard_context",
        "target_session_id": session_id,
        "requested_session_id": request.session_id.trim(),
        "plan": request.plan,
        "updated_at_unix_ms": dashboard_unix_time_ms(),
    });
    let payload_sha256 = dashboard_payload_sha256(&payload);
    let workspace_put = match state.health_service.dashboard_workspace_put(
        key.clone(),
        request.expected_version,
        payload,
    ) {
        Ok(readback) => readback,
        Err(error) => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                &dashboard_error_code(&error),
                &error.message,
                error.data,
            ));
        }
    };
    let notification = if request.notify_agent.unwrap_or(true) {
        let instruction = format!(
            "Plan updated in workspace key {key}. Re-read that plan artifact before continuing and acknowledge the changed step."
        );
        match state
            .health_service
            .dashboard_agent_steer(session_id, instruction, true)
        {
            Ok(readback) => DashboardContextPlanNotification::Delivered { readback },
            Err(error) => DashboardContextPlanNotification::Failed {
                error_code: dashboard_error_code(&error),
                message: error.message.to_string(),
                data: error.data,
            },
        }
    } else {
        DashboardContextPlanNotification::Skipped
    };
    with_dashboard_security_headers(
        Json(DashboardContextPlanResponse {
            ok: true,
            trigger: "dashboard.context_plan",
            source_of_truth:
                "workspace_put CF_KV plan artifact + optional agent_steer notification + CF_ACTION_LOG command audit",
            key,
            payload_sha256,
            workspace_put,
            notification,
        })
        .into_response(),
    )
}

async fn dashboard_agent_kill(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentKillRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_agent_kill_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state
        .health_service
        .dashboard_agent_kill_request(params)
        .await
    {
        Ok(kill) => with_dashboard_security_headers(
            Json(DashboardAgentKillResponse {
                ok: true,
                trigger: "dashboard.agent_kill",
                source_of_truth:
                    "OS process table, session registry, CF_AGENT_EVENTS, command audit rows, agent spawn artifacts",
                kill,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_create(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_tasks::TaskCreateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_task_create(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_create",
                source_of_truth: "CF_KV agent-task/v1 rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_update(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_tasks::TaskUpdateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_task_update(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_update",
                source_of_truth: "CF_KV agent-task/v1 rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_cancel(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_tasks::TaskCancelParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_task_cancel(params).await {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_cancel",
                source_of_truth:
                    "CF_KV agent-task/v1 rows plus OS process table/session registry for live-attempt interrupt",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_dispatch_once(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTaskDispatchOnceRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::server::agent_tasks::TaskDispatchOnceParams {
        concurrency_cap: request
            .concurrency_cap
            .unwrap_or_else(crate::server::agent_tasks::default_cap),
        mcp_url: crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr),
        wait_timeout_ms: request
            .wait_timeout_ms
            .unwrap_or_else(crate::m4::default_agent_spawn_wait_timeout_ms),
    };
    match state.health_service.dashboard_task_dispatch_once(params).await {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_dispatch_once",
                source_of_truth:
                    "CF_KV agent-task/v1 rows, CF_AGENT_EVENTS, session registry, and agent spawn artifacts",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_pause(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelinePauseRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::timeline_control::TimelinePauseParams {
        duration_ms: request.duration_ms,
    };
    match state.health_service.dashboard_timeline_pause(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_pause",
                source_of_truth: "CF_KV timeline recorder control row + live recorder gate",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_resume(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state
        .health_service
        .dashboard_timeline_resume(crate::m3::timeline_control::TimelineResumeParams::default())
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_resume",
                source_of_truth: "CF_KV timeline recorder control row + live recorder gate",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_get(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelineQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::timeline::TimelineGetParams {
        start_ts_ns,
        end_ts_ns,
        kinds: request.kinds,
        actor: request.actor,
        limit: request.limit,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_timeline_get(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_get",
                source_of_truth: "timeline_get bounded read over CF_TIMELINE",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_search(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelineQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::timeline::TimelineSearchParams {
        start_ts_ns,
        end_ts_ns,
        apps: request.apps,
        text: request.text,
        kinds: request.kinds,
        actor: request.actor,
        limit: request.limit,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_timeline_search(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_search",
                source_of_truth: "timeline_search scan over CF_TIMELINE",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_digest(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::timeline_digest::TimelineDigestParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_timeline_digest(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_digest",
                source_of_truth: "timeline_digest derived from CF_EPISODES plus CF_ROUTINES",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_episode_list(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardEpisodeListRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::episodes::EpisodeListParams {
        start_ts_ns,
        end_ts_ns,
        apps: request.apps,
        actor: request.actor,
        min_duration_ms: request.min_duration_ms,
        limit: request.limit,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_episode_list(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.episode_list",
                source_of_truth: "episode_list read over CF_EPISODES",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_episode_get(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardEpisodeGetRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::episodes::EpisodeGetParams {
        episode_id: request.episode_id,
        start_ts_ns,
        refs_limit: request.refs_limit,
        refs_cursor: request.refs_cursor,
    };
    match state.health_service.dashboard_episode_get(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.episode_get",
                source_of_truth: "episode_get read over CF_EPISODES plus CF_TIMELINE refs",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_routine_list(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::routines::RoutineListParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_routine_list(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.routine_list",
                source_of_truth: "routine_list read over CF_ROUTINES joined to CF_ROUTINE_STATE",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_routine_inspect(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::routines::RoutineInspectParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_routine_inspect(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.routine_inspect",
                source_of_truth: "routine_inspect read over CF_ROUTINES, CF_ROUTINE_STATE, and armed rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_routine_update(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::routines::RoutineUpdateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_routine_update(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.routine_update",
                source_of_truth: "routine_update write/readback over CF_ROUTINE_STATE and armed_routine/v1 rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

/// Parses an optional decimal epoch-nanosecond string into `Option<u64>`,
/// returning a `TOOL_PARAMS_INVALID` response on a malformed value. ns values
/// arrive as strings because they exceed JS `Number.MAX_SAFE_INTEGER`.
fn parse_optional_ns(value: Option<&str>, field: &str) -> Result<Option<u64>, Response> {
    let Some(raw) = value else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<u64>().map(Some).map_err(|error| {
        dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            &format!(
                "dashboard storage purge {field} must be a decimal nanosecond integer: {error}"
            ),
            None,
        )
    })
}

async fn dashboard_storage_timeline_purge(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelinePurgeRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::timeline::TimelinePurgeParams {
        start_ts_ns,
        end_ts_ns,
        apps: request.apps,
        text: None,
        kinds: request.kinds,
        actor: request.actor,
        flag_ids: None,
        all: request.all,
        dry_run: request.dry_run,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_timeline_purge(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_purge",
                source_of_truth: "CF_TIMELINE hard-delete + range compaction + counts-only purge audit row",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_storage_gc(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardStorageGcRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::storage::StorageGcOnceParams {
        cf_name: request.cf_name,
        soft_cap_rows: request.soft_cap_rows,
        hard_cap_rows: request.hard_cap_rows,
        run_id: request.run_id,
        now_ns: request.now_ns,
        max_age_ns: request.max_age_ns,
        dedupe_window_ns: request.dedupe_window_ns,
        profile_id: request.profile_id,
    };
    match state.health_service.dashboard_storage_gc(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.storage_gc_once",
                source_of_truth: "storage_gc_once oldest-row eviction over the column family",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_control_lease_force_release(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardControlLeaseForceReleaseRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (owner_session_id, confirmed) = match dashboard_control_lease_force_release_params(request)
    {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state
        .health_service
        .dashboard_control_lease_force_release(owner_session_id, confirmed)
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.control_lease_force_release",
                source_of_truth:
                    "synapse_action::lease + CF_KV MCP session lease rows + CF_AGENT_EVENTS + CF_ACTION_LOG",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_control_lease_handoff(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardControlLeaseHandoffRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (from_session_id, to_session_id, ttl_ms) =
        match dashboard_control_lease_handoff_params(request) {
            Ok(params) => params,
            Err(response) => return with_dashboard_security_headers(response),
        };
    match state
        .health_service
        .dashboard_control_lease_handoff(from_session_id, to_session_id, ttl_ms)
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.control_lease_handoff",
                source_of_truth:
                    "synapse_action::lease + CF_KV MCP session lease rows + CF_AGENT_EVENTS + CF_ACTION_LOG",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_target_claims_prune(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_target_claim_prune() {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.target_claims_prune",
                source_of_truth: "daemon target claim registry + CF_ACTION_LOG",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_template_list(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_list_agent_templates() {
        Ok(list) => with_dashboard_security_headers(
            Json(DashboardTemplateListResponse {
                ok: true,
                trigger: "dashboard.template_list",
                source_of_truth: "CF_KV agent-template/v2/cur/",
                list,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_template_upsert(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_templates::AgentTemplatePutParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_put_agent_template(params) {
        Ok(put) => with_dashboard_security_headers(
            Json(DashboardTemplateUpsertResponse {
                ok: true,
                trigger: "dashboard.template_upsert",
                source_of_truth: "CF_KV agent-template/v2/cur/",
                put,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_template_delete(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(template_id): Path<String>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state
        .health_service
        .dashboard_delete_agent_template(&template_id)
    {
        Ok(delete) => with_dashboard_security_headers(
            Json(DashboardTemplateDeleteResponse {
                ok: true,
                trigger: "dashboard.template_delete",
                source_of_truth: "CF_KV agent-template/v2/cur/",
                delete,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

fn dashboard_local_model_spawn_params(
    request: DashboardLocalModelSpawnRequest,
) -> Result<crate::m4::ActSpawnAgentParams, Response> {
    let model_ref = request.model_ref.trim();
    if model_ref.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard local-model spawn requires model_ref",
            None,
        ));
    }
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard local-model spawn requires prompt",
            None,
        ));
    }
    Ok(crate::m4::ActSpawnAgentParams {
        cli: None,
        kind: Some(crate::m4::ActSpawnAgentCli::LocalModel),
        model: None,
        model_ref: Some(model_ref.to_owned()),
        prompt: Some(prompt.to_owned()),
        target: None,
        working_dir: request
            .working_dir
            .and_then(|value| trim_optional_non_empty(&value)),
        mcp_url: crate::m4::default_agent_spawn_mcp_url(),
        wait_timeout_ms: request
            .wait_timeout_ms
            .unwrap_or_else(crate::m4::default_agent_spawn_wait_timeout_ms),
        hold_open_ms: request
            .hold_open_ms
            .unwrap_or_else(crate::m4::default_agent_spawn_hold_open_ms),
        // Local-model spawns have no permission-prompt-tool hook; the gate flag
        // is inert for them but kept at the default for struct completeness.
        require_approval_gate: crate::m4::default_require_approval_gate(),
        template_id: None,
        template_version: None,
        template_config_hash: None,
    })
}

fn dashboard_spawn_agent_request_params(
    request: DashboardSpawnAgentRequest,
) -> Result<(u32, crate::m4::ActSpawnAgentRequest), Response> {
    let fan_out = request.fan_out.unwrap_or(1);
    if fan_out == 0 || fan_out > DASHBOARD_SPAWN_FAN_OUT_MAX {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            &format!("dashboard spawn fan_out must be 1..={DASHBOARD_SPAWN_FAN_OUT_MAX}"),
            None,
        ));
    }
    Ok((
        fan_out,
        crate::m4::ActSpawnAgentRequest {
            template_id: request
                .template_id
                .and_then(|value| trim_optional_non_empty(&value)),
            template_version: request.template_version,
            template_params: request.template_params,
            cli: request.cli,
            kind: request.kind,
            model: request
                .model
                .and_then(|value| trim_optional_non_empty(&value)),
            model_ref: request
                .model_ref
                .and_then(|value| trim_optional_non_empty(&value)),
            prompt: request
                .prompt
                .and_then(|value| trim_optional_non_empty(&value)),
            target: request.target,
            working_dir: request
                .working_dir
                .and_then(|value| trim_optional_non_empty(&value)),
            mcp_url: crate::m4::default_agent_spawn_mcp_url(),
            wait_timeout_ms: request
                .wait_timeout_ms
                .unwrap_or_else(crate::m4::default_agent_spawn_wait_timeout_ms),
            hold_open_ms: request
                .hold_open_ms
                .unwrap_or_else(crate::m4::default_agent_spawn_hold_open_ms),
            require_approval_gate: request
                .require_approval_gate
                .unwrap_or_else(crate::m4::default_require_approval_gate),
        },
    ))
}

fn dashboard_agent_kill_params(
    request: DashboardAgentKillRequest,
) -> Result<crate::server::agent_control::AgentKillParams, Response> {
    let session_id = request.session_id.trim();
    if session_id.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard agent kill requires session_id or spawn_id",
            None,
        ));
    }
    Ok(crate::server::agent_control::AgentKillParams {
        session_id: session_id.to_owned(),
        grace_ms: request
            .grace_ms
            .unwrap_or(DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS),
        interrupt_first: request.interrupt_first.unwrap_or(true),
    })
}

fn dashboard_control_lease_force_release_params(
    request: DashboardControlLeaseForceReleaseRequest,
) -> Result<(String, bool), Response> {
    let owner_session_id = request.owner_session_id.trim();
    if owner_session_id.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease force-release requires owner_session_id",
            None,
        ));
    }
    if !request.confirmed {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease force-release requires confirmation",
            None,
        ));
    }
    Ok((owner_session_id.to_owned(), request.confirmed))
}

fn dashboard_control_lease_handoff_params(
    request: DashboardControlLeaseHandoffRequest,
) -> Result<(String, String, u64), Response> {
    let from_session_id = request.from_session_id.trim();
    let to_session_id = request.to_session_id.trim();
    if from_session_id.is_empty() || to_session_id.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease handoff requires from_session_id and to_session_id",
            None,
        ));
    }
    if from_session_id == to_session_id {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease handoff requires distinct sessions",
            None,
        ));
    }
    let ttl_ms = request
        .ttl_ms
        .unwrap_or(synapse_action::DEFAULT_LEASE_TTL_MS);
    if !(synapse_action::MIN_LEASE_TTL_MS..=synapse_action::MAX_LEASE_TTL_MS).contains(&ttl_ms) {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease handoff ttl_ms is outside the allowed lease range",
            None,
        ));
    }
    Ok((from_session_id.to_owned(), to_session_id.to_owned(), ttl_ms))
}

async fn dashboard_api_model_register(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardApiModelRegisterRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_api_model_register_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state
        .health_service
        .dashboard_register_api_model(params)
        .await
    {
        Ok(register) => with_dashboard_security_headers(
            Json(DashboardApiModelRegisterResponse {
                ok: true,
                trigger: "dashboard.api_model_register",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                register,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_api_model_update(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::local_models::LocalModelUpdateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state
        .health_service
        .dashboard_update_local_model(params)
        .await
    {
        Ok(update) => with_dashboard_security_headers(
            Json(DashboardModelUpdateResponse {
                ok: true,
                trigger: "dashboard.api_model_update",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                update,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_api_model_remove(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardModelRemoveRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::local_models::LocalModelRemoveParams { name: request.name };
    match state.health_service.dashboard_remove_local_model(params) {
        Ok(remove) => with_dashboard_security_headers(
            Json(DashboardModelRemoveResponse {
                ok: true,
                trigger: "dashboard.api_model_remove",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                remove,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_model_list(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_list_local_models() {
        Ok(list) => with_dashboard_security_headers(
            Json(DashboardModelListResponse {
                ok: true,
                trigger: "dashboard.model_list",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                list,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

fn dashboard_api_model_register_params(
    request: DashboardApiModelRegisterRequest,
) -> Result<crate::m3::local_models::LocalModelRegisterParams, Response> {
    fn require_non_empty(value: &str, field: &str) -> Result<String, Response> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                &format!("dashboard api-model register requires {field}"),
                None,
            ));
        }
        Ok(trimmed.to_owned())
    }
    let name = require_non_empty(&request.name, "name")?;
    let base_url = require_non_empty(&request.base_url, "base_url")?;
    let model_id = require_non_empty(&request.model_id, "model_id")?;
    let api_key_env_var = require_non_empty(&request.api_key_env_var, "api_key_env_var")?;
    Ok(crate::m3::local_models::LocalModelRegisterParams {
        name,
        base_url,
        model_id,
        // A remote cloud provider is OpenAI chat-completions over https; these
        // two settings are not the operator's to get wrong, so we fix them here.
        api_shape: crate::m3::local_models::LocalModelApiShape::OpenAiChatCompletions,
        runtime_preset: request.runtime_preset,
        context_length: request.context_length,
        max_tools: request.max_tools,
        notes: request
            .notes
            .and_then(|value| trim_optional_non_empty(&value)),
        enabled: true,
        allow_non_loopback: true,
        api_key_env_var: Some(api_key_env_var),
        // Optional plaintext key entered in the dashboard form. When present it
        // is DPAPI-encrypted at rest by register_local_model; never persisted in
        // plaintext and never echoed back.
        api_key: request
            .api_key
            .and_then(|value| trim_optional_non_empty(&value)),
        probe_timeout_ms: request.probe_timeout_ms,
    })
}

fn trim_optional_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn dashboard_error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    data: Option<serde_json::Value>,
) -> Response {
    (
        status,
        Json(serde_json::json!({
            "ok": false,
            "code": code,
            "message": message,
            "data": data,
        })),
    )
        .into_response()
}

fn dashboard_error_code(error: &rmcp::ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{:?}", error.code))
}

fn dashboard_context_resolve_live_session_id(
    state: &HttpState,
    requested_session_id: &str,
) -> Result<String, Response> {
    let requested = requested_session_id.trim();
    if requested.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "CONTEXT_TARGET_EMPTY",
            "session_id must be a non-empty live MCP session id or spawn id",
            None,
        ));
    }
    let sessions = state
        .health_service
        .session_list_impl(false)
        .map_err(|error| {
            dashboard_error_response(
                StatusCode::BAD_REQUEST,
                &dashboard_error_code(&error),
                &error.message,
                error.data,
            )
        })?;
    for row in sessions.sessions {
        if row.registry.session_id == requested {
            return Ok(row.registry.session_id);
        }
        if row
            .registry
            .spawned_agent
            .as_ref()
            .is_some_and(|spawn| spawn.spawn_id == requested)
        {
            return Ok(row.registry.session_id);
        }
    }
    Err(dashboard_error_response(
        StatusCode::BAD_REQUEST,
        "CONTEXT_TARGET_NOT_LIVE",
        &format!("context target {requested:?} is not a live session/spawn in session_list"),
        Some(serde_json::json!({
            "source_of_truth": "session_list live sessions",
            "requested_session_id": requested,
        })),
    ))
}

fn dashboard_payload_sha256(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| value.to_string().into_bytes());
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn dashboard_storage_error_response(
    message: &str,
    error: synapse_storage::StorageError,
) -> Response {
    tracing::error!(
        code = synapse_core::error_codes::STORAGE_READ_FAILED,
        detail = %error,
        "dashboard storage operation failed"
    );
    dashboard_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        synapse_core::error_codes::STORAGE_READ_FAILED,
        message,
        None,
    )
}

async fn approval_activate(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<crate::m3::approvals::ApprovalActivationParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    if params.bind != state.bind_addr.to_string() {
        return with_dashboard_security_headers(
            (StatusCode::BAD_REQUEST, "APPROVAL_ACTIVATION_BIND_MISMATCH").into_response(),
        );
    }
    match state
        .health_service
        .approval_decide_from_activation(&params, "approval_protocol")
    {
        Ok(response) => with_dashboard_security_headers(
            Html(approval_activation_html(&response)).into_response(),
        ),
        Err(error) => with_dashboard_security_headers(
            (
                StatusCode::BAD_REQUEST,
                format!("APPROVAL_ACTIVATION_FAILED: {}", error.message),
            )
                .into_response(),
        ),
    }
}

fn approval_activation_html(
    response: &crate::m3::approvals::ApprovalActivationDecisionResponse,
) -> String {
    let status = response.decision.after_status.as_str();
    format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<title>Synapse Approval</title>",
            "<link rel=\"stylesheet\" href=\"/dashboard/assets/{css_file}\">",
            "</head><body><h1>Synapse Approval</h1>",
            "<p>Approval <strong>{approval_id}</strong> is now <strong>{status}</strong>.</p>",
            "<p>Activation <code>{activation_id}</code> consumed.</p>",
            "</body></html>"
        ),
        css_file = DASHBOARD_CSS_FILE,
        approval_id = escape_html(&response.decision.approval_id),
        status = escape_html(status),
        activation_id = escape_html(&response.activation_id),
    )
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn dashboard_asset_response(content_type: &'static str, body: &'static str) -> Response {
    with_dashboard_security_headers(([(header::CONTENT_TYPE, content_type)], body).into_response())
}

fn with_dashboard_security_headers(mut response: Response) -> Response {
    const DASHBOARD_CSP: &str = concat!(
        "default-src 'none'; ",
        "base-uri 'none'; ",
        "object-src 'none'; ",
        "frame-ancestors 'none'; ",
        "form-action 'none'; ",
        "script-src 'self'; ",
        "style-src 'self'; ",
        "connect-src 'self'; ",
        "img-src 'self' data:; ",
        "font-src 'self'"
    );
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DASHBOARD_CSP),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(
            "camera=(), microphone=(), geolocation=(), payment=(), usb=(), serial=(), clipboard-read=(), clipboard-write=()",
        ),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(header::EXPIRES, HeaderValue::from_static("0"));
    response
}

fn dashboard_primary_session_list_data(
    sessions: impl Serialize,
    acked_attention_anchors: Result<BTreeSet<String>, rmcp::ErrorData>,
) -> serde_json::Value {
    let mut data = serde_json::to_value(sessions).unwrap_or_else(|error| {
        serde_json::json!({
            "serialization_error": error.to_string(),
        })
    });
    let Some(object) = data.as_object_mut() else {
        return data;
    };
    let Some(unbound_value) = object.remove("unbound_agent_states") else {
        return data;
    };
    let Some(unbound_rows) = unbound_value.as_array() else {
        object.insert("unbound_agent_states".to_owned(), unbound_value);
        return data;
    };
    let existing_terminal_rows = object
        .remove("terminal_unbound_agent_states")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();

    let mut primary_rows = Vec::new();
    let mut acknowledged_rows = Vec::new();
    let mut terminal_rows = existing_terminal_rows;
    let (acked_anchors, acked_anchor_error) = match acked_attention_anchors {
        Ok(anchors) => (anchors, None),
        Err(error) => (BTreeSet::new(), Some(error.message.to_string())),
    };
    for row in unbound_rows.iter().cloned() {
        if dashboard_agent_row_is_terminal(&row) {
            terminal_rows.push(row);
        } else if dashboard_agent_row_is_acknowledged_attention(&row, &acked_anchors) {
            acknowledged_rows.push(dashboard_mark_acknowledged_attention(row));
        } else {
            primary_rows.push(row);
        }
    }
    let primary_count = primary_rows.len();
    let acknowledged_count = acknowledged_rows.len();
    let terminal_count = terminal_rows.len();

    object.insert(
        "unbound_agent_states".to_owned(),
        serde_json::Value::Array(primary_rows),
    );
    object.insert(
        "acknowledged_unbound_agent_states".to_owned(),
        serde_json::Value::Array(acknowledged_rows),
    );
    object.insert(
        "terminal_unbound_agent_states".to_owned(),
        serde_json::Value::Array(terminal_rows),
    );
    object.insert(
        "dashboard_unbound_agent_filter".to_owned(),
        serde_json::json!({
            "source_of_truth": "session_list unbound_agent_states/terminal_unbound_agent_states + CF_KV escalation/v1/item acknowledged-open anchors split for dashboard attention feed",
            "primary_unbound_agent_count": primary_count,
            "acknowledged_unbound_agent_count": acknowledged_count,
            "terminal_unbound_agent_count": terminal_count,
            "terminal_states": ["dead", "done", "exited", "closed"],
            "terminal_attention_classes": ["terminal_setup_failure", "terminal_runtime_failure"],
            "acknowledged_attention_statuses": ["acked"],
            "acknowledged_attention_anchor_count": acked_anchors.len(),
            "acknowledged_attention_read_error": acked_anchor_error,
            "reason": "terminal unbound history is diagnostic history, not actionable attention",
        }),
    );
    data
}

fn dashboard_agent_row_is_terminal(row: &serde_json::Value) -> bool {
    if let Some(attention_class) = row
        .get("attention_class")
        .and_then(serde_json::Value::as_str)
    {
        return matches!(
            attention_class,
            "terminal_setup_failure" | "terminal_runtime_failure"
        );
    }
    row.get("state")
        .and_then(serde_json::Value::as_str)
        .map(|state| matches!(state, "dead" | "done" | "exited" | "closed"))
        .unwrap_or(false)
}

fn dashboard_agent_row_is_acknowledged_attention(
    row: &serde_json::Value,
    acked_anchors: &BTreeSet<String>,
) -> bool {
    let Some(state) = row.get("state").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if !matches!(
        state,
        "stuck" | "needs_input" | "awaiting_approval" | "ready_for_review"
    ) {
        return false;
    }
    dashboard_agent_row_anchor(row).is_some_and(|anchor| acked_anchors.contains(anchor))
}

fn dashboard_agent_row_anchor(row: &serde_json::Value) -> Option<&str> {
    row.get("anchor")
        .and_then(serde_json::Value::as_str)
        .or_else(|| row.get("spawn_id").and_then(serde_json::Value::as_str))
        .or_else(|| row.get("session_id").and_then(serde_json::Value::as_str))
}

fn dashboard_mark_acknowledged_attention(mut row: serde_json::Value) -> serde_json::Value {
    if let Some(object) = row.as_object_mut() {
        object.insert(
            "dashboard_attention_suppressed".to_owned(),
            serde_json::json!({
                "reason": "acknowledged_escalation",
                "source_of_truth": "CF_KV escalation/v1/item status=acked",
            }),
        );
    }
    row
}

fn dashboard_events_panel(state: &HttpState) -> DashboardPanel {
    let (owner_session_ids, owner_read_error) =
        match state.sse_state.subscription_owner_session_ids() {
            Ok(owner_session_ids) => (owner_session_ids, None),
            Err(error) => (Vec::new(), Some(format!("{error:?}"))),
        };
    DashboardPanel::ok(
        "SseState subscriptions + process-lifetime ingress counters",
        DashboardEventSurface {
            source_of_truth: "SseState subscriptions + process-lifetime ingress counters",
            active_subscription_count: state.sse_state.active_subscription_count(),
            owner_session_ids,
            owner_read_error,
            agent_event_ingress: crate::server::agent_event_ingress::ingress_stats(),
            agent_transcript_ingest: crate::server::agent_transcripts::ingest_stats(),
        },
    )
}

fn dashboard_hidden_desktops_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.hidden_desktop_readbacks() {
        Ok(rows) => DashboardPanel::ok(
            "session process resource ledger / hidden desktop leases",
            DashboardHiddenDesktopSurface {
                source_of_truth: "session process resource ledger / hidden desktop leases",
                row_count: rows.len(),
                rows,
            },
        ),
        Err(error) => DashboardPanel::error(
            "session process resource ledger / hidden desktop leases",
            format!("{error:?}"),
        ),
    }
}

fn dashboard_cdp_attachments_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.cdp_target_owner_readbacks() {
        Ok(rows) => DashboardPanel::ok(
            "CDP target ownership registry",
            DashboardCdpAttachmentSurface {
                source_of_truth: "CDP target ownership registry",
                row_count: rows.len(),
                rows,
            },
        ),
        Err(error) => DashboardPanel::error("CDP target ownership registry", format!("{error:?}")),
    }
}

fn dashboard_shell_jobs_panel() -> DashboardPanel {
    match crate::m4::shell_jobs_dashboard_snapshot(50) {
        Ok(snapshot) => DashboardPanel::ok(
            "act_run_shell_status + durable shell status files",
            snapshot,
        ),
        Err(error) => DashboardPanel::error(
            "act_run_shell_status + durable shell status files",
            format!("{error:?}"),
        ),
    }
}

fn context_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    let workspace = if tool_names.contains("workspace_list") {
        match state
            .health_service
            .dashboard_workspace_list_snapshot(None, 200, true)
        {
            Ok(list) => DashboardContextWorkspaceSurface {
                tool: "workspace_list",
                available: true,
                list: Some(list),
                error: None,
            },
            Err(error) => DashboardContextWorkspaceSurface {
                tool: "workspace_list",
                available: true,
                list: None,
                error: Some(format!("{error:?}")),
            },
        }
    } else {
        DashboardContextWorkspaceSurface {
            tool: "workspace_list",
            available: false,
            list: None,
            error: Some("workspace_list is not visible in the active tool profile".to_owned()),
        }
    };

    let mut inboxes = Vec::new();
    if tool_names.contains("agent_inbox") {
        match state.health_service.session_list_impl(false) {
            Ok(sessions) => {
                for row in sessions.sessions.iter().take(50) {
                    let spawn_id = row
                        .registry
                        .spawned_agent
                        .as_ref()
                        .map(|spawn| spawn.spawn_id.clone());
                    match state.health_service.dashboard_agent_inbox_snapshot(
                        &row.registry.session_id,
                        25,
                        Vec::new(),
                    ) {
                        Ok(inbox) => inboxes.push(DashboardContextInboxSurface {
                            session_id: row.registry.session_id.clone(),
                            spawn_id,
                            agent_kind: row.registry.agent_kind.clone(),
                            lifecycle: row.registry.lifecycle.clone(),
                            source_of_truth: "CF_KV agent-mailbox/v1 peek via agent_inbox drain=false",
                            inbox: Some(inbox),
                            error: None,
                        }),
                        Err(error) => inboxes.push(DashboardContextInboxSurface {
                            session_id: row.registry.session_id.clone(),
                            spawn_id,
                            agent_kind: row.registry.agent_kind.clone(),
                            lifecycle: row.registry.lifecycle.clone(),
                            source_of_truth: "CF_KV agent-mailbox/v1 peek via agent_inbox drain=false",
                            inbox: None,
                            error: Some(format!("{error:?}")),
                        }),
                    }
                }
            }
            Err(error) => {
                inboxes.push(DashboardContextInboxSurface {
                    session_id: "session_list".to_owned(),
                    spawn_id: None,
                    agent_kind: "dashboard".to_owned(),
                    lifecycle: "error".to_owned(),
                    source_of_truth: "session_list + CF_KV agent-mailbox/v1",
                    inbox: None,
                    error: Some(format!("{error:?}")),
                });
            }
        }
    }

    DashboardPanel::ok(
        "workspace_list + agent_inbox drain=false + session_list",
        DashboardContextSurface {
            source_of_truth: "workspace_list CF_KV workspace-blackboard/v1 + agent_inbox CF_KV agent-mailbox/v1 + session_list target/session rows",
            workspace,
            inboxes,
        },
    )
}

fn approval_panel(
    state: &HttpState,
    tool_names: &BTreeSet<&str>,
    kind: Option<crate::m3::approvals::ApprovalKind>,
) -> DashboardPanel {
    if !tool_names.contains("approval_list") {
        return deferred_panel("approval_list", tool_names);
    }
    match state.health_service.approval_queue_snapshot(kind) {
        Ok(rows) => DashboardPanel::ok(
            "approval_list",
            DashboardApprovalSurface {
                tool: "approval_list",
                available: true,
                rows,
            },
        ),
        Err(error) => DashboardPanel::error("approval_list", format!("{error:?}")),
    }
}

fn tasks_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    if !tool_names.contains("task_list") {
        return deferred_panel("task_list", tool_names);
    }
    let list = match state.health_service.dashboard_task_snapshot(1000) {
        Ok(list) => list,
        Err(error) => return DashboardPanel::error("task_list", format!("{error:?}")),
    };
    let next = match state
        .health_service
        .dashboard_task_next(crate::server::agent_tasks::default_cap())
    {
        Ok(next) => next,
        Err(error) => return DashboardPanel::error("task_next", format!("{error:?}")),
    };
    DashboardPanel::ok(
        "CF_KV agent-task/v1 via task_list",
        DashboardTaskSurface {
            tool: "task_list",
            available: true,
            source_of_truth: "CF_KV agent-task/v1",
            row_count: list.count,
            tasks: list.tasks,
            reconciled_orphans: list.reconciled_orphans,
            next,
        },
    )
}

fn local_model_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    if !tool_names.contains("local_model_list") {
        return deferred_panel("local_model_list", tool_names);
    }
    match state.health_service.local_model_registry_snapshot() {
        Ok(rows) => {
            let enabled_count = rows.iter().filter(|row| row.enabled).count();
            let unhealthy_count = rows
                .iter()
                .filter(|row| row.last_probe.as_ref().is_some_and(|probe| !probe.healthy))
                .count();
            DashboardPanel::ok(
                "local_model_list",
                DashboardLocalModelSurface {
                    tool: "local_model_list",
                    available: true,
                    enabled_count,
                    unhealthy_count,
                    rows,
                },
            )
        }
        Err(error) => DashboardPanel::error("local_model_list", format!("{error:?}")),
    }
}

fn agent_transcript_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.agent_transcript_snapshot(50) {
        Ok(rows) => DashboardPanel::ok(
            "CF_AGENT_TRANSCRIPTS",
            DashboardTranscriptSurface {
                source_of_truth: "CF_AGENT_TRANSCRIPTS",
                row_count: rows.len(),
                rows,
            },
        ),
        Err(error) => DashboardPanel::error("CF_AGENT_TRANSCRIPTS", format!("{error:?}")),
    }
}

fn command_audit_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.command_audit_snapshot() {
        Ok(snapshot) => DashboardPanel::ok("CF_ACTION_LOG command_audit", snapshot),
        Err(error) => DashboardPanel::error("CF_ACTION_LOG command_audit", format!("{error:?}")),
    }
}

fn hygiene_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    if !tool_names.contains("hygiene_report") {
        return deferred_panel("hygiene_report", tool_names);
    }
    match state.health_service.hygiene_report_snapshot(100) {
        Ok(response) => DashboardPanel::ok(
            "hygiene_report",
            DashboardHygieneSurface {
                tool: "hygiene_report",
                available: true,
                source_of_truth: "CF_KV hygiene/flag/v1 plus CF_EPISODES/CF_ROUTINES/CF_PROFILES joins",
                report: response,
            },
        ),
        Err(error) => DashboardPanel::error("hygiene_report", format!("{error:?}")),
    }
}

fn deferred_panel(tool: &'static str, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    DashboardPanel::unavailable(
        tool,
        DashboardDeferredSurface {
            tool,
            available: tool_names.contains(tool),
            rows: Vec::new(),
        },
    )
}

/// The dashboard's ONLY access control, by deliberate policy.
///
/// POLICY (binding — see STATE/DECISION_LOG.md, issues #892/#913): the local
/// dashboard must NEVER be locked behind an access token, login, cookie session,
/// or CSRF gate. Synapse is single-user on the operator's own machine; the OS
/// login is the trust boundary and an app-layer credential is pure friction that
/// has been removed twice. This guard is transport hardening, NOT authentication:
/// it requires a loopback bind and a loopback `Host` header (blocking DNS-rebind /
/// cross-origin reach) and nothing else. Do not add `Authorization`/cookie/token
/// checks to any `/dashboard/*` route.
fn dashboard_local_only(state: &HttpState, headers: &HeaderMap) -> Result<(), Response> {
    if !state.bind_addr.ip().is_loopback() {
        return Err((StatusCode::FORBIDDEN, "DASHBOARD_LOOPBACK_BIND_REQUIRED").into_response());
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return Err((StatusCode::FORBIDDEN, "DASHBOARD_HOST_REQUIRED").into_response());
    };
    if dashboard_host_allowed(host) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "DASHBOARD_HOST_REFUSED").into_response())
    }
}

fn dashboard_host_allowed(raw: &str) -> bool {
    let raw = raw.trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        raw.split(':').next().unwrap_or(raw)
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

fn dashboard_unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

const DASHBOARD_CSS_FILE: &str = "dashboard-BYyrzUYm.css";
const DASHBOARD_JS_FILE: &str = "dashboard-Bkan6uaj.js";
const DASHBOARD_HTML: &str = include_str!("../../../../dashboard/dist/index.html");
const DASHBOARD_CSS: &str =
    include_str!("../../../../dashboard/dist/assets/dashboard-BYyrzUYm.css");
const DASHBOARD_JS: &str = include_str!("../../../../dashboard/dist/assets/dashboard-Bkan6uaj.js");
#[cfg(test)]
const DASHBOARD_APP_SOURCE: &str = include_str!("../../../../dashboard/src/app.tsx");
#[cfg(test)]
const DASHBOARD_STATE_SOURCE: &str =
    include_str!("../../../../dashboard/src/lib/dashboard-state.ts");
#[cfg(test)]
const DASHBOARD_UTILS_SOURCE: &str = include_str!("../../../../dashboard/src/lib/utils.ts");
#[cfg(test)]
const DASHBOARD_PRIMITIVES_SOURCE: &str =
    include_str!("../../../../dashboard/src/primitives/index.tsx");
#[cfg(test)]
const DASHBOARD_CHARTER_CHECK_SOURCE: &str =
    include_str!("../../../../dashboard/scripts/check-dashboard-charter.ts");
async fn health(State(state): State<HttpState>) -> Json<Health> {
    tracing::info!(
        code = "MCP_HTTP_HEALTH",
        "tool.invocation kind=health transport=http"
    );
    let active_sessions = state.session_manager.sessions.read().await.len();
    Json(
        state
            .health_service
            .health_payload_with_http_sessions(Some(active_sessions)),
    )
}

async fn shutdown(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    let active_sessions = state.session_manager.sessions.read().await.len();
    let drain = state.drain_state.mark_draining("http_shutdown");
    let shutdown_cancel = state.shutdown_cancel.clone();
    tokio::spawn(async move {
        time::sleep(DRAIN_RESPONSE_GRACE_TIMEOUT).await;
        shutdown_cancel.cancel();
    });
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>");
    tracing::warn!(
        code = "MCP_HTTP_SHUTDOWN_REQUESTED",
        pid = std::process::id(),
        active_sessions,
        user_agent,
        "authenticated HTTP daemon shutdown requested"
    );
    tracing::info!(
        code = "MCP_SHUTDOWN_GRACEFUL",
        source = "http_shutdown",
        pid = std::process::id(),
        delay_ms = DRAIN_RESPONSE_GRACE_TIMEOUT.as_millis(),
        "HTTP shutdown endpoint scheduled daemon shutdown after drain grace"
    );
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "ok": true,
            "pid": std::process::id(),
            "shutdown": "requested",
            "drain": drain,
            "active_sessions_before_shutdown": active_sessions,
        })),
    )
        .into_response()
}

async fn events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<sse::EventsQuery>,
) -> Response {
    state.sse_state.open(&headers, query)
}

async fn publish_event(
    State(state): State<HttpState>,
    Json(request): Json<sse::PublishRequest>,
) -> Response {
    state.sse_state.publish(request)
}

async fn event_stats(
    State(state): State<HttpState>,
    Query(query): Query<sse::StatsQuery>,
) -> Response {
    state.sse_state.stats(&query)
}

/// Push-telemetry ingress (#899): spawned agents POST their native hook /
/// notify payloads here; the daemon normalizes them into `CF_AGENT_EVENTS`
/// rows. Authentication is enforced by the surrounding bearer middleware.
async fn agent_events_ingest(
    State(state): State<HttpState>,
    Query(query_pairs): Query<Vec<(String, String)>>,
    request: Request<Body>,
) -> Response {
    use crate::server::agent_event_ingress as ingress;

    let identity = match ingress::validate_ingress_identity(&query_pairs) {
        Ok(identity) => identity,
        Err(refusal) => return agent_events_refusal_response(&refusal),
    };
    let body = match axum::body::to_bytes(
        request.into_body(),
        ingress::MAX_AGENT_EVENT_INGRESS_BODY_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            return agent_events_refusal_response(&ingress::refuse_oversized_or_unreadable_body(
                &identity.spawn_id,
                &error.to_string(),
            ));
        }
    };
    match ingress::ingest_agent_event(&state.agent_events_db, &identity, &body) {
        Ok((readback, record)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "accepted": true,
                "kind": record.kind,
                "ts_ns": readback.ts_ns,
                "seq": readback.seq,
            })),
        )
            .into_response(),
        Err(refusal) => agent_events_refusal_response(&refusal),
    }
}

fn agent_events_refusal_response(
    refusal: &crate::server::agent_event_ingress::AgentEventIngressRefusal,
) -> Response {
    let status =
        StatusCode::from_u16(refusal.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(refusal.response_body())).into_response()
}

/// Codex app-server bridge: the PowerShell runner POSTs server-to-client
/// app-server requests here, this route blocks on the durable Synapse approval
/// row, then returns the app-server response payload the runner should send.
async fn codex_app_server_request(
    State(state): State<HttpState>,
    Json(request): Json<crate::server::codex_app_server_bridge::CodexAppServerRequestEnvelope>,
) -> Response {
    match crate::server::codex_app_server_bridge::handle_codex_app_server_request(
        &state.agent_events_db,
        request,
    )
    .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => {
            let status = StatusCode::from_u16(error.http_status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(error.response_body())).into_response()
        }
    }
}

/// Process-lifetime acceptance/rejection counters for the ingress, proving
/// "no silent drops" (#899 acceptance).
async fn agent_events_ingress_stats() -> Response {
    Json(crate::server::agent_event_ingress::ingress_stats()).into_response()
}

async fn agent_transcripts_ingest_stats() -> Response {
    Json(serde_json::json!({
        "spawn_dir": crate::server::agent_transcripts::ingest_stats(),
        "ambient": crate::server::ambient_agents::ingest_stats(),
    }))
    .into_response()
}

fn spawn_server(
    listener: TcpListener,
    app: Router,
    shutdown_cancel: CancellationToken,
) -> JoinHandle<io::Result<()>> {
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_cancel.cancelled_owned().await })
            .await
    })
}

async fn wait_for_server_stop(server_task: &mut JoinHandle<io::Result<()>>) -> anyhow::Result<()> {
    match tokio::time::timeout(Duration::from_secs(2), &mut *server_task).await {
        Ok(result) => {
            result
                .context("join stopped HTTP MCP transport")?
                .context("stop HTTP MCP transport")?;
        }
        Err(_elapsed) => {
            server_task.abort();
            tracing::warn!(
                code = "MCP_HTTP_SHUTDOWN_TIMEOUT",
                "HTTP transport did not stop within shutdown timeout"
            );
        }
    }
    Ok(())
}

async fn wait_for_m2_emitter_done(
    done: Option<watch::Receiver<Option<ActionStateSnapshot>>>,
    source: &'static str,
) {
    let Some(mut done) = done else {
        tracing::warn!(
            code = "MCP_M2_EMITTER_SHUTDOWN_UNOBSERVED",
            source,
            "M2 emitter final snapshot receiver was unavailable during HTTP shutdown"
        );
        return;
    };

    let result = time::timeout(M2_EMITTER_SHUTDOWN_TIMEOUT, async {
        loop {
            if done.borrow().is_some() {
                break;
            }
            if done.changed().await.is_err() {
                break;
            }
        }
    })
    .await;

    match (result, done.borrow().as_ref()) {
        (Ok(()), Some(snapshot)) => {
            tracing::info!(
                code = "MCP_M2_EMITTER_SHUTDOWN_DONE",
                source,
                held_keys = snapshot.held_keys.len(),
                held_buttons = snapshot.held_buttons.len(),
                held_pads = snapshot.pad_state.len(),
                held_key_timer_count = snapshot.held_key_timer_count,
                "readback=action_emitter_state edge=http_shutdown after_emitter_done"
            );
        }
        (Ok(()), None) => {
            tracing::warn!(
                code = "MCP_M2_EMITTER_SHUTDOWN_UNOBSERVED",
                source,
                "M2 emitter ended without publishing a final snapshot during HTTP shutdown"
            );
        }
        (Err(_elapsed), _) => {
            tracing::error!(
                code = "MCP_M2_EMITTER_SHUTDOWN_TIMEOUT",
                source,
                timeout_ms = M2_EMITTER_SHUTDOWN_TIMEOUT.as_millis(),
                "M2 emitter did not publish final shutdown snapshot before HTTP daemon exit"
            );
        }
    }
}

#[cfg(windows)]
async fn wait_for_shutdown_signal(phase: &'static str) -> anyhow::Result<()> {
    let mut ctrl_break = tokio::signal::windows::ctrl_break()
        .with_context(|| format!("register ctrl-break handler {phase}"))?;
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.with_context(|| format!("wait for ctrl-c {phase}"))?;
        }
        received = ctrl_break.recv() => {
            if received.is_none() {
                anyhow::bail!("ctrl-break stream ended while waiting for shutdown signal {phase}");
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
async fn wait_for_shutdown_signal(phase: &'static str) -> anyhow::Result<()> {
    tokio::signal::ctrl_c()
        .await
        .with_context(|| format!("wait for ctrl-c {phase}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use crate::test_support;
    use anyhow::Context as _;
    use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
    use rmcp::transport::streamable_http_server::session::SessionManager as _;
    use synapse_action::{ActionBackend, ActionEmitter, RecordingBackend};
    use synapse_core::{Action, Backend, Key, KeyCode, SCHEMA_VERSION};

    use super::*;

    const TEST_RESET_REASON: &str = "http_transport_lease_test_reset";

    #[test]
    fn dashboard_host_gate_accepts_loopback_only() {
        assert!(dashboard_host_allowed("127.0.0.1:7700"));
        assert!(dashboard_host_allowed("localhost:7700"));
        assert!(dashboard_host_allowed("[::1]:7700"));
        assert!(!dashboard_host_allowed("192.168.1.20:7700"));
        assert!(!dashboard_host_allowed("evil.example"));
    }

    #[test]
    fn dashboard_html_does_not_embed_bearer_material() {
        assert!(DASHBOARD_HTML.contains("Synapse Command Center"));
        assert!(!DASHBOARD_HTML.contains("Authorization"));
        assert!(!DASHBOARD_HTML.contains("Bearer"));
        assert!(!DASHBOARD_HTML.contains("SYNAPSE_BEARER_TOKEN"));
        assert!(!DASHBOARD_CSS.contains("Authorization"));
        assert!(!DASHBOARD_CSS.contains("Bearer"));
        assert!(!DASHBOARD_CSS.contains("SYNAPSE_BEARER_TOKEN"));
        assert!(!DASHBOARD_JS.contains("Authorization"));
        assert!(!DASHBOARD_JS.contains("Bearer"));
        assert!(!DASHBOARD_JS.contains("SYNAPSE_BEARER_TOKEN"));
    }

    #[test]
    fn dashboard_html_uses_external_assets_without_inline_blocks() {
        assert!(DASHBOARD_HTML.contains(&format!("/dashboard/assets/{DASHBOARD_CSS_FILE}")));
        assert!(DASHBOARD_HTML.contains(&format!("/dashboard/assets/{DASHBOARD_JS_FILE}")));
        assert!(DASHBOARD_HTML.contains("id=\"root\""));
        assert!(DASHBOARD_HTML.contains("<script type=\"module\""));
        assert!(!DASHBOARD_HTML.contains("<style"));
        assert!(!DASHBOARD_HTML.contains("src=\"http://"));
        assert!(!DASHBOARD_HTML.contains("src=\"https://"));
        assert!(!DASHBOARD_HTML.contains("href=\"http://"));
        assert!(!DASHBOARD_HTML.contains("href=\"https://"));
        assert!(!DASHBOARD_HTML.contains("<script>"));
    }

    #[test]
    fn dashboard_bundle_contains_asset_reload_contract() {
        assert!(DASHBOARD_STATE_SOURCE.contains("dashboardAssetReloadDecision"));
        assert!(DASHBOARD_STATE_SOURCE.contains("invalid_server_asset_id"));
        assert!(DASHBOARD_STATE_SOURCE.contains("_synapse_dashboard_asset"));
        assert!(DASHBOARD_APP_SOURCE.contains("claimDashboardAssetReload"));
        assert!(DASHBOARD_JS.contains("_synapse_dashboard_asset"));
        assert!(DASHBOARD_JS.contains("synapse.dashboard.asset-reload"));
        assert!(DASHBOARD_JS.contains("invalid_server_asset_id"));
    }

    #[test]
    fn dashboard_event_scope_filters_are_panel_scoped() {
        let agent_state = dashboard_scope_test_event(
            EventSource::System,
            crate::server::agent_state::AGENT_STATE_EVENT_KIND,
        );
        let profile_changed = dashboard_scope_test_event(EventSource::System, "profile-changed");
        let audit = dashboard_scope_test_event(EventSource::ActionEmitter, "command_finished");
        let filesystem = dashboard_scope_test_event(EventSource::Filesystem, "file_changed");
        let approval_request = dashboard_scope_test_event(
            EventSource::System,
            crate::server::APPROVAL_REQUEST_EVENT_KIND,
        );
        let approval_decision = dashboard_scope_test_event(
            EventSource::System,
            crate::server::APPROVAL_DECISION_EVENT_KIND,
        );
        let approval_timeout = dashboard_scope_test_event(
            EventSource::System,
            crate::server::APPROVAL_TIMEOUT_EVENT_KIND,
        );

        assert!(dashboard_scope_matches(
            DashboardEventScope::Fleet,
            &agent_state
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Agent,
            &profile_changed
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Tasks,
            &agent_state
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Fleet,
            &approval_request
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Fleet,
            &approval_decision
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Tasks,
            &approval_timeout
        ));
        assert!(dashboard_scope_matches(DashboardEventScope::Audit, &audit));
        assert!(dashboard_scope_matches(
            DashboardEventScope::System,
            &filesystem
        ));

        assert!(!dashboard_scope_matches(DashboardEventScope::Fleet, &audit));
        assert!(!dashboard_scope_matches(
            DashboardEventScope::Tasks,
            &profile_changed
        ));
        assert!(!dashboard_scope_matches(
            DashboardEventScope::Agent,
            &approval_request
        ));
        assert!(!dashboard_scope_matches(
            DashboardEventScope::Audit,
            &filesystem
        ));
    }

    #[test]
    fn dashboard_event_url_keeps_subscription_id_for_last_event_id_replay() {
        assert_eq!(
            dashboard_event_url("sub-01234567-89ab-cdef"),
            "/dashboard/events?subscription_id=sub-01234567-89ab-cdef"
        );
    }

    #[test]
    fn dashboard_bundle_contains_terminal_ws_contract() {
        assert!(DASHBOARD_APP_SOURCE.contains("/dashboard/agent-terminal/"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_CLIENT_PAUSE"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_CLIENT_RESUME"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_CLIENT_INPUT"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_SERVER_OUTPUT"));
        assert!(DASHBOARD_JS.contains("/dashboard/agent-terminal/"));
    }

    fn dashboard_scope_matches(scope: DashboardEventScope, event: &synapse_core::Event) -> bool {
        let (filter, kinds, _) = dashboard_event_subscription(scope);
        (kinds.is_empty() || kinds.iter().any(|kind| kind == &event.kind)) && filter.matches(event)
    }

    fn dashboard_scope_test_event(source: EventSource, kind: &str) -> synapse_core::Event {
        synapse_core::Event {
            seq: 1,
            at: chrono::Utc::now(),
            source,
            kind: kind.to_owned(),
            data: serde_json::json!({}),
            correlations: Vec::new(),
        }
    }

    #[test]
    fn terminal_ws_resize_payload_accepts_text_and_json() {
        assert_eq!(
            terminal_ws_parse_resize(b"120x40").expect("text resize"),
            (120, 40)
        );
        assert_eq!(
            terminal_ws_parse_resize(br#"{"cols":100,"rows":32}"#).expect("json resize"),
            (100, 32)
        );
        assert!(terminal_ws_parse_resize(b"0x40").is_err());
        assert!(terminal_ws_parse_resize(b"120").is_err());
    }

    #[test]
    fn terminal_ws_paused_buffer_preserves_order_and_caps_floods() {
        let mut frames = VecDeque::new();
        let mut bytes = 0usize;
        let first = terminal_ws_frame(TERMINAL_WS_SERVER_OUTPUT, b"first");
        let second = terminal_ws_frame(TERMINAL_WS_SERVER_OUTPUT, b"second");

        terminal_ws_buffer_paused_frame(&mut frames, &mut bytes, first.clone())
            .expect("first paused frame should buffer");
        terminal_ws_buffer_paused_frame(&mut frames, &mut bytes, second.clone())
            .expect("second paused frame should buffer");

        assert_eq!(bytes, first.len() + second.len());
        assert_eq!(frames.pop_front(), Some(first.clone()));
        assert_eq!(frames.pop_front(), Some(second.clone()));

        let mut near_limit = TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX;
        let mut full = VecDeque::new();
        full.push_back(vec![TERMINAL_WS_SERVER_OUTPUT; near_limit]);
        assert!(terminal_ws_buffer_paused_frame(&mut full, &mut near_limit, vec![b'x']).is_err());
        assert_eq!(near_limit, TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX);
        assert_eq!(full.len(), 1);
    }

    #[test]
    fn dashboard_session_feed_splits_terminal_unbound_history() {
        let source = serde_json::json!({
            "now_unix_ms": 10,
            "stale_after_ms": 300_000,
            "registry_entry_count": 1,
            "target_session_count": 0,
            "returned_count": 1,
            "input_lease_held": false,
            "sessions": [
                {
                    "session_id": "live-session",
                    "lifecycle": "live",
                    "agent_state": { "state": "idle" }
                }
            ],
            "unbound_agent_states": [
                {
                    "anchor": "agent-spawn-dead",
                    "spawn_id": "agent-spawn-dead",
                    "state": "dead",
                    "reason_code": "local_model_registry_row_missing",
                    "attention_class": "terminal_setup_failure"
                },
                {
                    "anchor": "agent-spawn-cleanup",
                    "spawn_id": "agent-spawn-cleanup",
                    "state": "dead",
                    "reason_code": "process_gone_without_exit_event",
                    "attention_class": "cleanup_required"
                },
                {
                    "anchor": "agent-spawn-stuck",
                    "spawn_id": "agent-spawn-stuck",
                    "state": "stuck",
                    "reason_code": "silent_timeout"
                },
                {
                    "anchor": "agent-spawn-acked-stuck",
                    "spawn_id": "agent-spawn-acked-stuck",
                    "state": "stuck",
                    "reason_code": "silent_timeout_unprobeable"
                },
                {
                    "anchor": "agent-spawn-needs-input",
                    "spawn_id": "agent-spawn-needs-input",
                    "state": "needs_input",
                    "reason_code": "permission_prompt"
                },
                "malformed-row"
            ]
        });

        let data = dashboard_primary_session_list_data(
            &source,
            Ok(BTreeSet::from(["agent-spawn-acked-stuck".to_owned()])),
        );
        let primary = data["unbound_agent_states"]
            .as_array()
            .expect("primary rows");
        let acknowledged = data["acknowledged_unbound_agent_states"]
            .as_array()
            .expect("acknowledged rows");
        let terminal = data["terminal_unbound_agent_states"]
            .as_array()
            .expect("terminal rows");

        assert_eq!(primary.len(), 4);
        assert_eq!(primary[0]["anchor"], "agent-spawn-cleanup");
        assert_eq!(primary[0]["attention_class"], "cleanup_required");
        assert_eq!(primary[1]["state"], "stuck");
        assert_eq!(primary[2]["state"], "needs_input");
        assert_eq!(primary[3], "malformed-row");
        assert_eq!(acknowledged.len(), 1);
        assert_eq!(acknowledged[0]["anchor"], "agent-spawn-acked-stuck");
        assert_eq!(
            acknowledged[0]["dashboard_attention_suppressed"]["reason"],
            "acknowledged_escalation"
        );
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0]["anchor"], "agent-spawn-dead");
        assert_eq!(terminal[0]["attention_class"], "terminal_setup_failure");
        assert_eq!(
            data["dashboard_unbound_agent_filter"]["acknowledged_unbound_agent_count"],
            1
        );
        assert_eq!(
            data["dashboard_unbound_agent_filter"]["terminal_unbound_agent_count"],
            1
        );
    }

    #[test]
    fn dashboard_security_headers_disallow_inline_script_and_eval() {
        let response = with_dashboard_security_headers(Html("").into_response());
        let csp = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .expect("CSP header present");
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains("style-src 'self'"));
        assert!(!csp.contains("'unsafe-inline'"));
        assert!(!csp.contains("'unsafe-eval'"));
        assert_eq!(
            response
                .headers()
                .get(HeaderName::from_static("x-content-type-options"))
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store, max-age=0")
        );
    }

    #[test]
    fn dashboard_source_uses_charter_guardrails() {
        let source = [
            DASHBOARD_APP_SOURCE,
            DASHBOARD_STATE_SOURCE,
            DASHBOARD_UTILS_SOURCE,
            DASHBOARD_PRIMITIVES_SOURCE,
        ]
        .join("\n");
        assert!(source.contains("stripTerminalSequences"));
        assert!(source.contains("ReactMarkdown"));
        assert!(source.contains("rehypeSanitize"));
        assert!(source.contains("RawValue"));
        assert!(source.contains("Section"));
        assert!(source.contains("/dashboard/state.json"));
        assert!(source.contains("cache: \"no-store\""));
        assert!(DASHBOARD_CHARTER_CHECK_SOURCE.contains("dangerouslySetInnerHTML"));
        assert!(DASHBOARD_CHARTER_CHECK_SOURCE.contains("insertAdjacentHTML"));
        assert!(DASHBOARD_CHARTER_CHECK_SOURCE.contains("every Section must declare questions"));
        assert!(
            DASHBOARD_CHARTER_CHECK_SOURCE.contains("RawValue disclosure must not default open")
        );
        assert!(!source.contains("dangerouslySetInnerHTML"));
        assert!(!source.contains(".innerHTML"));
        assert!(!source.contains("insertAdjacentHTML"));
        assert!(!source.contains("new Function"));
        assert!(!source.contains("eval("));
    }

    #[test]
    fn dashboard_local_model_spawn_params_force_local_model_kind() {
        let params = dashboard_local_model_spawn_params(DashboardLocalModelSpawnRequest {
            model_ref: " ollama-gemma4-e4b ".to_owned(),
            prompt: " write known result ".to_owned(),
            working_dir: Some(" C:\\code\\Synapse ".to_owned()),
            wait_timeout_ms: Some(300_000),
            hold_open_ms: Some(0),
        })
        .expect("valid dashboard local model spawn params");

        assert_eq!(params.cli, None);
        assert_eq!(params.kind, Some(crate::m4::ActSpawnAgentCli::LocalModel));
        assert_eq!(params.model_ref.as_deref(), Some("ollama-gemma4-e4b"));
        assert_eq!(params.prompt.as_deref(), Some("write known result"));
        assert_eq!(params.working_dir.as_deref(), Some("C:\\code\\Synapse"));
        assert_eq!(params.wait_timeout_ms, 300_000);
        assert_eq!(params.hold_open_ms, 0);
    }

    fn empty_dashboard_spawn_agent_request() -> DashboardSpawnAgentRequest {
        DashboardSpawnAgentRequest {
            fan_out: None,
            template_id: None,
            template_version: None,
            template_params: BTreeMap::new(),
            cli: None,
            kind: None,
            model: None,
            model_ref: None,
            prompt: None,
            target: None,
            working_dir: None,
            wait_timeout_ms: None,
            hold_open_ms: None,
            require_approval_gate: None,
        }
    }

    #[test]
    fn dashboard_spawn_agent_request_params_preserves_template_fanout() {
        let mut request = empty_dashboard_spawn_agent_request();
        request.fan_out = Some(5);
        request.template_id = Some(" issue923-template ".to_owned());
        request.template_version = Some(7);
        request
            .template_params
            .insert("task".to_owned(), "write-known-row".to_owned());

        let (fan_out, spawn) =
            dashboard_spawn_agent_request_params(request).expect("valid template spawn params");

        assert_eq!(fan_out, 5);
        assert_eq!(spawn.template_id.as_deref(), Some("issue923-template"));
        assert_eq!(spawn.template_version, Some(7));
        assert_eq!(
            spawn.template_params.get("task").map(String::as_str),
            Some("write-known-row")
        );
        assert_eq!(spawn.kind, None);
        assert_eq!(spawn.model_ref, None);
        assert_eq!(
            spawn.wait_timeout_ms,
            crate::m4::default_agent_spawn_wait_timeout_ms()
        );
        assert_eq!(
            spawn.hold_open_ms,
            crate::m4::default_agent_spawn_hold_open_ms()
        );
    }

    #[test]
    fn dashboard_spawn_agent_request_params_trims_direct_spawn_and_target() {
        let mut request = empty_dashboard_spawn_agent_request();
        request.kind = Some(crate::m4::ActSpawnAgentCli::Codex);
        request.model = Some(" gpt-5-codex ".to_owned());
        request.prompt = Some(" write known row ".to_owned());
        request.working_dir = Some(" C:\\code\\Synapse ".to_owned());
        request.target = Some(crate::m4::ActSpawnAgentTarget::Window {
            window_hwnd: 1116654,
        });
        request.wait_timeout_ms = Some(42_000);
        request.hold_open_ms = Some(0);

        let (fan_out, spawn) =
            dashboard_spawn_agent_request_params(request).expect("valid direct spawn params");

        assert_eq!(fan_out, 1);
        assert_eq!(spawn.kind, Some(crate::m4::ActSpawnAgentCli::Codex));
        assert_eq!(spawn.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(spawn.prompt.as_deref(), Some("write known row"));
        assert_eq!(spawn.working_dir.as_deref(), Some("C:\\code\\Synapse"));
        assert_eq!(
            spawn.target,
            Some(crate::m4::ActSpawnAgentTarget::Window {
                window_hwnd: 1116654
            })
        );
        assert_eq!(spawn.wait_timeout_ms, 42_000);
        assert_eq!(spawn.hold_open_ms, 0);
    }

    #[test]
    fn dashboard_spawn_agent_request_params_rejects_invalid_fanout() {
        for fan_out in [0, DASHBOARD_SPAWN_FAN_OUT_MAX + 1] {
            let mut request = empty_dashboard_spawn_agent_request();
            request.fan_out = Some(fan_out);
            let response = dashboard_spawn_agent_request_params(request)
                .expect_err("invalid dashboard fan-out should fail closed");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn dashboard_agent_kill_params_trim_id_and_keep_options() {
        let params = dashboard_agent_kill_params(DashboardAgentKillRequest {
            session_id: " agent-spawn-issue923 ".to_owned(),
            grace_ms: Some(0),
            interrupt_first: Some(false),
        })
        .expect("valid dashboard kill params");

        assert_eq!(params.session_id, "agent-spawn-issue923");
        assert_eq!(params.grace_ms, 0);
        assert!(!params.interrupt_first);
    }

    #[test]
    fn dashboard_agent_kill_params_reject_empty_id() {
        let response = dashboard_agent_kill_params(DashboardAgentKillRequest {
            session_id: "   ".to_owned(),
            grace_ms: None,
            interrupt_first: None,
        })
        .expect_err("empty dashboard kill id should fail closed");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn dashboard_control_lease_force_release_params_trim_owner() {
        let (owner_session_id, confirmed) = dashboard_control_lease_force_release_params(
            DashboardControlLeaseForceReleaseRequest {
                owner_session_id: " lease-owner-session ".to_owned(),
                confirmed: true,
            },
        )
        .expect("valid dashboard force-release params");

        assert_eq!(owner_session_id, "lease-owner-session");
        assert!(confirmed);
    }

    #[test]
    fn dashboard_control_lease_force_release_params_require_confirmation() {
        let response = dashboard_control_lease_force_release_params(
            DashboardControlLeaseForceReleaseRequest {
                owner_session_id: "lease-owner-session".to_owned(),
                confirmed: false,
            },
        )
        .expect_err("unconfirmed dashboard force-release should fail closed");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn dashboard_control_lease_handoff_params_trim_sessions_and_default_ttl() {
        let (from_session_id, to_session_id, ttl_ms) =
            dashboard_control_lease_handoff_params(DashboardControlLeaseHandoffRequest {
                from_session_id: " from-session ".to_owned(),
                to_session_id: " to-session ".to_owned(),
                ttl_ms: None,
            })
            .expect("valid dashboard handoff params");

        assert_eq!(from_session_id, "from-session");
        assert_eq!(to_session_id, "to-session");
        assert_eq!(ttl_ms, synapse_action::DEFAULT_LEASE_TTL_MS);
    }

    #[test]
    fn dashboard_control_lease_handoff_params_reject_bad_sessions_and_ttl() {
        let same_session =
            dashboard_control_lease_handoff_params(DashboardControlLeaseHandoffRequest {
                from_session_id: "same".to_owned(),
                to_session_id: " same ".to_owned(),
                ttl_ms: Some(synapse_action::DEFAULT_LEASE_TTL_MS),
            })
            .expect_err("same session handoff should fail closed");
        assert_eq!(same_session.status(), StatusCode::BAD_REQUEST);

        let bad_ttl = dashboard_control_lease_handoff_params(DashboardControlLeaseHandoffRequest {
            from_session_id: "from".to_owned(),
            to_session_id: "to".to_owned(),
            ttl_ms: Some(synapse_action::MIN_LEASE_TTL_MS - 1),
        })
        .expect_err("out-of-range dashboard handoff ttl should fail closed");
        assert_eq!(bad_ttl.status(), StatusCode::BAD_REQUEST);
    }

    fn test_session_state(name: &str) -> SessionState {
        SessionState::new(InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new(name, "0.0.0-test"),
        ))
    }

    fn test_store_error(error: SessionStoreError) -> anyhow::Error {
        anyhow::anyhow!("{error}")
    }

    fn empty_cdp_target_owners() -> crate::server::SharedCdpTargetOwners {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn empty_session_registry() -> crate::server::session_registry::SharedSessionRegistry {
        Arc::new(Mutex::new(
            crate::server::session_registry::SessionRegistry::default(),
        ))
    }

    #[test]
    fn shutdown_cleanup_session_ids_include_live_spawned_sessions() {
        let active = BTreeSet::from(["active".to_owned(), "both".to_owned()]);
        let live_spawns = BTreeSet::from(["both".to_owned(), "idle-spawn".to_owned()]);

        let cleanup = shutdown_cleanup_session_ids(&active, &live_spawns);

        assert_eq!(
            cleanup,
            BTreeSet::from([
                "active".to_owned(),
                "both".to_owned(),
                "idle-spawn".to_owned()
            ])
        );
    }

    #[tokio::test]
    async fn synapse_mcp_session_store_round_trips_exact_keys_and_deletes() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
        let store = SynapseMcpSessionStore::new(
            Arc::clone(&db),
            Some(Duration::from_mins(5)),
            empty_session_registry(),
        );

        assert!(
            store
                .load("codex-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "unknown session should not load"
        );

        let state = test_session_state("codex-test");
        let neighboring_state = test_session_state("codex-test-neighbor");
        store
            .store("codex-session", &state)
            .await
            .map_err(test_store_error)?;
        let stored_rows = db.scan_cf_prefix(cf::CF_KV, &mcp_session_store_key("codex-session"))?;
        let stored_row = stored_rows
            .iter()
            .find(|(key, _value)| key == &mcp_session_store_key("codex-session"))
            .context("stored row should exist in CF_KV")?;
        let persisted = synapse_storage::decode_json::<PersistedMcpSessionState>(&stored_row.1)?;
        assert_eq!(persisted.state.initialize_params, state.initialize_params);

        store
            .store("codex-session-extra", &neighboring_state)
            .await
            .map_err(test_store_error)?;

        let loaded = store
            .load("codex-session")
            .await
            .map_err(test_store_error)?
            .context("stored session should load")?;
        assert_eq!(loaded.initialize_params, state.initialize_params);

        store
            .delete("codex-session")
            .await
            .map_err(test_store_error)?;
        assert!(
            store
                .load("codex-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "deleted session should not load"
        );
        assert!(
            store
                .load("codex-session-extra")
                .await
                .map_err(test_store_error)?
                .is_some(),
            "deleting one session should not delete a prefix-sharing neighbor"
        );

        Ok(())
    }

    #[tokio::test]
    async fn synapse_mcp_session_store_deletes_expired_rows() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
        let store = SynapseMcpSessionStore::new(
            Arc::clone(&db),
            Some(Duration::from_millis(1)),
            empty_session_registry(),
        );
        let key = mcp_session_store_key("expired-session");

        store
            .store("expired-session", &test_session_state("expired-test"))
            .await
            .map_err(test_store_error)?;
        assert!(
            db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "stored row should physically exist before expiry"
        );

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(
            store
                .load("expired-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "expired session should not load"
        );
        assert!(
            !db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "expired session row should be deleted from CF_KV"
        );

        Ok(())
    }

    #[tokio::test]
    async fn synapse_mcp_session_store_deletes_legacy_rows_without_ttl() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
        let store = SynapseMcpSessionStore::new(
            Arc::clone(&db),
            Some(Duration::from_mins(5)),
            empty_session_registry(),
        );
        let key = mcp_session_store_key("legacy-session");
        let legacy_state = test_session_state("legacy-test");
        let legacy_encoded = synapse_storage::encode_json(&legacy_state)?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(key.clone(), legacy_encoded)])?;

        assert!(
            db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "legacy row should physically exist before load"
        );

        assert!(
            store
                .load("legacy-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "legacy row without persistent TTL metadata should not load"
        );
        assert!(
            !db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "legacy session row should be deleted from CF_KV"
        );

        Ok(())
    }

    #[tokio::test]
    async fn stale_session_cleanup_releases_absent_inputs_only() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let session_manager = Arc::new(LocalSessionManager::default());
        let (active_session_id, _active_transport) = session_manager
            .create_session()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let active_session_text = active_session_id.as_ref().to_owned();
        let stale_session_id = "stale-session".to_owned();
        let _prior = synapse_action::lease::force_clear("http_stale_inputs_test_reset");
        let _held = synapse_action::lease::try_acquire(&stale_session_id, Duration::from_secs(30));

        handle
            .with_session_id(Some(stale_session_id.clone()))
            .execute(Action::KeyDown {
                key: test_key("ctrl"),
                backend: Backend::Software,
            })
            .await?;
        handle
            .with_session_id(Some(active_session_text.clone()))
            .execute(Action::KeyDown {
                key: test_key("shift"),
                backend: Backend::Software,
            })
            .await?;

        let before_state = snapshot_handle.snapshot().await?;
        let before_ownership = handle.session_inputs_snapshot()?;
        let before_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_owner before_state={before_state:?} before_ownership={before_ownership:?} before_lease={before_lease:?} active_session_id={active_session_text}"
        );
        assert_eq!(
            before_lease.owner_session_id.as_deref(),
            Some(stale_session_id.as_str())
        );

        let cdp_target_owners = empty_cdp_target_owners();
        cleanup_stale_session_inputs_once(&handle, &session_manager, &cdp_target_owners).await;

        let after_state = snapshot_handle.snapshot().await?;
        let after_ownership = handle.session_inputs_snapshot()?;
        let after_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_owner after_state={after_state:?} after_ownership={after_ownership:?} after_lease={after_lease:?}"
        );

        assert_eq!(after_state.held_keys, vec![test_key("shift")]);
        assert!(!after_lease.held);
        assert!(
            after_ownership
                .sessions
                .iter()
                .any(|session| session.session_id == active_session_text),
            "active session ownership should be retained"
        );
        assert!(
            !after_ownership
                .sessions
                .iter()
                .any(|session| session.session_id == stale_session_id),
            "stale session ownership should be removed"
        );

        session_manager
            .close_session(&active_session_id)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        handle.execute(Action::ReleaseAll).await?;
        cancel.cancel();
        let final_snapshot = join.await?;
        assert!(final_snapshot.held_keys.is_empty());
        let _prior = synapse_action::lease::force_clear("http_stale_inputs_test_reset");

        Ok(())
    }

    #[tokio::test]
    async fn stale_session_cleanup_releases_absent_lease_without_inputs() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let session_manager = LocalSessionManager::default();
        let stale_session_id = "stale-lease-session";
        let _prior = synapse_action::lease::force_clear("http_stale_lease_test_reset");
        let _held = synapse_action::lease::try_acquire(stale_session_id, Duration::from_secs(30));

        let before_state = snapshot_handle.snapshot().await?;
        let before_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_lease before_state={before_state:?} before_lease={before_lease:?}"
        );

        let cdp_target_owners = empty_cdp_target_owners();
        cleanup_stale_session_inputs_once(&handle, &session_manager, &cdp_target_owners).await;

        let after_state = snapshot_handle.snapshot().await?;
        let after_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_lease after_state={after_state:?} after_lease={after_lease:?}"
        );
        assert!(!after_lease.held);
        assert_eq!(after_lease.owner_session_id, None);

        cancel.cancel();
        let final_snapshot = join.await?;
        assert!(final_snapshot.held_keys.is_empty());
        let _prior = synapse_action::lease::force_clear("http_stale_lease_test_reset");

        Ok(())
    }

    #[tokio::test]
    async fn expired_lease_cleanup_releases_held_input_before_reacquire() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let session_manager = LocalSessionManager::default();
        let expired_session_id = "expired-lease-session";
        let contender_session_id = "expired-lease-contender";
        let _prior = synapse_action::lease::force_clear("http_expired_lease_test_reset");

        handle
            .with_session_id(Some(expired_session_id.to_owned()))
            .execute(Action::KeyDown {
                key: test_key("ctrl"),
                backend: Backend::Software,
            })
            .await?;
        let _held = synapse_action::lease::try_acquire(
            expired_session_id,
            Duration::from_millis(synapse_action::MIN_LEASE_TTL_MS),
        );
        tokio::time::sleep(Duration::from_millis(synapse_action::MIN_LEASE_TTL_MS + 50)).await;

        let before_state = snapshot_handle.snapshot().await?;
        let before_ownership = handle.session_inputs_snapshot()?;
        let before_lease = synapse_action::lease::status();
        let before_pending = synapse_action::lease::expired_cleanup_snapshot();
        println!(
            "readback=http_session_cleanup edge=expired_lease before_state={before_state:?} before_ownership={before_ownership:?} before_lease={before_lease:?} before_pending={before_pending:?}"
        );
        assert_eq!(before_state.held_keys, vec![test_key("ctrl")]);
        assert!(!before_lease.held);
        assert_eq!(before_pending.len(), 1);
        match synapse_action::lease::try_acquire(contender_session_id, Duration::from_secs(30)) {
            synapse_action::LeaseOutcome::CleanupPending { expired, .. } => {
                assert_eq!(
                    expired.owner_session_id.as_deref(),
                    Some(expired_session_id)
                );
            }
            other => anyhow::bail!("contender should be refused pending cleanup, got {other:?}"),
        }

        let cdp_target_owners = empty_cdp_target_owners();
        cleanup_stale_session_inputs_once(&handle, &session_manager, &cdp_target_owners).await;

        let after_state = snapshot_handle.snapshot().await?;
        let after_ownership = handle.session_inputs_snapshot()?;
        let after_pending = synapse_action::lease::expired_cleanup_snapshot();
        let acquire_after_cleanup =
            synapse_action::lease::try_acquire(contender_session_id, Duration::from_secs(30));
        let after_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=expired_lease after_state={after_state:?} after_ownership={after_ownership:?} after_pending={after_pending:?} acquire_after_cleanup={acquire_after_cleanup:?} after_lease={after_lease:?}"
        );
        assert!(after_state.held_keys.is_empty());
        assert!(after_ownership.sessions.is_empty());
        assert!(after_pending.is_empty());
        assert!(matches!(
            acquire_after_cleanup,
            synapse_action::LeaseOutcome::Acquired(_)
        ));
        assert_eq!(
            after_lease.owner_session_id.as_deref(),
            Some(contender_session_id)
        );

        let _released = synapse_action::lease::release_if_owner(contender_session_id);
        cancel.cancel();
        let final_snapshot = join.await?;
        assert!(final_snapshot.held_keys.is_empty());
        let _prior = synapse_action::lease::force_clear("http_expired_lease_test_reset");

        Ok(())
    }

    fn test_key(value: &str) -> Key {
        Key {
            code: KeyCode::Named {
                value: value.to_owned(),
            },
            use_scancode: false,
        }
    }
}
