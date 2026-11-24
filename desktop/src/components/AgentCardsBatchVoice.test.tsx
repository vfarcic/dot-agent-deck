import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import { ANOTHER_BATCH_SENDING, BATCH_STILL_SENDING } from "../hooks/useAgentCardsView";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResolvedParamDto, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { AgentCardAction, AgentCardEntry, CardBatchOutcome, DeckRuntimeState } from "../types";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

/*
 * Issue #2399 review — the batch by voice, through the real shell, voice
 * panel and dispatch seam, with only the microphone and the resolved answer
 * synthetic (the `HostMetricsVoice.test.tsx` pattern, so one test can say
 * several things in a row):
 *
 * - 035: while a batch is being sent, voice's `close` (the `closeTopmost` →
 *   overlay-stack route that "cancel" and "never mind" take), `Escape` and
 *   Cancel all leave it open and say why, and a spoken new batch is refused;
 *   the results still arrive and are shown.
 * - 036: a selection voice builds one named card at a time — two unrelated
 *   roots, which no single subtree reaches — and a batch voice starts on it,
 *   which only opens the confirmation.
 */

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <div data-testid={`terminal-${agentId}`} />,
}));

const INSTANCE = "daemon-instance-batch-voice";
const MODE_KEY = "dot-agent-deck.overview.mode";
const commandsSource = readFileSync(resolve("src-tauri/src/voice/commands.toml"), "utf8");

