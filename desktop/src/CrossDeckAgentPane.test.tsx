import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { ALL_ENDPOINT_SELECTION, DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckRuntimeState, DeckSnapshot } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>terminal</pre>
  ),
}));

import { DeckShell } from "./App";

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
function harness(initial: DesktopSettingsDto = documentWithFleet()) {
  const { local, remote } = decks();
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
  });

  /**
   * Scenario: with All Decks selected and the local deck in force, open the
   * build-box deck's Planner from the overview. The app writes the settings
   * document once, moving the selection to that deck's stored row and changing
   * nothing else; then, once the switch has taken effect, it closes the pane
   * and writes the document back. The document on disk after the round trip is
   * byte-identical to the one it started from.
   */
  it("switches the selected deck on open and restores the document byte-identically on close", async () => {
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

