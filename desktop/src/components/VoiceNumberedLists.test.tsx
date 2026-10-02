import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureFleet, createFixtureSnapshot, fixtureDirectoryTree, FIXTURE_HOMES } from "../data/fixture";
import { VOICE_PAGES_HOME, voicePagesDirectory } from "../data/fixtureCrowded";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";
import { VOICE_JOIN_WINDOW_MS, VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";
import type { VoiceNumberAnswerDto } from "../lib/voiceNumbers";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <div data-testid={`terminal-${agentId}`} />,
}));

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

function makeRuntime(voice: ReturnType<typeof microphone>, snapshot = createFixtureSnapshot("docs")) {
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => ({
    backend: "stub", resolveMs: 21,
    outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` },
  }));
  const runtime = {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures(), snapshot, fleet: [snapshot],
    terminalData: {}, clearError: vi.fn(), runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined), reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    setZoom: vi.fn(async (level: number) => level),
    testEndpoint: vi.fn(async () => { throw new Error("not used"); }),
    secretStatus: vi.fn(async () => ({ stored: false })),
    storeSecret: vi.fn(async () => ({ stored: true })),
    forgetSecret: vi.fn(async () => ({ stored: false })),
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: undefined })),
    saveSettings: vi.fn(async () => structuredClone(DEFAULT_DESKTOP_SETTINGS)),
    resolveVoice, ...voice,
  } as unknown as DeckRuntimeState;
  return { runtime, resolveVoice };
}

function makeDialogRuntime(voice: ReturnType<typeof microphone>) {
  const fleet = createFixtureFleet("voice-pages").slice(0, 2);
  const { runtime, resolveVoice } = makeRuntime(voice, fleet[0]);
  runtime.fleet = fleet;
  runtime.listDirectories = vi.fn(async (_deckId: string, path?: string) => {
    const listing = voicePagesDirectory(path ?? VOICE_PAGES_HOME);
    if (!listing) throw new Error(`No fixture directory at ${path}`);
    return { kind: "listing" as const, ...listing, displayPath: listing.path, truncated: false };
  });
  runtime.newAgentOptions = vi.fn(async () => ({ kind: "deck" as const, agents: [], experimental: false, authoringKinds: ["schedule", "schedule-issues", "dispatcher"] }));
  return { runtime, resolveVoice };
}

function makeFleetDialogRuntime(voice: ReturnType<typeof microphone>) {
  const fleet = createFixtureFleet("fleet").slice(0, 2);
  const { runtime, resolveVoice } = makeRuntime(voice, fleet[0]);
  runtime.fleet = fleet;
  runtime.listDirectories = vi.fn(async (deckId: string, path?: string) => {
    const home = FIXTURE_HOMES[deckId];
    const listing = fixtureDirectoryTree(home).get(path ?? home);
    if (!listing) throw new Error(`No fixture directory at ${path}`);
    return { kind: "listing" as const, ...listing, displayPath: listing.path, truncated: false };
  });
  runtime.newAgentOptions = vi.fn(async () => ({ kind: "deck" as const, agents: [], experimental: false, authoringKinds: ["schedule", "schedule-issues", "dispatcher"] }));
  return { runtime, resolveVoice };
}

/** Give jsdom's layout-free directory list a measured page that actually displays row 13. */
function showThirteenDirectories() {
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.matches(".new-agent-list.is-directories") ? 640 : 0;
  });
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this.matches(".new-agent-list.is-directories") ? 240 : 0;
  });
}

/** Keep both short sections visible in jsdom so a bare number genuinely has two on-screen matches. */
function showDialogListsFit() {
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.matches(".new-agent-list.is-directories, .new-agent-body, .new-agent-chips") ? 640 : 0;
  });
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
    if (this.matches(".new-agent-body")) return 500;
    return this.matches(".new-agent-list.is-directories, .new-agent-chips") ? 240 : 0;
  });
}

async function poll() {
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

async function finishPossibleNoMatch() {
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_JOIN_WINDOW_MS); });
}

describe("numbered voice lists through the dashboard", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

  /** Scenario: with the dashboard's third row displayed, saying its number alone or “open agent 3” opens that agent. Neither phrase asks the Commands backend. */
  it.each(["three", "open agent 3"])("opens the displayed third agent for %s without a Commands backend call", async (phrase) => {
    const voice = microphone();
    const { runtime, resolveVoice } = makeRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    expect(screen.getAllByRole("row", { name: /^3\./ })).toHaveLength(1);
    voice.deliver(phrase);
    await poll();
    await finishPossibleNoMatch();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("no matching action");
    expect(resolveVoice).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-pane-overlay").querySelector(".agent-assignment p"))
      .toHaveTextContent("Check the payment API for breaking changes.");
  });

  /** Scenario: the New agent dialog shows daemon 1, and “Select daemon 1” chooses that daemon locally. The selected daemon's directory browser opens without a Commands call. */
  it("chooses the daemon showing 1 when its kind is spoken", async () => {
    const voice = microphone();
    const { runtime, resolveVoice } = makeDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    const daemon = within(screen.getByTestId("new-agent-deck-list")).getByRole("option", { name: /^1\./ });
    const deckId = daemon.getAttribute("data-deck-id");
    expect(deckId).toBeTruthy();
    voice.deliver("Select daemon 1");
    await poll();
    await finishPossibleNoMatch();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("no matching action");
    expect(screen.getByTestId("new-agent-deck-list").querySelector("[data-chosen='true']")).toHaveAttribute("data-deck-id", deckId);
    expect(screen.getByTestId("new-agent-directory-list")).toBeInTheDocument();
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: “Select directory 13” enters the directory visibly labelled 13 in the New agent dialog. No Commands call is needed to identify the displayed row. */
  it("enters the directory showing 13 when its kind is spoken", async () => {
    showThirteenDirectories();
    const voice = microphone();
    const { runtime, resolveVoice } = makeDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    const directory = within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { name: /^13\./ });
    const path = directory.getAttribute("data-path");
    expect(path).toBeTruthy();
    voice.deliver("Select directory 13");
    await poll();
    await finishPossibleNoMatch();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("no matching action");
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(path!);
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: in the New agent form Schedule is mode 2, regardless of preceding daemon and directory rows. “Choose mode 2” selects it locally. */
  it("chooses a mode by its displayed number and kind", async () => {
    const voice = microphone();
    const { runtime, resolveVoice } = makeDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "docs" } });
    fireEvent.click(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { name: /docs/ }));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    const schedule = screen.getByTestId("new-agent-mode-schedule");
    expect(schedule).toHaveAccessibleName(/^2\./);
    expect(schedule).toHaveAttribute("aria-pressed", "false");
    voice.deliver("choose mode 2");
    await poll();
    await finishPossibleNoMatch();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("no matching action");
    expect(schedule).toHaveAttribute("aria-pressed", "true");
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: Directory 3 and Mode 3 are both on screen. Bare “three” offers just those labelled options, and choosing Mode 3 selects its chip. */
  it("offers exactly the matching sections for an ambiguous bare three", async () => {
    showDialogListsFit();
    const voice = microphone();
    const { runtime, resolveVoice } = makeFleetDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    const thirdMode = within(screen.getByTestId("new-agent-modes")).getAllByRole("button")[2];
    const modeId = thirdMode.getAttribute("data-mode");
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("three");
    await poll();
    const choice = screen.getByTestId("voice-choice");
    expect(within(choice).getAllByRole("button")).toHaveLength(3); // two answers and Cancel
    expect(within(choice).getByRole("button", { name: /^1\. Directory 3: scratch$/ })).toBeVisible();
    fireEvent.click(within(choice).getByRole("button", { name: /^2\. Mode 3: / }));
    expect(screen.queryByTestId("voice-choice")).toBeNull();
    expect(screen.getByTestId(`new-agent-mode-${modeId}`)).toHaveAttribute("aria-pressed", "true");
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: a directory list changes after speech capture but before “directory 3” is answered. The old number is refused as stale rather than opening another directory. */
  it("refuses a stale directory number after filtering its section", async () => {
    const voice = microphone();
    const { runtime, resolveVoice } = makeDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    expect(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { name: /^3\./ })).toBeVisible();
    voice.deliver("directory 3");
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "docs" } });
    await poll();
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(VOICE_PAGES_HOME);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/moved on|changed/i);
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: directory 13 is visible, but “select daemon 13” must not enter it because its kind is wrong. The report briefly names the mismatch without asking Commands. */
  it("refuses a visible number paired with the wrong kind", async () => {
    showThirteenDirectories();
    const voice = microphone();
    const { runtime, resolveVoice } = makeDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    expect(within(screen.getByTestId("new-agent-directory-list")).getByRole("option", { name: /^13\./ })).toBeVisible();
    voice.deliver("select daemon 13");
    await poll();
    await finishPossibleNoMatch();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("no matching action");
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(VOICE_PAGES_HOME);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/no daemon.*13|13 is.*directory|13.*not.*daemon/i);
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: “select directory 99” names no displayed directory, so the New agent browser stays put and reports that 99 is out of range. */
  it("refuses a directory number that no item shows", async () => {
    const voice = microphone();
    const { runtime, resolveVoice } = makeDialogRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(within(screen.getByTestId("new-agent-deck-list")).getAllByRole("option")[0]);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    expect(within(screen.getByTestId("new-agent-directory-list")).queryByRole("option", { name: /^99\./ })).toBeNull();
    voice.deliver("select directory 99");
    await poll();
    await finishPossibleNoMatch();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("no matching action");
    expect(screen.getByTestId("new-agent-current-path")).toHaveTextContent(VOICE_PAGES_HOME);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/no directory.*99|99.*not|99.*out of range/i);
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: the spoken number names both the first displayed position and an agent named orchestrator-1 in another position. The app offers both choices rather than guessing either agent. */
  it("offers a numbered choice when an agent name collides with one", async () => {
    const voice = microphone();
    const snapshot = createFixtureSnapshot("docs");
    snapshot.agents[1] = { ...snapshot.agents[1], displayName: "orchestrator-1" };
    const { runtime, resolveVoice } = makeRuntime(voice, snapshot);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("one");
    await poll();
    const choice = screen.getByRole("dialog", { name: "Which agent?" });
    expect(choice).toHaveTextContent("Plan / architecture");
    expect(choice).toHaveTextContent("orchestrator-1");
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: while a spoken number is being captured, the first dashboard agent disappears and the visible numbers move. The old answer is refused with nothing opened or sent to Commands. */
  it("refuses an answer after its numbered list changes", async () => {
    const voice = microphone();
    const snapshot = createFixtureSnapshot("docs");
    const { runtime, resolveVoice } = makeRuntime(voice, snapshot);
    const view = render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("three");
    const changed = { ...snapshot, agents: snapshot.agents.slice(1) };
    view.rerender(<DeckShell runtime={{ ...runtime, snapshot: changed, fleet: [changed] }} initialView={{ kind: "overview" }} />);
    await poll();
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/moved on|changed/i);
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: the displayed agents change and return to their original order while a number is being answered. The old utterance is stale even though the list looks the same again. */
  it("refuses an answer after its numbered list changes away and back", async () => {
    const voice = microphone();
    const snapshot = createFixtureSnapshot("docs");
    const { runtime } = makeRuntime(voice, snapshot);
    let finishAnswer!: (answer: VoiceNumberAnswerDto) => void;
    runtime.answerVoiceNumber = vi.fn(() => new Promise<VoiceNumberAnswerDto>((resolve) => { finishAnswer = resolve; }));
    const view = render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("three");
    await poll();
    const changed = { ...snapshot, agents: snapshot.agents.slice(1) };
    view.rerender(<DeckShell runtime={{ ...runtime, snapshot: changed, fleet: [changed] }} initialView={{ kind: "overview" }} />);
    view.rerender(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { finishAnswer({ kind: "selected", number: 3 }); });
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/moved on|changed/i);
  });

  /** Scenario: a spoken number is pending while the selected daemon changes. Even with identical dashboard rows, the answer belongs to the old context and must not open an agent. */
  it("refuses a number answered after the selected daemon changes", async () => {
    const voice = microphone();
    const snapshot = createFixtureSnapshot("docs");
    const { runtime } = makeRuntime(voice, snapshot);
    let finishAnswer!: (answer: VoiceNumberAnswerDto) => void;
    runtime.answerVoiceNumber = vi.fn(() => new Promise<VoiceNumberAnswerDto>((resolve) => { finishAnswer = resolve; }));
    const view = render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("three");
    await poll();
    const switched = { ...snapshot, connection: { ...snapshot.connection, deckId: "another-daemon" } };
    view.rerender(<DeckShell runtime={{ ...runtime, snapshot: switched }} initialView={{ kind: "overview" }} />);
    await act(async () => { finishAnswer({ kind: "selected", number: 3 }); });
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/moved on|changed/i);
  });
});
