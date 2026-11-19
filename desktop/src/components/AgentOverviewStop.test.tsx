import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import type { AgentSession, DeckActionResult, DeckFleet, DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";

/**
 * PRD #1223 U4 — closing what the New agent flow creates, from the overview:
 * one agent from its row, and a whole orchestration from its card. Both ask for
 * confirmation first and name the daemon the agent is ON, never the selected one.
 */

/** A role of the `loop` orchestration on `deckId`. */
function role(deckId: string, id: string, roleName: string, roleIndex: number): AgentSession {
  return {
    ...createFixtureStartedAgent({ id, daemonId: deckId, displayName: roleName, command: "claude", cwd: "/home/build/demo-project" }),
    tab: { kind: "orchestration", orchestrationId: "orc-loop", name: "loop", displayTitle: "demo-project-orchestrator-1", roleName, roleIndex, isStartRole: roleIndex === 0, cwd: "/home/build/demo-project" },
    inOrchestration: true,
    incarnation: "listed-process-a",
    paneId: `pane-${id}`,
  };
}

/** The fleet preview, with the remote deck also running a two-role orchestration and a plain agent that shares an id with the local deck's. */
function fleetWithOrchestration(): DeckFleet {
  return createFixtureFleet("fleet").map((deck) => deck.connection.deckId === FIXTURE_REMOTE_DAEMON_ID
    ? { ...deck, agents: [...deck.agents, role(FIXTURE_REMOTE_DAEMON_ID, "41", "planner", 0), role(FIXTURE_REMOTE_DAEMON_ID, "42", "builder", 1)] }
    : deck);
}

function runtime(runAction: DeckRuntimeState["runAction"], fleet: DeckFleet = fleetWithOrchestration()): DeckRuntimeState {
  return {
    mode: "fixture",
    snapshot: fleet[0],
    fleet,
    terminalData: {},
    clearError: vi.fn(),
    runAction,
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(),
    saveSettings: vi.fn(),
    testEndpoint: vi.fn(),
    secretStatus: vi.fn(),
    storeSecret: vi.fn(),
    forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level),
  } as DeckRuntimeState;
}

const row = (deckId: string, agentId: string) => screen.getByTestId(`overview-agent-${agentKey({ daemonId: deckId, id: agentId })}`);
const confirmation = () => screen.queryByRole("alertdialog");

