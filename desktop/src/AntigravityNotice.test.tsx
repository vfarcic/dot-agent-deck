import { act, fireEvent, render, renderHook, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import { OVERLAY_LAYER, topmostOverlayClose, useInertBackground } from "./hooks/useInertBackground";
import {
  announceNewAgentStart,
  antigravityNoticeIsShown,
  ANTIGRAVITY_NOTICE_COVERED,
  chooseShownAntigravityNotice,
  DONT_SHOW_AGAIN,
  NO_ANTIGRAVITY_NOTICE,
  NOTICE_BODY,
  NOTICE_ISSUES_DISPLAY,
  NOTICE_KEYS_ARM_MS,
  NOTICE_LATER,
  NOTICE_THANKS,
  NOTICE_TITLE,
  OPEN_ISSUES,
  useAntigravityNotice,
  type NoticeChoice,
} from "./lib/antigravityNotice";
import { VOICE_ACTIONS } from "./lib/voiceActions";
import { AntigravityNoticeDialog } from "./components/AntigravityNoticeDialog";
import type { DeckDirectoryListing, DeckRuntimeState, NewAgentOptions } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

/**
 * PRD #211 — the desktop's Antigravity experimental notice: the TUI's notice,
 * with its words and keys, shown after a New agent start of Antigravity when
 * the shared notice state says that start is one it shows at.
 */
function runtimeOf(extra: Partial<DeckRuntimeState>): DeckRuntimeState {
  const deck = createFixtureSnapshot("connected");
  const settings: DesktopSettingsDto = { ...structuredClone(DEFAULT_DESKTOP_SETTINGS), endpoints: { remote: [], selection: LOCAL_ENDPOINT_SELECTION } };
  return {
    mode: "live",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: vi.fn(async () => ({ settings: structuredClone(settings), path: "/tmp/desktop.toml" })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => structuredClone(next)),
    snapshot: deck,
    fleet: [deck],
    ...extra,
  } as unknown as DeckRuntimeState;
}

/** Wait until the dialog's keys choose (`NOTICE_KEYS_ARM_MS` after it is drawn). */
async function keysReady(dialog: HTMLElement): Promise<void> {
  await waitFor(() => expect(dialog).toHaveAttribute("data-keys-ready", "true"), { timeout: NOTICE_KEYS_ARM_MS * 5 });
}

/** An overlay drawn over the notice, standing in for any dialog opened after it. */
function Above() {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.dialog);
  return <div ref={ref} data-testid="above">above</div>;
}

