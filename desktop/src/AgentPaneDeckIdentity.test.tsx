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
const TERMINAL_INPUT_SENTINEL = "typed-only-into-build-box-planner";
const viewportProps: { agentId: string; deckId?: string; applied?: { rows: number; cols: number } }[] = [];
vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, deckId, label, applied, onInput }: { agentId: string; deckId?: string; label: string; applied?: { rows: number; cols: number }; onInput: (data: string) => void }) => {
    viewportProps.push({ agentId, deckId, applied });
    return (
      <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>
        terminal
        <button aria-label={`Type into ${label} terminal`} onClick={() => onInput(TERMINAL_INPUT_SENTINEL)}>type</button>
      </pre>
    );
  },
}));

import { DeckShell, DeckSurface } from "./App";
import { agentKey } from "./lib/agentKey";
import type { DesktopSettingsState } from "./hooks/useDesktopSettings";

/**
 * The second deck, described exactly as the crate describes a remote one —
 * `Endpoint::describe()` renders `user@host[:port]` and never a socket path.
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
  const sendTerminalInput = vi.fn(async () => undefined);
  const base = {
    mode: "live",
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput,
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
    sendTerminalInput,
    saveSettings,
    /** Whatever the last save left on disk. */
    onDisk: () => structuredClone(stored),
    savedDocuments: () => saveSettings.mock.calls.map(([document]) => document),
  };
}

/** The overview's open control for one agent, named as the row renders it. */
const openControl = (name: string) => screen.getByRole("button", { name: `Open ${name} agent` });

/**
 * PRD #1105 — the pane names a deck as well as an agent, and everything it
 * reads is addressed by that pair.
 *
 * # What this file is, and what it used to be
 *
 * It was `CrossDeckAgentPane.test.tsx`, and it covered opening a NON-SELECTED
 * deck's agent by switching the selected deck first and reverting on close.
 * **The SWITCH was descoped** after two security audits: it left state created
 * under deck A attributed to deck B, because identity was read from the current
 * selection at use time rather than captured at creation. Cross-deck attach
 * itself is [#1073](https://github.com/vfarcic/dot-agent-deck/issues/1073), a
 * wire change by construction.
 *
 * **Opening was not descoped, and an intermediate commit that also removed it
 * went too far.** Every agent the overview lists is openable, whichever deck it
 * is on: a listed row with no way into it is a dead row. The pane carries the
 * creating deck all the way through attach, output, input and resize, so a
 * non-selected deck's agent opens with the same live terminal as a selected
 * one without switching the process-global selection.
 *
 * So this file covers the full cross-deck pane, its composite lookup, and the
 * identity fence that refuses to retarget a deck-origin pane. Selection is no
 * longer terminal ownership: moving it must not tear down, relabel or retarget
 * a pane whose stream was created for another deck.
 */