describe("desktop/config-gen/002: generating configuration from an agent", () => {
  type GenerationResult = { outcome: "sent"; message: string; messageId: string } | { outcome: "unsupported" | "failed"; message: string };
  const title = "Generate .dot-agent-deck.toml";
  const sent = "Config request sent to the agent's inbox.";
  const upgrade = "This daemon is too old to generate a config. Upgrade the daemon to use Generate config.";
  const failed = "Failed to send config prompt: daemon refused the target";
  const open = () => fireEvent.click(within(row(FIXTURE_REMOTE_DAEMON_ID, "41")).getByRole("button", { name: /Generate config/i }));
  const generation = (outcome: GenerationResult["outcome"] = "sent", message = sent) => {
    const generateConfig = vi.fn(async (): Promise<GenerationResult> => outcome === "sent" ? { outcome, message, messageId: "m-config" } : { outcome, message });
    return { generateConfig, rt: { ...runtime(vi.fn()), generateConfig } };
  };

  /// Scenario: Send a config request, dismiss its pending question with No, and send again for the same agent. The second request succeeds before the first fails, and its success notice must stay visible.
  it("keeps the latest overview result when an older request fails late", async () => {
    const pending: Array<(result: GenerationResult) => void> = [];
    const generateConfig = vi.fn(() => new Promise<GenerationResult>((resolve) => { pending.push(resolve); }));
    render(<AgentOverview runtime={{ ...runtime(vi.fn()), generateConfig }} onNavigate={vi.fn()} />);
    open();
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Yes" }));
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "No" }));
    open();
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Yes" }));
    expect(generateConfig).toHaveBeenCalledTimes(2);

    await act(async () => pending[1]({ outcome: "sent", message: sent, messageId: "m-new" }));
    expect(screen.getByText(sent)).toBeVisible();
    await act(async () => pending[0]({ outcome: "failed", message: failed }));

    expect(screen.queryByText(failed)).not.toBeInTheDocument();
    expect(screen.getByText(sent)).toBeVisible();
  });

  /** Scenario: Ask the remote planner to create or update a config, including an older daemon with unknown file state. Its row and neutral Yes/No confirmation say Update only for an existing file; No sends nothing. */
  it.each([undefined, false, true])("offers a neutral Yes/No confirmation and No sends nothing (existing config: %s)", (exists) => {
    const { rt, generateConfig } = generation();
    rt.fleet = rt.fleet.map((deck) => ({ ...deck, agents: deck.agents.map((agent) => ({ ...agent, projectConfigExists: exists })) }));
    const navigate = vi.fn();
    render(<AgentOverview runtime={rt} onNavigate={navigate} />);
    const verb = exists ? "Update" : "Generate";
    const button = within(row(FIXTURE_REMOTE_DAEMON_ID, "41")).getByRole("button", { name: `${verb} config with planner agent` });
    expect(button).toHaveAttribute("title", `${verb} config with planner`);
    fireEvent.click(button);
    const dialog = screen.getByRole("alertdialog", { name: `${verb} .dot-agent-deck.toml` });
    if (exists) {
      expect(dialog).toHaveTextContent(/instruct your agent to analyze the project and update the existing config/i);
      expect(dialog).not.toHaveTextContent("No orchestration config found");
    } else {
      expect(dialog).toHaveTextContent("No orchestration config found for this project. Want to instruct your agent to analyze the project and create one?");
    }
    expect(within(dialog).getAllByRole("button").map((button) => button.textContent).sort()).toEqual(["No", "Yes"]);
    expect(dialog.querySelector(".danger, .danger-icon")).toBeNull();
    fireEvent.click(within(dialog).getByRole("button", { name: "No" }));
    expect(confirmation()).toBeNull();
    expect(generateConfig).not.toHaveBeenCalled();
    expect(rt.runAction).not.toHaveBeenCalled();
    expect(navigate).not.toHaveBeenCalled();
  });

  /** Scenario: Confirm generation on a remote planner, then refresh its row from a successor daemon before pressing Yes. The command still names the captured daemon, pane, agent and incarnation, opens that agent's pane, and shows the daemon's success sentence. */
  it("sends the captured agent binding, focuses its pane, and shows success", async () => {
    const { rt, generateConfig } = generation();
    const navigate = vi.fn();
    const view = render(<AgentOverview runtime={rt} onNavigate={navigate} />);
    open();
    const successor = fleetWithOrchestration().map((deck) => ({ ...deck, agents: deck.agents.map((agent) => ({ ...agent, incarnation: "successor-process-b" })) }));
    view.rerender(<AgentOverview runtime={{ ...rt, ...runtime(rt.runAction, successor), generateConfig }} onNavigate={navigate} />);
    await act(async () => fireEvent.click(within(screen.getByRole("alertdialog", { name: title })).getByRole("button", { name: "Yes" })));
    expect(generateConfig).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, "41", "pane-41", "listed-process-a");
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", deckId: FIXTURE_REMOTE_DAEMON_ID, agentId: "41", incarnation: "listed-process-a" }));
    expect(screen.getByText(sent)).toBeVisible();
    expect(confirmation()).toBeNull();
  });

  /** Scenario: Confirm generation for planner, dismiss the busy question, and open builder while the request is still pending. A late successful send must leave builder open instead of taking the user back to planner. */
  it("keeps the user's navigation after dismissing a pending generation with No", async () => {
    let settle!: (result: GenerationResult) => void;
    const generateConfig = vi.fn(() => new Promise<GenerationResult>((resolve) => { settle = resolve; }));
    const navigate = vi.fn();
    render(<AgentOverview runtime={{ ...runtime(vi.fn()), generateConfig }} onNavigate={navigate} />);
    open();
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Yes" }));
    expect(generateConfig).toHaveBeenCalledTimes(1);
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "No" }));
    expect(confirmation()).toBeNull();
    fireEvent.click(within(row(FIXTURE_REMOTE_DAEMON_ID, "42")).getByRole("button", { name: "Open builder agent" }));
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "agent", agentId: "42" }));
    navigate.mockClear();

    await act(async () => settle({ outcome: "sent", message: sent, messageId: "m-late" }));

    expect(navigate).not.toHaveBeenCalled();
    expect(confirmation()).toBeNull();
  });

  /** Scenario: The daemon answers unsupported or refuses the captured target after Yes. The shared upgrade/failure sentence is visible and the app does not present a successful send or enter the agent pane. */
  it.each([["unsupported", upgrade], ["failed", failed]] as const)("shows the shared %s outcome", async (outcome, message) => {
    const { rt, generateConfig } = generation(outcome, message);
    const navigate = vi.fn();
    render(<AgentOverview runtime={rt} onNavigate={navigate} />);
    open();
    await act(async () => fireEvent.click(within(screen.getByRole("alertdialog", { name: title })).getByRole("button", { name: "Yes" })));
    expect(generateConfig).toHaveBeenCalledTimes(1);
    expect(screen.getByText(message)).toBeVisible();
    expect(screen.queryByText(sent)).toBeNull();
    expect(navigate).not.toHaveBeenCalled();
  });
});

