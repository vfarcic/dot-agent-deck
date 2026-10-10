import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS, mapDesktopSnapshot, type DesktopAgentDto, type DesktopSettingsDto, type PullRequestInfoDto, type VoiceResultDto, type VoiceStatusDto } from "./lib/bridge";
import { PrBrowserHostContext, type PrBrowserHost } from "./lib/prBrowser";
import { PullRequestBadge } from "./components/PullRequestBadge";
import { pullRequestFromDto, pullRequestLabel } from "./lib/pullRequest";
import type { DeckActionResult, DeckRuntimeState } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

const DECK = "deck-000000000000dec1";
const PR_URL = "https://github.com/vfarcic/dot-agent-deck/pull/12345";

function fakeHost() {
  let closedListener: (() => void) | undefined;
  const host = {
    available: true,
    open: vi.fn(async () => undefined),
    setBounds: vi.fn(async () => undefined),
    setVisible: vi.fn(async () => undefined),
    back: vi.fn(async () => undefined),
    scroll: vi.fn(async () => undefined),
    openExternal: vi.fn(async () => undefined),
    close: vi.fn(async () => undefined),
    signOut: vi.fn(async () => undefined),
    onClosed: vi.fn((listener: () => void) => {
      closedListener = listener;
      return () => { closedListener = undefined; };
    }),
  } satisfies PrBrowserHost;
  return { host, closeFromPage: () => closedListener?.() };
}

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      document = structuredClone(next);
      return structuredClone(document);
    }),
  };
}

function agent(id: string, pullRequest?: PullRequestInfoDto): DesktopAgentDto {
  return { id, displayName: `Coder ${id}`, cwd: "/tmp/project", rows: 32, cols: 120, agentType: "claude_code", status: "working", toolCount: 3, tab: { kind: "dashboard" }, ...(pullRequest ? { pullRequest } : {}) };
}

function runtime(agents: DesktopAgentDto[], overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const snapshot = mapDesktopSnapshot({
    connection: { status: "connected", deckId: DECK, socketPath: "/tmp/deck.sock", deckKind: "local", clientProtocolVersion: 8, serverProtocolVersion: 8, clientBuildVersion: "0.1.0", daemonBuildVersion: "0.1.0" },
    agents,
    protocolVersion: 8,
    source: "daemon",
  });
  const settings = settingsStore();
  return {
    mode: "live",
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    ...overrides,
  } as unknown as DeckRuntimeState;
}

/**
 * The real shell and voice panel with one synthetic utterance: the microphone
 * and the intent answer are stubbed, everything after them is the app's.
 */
function voiceRuntime(outcome: VoiceResultDto["outcome"]) {
  let started = false;
  let delivered = false;
  let nextOutcome = outcome;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({ state, capturedMs: 1240, maxMs: 30_000, capped: false, available: true, backend: "remote" });
  const resolveVoice = vi.fn(async (): Promise<VoiceResultDto> => ({ outcome: nextOutcome, resolveMs: null, backend: "stub" }));
  const declareVoiceScreen = vi.fn();
  return {
    deliver: (next: VoiceResultDto["outcome"]) => { nextOutcome = next; delivered = false; },
    resolveVoice,
    declareVoiceScreen,
    overrides: {
      resolveVoice,
      declareVoiceScreen,
      voiceStatus: vi.fn(async () => {
        if (started && !delivered) { delivered = true; return status("done"); }
        return status(started ? "recording" : "idle");
      }),
      voiceStart: vi.fn(async () => { started = true; return status("recording"); }),
      voiceStop: vi.fn(async () => {
        const said = nextOutcome.kind === "dispatch" ? nextOutcome.transcript : "";
        return { outcome: { kind: "heard" as const, transcript: said, sentence: `Heard: ${said}.` }, transcribeMs: null, backend: "remote" as const, audioMs: 1240 };
      }),
      voiceCancel: vi.fn(async () => status("idle")),
      voiceCommands: vi.fn(async () => []),
    } satisfies Partial<DeckRuntimeState>,
  };
}

function dispatchOutcome(action: string, invoke: string, transcript: string, sentence: string): VoiceResultDto["outcome"] {
  return { kind: "dispatch", transcript, action, invoke, params: [], sentence };
}

const withPr = agent("7", { number: 12345, url: PR_URL, state: "open", review: "review_required" });
const agentView = { kind: "agent" as const, deckId: DECK, agentId: "7", from: "overview" as const };

