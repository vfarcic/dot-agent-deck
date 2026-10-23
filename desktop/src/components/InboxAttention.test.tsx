import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import words from "../../../tests/fixtures/inbox-display.json";
import { AgentTile } from "./AgentTile";
import { mapDesktopSnapshot, type DesktopSnapshotDto } from "../lib/bridge";
import { INBOX_WORDS } from "../lib/inboxWords";
import type { AgentSession } from "../types";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

// PRD #1666 §6.4–§6.5, §11: the tile's half of the both-client card. The TUI's
// card prints the same words from `src/inbox_display.rs`, and the shared
// fixture pins them to each other.
const first = "m-k3xzq2ab-0f3c9a71d2e4b850";
const second = "m-k3xzq2ab-0f3c9a71d2e4b851";

function item(messageId: string, kind: string, revision = 4) {
  return { messageId, revision, kind, fromLabel: "the orchestrator", subject: `review ${messageId.slice(-3)}` };
}

function snapshot(inbox: Record<string, unknown>): DesktopSnapshotDto {
  const agent = {
    id: "7", paneId: "worker", displayName: "Coder", agentType: "open_code" as const, incarnation: "listed-inbox-process-a",
    rows: 24, cols: 80, status: "working" as const, toolCount: 3,
    lastUserPrompt: "inspect the fixture",
    tab: { kind: "orchestration" as const, name: "test", roleIndex: 1, roleName: "coder", isStartRole: false },
    inbox: { queued: 0, fetched: 1, undeliverable: 0, wake: "native", component: "opencode_plugin", ...inbox },
  };
  return {
    connection: { status: "connected", deckId: "deck-0000000000001666", socketPath: "/tmp/test.sock",
      deckKind: "local", clientProtocolVersion: 11, clientBuildVersion: "test" },
    agents: [agent], protocolVersion: 11, source: "daemon",
  };
}

function tile(agent: AgentSession, onInboxAction = vi.fn(async () => ({ outcome: "applied" }))) {
  render(<AgentTile agent={agent} presentation="tile" showDetails={false} mode="live"
    selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()}
    onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
    onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()}
    onInboxAction={onInboxAction} />);
  return within(screen.getByRole("article", { name: "Coder" }));
}

