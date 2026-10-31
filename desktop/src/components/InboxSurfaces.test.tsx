import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import words from "../../../tests/fixtures/inbox-display.json";
import terminationCases from "../../../tests/fixtures/inbox-termination-cards.json";
import { AgentTile } from "./AgentTile";
import { HandoffRail } from "./HandoffRail";
import { mapDesktopSnapshot, TauriDeckBridge, type DesktopSnapshotDto } from "../lib/bridge";
import { applyHandoffEvent } from "../lib/daemonEvents";
import { INBOX_WORDS } from "../lib/inboxWords";
import type { AgentSession, DeckFleet, HandoffEdge } from "../types";

const listeners = new Map<string, (event: { payload: unknown }) => void>();
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: (event: { payload: unknown }) => void) => {
    listeners.set(name, callback);
    return () => listeners.delete(name);
  }),
}));
vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

// Handwritten from AgentRecord.inbox / InboxNotice: no generated TS inbox
// bindings exist yet. Structural assignment keeps these REDs behavioural.
function snapshot(queued = 0, fetched = 0, wakeStalled = false): DesktopSnapshotDto {
  const agent = {
    id: "7", paneId: "worker", displayName: "Coder", agentType: "codex" as const,
    rows: 24, cols: 80, status: "idle" as const, toolCount: 14,
    lastUserPrompt: "inspect the human fixture",
    tab: { kind: "orchestration" as const, name: "test", roleIndex: 1, roleName: "coder", isStartRole: false },
    inbox: { queued, fetched, undeliverable: 0, wake: "doorbell", component: undefined,
      ...(wakeStalled ? { wakeStalled } : {}) },
  };
  return {
    connection: { status: "connected", deckId: "deck-0000000000001666", socketPath: "/tmp/test.sock",
      deckKind: "local", clientProtocolVersion: 11, clientBuildVersion: "test" },
    agents: [agent], protocolVersion: 11, source: "daemon",
  };
}

function card(agent: AgentSession, evidence = mapDesktopSnapshot(snapshot()).evidence) {
  return <AgentTile agent={agent} presentation="tile" showDetails={false} mode="live"
    selected={false} tab="terminal" evidence={evidence} onSelect={vi.fn()}
    onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
    onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()} />;
}

const id = "m-k3xzq2ab-0f3c9a71d2e4b850";
function inbox(state: string, extra: Record<string, unknown> = {}) {
  return { kind: "inbox", pane_id: "worker", agent_id: "7", message_id: id,
    subject: "review the fixture", from_label: "the orchestrator", state, ...extra };
}
function edge(status: HandoffEdge["status"]): HandoffEdge {
  return { id, toRole: "coder", taskPreview: "review the fixture", status, respawned: false, at: "—" };
}

