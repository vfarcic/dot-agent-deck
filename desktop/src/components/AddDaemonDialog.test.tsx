/**
 * PRD #2274 — adding a daemon from where a new user looks: the Daemon
 * selector's **Add a daemon…** entry and the empty-agents note's **Add a
 * daemon** button, both opening the add form Settings → Daemons uses, in a
 * dialog over the screen the user is on.
 *
 * Mounted through the whole shell, because that is where the dialog lives and
 * where its opener comes from: a selector or a note rendered alone has no
 * opener, and offers neither control.
 */
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import "../styles.css";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto, type EndpointSettingsDto } from "../lib/bridge";
import type { DeckRuntimeState, DeckSnapshot, DeckView } from "../types";
import { DeckShell } from "../App";
import { AddDaemonDialog } from "./AddDaemonDialog";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import { SettingsBridgeProvider, type SettingsBridge } from "../lib/settingsBridge";

/** A bridge with no keychain, for the cases that need only the name check. */
const NO_SECRETS: SettingsBridge = {
  testEndpoint: vi.fn(),
  secretStatus: vi.fn(),
  storeSecret: vi.fn(),
  forgetSecret: vi.fn(),
};

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

const BUILD_BOX = "a1b2c3d4e5f60718";

/** One remote daemon configured beside the local one. */
function oneRemote(selection = "local"): EndpointSettingsDto {
  return { remote: [{ host: "build-box.example.com", id: BUILD_BOX, port: 22, user: "vf" }], selection };
}

function settingsWith(endpoints?: EndpointSettingsDto): DesktopSettingsDto {
  return endpoints ? { ...structuredClone(DEFAULT_DESKTOP_SETTINGS), endpoints } : structuredClone(DEFAULT_DESKTOP_SETTINGS);
}

/** A settings store that keeps what it is given, as the real file does. */
function store(initial: DesktopSettingsDto) {
  let current = structuredClone(initial);
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(current) })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      current = structuredClone(next);
      return structuredClone(next);
    }),
  };
}

function runtime(snapshot: DeckSnapshot, settings: DesktopSettingsDto, overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  return {
    mode: "live",
    desktopFeatures: fixtureDesktopFeatures(""),
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
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
      message: "No daemon is reachable from this test runtime.",
      disclosureKnown: false,
      forwards: [],
      knownHosts: [],
      clientProtocolVersion: 0,
      clientBuildVersion: "test",
    })),
    secretStatus: vi.fn(async () => ({ stored: false, problem: "No credential store is reachable from this test runtime." })),
    storeSecret: vi.fn(async () => { throw new Error("No credential store is reachable from this test runtime."); }),
    forgetSecret: vi.fn(async () => { throw new Error("No credential store is reachable from this test runtime."); }),
    ...store(settings),
    ...overrides,
  };
}

/** Mount the whole shell and wait for the settings read to land. */
async function mountShell(deck: DeckRuntimeState, initialView: DeckView = { kind: "overview" }) {
  const rendered = render(<DeckShell runtime={deck} initialView={initialView} />);
  await act(async () => { await Promise.resolve(); });
  return rendered;
}

async function openMenu() {
  fireEvent.click(screen.getByTestId("deck-selector-toggle"));
  return await screen.findByTestId("deck-selector-menu");
}

