import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { flexRender, getCoreRowModel, getSortedRowModel, useReactTable, type ColumnDef, type SortingState } from "@tanstack/react-table";
import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip as ChartTooltip, XAxis, YAxis } from "recharts";
import {
  ArrowUpDown,
  Bell,
  CalendarClock,
  ChevronDown,
  CheckCircle2,
  ClipboardList,
  Command,
  Database,
  Eraser,
  FileSearch,
  Gauge,
  MonitorUp,
  Moon,
  Network,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Rocket,
  Rows3,
  Save,
  Search,
  ServerCog,
  ShieldCheck,
  SkipForward,
  Sun,
  Trash2,
  UserRound,
  X,
  type LucideIcon
} from "lucide-react";
import commandMarkUrl from "@/assets/synapse-command-mark.png";
import emptyStateUrl from "@/assets/synapse-empty-state.webp";
import statusIconsUrl from "@/assets/synapse-status-icons.webp";
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
  TranscriptTurn,
  transcriptRowHasContent
} from "@/primitives";
import {
  buildAgents,
  buildToolCalls,
  claimDashboardAssetReload,
  dashboardAssetReloadDecision,
  dashboardAssetReloadUrl,
  decideApproval,
  deleteDashboardView,
  fetchAuditQuery,
  fetchTemplates,
  putTemplate,
  deleteTemplate,
  fetchDashboardState,
  fetchModels,
  fetchSavedViews,
  forceReleaseLease,
  handoffLease,
  killAgent,
  panelData,
  pauseTimeline,
  pruneTargetClaims,
  purgeTimelineRecordings,
  registerApiModel,
  resumeTimeline,
  runStorageGc,
  saveDashboardView,
  spawnAgent,
  type AgentSummary,
  type AuditQueryFilters,
  type AuditQueryResponse,
  type AuditQueryRow,
  type AgentTemplateRow,
  type DashboardControlResponse,
  type DashboardRouteReadback,
  type DashboardSavedView,
  type DashboardState,
  type FleetStatus,
  type ModelRow,
  type AgentKillResponse,
  type SpawnAgentResponse,
  type StorageGcReadback,
  type TimelineControlResponse,
  type TimelinePurgeReadback,
  type TimelinePurgeRequest
} from "@/lib/dashboard-state";
import { asArray, asRecord, cn, nsToTime, rawText, timeAgo, unixMsToTime } from "@/lib/utils";
import { useUiStore, type DashboardRouteId, type Density, type Theme } from "@/store/ui-store";

type RouteDefinition = {
  id: DashboardRouteId;
  label: string;
  title: string;
  icon: LucideIcon;
};

const routeDefinitions: RouteDefinition[] = [
  { id: "fleet", label: "Fleet", title: "Fleet Overview", icon: Gauge },
  { id: "agent", label: "Agent", title: "Agent Detail", icon: UserRound },
  { id: "tasks", label: "Tasks", title: "Task Board", icon: ClipboardList },
  { id: "system", label: "System", title: "System, Storage & Activity", icon: ServerCog },
  { id: "audit", label: "Audit", title: "Audit Explorer", icon: ShieldCheck }
];

// Alias routes resolve to a canonical view but stay valid as deep-link / stored
// route ids (so bookmarks, saved views, and persisted UI state never land on a
// blank route).
//   - `analytics` / `timeline` merged into the System view (#927 follow-up).
//   - `approvals` merged INTO the Fleet manager (#1027): the approval/attention
//     inbox is now a section at the top of the Fleet page, so `#/approvals`
//     deep-links there instead of maintaining a divergent second surface.
const ROUTE_ALIASES: Partial<Record<DashboardRouteId, DashboardRouteId>> = {
  analytics: "system",
  timeline: "system",
  approvals: "fleet"
};

function normalizeRoute(route: DashboardRouteId): DashboardRouteId {
  return ROUTE_ALIASES[route] ?? route;
}

// Every id that is a legal route target from a hash / stored state: the
// canonical nav routes plus the alias source keys above. `isDashboardRouteId`
// must accept the alias keys too, otherwise `routeFromHash` drops `#/approvals`
// (and `#/analytics`/`#/timeline`) on the floor and the deep-link never resolves.
const VALID_ROUTE_IDS = new Set<string>([
  ...routeDefinitions.map((item) => item.id),
  ...Object.keys(ROUTE_ALIASES)
]);

const auditTextFields = ["cursor", "start_ts_ns", "end_ts_ns", "session_id", "tool", "status", "error_code", "row_kind"] as const;

function defaultAuditFilters(): AuditQueryFilters {
  const sixHoursMs = 6 * 60 * 60 * 1000;
  return {
    limit: "100",
    scan_limit: "1000",
    row_kind: "all",
    start_ts_ns: (BigInt(Date.now() - sixHoursMs) * 1_000_000n).toString()
  };
}

function auditCursorFilters(keyHex: string): AuditQueryFilters {
  return {
    limit: "1",
    scan_limit: "1",
    cursor: keyHex,
    row_kind: "all"
  };
}

function sanitizeAuditFilters(value: unknown): AuditQueryFilters {
  const source = asRecord(value);
  const base = defaultAuditFilters();
  const next: AuditQueryFilters = {
    limit: rawText(source.limit || base.limit),
    scan_limit: rawText(source.scan_limit || base.scan_limit)
  };
  for (const field of auditTextFields) {
    const text = rawText(source[field]);
    if (text) next[field] = text;
  }
  if (!next.row_kind) next.row_kind = "all";
  return next;
}

function shortKey(value?: string | null) {
  if (!value) return "";
  return value.length > 18 ? `${value.slice(0, 10)}...${value.slice(-6)}` : value;
}

export function App() {
  const density = useUiStore((state) => state.density);
  const setDensity = useUiStore((state) => state.setDensity);
  const theme = useUiStore((state) => state.theme);
  const setTheme = useUiStore((state) => state.setTheme);
  const route = useUiStore((state) => state.route);
  const setRoute = useUiStore((state) => state.setRoute);
  const savedViewId = useUiStore((state) => state.savedViewId);
  const setSavedViewId = useUiStore((state) => state.setSavedViewId);
  const selectedAgentId = useUiStore((state) => state.selectedAgentId);
  const setSelectedAgentId = useUiStore((state) => state.setSelectedAgentId);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [hashRouteReady, setHashRouteReady] = useState(false);
  const [savedViewName, setSavedViewName] = useState("");
  const [savedViewError, setSavedViewError] = useState("");
  const [auditFilters, setAuditFilters] = useState<AuditQueryFilters>(() => defaultAuditFilters());

  const query = useQuery({
    queryKey: ["dashboard-state"],
    queryFn: fetchDashboardState
  });
  const savedViewsQuery = useQuery({
    queryKey: ["dashboard-saved-views"],
    queryFn: fetchSavedViews
  });

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.density = density;
  }, [theme, density]);

  useEffect(() => {
    const syncFromHash = () => {
      const hashRoute = routeFromHash(window.location.hash);
      if (hashRoute && useUiStore.getState().route !== hashRoute) {
        setRoute(hashRoute);
      }
    };
    syncFromHash();
    setHashRouteReady(true);
    window.addEventListener("hashchange", syncFromHash);
    return () => window.removeEventListener("hashchange", syncFromHash);
  }, [setRoute]);

  useEffect(() => {
    if (!hashRouteReady) return;
    const expected = `#/${route}`;
    if (window.location.hash !== expected) {
      window.history.replaceState(null, "", expected);
    }
  }, [hashRouteReady, route]);

  useEffect(() => {
    let icon = document.querySelector<HTMLLinkElement>('link[rel="icon"][data-synapse-generated="true"]');
    if (!icon) {
      icon = document.createElement("link");
      icon.rel = "icon";
      icon.type = "image/png";
      icon.dataset.synapseGenerated = "true";
      document.head.appendChild(icon);
    }
    icon.href = commandMarkUrl;
  }, []);

  useEffect(() => {
    const decision = dashboardAssetReloadDecision(query.data);
    if (!claimDashboardAssetReload(decision)) return;
    document.title = "Synapse Command Center";
    window.location.replace(dashboardAssetReloadUrl(decision.expectedJsFile || ""));
  }, [query.data]);

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

  const attentionCount = attentionAgents.length;
  useEffect(() => {
    document.title = attentionCount ? `${attentionCount} awaiting input - Synapse Command Center` : "Synapse Command Center";
  }, [attentionCount]);

  const advanceAttention = () => {
    if (attentionAgents.length === 0) return;
    const current = attentionAgents.findIndex((agent) => agent.id === selectedAgent?.id);
    const next = attentionAgents[(current + 1 + attentionAgents.length) % attentionAgents.length];
    setSelectedAgentId(next.id);
    setRoute("agent");
  };

  const focusAuditKey = (keyHex: string) => {
    if (!keyHex) return;
    setAuditFilters(auditCursorFilters(keyHex));
    setRoute("audit");
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (isEditableShortcutTarget(event.target)) return;
      const key = event.key.toLowerCase();
      const plainKey = !event.altKey && !event.ctrlKey && !event.metaKey && !event.shiftKey;
      if (((event.ctrlKey || event.metaKey) && key === "k") || (plainKey && (key === "/" || key === "p"))) {
        event.preventDefault();
        setPaletteOpen(true);
      }
      if ((event.altKey && key === "n") || (plainKey && key === "n")) {
        event.preventDefault();
        advanceAttention();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  const state = query.data;
  const freshnessMs = state ? Date.now() - state.generated_at_unix_ms : undefined;
  const stale = query.isError || (freshnessMs !== undefined && freshnessMs > 10000);
  const normalizedRoute = normalizeRoute(route);
  const activeRoute = routeDefinitions.find((item) => item.id === normalizedRoute) ?? routeDefinitions[0];

  const commands = useMemo(
    () => [
      ...routeDefinitions.map((item) => ({
        id: `route-${item.id}`,
        label: item.title,
        group: "Views",
        icon: item.icon,
        action: () => setRoute(item.id)
      })),
      {
        id: "attention-next",
        label: "Next attention",
        group: "Controls",
        icon: Bell,
        action: advanceAttention
      },
      {
        id: "refresh",
        label: "Refresh dashboard state",
        group: "Controls",
        icon: RefreshCw,
        action: () => {
          query.refetch();
          savedViewsQuery.refetch();
        }
      },
      ...agents.slice(0, 8).map((agent) => ({
        id: `agent-${agent.id}`,
        label: agent.id,
        group: "Agents",
        icon: UserRound,
        action: () => {
          setSelectedAgentId(agent.id);
          setRoute("agent");
        }
      }))
    ],
    [agents, query, savedViewsQuery, setRoute, setSelectedAgentId]
  );

  const applySavedView = (view?: DashboardSavedView) => {
    setSavedViewError("");
    if (!view) {
      setSavedViewId(null);
      setSavedViewName("");
      return;
    }
    setSavedViewId(view.view_id);
    setSavedViewName(view.name);
    if (isDashboardRouteId(view.route)) setRoute(view.route);
    const filters = asRecord(view.filters);
    const agentId = rawText(filters.selectedAgentId);
    if (agentId) setSelectedAgentId(agentId);
    if (isDensity(filters.density)) setDensity(filters.density);
    if (isTheme(filters.theme)) setTheme(filters.theme);
    if (filters.audit) setAuditFilters(sanitizeAuditFilters(filters.audit));
  };

  const saveCurrentView = async () => {
    const name = savedViewName.trim();
    if (!name) return;
    setSavedViewError("");
    try {
      const saved = await saveDashboardView({
        view_id: savedViewId || undefined,
        name,
        route,
        filters: {
          selectedAgentId,
          density,
          theme,
          audit: auditFilters
        }
      });
      setSavedViewId(saved.view.view_id);
      setSavedViewName(saved.view.name);
      await savedViewsQuery.refetch();
    } catch (error) {
      setSavedViewError(rawText(error) || "Saved view write failed");
    }
  };

  const deleteCurrentView = async () => {
    if (!savedViewId) return;
    setSavedViewError("");
    try {
      await deleteDashboardView(savedViewId);
      setSavedViewId(null);
      setSavedViewName("");
      await savedViewsQuery.refetch();
    } catch (error) {
      setSavedViewError(rawText(error) || "Saved view delete failed");
    }
  };

  return (
    <AppShell sidebar={<Sidebar route={route} setRoute={setRoute} state={state} />}>
      <PageHeader
        title={activeRoute.title}
        subtitle={
          <span className={stale ? "text-warning-fg" : "text-secondary"}>
            {query.isError ? rawText(query.error) : `Updated ${freshnessMs === undefined ? "pending" : timeAgo(freshnessMs)} ago`}
          </span>
        }
        actions={
          <>
            <SavedViewControls
              views={savedViewsQuery.data?.views ?? []}
              selectedId={savedViewId}
              name={savedViewName}
              error={savedViewError}
              onNameChange={setSavedViewName}
              onSelect={applySavedView}
              onSave={saveCurrentView}
              onDelete={deleteCurrentView}
              saving={savedViewsQuery.isFetching}
            />
            <Tooltip>
              <TooltipTrigger asChild>
                <Button size="icon" variant="ghost" onClick={() => setPaletteOpen(true)} aria-label="Open command palette" aria-keyshortcuts="/ P Control+K">
                  <Command aria-hidden="true" className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Command palette</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button size="icon" variant="ghost" onClick={() => query.refetch()} aria-label="Refresh dashboard state">
                  <RefreshCw aria-hidden="true" className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Refresh</TooltipContent>
            </Tooltip>
            <DensityControl density={density} setDensity={setDensity} />
            <label className="flex items-center gap-2 text-sm text-secondary">
              {theme === "dark" ? <Moon aria-hidden="true" className="h-4 w-4" /> : <Sun aria-hidden="true" className="h-4 w-4" />}
              <Switch checked={theme === "light"} onCheckedChange={(checked) => setTheme(checked ? "light" : "dark")} aria-label="Toggle light theme" />
            </label>
          </>
        }
      />

      {normalizedRoute === "fleet" ? (
        <FleetView
          state={state}
          agents={agents}
          attentionAgents={attentionAgents}
          selectedAgent={selectedAgent}
          setSelectedAgentId={setSelectedAgentId}
          attentionCount={attentionCount}
          stale={stale}
          toolCalls={toolCalls}
          advanceAttention={advanceAttention}
          onSpawned={() => query.refetch()}
          onAuditKeySelect={focusAuditKey}
        />
      ) : null}
      {normalizedRoute === "agent" ? <AgentView state={state} selectedAgent={selectedAgent} toolCalls={toolCalls} onAuditKeySelect={focusAuditKey} /> : null}
      {normalizedRoute === "tasks" ? <TasksView agents={agents} attentionCount={attentionCount} /> : null}
      {normalizedRoute === "system" ? (
        <SystemView state={state} agents={agents} attentionCount={attentionCount} toolCalls={toolCalls} stale={stale} onRefresh={() => query.refetch()} />
      ) : null}
      {normalizedRoute === "audit" ? (
        <AuditView state={state} toolCalls={toolCalls} filters={auditFilters} onFiltersChange={setAuditFilters} />
      ) : null}

      <CommandPalette open={paletteOpen} commands={commands} onClose={() => setPaletteOpen(false)} />
    </AppShell>
  );
}

function Sidebar({
  route,
  setRoute,
  state
}: {
  route: DashboardRouteId;
  setRoute: (route: DashboardRouteId) => void;
  state?: DashboardState;
}) {
  const health = asRecord(panelData(state?.daemon));
  return (
    <nav className="space-y-4" aria-label="Dashboard">
      <button type="button" className="flex w-full items-center gap-3 text-left" onClick={() => setRoute("fleet")}>
        <img src={commandMarkUrl} alt="" className="h-10 w-10 rounded-lg border border-border bg-surface-2 object-cover" />
        <span className="min-w-0">
          <span className="block text-md font-semibold text-primary">Synapse</span>
          <span className="block truncate text-xs text-muted">{rawText(health.version || "dashboard")}</span>
        </span>
      </button>
      <div className="grid gap-1">
        {routeDefinitions.map((item) => (
          <SidebarItem key={item.id} item={item} active={route === item.id} onSelect={() => setRoute(item.id)} />
        ))}
      </div>
      <div className="rounded-lg border border-border bg-surface-2 p-3">
        <div className="text-label font-medium uppercase text-muted">Loopback</div>
        <div className="mt-1 truncate font-mono text-sm text-primary">{state?.bind_addr || "pending"}</div>
      </div>
      <div className="rounded-lg border border-border bg-surface-2 p-3">
        <div className="text-label font-medium uppercase text-muted">Trust</div>
        <div className="mt-1 truncate font-mono text-sm text-primary">local-only</div>
      </div>
      <img src={statusIconsUrl} alt="" className="w-full rounded-lg border border-border bg-surface-2 object-cover" />
    </nav>
  );
}

function SidebarItem({ item, active, onSelect }: { item: RouteDefinition; active: boolean; onSelect: () => void }) {
  const Icon = item.icon;
  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        "flex min-h-10 w-full items-center gap-2 rounded-md px-3 text-left text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-focus-ring",
        active ? "bg-surface-2 text-primary" : "text-secondary hover:bg-surface-2 hover:text-primary"
      )}
    >
      <Icon aria-hidden="true" className="h-4 w-4" />
      {item.label}
    </button>
  );
}