function microphone() {
  const queue: string[] = [];
  let recording = false;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({
    state, capturedMs: 900, maxMs: 30_000, capped: false, speech: queue.length > 0,
    available: true, backend: "remote",
  });
  return {
    deliver: (text: string) => queue.push(text),
    voiceStart: vi.fn(async () => { recording = true; return status("recording"); }),
    voiceStatus: vi.fn(async () => status(recording && queue.length ? "done" : "recording")),
    voiceStop: vi.fn(async (): Promise<VoiceTranscriptionDto> => {
      recording = false;
      const transcript = queue.shift() ?? "";
      return { outcome: { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` }, transcribeMs: 11, backend: "stub", audioMs: 900 };
    }),
    voiceCancel: vi.fn(async () => { recording = false; return status("idle"); }),
  };
}

const goAhead: AgentCardAction = {
  id: "go-ahead", label: "Go ahead", kind: "prompt", prompt: "Go ahead.", prompt_source: "catalog", text_input: false, confirm: false,
  confirm_detail: null, icon: null, placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: "tok-go",
};

function cardFor(agentId: string, name: string, overrides: Partial<AgentCardEntry> = {}): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`, instance_id: INSTANCE, pane_id: `pane-${agentId}`, agent_id: agentId, agent_type: "claude_code", name,
    status: "idle", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: null, fields: [], questions: [], actions: [goAhead], primary_action_id: "go-ahead",
    facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-23T00:00:00Z", ...overrides,
  };
}

const agentParam = (agentId: string, label: string): VoiceResolvedParamDto => ({ name: "agent", kind: "agent_ref", spoken: label, value: agentId, label });
const actionParam = (label: string): VoiceResolvedParamDto => ({ name: "action", kind: "card_action", spoken: label.toLowerCase(), value: label, label });

type Said = { action: string; invoke: string; params: VoiceResolvedParamDto[] };

/** The real shell on the dashboard's cards, one daemon whose cards are `entries`, and what each utterance resolves to. */
function setup(entries: (agents: { id: string }[]) => AgentCardEntry[], said: (agents: { id: string }[]) => Record<string, Said>, { holdExecution = false, extra = {} }: { holdExecution?: boolean; extra?: Record<string, unknown> } = {}) {
  const voice = microphone();
  const base = createFixtureSnapshot("connected");
  const snapshot = { ...base, connection: { ...base.connection, incarnation: INSTANCE }, agents: base.agents.map((agent) => ({ ...agent, incarnation: INSTANCE })) };
  const cards = entries(snapshot.agents);
  const table = said(snapshot.agents);
  let release: (() => void) | undefined;
  const invokeCardAction = vi.fn();
  const invokeCardActionBatch = vi.fn(async (_deckId: string, _actionId: string, options: { mode: string; requestId: string; entries: { pane_id: string; agent_id: string }[] }) => {
    const nameOf = (agentId: string) => cards.find((card) => card.agent_id === agentId)?.name;
    if (options.mode === "execute" && holdExecution) await new Promise<void>((done) => { release = done; });
    return {
      request_id: options.requestId,
      entries: options.entries.map((target): CardBatchOutcome => options.mode === "preview"
        ? { ...target, name: nameOf(target.agent_id), outcome: "included", prompt: "Go ahead.", prompt_source: "catalog", action_token: "tok-go", instance_id: INSTANCE }
        : { ...target, outcome: "delivered", message_id: `m-${target.agent_id}` }),
    };
  });
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => {
    const row = transcript === "never mind"
      ? { action: "close", invoke: "closeTopmost", params: [] }
      : table[transcript];
    if (!row) return { backend: "stub", resolveMs: 21, outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` } };
    return { backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, ...row, sentence: "Done." } };
  });
  const runtime = {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures("?experimental=1"), snapshot, fleet: [snapshot],
    terminalData: {}, clearError: vi.fn(), runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined), reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: undefined })),
    saveSettings: vi.fn(async () => structuredClone(DEFAULT_DESKTOP_SETTINGS)),
    setZoom: vi.fn(async (level: number) => level),
    voiceCommands: vi.fn(async () => []),
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries: cards, catalogErrors: [], tree: true, batch: true, catalogEdit: true } },
    invokeCardAction, invokeCardActionBatch, resolveVoice, ...voice, ...extra,
  } as unknown as DeckRuntimeState;
  render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
  return { voice, cards, invokeCardAction, invokeCardActionBatch, release: () => release?.() };
}

async function turnOnVoice() {
  await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
  expect(screen.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "true");
}

async function speak(voice: ReturnType<typeof microphone>, words: string) {
  voice.deliver(words);
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

function cardNode(card: AgentCardEntry) {
  const node = document.querySelector<HTMLElement>(`article[data-card-key="${card.key}"]`);
  if (!node) throw new Error(`No card ${card.name}`);
  return within(node);
}

const previewed = (calls: ReturnType<typeof vi.fn>) => calls.mock.calls.filter(([, , options]) => (options as { mode: string }).mode === "preview");
const executed = (calls: ReturnType<typeof vi.fn>) => calls.mock.calls.filter(([, , options]) => (options as { mode: string }).mode === "execute");

describe("batch card actions by voice (issue 2399 review)", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/?fixture=1&experimental=1");
    window.localStorage.clear();
    window.sessionStorage.clear();
    window.localStorage.setItem(MODE_KEY, "cards");
    act(() => setDashboardFilter(clearDashboardFilter()));
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    window.localStorage.clear();
  });

  /// Scenario: Start a batch from a parent card's overlay and press Send while the daemon holds the execution. Saying "never mind", pressing Escape and Cancel all leave the dialog open with a sentence saying it is still sending, and a spoken new batch is refused; once the daemon answers, the result for each agent is shown and Escape then closes it.
  it("desktop/agent-cards/035: a batch being sent cannot be dismissed or replaced, by voice, Escape or Cancel", async () => {
    const run = setup(
      (agents) => {
        const parent = cardFor(agents[0].id, "parent-card", { rollup: { total: 1, worst: "idle", counts: { idle: 1 }, needs_you: 0 } });
        return [parent, cardFor(agents[1].id, "child-card", { parent: parent.key })];
      },
      (agents) => ({ "go ahead on everything under the parent": { action: "run_card_action_batch", invoke: "runCardActionBatch", params: [agentParam(agents[0].id, "parent-card"), actionParam("Go ahead")] } }),
      { holdExecution: true },
    );
    const [parent] = run.cards;
    await turnOnVoice();
    fireEvent.click(cardNode(parent).getByTestId("agent-card-details"));
    // PRD #2374 decision 2026-11-24 (3): the overlay's batch buttons are icons, found by the action's label.
    await act(async () => { fireEvent.click(within(screen.getByTestId("agent-card-batch")).getByRole("button", { name: "Go ahead" })); });
    const dialog = screen.getByTestId("agent-cards-batch-confirm");
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    expect(dialog).toHaveAttribute("data-stage", "sending");
    expect(executed(run.invokeCardActionBatch)).toHaveLength(1);

    // Voice's close: "never mind" resolves to `close`, which reaches the dialog through the overlay stack.
    await speak(run.voice, "never mind");
    expect(screen.getByTestId("agent-cards-batch-confirm")).toHaveAttribute("data-stage", "sending");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(BATCH_STILL_SENDING);
    expect(screen.getByTestId("agent-cards-batch-refusal")).toHaveTextContent(BATCH_STILL_SENDING);

    // Escape, which closes the dialog otherwise, is refused the same way.
    fireEvent.keyDown(screen.getByTestId("agent-cards-batch-confirm"), { key: "Escape" });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toHaveAttribute("data-stage", "sending");

    // Cancel cannot be pressed.
    expect(within(screen.getByTestId("agent-cards-batch-confirm")).getByRole("button", { name: /^Cancel$/ })).toBeDisabled();

    // A new batch by voice is refused, and no preview replaces the one being sent.
    await speak(run.voice, "go ahead on everything under the parent");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(ANOTHER_BATCH_SENDING);
    expect(previewed(run.invokeCardActionBatch)).toHaveLength(1);
    expect(screen.getByTestId("agent-cards-batch-confirm")).toHaveAttribute("data-stage", "sending");

    // The daemon answers: the result for each agent is shown, not lost.
    await act(async () => { run.release(); await Promise.resolve(); await Promise.resolve(); });
    const results = screen.getByTestId("agent-cards-batch-results");
    expect(within(results).getAllByTestId("agent-cards-batch-entry").map((row) => row.getAttribute("data-outcome"))).toEqual(["delivered", "delivered"]);
    expect(screen.queryByTestId("agent-cards-batch-refusal")).toBeNull();
    fireEvent.keyDown(results, { key: "Escape" });
    expect(screen.queryByTestId("agent-cards-batch-results")).toBeNull();
    expect(run.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: With three unrelated cards on the dashboard, say to select the first and the third card (the first twice), then to send Go ahead to the selection. The preview names exactly those two cards and the confirmation opens without sending; unselecting the third and asking again previews the first alone.
  it("desktop/agent-cards/036: voice builds a selection by name and starts a batch on it, which only opens the confirmation", async () => {
    const run = setup(
      (agents) => [cardFor(agents[0].id, "first-card"), cardFor(agents[1].id, "second-card"), cardFor(agents[2].id, "third-card")],
      (agents) => ({
        "select the first card": { action: "select_agent_card", invoke: "selectAgentCard", params: [agentParam(agents[0].id, "first-card")] },
        "select the third card": { action: "select_agent_card", invoke: "selectAgentCard", params: [agentParam(agents[2].id, "third-card")] },
        "unselect the third card": { action: "unselect_agent_card", invoke: "unselectAgentCard", params: [agentParam(agents[2].id, "third-card")] },
        "go ahead on the selected agents": { action: "run_card_action_on_selection", invoke: "runSelectionBatch", params: [actionParam("Go ahead")] },
      }),
    );
    const [first, second, third] = run.cards;
    await turnOnVoice();
    await speak(run.voice, "select the first card");
    await speak(run.voice, "select the third card");
    // Said twice, a selection stays: voice says which it wants rather than flipping a checkbox.
    await speak(run.voice, "select the first card");
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("2 selected");
    expect(cardNode(first).getByRole("checkbox", { name: "Select first-card" })).toBeChecked();
    expect(cardNode(second).getByRole("checkbox", { name: "Select second-card" })).not.toBeChecked();
    expect(cardNode(third).getByRole("checkbox", { name: "Select third-card" })).toBeChecked();

    await speak(run.voice, "go ahead on the selected agents");
    const dialog = screen.getByTestId("agent-cards-batch-confirm");
    expect(previewed(run.invokeCardActionBatch)).toHaveLength(1);
    expect(run.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "go-ahead", expect.objectContaining({
      mode: "preview", entries: [first, third].map(({ pane_id, agent_id }) => ({ pane_id, agent_id })),
    }));
    expect(executed(run.invokeCardActionBatch)).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole("button", { name: /^Cancel$/ }));

    await speak(run.voice, "unselect the third card");
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("1 selected");
    await speak(run.voice, "go ahead on the selected agents");
    expect(run.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "go-ahead", expect.objectContaining({
      mode: "preview", entries: [{ pane_id: first.pane_id, agent_id: first.agent_id }],
    }));
    expect(executed(run.invokeCardActionBatch)).toHaveLength(0);
    expect(run.invokeCardAction).not.toHaveBeenCalled();

    // The registry and the command table agree on the three rows; Send stays off voice.
    const registry = VOICE_ACTIONS as unknown as Record<string, { voice?: boolean; no_voice?: string }>;
    for (const [id, row] of [["selectAgentCard", "select_agent_card"], ["unselectAgentCard", "unselect_agent_card"], ["runSelectionBatch", "run_card_action_on_selection"]]) {
      expect(registry[id]?.voice, id).toBe(true);
      expect(commandsSource).toMatch(new RegExp(`id\\s*=\\s*"${row}"[\\s\\S]*?invoke\\s*=\\s*"${id}"`));
    }
    expect(registry.sendCardBatch?.no_voice).toBeTruthy();
  });

  /// Scenario: By voice, open the card actions editor, open the form for a new action, open Ship it for editing, and ask to remove Ship it. The editor opens on the local daemon's catalog, each request opens what it names, and the removal only opens the editor's own confirmation: nothing is written until it is pressed by hand. The registry entries behind each are voice-enabled with matching command rows, and the confirmation's press stays off voice.
  it("desktop/agent-cards/060: voice opens the card actions editor, its forms and a removal's confirmation, and writes nothing", async () => {
    const ship = { name: "ship", label: "Ship it", description: "Offer when it is ready.", prompt: "Ship it.", icon: "rocket", placement: "card", confirm: false, always: false, source: "user", default: false, overridden: false, hidden: false };
    const goDefault = { ...ship, name: "go-ahead", label: "Go ahead", prompt: "Go ahead.", icon: "play", source: "default", default: true };
    const readCardCatalog = vi.fn(async () => ({ actions: [goDefault, ship], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const editCardCatalog = vi.fn();
    const run = setup(
      (agents) => [cardFor(agents[0].id, "coder-card")],
      () => ({
        "open the card actions editor": { action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [] },
        "add a card action": { action: "add_card_action", invoke: "addCardAction", params: [] },
        "edit the ship it action": { action: "edit_card_action", invoke: "editCardAction", params: [actionParam("Ship it")] },
        "remove the ship it action": { action: "remove_card_action", invoke: "removeCardAction", params: [actionParam("Ship it")] },
      }),
      { extra: { readCardCatalog, editCardCatalog } },
    );
    await turnOnVoice();
    await speak(run.voice, "open the card actions editor");
    const editor = screen.getByRole("dialog", { name: "Card actions" });
    expect(readCardCatalog).toHaveBeenCalledWith(FIXTURE_DAEMON_ID);

    await speak(run.voice, "add a card action");
    expect((within(editor).getByLabelText("Name") as HTMLInputElement).value).toBe("");
    expect(within(editor).getByLabelText("Text it sends")).toBeInTheDocument();

    await speak(run.voice, "edit the ship it action");
    expect((within(editor).getByLabelText("Name") as HTMLInputElement).value).toBe("ship");
    expect((within(editor).getByLabelText("Label") as HTMLInputElement).value).toBe("Ship it");

    await speak(run.voice, "remove the ship it action");
    expect(screen.getByRole("alertdialog", { name: "Remove Ship it?" })).toBeInTheDocument();
    expect(editCardCatalog).not.toHaveBeenCalled();
    expect(run.invokeCardAction).not.toHaveBeenCalled();

    const registry = VOICE_ACTIONS as unknown as Record<string, { voice?: boolean; no_voice?: string }>;
    for (const [id, row] of [["openCardActionsEditor", "open_card_actions_editor"], ["addCardAction", "add_card_action"], ["editCardAction", "edit_card_action"], ["removeCardAction", "remove_card_action"]]) {
      expect(registry[id]?.voice, id).toBe(true);
      expect(commandsSource).toMatch(new RegExp(`id\\s*=\\s*"${row}"[\\s\\S]*?invoke\\s*=\\s*"${id}"`));
    }
    // The press that writes a removal is the user's hand; saving the form is either a voice row or says why it is not.
    expect(registry.confirmRemoveCardAction?.no_voice).toBeTruthy();
    const save = registry.saveCardAction;
    expect(save, "saveCardAction is in the registry").toBeDefined();
    if (save?.voice) expect(commandsSource).toMatch(/invoke\s*=\s*"saveCardAction"/);
    else expect(save?.no_voice).toBeTruthy();
  });
});
