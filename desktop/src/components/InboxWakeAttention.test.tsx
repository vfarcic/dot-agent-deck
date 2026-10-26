import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import words from "../../../tests/fixtures/inbox-display.json";
import { DeckShell } from "../App";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, mapDesktopSnapshot, TauriDeckBridge, type DesktopSnapshotDto } from "../lib/bridge";
import type { DeckFleet, DeckRuntimeState, DeckSnapshot, PanelTab } from "../types";
import { AgentTile } from "./AgentTile";

const listeners = new Map<string, (event: { payload: unknown }) => void>();
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", async (importOriginal) => ({ ...await importOriginal<object>(), invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: (event: { payload: unknown }) => void) => {
    listeners.set(name, callback);
    return () => listeners.delete(name);
  }),
}));
vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));
afterEach(() => { listeners.clear(); vi.clearAllMocks(); });

const message = "m-k3xzq2ab-0f3c9a71d2e4b850";
function snapshot(hold: string | null = "possible_draft"): DesktopSnapshotDto {
  const agent = {
    id: "7", incarnation: "daemon-a-generation-7", paneId: "captain-pane", displayName: "Coordinator", agentType: "codex" as const,
    rows: 24, cols: 80, status: "idle" as const, toolCount: 0,
    // Start-role membership determines prominence, not a literal role name.
    tab: { kind: "orchestration" as const, name: "team-away", orchestrationId: "away-19", roleIndex: 0, roleName: "captain", isStartRole: true },
    inbox: { queued: 1, fetched: 0, undeliverable: 0, wake: "doorbell", wakeStalled: true, ...(hold ? { wakeHold: hold } : {}) },
  };
  return {
    connection: { status: "connected", deckId: "deck-0000000000001666", socketPath: "/tmp/attention-test.sock",
      deckKind: "local", clientProtocolVersion: 11, clientBuildVersion: "test" },
    agents: [agent, { ...agent, id: "8", incarnation: "daemon-a-generation-8", paneId: "worker-pane", displayName: "Helper",
      tab: { ...agent.tab, roleIndex: 1, roleName: "coder", isStartRole: false } }],
    protocolVersion: 11, source: "daemon",
  };
}

function runtime(snapshot: DeckSnapshot): DeckRuntimeState {
  return {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures("?experimental=1"), snapshot, fleet: [snapshot], terminalData: {},
    clearError: vi.fn(), runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => {}), resizeTerminal: vi.fn(async () => {}), setShownTerminals: vi.fn(async () => {}),
    reconnect: vi.fn(async () => {}), listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("not used by attention"); }), setZoom: vi.fn(async (level) => level),
    getSettings: vi.fn(async () => ({ settings: DEFAULT_DESKTOP_SETTINGS })), saveSettings: vi.fn(async (settings) => settings),
    testEndpoint: vi.fn(async (_settings, selection) => ({ endpointId: selection, deck: selection, state: "ssh_unavailable" as const, ok: false,
      message: "No daemon in a component test", disclosureKnown: false, forwards: [], knownHosts: [], clientProtocolVersion: 11, clientBuildVersion: "test" })),
    secretStatus: vi.fn(async () => ({ stored: false })), storeSecret: vi.fn(async () => ({ stored: false })), forgetSecret: vi.fn(async () => ({ stored: false })),
  };
}

function tile(mapped: DeckSnapshot, tab: PanelTab = "terminal") {
  return <AgentTile agent={mapped.agents[0]} presentation="tile" showDetails={true} mode="live"
    selected={false} tab={tab} evidence={mapped.evidence} onSelect={vi.fn()} onTabChange={vi.fn()}
    onTerminalInput={vi.fn(async () => {})} onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()} />;
}

function header(container: HTMLElement): HTMLElement {
  const top = container.querySelector<HTMLElement>(".topbar");
  expect(top, "control: the real desktop header rendered").not.toBeNull();
  expect(top).toBeVisible();
  return top!;
}

// Keep the labelled-count grammar identical to e2e_inbox_status.rs.
const oneOtherHeld = /(?:^|[^a-z0-9_])(?:1\s+(?:other|held|blocked)|(?:other|held|blocked)\s*:?\s*1)(?:$|[^a-z0-9_])/i;

function expectAttention(top: HTMLElement, reason = words.wakeHolds.possible_draft) {
  expect(top).toHaveTextContent(/captain/i);
  expect(top).toHaveTextContent(reason);
  expect(top).toHaveTextContent(words.glyph);
  expect(top).toHaveTextContent(oneOtherHeld);
  expect(top).not.toHaveTextContent(/coder/i);
}

function expectOnlyCount(top: HTMLElement) {
  expect(top).not.toHaveTextContent(/captain|coder/i);
  expect(top).not.toHaveTextContent(words.wakeHolds.possible_draft);
  expect(top).not.toHaveTextContent(words.wakeHolds.awaiting_prompt);
  expect(top).toHaveTextContent(words.glyph);
  expect(top).toHaveTextContent(oneOtherHeld);
}

function wake(agentId: string, paneId: string, hold?: string) {
  listeners.get("desktop://daemon-event")?.({ payload: {
    kind: "inbox", pane_id: paneId, agent_id: agentId, message_id: message,
    subject: "worker report", from_label: "coder", state: "wake_update", wake_stalled: true,
    ...(hold ? { wake_hold: hold } : {}),
  } });
}