function renderShell(host: PrBrowserHost, agents: DesktopAgentDto[] = [withPr]) {
  const result = render(
    <PrBrowserHostContext.Provider value={host}>
      <DeckShell runtime={runtime(agents)} initialView={agentView} />
    </PrBrowserHostContext.Provider>,
  );
  return {
    ...result,
    rerenderWith: (next: DesktopAgentDto[]) => result.rerender(
      <PrBrowserHostContext.Provider value={host}>
        <DeckShell runtime={runtime(next)} initialView={agentView} />
      </PrBrowserHostContext.Provider>,
    ),
  };
}

describe("PRD #1401 — the pull request badge", () => {
  /**
   * Scenario: an agent the daemon found an open pull request for, still
   * waiting on a review. Its screen shows `#12345` with the state and review
   * icons, named in words for a screen reader and a tooltip.
   */
  it("shows the number, the state and the review on the agent's screen", () => {
    const { host } = fakeHost();
    renderShell(host);
    const badge = screen.getByTestId("pr-badge-7");

    expect(badge).toHaveTextContent("#12345");
    expect(badge).toHaveAttribute("data-pr-state", "open");
    expect(badge).toHaveAttribute("data-pr-review", "review_required");
    expect(badge).toHaveAccessibleName("Pull request #12345: open, review required");
    expect(badge.getAttribute("title")).toContain("Pull request #12345: open, review required");
    expect(badge.querySelectorAll("svg")).toHaveLength(2);
  });

  /** Scenario: an agent with no pull request shows nothing new. */
  it("shows nothing for an agent with no pull request", () => {
    const { host } = fakeHost();
    renderShell(host, [agent("7")]);
    expect(screen.queryByTestId("pr-badge-7")).not.toBeInTheDocument();
    expect(screen.queryByText(/Pull request #/)).not.toBeInTheDocument();
  });

  /** Scenario: a pull request whose state is unknown uses a different glyph from an open request, with its unknown meaning named for readers. */
  it("distinguishes an unknown PR state from the open PR icon", () => {
    render(<>
      <PullRequestBadge pullRequest={{ number: 9, state: "open" }} testId="open-badge" />
      <PullRequestBadge pullRequest={{ number: 9, state: "unknown" }} testId="unknown-badge" />
    </>);
    const unknown = screen.getByTestId("unknown-badge");
    expect(unknown).toHaveAccessibleName("Pull request #9: state unknown");
    const glyph = (badge: HTMLElement) => badge.querySelector("svg")!.innerHTML;
    expect(glyph(unknown), "Unknown must not show the open pull request glyph").not.toBe(glyph(screen.getByTestId("open-badge")));
  });

  it.each([
    ["open", undefined, "Pull request #9: open"],
    ["draft", "review_required", "Pull request #9: draft, review required"],
    ["merged", "approved", "Pull request #9: merged, approved"],
    ["closed", "changes_requested", "Pull request #9: closed, changes requested"],
    ["unknown", "unknown", "Pull request #9: state unknown, review status unknown"],
  ] as const)("names a %s pull request with review %s", (state, review, label) => {
    const pullRequest = pullRequestFromDto({ number: 9, url: "https://github.com/o/r/pull/9", state, ...(review ? { review } : {}) })!;
    render(<PullRequestBadge pullRequest={pullRequest} testId="badge" />);
    const badge = screen.getByTestId("badge");

    expect(badge).toHaveAccessibleName(label);
    expect(badge).toHaveAttribute("data-pr-state", state);
    expect(badge.className).toContain(`pr-state-${state}`);
    // One icon for the state, and one for the review only when there is one.
    expect(badge.querySelectorAll("svg")).toHaveLength(review ? 2 : 1);
    // Without an opener it is a label, not a control.
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  /** A value from a newer daemon reads as unknown rather than failing the record. */
  it("reads a state or review it does not know as unknown, and refuses a URL off github.com", () => {
    const odd = pullRequestFromDto({ number: 3, url: "https://github.com.evil.example/o/r/pull/3", state: "queued" as never, review: "dismissed" as never })!;
    expect(odd).toEqual({ number: 3, state: "unknown", review: "unknown" });
    expect(pullRequestLabel(odd)).toBe("Pull request #3: state unknown, review status unknown");
    expect(pullRequestFromDto({ number: 0, url: PR_URL, state: "open" })).toBeUndefined();
    expect(pullRequestFromDto({ number: 1.5, url: PR_URL, state: "open" })).toBeUndefined();
    expect(pullRequestFromDto(undefined)).toBeUndefined();
    for (const bad of ["javascript:alert(1)", "http://github.com/o/r/pull/1", "https://github.com/o/r/issues/1", "https://user@github.com/o/r/pull/1", "https://github.com:8443/o/r/pull/1"]) {
      expect(pullRequestFromDto({ number: 1, url: bad, state: "open" })?.url, bad).toBeUndefined();
    }
  });

  /** Scenario: a badge whose address is not a github.com pull request opens nothing. */
  it("is not a control when its address is not one the app can open", () => {
    const { host } = fakeHost();
    renderShell(host, [agent("7", { number: 5, url: "https://example.com/o/r/pull/5", state: "open" })]);
    const badge = screen.getByTestId("pr-badge-7");
    expect(badge.tagName).toBe("SPAN");
    fireEvent.click(badge);
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(host.open).not.toHaveBeenCalled();
  });
});

describe("PRD #1401 — the in-app pull request browser", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  /**
   * Scenario: on the agent's screen, click the badge. GitHub's page opens in
   * the app over the agent's screen, with the app's toolbar around it, and the
   * agent's pane is still there underneath.
   */
  it("opens over the agent's screen from the badge", async () => {
    const { host } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));

    const browser = screen.getByTestId("pr-browser");
    expect(browser).toHaveAccessibleName("Pull request #12345 for Coder 7");
    expect(host.open).toHaveBeenCalledTimes(1);
    expect(host.open).toHaveBeenCalledWith(PR_URL, expect.objectContaining({ x: expect.any(Number), y: expect.any(Number), width: expect.any(Number), height: expect.any(Number), viewportWidth: expect.any(Number), viewportHeight: expect.any(Number) }));
    for (const name of ["Back", "Open in browser", "Close pull request"]) {
      expect(within(browser).getByRole("button", { name })).toBeVisible();
    }
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    // MutationObserver in the still-mounted pane must have had a chance to
    // notice the new sibling. fireEvent itself does not respect inert.
    await act(async () => { await Promise.resolve(); });
    expect.soft(browser.closest("[inert]") !== null, "The frontmost PR browser must accept pointer and keyboard input").toBe(false);
    for (const name of ["Back", "Open in browser", "Close pull request"]) {
      expect.soft(within(browser).getByRole("button", { name }).closest("[inert]") !== null, `${name} must be reachable`).toBe(false);
    }
    expect(screen.getByTestId("agent-pane-overlay").closest("[inert]")).not.toBeNull();
  });

  /** Scenario: the toolbar's Back steps the page back and leaves it open. */
  it("steps the page back from the toolbar", () => {
    const { host } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(screen.getByRole("button", { name: "Back" }));

    expect(host.back).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("pr-browser")).toBeInTheDocument();
  });

  /** Scenario: Open in browser hands the page to the system browser and closes it in the app. */
  it("hands the page to the system browser and closes", async () => {
    const { host } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Open in browser" })); });

    expect(host.openExternal).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
  });

  /** Scenario: the system browser refuses to open the page; the app keeps the PR and explains the failure so the user can retry. */
  it("keeps the PR open and shows a failed system-browser handoff", async () => {
    const { host } = fakeHost();
    host.openExternal.mockRejectedValueOnce(new Error("System browser launch failed"));
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Open in browser" })); });

    expect.soft(screen.queryByTestId("pr-browser"), "A failed handoff must preserve the page").toBeInTheDocument();
    expect.soft(screen.queryByRole("alert"), "The launch error must be visible").toHaveTextContent("System browser launch failed");
    expect(host.close).not.toHaveBeenCalled();
  });

  /** Scenario: Back cannot navigate the native PR page; the toolbar leaves it open and shows the navigation error. */
  it("surfaces a rejected Back command", async () => {
    const { host } = fakeHost();
    host.back.mockRejectedValueOnce(new Error("PR page cannot go back"));
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Back" })); });
    expect(host.back).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("pr-browser")).toBeInTheDocument();
    expect(screen.queryByRole("alert"), "Back errors must be visible").toHaveTextContent("PR page cannot go back");
  });

  /**
   * Scenario: Close returns to exactly the agent's screen, with focus back on
   * the badge the browser was opened from, and the page is closed.
   */
  it("closes back to the agent's screen and the control that opened it", async () => {
    const { host } = fakeHost();
    renderShell(host);
    const badge = screen.getByTestId("pr-badge-7");
    badge.focus();
    fireEvent.click(badge);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Close pull request" })); });

    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(host.close).toHaveBeenCalled();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    expect(document.activeElement).toBe(screen.getByTestId("pr-badge-7"));
  });

  /**
   * Scenario: Escape in the app closes the browser and ONLY the browser — the
   * agent's pane underneath stays — and a second Escape then closes the pane.
   */
  it("closes on Escape before anything under it", () => {
    const { host } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
  });

  /** Scenario: Escape inside the page closed it; the app forgets it and keeps the agent's pane. */
  it("forgets a page that closed itself", () => {
    const { host, closeFromPage } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    act(() => closeFromPage());

    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
  });

  /** Scenario: the agent ends while its pull request is open; the browser closes with it. */
  it("closes when the agent is gone from a daemon that is answering", () => {
    const { host } = fakeHost();
    const shell = renderShell(host, [withPr, agent("8")]);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    expect(screen.getByTestId("pr-browser")).toBeInTheDocument();

    shell.rerenderWith([agent("8")]);
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(host.close).toHaveBeenCalled();
  });

  /** Scenario: leaving the agent's screen (its own Back to dashboard) closes the browser too. */
  it("closes when the screen it was opened over is left", () => {
    const { host } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Back to dashboard" }));

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
  });

  /**
   * Scenario: a dialog of the app's opens over the browser. The page — drawn
   * by a native webview above the whole document — is hidden while it is up,
   * and Escape is left to that dialog.
   */
  it("hides the page while one of the app's dialogs is over it", async () => {
    const { host } = fakeHost();
    renderShell(host);
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    expect(host.setVisible).toHaveBeenLastCalledWith(true);

    const dialog = document.createElement("div");
    dialog.setAttribute("aria-modal", "true");
    await act(async () => { document.body.appendChild(dialog); });
    expect(host.setVisible).toHaveBeenLastCalledWith(false);
    expect(screen.getByTestId("pr-browser")).toHaveAttribute("data-covered", "true");

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.getByTestId("pr-browser")).toBeInTheDocument();

    await act(async () => { dialog.remove(); });
    expect(host.setVisible).toHaveBeenLastCalledWith(true);
  });
});

