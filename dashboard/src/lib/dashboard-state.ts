import { asArray, asRecord, rawText } from "@/lib/utils";

export interface DashboardPanel<T = unknown> {
  status: "ok" | "error" | "unavailable";
  source: string;
  data: T;
  error?: string;
}

export interface DashboardState {
  schema_version: number;
  generated_at_unix_ms: number;
  bind_addr: string;
  token_policy: string;
  auth: DashboardPanel;
  daemon: DashboardPanel;
  sessions: DashboardPanel;
  lease: DashboardPanel;
  storage: DashboardPanel;
  command_audit: DashboardPanel;
  approvals: DashboardPanel;
  suggestions: DashboardPanel;
  armed_runs: DashboardPanel;
  agent_transcripts: DashboardPanel;
  hygiene: DashboardPanel;
  local_models: DashboardPanel;
}

export interface AgentSummary {
  id: string;
  kind: string;
  lifecycle: string;
  status: FleetStatus;
  summary: string;
  lastSeenMs?: number;
  lastAction?: string;
  target?: string;
  reason?: string;
  diffStats: {
    events: number;
    transcripts: number;
    actions: number;
  };
  raw: unknown;
}

export type FleetStatus =
  | "working"
  | "idle"
  | "ready_for_review"
  | "needs_input"
  | "awaiting_approval"
  | "stuck"
  | "done";

export interface ToolCallSummary {
  id: string;
  tool: string;
  lifecycle: "pending" | "running" | "success" | "error";
  summary: string;
  actor?: string;
  target?: string;
  time?: string;
  raw: unknown;
}

export interface DashboardAuthStatus {
  ok: boolean;
  authenticated: boolean;
  method: "cookie" | "bearer";
  csrf_token?: string;
  expires_unix_ms?: number;
  source_of_truth: string;
}

let csrfToken: string | null = null;

export function dashboardCsrfToken() {
  return csrfToken;
}

function setCsrfToken(value?: string | null) {
  csrfToken = value || null;
}

export async function fetchDashboardAuthStatus(): Promise<DashboardAuthStatus> {
  const response = await fetch("/dashboard/auth/status", {
    cache: "no-store",
    credentials: "same-origin"
  });
  if (response.status === 401) {
    setCsrfToken(null);
    return {
      ok: false,
      authenticated: false,
      method: "cookie",
      source_of_truth: "CF_KV dashboard-auth/v1"
    };
  }
  if (!response.ok) {
    throw new Error(`dashboard auth failed: ${response.status}`);
  }
  const status = (await response.json()) as DashboardAuthStatus;
  setCsrfToken(status.csrf_token);
  return status;
}

export async function loginDashboard(credential: string): Promise<DashboardAuthStatus> {
  const response = await fetch("/dashboard/auth/login", {
    method: "POST",
    cache: "no-store",
    credentials: "same-origin",
    headers: {
      "Content-Type": "application/json"
    },
    body: JSON.stringify({ credential })
  });
  if (!response.ok) {
    setCsrfToken(null);
    throw new Error(`dashboard login failed: ${response.status}`);
  }
  const status = (await response.json()) as DashboardAuthStatus;
  setCsrfToken(status.csrf_token);
  return status;
}

export async function logoutDashboard(): Promise<void> {
  const headers: Record<string, string> = {};
  if (csrfToken) headers["X-CSRF-Token"] = csrfToken;
  const response = await fetch("/dashboard/auth/logout", {
    method: "POST",
    cache: "no-store",
    credentials: "same-origin",
    headers
  });
  setCsrfToken(null);
  if (!response.ok) {
    throw new Error(`dashboard logout failed: ${response.status}`);
  }
}

export interface ModelRow {
  name: string;
  base_url: string;
  model_id: string;
  api_shape: string;
  runtime_preset?: string;
  enabled: boolean;
  allow_non_loopback: boolean;
  api_key_env_var?: string | null;
  context_length?: number | null;
  max_tools?: number | null;
  last_probe?: { healthy?: boolean; status?: string; latency_ms?: number } | null;
}

export interface RegisterApiModelRequest {
  name: string;
  base_url: string;
  model_id: string;
  runtime_preset: string;
  api_key_env_var: string;
  context_length?: number;
  max_tools?: number;
  notes?: string;
  probe_timeout_ms?: number;
}

export interface SpawnRequest {
  model_ref: string;
  prompt: string;
  working_dir?: string;
  wait_timeout_ms?: number;
  hold_open_ms?: number;
}

export type DashboardRouteReadback = Record<string, unknown>;

function csrfHeaders(): Record<string, string> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (csrfToken) headers["X-CSRF-Token"] = csrfToken;
  return headers;
}

async function readJsonOrThrow(response: Response): Promise<Record<string, unknown>> {
  let body: Record<string, unknown> = {};
  try {
    body = (await response.json()) as Record<string, unknown>;
  } catch {
    /* non-JSON error body */
  }
  if (!response.ok || body.ok === false) {
    const code = typeof body.code === "string" ? body.code : `HTTP ${response.status}`;
    const message = typeof body.message === "string" ? body.message : response.statusText;
    throw new Error(`${code}: ${message}`);
  }
  return body;
}

export async function fetchModels(): Promise<ModelRow[]> {
  const response = await fetch("/dashboard/models", {
    cache: "no-store",
    credentials: "same-origin"
  });
  const body = await readJsonOrThrow(response);
  const list = (body.list ?? {}) as { rows?: ModelRow[] };
  return list.rows ?? [];
}

