import assert from "node:assert/strict";
import { describe, test } from "node:test";
import {
  createDashboardLiveController,
  dashboardEventScopesForEvent,
  initialDashboardLiveState,
  type DashboardEventSourceLike,
  type DashboardLiveEventMeta,
  type DashboardLiveState,
  type DashboardPanelScope
} from "../src/lib/live-data";

class FakeEventSource implements DashboardEventSourceLike {
  static created: FakeEventSource[] = [];
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  closed = false;
  readonly url: string;
  private listeners = new Map<string, ((event: MessageEvent) => void)[]>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.created.push(this);
  }

  addEventListener(type: string, listener: (event: MessageEvent) => void): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.(new Event("open"));
  }

  error(): void {
    this.onerror?.(new Event("error"));
  }

  emit(type: string, data: unknown, lastEventId = "1"): void {
    const event = { data: JSON.stringify(data), lastEventId } as MessageEvent;
    for (const listener of this.listeners.get(type) ?? []) {
      listener(event);
    }
  }
}

describe("dashboard live-data scope routing", () => {
  test("planted event sequence updates only matching panel scopes", () => {
    const sequence = [
      { kind: "agent_state_changed", source: "system" },
      { kind: "profile-changed", source: "system" },
      { kind: "approval_decision", source: "system" },
      { kind: "command_finished", source: "action_emitter" },
      { kind: "file_changed", source: "filesystem" }
    ];

    const updates = new Map<DashboardPanelScope, string[]>();
    for (const event of sequence) {
      for (const scope of dashboardEventScopesForEvent(event)) {
        updates.set(scope, [...(updates.get(scope) ?? []), event.kind]);
      }
    }

    assert.deepEqual(updates.get("fleet"), ["agent_state_changed", "approval_decision"]);
    assert.deepEqual(updates.get("agent"), ["agent_state_changed", "profile-changed"]);
    assert.deepEqual(updates.get("tasks"), ["agent_state_changed", "approval_decision"]);
    assert.deepEqual(updates.get("audit"), ["command_finished"]);
    assert.deepEqual(updates.get("system"), [
      "agent_state_changed",
      "profile-changed",
      "approval_decision",
      "command_finished",
      "file_changed"
    ]);
  });

  test("controller resyncs a scope on lossy replay and ignores nonmatching normal events", async () => {
    FakeEventSource.created = [];
    let state: DashboardLiveState = initialDashboardLiveState(["fleet"]);
    const scopeEvents: { scope: DashboardPanelScope; meta: DashboardLiveEventMeta }[] = [];
    const controller = createDashboardLiveController({
      scopes: ["fleet"],
      now: () => 1_800_000_000_000,
      setTimer: () => 0,
      clearTimer: () => undefined,
      subscribe: async (scope) => ({
        ok: true,
        source_of_truth: "test",
        scope,
        subscription_id: `sub-${scope}`,
        event_url: `/events?subscription_id=sub-${scope}`,
        replay_contract: "test"
      }),
      eventSourceFactory: (url) => new FakeEventSource(url),
      onStateChange: (next) => {
        state = next;
      },
      onScopeEvent: (scope, _event, meta) => {
        scopeEvents.push({ scope, meta });
      }
    });

    controller.start();
    await Promise.resolve();
    assert.equal(FakeEventSource.created.length, 1);
    const source = FakeEventSource.created[0];
    assert.equal(source.url, "/events?subscription_id=sub-fleet");
    source.open();
    assert.equal(state.fleet.status, "open");

    source.emit(
      "synapse/event",
      {
        params: {
          stream_seq: 2,
          lossy: false,
          event: { kind: "command_finished", source: "action_emitter" }
        }
      },
      "2"
    );
    assert.equal(scopeEvents.length, 0);
    assert.equal(state.fleet.eventCount, 0);

    source.emit(
      "synapse/event",
      {
        params: {
          stream_seq: 3,
          lossy: true,
          event: { kind: "file_changed", source: "filesystem" }
        }
      },
      "3"
    );
    assert.equal(scopeEvents.length, 1);
    assert.equal(scopeEvents[0].scope, "fleet");
    assert.equal(scopeEvents[0].meta.reason, "lossy_replay");
    assert.equal(state.fleet.eventCount, 1);
    assert.equal(state.fleet.resyncCount, 1);
    assert.equal(state.fleet.lastResyncUnixMs, 1_800_000_000_000);

    controller.stop();
  });

  test("controller schedules a replacement subscription after a hard stream error", async () => {
    FakeEventSource.created = [];
    let state: DashboardLiveState = initialDashboardLiveState(["fleet"]);
    const timers: { callback: () => void; cancelled: boolean }[] = [];
    let subscriptionOrdinal = 0;
    const controller = createDashboardLiveController({
      scopes: ["fleet"],
      setTimer: (callback) => {
        const timer = { callback, cancelled: false };
        timers.push(timer);
        return timer;
      },
      clearTimer: (timer) => {
        (timer as { cancelled: boolean }).cancelled = true;
      },
      subscribe: async (scope) => {
        subscriptionOrdinal += 1;
        return {
          ok: true,
          source_of_truth: "test",
          scope,
          subscription_id: `sub-${scope}-${subscriptionOrdinal}`,
          event_url: `/dashboard/events?subscription_id=sub-${scope}-${subscriptionOrdinal}`,
          replay_contract: "test"
        };
      },
      eventSourceFactory: (url) => new FakeEventSource(url),
      onStateChange: (next) => {
        state = next;
      }
    });

    controller.start();
    await Promise.resolve();
    const first = FakeEventSource.created[0];
    first.open();
    assert.equal(state.fleet.status, "open");

    first.error();
    assert.equal(state.fleet.status, "disconnected");
    assert.equal(timers.length, 1);
    assert.equal(FakeEventSource.created.length, 1);

    timers[0].callback();
    await Promise.resolve();
    assert.equal(first.closed, true);
    assert.equal(FakeEventSource.created.length, 2);
    assert.equal(FakeEventSource.created[1].url, "/dashboard/events?subscription_id=sub-fleet-2");

    controller.stop();
  });
});
