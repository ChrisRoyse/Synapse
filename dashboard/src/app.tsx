import { useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip as ChartTooltip, XAxis, YAxis } from "recharts";
import {
  Bell,
  CheckCircle2,
  Gauge,
  LayoutDashboard,
  LogIn,
  LogOut,
  Moon,
  Plus,
  RefreshCw,
  Rocket,
  Rows3,
  Sun,
  TerminalSquare
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { StatusBadge } from "@/components/ui/badge";
import {
  AgentPeek,
  AppShell,
  DataTable,
  EmptyState,
  FleetRow,
  MetricRow,
  PageHeader,
  RawValue,
  Section,
  StatCard,
  ToolCallCard,
  TranscriptTurn
} from "@/primitives";
import {
  buildAgents,
  buildToolCalls,
  fetchDashboardAuthStatus,
  fetchDashboardState,
  fetchModels,
  loginDashboard,
  logoutDashboard,
  panelData,
  registerApiModel,
  spawnLocalModelAgent,
  type AgentSummary,
  type DashboardAuthStatus,
  type DashboardRouteReadback,
  type DashboardState,
  type FleetStatus,
  type ModelRow
} from "@/lib/dashboard-state";
import { asArray, asRecord, nsToTime, rawText, timeAgo, unixMsToTime } from "@/lib/utils";
import { useUiStore } from "@/store/ui-store";

export function App() {
  const density = useUiStore((state) => state.density);
  const setDensity = useUiStore((state) => state.setDensity);
  const theme = useUiStore((state) => state.theme);
  const setTheme = useUiStore((state) => state.setTheme);
  const selectedAgentId = useUiStore((state) => state.selectedAgentId);
  const setSelectedAgentId = useUiStore((state) => state.setSelectedAgentId);
  const authQuery = useQuery({
    queryKey: ["dashboard-auth"],
    queryFn: fetchDashboardAuthStatus,
    retry: false
  });
  const query = useQuery({
    queryKey: ["dashboard-state"],
    queryFn: fetchDashboardState,
    enabled: authQuery.data?.authenticated === true
  });

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.density = density;
  }, [theme, density]);

  const agents = useMemo(() => buildAgents(query.data), [query.data]);
  const toolCalls = useMemo(() => buildToolCalls(query.data), [query.data]);
  const attentionAgents = useMemo(
    () => agents.filter((agent) => ["stuck", "needs_input", "awaiting_approval", "ready_for_review"].includes(agent.status)),
    [agents]
  );
  const selectedAgent = agents.find((agent) => agent.id === selectedAgentId) ?? attentionAgents[0] ?? agents[0];

  useEffect(() => {
    if (!selectedAgentId && selectedAgent) {
      setSelectedAgentId(selectedAgent.id);
    }
  }, [selectedAgentId, selectedAgent, setSelectedAgentId]);

  const advanceAttention = () => {
    if (attentionAgents.length === 0) return;
    const current = attentionAgents.findIndex((agent) => agent.id === selectedAgent?.id);
    const next = attentionAgents[(current + 1 + attentionAgents.length) % attentionAgents.length];
    setSelectedAgentId(next.id);
  };

  const state = query.data;
  const freshnessMs = state ? Date.now() - state.generated_at_unix_ms : undefined;
  const stale = query.isError || (freshnessMs !== undefined && freshnessMs > 10000);

  if (authQuery.data?.authenticated !== true) {
    return (
      <AppShell sidebar={<Sidebar state={state} auth={authQuery.data} />}>
        <LoginView
          auth={authQuery.data}
          pending={authQuery.isLoading}
          onAuthenticated={() => {
            authQuery.refetch();
            query.refetch();
          }}
        />
      </AppShell>
    );
  }

  return (
    <AppShell sidebar={<Sidebar state={state} auth={authQuery.data} />}>
      <PageHeader
        title="Fleet Overview"
        subtitle={
          <span className={stale ? "text-warning-fg" : "text-secondary"}>
            {query.isError ? rawText(query.error) : `Updated ${freshnessMs === undefined ? "pending" : timeAgo(freshnessMs)} ago`}
          </span>
        }
        actions={
          <>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button size="icon" variant="ghost" onClick={() => query.refetch()} aria-label="Refresh dashboard state">
                  <RefreshCw aria-hidden="true" className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Refresh</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  size="icon"
                  variant="ghost"
                  onClick={() =>
                    logoutDashboard().then(() => {
                      authQuery.refetch();
                    })
                  }
                  aria-label="Lock dashboard"
                >
                  <LogOut aria-hidden="true" className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Lock</TooltipContent>
            </Tooltip>
            <DensityControl density={density} setDensity={setDensity} />
            <label className="flex items-center gap-2 text-sm text-secondary">
              {theme === "dark" ? <Moon aria-hidden="true" className="h-4 w-4" /> : <Sun aria-hidden="true" className="h-4 w-4" />}
              <Switch checked={theme === "light"} onCheckedChange={(checked) => setTheme(checked ? "light" : "dark")} aria-label="Toggle light theme" />
            </label>
          </>
        }
      />

      <OverviewBand state={state} agents={agents} attentionCount={attentionAgents.length} stale={stale} />

      <Section
        title="Spawn Console"
        tier="triage"
        questions={[
          "Which models can I launch right now?",
          "How do I add a cloud API model like DeepSeek?",
          "Did the spawn succeed, step by step?"
        ]}
      >
        <SpawnConsole onSpawned={() => query.refetch()} />
      </Section>

      <div className="grid gap-6 xl:grid-cols-[minmax(0,1fr)_minmax(20rem,0.42fr)]">
        <div className="min-w-0">
          <Section
            title="Attention Groups"
            tier="triage"
            questions={[
              "Which agents need a human now?",
              "Which session should I inspect first?",
              "What changed since the last refresh?"
            ]}
            actions={
              <Button variant="secondary" size="sm" onClick={advanceAttention} disabled={attentionAgents.length === 0}>
                <Bell aria-hidden="true" className="h-4 w-4" />
                Next
              </Button>
            }
          >
            <FleetList agents={attentionAgents.length ? attentionAgents : agents} selectedId={selectedAgent?.id} onSelect={setSelectedAgentId} />
          </Section>

          <Section
            title="Tool Activity"
            tier="triage"
            questions={[
              "Which tools are still running?",
              "Which calls failed?",
              "Where is the verification detail?"
            ]}
          >
            {toolCalls.length ? (
              <div className="grid gap-3 lg:grid-cols-2">
                {toolCalls.slice(0, 6).map((call) => (
                  <ToolCallCard call={call} key={call.id} />
                ))}
              </div>
            ) : (
              <EmptyState title="No command audit rows" />
            )}
          </Section>

          <Section
            title="Fleet Table"
            tier="drill-down"
            questions={[
              "Which sessions are live?",
              "Which rows are stale?",
              "Which row links to detail?"
            ]}
          >
            <FleetTable agents={agents} onSelect={setSelectedAgentId} />
          </Section>
        </div>

        <aside className="min-w-0">
          <Section
            title="Peek Panel"
            tier="drill-down"
            questions={[
              "Why is this agent in its current state?",
              "Which detail surface proves it?",
              "Is raw verification available without flooding the page?"
            ]}
          >
            <AgentPeek agent={selectedAgent} />
          </Section>

          <Section
            title="System Shape"
            tier="overview"
            questions={[
              "Is storage pressure rising?",
              "Which column family is largest?",
              "Is the daemon still local?"
            ]}
          >
            <SystemShape state={state} />
          </Section>
        </aside>
      </div>

      <Section
        title="Transcript Samples"
        tier="drill-down"
        questions={[
          "What did recent agents say?",
          "Was output sanitized before render?",
          "Where is the source row?"
        ]}
      >
        <TranscriptSamples state={state} />
      </Section>
    </AppShell>
  );
}

