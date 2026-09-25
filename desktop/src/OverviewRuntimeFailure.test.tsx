import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { LaunchCleanupError } from "./lib/actionError";
import { DEFAULT_DESKTOP_SETTINGS as DEFAULT_SETTINGS, fixtureDesktopFeatures, type DeckBridge } from "./lib/bridge";
import type { DeckDirectoryListing, NewAgentOptions } from "./types";

/**
 * PRD #1223 audit W2 — the deck's toast was the ONLY surface rendering
 * `runtime.error`, and the overview mounts *instead of* the deck.
 *
 * The New agent flow lives on the overview, and its dialog deliberately leaves
 * the runtime's error alone once it is unmounted, because by then that error is
 * the only copy of the failure — and, for a launch whose rollback could not
 * confirm every stop, the only place the user can learn that roles may still be
 * running. So a delayed failure arriving while the user is back on the overview
 * was reported nowhere, and the overview's Refresh calls `reconnect()`, which
 * clears it unseen.
 *
 * # Why the REAL runtime, and not a runtime fake with `error` prefilled
 *
 * `App.test.tsx` has that test already (`shows the runtime's unconfirmed roles
 * on the toast`) and it renders `ControlDeck` — the deck — with the state
 * handed to it. It cannot fail for this defect, because the defect is *which
 * screen is mounted when the failure lands*. This file drives the real hook
 * over a stub bridge so the failure travels the way it does in the app: a start
 * the user submitted, answered after the dialog that submitted it is gone.
 */
const HOME: DeckDirectoryListing = { kind: "listing", path: "/home/dev", displayPath: "/home/dev", parent: "/home", entries: [], truncated: false };
const OPTIONS: NewAgentOptions = { kind: "deck", agents: [], experimental: false, authoringKinds: [], lastCommand: "claude" };

const { bridge } = vi.hoisted(() => ({
  bridge: {
    mode: "fixture",
    desktopFeatures: vi.fn(async () => fixtureDesktopFeatures("?experimental=1")),
    connect: vi.fn(),
    subscribe: vi.fn(async () => () => {}),
    runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => {}),
    resizeTerminal: vi.fn(async () => {}),
    onTerminalGeometry: vi.fn(() => () => {}),
    setZoom: vi.fn(async (level: number) => level),
    // The real document, because `useZoom` reads `settings.zoom.level` on
    // the first render of `DeckShell` and an empty one throws there.
    getSettings: vi.fn(async () => ({ settings: { ...DEFAULT_SETTINGS } })),
    saveSettings: vi.fn(async (settings: unknown) => settings),
    testEndpoint: vi.fn(),
    secretStatus: vi.fn(async () => ({ stored: false })),
    storeSecret: vi.fn(async () => {}),
    forgetSecret: vi.fn(async () => {}),
    declareVoiceScreen: vi.fn(async () => {}),
    resolveVoice: vi.fn(),
    voiceCommands: vi.fn(async () => []),
    voiceStart: vi.fn(async () => {}),
    voiceStop: vi.fn(async () => {}),
    voiceStatus: vi.fn(),
    voiceCancel: vi.fn(async () => {}),
    setShownTerminals: vi.fn(async () => {}),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    listDirectories: vi.fn(async () => structuredClone(HOME)),
    newAgentOptions: vi.fn(async () => structuredClone(OPTIONS)),
    newAgentOrchestrations: vi.fn(async () => ({ kind: "orchestrations" as const, path: "/home/dev", orchestrations: [], live: [] })),
    dispose: vi.fn(async () => {}),
  },
}));

vi.mock("./lib/bridge", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/bridge")>()),
  selectRuntimeMode: () => "fixture" as const,
  createDeckBridge: () => bridge as unknown as DeckBridge,
}));

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";
import { useDeckRuntime } from "./hooks/useDeckRuntime";

/** The app as `App` mounts it, opened on the overview. */
function App() {
  return <DeckShell runtime={useDeckRuntime()} initialView={{ kind: "overview" }} />;
}

describe("a launch failure that lands while the overview is up (PRD #1223 audit W2)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // ONE deck, so the dialog's deck step preselects it and a single Enter
    // confirms it — the fleet size is not what this test is about.
    bridge.connect.mockResolvedValue([createFixtureSnapshot("connected")]);
    bridge.subscribe.mockResolvedValue(() => {});
    bridge.onTerminalGeometry.mockReturnValue(() => {});
    bridge.listDirectories.mockImplementation(async () => structuredClone(HOME));
    bridge.newAgentOptions.mockImplementation(async () => structuredClone(OPTIONS));
  });

  /**
   * Scenario: on the overview, open New agent, pick the deck and its home
   * directory and press Start. While the deck is still thinking, leave for the
   * deck screen and come back — which unmounts the dialog, so the runtime's
   * error becomes the only copy of the failure. The start then fails with roles
   * its rollback could not confirm stopped. Those roles must be on screen,
   * named, before the user touches Refresh.
   */
  it("shows the unconfirmed roles on the overview, without a refresh", async () => {
    let rejectStart: ((cause: unknown) => void) | undefined;
    bridge.runAction.mockImplementation(() => new Promise((_resolve, reject) => { rejectStart = reject; }));
    render(<App />);
    await waitFor(() => expect(screen.getByTestId("overview-new-agent")).toBeTruthy());

    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.keyDown(screen.getByTestId("new-agent-deck-list"), { key: "Enter" });
    fireEvent.keyDown(await screen.findByTestId("new-agent-directory-list"), { key: " " });
    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("claude"));
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await waitFor(() => expect(bridge.runAction).toHaveBeenCalled());

    // Away and back. The dialog goes with the screen, which is the state the
    // runtime holds these roles for at all.
    fireEvent.click(screen.getByTestId("overview-open-deck"));
    fireEvent.click(await screen.findByTestId("open-overview"));
    await waitFor(() => expect(screen.queryByTestId("new-agent-dialog")).toBeNull());

    await act(async () => {
      rejectStart?.(new LaunchCleanupError(
        "failed to start orchestration role coder: refused; cleanup could not confirm stop for 2 of 2 already-started role(s): orchestrator (agent-0: stop refused), planner (agent-1: stop refused)",
        ["orchestrator", "planner"],
      ));
      await Promise.resolve();
    });

    const toast = await screen.findByTestId("toast");
    const warning = within(toast).getByTestId("toast-cleanup-warning");
    expect(warning).toHaveTextContent("2 roles may still be running on this deck");
    expect(warning).toHaveTextContent("orchestrator");
    expect(warning).toHaveTextContent("planner");
    expect(toast).toHaveTextContent("failed to start orchestration role");
    // On the overview, and before any Refresh — the button that would have
    // erased this is still untouched.
    expect(screen.getByTestId("overview-new-agent")).toBeTruthy();
    expect(bridge.connect).toHaveBeenCalledTimes(1);
  });
});
