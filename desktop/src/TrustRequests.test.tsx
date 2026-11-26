import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import { OVERLAY_LAYER, topmostOverlayClose, useInertBackground } from "./hooks/useInertBackground";
import {
  approveShownReview,
  APPROVE,
  APPROVE_AND_START,
  ASKED_BY_LABEL,
  DENY,
  NO_APPROVAL_WAITING,
  REQUEST_INTRO,
  REQUEST_KEYS_ARM_MS,
  requestAnsweredElsewhereNotice,
  requestApprovedNotice,
  requestDeniedNotice,
  shownReviewToken,
  TRUST_TITLE,
  type ProjectTrust,
  type ProjectTrustTarget,
} from "./lib/projectTrust";
import { pendingTrustRequests, showHeldTrustRequest, TRUST_REQUEST_RETRY_MS, type TrustRequestRuntime } from "./lib/trustRequests";
import { VOICE_ACTIONS } from "./lib/voiceActions";
import { TrustRequestGate } from "./components/TrustRequestGate";
import type { DeckFleet, DeckRuntimeState, DeckSnapshot, TrustRequest } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

/**
 * Issue #2330 — a deck refused to start an orchestration that is not approved
 * as its config now reads, and lists a request; the app shows the review for
 * it, names the deck that asked, and Approve or Deny answers that deck.
 */

const LOCAL = "deck-local";
const REMOTE = "deck-remote";

function request(id: number, overrides: Partial<TrustRequest> = {}): TrustRequest {
  return { id, project: "/srv/demo", configDir: "/srv/demo", orchestration: "team", digest: "ab12", askedBy: "dispatch", ...overrides };
}

/** A connected deck listing `requests`: the local deck, or a remote one at `deploy@build-box`. */
function deck(deckId: string, requests: TrustRequest[], incarnation = "process-a"): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  const remote = deckId === REMOTE;
  return {
    ...snapshot,
    connection: {
      ...snapshot.connection,
      status: "connected",
      deckId,
      socketPath: remote ? "deploy@build-box" : "/tmp/attach.sock",
      deckKind: remote ? "remote" : "local",
      incarnation,
      ...(requests.length ? { trustRequests: requests } : {}),
    },
  };
}

/** The deck's answer for an orchestration: not approved before, with one role. */
function review(target: ProjectTrustTarget, state: "never_approved" | "approved" = "never_approved"): ProjectTrust {
  return { kind: "review", project: target.path, orchestration: target.orchestration, state, roles: [{ name: "coder", command: "claude", start: true, clear: false }], removedRoles: [], digest: "ab12" };
}

function runtimeOf(overrides: Partial<TrustRequestRuntime> = {}) {
  return {
    projectTrustState: vi.fn(async (target: ProjectTrustTarget) => review(target)),
    approveProjectTrust: vi.fn(async (target: ProjectTrustTarget) => review(target, "approved")),
    denyTrustRequest: vi.fn(async () => undefined),
    ...overrides,
  };
}

/** An overlay already open when the request arrives, standing in for any dialog the user is in. */
function Other() {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.dialog);
  return <div ref={ref} data-testid="other">other</div>;
}

function Gate({ runtime, fleet, other = false, ready = true, summonable = true, onNotice }: { runtime: TrustRequestRuntime; fleet: DeckFleet; other?: boolean; ready?: boolean; summonable?: boolean; onNotice: (notice: { message: string }) => void }) {
  return <>
    {other && <Other />}
    <TrustRequestGate runtime={runtime} fleet={fleet} ready={ready} summonable={summonable} onNotice={onNotice} />
  </>;
}

/** Wait until the dialog answers keys (`REQUEST_KEYS_ARM_MS` after it is drawn). */
async function keysReady(dialog: HTMLElement): Promise<void> {
  await waitFor(() => expect(dialog).toHaveAttribute("data-keys-ready", "true"), { timeout: REQUEST_KEYS_ARM_MS * 5 });
}

