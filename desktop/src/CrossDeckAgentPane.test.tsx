import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { ALL_ENDPOINT_SELECTION, DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./lib/bridge";
import type { AgentSession, DeckRuntimeState, DeckSnapshot } from "./types";

/**
 * The mock records the identity and the applied geometry it was handed, because
 * PRD #1105's security audit found both being resolved by BARE agent id — and
 * an id alone names an agent on every deck, so neither is observable from the
 * DOM the way a wrong heading would be.
 */
const viewportProps: { agentId: string; deckId?: string; applied?: { rows: number; cols: number } }[] = [];
vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, deckId, label, applied }: { agentId: string; deckId?: string; label: string; applied?: { rows: number; cols: number } }) => {
    viewportProps.push({ agentId, deckId, applied });
    return <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>terminal</pre>;
  },
}));

import { DeckShell, DeckSurface } from "./App";
import { agentKey } from "./lib/agentKey";
import type { DesktopSettingsState } from "./hooks/useDesktopSettings";

/**
 * The second deck, described exactly as the crate describes a remote one —
 * `Endpoint::describe()` renders `user@host[:port]` and never a socket path —
 * so the label this fixture carries is the label `describeEndpoint` derives
 * from the stored row below it.
 */
const REMOTE_DECK_ID = "deck-00000000000000b2";
const REMOTE_DECK_LABEL = "dev@build-box";
const REMOTE_ROW_ID = "row-build-box";

/** The stored row that names the remote deck, and the document holding it. */
const remoteRow = { id: REMOTE_ROW_ID, host: "build-box", user: "dev", port: 22, socket: "/run/deck.sock" };
const documentWithFleet = (): DesktopSettingsDto => ({
  ...structuredClone(DEFAULT_DESKTOP_SETTINGS),
  endpoints: { remote: [structuredClone(remoteRow)], selection: ALL_ENDPOINT_SELECTION },
});

/**
 * Two decks running the SAME agent ids, which is the ordinary case rather than
 * a contrived one: agent ids are per-daemon monotonic integers and are *"unique
 * only within a daemon"*. The names differ so a test can say which deck's agent
 * it is looking at; the ids deliberately do not.
 */
function decks(): { local: DeckSnapshot; remote: DeckSnapshot } {
  const local = createFixtureSnapshot("connected");
  const remote: DeckSnapshot = {
    ...local,
    connection: { ...local.connection, deckId: REMOTE_DECK_ID, socketPath: REMOTE_DECK_LABEL, deckKind: "remote" },
    agents: local.agents.map((agent) => ({ ...agent, daemonId: REMOTE_DECK_ID, role: `${agent.role} on build-box`, displayName: `${agent.displayName} on build-box` })),
  };
  return { local, remote };
}

/**
 * A runtime whose callbacks keep their identity across re-renders, so a test
 * can move the SELECTED deck — which is what applying a settings save does —
 * without re-running `useDesktopSettings`' load or rebuilding its `save`.
 */
function harness(
  initial: DesktopSettingsDto = documentWithFleet(),
  extra: Partial<DeckRuntimeState> = {},
  /** Applied to every agent on BOTH decks, so the two stay each other's mirror. */
  mutateAgent: (agent: AgentSession) => AgentSession = (agent) => agent,
) {
  const pair = decks();
  const local = { ...pair.local, agents: pair.local.agents.map(mutateAgent) };
  const remote = { ...pair.remote, agents: pair.remote.agents.map(mutateAgent) };
  let stored = structuredClone(initial);
  const saveSettings = vi.fn(async (next: DesktopSettingsDto) => {
    stored = structuredClone(next);
    return structuredClone(stored);
  });
  const getSettings = vi.fn(async () => ({ settings: structuredClone(stored), path: "/tmp/desktop.toml" }));
  const setShownTerminals = vi.fn(async () => undefined);
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
    getSettings,
    saveSettings,
  };
  /** The runtime as it looks while `selected` is the deck in force. */
  const runtime = (selected: "local" | "remote"): DeckRuntimeState => ({
    ...base,
    ...extra,
    snapshot: selected === "local" ? local : remote,
    fleet: selected === "local" ? [local, remote] : [remote, local],
  } as unknown as DeckRuntimeState);
  return {
    local,
    remote,
    runtime,
    setShownTerminals,
    saveSettings,
    /** Whatever the last save left on disk. */
    onDisk: () => structuredClone(stored),
    savedDocuments: () => saveSettings.mock.calls.map(([document]) => document),
  };
}

