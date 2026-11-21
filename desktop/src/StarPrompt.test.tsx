import { act, fireEvent, render, renderHook, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import { OVERLAY_LAYER, topmostOverlayClose, useInertBackground } from "./hooks/useInertBackground";
import {
  chooseShownStarPrompt,
  DONT_ASK_AGAIN,
  LATER,
  NO_STAR_PROMPT,
  STAR,
  STAR_BODY_FIRST,
  STAR_BODY_SECOND,
  STAR_KEYS_ARM_MS,
  STAR_PROMPT_COVERED,
  STAR_REPO_DISPLAY,
  STAR_THANKS,
  STAR_TITLE,
  starPromptIsShown,
  useStarPrompt,
  type StarChoice,
} from "./lib/starPrompt";
import { VOICE_ACTIONS } from "./lib/voiceActions";
import { StarPromptDialog } from "./components/StarPromptDialog";
import type { DeckDirectoryListing, DeckRuntimeState, NewAgentOptions } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

/**
 * PRD #2174 — the desktop's star reminder: the TUI's star prompt, with its
 * words and keys, shown at startup when the shared reminder state says this
 * launch is one it shows at.
 */
function runtimeOf(star: Partial<DeckRuntimeState>): DeckRuntimeState {
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
    ...star,
  } as unknown as DeckRuntimeState;
}

/** Wait until the dialog's keys choose (`STAR_KEYS_ARM_MS` after it is drawn). */
async function keysReady(dialog: HTMLElement): Promise<void> {
  await waitFor(() => expect(dialog).toHaveAttribute("data-keys-ready", "true"), { timeout: STAR_KEYS_ARM_MS * 5 });
}

/** An overlay drawn over the reminder, standing in for any dialog opened after it. */
function Above() {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.dialog);
  return <div ref={ref} data-testid="above">above</div>;
}

