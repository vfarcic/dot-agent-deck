import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureFleet, createFixtureSnapshot, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import { deckName } from "../lib/displayText";
import { ANOTHER_BATCH_SENDING, BATCH_STILL_SENDING } from "../hooks/useAgentCardsView";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResolvedParamDto, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import { CARD_ACTION_FORM_OPENED, CARD_ACTION_REMOVAL_ASKED, VOICE_ACTIONS } from "../lib/voiceActions";
import type { AgentCardAction, AgentCardEntry, CardBatchOutcome, DeckRuntimeState, DeckView } from "../types";
import { VOICE_CHAIN_SETTLE_MS, VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";
import { agentKey } from "./AgentOverview";

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
const REMOTE_INSTANCE = "daemon-instance-batch-voice-remote";
const MODE_KEY = "dot-agent-deck.overview.mode";
const commandsSource = readFileSync(resolve("src-tauri/src/voice/commands.toml"), "utf8");

type Row = { id: string; invoke: string; screens: string[]; params: { name: string; kind: string }[] };

/** The `[[commands]]` rows of the table, one at a time, with the keys these tests read (as `AgentCardsVoice.test.tsx` parses them). */
function commandRows(): Row[] {
  return commandsSource.split(/^\[\[commands\]\]\s*$/m).slice(1).map((chunk) => {
    const value = (key: string) => chunk.match(new RegExp(`^${key}\\s*=\\s*"([^"]*)"`, "m"))?.[1] ?? "";
    const screens = chunk.match(/^screens\s*=\s*\[([^\]]*)\]/m)?.[1] ?? "";
    const params = [...chunk.matchAll(/\[\[commands\.params\]\]\s*\n\s*name\s*=\s*"([^"]+)"\s*\n\s*kind\s*=\s*"([^"]+)"/g)].map((found) => ({ name: found[1], kind: found[2] }));
    return { id: value("id"), invoke: value("invoke"), screens: [...screens.matchAll(/"([^"]+)"/g)].map((found) => found[1]), params };
  });
}

/** The one row with `id`, which must invoke `invoke`, on `screens`, with exactly `params`. */
function expectRow(id: string, invoke: string, screens: string[], params: { name: string; kind: string }[]) {
  const found = commandRows().filter((row) => row.id === id);
  expect(found, `one row ${id}`).toHaveLength(1);
  expect(found[0].invoke, `${id} invokes`).toBe(invoke);
  expect(found[0].screens, `${id} screens`).toEqual(screens);
  expect(found[0].params, `${id} params`).toEqual(params);
}

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

/**
 * The real shell on the dashboard's cards, one daemon whose cards are `entries`, and what each utterance resolves to.
 * With `gateScreens`, the stand-in resolver refuses a row that `commands.toml` does not make callable on the screen the
 * voice panel last DECLARED, as the crate does (`CommandRow::callable_on`), so a test reads the real declaration and the
 * real table rather than a dispatch it was handed. `initialView` opens the shell somewhere other than the dashboard.
 */
function setup(entries: (agents: { id: string }[]) => AgentCardEntry[], said: (agents: { id: string }[]) => Record<string, Said>, { holdExecution = false, extra = {}, catalogEdit = true, remote = false, gateScreens = false, initialView = { kind: "overview" } }: { holdExecution?: boolean; extra?: Record<string, unknown>; catalogEdit?: boolean; remote?: boolean; gateScreens?: boolean; initialView?: DeckView } = {}) {
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
  const declareVoiceScreen = vi.fn();
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => {
    const row = transcript === "never mind"
      ? { action: "close", invoke: "closeTopmost", params: [] }
      : table[transcript];
    if (!row) return { backend: "stub", resolveMs: 21, outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` } };
    if (gateScreens) {
      const declared = declareVoiceScreen.mock.calls.at(-1)?.[0] as string | undefined;
      const listed = commandRows().find((candidate) => candidate.id === row.action);
      if (!declared || (listed && listed.screens.length > 0 && !listed.screens.includes(declared))) {
        return { backend: "stub", resolveMs: 21, outcome: { kind: "unavailable", transcript, action: row.action, hint: "not on this screen", sentence: `Not available on the ${declared ?? "undeclared"} screen.` } };
      }
    }
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
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries: cards, catalogErrors: [], tree: true, batch: true, catalogEdit } },
    invokeCardAction, invokeCardActionBatch, resolveVoice, ...(gateScreens ? { declareVoiceScreen } : {}), ...voice, ...extra,
  } as unknown as DeckRuntimeState;
  /* PR #2491 decision 2026-11-25 — a second daemon with cards of its own, for the questions only several daemons raise. */
  if (remote) {
    const far = createFixtureFleet("fleet")[1];
    const farDeck = { ...far, connection: { ...far.connection, incarnation: REMOTE_INSTANCE }, agents: [{ ...createFixtureStartedAgent({ id: "1", daemonId: FIXTURE_REMOTE_DAEMON_ID, displayName: "far-coder", command: "claude", cwd: "/w" }), incarnation: REMOTE_INSTANCE }] };
    runtime.fleet = [snapshot, farDeck] as DeckRuntimeState["fleet"];
    runtime.agentCards = { ...runtime.agentCards, [FIXTURE_REMOTE_DAEMON_ID]: { deckId: FIXTURE_REMOTE_DAEMON_ID, instanceId: REMOTE_INSTANCE, revision: 1, entries: [{ ...cardFor("1", "far-coder"), instance_id: REMOTE_INSTANCE }], catalogErrors: [], tree: true, batch: true, catalogEdit } } as DeckRuntimeState["agentCards"];
  }
  const { rerender } = render(<DeckShell runtime={runtime} initialView={initialView} />);
  /* The daemon's next list of cards, as a new runtime state. */
  const setCards = (next: AgentCardEntry[]) => {
    const agentCards = { [FIXTURE_DAEMON_ID]: { ...runtime.agentCards![FIXTURE_DAEMON_ID], revision: 2, entries: next } };
    rerender(<DeckShell runtime={{ ...runtime, agentCards } as DeckRuntimeState} initialView={{ kind: "overview" }} />);
  };
  return { voice, cards, invokeCardAction, invokeCardActionBatch, declareVoiceScreen, setCards, release: () => release?.() };
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

  /// Scenario: Tick a parent card, which selects it and the card under it, start a Go ahead batch from the selection bar and press Send while the daemon holds the execution. Saying "never mind", pressing Escape and Cancel all leave the dialog open with a sentence saying it is still sending, and a spoken new batch is refused; once the daemon answers, the result for each agent is shown and Escape then closes it.
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
    // PR #2491 decision 2026-11-25: a closed card's checkbox selects everything under it, and the selection bar acts on them.
    fireEvent.click(cardNode(parent).getByRole("checkbox", { name: "Select parent-card" }));
    await act(async () => { fireEvent.click(within(screen.getByTestId("agent-cards-selection-bar")).getByRole("button", { name: "Go ahead" })); });
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
      const found = commandRows().filter((candidate) => candidate.id === row);
      expect(found, `one row ${row}`).toHaveLength(1);
      expect(found[0].invoke, row).toBe(id);
    }
    expect(registry.sendCardBatch?.no_voice).toBeTruthy();
  });

  /// Scenario: With three cards on the dashboard, say to select two of them, then "clear the selection". Both checkboxes are cleared and the selection bar goes away, with nothing sent; saying it again with nothing selected says so in words. The registry's clear entry is voice-enabled with one command row on the dashboard, and the old switch for showing the checkboxes is gone.
  it("desktop/agent-cards/086: voice clears the selection, and there is no selection switch left to say", async () => {
    const run = setup(
      (agents) => [cardFor(agents[0].id, "first-card"), cardFor(agents[1].id, "second-card"), cardFor(agents[2].id, "third-card")],
      (agents) => ({
        "select the first card": { action: "select_agent_card", invoke: "selectAgentCard", params: [agentParam(agents[0].id, "first-card")] },
        "select the third card": { action: "select_agent_card", invoke: "selectAgentCard", params: [agentParam(agents[2].id, "third-card")] },
        "clear the selection": { action: "clear_card_selection", invoke: "clearCardSelection", params: [] },
      }),
    );
    const [first, , third] = run.cards;
    await turnOnVoice();
    await speak(run.voice, "select the first card");
    await speak(run.voice, "select the third card");
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("2 selected");
    await speak(run.voice, "clear the selection");
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
    expect(cardNode(first).getByRole("checkbox", { name: "Select first-card" })).not.toBeChecked();
    expect(cardNode(third).getByRole("checkbox", { name: "Select third-card" })).not.toBeChecked();
    await speak(run.voice, "clear the selection");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("No card is selected.");
    expect(run.invokeCardActionBatch).not.toHaveBeenCalled();
    expect(run.invokeCardAction).not.toHaveBeenCalled();

    const registry = VOICE_ACTIONS as unknown as Record<string, { voice?: boolean; no_voice?: string } | undefined>;
    expect(registry.clearCardSelection?.voice).toBe(true);
    expectRow("clear_card_selection", "clearCardSelection", ["overview"], []);
    expect(registry.toggleCardSelecting, "no Select switch to say").toBeUndefined();
    expect(commandRows().some((row) => row.invoke === "toggleCardSelecting")).toBe(false);
  });

  /// Scenario: By voice, select the first card and then the third, put keyboard focus on the selection bar's Clear, and say "clear the selection". The bar goes, and focus is on the third card's checkbox, the card last selected, rather than left on nothing. Select the first card again, focus Clear, take that card off the daemon's list, and the bar goes with focus on the dashboard's scroll region.
  it("desktop/agent-cards/090: the bar going while it has focus hands focus to the last ticked card, or else the dashboard", async () => {
    const run = setup(
      (agents) => [cardFor(agents[0].id, "first-card"), cardFor(agents[1].id, "second-card"), cardFor(agents[2].id, "third-card")],
      (agents) => ({
        "select the first card": { action: "select_agent_card", invoke: "selectAgentCard", params: [agentParam(agents[0].id, "first-card")] },
        "select the third card": { action: "select_agent_card", invoke: "selectAgentCard", params: [agentParam(agents[2].id, "third-card")] },
        "clear the selection": { action: "clear_card_selection", invoke: "clearCardSelection", params: [] },
      }),
    );
    const [first, second, third] = run.cards;
    await turnOnVoice();
    await speak(run.voice, "select the first card");
    await speak(run.voice, "select the third card");
    act(() => { screen.getByTestId("agent-cards-selection-clear").focus(); });
    expect(document.activeElement).toBe(screen.getByTestId("agent-cards-selection-clear"));
    await speak(run.voice, "clear the selection");
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
    expect(document.activeElement, "focus is on the last selected card's checkbox").toBe(cardNode(third).getByRole("checkbox", { name: "Select third-card" }));

    // The last ticked card gone with the selection: focus goes to the dashboard's scroll region.
    await speak(run.voice, "select the first card");
    act(() => { screen.getByTestId("agent-cards-selection-clear").focus(); });
    await act(async () => { run.setCards([second, third]); });
    expect(document.querySelector(`article[data-card-key="${first.key}"]`)).toBeNull();
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
    expect(document.activeElement, "focus is on the dashboard").toBe(document.querySelector(".overview-body"));
  });

  /// Scenario: By voice, open the card actions editor, open the form for a new action, open Ship it for editing, and ask to remove Ship it. The editor opens on the local daemon's catalog, each request opens what it names, and the removal only opens the editor's own confirmation: nothing is written until it is pressed by hand. The registry entries behind each are voice-enabled with one matching command row each, parsed row by row with its screen and parameter, Ship it resolves from the editor although no card offers it, and every other control of the editor — Save, the removal's Remove, Hide, Restore, choosing the daemon, choosing an icon — is either a row or carries a written reason it is not.
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
    const editor = screen.getByRole("dialog", { name: /^Card actions: / });
    expect(readCardCatalog).toHaveBeenCalledWith(FIXTURE_DAEMON_ID, INSTANCE);

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

    // Ship it is in the editor's catalog but offered on no card: the spoken edit and removal resolve it from the editor's list.
    expect(screen.queryByRole("button", { name: "Ship it" })).toBeNull();

    // The registry and the command table agree, row by row: each voice entry has one row invoking it, on the dashboard and over an
    // agent's pane (where the Card tab's Edit opens the editor, PR #2491 review T1), with its parameters.
    const registry = VOICE_ACTIONS as unknown as Record<string, { voice?: boolean; no_voice?: string }>;
    const rows: [string, string, { name: string; kind: string }[]][] = [
      ["openCardActionsEditor", "open_card_actions_editor", [{ name: "deck", kind: "deck_ref" }]],
      ["addCardAction", "add_card_action", []],
      ["editCardAction", "edit_card_action", [{ name: "action", kind: "card_action" }]],
      ["removeCardAction", "remove_card_action", [{ name: "action", kind: "card_action" }]],
    ];
    for (const [id, row, params] of rows) {
      expect(registry[id]?.voice, id).toBe(true);
      expectRow(row, id, ["overview", "agent"], params);
    }
    // Every other control of the editor is reachable by voice through a row of its own, or says, in a reason someone can
    // disagree with, why it is not (CLAUDE.md rule 23): the presses that write the daemon's config, choosing an icon, and
    // closing the form or the editor. The editor's daemon is named when it opens (PR #2491 decision 2026-11-25), so the
    // Daemon picker's entry is gone.
    expect((VOICE_ACTIONS as unknown as Record<string, unknown>).chooseCardActionsDaemon, "the Daemon picker's entry is retired").toBeUndefined();
    expect(commandRows().some((candidate) => candidate.id === "choose_card_actions_daemon"), "and so is its row").toBe(false);
    for (const id of ["confirmRemoveCardAction", "saveCardAction", "hideCardAction", "restoreCardAction", "chooseCardActionIcon", "showCardActionOnCard", "showCardActionOnlyInCardTab", "askBeforeSendingCardAction", "sendCardActionWithoutAsking", "offerCardActionOnEveryCard", "offerCardActionWhenAgentsChoose", "refreshCardActions", "cancelCardActionForm", "closeCardActionsEditor"]) {
      const entry = registry[id];
      expect(entry, `${id} is in the registry`).toBeDefined();
      if (entry?.voice) {
        const found = commandRows().filter((candidate) => candidate.invoke === id);
        expect(found, `${id} has one row`).toHaveLength(1);
        expect(found[0].screens, id).toEqual(["overview", "agent"]);
      } else {
        expect(typeof entry?.no_voice, `${id} says why it is not on voice`).toBe("string");
        expect(entry!.no_voice!.length, `${id}'s reason is a reason, not a placeholder`).toBeGreaterThan(60);
      }
    }
    // The presses that write a removal or the form's text stay the user's own.
    expect(registry.confirmRemoveCardAction?.voice).not.toBe(true);
    expect(registry.saveCardAction?.voice).not.toBe(true);
  });

  /// Scenario: Open the editor and Ship it's form by voice, then say an icon's name ("icon git merge"), "ask before sending", "offer on every card", "only in the Card tab", their opposites, "refresh the card actions" and "open card actions" naming a daemon. The form's icon becomes git-merge and an unknown icon name is refused in words; each checkbox and the Placement choice follow what was said, whatever they were before; Refresh reads the list again and keeps the form; naming the daemon the editor is already on keeps it as it is, and a daemon not on the dashboard is refused in words. Nothing is written, and each of these entries is a voice entry with exactly one row (issue #2399 review 12, re-review N4; PR #2491 decision 2026-11-25).
  it("desktop/agent-cards/075: voice names the editor's daemon, reads it again and sets the form's icon and checkboxes, writing nothing", async () => {
    const ship = { name: "ship", label: "Ship it", description: "Offer when it is ready.", prompt: "Ship it.", icon: "rocket", placement: "card", confirm: false, always: false, source: "user", default: false, overridden: false, hidden: false };
    const readCardCatalog = vi.fn(async () => ({ actions: [ship], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const editCardCatalog = vi.fn();
    const filter = (name: string, words: string): VoiceResolvedParamDto => ({ name, kind: "filter_text", spoken: words, value: words, label: words });
    const deck = (value: string, label: string): VoiceResolvedParamDto => ({ name: "deck", kind: "deck_ref", spoken: label, value, label });
    const row = (action: string, invoke: string, params: VoiceResolvedParamDto[] = []) => ({ action, invoke, params });
    const run = setup(
      (agents) => [cardFor(agents[0].id, "coder-card")],
      () => ({
        "open the card actions editor": row("open_card_actions_editor", "openCardActionsEditor"),
        "edit the ship it action": row("edit_card_action", "editCardAction", [actionParam("Ship it")]),
        "icon git merge": row("choose_card_action_icon", "chooseCardActionIcon", [filter("icon", "git merge")]),
        "icon unicorn": row("choose_card_action_icon", "chooseCardActionIcon", [filter("icon", "unicorn")]),
        "ask before sending": row("ask_before_sending_card_action", "askBeforeSendingCardAction"),
        "send without asking": row("send_card_action_without_asking", "sendCardActionWithoutAsking"),
        "offer on every card": row("offer_card_action_on_every_card", "offerCardActionOnEveryCard"),
        "only when agents suggest it": row("offer_card_action_when_agents_choose", "offerCardActionWhenAgentsChoose"),
        "only in the Card tab": row("show_card_action_only_in_the_card_tab", "showCardActionOnlyInCardTab"),
        "show it on the card": row("show_card_action_on_the_card", "showCardActionOnCard"),
        "refresh the card actions": row("refresh_card_actions", "refreshCardActions"),
        "open card actions on the local daemon": row("open_card_actions_editor", "openCardActionsEditor", [deck(FIXTURE_DAEMON_ID, "Local daemon")]),
        "open card actions on the moon": row("open_card_actions_editor", "openCardActionsEditor", [deck("deck-not-on-the-dashboard", "moon")]),
      }),
      { extra: { readCardCatalog, editCardCatalog } },
    );
    await turnOnVoice();
    await speak(run.voice, "open the card actions editor");
    await speak(run.voice, "edit the ship it action");
    const editor = screen.getByRole("dialog", { name: /^Card actions: / });
    const box = (label: string) => within(editor).getByLabelText(label) as HTMLInputElement;

    await speak(run.voice, "icon git merge");
    expect(within(editor).getByTestId("agent-card-actions-editor-icon")).toHaveAttribute("data-icon", "git-merge");
    await speak(run.voice, "icon unicorn");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/No single icon is called "unicorn"/);
    expect(within(editor).getByTestId("agent-card-actions-editor-icon")).toHaveAttribute("data-icon", "git-merge");

    await speak(run.voice, "ask before sending");
    expect(box("Ask before sending")).toBeChecked();
    await speak(run.voice, "ask before sending");
    expect(box("Ask before sending"), "saying it again does not toggle it off").toBeChecked();
    await speak(run.voice, "send without asking");
    expect(box("Ask before sending")).not.toBeChecked();
    await speak(run.voice, "offer on every card");
    expect(box("Offer on every card")).toBeChecked();
    await speak(run.voice, "only when agents suggest it");
    expect(box("Offer on every card")).not.toBeChecked();
    const placement = () => within(editor).getByLabelText("Placement") as HTMLSelectElement;
    expect(placement().value).toBe("card");
    await speak(run.voice, "only in the Card tab");
    expect(placement().value).toBe("detail");
    await speak(run.voice, "only in the Card tab");
    expect(placement().value, "saying it again keeps it").toBe("detail");
    await speak(run.voice, "show it on the card");
    expect(placement().value).toBe("card");

    const reads = readCardCatalog.mock.calls.length;
    await speak(run.voice, "refresh the card actions");
    expect(readCardCatalog).toHaveBeenCalledTimes(reads + 1);
    expect((within(editor).getByLabelText("Name") as HTMLInputElement).value, "the form is kept").toBe("ship");

    await speak(run.voice, "open card actions on the local daemon");
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent(/not on the dashboard/);
    expect((within(editor).getByLabelText("Name") as HTMLInputElement).value, "naming the daemon it is on keeps the form").toBe("ship");
    await speak(run.voice, "open card actions on the moon");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("That daemon is not on the dashboard, or shows no cards, so its card actions cannot be shown.");
    expect(editCardCatalog).not.toHaveBeenCalled();

    const registry = VOICE_ACTIONS as unknown as Record<string, { voice?: boolean }>;
    const rows: [string, string, { name: string; kind: string }[]][] = [
      ["refreshCardActions", "refresh_card_actions", []],
      ["chooseCardActionIcon", "choose_card_action_icon", [{ name: "icon", kind: "filter_text" }]],
      ["askBeforeSendingCardAction", "ask_before_sending_card_action", []],
      ["sendCardActionWithoutAsking", "send_card_action_without_asking", []],
      ["offerCardActionOnEveryCard", "offer_card_action_on_every_card", []],
      ["offerCardActionWhenAgentsChoose", "offer_card_action_when_agents_choose", []],
      ["showCardActionOnCard", "show_card_action_on_the_card", []],
      ["showCardActionOnlyInCardTab", "show_card_action_only_in_the_card_tab", []],
    ];
    for (const [id, name, params] of rows) {
      expect(registry[id]?.voice, id).toBe(true);
      expectRow(name, id, ["overview", "agent"], params);
    }
  });
  /// Scenario: With the Card actions editor closed, ask by voice to edit or remove a card action. On a daemon whose card actions cannot be edited from the app, voice says so at once and nothing is read or queued; when the list is read and has no action of that name, or the read fails, voice says that instead of going quiet; and when the action is listed, its form, or the removal's confirmation, opens and voice says so. Nothing is written.
  it("desktop/agent-cards/084: a spoken edit or removal with the editor closed always ends in what it asked for or a refusal", async () => {
    const ship = { name: "ship", label: "Ship it", description: "Offer when it is ready.", prompt: "Ship it.", icon: "rocket", placement: "card", confirm: false, always: false, source: "user", default: false, overridden: false, hidden: false };
    const listed = { actions: [ship], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" };
    const said = () => ({
      "edit the ship it action": { action: "edit_card_action", invoke: "editCardAction", params: [actionParam("Ship it")] },
      "edit the deploy it action": { action: "edit_card_action", invoke: "editCardAction", params: [actionParam("Deploy it")] },
      "remove the deploy it action": { action: "remove_card_action", invoke: "removeCardAction", params: [actionParam("Deploy it")] },
      "remove the ship it action": { action: "remove_card_action", invoke: "removeCardAction", params: [actionParam("Ship it")] },
      "close the card actions editor": { action: "close", invoke: "closeTopmost", params: [] },
    });
    const settled = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); await act(async () => { await Promise.resolve(); await Promise.resolve(); }); };
    const report = () => screen.getByTestId("voice-report");
    const cards = (agents: { id: string }[]) => [cardFor(agents[0].id, "coder-card")];

    // A daemon whose card actions cannot be edited from the app: refused at once, nothing read, nothing waiting.
    {
      const readCardCatalog = vi.fn(async () => listed);
      const editCardCatalog = vi.fn();
      const run = setup(cards, said, { catalogEdit: false, extra: { readCardCatalog, editCardCatalog } });
      await turnOnVoice();
      await speak(run.voice, "edit the ship it action");
      await settled();
      expect(report()).toHaveTextContent(/cannot be edited from the app/);
      expect(readCardCatalog).not.toHaveBeenCalled();
      expect(screen.queryByLabelText("Name")).toBeNull();
      await speak(run.voice, "remove the ship it action");
      await settled();
      expect(report()).toHaveTextContent(/cannot be edited from the app/);
      expect(screen.queryByRole("alertdialog")).toBeNull();
      expect(readCardCatalog).not.toHaveBeenCalled();
      expect(editCardCatalog).not.toHaveBeenCalled();
      cleanup();
    }

    // The list is read and names no such action: voice says so, for an edit and for a removal.
    {
      const readCardCatalog = vi.fn(async () => listed);
      const editCardCatalog = vi.fn();
      const run = setup(cards, said, { extra: { readCardCatalog, editCardCatalog } });
      await turnOnVoice();
      await speak(run.voice, "edit the deploy it action");
      await settled();
      expect(readCardCatalog).toHaveBeenCalledTimes(1);
      expect(screen.getByRole("dialog", { name: /^Card actions: / })).toBeInTheDocument();
      expect(report()).toHaveTextContent('No single card action is called "Deploy it" in the editor.');
      expect(screen.queryByLabelText("Name")).toBeNull();
      await speak(run.voice, "close the card actions editor");
      await settled();
      expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
      await speak(run.voice, "remove the deploy it action");
      await settled();
      expect(report()).toHaveTextContent('No single card action is called "Deploy it" in the editor.');
      expect(screen.queryByRole("alertdialog")).toBeNull();
      expect(editCardCatalog).not.toHaveBeenCalled();
      cleanup();
    }

    // The read fails: voice says why, rather than going quiet.
    {
      const readCardCatalog = vi.fn(async () => { throw new Error("the daemon did not answer"); });
      const run = setup(cards, said, { extra: { readCardCatalog, editCardCatalog: vi.fn() } });
      await turnOnVoice();
      await speak(run.voice, "edit the ship it action");
      await settled();
      expect(report()).toHaveTextContent("The card actions could not be read: the daemon did not answer");
      expect(screen.queryByLabelText("Name")).toBeNull();
      cleanup();
    }

    // The action is listed: its form opens, and then the removal's confirmation, each from a closed editor, and voice says so.
    {
      const readCardCatalog = vi.fn(async () => listed);
      const editCardCatalog = vi.fn();
      const run = setup(cards, said, { extra: { readCardCatalog, editCardCatalog } });
      await turnOnVoice();
      await speak(run.voice, "edit the ship it action");
      await settled();
      const editor = screen.getByRole("dialog", { name: /^Card actions: / });
      expect((within(editor).getByLabelText("Name") as HTMLInputElement).value).toBe("ship");
      expect(report()).toHaveTextContent(CARD_ACTION_FORM_OPENED);
      await speak(run.voice, "close the card actions editor");
      await settled();
      expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
      await speak(run.voice, "remove the ship it action");
      await settled();
      expect(screen.getByRole("alertdialog", { name: "Remove Ship it?" })).toBeInTheDocument();
      expect(report()).toHaveTextContent(CARD_ACTION_REMOVAL_ASKED);
      expect(editCardCatalog).not.toHaveBeenCalled();
    }
  });

  /// Scenario: On the Table view, say "only the ones that need me" and then "show all cards". The first keeps the Table on screen and shows only the agent whose card needs you and the agent above it, with the Needs you only chip beside Filter; the second brings every row back and the chip goes (PR #2491 decision 2026-11-25).
  it("desktop/agent-cards/114: the spoken Needs you only applies to the Table view and keeps it", async () => {
    window.localStorage.setItem(MODE_KEY, "table");
    const needsYou = (value: "on" | "off"): VoiceResolvedParamDto => ({ name: "needs_you", kind: "needs_you_filter", spoken: value, value, label: value });
    const run = setup(
      (agents) => {
        const parent = cardFor(agents[0].id, "parent-card");
        return [parent, cardFor(agents[1].id, "child-card", { parent: parent.key, needs_you: true, needs_you_reasons: ["question"], status: "waiting_for_input" }), ...agents.slice(2).map((agent, at) => cardFor(agent.id, `other-${at}`))];
      },
      () => ({
        "only the ones that need me": { action: "filter_needs_you_cards", invoke: "toggleNeedsYouFilter", params: [needsYou("on")] },
        "show all cards": { action: "filter_needs_you_cards", invoke: "toggleNeedsYouFilter", params: [needsYou("off")] },
      }),
    );
    const rows = () => screen.queryAllByRole("row", { name: /Open .* agent$/ });
    const before = rows().length;
    expect(before).toBeGreaterThan(2);
    await turnOnVoice();
    await speak(run.voice, "only the ones that need me");
    expect(screen.getByTestId("overview-mode-table")).toHaveAttribute("aria-checked", "true");
    expect(document.querySelector("[data-testid^='agent-cards-deck-']"), "the Cards view is not shown").toBeNull();
    expect(rows()).toHaveLength(2);
    expect(screen.getByTestId("needs-you-chip")).toBeInTheDocument();
    await speak(run.voice, "show all cards");
    expect(rows()).toHaveLength(before);
    expect(screen.queryByTestId("needs-you-chip")).toBeNull();
    expect(screen.getByTestId("overview-mode-table")).toHaveAttribute("aria-checked", "true");
  });

  /// Scenario: With two daemons showing cards, say "open card actions" without naming one. Nothing opens; a dialog asks "Which daemon?" and lists both daemons by the names the dashboard gives them; choosing the second opens the Card actions editor bound to it, titled with its name, and reads only its card actions. Saying "open card actions on" the first daemon opens that one's editor straight away (PR #2491 decision 2026-11-25).
  it("desktop/agent-cards/115: with several daemons, a bare open card actions asks which daemon, as naming an agent does", async () => {
    const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const [local, far] = createFixtureFleet("fleet");
    const localName = deckName(local.connection);
    const farName = deckName(far.connection);
    const deck = (value: string, label: string): VoiceResolvedParamDto => ({ name: "deck", kind: "deck_ref", spoken: label, value, label });
    const run = setup(
      (agents) => [cardFor(agents[0].id, "coder-card")],
      () => ({
        "open card actions": { action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [] },
        "open card actions on the local daemon": { action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [deck(FIXTURE_DAEMON_ID, localName)] },
      }),
      { extra: { readCardCatalog, editCardCatalog: vi.fn() }, remote: true },
    );
    await turnOnVoice();
    await speak(run.voice, "open card actions");
    expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
    expect(readCardCatalog).not.toHaveBeenCalled();
    const choice = screen.getByRole("dialog", { name: "Which daemon?" });
    expect(within(choice).getByRole("button", { name: new RegExp(localName) })).toBeInTheDocument();
    const second = within(choice).getByRole("button", { name: new RegExp(farName) });
    await act(async () => { fireEvent.click(second); });
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    const editor = screen.getByRole("dialog", { name: `Card actions: ${farName}` });
    expect(editor).toBeInTheDocument();
    expect(readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE);
    await act(async () => { fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close")); });

    await speak(run.voice, "open card actions on the local daemon");
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    expect(screen.getByRole("dialog", { name: `Card actions: ${localName}` })).toBeInTheDocument();
    expect(readCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, INSTANCE);
  });

  /// Scenario: With two daemons showing cards, say "open card actions and add a card action" without naming a daemon. The chain stops at its first step on a "Which daemon?" choice listing both daemons, and no editor opens and no form appears on a guessed daemon. Choosing the second daemon opens its Card actions editor and the chain goes on with its next step there, opening the empty form. Afterwards "open card actions on the local daemon" opens that editor at once, with no choice left over from the chain (PR #2491, Qodo 12).
  it("desktop/agent-cards/115: a chained open card actions with several daemons asks which daemon, and goes on with the chosen one", async () => {
    const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const [local, far] = createFixtureFleet("fleet");
    const localName = deckName(local.connection);
    const farName = deckName(far.connection);
    const said = "open card actions and add a card action";
    type ChainStep = Extract<VoiceResultDto["outcome"], { kind: "chain" }>["steps"][number];
    const steps: ChainStep[] = [
      { said: "open card actions", action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [], sentence: "Opening the Card actions editor.", screen: "overview" },
      { said: "add a card action", action: "add_card_action", invoke: "addCardAction", params: [], sentence: "Adding a card action.", screen: "overview" },
    ];
    const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => transcript === said
      ? { backend: "stub", resolveMs: 21, outcome: { kind: "chain", transcript, steps, sentence: "Opening the Card actions editor, then adding a card action." } }
      : transcript === "open card actions on the local daemon"
        ? { backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [{ name: "deck", kind: "deck_ref", spoken: localName, value: FIXTURE_DAEMON_ID, label: localName }], sentence: "Done." } }
        : { backend: "stub", resolveMs: 21, outcome: { kind: "no_match", transcript, sentence: "No match." } });
    const run = setup((agents) => [cardFor(agents[0].id, "coder-card")], () => ({}), { extra: { readCardCatalog, editCardCatalog: vi.fn(), resolveVoice }, remote: true });
    await turnOnVoice();
    await speak(run.voice, said);
    const choice = screen.getByRole("dialog", { name: "Which daemon?" });
    expect(within(choice).getByRole("button", { name: new RegExp(localName) })).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: /^Card actions: / }), "no editor opens on a guessed daemon").toBeNull();
    expect(readCardCatalog).not.toHaveBeenCalled();

    await act(async () => { fireEvent.click(within(choice).getByRole("button", { name: new RegExp(farName) })); });
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_CHAIN_SETTLE_MS * 2); });
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    const editor = screen.getByRole("dialog", { name: `Card actions: ${farName}` });
    expect(readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE);
    expect((within(editor).getByLabelText("Name") as HTMLInputElement).value, "the chain went on with its next step in the chosen daemon's editor").toBe("");
    await act(async () => { fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close")); });

    await speak(run.voice, "open card actions on the local daemon");
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    expect(screen.getByRole("dialog", { name: `Card actions: ${localName}` })).toBeInTheDocument();
    expect(readCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, INSTANCE);
  });

  /// Scenario: With two daemons showing cards, tick a card on the second daemon only and say "open card actions" without naming a daemon: that daemon's Card actions editor opens with no question. Close it, add a card on the first daemon to the selection and say it again: the selection spans both daemons, so the app asks "Which daemon?" as it does with nothing selected (PR #2491, 2026-11-25).
  it("desktop/agent-cards/119: a bare open card actions prefers the selected cards' daemon when there is exactly one", async () => {
    const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const farName = deckName(createFixtureFleet("fleet")[1].connection);
    const run = setup(
      (agents) => [cardFor(agents[0].id, "coder-card")],
      () => ({ "open card actions": { action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [] } }),
      { extra: { readCardCatalog, editCardCatalog: vi.fn() }, remote: true },
    );
    await turnOnVoice();
    fireEvent.click(screen.getByRole("checkbox", { name: "Select far-coder" }));
    await speak(run.voice, "open card actions");
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    const editor = screen.getByRole("dialog", { name: `Card actions: ${farName}` });
    expect(readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE);
    await act(async () => { fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close")); });

    fireEvent.click(cardNode(run.cards[0]).getByRole("checkbox", { name: "Select coder-card" }));
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("2 selected");
    await speak(run.voice, "open card actions");
    expect(screen.getByRole("dialog", { name: "Which daemon?" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
    expect(readCardCatalog).toHaveBeenCalledTimes(1);
  });

  /// Scenario: With two daemons showing cards, tick a card on the second daemon, then open the first daemon's agent's pane from its card. Over the pane, with voice declaring the agent screen, say "open card actions": the editor of the PANE's daemon opens over the pane, not the selected card's daemon, and the pane stays open. Say "add a card action" and "ask before sending": the form opens and its box is ticked, and nothing is written (PR #2491 review T1).
  it("desktop/agent-cards/121: over an agent's pane, voice opens the pane's daemon's Card actions editor and drives its form", async () => {
    const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const editCardCatalog = vi.fn();
    const localName = deckName(createFixtureSnapshot("connected").connection);
    const run = setup(
      (agents) => [cardFor(agents[0].id, "coder-card")],
      () => ({
        "open card actions": { action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [] },
        "add a card action": { action: "add_card_action", invoke: "addCardAction", params: [] },
        "ask before sending": { action: "ask_before_sending_card_action", invoke: "askBeforeSendingCardAction", params: [] },
      }),
      { extra: { readCardCatalog, editCardCatalog }, remote: true, gateScreens: true },
    );
    await turnOnVoice();
    fireEvent.click(screen.getByRole("checkbox", { name: "Select far-coder" }));
    fireEvent.click(document.querySelector<HTMLElement>(`article[data-card-key="${run.cards[0].key}"]`)!);
    const pane = screen.getByTestId("agent-pane-overlay");
    // A card opens its agent's pane on Terminal, with the Card tab beside it (PR #2491 decision 2026-11-26).
    expect(within(pane).getByRole("tab", { selected: true })).toHaveAccessibleName("Terminal");
    expect(within(pane).getByRole("tab", { name: /^Card/ })).toBeInTheDocument();

    await speak(run.voice, "open card actions");
    expect(run.declareVoiceScreen.mock.calls.at(-1)?.[0], "voice declared the agent screen").toBe("agent");
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    const editor = screen.getByRole("dialog", { name: `Card actions: ${localName}` });
    expect(readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, INSTANCE);
    expect(screen.getByTestId("agent-pane-overlay"), "the pane stays open under the editor").toBeInTheDocument();

    await speak(run.voice, "add a card action");
    expect((within(editor).getByLabelText("Name") as HTMLInputElement).value).toBe("");
    await speak(run.voice, "ask before sending");
    expect(within(editor).getByLabelText("Ask before sending")).toBeChecked();
    expect(editCardCatalog).not.toHaveBeenCalled();
  });

  /// Scenario: With only a second daemon showing cards, open, from its table row, the pane of an agent on the first daemon, which shows no cards, and say "open card actions": the app says in words that this agent's daemon has no Card actions editor, and no editor opens, not even the other daemon's. Then open an agent's pane on the Daemons screen and say it there: it is refused in words too (PR #2491 review T1).
  it("desktop/agent-cards/121: over a pane whose daemon has no editor, or a Daemons-screen pane, a spoken open card actions is refused in words", async () => {
    window.localStorage.setItem(MODE_KEY, "table");
    const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const said = () => ({ "open card actions": { action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [] } });
    const run = setup(() => [], said, { extra: { readCardCatalog, editCardCatalog: vi.fn(), agentCards: {} }, remote: true, gateScreens: true });
    const local = createFixtureSnapshot("connected").agents[0];
    const row = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: local.id })}`);
    fireEvent.click(row.querySelector("td:last-child") ?? row);
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    await turnOnVoice();
    await speak(run.voice, "open card actions");
    expect(run.declareVoiceScreen.mock.calls.at(-1)?.[0]).toBe("agent");
    expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
    expect(screen.queryByRole("dialog", { name: "Which daemon?" })).toBeNull();
    expect(readCardCatalog).not.toHaveBeenCalled();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("This agent's daemon shows no cards, so it has no Card actions editor.");
    cleanup();

    const deckRun = setup((agents) => [cardFor(agents[0].id, "coder-card")], said, {
      extra: { readCardCatalog }, gateScreens: true,
      initialView: { kind: "agent", deckId: FIXTURE_DAEMON_ID, agentId: local.id, from: "deck" },
    });
    await turnOnVoice();
    await speak(deckRun.voice, "open card actions");
    expect(deckRun.declareVoiceScreen.mock.calls.at(-1)?.[0]).toBe("agent");
    expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
    expect(readCardCatalog).not.toHaveBeenCalled();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("The Card actions editor opens over an agent's pane opened from the agent dashboard, not from the Daemons screen.");
  });

  /// Scenario: Open an agent's pane from the Daemons screen, where no dashboard is behind it, and say one of the dashboard's card commands: "next agent", "previous agent", "next one that needs me", "show coder-card's card" or "go ahead on coder-card". The app says in words that the command works over a pane opened from the agent dashboard, instead of saying the command is not wired, and the pane stays where it is with nothing sent (PR #2491 review, Qodo 6).
  it.each([
    ["next agent", "next_agent_card", "nextAgentCard", () => [], "Moving between agents works over an agent's pane opened from the agent dashboard, not from the Daemons screen."],
    ["previous agent", "previous_agent_card", "previousAgentCard", () => [], "Moving between agents works over an agent's pane opened from the agent dashboard, not from the Daemons screen."],
    ["next one that needs me", "next_needs_you_card", "nextNeedsYouCard", () => [], "Moving between agents works over an agent's pane opened from the agent dashboard, not from the Daemons screen."],
    ["show coder-card's card", "show_agent_card", "showAgentCard", (id: string) => [agentParam(id, "coder-card")], "An agent's card opens over a pane opened from the agent dashboard, not from the Daemons screen."],
    ["go ahead on coder-card", "run_card_action", "runCardAction", (id: string) => [agentParam(id, "coder-card"), actionParam("Go ahead")], "A card's actions are pressed over a pane opened from the agent dashboard, not from the Daemons screen."],
  ] as const)("a dashboard card command said over a Daemons-screen pane is refused in words: %s", async (said, action, invoke, params, sentence) => {
    const local = createFixtureSnapshot("connected").agents[0];
    const run = setup((agents) => [cardFor(agents[0].id, "coder-card")], (agents) => ({ [said]: { action, invoke, params: params(agents[0].id) } }), {
      gateScreens: true,
      initialView: { kind: "agent", deckId: FIXTURE_DAEMON_ID, agentId: local.id, from: "deck" },
    });
    const pane = screen.getByTestId("agent-pane-overlay").getAttribute("aria-label");
    await turnOnVoice();
    await speak(run.voice, said);
    expect(run.declareVoiceScreen.mock.calls.at(-1)?.[0]).toBe("agent");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(sentence);
    expect(screen.getByTestId("agent-pane-overlay").getAttribute("aria-label")).toBe(pane);
    expect(run.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: On the agent dashboard, with no daemon on it showing cards, say "next one that needs me". The app says in words that no daemon on the dashboard shows cards, instead of saying the command is not wired (PR #2491 review, Qodo 6).
  it("a dashboard card command said where no daemon shows cards is refused in words", async () => {
    const run = setup(() => [], () => ({ "next one that needs me": { action: "next_needs_you_card", invoke: "nextNeedsYouCard", params: [] } }), { gateScreens: true, extra: { agentCards: undefined } });
    await turnOnVoice();
    await speak(run.voice, "next one that needs me");
    expect(run.declareVoiceScreen.mock.calls.at(-1)?.[0]).toBe("overview");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("No daemon on the agent dashboard shows cards.");
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
  });

  /// Scenario: With two daemons showing cards, open the first daemon's agent's pane and say an editor command that names no daemon ("open card actions", "add a card action", "edit the go ahead action" or "remove the go ahead action"). While it is still being worked out, go back to the dashboard and open the second daemon's agent's pane, leaving the selected daemon as it was. When the answer arrives, nothing opens: the app says the pane on screen changed, reads neither daemon's card actions and shows no form or confirmation over the second pane. Saying "open card actions on" the second daemon over that pane still opens its editor (PR #2491 review V1).
  it.each([
    ["open card actions", "open_card_actions_editor", "openCardActionsEditor", []],
    ["add a card action", "add_card_action", "addCardAction", []],
    ["edit the go ahead action", "edit_card_action", "editCardAction", [actionParam("Go ahead")]],
    ["remove the go ahead action", "remove_card_action", "removeCardAction", [actionParam("Go ahead")]],
  ] as const)("desktop/agent-cards/123: a nameless %s said over one pane is refused once another daemon's pane is on screen", async (said, action, invoke, params) => {
    const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
    const editCardCatalog = vi.fn();
    const farName = deckName(createFixtureFleet("fleet")[1].connection);
    const deck = (value: string, label: string): VoiceResolvedParamDto => ({ name: "deck", kind: "deck_ref", spoken: label, value, label });
    const declareVoiceScreen = vi.fn();
    let release: ((answer: VoiceResultDto) => void) | undefined;
    const resolveVoice = vi.fn((transcript: string) => transcript === said
      ? new Promise<VoiceResultDto>((resolve) => { release = resolve; })
      : Promise.resolve<VoiceResultDto>({ backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript, action: "open_card_actions_editor", invoke: "openCardActionsEditor", params: [deck(FIXTURE_REMOTE_DAEMON_ID, farName)], sentence: "Done." } }));
    const run = setup((agents) => [cardFor(agents[0].id, "coder-card")], () => ({}), { extra: { readCardCatalog, editCardCatalog, resolveVoice, declareVoiceScreen }, remote: true });
    await turnOnVoice();
    fireEvent.click(document.querySelector<HTMLElement>(`article[data-card-key="${run.cards[0].key}"]`)!);
    expect(within(screen.getByTestId("agent-pane-overlay")).getByRole("tab", { name: /^Card/ })).toBeInTheDocument();
    const firstPane = screen.getByTestId("agent-pane-overlay").getAttribute("aria-label");

    await speak(run.voice, said);
    expect(resolveVoice).toHaveBeenCalledTimes(1);
    expect(declareVoiceScreen.mock.calls.at(-1)?.[0], "said over the first daemon's pane").toBe("agent");
    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Back to dashboard" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select far-coder" }).closest("article")!);
    expect(screen.getByTestId("agent-pane-overlay").getAttribute("aria-label"), "the second daemon's pane is on screen").not.toBe(firstPane);

    await act(async () => { release!({ backend: "stub", resolveMs: 21, outcome: { kind: "dispatch", transcript: said, action, invoke, params: [...params], sentence: "Done." } }); });
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Nothing ran — the pane on screen changed. Say it again.");
    expect(readCardCatalog).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();
    expect(screen.queryByLabelText("Name")).toBeNull();
    expect(editCardCatalog).not.toHaveBeenCalled();

    /* A daemon named in the utterance still wins over the pane it was said in. */
    await speak(run.voice, "open card actions on the far daemon");
    expect(screen.getByRole("dialog", { name: `Card actions: ${farName}` })).toBeInTheDocument();
    expect(readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE);
  });

  /// Scenario: On the Table view with Needs you only off, say "only the ones that need me": the voice panel declares the dashboard's view and the filter as they stood (table, off), and the filter turns on. Say "show all cards": it declares table and on, and the filter turns off. Switch to the Cards view and say "only the ones that need me" again: it declares cards and off. That declaration is what tells the command model that "show all cards" turns the filter off while it is on (PR #2491, 2026-11-25).
  it("desktop/agent-cards/120: each utterance declares the dashboard's view and whether Needs you only is on", async () => {
    window.localStorage.setItem(MODE_KEY, "table");
    const declareVoiceScreen = vi.fn();
    const needsYou = (value: "on" | "off"): VoiceResolvedParamDto => ({ name: "needs_you", kind: "needs_you_filter", spoken: value, value, label: value });
    const run = setup(
      (agents) => [cardFor(agents[0].id, "parent-card"), cardFor(agents[1].id, "child-card", { needs_you: true, needs_you_reasons: ["question"], status: "waiting_for_input" })],
      () => ({
        "only the ones that need me": { action: "filter_needs_you_cards", invoke: "toggleNeedsYouFilter", params: [needsYou("on")] },
        "show all cards": { action: "filter_needs_you_cards", invoke: "toggleNeedsYouFilter", params: [needsYou("off")] },
      }),
      { extra: { declareVoiceScreen } },
    );
    const lastDashboard = () => declareVoiceScreen.mock.calls.at(-1)?.[6];
    await turnOnVoice();
    await speak(run.voice, "only the ones that need me");
    expect(lastDashboard()).toEqual({ view: "table", needsYouOnly: false });
    expect(screen.getByTestId("needs-you-chip")).toBeInTheDocument();
    await speak(run.voice, "show all cards");
    expect(lastDashboard()).toEqual({ view: "table", needsYouOnly: true });
    expect(screen.queryByTestId("needs-you-chip")).toBeNull();
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    await speak(run.voice, "only the ones that need me");
    expect(lastDashboard()).toEqual({ view: "cards", needsYouOnly: false });
  });
});
