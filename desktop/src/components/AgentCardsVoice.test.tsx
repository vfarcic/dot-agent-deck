import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { approvedProjectTrust } from "../test/projectTrust";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto, type VoiceResultDto, type VoiceStatusDto } from "../lib/bridge";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { AgentCardAction, AgentCardEntry, DeckActionResult, DeckRuntimeState } from "../types";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    createElement("div", { "data-testid": `terminal-${agentId}`, role: "group", "aria-label": `${label} terminal` })
  ),
}));

import { DeckShell } from "../App";

/*
 * PRD #2374 — `desktop/agent-cards/008`: the cards' voice rows (contract §8,
 * change 13). Two halves:
 *
 * 1. the registry and the command table agree: every new registry id is
 *    `voice: true` with exactly one `commands.toml` row that invokes it on the
 *    overview, and the two controls the contract keeps off voice carry a
 *    reason and no row. Read off the file the Rust crate embeds, as
 *    `src/data/fixture.test.ts` does, with the row ids and param names written
 *    out as the contract gives them.
 * 2. a spoken `run_card_action` / `reply_to_agent_card` goes through the real
 *    shell, voice panel and dispatch seam, with only the microphone and the
 *    resolved answer synthetic. The answer is shaped as the crate sends it:
 *    the action's param is of kind `card_action` and the reply's of kind
 *    `reply_text` (`desktop/src-tauri/src/voice/table.rs`). A confirm action,
 *    and a close, open the card's confirm step and send nothing; a plain
 *    prompt action is sent; a reply sends the user's words.
 */

const commandsSource = readFileSync(resolve("src-tauri/src/voice/commands.toml"), "utf8");

type Row = { id: string; invoke: string; screens: string[]; params: { name: string; kind: string }[] };

/** The `[[commands]]` rows of the table, with the keys this test reads. */
function commandRows(): Row[] {
  return commandsSource.split(/^\[\[commands\]\]\s*$/m).slice(1).map((chunk) => {
    const value = (key: string) => chunk.match(new RegExp(`^${key}\\s*=\\s*"([^"]*)"`, "m"))?.[1] ?? "";
    const screens = chunk.match(/^screens\s*=\s*\[([^\]]*)\]/m)?.[1] ?? "";
    const params = [...chunk.matchAll(/\[\[commands\.params\]\]\s*\n\s*name\s*=\s*"([^"]+)"\s*\n\s*kind\s*=\s*"([^"]+)"/g)].map((found) => ({ name: found[1], kind: found[2] }));
    return { id: value("id"), invoke: value("invoke"), screens: [...screens.matchAll(/"([^"]+)"/g)].map((found) => found[1]), params };
  });
}

/** Registry id → (commands.toml row id, its params), from the contract's §8 table as change 13 settled the param names. */
const CARD_ROWS: Record<string, { row: string; params: { name: string; kind: string }[] }> = {
  setOverviewMode: { row: "set_overview_mode", params: [{ name: "view", kind: "overview_mode" }] },
  showAgentCard: { row: "show_agent_card", params: [{ name: "agent", kind: "agent_ref" }] },
  nextAgentCard: { row: "next_agent_card", params: [] },
  previousAgentCard: { row: "previous_agent_card", params: [] },
  nextNeedsYouCard: { row: "next_needs_you_card", params: [] },
  toggleNeedsYouFilter: { row: "filter_needs_you_cards", params: [{ name: "needs_you", kind: "needs_you_filter" }] },
  closeAgentCard: { row: "close_agent_card", params: [] },
  runCardAction: { row: "run_card_action", params: [{ name: "agent", kind: "agent_ref" }, { name: "action", kind: "card_action" }] },
  replyToAgentCard: { row: "reply_to_agent_card", params: [{ name: "agent", kind: "agent_ref" }, { name: "reply", kind: "reply_text" }] },
};

const MODE_KEY = "dot-agent-deck.overview.mode";
const INSTANCE = "daemon-instance-voice";

function action(id: string, overrides: Partial<AgentCardAction> = {}): AgentCardAction {
  return {
    id, label: id, kind: "prompt", prompt: `Do ${id}.`, text_input: false, confirm: false, confirm_detail: null, icon: null,
    placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: `tok-${id}`, ...overrides,
  };
}

