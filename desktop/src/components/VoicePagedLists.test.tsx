import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureFleet, createFixtureSnapshot } from "../data/fixture";
import { VOICE_PAGES_DIRECTORY_NAMES, voicePagesDirectory, voicePagesOrchestrations } from "../data/fixtureCrowded";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <div data-testid={`terminal-${agentId}`} />,
}));

const HOME = "/home/dev";
const NAMES = Array.from({ length: 32 }, (_, index) => index === 27 ? "docs" : `folder-${String(index + 1).padStart(2, "0")}`);

function microphone() {
  const queue: string[] = [];
  let recording = false;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({
    state, capturedMs: 900, maxMs: 30_000, capped: false, speech: queue.length > 0,
    available: true, backend: "remote",
  });
  return {
    deliver: (text: string) => queue.push(text),
    voiceStart: vi.fn(async () => { recording = true; return status("recording"); }),
    voiceStatus: vi.fn(async () => status(recording && queue.length ? "done" : "recording")),
    voiceStop: vi.fn(async (): Promise<VoiceTranscriptionDto> => {
      recording = false;
      const transcript = queue.shift() ?? "";
      return { outcome: { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` }, transcribeMs: 11, backend: "stub", audioMs: 900 };
    }),
    voiceCancel: vi.fn(async () => { recording = false; return status("idle"); }),
  };
}

function runtime(voice: ReturnType<typeof microphone>, crowded = false) {
  const snapshot = createFixtureSnapshot(crowded ? "crowded" : "docs");
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => {
    if (transcript === "next page" || transcript === "previous page") {
      const next = transcript === "next page";
      return { backend: "stub", resolveMs: 21, outcome: {
        kind: "dispatch", transcript, action: next ? "next_page" : "previous_page",
        invoke: next ? "nextPage" : "previousPage", params: [], sentence: next ? "Next page." : "Previous page.",
      } };
    }
    return {
      backend: "stub", resolveMs: 21,
      outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` },
    };
  });
  return {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures(), snapshot, fleet: [snapshot],
    terminalData: {}, clearError: vi.fn(), runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined), reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    listDirectories: vi.fn(async (_deckId: string, path?: string) => ({
      kind: "listing" as const, path: path ?? HOME, displayPath: path ?? HOME, parent: "/home",
      entries: path ? [] : NAMES.map((name) => ({ path: `${HOME}/${name}`, displayName: name, isProject: false })),
      truncated: false,
    })),
    newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, agents: [], experimental: false, authoringKinds: ["schedule", "schedule-issues", "dispatcher"] })),
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: undefined })),
    saveSettings: vi.fn(async () => structuredClone(DEFAULT_DESKTOP_SETTINGS)),
    setZoom: vi.fn(async (level: number) => level),
    resolveVoice, ...voice,
  } as unknown as DeckRuntimeState;
}

function runtimeWithSharedProject(voice: ReturnType<typeof microphone>, hidden = false) {
  const deck = runtime(voice);
  const fleet = createFixtureFleet("voice-pages").slice(0, 2).map((item) => ({
    ...item, connection: { ...item.connection, listingOptions: hidden },
  }));
  deck.snapshot = fleet[0];
  deck.fleet = fleet;
  deck.listDirectories = vi.fn(async (_deckId: string, path?: string, options?: { includeHidden?: boolean }) => ({
    kind: "listing" as const, path: path ?? HOME, displayPath: path ?? HOME, parent: path && path !== HOME ? HOME : "/home",
    entries: path && path !== HOME ? [] : [
      ...NAMES.map((name) => ({ path: `${HOME}/${name}`, displayName: name, isProject: name === "docs" })),
      ...(options?.includeHidden ? [{ path: `${HOME}/.hidden`, displayName: ".hidden", isProject: false }] : []),
    ],
    truncated: false,
  }));
  deck.newAgentOrchestrations = vi.fn(async (_deckId: string, path: string) => ({
    kind: "project" as const,
    path,
    displayPath: path,
    displayName: "docs",
    orchestrations: voicePagesOrchestrations(path) ?? [],
  }));
  return deck;
}

async function chooseProject(deckIndex: number) {
  fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[deckIndex]);
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
  fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "docs" } });
  fireEvent.click(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { name: /docs/i }));
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
  fireEvent.click(screen.getByTestId("new-agent-use-directory"));
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
  expect(screen.getByTestId("new-agent-dir")).toHaveTextContent(`${HOME}/docs`);
}

function expectSelectedModeVisible() {
  expect(screen.getByTestId("new-agent-mode-page")).toHaveTextContent(/Page 1 of [2-9]\d*/i);
  const modes = screen.getByTestId("new-agent-modes");
  expect(within(modes).getByRole("button", { pressed: true })).toHaveAttribute("data-mode", "none");
}