function LoginView({
  auth,
  pending,
  onAuthenticated
}: {
  auth?: DashboardAuthStatus;
  pending: boolean;
  onAuthenticated: () => void;
}) {
  const [credential, setCredential] = useState("");
  const [error, setError] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setError("");
    setSubmitting(true);
    try {
      await loginDashboard(credential);
      setCredential("");
      onAuthenticated();
    } catch (loginError) {
      setError(rawText(loginError) || "Access denied");
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <>
      <PageHeader
        title="Dashboard Access"
        subtitle={<span>{pending ? "Checking session" : auth?.authenticated ? "Session active" : "Session required"}</span>}
        actions={<span className="text-sm text-secondary">Loopback only</span>}
      />
      <Section
        title="Unlock"
        tier="overview"
        questions={[
          "Is a dashboard session active?",
          "Can the operator mint a cookie session?",
          "Did the login fail closed?"
        ]}
      >
        <form className="max-w-md space-y-3" onSubmit={submit}>
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Access token</span>
            <input
              className="h-10 w-full rounded-md border border-border bg-surface-1 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              type="password"
              value={credential}
              autoComplete="off"
              onChange={(event) => setCredential(event.target.value)}
            />
          </label>
          {error ? <div className="text-sm text-danger-fg">{error}</div> : null}
          <Button type="submit" variant="primary" disabled={!credential.trim() || submitting}>
            <LogIn aria-hidden="true" className="h-4 w-4" />
            Unlock
          </Button>
        </form>
      </Section>
    </>
  );
}