function cardFor(agentId: string, name: string): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`, instance_id: INSTANCE, pane_id: `pane-${agentId}`, agent_id: agentId, agent_type: "claude_code", name,
    status: "working", needs_you: true, needs_you_reasons: ["waiting"], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: { source: "agent", status: "waiting", summary: "Ready to merge", published_at: "2026-11-17T00:00:00Z", revision: 1 },
    fields: [], questions: [],
    actions: [
      action("merge", { label: "Merge it", prompt: "Merge it, following this project's own merge procedure, then update your card.", confirm: true, confirm_detail: "Still pending: open pull request #7." }),
      action("go-ahead", { label: "Go ahead", prompt: "Go ahead." }),
      action("reply", { label: "Reply", prompt: null, text_input: true, source: "deck", placement: "detail" }),
      action("open", { label: "Open", kind: "open", prompt: null, source: "deck", placement: "detail" }),
      action("close", { label: "Close", kind: "close", prompt: null, confirm: true, source: "deck", placement: "detail" }),
    ],
    primary_action_id: "go-ahead", facts: { source: "deck", pull_request: null }, revision: 3, updated_at: "2026-11-17T00:00:00Z",
  };
}

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => { document = structuredClone(next); return structuredClone(document); }),
  };
}

/** The real shell on the dashboard, a daemon list with one card per fixture agent, and one utterance resolved to `outcome`. */
function renderSpoken(outcome: VoiceResultDto["outcome"]) {
  let started = false;
  let delivered = false;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({ state, capturedMs: 900, maxMs: 30_000, capped: false, available: true, backend: "remote" });
  const resolveVoice = vi.fn(async (): Promise<VoiceResultDto> => ({ outcome, resolveMs: null, backend: "stub" }));
  const snapshot = createFixtureSnapshot("connected");
  const agent = snapshot.agents[0];
  const card = cardFor(agent.id, "voice-card-agent");
  const settings = settingsStore();
  const invokeCardAction = vi.fn(async (_target: unknown, _actionId: string, options?: { requestId?: string }) => ({ request_id: options?.requestId ?? "r", outcome: "sent" as const, message_id: "m-voice-1" }));
  const runtime = {
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
    ...approvedProjectTrust(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 3, entries: [card], catalogErrors: [] } },
    invokeCardAction,
    resolveVoice,
    voiceStatus: vi.fn(async () => {
      if (started && !delivered) { delivered = true; return status("done"); }
      return status(started ? "recording" : "idle");
    }),
    voiceStart: vi.fn(async () => { started = true; return status("recording"); }),
    voiceStop: vi.fn(async () => ({ outcome: { kind: "heard" as const, transcript: outcome.kind === "dispatch" ? outcome.transcript : "", sentence: "Heard." }, transcribeMs: null, backend: "remote" as const, audioMs: 900 })),
    voiceCancel: vi.fn(async () => status("idle")),
  } as unknown as DeckRuntimeState;
  render(createElement(DeckShell, { runtime, initialView: { kind: "overview" } }));
  return { agent, card, invokeCardAction, resolveVoice };
}

async function speak(resolveVoice: ReturnType<typeof vi.fn>) {
  await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
  await waitFor(() => expect(resolveVoice).toHaveBeenCalledTimes(1));
}

function runCardAction(agentId: string, label: string, transcript: string): VoiceResultDto["outcome"] {
  return {
    kind: "dispatch", transcript, action: "run_card_action", invoke: "runCardAction",
    params: [
      { name: "agent", kind: "agent_ref", spoken: "the voice card agent", value: agentId, label: "voice-card-agent" },
      { name: "action", kind: "card_action", spoken: label, value: label, label },
    ],
    sentence: `Pressed ${label} on voice-card-agent's card.`,
  } as VoiceResultDto["outcome"];
}

