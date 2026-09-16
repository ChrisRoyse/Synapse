export type StoryTheme = "dark" | "light";
export type StoryDensity = "comfortable" | "compact";

export interface VisualStoryCase {
  id: string;
  label: string;
}

export const primitiveCoverage = [
  "AgentPeek",
  "AppShell",
  "Button",
  "DataTable",
  "EmptyState",
  "FleetRow",
  "FlagBadge",
  "MetricRow",
  "PageHeader",
  "RawValue",
  "Section",
  "StatCard",
  "StatusBadge",
  "Switch",
  "Tabs",
  "ToolCallCard",
  "Tooltip",
  "TranscriptTurn"
] as const;

export const viewStateCoverage = ["loading", "empty", "populated", "error"] as const;

export const visualStoryCases: VisualStoryCase[] = [
  { id: "command-center-fleet-overview--populated", label: "fleet-overview-populated" },
  { id: "command-center-fleet-overview--empty", label: "fleet-overview-empty" },
  { id: "command-center-fleet-overview--loading", label: "fleet-overview-loading" },
  { id: "command-center-fleet-overview--error", label: "fleet-overview-error" },
  { id: "command-center-primitives--stat-card-attention", label: "stat-card-attention" },
  { id: "command-center-primitives--badges-statuses", label: "status-badges" },
  { id: "command-center-primitives--section-questioned", label: "section" },
  { id: "command-center-primitives--tool-call-card-success-and-error", label: "tool-call-card" },
  { id: "command-center-primitives--fleet-row-attention", label: "fleet-row" },
  { id: "command-center-primitives--data-table-fleet-table", label: "data-table" },
  { id: "command-center-primitives--agent-peek-tabs", label: "agent-peek" },
  { id: "command-center-primitives--transcript-turn-sanitized-markdown", label: "transcript-turn" },
  { id: "command-center-primitives--empty-state-quiet", label: "empty-state" },
  { id: "command-center-primitives--controls-buttons-switch-tooltip", label: "controls" },
  { id: "command-center-primitives--app-shell-layout", label: "app-shell" }
];

export const visualModes: Array<{ theme: StoryTheme; density: StoryDensity }> = [
  { theme: "dark", density: "comfortable" },
  { theme: "dark", density: "compact" },
  { theme: "light", density: "comfortable" },
  { theme: "light", density: "compact" }
];