function Sidebar({ state, auth }: { state?: DashboardState; auth?: DashboardAuthStatus }) {
  const health = asRecord(panelData(state?.daemon));
  return (
    <nav className="space-y-4" aria-label="Dashboard">
      <div className="flex items-center gap-3">
        <div className="flex h-9 w-9 items-center justify-center rounded-lg border border-border bg-surface-2">
          <LayoutDashboard aria-hidden="true" className="h-5 w-5 text-accent" />
        </div>
        <div>
          <div className="text-md font-semibold text-primary">Synapse</div>
          <div className="text-xs text-muted">{rawText(health.version || "dashboard")}</div>
        </div>
      </div>
      <div className="grid gap-2">
        <SidebarItem icon={<Gauge aria-hidden="true" />} label="Fleet" active />
        <SidebarItem icon={<Rows3 aria-hidden="true" />} label="Actions" />
        <SidebarItem icon={<TerminalSquare aria-hidden="true" />} label="Terminal" />
        <SidebarItem icon={<CheckCircle2 aria-hidden="true" />} label="Approvals" />
      </div>
      <div className="rounded-lg border border-border bg-surface-2 p-3">
        <div className="text-label font-medium uppercase text-muted">Loopback</div>
        <div className="mt-1 truncate font-mono text-sm text-primary">{state?.bind_addr || "pending"}</div>
      </div>
      <div className="rounded-lg border border-border bg-surface-2 p-3">
        <div className="text-label font-medium uppercase text-muted">Auth</div>
        <div className="mt-1 truncate font-mono text-sm text-primary">{auth?.authenticated ? auth.method : "locked"}</div>
      </div>
    </nav>
  );
}

function SidebarItem({ icon, label, active = false }: { icon: ReactNode; label: string; active?: boolean }) {
  return (
    <a
      href="#"
      className={`flex min-h-10 items-center gap-2 rounded-md px-3 text-sm ${active ? "bg-surface-2 text-primary" : "text-secondary hover:bg-surface-2 hover:text-primary"}`}
    >
      <span className="h-4 w-4">{icon}</span>
      {label}
    </a>
  );
}