describe("desktop/agent-cards/008 — the cards by voice (PRD #2374)", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/?fixture=1&experimental=1");
    window.localStorage.clear();
    window.sessionStorage.clear();
    window.localStorage.setItem(MODE_KEY, "cards");
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: false, media: query, onchange: null, addListener: vi.fn(), removeListener: vi.fn(),
      addEventListener: vi.fn(), removeEventListener: vi.fn(), dispatchEvent: vi.fn(),
    })));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  /// Scenario: read the action registry and the command table the crate embeds. Each of the nine card controls is `voice: true` with one overview row that invokes it under the contract's row id and param names; the confirm step's send and the row menu are kept off voice with a reason and no row.
  it("desktop/agent-cards/008: every new card control is voice: true with its commands.toml row", () => {
    const rows = commandRows();
    const registry = VOICE_ACTIONS as unknown as Record<string, { voice?: boolean; no_voice?: string }>;
    for (const [invoke, wanted] of Object.entries(CARD_ROWS)) {
      expect(registry[invoke]?.voice, `${invoke} is voice: true`).toBe(true);
      expect(registry[invoke]?.no_voice, `${invoke} has no no_voice`).toBeUndefined();
      const invoking = rows.filter((row) => row.invoke === invoke);
      expect(invoking.map((row) => row.id), `the rows invoking ${invoke}`).toEqual([wanted.row]);
      expect(invoking[0].screens).toEqual(["overview"]);
      expect(invoking[0].params).toEqual(wanted.params);
    }
    for (const kept of ["confirmCardAction", "toggleOverviewRowMenu"]) {
      expect(registry[kept]?.voice).not.toBe(true);
      expect((registry[kept]?.no_voice ?? "").trim().length, `${kept} carries a no_voice reason`).toBeGreaterThan(0);
      expect(rows.filter((row) => row.invoke === kept)).toEqual([]);
    }
  });

  /// Scenario: say "press merge it on the voice card agent's card", where Merge it asks for confirmation. The card's overlay opens at its confirm step showing the exact prompt and what is pending, and nothing is sent.
  it("desktop/agent-cards/008: a spoken confirm action opens the card's confirm step and sends nothing", async () => {
    const { card, invokeCardAction, resolveVoice } = renderSpoken(runCardAction(createFixtureSnapshot("connected").agents[0].id, "Merge it", "press merge it on the voice card agent's card"));
    await speak(resolveVoice);
    await waitFor(() => expect(screen.getByTestId("agent-card-confirm")).toBeInTheDocument());
    expect(screen.getByTestId("agent-card-overlay").getAttribute("data-card-key")).toBe(card.key);
    const step = screen.getByTestId("agent-card-confirm");
    expect(step.textContent).toContain("Merge it, following this project's own merge procedure, then update your card.");
    expect(step.textContent).toContain("Still pending: open pull request #7.");
    expect(invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: say "press close on the voice card agent's card". The overlay opens at the close confirmation and the agent is not closed.
  it("desktop/agent-cards/008: a spoken close only opens its confirmation", async () => {
    const { card, invokeCardAction, resolveVoice } = renderSpoken(runCardAction(createFixtureSnapshot("connected").agents[0].id, "Close", "press close on the voice card agent's card"));
    await speak(resolveVoice);
    await waitFor(() => expect(screen.getByTestId("agent-card-confirm")).toBeInTheDocument());
    expect(screen.getByTestId("agent-card-overlay").getAttribute("data-card-key")).toBe(card.key);
    expect(invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: say "press go ahead on the voice card agent's card", a prompt action that needs no confirmation. Exactly that action of that card is sent once, unconfirmed.
  it("desktop/agent-cards/008: a spoken prompt action is sent to that card's agent", async () => {
    const agentId = createFixtureSnapshot("connected").agents[0].id;
    const { invokeCardAction, resolveVoice } = renderSpoken(runCardAction(agentId, "Go ahead", "press go ahead on the voice card agent's card"));
    await speak(resolveVoice);
    await waitFor(() => expect(invokeCardAction).toHaveBeenCalledTimes(1));
    expect(invokeCardAction).toHaveBeenCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: `pane-${agentId}`, agentId }, "go-ahead", expect.objectContaining({ requestId: expect.any(String) }));
    expect((invokeCardAction.mock.calls[0][2] as { confirm?: boolean }).confirm).not.toBe(true);
  });

  /// Scenario: say "reply to the voice card agent use the release branch". The card's `reply` action is sent with the user's own words as its text.
  it("desktop/agent-cards/008: a spoken reply sends the user's words", async () => {
    const agentId = createFixtureSnapshot("connected").agents[0].id;
    const transcript = "reply to the voice card agent use the release branch";
    const { invokeCardAction, resolveVoice } = renderSpoken({
      kind: "dispatch", transcript, action: "reply_to_agent_card", invoke: "replyToAgentCard",
      params: [
        { name: "agent", kind: "agent_ref", spoken: "the voice card agent", value: agentId, label: "voice-card-agent" },
        { name: "reply", kind: "reply_text", spoken: "use the release branch", value: "use the release branch", label: "use the release branch" },
      ],
      sentence: "Replied to voice-card-agent.",
    } as VoiceResultDto["outcome"]);
    await speak(resolveVoice);
    await waitFor(() => expect(invokeCardAction).toHaveBeenCalledTimes(1));
    expect(invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId }), "reply", expect.objectContaining({ text: "use the release branch" }));
    expect(within(document.body).queryByText(/Say what to reply/)).toBeNull();
  });
});
