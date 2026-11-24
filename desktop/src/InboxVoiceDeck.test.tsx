import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures } from "./lib/bridge";
import { INBOX_WORDS } from "./lib/inboxWords";
import { dispatchVoiceAction, type VoiceDispatchContext, type VoiceScreenContext } from "./lib/voiceActions";
import type { DeckRuntimeState } from "./types";

vi.mock("./components/TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

import { DeckSurface } from "./App";
import type { DesktopSettingsState } from "./hooks/useDesktopSettings";

/* Issue #2148 — Retry and Cancel on an agent's deck messages, by voice, on the
   Daemons screen whose tiles list them. */

function deckScreen(inboxAction = vi.fn(async () => ({ outcome: "applied" }))) {
  const snapshot = createFixtureSnapshot("connected");
  const [first, ...rest] = snapshot.agents;
  const agent = {
    ...first,
    paneId: "worker",
    incarnation: "process-a",
    inbox: { ...(first.inbox ?? {}), attention: [{ messageId: "m-7", revision: 5, kind: "uncertain" as const, fromLabel: "the orchestrator", subject: "review the parser" }] },
  } as typeof first;
  const runtime = {
    mode: "live",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    inboxAction,
    snapshot: { ...snapshot, agents: [agent, ...rest] },
    fleet: [{ ...snapshot, agents: [agent, ...rest] }],
  } as unknown as DeckRuntimeState;
  const settings: DesktopSettingsState = { settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), loaded: true, chosen: false, save: () => undefined };
  const channel: { current: VoiceScreenContext | undefined } = { current: undefined };
  render(<DeckSurface runtime={runtime} settings={settings} voiceChannel={channel} />);
  const reportRefused = vi.fn();
  const say = (invoke: string) => act(() => {
    const dispatched = dispatchVoiceAction(invoke, { ...channel.current, reportRefused } as unknown as VoiceDispatchContext, { deckId: agent.daemonId, agentId: agent.id, from: "deck", incarnation: agent.incarnation });
    expect(dispatched).toBe(true);
  });
  return { agent, inboxAction, reportRefused, say };
}

describe("deck messages by voice on the Daemons screen (issue #2148)", () => {
  /// Scenario: On the Daemons screen an agent's tile lists an uncertain deck message; the user says "retry the message to" that agent, a confirmation names the message's sender and subject, nothing is sent until its Retry is pressed, and pressing it retries that message at the revision shown.
  it("a spoken Retry opens a confirmation, and only its button retries the message", async () => {
    const deck = deckScreen();
    deck.say("confirmRetryInboxMessage");
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveTextContent("Retry the deck message to");
    expect(dialog).toHaveTextContent("from the orchestrator: “review the parser”");
    expect(deck.inboxAction).not.toHaveBeenCalled();
    expect(deck.reportRefused).not.toHaveBeenCalled();

    fireEvent.click(within(dialog).getByRole("button", { name: INBOX_WORDS.retry }));
    await waitFor(() => expect(deck.inboxAction).toHaveBeenCalledWith(
      { deckId: deck.agent.daemonId, agentId: deck.agent.id, incarnation: "process-a" },
      { paneId: "worker", messageId: "m-7", revision: 5 },
      "retry",
    ));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument());
    expect(screen.getByText(new RegExp(INBOX_WORDS.retryApplied))).toBeInTheDocument();
  });

  /// Scenario: The user says "cancel the message to" the agent; a destructive confirmation names the message, and pressing Keep it closes it with nothing cancelled.
  it("a spoken Cancel opens a confirmation that Keep it dismisses without cancelling", async () => {
    const deck = deckScreen();
    deck.say("confirmCancelInboxMessage");
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveTextContent("Cancel the deck message to");
    expect(within(dialog).getByRole("button", { name: "Cancel message" })).toBeVisible();
    fireEvent.click(within(dialog).getByRole("button", { name: "Keep it" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument());
    expect(deck.inboxAction).not.toHaveBeenCalled();
  });

  /// Scenario: A spoken Retry is asked for while its confirmation is still open; the second is refused in words rather than replacing the first.
  it("refuses a second spoken Retry while the first confirmation is open", () => {
    const deck = deckScreen();
    deck.say("confirmRetryInboxMessage");
    deck.say("confirmCancelInboxMessage");
    expect(deck.reportRefused).toHaveBeenCalledWith(expect.stringContaining("A confirmation is already open"));
    expect(screen.getAllByRole("alertdialog")).toHaveLength(1);
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Retry the deck message to");
  });
});