describe("the Antigravity notice dialog", () => {
  /** Scenario: the notice opens. It shows the TUI notice's title, its request and the issues address, and the three choices in the TUI's words, each with its key. */
  it("says what the TUI's notice says", () => {
    render(<AntigravityNoticeDialog onChoose={vi.fn()} />);
    const dialog = screen.getByRole("alertdialog", { name: NOTICE_TITLE });
    expect(dialog).toHaveTextContent(NOTICE_BODY);
    expect(dialog).toHaveTextContent(NOTICE_ISSUES_DISPLAY);
    expect(within(dialog).getByRole("button", { name: `o ${OPEN_ISSUES}` })).toBeVisible();
    expect(within(dialog).getByRole("button", { name: `l ${NOTICE_LATER}` })).toBeVisible();
    expect(within(dialog).getByRole("button", { name: `d ${DONT_SHOW_AGAIN}` })).toBeVisible();
  });

  /** Scenario: with the notice open the user presses `o`, `l`, `Esc` and `d`. Each key makes the TUI's choice; other keys, the keys with a modifier held, and a held key's repeats make none. */
  it.each<[string, NoticeChoice]>([["o", "open"], ["l", "later"], ["Escape", "later"], ["d", "dismiss"]])("makes the TUI's choice for %s", async (key, choice) => {
    const onChoose = vi.fn();
    render(<AntigravityNoticeDialog onChoose={onChoose} />);
    const dialog = screen.getByRole("alertdialog");
    await keysReady(dialog);
    fireEvent.keyDown(dialog, { key, repeat: true });
    fireEvent.keyDown(dialog, { key: "x" });
    fireEvent.keyDown(dialog, { key, ctrlKey: true });
    fireEvent.keyDown(dialog, { key: "Enter" });
    expect(onChoose).not.toHaveBeenCalled();
    fireEvent.keyDown(dialog, { key });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith(choice);
  });

  /** Scenario: the user clicks each of the notice's buttons. Each makes its choice. */
  it.each<[string, NoticeChoice]>([[OPEN_ISSUES, "open"], [NOTICE_LATER, "later"], [DONT_SHOW_AGAIN, "dismiss"]])("makes the choice of its %s button", (label, choice) => {
    const onChoose = vi.fn();
    render(<AntigravityNoticeDialog onChoose={onChoose} />);
    fireEvent.click(screen.getByRole("button", { name: new RegExp(`${label}$`) }));
    expect(onChoose).toHaveBeenCalledExactlyOnceWith(choice);
  });

  /** Scenario: the notice opens while the user is typing into the new agent, and the next keys are `d` and `o`. Neither answers it nor travels on; once it has been on screen a moment, `d` does. */
  it("lets no key typed as it opens answer it or pass it", async () => {
    const onChoose = vi.fn();
    const behind = vi.fn();
    render(<div onKeyDown={behind}><AntigravityNoticeDialog onChoose={onChoose} /></div>);
    const dialog = screen.getByRole("alertdialog");
    fireEvent.keyDown(dialog, { key: "d" });
    fireEvent.keyDown(dialog, { key: "o" });
    expect(onChoose).not.toHaveBeenCalled();
    expect(behind).not.toHaveBeenCalled();
    await keysReady(dialog);
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("later");
    expect(behind).not.toHaveBeenCalled();
  });

  /** Scenario: the notice takes the keyboard when it opens, so `o`, `l` and `d` reach it rather than the agent behind it. */
  it("takes focus when it opens", () => {
    render(<AntigravityNoticeDialog onChoose={vi.fn()} />);
    expect(screen.getByRole("alertdialog")).toHaveFocus();
  });

  /** Scenario: the notice is on the overlay stack, so voice's `close` ("cancel", "never mind") closes it as Esc does: Later. */
  it("is Later for voice's close", () => {
    const onChoose = vi.fn();
    render(<AntigravityNoticeDialog onChoose={onChoose} />);
    act(() => { topmostOverlayClose()?.(); });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("later");
  });
});

describe("the Antigravity notice by voice", () => {
  /** Scenario: with no notice showing, the user says "open issues" or "don't show again". Nothing is chosen, and the app says the notice is not showing. */
  it("does nothing while the notice is not showing", () => {
    expect(antigravityNoticeIsShown()).toBe(false);
    for (const choice of ["open", "later", "dismiss"] as const) expect(chooseShownAntigravityNotice(choice)).toBe(NO_ANTIGRAVITY_NOTICE);
    const reportRefused = vi.fn();
    VOICE_ACTIONS.openAntigravityIssues.run({ openAntigravityIssues: () => chooseShownAntigravityNotice("open"), reportRefused });
    VOICE_ACTIONS.dismissAntigravityNotice.run({ dismissAntigravityNotice: () => chooseShownAntigravityNotice("dismiss"), reportRefused });
    expect(reportRefused.mock.calls).toEqual([[NO_ANTIGRAVITY_NOTICE], [NO_ANTIGRAVITY_NOTICE]]);
  });

  /** Scenario: with the notice showing, each spoken choice makes that choice; once the dialog closes, the same words do nothing again. */
  it.each<NoticeChoice>(["open", "later", "dismiss"])("chooses %s while the notice is showing", (choice) => {
    const onChoose = vi.fn();
    const { unmount } = render(<AntigravityNoticeDialog onChoose={onChoose} />);
    expect(chooseShownAntigravityNotice(choice)).toBeUndefined();
    expect(onChoose).toHaveBeenCalledExactlyOnceWith(choice);
    unmount();
    expect(chooseShownAntigravityNotice(choice)).toBe(NO_ANTIGRAVITY_NOTICE);
  });

  /** Scenario: something is drawn over the notice when the user speaks a choice. Nothing is chosen, and the app says why. */
  it("chooses nothing while something covers the notice", () => {
    const onChoose = vi.fn();
    render(<><AntigravityNoticeDialog onChoose={onChoose} /><Above /></>);
    expect(chooseShownAntigravityNotice("dismiss")).toBe(ANTIGRAVITY_NOTICE_COVERED);
    expect(onChoose).not.toHaveBeenCalled();
  });

  /** Scenario: Later has no voice row of its own: voice's `close` answers it, and the registry says so. */
  it("leaves Later to close", () => {
    expect(VOICE_ACTIONS.remindAntigravityLater.no_voice).toMatch(/close/);
    expect(VOICE_ACTIONS.openAntigravityIssues.voice).toBe(true);
    expect(VOICE_ACTIONS.dismissAntigravityNotice.voice).toBe(true);
  });
});