describe("stopping from the overview (PRD #1223 U4)", () => {
  /** Scenario: close a remote dispatched agent or its whole team. The confirmation names the worktree the daemon predicts will be kept; an ordinary close has no warning, and an inconclusive probe does not claim confirmed uncommitted changes. */
  it.each(["agent", "orchestration"] as const)("previews the kept worktree before closing a remote %s", async (kind) => {
    const path = "/home/build/project-dispatch-1674";
    const preview = vi.fn<NonNullable<DeckRuntimeState["previewWorktreeClose"]>>(async () => ({ kept: { path, confirmed_dirty: true }, keepWorktree: null }));
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    const rt = { ...runtime(runAction), previewWorktreeClose: preview };
    render(<AgentOverview runtime={rt} onNavigate={vi.fn()} />);
    const open = () => fireEvent.click(kind === "agent"
      ? within(row(FIXTURE_REMOTE_DAEMON_ID, "41")).getByTestId("overview-stop-agent")
      : screen.getByRole("button", { name: "Close demo-project-orchestrator-1 orchestration" }));
    open();
    await waitFor(() => expect(confirmation()).toHaveTextContent(path));
    expect(confirmation()).toHaveTextContent("Uncommitted work here is KEPT, not deleted:");
    expect(preview).toHaveBeenCalledWith(FIXTURE_REMOTE_DAEMON_ID, kind === "agent" ? ["pane-41"] : ["pane-41", "pane-42"], "listed-process-a");
    expect(runAction).not.toHaveBeenCalled();
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Cancel" }));

    preview.mockResolvedValueOnce({ kept: { path, confirmed_dirty: false }, keepWorktree: null });
    open();
    await waitFor(() => expect(confirmation()).toHaveTextContent("Uncommitted work here, if any, is KEPT:"));
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Cancel" }));

    preview.mockResolvedValueOnce({ kept: null, keepWorktree: null });
    open();
    await waitFor(() => expect(within(confirmation()!).getByRole("button", { name: kind === "agent" ? "Close agent" : "Close all 2 roles" })).toBeEnabled());
    expect(confirmation()).not.toHaveTextContent("KEPT");
    expect(within(confirmation()!).queryByRole("button", { name: /keep worktree/ })).toBeNull();
  });

  /**
   * Scenario (issue #1958): close a remote dispatched agent, or its whole team,
   * whose deck can keep the worktree. The confirmation offers Cancel, the close,
   * and "…, keep worktree", in that order; pressing the keep button sends the
   * stop with `keepWorktree: true` — for an orchestration, one
   * `stop_orchestration` the crate fans out to every role with the keep.
   */
  it.each(["agent", "orchestration"] as const)("offers a close that keeps the worktree of a remote %s and sends the keep", async (kind) => {
    const path = "/home/build/project-dispatch-1958";
    const preview = vi.fn<NonNullable<DeckRuntimeState["previewWorktreeClose"]>>(async () => ({ kept: null, keepWorktree: path }));
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    render(<AgentOverview runtime={{ ...runtime(runAction), previewWorktreeClose: preview }} onNavigate={vi.fn()} />);
    fireEvent.click(kind === "agent"
      ? within(row(FIXTURE_REMOTE_DAEMON_ID, "41")).getByTestId("overview-stop-agent")
      : screen.getByRole("button", { name: "Close demo-project-orchestrator-1 orchestration" }));
    const close = kind === "agent" ? "Close agent" : "Close all 2 roles";
    const keep = await within(confirmation()!).findByRole("button", { name: `${close}, keep worktree` });
    expect(within(confirmation()!).getAllByRole("button").map((button) => button.textContent)).toEqual(["Cancel", close, `${close}, keep worktree`]);
    expect(runAction).not.toHaveBeenCalled();
    await act(async () => fireEvent.click(keep));
    expect(runAction).toHaveBeenCalledExactlyOnceWith(kind === "agent"
      ? { type: "stop_agent", deckId: FIXTURE_REMOTE_DAEMON_ID, agentId: "41", incarnation: "listed-process-a", keepWorktree: true }
      : { type: "stop_orchestration", deckId: FIXTURE_REMOTE_DAEMON_ID, roles: [{ agentId: "41", name: "planner", incarnation: "listed-process-a" }, { agentId: "42", name: "builder", incarnation: "listed-process-a" }], keepWorktree: true });
    expect(confirmation()).toBeNull();
  });

  /**
   * Scenario: on the fleet, press the stop control on the planner role's row —
   * an agent on the REMOTE deck. A confirmation names the agent and that daemon;
   * Cancel sends nothing. Pressed again and confirmed, it sends one
   * `stop_agent` naming the remote deck, agent id and the process captured when the confirmation opened, even if a refreshed row belongs to a successor.
   */
  it("confirms a stop and sends it to the daemon the agent is on", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    const { rerender } = render(<AgentOverview runtime={runtime(runAction)} onNavigate={vi.fn()} />);

    const stop = within(row(FIXTURE_REMOTE_DAEMON_ID, "41")).getByTestId("overview-stop-agent");
    expect(stop).toHaveAccessibleName("Close planner agent");
    expect(stop).toHaveAttribute("title", "Close planner");
    fireEvent.click(stop);
    expect(confirmation()).toHaveTextContent("Close planner?");
    expect(confirmation()).toHaveTextContent("dev@build-box");
    expect(confirmation()).toHaveTextContent("This closes only this role, leaving the orchestration one role short. Other roles keep running.");
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Cancel" }));
    expect(confirmation()).toBeNull();
    expect(runAction).not.toHaveBeenCalled();

    fireEvent.click(stop);
    const successor = fleetWithOrchestration().map((deck) => ({ ...deck, agents: deck.agents.map((agent) => ({ ...agent, incarnation: "successor-process-b" })) }));
    rerender(<AgentOverview runtime={runtime(runAction, successor)} onNavigate={vi.fn()} />);
    await act(async () => fireEvent.click(within(confirmation()!).getByRole("button", { name: "Close agent" })));

    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction).toHaveBeenCalledWith({ type: "stop_agent", deckId: FIXTURE_REMOTE_DAEMON_ID, agentId: "41", incarnation: "listed-process-a" });
    expect(confirmation()).toBeNull();
  });

  /**
   * Scenario: press Close on the orchestration's card. The confirmation says
   * plainly that it stops EVERY role and names both; confirmed, one
   * `stop_orchestration` names the remote deck and both roles with their
   * names. Every orchestration card offers a Close; a plain agent's group
   * offers none. A refresh after opening the confirmation must preserve each captured role's incarnation.
   */
  it("confirms closing an orchestration, naming every role, and stops them all on its deck", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    const { rerender } = render(<AgentOverview runtime={runtime(runAction)} onNavigate={vi.fn()} />);

    const remoteGroup = screen.getAllByTestId("daemon-group").find((group) => group.getAttribute("data-daemon-id") === FIXTURE_REMOTE_DAEMON_ID)!;
    const orchestrationCards = [...remoteGroup.querySelectorAll("[data-group-kind='orchestration']")];
    expect(within(remoteGroup).getAllByTestId("overview-close-orchestration")).toHaveLength(orchestrationCards.length);
    const closes = [within(remoteGroup).getByRole("button", { name: "Close demo-project-orchestrator-1 orchestration" })];
    const plainCards = [...remoteGroup.querySelectorAll("[data-group-kind]")].filter((card) => card.getAttribute("data-group-kind") !== "orchestration");
    expect(plainCards.length).toBeGreaterThan(0);
    for (const card of plainCards) expect(within(card as HTMLElement).queryByTestId("overview-close-orchestration")).toBeNull();
    expect(closes[0]).toHaveAccessibleName("Close demo-project-orchestrator-1 orchestration");
    fireEvent.click(closes[0]);
    const dialog = confirmation()!;
    expect(dialog).toHaveTextContent("Close demo-project-orchestrator-1?");
    expect(dialog).toHaveTextContent("This stops every role of this orchestration on dev@build-box — all 2 of its roles: planner, builder.");
    const successor = fleetWithOrchestration().map((deck) => ({ ...deck, agents: deck.agents.map((agent) => ({ ...agent, incarnation: "successor-process-b" })) }));
    rerender(<AgentOverview runtime={runtime(runAction, successor)} onNavigate={vi.fn()} />);
    await act(async () => fireEvent.click(within(dialog).getByRole("button", { name: "Close all 2 roles" })));

    expect(runAction).toHaveBeenCalledWith({
      type: "stop_orchestration",
      deckId: FIXTURE_REMOTE_DAEMON_ID,
      roles: [{ agentId: "41", name: "planner", incarnation: "listed-process-a" }, { agentId: "42", name: "builder", incarnation: "listed-process-a" }],
    });
  });

  /**
   * Scenario: confirm a stop the daemon takes its time over. While it is in
   * flight the confirmation's button is disabled and reads Stopping…, so a
   * second press sends nothing; once the daemon answers the dialog closes. A
   * refusal closes it too — the runtime files it under its global error — and
   * nothing is thrown out of the screen.
   */
  it("disables the confirmation while the stop is in flight, and survives a refusal", async () => {
    let settle!: (outcome: "ok" | "refused") => void;
    const runAction = vi.fn(() => new Promise<DeckActionResult>((resolve, reject) => {
      settle = (outcome) => (outcome === "ok" ? resolve({ ok: true }) : reject(new Error("daemon returned error: no such agent")));
    }));
    render(<AgentOverview runtime={runtime(runAction)} onNavigate={vi.fn()} />);

    fireEvent.click(within(row(FIXTURE_DAEMON_ID, "planner")).getByTestId("overview-stop-agent"));
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Close agent" }));
    const busy = within(confirmation()!).getByRole("button", { name: "Stopping…" });
    expect(busy).toBeDisabled();
    fireEvent.click(busy);
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction).toHaveBeenCalledWith({ type: "stop_agent", deckId: FIXTURE_DAEMON_ID, agentId: "planner" });

    await act(async () => settle("refused"));
    expect(confirmation()).toBeNull();
  });
});