describe("an in-deck approval request", () => {
  /** Scenario: the remote deck, which is not the selected one, lists a request while nothing is open. Its review opens naming that deck and what asked, with the TUI's sentence, and its buttons are Deny and Approve. */
  it("opens the review when nothing is open, naming the deck that asked", async () => {
    const runtime = runtimeOf();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, []), deck(REMOTE, [request(3, { askedBy: "schedule nightly", configDir: "/srv/demo-wt" })])]} onNotice={vi.fn()} />);
    const dialog = await screen.findByRole("alertdialog", { name: TRUST_TITLE });
    expect(runtime.projectTrustState).toHaveBeenCalledExactlyOnceWith({ deckId: REMOTE, path: "/srv/demo-wt", orchestration: "team" });
    expect(within(dialog).getByTestId("project-trust-deck")).toHaveTextContent("deploy@build-box");
    expect(within(dialog).getByTestId("project-trust-asked-by")).toHaveTextContent("schedule nightly");
    expect(dialog).toHaveTextContent(`${ASKED_BY_LABEL}:`);
    expect(within(dialog).getByTestId("project-trust-request-intro")).toHaveTextContent(REQUEST_INTRO);
    expect(within(dialog).getByRole("button", { name: DENY })).toBeVisible();
    expect(within(dialog).getByRole("button", { name: APPROVE })).toBeVisible();
    expect(within(dialog).queryByRole("button", { name: APPROVE_AND_START })).toBeNull();
    expect(within(dialog).queryByRole("button", { name: "Cancel" })).toBeNull();
  });

  /** Scenario: a request arrives while another dialog is open. No review opens over it, and a marker says "1 approval waiting"; once that dialog closes, the review opens by itself and the marker goes. */
  it("waits while another overlay is open, saying so", async () => {
    const runtime = runtimeOf();
    const fleet = [deck(LOCAL, [request(1)])];
    const { rerender } = render(<Gate runtime={runtime} fleet={fleet} other onNotice={vi.fn()} />);
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
    expect(runtime.projectTrustState).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: /^1 approval waiting/ })).toBeVisible();
    rerender(<Gate runtime={runtime} fleet={fleet} onNotice={vi.fn()} />);
    expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
    expect(screen.queryByTestId("trust-waiting-marker")).toBeNull();
  });

  /** Scenario: two decks each ask for an approval while a dialog is open. The marker says "2 approvals waiting"; clicking it opens the first review over the dialog, and once that is answered the marker says "1 approval waiting" for the other. */
  it("counts the waiting requests, and opens them over the dialog one at a time", async () => {
    const runtime = runtimeOf();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)]), deck(REMOTE, [request(2, { orchestration: "other", digest: "cd34" })])]} other onNotice={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: /^2 approvals waiting/ }));
    const dialog = await screen.findByTestId("project-trust-dialog");
    expect(screen.getByTestId("other")).toBeInTheDocument();
    expect(screen.queryByTestId("trust-waiting-marker")).toBeNull();
    fireEvent.click(within(dialog).getByRole("button", { name: DENY }));
    await waitFor(() => expect(runtime.denyTrustRequest).toHaveBeenCalledExactlyOnceWith(LOCAL, 1, "ab12"));
    expect(await screen.findByRole("button", { name: /^1 approval waiting/ })).toBeVisible();
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
  });

  /** Scenario: a request arrives while the shell is not ready for a review (the connection banner is up, say) and nothing is open. The marker says so, and clicking it opens the review. */
  it("says an approval is waiting while the shell holds the review back", async () => {
    const runtime = runtimeOf();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)])]} ready={false} onNotice={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: /^1 approval waiting/ }));
    expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
  });

  /** Scenario: the user clicks the marker while voice's numbered choice is drawn over everything. No review opens beneath the choice and the marker stays; once the choice closes, the review opens. */
  it("waits for voice's numbered choice before opening a review asked for", async () => {
    const runtime = runtimeOf();
    const fleet = [deck(LOCAL, [request(1)])];
    const { rerender } = render(<Gate runtime={runtime} fleet={fleet} other summonable={false} onNotice={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: /^1 approval waiting/ }));
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
    expect(runtime.projectTrustState).not.toHaveBeenCalled();
    expect(screen.getByTestId("trust-waiting-marker")).toBeVisible();
    rerender(<Gate runtime={runtime} fleet={fleet} other onNotice={vi.fn()} />);
    expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
  });

  /** Scenario: the user clicks the marker, but the deck does not answer when the app reads the review. The app says why, the marker comes back, and the retry after the back-off does not open the review over the dialog unasked. */
  it("asks once per click: a review that could not be read brings the marker back", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let fail = true;
      const runtime = runtimeOf({
        projectTrustState: vi.fn(async (target: ProjectTrustTarget) => {
          if (fail) throw new Error("timed out");
          return review(target);
        }),
      });
      const onNotice = vi.fn();
      render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)])]} other onNotice={onNotice} />);
      fireEvent.click(await screen.findByRole("button", { name: /^1 approval waiting/ }));
      await waitFor(() => expect(onNotice).toHaveBeenCalledOnce());
      expect(await screen.findByRole("button", { name: /^1 approval waiting/ })).toBeVisible();
      fail = false;
      await act(async () => { vi.advanceTimersByTime(TRUST_REQUEST_RETRY_MS * 2); });
      await act(async () => { await Promise.resolve(); });
      expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
      expect(runtime.projectTrustState).toHaveBeenCalledOnce();
      fireEvent.click(screen.getByRole("button", { name: /^1 approval waiting/ }));
      expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
    } finally {
      vi.useRealTimers();
    }
  });

  /** Scenario: with a request held behind a dialog, the user says "show the approval": the review opens over the dialog. With none waiting, it answers that no approval is waiting. */
  it("is reachable by voice while held: show the approval opens it", async () => {
    expect(showHeldTrustRequest()).toBe(NO_APPROVAL_WAITING);
    const runtime = runtimeOf();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)])]} other onNotice={vi.fn()} />);
    await screen.findByTestId("trust-waiting-marker");
    const reportRefused = vi.fn();
    act(() => { VOICE_ACTIONS.showApprovalRequest.run({ showApprovalRequest: showHeldTrustRequest, reportRefused }); });
    expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
    expect(reportRefused).not.toHaveBeenCalled();
    expect(VOICE_ACTIONS.showApprovalRequest.voice).toBe(true);
  });

  /** Scenario: the user approves a request. The approval carries the digest of the text shown, nothing is started, the review closes with the TUI's sentence, and a later snapshot still listing the request opens no second review. */
  it("approves, starts nothing, and opens no second review for the same request", async () => {
    const runtime = runtimeOf();
    const onNotice = vi.fn();
    const fleet = [deck(LOCAL, [request(5, { askedBy: "pane restart" })])];
    const { rerender } = render(<Gate runtime={runtime} fleet={fleet} onNotice={onNotice} />);
    fireEvent.click(within(await screen.findByTestId("project-trust-dialog")).getByRole("button", { name: APPROVE }));
    await waitFor(() => expect(screen.queryByTestId("project-trust-dialog")).toBeNull());
    expect(runtime.approveProjectTrust).toHaveBeenCalledExactlyOnceWith({ deckId: LOCAL, path: "/srv/demo", orchestration: "team" }, "ab12");
    expect(onNotice).toHaveBeenCalledExactlyOnceWith({ message: requestApprovedNotice("team", "pane restart") });
    expect(runtime.denyTrustRequest).not.toHaveBeenCalled();
    rerender(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(5, { askedBy: "pane restart" })])]} onNotice={onNotice} />);
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
    expect(runtime.projectTrustState).toHaveBeenCalledOnce();
  });

  /** Scenario: the user clicks Deny. The deck that asked is told, by the request's number and the digest of its text, and the review closes with the TUI's sentence. */
  it("denies on Deny", async () => {
    const runtime = runtimeOf();
    const onNotice = vi.fn();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, []), deck(REMOTE, [request(9)])]} onNotice={onNotice} />);
    fireEvent.click(within(await screen.findByTestId("project-trust-dialog")).getByRole("button", { name: DENY }));
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
    await waitFor(() => expect(onNotice).toHaveBeenCalledExactlyOnceWith({ message: requestDeniedNotice("team") }));
    expect(runtime.denyTrustRequest).toHaveBeenCalledExactlyOnceWith(REMOTE, 9, "ab12");
    expect(runtime.approveProjectTrust).not.toHaveBeenCalled();
  });

  /** Scenario: the review opens while the user is typing, and the next key is Esc, then a held Esc repeats. Neither declines it; once it has been on screen a moment, Esc does. Enter never approves. */
  it("ignores Esc as it opens and a held key's repeats, then Esc denies", async () => {
    const runtime = runtimeOf();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(2)])]} onNotice={vi.fn()} />);
    const dialog = await screen.findByTestId("project-trust-dialog");
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(runtime.denyTrustRequest).not.toHaveBeenCalled();
    await keysReady(dialog);
    fireEvent.keyDown(dialog, { key: "Escape", repeat: true });
    fireEvent.keyDown(dialog, { key: "Enter" });
    expect(runtime.denyTrustRequest).not.toHaveBeenCalled();
    expect(runtime.approveProjectTrust).not.toHaveBeenCalled();
    fireEvent.keyDown(dialog, { key: "Escape" });
    await waitFor(() => expect(runtime.denyTrustRequest).toHaveBeenCalledExactlyOnceWith(LOCAL, 2, "ab12"));
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
  });

  /** Scenario: while the review is open the request leaves its deck's list (answered in the TUI). The review closes and the app says it was answered elsewhere; nothing is sent to the deck. */
  it("closes when the request is answered elsewhere", async () => {
    const runtime = runtimeOf();
    const onNotice = vi.fn();
    const { rerender } = render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(4)])]} onNotice={onNotice} />);
    await screen.findByTestId("project-trust-dialog");
    rerender(<Gate runtime={runtime} fleet={[deck(LOCAL, [])]} onNotice={onNotice} />);
    await waitFor(() => expect(screen.queryByTestId("project-trust-dialog")).toBeNull());
    expect(onNotice).toHaveBeenCalledExactlyOnceWith({ message: requestAnsweredElsewhereNotice("team") });
    expect(runtime.denyTrustRequest).not.toHaveBeenCalled();
    expect(runtime.approveProjectTrust).not.toHaveBeenCalled();
  });

  /** Scenario: the deck disconnects while the review is open, then reconnects with a list that no longer has the request. The review stays while the deck is away, and closes as answered elsewhere when it is back. */
  it("closes when its deck reconnects without the request", async () => {
    const runtime = runtimeOf();
    const onNotice = vi.fn();
    const { rerender } = render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(4)])]} onNotice={onNotice} />);
    await screen.findByTestId("project-trust-dialog");
    const away = deck(LOCAL, []);
    away.connection = { ...away.connection, status: "disconnected" };
    rerender(<Gate runtime={runtime} fleet={[away]} onNotice={onNotice} />);
    await act(async () => { await Promise.resolve(); });
    expect(screen.getByTestId("project-trust-dialog")).toBeVisible();
    rerender(<Gate runtime={runtime} fleet={[deck(LOCAL, [], "process-b")]} onNotice={onNotice} />);
    await waitFor(() => expect(screen.queryByTestId("project-trust-dialog")).toBeNull());
    expect(onNotice).toHaveBeenCalledExactlyOnceWith({ message: requestAnsweredElsewhereNotice("team") });
  });

  /** Scenario: the deck lists a request for an orchestration that is approved by the time the app asks. No review opens, and the next request opens instead. */
  it("opens nothing for a request the deck answers approved", async () => {
    const runtime = runtimeOf({
      projectTrustState: vi.fn(async (target: ProjectTrustTarget) => review(target, target.orchestration === "solo" ? "approved" : "never_approved")),
    });
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1, { orchestration: "solo" }), request(2, { orchestration: "team", digest: "cd34" })])]} onNotice={vi.fn()} />);
    const dialog = await screen.findByTestId("project-trust-dialog");
    expect(within(dialog).getByTestId("project-trust-orchestration")).toHaveTextContent("team");
    expect(runtime.projectTrustState).toHaveBeenCalledTimes(2);
  });

  /** Scenario: with a request's review open, the user says "approve project commands", then on the next request "cancel". The first approves exactly as the button does; the second is voice's close, which denies. */
  it("is reachable by voice: approve approves, close denies", async () => {
    const runtime = runtimeOf();
    const onNotice = vi.fn();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1), request(2, { orchestration: "other", digest: "cd34" })])]} onNotice={onNotice} />);
    await screen.findByTestId("project-trust-dialog");
    act(() => { expect(approveShownReview(shownReviewToken())).toBeUndefined(); });
    await waitFor(() => expect(runtime.approveProjectTrust).toHaveBeenCalledExactlyOnceWith({ deckId: LOCAL, path: "/srv/demo", orchestration: "team" }, "ab12"));
    await waitFor(() => expect(screen.getByTestId("project-trust-orchestration")).toHaveTextContent("other"));
    act(() => { topmostOverlayClose()?.(); });
    await waitFor(() => expect(runtime.denyTrustRequest).toHaveBeenCalledExactlyOnceWith(LOCAL, 2, "cd34"));
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
  });

  /** Scenario: a deck that is not connected, and a runtime that cannot decline, show no request. */
  it("shows nothing from a deck that is not connected, or without a way to decline", async () => {
    const away = deck(LOCAL, [request(1)]);
    away.connection = { ...away.connection, status: "disconnected" };
    expect(pendingTrustRequests([away])).toEqual([]);
    const { denyTrustRequest: _deny, ...cannotDeny } = runtimeOf();
    render(<Gate runtime={cannotDeny} fleet={[deck(LOCAL, [request(1)])]} onNotice={vi.fn()} />);
    await act(async () => { await Promise.resolve(); });
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
    expect(cannotDeny.projectTrustState).not.toHaveBeenCalled();
  });

  /** Scenario: the deck's daemon restarted and raised the same request again after the user had declined it. The app shows it again. */
  it("shows a restarted deck's request again", async () => {
    const runtime = runtimeOf();
    const { rerender } = render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)])]} onNotice={vi.fn()} />);
    fireEvent.click(within(await screen.findByTestId("project-trust-dialog")).getByRole("button", { name: DENY }));
    await waitFor(() => expect(runtime.denyTrustRequest).toHaveBeenCalledOnce());
    rerender(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)], "process-b")]} onNotice={vi.fn()} />);
    expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
  });

  /** Scenario: the deck does not answer when the app reads a request's review. No review opens and the app says why once; it does not ask again at once, and asks again after the back-off, when the review opens. */
  it("asks again after a back-off when the review cannot be read", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let fail = true;
      const runtime = runtimeOf({
        projectTrustState: vi.fn(async (target: ProjectTrustTarget) => {
          if (fail) throw new Error("timed out");
          return review(target);
        }),
      });
      const onNotice = vi.fn();
      const fleet = [deck(LOCAL, [request(1)])];
      const { rerender } = render(<Gate runtime={runtime} fleet={fleet} onNotice={onNotice} />);
      await waitFor(() => expect(onNotice).toHaveBeenCalledOnce());
      expect(onNotice.mock.calls[0]?.[0].message).toContain("timed out");
      expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
      // A later snapshot inside the back-off asks nothing.
      rerender(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1)])]} onNotice={onNotice} />);
      await act(async () => { await Promise.resolve(); });
      expect(runtime.projectTrustState).toHaveBeenCalledOnce();
      // A second failure after the back-off is not said again.
      await act(async () => { vi.advanceTimersByTime(TRUST_REQUEST_RETRY_MS); });
      await waitFor(() => expect(runtime.projectTrustState).toHaveBeenCalledTimes(2));
      expect(onNotice).toHaveBeenCalledOnce();
      fail = false;
      await act(async () => { vi.advanceTimersByTime(TRUST_REQUEST_RETRY_MS); });
      expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
      expect(runtime.projectTrustState).toHaveBeenCalledTimes(3);
    } finally {
      vi.useRealTimers();
    }
  });

  /** Scenario: a request names an orchestration with a bidi override in its name and directory. The review is read with the name and directory exactly as the deck sent them, and the approved sentence names the orchestration as the review shows it, escaped. */
  it("opens the review with the request's identifiers verbatim and names the reviewed orchestration", async () => {
    const shownName = "te\\u{202e}am";
    const runtime = runtimeOf({
      projectTrustState: vi.fn(async (target: ProjectTrustTarget) => ({ ...review(target), orchestration: shownName })),
      approveProjectTrust: vi.fn(async (target: ProjectTrustTarget) => ({ ...review(target, "approved"), orchestration: shownName })),
    });
    const onNotice = vi.fn();
    render(<Gate runtime={runtime} fleet={[deck(LOCAL, [request(1, { orchestration: "te\u202eam", configDir: "/srv/de\u202emo" })])]} onNotice={onNotice} />);
    const dialog = await screen.findByTestId("project-trust-dialog");
    expect(runtime.projectTrustState).toHaveBeenCalledExactlyOnceWith({ deckId: LOCAL, path: "/srv/de\u202emo", orchestration: "te\u202eam" });
    expect(within(dialog).getByTestId("project-trust-orchestration")).toHaveTextContent(shownName);
    fireEvent.click(within(dialog).getByRole("button", { name: APPROVE }));
    await waitFor(() => expect(onNotice).toHaveBeenCalledExactlyOnceWith({ message: requestApprovedNotice(shownName, "dispatch") }));
  });
});

