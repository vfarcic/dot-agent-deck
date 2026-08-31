import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { WINDOWS_WORKFLOW_BLOCK_REASON } from "./lib/platform";
import type { DeckRuntimeState } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { ControlDeck } from "./App";

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  return {
    mode: "fixture",
    snapshot: createFixtureSnapshot("connected"),
    terminalData: {},
    runAction: vi.fn(async () => ({ ok: true }) as import("./types").DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    ...overrides,
  };
}

describe("ControlDeck", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: true,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })));
  });

  afterEach(() => vi.unstubAllGlobals());

  it("renders the deterministic four-agent cockpit with evidence and stable test seams", () => {
    render(<ControlDeck runtime={runtime()} />);
    expect(screen.getByText("DEMO DATA")).toBeVisible();
    expect(screen.getByTestId("run-health")).toHaveTextContent("healthy");
    expect(screen.getByTestId("workflow-node-plan")).toBeVisible();
    expect(screen.getByTestId("agent-tile-builder")).toBeVisible();
    expect(screen.getByTestId("terminal-builder")).toBeVisible();
    expect(screen.getByTestId("evidence-drawer")).toBeVisible();
  });

  it("manages projects and sends the active project defaults into the workflow launcher", async () => {
    const live = runtime({ mode: "live" });
    render(<ControlDeck runtime={live} />);

    fireEvent.click(screen.getByTestId("open-projects"));
    expect(screen.getByTestId("projects-panel")).toBeVisible();
    await waitFor(() => expect(screen.getByLabelText("Saved projects")).toBeVisible());
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    fireEvent.change(screen.getByLabelText("Project display name"), { target: { value: "Clipmaker" } });
    fireEvent.change(screen.getByLabelText("Project directory"), { target: { value: "/Users/prabhusriramulu/dev/active/clipmaker" } });
    fireEvent.change(screen.getByLabelText("Project workflow name"), { target: { value: "clipmaker-loop" } });
    fireEvent.change(screen.getByLabelText("Project notes"), { target: { value: "Video pipeline work" } });
    fireEvent.click(screen.getByRole("button", { name: "Use this project" }));

    await waitFor(() => expect(screen.getByText("Active project updated. Open Workflows when you are ready to launch its agents.")).toBeVisible());
    expect(window.localStorage.getItem("dot-agent-deck.desktop.projects.v1.fixture")).toContain("clipmaker-loop");
    fireEvent.click(screen.getByRole("button", { name: "Configure workflow" }));
    expect(screen.getByLabelText("Workflow name")).toHaveValue("clipmaker-loop");
    expect(screen.getByLabelText("Absolute project directory")).toHaveValue("/Users/prabhusriramulu/dev/active/clipmaker");
  });

  it("requires two clicks to remove only the local project entry", async () => {
    render(<ControlDeck runtime={runtime()} />);
    fireEvent.click(screen.getByTestId("open-projects"));
    await waitFor(() => expect(screen.getByRole("button", { name: "Remove entry" })).toBeVisible());
    fireEvent.click(screen.getByRole("button", { name: "Remove entry" }));
    expect(screen.getByRole("button", { name: "Confirm remove" })).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Confirm remove" }));
    expect(screen.getByText("Project entry removed. The repository folder was not changed.")).toBeVisible();
  });

  it("edits agent profiles locally and clearly marks the draft as not written to TOML", () => {
    render(<ControlDeck runtime={runtime()} />);
    fireEvent.click(screen.getByTestId("open-agent-profiles"));
    fireEvent.click(screen.getByRole("button", { name: /Coder/ }));
    fireEvent.change(screen.getByLabelText("Provider"), { target: { value: "Anthropic" } });
    expect(screen.getByLabelText("CLI")).toHaveValue("claude");
    expect((screen.getByLabelText("Generated launch command") as HTMLTextAreaElement).value).toContain("'claude' --model");
    fireEvent.change(screen.getByLabelText("Provider"), { target: { value: "OpenAI" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "gpt-5.6-sol-fast" } });
    expect((screen.getByLabelText("Generated launch command") as HTMLTextAreaElement).value).toContain("--model gpt-5.6-sol-fast");
    expect(screen.getByTestId("agent-profiles-panel")).toHaveTextContent("Local draft");
    expect(screen.getByRole("button", { name: /Coder/ })).toHaveTextContent("LOCAL");
    expect(window.localStorage.getItem("dot-agent-deck.desktop.agent-profiles.v1.fixture")).toContain("gpt-5.6-sol-fast");
  });

  it("launches the provider-derived command after CLI, model, effort, and permission edits", async () => {
    const live = runtime({ mode: "live" });
    render(<ControlDeck runtime={live} />);
    fireEvent.click(screen.getByTestId("open-agent-profiles"));
    fireEvent.click(screen.getByRole("button", { name: /Coder/ }));
    fireEvent.change(screen.getByLabelText("CLI"), { target: { value: "/Applications/Codex Nightly/bin/codex" } });
    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "gpt-5.6-sol-fast" } });
    fireEvent.change(screen.getByLabelText("Reasoning effort"), { target: { value: "high" } });
    fireEvent.change(screen.getByLabelText("Permission mode"), { target: { value: "full-access" } });
    fireEvent.click(screen.getByRole("button", { name: "Close agent profiles" }));

    fireEvent.click(screen.getByRole("button", { name: "Workflows" }));
    fireEvent.change(screen.getByLabelText("Task prompt"), { target: { value: "Build the project switcher polish." } });
    fireEvent.click(screen.getByTestId("launch-live-loop"));
    expect(screen.getByRole("alertdialog")).toHaveTextContent("All 6 commands are generated from the current provider, CLI, model, effort, and permission fields.");
    expect(screen.getByRole("alertdialog")).toHaveTextContent("sends your task prompt to the coordinator");
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Among generated commands, 1 role runs unrestricted");
    fireEvent.click(screen.getAllByRole("button", { name: "Launch live loop" }).at(-1)!);

    await waitFor(() => {
      const launch = vi.mocked(live.runAction).mock.calls[0]?.[0];
      expect(launch).toMatchObject({ type: "start_workflow" });
      if (launch?.type !== "start_workflow") throw new Error("expected workflow launch");
      expect(launch.taskPrompt).toBe("Build the project switcher polish.");
      expect(launch.roles.find((role) => role.role === "coder")?.command).toBe("'/Applications/Codex Nightly/bin/codex' --model gpt-5.6-sol-fast --sandbox danger-full-access --ask-for-approval on-request -c model_reasoning_effort=high");
    });
  });

  it("requires an explicit advanced toggle for a custom command and calls out the bypass at launch", async () => {
    const live = runtime({ mode: "live" });
    render(<ControlDeck runtime={live} />);
    fireEvent.click(screen.getByTestId("open-agent-profiles"));
    fireEvent.click(screen.getByRole("button", { name: /Coder/ }));
    fireEvent.change(screen.getByLabelText("Permission mode"), { target: { value: "full-access" } });
    fireEvent.click(screen.getByLabelText("Use advanced custom command override"));
    fireEvent.change(screen.getByLabelText("Custom launch command"), { target: { value: "devbox run agent-coder" } });
    expect(screen.getByTestId("agent-profiles-panel")).toHaveTextContent("Permissions are unmanaged here and must be encoded and reviewed in that command.");
    fireEvent.click(screen.getByRole("button", { name: "Close agent profiles" }));

    fireEvent.click(screen.getByRole("button", { name: "Workflows" }));
    fireEvent.change(screen.getByLabelText("Task prompt"), { target: { value: "Use the custom coder to fix failing tests." } });
    fireEvent.click(screen.getByTestId("launch-live-loop"));
    expect(screen.getByRole("alertdialog")).toHaveTextContent("1 explicit custom command override bypasses those fields");
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Custom commands may carry arbitrary permissions and are not covered by structured permission claims.");
    expect(screen.getByRole("alertdialog")).not.toHaveTextContent("role runs unrestricted");
    fireEvent.click(screen.getAllByRole("button", { name: "Launch live loop" }).at(-1)!);

    await waitFor(() => {
      const launch = vi.mocked(live.runAction).mock.calls[0]?.[0];
      if (launch?.type !== "start_workflow") throw new Error("expected workflow launch");
      expect(launch.roles.find((role) => role.role === "coder")?.command).toBe("devbox run agent-coder");
    });
  });

  it("confirmation-gates live workflow launch and keeps orchestrator as the start role", async () => {
    const live = runtime({ mode: "live" });
    render(<ControlDeck runtime={live} />);
    fireEvent.click(screen.getByRole("button", { name: "Workflows" }));
    expect(screen.getByTestId("launch-live-loop")).toBeDisabled();
    expect(screen.getByText("Add the task you want the coordinator to run.")).toBeVisible();
    fireEvent.change(screen.getByLabelText("Task prompt"), { target: { value: "Wire the launch prompt into the workflow." } });
    fireEvent.click(screen.getByTestId("launch-live-loop"));
    expect(live.runAction).not.toHaveBeenCalled();
    fireEvent.click(screen.getAllByRole("button", { name: "Launch live loop" }).at(-1)!);
    await waitFor(() => expect(live.runAction).toHaveBeenCalledWith(expect.objectContaining({
      type: "start_workflow",
      name: "dot-agent-deck",
      taskPrompt: "Wire the launch prompt into the workflow.",
      roles: expect.arrayContaining([expect.objectContaining({ role: "orchestrator", start: true }), expect.objectContaining({ role: "coder", start: false })]),
    })));
  });

  it("explains and disables live workflow launch on Windows before confirmation", () => {
    const live = runtime({ mode: "live" });
    render(<ControlDeck runtime={live} workflowPlatformIssue={WINDOWS_WORKFLOW_BLOCK_REASON} />);
    fireEvent.click(screen.getByRole("button", { name: "Workflows" }));
    fireEvent.change(screen.getByLabelText("Task prompt"), { target: { value: "Try to launch on Windows." } });

    expect(screen.getByTestId("workflow-platform-issue")).toHaveTextContent("unavailable in this Windows preview");
    expect(screen.getByTestId("launch-live-loop")).toBeDisabled();
    fireEvent.click(screen.getByTestId("launch-live-loop"));
    expect(live.runAction).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  });

  it("confirmation-gates an explicit daemon start from the disconnected state", async () => {
    const disconnected = createFixtureSnapshot("disconnected");
    disconnected.agents = [];
    let releaseStart!: () => void;
    const runAction = vi.fn(() => new Promise<import("./types").DeckActionResult>((resolve) => { releaseStart = () => resolve({ ok: true }); }));
    const live = runtime({ mode: "live", snapshot: disconnected, runAction });
    render(<ControlDeck runtime={live} />);
    fireEvent.click(screen.getByTestId("start-daemon"));
    expect(live.runAction).not.toHaveBeenCalled();
    fireEvent.click(screen.getAllByRole("button", { name: "Start daemon" }).at(-1)!);
    expect(screen.getByRole("button", { name: "Starting…" })).toBeDisabled();
    expect(screen.queryByText("Stopping…")).not.toBeInTheDocument();
    releaseStart();
    await waitFor(() => expect(live.runAction).toHaveBeenCalledWith({ type: "start_daemon" }));
    expect(live.reconnect).not.toHaveBeenCalled();
  });

  it("stops the daemon from the topbar when no agent is selected", async () => {
    const connectedNoAgents = createFixtureSnapshot("connected");
    connectedNoAgents.agents = [];
    connectedNoAgents.stages = [];
    connectedNoAgents.evidence = [];
    const runAction = vi.fn(async () => ({ ok: true }) as import("./types").DeckActionResult);
    const live = runtime({ mode: "live", snapshot: connectedNoAgents, runAction });
    render(<ControlDeck runtime={live} />);

    fireEvent.click(screen.getByRole("button", { name: "Stop daemon" }));
    expect(live.runAction).not.toHaveBeenCalled();
    fireEvent.click(screen.getAllByRole("button", { name: "Stop daemon" }).at(-1)!);

    await waitFor(() => expect(live.runAction).toHaveBeenCalledWith({ type: "stop_daemon" }));
    expect(screen.getByText("Local daemon stopped.")).toBeVisible();
  });

  it("replaces an incompatible zero-agent daemon through an explicit confirmation", async () => {
    const incompatible = createFixtureSnapshot("error");
    incompatible.agents = [];
    incompatible.stages = [];
    incompatible.evidence = [];
    incompatible.connection = {
      status: "error",
      socketPath: "/tmp/dot-agent-deck.sock",
      message: "build mismatch",
      daemonDetected: true,
      runningAgentCount: 0,
    };
    const runAction = vi.fn(async () => ({ ok: true }) as import("./types").DeckActionResult);
    const live = runtime({ mode: "live", snapshot: incompatible, runAction });
    render(<ControlDeck runtime={live} />);

    expect(screen.getByRole("button", { name: "Stop daemon" })).toBeEnabled();
    fireEvent.click(screen.getByTestId("replace-daemon"));
    expect(live.runAction).not.toHaveBeenCalled();
    expect(screen.getByRole("alertdialog")).toHaveTextContent("exact daemon build bundled with this desktop app");
    fireEvent.click(screen.getAllByRole("button", { name: "Replace daemon" }).at(-1)!);

    await waitFor(() => expect(live.runAction).toHaveBeenCalledWith({ type: "restart_daemon" }));
    expect(screen.getByText("Matching daemon started and reconnected.")).toBeVisible();
  });

  it("does not offer daemon replacement while an incompatible daemon reports live agents", () => {
    const incompatible = createFixtureSnapshot("error");
    incompatible.agents = [];
    incompatible.connection = {
      status: "error",
      message: "build mismatch",
      daemonDetected: true,
      runningAgentCount: 2,
    };
    render(<ControlDeck runtime={runtime({ mode: "live", snapshot: incompatible })} />);

    expect(screen.queryByTestId("replace-daemon")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stop daemon" })).toBeEnabled();
  });

  it("never reports daemon-start success when the start action fails", async () => {
    const disconnected = createFixtureSnapshot("disconnected");
    disconnected.agents = [];
    const live = runtime({
      mode: "live",
      snapshot: disconnected,
      runAction: vi.fn(async () => { throw new Error("daemon start timed out"); }),
    });
    render(<ControlDeck runtime={live} />);

    fireEvent.click(screen.getByTestId("start-daemon"));
    fireEvent.click(screen.getAllByRole("button", { name: "Start daemon" }).at(-1)!);

    await waitFor(() => expect(screen.getByText("daemon start timed out")).toBeVisible());
    expect(live.reconnect).not.toHaveBeenCalled();
    expect(screen.queryByText("Local daemon started and control channel reconnected.")).not.toBeInTheDocument();
  });

  it("keeps the mobile summary visible on first load and opens Evidence on demand", () => {
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

    render(<ControlDeck runtime={runtime()} />);
    expect(screen.queryByTestId("evidence-drawer")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stop Planner" })).toBeVisible();

    fireEvent.click(screen.getByRole("button", { name: "Evidence" }));
    expect(screen.getByTestId("evidence-drawer")).toBeVisible();
  });

  /**
   * Scenario: render the four-agent deck with every tile on its default
   * terminal tab and watch what the deck tells the bridge. It must declare all
   * four ids in ONE call, because `setShownTerminals` is declarative: four
   * single-id calls would leave three of the four in the bridge's warm set and
   * the bound would start evicting visible panes (PRD #745 M7).
   *
   * `TerminalViewport` is mocked in this file, so nothing here can be asserted
   * from mounted terminals — the bridge call IS the observable.
   */
  it("declares every terminal-tab tile to the bridge in a single call", () => {
    const setShownTerminals = vi.fn(async () => undefined);
    render(<ControlDeck runtime={runtime({ setShownTerminals })} />);

    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenCalledWith(["planner", "builder", "reviewer", "tester"]);
  });

  /**
   * Scenario: switch one tile off its terminal tab and leave the other three
   * alone. Exactly that agent leaves the shown set, and the surviving three are
   * re-declared together in one call — a tile showing a diff is not showing a
   * terminal, and that is the whole signal the bridge has for letting the PTY
   * go warm.
   */
  it("drops exactly the tile switched away from its terminal tab", () => {
    const setShownTerminals = vi.fn(async () => undefined);
    render(<ControlDeck runtime={runtime({ setShownTerminals })} />);

    const tile = screen.getByTestId("agent-tile-builder");
    fireEvent.click(within(tile).getByRole("tab", { name: "Diff" }));

    expect(setShownTerminals).toHaveBeenCalledTimes(2);
    expect(setShownTerminals).toHaveBeenLastCalledWith(["planner", "reviewer", "tester"]);
  });

  /**
   * Scenario: an empty deck. The deck still has to state that it shows no
   * terminal — omitting the call would leave whatever the previous screen
   * declared attached, which is exactly the leak M7 exists to close.
   */
  /**
   * Scenario: the daemon reports one agent whose id contains a newline and the
   * deck renders its tile on the default terminal tab. Agent ids are raw daemon
   * identities, so the id has to reach the bridge whole, as ONE shown terminal.
   * The effect keys on the ids joined by "\n"; reconstructing the array by
   * splitting that key back apart turns one tile into two attach paths, and
   * turns one identity `validate_agent_id` rejects for its control character
   * into two it accepts (PRD #745 M7).
   */
  it("declares an agent id containing a newline as one shown terminal", () => {
    const hostile = createFixtureSnapshot("connected");
    hostile.agents = [{ ...hostile.agents[0], id: "agent-a\nagent-b" }];
    const setShownTerminals = vi.fn(async () => undefined);
    render(<ControlDeck runtime={runtime({ snapshot: hostile, setShownTerminals })} />);

    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenCalledWith(["agent-a\nagent-b"]);
  });

  it("declares an empty shown set when the daemon owns no agents", () => {
    const empty = createFixtureSnapshot("empty");
    const setShownTerminals = vi.fn(async () => undefined);
    render(<ControlDeck runtime={runtime({ snapshot: empty, setShownTerminals })} />);

    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenCalledWith([]);
  });
});