describe("inbox cards and handoff rail (S13)", () => {
  /// Scenario: Render the real card from a desktop DTO with queued and fetched messages; both count toward its envelope badge.
  it.each([[2, 0], [0, 2], [1, 1]])("shows ✉2 for %i queued and %i fetched messages", (queued, fetched) => {
    const mapped = mapDesktopSnapshot(snapshot(queued, fetched));
    render(card(mapped.agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(`${words.glyph}2`);
  });

  /// Scenario: The daemon reports that nothing has woken the agent for its waiting message; the card says it is waiting to wake and what to do, in the words the TUI uses.
  it("shows a stalled wake and what to do on the agent card", () => {
    render(card(mapDesktopSnapshot(snapshot(1, 0, true)).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(
      `${words.glyph}1 ${words.wakeStalled}: ${words.wakeStalledHint}`,
    );
  });

  /// Scenario: inbox/status/030 — the daemon says why it holds the doorbell for an idle agent's waiting message whose wake also stalled: a possible draft, a possible prompt, an unfinished paste, a release or input the deck can't type into, an Enter the agent did not take, the deck's line possibly left in the input, and a dialog that may have taken its keys. The tile's notice row says why with the count, in the words the TUI uses, and its badge keeps the count without the stall's words.
  it.each([
    "possible_draft",
    "awaiting_prompt",
    "paste_open",
    "cannot_type",
    "not_taken",
    "uncertain_input",
    "dialog_during_ring",
    "needs_fetch",
    "cannot_check",
  ] as const)("shows why the doorbell holds (%s) on the agent card", (hold) => {
    const dto = snapshot(1, 0, true);
    Object.assign(dto.agents[0].inbox!, { wakeHold: hold });
    render(card(mapDesktopSnapshot(dto).agents[0]));
    const tile = screen.getByRole("article", { name: "Coder" });
    expect(tile).toHaveTextContent(`${words.glyph}1 ${words.wakeHolds[hold]}`);
    expect(tile).not.toHaveTextContent(words.wakeStalled);
  });

  /// Scenario: inbox/probe/018 — each held-typing cause arrives through the snapshot bridge with no waiting messages, while the agent is idle or thinking. The tile shows the shared words and clears them on the next snapshot of the same agent.
  it.each(Object.entries(words.inputHeld).flatMap(([cause, text]) => ["idle", "thinking"].map((status) => [cause, status, text])))
  ("inbox/probe/018: held typing %s while %s (%s)", (cause, status, text) => {
    const dto = snapshot();
    Object.assign(dto.agents[0], { status, inputHeld: { generation: 9, cause, heldOps: 2, heldBytes: 7, sinceMs: 1 } });
    const view = render(card(mapDesktopSnapshot(dto).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(text);
    view.rerender(card(mapDesktopSnapshot(snapshot()).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).not.toHaveTextContent(text);
  });

  /// Scenario: inbox/probe/018 — a cannot-check detail names the refused path and its cause. Replacing the reason with needs-fetch and then clearing it removes the old path from the same tile.
  it.each(["filesystem", "writable_directory", "foreign_owner", "acl", "export_file", "io"])
  ("inbox/probe/018: displays and clears %s detail", (cause) => {
    const dto = snapshot(1);
    Object.assign(dto.agents[0].inbox!, { wakeHold: "cannot_check", holdDetail: {
      cause, pathComponent: "/safe/old-path", filesystem: "overlayfs",
    } });
    const view = render(card(mapDesktopSnapshot(dto).agents[0]));
    const tile = screen.getByRole("article", { name: "Coder" });
    expect(tile).toHaveTextContent("/safe/old-path");
    if (cause === "filesystem") {
      for (const required of ["overlayfs", "TMPDIR", "CODEX_HOME", "ext4", "XFS", "Btrfs", "tmpfs", "APFS", "HFS+"]) {
        expect(tile).toHaveTextContent(required);
      }
    }
    if (cause === "writable_directory") expect(tile).toHaveTextContent("chmod g-w");
    const next = snapshot(1);
    Object.assign(next.agents[0].inbox!, { wakeHold: "needs_fetch" });
    view.rerender(card(mapDesktopSnapshot(next).agents[0]));
    expect(tile).toHaveTextContent(words.wakeHolds.needs_fetch);
    expect(tile).not.toHaveTextContent("old-path");
    view.rerender(card(mapDesktopSnapshot(snapshot()).agents[0]));
    expect(tile).not.toHaveTextContent("old-path");
    expect(tile).not.toHaveTextContent(words.wakeHolds.needs_fetch);
  });

  /// Scenario: inbox/probe/018 — missing and unfamiliar cannot-check details use generic words, including when the stalled bit is absent. No stale path is attributed to a cause this client does not understand.
  it.each([undefined, { cause: "future_cause", pathComponent: "do-not-inherit" }])
  ("inbox/probe/018: generic cannot-check detail %j", (holdDetail) => {
    const dto = snapshot(1);
    Object.assign(dto.agents[0].inbox!, { wakeHold: "cannot_check", holdDetail });
    render(card(mapDesktopSnapshot(dto).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(words.wakeHolds.cannot_check);
    expect(screen.getByRole("article", { name: "Coder" })).not.toHaveTextContent("do-not-inherit");
  });

  /// Scenario: inbox/probe/018 — an older client receives a future hold without the stalled bit. Its card retains the generic waiting-to-wake fallback.
  it("inbox/probe/018: an unrecognized wordless hold keeps the generic fallback", () => {
    const dto = snapshot(1);
    Object.assign(dto.agents[0].inbox!, { wakeHold: "future_hold" });
    render(card(mapDesktopSnapshot(dto).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(`${words.wakeStalled}: ${words.wakeStalledHint}`);
  });

  /// Scenario: inbox/probe/018 — a needs-fetch or cannot-check hold has no display words and no stalled bit. The real tile must still show the generic waiting-to-wake fallback, rather than queued or undefined.
  it.each(["needs_fetch", "cannot_check"])("inbox/probe/018: wordless %s uses the generic fallback", (hold) => {
    const table: Partial<Record<string, string>> = INBOX_WORDS.wakeHolds;
    const saved = table[hold];
    delete table[hold];
    try {
      const dto = snapshot(1);
      Object.assign(dto.agents[0].inbox!, { wakeHold: hold });
      render(card(mapDesktopSnapshot(dto).agents[0]));
      expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(`${words.wakeStalled}: ${words.wakeStalledHint}`);
      expect(screen.getByRole("article", { name: "Coder" })).not.toHaveTextContent("undefined");
    } finally {
      if (saved === undefined) delete table[hold];
      else table[hold] = saved;
    }
  });

  /// Scenario: inbox/probe/018 — oversized path and filesystem strings arrive from the wire with controls, bidi and shell punctuation. The rendered diagnostic escapes the controls, bounds each field to 256 source characters and keeps the path out of command markup.
  it("inbox/probe/018: bounds and escapes both wire detail strings", () => {
    const dto = snapshot(1);
    const pathComponent = `/safe/\n\u001b\u202e$(touch sentinel);${"é".repeat(270)}PATH_TAIL`;
    const filesystem = `fs-\n\u001b\u202e${"界".repeat(270)}FS_TAIL`;
    Object.assign(dto.agents[0].inbox!, { wakeHold: "cannot_check", holdDetail: {
      cause: "filesystem", pathComponent, filesystem,
    } });
    render(card(mapDesktopSnapshot(dto).agents[0]));
    const tile = screen.getByRole("article", { name: "Coder" });
    expect(tile.textContent).toContain("/safe/\\n\\u{1b}\\u{202e}$(touch sentinel);");
    expect(tile.textContent).toContain("fs-\\n\\u{1b}\\u{202e}");
    for (const forbidden of ["PATH_TAIL", "FS_TAIL", "\u001b", "\u202e", "é".repeat(257), "界".repeat(257)]) {
      expect(tile.textContent).not.toContain(forbidden);
    }
    for (const raw of [pathComponent, filesystem]) {
      const escaped = (limit: number) => [...raw].slice(0, limit).join("")
        .replaceAll("\n", "\\n").replaceAll("\u001b", "\\u{1b}").replaceAll("\u202e", "\\u{202e}");
      expect(tile.textContent).toContain(escaped(128));
      expect(tile.textContent).not.toContain(escaped(257));
    }
    expect(screen.getByTestId("inbox-notice-7").querySelector("code, pre, a[href^='javascript:']")).toBeNull();
    expect(tile.textContent).not.toContain("chmod g-w /safe/");
  });

  /// Scenario: inbox/probe/018 — a wake notice replaces a snapshot's cannot-check detail with a new detail, then changes to needs-fetch without detail. The subscribed bridge and tile show the current explanation and discard the old path.
  it("inbox/probe/018: streamed reason changes replace and clear the detail", async () => {
    invoke.mockResolvedValue(snapshot(1));
    const bridge = new TauriDeckBridge();
    let fleet: DeckFleet = [];
    try {
      await bridge.subscribe((next) => { fleet = next; }, vi.fn());
      fleet = await bridge.connect();
      const view = render(card(fleet[0].agents[0], fleet[0].evidence));
      for (const path of ["/safe/first-path", "/safe/second-path"]) {
        listeners.get("desktop://daemon-event")?.({ payload: inbox("queued", {
          wake_update: true, wake_hold: "cannot_check", hold_detail: { cause: "io", path_component: path },
        }) });
        view.rerender(card(fleet[0].agents[0], fleet[0].evidence));
        expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(path);
        if (path.includes("second")) expect(screen.getByRole("article", { name: "Coder" })).not.toHaveTextContent("first-path");
      }
      listeners.get("desktop://daemon-event")?.({ payload: inbox("queued", { wake_update: true, wake_hold: "needs_fetch" }) });
      view.rerender(card(fleet[0].agents[0], fleet[0].evidence));
      expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(words.wakeHolds.needs_fetch);
      expect(screen.getByRole("article", { name: "Coder" })).not.toHaveTextContent("second-path");
    } finally { await bridge.dispose(); }
  });

  /// Scenario: inbox/status/030 — with nothing waiting, the deck's line possibly left in the input and a dialog that may have taken its keys still warn on the tile, in the words the TUI uses; a release the deck can't type into says nothing then.
  it.each([
    ["uncertain_input", true],
    ["dialog_during_ring", true],
    ["cannot_type", false],
  ] as const)("warns about %s with nothing waiting: %s", (hold, warns) => {
    const dto = snapshot(0, 0, false);
    Object.assign(dto.agents[0].inbox!, { wakeHold: hold });
    render(card(mapDesktopSnapshot(dto).agents[0]));
    const tile = screen.getByRole("article", { name: "Coder" });
    const alone = (words.wakeHoldsAlone as Record<string, string>)[hold];
    if (warns) {
      expect(tile).toHaveTextContent(alone);
    } else {
      expect(alone).toBeUndefined();
      expect(tile).not.toHaveTextContent(words.wakeHolds[hold]);
    }
  });

  /// Scenario: inbox/status/034 — a reason a newer daemon names that this build does not know shows the stall's words, as an older daemon's summary without the field does.
  it.each<[Record<string, unknown>, string]>([
    [{ wakeHold: "a_newer_reason" }, "a reason this build does not know"],
    [{ wakeStalled: true }, "a daemon older than the field"],
  ])("shows the stall's words for %o (%s)", (extra, _why) => {
    const dto = snapshot(1, 0, false);
    Object.assign(dto.agents[0].inbox!, extra);
    render(card(mapDesktopSnapshot(dto).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" })).toHaveTextContent(
      `${words.glyph}1 ${words.wakeStalled}: ${words.wakeStalledHint}`,
    );
  });

  /// Scenario: Acknowledged messages leave no pending badge on the card.
  it("omits the pending envelope when queued and fetched are zero", () => {
    render(card(mapDesktopSnapshot(snapshot()).agents[0]));
    expect(screen.getByRole("article", { name: "Coder" }).textContent).not.toMatch(/✉\d/);
  });

  /// Scenario: inbox/status/008 — receive each pane-termination broadcast and its captured daemon delivery-notice event through the real subscribed bridge. The tile shows Error and the same complete reason and exit code as the TUI, including a wrapper-attributed exit retained after close.
  it.each(terminationCases)("shows Error and the pane's termination on the agent card ($name)", async (testCase) => {
    const dto = snapshot();
    invoke.mockResolvedValue(dto);
    const bridge = new TauriDeckBridge();
    let fleet: DeckFleet = [];
    try {
      await bridge.subscribe((next) => { fleet = next; }, vi.fn());
      await bridge.connect();
      const notice = (words.cardTerminations as Record<string, string>)[testCase.wordKey]
        .replace("{code}", String(testCase.termination.code));
      listeners.get("desktop://daemon-event")?.({ payload: inbox("undeliverable", {
        reason: "agent_exited", termination: testCase.termination,
      }) });
      listeners.get("desktop://daemon-event")?.({ payload: {
        kind: "event", session_id: "session-7", pane_id: "worker", agent_id: "7",
        agent_type: "codex", event_type: "error", timestamp: "2026-10-13T00:00:00Z",
        tool_detail: testCase.cardNotice, metadata: { delivery_notice: testCase.cardNotice },
      } });
      render(card(fleet[0].agents[0], fleet[0].evidence));
      const tile = screen.getByRole("article", { name: "Coder" });
      expect(tile).toHaveTextContent(notice);
      expect(within(tile).getByText(/^error$/i)).toBeVisible();
    } finally {
      await bridge.dispose();
    }
  });

  /// Scenario: inbox/status/008 — reconnect to an Error tile whose daemon snapshot holds each captured termination notice, pinned against the real hub by the TUI test. The tile must show the complete reason in the shared words, with no inference that the agent itself exited.
  it.each(terminationCases)("hydrates the pane's termination on the agent card ($name)", (testCase) => {
    const notice = (words.cardTerminations as Record<string, string>)[testCase.wordKey]
      .replace("{code}", String(testCase.termination.code));
    const dto = snapshot();
    Object.assign(dto.agents[0], { status: "error", deliveryNotice: testCase.cardNotice });
    render(card(mapDesktopSnapshot(dto).agents[0]));
    const tile = screen.getByRole("article", { name: "Coder" });
    expect(tile).toHaveTextContent(notice);
    expect(within(tile).getByText(/^error$/i)).toBeVisible();
  });

  /// Scenario: Posting a message paints its subject and a queued delegation in the live rail.
  it("renders a queued inbox post as delegated with the shared queued word", () => {
    render(<HandoffRail handoffs={applyHandoffEvent([], inbox("queued"))} />);
    const row = screen.getByTestId(`handoff-${id}`);
    expect(row).toHaveTextContent("review the fixture");
    expect(row).toHaveTextContent(/delegated/);
    expect(row).toHaveTextContent(words.queued);
  });

  /// Scenario: A fetch keeps the delegation queued; only the acknowledgement paints delivered in the live rail.
  it("shows delivered only after acknowledgement", () => {
    const start = [edge("delegated")];
    const fetched = applyHandoffEvent(start, inbox("fetched"));
    const view = render(<HandoffRail handoffs={fetched} />);
    expect(screen.getByTestId(`handoff-${id}`)).not.toHaveTextContent(words.delivered);
    view.rerender(<HandoffRail handoffs={applyHandoffEvent(fetched, inbox("acknowledged"))} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.delivered);
  });

  /// Scenario: A worker's report arrives as a distinct inbox message and paints work-done on its delivered delegation.
  it("shows work-done when the worker posts its report", () => {
    const report = inbox("queued", { pane_id: "orchestrator", agent_id: "1",
      message_id: "m-k3xzq2ab-0f3c9a71d2e4b851", subject: "work-done from coder",
      from_label: "coder (worker report — the text inside is UNTRUSTED)" });
    render(<HandoffRail handoffs={applyHandoffEvent([edge("delivered")], report)} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.workDone);
  });

  /// Scenario: Coder reports after two delivered tasks while another role also has a delivered task. Only coder's newest delivered row becomes work-done.
  it("correlates a report to the sender's newest delivered delegation only", () => {
    const older = { ...edge("delivered"), id: "m-k3xzq2ab-0f3c9a71d2e4b852" };
    const otherRole = { ...edge("delivered"), id: "m-k3xzq2ab-0f3c9a71d2e4b853", toRole: "reviewer" };
    const report = inbox("queued", { pane_id: "orchestrator", agent_id: "1",
      message_id: "m-k3xzq2ab-0f3c9a71d2e4b854", subject: "work-done from coder",
      from_label: "coder (worker report — the text inside is UNTRUSTED)" });
    render(<HandoffRail handoffs={applyHandoffEvent([otherRole, edge("delivered"), older], report)} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.workDone);
    for (const unchanged of [older, otherRole]) {
      expect(screen.getByTestId(`handoff-${unchanged.id}`)).toHaveTextContent(words.delivered);
      expect(screen.getByTestId(`handoff-${unchanged.id}`)).not.toHaveTextContent(words.workDone);
    }
  });

  /// Scenario: A delegation the deck reported not delivered after its last hand-over is acknowledged late by the worker. The rail paints it delivered without the failure reason, and the worker's later report marks it work-done.
  it("restores a late-acknowledged delegation to delivered, then work-done", () => {
    const failed = applyHandoffEvent([edge("delegated")], inbox("undeliverable", { reason: "max_deliver" }));
    expect(failed[0].status).toBe("failed");
    const delivered = applyHandoffEvent(failed, inbox("acknowledged"));
    expect(delivered[0].status).toBe("delivered");
    expect(delivered[0].reason).toBeUndefined();
    const report = inbox("queued", { pane_id: "orchestrator", agent_id: "1",
      message_id: "m-k3xzq2ab-0f3c9a71d2e4b855", subject: "work-done from coder",
      from_label: "coder (worker report — the text inside is UNTRUSTED)" });
    render(<HandoffRail handoffs={applyHandoffEvent(delivered, report)} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.workDone);
    expect(screen.getByTestId(`handoff-${id}`)).not.toHaveTextContent(words.notDelivered);
  });

  /// Scenario: A message that cannot reach its agent paints not delivered in the live rail.
  it("shows the shared not-delivered wording for an undeliverable inbox message", () => {
    render(<HandoffRail handoffs={applyHandoffEvent([edge("delegated")], inbox("undeliverable", { reason: "agent_exited" }))} />);
    expect(screen.getByTestId(`handoff-${id}`)).toHaveTextContent(words.notDelivered);
  });
});