function DensityControl({
  density,
  setDensity
}: {
  density: "comfortable" | "compact";
  setDensity: (density: "comfortable" | "compact") => void;
}) {
  return (
    <div className="inline-flex rounded-lg border border-border bg-surface-1 p-1" aria-label="Density">
      {(["comfortable", "compact"] as const).map((value) => (
        <button
          key={value}
          type="button"
          onClick={() => setDensity(value)}
          className={`h-8 rounded-md px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-focus-ring ${density === value ? "bg-surface-2 text-primary" : "text-muted hover:text-primary"}`}
        >
          {value === "comfortable" ? "Comfort" : "Compact"}
        </button>
      ))}
    </div>
  );
}

function OverviewBand({
  state,
  agents,
  attentionCount,
  stale
}: {
  state?: DashboardState;
  agents: AgentSummary[];
  attentionCount: number;
  stale: boolean;
}) {
  const health = asRecord(panelData(state?.daemon));
  const storage = asRecord(panelData(state?.storage));
  const storagePressure = rawText(asRecord(storage.pressure_level).name || asRecord(storage.pressure_level).value || "unknown");
  const liveAgents = agents.filter((agent) => agent.lifecycle === "live").length;
  const toolCount = Number(health.tool_count || 0);
  return (
    <Section
      title="Overview"
      tier="overview"
      questions={[
        "Is anything wrong?",
        "How many agents are live?",
        "Is the daemon stale?"
      ]}
    >
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <StatCard label="Attention" value={attentionCount} status={attentionCount ? "needs_input" : "done"} delta={attentionCount ? "human review queued" : "quiet"} />
        <StatCard label="Live Agents" value={liveAgents} status={liveAgents ? "working" : "idle"} delta={`${agents.length} total rows`} />
        <StatCard label="Tools" value={toolCount} status={toolCount ? "done" : "stuck"} delta="strict client surface" />
        <StatCard label="Freshness" value={stale ? "stale" : "live"} status={stale ? "stuck" : "working"} delta={storagePressure} />
      </div>
    </Section>
  );
}

const deepSeekPresets = {
  flash: {
    label: "DeepSeek Flash",
    name: "deepseek-flash",
    base_url: "https://api.deepseek.com",
    model_id: "deepseek-v4-flash",
    runtime_preset: "deepseek_v4_flash_non_thinking",
    api_key_env_var: "DEEPSEEK_API_KEY",
    context_length: "1000000",
    max_tools: "128",
    notes: "DeepSeek V4 Flash non-thinking API agent"
  },
  reasoning: {
    label: "DeepSeek Reasoning",
    name: "deepseek-reasoning",
    base_url: "https://api.deepseek.com",
    model_id: "deepseek-v4-flash",
    runtime_preset: "deepseek_v4_reasoning",
    api_key_env_var: "DEEPSEEK_API_KEY",
    context_length: "1000000",
    max_tools: "128",
    notes: "DeepSeek V4 Flash reasoning API agent"
  }
};