/** The overview's open control for one agent, named as the row renders it. */
const openControl = (name: string) => screen.getByRole("button", { name: `Open ${name} agent` });

/**
 * PRD #1105 M6 — opening a NON-SELECTED deck's agent.
 *
 * The overview merges every observed deck, while a tile's terminal is always
 * the selected deck's — so this is the one path where the pane and the deck in
 * force disagree, and the whole milestone is about closing that disagreement
 * and then putting the selection back.
 */
describe("cross-deck agent pane", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /**
   * Scenario: with All Decks selected and the local deck in force, open the
   * build-box deck's Planner from the overview. The app writes the settings
   * document once, moving the selection to that deck's stored row and changing
   * nothing else; then, once the switch has taken effect, it closes the pane
   * and writes the document back. The DTO after the round trip is equal to the
   * one it started from, remote rows included.
   *
   * **What this establishes, stated at its real width** (PRD #1105's security
   * audit). `onDisk` below is an in-memory object and both saves are mocked, so
   * what round-trips here is the **settings DTO** — every field this app sends
   * and reads back, which is what "a selection move asserts nothing about the
   * stored rows" needs. It is NOT a claim about the bytes of `desktop.toml`,
   * and that claim would be false: `desktop_set_settings` re-reads the file
   * into a `toml::Table` and rewrites it with `toml::to_string_pretty`
   * (`desktop/src-tauri/src/settings.rs`), and a `Table` represents neither
   * comments nor the original layout. So a hand-formatted or commented document
   * is rewritten by the first automatic cross-deck switch, and the second write
   * goes down the same path and restores nothing. Semantically identical, byte
   * for byte not.
   */
  it("switches the selected deck on open and restores the settings DTO unchanged on close", async () => {
    const deck = harness();
    const before = JSON.stringify(deck.onDisk());
    const { rerender } = render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.saveSettings).not.toHaveBeenCalled());

    fireEvent.click(openControl("Plan / architecture on build-box"));
    await waitFor(() => expect(deck.saveSettings).toHaveBeenCalledTimes(1));

    const switched = deck.savedDocuments()[0];
    expect(switched.endpoints).toEqual({ remote: [remoteRow], selection: REMOTE_ROW_ID });
    // Everything else the document says is untouched — a deck switch is a
    // selection move and nothing more, and `endpointSectionToSave` is what
    // keeps it from asserting `remote: []` over rows it never rendered.
    expect({ ...switched, endpoints: undefined }).toEqual({ ...JSON.parse(before), endpoints: undefined });

    // The crate applies the save: it detaches every session and the newly
    // selected deck becomes the one the single-deck surfaces are on.
    await act(async () => { rerender(<DeckShell runtime={deck.runtime("remote")} initialView={{ kind: "overview" }} />); });
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();

    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Close Planner on build-box agent" }));
    await waitFor(() => expect(deck.saveSettings).toHaveBeenCalledTimes(2));

    expect(JSON.stringify(deck.onDisk())).toBe(before);
  });

  /**
   * Scenario: open an agent on the deck that is ALREADY selected, from the
   * overview, and close it again. Nothing is written: a switch costs a disk
   * write and a full terminal teardown on the crate's side, so the path that
   * does not need one must not take it.
   */
  it("writes nothing when the pane's deck is already the selected one", async () => {
    const deck = harness();
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.saveSettings).not.toHaveBeenCalled());

    fireEvent.click(openControl("Plan / architecture"));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Close Planner agent" }));

    await waitFor(() => expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument());
    expect(deck.saveSettings).not.toHaveBeenCalled();
  });

  /**
   * Scenario: two stored rows describe the same address, so the deck the pane
   * wants cannot be named from this document — `dev@build-box` matches both and
   * neither is more likely than the other. The app opens the pane and writes
   * nothing rather than selecting one of them.
   *
   * The refusal is the safe direction and not timidity: agent ids are
   * per-daemon monotonic, so selecting the wrong row would attach a different
   * machine's agent of the same id under the right name.
   */
  it("does not guess when two stored rows describe the pane's deck", async () => {
    const ambiguous = documentWithFleet();
    ambiguous.endpoints = {
      remote: [structuredClone(remoteRow), { ...structuredClone(remoteRow), id: "row-build-box-2", socket: "/run/other.sock" }],
      selection: ALL_ENDPOINT_SELECTION,
    };
    const deck = harness(ambiguous);
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.saveSettings).not.toHaveBeenCalled());

    fireEvent.click(openControl("Plan / architecture on build-box"));
    await act(async () => { await Promise.resolve(); });

    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(deck.saveSettings).not.toHaveBeenCalled();
  });

  /**
   * Scenario: the pane's identity is composite, so the pane opened for the
   * build-box deck's `planner` shows THAT deck's agent — not the identically
   * identified `planner` on the deck that is still in force while the switch is
   * being applied.
   */
  it("resolves the pane's agent on the deck the view names, not on the selected one", async () => {
    const deck = harness();
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.saveSettings).not.toHaveBeenCalled());

    fireEvent.click(openControl("Plan / architecture on build-box"));

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();
  });

  /**
   * The M6 ORDERING TEST, and the question it exists to answer: across the
   * overview round trip, is there an interval in which the app has declared
   * something the bridge cannot honour — a shown terminal on a deck that is not
   * in force, or a shown set left stale after the switch?
   *
   * The adverse interval is the one between the save and the crate applying it.
   * `terminal::attach` attaches against `trusted_daemon` — whichever deck is
   * linked at the instant it runs — and the ids collide across decks, so a
   * declaration made in that interval would attach the OLD deck's `planner`,
   * which the switch then detaches. Repair would wait on an `End` and a
   * snapshot, in an order nothing guarantees.
   *
   * What this asserts is that the interval is never entered: the owner declares
   * the empty set until the pane's deck IS the selected one, then declares that
   * one agent exactly once, then the empty set again on close. Three commits,
   * three declarations, none of them against the wrong deck.
   */
  it("declares no terminal on the pane's deck until that deck is the one in force", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith([]);

    fireEvent.click(openControl("Plan / architecture on build-box"));
    await waitFor(() => expect(deck.saveSettings).toHaveBeenCalledTimes(1));

    // The pane is up, the save is out, and the local deck is still in force.
    // Nothing may have been declared shown in that window.
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(deck.setShownTerminals).toHaveBeenCalledTimes(1);

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("remote")} initialView={{ kind: "overview" }} />); });

    expect(deck.setShownTerminals).toHaveBeenCalledTimes(2);
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith(["planner"]);

    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Close Planner on build-box agent" }));
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(3));
    expect(deck.setShownTerminals).toHaveBeenLastCalledWith([]);
  });
});