describe("the Antigravity notice's flow", () => {
  /** Scenario: a New agent start is announced and the runtime says it shows the notice. The flow is due until a choice is made, reports the choice once, and passes Open issues' sentence on. */
  it("is due when a start says so, until a choice", async () => {
    const report = vi.fn();
    const antigravityNoticeChoose = vi.fn(async () => NOTICE_THANKS);
    const runtime = { antigravityNoticeStart: vi.fn(async () => true), antigravityNoticeChoose };
    const { result } = renderHook(() => useAntigravityNotice(runtime, report));
    expect(result.current.due).toBe(false);
    act(() => announceNewAgentStart("agy"));
    await waitFor(() => expect(result.current.due).toBe(true));
    expect(runtime.antigravityNoticeStart).toHaveBeenCalledExactlyOnceWith("agy");
    act(() => result.current.choose("open"));
    expect(result.current.due).toBe(false);
    await waitFor(() => expect(report).toHaveBeenCalledExactlyOnceWith(NOTICE_THANKS));
    expect(antigravityNoticeChoose).toHaveBeenCalledExactlyOnceWith("open");
  });

  /** Scenario: the user chooses Don't show again, and the choice cannot be saved. The app says why, and the notice comes back so it can be answered again. */
  it("comes back when a choice cannot be saved", async () => {
    const report = vi.fn();
    const runtime = { antigravityNoticeStart: vi.fn(async () => true), antigravityNoticeChoose: vi.fn(async () => { throw new Error("Failed to write Antigravity notice state"); }) };
    const { result } = renderHook(() => useAntigravityNotice(runtime, report));
    act(() => announceNewAgentStart("agy"));
    await waitFor(() => expect(result.current.due).toBe(true));
    act(() => result.current.choose("dismiss"));
    await waitFor(() => expect(report).toHaveBeenCalledExactlyOnceWith("Failed to write Antigravity notice state"));
    expect(result.current.due).toBe(true);
  });

  /** Scenario: a start the notice does not show at, a runtime that cannot answer, and a runtime without the call: none shows the notice. */
  it("is not due otherwise", async () => {
    for (const runtime of [{ antigravityNoticeStart: vi.fn(async () => false) }, { antigravityNoticeStart: vi.fn(async () => { throw new Error("unreadable"); }) }, {}]) {
      const { result, unmount } = renderHook(() => useAntigravityNotice(runtime, vi.fn()));
      act(() => announceNewAgentStart("agy"));
      await act(async () => { await Promise.resolve(); });
      expect(result.current.due).toBe(false);
      unmount();
    }
  });
});

