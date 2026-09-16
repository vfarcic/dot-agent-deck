import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { ALL_ENDPOINT_SELECTION, DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckRuntimeState, DeckSnapshot } from "./types";

/**
 * The mock records every mount, because the claim under test is that a pane
 * with no attach mounts **no viewport at all** — and a viewport that mounted
 * and immediately unmounted, or one rendered empty, is indistinguishable from
 * none by any DOM query. It records `deckId` for the same reason
 * `AgentPaneDeckIdentity.test.tsx` does: an agent id alone names an agent on
 * every deck.
 */
const viewportProps: { agentId: string; deckId?: string }[] = [];
vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, deckId, label }: { agentId: string; deckId?: string; label: string }) => {
    viewportProps.push({ agentId, deckId });
    return <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>terminal</pre>;
  },
}));

import { DeckShell } from "./App";

/**
 * The second deck, described exactly as the crate describes a remote one —
 * `Endpoint::describe()` renders `user@host[:port]` and never a socket path, so
 * `deckName` prints this string verbatim and the pane's sentence has to contain
 * it.
 */
const REMOTE_DECK_ID = "deck-00000000000000b2";
const REMOTE_DECK_LABEL = "dev@build-box";
/** The deck's own account of why it is not answering, which the pane repeats. */
const REMOTE_DECK_FAILURE = "No deck is listening on the configured socket.";

/**
 * Two decks running the SAME agent ids, which is the ordinary case rather than
 * a contrived one: agent ids are per-daemon monotonic integers and are unique
 * only within a daemon. The names differ so a test can say which deck's agent
 * it is looking at; the ids deliberately do not.
 *
 * `remoteStatus` is the whole variable this file turns: the remote deck is
 * either answering, in which case its agents attach exactly like the selected
 * deck's, or it is not, in which case they cannot.
 */
function harness(remoteStatus: "connected" | "disconnected" = "disconnected") {
  const local = createFixtureSnapshot("connected");
  const remote: DeckSnapshot = {
    ...local,
    connection: {
      ...local.connection,
      deckId: REMOTE_DECK_ID,
      socketPath: REMOTE_DECK_LABEL,
      deckKind: "remote",
      status: remoteStatus,
      message: remoteStatus === "connected" ? "Deck responding" : REMOTE_DECK_FAILURE,
    },
    agents: local.agents.map((agent) => ({ ...agent, daemonId: REMOTE_DECK_ID, role: `${agent.role} on build-box`, displayName: `${agent.displayName} on build-box` })),
  };
  const setShownTerminals = vi.fn(async () => undefined);
  /*
    A document that lists the remote deck and selects ALL, which is the state a
    fleet is observed under. Cloned on the way out of `getSettings` so the hook
    cannot mutate the fixture.
  */
  const stored: DesktopSettingsDto = {
    ...structuredClone(DEFAULT_DESKTOP_SETTINGS),
    endpoints: { remote: [{ id: "row-build-box", host: "build-box", user: "dev", port: 22, socket: "/run/deck.sock" }], selection: ALL_ENDPOINT_SELECTION },
  };
  const saveSettings = vi.fn(async (next: DesktopSettingsDto) => structuredClone(next));
  const base = {
    mode: "live",
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals,
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: vi.fn(async () => ({ settings: structuredClone(stored), path: "/tmp/desktop.toml" })),
    saveSettings,
  };
  /** The runtime as it looks while `selected` is the deck in force. */
  const runtime = (selected: "local" | "remote"): DeckRuntimeState => ({
    ...base,
    snapshot: selected === "local" ? local : remote,
    fleet: selected === "local" ? [local, remote] : [remote, local],
  } as unknown as DeckRuntimeState);
  return { local, remote, runtime, setShownTerminals, saveSettings };
}

/** The overview's open control for one agent, named as the row renders it. */
const openControl = (name: string) => screen.getByRole("button", { name: `Open ${name} agent` });