describe("a spoken stop holds to the daemon process it named (PRD #1666 part 2, G-fix7)", () => {
  /**
   * Scenario: voice hands the overview a stop (and a close) for the planner
   * role as daemon process A listed it, but the deck now lists an unrelated
   * agent under that id from process B. No confirmation opens and the answer
   * says the agent is gone with its deck's restart. The same targets naming
   * the process that lists them now (control) open their confirmations.
   */
  it.each([
    ["successor-process-b", false],
    ["listed-process-a", true],
  ] as const)("opens a spoken stop only for the process it named (deck lists %s)", async (listedBy, opens) => {
    const { STOP_TARGET_RESTARTED } = await import("./AgentOverview");
    const channel: { current: Record<string, unknown> | undefined } = { current: undefined };
    const fleet = fleetWithOrchestration().map((deck) => ({ ...deck, agents: deck.agents.map((agent) => ({ ...agent, incarnation: listedBy })) }));
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    render(<AgentOverview runtime={runtime(runAction, fleet)} onNavigate={vi.fn()} voiceChannel={channel as never} />);
    const voice = channel.current as { confirmStopAgent: (target: object) => string | undefined; confirmCloseOrchestration: (target: object) => string | undefined };
    const target = { deckId: FIXTURE_REMOTE_DAEMON_ID, agentId: "41", from: "overview" };

    let answer: string | undefined;
    act(() => { answer = voice.confirmStopAgent({ ...target, incarnation: "listed-process-a" }); });
    expect(answer).toBe(opens ? undefined : STOP_TARGET_RESTARTED);
    expect(confirmation() !== null).toBe(opens);
    if (opens) fireEvent.click(within(confirmation()!).getByRole("button", { name: "Cancel" }));

    act(() => { answer = voice.confirmCloseOrchestration({ ...target, orchestrationAgentId: "41", orchestrationIncarnation: "listed-process-a" }); });
    expect(answer).toBe(opens ? undefined : STOP_TARGET_RESTARTED);
    expect(confirmation() !== null).toBe(opens);
    expect(runAction).not.toHaveBeenCalled();
  });
});