describe("inbox/status/034 — orchestrator wake attention", () => {
  /// Scenario: inbox/status/034 — Render an unselected start role's Diff tab with a possible draft, then open the real deck and select Coder while Captain remains held. The header must name Captain and count the other worker before and after selecting Coder, and retain attention through a dashboard round-trip with Coder selected again on return.
  it("shows an off-selected-tab orchestrator in the deck header and on its tile", () => {
    const mapped = mapDesktopSnapshot(snapshot());
    const offTab = render(tile(mapped, "diff"));
    expect(screen.getByRole("article", { name: "Captain" })).toHaveTextContent(words.wakeHolds.possible_draft);
    expect(screen.getByRole("tab", { name: "Diff" })).toHaveAttribute("aria-selected", "true");
    offTab.unmount();
    const view = render(<DeckShell runtime={runtime(mapped)} initialView={{ kind: "deck" }} />);
    const coder = screen.getByRole("article", { name: "Coder" });
    expect(coder).not.toHaveClass("is-selected");
    expectAttention(header(view.container));
    fireEvent.mouseDown(coder);
    expect(coder).toHaveClass("is-selected");
    expect(screen.getByRole("article", { name: "Captain" })).not.toHaveClass("is-selected");
    expectAttention(header(view.container));
    fireEvent.click(screen.getByTestId("open-overview"));
    expectAttention(header(view.container));
    fireEvent.click(screen.getByTestId("open-deck"));
    fireEvent.mouseDown(screen.getByRole("article", { name: "Coder" }));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveClass("is-selected");
    expectAttention(header(view.container));
  });

  /// Scenario: inbox/status/034 — A connected real bridge receives a wake-update reason change and then its clear for the listed generation. The already-mounted tile and header must follow both without a replacement snapshot; clearing Captain's hold removes its header detail while the other held worker remains counted.
  it.each(["awaiting_prompt", null])("renders a broadcast reason change or clear (%s) without waiting for a snapshot", async (nextHold) => {
    invoke.mockResolvedValue(snapshot());
    const bridge = new TauriDeckBridge();
    let fleet: DeckFleet = [];
    try {
      await bridge.subscribe((next) => { fleet = next; }, vi.fn());
      fleet = await bridge.connect();
      const shell = () => <DeckShell runtime={runtime(fleet[0])} initialView={{ kind: "deck" }} />;
      const view = render(shell());
      expect(screen.getByRole("article", { name: "Captain" })).toHaveTextContent(words.wakeHolds.possible_draft);
      expectAttention(header(view.container));
      await act(async () => { wake("7", "captain-pane", nextHold ?? undefined); });
      view.rerender(shell());
      expect(screen.getByRole("article", { name: "Captain" })).not.toHaveTextContent(words.wakeHolds.possible_draft);
      expect(screen.getByRole("article", { name: "Captain" })).toHaveTextContent(nextHold ? words.wakeHolds.awaiting_prompt : words.wakeStalled);
      expect(header(view.container)).not.toHaveTextContent(words.wakeHolds.possible_draft);
      if (nextHold) expectAttention(header(view.container), words.wakeHolds.awaiting_prompt);
      else expectOnlyCount(header(view.container));
      await act(async () => { wake("7", "captain-pane"); });
      view.rerender(shell());
      expect(screen.getByRole("article", { name: "Captain" })).not.toHaveTextContent(words.wakeHolds.awaiting_prompt);
      expect(screen.getByRole("article", { name: "Captain" })).toHaveTextContent(words.wakeStalled);
      expectOnlyCount(header(view.container));
    } finally { await bridge.dispose(); }
  });

  /// Scenario: inbox/status/034 — Reconnect to a daemon whose snapshot already lists a held start role without receiving a wake broadcast. The newly mounted deck header must show the same attention directly from that snapshot.
  it("restores deck-header attention from the reconnect snapshot alone", async () => {
    invoke.mockResolvedValue(snapshot());
    const bridge = new TauriDeckBridge();
    try {
      await bridge.subscribe(vi.fn(), vi.fn());
      await bridge.connect();
      const fleet = await bridge.connect();
      const view = render(<DeckShell runtime={runtime(fleet[0])} initialView={{ kind: "deck" }} />);
      expectAttention(header(view.container));
    } finally { await bridge.dispose(); }
  });

  /// Scenario: inbox/status/034 — Replace a held start-role generation with a clean successor in the same pane, then receive a delayed predecessor wake update. Neither the successor tile nor header may inherit that hold; a fresh update for the successor must appear under its own role.
  it("binds attention to the current generation and start-role membership", async () => {
    invoke.mockResolvedValue(snapshot());
    const bridge = new TauriDeckBridge();
    let fleet: DeckFleet = [];
    try {
      await bridge.subscribe((next) => { fleet = next; }, vi.fn());
      fleet = await bridge.connect();
      const replacement = snapshot(null);
      replacement.agents[0] = { ...replacement.agents[0], id: "9", incarnation: "daemon-a-generation-9" };
      replacement.agents[1] = { ...replacement.agents[1], inbox: { ...replacement.agents[1].inbox!, wakeHold: "possible_draft" } };
      await act(async () => { listeners.get("desktop://snapshot")?.({ payload: replacement }); });
      await act(async () => { wake("7", "captain-pane", "possible_draft"); });
      const view = render(<DeckShell runtime={runtime(fleet[0])} initialView={{ kind: "deck" }} />);
      expect(screen.getByRole("article", { name: "Captain" })).not.toHaveTextContent(words.wakeHolds.possible_draft);
      expect(header(view.container)).not.toHaveTextContent(words.wakeHolds.possible_draft);
      await act(async () => { wake("9", "captain-pane", "awaiting_prompt"); });
      view.rerender(<DeckShell runtime={runtime(fleet[0])} initialView={{ kind: "deck" }} />);
      expectAttention(header(view.container), words.wakeHolds.awaiting_prompt);
    } finally { await bridge.dispose(); }
  });
});
