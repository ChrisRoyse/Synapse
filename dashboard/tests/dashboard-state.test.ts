import assert from "node:assert/strict";
import { describe, test } from "node:test";

import {
  buildAgents,
  buildTargetDashboardRows,
  buildTaskRows,
  claimDashboardAssetReload,
  dashboardAssetReloadDecision,
  dashboardAssetReloadUrl,
  localModelLaunchReady,
  localModelProbeLabel,
  modelRegistryStatus,
  shellJobCommandLabel,
  shellJobEvidence,
  shellJobNeedsReview,
  type DashboardPanel,
  type DashboardState,
  type ModelRow
} from "../src/lib/dashboard-state";

function panel<T = unknown>(data: T): DashboardPanel<T> {
  return { status: "ok" as const, source: "test", data };
}

function dashboardState(sessionsData: unknown): DashboardState {
  return {
    schema_version: 1,
    generated_at_unix_ms: 1,
    bind_addr: "127.0.0.1:7700",
    token_policy: "test",
    auth: panel({}),
    daemon: panel({}),
    sessions: panel(sessionsData),
    lease: panel({}),
    storage: panel({}),
    target_claims: panel({}),
    timeline: panel({}),
    events: panel({}),
    hidden_desktops: panel({}),
    cdp_attachments: panel({}),
    shell_jobs: panel({}),
    command_audit: panel({ rows: [] }),
    approvals: panel({}),
    suggestions: panel({}),
    armed_runs: panel({}),
    agent_transcripts: panel({ rows: [] }),
    context: panel({ workspace: { list: { entries: [] } }, inboxes: [] }),
    hygiene: panel({}),
    local_models: panel({})
  };
}

function liveSession(state: string, reason = "session_initialized", lastSeenMs = 900_000) {
  return {
    session_id: `session-${state}-${reason}`,
    lifecycle: "live",
    agent_kind: "codex",
    last_seen_ms_ago: lastSeenMs,
    last_action: "tools/call:health",
    agent_state: {
      state,
      reason_code: reason
    }
  };
}

function dashboardStateWithAsset(jsFile: string): DashboardState {
  const state = dashboardState({
    sessions: [],
    unbound_agent_states: []
  });
  state.dashboard_assets = panel({
    schema_version: 1,
    source_of_truth: "test asset surface",
    js_file: jsFile,
    css_file: "dashboard-test.css"
  });
  return state;
}

function installDashboardDom(currentJsFile: string, href = "http://127.0.0.1:7700/dashboard#/fleet") {
  const storage = new Map<string, string>();
  const fakeWindow = {
    location: { href },
    sessionStorage: {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => {
        storage.set(key, value);
      }
    }
  };
  const fakeDocument = {
    scripts: [
      {
        getAttribute: (name: string) => (name === "src" ? `/dashboard/assets/${currentJsFile}` : null)
      }
    ]
  };
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: fakeWindow
  });
  Object.defineProperty(globalThis, "document", {
    configurable: true,
    value: fakeDocument
  });
  return { storage, fakeWindow };
}

const MODEL_STATUS_NOW_MS = 1_782_400_000_000;

function registryRow(overrides: Partial<ModelRow> = {}): ModelRow {
  return {
    name: "deepseek-flash",
    base_url: "https://api.deepseek.com",
    model_id: "deepseek-v4-flash",
    api_shape: "open_ai_compatible",
    runtime_preset: "deepseek_v4_flash_non_thinking",
    enabled: true,
    allow_non_loopback: true,
    api_key_env_var: "DEEPSEEK_API_KEY",
    has_api_key_secret: true,
    last_probe: {
      healthy: true,
      status: "ok",
      latency_ms: 250,
      observed_at_unix_ms: MODEL_STATUS_NOW_MS - 30_000
    },
    ...overrides
  };
}

