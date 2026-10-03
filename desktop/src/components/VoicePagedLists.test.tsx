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

/** Issue #1492 — the four scroll rows, as Rust dispatches them. */
const SCROLLS: Record<string, { action: string; invoke: string; report: string }> = {
  "scroll down": { action: "scroll_down", invoke: "scrollDown", report: "Scrolling down." },
  "scroll up": { action: "scroll_up", invoke: "scrollUp", report: "Scrolling up." },
  "scroll to the top": { action: "scroll_to_top", invoke: "scrollToTop", report: "Scrolled to the top." },
  "scroll to the bottom": { action: "scroll_to_bottom", invoke: "scrollToBottom", report: "Scrolled to the bottom." },
};

/**
 * jsdom lays nothing out and does not scroll, so the dashboard's scroll region
 * (`.overview-body`) is given content `height` tall in a 640px box, and
 * `scrollBy` / `scrollTo` — which jsdom does not define on elements — move its
 * `scrollTop` within that the way a browser would. Removed again by
 * {@link restoreScrolling}.
 */
function scrollableDashboard(height: number) {
  const box = 640;
  let y = 0;
  const isRegion = (element: Element) => element.classList.contains("overview-body");
  const clamp = (top: number) => { y = Math.min(Math.max(0, height - box), Math.max(0, top)); };
  vi.spyOn(Element.prototype, "scrollHeight", "get").mockImplementation(function (this: Element) { return isRegion(this) ? height : 0; });
  vi.spyOn(Element.prototype, "clientHeight", "get").mockImplementation(function (this: Element) { return isRegion(this) ? box : 0; });
  vi.spyOn(Element.prototype, "scrollTop", "get").mockImplementation(function (this: Element) { return isRegion(this) ? y : 0; });
  const scrollBy = vi.fn(function (this: Element, options: ScrollToOptions) { if (isRegion(this)) clamp(y + (options.top ?? 0)); });
  const scrollTo = vi.fn(function (this: Element, options: ScrollToOptions) { if (isRegion(this)) clamp(options.top ?? 0); });
  Object.assign(Element.prototype, { scrollBy, scrollTo });
  return { at: () => y, scrollBy, scrollTo };
}

function restoreScrolling() {
  delete (Element.prototype as Partial<Element>).scrollBy;
  delete (Element.prototype as Partial<Element>).scrollTo;
}

/** Voice-pages daemons, a daemon running a second orchestration (`orc-release`), and a daemon with no agents. */
function tallFleet() {
  const [, release] = createFixtureFleet("fleet");
  const idle = { ...release, runId: "run_idle", connection: { ...release.connection, deckId: "dev@idle", socketPath: "dev@idle", name: "Idle box" }, agents: [], totalNodes: 0 };
  return { fleet: [...createFixtureFleet("voice-pages"), release, idle], release };
}

function runtime(voice: ReturnType<typeof microphone>, crowded = false) {
  const snapshot = createFixtureSnapshot(crowded ? "crowded" : "docs");
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => {
    const scroll = SCROLLS[transcript];
    if (scroll) {
      return { backend: "stub", resolveMs: 21, outcome: {
        kind: "dispatch", transcript, action: scroll.action, invoke: scroll.invoke, params: [], sentence: scroll.report,
      } };
    }
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

});

