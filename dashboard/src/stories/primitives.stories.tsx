import type { Meta, StoryObj } from "@storybook/react-vite";
import { Bell, CheckCircle2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { FlagBadge, StatusBadge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
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
  attentionAgent,
  toolCall,
  transcriptSample,
  transcriptToolOnlySample
} from "@/stories/fixtures";

const meta = {
  title: "Command Center/Primitives",
  parameters: {
    layout: "centered"
  }
} satisfies Meta;

export default meta;

type Story = StoryObj<typeof meta>;

export const StatCardAttention: Story = {
  name: "StatCard / Attention",
  parameters: { docs: { description: { story: "coverage:StatCard coverage:MetricRow" } } },
  render: () => (
    <div className="w-[720px]">
      <div className="grid grid-cols-3 gap-4">
        <StatCard label="Attention" value={3} status="needs_input" delta="human review queued" />
        <StatCard label="Live Agents" value={7} status="working" delta="two blocked" />
        <div className="rounded-lg border border-border bg-surface-1 p-[var(--density-card-padding)]">
          <MetricRow label="Storage" value="Normal" />
          <MetricRow label="Rows" value="7400" />
        </div>
      </div>
    </div>
  )
};

export const BadgesStatuses: Story = {
  name: "Badges / Statuses",
  parameters: { docs: { description: { story: "coverage:StatusBadge coverage:FlagBadge" } } },
  render: () => (
    <div className="flex flex-wrap gap-2">
      {(["working", "idle", "ready_for_review", "needs_input", "awaiting_approval", "stuck", "failed", "done"] as const).map((status) => (
        <StatusBadge key={status} status={status} />
      ))}
      <FlagBadge tone="warn">truncated</FlagBadge>
      <FlagBadge tone="danger">hygiene</FlagBadge>
    </div>
  )
};

export const SectionQuestioned: Story = {
  name: "Section / Questioned",
  parameters: { docs: { description: { story: "coverage:Section coverage:RawValue" } } },
  render: () => (
    <div className="w-[760px]">
      <Section
        title="Tool Activity"
        tier="triage"
        questions={["Which tools are running?", "Which calls failed?", "Where is verification detail?"]}
        actions={<Button size="sm">Refresh</Button>}
      >
        <RawValue value={{ source: "CF_ACTION_LOG", status: "collapsed by default" }} label="Details" />
      </Section>
    </div>
  )
};

export const ToolCallCardSuccessAndError: Story = {
  name: "ToolCallCard / Success and Error",
  parameters: { docs: { description: { story: "coverage:ToolCallCard" } } },
  render: () => (
    <div className="grid w-[920px] grid-cols-2 gap-3">
      <ToolCallCard call={toolCall("success")} />
      <ToolCallCard call={toolCall("error")} />
    </div>
  )
};

export const FleetRowAttention: Story = {
  name: "FleetRow / Attention",
  parameters: { docs: { description: { story: "coverage:FleetRow" } } },
  render: () => (
    <div className="w-[680px] rounded-lg border border-border bg-surface-1">
      <FleetRow agent={attentionAgent()} selected onSelect={() => undefined} />
    </div>
  )
};

export const DataTableFleetTable: Story = {
  name: "DataTable / Fleet Table",
  parameters: { docs: { description: { story: "coverage:DataTable" } } },
  render: () => (
    <div className="w-[920px]">
      <DataTable
        data={[attentionAgent(), { ...attentionAgent(), id: "agent-local-002", status: "working", summary: "Running local model probe" }]}
        getRowId={(row) => row.id}
        columns={[
          { accessorKey: "id", header: "Agent" },
          { accessorKey: "kind", header: "Kind" },
          { accessorKey: "status", header: "Status" },
          { accessorKey: "summary", header: "Summary" }
        ]}
      />
    </div>
  )
};

export const AgentPeekTabs: Story = {
  name: "AgentPeek / Tabs",
  parameters: { docs: { description: { story: "coverage:AgentPeek coverage:Tabs" } } },
  render: () => (
    <div className="w-[520px]">
      <AgentPeek agent={attentionAgent()} />
    </div>
  )
};

export const TranscriptTurnSanitizedMarkdown: Story = {
  name: "TranscriptTurn / Sanitized Markdown",
  parameters: { docs: { description: { story: "coverage:TranscriptTurn" } } },
  render: () => (
    <div className="w-[640px]">
      <TranscriptTurn row={transcriptSample()} />
    </div>
  )
};

export const TranscriptTurnToolOnly: Story = {
  name: "TranscriptTurn / Tool Call Only",
  parameters: { docs: { description: { story: "coverage:TranscriptTurn" } } },
  render: () => (
    <div className="w-[640px]">
      <TranscriptTurn row={transcriptToolOnlySample()} />
    </div>
  )
};

export const EmptyStateQuiet: Story = {
  name: "EmptyState / Quiet",
  parameters: { docs: { description: { story: "coverage:EmptyState" } } },
  render: () => (
    <div className="w-[420px]">
      <EmptyState title="No command audit rows" />
    </div>
  )
};

export const ControlsButtonsSwitchTooltip: Story = {
  name: "Controls / Buttons Switch Tooltip",
  parameters: { docs: { description: { story: "coverage:Button coverage:Switch coverage:Tooltip" } } },
  render: () => (
    <div className="flex items-center gap-3">
      <Button>
        <Bell aria-hidden="true" className="h-4 w-4" />
        Next
      </Button>
      <Switch checked aria-label="Toggle light theme" />
      <Tooltip open>
        <TooltipTrigger asChild>
          <Button size="icon" variant="ghost" aria-label="Verified">
            <CheckCircle2 aria-hidden="true" className="h-4 w-4" />
          </Button>
        </TooltipTrigger>
        <TooltipContent>Verified</TooltipContent>
      </Tooltip>
    </div>
  )
};

export const AppShellLayout: Story = {
  name: "AppShell / Layout",
  parameters: { docs: { description: { story: "coverage:AppShell coverage:PageHeader" } } },
  render: () => (
    <div className="w-[960px] border border-border">
      <AppShell sidebar={<div className="text-sm text-primary">Synapse</div>}>
        <PageHeader title="Fleet Overview" subtitle="Updated now" actions={<Button size="sm">Refresh</Button>} />
        <EmptyState title="Layout surface" />
      </AppShell>
    </div>
  )
};