describe("dashboard asset reload contract", () => {
  test("reloads when URL marker names the expected bundle but the running script is stale", () => {
    const { storage } = installDashboardDom(
      "dashboard-old.js",
      "http://127.0.0.1:7700/dashboard?_synapse_dashboard_asset=dashboard-new.js#/fleet"
    );
    const decision = dashboardAssetReloadDecision(dashboardStateWithAsset("dashboard-new.js"));

    assert.equal(decision.reason, "asset_mismatch");
    assert.equal(claimDashboardAssetReload(decision), true);
    assert.match(storage.get("synapse.dashboard.asset-reload") || "", /"attempts":1/);
    assert.equal(
      dashboardAssetReloadUrl("dashboard-new.js"),
      "http://127.0.0.1:7700/dashboard?_synapse_dashboard_asset=dashboard-new.js&_synapse_dashboard_asset_attempt=1#/fleet"
    );
  });

  test("fails closed after bounded reload attempts for the same stale bundle transition", () => {
    const { storage } = installDashboardDom(
      "dashboard-old.js",
      "http://127.0.0.1:7700/dashboard?_synapse_dashboard_asset=dashboard-new.js&_synapse_dashboard_asset_attempt=2#/fleet"
    );
    storage.set(
      "synapse.dashboard.asset-reload",
      JSON.stringify({ reloadKey: "dashboard-old.js->dashboard-new.js", attempts: 2 })
    );
    const decision = dashboardAssetReloadDecision(dashboardStateWithAsset("dashboard-new.js"));
    const originalConsoleError = console.error;
    console.error = () => undefined;
    try {
      assert.throws(
        () => claimDashboardAssetReload(decision),
        /DASHBOARD_ASSET_RELOAD_EXHAUSTED: current=dashboard-old\.js: expected=dashboard-new\.js: attempts=2/
      );
    } finally {
      console.error = originalConsoleError;
    }
  });
});

describe("system dashboard diagnostics", () => {
  test("separates live target sessions from retained cleanup rows and persisted CDP owners", () => {
    const rows = buildTargetDashboardRows([
      {
        session_id: "live-session",
        lifecycle: "live",
        active_target: { kind: "cdp", cdp_target_id: "chrome-tab:live", window_hwnd: 10 },
        foreground_lane: { status: "unclaimed_session_target" }
      },
      {
        session_id: "orphan-session",
        lifecycle: "unregistered",
        attention_class: "cleanup_required",
        active_target: { kind: "window", window_hwnd: 20 },
        foreground_lane: { status: "unclaimed_session_target" },
        persisted_cdp_target_owners: [
          {
            owner_session_id: "orphan-session",
            cdp_target_id: "chrome-tab:orphan",
            window_hwnd: 20,
            cleanup_action: "call session_end with session_id=orphan-session"
          }
        ]
      }
    ]);

    assert.equal(rows.liveTargetSessions.length, 1);
    assert.equal(rows.liveTargetSessions[0].session_id, "live-session");
    assert.equal(rows.retainedTargetSessions.length, 1);
    assert.equal(rows.retainedTargetSessions[0].cleanup_action, "session_end orphan-session");
    assert.equal(rows.persistedCdpOwnerRows.length, 1);
    assert.equal(rows.persistedCdpOwnerRows[0].cdp_target_id, "chrome-tab:orphan");
  });

  test("surfaces shell job command, exit code, and bounded failure evidence", () => {
    const row = {
      job_id: "issue989-exit7-20260614184340765",
      stderr_tail: "Write-Error issue989-exit7",
      stdout_tail: "",
      job: {
        status: "exit_nonzero",
        command_line: "powershell.exe -NoProfile -Command \"Write-Error issue989-exit7; exit 7\"",
        exit_code: 7,
        diagnostics: {
          output_state: "terminal_stderr_only",
          actionable_hints: ["inspect_stderr_tail"]
        }
      }
    };

    assert.equal(shellJobNeedsReview(row), true);
    assert.match(shellJobCommandLabel(row), /powershell\.exe/);
    assert.match(shellJobEvidence(row), /exit_code=7/);
    assert.match(shellJobEvidence(row), /Write-Error issue989-exit7/);
    assert.match(shellJobEvidence(row), /inspect_stderr_tail/);
  });
});