function SavedViewControls({
  views,
  selectedId,
  name,
  error,
  onNameChange,
  onSelect,
  onSave,
  onDelete,
  saving
}: {
  views: DashboardSavedView[];
  selectedId: string | null;
  name: string;
  error: string;
  onNameChange: (value: string) => void;
  onSelect: (view?: DashboardSavedView) => void;
  onSave: () => void;
  onDelete: () => void;
  saving: boolean;
}) {
  return (
    <div className="flex max-w-full flex-wrap items-center gap-2">
      <label className="sr-only" htmlFor="saved-view-select">
        Saved view
      </label>
      <select
        id="saved-view-select"
        className="h-9 min-w-36 rounded-md border border-border bg-surface-1 px-2 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
        value={selectedId ?? ""}
        onChange={(event) => onSelect(views.find((view) => view.view_id === event.target.value))}
      >
        <option value="">Default view</option>
        {views.map((view) => (
          <option key={view.view_id} value={view.view_id}>
            {view.name}
          </option>
        ))}
      </select>
      <label className="sr-only" htmlFor="saved-view-name">
        View name
      </label>
      <input
        id="saved-view-name"
        className="h-9 w-36 rounded-md border border-border bg-surface-1 px-2 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
        value={name}
        placeholder="View name"
        onChange={(event) => onNameChange(event.target.value)}
      />
      <Tooltip>
        <TooltipTrigger asChild>
          <Button size="icon" variant="ghost" onClick={onSave} disabled={!name.trim() || saving} aria-label="Save view">
            <Save aria-hidden="true" className="h-4 w-4" />
          </Button>
        </TooltipTrigger>
        <TooltipContent>Save view</TooltipContent>
      </Tooltip>
      <Tooltip>
        <TooltipTrigger asChild>
          <Button size="icon" variant="ghost" onClick={onDelete} disabled={!selectedId || saving} aria-label="Delete view">
            <Trash2 aria-hidden="true" className="h-4 w-4" />
          </Button>
        </TooltipTrigger>
        <TooltipContent>Delete view</TooltipContent>
      </Tooltip>
      {error ? <span className="max-w-56 truncate text-xs text-danger-fg">{error}</span> : null}
    </div>
  );
}

function DensityControl({
  density,
  setDensity
}: {
  density: Density;
  setDensity: (density: Density) => void;
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

function CommandPalette({
  open,
  commands,
  onClose
}: {
  open: boolean;
  commands: Array<{ id: string; label: string; group: string; icon: LucideIcon; action: () => void }>;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    if (open) {
      setQuery("");
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);
  if (!open) return null;
  const filtered = commands.filter((command) => `${command.group} ${command.label}`.toLowerCase().includes(query.toLowerCase())).slice(0, 12);
  return (
    <div className="fixed inset-0 z-50 bg-black/60 p-4" role="dialog" aria-modal="true" aria-label="Command palette" onMouseDown={onClose}>
      <div className="mx-auto mt-16 max-w-2xl rounded-lg border border-border bg-surface-1 shadow-xl" onMouseDown={(event) => event.stopPropagation()}>
        <div className="flex items-center gap-2 border-b border-border px-3 py-2">
          <Search aria-hidden="true" className="h-4 w-4 text-muted" />
          <input
            ref={inputRef}
            className="h-10 min-w-0 flex-1 bg-transparent text-sm text-primary outline-none"
            value={query}
            placeholder="Search commands"
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") onClose();
              if (event.key === "Enter" && filtered[0]) {
                filtered[0].action();
                onClose();
              }
            }}
          />
          <Button size="icon" variant="ghost" onClick={onClose} aria-label="Close command palette">
            <X aria-hidden="true" className="h-4 w-4" />
          </Button>
        </div>
        <div className="max-h-[60vh] overflow-auto p-2">
          {filtered.length ? (
            filtered.map((command) => {
              const Icon = command.icon;
              return (
                <button
                  key={command.id}
                  type="button"
                  className="flex min-h-11 w-full items-center gap-3 rounded-md px-3 text-left text-sm text-secondary hover:bg-surface-2 hover:text-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-focus-ring"
                  onClick={() => {
                    command.action();
                    onClose();
                  }}
                >
                  <Icon aria-hidden="true" className="h-4 w-4 text-info" />
                  <span className="min-w-0 flex-1 truncate">{command.label}</span>
                  <span className="text-label uppercase text-muted">{command.group}</span>
                </button>
              );
            })
          ) : (
            <EmptyState title="No matching commands" />
          )}
        </div>
      </div>
    </div>
  );
}

function FleetView({
  state,
  agents,
  attentionAgents,
  selectedAgent,
  setSelectedAgentId,
  attentionCount,
  stale,
  toolCalls,
  advanceAttention,
  onSpawned,
  onAuditKeySelect
}: {
  state?: DashboardState;
  agents: AgentSummary[];
  attentionAgents: AgentSummary[];
  selectedAgent?: AgentSummary;
  setSelectedAgentId: (id: string) => void;
  attentionCount: number;
  stale: boolean;
  toolCalls: ReturnType<typeof buildToolCalls>;
  advanceAttention: () => void;
  onSpawned: () => void;
  onAuditKeySelect: (keyHex: string) => void;
}) {
  const [killingId, setKillingId] = useState("");
  const [killError, setKillError] = useState("");
  const [killReadback, setKillReadback] = useState<AgentKillResponse | null>(null);

  const killFromFleet = async (agent: AgentSummary) => {
    if (!agent.killable || !agent.killId || killingId) return;
    setKillingId(agent.killId);
    setKillError("");
    try {
      const readback = await killAgent({
        session_id: agent.killId,
        grace_ms: 0,
        interrupt_first: false
      });
      setKillReadback(readback);
      onSpawned();
    } catch (error) {
      setKillError(rawText(error) || "Agent kill failed");
    } finally {
      setKillingId("");
    }
  };

  return (
    <>
      <OverviewBand state={state} agents={agents} attentionCount={attentionCount} stale={stale} />
      {/* #1027: the approval/attention inbox lives INSIDE the fleet manager so every
          agent that has stopped — awaiting approval, asking a question, or ready for
          review — is actionable here, across all model kinds. `#/approvals` aliases
          to this page. Reuses the same ApprovalsView component (single, non-divergent
          surface) fed from the same `state.approvals` panel. */}
      <ApprovalsView state={state} onDecided={onSpawned} />
      <Section
        title="Spawn Console"
        tier="triage"
        questions={["Which models can I launch right now?", "How do I add a cloud API model like DeepSeek?", "Did the spawn succeed, step by step?"]}
      >
        <SpawnConsole onSpawned={onSpawned} />
      </Section>
      <Section
        title="Templates"
        tier="triage"
        questions={["What reusable spawn presets do I have?", "What model/directory/prompt does each run?", "Can I launch one right now?"]}
      >
        <TemplatesPanel onSpawned={onSpawned} />
      </Section>
      <div className="grid gap-6 xl:grid-cols-[minmax(0,1fr)_minmax(20rem,0.42fr)]">
        <div className="min-w-0">
          <Section
            title="Attention Groups"
            tier="triage"
            questions={["Which agents need a human now?", "Which session should I inspect first?", "What changed since the last refresh?"]}
            actions={
              <Button variant="secondary" size="sm" onClick={advanceAttention} disabled={attentionAgents.length === 0} aria-keyshortcuts="N Alt+N">
                <Bell aria-hidden="true" className="h-4 w-4" />
                Next
              </Button>
            }
          >
            <FleetList
              agents={attentionAgents.length ? attentionAgents : agents}
              selectedId={selectedAgent?.id}
              onSelect={setSelectedAgentId}
              onKill={killFromFleet}
              killingId={killingId}
            />
          </Section>
          <ToolActivity toolCalls={toolCalls} onAuditKeySelect={onAuditKeySelect} />
          <Section
            title="Fleet Table"
            tier="drill-down"
            questions={["Which sessions are live?", "Which rows are stale?", "Which row links to detail?"]}
          >
            <FleetTable agents={agents} onSelect={setSelectedAgentId} onKill={killFromFleet} killingId={killingId} />
            {killError ? <div className="mt-3 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{killError}</div> : null}
            {killReadback ? <RawValue value={killReadback} label="Kill readback" /> : null}
          </Section>
        </div>
        <aside className="min-w-0">
          <Section
            title="Peek Panel"
            tier="drill-down"
            questions={["Why is this agent in its current state?", "Which detail surface proves it?", "Is raw verification available without flooding the page?"]}
          >
            <AgentPeek agent={selectedAgent} />
          </Section>
          <Section
            title="System Shape"
            tier="overview"
            questions={["Is storage pressure rising?", "Which column family is largest?", "Is the daemon still local?"]}
          >
            <SystemShape state={state} />
          </Section>
        </aside>
      </div>
    </>
  );
}

function AgentView({
  state,
  selectedAgent,
  toolCalls,
  onAuditKeySelect
}: {
  state?: DashboardState;
  selectedAgent?: AgentSummary;
  toolCalls: ReturnType<typeof buildToolCalls>;
  onAuditKeySelect: (keyHex: string) => void;
}) {
  return (
    <div className="grid gap-6 xl:grid-cols-[minmax(0,0.42fr)_minmax(0,1fr)]">
      <Section
        title="Agent Surfaces"
        tier="drill-down"
        questions={["Which agent is selected?", "Which state explains the current badge?", "Can raw verification stay collapsed?"]}
      >
        <AgentPeek agent={selectedAgent} />
      </Section>
      <div className="min-w-0">
        <ToolActivity toolCalls={toolCalls} onAuditKeySelect={onAuditKeySelect} />
        <TranscriptSamples state={state} />
      </div>
    </div>
  );
}

function TasksView({ agents, attentionCount }: { agents: AgentSummary[]; attentionCount: number }) {
  const reviewAgents = agents.filter((agent) => agent.status === "ready_for_review").length;
  return (
    <>
      <Section
        title="Task Queue"
        tier="overview"
        questions={["How many review items need action?", "Which rows are attention candidates?", "Is the shell route stable for #924?"]}
      >
        <div className="grid gap-4 md:grid-cols-3">
          <StatCard label="Attention" value={attentionCount} status={attentionCount ? "needs_input" : "done"} />
          <StatCard label="Review" value={reviewAgents} status={reviewAgents ? "ready_for_review" : "idle"} />
          <StatCard label="Shell" value="ready" status="working" />
        </div>
      </Section>
      <Section
        title="Board"
        tier="triage"
        questions={["Which task lane is populated?", "Which attempt needs review?", "Where will queue transitions appear?"]}
      >
        <EmptyStateArt title="No task board rows in this shell feed" />
      </Section>
    </>
  );
}

interface ApprovalRow {
  approvalId: string;
  kind: string;
  status: string;
  title: string;
  body: string;
  destructive: boolean;
  createdMs: number;
  updatedMs: number;
  expiresMs?: number;
  toolName?: string;
  spawnId?: string;
  command?: string;
  inputPretty?: string;
  raw: Record<string, unknown>;
}

function parseApprovalRows(panel?: DashboardState["approvals"]): ApprovalRow[] {
  const data = asRecord(panelData(panel));
  const rows = asArray<Record<string, unknown>>(data.rows);
  return rows
    .map((row): ApprovalRow | null => {
      const item = asRecord(row.item);
      const approvalId = rawText(item.approval_id);
      if (!approvalId) return null;
      let payload: Record<string, unknown> = {};
      const payloadJson = item.payload_json;
      if (typeof payloadJson === "string" && payloadJson.length) {
        try {
          payload = asRecord(JSON.parse(payloadJson));
        } catch {
          /* leave payload empty; raw stays available below */
        }
      }
      const input = payload.input;
      const command =
        asRecord(input).command && typeof asRecord(input).command === "string"
          ? (asRecord(input).command as string)
          : undefined;
      const inputPretty =
        input === undefined || input === null
          ? undefined
          : JSON.stringify(input, null, 2);
      return {
        approvalId,
        kind: rawText(item.kind) || "unknown",
        status: rawText(item.status) || "pending",
        title: rawText(item.title) || approvalId,
        body: rawText(item.body),
        destructive: item.destructive === true,
        createdMs: Number(item.created_at_unix_ms) || 0,
        updatedMs: Number(item.updated_at_unix_ms) || 0,
        expiresMs:
          item.expires_at_unix_ms == null ? undefined : Number(item.expires_at_unix_ms),
        toolName: rawText(payload.tool_name) || undefined,
        spawnId: rawText(payload.spawn_id) || undefined,
        command,
        inputPretty,
        raw: item
      };
    })
    .filter((row): row is ApprovalRow => row !== null);
}

const APPROVAL_KIND_LABEL: Record<string, string> = {
  agent_permission: "Agent permission",
  agent_escalation: "Agent escalation",
  suggestion: "Suggestion",
  armed_run_review: "Armed run"
};

function ApprovalsView({ state, onDecided }: { state?: DashboardState; onDecided?: () => void }) {
  const rows = useMemo(() => parseApprovalRows(state?.approvals), [state]);
  const agentRows = useMemo(
    () => rows.filter((row) => row.kind === "agent_permission" || row.kind === "agent_escalation"),
    [rows]
  );
  const otherRows = useMemo(
    () => rows.filter((row) => row.kind !== "agent_permission" && row.kind !== "agent_escalation"),
    [rows]
  );

  const [busy, setBusy] = useState<Record<string, boolean>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [notes, setNotes] = useState<Record<string, string>>({});
  const [selected, setSelected] = useState<Set<string>>(new Set());

  async function decide(approvalId: string, decision: "approve" | "deny") {
    setBusy((prev) => ({ ...prev, [approvalId]: true }));
    setErrors((prev) => {
      const next = { ...prev };
      delete next[approvalId];
      return next;
    });
    try {
      const note = notes[approvalId]?.trim();
      await decideApproval({ approval_id: approvalId, decision, note: note || undefined });
      setSelected((prev) => {
        const next = new Set(prev);
        next.delete(approvalId);
        return next;
      });
      onDecided?.();
    } catch (err) {
      setErrors((prev) => ({
        ...prev,
        [approvalId]: err instanceof Error ? err.message : String(err)
      }));
    } finally {
      setBusy((prev) => ({ ...prev, [approvalId]: false }));
    }
  }

  async function decideSelected(decision: "approve" | "deny") {
    for (const id of Array.from(selected)) {
      // Sequential so one failure surfaces against its own card without
      // racing the shared refetch.
      // eslint-disable-next-line no-await-in-loop
      await decide(id, decision);
    }
  }

  function toggleSelected(approvalId: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(approvalId)) next.delete(approvalId);
      else next.add(approvalId);
      return next;
    });
  }

  const selectedCount = selected.size;

  return (
    <div className="space-y-6">
      <Section
        title="Agents awaiting your decision"
        tier="triage"
        questions={[
          "Which agents are paused waiting on a human?",
          "What exact action does each want to run?",
          "Does approving here resume the blocked agent?"
        ]}
        actions={
          selectedCount ? (
            <div className="flex items-center gap-2">
              <span className="text-xs text-secondary">{selectedCount} selected</span>
              <Button size="sm" variant="secondary" onClick={() => void decideSelected("approve")}>
                <CheckCircle2 className="size-4" /> Approve selected
              </Button>
              <Button size="sm" variant="ghost" onClick={() => void decideSelected("deny")}>
                <X className="size-4" /> Deny selected
              </Button>
            </div>
          ) : undefined
        }
      >
        <div className="mb-3 flex items-center gap-3">
          <StatCard
            label="Pending"
            value={agentRows.length}
            status={agentRows.length ? "awaiting_approval" : "done"}
            delta={agentRows.length ? "blocking agents" : "fleet unblocked"}
          />
        </div>
        {agentRows.length ? (
          <div className="space-y-3">
            {agentRows.map((row) => (
              <ApprovalCard
                key={row.approvalId}
                row={row}
                busy={!!busy[row.approvalId]}
                error={errors[row.approvalId]}
                note={notes[row.approvalId] ?? ""}
                selected={selected.has(row.approvalId)}
                onToggleSelected={() => toggleSelected(row.approvalId)}
                onNote={(value) => setNotes((prev) => ({ ...prev, [row.approvalId]: value }))}
                onApprove={() => void decide(row.approvalId, "approve")}
                onDeny={() => void decide(row.approvalId, "deny")}
              />
            ))}
          </div>
        ) : (
          <EmptyStateArt title="No agents are waiting for approval" />
        )}
      </Section>

      {otherRows.length ? (
        <Section
          title="Suggestions & armed runs"
          tier="triage"
          questions={["Which non-blocking proposals are queued?", "Do any need a decision before they expire?"]}
        >
          <div className="space-y-3">
            {otherRows.map((row) => (
              <ApprovalCard
                key={row.approvalId}
                row={row}
                busy={!!busy[row.approvalId]}
                error={errors[row.approvalId]}
                note={notes[row.approvalId] ?? ""}
                selected={selected.has(row.approvalId)}
                onToggleSelected={() => toggleSelected(row.approvalId)}
                onNote={(value) => setNotes((prev) => ({ ...prev, [row.approvalId]: value }))}
                onApprove={() => void decide(row.approvalId, "approve")}
                onDeny={() => void decide(row.approvalId, "deny")}
              />
            ))}
          </div>
        </Section>
      ) : null}
    </div>
  );
}