describe("the agent dashboard scrolls while voice is on", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); restoreScrolling(); });

  /** Scenario: with All daemons and voice on, a fleet taller than the window — the voice-pages daemons, a daemon running a second orchestration and a daemon with no agents — shows every daemon section, every orchestration card and every agent row, exactly as with voice off. */
  it("shows every daemon section and orchestration card of a tall fleet while voice is on", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    const { fleet, release } = tallFleet();
    deck.snapshot = fleet[0];
    deck.fleet = fleet;
    const connected = fleet.filter((item) => item.connection.status === "connected");
    const orchestrations = new Set(connected.flatMap((item) => item.agents.flatMap((agent) => agent.tab.kind === "orchestration" ? [`${item.connection.deckId}/${agent.tab.orchestrationId}`] : [])));
    const rows = connected.reduce((total, item) => total + item.agents.length, 0);
    expect(orchestrations.has(`${release.connection.deckId}/orc-release`)).toBe(true);

    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    const shown = () => ({
      sections: screen.getAllByTestId("daemon-group").length,
      orchestrations: document.querySelectorAll("[data-group-kind='orchestration']").length,
      rows: document.querySelectorAll(".overview-row").length,
    });
    const expected = { sections: fleet.length, orchestrations: orchestrations.size, rows };
    expect(shown()).toEqual(expected);
    await turnOnVoice();
    expect(shown()).toEqual(expected);
    expect(document.querySelector("[data-group-id='orc-release']")).not.toBeNull();
    expect(screen.queryByText(/Page \d+ of \d+/i)).toBeNull();
  });

  /** Scenario: with voice on, the dashboard's agent rows carry one continuous sequence of numbers across every daemon, 1 to the last row, with no number repeated. */
  it("numbers every dashboard row once, continuously across daemons", async () => {
    const deck = runtime(microphone(), true);
    deck.fleet = tallFleet().fleet;
    deck.snapshot = deck.fleet[0];
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    const numbers = Array.from(document.querySelectorAll(".overview-row")).map((row) => Number(row.getAttribute("data-voice-number")));
    expect(numbers.length).toBeGreaterThan(20);
    expect(numbers).toEqual(numbers.map((_, at) => at + 1));
  });

  /** Scenario: with voice on and a fleet taller than the window, “scroll down”, “scroll up”, “scroll to the bottom” and “scroll to the top” move the dashboard by about a window or to either end. A scroll past an end moves nothing and says the dashboard is already there. */
  it("scrolls down, up and to either end by voice", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    deck.fleet = tallFleet().fleet;
    deck.snapshot = deck.fleet[0];
    const page = scrollableDashboard(3000);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();

    await speak(voice, "scroll up");
    expect(page.at()).toBe(0);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/already at the top/i);

    await speak(voice, "scroll down");
    const screenful = page.at();
    expect(screenful).toBeGreaterThan(300);
    expect(screenful).toBeLessThanOrEqual(640);
    await speak(voice, "scroll down");
    expect(page.at()).toBe(2 * screenful);
    await speak(voice, "scroll up");
    expect(page.at()).toBe(screenful);

    await speak(voice, "scroll to the bottom");
    expect(page.at()).toBe(2360);
    await speak(voice, "scroll down");
    expect(page.at()).toBe(2360);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/already at the bottom/i);

    await speak(voice, "scroll to the top");
    expect(page.at()).toBe(0);
  });

  /** Scenario: on a dashboard that fits the window, “scroll down” moves nothing and says the whole dashboard is already on screen. */
  it("says there is nothing to scroll on a dashboard that fits", async () => {
    const voice = microphone();
    const page = scrollableDashboard(600);
    render(<DeckShell runtime={runtime(voice)} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    await speak(voice, "scroll down");
    expect(page.scrollBy).not.toHaveBeenCalled();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/whole dashboard is already on screen/i);
  });

  /** Scenario: on the dashboard, “next page” and “previous page” scroll it down and back up instead of turning pages, so the old phrasing keeps working. */
  it("scrolls the dashboard on next page and previous page", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    deck.fleet = tallFleet().fleet;
    deck.snapshot = deck.fleet[0];
    const page = scrollableDashboard(3000);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    await speak(voice, "next page");
    expect(page.at()).toBeGreaterThan(300);
    expect(screen.queryByText(/Page \d+ of \d+/i)).toBeNull();
    await speak(voice, "previous page");
    expect(page.at()).toBe(0);
  });

  /** Scenario: with the Daemon selector's menu open over a tall dashboard, “scroll down” and “next page” leave the dashboard where it is and say something is open over it. */
  it("does not scroll the dashboard behind the open Daemon selector", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    deck.fleet = tallFleet().fleet;
    deck.snapshot = deck.fleet[0];
    const page = scrollableDashboard(3000);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    fireEvent.click(screen.getByTestId("deck-selector-toggle"));
    expect(screen.getByTestId("deck-selector-menu")).toBeInTheDocument();
    await speak(voice, "scroll down");
    expect(page.scrollBy).not.toHaveBeenCalled();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/open over the dashboard/i);
    await speak(voice, "next page");
    expect(page.scrollBy).not.toHaveBeenCalled();
    expect(page.at()).toBe(0);
  });

  /** Scenario: with the Settings sheet open, or a stop confirmation open, over a tall dashboard, “scroll down” and “next page” leave the dashboard where it is and say something is open over it. */
  it("does not scroll the dashboard behind Settings or a stop confirmation", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    deck.fleet = tallFleet().fleet;
    deck.snapshot = deck.fleet[0];
    const page = scrollableDashboard(3000);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();

    fireEvent.click(screen.getByTestId("open-settings"));
    expect(screen.getByTestId("settings-panel")).toBeInTheDocument();
    await speak(voice, "scroll down");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/open over the dashboard/i);
    await speak(voice, "next page");
    expect(page.scrollBy).not.toHaveBeenCalled();
    fireEvent.click(within(screen.getByTestId("settings-panel")).getByRole("button", { name: /^close/i }));
    expect(screen.queryByTestId("settings-panel")).toBeNull();

    fireEvent.click(screen.getAllByRole("button", { name: /^Close .* agent$/ })[0]);
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
    await speak(voice, "scroll down");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/open over the dashboard/i);
    expect(page.scrollBy).not.toHaveBeenCalled();
    expect(page.at()).toBe(0);
  });

  /** Scenario: control — with the New agent dialog open over a tall dashboard, “next page” turns the directory browser's page and leaves the dashboard behind it where it was. */
  it("turns the directory page, not the dashboard, while the New agent dialog is open", async () => {
    const voice = microphone();
    const page = scrollableDashboard(3000);
    render(<DeckShell runtime={runtime(voice, true)} initialView={{ kind: "overview" }} />);
    await openBrowser();
    await turnOnVoice();
    await speak(voice, "next page");
    expect(screen.getByTestId("new-agent-directory-page")).toHaveTextContent(/Page 2 of \d+/i);
    expect(page.scrollBy).not.toHaveBeenCalled();
    expect(page.at()).toBe(0);
  });

  /** Scenario: saying the number of a row scrolled out of view — “twenty five” on a tall fleet — scrolls that row into view and opens its agent's pane. */
  it("scrolls a numbered row into view when its number is said", async () => {
    const voice = microphone();
    const deck = runtime(voice, true);
    deck.fleet = tallFleet().fleet;
    deck.snapshot = deck.fleet[0];
    const revealed: Element[] = [];
    Element.prototype.scrollIntoView = vi.fn(function (this: Element) { revealed.push(this); });
    try {
      render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
      await turnOnVoice();
      const row = document.querySelector(".overview-row[data-voice-number='25']");
      expect(row).not.toBeNull();
      const agentId = row!.getAttribute("data-testid")!.split(":").at(-1)!;
      await speak(voice, "twenty five");
      expect(revealed).toContain(row);
      expect(screen.queryByTestId("agent-pane-overlay"), screen.getByTestId("voice-report").textContent ?? "no voice report").not.toBeNull();
      expect(screen.getByTestId(`terminal-${decodeURIComponent(agentId)}`)).toBeInTheDocument();
    } finally {
      delete (Element.prototype as Partial<Element>).scrollIntoView;
    }
  });

  /** Scenario: two daemons have agents with the same id. Naming the selected daemon's agent opens it, not its namesake on another daemon further down the dashboard. */
  it("opens the selected daemon's named agent despite a namesake on another daemon", async () => {
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
    await speak(voice, `open ${selected.displayName}`);
    expect(screen.queryByTestId("agent-pane-overlay"), screen.getByTestId("voice-report").textContent ?? "no voice report").not.toBeNull();
    expect(screen.getByTestId(`terminal-${selected.id}`)).toBeVisible();
  });

  /** Scenario: two daemons without reported ids have no agent rows and sit on either side of a crowded daemon. With voice on, both of their sections are on the dashboard together. */
  it("shows every id-less row-less daemon with voice on", async () => {
    const deck = runtime(microphone(), true);
    const rowless = createFixtureSnapshot("disconnected");
    const waiting = (name: string) => ({
      ...rowless,
      runId: `run_${name}`,
      connection: { ...rowless.connection, deckKind: "remote" as const, name, socketPath: name },
      agents: [],
      totalNodes: 0,
    });
    deck.fleet = [waiting("Waiting Alpha"), deck.snapshot, waiting("Waiting Beta")];
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnOnVoice();
    const sections = screen.getAllByTestId("daemon-group").map((section) => within(section).getByTestId("daemon-identity").textContent);
    expect(sections).toEqual(expect.arrayContaining(["Waiting Alpha", "Waiting Beta"]));
    expect(screen.queryByTestId("overview-page")).toBeNull();
  });
});