function SpawnConsole({ onSpawned }: { onSpawned: () => void }) {
  const modelsQuery = useQuery({
    queryKey: ["dashboard-models"],
    queryFn: fetchModels
  });
  const models = modelsQuery.data ?? [];
  const [selectedModel, setSelectedModel] = useState("");
  const [prompt, setPrompt] = useState("Use workspace_put with key issue985-deepseek-smoke and value {\"ok\":true}.");
  const [workingDir, setWorkingDir] = useState("C:\\code\\Synapse");
  const [registerForm, setRegisterForm] = useState(deepSeekPresets.flash);
  const [pendingAction, setPendingAction] = useState<"register" | "spawn" | "">("");
  const [error, setError] = useState("");
  const [lastRegister, setLastRegister] = useState<DashboardRouteReadback | null>(null);
  const [lastSpawn, setLastSpawn] = useState<DashboardRouteReadback | null>(null);

  useEffect(() => {
    if (!selectedModel && models.length > 0) {
      const firstLaunchable = models.find((model) => model.enabled && model.last_probe?.healthy) ?? models[0];
      setSelectedModel(firstLaunchable.name);
    }
  }, [models, selectedModel]);

  const selected = models.find((model) => model.name === selectedModel);
  const canSpawn = Boolean(selectedModel && prompt.trim() && !pendingAction);

  const submitRegister = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setError("");
    setPendingAction("register");
    try {
      const readback = await registerApiModel({
        name: registerForm.name,
        base_url: registerForm.base_url,
        model_id: registerForm.model_id,
        runtime_preset: registerForm.runtime_preset,
        api_key_env_var: registerForm.api_key_env_var,
        context_length: parsePositiveInteger(registerForm.context_length, "context_length"),
        max_tools: parsePositiveInteger(registerForm.max_tools, "max_tools"),
        notes: registerForm.notes,
        probe_timeout_ms: 30000
      });
      setLastRegister(readback);
      const row = asRecord(asRecord(readback.register).row) as unknown as ModelRow;
      if (row.name) setSelectedModel(row.name);
      await modelsQuery.refetch();
    } catch (registerError) {
      setError(rawText(registerError) || "API model registration failed");
    } finally {
      setPendingAction("");
    }
  };

  const submitSpawn = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setError("");
    setPendingAction("spawn");
    try {
      const readback = await spawnLocalModelAgent({
        model_ref: selectedModel,
        prompt,
        working_dir: workingDir.trim() || undefined,
        wait_timeout_ms: 300000,
        hold_open_ms: 0
      });
      setLastSpawn(readback);
      await modelsQuery.refetch();
      onSpawned();
    } catch (spawnError) {
      setError(rawText(spawnError) || "Local-model spawn failed");
    } finally {
      setPendingAction("");
    }
  };

  return (
    <div className="grid gap-4 xl:grid-cols-[minmax(0,0.9fr)_minmax(24rem,1.1fr)]">
      <div className="min-w-0 space-y-4">
        <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
          <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
            <div className="min-w-0">
              <h3 className="text-md font-semibold tracking-normal text-primary">Models</h3>
              <p className="mt-1 text-sm text-secondary">{modelsQuery.isFetching ? "Refreshing registry" : `${models.length} registry rows`}</p>
            </div>
            <Button size="sm" variant="ghost" onClick={() => modelsQuery.refetch()} disabled={modelsQuery.isFetching}>
              <RefreshCw aria-hidden="true" className="h-4 w-4" />
              Refresh
            </Button>
          </div>
          {models.length ? (
            <DataTable
              data={models}
              getRowId={(model) => model.name}
              columns={[
                {
                  id: "status",
                  header: "Status",
                  cell: ({ row }) => <StatusBadge status={modelFleetStatus(row.original)} />
                },
                {
                  accessorKey: "name",
                  header: "Name",
                  cell: ({ row }) => <span className="font-mono text-primary">{row.original.name}</span>
                },
                { accessorKey: "model_id", header: "Model" },
                {
                  id: "runtime",
                  header: "Runtime",
                  cell: ({ row }) => <span className="font-mono">{row.original.runtime_preset || "open_ai_compatible"}</span>
                },
                {
                  id: "env",
                  header: "Key env",
                  cell: ({ row }) => <span className="font-mono">{row.original.api_key_env_var || "none"}</span>
                }
              ]}
            />
          ) : (
            <EmptyState title={modelsQuery.isError ? rawText(modelsQuery.error) || "Model registry unavailable" : "No model rows"} />
          )}
        </div>

        <form className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]" onSubmit={submitSpawn}>
          <div className="mb-3 flex items-center gap-2">
            <Rocket aria-hidden="true" className="h-4 w-4 text-info" />
            <h3 className="text-md font-semibold tracking-normal text-primary">Spawn</h3>
          </div>
          <div className="grid gap-3 md:grid-cols-2">
            <label className="block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Model</span>
              <select
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                value={selectedModel}
                onChange={(event) => setSelectedModel(event.target.value)}
              >
                {models.map((model) => (
                  <option key={model.name} value={model.name}>
                    {model.name}
                  </option>
                ))}
              </select>
            </label>
            <label className="block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Working dir</span>
              <input
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                value={workingDir}
                onChange={(event) => setWorkingDir(event.target.value)}
              />
            </label>
          </div>
          <label className="mt-3 block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Prompt</span>
            <textarea
              className="min-h-28 w-full rounded-md border border-border bg-surface-2 px-3 py-2 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
            />
          </label>
          <div className="mt-3 flex flex-wrap items-center justify-between gap-3">
            <div className="min-w-0 text-sm text-secondary">
              {selected ? `${selected.model_id} / ${selected.last_probe?.status || "unprobed"}` : "No model selected"}
            </div>
            <Button type="submit" variant="primary" disabled={!canSpawn}>
              <Rocket aria-hidden="true" className="h-4 w-4" />
              {pendingAction === "spawn" ? "Spawning" : "Spawn"}
            </Button>
          </div>
        </form>
      </div>

      <div className="min-w-0 space-y-4">
        <form className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]" onSubmit={submitRegister}>
          <div className="mb-3 flex items-center gap-2">
            <Plus aria-hidden="true" className="h-4 w-4 text-info" />
            <h3 className="text-md font-semibold tracking-normal text-primary">Add API Model</h3>
          </div>
          <div className="grid gap-3 md:grid-cols-2">
            <label className="mt-3 block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Preset</span>
              <select
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                value={registerForm.runtime_preset}
                onChange={(event) => {
                  const preset = Object.values(deepSeekPresets).find((item) => item.runtime_preset === event.target.value);
                  if (preset) setRegisterForm(preset);
                }}
              >
                {Object.values(deepSeekPresets).map((preset) => (
                  <option key={preset.runtime_preset} value={preset.runtime_preset}>
                    {preset.label}
                  </option>
                ))}
              </select>
            </label>
            <TextField label="Name" value={registerForm.name} onChange={(value) => setRegisterForm((form) => ({ ...form, name: value }))} />
            <TextField label="Model" value={registerForm.model_id} onChange={(value) => setRegisterForm((form) => ({ ...form, model_id: value }))} />
            <TextField label="Base URL" value={registerForm.base_url} onChange={(value) => setRegisterForm((form) => ({ ...form, base_url: value }))} mono />
            <TextField label="Key env" value={registerForm.api_key_env_var} onChange={(value) => setRegisterForm((form) => ({ ...form, api_key_env_var: value }))} mono />
            <TextField label="Context" value={registerForm.context_length} onChange={(value) => setRegisterForm((form) => ({ ...form, context_length: value }))} mono />
            <TextField label="Max tools" value={registerForm.max_tools} onChange={(value) => setRegisterForm((form) => ({ ...form, max_tools: value }))} mono />
          </div>
          <TextField label="Notes" value={registerForm.notes} onChange={(value) => setRegisterForm((form) => ({ ...form, notes: value }))} />
          <div className="mt-3 flex justify-end">
            <Button type="submit" variant="secondary" disabled={pendingAction === "register"}>
              <Plus aria-hidden="true" className="h-4 w-4" />
              {pendingAction === "register" ? "Registering" : "Register"}
            </Button>
          </div>
        </form>

        {error ? <div className="rounded-lg border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{error}</div> : null}

        {lastRegister ? <RawValue value={lastRegister} label="Register readback" /> : null}
        {lastSpawn ? <RawValue value={lastSpawn} label="Spawn readback" /> : null}
      </div>
    </div>
  );
}