describe("PRD #1401 — the pull request browser by voice", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  function renderVoice(host: PrBrowserHost, outcome: VoiceResultDto["outcome"], agents: DesktopAgentDto[] = [withPr]) {
    const voice = voiceRuntime(outcome);
    render(
      <PrBrowserHostContext.Provider value={host}>
        <DeckShell runtime={runtime(agents, voice.overrides)} initialView={agentView} />
      </PrBrowserHostContext.Provider>,
    );
    return voice;
  }

  async function openVoiceHelp(host: PrBrowserHost) {
    const voice = renderVoice(host, dispatchOutcome("list_commands", "showVoiceCommands", "what can I say?", "Here is what you can say."));
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    expect(host.setVisible).toHaveBeenLastCalledWith(true);
    fireEvent.click(screen.getByTestId("voice-trigger"));
    await screen.findByTestId("voice-help");
    return voice;
  }

  /** Scenario: the real What you can say overlay covers the PR, hides the native page, and Escape dismisses only help. */
  it("hides the PR page under real voice help and leaves Escape to help", async () => {
    const { host } = fakeHost();
    await openVoiceHelp(host);
    await act(async () => { await Promise.resolve(); });
    expect.soft(screen.getByTestId("pr-browser")).toHaveAttribute("data-covered", "true");
    expect.soft(host.setVisible).toHaveBeenLastCalledWith(false);
    fireEvent.keyDown(window, { key: "Escape" });
    expect.soft(screen.queryByTestId("voice-help")).not.toBeInTheDocument();
    expect(screen.queryByTestId("pr-browser"), "Escape must preserve the PR underneath voice help").toBeInTheDocument();
  });

  /** Scenario: after asking for voice help over a PR, saying close dismisses the help through the real voice registry while keeping the PR open. */
  it("withholds PR close from the voice registry while real help is up", async () => {
    const { host } = fakeHost();
    const voice = await openVoiceHelp(host);
    act(() => voice.deliver(dispatchOutcome("close", "closeTopmost", "close", "Closed.")));
    await waitFor(() => expect(voice.resolveVoice).toHaveBeenCalledTimes(2));
    await act(async () => { await Promise.resolve(); });
    expect.soft(screen.queryByTestId("voice-help")).not.toBeInTheDocument();
    expect.soft(screen.queryByTestId("pr-browser"), "Voice close must dismiss help and keep the PR").toBeInTheDocument();
    expect(host.close).not.toHaveBeenCalled();
  });

  /** Scenario: a spoken system-browser handoff fails; the PR stays available and voice reports the failure instead of announcing success. */
  it("does not announce a successful voice handoff when the host rejects", async () => {
    const { host } = fakeHost();
    host.openExternal.mockRejectedValueOnce(new Error("System browser launch failed"));
    renderVoice(host, dispatchOutcome("open_pr_in_browser", "openPullRequestInBrowser", "open it in the browser", "Opened it in your browser."));
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(screen.getByTestId("voice-trigger"));
    await waitFor(() => expect(host.openExternal).toHaveBeenCalledTimes(1));
    await act(async () => { await Promise.resolve(); });
    expect.soft(screen.queryByTestId("pr-browser")).toBeInTheDocument();
    expect.soft(screen.queryByText(/Opened it in your browser\./)).not.toBeInTheDocument();
    expect(screen.queryAllByText(/System browser launch failed/).length, "Voice or browser must explain the host failure").toBeGreaterThan(0);
  });

  /** Scenario: a spoken scroll reaches the native PR page but fails; the app keeps the PR and explains the scroll failure. */
  it("surfaces a rejected voice Scroll command", async () => {
    const { host } = fakeHost();
    host.scroll.mockRejectedValueOnce(new Error("PR page cannot scroll"));
    renderVoice(host, dispatchOutcome("scroll_down", "scrollDown", "scroll down", "Scrolling."));
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(screen.getByTestId("voice-trigger"));
    await waitFor(() => expect(host.scroll).toHaveBeenCalledWith("down"));
    await act(async () => { await Promise.resolve(); });
    expect(screen.getByTestId("pr-browser")).toBeInTheDocument();
    expect(screen.queryAllByText(/PR page cannot scroll/).length, "Scroll errors must be visible").toBeGreaterThan(0);
  });

  /** Scenario: on the agent's screen the user says "open the PR"; GitHub's page opens in the app. */
  it("opens the pane's agent's pull request when told to", async () => {
    const { host } = fakeHost();
    const voice = renderVoice(host, dispatchOutcome("open_pr", "openPullRequest", "open the PR", "Opening the pull request."));
    fireEvent.click(screen.getByTestId("voice-trigger"));

    await waitFor(() => expect(screen.getByTestId("pr-browser")).toBeInTheDocument());
    expect(host.open).toHaveBeenCalledWith(PR_URL, expect.any(Object));
    expect(voice.resolveVoice).toHaveBeenCalledTimes(1);
  });

  /** Scenario: "open the PR" for an agent that has none says so, and opens nothing. */
  it("says the agent has no pull request rather than opening one", async () => {
    const { host } = fakeHost();
    renderVoice(host, dispatchOutcome("open_pr", "openPullRequest", "open the PR", "Opening the pull request."), [agent("7")]);
    fireEvent.click(screen.getByTestId("voice-trigger"));

    await waitFor(() => expect(screen.getByText(/Coder 7 has no pull request\./)).toBeInTheDocument());
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
    expect(host.open).not.toHaveBeenCalled();
  });

  /**
   * Scenario: with the pull request open, "scroll down" scrolls the PAGE from
   * the app, and the voice surface declares the browser's own screen.
   */
  it.each([
    ["scroll_down", "scrollDown", "scroll down", "down"],
    ["scroll_up", "scrollUp", "scroll up", "up"],
    ["scroll_to_top", "scrollToTop", "scroll to the top", "top"],
    ["scroll_to_bottom", "scrollToBottom", "scroll to the bottom", "bottom"],
  ] as const)("scrolls the page for %s", async (action, invoke, said, move) => {
    const { host } = fakeHost();
    const voice = renderVoice(host, dispatchOutcome(action, invoke, said, "Scrolling."));
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(screen.getByTestId("voice-trigger"));

    await waitFor(() => expect(host.scroll).toHaveBeenCalledWith(move));
    expect(voice.declareVoiceScreen.mock.calls.some(([declared]) => declared === "pull_request")).toBe(true);
    expect(screen.getByTestId("pr-browser")).toBeInTheDocument();
  });

  /** Scenario: "close" with the pull request open closes it, and only it. */
  it("closes the browser first on close", async () => {
    const { host } = fakeHost();
    renderVoice(host, dispatchOutcome("close", "closeTopmost", "close", "Closed."));
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(screen.getByTestId("voice-trigger"));

    await waitFor(() => expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument());
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
  });

  /** Scenario: "open it in the browser" hands the page to the system browser and closes it. */
  it("hands off to the system browser when told to", async () => {
    const { host } = fakeHost();
    renderVoice(host, dispatchOutcome("open_pr_in_browser", "openPullRequestInBrowser", "open it in the browser", "Opened it in your browser."));
    fireEvent.click(screen.getByTestId("pr-badge-7"));
    fireEvent.click(screen.getByTestId("voice-trigger"));

    await waitFor(() => expect(host.openExternal).toHaveBeenCalledTimes(1));
    expect(screen.queryByTestId("pr-browser")).not.toBeInTheDocument();
  });
});
