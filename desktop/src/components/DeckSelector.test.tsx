/**
 * The Deck selector (PRD #741 M9).
 *
 * Driven through `DeckShell` wherever the assertion is about a *choice* rather
 * than a rendering, because the whole of "switching re-renders the fleet" is
 * that the choice is written to the settings document and nothing else: the
 * re-render is `apply_selection`'s, Rust-side, and a test that stubbed a
 * `switchDeck` bridge call would be asserting a path that does not exist.
 */
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import "../styles.css";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto, type EndpointSettingsDto } from "../lib/bridge";
import type { DeckRuntimeState, DeckSnapshot } from "../types";
import { DeckShell } from "../App";
import { deckStateNote } from "./DeckSelector";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

const BUILD_BOX = "a1b2c3d4e5f60718";
const RELAY = "0f1e2d3c4b5a6978";

/** Two configured decks, so "lists the configured decks" has something to list. */
function twoDecks(selection: string): EndpointSettingsDto {
  return {
    remote: [
      { host: "build-box.example.com", id: BUILD_BOX, port: 22, user: "vf", socket: "/run/deck.sock" },
      { host: "relay.example.com", id: RELAY, port: 2222 },
    ],
    selection,
  };
}

function settingsWith(endpoints?: EndpointSettingsDto): DesktopSettingsDto {
  return endpoints ? { ...structuredClone(DEFAULT_DESKTOP_SETTINGS), endpoints } : structuredClone(DEFAULT_DESKTOP_SETTINGS);
}

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  return {
    mode: "live",
    snapshot: createFixtureSnapshot("crowded"),
    terminalData: {},
    runAction: vi.fn(async () => ({ ok: true }) as import("../types").DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no project here"); }),
    setZoom: vi.fn(async (level: number) => level),
    testEndpoint: vi.fn(async (_settings, selection: string) => ({
      endpointId: selection,
      deck: selection,
      state: "ssh_unavailable" as const,
      ok: false,
      message: "No deck is reachable from this test runtime.",
      disclosureKnown: false,
      forwards: [],
      knownHosts: [],
      clientProtocolVersion: 0,
      clientBuildVersion: "test",
    })),
    getSettings: vi.fn(async () => ({ settings: settingsWith() })),
    saveSettings: vi.fn(async (settings: DesktopSettingsDto) => structuredClone(settings)),
    ...overrides,
  };
}

/** A snapshot whose connection is exactly the one a case is about. */
function withConnection(overrides: Partial<DeckSnapshot["connection"]>): DeckSnapshot {
  const snapshot = createFixtureSnapshot("crowded");
  return { ...snapshot, connection: { ...snapshot.connection, ...overrides } };
}

/** Mount the whole shell and wait for the settings read to land. */
async function mountShell(overrides: Partial<DeckRuntimeState> = {}, initialView: import("../types").DeckView = { kind: "deck" }) {
  const deck = runtime(overrides);
  const rendered = render(<DeckShell runtime={deck} initialView={initialView} />);
  await act(async () => { await Promise.resolve(); });
  return { deck, rendered };
}

async function openMenu() {
  fireEvent.click(screen.getByTestId("deck-selector-toggle"));
  return await screen.findByTestId("deck-selector-menu");
}

