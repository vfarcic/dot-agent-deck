import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { agentDomKey } from "../components/AgentOverview";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto, type VoiceResultDto, type VoiceStatusDto } from "./bridge";
import type { DeckActionResult, DeckRuntimeState } from "../types";

type RegistryEntry = {
  run: (...args: unknown[]) => unknown;
  voice?: boolean;
  no_voice?: string;
};

const registryDispatch = vi.hoisted(() => vi.fn<(actionId: string) => void>());

/*
 * Wrap the real registry rather than replacing it. Once App consumes the
 * registry, clicks still execute the production callable and therefore still
 * have to produce the same rendered result; the wrapper only records which
 * shared seam they crossed on the way there.
 *
 * Before the registry exists, this factory is never evaluated by the App
 * import below. That is deliberate: the integration cases can then prove the
 * current controls bypass it, while the contract case reports the absent
 * module independently.
 */
vi.mock("./voiceActions", async (importOriginal) => {
  const actual = await importOriginal<{ VOICE_ACTIONS: Record<string, RegistryEntry> }>();
  return {
    ...actual,
    VOICE_ACTIONS: Object.fromEntries(
      Object.entries(actual.VOICE_ACTIONS).map(([actionId, entry]) => [
        actionId,
        {
          ...entry,
          run: (...args: unknown[]) => {
            registryDispatch(actionId);
            return entry.run(...args);
          },
        },
      ]),
    ),
  };
});

vi.mock("../components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    createElement(
      "div",
      { "data-testid": `terminal-${agentId}`, role: "group", "aria-label": `${label} terminal` },
      createElement("textarea", { className: "xterm-helper-textarea", "aria-label": `${label} terminal input` }),
    )
  ),
}));

import { DeckShell } from "../App";
import { saysCommand } from "./voiceActions";

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      document = structuredClone(next);
      return structuredClone(document);
    }),
  };
}

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const snapshot = overrides.snapshot ?? createFixtureSnapshot("connected");
  const settings = settingsStore();
  return {
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
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
    ...overrides,
  } as DeckRuntimeState;
}

function renderDeck(overrides: Partial<DeckRuntimeState> = {}) {
  return render(createElement(DeckShell, { runtime: runtime(overrides), initialView: { kind: "deck" } }));
}

function openPalette() {
  fireEvent.keyDown(document.body, { key: "k", metaKey: true });
  expect(screen.getByRole("dialog", { name: "Command menu" })).toBeVisible();
  registryDispatch.mockClear();
}

function clickPaletteEntry(label: string | RegExp) {
  fireEvent.click(within(screen.getByRole("dialog", { name: "Command menu" })).getByRole("button", { name: label }));
}

function expectOneRegistryDispatch(actionId?: string) {
  expect(registryDispatch).toHaveBeenCalledTimes(1);
  if (actionId) expect(registryDispatch).toHaveBeenCalledWith(actionId);
}

// Exercise the actual shell, voice panel and registry. Only the microphone
// and remote intent answer are synthetic, as in VoiceControlPanel.test.tsx.
function renderDashboardVoice(said: string, initial: "deck" | "overview" = "overview", outcome?: VoiceResultDto["outcome"]) {
  let started = false;
  let delivered = false;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({
    state, capturedMs: 1240, maxMs: 30_000, capped: false, available: true, backend: "remote",
  });
  const resolveVoice = vi.fn(async (): Promise<VoiceResultDto> => ({
    outcome: outcome ?? {
      kind: "dispatch", transcript: said, action: "clear_dashboard_filter",
      invoke: "clearDashboardFilter", params: [], sentence: "Showing all agents.",
    }, resolveMs: null, backend: "stub",
  }));
  const snapshot = createFixtureSnapshot("connected");
  snapshot.agents = snapshot.agents.map((agent, at) => ({ ...agent, displayName: `filter-voice-agent-${at + 1}`, agentType: at === 0 ? "codex" : "claude_code" }));
  const deck = runtime({
    snapshot,
    resolveVoice,
    voiceStatus: vi.fn(async () => {
      if (started && !delivered) { delivered = true; return status("done"); }
      return status(started ? "recording" : "idle");
    }),
    voiceStart: vi.fn(async () => { started = true; return status("recording"); }),
    voiceStop: vi.fn(async () => ({ outcome: { kind: "heard" as const, transcript: said, sentence: `Heard: ${said}.` }, transcribeMs: null, backend: "remote" as const, audioMs: 1240 })),
    voiceCancel: vi.fn(async () => status("idle")),
  });
  render(createElement(DeckShell, { runtime: deck, initialView: { kind: initial } }));
  return { deck, resolveVoice };
}

async function speakDashboardCommand(resolveVoice: ReturnType<typeof vi.fn>) {
  fireEvent.click(screen.getByTestId("voice-trigger"));
  await waitFor(() => expect(resolveVoice).toHaveBeenCalledTimes(1));
}