describe("the star reminder dialog", () => {
  /** Scenario: the dialog opens. It shows the TUI prompt's title, its two lines and the repository, and the three choices in the TUI's words, each with its key. */
  it("says what the TUI's prompt says", () => {
    render(<StarPromptDialog onChoose={vi.fn()} />);
    const dialog = screen.getByRole("alertdialog", { name: STAR_TITLE });
    expect(dialog).toHaveTextContent(STAR_BODY_FIRST);
    expect(dialog).toHaveTextContent(STAR_BODY_SECOND);
    expect(dialog).toHaveTextContent(STAR_REPO_DISPLAY);
    expect(within(dialog).getByRole("button", { name: `s ${STAR}` })).toBeVisible();
    expect(within(dialog).getByRole("button", { name: `l ${LATER}` })).toBeVisible();
    expect(within(dialog).getByRole("button", { name: `d ${DONT_ASK_AGAIN}` })).toBeVisible();
  });

  /** Scenario: with the dialog open the user presses `s`, `l`, `Esc` and `d`, and clicks each button. Each key and each button makes the TUI's choice; other keys, the keys with a modifier held, and a held key's repeats make none. */
  it.each<[string, StarChoice]>([["s", "star"], ["l", "later"], ["Escape", "later"], ["d", "dismiss"]])("makes the TUI's choice for %s", async (key, choice) => {
    const onChoose = vi.fn();
    render(<StarPromptDialog onChoose={onChoose} />);
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

  it.each<[string, StarChoice]>([[STAR, "star"], [LATER, "later"], [DONT_ASK_AGAIN, "dismiss"]])("makes the choice of its %s button", (label, choice) => {
    const onChoose = vi.fn();
    render(<StarPromptDialog onChoose={onChoose} />);
    fireEvent.click(screen.getByRole("button", { name: new RegExp(`${label}$`) }), { detail: 1 });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith(choice);
  });

  /** Scenario: the dialog opens while the user is typing, and the next keys they type are `d` and `s`. Neither answers it; once it has been on screen a moment, `d` does. */
  it("lets no key typed as it opens answer it", async () => {
    const onChoose = vi.fn();
    render(<StarPromptDialog onChoose={onChoose} />);
    const dialog = screen.getByRole("alertdialog");
    fireEvent.keyDown(dialog, { key: "d" });
    fireEvent.keyDown(dialog, { key: "s" });
    expect(onChoose).not.toHaveBeenCalled();
    await keysReady(dialog);
    fireEvent.keyDown(dialog, { key: "d" });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("dismiss");
  });

  /** Scenario: a control behind the reminder has focus when the reminder opens, and the user answers it with `l`. Focus goes back to that control (PRD #211). */
  it("gives focus back to what had it when it closes", async () => {
    function Host() {
      const [open, setOpen] = useState(false);
      return <>
        <button data-testid="opener" onClick={() => setOpen(true)}>opener</button>
        {open && <StarPromptDialog onChoose={() => setOpen(false)} />}
      </>;
    }
    render(<Host />);
    const opener = screen.getByTestId("opener");
    opener.focus();
    fireEvent.click(opener, { detail: 1 });
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveFocus();
    await keysReady(dialog);
    fireEvent.keyDown(dialog, { key: "l" });
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(opener).toHaveFocus();
  });

  /** Scenario: with the reminder open, before and after its keys arm, the user presses keys the app binds at the window (`?`, a digit, `Ctrl+K`) with focus on the reminder and on the page behind it. None reaches the window (PRD #211). */
  it("lets no key reach the window behind it", async () => {
    const behind = vi.fn();
    window.addEventListener("keydown", behind);
    try {
      render(<StarPromptDialog onChoose={vi.fn()} />);
      const dialog = screen.getByRole("alertdialog");
      const keys = [{ key: "?" }, { key: "1" }, { key: "k", ctrlKey: true }];
      for (const target of [dialog, document.body]) for (const key of keys) fireEvent.keyDown(target, key);
      await keysReady(dialog);
      for (const target of [dialog, document.body]) for (const key of keys) fireEvent.keyDown(target, key);
      expect(behind).not.toHaveBeenCalled();
    } finally {
      window.removeEventListener("keydown", behind);
    }
  });

  /** Scenario: the reminder opens while the user is typing, focus is on Don't ask again, and the next key is Enter. It presses nothing before the keys arm, and the click a key makes is refused; once armed, it chooses (PRD #211). */
  it("lets no Enter press a button before its keys arm", async () => {
    const onChoose = vi.fn();
    render(<StarPromptDialog onChoose={onChoose} />);
    const dialog = screen.getByRole("alertdialog");
    const dismiss = within(dialog).getByRole("button", { name: `d ${DONT_ASK_AGAIN}` });
    dismiss.focus();
    expect(fireEvent.keyDown(dismiss, { key: "Enter" })).toBe(false);
    fireEvent.click(dismiss, { detail: 0 });
    expect(onChoose).not.toHaveBeenCalled();
    await keysReady(dialog);
    expect(fireEvent.keyDown(dismiss, { key: "Enter" })).toBe(true);
    fireEvent.click(dismiss, { detail: 0 });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("dismiss");
  });

  /** Scenario: the dialog takes the keyboard when it opens, so `s`, `l` and `d` reach it without a click. */
  it("takes focus when it opens", () => {
    render(<StarPromptDialog onChoose={vi.fn()} />);
    expect(screen.getByRole("alertdialog")).toHaveFocus();
  });

  /** Scenario: the dialog is on the overlay stack, so voice's `close` ("cancel", "never mind") closes it as Esc does: Later. */
  it("is Later for voice's close", () => {
    const onChoose = vi.fn();
    render(<StarPromptDialog onChoose={onChoose} />);
    act(() => { topmostOverlayClose()?.(); });
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("later");
  });
});

describe("the star reminder by voice", () => {
  /** Scenario: with no reminder showing, the user says "star the repo", "remind me later" or "don't ask again". Nothing is chosen, and the app says the reminder is not showing. */
  it("does nothing while the reminder is not showing", () => {
    expect(starPromptIsShown()).toBe(false);
    for (const choice of ["star", "later", "dismiss"] as const) expect(chooseShownStarPrompt(choice)).toBe(NO_STAR_PROMPT);
    const reportRefused = vi.fn();
    VOICE_ACTIONS.starRepo.run({ starRepo: () => chooseShownStarPrompt("star"), reportRefused });
    VOICE_ACTIONS.remindStarLater.run({ remindStarLater: () => chooseShownStarPrompt("later"), reportRefused });
    VOICE_ACTIONS.dismissStarPrompt.run({ dismissStarPrompt: () => chooseShownStarPrompt("dismiss"), reportRefused });
    expect(reportRefused.mock.calls).toEqual([[NO_STAR_PROMPT], [NO_STAR_PROMPT], [NO_STAR_PROMPT]]);
  });

  /** Scenario: with the reminder showing, each spoken choice makes that choice; once the dialog closes, the same words do nothing again. */
  it.each<StarChoice>(["star", "later", "dismiss"])("chooses %s while the reminder is showing", (choice) => {
    const onChoose = vi.fn();
    const { unmount } = render(<StarPromptDialog onChoose={onChoose} />);
    expect(chooseShownStarPrompt(choice)).toBeUndefined();
    expect(onChoose).toHaveBeenCalledExactlyOnceWith(choice);
    unmount();
    expect(chooseShownStarPrompt(choice)).toBe(NO_STAR_PROMPT);
  });

  /** Scenario: something is drawn over the reminder when the user speaks a choice. Nothing is chosen, and the app says why. */
  it("chooses nothing while something covers the reminder", () => {
    const onChoose = vi.fn();
    render(<><StarPromptDialog onChoose={onChoose} /><Above /></>);
    expect(chooseShownStarPrompt("dismiss")).toBe(STAR_PROMPT_COVERED);
    expect(onChoose).not.toHaveBeenCalled();
  });
});

describe("the star reminder's flow", () => {
  /** Scenario: the runtime says this launch shows the reminder. The flow is due until a choice is made, reports the choice to the runtime once, and passes Star's sentence on. */
  it("is due when the launch says so, until a choice", async () => {
    const report = vi.fn();
    const starPromptChoose = vi.fn(async () => STAR_THANKS);
    const runtime = { starPromptLaunch: vi.fn(async () => true), starPromptChoose };
    const { result } = renderHook(() => useStarPrompt(runtime, report));
    await waitFor(() => expect(result.current.due).toBe(true));
    act(() => result.current.choose("star"));
    expect(result.current.due).toBe(false);
    await waitFor(() => expect(report).toHaveBeenCalledExactlyOnceWith(STAR_THANKS));
    expect(starPromptChoose).toHaveBeenCalledExactlyOnceWith("star");
    expect(runtime.starPromptLaunch).toHaveBeenCalledOnce();
  });

  /** Scenario: the user chooses Don't ask again, and the choice cannot be saved. The app says why, and the reminder comes back so it can be answered again. */
  it("comes back when a choice cannot be saved", async () => {
    const report = vi.fn();
    const runtime = { starPromptLaunch: vi.fn(async () => true), starPromptChoose: vi.fn(async () => { throw new Error("Failed to write star prompt state"); }) };
    const { result } = renderHook(() => useStarPrompt(runtime, report));
    await waitFor(() => expect(result.current.due).toBe(true));
    act(() => result.current.choose("dismiss"));
    await waitFor(() => expect(report).toHaveBeenCalledExactlyOnceWith("Failed to write star prompt state"));
    expect(result.current.due).toBe(true);
  });

  /** Scenario: a launch the reminder does not show at, a runtime that cannot answer, and a runtime without the call: none shows the reminder. */
  it("is not due otherwise", async () => {
    for (const runtime of [{ starPromptLaunch: vi.fn(async () => false) }, { starPromptLaunch: vi.fn(async () => { throw new Error("unreadable"); }) }, {}]) {
      const { result } = renderHook(() => useStarPrompt(runtime, vi.fn()));
      await act(async () => { await Promise.resolve(); });
      expect(result.current.due).toBe(false);
    }
  });
});

describe("the star reminder in the app", () => {
  beforeEach(() => window.localStorage.clear());

  /** Scenario: the app starts on a launch the shared reminder state says is due. The reminder comes up over the dashboard; the user presses `d`, the choice goes to the runtime, and the dialog closes. */
  it("shows at startup when due, and a key answers it", async () => {
    const starPromptChoose = vi.fn(async () => undefined);
    render(<DeckShell runtime={runtimeOf({ starPromptLaunch: vi.fn(async () => true), starPromptChoose })} initialView={{ kind: "overview" }} />);
    const dialog = await screen.findByTestId("star-prompt-dialog");
    await keysReady(dialog);
    fireEvent.keyDown(dialog, { key: "d" });
    await waitFor(() => expect(screen.queryByTestId("star-prompt-dialog")).toBeNull());
    expect(starPromptChoose).toHaveBeenCalledExactlyOnceWith("dismiss");
  });

  /** Scenario: the user stars from the reminder on the deck screen. The app shows the sentence the runtime answers, as the TUI's status line does, until it is dismissed. */
  it("shows Star's sentence in the toast", async () => {
    render(<DeckShell runtime={runtimeOf({ starPromptLaunch: vi.fn(async () => true), starPromptChoose: vi.fn(async () => STAR_THANKS) })} initialView={{ kind: "deck" }} />);
    fireEvent.click(within(await screen.findByTestId("star-prompt-dialog")).getByRole("button", { name: `s ${STAR}` }), { detail: 1 });
    const toast = await screen.findByTestId("star-toast");
    expect(toast).toHaveTextContent(STAR_THANKS);
    fireEvent.click(within(toast).getByRole("button", { name: "Dismiss star message" }));
    expect(screen.queryByTestId("star-toast")).toBeNull();
  });

  /** Scenario: the user opens New agent before the startup check answers that the reminder is due. The reminder waits behind the dialog rather than covering it, and comes up once the dialog closes. */
  it("waits while the New agent dialog is open", async () => {
    let answer!: (due: boolean) => void;
    const starPromptLaunch = vi.fn(() => new Promise<boolean>((resolve) => { answer = resolve; }));
    const listing: DeckDirectoryListing = { kind: "listing", path: "/home/dev", displayPath: "/home/dev", parent: "/home", entries: [], truncated: false };
    const options: NewAgentOptions = { kind: "deck", agents: [], experimental: false, authoringKinds: [], lastCommand: "claude" };
    render(<DeckShell runtime={runtimeOf({ starPromptLaunch, listDirectories: vi.fn(async () => structuredClone(listing)), newAgentOptions: vi.fn(async () => structuredClone(options)) })} initialView={{ kind: "overview" }} />);
    fireEvent.click(await screen.findByTestId("overview-new-agent"));
    const dialog = await screen.findByTestId("new-agent-dialog");
    await act(async () => { answer(true); });
    expect(screen.queryByTestId("star-prompt-dialog")).toBeNull();
    fireEvent.click(within(dialog).getByRole("button", { name: "Close new agent" }));
    expect(await screen.findByTestId("star-prompt-dialog")).toBeVisible();
  });

  /** Scenario: the app starts on a due launch while its daemon is disconnected, so the connection banner is up. The reminder waits rather than cover it, and comes up once the daemon connects. */
  it("waits while the connection banner is up", async () => {
    const starPromptLaunch = vi.fn(async () => true);
    const connected = runtimeOf({ starPromptLaunch });
    const down = { ...connected, snapshot: { ...connected.snapshot, connection: { ...connected.snapshot.connection, status: "disconnected" as const } } };
    const { rerender } = render(<DeckShell runtime={down} initialView={{ kind: "deck" }} />);
    await waitFor(() => expect(starPromptLaunch).toHaveBeenCalled());
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("star-prompt-dialog")).toBeNull();
    rerender(<DeckShell runtime={connected} initialView={{ kind: "deck" }} />);
    expect(await screen.findByTestId("star-prompt-dialog")).toBeVisible();
  });

  /** Scenario: the app starts on a launch the reminder state says is not due. No reminder shows. */
  it("shows nothing when not due", async () => {
    const starPromptLaunch = vi.fn(async () => false);
    render(<DeckShell runtime={runtimeOf({ starPromptLaunch })} initialView={{ kind: "overview" }} />);
    await waitFor(() => expect(starPromptLaunch).toHaveBeenCalled());
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("star-prompt-dialog")).toBeNull();
  });
});