describe("DeckSelector", () => {
  it("defaults to the local deck when nothing is configured", async () => {
    await mountShell();
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("This machine");
    const menu = await openMenu();
    // The one entry, and it needs no configuration to exist — `Endpoint::local()`
    // resolves it from the platform paths.
    expect(within(menu).getAllByRole("radio")).toHaveLength(1);
    expect(within(menu).getByTestId("deck-selector-option-local")).toHaveAttribute("aria-checked", "true");
  });

  it("lists every configured deck, local first, named by its address", async () => {
    await mountShell({ getSettings: vi.fn(async () => ({ settings: settingsWith(twoDecks("local")) })) });
    const menu = await openMenu();

    const options = within(menu).getAllByRole("radio").map((option) => option.textContent);
    expect(options).toEqual(["This machine", "vf@build-box.example.com", "relay.example.com:2222"]);
    // No display name is stored, so every label is derived from the address the
    // same way `RemoteEndpoint::describe()` derives it — including the port,
    // which is shown only when it is not 22.
    expect(within(menu).getByTestId(`deck-selector-option-${BUILD_BOX}`)).toBeInTheDocument();
  });

  it("names the stored selection on the trigger, not merely inside the menu", async () => {
    await mountShell({ getSettings: vi.fn(async () => ({ settings: settingsWith(twoDecks(BUILD_BOX)) })) });
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("vf@build-box.example.com");
  });

  it("switching writes the new selection through the settings document", async () => {
    const { deck } = await mountShell({ getSettings: vi.fn(async () => ({ settings: settingsWith(twoDecks("local")) })) });
    const menu = await openMenu();

    fireEvent.click(within(menu).getByTestId(`deck-selector-option-${BUILD_BOX}`));

    // The one route a selection change takes. `desktop_set_settings` →
    // `apply_selection` is what drops the links, releases the other tunnels,
    // restarts the watcher's subscription and emits the new deck's snapshot —
    // so a second route from this control would be a second half of that.
    await waitFor(() => expect(deck.saveSettings).toHaveBeenCalled());
    const written = vi.mocked(deck.saveSettings).mock.calls[0][0];
    expect(written.endpoints?.selection).toBe(BUILD_BOX);
    // The rows travel unchanged: this control chooses, it does not edit.
    expect(written.endpoints?.remote).toHaveLength(2);
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("vf@build-box.example.com");
  });

  it("choosing the deck already selected writes nothing", async () => {
    const { deck } = await mountShell({ getSettings: vi.fn(async () => ({ settings: settingsWith() })) });
    const menu = await openMenu();

    fireEvent.click(within(menu).getByTestId("deck-selector-option-local"));

    await act(async () => { await Promise.resolve(); });
    // Not merely tidy. With no `[endpoints]` section stored, a save here would
    // write `{ remote: [], selection: "local" }` over the file — the exact
    // fabrication `normalizeEndpointSettings` preserves absence to avoid, and
    // the one that deletes a user's decks on a click that changed nothing.
    expect(deck.saveSettings).not.toHaveBeenCalled();
  });

  it("closes on a second press rather than reopening", async () => {
    await mountShell();
    const toggle = screen.getByTestId("deck-selector-toggle");

    fireEvent.click(toggle);
    expect(screen.getByTestId("deck-selector-menu")).toBeInTheDocument();
    // The dismiss listener ignores anything inside the picker's root, and the
    // TRIGGER is inside it — otherwise its pointer-down would close the menu and
    // its click would toggle it straight back open.
    fireEvent.pointerDown(toggle);
    fireEvent.click(toggle);
    expect(screen.queryByTestId("deck-selector-menu")).not.toBeInTheDocument();
  });

  it("an unreachable deck shows its state and the screen keeps its content", async () => {
    await mountShell({
      getSettings: vi.fn(async () => ({ settings: settingsWith(twoDecks(BUILD_BOX)) })),
      snapshot: withConnection({ status: "disconnected", message: "ssh could not reach build-box.example.com." }),
    });

    expect(screen.getByTestId("deck-selector-state")).toHaveTextContent("ssh could not reach build-box.example.com.");
    // Blanking is the failure this is about: the deck is still named, the
    // selector still works, and the rest of the shell is still on screen.
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("vf@build-box.example.com");
    const menu = await openMenu();
    expect(within(menu).getAllByRole("radio")).toHaveLength(3);
  });

  it("a substitution that leaves the app CONNECTED is still reported", async () => {
    await mountShell({
      getSettings: vi.fn(async () => ({ settings: settingsWith(twoDecks(RELAY)) })),
      snapshot: withConnection({
        status: "connected",
        message: "Connected.",
        selectionFallback: "The deck you selected has no socket path yet, so this is the deck on this machine.",
      }),
    });

    // The M7 gap, pinned. `SelectionFallback::NoRemoteSocket` leaves `status`
    // at "connected", so anything keyed on the status alone finds nothing wrong
    // — and the user acts on the wrong machine's agents without being told.
    expect(screen.getByTestId("deck-selector-state")).toHaveTextContent("has no socket path yet");
  });

  it("is the same control on the overview", async () => {
    await mountShell(
      { getSettings: vi.fn(async () => ({ settings: settingsWith(twoDecks(BUILD_BOX)) })) },
      { kind: "overview" },
    );

    expect(screen.getByTestId("open-deck")).toBeInTheDocument();
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("vf@build-box.example.com");
    const menu = await openMenu();
    expect(within(menu).getAllByRole("radio")).toHaveLength(3);
  });

  it("groups its options with a span, never a legend (issue 1032)", async () => {
    await mountShell();
    const menu = await openMenu();

    const group = within(menu).getByRole("radiogroup");
    expect(group).toHaveAccessibleName("Deck");
    // WebKit forces a rendered legend's `float` to `none`, so the fieldset form
    // collapses on the engine the app actually ships on. This is new surface, so
    // it is written in the form that holds in both from the start.
    expect(menu.querySelector("legend")).toBeNull();
    expect(menu.querySelector("fieldset")).toBeNull();
  });
});

describe("deckStateNote", () => {
  const base = { status: "connected" as const, message: "Connected." };

  it("prefers the substitution over every other reason", () => {
    // The ORDER is the behaviour. A fallback can arrive on a connection that is
    // otherwise perfectly healthy, so it has to be read first.
    expect(deckStateNote({ ...base, selectionFallback: "gone", buildStampMismatchOnly: true })).toBe("gone");
  });

  it("says nothing about a healthy connection", () => {
    expect(deckStateNote(base)).toBeUndefined();
  });

  it("keeps a build-stamp caveat on screen while connected", () => {
    // PRD #741 M8: for a remote deck the stamp never refuses, so "connected with
    // something still to say" is the ordinary case rather than the exotic one.
    expect(deckStateNote({ ...base, buildStampMismatchOnly: true, message: "build mismatch: …" })).toBe("build mismatch: …");
  });

  it("reports a failure's own message", () => {
    expect(deckStateNote({ status: "error", message: "handshake refused" })).toBe("handshake refused");
    expect(deckStateNote({ status: "loading" })).toBe("Connecting to this deck…");
  });
});
