import { act, fireEvent, render, screen, within } from "@testing-library/react";
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

describe("stopping from the overview (PRD #1223 U4)", () => {
  /**
   * Scenario: on the fleet, press the stop control on the planner role's row —
   * an agent on the REMOTE deck. A confirmation names the agent and that daemon;
   * Cancel sends nothing. Pressed again and confirmed, it sends one
   * `stop_agent` naming the remote deck and the agent's own id.
   */
  it("confirms a stop and sends it to the daemon the agent is on", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    render(<AgentOverview runtime={runtime(runAction)} onNavigate={vi.fn()} />);

    const stop = within(row(FIXTURE_REMOTE_DAEMON_ID, "41")).getByTestId("overview-stop-agent");
    expect(stop).toHaveAccessibleName("Close planner agent");
    expect(stop).toHaveAttribute("title", "Close planner");
    fireEvent.click(stop);
    expect(confirmation()).toHaveTextContent("Close planner?");
    expect(confirmation()).toHaveTextContent("dev@build-box");
    fireEvent.click(within(confirmation()!).getByRole("button", { name: "Cancel" }));
    expect(confirmation()).toBeNull();
    expect(runAction).not.toHaveBeenCalled();

    fireEvent.click(stop);
    await act(async () => fireEvent.click(within(confirmation()!).getByRole("button", { name: "Close agent" })));

    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction).toHaveBeenCalledWith({ type: "stop_agent", deckId: FIXTURE_REMOTE_DAEMON_ID, agentId: "41" });
    expect(confirmation()).toBeNull();
  });

  /**
   * Scenario: press Close on the orchestration's card. The confirmation says
   * plainly that it stops EVERY role and names both; confirmed, one
   * `stop_orchestration` names the remote deck and both roles with their
   * names. Every orchestration card offers a Close; a plain agent's group
   * offers none.
   */
  it("confirms closing an orchestration, naming every role, and stops them all on its deck", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    render(<AgentOverview runtime={runtime(runAction)} onNavigate={vi.fn()} />);

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
    await act(async () => fireEvent.click(within(dialog).getByRole("button", { name: "Close all 2 roles" })));

    expect(runAction).toHaveBeenCalledWith({
      type: "stop_orchestration",
      deckId: FIXTURE_REMOTE_DAEMON_ID,
      roles: [{ agentId: "41", name: "planner" }, { agentId: "42", name: "builder" }],
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