/**
 * PRD #1105's security audit, BLOCKER 1 — the modal could retarget a
 * deck-origin pane, and its keystrokes, to another deck's namesake.
 *
 * Both halves of the audit's remedy are here, because they close different
 * things. The pane declares `role="dialog"` and `aria-modal="true"` over a base
 * screen that is deliberately still MOUNTED, so every control on it — the rail,
 * the tiles, and the `DeckSelector` that retargets the whole app — kept its
 * place in the tab order behind a full-window overlay. That is the a11y claim
 * being false and the route to the attack in one. And when the selection did
 * move, `DeckShell` kept the old view value while `DeckSurface` matched on a
 * bare `openAgentId`, so the arriving deck's agent of the same per-daemon
 * monotonic id was promoted into the open pane, its session attached, and
 * `sendTerminalInput(agentId, …)` resolved to it — under the same role, the
 * same display text, and no deck identity anywhere in the dialog.
 */
describe("agent pane identity fence", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /** Everything focusable that is neither inside the pane nor under an `inert`. */
  function reachableOutside(pane: HTMLElement): Element[] {
    const candidates = document.querySelectorAll("button, a[href], input, select, textarea, [tabindex]");
    return Array.from(candidates).filter((element) => !pane.contains(element) && !element.closest("[inert]"));
  }

  /**
   * Scenario: open Planner's pane from the deck and try to reach the screen
   * behind it. Every control on the base screen — the deck selector above all —
   * is inert, focus has moved into the pane, and closing gives the screen back
   * exactly as it was.
   */
  it("makes the whole base screen inert while a pane is open, and gives it back on close", () => {
    const deck = harness();
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "deck" }} />);

    // The control that makes this a security fix rather than an a11y one: it is
    // on screen, and pressing it retargets the app to another deck.
    expect(screen.getByTestId("deck-selector-toggle")).toBeVisible();
    expect(screen.getByTestId("deck-selector-toggle").closest("[inert]")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    const pane = screen.getByTestId("agent-pane-overlay");

    expect(screen.getByTestId("deck-selector-toggle").closest("[inert]")).not.toBeNull();
    expect(reachableOutside(pane)).toEqual([]);
    // And the pane itself is genuinely live, so this is containment rather than
    // a screen that has simply been switched off.
    expect(within(pane).getByRole("button", { name: "Close Planner agent" })).toBeVisible();
    expect(pane.contains(document.activeElement)).toBe(true);

    fireEvent.click(within(pane).getByRole("button", { name: "Close Planner agent" }));
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(document.querySelectorAll("[inert]")).toHaveLength(0);
    expect(screen.getByTestId("deck-selector-toggle").closest("[inert]")).toBeNull();
  });

  /**
   * Scenario: a deck-origin pane is open on the local deck when the selected
   * deck moves to build-box, which runs a Planner of its own with the same id.
   * The pane does not adopt it — no pane is on screen at all — and it does not
   * come back when the original deck is selected again, because the view was
   * closed rather than merely hidden.
   *
   * The last step is the one that separates a fence from a curtain: a pane that
   * only stopped RENDERING would resurrect itself on the way back, which is the
   * same retargeting one step later.
   */
  it("closes a deck-origin pane when the selected deck moves out from under it", async () => {
    const deck = harness();
    const paneView = { kind: "agent" as const, deckId: FIXTURE_DAEMON_ID, agentId: "planner", from: "deck" as const };
    const { rerender } = render(<DeckShell runtime={deck.runtime("local")} initialView={paneView} />);

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner" })).toBeVisible();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("remote")} initialView={paneView} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    // build-box's Planner is on screen — it is the selected deck's now — but as
    // a tile among tiles, offering Open rather than wearing the pane the user
    // opened for the other machine's agent.
    expect(screen.getByTestId("agent-tile-planner-on-build-box")).toHaveAttribute("data-presentation", "tile");
    expect(screen.queryByRole("button", { name: /^Close .* agent$/ })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open Planner on build-box agent" })).toBeVisible();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("local")} initialView={paneView} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner")).toHaveAttribute("data-presentation", "tile");
  });

  /**
   * Scenario: hand the deck surface an open-pane identity naming a deck it is
   * not showing, and read back what it promoted. Nothing.
   *
   * This drives `DeckSurface` directly rather than through `DeckShell`, and
   * that is the point of it: the test above proves the view is CLOSED, which is
   * an effect, and effects run after the commit is painted. The commit in
   * between is the one the audit's attack lives in — a bare-id match promotes
   * the arriving deck's namesake into the open pane for that frame, attaches
   * its session and resolves this client's keystrokes to it. So the render seam
   * has to refuse on its own, without waiting to be rescued.
   */
  it("promotes no tile for an open-pane identity naming another deck", () => {
    const deck = harness();
    const settings: DesktopSettingsState = {
      settings: structuredClone(DEFAULT_DESKTOP_SETTINGS),
      loaded: true,
      chosen: false,
      save: () => undefined,
    };

    const wrongDeck = render(
      <DeckSurface
        runtime={deck.runtime("remote")}
        settings={settings}
        openAgent={{ deckId: FIXTURE_DAEMON_ID, agentId: "planner" }}
        onCloseAgent={() => undefined}
      />,
    );

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner-on-build-box")).toHaveAttribute("data-presentation", "tile");
    wrongDeck.unmount();

    // The control: the same surface with the identity it IS showing does
    // promote, so the refusal above is about the deck rather than about the
    // prop being ignored.
    render(
      <DeckSurface
        runtime={deck.runtime("remote")}
        settings={settings}
        openAgent={{ deckId: REMOTE_DECK_ID, agentId: "planner" }}
        onCloseAgent={() => undefined}
      />,
    );
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(screen.getByTestId("agent-tile-planner-on-build-box")).toHaveAttribute("data-presentation", "overlay");
  });
});

