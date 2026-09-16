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

/**
 * Two decks running the SAME agent ids, which is the ordinary case rather than
 * a contrived one: agent ids are per-daemon monotonic integers and are unique
 * only within a daemon. The names differ so a test can say which deck's agent
 * it is looking at; the ids deliberately do not.
 */
function harness() {
  const local = createFixtureSnapshot("connected");
  const remote: DeckSnapshot = {
    ...local,
    connection: { ...local.connection, deckId: REMOTE_DECK_ID, socketPath: REMOTE_DECK_LABEL, deckKind: "remote" },
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
 * # The two halves, and why they are one condition
 *
 * The overview merges every observed deck (PRD
 * [#742](https://github.com/vfarcic/dot-agent-deck/issues/742)) and **every
 * agent it lists is openable** — a listed row with no way into it is a dead
 * row, and nothing on a row tells a reader which deck they are pointing at. A
 * terminal in this app is always the *selected* deck's, though: `overviewShown`
 * declares an attach only once the pane's deck is the one in force, because
 * `terminal::attach` resolves its daemon through the process-global
 * `trusted_daemon()` and agent ids collide across decks, so declaring
 * `[agentId]` under another deck attaches **that** deck's agent of the same id.
 *
 * So the pane opens, attaches nothing, and says so. `DeckShell`'s
 * `paneDeckSelected` is the single expression behind both, which is what stops
 * the two from ever disagreeing — a pane rendering a terminal nothing attached
 * to is precisely the lie this state replaces.
 *
 * # Why not a mounted terminal
 *
 * With no attach a `TerminalViewport` sits there receiving no bytes for as long
 * as the pane is open. That is a black rectangle reading *"this agent is
 * producing no output"* about an agent working normally on another machine.
 * Hence no viewport at all, and the mount-recording mock above rather than a
 * DOM query.
 *
 * # What this file does NOT claim
 *
 * Nothing here says a non-selected deck's terminal *cannot* attach — only that
 * today it does not, and that the pane is honest about it. Making it attach is
 * cross-deck attach, [#1073](https://github.com/vfarcic/dot-agent-deck/issues/1073),
 * a `PROTOCOL_VERSION` bump by construction (the attach frame header carries no
 * stream id) with four open identity findings in
 * [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116). When it lands
 * these tests are what it replaces.
 */
describe("a pane with no terminal", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /**
   * Scenario: with the local deck selected, open build-box's Planner from the
   * overview — a deck this app is not attached to. The pane opens and works;
   * its terminal tab carries no `TerminalViewport` at all but an explicit
   * no-terminal state naming build-box and saying that selecting build-box
   * attaches the terminal. Nothing is declared shown.
   */
  it("opens a non-selected deck's agent with an explicit no-terminal state naming its deck", async () => {
    const deck = harness();
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith([]);

    fireEvent.click(openControl("Plan / architecture on build-box"));

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();

    // The state, readable from the DOM rather than inferred from a blank box.
    expect(pane.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "other-deck");
    const absent = within(pane).getByTestId("terminal-absent-planner");
    expect(absent).toHaveAttribute("role", "status");
    // Which deck, and what selecting it does. Named the way the rest of the UI
    // names it — `deckName`, which is also the overview group header the user
    // just came from and the label in the Deck selector they are pointed at.
    expect(absent).toHaveTextContent(REMOTE_DECK_LABEL);
    expect(absent).toHaveTextContent(/not the selected deck/);
    expect(absent).toHaveTextContent(/select .*attaches/i);

    // No terminal was MOUNTED for it, and nothing was declared shown — the two
    // halves that must agree, since either alone is a lie about the other.
    expect(viewportProps).toHaveLength(0);
    expect(screen.queryByTestId("terminal-planner")).not.toBeInTheDocument();
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith([]);
    expect(deck.setShownTerminals).toHaveBeenCalledTimes(1);
    // And no input gate is rendered beside it: there is no terminal to type
    // into, so a `data-input-state` claim there would be about nothing.
    expect(screen.queryByTestId("terminal-input-status-planner")).not.toBeInTheDocument();

    // The rest of the pane is a working pane rather than a placeholder: the
    // agent's own identity, all five tabs, and the way out.
    expect(within(pane).getByRole("button", { name: "Close Planner on build-box agent" })).toBeVisible();
    expect(within(pane).getAllByRole("tab")).toHaveLength(5);
    // Nothing here moves the selection for the user, which is the line the
    // descope draws: the remedy is stated, not performed.
    expect(deck.saveSettings).not.toHaveBeenCalled();
  });

  /**
   * Scenario: that pane is open on build-box's Planner when the user selects
   * build-box. The terminal attaches under the pane — same pane, same agent, no
   * close and no reopen — and the notice goes away.
   *
   * This is the reverse of `keeps an overview pane … when the selection moves
   * under it` (`AgentPaneDeckIdentity.test.tsx`), and it is the direction that
   * proves the no-terminal state is a STATE. A pane that could only ever lose
   * its terminal would be indistinguishable from one the move had broken; this
   * one recovers, on the same DOM node, which is what `expect(after).toBe(
   * before)` asserts rather than a rendering detail.
   */
  it("attaches the terminal and clears the notice when the pane's own deck is selected", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));

    fireEvent.click(openControl("Plan / architecture on build-box"));
    const before = screen.getByTestId("agent-pane-overlay");
    expect(within(before).getByTestId("terminal-absent-planner")).toBeVisible();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("remote")} initialView={{ kind: "overview" }} />); });

    const after = screen.getByTestId("agent-pane-overlay");
    // The same pane, not a replacement: no unmount, no identity change.
    expect(after).toBe(before);
    expect(within(after).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();

    // The notice is gone and a terminal is there, keyed to the pane's own deck.
    expect(within(after).queryByTestId("terminal-absent-planner")).toBeNull();
    expect(after.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "attached");
    expect(within(after).getByTestId("terminal-planner")).toBeVisible();
    expect(viewportProps.at(-1)).toMatchObject({ agentId: "planner", deckId: REMOTE_DECK_ID });
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith(["planner"]);
  });

  /**
   * The control for both tests above, stated as a fact about the fixture rather
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