function ApprovalCard({
  row,
  busy,
  error,
  note,
  selected,
  onToggleSelected,
  onNote,
  onApprove,
  onDeny
}: {
  row: ApprovalRow;
  busy: boolean;
  error?: string;
  note: string;
  selected: boolean;
  onToggleSelected: () => void;
  onNote: (value: string) => void;
  onApprove: () => void;
  onDeny: () => void;
}) {
  const kindLabel = APPROVAL_KIND_LABEL[row.kind] ?? row.kind;
  const expiresInMs = row.expiresMs ? row.expiresMs - Date.now() : undefined;
  const expiresLabel =
    expiresInMs === undefined
      ? undefined
      : expiresInMs <= 0
        ? "expired"
        : `expires in ${Math.max(1, Math.round(expiresInMs / 60000))}m`;

  return (
    <div className="rounded-lg border border-border bg-surface p-[var(--density-card-padding)]">
      <div className="flex items-start gap-3">
        <input
          type="checkbox"
          aria-label="Select approval"
          checked={selected}
          onChange={onToggleSelected}
          className="mt-1 size-4 accent-[var(--accent)]"
        />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="rounded bg-canvas px-2 py-0.5 font-mono text-[11px] uppercase tracking-wide text-secondary">
              {kindLabel}
            </span>
            {row.destructive ? (
              <span className="rounded bg-[var(--danger-surface,#3a1f1f)] px-2 py-0.5 text-[11px] font-semibold text-[var(--danger,#ff6b6b)]">
                destructive
              </span>
            ) : null}
            {row.toolName ? (
              <span className="font-mono text-xs text-primary">{row.toolName}</span>
            ) : null}
            <span className="ml-auto text-[11px] text-tertiary">
              {row.createdMs ? timeAgo(row.createdMs) : ""}
              {expiresLabel ? ` · ${expiresLabel}` : ""}
            </span>
          </div>

          <div className="mt-1 text-sm font-medium text-primary">{row.title}</div>
          {row.spawnId ? (
            <div className="mt-0.5 font-mono text-[11px] text-tertiary">agent: {row.spawnId}</div>
          ) : null}

          {row.command ? (
            <pre className="mt-2 overflow-x-auto rounded bg-canvas p-2 font-mono text-xs text-primary">
              {row.command}
            </pre>
          ) : row.inputPretty ? (
            <pre className="mt-2 max-h-48 overflow-auto rounded bg-canvas p-2 font-mono text-[11px] text-secondary">
              {row.inputPretty}
            </pre>
          ) : (
            <div className="mt-2 text-xs text-secondary">{row.body}</div>
          )}

          <div className="mt-3 flex flex-wrap items-center gap-2">
            <input
              type="text"
              value={note}
              onChange={(event) => onNote(event.target.value)}
              placeholder="Optional note / reason…"
              className="h-8 min-w-[12rem] flex-1 rounded border border-border bg-canvas px-2 text-xs text-primary placeholder:text-tertiary"
            />
            <Button size="sm" variant="secondary" disabled={busy} onClick={onApprove}>
              <CheckCircle2 className="size-4" /> {busy ? "…" : "Approve"}
            </Button>
            <Button size="sm" variant="ghost" disabled={busy} onClick={onDeny}>
              <X className="size-4" /> Deny
            </Button>
          </div>
          {error ? (
            <div className="mt-2 text-xs text-[var(--danger,#ff6b6b)]">{error}</div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Storage usage model (#927 follow-up): disk usage is measured in BYTES
// (cf_sizes), never row counts. A column family with millions of tiny rows can
// occupy less disk than one with a few large OCR/screenshot blobs, so the byte
// footprint is the only honest answer to "what is using my disk".
// ---------------------------------------------------------------------------

type CfCategory = "recordings" | "audit" | "agents" | "intelligence" | "system";

interface CfMeta {
  label: string;
  category: CfCategory;
  description: string;
  /** Which management lever applies: time-range timeline purge, row-cap GC, or none. */
  manage: "timeline" | "gc" | null;
}

const CF_META: Record<string, CfMeta> = {
  CF_TIMELINE: {
    label: "Activity Recordings",
    category: "recordings",
    description: "Focus changes, browser navigation, window titles, clipboard and file activity captured as you work.",
    manage: "timeline"
  },
  CF_ACTION_LOG: {
    label: "Action & Command Audit",
    category: "audit",
    description: "Every tool / command invocation and its outcome.",
    manage: "gc"
  },
  CF_REFLEX_AUDIT: {
    label: "Reflex Audit",
    category: "audit",
    description: "Fired automation reflexes and the events that triggered them.",
    manage: "gc"
  },
  CF_EVENTS: { label: "Event Stream", category: "audit", description: "Internal event-bus records.", manage: "gc" },
  CF_AGENT_TRANSCRIPTS: {
    label: "Agent Transcripts",
    category: "agents",
    description: "Captured stdout transcripts from spawned agents.",
    manage: "gc"
  },
  CF_AGENT_EVENTS: {
    label: "Agent Journal",
    category: "agents",
    description: "Lifecycle journal (OTel GenAI) for spawned agents.",
    manage: "gc"
  },
  CF_EPISODES: {
    label: "Activity Episodes",
    category: "intelligence",
    description: "Segmented work sessions derived from the timeline.",
    manage: "gc"
  },
  CF_ROUTINES: { label: "Mined Routines", category: "intelligence", description: "Recurring patterns mined from episodes.", manage: null },
  CF_ROUTINE_STATE: {
    label: "Routine State",
    category: "intelligence",
    description: "Operator lifecycle state for mined routines.",
    manage: null
  },
  CF_OBSERVATIONS: {
    label: "Observations / OCR",
    category: "intelligence",
    description: "Screen observations and OCR snapshots — large blobs, few rows.",
    manage: "gc"
  },
  CF_OCR_CACHE: { label: "OCR Cache", category: "system", description: "Cached OCR text keyed by image hash.", manage: "gc" },
  CF_PROCESS_HISTORY: {
    label: "Process History",
    category: "system",
    description: "Foreground process and window history.",
    manage: "gc"
  },
  CF_KV: {
    label: "Config & State",
    category: "system",
    description: "Dashboard auth, saved views, templates, control rows, encrypted secrets.",
    manage: null
  },
  CF_MODEL_CACHE: { label: "Model Cache", category: "system", description: "Cached model metadata.", manage: null },
  CF_SESSIONS: { label: "Sessions", category: "system", description: "Session lifecycle and ownership records.", manage: null },
  CF_PROFILES: { label: "Profiles", category: "system", description: "Installed perception / automation profiles.", manage: null },
  CF_TELEMETRY: { label: "Telemetry", category: "system", description: "Local telemetry counters.", manage: null }
};

const CATEGORY_ORDER: CfCategory[] = ["recordings", "audit", "agents", "intelligence", "system"];

const CATEGORY_META: Record<CfCategory, { label: string; color: string }> = {
  recordings: { label: "Activity Recordings", color: "var(--info)" },
  audit: { label: "Audit & Logs", color: "var(--warning)" },
  agents: { label: "Agent Data", color: "var(--success)" },
  intelligence: { label: "Derived Intelligence", color: "var(--accent)" },
  system: { label: "System & Cache", color: "var(--text-muted)" }
};

function cfMeta(cf: string): CfMeta {
  return CF_META[cf] ?? { label: cf.replace(/^CF_/, ""), category: "system", description: "Uncategorized column family.", manage: null };
}

/** Epoch ms → epoch ns as a decimal string (ns exceed Number.MAX_SAFE_INTEGER). */
function msToNsString(ms: number): string {
  return (BigInt(Math.floor(ms)) * 1_000_000n).toString();
}

// Mirrors synapse-storage gc.rs `remove_count` (Rows unit): an explicit row cap
// evicts EXACTLY the overage, landing the store precisely on the cap (no minimum
// batch), so the dashboard's predicted count is the count GC will delete.
function predictEviction(rows: number, cap: number): number {
  if (rows <= cap || rows <= 0) return 0;
  return rows - cap;
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exp;
  const text = value >= 100 || exp === 0 ? Math.round(value).toString() : value.toFixed(1);
  return `${text} ${units[exp]}`;
}

interface StorageRow {
  cf: string;
  label: string;
  category: CfCategory;
  description: string;
  manage: CfMeta["manage"];
  bytes: number;
  rows: number;
  avgRow: number;
  pct: number;
}

interface StorageModel {
  rows: StorageRow[];
  totalBytes: number;
  totalRows: number;
  maxBytes: number;
  categoryTotals: Array<{ category: CfCategory; bytes: number; pct: number }>;
}

function buildStorageModel(state?: DashboardState): StorageModel {
  const storage = asRecord(panelData(state?.storage));
  const sizes = asRecord(storage.cf_sizes);
  const counts = asRecord(storage.cf_row_counts);
  const names = new Set<string>([...Object.keys(sizes), ...Object.keys(counts)]);
  const rows: StorageRow[] = [...names].map((cf) => {
    const meta = cfMeta(cf);
    const bytes = Number(sizes[cf]) || 0;
    const rowCount = Number(counts[cf]) || 0;
    return {
      cf,
      label: meta.label,
      category: meta.category,
      description: meta.description,
      manage: meta.manage,
      bytes,
      rows: rowCount,
      avgRow: rowCount > 0 ? bytes / rowCount : 0,
      pct: 0
    };
  });
  const totalBytes = rows.reduce((sum, row) => sum + row.bytes, 0);
  const totalRows = rows.reduce((sum, row) => sum + row.rows, 0);
  for (const row of rows) row.pct = totalBytes > 0 ? (row.bytes / totalBytes) * 100 : 0;
  rows.sort((a, b) => b.bytes - a.bytes);
  const maxBytes = rows.reduce((max, row) => Math.max(max, row.bytes), 0);
  const categoryTotals = CATEGORY_ORDER.map((category) => {
    const bytes = rows.filter((row) => row.category === category).reduce((sum, row) => sum + row.bytes, 0);
    return { category, bytes, pct: totalBytes > 0 ? (bytes / totalBytes) * 100 : 0 };
  }).filter((entry) => entry.bytes > 0);
  return { rows, totalBytes, totalRows, maxBytes, categoryTotals };
}

function StorageUsage({ state }: { state?: DashboardState }) {
  const model = useMemo(() => buildStorageModel(state), [state]);
  const storage = asRecord(panelData(state?.storage));
  const pressureName = rawText(asRecord(storage.pressure_level).name || "unknown");
  if (!model.rows.length || model.totalBytes <= 0) return <EmptyStateArt title="No storage rows" />;
  return (
    <div className="space-y-4">
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard label="Total on disk" value={formatBytes(model.totalBytes)} status="working" delta={`${model.totalRows.toLocaleString()} rows`} />
        <StatCard label="Stores" value={model.rows.filter((row) => row.bytes > 0).length} status="idle" delta={`${model.rows.length} column families`} />
        <StatCard label="Pressure" value={pressureName} status={/level[34]|refus/i.test(pressureName) ? "stuck" : /level[12]/i.test(pressureName) ? "needs_input" : "done"} delta={state?.storage.source || "storage_inspect"} />
      </div>

      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        <div className="mb-2 text-label font-medium uppercase text-muted">Composition by category (bytes)</div>
        <div className="flex h-4 w-full overflow-hidden rounded-md border border-border-subtle bg-surface-2" role="img" aria-label="Storage composition by category">
          {model.categoryTotals.map((entry) => (
            <div
              key={entry.category}
              style={{ width: `${Math.max(entry.pct, 0.5)}%`, background: CATEGORY_META[entry.category].color }}
              title={`${CATEGORY_META[entry.category].label}: ${formatBytes(entry.bytes)} (${entry.pct.toFixed(1)}%)`}
            />
          ))}
        </div>
        <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1">
          {model.categoryTotals.map((entry) => (
            <span key={entry.category} className="inline-flex items-center gap-2 text-xs text-secondary">
              <span className="h-2.5 w-2.5 rounded-sm" style={{ background: CATEGORY_META[entry.category].color }} aria-hidden="true" />
              {CATEGORY_META[entry.category].label}
              <span className="font-mono text-muted">{formatBytes(entry.bytes)} · {entry.pct.toFixed(0)}%</span>
            </span>
          ))}
        </div>
      </div>

      <div className="overflow-hidden rounded-lg border border-border">
        <table className="w-full min-w-[640px] border-collapse text-sm">
          <thead className="bg-surface-2 text-label uppercase text-muted">
            <tr>
              <th className="px-3 py-2 text-left font-medium">Data store</th>
              <th className="px-3 py-2 text-right font-medium">On disk</th>
              <th className="px-3 py-2 text-left font-medium">Share</th>
              <th className="px-3 py-2 text-right font-medium">Rows</th>
              <th className="px-3 py-2 text-right font-medium">Avg / row</th>
            </tr>
          </thead>
          <tbody>
            {model.rows.map((row) => (
              <tr key={row.cf} className="border-t border-border-subtle align-middle hover:bg-surface-2">
                <td className="px-3 py-2">
                  <div className="flex items-center gap-2">
                    <span className="h-2.5 w-2.5 shrink-0 rounded-sm" style={{ background: CATEGORY_META[row.category].color }} aria-hidden="true" />
                    <span className="min-w-0">
                      <span className="block text-primary">{row.label}</span>
                      <span className="block truncate text-xs text-muted" title={row.description}>{row.description}</span>
                    </span>
                  </div>
                </td>
                <td className="px-3 py-2 text-right font-mono text-primary">{formatBytes(row.bytes)}</td>
                <td className="px-3 py-2">
                  <div className="flex items-center gap-2">
                    <span className="h-2 w-24 overflow-hidden rounded-sm bg-surface-2" aria-hidden="true">
                      <span className="block h-full rounded-sm" style={{ width: `${model.maxBytes > 0 ? (row.bytes / model.maxBytes) * 100 : 0}%`, background: CATEGORY_META[row.category].color }} />
                    </span>
                    <span className="font-mono text-xs text-muted">{row.pct.toFixed(1)}%</span>
                  </div>
                </td>
                <td className="px-3 py-2 text-right font-mono text-secondary">{row.rows.toLocaleString()}</td>
                <td className="px-3 py-2 text-right font-mono text-secondary">{row.avgRow > 0 ? formatBytes(row.avgRow) : "-"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <RawValue value={{ cf_sizes: storage.cf_sizes, cf_row_counts: storage.cf_row_counts }} label="Raw storage byte + row counts" />
    </div>
  );
}

function RecordingsOverTime({ state }: { state?: DashboardState }) {
  const timeline = asRecord(panelData(state?.timeline));
  const byDay = asRecord(timeline.rows_by_day_utc);
  const byKind = asRecord(timeline.rows_by_kind);
  const dayData = Object.entries(byDay)
    .map(([day, value]) => ({ day, rows: Number(value) || 0 }))
    .sort((a, b) => a.day.localeCompare(b.day));
  const storageBytes = Number(timeline.storage_bytes) || 0;
  const totalRows = Number(timeline.total_rows) || 0;
  const oldest = timeline.oldest_ts_ns ? nsToTime(timeline.oldest_ts_ns as number) : "-";
  const newest = timeline.newest_ts_ns ? nsToTime(timeline.newest_ts_ns as number) : "-";
  const scanComplete = timeline.scan_complete !== false;
  return (
    <div className="space-y-4">
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard label="Recordings on disk" value={formatBytes(storageBytes)} status="working" delta={`${totalRows.toLocaleString()} rows`} />
        <StatCard label="Oldest → newest" value={dayData.length} status="idle" delta={`${oldest} → ${newest}`} />
        <StatCard label="Scan" value={scanComplete ? "complete" : "partial"} status={scanComplete ? "done" : "needs_input"} delta={state?.timeline.source || "timeline_stats"} />
      </div>
      {dayData.length ? (
        <div className="h-56 rounded-lg border border-border bg-surface-1 p-3">
          <ResponsiveContainer width="100%" height="100%">
            <BarChart data={dayData} margin={{ top: 8, right: 8, bottom: 8, left: 8 }}>
              <CartesianGrid stroke="var(--border-subtle)" vertical={false} />
              <XAxis dataKey="day" stroke="var(--text-muted)" tickLine={false} axisLine={false} fontSize={12} />
              <YAxis stroke="var(--text-muted)" tickLine={false} axisLine={false} fontSize={12} />
              <ChartTooltip contentStyle={{ background: "var(--surface-3)", border: "1px solid var(--border)", color: "var(--text-primary)" }} />
              <Bar dataKey="rows" fill="var(--info)" radius={[4, 4, 0, 0]} name="recordings" />
            </BarChart>
          </ResponsiveContainer>
        </div>
      ) : (
        <EmptyStateArt title="No timeline recordings in range" />
      )}
      <div className="grid gap-4 xl:grid-cols-2">
        <RawValue value={byKind} label="Recordings by kind" />
        <RawValue value={byDay} label="Recordings by day (UTC)" />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Storage management (#927 follow-up): give the operator direct control over
// data that builds up on their disk. Recordings purge by time period
// (timeline_purge — hard-delete + range compaction); other stores trim to their
// newest N rows (storage_gc_once — oldest-row eviction). Both destructive
// actions require an explicit confirm; the purge requires a dry-run preview
// first so the operator sees exactly how many recordings will be deleted.
// ---------------------------------------------------------------------------

function StorageManagement({ state, onRefresh }: { state?: DashboardState; onRefresh: () => void | Promise<unknown> }) {
  const timeline = asRecord(panelData(state?.timeline));
  const byKind = asRecord(timeline.rows_by_kind);
  const kindOptions = Object.keys(byKind)
    .filter((kind) => kind !== "purge")
    .sort();
  const model = useMemo(() => buildStorageModel(state), [state]);
  const gcCfs = model.rows.filter((row) => row.manage === "gc");

  const [purgeMode, setPurgeMode] = useState<"older" | "range" | "all">("older");
  const [olderDays, setOlderDays] = useState("30");
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [purgeKind, setPurgeKind] = useState("");
  const [purgePreview, setPurgePreview] = useState<TimelinePurgeReadback | null>(null);
  const [purgeConfirmed, setPurgeConfirmed] = useState(false);
  const [purgeBusy, setPurgeBusy] = useState<"preview" | "delete" | "">("");
  const [purgeError, setPurgeError] = useState("");
  const [purgeResult, setPurgeResult] = useState<TimelinePurgeReadback | null>(null);

  // Any filter change invalidates a prior preview so a delete can never run
  // against a count the operator did not just see.
  useEffect(() => {
    setPurgePreview(null);
    setPurgeConfirmed(false);
    setPurgeResult(null);
    setPurgeError("");
  }, [purgeMode, olderDays, startDate, endDate, purgeKind]);

  const buildPurgeRequest = (): TimelinePurgeRequest | string => {
    const base: TimelinePurgeRequest = {};
    if (purgeKind) base.kinds = [purgeKind];
    if (purgeMode === "all") {
      if (purgeKind) return "Clearing a single kind uses a time period — turn the kind filter off to clear everything.";
      base.all = true;
      return base;
    }
    if (purgeMode === "older") {
      const days = Number(olderDays.trim());
      if (!Number.isFinite(days) || days < 0) return "Enter a non-negative number of days.";
      base.end_ts_ns = msToNsString(Date.now() - days * 86400000);
      return base;
    }
    if (!startDate || !endDate) return "Pick both a start and end date.";
    const startMs = Date.parse(`${startDate}T00:00:00`);
    const endMs = Date.parse(`${endDate}T23:59:59.999`);
    if (!Number.isFinite(startMs) || !Number.isFinite(endMs)) return "Invalid date.";
    if (startMs > endMs) return "Start date must be on or before end date.";
    base.start_ts_ns = msToNsString(startMs);
    base.end_ts_ns = msToNsString(endMs);
    return base;
  };

  const runPurge = async (dryRun: boolean) => {
    const built = buildPurgeRequest();
    if (typeof built === "string") {
      setPurgeError(built);
      return;
    }
    setPurgeBusy(dryRun ? "preview" : "delete");
    setPurgeError("");
    try {
      const readback = await purgeTimelineRecordings({ ...built, dry_run: dryRun });
      if (dryRun) {
        setPurgePreview(readback);
        setPurgeConfirmed(false);
      } else {
        setPurgeResult(readback);
        setPurgePreview(null);
        setPurgeConfirmed(false);
        await onRefresh();
      }
    } catch (error) {
      setPurgeError(error instanceof Error ? error.message : String(error));
    } finally {
      setPurgeBusy("");
    }
  };

  const [gcCf, setGcCf] = useState("");
  const [gcKeep, setGcKeep] = useState("1000");
  const [gcConfirmed, setGcConfirmed] = useState(false);
  const [gcBusy, setGcBusy] = useState(false);
  const [gcError, setGcError] = useState("");
  const [gcResult, setGcResult] = useState<StorageGcReadback | null>(null);

  useEffect(() => {
    if (!gcCf && gcCfs.length > 0) setGcCf(gcCfs[0].cf);
  }, [gcCf, gcCfs]);
  useEffect(() => {
    setGcConfirmed(false);
    setGcResult(null);
    setGcError("");
  }, [gcCf, gcKeep]);

  const selectedGcRow = gcCfs.find((row) => row.cf === gcCf);
  const runGc = async () => {
    const keep = Number(gcKeep.trim());
    if (!Number.isInteger(keep) || keep < 1 || keep > 1_000_000) {
      setGcError("Keep newest rows must be an integer between 1 and 1,000,000.");
      return;
    }
    if (!gcCf) {
      setGcError("Pick a data store to trim.");
      return;
    }
    setGcBusy(true);
    setGcError("");
    try {
      const readback = await runStorageGc({ cf_name: gcCf, soft_cap_rows: keep, hard_cap_rows: keep });
      setGcResult(readback);
      setGcConfirmed(false);
      await onRefresh();
    } catch (error) {
      setGcError(error instanceof Error ? error.message : String(error));
    } finally {
      setGcBusy(false);
    }
  };

  return (
    <div className="grid gap-4 xl:grid-cols-2">
      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        <div className="mb-3 flex items-center gap-2 text-primary">
          <CalendarClock aria-hidden="true" className="h-4 w-4 text-info" />
          <h3 className="text-md font-medium tracking-normal">Clear activity recordings</h3>
        </div>
        <p className="mb-3 text-xs text-muted">
          Permanently delete recorded activity (CF_TIMELINE) to reclaim disk space. Preview first to see exactly how many recordings match, then confirm.
        </p>
        <div className="grid gap-3">
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Period</span>
            <select
              className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              value={purgeMode}
              onChange={(event) => setPurgeMode(event.target.value as "older" | "range" | "all")}
            >
              <option value="older">Older than…</option>
              <option value="range">Date range</option>
              <option value="all">Everything</option>
            </select>
          </label>
          {purgeMode === "older" ? (
            <label className="block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Delete recordings older than (days)</span>
              <input
                value={olderDays}
                onChange={(event) => setOlderDays(event.target.value)}
                inputMode="numeric"
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              />
            </label>
          ) : null}
          {purgeMode === "range" ? (
            <div className="grid gap-3 md:grid-cols-2">
              <label className="block text-sm text-secondary">
                <span className="mb-1 block text-label font-medium uppercase text-muted">From</span>
                <input type="date" value={startDate} onChange={(event) => setStartDate(event.target.value)} className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring" />
              </label>
              <label className="block text-sm text-secondary">
                <span className="mb-1 block text-label font-medium uppercase text-muted">To</span>
                <input type="date" value={endDate} onChange={(event) => setEndDate(event.target.value)} className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring" />
              </label>
            </div>
          ) : null}
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Kind (optional)</span>
            <select
              className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring disabled:opacity-50"
              value={purgeKind}
              disabled={purgeMode === "all"}
              onChange={(event) => setPurgeKind(event.target.value)}
            >
              <option value="">All kinds</option>
              {kindOptions.map((kind) => (
                <option key={kind} value={kind}>{kind}</option>
              ))}
            </select>
          </label>
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <Button type="button" variant="secondary" size="sm" disabled={Boolean(purgeBusy)} onClick={() => runPurge(true)}>
            <Search aria-hidden="true" className="h-4 w-4" />
            {purgeBusy === "preview" ? "Previewing" : "Preview"}
          </Button>
          {purgePreview ? (
            <label className="inline-flex h-9 items-center gap-2 rounded-md border border-border bg-surface-2 px-3 text-sm text-primary">
              <input type="checkbox" checked={purgeConfirmed} onChange={(event) => setPurgeConfirmed(event.target.checked)} className="h-4 w-4 accent-[rgb(var(--color-danger))]" />
              Confirm delete
            </label>
          ) : null}
          <Button type="button" variant="danger" size="sm" disabled={!purgePreview || !purgeConfirmed || Boolean(purgeBusy) || purgePreview.matched_rows === 0} onClick={() => runPurge(false)}>
            <Eraser aria-hidden="true" className="h-4 w-4" />
            {purgeBusy === "delete" ? "Deleting" : purgePreview ? `Delete ${purgePreview.matched_rows.toLocaleString()} recordings` : "Delete"}
          </Button>
        </div>
        {purgePreview ? (
          <div className="mt-3 rounded-md border border-warning-border bg-warning-bg p-3 text-sm text-warning-fg">
            Preview: {purgePreview.matched_rows.toLocaleString()} recordings match and would be deleted ({purgePreview.scanned_rows.toLocaleString()} scanned
            {purgePreview.stopped_because === "scan_budget_exhausted" ? "; scan budget paused — delete then preview again to continue" : ""}).
          </div>
        ) : null}
        {purgeError ? <div className="mt-3 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{purgeError}</div> : null}
        {purgeResult ? (
          <div className="mt-3 space-y-2">
            <div className="rounded-md border border-success-border bg-success-bg p-3 text-sm text-success-fg">
              Deleted {purgeResult.deleted_rows.toLocaleString()} recordings{purgeResult.compacted ? " and compacted the freed range" : ""}.
              {purgeResult.next_cursor ? " Scan budget paused — run the same purge again to continue." : ""}
            </div>
            <RawValue value={purgeResult} label="Purge readback" />
          </div>
        ) : null}
      </div>

      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        <div className="mb-3 flex items-center gap-2 text-primary">
          <Database aria-hidden="true" className="h-4 w-4 text-info" />
          <h3 className="text-md font-medium tracking-normal">Cap a data store</h3>
        </div>
        <p className="mb-3 text-xs text-muted">
          Cap a growing store (audit logs, transcripts, OCR, observations) at a row count. When it exceeds the cap, the oldest rows are permanently evicted down to exactly the cap to reclaim space.
        </p>
        <div className="grid gap-3">
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Data store</span>
            <select
              className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              value={gcCf}
              onChange={(event) => setGcCf(event.target.value)}
            >
              {gcCfs.map((row) => (
                <option key={row.cf} value={row.cf}>{row.label} ({row.rows.toLocaleString()} rows · {formatBytes(row.bytes)})</option>
              ))}
            </select>
          </label>
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Cap at rows</span>
            <input
              value={gcKeep}
              onChange={(event) => setGcKeep(event.target.value)}
              inputMode="numeric"
              className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
            />
          </label>
          {selectedGcRow ? (
            <p className="text-xs text-muted">
              {selectedGcRow.label} currently holds {selectedGcRow.rows.toLocaleString()} rows. Capping at {gcKeep || "?"} evicts the oldest{" "}
              {predictEviction(selectedGcRow.rows, Number(gcKeep) || 0).toLocaleString()} rows.
            </p>
          ) : null}
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <label className="inline-flex h-9 items-center gap-2 rounded-md border border-border bg-surface-2 px-3 text-sm text-primary">
            <input type="checkbox" checked={gcConfirmed} onChange={(event) => setGcConfirmed(event.target.checked)} className="h-4 w-4 accent-[rgb(var(--color-danger))]" />
            Confirm trim
          </label>
          <Button type="button" variant="danger" size="sm" disabled={!gcConfirmed || gcBusy || !gcCf} onClick={runGc}>
            <Trash2 aria-hidden="true" className="h-4 w-4" />
            {gcBusy ? "Trimming" : "Trim"}
          </Button>
        </div>
        {gcError ? <div className="mt-3 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{gcError}</div> : null}
        {gcResult ? (
          <div className="mt-3 space-y-2">
            <div className="rounded-md border border-success-border bg-success-bg p-3 text-sm text-success-fg">
              {gcResult.cf_name}: {gcResult.before_rows.toLocaleString()} → {gcResult.after_rows.toLocaleString()} rows ({gcResult.total_evicted_rows.toLocaleString()} evicted).
            </div>
            <RawValue value={gcResult} label="Trim readback" />
          </div>
        ) : null}
      </div>
    </div>
  );
}

function SystemView({
  state,
  agents,
  attentionCount,
  toolCalls,
  stale,
  onRefresh
}: {
  state?: DashboardState;
  agents: AgentSummary[];
  attentionCount: number;
  toolCalls: ReturnType<typeof buildToolCalls>;
  stale: boolean;
  onRefresh: () => void | Promise<unknown>;
}) {
  const health = asRecord(panelData(state?.daemon));
  const subsystems = asRecord(health.subsystems);
  const storage = asRecord(panelData(state?.storage));
  const pressure = asRecord(storage.pressure_level);
  const timeline = asRecord(panelData(state?.timeline));
  const recorder = asRecord(timeline.recorder);
  const lease = asRecord(panelData(state?.lease));
  const targetClaims = asRecord(panelData(state?.target_claims));
  const claims = asArray<Record<string, unknown>>(targetClaims.claims);
  const sessions = asArray<Record<string, unknown>>(asRecord(panelData(state?.sessions)).sessions);
  const commandAuditRows = asArray<Record<string, unknown>>(asRecord(panelData(state?.command_audit)).rows);
  const hiddenDesktops = asArray<Record<string, unknown>>(asRecord(panelData(state?.hidden_desktops)).rows);
  const cdpAttachments = asArray<Record<string, unknown>>(asRecord(panelData(state?.cdp_attachments)).rows);
  const shellJobsData = asRecord(panelData(state?.shell_jobs));
  const shellJobs = asArray<Record<string, unknown>>(shellJobsData.rows);
  const events = asRecord(panelData(state?.events));
  const pressureName = rawText(pressure.name || pressure.level || pressure.value || storage.pressure_level || "unknown");
  const pressureStatus: FleetStatus = /level[34]|l[34]|refus/i.test(pressureName) ? "stuck" : /level[12]|l[12]/i.test(pressureName) ? "needs_input" : "done";
  const recorderPaused = recorder.paused === true;
  const [recorderBusy, setRecorderBusy] = useState<"pause" | "resume" | "">("");
  const [recorderError, setRecorderError] = useState("");
  const [lastRecorderControl, setLastRecorderControl] = useState<TimelineControlResponse | null>(null);
  const ownerSessionId = rawText(lease.owner_session_id);
  const [leaseBusy, setLeaseBusy] = useState<"force" | "handoff" | "prune" | "">("");
  const [leaseError, setLeaseError] = useState("");
  const [lastLeaseControl, setLastLeaseControl] = useState<DashboardControlResponse | null>(null);
  const [forceReleaseOwner, setForceReleaseOwner] = useState("");
  const [forceReleaseConfirmed, setForceReleaseConfirmed] = useState(false);
  const [handoffToSession, setHandoffToSession] = useState("");
  const [handoffTtlMs, setHandoffTtlMs] = useState("5000");
  const forceReleaseTarget = forceReleaseOwner.trim() || ownerSessionId;
  const contentionRows = commandAuditRows.filter((row) => {
    const errorCode = rawText(row.error_code);
    return errorCode === "ACTION_FOREGROUND_LEASE_BUSY" || rawText(row).includes("ACTION_FOREGROUND_LEASE_BUSY");
  });
  const contentionGroups: Record<string, unknown>[] = Object.values(
    contentionRows.reduce<Record<string, { session_id: string; count: number; last_ts_ns: number; tools: string[] }>>((groups, row) => {
      const payload = asRecord(row.payload_bounded);
      const sessionId =
        rawText(row.actor_session_id || payload.session_id || payload.requesting_session_id || row.target_session_id) || "unknown";
      const group = groups[sessionId] ?? {
        session_id: sessionId,
        count: 0,
        last_ts_ns: 0,
        tools: []
      };
      group.count += 1;
      group.last_ts_ns = Math.max(group.last_ts_ns, Number(row.ts_ns || 0));
      const tool = rawText(row.tool || row.verb || "unknown");
      if (tool && !group.tools.includes(tool)) group.tools.push(tool);
      groups[sessionId] = group;
      return groups;
    }, {})
  )
    .sort((a, b) => b.last_ts_ns - a.last_ts_ns)
    .map((group) => ({
      session_id: group.session_id,
      count: group.count,
      last_ts_ns: group.last_ts_ns,
      tools: group.tools
    }));

  const submitRecorderControl = async (mode: "pause" | "resume") => {
    setRecorderBusy(mode);
    setRecorderError("");
    try {
      const response = mode === "pause" ? await pauseTimeline() : await resumeTimeline();
      setLastRecorderControl(response);
      await onRefresh();
    } catch (error) {
      setRecorderError(error instanceof Error ? error.message : String(error));
    } finally {
      setRecorderBusy("");
    }
  };

  const submitForceRelease = async () => {
    if (!forceReleaseTarget) return;
    setLeaseBusy("force");
    setLeaseError("");
    try {
      const response = await forceReleaseLease({
        owner_session_id: forceReleaseTarget,
        confirmed: forceReleaseConfirmed
      });
      setLastLeaseControl(response);
      setForceReleaseConfirmed(false);
      await onRefresh();
    } catch (error) {
      setLeaseError(error instanceof Error ? error.message : String(error));
    } finally {
      setLeaseBusy("");
    }
  };

  const submitHandoff = async () => {
    const toSession = handoffToSession.trim();
    if (!ownerSessionId || !toSession) return;
    const ttl = handoffTtlMs.trim() ? Number(handoffTtlMs.trim()) : undefined;
    if (ttl !== undefined && !Number.isFinite(ttl)) {
      setLeaseError("TOOL_PARAMS_INVALID: ttl_ms must be numeric");
      return;
    }
    setLeaseBusy("handoff");
    setLeaseError("");
    try {
      const response = await handoffLease({
        from_session_id: ownerSessionId,
        to_session_id: toSession,
        ttl_ms: ttl
      });
      setLastLeaseControl(response);
      await onRefresh();
    } catch (error) {
      setLeaseError(error instanceof Error ? error.message : String(error));
    } finally {
      setLeaseBusy("");
    }
  };

  const submitPruneClaims = async () => {
    setLeaseBusy("prune");
    setLeaseError("");
    try {
      const response = await pruneTargetClaims();
      setLastLeaseControl(response);
      await onRefresh();
    } catch (error) {
      setLeaseError(error instanceof Error ? error.message : String(error));
    } finally {
      setLeaseBusy("");
    }
  };

  const chromeBridge = asRecord(subsystems.chrome_bridge);
  const action = asRecord(subsystems.action);
  const daemonLifecycle = asRecord(subsystems.daemon_lifecycle);
  const http = asRecord(subsystems.http);

  return (
    <>
      <OverviewBand state={state} agents={agents} attentionCount={attentionCount} stale={stale} />

      <Section title="Substrate" tier="overview" questions={["Is the daemon healthy?", "Is storage refusing work?", "Is the recorder paused?"]}>
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
          <StatCard label="Health" value={health.ok === true ? "ok" : "check"} status={health.ok === true && !stale ? "done" : "stuck"} delta={`pid ${rawText(health.pid || "unknown")}`} />
          <StatCard label="Storage" value={pressureName || "unknown"} status={pressureStatus} delta={state?.storage.source || "storage_inspect"} />
          <StatCard label="Recorder" value={recorderPaused ? "paused" : "running"} status={recorderPaused ? "needs_input" : "working"} delta={state?.timeline.source || "timeline_stats"} />
          <StatCard label="Shell Jobs" value={rawText(shellJobsData.running_count || 0)} status={Number(shellJobsData.running_count || 0) ? "working" : "idle"} delta={`${rawText(shellJobsData.job_count || 0)} status files`} />
        </div>
      </Section>

      <Section
        title="Storage Usage"
        tier="triage"
        questions={["What is actually using disk space (in bytes)?", "Which category dominates the footprint?", "How big is each store and its average row?"]}
      >
        <StorageUsage state={state} />
      </Section>

      <Section
        title="Activity Recordings Over Time"
        tier="triage"
        questions={["How fast are recordings accumulating?", "Which days and kinds dominate?", "How much disk do recordings hold?"]}
      >
        <RecordingsOverTime state={state} />
      </Section>

      <Section
        title="Storage Management"
        tier="triage"
        questions={["How do I reclaim disk space?", "Exactly how many recordings will a purge delete?", "How do I trim a store that keeps growing?"]}
      >
        <StorageManagement state={state} onRefresh={onRefresh} />
      </Section>

      <Section
        title="Daemon"
        tier="triage"
        questions={["Which process owns the server?", "Is Chrome bridge connected?", "Is action routing healthy?"]}
      >
        <div className="grid gap-4 xl:grid-cols-[1fr_1fr]">
          <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
            <div className="mb-2 flex items-center gap-2 text-primary">
              <ServerCog aria-hidden="true" className="h-4 w-4 text-info" />
              <h3 className="text-md font-medium tracking-normal">Runtime</h3>
            </div>
            <MetricRow label="Bind" value={state?.bind_addr || rawText(http.bind_addr)} />
            <MetricRow label="PID" value={rawText(health.pid)} />
            <MetricRow label="Tools" value={rawText(health.tool_count)} />
            <MetricRow label="Lifecycle" value={rawText(daemonLifecycle.status)} />
            <MetricRow label="Trust" value="local-only (loopback + Host guard)" />
          </div>
          <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
            <div className="mb-2 flex items-center gap-2 text-primary">
              <Network aria-hidden="true" className="h-4 w-4 text-info" />
              <h3 className="text-md font-medium tracking-normal">Bridge</h3>
            </div>
            <MetricRow label="Chrome" value={rawText(chromeBridge.status)} />
            <MetricRow label="HTTP" value={rawText(http.status)} />
            <MetricRow label="Action" value={rawText(action.status)} />
            <MetricRow label="SSE" value={rawText(events.active_subscription_count || 0)} />
            <MetricRow label="Tool SoT" value={state?.daemon.source || "health"} />
          </div>
        </div>
        <div className="mt-3">
          <RawValue value={{ daemon: state?.daemon, chrome_bridge: chromeBridge }} label="Daemon readback" />
        </div>
      </Section>

      <Section
        title="Recorder"
        tier="triage"
        questions={["Is recording active?", "Do pause and resume hit the live gate?", "Which timeline counts changed?"]}
        actions={
          <>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button type="button" variant="ghost" size="sm" disabled={Boolean(recorderBusy)} onClick={() => submitRecorderControl("pause")}>
                  <Pause aria-hidden="true" className="h-4 w-4" />
                  {recorderBusy === "pause" ? "Pausing" : "Pause"}
                </Button>
              </TooltipTrigger>
              <TooltipContent>Pause timeline recorder</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button type="button" variant="ghost" size="sm" disabled={Boolean(recorderBusy)} onClick={() => submitRecorderControl("resume")}>
                  <Play aria-hidden="true" className="h-4 w-4" />
                  {recorderBusy === "resume" ? "Resuming" : "Resume"}
                </Button>
              </TooltipTrigger>
              <TooltipContent>Resume timeline recorder</TooltipContent>
            </Tooltip>
          </>
        }
      >
        <div className="grid gap-4 xl:grid-cols-3">
          <StatCard label="Rows" value={rawText(timeline.total_rows || 0)} status={timeline.scan_complete === false ? "needs_input" : "done"} delta={`${rawText(timeline.scanned_rows || 0)} scanned`} />
          <StatCard label="Invalid" value={rawText(timeline.invalid_rows || 0)} status={Number(timeline.invalid_rows || 0) ? "stuck" : "done"} delta="CF_TIMELINE decode" />
          <StatCard label="Feeds" value={recorder.clipboard_feed_enabled || recorder.file_activity_feed_enabled ? "enabled" : "base"} status="working" delta={`paused=${rawText(recorder.paused)}`} />
        </div>
        {recorderError ? <div className="mt-3 rounded-lg border border-danger/40 bg-danger/10 p-3 text-sm text-danger">{recorderError}</div> : null}
        {lastRecorderControl ? (
          <div className="mt-3">
            <RawValue value={lastRecorderControl} label="Recorder control readback" />
          </div>
        ) : null}
      </Section>

      <ToolActivity toolCalls={toolCalls} />
      <TranscriptSamples state={state} />

      <Section
        title="Claims"
        tier="drill-down"
        questions={["Who holds the input lease?", "Which sessions are waiting?", "Which targets are stale?"]}
        actions={
          <Tooltip>
            <TooltipTrigger asChild>
              <Button type="button" variant="ghost" size="sm" disabled={leaseBusy === "prune"} onClick={submitPruneClaims}>
                <RefreshCw aria-hidden="true" className="h-4 w-4" />
                {leaseBusy === "prune" ? "Pruning" : "Prune"}
              </Button>
            </TooltipTrigger>
            <TooltipContent>Prune expired target claims</TooltipContent>
          </Tooltip>
        }
      >
        <div className="mb-3 grid gap-4 md:grid-cols-3">
          <StatCard label="Lease" value={lease.held === true ? "held" : "free"} status={lease.held === true ? "needs_input" : "done"} delta={ownerSessionId || "no owner"} />
          <StatCard label="Claims" value={rawText(targetClaims.claim_count || claims.length)} status={claims.length ? "working" : "idle"} delta={state?.target_claims.source || "target_claim_status"} />
          <StatCard label="Contention" value={contentionRows.length} status={contentionRows.length ? "needs_input" : "done"} delta={state?.command_audit.source || "CF_ACTION_LOG"} />
        </div>
        <div className="mb-3 grid gap-3 xl:grid-cols-2">
          <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
            <div className="mb-3 flex items-center gap-2 text-primary">
              <X aria-hidden="true" className="h-4 w-4 text-warning" />
              <h3 className="text-md font-medium tracking-normal">Force Release</h3>
            </div>
            <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_auto]">
              <input
                value={forceReleaseOwner}
                onChange={(event) => setForceReleaseOwner(event.target.value)}
                placeholder={ownerSessionId || "owner session id"}
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              />
              <label className="inline-flex h-10 items-center gap-2 rounded-md border border-border bg-surface-2 px-3 text-sm text-primary">
                <input
                  type="checkbox"
                  checked={forceReleaseConfirmed}
                  onChange={(event) => setForceReleaseConfirmed(event.target.checked)}
                  className="h-4 w-4 accent-[rgb(var(--color-info))]"
                />
                Confirm
              </label>
            </div>
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button
                type="button"
                variant="ghost"
                size="sm"
                disabled={!forceReleaseTarget || !forceReleaseConfirmed || Boolean(leaseBusy)}
                onClick={submitForceRelease}
              >
                <X aria-hidden="true" className="h-4 w-4" />
                {leaseBusy === "force" ? "Releasing" : "Release"}
              </Button>
              <span className="max-w-full overflow-hidden text-ellipsis whitespace-nowrap font-mono text-xs text-secondary">
                {forceReleaseTarget || "no target"}
              </span>
            </div>
          </div>
          <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
            <div className="mb-3 flex items-center gap-2 text-primary">
              <SkipForward aria-hidden="true" className="h-4 w-4 text-info" />
              <h3 className="text-md font-medium tracking-normal">Handoff</h3>
            </div>
            <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_7rem]">
              <input
                value={handoffToSession}
                onChange={(event) => setHandoffToSession(event.target.value)}
                placeholder="recipient session id"
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              />
              <input
                value={handoffTtlMs}
                onChange={(event) => setHandoffTtlMs(event.target.value)}
                inputMode="numeric"
                aria-label="Lease TTL ms"
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              />
            </div>
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button
                type="button"
                variant="ghost"
                size="sm"
                disabled={!ownerSessionId || !handoffToSession.trim() || Boolean(leaseBusy)}
                onClick={submitHandoff}
              >
                <SkipForward aria-hidden="true" className="h-4 w-4" />
                {leaseBusy === "handoff" ? "Handing off" : "Handoff"}
              </Button>
              <span className="max-w-full overflow-hidden text-ellipsis whitespace-nowrap font-mono text-xs text-secondary">
                {ownerSessionId || "no holder"}
              </span>
            </div>
          </div>
        </div>
        {leaseError ? <div className="mb-3 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{leaseError}</div> : null}
        <div className="mb-3 grid gap-4 xl:grid-cols-2">
          <SystemTable
            title="Contention"
            icon={Gauge}
            rows={contentionGroups}
            empty="No lease denials"
            columns={[
              { id: "session", header: "Waiting Session", cell: ({ row }) => rawText(row.original.session_id) },
              { id: "count", header: "Denials", cell: ({ row }) => rawText(row.original.count) },
              { id: "last", header: "Last", cell: ({ row }) => nsToTime(Number(row.original.last_ts_ns || 0)) || rawText(row.original.last_ts_ns) },
              { id: "tools", header: "Tools", cell: ({ row }) => asArray<string>(row.original.tools).join(", ") || "unknown" }
            ]}
          />
          <SystemTable
            title="Sessions"
            icon={MonitorUp}
            rows={sessions}
            empty="No session rows"
            columns={[
              { id: "session", header: "Session", cell: ({ row }) => rawText(row.original.session_id) },
              { id: "lifecycle", header: "Lifecycle", cell: ({ row }) => rawText(row.original.lifecycle) },
              { id: "lease", header: "Lease", cell: ({ row }) => rawText(asRecord(row.original.lease).held) },
              { id: "last", header: "Last Action", cell: ({ row }) => rawText(row.original.last_action) }
            ]}
          />
        </div>
        {claims.length ? (
          <DataTable
            data={claims}
            getRowId={(row, index) => rawText(row.target_key || row.owner_session_id || index)}
            columns={[
              { id: "target", header: "Target", cell: ({ row }) => rawText(row.original.target_key || row.original.target) },
              { id: "owner", header: "Owner", cell: ({ row }) => rawText(row.original.owner_session_id) },
              { id: "expires", header: "Expires", cell: ({ row }) => rawText(row.original.expires_in_ms) },
              { id: "generation", header: "Gen", cell: ({ row }) => rawText(row.original.generation) }
            ]}
          />
        ) : (
          <EmptyState title="No target claims" />
        )}
        <div className="mt-3">
          <RawValue value={{ lease: state?.lease, target_claims: state?.target_claims, last_control: lastLeaseControl }} label="Lease and claims readback" />
        </div>
      </Section>

      <Section
        title="Targets"
        tier="drill-down"
        questions={["Which sessions own browser tabs?", "Which hidden desktops exist?", "Which session owns each target?"]}
      >
        <div className="grid gap-4 xl:grid-cols-2">
          <SystemTable
            title="Sessions"
            icon={MonitorUp}
            rows={sessions}
            empty="No session rows"
            columns={[
              { id: "session", header: "Session", cell: ({ row }) => rawText(row.original.session_id) },
              { id: "lifecycle", header: "Lifecycle", cell: ({ row }) => rawText(row.original.lifecycle) },
              { id: "target", header: "Target", cell: ({ row }) => rawText(row.original.active_target) },
              { id: "last", header: "Last Action", cell: ({ row }) => rawText(row.original.last_action) }
            ]}
          />
          <SystemTable
            title="CDP Attachments"
            icon={Network}
            rows={cdpAttachments}
            empty="No CDP owner rows"
            columns={[
              { id: "session", header: "Session", cell: ({ row }) => rawText(row.original.session_id) },
              { id: "target", header: "Target", cell: ({ row }) => rawText(row.original.cdp_target_id) },
              { id: "window", header: "HWND", cell: ({ row }) => rawText(row.original.window_hwnd) },
              { id: "url", header: "URL", cell: ({ row }) => <span className="line-clamp-2">{rawText(row.original.target_url)}</span> }
            ]}
          />
        </div>
        <div className="mt-4">
          <SystemTable
            title="Hidden Desktops"
            icon={MonitorUp}
            rows={hiddenDesktops}
            empty="No session-owned hidden desktop rows"
            columns={[
              { id: "session", header: "Session", cell: ({ row }) => rawText(row.original.session_id) },
              { id: "desktops", header: "Desktops", cell: ({ row }) => rawText(row.original.desktop_names) },
              { id: "pids", header: "PIDs", cell: ({ row }) => rawText(row.original.launch_pids) },
              { id: "count", header: "Resources", cell: ({ row }) => rawText(row.original.resource_count) }
            ]}
          />
        </div>
      </Section>

      <Section title="Shell Jobs" tier="drill-down" questions={["Which jobs are still running?", "Which status files were read?", "Can a job be inspected?"]}>
        <div className="mb-3 grid gap-4 md:grid-cols-3">
          <StatCard label="Total" value={rawText(shellJobsData.job_count || 0)} status="idle" delta={rawText(shellJobsData.job_root)} />
          <StatCard label="Running" value={rawText(shellJobsData.running_count || 0)} status={Number(shellJobsData.running_count || 0) ? "working" : "done"} />
          <StatCard label="Unreadable" value={rawText(shellJobsData.skipped_unreadable_status_files || 0)} status={Number(shellJobsData.skipped_unreadable_status_files || 0) ? "stuck" : "done"} />
        </div>
        {shellJobs.length ? (
          <DataTable
            data={shellJobs}
            getRowId={(row, index) => rawText(row.job_id || index)}
            columns={[
              { id: "job", header: "Job", cell: ({ row }) => rawText(row.original.job_id) },
              { id: "status", header: "Status", cell: ({ row }) => rawText(asRecord(row.original.job).status || row.original.status) },
              { id: "running", header: "Running", cell: ({ row }) => rawText(row.original.running) },
              { id: "pid", header: "PID", cell: ({ row }) => rawText(row.original.pid) },
              { id: "session", header: "Session", cell: ({ row }) => rawText(row.original.session_id) }
            ]}
          />
        ) : (
          <EmptyState title="No durable shell jobs" />
        )}
        <div className="mt-3">
          <RawValue value={state?.shell_jobs} label="Shell job readback" />
        </div>
      </Section>

      <Section title="Events" tier="drill-down" questions={["How many SSE subscriptions are open?", "Which sessions own them?", "Are ingress counters moving?"]}>
        <div className="grid gap-4 md:grid-cols-3">
          <StatCard label="SSE" value={rawText(events.active_subscription_count || 0)} status={Number(events.active_subscription_count || 0) ? "working" : "idle"} delta={state?.events.source || "SseState"} />
          <StatCard label="Owners" value={asArray(events.owner_session_ids).length} status="idle" />
          <StatCard label="Ingress" value={rawText(asRecord(events.agent_event_ingress).accepted || asRecord(events.agent_event_ingress).received || 0)} status="working" />
        </div>
        <div className="mt-3">
          <RawValue value={state?.events} label="Event readback" />
        </div>
      </Section>
    </>
  );
}

function SystemTable({
  title,
  icon: Icon,
  rows,
  empty,
  columns
}: {
  title: string;
  icon: LucideIcon;
  rows: Record<string, unknown>[];
  empty: string;
  columns: ColumnDef<Record<string, unknown>>[];
}) {
  return (
    <div className="min-w-0">
      <div className="mb-2 flex items-center gap-2 text-primary">
        <Icon aria-hidden="true" className="h-4 w-4 text-info" />
        <h3 className="text-md font-medium tracking-normal">{title}</h3>
      </div>
      {rows.length ? (
        <DataTable
          data={rows}
          getRowId={(row, index) => rawText(row.owner_key || row.session_id || row.cdp_target_id || row.job_id || index)}
          columns={columns}
        />
      ) : (
        <EmptyState title={empty} />
      )}
    </div>
  );
}

function AuditView({
  state,
  toolCalls,
  filters,
  onFiltersChange
}: {
  state?: DashboardState;
  toolCalls: ReturnType<typeof buildToolCalls>;
  filters: AuditQueryFilters;
  onFiltersChange: (filters: AuditQueryFilters) => void;
}) {
  const [draft, setDraft] = useState<AuditQueryFilters>(() => sanitizeAuditFilters(filters));
  const [selectedKey, setSelectedKey] = useState("");
  const auditQuery = useQuery({
    queryKey: ["dashboard-audit-query", filters],
    queryFn: () => fetchAuditQuery(filters),
    staleTime: 2000
  });

  useEffect(() => {
    setDraft(sanitizeAuditFilters(filters));
  }, [filters]);

  const query = auditQuery.data;
  const rows = query?.rows ?? [];
  const selectedRow = rows.find((row) => row.key_hex === selectedKey) ?? rows[0];

  useEffect(() => {
    if (!selectedKey && rows[0]) {
      setSelectedKey(rows[0].key_hex);
    }
    if (selectedKey && rows.length && !rows.some((row) => row.key_hex === selectedKey)) {
      setSelectedKey(rows[0].key_hex);
    }
  }, [rows, selectedKey]);

  const updateDraft = (field: keyof AuditQueryFilters, value: string) => {
    setDraft((current) => ({ ...current, [field]: value }));
  };

  const applyFilters = (event: FormEvent) => {
    event.preventDefault();
    onFiltersChange(sanitizeAuditFilters(draft));
  };

  const continueFromPartial = () => {
    const next = query?.next_start_key_hex;
    if (!next) return;
    onFiltersChange({ ...filters, cursor: next, start_ts_ns: "" });
  };

  const focusToolActivityRow = (keyHex: string) => {
    setSelectedKey(keyHex);
    onFiltersChange(auditCursorFilters(keyHex));
  };

  return (
    <>
      <ToolActivity toolCalls={toolCalls} onAuditKeySelect={focusToolActivityRow} />
      <Section
        title="Audit Filters"
        tier="triage"
        questions={["Which bounded CF_ACTION_LOG window is scanned?", "Which filters are active?", "Is continuation explicit?"]}
        actions={
          <Button variant="secondary" size="sm" onClick={() => auditQuery.refetch()} disabled={auditQuery.isFetching}>
            <RefreshCw aria-hidden="true" className="h-4 w-4" />
            Refresh
          </Button>
        }
      >
        <form onSubmit={applyFilters} className="space-y-4">
          <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
            <TextField label="Session" value={rawText(draft.session_id)} onChange={(value) => updateDraft("session_id", value)} mono placeholder="session id" />
            <TextField label="Tool" value={rawText(draft.tool)} onChange={(value) => updateDraft("tool", value)} mono placeholder="act_run_shell" />
            <TextField label="Status" value={rawText(draft.status)} onChange={(value) => updateDraft("status", value)} mono placeholder="ok / error / denied" />
            <TextField label="Error Code" value={rawText(draft.error_code)} onChange={(value) => updateDraft("error_code", value)} mono placeholder="TOOL_PARAMS_INVALID" />
            <TextField label="Start ns" value={rawText(draft.start_ts_ns)} onChange={(value) => updateDraft("start_ts_ns", value)} mono placeholder="inclusive timestamp" />
            <TextField label="End ns" value={rawText(draft.end_ts_ns)} onChange={(value) => updateDraft("end_ts_ns", value)} mono placeholder="inclusive timestamp" />
            <TextField label="Cursor" value={rawText(draft.cursor)} onChange={(value) => updateDraft("cursor", value)} mono placeholder="physical key hex" />
            <label className="mt-3 block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Row Kind</span>
              <select
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                value={rawText(draft.row_kind || "all")}
                onChange={(event) => updateDraft("row_kind", event.target.value)}
              >
                <option value="all">All rows</option>
                <option value="command_audit">Command audit</option>
                <option value="action_audit">Action audit</option>
              </select>
            </label>
            <TextField label="Returned Limit" value={rawText(draft.limit)} onChange={(value) => updateDraft("limit", value)} type="number" />
            <TextField label="Scan Budget" value={rawText(draft.scan_limit)} onChange={(value) => updateDraft("scan_limit", value)} type="number" />
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" variant="primary" size="sm">
              <Search aria-hidden="true" className="h-4 w-4" />
              Apply
            </Button>
            <Button type="button" variant="secondary" size="sm" onClick={() => onFiltersChange(defaultAuditFilters())}>
              <RefreshCw aria-hidden="true" className="h-4 w-4" />
              Reset
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => onFiltersChange({ limit: "100", scan_limit: "1000", row_kind: "all" })}
            >
              <FileSearch aria-hidden="true" className="h-4 w-4" />
              All Time
            </Button>
          </div>
        </form>
      </Section>

      <Section
        title="Audit Results"
        tier="drill-down"
        questions={["Which rows matched?", "Is truncation visible?", "Can I continue from the exact physical key?"]}
      >
        <div className="grid gap-4 md:grid-cols-4">
          <StatCard label="Scanned" value={rawText(query?.scanned_rows || 0)} status={query?.partial ? "needs_input" : "done"} delta={`budget ${rawText(query?.scan_limit || rawText(filters.scan_limit || ""))}`} />
          <StatCard label="Matched" value={rawText(query?.matched_rows || 0)} status={rows.length ? "working" : "idle"} delta={query?.cf_name || "CF_ACTION_LOG"} />
          <StatCard label="Returned" value={rawText(query?.returned_count || rows.length)} status={query?.partial ? "needs_input" : "done"} delta={`limit ${rawText(query?.limit || rawText(filters.limit || ""))}`} />
          <StatCard label="Corrupt" value={rawText(query?.corrupt_row_count || 0)} status={Number(query?.corrupt_row_count || 0) ? "stuck" : "done"} delta={query?.source_of_truth || "bounded scan"} />
        </div>
        {auditQuery.isError ? (
          <div className="mt-4 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{rawText(auditQuery.error)}</div>
        ) : null}
        {query?.partial ? (
          <div className="mt-4 flex flex-wrap items-center justify-between gap-3 rounded-md border border-warning-border bg-warning-bg p-3 text-sm text-warning-fg">
            <span>
              Partial results: scanned {query.scanned_rows} rows, returned {query.returned_count}. Continue from {shortKey(query.next_start_key_hex)}.
            </span>
            <Button type="button" variant="secondary" size="sm" onClick={continueFromPartial} disabled={!query.next_start_key_hex}>
              <SkipForward aria-hidden="true" className="h-4 w-4" />
              Continue
            </Button>
          </div>
        ) : null}
        <div className="mt-4">
          <AuditResultsTable rows={rows} selectedKey={selectedRow?.key_hex} onSelect={setSelectedKey} loading={auditQuery.isFetching} />
        </div>
      </Section>

      <Section
        title="Row Detail"
        tier="drill-down"
        questions={["Which exact physical row is selected?", "What complete structured record is stored?", "Which Source of Truth backs it?"]}
      >
        {selectedRow ? (
          <div className="grid gap-4 xl:grid-cols-[minmax(0,0.36fr)_minmax(0,1fr)]">
            <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
              <MetricRow label="Key" value={<span className="font-mono">{shortKey(selectedRow.key_hex)}</span>} />
              <MetricRow label="Kind" value={selectedRow.row_kind} />
              <MetricRow label="Tool" value={selectedRow.tool} />
              <MetricRow label="Status" value={auditRowStatus(selectedRow)} />
              <MetricRow label="Session" value={selectedRow.actor_session_id || selectedRow.session_id || "none"} />
              <MetricRow label="Bytes" value={rawText(selectedRow.value_len_bytes)} />
              <RawValue value={selectedRow.source_of_truth} label="Physical storage key" />
            </div>
            <RawValue value={selectedRow.record} label="Full structured record" />
          </div>
        ) : (
          <EmptyState title={auditQuery.isFetching ? "Loading audit rows" : "No matching audit rows"} />
        )}
        <div className="mt-4">
          <RawValue value={{ query, snapshot: state?.command_audit }} label="Audit readback" />
        </div>
      </Section>
    </>
  );
}

function AuditResultsTable({
  rows,
  selectedKey,
  onSelect,
  loading
}: {
  rows: AuditQueryRow[];
  selectedKey?: string;
  onSelect: (keyHex: string) => void;
  loading: boolean;
}) {
  const [sorting, setSorting] = useState<SortingState>([{ id: "time", desc: true }]);
  const [scrollTop, setScrollTop] = useState(0);
  const columns = useMemo<ColumnDef<AuditQueryRow>[]>(
    () => [
      {
        id: "time",
        header: "Time",
        accessorFn: (row) => rawText(row.ts_ns_text || row.ts_ns),
        cell: ({ row }) => <span className="font-mono">{nsToTime(row.original.ts_ns_text || row.original.ts_ns)}</span>
      },
      {
        id: "kind",
        header: "Kind",
        accessorFn: (row) => row.row_kind,
        cell: ({ row }) => row.original.row_kind
      },
      {
        id: "tool",
        header: "Tool",
        accessorFn: (row) => row.tool,
        cell: ({ row }) => <span className="font-mono">{row.original.tool}</span>
      },
      {
        id: "status",
        header: "Status",
        accessorFn: auditRowStatus,
        cell: ({ row }) => auditRowStatus(row.original)
      },
      {
        id: "session",
        header: "Session",
        accessorFn: (row) => row.actor_session_id || row.session_id || "",
        cell: ({ row }) => <span className="font-mono">{shortKey(row.original.actor_session_id || row.original.session_id)}</span>
      },
      {
        id: "error",
        header: "Error",
        accessorFn: (row) => row.error_code || "",
        cell: ({ row }) => <span className="font-mono text-danger-fg">{row.original.error_code || ""}</span>
      },
      {
        id: "key",
        header: "Key",
        accessorFn: (row) => row.key_hex,
        cell: ({ row }) => <span className="font-mono">{shortKey(row.original.key_hex)}</span>
      },
      {
        id: "detail",
        header: "",
        enableSorting: false,
        cell: ({ row }) => (
          <Button type="button" variant="ghost" size="sm" onClick={() => onSelect(row.original.key_hex)} aria-label={`Select audit row ${row.original.key_hex}`}>
            <FileSearch aria-hidden="true" className="h-4 w-4" />
            Detail
          </Button>
        )
      }
    ],
    [onSelect]
  );
  const table = useReactTable({
    data: rows,
    columns,
    state: { sorting },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getRowId: (row) => row.key_hex
  });
  const tableRows = table.getRowModel().rows;
  const rowHeight = 48;
  const viewportHeight = 460;
  const overscan = 6;
  const startIndex = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const endIndex = Math.min(tableRows.length, Math.ceil((scrollTop + viewportHeight) / rowHeight) + overscan);
  const visibleRows = tableRows.slice(startIndex, endIndex);
  const topPad = startIndex * rowHeight;
  const bottomPad = Math.max(0, (tableRows.length - endIndex) * rowHeight);
  const columnCount = columns.length;

  if (!rows.length) {
    return <EmptyState title={loading ? "Loading audit rows" : "No audit rows match this bounded scan"} />;
  }

  return (
    <div className="rounded-lg border border-border">
      <div className="border-b border-border bg-surface-2 px-3 py-2 text-xs text-muted">
        Showing {visibleRows.length} of {tableRows.length} sorted rows in the visible window.
      </div>
      <div className="max-h-[460px] overflow-auto" onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}>
        <table className="w-full min-w-[960px] border-collapse text-sm">
          <thead className="sticky top-0 z-10 bg-surface-2">
            {table.getHeaderGroups().map((headerGroup) => (
              <tr key={headerGroup.id}>
                {headerGroup.headers.map((header) => {
                  const sorted = header.column.getIsSorted();
                  return (
                    <th key={header.id} className="border-b border-border px-3 py-2 text-left text-label font-medium uppercase text-muted">
                      {header.isPlaceholder ? null : header.column.getCanSort() ? (
                        <button type="button" className="inline-flex items-center gap-1" onClick={header.column.getToggleSortingHandler()}>
                          {flexRender(header.column.columnDef.header, header.getContext())}
                          {sorted ? <ChevronDown aria-hidden="true" className={cn("h-3 w-3", sorted === "asc" && "rotate-180")} /> : <ArrowUpDown aria-hidden="true" className="h-3 w-3" />}
                        </button>
                      ) : (
                        flexRender(header.column.columnDef.header, header.getContext())
                      )}
                    </th>
                  );
                })}
              </tr>
            ))}
          </thead>
          <tbody>
            {topPad ? (
              <tr aria-hidden="true">
                <td colSpan={columnCount} style={{ height: topPad }} />
              </tr>
            ) : null}
            {visibleRows.map((row) => (
              <tr
                key={row.id}
                aria-selected={row.original.key_hex === selectedKey}
                className={cn(
                  "h-12 border-b border-border-subtle last:border-b-0 hover:bg-surface-2",
                  row.original.key_hex === selectedKey && "bg-surface-2"
                )}
              >
                {row.getVisibleCells().map((cell) => (
                  <td key={cell.id} className="max-w-72 px-3 py-2 align-middle text-secondary">
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </td>
                ))}
              </tr>
            ))}
            {bottomPad ? (
              <tr aria-hidden="true">
                <td colSpan={columnCount} style={{ height: bottomPad }} />
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function auditRowStatus(row: AuditQueryRow) {
  return rawText(row.status || row.outcome || row.phase || "");
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
    <Section title="Overview" tier="overview" questions={["Is anything wrong?", "How many agents are live?", "Is the daemon stale?"]}>
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
    base_url: "",
    model_id: "deepseek-v4-flash",
    runtime_preset: "deepseek_v4_flash_non_thinking",
    api_key_env_var: "DEEPSEEK_API_KEY",
    api_key: "",
    context_length: "1000000",
    max_tools: "128",
    notes: "DeepSeek V4 Flash non-thinking API agent"
  },
  reasoning: {
    label: "DeepSeek Reasoning",
    name: "deepseek-reasoning",
    base_url: "",
    model_id: "deepseek-v4-flash",
    runtime_preset: "deepseek_v4_reasoning",
    api_key_env_var: "DEEPSEEK_API_KEY",
    api_key: "",
    context_length: "1000000",
    max_tools: "128",
    notes: "DeepSeek V4 Flash reasoning API agent"
  }
};

type SpawnMode = "local_model" | "codex" | "claude";
type SpawnTargetMode = "none" | "window" | "cdp";

function SpawnConsole({ onSpawned }: { onSpawned: () => void }) {
  const modelsQuery = useQuery({
    queryKey: ["dashboard-models"],
    queryFn: fetchModels
  });
  const models = modelsQuery.data ?? [];
  const [spawnMode, setSpawnMode] = useState<SpawnMode>("claude");
  const [selectedModel, setSelectedModel] = useState("");
  const [fanOut, setFanOut] = useState("1");
  const [waitTimeoutMs, setWaitTimeoutMs] = useState("300000");
  const [holdOpenMs, setHoldOpenMs] = useState("300000");
  const [prompt, setPrompt] = useState('Use workspace_put with key issue985-deepseek-smoke and value {"ok":true}.');
  const [workingDir, setWorkingDir] = useState("C:\\code\\Synapse");
  const [directModel, setDirectModel] = useState("");
  const [targetMode, setTargetMode] = useState<SpawnTargetMode>("none");
  const [targetWindowHwnd, setTargetWindowHwnd] = useState("");
  const [targetCdpId, setTargetCdpId] = useState("");
  const [registerForm, setRegisterForm] = useState(deepSeekPresets.flash);
  const [pendingAction, setPendingAction] = useState<"register" | "spawn" | "">("");
  const [error, setError] = useState("");
  const [lastRegister, setLastRegister] = useState<DashboardRouteReadback | null>(null);
  const [lastSpawn, setLastSpawn] = useState<SpawnAgentResponse | null>(null);

  useEffect(() => {
    if (!selectedModel && models.length > 0) {
      const firstLaunchable = models.find((model) => model.enabled && model.last_probe?.healthy) ?? models[0];
      setSelectedModel(firstLaunchable.name);
    }
  }, [models, selectedModel]);

  const selected = models.find((model) => model.name === selectedModel);

  const fanOutNumber = Number(fanOut.trim() || "1");
  const fanOutValid = Number.isInteger(fanOutNumber) && fanOutNumber >= 1 && fanOutNumber <= 5;
  const directReady =
    spawnMode === "local_model" ? Boolean(selectedModel && prompt.trim()) : Boolean(prompt.trim());
  const canSpawn = Boolean(!pendingAction && fanOutValid && directReady);

  const buildTarget = () => {
    if (targetMode === "none") return undefined;
    const parsedWindow = Number(targetWindowHwnd.trim());
    if (!Number.isInteger(parsedWindow) || parsedWindow <= 0) {
      throw new Error("target window HWND must be a positive integer");
    }
    if (targetMode === "window") {
      return { kind: "window" as const, window_hwnd: parsedWindow };
    }
    const cdpTargetId = targetCdpId.trim();
    if (!cdpTargetId) {
      throw new Error("target CDP id is required");
    }
    return { kind: "cdp" as const, window_hwnd: parsedWindow, cdp_target_id: cdpTargetId };
  };

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
        api_key: registerForm.api_key.trim() ? registerForm.api_key : undefined,
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
      const fanOutValue = parsePositiveInteger(fanOut, "fan_out") ?? 1;
      if (fanOutValue > 5) {
        throw new Error("fan_out must be 1..=5");
      }
      const request = {
        fan_out: fanOutValue,
        wait_timeout_ms: parsePositiveInteger(waitTimeoutMs, "wait_timeout_ms") ?? 300000,
        hold_open_ms: parseNonNegativeInteger(holdOpenMs, "hold_open_ms") ?? 0
      };
      Object.assign(request, {
        kind: spawnMode,
        model: spawnMode === "local_model" ? undefined : directModel.trim() || undefined,
        model_ref: spawnMode === "local_model" ? selectedModel : undefined,
        prompt,
        target: buildTarget(),
        working_dir: workingDir.trim() || undefined
      });
      const readback = await spawnAgent(request);
      setLastSpawn(readback);
      await modelsQuery.refetch();
      onSpawned();
      if (readback.failed_count > 0) {
        setError(`${readback.failed_count} spawn attempt failed; inspect readback rows.`);
      }
    } catch (spawnError) {
      setError(rawText(spawnError) || "Spawn failed");
    } finally {
      setPendingAction("");
    }
  };

  return (
    <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_minmax(26rem,1fr)]">
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
                },
                {
                  id: "key",
                  header: "API key",
                  cell: ({ row }) =>
                    row.original.has_api_key_secret ? (
                      <span className="font-mono text-success" title="Encrypted API key stored at rest (DPAPI)">🔑 stored</span>
                    ) : row.original.api_key_env_var ? (
                      <span className="font-mono text-warning" title="No stored key; resolves from daemon environment">env only</span>
                    ) : (
                      <span className="font-mono text-muted">none</span>
                    )
                }
              ]}
            />
          ) : (
            <EmptyStateArt title={modelsQuery.isError ? rawText(modelsQuery.error) || "Model registry unavailable" : "No model rows"} />
          )}
        </div>

        <form className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]" onSubmit={submitSpawn}>
          <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-2">
              <Rocket aria-hidden="true" className="h-4 w-4 text-info" />
              <h3 className="text-md font-semibold tracking-normal text-primary">Spawn</h3>
            </div>
            <div className="inline-flex rounded-md border border-border bg-surface-2 p-1">
              {(["local_model", "codex", "claude"] as SpawnMode[]).map((mode) => (
                <button
                  key={mode}
                  type="button"
                  className={cn("h-8 rounded px-3 text-sm text-secondary", spawnMode === mode && "bg-accent text-accent-fg")}
                  onClick={() => setSpawnMode(mode)}
                >
                  {mode === "local_model" ? "Local" : mode[0].toUpperCase() + mode.slice(1)}
                </button>
              ))}
            </div>
          </div>
          <div className="grid gap-3 md:grid-cols-2">
            <label className="block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Fan-out</span>
              <input
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                min={1}
                max={5}
                type="number"
                value={fanOut}
                onChange={(event) => setFanOut(event.target.value)}
              />
              <span className="mt-1 block text-xs leading-snug text-muted">
                How many identical agents to launch at once from this single spawn (1–5). Each runs
                independently with the same model, directory, and prompt — use it to parallelize one
                task across several agents.
              </span>
            </label>
            <TextField label="Wait timeout ms" value={waitTimeoutMs} onChange={setWaitTimeoutMs} mono type="number" />
            <TextField label="Hold open ms" value={holdOpenMs} onChange={setHoldOpenMs} mono type="number" />
            <label className="block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Working dir</span>
              <input
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                value={workingDir}
                onChange={(event) => setWorkingDir(event.target.value)}
              />
            </label>
          </div>
          <div className="mt-3 grid gap-3 md:grid-cols-2">
            {spawnMode === "local_model" ? (
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
            ) : (
              <TextField label="Model" value={directModel} onChange={setDirectModel} mono />
            )}
            <label className="block text-sm text-secondary">
              <span className="mb-1 block text-label font-medium uppercase text-muted">Target</span>
              <select
                className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
                value={targetMode}
                onChange={(event) => setTargetMode(event.target.value as SpawnTargetMode)}
              >
                <option value="none">None</option>
                <option value="window">Window</option>
                <option value="cdp">CDP tab</option>
              </select>
            </label>
            {targetMode !== "none" ? <TextField label="Window HWND" value={targetWindowHwnd} onChange={setTargetWindowHwnd} mono /> : null}
            {targetMode === "cdp" ? <TextField label="CDP target" value={targetCdpId} onChange={setTargetCdpId} mono /> : null}
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
              {spawnMode === "local_model"
                ? selected
                  ? `${selected.model_id} / ${selected.last_probe?.status || "unprobed"}`
                  : "No model selected"
                : `${spawnMode} agent`}
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
                  if (preset) setRegisterForm({ ...preset });
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
            <TextField
              label="API key (stored encrypted)"
              value={registerForm.api_key}
              onChange={(value) => setRegisterForm((form) => ({ ...form, api_key: value }))}
              type="password"
              autoComplete="off"
              placeholder="sk-… (encrypted at rest via Windows DPAPI)"
              mono
            />
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
        <SpawnReadbackStrip readback={lastSpawn} />
      </div>
    </div>
  );
}

function SpawnReadbackStrip({ readback }: { readback: SpawnAgentResponse | null }) {
  if (!readback) return null;
  return (
    <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h3 className="text-md font-semibold tracking-normal text-primary">Spawn Readbacks</h3>
          <p className="mt-1 text-sm text-secondary">
            {readback.succeeded_count} ok / {readback.failed_count} error / {readback.requested_count} requested
          </p>
        </div>
        <span className="font-mono text-xs text-muted">{readback.source_of_truth}</span>
      </div>
      <div className="space-y-3">
        {readback.attempts.map((attempt) => {
          const spawn = asRecord(attempt.spawn);
          const logPaths = asRecord(spawn.log_paths);
          const rows = ([
            ["Spawn", spawn.spawn_id],
            ["Session", spawn.session_id],
            ["Launcher PID", spawn.launcher_process_id],
            ["Agent PID", spawn.agent_process_id],
            ["Kind", spawn.kind],
            ["Template", spawn.template_id],
            ["Template version", spawn.template_version],
            ["Launch target", spawn.launch_target],
            ["Log dir", logPaths.log_dir],
            ["Prompt", logPaths.prompt_path],
            ["Task started", logPaths.task_started_path],
            ["Completion", logPaths.completion_status_path]
          ] as Array<[string, unknown]>).filter((row) => rawText(row[1]));
          return (
            <article key={attempt.index} className="rounded-md border border-border bg-surface-2 p-3">
              <div className="mb-2 flex flex-wrap items-center justify-between gap-3">
                <div className="flex items-center gap-2">
                  <StatusBadge status={attempt.status === "ok" ? "done" : "stuck"} />
                  <span className="font-mono text-sm text-primary">attempt {attempt.index}</span>
                </div>
                {attempt.error_code ? <span className="font-mono text-xs text-danger-fg">{attempt.error_code}</span> : null}
              </div>
              {attempt.status === "ok" ? (
                <div className="grid gap-2 text-sm md:grid-cols-2">
                  {rows.map(([label, value]) => (
                    <div key={String(label)} className="min-w-0">
                      <div className="text-label font-medium uppercase text-muted">{label}</div>
                      <div className="truncate font-mono text-primary">{rawText(value)}</div>
                    </div>
                  ))}
                </div>
              ) : (
                <div className="space-y-2 text-sm text-danger-fg">
                  <div>{attempt.message || "spawn failed"}</div>
                  {attempt.data ? <RawValue value={attempt.data} label="Error data" /> : null}
                </div>
              )}
              <RawValue value={attempt} label="Attempt raw" />
            </article>
          );
        })}
      </div>
      <RawValue value={readback} label="Response raw" />
    </div>
  );
}

function slugifyTemplateId(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[-.]+|[-.]+$/g, "")
    .slice(0, 200);
}

function TemplatesPanel({ onSpawned }: { onSpawned: () => void }) {
  const templatesQuery = useQuery({ queryKey: ["dashboard-templates"], queryFn: fetchTemplates });
  const modelsQuery = useQuery({ queryKey: ["dashboard-models"], queryFn: fetchModels });
  const templates = templatesQuery.data ?? [];
  const models = modelsQuery.data ?? [];
  const modelOptions = useMemo(() => ["claude", "codex", ...models.map((model) => model.name)], [models]);

  const [editingId, setEditingId] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [model, setModel] = useState("claude");
  const [directory, setDirectory] = useState("C:\\code\\Synapse");
  const [prompt, setPrompt] = useState("");
  const [fanOut, setFanOut] = useState("1");
  const [pending, setPending] = useState<"" | "save" | "run" | "delete">("");
  const [busyId, setBusyId] = useState("");
  const [error, setError] = useState("");

  const resetForm = () => {
    setEditingId(null);
    setName("");
    setDescription("");
    setModel("claude");
    setDirectory("C:\\code\\Synapse");
    setPrompt("");
    setError("");
  };

  const startEdit = (row: AgentTemplateRow) => {
    setEditingId(row.template_id);
    setName(row.name);
    setDescription(row.description || "");
    setModel(row.model);
    setDirectory(row.directory || "");
    setPrompt(row.prompt);
    setError("");
  };

  const canSave = Boolean(name.trim() && model.trim() && prompt.trim() && pending === "");

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!canSave) return;
    setError("");
    setPending("save");
    try {
      const templateId = editingId ?? slugifyTemplateId(name);
      if (!templateId) throw new Error("Name must contain letters or digits");
      await putTemplate({
        template_id: templateId,
        name: name.trim(),
        description: description.trim(),
        model: model.trim(),
        directory: directory.trim() || undefined,
        prompt
      });
      await templatesQuery.refetch();
      resetForm();
    } catch (saveError) {
      setError(rawText(saveError) || "Template save failed");
    } finally {
      setPending("");
    }
  };

  const removeTemplate = async (templateId: string) => {
    setError("");
    setPending("delete");
    setBusyId(templateId);
    try {
      await deleteTemplate(templateId);
      if (editingId === templateId) resetForm();
      await templatesQuery.refetch();
    } catch (deleteError) {
      setError(rawText(deleteError) || "Template delete failed");
    } finally {
      setPending("");
      setBusyId("");
    }
  };

  const runTemplate = async (templateId: string) => {
    setError("");
    setPending("run");
    setBusyId(templateId);
    try {
      const fanOutValue = parsePositiveInteger(fanOut, "fan_out") ?? 1;
      if (fanOutValue > 5) throw new Error("Fan-out must be 1..=5");
      const readback = await spawnAgent({ template_id: templateId, fan_out: fanOutValue });
      onSpawned();
      if (readback.failed_count > 0) {
        setError(`${readback.failed_count} spawn attempt failed; inspect the fleet.`);
      }
    } catch (runError) {
      setError(rawText(runError) || "Template run failed");
    } finally {
      setPending("");
      setBusyId("");
    }
  };

  return (
    <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_minmax(24rem,0.8fr)]">
      <div className="min-w-0 space-y-3">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Fan-out for runs</span>
            <input
              className="h-9 w-24 rounded-md border border-border bg-surface-2 px-3 font-mono text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              min={1}
              max={5}
              type="number"
              value={fanOut}
              onChange={(event) => setFanOut(event.target.value)}
            />
            <span className="mt-1 block max-w-md text-xs leading-snug text-muted">
              How many identical agents Run launches at once (1–5). Each runs independently with this
              template's model, directory, and prompt.
            </span>
          </label>
          <Button size="sm" variant="ghost" onClick={() => templatesQuery.refetch()} disabled={templatesQuery.isFetching}>
            <RefreshCw aria-hidden="true" className="h-4 w-4" />
            Refresh
          </Button>
        </div>
        {templates.length ? (
          <div className="space-y-2">
            {templates.map((template) => (
              <article key={template.template_id} className="rounded-lg border border-border bg-surface-1 p-3">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="text-md font-semibold text-primary">{template.name}</div>
                    {template.description ? (
                      <div className="mt-0.5 text-sm text-secondary">{template.description}</div>
                    ) : null}
                    <div className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 font-mono text-xs text-muted">
                      <span>model: {template.model}</span>
                      <span>dir: {template.directory || "—"}</span>
                    </div>
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    <Button
                      size="sm"
                      variant="primary"
                      onClick={() => runTemplate(template.template_id)}
                      disabled={pending !== ""}
                    >
                      <Rocket aria-hidden="true" className="h-4 w-4" />
                      {pending === "run" && busyId === template.template_id ? "Running" : "Run"}
                    </Button>
                    <Button size="sm" variant="secondary" onClick={() => startEdit(template)} disabled={pending !== ""}>
                      Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => removeTemplate(template.template_id)}
                      disabled={pending !== ""}
                    >
                      {pending === "delete" && busyId === template.template_id ? "Deleting" : "Delete"}
                    </Button>
                  </div>
                </div>
                <p className="mt-2 line-clamp-3 whitespace-pre-wrap text-sm text-secondary">{template.prompt}</p>
              </article>
            ))}
          </div>
        ) : (
          <EmptyStateArt
            title={templatesQuery.isError ? rawText(templatesQuery.error) || "Templates unavailable" : "No templates yet — create one on the right"}
          />
        )}
      </div>

      <form className="min-w-0 rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]" onSubmit={submit}>
        <div className="mb-3 flex items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <Plus aria-hidden="true" className="h-4 w-4 text-info" />
            <h3 className="text-md font-semibold tracking-normal text-primary">
              {editingId ? `Edit "${editingId}"` : "New template"}
            </h3>
          </div>
          {editingId ? (
            <Button size="sm" variant="ghost" type="button" onClick={resetForm} disabled={pending !== ""}>
              Cancel edit
            </Button>
          ) : null}
        </div>
        <div className="space-y-3">
          <TextField label="Name" value={name} onChange={setName} placeholder="Code reviewer" />
          <TextField
            label="Description (what it's for)"
            value={description}
            onChange={setDescription}
            placeholder="Reviews the repo for correctness bugs"
          />
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Model</span>
            <select
              className="h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              value={model}
              onChange={(event) => setModel(event.target.value)}
            >
              {modelOptions.map((option) => (
                <option key={option} value={option}>
                  {option}
                </option>
              ))}
              {model && !modelOptions.includes(model) ? <option value={model}>{model} (saved)</option> : null}
            </select>
          </label>
          <TextField label="Directory" value={directory} onChange={setDirectory} mono placeholder="C:\\code\\Synapse" />
          <label className="block text-sm text-secondary">
            <span className="mb-1 block text-label font-medium uppercase text-muted">Prompt</span>
            <textarea
              className="min-h-32 w-full rounded-md border border-border bg-surface-2 px-3 py-2 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring"
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              placeholder="What should this agent do?"
            />
          </label>
        </div>
        {error ? <div className="mt-3 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger-fg">{error}</div> : null}
        <div className="mt-3 flex justify-end">
          <Button type="submit" variant="primary" disabled={!canSave}>
            <Plus aria-hidden="true" className="h-4 w-4" />
            {pending === "save" ? "Saving" : editingId ? "Save changes" : "Create template"}
          </Button>
        </div>
      </form>
    </div>
  );
}