describe("deck messages that need a decision (PRD #1666 §6)", () => {
  /// Scenario: The daemon's snapshot lists one uncertain delivery; the tile says delivery uncertain and what it means, and lists the message with Retry and Cancel.
  it("shows one uncertain delivery with its reason, sender, subject and both actions", () => {
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "uncertain")] })).agents[0];
    const card = tile(agent);
    expect(card.getByTestId("inbox-badge-7")).toHaveTextContent(`${words.glyph} ${words.uncertain}`);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.uncertainReason);
    const row = card.getByTestId(`inbox-attention-${first}`);
    expect(row).toHaveTextContent("the orchestrator");
    expect(row).toHaveTextContent(`review ${first.slice(-3)}`);
    expect(within(row).getByRole("button", { name: words.retry })).toBeVisible();
    expect(within(row).getByRole("button", { name: words.cancel })).toBeVisible();
  });

  /// Scenario: Two deliveries on one card are uncertain and one is out of attempts; the tile counts them together and offers only Cancel for the exhausted one.
  it("counts several on one card and offers only Cancel once a message is out of attempts", () => {
    const agent = mapDesktopSnapshot(snapshot({
      attention: [item(first, "uncertain"), item(second, "out_of_attempts")],
    })).agents[0];
    const card = tile(agent);
    expect(card.getByTestId("inbox-badge-7")).toHaveTextContent(`${words.glyph} 2 ${words.uncertainMany}`);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.uncertainReason);
    const exhausted = card.getByTestId(`inbox-attention-${second}`);
    expect(exhausted).toHaveTextContent(words.outOfAttempts);
    expect(within(exhausted).queryByRole("button", { name: words.retry })).toBeNull();
    expect(within(exhausted).getByRole("button", { name: words.cancel })).toBeVisible();
  });

  /// Scenario: The only uncertain delivery on a card is out of attempts; the tile still says delivery uncertain, and its notice says what running out of attempts means instead of offering Retry, as the TUI's card does.
  it("explains a delivery out of attempts without offering Retry", () => {
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "out_of_attempts")] })).agents[0];
    const card = tile(agent);
    expect(card.getByTestId("inbox-badge-7")).toHaveTextContent(`${words.glyph} ${words.uncertain}`);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.outOfAttempts);
    expect(card.getByTestId("inbox-notice-7")).not.toHaveTextContent(words.retry);
    expect(within(card.getByTestId(`inbox-attention-${first}`)).queryByRole("button", { name: words.retry })).toBeNull();
  });

  /// Scenario: A message held for an hour shows the attention sentence and still offers Retry and Cancel, without marking the agent failed.
  it("shows held_long as attention with both actions", () => {
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "held_long")] })).agents[0];
    const card = tile(agent);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.heldLong);
    const row = card.getByTestId(`inbox-attention-${first}`);
    expect(within(row).getByRole("button", { name: words.retry })).toBeVisible();
    expect(card.queryByText(/^error$/i)).toBeNull();
  });

  /// Scenario: The daemon's snapshot lists a deck message a Pi agent may not have received; the tile says so instead of calling it uncertain, explains that Retry may make the agent get it twice, and offers Retry and Cancel, without marking the agent failed.
  it("shows a message that may not have reached the agent with its duplicate warning and both actions", () => {
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "not_started")] })).agents[0];
    const card = tile(agent);
    expect(card.getByTestId("inbox-badge-7")).toHaveTextContent(`${words.glyph} ${words.notStarted}`);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.notStartedReason);
    const row = card.getByTestId(`inbox-attention-${first}`);
    expect(row).toHaveTextContent(words.notStartedReason);
    expect(within(row).getByRole("button", { name: words.retry })).toBeVisible();
    expect(within(row).getByRole("button", { name: words.cancel })).toBeVisible();
    expect(card.queryByText(/^error$/i)).toBeNull();
  });

  /// Scenario: Two messages on one card may not have reached the agent; the tile counts them, as the TUI's card does.
  it("counts several messages that may not have reached the agent", () => {
    const agent = mapDesktopSnapshot(snapshot({
      attention: [item(first, "not_started"), item(second, "not_started")],
    })).agents[0];
    expect(tile(agent).getByTestId("inbox-badge-7")).toHaveTextContent(`${words.glyph} 2 ${words.notStarted}`);
  });

  /// Scenario: One card lists an uncertain delivery, a message that may not have reached the agent and one held for an hour; the uncertain delivery leads the badge and the notice, and each row keeps its own reason.
  it("leads with the uncertain delivery on a card that lists all three kinds", () => {
    const third = "m-k3xzq2ab-0f3c9a71d2e4b852";
    const agent = mapDesktopSnapshot(snapshot({
      attention: [item(first, "uncertain"), item(second, "not_started"), item(third, "held_long")],
    })).agents[0];
    const card = tile(agent);
    expect(card.getByTestId("inbox-badge-7")).toHaveTextContent(new RegExp(`^${words.glyph} ${words.uncertain}$`));
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.uncertainReason);
    expect(card.getByTestId(`inbox-attention-${second}`)).toHaveTextContent(words.notStartedReason);
    expect(card.getByTestId(`inbox-attention-${third}`)).toHaveTextContent(words.heldLong);
  });

  /// Scenario: A message that may not have reached the agent and one held for an hour are on one card; the notice explains the first, not the held-for-an-hour words.
  it("explains the not-started message ahead of held_long", () => {
    const agent = mapDesktopSnapshot(snapshot({
      attention: [item(first, "held_long"), item(second, "not_started")],
    })).agents[0];
    expect(tile(agent).getByTestId("inbox-notice-7")).toHaveTextContent(words.notStartedReason);
  });

  /// Scenario: Retry on a message that may not have reached the agent is pressed after a turn started and the deck no longer lists it; the row says so in the shared words.
  it("says when Retry on a not-started message came too late", async () => {
    const onInboxAction = vi.fn(async () => ({ outcome: "not_applicable" }));
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "not_started", 6)] })).agents[0];
    const card = tile(agent, onInboxAction);
    fireEvent.click(within(card.getByTestId(`inbox-attention-${first}`)).getByRole("button", { name: words.retry }));
    await waitFor(() => expect(onInboxAction).toHaveBeenCalledWith(
      { deckId: agent.daemonId, agentId: "7", incarnation: "listed-inbox-process-a" },
      { paneId: "worker", messageId: first, revision: 6 },
      "retry",
    ));
    expect(await card.findByText(words.actionGone)).toBeVisible();
  });

  /// Scenario: The desktop's copy of the words is the shared fixture, the copy the TUI's words are checked against too.
  it("uses exactly the shared words", () => {
    expect(INBOX_WORDS).toEqual(words);
  });

  /// Scenario: Pressing Retry asks the daemon for that message at the revision the tile shows; when another client acted first, the row says so in the shared words.
  it("sends Retry at the item's revision and says when another client got there first", async () => {
    const onInboxAction = vi.fn(async () => ({ outcome: "stale" }));
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "uncertain", 9)] })).agents[0];
    const card = tile(agent, onInboxAction);
    fireEvent.click(within(card.getByTestId(`inbox-attention-${first}`)).getByRole("button", { name: words.retry }));
    await waitFor(() => expect(onInboxAction).toHaveBeenCalledWith(
      { deckId: agent.daemonId, agentId: "7", incarnation: "listed-inbox-process-a" },
      { paneId: "worker", messageId: first, revision: 9 },
      "retry",
    ));
    expect(await card.findByText(words.actionStale)).toBeVisible();
  });

  /// Scenario: Pressing Cancel asks the daemon to stop delivering that message and the row says it was cancelled.
  it("sends Cancel and reports the deck's answer", async () => {
    const onInboxAction = vi.fn(async () => ({ outcome: "applied" }));
    const agent = mapDesktopSnapshot(snapshot({ attention: [item(first, "uncertain")] })).agents[0];
    const card = tile(agent, onInboxAction);
    fireEvent.click(within(card.getByTestId(`inbox-attention-${first}`)).getByRole("button", { name: words.cancel }));
    await waitFor(() => expect(onInboxAction).toHaveBeenCalledWith(
      { deckId: agent.daemonId, agentId: "7", incarnation: "listed-inbox-process-a" },
      { paneId: "worker", messageId: first, revision: 4 },
      "cancel",
    ));
    expect(await card.findByText(words.cancelApplied)).toBeVisible();
  });

  /// Scenario: Nine deliveries on one card are uncertain. Each is listed with its own Retry and Cancel in one list that scrolls in place, and Cancel on the last one asks the daemon for that message at its revision, as the TUI's picker does for the row its cursor is on.
  it("lists every one of many deliveries, each with its own actions", async () => {
    const onInboxAction = vi.fn(async () => ({ outcome: "applied" }));
    const ids = Array.from({ length: 9 }, (_, n) => `m-k3xzq2ab-0f3c9a71d2e4b8${String(n).padStart(2, "0")}`);
    const agent = mapDesktopSnapshot(snapshot({
      attention: ids.map((id, n) => item(id, "uncertain", 10 + n)),
    })).agents[0];
    const card = tile(agent, onInboxAction);
    const list = card.getByTestId("inbox-attention-list-7");
    expect(list).toHaveClass("agent-inbox-attention");
    for (const id of ids) {
      const row = within(list).getByTestId(`inbox-attention-${id}`);
      expect(within(row).getByRole("button", { name: words.retry })).toBeInTheDocument();
      expect(within(row).getByRole("button", { name: words.cancel })).toBeInTheDocument();
    }
    fireEvent.click(within(card.getByTestId(`inbox-attention-${ids[8]}`)).getByRole("button", { name: words.cancel }));
    await waitFor(() => expect(onInboxAction).toHaveBeenCalledWith(
      { deckId: agent.daemonId, agentId: "7", incarnation: "listed-inbox-process-a" },
      { paneId: "worker", messageId: ids[8], revision: 18 },
      "cancel",
    ));
  });

  /// Scenario: The agent's integration is out of date, its CLI too old, or it has not connected yet; the tile shows that notice in the words the TUI uses.
  it.each([
    ["adapter_too_old", words.adapterTooOld],
    ["cli_too_old", words.cliTooOld],
    ["not_armed", words.notArmed],
  ])("shows the %s integration notice", (code, text) => {
    const card = tile(mapDesktopSnapshot(snapshot({ integration: code })).agents[0]);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(text);
  });

  /// Scenario: The agent's deck hooks run a dot-agent-deck too old for its messages; the tile names the command the deck gives to update them, or says to upgrade when it gives none, in the words the TUI uses.
  it("shows the hooks_too_old notice with its fix", () => {
    const command = "/opt/deck/bin/dot-agent-deck hooks install --agent claude-code";
    let card = tile(mapDesktopSnapshot(snapshot({ integration: "hooks_too_old", integrationCommand: command })).agents[0]);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(`${words.hooksTooOld}: run ${command}, then restart the agent`);
    document.body.innerHTML = "";
    card = tile(mapDesktopSnapshot(snapshot({ integration: "hooks_too_old" })).agents[0]);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(`${words.hooksTooOld}: ${words.hooksTooOldUpgrade}`);
  });

  /// Scenario: Hand-outs are paused for room in the evidence store, or receipts cannot be recorded; the tile says so in the words the TUI uses.
  it("shows paused hand-outs and failed receipts", () => {
    let card = tile(mapDesktopSnapshot(snapshot({ evidencePaused: 3 })).agents[0]);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(words.evidencePaused.replace("{n}", "3"));
    document.body.innerHTML = "";
    card = tile(mapDesktopSnapshot(snapshot({ receiptsDegraded: "disk full" })).agents[0]);
    expect(card.getByTestId("inbox-notice-7")).toHaveTextContent(`${words.receiptsDegraded} (disk full)`);
  });
});
