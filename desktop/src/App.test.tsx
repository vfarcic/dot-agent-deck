import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { WINDOWS_WORKFLOW_BLOCK_REASON } from "./lib/platform";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckRuntimeState } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { ControlDeck } from "./App";
import { SETTINGS_SECTIONS } from "./lib/settingsRegistry";

/**
 * A settings store with the shape the real bridges have: `getSettings` answers
 * with the document AND where it lives, `saveSettings` echoes what it wrote.
 * Backed by a plain object so a remount reads back what an earlier render
 * saved, which is what makes the persistence assertions mean anything.
 */
function settingsStore(initial?: Partial<DesktopSettingsDto>, path?: string) {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS, ...initial };
  return {
    get current() { return document; },
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      document = structuredClone(next);
      return structuredClone(document);
    }),
  };
}

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const settings = settingsStore();
  return {
    mode: "fixture",
    snapshot: createFixtureSnapshot("connected"),
    terminalData: {},
    runAction: vi.fn(async () => ({ ok: true }) as import("./types").DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
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

  afterEach(() => {
    vi.unstubAllGlobals();
    // The appearance override lives on the document root, which persists
    // across renders in one jsdom, so a test that sets it would otherwise
    // theme every test after it.
    document.documentElement.removeAttribute("data-theme");
  });

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

  // ── The settings surface (PRD #803 M3) and the appearance override (#743 M4)

  it("opens settings from the rail button and from the command palette, and closes three ways", async () => {
    const { unmount } = render(<ControlDeck runtime={runtime()} />);

    // The rail button was a toast stub with no `active` and no test seam; it is
    // a real overlay now, and the sixth one.
    expect(screen.queryByTestId("settings-panel")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("open-settings"));
    expect(screen.getByTestId("settings-panel")).toBeVisible();
    expect(screen.getByTestId("open-settings")).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("dialog", { name: "Settings" })).toHaveAttribute("aria-modal", "true");

    // Escape, through the single handler that closes every overlay.
    fireEvent.keyDown(document.body, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("settings-panel")).not.toBeInTheDocument());

    // The backdrop, but not the sheet itself — a mousedown inside must not close.
    fireEvent.click(screen.getByTestId("open-settings"));
    fireEvent.mouseDown(screen.getByTestId("settings-panel"));
    expect(screen.getByTestId("settings-panel")).toBeVisible();
    fireEvent.mouseDown(screen.getByTestId("settings-panel").parentElement as HTMLElement);
    await waitFor(() => expect(screen.queryByTestId("settings-panel")).not.toBeInTheDocument());

    // And the command palette.
    fireEvent.keyDown(document.body, { key: "k", metaKey: true });
    fireEvent.click(screen.getByRole("button", { name: /Open settings/ }));
    expect(screen.getByTestId("settings-panel")).toBeVisible();

    // The close button.
    fireEvent.click(screen.getByRole("button", { name: "Close settings" }));
    await waitFor(() => expect(screen.queryByTestId("settings-panel")).not.toBeInTheDocument());
    unmount();
  });

  it("renders the registry's active section, with no column while there is only one", () => {
    render(<ControlDeck runtime={runtime()} />);
    fireEvent.click(screen.getByTestId("open-settings"));

    // The registry drives what is rendered, and today it holds one row, so the
    // section column is dropped and the panel takes the whole sheet. The
    // column's own two states are pinned with stub sections in
    // `components/SettingsSheet.test.tsx`; what matters here is that the real
    // registry reaches the real sheet.
    expect(SETTINGS_SECTIONS).toHaveLength(1);
    expect(screen.getByTestId("settings-layout")).toHaveClass("is-single");
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).not.toBeInTheDocument();

    expect(screen.getByTestId(`settings-panel-${SETTINGS_SECTIONS[0].id}`)).toBeVisible();
    expect(screen.getByRole("group", { name: "Appearance" })).toBeVisible();
  });

  it("applies each appearance choice to the document root, and System CLEARS the attribute", async () => {
    const store = settingsStore();
    render(<ControlDeck runtime={runtime({ getSettings: store.getSettings, saveSettings: store.saveSettings })} />);
    fireEvent.click(screen.getByTestId("open-settings"));

    // System is the default, and it must leave no attribute at all — there is
    // no [data-theme="system"] block, so writing one would match neither dark
    // rule and pin the app to light.
    await waitFor(() => expect(screen.getByRole("radio", { name: /System/ })).toBeChecked());
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);

    fireEvent.click(screen.getByRole("radio", { name: /Dark/ }));
    await waitFor(() => expect(document.documentElement.getAttribute("data-theme")).toBe("dark"));

    fireEvent.click(screen.getByRole("radio", { name: /Light/ }));
    await waitFor(() => expect(document.documentElement.getAttribute("data-theme")).toBe("light"));

    // Back to System: the attribute is REMOVED, not set to "system".
    fireEvent.click(screen.getByRole("radio", { name: /System/ }));
    await waitFor(() => expect(document.documentElement.hasAttribute("data-theme")).toBe(false));
    expect(store.current.appearance.mode).toBe("system");
  });

  it("round-trips the appearance choice through the bridge and restores it on a remount", async () => {
    const store = settingsStore();
    const first = render(<ControlDeck runtime={runtime({ getSettings: store.getSettings, saveSettings: store.saveSettings })} />);

    fireEvent.click(screen.getByTestId("open-settings"));
    fireEvent.click(screen.getByRole("radio", { name: /Dark/ }));
    await waitFor(() => expect(store.saveSettings).toHaveBeenCalled());

    // The whole document crosses the bridge, so a save can never drop a section
    // this build's UI has not loaded.
    expect(store.saveSettings).toHaveBeenCalledWith({ version: 1, appearance: { mode: "dark" } });
    expect(store.current.appearance.mode).toBe("dark");

    first.unmount();
    document.documentElement.removeAttribute("data-theme");

    // A fresh mount reads the stored choice and applies it without the sheet
    // ever being opened — the "applied on load, not only on change" property.
    render(<ControlDeck runtime={runtime({ getSettings: store.getSettings, saveSettings: store.saveSettings })} />);
    await waitFor(() => expect(document.documentElement.getAttribute("data-theme")).toBe("dark"));
    fireEvent.click(screen.getByTestId("open-settings"));
    await waitFor(() => expect(screen.getByRole("radio", { name: /Dark/ })).toBeChecked());
  });

  it("names the settings file in live mode and admits there is none in the fixture preview", async () => {
    const live = settingsStore(undefined, "/home/dev/.config/dot-agent-deck/desktop.toml");
    const { unmount } = render(<ControlDeck runtime={runtime({ mode: "live", getSettings: live.getSettings, saveSettings: live.saveSettings })} />);
    fireEvent.click(screen.getByTestId("open-settings"));
    await waitFor(() => expect(screen.getByTestId("settings-location"))
      .toHaveTextContent("/home/dev/.config/dot-agent-deck/desktop.toml"));
    unmount();

    // The browser preview has no filesystem at all, so it must say so rather
    // than print a plausible-looking path for a file that does not exist.
    const preview = settingsStore();
    render(<ControlDeck runtime={runtime({ mode: "fixture", getSettings: preview.getSettings, saveSettings: preview.saveSettings })} />);
    fireEvent.click(screen.getByTestId("open-settings"));
    const location = await screen.findByTestId("settings-location");
    expect(location).toHaveTextContent(/local storage/i);
    expect(location.textContent).not.toMatch(/desktop\.toml/);
  });

  it("keeps a failed save visible and applied rather than silently reverting it", async () => {
    const store = settingsStore();
    store.saveSettings.mockRejectedValueOnce(new Error("Permission denied"));
    render(<ControlDeck runtime={runtime({ getSettings: store.getSettings, saveSettings: store.saveSettings })} />);

    fireEvent.click(screen.getByTestId("open-settings"));
    fireEvent.click(screen.getByRole("radio", { name: /Dark/ }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Permission denied");
    expect(alert).toHaveTextContent(/will not survive a restart/);
    // Applied anyway: the user asked for it and can see it.
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(screen.getByRole("radio", { name: /Dark/ })).toBeChecked();
  });
});