function TextField({
  label,
  value,
  onChange,
  mono = false
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  mono?: boolean;
}) {
  return (
    <label className="mt-3 block text-sm text-secondary">
      <span className="mb-1 block text-label font-medium uppercase text-muted">{label}</span>
      <input
        className={`h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring ${mono ? "font-mono" : ""}`}
        value={value}
        onChange={(event) => onChange(event.target.value)}
      />
    </label>
  );
}

function parsePositiveInteger(value: string, field: string): number | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  const parsed = Number(trimmed);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error(`${field} must be a positive integer`);
  }
  return parsed;
}

function modelFleetStatus(model: ModelRow): FleetStatus {
  if (!model.enabled) return "idle";
  if (model.last_probe?.healthy) return "done";
  if (model.last_probe) return "stuck";
  return "needs_input";
}

function FleetList({
  agents,
  selectedId,
  onSelect
}: {
  agents: AgentSummary[];
  selectedId?: string;
  onSelect: (id: string) => void;
}) {
  if (agents.length === 0) return <EmptyState title="No agent rows" />;
  return (
    <div className="rounded-lg border border-border bg-surface-1">
      {agents.map((agent) => (
        <FleetRow key={agent.id} agent={agent} selected={agent.id === selectedId} onSelect={() => onSelect(agent.id)} />
      ))}
    </div>
  );
}

