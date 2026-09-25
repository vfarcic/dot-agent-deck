import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
import type { DeckRuntimeState } from "./types";

/** The runtime the last render of {@link App} handed the shell, for tests that drive it directly. */
let latestRuntime: DeckRuntimeState | undefined;

/** The app as `App` mounts it, opened on the overview. */
function App() {
  const runtime = useDeckRuntime();
  latestRuntime = runtime;
  return <DeckShell runtime={runtime} initialView={{ kind: "overview" }} />;
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
    const warning = screen.getByTestId("toast-cleanup-warning");
    expect(warning).toHaveTextContent("2 roles may still be running on Local deck");
    expect(warning).toHaveTextContent("orchestrator");
    expect(warning).toHaveTextContent("planner");
    expect(toast).toHaveTextContent("failed to start orchestration role");
    // On the overview, and before any Refresh — the button that would have
    // erased this is still untouched.
    expect(screen.getByTestId("overview-new-agent")).toBeTruthy();
    expect(bridge.connect).toHaveBeenCalledTimes(1);
  });
});

describe("an unconfirmed-stop warning outlives the failure slot (issue #1234)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    latestRuntime = undefined;
    bridge.connect.mockResolvedValue([createFixtureSnapshot("connected")]);
    bridge.subscribe.mockResolvedValue(() => {});
    bridge.onTerminalGeometry.mockReturnValue(() => {});
  });

  /**
   * Scenario: two actions are in flight on the overview. The first rejects
   * with roles its rollback could not confirm stopped, and the second rejects
   * ordinarily straight after it, in the same React batch. The screen must
   * show the second failure's sentence AND the first one's roles — they used
   * to share one slot, so the ordinary failure replaced the warning before any
   * frame drew it. Refresh (`reconnect()`) and dismissing the message must
   * leave the warning up; its own dismiss ends it.
   */
  it("shows a cleanup warning that an ordinary failure landed on top of in the same batch", async () => {
    let rejectLaunch: ((cause: unknown) => void) | undefined;
    let rejectOther: ((cause: unknown) => void) | undefined;
    bridge.runAction
      .mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectLaunch = reject; }))
      .mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectOther = reject; }));
    render(<App />);
    await waitFor(() => expect(screen.getByTestId("overview-new-agent")).toBeTruthy());

    await act(async () => {
      const launch = latestRuntime!.runAction({ type: "pause_run" }).catch(() => {});
      const other = latestRuntime!.runAction({ type: "pause_run" }).catch(() => {});
      rejectLaunch?.(new LaunchCleanupError(
        "failed to start orchestration role coder: refused; cleanup could not confirm stop for 1 of 1 already-started role(s): orchestrator (agent-0: stop refused)",
        ["orchestrator"],
      ));
      rejectOther?.(new Error("daemon returned error: publish-failed"));
      await Promise.all([launch, other]);
    });

    expect(screen.getByTestId("toast")).toHaveTextContent("publish-failed");
    const warning = screen.getByTestId("toast-cleanup-warning");
    expect(warning).toHaveTextContent("1 role may still be running on Local deck");
    expect(warning).toHaveTextContent("orchestrator");

    // Refresh clears the sentence slot, and a successful one leaves it empty.
    await act(async () => {
      await latestRuntime!.reconnect();
    });
    expect(screen.queryByTestId("toast")).toBeNull();
    expect(screen.getByTestId("toast-cleanup-warning")).toHaveTextContent("orchestrator");

    fireEvent.click(screen.getByLabelText("Dismiss cleanup warning"));
    await waitFor(() => expect(screen.queryByTestId("toast-cleanup-warning")).toBeNull());
  });
});