export async function registerApiModel(
  request: RegisterApiModelRequest
): Promise<DashboardRouteReadback> {
  const response = await fetch("/dashboard/api-model/register", {
    method: "POST",
    cache: "no-store",
    credentials: "same-origin",
    headers: csrfHeaders(),
    body: JSON.stringify(request)
  });
  return readJsonOrThrow(response);
}

export async function spawnLocalModelAgent(
  request: SpawnRequest
): Promise<DashboardRouteReadback> {
  const response = await fetch("/dashboard/local-model-spawn", {
    method: "POST",
    cache: "no-store",
    credentials: "same-origin",
    headers: csrfHeaders(),
    body: JSON.stringify(request)
  });
  return readJsonOrThrow(response);
}

export async function fetchDashboardState(): Promise<DashboardState> {
  const response = await fetch("/dashboard/state.json", {
    cache: "no-store",
    credentials: "same-origin"
  });
  if (!response.ok) {
    throw new Error(`dashboard state failed: ${response.status}`);
  }
  return response.json();
}

export function panelData<T = Record<string, unknown>>(panel?: DashboardPanel): T {
  return (panel?.data ?? {}) as T;
}

export function buildAgents(state?: DashboardState): AgentSummary[] {
  if (!state) return [];
  const sessionData = asRecord(state.sessions.data);
  const sessionRows = asArray<Record<string, unknown>>(sessionData.sessions);
  const unbound = asArray<Record<string, unknown>>(sessionData.unbound_agent_states);
  const transcripts = asArray<Record<string, unknown>>(asRecord(state.agent_transcripts.data).rows);
  const actions = asArray<Record<string, unknown>>(asRecord(state.command_audit.data).rows);

  const transcriptCounts = new Map<string, number>();
  for (const row of transcripts) {
    const spawnId = rawText(row.spawn_id);
    if (spawnId) transcriptCounts.set(spawnId, (transcriptCounts.get(spawnId) ?? 0) + 1);
  }

  const actionCounts = new Map<string, number>();
  for (const row of actions) {
    const target = rawText(row.target_session_id);
    const actor = rawText(row.actor_session_id);
    for (const id of [target, actor]) {
      if (id) actionCounts.set(id, (actionCounts.get(id) ?? 0) + 1);
    }
  }

  const live = sessionRows.map((row) => {
    const agentState = asRecord(row.agent_state);
    const sessionId = rawText(row.session_id);
    const stateName = rawText(agentState.state || row.lifecycle);
    const lastSeenMs = Number(row.last_seen_ms_ago);
    const lastAction = rawText(row.last_action);
    return {
      id: sessionId,
      kind: rawText(row.agent_kind || row.client_name || "agent"),
      lifecycle: rawText(row.lifecycle),
      status: statusFromLiveSession(stateName, lastSeenMs, lastAction),
      summary: lastAction || stateName || "session live",
      lastSeenMs: Number.isFinite(lastSeenMs) ? lastSeenMs : undefined,
      lastAction,
      target: row.active_target ? rawText(row.active_target) : "",
      reason: rawText(agentState.reason_code),
      diffStats: {
        events: 1,
        transcripts: transcriptCounts.get(sessionId) ?? 0,
        actions: actionCounts.get(sessionId) ?? 0
      },
      raw: row
    } satisfies AgentSummary;
  });

  const historical = unbound.map((row) => {
    const id = rawText(row.session_id || row.spawn_id || row.anchor);
    const stateName = rawText(row.state);
    const reason = rawText(row.reason_code);
    return {
      id,
      kind: rawText(row.agent_kind || "agent"),
      lifecycle: stateName || "unbound",
      status: statusFromHistorical(stateName, reason),
      summary: [stateName, reason].filter(Boolean).join(" / ") || "historical state",
      lastSeenMs: undefined,
      lastAction: "",
      target: "",
      reason,
      diffStats: {
        events: 1,
        transcripts: transcriptCounts.get(id) ?? 0,
        actions: actionCounts.get(id) ?? 0
      },
      raw: row
    } satisfies AgentSummary;
  });

  return [...live, ...historical].filter((agent) => agent.id);
}

export function buildToolCalls(state?: DashboardState): ToolCallSummary[] {
  if (!state) return [];
  const rows = asArray<Record<string, unknown>>(asRecord(state.command_audit.data).rows);
  return rows.slice(0, 16).map((row, index) => {
    const error = rawText(row.error_code);
    const outcome = rawText(row.outcome);
    return {
      id: rawText(row.key_hex) || `${index}`,
      tool: rawText(row.tool || row.verb || "tool"),
      lifecycle: error ? "error" : outcome === "ok" ? "success" : "running",
      summary: [rawText(row.verb), outcome, error].filter(Boolean).join(" / ") || "tool call",
      actor: rawText(row.actor_session_id),
      target: rawText(row.target_session_id),
      time: rawText(row.ts_ns),
      raw: row
    };
  });
}

function statusFromLiveSession(stateName: string, lastSeenMs: number, lastAction: string): FleetStatus {
  if (Number.isFinite(lastSeenMs) && lastSeenMs > 300000) return "stuck";
  if (/approval/i.test(lastAction)) return "awaiting_approval";
  if (/wait|inbox|input/i.test(lastAction)) return "needs_input";
  if (/review/i.test(lastAction)) return "ready_for_review";
  if (/idle/i.test(stateName)) return "idle";
  return "working";
}

function statusFromHistorical(stateName: string, reason: string): FleetStatus {
  if (/failed|unhealthy|missing|error|interrupted|timeout/i.test(`${stateName} ${reason}`)) return "stuck";
  if (/dead|done|exited|closed/i.test(stateName)) return "done";
  return "idle";
}