function FleetTable({ agents, onSelect }: { agents: AgentSummary[]; onSelect: (id: string) => void }) {
  if (agents.length === 0) return <EmptyState title="No fleet rows" />;
  return (
    <DataTable
      data={agents}
      getRowId={(agent) => agent.id}
      columns={[
        {
          id: "status",
          header: "Status",
          cell: ({ row }) => <StatusBadge status={row.original.status} />
        },
        {
          accessorKey: "id",
          header: "Agent",
          cell: ({ row }) => (
            <button className="truncate text-left text-primary underline-offset-4 hover:underline" type="button" onClick={() => onSelect(row.original.id)}>
              {row.original.id}
            </button>
          )
        },
        { accessorKey: "kind", header: "Kind" },
        { accessorKey: "lifecycle", header: "Lifecycle" },
        {
          id: "summary",
          header: "Summary",
          cell: ({ row }) => <span className="line-clamp-2">{row.original.summary}</span>
        },
        {
          id: "diff",
          header: "Diff",
          cell: ({ row }) => `${row.original.diffStats.actions}/${row.original.diffStats.transcripts}`
        }
      ]}
    />
  );
}

function SystemShape({ state }: { state?: DashboardState }) {
  const storage = asRecord(panelData(state?.storage));
  const counts = asRecord(storage.cf_row_counts);
  const chartData = Object.entries(counts)
    .map(([name, value]) => ({ name: name.replace("CF_", ""), rows: Number(value) || 0 }))
    .sort((a, b) => b.rows - a.rows)
    .slice(0, 8);
  if (!chartData.length) return <EmptyState title="No storage rows" />;
  return (
    <div className="space-y-4">
      <div className="h-64 rounded-lg border border-border bg-surface-1 p-3">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={chartData} margin={{ top: 8, right: 8, bottom: 8, left: 8 }}>
            <CartesianGrid stroke="var(--border-subtle)" vertical={false} />
            <XAxis dataKey="name" stroke="var(--text-muted)" tickLine={false} axisLine={false} />
            <YAxis stroke="var(--text-muted)" tickLine={false} axisLine={false} />
            <ChartTooltip contentStyle={{ background: "var(--surface-3)", border: "1px solid var(--border)", color: "var(--text-primary)" }} />
            <Bar dataKey="rows" fill="var(--info)" radius={[4, 4, 0, 0]} />
          </BarChart>
        </ResponsiveContainer>
      </div>
      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        <MetricRow label="Schema" value={rawText(storage.schema_version)} />
        <MetricRow label="Policy count" value={rawText(storage.audit_retention_policy_count)} />
        <MetricRow label="Generated" value={unixMsToTime(state?.generated_at_unix_ms)} />
      </div>
    </div>
  );
}

function TranscriptSamples({ state }: { state?: DashboardState }) {
  const rows = asArray<Record<string, unknown>>(asRecord(panelData(state?.agent_transcripts)).rows).slice(0, 4);
  if (!rows.length) return <EmptyState title="No transcript rows" />;
  return (
    <div className="grid gap-3 lg:grid-cols-2">
      {rows.map((row, index) => (
        <TranscriptTurn key={`${rawText(row.spawn_id)}-${rawText(row.line_no)}-${index}`} row={row} />
      ))}
    </div>
  );
}