describe("the Daemon selector's Add a daemon… entry", () => {
  /// Scenario: open the Daemon selector with only this machine configured, then again with a remote daemon added; the menu ends with Add a daemon… both times, after every daemon.
  it("ends the menu whatever is configured", async () => {
    for (const endpoints of [undefined, oneRemote()]) {
      const view = await mountShell(runtime(createFixtureSnapshot("crowded"), settingsWith(endpoints)));
      const menu = await openMenu();
      const buttons = within(menu).getAllByRole("button");
      expect(buttons.at(-1)).toBe(within(menu).getByTestId("deck-selector-add"));
      expect(buttons.at(-1)).toHaveTextContent("Add a daemon…");
      view.unmount();
    }
  });

  /// Scenario: choose Add a daemon… in the Daemon selector. The menu closes and the Add a daemon dialog opens with its Host field focused, and nothing is stored yet.
  it("closes the menu and opens the add form in a dialog", async () => {
    const deck = runtime(createFixtureSnapshot("crowded"), settingsWith());
    await mountShell(deck);
    fireEvent.click(within(await openMenu()).getByTestId("deck-selector-add"));

    expect(screen.queryByTestId("deck-selector-menu")).toBeNull();
    const dialog = screen.getByRole("dialog", { name: "Add a daemon" });
    expect(within(dialog).getByLabelText("Host")).toHaveFocus();
    expect(deck.saveSettings).not.toHaveBeenCalled();
  });

  /// Scenario: open the dialog from the Daemon selector, type a host, then press Cancel; open it again and press Escape. Each time the dialog closes, nothing is stored and the dashboard is the one the user left.
  it("cancels back to the screen the user was on, storing nothing", async () => {
    const deck = runtime(createFixtureSnapshot("crowded"), settingsWith());
    await mountShell(deck);
    fireEvent.click(within(await openMenu()).getByTestId("deck-selector-add"));
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });
    fireEvent.click(screen.getByTestId("add-daemon-cancel"));

    expect(screen.queryByTestId("add-daemon-dialog")).toBeNull();
    expect(screen.getByText("Agent dashboard")).toBeVisible();

    fireEvent.click(within(await openMenu()).getByTestId("deck-selector-add"));
    fireEvent.keyDown(screen.getByLabelText("Host"), { key: "Escape" });
    expect(screen.queryByTestId("add-daemon-dialog")).toBeNull();
    expect(deck.saveSettings).not.toHaveBeenCalled();
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("This machine");
  });

  /// Scenario: open the dialog, type a host and press Add this daemon. The daemon is stored and chosen exactly as Settings → Daemons stores it, the dialog closes, and the Daemon selector now names the new daemon.
  it("stores and chooses the daemon, as Settings does", async () => {
    const deck = runtime(createFixtureSnapshot("crowded"), settingsWith());
    await mountShell(deck);
    fireEvent.click(within(await openMenu()).getByTestId("deck-selector-add"));
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });
    fireEvent.change(screen.getByLabelText("User"), { target: { value: "vf" } });
    fireEvent.click(screen.getByTestId("add-daemon-save"));

    await waitFor(() => expect(deck.saveSettings).toHaveBeenCalled());
    const written = vi.mocked(deck.saveSettings).mock.calls[0][0];
    expect(written.endpoints?.remote).toHaveLength(1);
    expect(written.endpoints?.remote[0]).toMatchObject({ host: "build-box", user: "vf", port: 22 });
    expect(written.endpoints?.selection).toBe(written.endpoints?.remote[0].id);
    await waitFor(() => expect(screen.queryByTestId("add-daemon-dialog")).toBeNull());
    expect(screen.getByTestId("deck-selector-current")).toHaveTextContent("vf@build-box");
  });
});

describe("the empty-agents note", () => {
  /// Scenario: a fresh install with only this machine configured and no agents. The dashboard's empty-agents note reads in plain words and offers Add a daemon beside New agent; pressing it opens the Add a daemon dialog.
  it("offers Add a daemon while this machine is the only daemon", async () => {
    await mountShell(runtime(createFixtureSnapshot("empty"), settingsWith()));
    const note = screen.getByTestId("overview-first-run");

    expect(note).toHaveTextContent("This daemon has no agents yet.");
    expect(note).not.toHaveTextContent(/owns nothing|adopts|snapshot/);
    expect(within(note).getByTestId("overview-first-run-add-daemon-hint")).toHaveTextContent("Running agents on another machine?");

    fireEvent.click(within(note).getByTestId("overview-first-run-add-daemon"));
    expect(screen.getByRole("dialog", { name: "Add a daemon" })).toBeVisible();
  });

  /// Scenario: the settings file cannot be read, so the app does not know which daemons are configured. The empty-agents note offers no Add a daemon rather than assuming this machine is the only one; the Daemon selector still offers Add a daemon….
  it("offers nothing extra when the settings could not be read", async () => {
    await mountShell(runtime(createFixtureSnapshot("empty"), settingsWith(), { getSettings: vi.fn(async () => { throw new Error("unreadable"); }) }));
    const note = screen.getByTestId("overview-first-run");

    expect(within(note).queryByTestId("overview-first-run-add-daemon")).toBeNull();
    expect(within(await openMenu()).getByTestId("deck-selector-add")).toBeVisible();
  });

  /// Scenario: a remote daemon is already configured and this machine has no agents. Its empty-agents note shows no Add a daemon button or hint; the Daemon selector still offers Add a daemon….
  it("offers nothing extra once another daemon is configured", async () => {
    await mountShell(runtime(createFixtureSnapshot("empty"), settingsWith(oneRemote())));
    const note = screen.getByTestId("overview-first-run");

    expect(note).toHaveTextContent("This daemon has no agents yet.");
    expect(within(note).queryByTestId("overview-first-run-add-daemon")).toBeNull();
    expect(within(note).queryByTestId("overview-first-run-add-daemon-hint")).toBeNull();
    expect(within(await openMenu()).getByTestId("deck-selector-add")).toBeVisible();
  });
});