describe("the Antigravity notice in the app", () => {
  beforeEach(() => window.localStorage.clear());

  const listing: DeckDirectoryListing = { kind: "listing", path: "/home/dev", displayPath: "/home/dev", parent: "/home", entries: [], truncated: false };
  const optionsFor = (lastCommand: string): NewAgentOptions => ({ kind: "deck", agents: [], experimental: false, authoringKinds: [], lastCommand });

  /** Open New agent, use the home directory, and Start with the Command the deck last ran. */
  async function startFromNewAgent(command: string): Promise<void> {
    fireEvent.click(await screen.findByTestId("overview-new-agent"));
    const dialog = await screen.findByTestId("new-agent-dialog");
    await waitFor(() => expect(within(dialog).getByTestId("new-agent-current-path").textContent).toBe("/home/dev"));
    fireEvent.keyDown(within(dialog).getByTestId("new-agent-directory-list"), { key: "Enter" });
    await waitFor(() => expect(within(dialog).getByTestId("new-agent-command")).toHaveValue(command));
    fireEvent.click(within(dialog).getByTestId("new-agent-start"));
  }

  /** Scenario: the user starts `agy` from the New agent dialog on a start the shared notice state says is due. The notice comes up once the dialog closes; the user presses `d`, the choice goes to the runtime, and the notice closes. */
  it("shows after a New agent start of Antigravity, and a key answers it", async () => {
    const antigravityNoticeStart = vi.fn(async (command: string | undefined) => command === "agy");
    const antigravityNoticeChoose = vi.fn(async () => undefined);
    render(<DeckShell runtime={runtimeOf({ antigravityNoticeStart, antigravityNoticeChoose, listDirectories: vi.fn(async () => structuredClone(listing)), newAgentOptions: vi.fn(async () => optionsFor("agy")) })} initialView={{ kind: "overview" }} />);
    await startFromNewAgent("agy");
    const notice = await screen.findByTestId("antigravity-notice-dialog");
    expect(antigravityNoticeStart).toHaveBeenCalledExactlyOnceWith("agy");
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
    await keysReady(notice);
    fireEvent.keyDown(notice, { key: "d" });
    await waitFor(() => expect(screen.queryByTestId("antigravity-notice-dialog")).toBeNull());
    expect(antigravityNoticeChoose).toHaveBeenCalledExactlyOnceWith("dismiss");
  });

  /** Open New agent on a deck whose `default_command` is `agy`, use the home directory, and clear the Command field. */
  async function blankCommandOnAgyDeck(): Promise<HTMLElement> {
    fireEvent.click(await screen.findByTestId("overview-new-agent"));
    const dialog = await screen.findByTestId("new-agent-dialog");
    await waitFor(() => expect(within(dialog).getByTestId("new-agent-current-path").textContent).toBe("/home/dev"));
    fireEvent.keyDown(within(dialog).getByTestId("new-agent-directory-list"), { key: "Enter" });
    await waitFor(() => expect(within(dialog).getByTestId("new-agent-command")).toHaveValue("agy"));
    fireEvent.change(within(dialog).getByTestId("new-agent-command"), { target: { value: "" } });
    return dialog;
  }
  const agyDeck: NewAgentOptions = { kind: "deck", agents: [], experimental: false, authoringKinds: ["dispatcher"], defaultCommand: "agy" };

  /** Scenario: on a deck whose `default_command` is `agy`, the user chooses the dispatcher mode, leaves Command empty and starts. The empty Command starts `agy`, so the start is announced as `agy` and the notice comes up. */
  it("counts a start whose empty Command a built-in mode resolves to agy", async () => {
    const runAction = vi.fn(async () => ({ ok: true }));
    const antigravityNoticeStart = vi.fn(async (command: string | undefined) => command === "agy");
    render(<DeckShell runtime={runtimeOf({ runAction, antigravityNoticeStart, antigravityNoticeChoose: vi.fn(async () => undefined), listDirectories: vi.fn(async () => structuredClone(listing)), newAgentOptions: vi.fn(async () => structuredClone(agyDeck)) })} initialView={{ kind: "overview" }} />);
    const dialog = await blankCommandOnAgyDeck();
    fireEvent.click(await within(dialog).findByTestId("new-agent-mode-dispatcher"));
    fireEvent.click(within(dialog).getByTestId("new-agent-start"));
    expect(await screen.findByTestId("antigravity-notice-dialog")).toBeVisible();
    expect(runAction).toHaveBeenCalledWith(expect.objectContaining({ type: "start_agent", command: "agy", authoringKind: "dispatcher" }));
    expect(antigravityNoticeStart).toHaveBeenCalledExactlyOnceWith("agy");
  });

  /** Scenario: on the same deck the user leaves Command empty with No mode and starts. That start runs the deck's default shell, not `agy`, so it is announced with no command and no notice comes up. */
  it("does not count an empty plain Command, which starts the deck's shell", async () => {
    const runAction = vi.fn(async () => ({ ok: true }));
    const antigravityNoticeStart = vi.fn(async (command: string | undefined) => command === "agy");
    render(<DeckShell runtime={runtimeOf({ runAction, antigravityNoticeStart, listDirectories: vi.fn(async () => structuredClone(listing)), newAgentOptions: vi.fn(async () => structuredClone(agyDeck)) })} initialView={{ kind: "overview" }} />);
    const dialog = await blankCommandOnAgyDeck();
    fireEvent.click(within(dialog).getByTestId("new-agent-start"));
    await waitFor(() => expect(antigravityNoticeStart).toHaveBeenCalledExactlyOnceWith(undefined));
    expect(runAction).toHaveBeenCalledWith(expect.not.objectContaining({ command: expect.anything() }));
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("antigravity-notice-dialog")).toBeNull();
  });

  /** Scenario: the user answers the notice with Open issues. Its sentence shows in a toast whose close button names the Antigravity notice, not the star reminder, and closing it clears the toast. */
  it("reports Open issues in a toast of its own", async () => {
    render(<DeckShell runtime={runtimeOf({ antigravityNoticeStart: vi.fn(async () => true), antigravityNoticeChoose: vi.fn(async () => NOTICE_THANKS) })} initialView={{ kind: "deck" }} />);
    await act(async () => { announceNewAgentStart("agy"); await Promise.resolve(); });
    fireEvent.click(within(await screen.findByTestId("antigravity-notice-dialog")).getByRole("button", { name: `o ${OPEN_ISSUES}` }));
    const toast = await screen.findByTestId("antigravity-toast");
    expect(toast).toHaveTextContent(NOTICE_THANKS);
    expect(screen.queryByTestId("star-toast")).toBeNull();
    expect(within(toast).queryByRole("button", { name: "Dismiss star message" })).toBeNull();
    fireEvent.click(within(toast).getByRole("button", { name: "Dismiss Antigravity message" }));
    expect(screen.queryByTestId("antigravity-toast")).toBeNull();
  });

  /** Scenario: the user starts another agent from the New agent dialog. The start is announced, the runtime says it shows nothing, and no notice comes up. */
  it("shows nothing after a start the runtime does not show it for", async () => {
    const antigravityNoticeStart = vi.fn(async (command: string | undefined) => command === "agy");
    render(<DeckShell runtime={runtimeOf({ antigravityNoticeStart, listDirectories: vi.fn(async () => structuredClone(listing)), newAgentOptions: vi.fn(async () => optionsFor("claude")) })} initialView={{ kind: "overview" }} />);
    await startFromNewAgent("claude");
    await waitFor(() => expect(antigravityNoticeStart).toHaveBeenCalledExactlyOnceWith("claude"));
    await waitFor(() => expect(screen.queryByTestId("new-agent-dialog")).toBeNull());
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("antigravity-notice-dialog")).toBeNull();
  });

  /** Scenario: the star reminder is due at startup but waits behind the New agent dialog, and the user starts Antigravity from it. The notice comes up alone; once it is answered the star reminder follows — never both at once. */
  it("shows one reminder at a time with the star reminder", async () => {
    let answer!: (due: boolean) => void;
    const starPromptLaunch = vi.fn(() => new Promise<boolean>((resolve) => { answer = resolve; }));
    const antigravityNoticeStart = vi.fn(async () => true);
    render(<DeckShell runtime={runtimeOf({ starPromptLaunch, starPromptChoose: vi.fn(async () => undefined), antigravityNoticeStart, antigravityNoticeChoose: vi.fn(async () => undefined), listDirectories: vi.fn(async () => structuredClone(listing)), newAgentOptions: vi.fn(async () => optionsFor("agy")) })} initialView={{ kind: "overview" }} />);
    fireEvent.click(await screen.findByTestId("overview-new-agent"));
    await screen.findByTestId("new-agent-dialog");
    await act(async () => { answer(true); });
    fireEvent.click(within(screen.getByTestId("new-agent-dialog")).getByRole("button", { name: "Close new agent" }));
    // The star reminder came up as the dialog closed; answer it Later so the
    // next New agent start is the one the notice follows.
    const star = await screen.findByTestId("star-prompt-dialog");
    await keysReady(star);
    fireEvent.keyDown(star, { key: "l" });
    await waitFor(() => expect(screen.queryByTestId("star-prompt-dialog")).toBeNull());
    await startFromNewAgent("agy");
    const notice = await screen.findByTestId("antigravity-notice-dialog");
    expect(screen.queryByTestId("star-prompt-dialog")).toBeNull();
    await keysReady(notice);
    fireEvent.keyDown(notice, { key: "l" });
    await waitFor(() => expect(screen.queryByTestId("antigravity-notice-dialog")).toBeNull());
  });

  /** Scenario: the star reminder is due and on screen when a start makes the notice due. The notice waits behind the reminder and comes up once it is answered. */
  it("waits behind the star reminder on screen", async () => {
    render(<DeckShell runtime={runtimeOf({ starPromptLaunch: vi.fn(async () => true), starPromptChoose: vi.fn(async () => undefined), antigravityNoticeStart: vi.fn(async () => true), antigravityNoticeChoose: vi.fn(async () => undefined) })} initialView={{ kind: "overview" }} />);
    const star = await screen.findByTestId("star-prompt-dialog");
    await act(async () => { announceNewAgentStart("agy"); await Promise.resolve(); });
    expect(screen.queryByTestId("antigravity-notice-dialog")).toBeNull();
    await keysReady(star);
    fireEvent.keyDown(star, { key: "d" });
    expect(await screen.findByTestId("antigravity-notice-dialog")).toBeVisible();
    expect(screen.queryByTestId("star-prompt-dialog")).toBeNull();
  });
});