describe("an in-deck approval request in the app", () => {
  function shellRuntime(overrides: Partial<DeckRuntimeState>): DeckRuntimeState {
    const local = deck(LOCAL, []);
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
      snapshot: local,
      fleet: [local, deck(REMOTE, [request(7, { askedBy: "dispatch" })])],
      ...runtimeOf(),
      ...overrides,
    } as unknown as DeckRuntimeState;
  }

  /** Scenario: under All decks the app shows the dashboard while the remote deck lists a request. The review opens over the dashboard naming the remote deck; Approve sends the approval and the toast says the attempt starts when it is retried. No agent is started. */
  it("opens over the dashboard and leaves the approved sentence in the toast", async () => {
    const runtime = shellRuntime({});
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    const dialog = await screen.findByTestId("project-trust-dialog");
    expect(within(dialog).getByTestId("project-trust-deck")).toHaveTextContent("deploy@build-box");
    fireEvent.click(within(dialog).getByRole("button", { name: APPROVE }));
    const toast = await screen.findByTestId("trust-request-toast");
    expect(toast).toHaveTextContent(requestApprovedNotice("team", "dispatch"));
    expect(runtime.runAction).not.toHaveBeenCalled();
    fireEvent.click(within(toast).getByRole("button", { name: "Dismiss approval request message" }));
    expect(screen.queryByTestId("trust-request-toast")).toBeNull();
  });

  /** Scenario: on the deck screen the user denies a request; the toast says it was declined. */
  it("leaves the declined sentence in the deck screen's toast", async () => {
    const runtime = shellRuntime({});
    render(<DeckShell runtime={runtime} initialView={{ kind: "deck" }} />);
    fireEvent.click(within(await screen.findByTestId("project-trust-dialog")).getByRole("button", { name: DENY }));
    expect(await screen.findByTestId("trust-request-toast")).toHaveTextContent(requestDeniedNotice("team"));
    expect(runtime.denyTrustRequest).toHaveBeenCalledExactlyOnceWith(REMOTE, 7, "ab12");
  });
});