describe("VOICE_ACTIONS", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/?fixture=1&experimental=1");
    window.localStorage.clear();
    window.sessionStorage.clear();
    registryDispatch.mockClear();
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  /**
   * Scenario: load the frontend action registry and enumerate it the way the
   * structural guard will. Every entry the command table names is callable, and
   * every entry at all is explicitly classified for voice.
   *
   * The list is by VALUE rather than derived, which is the same choice the Rust
   * side makes about the shipped row set: a test that read the flags back off
   * the registry would pass while the registry said something else. Its cost is
   * one line per new voice-reachable entry, and that is what it is for. (Its
   * name said *four* while there were five, which is why it no longer counts
   * them.)
   */
  it("exports a literal registry with every command-table action and total voice classification", async () => {
    const { VOICE_ACTIONS } = await vi.importActual<{ VOICE_ACTIONS: Record<string, RegistryEntry> }>("./voiceActions");
    const voiceActionIds = [
      "openAgent",
      "openOverview",
      "filterDashboard",
      "clearDashboardFilter",
      "openDeck",
      "switchDeck",
      // `closeAgentView` is deliberately NOT here any more: `close` names
      // `closeTopmost`, which decides between the overlay and the pane and then
      // calls the context member. The entry keeps a `no_voice` reason saying
      // exactly that, and the total-classification loop below is what checks it.
      "closeTopmost",
      "openSettings",
      "stopVoice",
      "showVoiceCommands",
      "dictateToAgent",
      "submitAgentPrompt",
      "interruptAgent",
      "clearAgentPrompt",
      "scratchLastDictation",
      // PRD #1223: the `open_new_agent` row.
      "openNewAgent",
      // PRD #1223: the directory browser's rows.
      "openDirectory",
      "goToParentDirectory",
      "useThisDirectory",
      // PR #1451 round 3, change 5: the browser's Filter box.
      "filterDirectories",
      "clearDirectoryFilter",
      // PR #1451 round 3, change 4: turning the page of a paged list.
      "nextPage",
      "previousPage",
      // Issue #1492: scrolling the agent dashboard.
      "scrollDown",
      "scrollUp",
      "scrollToTop",
      "scrollToBottom",
      // PR #1451 round 4, D8: the New agent form's Command field.
      "setNewAgentCommand",
      // PRD #1401: an agent's pull request in the app's own browser.
      "openPullRequest",
      "openPullRequestInBrowser",
    ];

    expect(Array.isArray(VOICE_ACTIONS)).toBe(false);
    expect(Object.getPrototypeOf(VOICE_ACTIONS)).toBe(Object.prototype);
    for (const actionId of voiceActionIds) {
      expect(VOICE_ACTIONS).toHaveProperty(actionId);
      expect(typeof VOICE_ACTIONS[actionId].run).toBe("function");
      expect(VOICE_ACTIONS[actionId].voice).toBe(true);
      expect(VOICE_ACTIONS[actionId].no_voice).toBeUndefined();
    }

    for (const [actionId, entry] of Object.entries(VOICE_ACTIONS)) {
      expect(typeof entry.run, `${actionId} has no callable`).toBe("function");
      const noVoiceReason = typeof entry.no_voice === "string" ? entry.no_voice.trim() : "";
      expect(
        entry.voice === true || noVoiceReason.length > 0,
        `${actionId} must carry voice: true or a non-empty no_voice reason`,
      ).toBe(true);
      expect(
        entry.voice === true && noVoiceReason.length > 0,
        `${actionId} cannot be both voice-enabled and excluded from voice`,
      ).toBe(false);
    }
  });

  /// Scenario: each clear phrase is heard while a text filter hides every agent except one. The actual voice dispatch restores the full dashboard and removes its active-filter controls.
  it.each(["show everything", "show all agents", "clear the filter", "remove the filter", "reset"])(
    "clears an active dashboard filter by voice: %s", async (said) => {
      const { deck, resolveVoice } = renderDashboardVoice(said);
      const wanted = deck.snapshot.agents[0];
      fireEvent.change(screen.getByRole("textbox", { name: "Filter agents" }), { target: { value: wanted.displayName } });
      expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(1);
      await speakDashboardCommand(resolveVoice);
      await waitFor(() => expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(deck.snapshot.agents.length));
      expect(screen.getByRole("textbox", { name: "Filter agents" })).toHaveValue("");
      expect(screen.queryByRole("button", { name: "Show all" })).not.toBeInTheDocument();
    },
  );

  /// Scenario: say show everything from the deck with no filter active. It opens the ordinary unfiltered dashboard just as the existing overview command does.
  it("opens the dashboard from another screen when show everything has no active filter", async () => {
    const { deck, resolveVoice } = renderDashboardVoice("show everything", "deck");
    await speakDashboardCommand(resolveVoice);
    await waitFor(() => expect(screen.getByTestId("overview-table-region")).toBeVisible());
    expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(deck.snapshot.agents.length);
  });

  /// Scenario: say show the dashboard from Daemons using the existing overview voice row. The same synthetic microphone and actual dispatch path still open every agent, proving the new clear-command failures are not capture failures.
  it("keeps the existing overview voice dispatch working without a filter", async () => {
    const { deck, resolveVoice } = renderDashboardVoice("show the dashboard", "deck", {
      kind: "dispatch", transcript: "show the dashboard", action: "open_overview", invoke: "openOverview", params: [], sentence: "Opening the agent dashboard.",
    });
    await speakDashboardCommand(resolveVoice);
    await waitFor(() => expect(screen.getByTestId("overview-table-region")).toBeVisible());
    expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(deck.snapshot.agents.length);
  });

  /// Scenario: activate a filter, visit the deck, then say show everything there. Returning by voice clears the window's previous filter as well as opening the dashboard.
  it("clears a retained filter and opens the dashboard from another screen", async () => {
    const { deck, resolveVoice } = renderDashboardVoice("show everything");
    fireEvent.change(screen.getByRole("textbox", { name: "Filter agents" }), { target: { value: deck.snapshot.agents[0].displayName } });
    expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Daemons" }));
    expect(screen.queryByTestId("overview-table-region")).not.toBeInTheDocument();
    await speakDashboardCommand(resolveVoice);
    await waitFor(() => expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(deck.snapshot.agents.length));
    expect(screen.getByRole("textbox", { name: "Filter agents" })).toHaveValue("");
  });

  /// Scenario: activate a dashboard filter, visit the Daemons screen, then return to Dashboard within the same window session. The chosen facet and its visible row survive that navigation until explicitly cleared.
  it("retains the dashboard filter across navigation within the window session", () => {
    const { deck } = renderDashboardVoice("show everything");
    const text = deck.snapshot.agents[0].displayName;
    fireEvent.change(screen.getByRole("textbox", { name: "Filter agents" }), { target: { value: text } });
    expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Daemons" }));
    fireEvent.click(screen.getByRole("button", { name: "Dashboard" }));
    expect(screen.getByRole("textbox", { name: "Filter agents" })).toHaveValue(text);
    expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(1);
    expect(screen.getByRole("button", { name: "Show all" })).toBeVisible();
  });

  /// Scenario: ask for Codex agents on the unfiltered dashboard. The resolved type facet reaches the same header filter as a click and hides every other type.
  it("applies the voice type facet to the displayed dashboard", async () => {
    const { deck, resolveVoice } = renderDashboardVoice("show Codex agents", "overview", {
      kind: "dispatch", transcript: "show Codex agents", action: "filter_dashboard", invoke: "filterDashboard",
      params: [{ name: "agent_type", kind: "agent_type_ref", spoken: "Codex", value: "codex", label: "Codex" }],
      sentence: "Showing Codex agents.",
    });
    await speakDashboardCommand(resolveVoice);
    await waitFor(() => expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(1));
    expect(screen.getByTestId(`overview-agent-${agentDomKey(deck.snapshot.agents[0])}`)).toBeVisible();
    expect(screen.getByRole("button", { name: "Show all" })).toBeVisible();
  });

  /// Scenario: filter away the earlier row, then say one with the actual voice panel listening. The first remaining row's pane opens through the locally resolved number instead of the first agent in the original fleet.
  it("opens the filtered first row when its number is spoken", async () => {
    const { deck } = renderDashboardVoice("one");
    const wanted = deck.snapshot.agents[1];
    fireEvent.change(screen.getByRole("textbox", { name: "Filter agents" }), { target: { value: wanted.displayName } });
    expect(screen.getAllByTestId(/^overview-agent-/)).toHaveLength(1);
    fireEvent.click(screen.getByTestId("voice-trigger"));
    await waitFor(() => expect(screen.getByTestId(`terminal-${wanted.id}`)).toBeVisible());
    expect(screen.queryByTestId(`terminal-${deck.snapshot.agents[0].id}`)).not.toBeInTheDocument();
  });

  /** Scenario: inspect the three prompt-control entries that the typing-mode command rows invoke. Each declares its panel callable as a required capability. */
  it.each(["interruptAgent", "clearAgentPrompt", "scratchLastDictation"])(
    "declares the %s prompt-control callable in needs",
    async (actionId) => {
      const { VOICE_ACTIONS } = await vi.importActual<{
        VOICE_ACTIONS: Record<string, RegistryEntry & { needs: string[] }>;
      }>("./voiceActions");
      expect(VOICE_ACTIONS).toHaveProperty(actionId);
      expect(VOICE_ACTIONS[actionId].needs).toEqual([actionId]);
      expect(typeof VOICE_ACTIONS[actionId].run).toBe("function");
      expect(VOICE_ACTIONS[actionId].voice).toBe(true);
    },
  );

  /**
   * Scenario: choose a configured remote daemon in the header selector. The
   * selected name changes and the settings document records that daemon through
   * the same switchDeck action voice can dispatch.
   */
  it("dispatches a header daemon selection through switchDeck", async () => {
    const remoteId = "a1b2c3d4e5f60718";
    const saveSettings = vi.fn(async (next: DesktopSettingsDto) => structuredClone(next));
    renderDeck({
      getSettings: vi.fn(async () => ({
        settings: {
          ...structuredClone(DEFAULT_DESKTOP_SETTINGS),
          endpoints: {
            selection: "local",
            remote: [{ id: remoteId, host: "build-box.example.com", user: "vf", port: 22 }],
          },
        },
      })),
      saveSettings,
    });
    fireEvent.click(screen.getByTestId("deck-selector-toggle"));
    const menu = await screen.findByTestId("deck-selector-menu");
    // Waited for: the menu can open before `getSettings` lands, and only then
    // does it list the remote deck.
    fireEvent.click(await within(menu).findByTestId(`deck-selector-option-${remoteId}`));

    await waitFor(() => expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("vf@build-box.example.com"));
    expect(saveSettings).toHaveBeenCalledTimes(1);
    expect(saveSettings.mock.calls[0][0].endpoints?.selection).toBe(remoteId);
    expectOneRegistryDispatch("switchDeck");
  });

  /**
   * Scenario: choose the already selected local daemon in the header selector.
   * The name stays put and the switchDeck action reports a no-op by leaving
   * the settings document unwritten.
   */
  it("dispatches the selected daemon through switchDeck without rewriting settings", async () => {
    const saveSettings = vi.fn(async (next: DesktopSettingsDto) => structuredClone(next));
    renderDeck({ saveSettings });
    fireEvent.click(screen.getByTestId("deck-selector-toggle"));
    const menu = await screen.findByTestId("deck-selector-menu");
    fireEvent.click(within(menu).getByTestId("deck-selector-option-local"));

    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("This machine");
    expect(saveSettings).not.toHaveBeenCalled();
    expectOneRegistryDispatch("switchDeck");
  });

  /**
   * Scenario: click each control in the daemon's primary rail from a state where
   * its result is visible. Every click crosses the shared registry once and
   * still opens or closes the same screen or overlay the user sees today.
   */
  it.each([
    ["Projects", "projects-panel"],
    ["Prompts", "prompt-library-panel"],
    ["Orchestrations", "orchestration-editor"],
    ["Agent Profiles", "agent-profiles-panel"],
    ["Settings", "settings-panel"],
  ])("dispatches the %s deck-rail button through the registry", (label, testId) => {
    renderDeck();
    fireEvent.click(screen.getByRole("button", { name: label }));

    expect(screen.getByTestId(testId)).toBeVisible();
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: open a panel over the Daemons screen and then click Daemons in
   * the primary rail. The panel disappears, the Daemons screen remains visible,
   * and the reset action crossed the registry rather than closing the booleans
   * beside it.
   */
  it("dispatches the Daemons rail button through the registry", () => {
    renderDeck();
    fireEvent.click(screen.getByRole("button", { name: "Projects" }));
    expect(screen.getByTestId("projects-panel")).toBeVisible();
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Daemons" }));

    expect(screen.queryByTestId("projects-panel")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: click Overview in the daemon rail. The agent grid is replaced by
   * the fleet overview, and the transition is the openOverview registry action
   * that voice will dispatch too.
   */
  it("dispatches the Overview deck-rail button through openOverview", () => {
    renderDeck();
    fireEvent.click(screen.getByRole("button", { name: "Dashboard" }));

    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(screen.queryByTestId("agent-tile-planner")).not.toBeInTheDocument();
    expectOneRegistryDispatch("openOverview");
  });

  /**
   * Scenario: start on the overview and click each of its two rail buttons.
   * The active Overview control stays on the overview through openOverview,
   * while Deck returns to the terminal grid through openDeck.
   */
  it.each([
    ["Dashboard", "openOverview", "overview-table-region"],
    ["Daemons", "openDeck", "agent-tile-planner"],
  ])("dispatches the %s overview-rail button through %s", (label, actionId, resultTestId) => {
    render(createElement(DeckShell, { runtime: runtime(), initialView: { kind: "overview" } }));
    fireEvent.click(screen.getByRole("button", { name: label }));

    expect(screen.getByTestId(resultTestId)).toBeVisible();
    expectOneRegistryDispatch(actionId);
  });

  /**
   * Scenario: choose each static overlay entry from the command palette. The
   * named overlay appears after the palette closes, and each selection crosses
   * exactly one registry entry rather than calling its setter directly.
   */
  it.each([
    [/Manage projects/, "projects-panel"],
    [/Open prompt library/, "prompt-library-panel"],
    [/Open agent profiles/, "agent-profiles-panel"],
    [/Edit orchestration order/, "orchestration-editor"],
    [/Open settings/, "settings-panel"],
  ])("dispatches the %s palette entry through the registry", (label, testId) => {
    renderDeck();
    openPalette();
    clickPaletteEntry(label);

    expect(screen.queryByRole("dialog", { name: "Command menu" })).not.toBeInTheDocument();
    expect(screen.getByTestId(testId)).toBeVisible();
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: choose Show events drawer from a daemon whose drawer starts
   * closed. The evidence appears after the palette closes and the toggle is
   * performed by one registry action.
   */
  it("dispatches the evidence palette entry through the registry", () => {
    renderDeck();
    expect(screen.queryByTestId("evidence-drawer")).not.toBeInTheDocument();
    openPalette();
    clickPaletteEntry(/Show events drawer/);

    expect(screen.getByTestId("evidence-drawer")).toBeVisible();
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: choose every generated Focus-agent entry from the palette, after
   * arranging for its target not to be selected already. The requested tile
   * becomes the visible selection and each generated entry uses the registry.
   */
  it.each([
    ["Planner", "planner", "2"],
    ["Builder", "builder", "1"],
    ["Reviewer", "reviewer", "1"],
    ["Tester", "tester", "1"],
  ])("dispatches the Focus %s palette entry through the registry", (role, agentId, initialSelectionKey) => {
    renderDeck();
    fireEvent.keyDown(document.body, { key: initialSelectionKey });
    expect(screen.getByTestId(`agent-tile-${agentId}`).className).not.toContain("is-selected");
    openPalette();
    clickPaletteEntry(new RegExp(`Focus ${role}`));

    expect(screen.getByTestId(`agent-tile-${agentId}`).className).toContain("is-selected");
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: mark Builder as the live coordinator and choose Message
   * coordinator from the palette while Planner is selected. Builder's terminal
   * becomes selected through one registry dispatch and no message is sent.
   */
  it("dispatches the Message orchestrator palette entry through the registry", () => {
    const snapshot = createFixtureSnapshot("connected");
    snapshot.agents = snapshot.agents.map((agent) => ({ ...agent, isStartRole: agent.id === "builder" }));
    renderDeck({ mode: "live", snapshot, fleet: [snapshot] });
    expect(screen.getByTestId("agent-tile-planner").className).toContain("is-selected");
    openPalette();
    clickPaletteEntry(/Message orchestrator/);

    expect(screen.getByTestId("agent-tile-builder").className).toContain("is-selected");
    expect(screen.getByTestId("terminal-builder")).toBeVisible();
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: choose Advance fixture while the runtime refuses the action.
   * The user sees the refusal in a toast, and the attempted fixture advance
   * crossed the action registry once before reaching the runtime.
   */
  it("dispatches the Advance fixture palette entry through the registry", async () => {
    renderDeck({ runAction: vi.fn(async () => { throw new Error("Fixture advance refused for test"); }) });
    openPalette();
    clickPaletteEntry(/Advance fixture/);

    await waitFor(() => expect(screen.getByTestId("toast")).toHaveTextContent("Fixture advance refused for test"));
    expectOneRegistryDispatch();
  });

  /**
   * Scenario: open Planner's pane from the daemon and close it from the pane.
   * The same visible round trip now used by clicks dispatches openAgent and
   * closeAgentView. Only the first is named by a command-table row; the second
   * is the pane's X, which `close` reaches through `closeTopmost` once it has
   * decided nothing is on top of the pane — so this is the CLICK path, and it
   * is the reason that entry still exists after the row folded away.
   */
  it("dispatches the agent-pane round trip through the two voice navigation actions", () => {
    renderDeck();
    const planner = screen.getByTestId("agent-tile-planner");
    fireEvent.click(within(planner).getByRole("button", { name: "Open Planner agent" }));

    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expectOneRegistryDispatch("openAgent");
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expectOneRegistryDispatch("closeAgentView");
  });

  /**
   * Scenario: start on the fleet overview and click Planner's row. The pane
   * opens over the overview through openAgent, preserving the second existing
   * click path rather than only routing the daemon tile through the registry.
   */
  it("dispatches an overview-row open through openAgent", () => {
    const snapshot = createFixtureSnapshot("connected");
    render(createElement(DeckShell, { runtime: runtime({ snapshot, fleet: [snapshot] }), initialView: { kind: "overview" } }));
    fireEvent.click(screen.getByTestId(`overview-agent-${agentDomKey(snapshot.agents[0])}`));

    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expectOneRegistryDispatch("openAgent");
  });

  // -- PRD #1195 M1: the five second dispatch paths, closed ---------------
  //
  // Each of these controls used to call a state setter directly while a
  // registry entry wrapped the same state (PRD #802's deferred D10). Every
  // case asserts the visible effect AND the one registry dispatch, so a
  // control that went back to its setter would keep the first and lose the
  // second.

  /**
   * Scenario: press the mouse on Builder's tile while Planner is the selected
   * one. Builder becomes the selected tile, and the selection is the
   * focusAgent entry the palette's Focus items and the number keys dispatch.
   */
  it("dispatches an agent tile's own selection through focusAgent", () => {
    renderDeck();
    expect(screen.getByTestId("agent-tile-planner").className).toContain("is-selected");
    registryDispatch.mockClear();

    fireEvent.mouseDown(screen.getByTestId("agent-tile-builder"));

    expect(screen.getByTestId("agent-tile-builder").className).toContain("is-selected");
    expect(screen.getByTestId("agent-tile-planner").className).not.toContain("is-selected");
    expectOneRegistryDispatch("focusAgent");
  });

  /**
   * Scenario: click the workspace header's Events button twice. The drawer
   * opens and then closes again, each click being one toggleEvidenceDrawer
   * dispatch — the same entry the palette's events item runs.
   */
  it("dispatches the workspace header's Events button through toggleEvidenceDrawer", () => {
    renderDeck();
    expect(screen.queryByTestId("evidence-drawer")).not.toBeInTheDocument();
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Events" }));
    expect(screen.getByTestId("evidence-drawer")).toBeVisible();
    expectOneRegistryDispatch("toggleEvidenceDrawer");
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Events" }));
    expect(screen.queryByTestId("evidence-drawer")).not.toBeInTheDocument();
    expectOneRegistryDispatch("toggleEvidenceDrawer");
  });

  /**
   * Scenario: open Builder's Delegations tab and click one of its event rows,
   * first with the drawer closed and then with it already open. Both times the
   * drawer ends up open on that item — a row selects and SHOWS, it never
   * hides — and each click is one toggleEvidenceDrawer dispatch.
   */
  it("dispatches an event row's select-and-open through toggleEvidenceDrawer without flipping an open drawer shut", () => {
    renderDeck();
    const builder = screen.getByTestId("agent-tile-builder");
    fireEvent.click(within(builder).getByRole("tab", { name: "Delegations" }));
    expect(screen.queryByTestId("evidence-drawer")).not.toBeInTheDocument();
    registryDispatch.mockClear();

    fireEvent.click(within(builder).getByRole("button", { name: /Fixture exposed stale listener/ }));
    const drawer = screen.getByTestId("evidence-drawer");
    expect(drawer).toBeVisible();
    expect(within(drawer).getByRole("heading", { name: "Fixture exposed stale listener" })).toBeVisible();
    expectOneRegistryDispatch("toggleEvidenceDrawer");
    registryDispatch.mockClear();

    fireEvent.click(within(builder).getByRole("button", { name: /Validation recovered/ }));
    expect(screen.getByTestId("evidence-drawer")).toBeVisible();
    expect(within(screen.getByTestId("evidence-drawer")).getByRole("heading", { name: "Validation recovered" })).toBeVisible();
    expectOneRegistryDispatch("toggleEvidenceDrawer");
  });

  /**
   * Scenario: click the run graph's Edit loop button in its header. The
   * orchestration editor opens, through the openOrchestrationOrder entry the rail's
   * Orchestrations button dispatches.
   */
  it("dispatches the run graph's Edit loop button through openOrchestrationOrder", () => {
    renderDeck();
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Edit loop" }));

    expect(screen.getByTestId("orchestration-editor")).toBeVisible();
    expectOneRegistryDispatch("openOrchestrationOrder");
  });

  /**
   * Scenario: render a deck whose run graph reports no nodes and click the
   * Edit loop link inside the empty-state sentence. The orchestration editor opens
   * through openOrchestrationOrder, exactly as the header's button does.
   */
  it("dispatches the empty run graph's Edit loop link through openOrchestrationOrder", () => {
    const snapshot = { ...createFixtureSnapshot("connected"), stages: [] };
    renderDeck({ snapshot, fleet: [snapshot] });
    registryDispatch.mockClear();

    fireEvent.click(within(screen.getByText(/No workflow nodes reported/)).getByRole("button", { name: "Edit loop" }));

    expect(screen.getByTestId("orchestration-editor")).toBeVisible();
    expectOneRegistryDispatch("openOrchestrationOrder");
  });

  /**
   * Scenario: choose the one project a live deck offers in Projects and press
   * Configure orchestration. Projects closes and the orchestration editor opens on that
   * project's orchestration, the opening half being one openOrchestrationOrder dispatch.
   */
  it("dispatches Projects' Configure orchestration through openOrchestrationOrder", async () => {
    const resolveProject = vi.fn(async () => ({
      path: "/home/dev/code/clipmaker",
      displayPath: "/home/dev/code/clipmaker",
      displayName: "clipmaker",
      orchestrations: [{ name: "clipmaker-loop", displayName: "clipmaker-loop", default: true, roles: [{ name: "orchestrator", displayName: "orchestrator", start: true }] }],
      configRevision: "revision-1",
    }));
    renderDeck({
      mode: "live",
      listProjects: vi.fn(async () => ({ projects: [{ path: "/home/dev/current", displayPath: "/home/dev/current", displayName: "current" }], primary: "/home/dev/current" })),
      resolveProject,
    });
    fireEvent.click(screen.getByTestId("open-projects"));
    await waitFor(() => expect(screen.getByRole("button", { name: /current/ })).toBeVisible());
    fireEvent.click(screen.getByRole("button", { name: /current/ }));
    await waitFor(() => expect(screen.getByTestId("selected-project")).toBeVisible());
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Configure orchestration" }));

    expect(screen.queryByTestId("projects-panel")).not.toBeInTheDocument();
    expect(screen.getByTestId("orchestration-editor")).toBeVisible();
    expect(screen.getByLabelText("Orchestration name")).toHaveValue("clipmaker-loop");
    expectOneRegistryDispatch("openOrchestrationOrder");
  });

  /**
   * Scenario: open a live deck's orchestration editor with no project chosen and click the
   * Choose one link it shows. The editor closes and Projects opens in its
   * place, the opening half being one openProjects dispatch.
   */
  it("dispatches the orchestration editor's Choose one link through openProjects", async () => {
    renderDeck({ mode: "live", listProjects: vi.fn(async () => ({ projects: [] })) });
    fireEvent.click(screen.getByRole("button", { name: "Orchestrations" }));
    await waitFor(() => expect(screen.getByTestId("orchestration-needs-project")).toBeVisible());
    registryDispatch.mockClear();

    fireEvent.click(within(screen.getByTestId("orchestration-needs-project")).getByRole("button", { name: "Choose one" }));

    expect(screen.queryByTestId("orchestration-editor")).not.toBeInTheDocument();
    expect(screen.getByTestId("projects-panel")).toBeVisible();
    expectOneRegistryDispatch("openProjects");
  });

  /**
   * Scenario: render a connected deck with no agents and click the empty
   * state's Configure agents button. Agent profiles opens, through the
   * openAgentProfiles entry the rail's Agent Profiles button dispatches.
   */
  it("dispatches the empty deck's Configure agents button through openAgentProfiles", () => {
    const snapshot = createFixtureSnapshot("empty");
    renderDeck({ snapshot, fleet: [snapshot] });
    registryDispatch.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Configure agents" }));

    expect(screen.getByTestId("agent-profiles-panel")).toBeVisible();
    expectOneRegistryDispatch("openAgentProfiles");
  });

  /**
   * Scenario: activate a live orchestration whose project the daemon no longer knows
   * by the time Activate is confirmed. The editor closes, Projects reopens with
   * the sentence saying why, and the reopening is the openProjects entry
   * rather than the panel's setter.
   */
  it("reopens Projects through openProjects when an activation finds its project gone", async () => {
    const roles = ["orchestrator", "coder", "reviewer", "auditor", "tester", "release"];
    renderDeck({
      mode: "live",
      listProjects: vi.fn(async () => ({ projects: [{ path: "/home/dev/code/deck", displayPath: "/home/dev/code/deck", displayName: "deck" }], primary: "/home/dev/code/deck" })),
      resolveProject: vi.fn(async () => ({
        path: "/home/dev/code/deck",
        displayPath: "/home/dev/code/deck",
        displayName: "deck",
        orchestrations: [{ name: "dot-agent-deck", displayName: "dot-agent-deck", default: true, roles: roles.map((role) => ({ name: role, displayName: role, start: role === "orchestrator" })) }],
        configRevision: "revision-1",
      })),
      runAction: vi.fn(async (action: { type: string }) => {
        if (action.type === "activate_orchestration") throw new Error("daemon returned error: unresolved: that path is not a project this daemon can offer");
        return { ok: true } as DeckActionResult;
      }) as DeckRuntimeState["runAction"],
    });
    fireEvent.click(screen.getByTestId("open-projects"));
    await waitFor(() => expect(screen.getByRole("button", { name: /deck/ })).toBeVisible());
    fireEvent.click(screen.getByRole("button", { name: /deck/ }));
    await waitFor(() => expect(screen.getByTestId("selected-project")).toBeVisible());
    fireEvent.click(screen.getByRole("button", { name: "Configure orchestration" }));
    fireEvent.change(screen.getByLabelText("Task prompt"), { target: { value: "Build it." } });
    fireEvent.click(screen.getByTestId("activate-orchestration"));
    registryDispatch.mockClear();

    fireEvent.click(screen.getAllByRole("button", { name: "Activate orchestration" }).at(-1)!);

    await waitFor(() => expect(screen.getByTestId("projects-panel")).toBeVisible());
    expect(screen.queryByTestId("orchestration-editor")).not.toBeInTheDocument();
    expect(screen.getByTestId("toast")).toHaveTextContent("That project is no longer one this daemon knows");
    expectOneRegistryDispatch("openProjects");
  });
});

/**
 * PRD #1401 — the pull request browser's entries, driven through the one
 * dispatch seam voice uses.
 */
describe("the pull request browser by voice (PRD #1401)", () => {
  const target = { deckId: "deck-a", agentId: "7", from: "overview" as const };
  const base = () => ({
    navigate: vi.fn(),
    closeAgentView: vi.fn(),
    reportNothingToClose: vi.fn(),
    reportRefused: vi.fn(),
  });

  /** Scenario: "close" with the browser on top closes the browser, not the pane under it nor the voice overlay. */
  it("puts the browser at the top of close's order", async () => {
    const { dispatchVoiceAction } = await vi.importActual<typeof import("./voiceActions")>("./voiceActions");
    const context = { ...base(), closePullRequest: vi.fn(), dismissVoiceOverlay: vi.fn(), closeSettings: vi.fn() };

    expect(dispatchVoiceAction("closeTopmost", context, { ...target, agentViewOpen: true })).toBe(true);
    expect(context.closePullRequest).toHaveBeenCalledTimes(1);
    expect(context.dismissVoiceOverlay).not.toHaveBeenCalled();
    expect(context.closeAgentView).not.toHaveBeenCalled();
    expect(context.closeSettings).not.toHaveBeenCalled();
  });

  /** Scenario: with the browser not published (closed, or covered), close falls through as before. */
  it("leaves close's order alone without the browser", async () => {
    const { dispatchVoiceAction } = await vi.importActual<typeof import("./voiceActions")>("./voiceActions");
    const context = { ...base(), dismissVoiceOverlay: vi.fn() };

    dispatchVoiceAction("closeTopmost", context, { ...target, agentViewOpen: true });
    expect(context.dismissVoiceOverlay).toHaveBeenCalledTimes(1);
    expect(context.closeAgentView).not.toHaveBeenCalled();
  });

  /** Scenario: "open the PR" on an agent with none is refused in the app's own sentence. */
  it("opens the pane's agent's pull request, or says why not", async () => {
    const { dispatchVoiceAction } = await vi.importActual<typeof import("./voiceActions")>("./voiceActions");
    const opened = { ...base(), openPullRequest: vi.fn(() => undefined) };
    expect(dispatchVoiceAction("openPullRequest", opened, target)).toBe(true);
    expect(opened.openPullRequest).toHaveBeenCalledWith({ deckId: "deck-a", agentId: "7" });
    expect(opened.reportRefused).not.toHaveBeenCalled();

    const refused = { ...base(), openPullRequest: vi.fn(() => "Coder has no pull request.") };
    dispatchVoiceAction("openPullRequest", refused, target);
    expect(refused.reportRefused).toHaveBeenCalledWith("Coder has no pull request.");
  });

  /** Scenario: "open it in the browser" hands the page off, or says no pull request is open. */
  it("hands the open pull request to the system browser, or says none is open", async () => {
    const { dispatchVoiceAction } = await vi.importActual<typeof import("./voiceActions")>("./voiceActions");
    const handed = { ...base(), openPullRequestInBrowser: vi.fn(() => undefined) };
    expect(dispatchVoiceAction("openPullRequestInBrowser", handed, target)).toBe(true);
    expect(handed.openPullRequestInBrowser).toHaveBeenCalledTimes(1);

    const none = { ...base(), openPullRequestInBrowser: vi.fn(() => "No pull request is open.") };
    dispatchVoiceAction("openPullRequestInBrowser", none, target);
    expect(none.reportRefused).toHaveBeenCalledWith("No pull request is open.");
  });

  /** A host that does not serve the browser refuses the row rather than throwing. */
  it("refuses the browser's rows where nothing serves them", async () => {
    const { dispatchVoiceAction } = await vi.importActual<typeof import("./voiceActions")>("./voiceActions");
    expect(dispatchVoiceAction("openPullRequest", base(), target)).toBe(false);
    expect(dispatchVoiceAction("openPullRequestInBrowser", base(), target)).toBe(false);
  });
});

describe("saysCommand (PR #1451 round 4, audit A1)", () => {
  /** Scenario: a sentence that says "command" is about the Command field, whatever case or punctuation the transcriber used; "commands" and a word that merely contains it are not. */
  it("reads the singular word command and nothing else", () => {
    expect(["Set the command to devbox run agent.", "COMMAND: bash", "start it with the command npm test"].map(saysCommand)).toEqual([true, true, true]);
    expect(["what commands can I say", "start it", "commander", ""].map(saysCommand)).toEqual([false, false, false, false]);
  });
});