describe("modelRegistryStatus", () => {
  test("keeps fresh healthy registry rows launch-ready", () => {
    const row = registryRow();

    const status = modelRegistryStatus(row, MODEL_STATUS_NOW_MS);

    assert.equal(status.status, "done");
    assert.equal(status.reason, "ready");
    assert.equal(status.launchReady, true);
    assert.equal(localModelLaunchReady(row, MODEL_STATUS_NOW_MS), true);
  });

  test("does not call stale healthy registry probes stuck", () => {
    const row = registryRow({
      name: "deepseek-reasoning",
      runtime_preset: "deepseek_v4_reasoning",
      last_probe: {
        healthy: true,
        status: "ok",
        latency_ms: 275,
        observed_at_unix_ms: MODEL_STATUS_NOW_MS - 16 * 60 * 1000
      }
    });

    const status = modelRegistryStatus(row, MODEL_STATUS_NOW_MS);

    assert.equal(status.status, "needs_input");
    assert.equal(status.reason, "stale_probe");
    assert.equal(status.launchReady, false);
    assert.equal(localModelLaunchReady(row, MODEL_STATUS_NOW_MS), false);
    assert.match(localModelProbeLabel(row, MODEL_STATUS_NOW_MS), /^stale healthy probe/);
  });

  test("classifies unreachable endpoints as failed with exact probe error", () => {
    const row = registryRow({
      name: "qwen8v2-tool-live",
      base_url: "http://127.0.0.1:8002/v1/chat/completions",
      model_id: "qwen8v2-tool",
      api_key_env_var: null,
      has_api_key_secret: false,
      last_probe: {
        healthy: false,
        status: "MODEL_ENDPOINT_UNREACHABLE",
        error_code: "MODEL_ENDPOINT_UNREACHABLE",
        error_detail: "connect ECONNREFUSED 127.0.0.1:8002",
        observed_at_unix_ms: MODEL_STATUS_NOW_MS - 30_000
      }
    });

    const status = modelRegistryStatus(row, MODEL_STATUS_NOW_MS);

    assert.equal(status.status, "failed");
    assert.equal(status.reason, "unhealthy");
    assert.equal(status.label, "MODEL_ENDPOINT_UNREACHABLE");
    assert.equal(status.launchReady, false);
  });

  test("keeps disabled registry rows idle even when an old probe exists", () => {
    const row = registryRow({
      name: "qwen06v2-tool-live",
      model_id: "qwen06v2-tool",
      runtime_preset: "internalized_no_catalog",
      enabled: false,
      api_key_env_var: null,
      has_api_key_secret: false,
      last_probe: {
        healthy: true,
        status: "ok",
        latency_ms: 90,
        observed_at_unix_ms: MODEL_STATUS_NOW_MS - 24 * 60 * 60 * 1000
      }
    });

    const status = modelRegistryStatus(row, MODEL_STATUS_NOW_MS);

    assert.equal(status.status, "idle");
    assert.equal(status.reason, "disabled");
    assert.equal(status.launchReady, false);
  });

  test("marks enabled registry rows with no probe as needing input", () => {
    const row = registryRow({
      name: "unprobed-real-registry-shape",
      last_probe: null
    });

    const status = modelRegistryStatus(row, MODEL_STATUS_NOW_MS);

    assert.equal(status.status, "needs_input");
    assert.equal(status.reason, "unprobed");
    assert.equal(status.label, "unprobed");
    assert.equal(status.launchReady, false);
  });
});

