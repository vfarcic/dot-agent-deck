import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { ALL_ENDPOINT_SELECTION, DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./lib/bridge";
import type { AgentTarget, DeckRuntimeState, DeckSnapshot } from "./types";

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
 * The status the fixture's `planner` carries, read from the fixture rather than
 * written out, because issue #1143's assertions are about how the chip is
 * WORDED and not about which status it happens to hold — a fixture edit should
 * not silently turn them into assertions about nothing.
 */
const PANE_AGENT_STATUS = createFixtureSnapshot("connected").agents.find((agent) => agent.id === "planner")!.status;

/**
 * Two decks running the SAME agent ids, which is the ordinary case rather than
 * a contrived one: agent ids are per-daemon monotonic integers and are unique
 * only within a daemon. The names differ so a test can say which deck's agent
 * it is looking at; the ids deliberately do not.
 *
 * `remoteStatus` is the whole variable this file turns: the remote deck is
 * either answering, in which case its agents attach exactly like the selected
 * deck's, or it is not, in which case **it lists no agents at all**.
 *
 * # That empty list is the correction issue #1143 made to this file
 *
 * This harness used to keep `agents` populated on a disconnected deck, and the
 * crate never does: `disconnected_snapshot` and `snapshot_with`'s non-connected
 * early return both carry `agents: Vec::new()` — they are the only two empty
 * agent lists under `desktop/src-tauri/src/` and both are on the non-connected
 * path — and `mapDesktopSnapshot` builds `agents` from `dto.agents` alone,
 * carrying none over from the previous snapshot. So every assertion in this
 * file about an unreachable deck's pane was made GIVEN a record the production
 * path does not supply, and the state they assert was unreachable in the app
 * while every test here was green. A harness that models a failure more kindly
 * than production does is worse than no harness: it certifies the thing it was
 * built to catch.
 *
 * So `disconnected` now empties the list unconditionally, which makes these
 * tests load-bearing for the record-holding that #1143 added — take the hold
 * out and they fail, where before they could not.
 *
 * `retired` is the second variable, added for the describe block at the bottom:
 * the id of an agent the REMOTE deck no longer lists **while answering**. It
 * now stands in for one of the two ways a pane's agent stops resolving rather
 * than both, because the other one — a deck that has stopped answering — is
 * what `remoteStatus` itself produces, and conflating them is what let the two
 * be treated alike.
 */
function harness(remoteStatus: "connected" | "disconnected" = "connected", retired?: string, selection: string = ALL_ENDPOINT_SELECTION) {
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
    agents: remoteStatus === "connected"
      ? local.agents
          .filter((agent) => agent.id !== retired)
          .map((agent) => ({ ...agent, daemonId: REMOTE_DECK_ID, role: `${agent.role} on build-box`, displayName: `${agent.displayName} on build-box` }))
      : [],
  };
  /* Typed with the real signature, so a test can read back WHICH targets were
     declared rather than only the most recent call. */
  const setShownTerminals = vi.fn(async (_targets: AgentTarget[]) => undefined);
  /*
    A document that lists the remote deck and selects ALL, which is the state a
    fleet is observed under. Cloned on the way out of `getSettings` so the hook
    cannot mutate the fixture.
  */
  const stored: DesktopSettingsDto = {
    ...structuredClone(DEFAULT_DESKTOP_SETTINGS),
    endpoints: { remote: [{ id: "row-build-box", host: "build-box", user: "dev", port: 22, socket: "/run/deck.sock" }], selection },
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

/** The stored row id for build-box, which is also its selection token. */
const BUILD_BOX_ROW = "row-build-box";

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
   *
   * # The premise is load-bearing, and issue #1143 is why it is spelled out
   *
   * The rerender below is to a deck that lists NO agents, which is what the
   * crate reports for one with no live link. So the record every field in this
   * pane is drawn from is one `DeckShell` HELD from the healthy commit — take
   * that hold away and there is no pane here at all, which is precisely the
   * state this file used to certify while its harness quietly kept the agents
   * populated.
   *
   * A held record is shown as a past report rather than a present one, and the
   * three affordances asserted below are that decision: the status says `last
   * seen`, `data-agent-record` says `held`, and the sentence dates the report.
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

    /*
      Issue #1143 — the record on screen is HELD, and the pane says so three
      ways. Without all three the pane asserts a liveness the app has no
      evidence for, which the issue judges worse than rendering nothing.
    */
    // 1. The sentence dates the report, in the app's own relative vocabulary,
    //    with the exact instant on the hover — the `ActivityDisplay` pairing
    //    every other relativised instant in this app uses. The label itself is
    //    asserted exactly in `lib/terminalInput.test.ts`, against an injected
    //    age; what this proves is that a REAL instant reached it.
    expect(absent).toHaveTextContent(/last reported (just now|\d+[mhd] ago), and nothing in it is being updated/);
    expect(absent.getAttribute("title")).toMatch(/^Last reported by the deck at: \d{4}-\d{2}-\d{2}T/);
    // 2. The machine-readable seam, sibling of `data-status`.
    expect(pane.querySelector(".agent-tile")).toHaveAttribute("data-agent-record", "held");
    // 3. The status stops asserting. It is the field that lies hardest — a bare
    //    `running` is a claim about NOW — so it is worded as the past reading it
    //    is, and drops the live colour that says "now" louder than a label can
    //    unsay.
    const status = within(pane).getByText(`last seen: ${PANE_AGENT_STATUS}`);
    expect(status).toHaveClass("is-held");
    expect(within(pane).queryByText(PANE_AGENT_STATUS, { exact: true })).toBeNull();

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
   * Scenario: build-box's Planner is open and working, build-box stops
   * answering, and then answers again. The terminal comes back under the pane —
   * same pane, same agent, no close and no reopen — the notice goes away, and
   * the header stops hedging: the status is a plain `running` again and the
   * record reads `live`.
   *
   * This is the direction that proves the no-terminal state is a STATE. A pane
   * that could only ever lose its terminal would be indistinguishable from one
   * the failure had broken; this one recovers, on the same DOM node, which is
   * what `expect(after).toBe(before)` asserts rather than a rendering detail.
   *
   * # It is a ROUND TRIP now, and that is issue #1143's correction
   *
   * It used to start cold on a disconnected deck, which the corrected harness
   * makes impossible to render — there is no record to hold, because no deck
   * ever gave one — and which was never a path a user could reach anyway: the
   * overview lists no agents for a deck that is not answering, so there is no
   * control to press. Going healthy -> away -> healthy is the only sequence
   * that reaches this state in the app, and it also proves the half a cold
   * start could not: that the held record is **surrendered** when a live one
   * arrives, rather than pinning the pane to a past reading forever.
   *
   * The selected deck does not move in either test, and that is deliberate: it
   * is the deck's REACHABILITY that flips, which is now the only thing that
   * decides whether a pane has a terminal.
   */
  it("attaches the terminal and clears the notice when the pane's own deck answers again", async () => {
    const paneView = { kind: "agent" as const, deckId: REMOTE_DECK_ID, agentId: "planner", from: "overview" as const };
    const healthy = harness("connected");
    const { rerender } = render(<DeckShell runtime={healthy.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(healthy.setShownTerminals).toHaveBeenCalledTimes(1));

    const before = screen.getByTestId("agent-pane-overlay");

    const unreachable = harness("disconnected");
    await act(async () => { rerender(<DeckShell runtime={unreachable.runtime("local")} initialView={paneView} />); });

    // The premise: a pane rendered entirely from the held record, saying so.
    expect(within(before).getByTestId("terminal-absent-planner")).toBeVisible();
    expect(before.querySelector(".agent-tile")).toHaveAttribute("data-agent-record", "held");
    expect(unreachable.setShownTerminals).toHaveBeenLastCalledWith([]);

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

    // The hold is SURRENDERED: a live record outranks a held one, so the header
    // goes back to asserting. A pane that kept hedging after its deck returned
    // would be the mirror image of the defect #1143 fixed.
    expect(after.querySelector(".agent-tile")).toHaveAttribute("data-agent-record", "live");
    expect(within(after).getByText(PANE_AGENT_STATUS, { exact: true })).toBeVisible();
    expect(within(after).queryByText(/last seen/i)).toBeNull();
  });

  /**
   * Scenario: the app comes up already holding a pane view for an agent on a
   * deck that has never answered — a deep link, or `initialView` — so nothing
   * has ever reported the record the pane would be drawn from. No pane renders.
   * The view survives, so when that deck answers the pane is simply there.
   *
   * # This is the LIMIT of #1143's fix, asserted rather than left implicit
   *
   * The fix holds the last record a pane's deck gave. Where no deck ever gave
   * one there is nothing to hold, anywhere in the app — the crate reports no
   * agents for a deck with no live link, and `mapDesktopSnapshot` carries none
   * over — so this is not a hold that failed but a record that does not exist.
   * Rendering a pane here would mean inventing one.
   *
   * It is also not a path a user reaches by pressing anything: the overview
   * lists no agents for a deck that is not answering, so the Open control this
   * pane would come from is not on the screen. What the app owes here is that
   * the view is kept, which the last two steps prove.
   */
  it("renders no pane for a deck that has never answered, and brings one back when it does", async () => {
    const paneView = { kind: "agent" as const, deckId: REMOTE_DECK_ID, agentId: "planner", from: "overview" as const };
    const cold = harness("disconnected");
    const { rerender } = render(<DeckShell runtime={cold.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(cold.setShownTerminals).toHaveBeenCalledTimes(1));

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    // Nothing is declared shown, and nothing is mounted for an agent no deck
    // has vouched for.
    expect(cold.setShownTerminals).toHaveBeenLastCalledWith([]);
    expect(viewportProps).toHaveLength(0);

    const answering = harness("connected");
    await act(async () => { rerender(<DeckShell runtime={answering.runtime("local")} initialView={paneView} />); });

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(pane.querySelector(".agent-tile")).toHaveAttribute("data-agent-record", "live");
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
    // And the header ASSERTS, because this record is the deck's current answer.
    // Issue #1143's hedging is for a held record and must not leak to a live
    // one, which is the failure mode opposite to the one it fixed.
    expect(pane.querySelector(".agent-tile")).toHaveAttribute("data-agent-record", "live");
    expect(within(pane).getByText(PANE_AGENT_STATUS, { exact: true })).toBeVisible();
  });

  /**
   * Scenario: a DECK-origin pane is open — build-box is the selected deck and
   * its Planner is a promoted tile — and build-box stops answering. No pane is
   * rendered from the held record, and the deck screen's own disconnected
   * banner, with the Reconnect control on it, is reachable.
   *
   * # This is a scope decision, and the reason is measured rather than argued
   *
   * Issue #1143's hold is read by the OVERVIEW-origin pane and by nothing else.
   * The deck path is not an oversight and not an effort question: a promoted
   * pane marks everything that is not one of its own ancestors `inert`
   * (`useInertBackground`, whose doc names a connection banner as exactly the
   * background content it re-marks on every commit). So a held deck-origin pane
   * would explain that the deck is not answering while putting the one control
   * that fixes it behind an inert barrier — measured on this very fixture: with
   * a pane promoted and a banner showing, the banner carries `inert` and its
   * `Reconnect` button inherits it.
   *
   * The two screens are not symmetrical, which is why one answer does not fit
   * both. The overview goes on listing every other deck's agents normally, so
   * the missing pane is the only thing that can say what happened, and `Esc`
   * returns to a screen that is still useful. The deck screen says it in a
   * banner, in its own words, with the remedy attached — and has no tiles at
   * all, so there is nothing for a reader to misread as live.
   *
   * What the deck path keeps is the guarantee PR #1126 gave it: the view is not
   * closed, so the pane is back the moment the deck answers.
   */
  it("leaves a deck-origin pane's screen to explain itself, with its remedy reachable", async () => {
    const deckView = { kind: "agent" as const, deckId: REMOTE_DECK_ID, agentId: "planner", from: "deck" as const };
    // build-box selected, not All Decks: the deck screen shows one deck, and
    // under All Decks it shows "Select a deck" instead (#1083).
    const answering = harness("connected", undefined, BUILD_BOX_ROW);
    const { rerender } = render(<DeckShell runtime={answering.runtime("remote")} initialView={deckView} />);
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();

    const away = harness("disconnected", undefined, BUILD_BOX_ROW);
    await act(async () => { rerender(<DeckShell runtime={away.runtime("remote")} initialView={deckView} />); });

    // No pane, so nothing is inert and the screen speaks for itself.
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    const banner = document.querySelector(".connection-banner");
    expect(banner).not.toBeNull();
    expect(banner).not.toHaveAttribute("inert");
    expect(within(banner as HTMLElement).getByRole("button", { name: /Reconnect/i })).toBeVisible();
    expect(banner).toHaveTextContent(REMOTE_DECK_FAILURE);

    // The view was KEPT, so the pane is back when the deck is — PR #1126's
    // guarantee, which this path still has and still needs.
    const returned = harness("connected", undefined, BUILD_BOX_ROW);
    await act(async () => { rerender(<DeckShell runtime={returned.runtime("remote")} initialView={deckView} />); });
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
  });

  /**
   * The control for every test above, stated as a fact about the fixture rather
   * than as an argument: both decks run an agent called `planner`, so "the
   * other deck's namesake" names a real, different agent on a real, different
   * machine. A fixture that avoided the collision would make the assertions
   * above pass for the wrong reason.
   *
   * Asserted while both decks are ANSWERING, which is the only state in which
   * either has an agent list at all — see the harness's note on why a
   * disconnected deck's list is empty.
   */
  it("has the same agent id on both decks, which is what makes the deck in the sentence matter", () => {
    const deck = harness("connected");

    expect(deck.local.connection.deckId).toBe(FIXTURE_DAEMON_ID);
    expect(deck.remote.connection.deckId).toBe(REMOTE_DECK_ID);
    expect(deck.local.agents.map((agent) => agent.id)).toEqual(deck.remote.agents.map((agent) => agent.id));
  });
});

/**
 * PRD #1105 open question 3, and the PR review's P1 on `OverviewAgentPane` —
 * **the view must not outlive its subject, and the shown declaration must not
 * outlive it either.**
 *
 * # The two cases look identical at the pane and must not be treated alike
 *
 * Both reach the pane as *"the agent this view names is not in the fleet"*, and
 * that is where the resemblance stops.
 *
 * A deck that is **answering** and does not list the agent is the daemon having
 * ended it. Its agent list is an answer: `connected_snapshot` is the only
 * producer of a connected snapshot and it maps a `ListAgents` reply every time
 * (`desktop/src-tauri/src/daemon_bridge.rs`), so an absence there is a real
 * absence. There is nothing left for the pane to be a pane over, and the view
 * closes to the screen it was opened from.
 *
 * A deck that is **not answering** reports `agents: Vec::new()` — both empty
 * lists in the crate are on that path, `disconnected_snapshot` and
 * `snapshot_with`'s non-connected early return — so the same absence there is
 * the deck saying nothing about its agents rather than saying they are gone.
 * Closing on it would throw a healthy pane away every time a remote deck
 * blinked, which is strictly worse than the defect: the PRD's own decision is
 * that such a deck *"replaces its terminal with a sentence rather than closing
 * the pane"*.
 *
 * So the condition is the pane deck being CONNECTED and not listing the agent,
 * and `DeckShell`'s `paneDeckAttachable` is already exactly that first half —
 * which is why the close reads it rather than a second comparison of the same
 * values.
 *
 * # Why the declaration half is not cosmetic
 *
 * `useShownTerminals` keys its effect on the joined `(deckId, agentId)` string,
 * so an agent leaving the fleet does not change the key and the effect does not
 * re-fire — one declaration made on open just stands. It is the BRIDGE that
 * turns that into a repeat: `subscribe`'s `desktop://snapshot` listener runs
 * `attachAgents(Array.from(this.shown.values()))` on every snapshot, and the
 * daemon's `end` event for the vanished agent has already taken it out of
 * `attached`, so it passes `attachAgents`' filter afresh each time. Each pass
 * takes the process-wide `attach_gate` and opens a socket for an agent that no
 * longer exists, serialising against the attaches of panes that do.
 */
describe("a pane whose agent has left the fleet", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /** The pane under test throughout: build-box's Planner, opened from the overview. */
  const paneView = { kind: "agent" as const, deckId: REMOTE_DECK_ID, agentId: "planner", from: "overview" as const };

  /**
   * Scenario: build-box's Planner is open in a working pane, and build-box —
   * still answering — stops listing that agent because the daemon ended it.
   * The pane goes away and the user is back on the overview, which lists what
   * does exist; nothing is declared shown for the agent any more; and bringing
   * the agent back does not resurrect the pane, because the view was closed
   * rather than merely hidden.
   *
   * The last step is what separates a close from a blank frame, and it is the
   * whole of the finding: before this, `OverviewAgentPane` returned `null` while
   * `DeckShell` stayed in `view.kind === "agent"` and went on declaring the
   * agent shown.
   */
  it("closes the view and stops declaring the agent when a deck that is answering retires it", async () => {
    const answering = harness("connected");
    const { rerender } = render(<DeckShell runtime={answering.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(answering.setShownTerminals).toHaveBeenCalledTimes(1));

    // The premise: a working pane with a live terminal on the agent's own deck.
    expect(within(screen.getByTestId("agent-pane-overlay")).getByTestId("terminal-planner")).toBeVisible();
    expect(answering.setShownTerminals).toHaveBeenLastCalledWith([{ deckId: REMOTE_DECK_ID, agentId: "planner" }]);

    const retired = harness("connected", "planner");
    await act(async () => { rerender(<DeckShell runtime={retired.runtime("local")} initialView={paneView} />); });

    // No pane, and no frame where one was: the view is back on the screen it
    // was opened from, which is the one that can say the agent is gone.
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Close .* agent$/ })).not.toBeInTheDocument();
    expect(screen.getByTestId("overview-open-deck")).toBeVisible();
    // And nothing is declared shown for it any more, which is the half no
    // amount of looking at the screen would have caught.
    expect(retired.setShownTerminals).toHaveBeenLastCalledWith([]);
    expect(retired.setShownTerminals.mock.calls.flatMap(([targets]) => targets)).not.toContainEqual({ deckId: REMOTE_DECK_ID, agentId: "planner" });

    // Closed, not hidden. An agent of the same id arriving later gets a row on
    // the overview, not the pane the user opened for the one that ended.
    const back = harness("connected");
    await act(async () => { rerender(<DeckShell runtime={back.runtime("local")} initialView={paneView} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(back.setShownTerminals).toHaveBeenLastCalledWith([]);
  });

  /**
   * Scenario: the same open pane, and build-box stops answering — reporting no
   * agents at all, which is what a deck with no live link reports. The view is
   * NOT closed: when build-box answers again the pane is there with its
   * terminal, having never been navigated away from.
   *
   * This is the control that stops the test above passing for the wrong reason.
   * The cheap version of that fix — close whenever the agent fails to resolve —
   * is green on it and throws a healthy pane away here, on nothing worse than a
   * remote deck blinking.
   *
   * The reconnect is how the claim is made observable. A view that is merely
   * rendering nothing and a view that has been closed look identical while the
   * deck is away; only the return distinguishes them.
   */
  it("keeps the view when the pane's deck stops answering, so the pane returns when it answers again", async () => {
    const answering = harness("connected");
    const { rerender } = render(<DeckShell runtime={answering.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(answering.setShownTerminals).toHaveBeenCalledTimes(1));
    expect(within(screen.getByTestId("agent-pane-overlay")).getByTestId("terminal-planner")).toBeVisible();

    // A deck with no live link, exactly as the crate reports one: the deck is
    // still in the fleet and it lists no agents. The second argument is gone
    // because it would say nothing — issue #1143 made `disconnected` empty the
    // list on its own, which is what the crate does and what this test always
    // claimed to be modelling.
    const away = harness("disconnected");
    await act(async () => { rerender(<DeckShell runtime={away.runtime("local")} initialView={paneView} />); });

    // Nothing is declared shown while the deck cannot be attached over — which
    // is `paneDeckAttachable` doing its existing job, and is also why a
    // standing declaration cannot outlive a deck going away.
    expect(away.setShownTerminals).toHaveBeenLastCalledWith([]);
    expect(screen.queryByTestId("terminal-planner")).not.toBeInTheDocument();
    // The view is kept AND, since #1143, rendered — from the record held for
    // this pane. The close below is what must not happen; a blank frame was
    // what used to happen instead.
    expect(screen.getByTestId("agent-pane-overlay").querySelector(".agent-tile")).toHaveAttribute("data-agent-record", "held");

    viewportProps.length = 0;
    const returned = harness("connected");
    await act(async () => { rerender(<DeckShell runtime={returned.runtime("local")} initialView={paneView} />); });

    // The view survived the outage, so the pane is simply back — same agent,
    // same deck, live terminal, and the declaration naming it again.
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(viewportProps.at(-1)).toMatchObject({ agentId: "planner", deckId: REMOTE_DECK_ID });
    expect(returned.setShownTerminals).toHaveBeenLastCalledWith([{ deckId: REMOTE_DECK_ID, agentId: "planner" }]);
  });

  /**
   * Scenario: the same retirement under a DECK-origin pane — build-box is the
   * selected deck, its Planner is open as a promoted tile, and build-box stops
   * listing it. The view closes and the deck grid is left with no pane over it.
   *
   * The deck path's shown declaration was never at risk: `DeckSurface` derives
   * it from `snapshot.agents`, so a retired agent shrinks the set, the joined
   * key changes and the effect re-fires on its own. What the deck path shared
   * with the overview was the STALE VIEW — `paneAgentId` resolves to
   * `undefined` and promotes no tile, so the screen self-heals while `DeckShell`
   * stays in `view.kind === "agent"` with the `Escape` listener still bound to a
   * pane nothing is rendering. One condition in `DeckShell` closes both, and
   * this is the half of it the deck surface cannot prove on its own.
   */
  it("closes a deck-origin pane when the deck under it retires the agent", async () => {
    const deckView = { ...paneView, from: "deck" as const };
    // build-box selected, for the reason the test above gives.
    const answering = harness("connected", undefined, BUILD_BOX_ROW);
    const { rerender } = render(<DeckShell runtime={answering.runtime("remote")} initialView={deckView} />);

    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();

    const retired = harness("connected", "planner", BUILD_BOX_ROW);
    await act(async () => { rerender(<DeckShell runtime={retired.runtime("remote")} initialView={deckView} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    // Closed rather than waiting to resurrect: the agent coming back gets a
    // tile offering Open, not the pane.
    const back = harness("connected", undefined, BUILD_BOX_ROW);
    await act(async () => { rerender(<DeckShell runtime={back.runtime("remote")} initialView={deckView} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner-on-build-box")).toHaveAttribute("data-presentation", "tile");
    expect(screen.getByRole("button", { name: "Open Planner on build-box agent" })).toBeVisible();
  });

  /**
   * Scenario: the app comes up already holding a pane view for an agent
   * build-box does not list — a deep link, or the pane's own `initialView`,
   * which `closeAgent`'s doc comment names as a view *"that was never navigated
   * TO"*. Nothing is ever declared shown for that agent, not even on the first
   * commit, and the app lands on the overview.
   *
   * This is the half of the fix the three tests above cannot see, and the reason
   * it is a separate case rather than a tidier condition. The close is an
   * EFFECT, so it runs after the commit is painted — but `useShownTerminals`
   * fires on that same first commit, when the shown key goes from nothing to
   * the pane's agent. Gating the declaration on the deck alone therefore
   * declares an agent that does not exist and waits to be rescued, which is the
   * same shape as `DeckSurface` refusing to promote a tile on its own rather
   * than trusting the close to arrive (`promotes no tile for an open-pane
   * identity naming another deck`, in `AgentPaneDeckIdentity.test.tsx`).
   */
  it("never declares an agent the pane's deck does not list, not even on the first commit", async () => {
    const retired = harness("connected", "planner");
    render(<DeckShell runtime={retired.runtime("local")} initialView={paneView} />);
    await waitFor(() => expect(retired.setShownTerminals).toHaveBeenCalled());

    expect(retired.setShownTerminals.mock.calls.flatMap(([targets]) => targets)).toEqual([]);
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("overview-open-deck")).toBeVisible();
  });
});