/**
 * PRD #1105's security audit, SHOULD-FIX — the other bare-id maps carried
 * wrong-deck state into the pane. `terminalInputResults` and `appliedGeometry`
 * were both read out of the runtime by bare agent id, so a pane opened for one
 * deck's agent answered with whatever had been recorded for another deck's
 * agent of the same id. See `CrossDeckTerminalState.test.tsx` for the runtime
 * half; this is the pane reading it.
 */
describe("cross-deck pane state", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /**
   * Scenario: the local deck's Planner has a `wrong-session` verdict recorded
   * against it and a geometry the daemon applied to it. Open build-box's
   * Planner — same id, another machine — from the overview. Its pane carries
   * neither: no rejection notice, no disabled input, and no inherited grid to
   * submit to that agent's PTY.
   */
  it("reads neither the verdict nor the geometry recorded for the other deck's namesake", async () => {
    const deck = harness(
      documentWithFleet(),
      {
        terminalInputResults: { [agentKey(FIXTURE_DAEMON_ID, "planner")]: "wrong-session" },
        appliedGeometry: { [agentKey(FIXTURE_DAEMON_ID, "planner")]: { rows: 24, cols: 80 } },
      },
      // Writable and running on BOTH decks, so the notice below can only come
      // from the leaked verdict: `terminalInputState` reads the lease first and
      // the fixture's default `read` lease would print the same sentence for a
      // reason that has nothing to do with this fix.
      (agent) => (agent.id === "planner" ? { ...agent, status: "running", writeLease: "write" } : agent),
    );
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.saveSettings).not.toHaveBeenCalled());

    fireEvent.click(openControl("Plan / architecture on build-box"));

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();
    // The verdict's whole user-visible consequence is this notice and the
    // read-only input it goes with. Neither belongs to this agent.
    expect(screen.queryByTestId("terminal-input-status-planner")).not.toBeInTheDocument();
    const mounted = viewportProps.filter((props) => props.agentId === "planner");
    expect(mounted).not.toHaveLength(0);
    expect(mounted.at(-1)).toMatchObject({ deckId: REMOTE_DECK_ID });
    expect(mounted.at(-1)?.applied).toBeUndefined();
  });
});

/**
 * The control for the ordering test above: the same component tree, driven by
 * hand into the state the gate refuses to enter, so the test that proves the
 * gate holds is not merely asserting an arrangement nothing would have broken.
 *
 * This is the cost being avoided, stated as a fact about the fixture rather
 * than as an argument: both decks run an agent called `planner`, so a
 * declaration made while the wrong deck is in force is not a harmless early
 * attach — it names a real, different agent on a real, different machine.
 */
describe("cross-deck agent ids", () => {
  it("has the same agent id on both decks, which is what makes the ordering matter", () => {
    const { local, remote } = decks();

    expect(local.connection.deckId).toBe(FIXTURE_DAEMON_ID);
    expect(remote.connection.deckId).toBe(REMOTE_DECK_ID);
    expect(local.agents.map((agent) => agent.id)).toEqual(remote.agents.map((agent) => agent.id));
  });
});