function expectSelectedDirectoryVisible() {
  expect(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { selected: true })).toBeVisible();
}

async function openBrowser() {
  fireEvent.click(screen.getByTestId("overview-new-agent"));
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
  expect(screen.getByTestId("new-agent-directory-list")).toBeInTheDocument();
}

async function turnOnVoice() {
  await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
  expect(screen.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "true");
}

async function speak(voice: ReturnType<typeof microphone>, words: string) {
  voice.deliver(words);
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

describe("visible pages for voice-selected lists", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => vi.useRealTimers());

  /** Scenario: after Voice turns the Mode row to page two, using the same project directory again resets the form to No mode. The Mode row shows that selected chip on page one. */
  it("shows No mode after reconfirming the same directory on a later mode page", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtimeWithSharedProject(voice)} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await turnOnVoice();
    await chooseProject(0);
    expectSelectedModeVisible();
    await speak(voice, "next page");
    expect(screen.getByTestId("new-agent-mode-page")).toHaveTextContent(/Page 2 of \d+/i);
    fireEvent.click(within(screen.getByTestId("new-agent-modes")).getAllByRole("button")[0]);
    expect(within(screen.getByTestId("new-agent-modes")).getByRole("button", { pressed: true })).toBeVisible();
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expectSelectedModeVisible();
  });

  /** Scenario: after Voice turns the first daemon's Mode row to page two, switching to a second daemon with the same project and mode ids starts a fresh form. Its selected No mode chip is visible on page one. */
  it("shows the second daemon's selected mode after switching decks on a later mode page", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtimeWithSharedProject(voice)} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await turnOnVoice();
    await chooseProject(0);
    expectSelectedModeVisible();
    await speak(voice, "next page");
    expect(screen.getByTestId("new-agent-mode-page")).toHaveTextContent(/Page 2 of \d+/i);
    await chooseProject(1);
    expectSelectedModeVisible();
  });

  /** Scenario: after Voice turns to a later Mode page, toggling Voice off and on again shows the form's selected No mode chip. */
  it("shows the selected mode after voice turns off and back on", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtimeWithSharedProject(voice)} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await turnOnVoice();
    await chooseProject(0);
    await speak(voice, "next page");
    expect(screen.getByTestId("new-agent-mode-page")).toHaveTextContent(/Page 2 of \d+/i);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); await Promise.resolve(); });
    expect(screen.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "false");
    await turnOnVoice();
    expectSelectedModeVisible();
  });

  /** Scenario: changing and clearing a directory filter after a spoken page turn keeps the browser's selected row on the visible page. */
  it("keeps the selected directory visible when the filter changes", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtimeWithSharedProject(voice)} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await turnOnVoice();
    await speak(voice, "next page");
    expect(screen.getByTestId("new-agent-directory-page")).toHaveTextContent(/Page 2 of \d+/i);
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "folder-01" } });
    expectSelectedDirectoryVisible();
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "" } });
    expectSelectedDirectoryVisible();
    expect(screen.getByTestId("new-agent-directory-page")).toHaveTextContent(/Page 1 of \d+/i);
  });

  /** Scenario: Show hidden refreshes a paged directory listing while keeping the highlighted row visible. Entering that row and returning to its parent also keeps the row visible. */
  it("keeps the selected directory visible through Show hidden and parent navigation", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtimeWithSharedProject(voice, true)} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await turnOnVoice();
    await speak(voice, "next page");
    expectSelectedDirectoryVisible();
    const selectedPath = within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { selected: true }).getAttribute("data-path");
    fireEvent.click(screen.getByTestId("new-agent-show-hidden"));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expectSelectedDirectoryVisible();
    expect(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { selected: true })).toHaveAttribute("data-path", selectedPath);
    fireEvent.click(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { selected: true }));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(selectedPath!);
    expectSelectedDirectoryVisible();
    fireEvent.click(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { name: /\.\./ }));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(HOME);
    expectSelectedDirectoryVisible();
    expect(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { selected: true })).toHaveAttribute("data-path", selectedPath);
  });

  /** Scenario: switching daemons while a directory list is paged gives the new daemon a visible selected directory row. If the highlighted daemon then disappears from the fleet, the remaining highlighted row is visible. */
  it("keeps directory and daemon selections visible across deck and fleet changes", async () => {
    const voice = microphone();
    const deck = runtimeWithSharedProject(voice);
    const view = render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    const deckRows = () => within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option");
    fireEvent.click(deckRows()[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await turnOnVoice();
    await speak(voice, "next page");
    expect(screen.getByTestId("new-agent-directory-page")).toHaveTextContent(/Page 2 of \d+/i);
    fireEvent.click(deckRows()[1]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expectSelectedDirectoryVisible();
    expect(screen.getByTestId("new-agent-directory-page")).toHaveTextContent(/Page 1 of \d+/i);
    await speak(voice, "next page");
    expectSelectedDirectoryVisible();
    await act(async () => { view.rerender(<DeckShell runtime={{ ...deck, fleet: [deck.fleet[0]] }} initialView={{ kind: "overview" }} />); });
    expect(deckRows()).toHaveLength(1);
    expect(deckRows()[0]).toHaveAttribute("aria-selected", "true");
    expectSelectedDirectoryVisible();
  });

  /** Scenario: the browser's crowded voice fixture has six usable daemons, one disconnected daemon, a long home directory, overflowing project modes, and fifteen tiles on its selected daemon. */
  it("provides the crowded browser state for every paged list", () => {
    const fleet = createFixtureFleet("voice-pages");
    expect(fleet.filter((deck) => deck.connection.status === "connected")).toHaveLength(6);
    expect(fleet.filter((deck) => deck.connection.status !== "connected")).toHaveLength(1);
    expect(fleet[0].agents).toHaveLength(15);
    expect(VOICE_PAGES_DIRECTORY_NAMES).toHaveLength(30);
    expect(voicePagesDirectory("/home/dev")?.entries).toHaveLength(30);
    expect(voicePagesOrchestrations("/home/dev/docs")).toHaveLength(12);
  });

  /** Scenario: with Voice off, the directory browser keeps every child in its scrolling list. After keyboard navigation to a later row, turning Voice on shows that row's page, and Enter opens the row the user can see. */
  it("keeps the ordinary scrolling directory list and reveals its cursor when voice turns on", async () => {
    render(<DeckShell runtime={runtime(microphone())} initialView={{ kind: "overview" }} />);
    await openBrowser();
    const list = screen.getByTestId("new-agent-directory-list");
    expect(within(list).getAllByRole("option")).toHaveLength(NAMES.length + 1);
    expect(screen.queryByText(/Page \d+ of \d+/i)).toBeNull();
    for (let index = 0; index < 20; index += 1) fireEvent.keyDown(list, { key: "ArrowDown" });
    const selectedPath = `${HOME}/${NAMES[20]}`;
    expect(within(list).getByRole("option", { selected: true })).toHaveAttribute("data-path", selectedPath);

    await turnOnVoice();
    expect(screen.getByTestId("new-agent-directory-page")).toHaveTextContent(/Page 2 of \d+/i);
    expect(within(list).getByRole("option", { selected: true })).toHaveAttribute("data-path", selectedPath);
    fireEvent.keyDown(list, { key: "Enter" });
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(selectedPath);
  });

  /** Scenario: a long directory has one visible page at a time while Voice is on. The marker identifies the page and hidden children have no selectable row. */
  it("shows only the current directory page while voice is on", async () => {
    render(<DeckShell runtime={runtime(microphone())} initialView={{ kind: "overview" }} />);
    await openBrowser();
    await turnOnVoice();
    expect(screen.getByText(/Page 1 of [2-9]\d*/i)).toBeVisible();
    const options = within(screen.getByTestId("new-agent-directory-list")).getAllByRole("option");
    expect(options.length).toBeLessThan(NAMES.length + 1);
    expect(options.some((option) => option.getAttribute("data-path") === `${HOME}/docs`)).toBe(false);
  });

  /** Scenario: a spoken next page exposes new directory choices, then “three” enters the third choice on that page. Page numbering starts again at one for the choices now showing. */
  it("turns a directory page by voice and selects its third visible item", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice)} initialView={{ kind: "overview" }} />);
    await openBrowser();
    await turnOnVoice();
    await speak(voice, "next page");
    expect(screen.getByText(/Page 2 of \d+/i)).toBeVisible();
    const options = within(screen.getByTestId("new-agent-directory-list")).getAllByRole("option");
    expect(options[0]).toHaveAccessibleName(/^1\./);
    expect(options[2]).toHaveAccessibleName(/^3\./);
    const thirdPath = options[2].getAttribute("data-path");
    expect(thirdPath).toBeTruthy();
    await speak(voice, "three");
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(thirdPath!);
  });

  /** Scenario: from the first directory page “previous page” is refused in the voice report, without browsing or changing the page. */
  it("refuses previous page at the first directory page", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice)} initialView={{ kind: "overview" }} />);
    await openBrowser();
    await turnOnVoice();
    await speak(voice, "previous page");
    expect(screen.getByText(/Page 1 of \d+/i)).toBeVisible();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/first page|no previous page/i);
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(HOME);
  });

  /** Scenario: a spoken previous page returns from page two to page one. Repeated next page requests stop at the final page and explain why no further page is available. */
  it("moves backward and refuses next page at the end", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice)} initialView={{ kind: "overview" }} />);
    await openBrowser();
    await turnOnVoice();
    await speak(voice, "next page");
    expect(screen.getByText(/Page 2 of \d+/i)).toBeVisible();
    await speak(voice, "previous page");
    expect(screen.getByText(/Page 1 of \d+/i)).toBeVisible();

    const marker = () => screen.getByText(/Page \d+ of \d+/i).textContent ?? "";
    const pages = Number(marker().match(/of (\d+)/i)?.[1]);
    expect(pages).toBeGreaterThan(1);
    for (let index = 1; index < pages; index += 1) await speak(voice, "next page");
    expect(marker()).toMatch(new RegExp(`Page ${pages} of ${pages}`, "i"));
    await speak(voice, "next page");
    expect(marker()).toMatch(new RegExp(`Page ${pages} of ${pages}`, "i"));
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/last page|no next page/i);
  });

  /** Scenario: a spoken name that exists only on a later directory page is refused with that page number. It does not enter the hidden directory. */
  it("refuses a directory name on another page", async () => {
    const voice = microphone();
    render(<DeckShell runtime={runtime(voice)} initialView={{ kind: "overview" }} />);
    await openBrowser();
    await turnOnVoice();
    await speak(voice, "open dir docs");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/docs.*page \d+/i);
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(HOME);
  });

  /** Scenario: the crowded dashboard shows only agents on its current voice page and names that page. A small dashboard leaves its agents unpaged. */
  it("pages crowded dashboard agents but not a fitting dashboard", async () => {
    const view = render(<DeckShell runtime={runtime(microphone(), true)} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    expect(screen.getByText(/Page 1 of [2-9]\d*/i)).toBeVisible();
    expect(screen.queryAllByTestId("agent-pane-overlay")).toHaveLength(0);
    view.unmount();
    render(<DeckShell runtime={runtime(microphone())} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    expect(screen.queryByText(/Page \d+ of \d+/i)).toBeNull();
  });

  /** Scenario: two daemons without reported ids have no agent rows and fall on opposite sides of a crowded dashboard's page break. Each daemon's section appears only on its own voice page. */
  it("shows each id-less row-less daemon on only its own dashboard page", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    const rowless = createFixtureSnapshot("disconnected");
    const waiting = (name: string) => ({
      ...rowless,
      runId: `run_${name}`,
      connection: { ...rowless.connection, deckKind: "remote" as const, name, socketPath: name },
      agents: [],
      totalNodes: 0,
    });
    deck.fleet = [waiting("Waiting Alpha"), deck.snapshot, waiting("Waiting Beta")];
    const sections = () => screen.getAllByTestId("daemon-group")
      .map((section) => within(section).getByTestId("daemon-identity").textContent);

    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    expect(screen.getByTestId("overview-page")).toHaveTextContent("Page 1 of 2");
    expect(sections()).toContain("Waiting Alpha");
    expect(sections()).not.toContain("Waiting Beta");

    await speak(voice, "next page");
    expect(screen.getByTestId("overview-page")).toHaveTextContent("Page 2 of 2");
    expect(sections()).toContain("Waiting Beta");
    expect(sections()).not.toContain("Waiting Alpha");
  });

  /** Scenario: two daemons have agents with the same id. Naming the one visible on page one opens it, even though its namesake on another daemon is on a later page. */
  it("opens a visible named agent despite an off-page agent with the same id", async () => {
    const voice = microphone();
    const fleet = createFixtureFleet("voice-pages");
    const selected = fleet[0].agents[0];
    expect(fleet.slice(1).some((deck) => deck.agents.some((agent) => agent.id === selected.id))).toBe(true);
    const deck = runtime(voice, true);
    deck.snapshot = fleet[0];
    deck.fleet = fleet;
    deck.resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => ({
      backend: "stub", resolveMs: 21,
      outcome: { kind: "dispatch", transcript, action: "open_agent", invoke: "openAgent", sentence: `Opening ${selected.displayName}.`, params: [
        { name: "agent", kind: "agent_ref", spoken: selected.displayName, value: selected.id, label: selected.displayName },
      ] },
    }));
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    expect(screen.getByText(/Page 1 of [2-9]\d*/i)).toBeVisible();
    expect(screen.getByRole("button", { name: new RegExp(`open ${selected.displayName} agent`, "i") })).toBeVisible();
    await speak(voice, `open ${selected.displayName}`);
    expect(screen.queryByTestId("agent-pane-overlay"), screen.getByTestId("voice-report").textContent ?? "no voice report").not.toBeNull();
    expect(screen.getByTestId(`terminal-${selected.id}`)).toBeVisible();
  });
});