function TextField({
  label,
  value,
  onChange,
  mono = false,
  type = "text",
  placeholder,
  autoComplete
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  mono?: boolean;
  type?: "text" | "password" | "number";
  placeholder?: string;
  autoComplete?: string;
}) {
  return (
    <label className="mt-3 block text-sm text-secondary">
      <span className="mb-1 block text-label font-medium uppercase text-muted">{label}</span>
      <input
        className={`h-10 w-full rounded-md border border-border bg-surface-2 px-3 text-sm text-primary outline-none focus:ring-2 focus:ring-focus-ring ${mono ? "font-mono" : ""}`}
        type={type}
        placeholder={placeholder}
        autoComplete={autoComplete}
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

function parseNonNegativeInteger(value: string, field: string): number | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  const parsed = Number(trimmed);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${field} must be a non-negative integer`);
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
  onSelect,
  onKill,
  killingId
}: {
  agents: AgentSummary[];
  selectedId?: string;
  onSelect: (id: string) => void;
  onKill: (agent: AgentSummary) => void;
  killingId: string;
}) {
  if (agents.length === 0) return <EmptyStateArt title="No agent rows" />;
  return (
    <div className="rounded-lg border border-border bg-surface-1">
      {agents.map((agent) => {
        const pending = killingId === agent.killId;
        return (
          <div key={agent.id} className="grid grid-cols-[minmax(0,1fr)_auto] items-stretch border-b border-border-subtle last:border-b-0">
            <FleetRow agent={agent} selected={agent.id === selectedId} onSelect={() => onSelect(agent.id)} />
            <div className="flex items-center px-3">
              <Button
                type="button"
                variant="ghost"
                size="sm"
                disabled={!agent.killable || pending}
                onClick={() => onKill(agent)}
                aria-label={agent.killable ? `Kill ${agent.killId || agent.id} from fleet list` : `Historical row ${agent.id}`}
              >
                {agent.killable ? <X aria-hidden="true" className="h-4 w-4" /> : <FileSearch aria-hidden="true" className="h-4 w-4" />}
                {agent.killable ? (pending ? "Killing" : "Kill") : "History"}
              </Button>
            </div>
          </div>
        );
      })}
    </div>
  );
}

function FleetTable({
  agents,
  onSelect,
  onKill,
  killingId
}: {
  agents: AgentSummary[];
  onSelect: (id: string) => void;
  onKill: (agent: AgentSummary) => void;
  killingId: string;
}) {
  if (agents.length === 0) return <EmptyStateArt title="No fleet rows" />;
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
        },
        {
          id: "actions",
          header: "Actions",
          cell: ({ row }) => {
            const agent = row.original;
            const pending = killingId === agent.killId;
            return (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    disabled={!agent.killable || pending}
                    onClick={() => onKill(agent)}
                    aria-label={agent.killable ? `Kill ${agent.killId || agent.id}` : `Historical row ${agent.id}`}
                  >
                    {agent.killable ? <X aria-hidden="true" className="h-4 w-4" /> : <FileSearch aria-hidden="true" className="h-4 w-4" />}
                    {agent.killable ? (pending ? "Killing" : "Kill") : "History"}
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{agent.killable ? `Kill ${agent.killId}` : "Historical row"}</TooltipContent>
              </Tooltip>
            );
          }
        }
      ]}
    />
  );
}

function ToolActivity({
  toolCalls,
  onAuditKeySelect
}: {
  toolCalls: ReturnType<typeof buildToolCalls>;
  onAuditKeySelect?: (keyHex: string) => void;
}) {
  return (
    <Section title="Tool Activity" tier="triage" questions={["Which tools are still running?", "Which calls failed?", "Where is the verification detail?"]}>
      {toolCalls.length ? (
        <div className="grid gap-3 lg:grid-cols-2">
          {toolCalls.slice(0, 6).map((call) => {
            const keyHex = rawText(asRecord(call.raw).key_hex);
            return (
              <div key={call.id} className="space-y-2">
                <ToolCallCard call={call} />
                {onAuditKeySelect && keyHex ? (
                  <Button type="button" variant="ghost" size="sm" onClick={() => onAuditKeySelect(keyHex)}>
                    <FileSearch aria-hidden="true" className="h-4 w-4" />
                    Audit
                  </Button>
                ) : null}
              </div>
            );
          })}
        </div>
      ) : (
        <EmptyStateArt title="No command audit rows" />
      )}
    </Section>
  );
}

// Compact disk-usage summary for the Fleet aside: total on-disk bytes, a
// category composition bar, and the top stores by BYTES (not row count). Deep
// breakdown + management live on the System page (`StorageUsage`).
function SystemShape({ state }: { state?: DashboardState }) {
  const model = useMemo(() => buildStorageModel(state), [state]);
  const storage = asRecord(panelData(state?.storage));
  if (!model.rows.length || model.totalBytes <= 0) return <EmptyStateArt title="No storage rows" />;
  const top = model.rows.slice(0, 5);
  return (
    <div className="space-y-4">
      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        <div className="flex items-baseline justify-between">
          <span className="text-label font-medium uppercase text-muted">On disk</span>
          <span className="font-mono text-lg text-primary">{formatBytes(model.totalBytes)}</span>
        </div>
        <div className="mt-2 flex h-3 w-full overflow-hidden rounded-md border border-border-subtle bg-surface-2" role="img" aria-label="Storage composition by category">
          {model.categoryTotals.map((entry) => (
            <div
              key={entry.category}
              style={{ width: `${Math.max(entry.pct, 0.5)}%`, background: CATEGORY_META[entry.category].color }}
              title={`${CATEGORY_META[entry.category].label}: ${formatBytes(entry.bytes)} (${entry.pct.toFixed(1)}%)`}
            />
          ))}
        </div>
      </div>
      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        {top.map((row) => (
          <div key={row.cf} className="flex items-center justify-between gap-2 border-b border-border-subtle py-1.5 last:border-b-0">
            <span className="flex min-w-0 items-center gap-2">
              <span className="h-2.5 w-2.5 shrink-0 rounded-sm" style={{ background: CATEGORY_META[row.category].color }} aria-hidden="true" />
              <span className="truncate text-sm text-secondary">{row.label}</span>
            </span>
            <span className="shrink-0 font-mono text-sm text-primary">{formatBytes(row.bytes)}</span>
          </div>
        ))}
      </div>
      <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
        <MetricRow label="Schema" value={rawText(storage.schema_version)} />
        <MetricRow label="Generated" value={unixMsToTime(state?.generated_at_unix_ms)} />
      </div>
    </div>
  );
}

function TranscriptSamples({ state }: { state?: DashboardState }) {
  const allRows = asArray<Record<string, unknown>>(asRecord(panelData(state?.agent_transcripts)).rows);
  // The store keeps one row per source line (assistant text, tool calls, tool
  // results, AND session-metadata records like `mode`/`file-history-snapshot`).
  // Only rows with something a human can read are worth a card; metadata rows
  // would otherwise crowd the panel with empty placeholders. The count of what
  // we hide is surfaced, never silently dropped.
  const renderable = allRows.filter(transcriptRowHasContent);
  const shown = renderable.slice(0, 6);
  const hiddenMetadata = allRows.length - renderable.length;
  return (
    <Section title="Transcript Samples" tier="drill-down" questions={["What did recent agents say?", "Was output sanitized before render?", "Where is the source row?"]}>
      {shown.length ? (
        <>
          <div className="grid gap-3 lg:grid-cols-2">
            {shown.map((row, index) => (
              <TranscriptTurn key={`${rawText(row.spawn_id)}-${rawText(row.line_no)}-${index}`} row={row} />
            ))}
          </div>
          {hiddenMetadata > 0 ? (
            <p className="mt-3 text-xs text-muted">
              Showing {shown.length} of {renderable.length} content rows · {hiddenMetadata} session-metadata row
              {hiddenMetadata === 1 ? "" : "s"} hidden (no transcript content).
            </p>
          ) : null}
        </>
      ) : (
        <EmptyStateArt title={allRows.length ? "No transcript content in recent rows" : "No transcript rows"} />
      )}
    </Section>
  );
}

function EmptyStateArt({ title }: { title: string }) {
  return (
    <div className="overflow-hidden rounded-lg border border-border bg-surface-1">
      <img src={emptyStateUrl} alt="" className="h-32 w-full object-cover opacity-80" />
      <div className="border-t border-border-subtle p-3">
        <EmptyState title={title} />
      </div>
    </div>
  );
}

function routeFromHash(hash: string): DashboardRouteId | null {
  const raw = hash.replace(/^#\/?/, "").split(/[/?]/)[0];
  return isDashboardRouteId(raw) ? raw : null;
}

function isEditableShortcutTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tagName = target.tagName.toLowerCase();
  return tagName === "input" || tagName === "textarea" || tagName === "select";
}

function isDashboardRouteId(value: unknown): value is DashboardRouteId {
  return typeof value === "string" && VALID_ROUTE_IDS.has(value);
}

function isDensity(value: unknown): value is Density {
  return value === "comfortable" || value === "compact";
}

function isTheme(value: unknown): value is Theme {
  return value === "dark" || value === "light";
}