describe("buildAgents live session status", () => {
  test("keeps stale idle live sessions out of actionable attention", () => {
    const agents = buildAgents(
      dashboardState({
        sessions: [liveSession("idle", "session_initialized", 900_000)],
        unbound_agent_states: [],
        terminal_unbound_agent_states: []
      })
    );

    assert.equal(agents.length, 1);
    assert.equal(agents[0].status, "idle");
    assert.equal(agents[0].lastSeenMs, 900_000);
  });

  test("still surfaces explicit backend attention states", () => {
    const agents = buildAgents(
      dashboardState({
        sessions: [
          liveSession("stuck", "silent_timeout", 10),
          liveSession("needs_input", "operator_reply", 10),
          liveSession("awaiting_approval", "approval", 10),
          liveSession("ready_for_review", "review", 10)
        ],
        unbound_agent_states: []
      })
    );

    assert.deepEqual(agents.map((agent) => agent.status), [
      "stuck",
      "needs_input",
      "awaiting_approval",
      "ready_for_review"
    ]);
  });

  test("preserves terminal failure classification", () => {
    const agents = buildAgents(
      dashboardState({
        sessions: [liveSession("dead", "agent_spawn_failed", 900_000)],
        unbound_agent_states: []
      })
    );

    assert.equal(agents[0].status, "failed");
  });

  test("does not make terminal attached registry failures actionable", () => {
    const agents = buildAgents(
      dashboardState({
        attached_agent_registry: {
          rows: [
            {
              registry_id: "agent-spawn-local-dead",
              kind: "local-model",
              lifecycle: "dead",
              state: "dead",
              reason_code: "local_model_registry_row_missing",
              counts_as_live: false,
              kill_handle: {
                available: true,
                target_id: "agent-spawn-local-dead"
              }
            }
          ]
        },
        sessions: [],
        unbound_agent_states: [],
        terminal_unbound_agent_states: []
      })
    );

    assert.equal(agents.length, 1);
    assert.equal(agents[0].status, "failed");
    assert.equal(agents[0].killable, false);
  });

  test("falls back to stale timing only for unknown live lifecycle rows", () => {
    const agents = buildAgents(
      dashboardState({
        sessions: [
          {
            session_id: "legacy-live-row",
            lifecycle: "live",
            agent_kind: "legacy",
            last_seen_ms_ago: 900_000,
            last_action: ""
          }
        ],
        unbound_agent_states: []
      })
    );

    assert.equal(agents[0].status, "stuck");
  });

  test("derives latest transcript summary and usage for fleet rows", () => {
    const state = dashboardState({
      sessions: [
        {
          session_id: "session-with-spawn",
          spawn_id: "agent-spawn-usage",
          lifecycle: "live",
          agent_kind: "codex",
          last_seen_ms_ago: 50,
          last_action: "tools/call:health",
          agent_state: {
            state: "live",
            reason_code: "working"
          }
        }
      ],
      unbound_agent_states: []
    });
    state.agent_transcripts = panel({
      rows: [
        {
          spawn_id: "agent-spawn-usage",
          line_no: 1,
          record: {
            content_summary: "Older turn",
            usage: {
              input_tokens: 10,
              output_tokens: 2
            }
          }
        },
        {
          spawn_id: "agent-spawn-usage",
          line_no: 2,
          record: {
            content_summary: "Latest agent_query-style activity summary",
            usage: {
              input_tokens: 1000,
              output_tokens: 200,
              cache_read_input_tokens: 5000,
              cache_creation_input_tokens: 300,
              reasoning_output_tokens: 50,
              total_cost_micro_usd: 12345
            }
          }
        }
      ]
    });

    const [agent] = buildAgents(state);

    assert.equal(agent.summary, "Latest agent_query-style activity summary");
    assert.equal(agent.diffStats.transcripts, 2);
    assert.deepEqual(agent.usage, {
      inputTokens: 1000,
      outputTokens: 200,
      cacheReadInputTokens: 5000,
      cacheCreationInputTokens: 300,
      reasoningOutputTokens: 50,
      totalCostMicroUsd: 12345,
      sourceLine: 2
    });
  });
});

describe("buildTaskRows", () => {
  test("reads durable dashboard task rows without inventing UI state", () => {
    const state = dashboardState({
      sessions: [],
      unbound_agent_states: []
    });
    state.tasks = panel({
      source_of_truth: "CF_KV agent-task/v1",
      row_count: 2,
      tasks: [
        {
          schema_version: 1,
          task_id: "issue-924-board",
          state: "review",
          title: "Verify task board",
          priority: 2,
          template_id: "codex-default",
          template_params: {},
          enqueue_seq: 7,
          attempts: [],
          created_unix_ms: 10,
          updated_unix_ms: 20
        },
        {
          schema_version: 1,
          state: "todo",
          title: "corrupt missing id"
        }
      ]
    });

    const rows = buildTaskRows(state);

    assert.equal(rows.length, 1);
    assert.equal(rows[0].task_id, "issue-924-board");
    assert.equal(rows[0].state, "review");
  });
});