describe("an in-deck approval request while the user is in something", () => {
  function paneRuntime(): DeckRuntimeState {
    const listed = deck(LOCAL, []);
    /* The fixture's agents, as this deck's own, so the pane can open on one. */
    const local = { ...listed, agents: listed.agents.map((agent) => ({ ...agent, daemonId: LOCAL })) };
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
      snapshot: local,
      fleet: [local, deck(REMOTE, [request(7, { askedBy: "dispatch" })])],
      ...runtimeOf(),
    } as unknown as DeckRuntimeState;
  }
  const PANE = { kind: "agent", deckId: LOCAL, agentId: "planner", from: "deck" } as const;

  /** Scenario: the user is in an agent's pane when a deck asks for an approval. The pane stays as it is, and a marker saying "1 approval waiting" is on screen; clicking it opens the review over the pane, naming the deck that asked. */
  it("says an approval is waiting while an agent's pane is open, and opens the review when clicked", async () => {
    render(<DeckShell runtime={paneRuntime()} initialView={PANE} />);
    expect(await screen.findByTestId("agent-pane-overlay")).toBeVisible();
    const marker = await screen.findByRole("button", { name: /1 approval waiting/ });
    expect(marker).toBeVisible();
    expect(marker).not.toHaveAttribute("inert");
    expect(marker.closest("[inert]")).toBeNull();
    expect(screen.queryByTestId("project-trust-dialog")).toBeNull();
    fireEvent.click(marker);
    const dialog = await screen.findByTestId("project-trust-dialog");
    expect(within(dialog).getByTestId("project-trust-deck")).toHaveTextContent("deploy@build-box");
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /approval waiting/ })).toBeNull();
  });

  /** Scenario: a keyboard user moves focus to the marker over an agent's pane, and the app renders again (a new snapshot). Focus stays on the marker rather than being pulled back into the pane. */
  it("keeps focus on the marker when the pane renders again", async () => {
    const rt = paneRuntime();
    const { rerender } = render(<DeckShell runtime={rt} initialView={PANE} />);
    const marker = await screen.findByRole("button", { name: /1 approval waiting/ });
    act(() => { marker.focus(); });
    expect(document.activeElement).toBe(marker);
    rerender(<DeckShell runtime={{ ...rt, fleet: [...rt.fleet] }} initialView={PANE} />);
    await act(async () => { await Promise.resolve(); });
    expect(document.activeElement).toBe(screen.getByRole("button", { name: /1 approval waiting/ }));
  });

  /** Scenario: a deck asks for an approval while the user is in an agent's pane, and the user goes back to the dashboard without touching the marker. The review opens by itself. */
  it("opens the held review by itself once the pane closes", async () => {
    render(<DeckShell runtime={paneRuntime()} initialView={PANE} />);
    await screen.findByRole("button", { name: /1 approval waiting/ });
    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Back to dashboard" }));
    expect(await screen.findByTestId("project-trust-dialog")).toBeVisible();
    expect(screen.queryByRole("button", { name: /approval waiting/ })).toBeNull();
  });
});