describe("AddDaemonDialog", () => {
  /// Scenario: press Add this daemon and have the save come back with the file on disk not holding the daemon (its name was taken in a terminal meanwhile). The dialog stays open with the draft and the reason under it, rather than closing on a daemon that was not stored.
  it("keeps the form when the file on disk did not take the daemon", async () => {
    const onClose = vi.fn();
    const save = vi.fn(async () => ({ saved: false as const, disk: settingsWith() }));
    const { rerender } = render(<AddDaemonDialog settings={{ settings: settingsWith(), save }} onClose={onClose} />);
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });
    fireEvent.click(screen.getByTestId("add-daemon-save"));
    await waitFor(() => expect(save).toHaveBeenCalled());
    rerender(<AddDaemonDialog settings={{ settings: settingsWith(), save, saveError: "The daemon list changed outside the app; nothing was written." }} onClose={onClose} />);

    await waitFor(() => expect(screen.getByLabelText("Host")).toHaveValue("build-box"));
    expect(screen.getByTestId("add-daemon-error")).toHaveTextContent("nothing was written");
    expect(onClose).not.toHaveBeenCalled();
  });

  /// Scenario: press Add this daemon and have the write fail with the change still applied, as a Settings save that fails is. The dialog closes: the daemon is added for this session and the next save writes it, exactly as when it is added from Settings.
  it("closes when the save is applied but not yet written, as Settings does", async () => {
    const onClose = vi.fn();
    const save = vi.fn(async () => ({ saved: false as const }));
    render(<AddDaemonDialog settings={{ settings: settingsWith(), save }} onClose={onClose} />);
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });
    fireEvent.click(screen.getByTestId("add-daemon-save"));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(save).toHaveBeenCalledTimes(1);
  });

  /// Scenario: type a host and a name, press Add this daemon, and have the dialog close while the name is still being checked. When the check then comes back clean, nothing is stored.
  it("stores nothing when the dialog closes while the name is being checked", async () => {
    let answer: (refusal: string | null) => void = () => undefined;
    const checkDeckName = vi.fn(() => new Promise<string | null>((resolve) => { answer = resolve; }));
    const save = vi.fn(async () => ({ saved: true as const }));
    const view = render(
      <SettingsBridgeProvider value={{ ...NO_SECRETS, checkDeckName }}>
        <AddDaemonDialog settings={{ settings: settingsWith(), save }} onClose={vi.fn()} />
      </SettingsBridgeProvider>,
    );
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });
    fireEvent.change(screen.getByLabelText("Deck name"), { target: { value: "build" } });
    await act(async () => { answer(null); await Promise.resolve(); });
    fireEvent.click(screen.getByTestId("add-daemon-save"));
    expect(checkDeckName).toHaveBeenCalledTimes(2);
    // Gone by a route that is not this dialog's own Cancel — the screen
    // under it switched, say — while the final name check is still out.
    view.unmount();
    await act(async () => { answer(null); await Promise.resolve(); });

    expect(save).not.toHaveBeenCalled();
  });

  /// Scenario: the Add this daemon button stays disabled until a host is typed, so an empty daemon can never be stored.
  it("cannot add a daemon with no host", () => {
    render(<AddDaemonDialog settings={{ settings: settingsWith(), save: vi.fn() }} onClose={vi.fn()} />);
    expect(screen.getByTestId("add-daemon-save")).toBeDisabled();
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });
    expect(screen.getByTestId("add-daemon-save")).toBeEnabled();
  });
});

describe("VOICE_ACTIONS.addDaemon", () => {
  /// Scenario: the registry entry the two buttons and voice dispatch opens the dialog through the shell's opener and nothing else.
  it("opens the dialog through the shell's opener", () => {
    const openAddDaemon = vi.fn();
    VOICE_ACTIONS.addDaemon.run({ openAddDaemon });
    expect(openAddDaemon).toHaveBeenCalledTimes(1);
  });
});