/**
 * PRD #1105 — a pane can be open for an agent this app has attached no terminal
 * to, and what it renders there is a STATE rather than an empty box.
 *
 * # The trigger was NARROWED, and the narrowing is the point of this file
 *
 * This state used to fire for every agent on a deck that was not the SELECTED
 * one, because `terminal::attach` resolved its daemon through the
 * process-global `trusted_daemon()` — declaring an attach for an agent on
 * another deck would have streamed whatever agent of the same per-daemon
 * monotonic id that deck happened to be running. The product owner reversed
 * that: the desktop app's reason to exist over the TUI is being a control plane
 * for every deck at once, and an agent it lists but cannot open as a working
 * pane is the feature failing on its own terms. The attach now names its deck.
 *
 * So what is left here is the case the old trigger was also covering and which
 * no amount of plumbing removes: **a deck with no live link**. A disconnected
 * deck, one still waiting to report, one configured with no address, one this
 * app is not observing. There is nothing to attach over, and a
 * `TerminalViewport` mounted there sits black for as long as the pane is open —
 * a black rectangle that reads *"this agent is producing no output"* about an
 * agent that may be working normally on a machine this app has merely lost
 * contact with.
 *
 * # Why not a mounted terminal
 *
 * Because that lie is the whole defect. Hence no viewport at all, and the
 * mount-recording mock above rather than a DOM query.
 *
 * # The two halves are one condition
 *
 * `DeckShell`'s `paneDeckAttachable` decides both whether an attach is declared
 * and whether the pane renders a terminal, so the two cannot disagree — a pane
 * showing a terminal nothing attached to, and a pane explaining a state it is
 * not in, are the two ways this drifts.
 */