describe("stopping a whole daemon from its card (issue #1073)", () => {
  /** A deck's card, by the name its header shows: "Local daemon" for this machine's, the address for a remote one. */
  const card = (deckId: string) => {
    const name = deckId === FIXTURE_DAEMON_ID ? "Local daemon" : deckId;
    const found = screen.getAllByTestId("daemon-group").find((section) => within(section).getByTestId("daemon-identity").textContent === name);
    if (!found) throw new Error(`no card for ${name}`);
    return found;
  };
  const live = (runAction: DeckRuntimeState["runAction"], fleet: DeckFleet = createFixtureFleet("fleet")) => ({ ...runtime(runAction, fleet), mode: "live" as const });

  /** Scenario: With the local daemon selected, Stop daemon on a remote daemon's card asks first, naming that daemon, and then stops THAT daemon rather than the selected one. */
  it("stops the daemon whose card it was pressed on, not the selected one", async () => {
    const runAction = vi.fn(async () => ({ ok: true, message: "Daemon stopped." }) as DeckActionResult);
    const deck = live(runAction);
    expect(deck.snapshot.connection.deckId).toBe(FIXTURE_DAEMON_ID);
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} />);

    fireEvent.click(within(card(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-stop"));
    expect(confirmation()).toHaveTextContent(`Stop the daemon on ${FIXTURE_REMOTE_DAEMON_ID}?`);
    expect(runAction).not.toHaveBeenCalled();
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Stop daemon" }));

    await waitFor(() => expect(runAction).toHaveBeenCalledWith({ type: "stop_daemon", deckId: FIXTURE_REMOTE_DAEMON_ID }));
    expect(runAction).toHaveBeenCalledTimes(1);
  });

  /** Scenario: A remote daemon accepts the stop but has not gone yet when the app stops waiting; the Dashboard shows the daemon's own "still shutting down" sentence, naming the daemon, until it is dismissed. */
  it("says what the stop did, naming the daemon", async () => {
    const still = "The daemon accepted the stop and is still shutting down; its agents are being closed.";
    const runAction = vi.fn(async () => ({ ok: true, message: still }) as DeckActionResult);
    render(<AgentOverview runtime={live(runAction)} onNavigate={vi.fn()} />);

    fireEvent.click(within(card(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-stop"));
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Stop daemon" }));

    const notice = await screen.findByTestId("overview-daemon-notice");
    expect(notice).toHaveTextContent(`${FIXTURE_REMOTE_DAEMON_ID}: ${still}`);
    fireEvent.click(within(notice).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByTestId("overview-daemon-notice")).not.toBeInTheDocument();
  });

  /** Scenario: Two stops are confirmed one after the other and the FIRST one settles last; the notice keeps the result of the stop pressed most recently. */
  it("keeps the latest stop's result when an older stop settles later", async () => {
    let settleFirst: ((result: DeckActionResult) => void) | undefined;
    const runAction = vi.fn(async (action: { type: string; deckId?: string }) => action.deckId === FIXTURE_REMOTE_DAEMON_ID
      ? new Promise<DeckActionResult>((resolve) => { settleFirst = resolve; })
      : ({ ok: true, message: "Daemon stopped gracefully." }) as DeckActionResult);
    render(<AgentOverview runtime={live(runAction as DeckRuntimeState["runAction"])} onNavigate={vi.fn()} />);

    fireEvent.click(within(card(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-stop"));
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Stop daemon" }));
    await waitFor(() => expect(settleFirst).toBeDefined());
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Cancel" }));
    fireEvent.click(within(card(FIXTURE_DAEMON_ID)).getByTestId("daemon-stop"));
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Stop daemon" }));
    expect(await screen.findByTestId("overview-daemon-notice")).toHaveTextContent("Local daemon: Daemon stopped gracefully.");

    await act(async () => { settleFirst?.({ ok: true, message: "The daemon accepted the stop and is still shutting down." }); });
    expect(screen.getByTestId("overview-daemon-notice")).toHaveTextContent("Local daemon: Daemon stopped gracefully.");
  });

  /** Scenario: The local daemon's card offers Stop daemon too, and it stops the local daemon by its own id. */
  it("stops the local daemon from its own card", async () => {
    const runAction = vi.fn(async () => ({ ok: true }) as DeckActionResult);
    render(<AgentOverview runtime={live(runAction)} onNavigate={vi.fn()} />);

    fireEvent.click(within(card(FIXTURE_DAEMON_ID)).getByTestId("daemon-stop"));
    expect(confirmation()).toHaveTextContent("Stop the daemon on this machine?");
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Stop daemon" }));
    await waitFor(() => expect(runAction).toHaveBeenCalledWith({ type: "stop_daemon", deckId: FIXTURE_DAEMON_ID }));
  });

  /** Scenario: A remote daemon too old to be stopped from the app shows Stop daemon disabled with the reason; daemons that are not answering show no Stop at all, and neither does the fixture preview. */
  it("offers Stop only where it can act", () => {
    const reason = "This daemon is too old to be stopped from this app.";
    const fleet = createFixtureFleet("fleet").map((deck) => deck.connection.deckId === FIXTURE_REMOTE_DAEMON_ID ? { ...deck, connection: { ...deck.connection, stopDaemonReason: reason } } : deck);
    const view = render(<AgentOverview runtime={live(vi.fn(), fleet)} onNavigate={vi.fn()} />);

    const old = within(card(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-stop");
    expect(old).toBeDisabled();
    expect(old).toHaveAttribute("title", reason);
    for (const section of screen.getAllByTestId("daemon-group").filter((section) => section.getAttribute("data-deck-connected") === "no")) {
      expect(within(section).queryByTestId("daemon-stop")).not.toBeInTheDocument();
    }

    view.unmount();
    render(<AgentOverview runtime={runtime(vi.fn())} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("daemon-stop")).not.toBeInTheDocument();
  });
});