describe("agent pane deck identity", () => {
  beforeEach(() => {
    window.localStorage.clear();
    viewportProps.length = 0;
  });

  /**
   * Scenario: open an agent's pane from the overview and close it again. The
   * settings document is never written — opening a pane is navigation, and a
   * navigation that rewrites `desktop.toml` is what the descope removed.
   *
   * This is the guard on the descope rather than a restatement of it. The
   * withdrawn implementation wrote the selection on open and wrote it back on
   * close, through `desktop_set_settings` — which persists to disk before it
   * applies, so a reader who hand-formatted or commented that file lost both.
   * Nothing on this path may write it again without turning this red.
   */
  it("writes no settings document when a pane opens or closes", async () => {
    const deck = harness();
    const before = JSON.stringify(deck.onDisk());
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.saveSettings).not.toHaveBeenCalled());

    fireEvent.click(openControl("Plan / architecture"));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Close Planner agent" }));

    await waitFor(() => expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument());
    expect(deck.saveSettings).not.toHaveBeenCalled();
    expect(JSON.stringify(deck.onDisk())).toBe(before);
  });

  /**
   * Scenario: an overview-origin pane is open on the local deck's Planner when
   * the selected deck moves to build-box, which runs a same-id Planner. The
   * original pane, terminal DOM node and local-deck declaration stay in place;
   * neither output nor input authority follows the mutable selection.
   */
  it("keeps an overview pane and its live terminal on their creating deck when selection moves", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));

    fireEvent.click(openControl("Plan / architecture"));
    const beforePane = screen.getByTestId("agent-pane-overlay");
    const beforeTerminal = within(beforePane).getByTestId("terminal-planner");
    expect(within(beforePane).getByRole("heading", { name: "Planner" })).toBeVisible();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("remote")} initialView={{ kind: "overview" }} />); });

    const afterPane = screen.getByTestId("agent-pane-overlay");
    expect(afterPane).toBe(beforePane);
    expect(within(afterPane).getByTestId("terminal-planner")).toBe(beforeTerminal);
    expect(within(afterPane).getByRole("heading", { name: "Planner" })).toBeVisible();
    expect(within(afterPane).queryByRole("heading", { name: "Planner on build-box" })).toBeNull();
    expect(within(afterPane).queryByTestId("terminal-absent-planner")).toBeNull();
    expect(viewportProps.at(-1)).toMatchObject({ agentId: "planner", deckId: FIXTURE_DAEMON_ID });
    const declaration = JSON.stringify(deck.setShownTerminals.mock.calls.at(-1));
    expect(declaration).toContain(FIXTURE_DAEMON_ID);
    expect(declaration).not.toContain(REMOTE_DECK_ID);
  });

  /**
   * Scenario: with the local deck selected, open build-box's same-id Planner
   * from All Decks. A real terminal mounts immediately, the declaration names
   * build-box, and a keystroke is sent with build-box's identity rather than to
   * the selected local Planner.
   */
  it("opens a non-selected deck's agent with a live terminal and routes its input to that deck", async () => {
    const deck = harness(
      documentWithFleet(),
      {
        terminalInputResults: {
          planner: "wrong-session",
          [agentKey(FIXTURE_DAEMON_ID, "planner")]: "wrong-session",
        },
        appliedGeometry: {
          planner: { rows: 24, cols: 80 },
          [agentKey(FIXTURE_DAEMON_ID, "planner")]: { rows: 24, cols: 80 },
          [agentKey(REMOTE_DECK_ID, "planner")]: { rows: 48, cols: 160 },
        },
      },
      (agent) => (agent.id === "planner" ? { ...agent, status: "running", writeLease: "write" } : agent),
    );
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));

    fireEvent.click(openControl("Plan / architecture on build-box"));

    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();
    expect(within(pane).queryByTestId("terminal-absent-planner")).toBeNull();
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(pane.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "attached");
    expect(viewportProps.at(-1)).toMatchObject({
      agentId: "planner",
      deckId: REMOTE_DECK_ID,
      applied: { rows: 48, cols: 160 },
    });
    expect(screen.queryByTestId("terminal-input-status-planner")).not.toBeInTheDocument();

    const declaration = JSON.stringify(deck.setShownTerminals.mock.calls.at(-1));
    expect(declaration).toContain(REMOTE_DECK_ID);
    expect(declaration).toContain("planner");
    expect(declaration).not.toContain(FIXTURE_DAEMON_ID);

    fireEvent.click(within(pane).getByRole("button", { name: "Type into Planner on build-box terminal" }));
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(1);
    const routedInput = JSON.stringify(deck.sendTerminalInput.mock.calls[0]);
    expect(routedInput).toContain(REMOTE_DECK_ID);
    expect(routedInput).toContain("planner");
    expect(routedInput).toContain(TERMINAL_INPUT_SENTINEL);
    expect(routedInput).not.toContain(FIXTURE_DAEMON_ID);
  });

  /**
   * Scenario: open build-box's Planner while the local deck is selected, move
   * selection to build-box and back, and keep using the pane. The same terminal
   * node stays live throughout and every declaration continues to name the
   * pane's deck rather than whichever deck became selected.
   */
  it("does not detach or retarget a cross-deck pane when the selected deck moves", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(deck.setShownTerminals).toHaveBeenCalledTimes(1));

    fireEvent.click(openControl("Plan / architecture on build-box"));
    const pane = screen.getByTestId("agent-pane-overlay");
    const terminal = within(pane).getByTestId("terminal-planner");

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("remote")} initialView={{ kind: "overview" }} />); });
    expect(screen.getByTestId("agent-pane-overlay")).toBe(pane);
    expect(within(pane).getByTestId("terminal-planner")).toBe(terminal);

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "overview" }} />); });
    expect(screen.getByTestId("agent-pane-overlay")).toBe(pane);
    expect(within(pane).getByTestId("terminal-planner")).toBe(terminal);
    expect(within(pane).getByRole("heading", { name: "Planner on build-box" })).toBeVisible();
    expect(within(pane).queryByTestId("terminal-absent-planner")).toBeNull();
    for (const call of deck.setShownTerminals.mock.calls.slice(1)) {
      const declaration = JSON.stringify(call);
      expect(declaration).toContain(REMOTE_DECK_ID);
      expect(declaration).not.toContain(FIXTURE_DAEMON_ID);
    }
  });

  /**
   * Scenario: the local deck's Planner has a `wrong-session` verdict recorded
   * against it and a geometry the daemon applied to it — filed both under the
   * bare agent id and under the local deck's composite key. Open build-box's
   * Planner, on build-box, which is the same id on another machine. Its pane
   * carries neither: no rejection notice, no disabled input, and no inherited
   * grid to submit to that agent's PTY.
   *
   * The bare-keyed decoy is what makes this discriminate. Both maps are keyed
   * by `agentKey(deckId, agentId)` in production, so a fixture holding only
   * composite keys would pass against a bare-id lookup too — it would simply
   * find nothing. A bare `"planner"` entry is the state a reverted production
   * would read.
   */
  it("reads neither the verdict nor the geometry recorded for another deck's namesake", async () => {
    const deck = harness(
      documentWithFleet(),
      {
        terminalInputResults: {
          planner: "wrong-session",
          [agentKey(FIXTURE_DAEMON_ID, "planner")]: "wrong-session",
        },
        appliedGeometry: {
          planner: { rows: 24, cols: 80 },
          [agentKey(FIXTURE_DAEMON_ID, "planner")]: { rows: 24, cols: 80 },
        },
      },
      // Writable and running on BOTH decks, so the notice below can only come
      // from the leaked verdict: `terminalInputState` reads the lease first and
      // the fixture's default `read` lease would print the same sentence for a
      // reason that has nothing to do with this fix.
      (agent) => (agent.id === "planner" ? { ...agent, status: "running", writeLease: "write" } : agent),
    );
    render(<DeckShell runtime={deck.runtime("remote")} initialView={{ kind: "overview" }} />);
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
 *
 * Native cross-deck attach does not retire either half. The selection still
 * moves for reasons this window did not cause, and `aria-modal` is a claim that
 * has to be true whatever the deck story.
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
   * is inert, while the peer Voice surface remains reachable; focus has moved
   * into the pane, and closing gives the screen back exactly as it was.
  */
  it("makes the base screen inert except for Voice while a pane is open, and gives it back on close", () => {
    const deck = harness(documentWithFleet(), {
      resolveVoice: vi.fn(async (utterance: string) => ({
        // `as const` and nothing else: without it `kind` widens to `string`,
        // which is not a `VoiceOutcomeDto` discriminant, and `tsc --noEmit`
        // refuses the whole runtime. It changes nothing this test asserts.
        outcome: { kind: "no_match" as const, transcript: utterance, sentence: "No matching action." },
        resolveMs: null,
        backend: "stub",
      })),
    });
    render(<DeckShell runtime={deck.runtime("local")} initialView={{ kind: "deck" }} />);

    // The control that makes this a security fix rather than an a11y one: it is
    // on screen, and pressing it retargets the app to another deck.
    expect(screen.getByTestId("deck-selector-toggle")).toBeVisible();
    expect(screen.getByTestId("deck-selector-toggle").closest("[inert]")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    const pane = screen.getByTestId("agent-pane-overlay");

    expect(screen.getByTestId("deck-selector-toggle").closest("[inert]")).not.toBeNull();
    // Voice is a peer dialog, not background. Equality to this one-element set
    // keeps the containment assertion strict: any other reachable control is a
    // regression, rather than something an allow-list filter could hide.
    expect(reachableOutside(pane)).toEqual([screen.getByTestId("voice-trigger")]);
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
   *
   * A deck-origin pane CLOSES where the overview-origin pane above keeps its
   * identity, and the difference is the screen underneath. The deck surface
   * renders the selected deck and nothing else, so a deck-origin pane's claim —
   * *"this agent, on the deck you are looking at"* — stops being true of
   * anything on screen; there is nothing left for it to be a pane over.
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
 * The control for every test above, stated as a fact about the fixture rather
 * than as an argument: both decks run an agent called `planner`, so "the wrong
 * deck's namesake" names a real, different agent on a real, different machine
 * rather than a hypothetical one. A fixture that avoided the collision would
 * make each of those tests pass for the wrong reason.
 */
describe("colliding agent ids across decks", () => {
  it("has the same agent id on both decks, which is what makes the identity fence matter", () => {
    const { local, remote } = decks();

    expect(local.connection.deckId).toBe(FIXTURE_DAEMON_ID);
    expect(remote.connection.deckId).toBe(REMOTE_DECK_ID);
    expect(local.agents.map((agent) => agent.id)).toEqual(remote.agents.map((agent) => agent.id));
  });
});