describe("a pane with no terminal", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /**
   * Scenario: build-box's Planner is open in a pane — a working one, with a
   * live terminal — and build-box then stops answering. The pane does not
   * close and does not go blank: the terminal is replaced by an explicit
   * no-terminal state naming build-box and repeating that deck's own account of
   * why, and nothing is declared shown any more.
   *
   * Entered through `initialView` rather than the overview's Open control, and
   * for a reason worth stating: the overview LISTS no agents for a deck that
   * stopped answering, because such a deck cannot vouch for what it was
   * running. So a disconnected deck's pane is reached by having been opened
   * while the deck was healthy — which is exactly the case this state exists
   * for, and the only one a user meets.
   */
  it("replaces an unreachable deck's terminal with an explicit state naming that deck", async () => {
    const paneView = { kind: "agent" as const, deckId: REMOTE_DECK_ID, agentId: "planner", from: "overview" as const };
    const answering = harness("connected");
    const { rerender } = render(<DeckShell runtime={answering.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(answering.setShownTerminals).toHaveBeenCalledTimes(1));

    // The premise: a live terminal on a deck that is not the selected one.
    const before = screen.getByTestId("agent-pane-overlay");
    expect(within(before).getByTestId("terminal-planner")).toBeVisible();
    expect(answering.setShownTerminals).toHaveBeenLastCalledWith([{ deckId: REMOTE_DECK_ID, agentId: "planner" }]);

    viewportProps.length = 0;
    const lost = harness("disconnected");
    await act(async () => { rerender(<DeckShell runtime={lost.runtime("local")} initialView={paneView} />); });

    const pane = screen.getByTestId("agent-pane-overlay");
    // The same pane, not a replacement: losing a deck is a state, not a close.
    expect(pane).toBe(before);
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();

    // The state, readable from the DOM rather than inferred from a blank box.
    expect(pane.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "unreachable-deck");
    const absent = within(pane).getByTestId("terminal-absent-planner");
    expect(absent).toHaveAttribute("role", "status");
    // Which deck, and why. Named the way the rest of the UI names it —
    // `deckName`, which is also the overview group header — and carrying the
    // deck's own failure sentence rather than a guess.
    expect(absent).toHaveTextContent(REMOTE_DECK_LABEL);
    expect(absent).toHaveTextContent(/no live connection/i);
    expect(absent).toHaveTextContent(REMOTE_DECK_FAILURE);
    // And it does NOT say the old thing, which was about selection and is now
    // false about every reachable deck.
    expect(absent).not.toHaveTextContent(/not the selected deck/);

    // No terminal is MOUNTED any more, and nothing is declared shown — the two
    // halves that must agree, since either alone is a lie about the other.
    expect(viewportProps).toHaveLength(0);
    expect(screen.queryByTestId("terminal-planner")).not.toBeInTheDocument();
    expect(lost.setShownTerminals).toHaveBeenLastCalledWith([]);
    // And no input gate is rendered beside it: there is no terminal to type
    // into, so a `data-input-state` claim there would be about nothing.
    expect(screen.queryByTestId("terminal-input-status-planner")).not.toBeInTheDocument();

    // The rest of the pane is a working pane rather than a placeholder: the
    // agent's own identity, all five tabs, and the way out.
    expect(within(pane).getByRole("button", { name: "Close Planner on build-box agent" })).toBeVisible();
    expect(within(pane).getAllByRole("tab")).toHaveLength(5);
    // Nothing here writes the settings document, which is the line the descope
    // draws: opening a pane is navigation.
    expect(lost.saveSettings).not.toHaveBeenCalled();
  });

  /**
   * Scenario: that pane is open on build-box's Planner while build-box is not
   * answering, and build-box then answers again. The terminal attaches under
   * the pane — same pane, same agent, no close and no reopen — and the notice
   * goes away.
   *
   * This is the direction that proves the no-terminal state is a STATE. A pane
   * that could only ever lose its terminal would be indistinguishable from one
   * the failure had broken; this one recovers, on the same DOM node, which is
   * what `expect(after).toBe(before)` asserts rather than a rendering detail.
   *
   * The selected deck does not move in either test, and that is deliberate: it
   * is the deck's REACHABILITY that flips, which is now the only thing that
   * decides whether a pane has a terminal.
   */
  it("attaches the terminal and clears the notice when the pane's own deck answers again", async () => {
    const paneView = { kind: "agent" as const, deckId: REMOTE_DECK_ID, agentId: "planner", from: "overview" as const };
    const unreachable = harness("disconnected");
    const { rerender } = render(<DeckShell runtime={unreachable.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(unreachable.setShownTerminals).toHaveBeenCalledTimes(1));
    expect(unreachable.setShownTerminals).toHaveBeenLastCalledWith([]);

    const before = screen.getByTestId("agent-pane-overlay");
    expect(within(before).getByTestId("terminal-absent-planner")).toBeVisible();

    const answering = harness("connected");
    await act(async () => { rerender(<DeckShell runtime={answering.runtime("local")} initialView={paneView} />); });

    const after = screen.getByTestId("agent-pane-overlay");
    // The same pane, not a replacement: no unmount, no identity change.
    expect(after).toBe(before);
    expect(within(after).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();

    // The notice is gone and a terminal is there, keyed to the pane's own deck
    // — which is still NOT the selected one.
    expect(within(after).queryByTestId("terminal-absent-planner")).toBeNull();
    expect(after.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "attached");
    expect(within(after).getByTestId("terminal-planner")).toBeVisible();
    expect(viewportProps.at(-1)).toMatchObject({ agentId: "planner", deckId: REMOTE_DECK_ID });
    expect(answering.setShownTerminals).toHaveBeenLastCalledWith([{ deckId: REMOTE_DECK_ID, agentId: "planner" }]);
  });

  /**
   * Scenario: the same overview, same non-selected deck, but it is ANSWERING.
   * Opening its agent mounts a real terminal and declares it shown against
   * build-box — no explanation, no empty box, and no change to the selection.
   *
   * This is the control that stops the two tests above passing for the wrong
   * reason. Without it, a production change that rendered the no-terminal state
   * for *every* non-selected deck — the behaviour the product owner reversed —
   * would leave both of them green.
   */
  it("gives a non-selected but reachable deck's agent a live terminal instead", async () => {
    const deck = harness("connected");
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));

    fireEvent.click(openControl("Plan / architecture on build-box"));

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).queryByTestId("terminal-absent-planner")).toBeNull();
    expect(pane.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "attached");
    expect(viewportProps.at(-1)).toMatchObject({ agentId: "planner", deckId: REMOTE_DECK_ID });
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith([{ deckId: REMOTE_DECK_ID, agentId: "planner" }]);
  });

  /**
   * The control for every test above, stated as a fact about the fixture rather
   * than as an argument: both decks run an agent called `planner`, so "the
   * other deck's namesake" names a real, different agent on a real, different
   * machine. A fixture that avoided the collision would make the assertions
   * above pass for the wrong reason.
   */
  it("has the same agent id on both decks, which is what makes the deck in the sentence matter", () => {
    const deck = harness();

    expect(deck.local.connection.deckId).toBe(FIXTURE_DAEMON_ID);
    expect(deck.remote.connection.deckId).toBe(REMOTE_DECK_ID);
    expect(deck.local.agents.map((agent) => agent.id)).toEqual(deck.remote.agents.map((agent) => agent.id));
  });
});
