import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { agentDomKey } from "../components/AgentOverview";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto } from "./bridge";
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

describe("VOICE_ACTIONS", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/?fixture=1&experimental=1");
    window.localStorage.clear();
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
      "openDeck",
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
      // PRD #1223: the `open_new_agent` row.
      "openNewAgent",
      // PRD #1223: the directory browser's rows.
      "openDirectory",
      "goToParentDirectory",
      "useThisDirectory",
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
});
