import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { StrictMode } from "react";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DeckBridge, type DesktopSettingsDto } from "../lib/bridge";
import { AGENTS_STOPPED_BY_RESTART } from "../lib/agentIdentity";
import {
  DISMISSED, INTERRUPTED_UNREAD, INTERRUPTED_WORK_TITLE, LIST_INCOMPLETE, LIST_REFRESH_FAILED, NOTHING_INTERRUPTED, OTHER_AGENTS_GROUP, RECLAIM_KEPT_DIRTY, RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE, NO_AGENT_TO_RESTORE, RESTORE_UNSUPPORTED, INTERRUPTED_WAIT, REFUSED_AMBIGUOUS, REFUSED_INCOMPLETE_READ, REFUSED_NOT_LISTED, dismissAllQuestion, gitFactsLine, interruptedFooter, reportRangeSentence,
  type DismissOutcome, type InterruptedItem, type InterruptedList, type InterruptedReply, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome, type RestoreMode, type RestoreOutcome, type StartOverOutcome,
} from "../lib/interruptedWork";
import { VOICE_ACTIONS, INTERRUPTED_RECLAIM_CHECKING, type VoiceOverviewChannel, type VoiceOverviewContext } from "../lib/voiceActions";
import { useInterruptedWork } from "../hooks/useInterruptedWork";
import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { AgentSession, DeckRuntimeState, DeckSnapshot } from "../types";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { AgentOverview } from "./AgentOverview";
import { DeckShell } from "../App";

/**
 * PRD #1790 stage 2 — the desktop's interrupted work, per deck: the banner
 * on the deck's dashboard section, the "Interrupted work" dialog, and the
 * restart notice's hint line. The same fields, words, confirms and refusals
 * as the TUI's (CLAUDE.md rule 22); the real `useInterruptedWork` hook runs
 * against a recording bridge.
 */

const LAST_TS = 1_795_082_400_000;
const KEPT_TS = 1_795_687_200_000;
const UNIT_ID = "w-abcdefgh-0000000000000001";
const TEAM_ID = "w-abcdefgh-0000000000000002";
const AGENT_ID = "w-abcdefgh-0000000000000003";
const NEW_ID = "w-abcdefgh-0000000000000004";

function item(id: string, kind: InterruptedItem["kind"], name: string, extra: Partial<InterruptedItem> = {}): InterruptedItem {
  return { id, kind, name, last_entry: { kind: "attempt_ended", ts_ms: LAST_TS }, reports: [], progress: { attempts: 1, delegations: 0, delegations_done: 0 }, ...extra };
}
const unit = () => item(UNIT_ID, "unit", "fix-login", { branch: "agent/dispatch-fix-login", worktree: "/home/me/code/app-dispatch-fix-login", launcher: "dispatcher", progress: { attempts: 1, roles: ["coder"], delegations: 0, delegations_done: 0 }, reports: [{ report_id: "r-fix-login-done", kept_until_ms: KEPT_TS }] });
const team = () => item(TEAM_ID, "orchestration", "release-team", { worktree: "/home/me/code/app", progress: { attempts: 3, roles: ["orchestrator", "coder", "reviewer"], delegations: 4, delegations_done: 2 } });
const agent = () => item(AGENT_ID, "agent", "scratch-shell", { worktree: "/home/me/notes" });
/** Most recent first, as the daemon answers: the plain agent first, so the grouping is the app's doing. */
const three = (): InterruptedList => ({ items: [agent(), unit(), team()] });

const answered = <T,>(target: InterruptedTarget, answer: T): InterruptedReply<T> => ({ outcome: "answered", instance: target.instance, answer });

/** A bridge that records every interrupted work request and answers from `state`. */
function recordingBridge(state: { list: InterruptedList | "unsupported"; dry?: ReclaimOutcome; show?: (id: string, from: number) => InterruptedShow }) {
  const calls: { verb: string; target: InterruptedTarget; args: unknown[] }[] = [];
  const bridge = {
    interruptedWorkList: vi.fn(async (target: InterruptedTarget, git: boolean) => {
      calls.push({ verb: "list", target, args: [git] });
      return state.list === "unsupported" ? { outcome: "unsupported" as const } : answered(target, structuredClone(state.list));
    }),
    interruptedWorkShow: vi.fn(async (target: InterruptedTarget, id: string, from: number) => {
      calls.push({ verb: "show", target, args: [id, from] });
      const listed = state.list === "unsupported" ? undefined : state.list.items.find((candidate) => candidate.id === id);
      return answered(target, state.show ? state.show(id, from) : { item: listed!, reports: [{ report_id: "r-fix-login-done", body: "[UNTRUSTED-WORKER-REPORT:\nall ‮ done\u001b[31m\n" }], reports_from: from });
    }),
    interruptedWorkReclaim: vi.fn(async (target: InterruptedTarget, id: string, dryRun: boolean) => {
      calls.push({ verb: "reclaim", target, args: [id, dryRun] });
      if (dryRun) return answered(target, state.dry ?? { id, dry_run: true, verdict: "would-remove", message: RECLAIM_WOULD_REMOVE, worktree: unit().worktree, branch: unit().branch, unpushed_commits: 2 });
      if (state.list !== "unsupported") state.list = { items: state.list.items.filter((candidate) => candidate.id !== id) };
      return answered(target, { id, dry_run: false, verdict: "removed", message: RECLAIM_REMOVED, recorded: true });
    }),
    interruptedWorkDismiss: vi.fn(async (target: InterruptedTarget, ids: string[]) => {
      calls.push({ verb: "dismiss", target, args: [ids] });
      const outcome: DismissOutcome = { dismissed: ids.map((id) => ({ id })), refused: [] };
      if (state.list !== "unsupported") state.list = { items: state.list.items.filter((candidate) => !ids.includes(candidate.id)) };
      return answered(target, outcome);
    }),
  };
  return { bridge: bridge as unknown as DeckBridge, calls };
}

function deckOf(process: string, agents: (agents: AgentSession[]) => AgentSession[] = (all) => all): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  return { ...snapshot, connection: { ...snapshot.connection, incarnation: process }, agents: agents(snapshot.agents).map((one) => ({ ...one, incarnation: process })) };
}

function baseRuntime(deck: DeckSnapshot): DeckRuntimeState {
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
  } as unknown as DeckRuntimeState;
}

/** The dashboard, with the real hook over `bridge`. */
function Overview({ bridge, deck }: { bridge: DeckBridge; deck: DeckSnapshot }) {
  const interruptedWork = useInterruptedWork(bridge, [deck]);
  return <AgentOverview runtime={{ ...baseRuntime(deck), ...(interruptedWork ? { interruptedWork } : {}) }} onNavigate={vi.fn()} />;
}

/** The whole app, for the restart notice. */
function Shell({ bridge, deck }: { bridge: DeckBridge; deck: DeckSnapshot }) {
  const interruptedWork = useInterruptedWork(bridge, [deck]);
  return <DeckShell runtime={{ ...baseRuntime(deck), ...(interruptedWork ? { interruptedWork } : {}) }} initialView={{ kind: "overview" }} />;
}

async function openDialog() {
  const show = await screen.findByTestId("interrupted-banner-show");
  await act(async () => { fireEvent.click(show); });
  const dialog = screen.getByTestId("interrupted-dialog");
  await waitFor(() => expect(within(dialog).queryAllByTestId("interrupted-row").length).toBeGreaterThan(0));
  return dialog;
}

function rows() {
  return screen.getAllByTestId("interrupted-row");
}

function rowOf(id: string) {
  return rows().find((row) => row.getAttribute("data-item-id") === id)!;
}

beforeEach(() => {
  window.history.replaceState({}, "", "/?fixture=1&experimental=1");
  window.localStorage.clear();
  window.sessionStorage.clear();
});

describe("the interrupted work banner", () => {
  /** Scenario: a deck whose daemon lists three items shows "3 interrupted items from before the restart" on its section; one listing a single item reads it singular, and items the daemon left out are counted. */
  it("counts what the daemon listed", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    expect(await screen.findByTestId("interrupted-banner-message")).toHaveTextContent(interruptedFooter(3));
    expect(calls).toEqual([{ verb: "list", target: { deckId: deckOf("process-a").connection.deckId!, instance: "process-a" }, args: [false] }]);

    const single = recordingBridge({ list: { items: [unit()], omitted: 2 } });
    const { unmount } = render(<Overview bridge={single.bridge} deck={deckOf("process-a")} />);
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner-message")[1]).toHaveTextContent("3 interrupted items from before the restart"));
    unmount();
    const one = recordingBridge({ list: { items: [unit()] } });
    render(<Overview bridge={one.bridge} deck={deckOf("process-z")} />);
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner-message").map((banner) => banner.textContent)).toContain("1 interrupted item from before the restart"));
  });

  /** Scenario: Show opens the "Interrupted work" dialog and asks the daemon for the list again, with the Git facts. Hide removes the banner, sends nothing, and the banner is back once the deck's connection names another daemon process. */
  it("shows the dialog and hides until another daemon process", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const { rerender } = render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    expect(within(dialog).getByRole("heading", { name: INTERRUPTED_WORK_TITLE })).toBeInTheDocument();
    await waitFor(() => expect(calls.at(-1)).toMatchObject({ verb: "list", args: [true] }));
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-close")); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();

    const before = calls.length;
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-banner-hide")); });
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();
    expect(calls.length).toBe(before);
    rerender(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();

    await act(async () => { rerender(<Overview bridge={bridge} deck={deckOf("process-b")} />); });
    expect(await screen.findByTestId("interrupted-banner")).toHaveTextContent(interruptedFooter(3));
    expect(calls.at(-1)).toMatchObject({ verb: "list", target: { instance: "process-b" }, args: [false] });
  });

  /** Scenario: a daemon that does not support interrupted work, and a bridge that offers none, draw nothing on the deck's section: no banner, no error. */
  it("draws nothing for an unsupported daemon", async () => {
    const { bridge, calls } = recordingBridge({ list: "unsupported" });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();
    expect(screen.queryByText(/interrupted work/i)).toBeNull();

    const { container } = render(<Overview bridge={{} as DeckBridge} deck={deckOf("process-a")} />);
    expect(within(container).queryByTestId("interrupted-banner")).toBeNull();
  });

  /** Scenario: a list request made of process A answers after the deck reconnected to process B; process A's items are not shown. */
  it("drops an answer from a daemon process the deck has left", async () => {
    let release: (() => void) | undefined;
    const { bridge } = recordingBridge({ list: three() });
    const slow = { ...bridge, interruptedWorkList: vi.fn(async (target: InterruptedTarget) => {
      if (target.instance === "process-a") await new Promise<void>((done) => { release = done; });
      return target.instance === "process-a" ? answered(target, three()) : answered(target, { items: [] });
    }) } as unknown as DeckBridge;
    const { rerender } = render(<Overview bridge={slow} deck={deckOf("process-a")} />);
    await act(async () => { rerender(<Overview bridge={slow} deck={deckOf("process-b")} />); });
    await act(async () => { release?.(); });
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();
  });
});

describe("the Interrupted work dialog", () => {
  /** Scenario: the list shows the unit and the orchestration first, then the plain agent under "Other agents", each row with its name, kind, branch, directory, who started it, its last entry and its progress. */
  it("groups work before other agents, with the TUI's fields", async () => {
    const { bridge } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    expect(rows().map((row) => row.getAttribute("data-item-id"))).toEqual([UNIT_ID, TEAM_ID, AGENT_ID]);
    const heading = within(dialog).getByTestId("interrupted-other-agents");
    expect(heading).toHaveTextContent(OTHER_AGENTS_GROUP);
    expect(heading.compareDocumentPosition(rowOf(TEAM_ID)) & Node.DOCUMENT_POSITION_PRECEDING).toBeTruthy();
    expect(heading.compareDocumentPosition(rowOf(AGENT_ID)) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(rowOf(UNIT_ID)).toHaveTextContent("fix-login · Unit · agent/dispatch-fix-login");
    expect(rowOf(UNIT_ID)).toHaveTextContent("Worktree: /home/me/code/app-dispatch-fix-login");
    expect(rowOf(UNIT_ID)).toHaveTextContent("Started by: dispatcher · Last entry: attempt_ended, 2026-11-19 10:00 UTC");
    expect(rowOf(UNIT_ID)).toHaveTextContent("Progress: 1 attempt; roles coder");
    expect(rowOf(TEAM_ID)).toHaveTextContent("Progress: 3 attempts; roles orchestrator, coder, reviewer; 2 of 4 delegations done");
    expect(rowOf(AGENT_ID)).toHaveTextContent("Folder: /home/me/notes");
  });

  /** Scenario: Details on the unit show its branch, worktree, launcher, last entry, progress and its kept report "kept until 2026-11-26 10:00 UTC", without the report's text; "Show reports" asks the daemon for the first page and shows the text framed, its control and bidi characters escaped. */
  it("shows details, and a report's text only on demand", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    const details = within(dialog).getByTestId("interrupted-details-view");
    expect(details).toHaveTextContent("Branch");
    expect(details).toHaveTextContent("agent/dispatch-fix-login");
    expect(details).toHaveTextContent("Started by");
    expect(details).toHaveTextContent("attempt_ended, 2026-11-19 10:00 UTC");
    expect(details).toHaveTextContent("1 attempt; roles coder");
    expect(within(details).getByTestId("interrupted-report-entry")).toHaveTextContent("r-fix-login-done, kept until 2026-11-26 10:00 UTC");
    expect(within(dialog).queryByTestId("interrupted-report-body")).toBeNull();
    expect(calls.some((call) => call.verb === "show")).toBe(false);

    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    expect(calls.at(-1)).toMatchObject({ verb: "show", args: [UNIT_ID, 0] });
    const body = await within(dialog).findByTestId("interrupted-report-body");
    expect(body.textContent).toContain("[UNTRUSTED-WORKER-REPORT:");
    expect(body.textContent).toContain("all \\u{202e} done\\u{1b}[31m");
    expect(body.textContent).not.toMatch(/[‮\u001b]/);
  });

  /** Scenario: Reclaim on the unit runs a dry run first; its verdict "Reclaim would remove its worktree and keep its branch." is shown with the worktree, branch and unpushed commits, and a Reclaim confirm is offered; pressing it reclaims for real, shows "Reclaimed: …" and the list is asked for again. */
  it("confirms a reclaim only after a dry run that would remove", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-reclaim")); });
    expect(calls.filter((call) => call.verb === "reclaim")).toEqual([expect.objectContaining({ args: [UNIT_ID, true] })]);
    const verdict = await within(dialog).findByTestId("interrupted-reclaim-verdict");
    expect(verdict).toHaveTextContent(RECLAIM_WOULD_REMOVE);
    expect(verdict).toHaveTextContent("2 commits on its branch are on no remote; the branch keeps them.");
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-reclaim-confirm")); });
    expect(calls.filter((call) => call.verb === "reclaim").map((call) => call.args)).toEqual([[UNIT_ID, true], [UNIT_ID, false]]);
    expect(await within(dialog).findByTestId("interrupted-message")).toHaveTextContent(RECLAIM_REMOVED);
    await waitFor(() => expect(calls.at(-1)).toMatchObject({ verb: "list" }));
  });

  /** Scenario: a dry run that keeps the worktree (uncommitted changes) shows why and offers no confirm, so no real reclaim can be sent; a plain agent offers no Reclaim at all. */
  it("offers no confirm for a kept verdict, and no Reclaim on a plain agent", async () => {
    const { bridge, calls } = recordingBridge({ list: three(), dry: { id: UNIT_ID, dry_run: true, verdict: "kept-dirty", message: RECLAIM_KEPT_DIRTY } });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-reclaim")); });
    expect(await within(dialog).findByTestId("interrupted-reclaim-verdict")).toHaveTextContent(RECLAIM_KEPT_DIRTY);
    expect(within(dialog).queryByTestId("interrupted-reclaim-confirm")).toBeNull();
    expect(calls.some((call) => call.verb === "reclaim" && call.args[1] === false)).toBe(false);
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    expect(within(dialog).queryByTestId("interrupted-reclaim")).toBeNull();
  });

  /** Scenario: Dismiss with the plain agent selected dismisses exactly that item, says "Dismissed.", and the refreshed list — and the banner's count — drop it. */
  it("dismisses the selected item", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss")); });
    expect(calls.filter((call) => call.verb === "dismiss")).toEqual([expect.objectContaining({ args: [[AGENT_ID]] })]);
    expect(await within(dialog).findByTestId("interrupted-message")).toHaveTextContent(DISMISSED);
    await waitFor(() => expect(rows()).toHaveLength(2));
    expect(screen.getByTestId("interrupted-banner-message")).toHaveTextContent(interruptedFooter(2));
  });

  /** Scenario: Dismiss all asks "Dismiss all: 3 interrupted items from before the restart? [y/N]" and sends nothing until confirmed; a refresh that lists a fourth item meanwhile does not widen it: the confirm dismisses exactly the three the question named. */
  it("asks before dismissing all, and dismisses exactly the listed ids", async () => {
    const state = { list: three() as InterruptedList };
    const { bridge, calls } = recordingBridge(state);
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss-all-open")); });
    expect(within(dialog).getByTestId("interrupted-dismiss-all-question")).toHaveTextContent(dismissAllQuestion(3));
    expect(calls.some((call) => call.verb === "dismiss")).toBe(false);
    state.list = { items: [item(NEW_ID, "unit", "newer-work"), ...three().items] };
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-close")); });
    await openDialog();
    await waitFor(() => expect(rows()).toHaveLength(4));
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-dismiss-all-open")); });
    expect(screen.getByTestId("interrupted-dismiss-all-question")).toHaveTextContent(dismissAllQuestion(4));
    state.list = { items: [...state.list.items, item("w-abcdefgh-0000000000000005", "agent", "late")] };
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-dismiss-all-confirm")); });
    expect(calls.filter((call) => call.verb === "dismiss").map((call) => call.args[0])).toEqual([[NEW_ID, UNIT_ID, TEAM_ID, AGENT_ID]]);
  });
});

describe("the restart notice's interrupted work line", () => {
  /** Scenario: the deck restarts and its new daemon lists three interrupted items; the restart toast adds "3 interrupted items from before the restart" with a Show that opens the "Interrupted work" dialog. A restart whose daemon lists none leaves the toast unchanged. */
  it.each([true, false])("adds the hint line only when items are listed (items: %s)", async (items) => {
    const { bridge } = recordingBridge({ list: items ? three() : { items: [] } });
    const { rerender } = render(<Shell bridge={bridge} deck={deckOf("process-a")} />);
    await act(async () => { rerender(<Shell bridge={bridge} deck={deckOf("process-b", () => [])} />); });
    const toast = await screen.findByTestId("restart-toast");
    expect(toast).toHaveTextContent(AGENTS_STOPPED_BY_RESTART);
    if (!items) {
      await waitFor(() => expect(bridge.interruptedWorkList).toHaveBeenCalledTimes(2));
      expect(within(toast).queryByTestId("restart-toast-interrupted")).toBeNull();
      return;
    }
    const hint = await within(toast).findByTestId("restart-toast-interrupted");
    expect(hint).toHaveTextContent(interruptedFooter(3));
    await act(async () => { fireEvent.click(within(hint).getByTestId("restart-toast-interrupted-show")); });
    expect(await screen.findByTestId("interrupted-dialog")).toBeInTheDocument();
  });
});

describe("the restart notice over an open agent pane (issue #2555)", () => {
  /** Process B keeps one agent of its own, so there is a pane to open after the restart. */
  const keepsOne = (agents: AgentSession[]) => agents.slice(0, 1).map((one) => ({ ...one, id: "b-only", displayName: "b-only-agent" }));

  /** The deck restarts and lists interrupted work; then the user opens B's one agent from the dashboard. */
  async function paneUnderNotice() {
    const { bridge } = recordingBridge({ list: three() });
    const { rerender } = render(<Shell bridge={bridge} deck={deckOf("process-a")} />);
    await act(async () => { rerender(<Shell bridge={bridge} deck={deckOf("process-b", keepsOne)} />); });
    const toast = await screen.findByTestId("restart-toast");
    const show = await within(toast).findByTestId("restart-toast-interrupted-show");
    await act(async () => { fireEvent.click(screen.getByRole("row", { name: "Open b-only-agent agent" })); });
    const pane = screen.getByTestId("agent-pane-overlay");
    await waitFor(() => expect(screen.getByTestId("deck-selector-toggle").closest("[inert]")).not.toBeNull());
    return { toast, show, pane };
  }

  /** Scenario: the deck restarts, listing interrupted work, and the user opens an agent's pane with the restart notice still in the corner. The notice's Show and X are the only controls reachable outside the pane; Show goes to the dashboard and opens the "Interrupted work" dialog. */
  it("keeps Show and the X reachable over the pane, and Show opens the dialog", async () => {
    const { toast, show, pane } = await paneUnderNotice();
    const outside = Array.from(document.querySelectorAll("button, a[href], input, select, textarea, [tabindex]"))
      .filter((element) => !pane.contains(element) && !element.closest("[inert]"));
    expect(outside).toEqual([show, within(toast).getByRole("button", { name: "Dismiss restart notice" })]);
    expect(show).toBeEnabled();
    await act(async () => { fireEvent.click(show); });
    expect(await screen.findByTestId("interrupted-dialog")).toBeInTheDocument();
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
  });

  /** Scenario: with the pane open under the restart notice, the user starts Close agent, so its confirmation is drawn above the pane. The notice's Show is disabled while the confirmation is up — it is not a way past a question being answered — and comes back once it is cancelled. */
  it("disables Show while a dialog is drawn above the pane", async () => {
    const { show, pane } = await paneUnderNotice();
    await act(async () => { fireEvent.click(within(pane).getByRole("button", { name: "Close agent" })); });
    const confirmation = await screen.findByRole("alertdialog");
    await waitFor(() => expect(show).toBeDisabled());
    await act(async () => { fireEvent.click(within(confirmation).getByRole("button", { name: "Cancel" })); });
    await waitFor(() => expect(show).toBeEnabled());
  });
});

describe("the restart notice and an incomplete empty list (PR #2431 review)", () => {
  /** Scenario: the deck restarts and its new daemon's interrupted work list comes back empty but incomplete, so the dashboard offers a banner; the restart toast adds "The deck could not read its whole work journal, so some interrupted work may not be listed." with a Show that opens the "Interrupted work" dialog, which says the list is incomplete. */
  it("offers Show for an incomplete empty list", async () => {
    const { bridge } = recordingBridge({ list: { items: [], incomplete: true } });
    const { rerender } = render(<Shell bridge={bridge} deck={deckOf("process-a")} />);
    await act(async () => { rerender(<Shell bridge={bridge} deck={deckOf("process-b", () => [])} />); });
    const toast = await screen.findByTestId("restart-toast");
    const hint = await within(toast).findByTestId("restart-toast-interrupted");
    expect(hint).toHaveTextContent(INTERRUPTED_UNREAD);
    await act(async () => { fireEvent.click(within(hint).getByTestId("restart-toast-interrupted-show")); });
    const dialog = await screen.findByTestId("interrupted-dialog");
    await waitFor(() => expect(dialog).toHaveTextContent(LIST_INCOMPLETE));
  });
});

describe("a report page keeps the Git facts the list read (PR #2431 review)", () => {
  /** Scenario: the list is read with each item's Git facts, and the unit's details show "worktree on disk, uncommitted changes, 2 commits on no remote, branch kept, pull request open". Show reports answers the unit without Git facts, as the daemon's show does; the details, now with the report's text, still show the same Git line. */
  it("keeps the list's Git facts when the report page arrives", async () => {
    const git = { worktree_exists: true, dirty: true, unpushed_commits: 2, branch_exists: true, pr: "open" as const };
    const listed = { ...unit(), git };
    const { bridge } = recordingBridge({
      list: { items: [listed] },
      show: (_id, from) => ({ item: { ...listed, git: undefined }, reports: [{ report_id: "r-fix-login-done", body: "[UNTRUSTED-WORKER-REPORT:\nall done\n" }], reports_from: from }),
    });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    const line = gitFactsLine(git);
    expect(within(dialog).getByTestId("interrupted-details-view")).toHaveTextContent(line);
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    await within(dialog).findByTestId("interrupted-report-body");
    expect(within(dialog).getByTestId("interrupted-details-view")).toHaveTextContent(line);
  });
});

describe("interrupted work by voice", () => {
  const commands = readFileSync(resolve("src-tauri/src/voice/commands.toml"), "utf8");
  const rowFor = (id: string) => commands.split(/^\[\[commands\]\]\s*$/m).find((chunk) => chunk.match(new RegExp(`^id\\s*=\\s*"${id}"`, "m")));

  /** Scenario: showing, reclaiming, dismissing, dismissing all, an item's details, its reports and the next and previous report page are spoken commands, each a `voice: true` registry entry with its row in the command table; the three confirms, the immediate Dismiss of the button and `d`, the selection, Back, Close and Hide are registry entries that say why they are not, and none of them has a row. */
  it("registers every control, with a row for each spoken one", () => {
    const spoken = {
      show_interrupted_work: "showInterruptedWork", reclaim_interrupted: "reclaimInterrupted", dismiss_interrupted: "dismissInterrupted", dismiss_all_interrupted: "dismissAllInterrupted",
      show_interrupted_details: "showInterruptedDetails", show_interrupted_reports: "showInterruptedReports", next_interrupted_reports: "nextInterruptedReports", previous_interrupted_reports: "previousInterruptedReports",
    } as const;
    for (const [id, invoke] of Object.entries(spoken)) {
      expect((VOICE_ACTIONS as Record<string, { voice?: true }>)[invoke].voice, invoke).toBe(true);
      const row = rowFor(id);
      expect(row, id).toBeDefined();
      expect(row).toMatch(new RegExp(`^invoke\\s*=\\s*"${invoke}"`, "m"));
      expect(row).toMatch(/^screens\s*=\s*\["overview"\]/m);
    }
    for (const invoke of ["confirmReclaimInterrupted", "confirmDismissInterrupted", "confirmDismissAllInterrupted", "dismissSelectedInterrupted", "selectInterruptedItem", "backInterruptedWork", "closeInterruptedWork", "hideInterruptedWork"]) {
      expect((VOICE_ACTIONS as Record<string, { no_voice?: string }>)[invoke].no_voice?.length ?? 0, invoke).toBeGreaterThan(20);
      expect(commands, invoke).not.toMatch(new RegExp(`^invoke\\s*=\\s*"${invoke}"`, "m"));
    }
  });

  /** Scenario: with the dialog open, "reclaim the second one" — the row's `item` words — runs the dry run on the second listed item (the orchestration), reads its verdict back, and leaves the confirm on screen; "dismiss all" opens the question and dismisses nothing. */
  it("reclaims an ordinal's item with a read-back verdict, and asks before dismissing all", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const channel: VoiceOverviewChannel = { current: undefined };
    /* What the dashboard published, as a dispatch reads it. */
    const published = () => channel.current as VoiceOverviewContext;
    function Voiced() {
      const interruptedWork = useInterruptedWork(bridge, [deckOf("process-a")]);
      return <AgentOverview runtime={{ ...baseRuntime(deckOf("process-a")), ...(interruptedWork ? { interruptedWork } : {}) }} onNavigate={vi.fn()} voiceChannel={channel} />;
    }
    render(<Voiced />);
    await screen.findByTestId("interrupted-banner");
    const reportRefused = vi.fn();
    const reportPending = vi.fn();
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run({ ...published(), reportRefused }, {}); });
    expect(reportRefused).not.toHaveBeenCalled();
    await screen.findByTestId("interrupted-dialog");
    await waitFor(() => expect(rows()).toHaveLength(3));
    await act(async () => { VOICE_ACTIONS.reclaimInterrupted.run({ ...published(), reportRefused, reportPending }, { filterText: "the second one" }); });
    expect(calls.filter((call) => call.verb === "reclaim")).toEqual([expect.objectContaining({ args: [TEAM_ID, true] })]);
    expect(reportPending).toHaveBeenCalledWith(INTERRUPTED_RECLAIM_CHECKING, expect.any(Promise), expect.any(String));
    await expect(reportPending.mock.calls[0][1]).resolves.toBe(RECLAIM_WOULD_REMOVE);
    expect(await screen.findByTestId("interrupted-reclaim-confirm")).toBeInTheDocument();
    expect(calls.some((call) => call.verb === "reclaim" && call.args[1] === false)).toBe(false);

    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-back")); });
    await act(async () => { VOICE_ACTIONS.reclaimInterrupted.run({ ...published(), reportRefused, reportPending }, { filterText: "scratch-shell" }); });
    expect(reportRefused).toHaveBeenLastCalledWith(expect.stringContaining("nothing to reclaim"));
    await act(async () => { VOICE_ACTIONS.dismissAllInterrupted.run({ ...published(), reportRefused }); });
    expect(screen.getByTestId("interrupted-dismiss-all-question")).toHaveTextContent(dismissAllQuestion(3));
    expect(calls.some((call) => call.verb === "dismiss")).toBe(false);
  });

  /** Scenario: restore is a spoken action that asks a question on the dashboard. Each of the three choices that starts an agent has a written reason for requiring a person's confirmation on screen. */
  it("journal/restore/010 registers the ask and keeps the three confirms off voice", () => {
    expect(VOICE_ACTIONS.askRestoreInterrupted.voice).toBe(true);
    const row = rowFor("restore_interrupted");
    expect(row).toBeDefined();
    expect(row).toMatch(/^invoke\s*=\s*"askRestoreInterrupted"/m);
    expect(row).toMatch(/^screens\s*=\s*\["overview"\]/m);
    expect(row).toMatch(/name\s*=\s*"item"/);
    expect(row).toMatch(/kind\s*=\s*"filter_text"/);
    expect(row).toMatch(/optional\s*=\s*true/);
    expect(row).toContain("restore the agent");
    expect(row).toContain("bring back");
    expect(row).toContain("conversation");
    for (const name of ["restoreAndContinueInterrupted", "restoreOnlyInterrupted", "startOverInterrupted"] as const) {
      const action = VOICE_ACTIONS[name] as { voice?: true; no_voice?: string };
      expect(action.voice, name).not.toBe(true);
      expect(action.no_voice, name).toMatch(/starts an agent/);
      expect(action.no_voice, name).toMatch(/spends credits/);
      expect(action.no_voice, name).toMatch(/confirm-before-acting/);
      expect(action.no_voice, name).toContain("restore_interrupted");
      expect(commands, name).not.toMatch(new RegExp(`^invoke\\s*=\\s*"${name}"`, "m"));
    }
  });

  /** Scenario: the documented restore phrases target the same ask action, and a restore said in an agent pane is unavailable. This checks fixture declarations; it does not run the model that interprets speech. */
  it("journal/restore/010 keeps each documented phrase fixture", () => {
    const fixtures = readFileSync(resolve("src-tauri/src/voice/phrase_fixtures.toml"), "utf8");
    for (const [utterance, screenName, outcome] of [
      ["restore the agent", "overview", "dispatch"],
      ["bring back fix-login", "overview", "dispatch"],
      ["resume fix-login's conversation", "overview", "dispatch"],
      ["restore the agent", "agent", "unavailable"],
    ] as const) {
      const row = fixtures.split(/^\[\[fixtures\]\]\s*$/m).find((part) =>
        part.includes(`utterance = "${utterance}"`) && part.includes(`screen = "${screenName}"`));
      expect(row, `${screenName}: ${utterance}`).toBeDefined();
      expect(row).toContain('action = "restore_interrupted"');
      expect(row).toContain(`outcome = "${outcome}"`);
    }
  });
});

// ---------------------------------------------------------------------------
// PRD #1790 stage 2, desktop review findings (reviewer 1-11, auditor 1-4).
// ---------------------------------------------------------------------------

/** A second, remote deck whose daemon is `process`, with no agents. */
function remoteDeck(deckId: string, name: string, process: string): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  return { ...snapshot, agents: [], connection: { ...snapshot.connection, deckId, deckKind: "remote", socketPath: `dev@${name}`, name, incarnation: process } };
}

/** A unit item named `name`, with an id ending in `n`. */
function named(n: number, name: string, kind: InterruptedItem["kind"] = "unit"): InterruptedItem {
  return item(`w-abcdefgh-${String(n).padStart(16, "0")}`, kind, name);
}

/** `recordingBridge`, with every dismissal and real reclaim held until `release` answers the oldest one. */
function deferredBridge(state: { list: InterruptedList }) {
  const recorded = recordingBridge(state);
  const held: (() => void)[] = [];
  const bridge = {
    ...(recorded.bridge as unknown as Record<string, unknown>),
    interruptedWorkDismiss: vi.fn(async (target: InterruptedTarget, ids: string[]) => {
      recorded.calls.push({ verb: "dismiss", target, args: [ids] });
      await new Promise<void>((done) => { held.push(done); });
      state.list = { items: state.list.items.filter((candidate) => !ids.includes(candidate.id)) };
      return answered(target, { dismissed: ids.map((id) => ({ id })), refused: [] } as DismissOutcome);
    }),
  } as unknown as DeckBridge;
  return { bridge, calls: recorded.calls, release: () => held.shift()?.() };
}

/** A daemon's report pages for an item with `total` reports: page lengths by first offset, sixteen otherwise. */
function pagedShow(base: InterruptedItem, total: number, lengths: Record<number, number> = {}) {
  return (_id: string, from: number): InterruptedShow => {
    const length = lengths[from] ?? Math.min(16, total - from);
    const ids = Array.from({ length }, (_, at) => `r-${from + at + 1}`);
    const next = from + length < total ? from + length : null;
    return {
      item: { ...base, reports: ids.map((report_id) => ({ report_id, kept_until_ms: KEPT_TS })), reports_omitted: total - length },
      reports: ids.map((report_id) => ({ report_id, body: `body of ${report_id}` })),
      reports_from: from,
      next_reports_from: next,
    };
  };
}

/** The dashboard over `bridge` with its voice channel published, for the voice cases. `cover` mounts another modal over everything. */
function voiced(bridge: DeckBridge, decks: DeckSnapshot[] = [deckOf("process-a")]) {
  const channel: VoiceOverviewChannel = { current: undefined };
  function Voiced({ fleet = decks, cover = false }: { fleet?: DeckSnapshot[]; cover?: boolean }) {
    const interruptedWork = useInterruptedWork(bridge, fleet);
    return (
      <>
        <AgentOverview runtime={{ ...baseRuntime(fleet[0]), fleet, ...(interruptedWork ? { interruptedWork } : {}) }} onNavigate={vi.fn()} voiceChannel={channel} />
        {cover && <Cover />}
      </>
    );
  }
  const reportRefused = vi.fn();
  const reportPending = vi.fn();
  /** What the dashboard published, as a dispatch reads it, with the voice surface's reports. */
  const context = () => ({ ...(channel.current as VoiceOverviewContext), reportRefused, reportPending });
  return { Voiced, context, reportRefused, reportPending };
}

/** Another modal, drawn over whatever is open. */
function Cover() {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.dialog);
  return <div ref={ref} role="dialog" aria-modal="true" data-testid="cover-modal">over it</div>;
}

const dismissCalls = (calls: { verb: string; args: unknown[] }[]) => calls.filter((call) => call.verb === "dismiss").map((call) => call.args[0]);
const reclaimCalls = (calls: { verb: string; args: unknown[] }[]) => calls.filter((call) => call.verb === "reclaim").map((call) => call.args);
const showCalls = (calls: { verb: string; args: unknown[] }[]) => calls.filter((call) => call.verb === "show").map((call) => call.args[1]);

describe("the dialog under React's development checks (reviewer 1)", () => {
  /** Scenario: the app's root renders in StrictMode, which mounts every effect twice. With the dialog open there, Show reports shows the report, Reclaim's dry run shows its verdict and its confirm reclaims, and Dismiss dismisses the plain agent; each answer reaches the dialog and the refreshed list drops what went. */
  it("acts on every answer when mounted under StrictMode", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<StrictMode><Overview bridge={bridge} deck={deckOf("process-a")} /></StrictMode>);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    expect(await within(dialog).findByTestId("interrupted-report-body")).toHaveTextContent("[UNTRUSTED-WORKER-REPORT:");
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });

    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-reclaim")); });
    expect(await within(dialog).findByTestId("interrupted-reclaim-verdict")).toHaveTextContent(RECLAIM_WOULD_REMOVE);
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-reclaim-confirm")); });
    expect(await within(dialog).findByTestId("interrupted-message")).toHaveTextContent(RECLAIM_REMOVED);
    await waitFor(() => expect(rows()).toHaveLength(2));

    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss")); });
    expect(await within(dialog).findByTestId("interrupted-message")).toHaveTextContent(DISMISSED);
    await waitFor(() => expect(rows()).toHaveLength(1));
    expect(reclaimCalls(calls)).toEqual([[UNIT_ID, true], [UNIT_ID, false]]);
    expect(dismissCalls(calls)).toEqual([[AGENT_ID]]);
  });
});

describe("a dismissal refused on an incomplete journal read (docs review R2)", () => {
  /** Scenario: the daemon refuses a dismissal whole because it could not read its whole work journal. Dismiss on one item, and Dismiss all confirmed, each show "Refused: the deck could not read its whole work journal, …"; a Dismiss all whose later part was refused that way says "Dismissed." and that 1 more item was not dismissed, with the same refusal. */
  it("says so for one item and for Dismiss all", async () => {
    const { bridge } = recordingBridge({ list: three() });
    let answer: DismissOutcome = { dismissed: [], refused: [], refusal: { reason: "incomplete-read", message: "x" } };
    const refusing = {
      ...(bridge as unknown as Record<string, unknown>),
      interruptedWorkDismiss: vi.fn(async (target: InterruptedTarget) => answered(target, structuredClone(answer))),
    } as unknown as DeckBridge;
    render(<Overview bridge={refusing} deck={deckOf("process-a")} />);
    const dialog = await openDialog();

    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-message")).toHaveTextContent(REFUSED_INCOMPLETE_READ));

    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss-all-open")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss-all-confirm")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-message")).toHaveTextContent(REFUSED_INCOMPLETE_READ));

    answer = { dismissed: [{ id: UNIT_ID }], refused: [], stopped: { reason: "refused", refusal: "incomplete-read", not_sent: 1 } };
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss-all-open")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss-all-confirm")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-message")).toHaveTextContent(`${DISMISSED} 1 more item was not dismissed. ${REFUSED_INCOMPLETE_READ}`));
  });
});

describe("a dismissal in flight when the dialog closes (reviewer 2)", () => {
  /** Scenario: Dismiss on the plain agent is sent and the daemon has not answered; the user closes the dialog and opens it again. The reopened dialog still waits for that dismissal — its Dismiss is disabled and `d` sends nothing — and when the daemon answers, the list is asked for again and drops the item, though no dialog of the first opening is left to ask. */
  it("keeps it pending across closing and reopening, and refreshes the list when it answers", async () => {
    const state = { list: three() };
    const { bridge, calls, release } = deferredBridge(state);
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    let dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss")); });
    expect(dismissCalls(calls)).toEqual([[AGENT_ID]]);
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-close")); });
    dialog = await openDialog();
    await waitFor(() => expect(rows()).toHaveLength(3));

    expect(within(dialog).getByTestId("interrupted-dismiss")).toBeDisabled();
    await act(async () => { fireEvent.keyDown(dialog, { key: "d" }); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss")); });
    expect(dismissCalls(calls)).toEqual([[AGENT_ID]]);

    await act(async () => { release(); });
    await waitFor(() => expect(rows()).toHaveLength(2));
    expect(screen.getByTestId("interrupted-banner-message")).toHaveTextContent(interruptedFooter(2));
    expect(within(dialog).getByTestId("interrupted-dismiss")).toBeEnabled();
  });

  /** Scenario: a dismissal sent to daemon process A is still waiting when the deck reconnects to process B; when A's answer comes, nothing asks process B for its list on its account. */
  it("does not refresh another daemon process when the one it was sent to answers", async () => {
    const state = { list: three() };
    const { bridge, calls, release } = deferredBridge(state);
    const { rerender } = render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-dismiss")); });
    await act(async () => { rerender(<Overview bridge={bridge} deck={deckOf("process-b")} />); });
    await waitFor(() => expect(calls.some((call) => call.verb === "list" && call.target.instance === "process-b")).toBe(true));
    const before = calls.filter((call) => call.verb === "list" && call.target.instance === "process-b").length;
    await act(async () => { release(); await Promise.resolve(); });
    expect(calls.filter((call) => call.verb === "list" && call.target.instance === "process-b").length).toBe(before);
  });
});

describe("report pages (reviewer 4)", () => {
  /** Scenario: the fix-login unit has 33 kept reports, which the daemon pages 16, 16 and 1. Show reports, Next reports twice, then Previous reports asks for the pages from 0, 16, 32 and back to 16, and shows "Reports 17 to 32 of 33.". */
  it("goes back to the page before, whatever the length of the page shown", async () => {
    const { bridge, calls } = recordingBridge({ list: three(), show: pagedShow(unit(), 33) });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    await act(async () => { fireEvent.click(await within(dialog).findByTestId("interrupted-next-reports")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(17, 32, 33)));
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-next-reports")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(33, 33, 33)));
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-previous-reports")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(17, 32, 33)));
    expect(showCalls(calls)).toEqual([0, 16, 32, 16]);
  });

  /** Scenario: the daemon cut the second page short to stay within its byte budget (16, 10, 7): Previous from the third page asks for the page from 16, the one actually shown before, not 16 reports back. */
  it("returns to the page actually shown before when the daemon cut one short", async () => {
    const { bridge, calls } = recordingBridge({ list: three(), show: pagedShow(unit(), 33, { 16: 10, 26: 7 }) });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    await act(async () => { fireEvent.click(await within(dialog).findByTestId("interrupted-next-reports")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(17, 26, 33)));
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-next-reports")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(27, 33, 33)));
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-previous-reports")); });
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(17, 26, 33)));
    expect(showCalls(calls)).toEqual([0, 16, 26, 16]);
  });
});

describe("a list that failed (reviewer 7, auditor 4)", () => {
  /** Scenario: the daemon's answer to the list request is an error whose text carries a right-to-left override, an isolate, a newline, an escape sequence and HTML-looking text. Opening the deck's dialog by voice shows the error with each of those characters spelled out, none of them live, and no HTML element made of it. */
  it("escapes the error it shows", async () => {
    const { bridge } = recordingBridge({ list: three() });
    const failing = { ...(bridge as unknown as Record<string, unknown>), interruptedWorkList: vi.fn(async () => { throw new Error("bad ‮ evil ⁦ <b>bold</b>\nline\u001b[31m"); }) } as unknown as DeckBridge;
    const voice = voiced(failing);
    render(<voice.Voiced />);
    await waitFor(() => expect(failing.interruptedWorkList).toHaveBeenCalled());
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), { preselectDeckId: deckOf("process-a").connection.deckId }); });
    const note = await screen.findByTestId("interrupted-unavailable");
    await waitFor(() => expect(note.textContent).toContain("\\u{202e}"));
    expect(note.textContent).toContain("\\u{2066}");
    expect(note.textContent).toContain("\\n");
    expect(note.textContent).toContain("\\u{1b}[31m");
    expect(note.textContent).not.toMatch(/[‮⁦\u001b\n]/);
    expect(note.querySelector("b")).toBeNull();
  });
});

describe("a spoken dismissal (reviewer 6, auditor 1)", () => {
  /** Scenario: with the dialog open, "dismiss the first one" sends nothing and asks whether to dismiss fix-login, naming it; Back cancels and nothing was sent. "Dismiss scratch shell" asks about the plain agent; saying "dismiss it" again sends nothing; the person's click on the question's Dismiss dismisses exactly that item. "Dismiss the ninth one" is refused as not listed and asks nothing. */
  it("asks about the item it names, and dismisses it only on the person's confirm", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    await screen.findByTestId("interrupted-banner");
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(rows()).toHaveLength(3));

    await act(async () => { VOICE_ACTIONS.dismissInterrupted.run(voice.context(), { filterText: "the first one" }); });
    expect(dismissCalls(calls)).toEqual([]);
    expect(voice.reportRefused).not.toHaveBeenCalled();
    expect(screen.getByTestId("interrupted-dismiss-question")).toHaveTextContent("fix-login");
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-back")); });
    expect(screen.queryByTestId("interrupted-dismiss-question")).toBeNull();
    expect(dismissCalls(calls)).toEqual([]);

    await act(async () => { VOICE_ACTIONS.dismissInterrupted.run(voice.context(), { filterText: "scratch shell" }); });
    expect(screen.getByTestId("interrupted-dismiss-question")).toHaveTextContent("scratch-shell");
    await act(async () => { VOICE_ACTIONS.dismissInterrupted.run(voice.context(), {}); });
    expect(dismissCalls(calls)).toEqual([]);
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-dismiss-confirm")); });
    expect(dismissCalls(calls)).toEqual([[AGENT_ID]]);
    await waitFor(() => expect(rows()).toHaveLength(2));

    await act(async () => { VOICE_ACTIONS.dismissInterrupted.run(voice.context(), { filterText: "the ninth one" }); });
    expect(voice.reportRefused).toHaveBeenLastCalledWith(REFUSED_NOT_LISTED);
    expect(screen.queryByTestId("interrupted-dismiss-question")).toBeNull();
    expect(dismissCalls(calls)).toEqual([[AGENT_ID]]);
  });

  /** Scenario: another modal is drawn over the open dialog; a spoken dismissal, reclaim or dismiss all is refused with a sentence and changes nothing in the dialog behind it. */
  it("refuses while another modal covers the dialog", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const voice = voiced(bridge);
    const { rerender } = render(<voice.Voiced />);
    await screen.findByTestId("interrupted-banner");
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(rows()).toHaveLength(3));
    rerender(<voice.Voiced cover />);
    await act(async () => { VOICE_ACTIONS.dismissInterrupted.run(voice.context(), { filterText: "the first one" }); });
    expect(voice.reportRefused).toHaveBeenLastCalledWith(expect.any(String));
    await act(async () => { VOICE_ACTIONS.reclaimInterrupted.run(voice.context(), { filterText: "the first one" }); });
    await act(async () => { VOICE_ACTIONS.dismissAllInterrupted.run(voice.context()); });
    expect(voice.reportRefused).toHaveBeenCalledTimes(3);
    expect(screen.queryByTestId("interrupted-dismiss-question")).toBeNull();
    expect(screen.queryByTestId("interrupted-dismiss-all-question")).toBeNull();
    expect(dismissCalls(calls)).toEqual([]);
    expect(reclaimCalls(calls)).toEqual([]);
  });
});

describe("a spoken Dismiss all's report (reviewer 9)", () => {
  /** Scenario: "dismiss all" with no dialog open is refused with the sentence that says to open it first; with the dialog open it opens the question and reports no refusal. */
  it("reports a refusal only when no dialog is open", async () => {
    const { bridge } = recordingBridge({ list: three() });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    await screen.findByTestId("interrupted-banner");
    await act(async () => { VOICE_ACTIONS.dismissAllInterrupted.run(voice.context()); });
    expect(voice.reportRefused).toHaveBeenCalledWith(expect.stringContaining("Open the interrupted work first"));
    voice.reportRefused.mockClear();
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(rows()).toHaveLength(3));
    await act(async () => { VOICE_ACTIONS.dismissAllInterrupted.run(voice.context()); });
    expect(screen.getByTestId("interrupted-dismiss-all-question")).toHaveTextContent(dismissAllQuestion(3));
    expect(voice.reportRefused).not.toHaveBeenCalled();
  });
});

describe("which item spoken words name (reviewer 10, auditor 3)", () => {
  /** Run a spoken reclaim naming `said` on the open dialog, then back out of whatever it opened. */
  async function reclaimSaid(voice: ReturnType<typeof voiced>, said: string) {
    await act(async () => { VOICE_ACTIONS.reclaimInterrupted.run(voice.context(), { filterText: said }); });
    const back = screen.queryByTestId("interrupted-back");
    if (back) await act(async () => { fireEvent.click(back); });
  }

  async function openWith(list: InterruptedList) {
    const { bridge, calls } = recordingBridge({ list });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    await screen.findByTestId("interrupted-banner");
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(rows()).toHaveLength(list.items.length));
    return { voice, calls };
  }

  /** Scenario: "fix login", "Fix_Login" and the branch said as "agent dispatch fix login" each name the fix-login unit, whose name and branch are spelled with a hyphen and a slash. */
  it("matches names and branches by their words, whatever the punctuation", async () => {
    const { voice, calls } = await openWith(three());
    await reclaimSaid(voice, "fix login");
    await reclaimSaid(voice, "Fix_Login");
    await reclaimSaid(voice, "agent dispatch fix login");
    expect(voice.reportRefused).not.toHaveBeenCalled();
    expect(reclaimCalls(calls)).toEqual([[UNIT_ID, true], [UNIT_ID, true], [UNIT_ID, true]]);
  });

  /** Scenario: units named "fix" and "fix-login" are listed; "fix" names the unit called exactly that rather than being refused as matching both. Two units both named "dup", or "release" against "release-a" and "release-b", are refused as ambiguous and nothing is sent. */
  it("prefers an exact name over a partial one, and refuses ties", async () => {
    const fix = named(11, "fix");
    const { voice, calls } = await openWith({ items: [named(12, "fix-login"), fix, named(13, "dup"), named(14, "dup"), named(15, "release-a"), named(16, "release-b")] });
    await reclaimSaid(voice, "fix");
    expect(reclaimCalls(calls)).toEqual([[fix.id, true]]);
    await reclaimSaid(voice, "dup");
    expect(voice.reportRefused).toHaveBeenLastCalledWith(REFUSED_AMBIGUOUS);
    await reclaimSaid(voice, "release");
    expect(voice.reportRefused).toHaveBeenLastCalledWith(REFUSED_AMBIGUOUS);
    expect(reclaimCalls(calls)).toEqual([[fix.id, true]]);
  });

  /** Scenario: units named "two", "last" and "first" are listed in that order. "first", "two" and "last" each name one item as a position and another by its name, so each is refused as ambiguous and nothing is sent; "the second one" still names the second item. */
  it("refuses words that are both an ordinal and another item's name", async () => {
    const [two, last, first] = [named(21, "two"), named(22, "last"), named(23, "first")];
    const { voice, calls } = await openWith({ items: [two, last, first] });
    for (const said of ["first", "two", "last"]) {
      await reclaimSaid(voice, said);
      expect(voice.reportRefused, said).toHaveBeenLastCalledWith(REFUSED_AMBIGUOUS);
    }
    expect(reclaimCalls(calls)).toEqual([]);
    await reclaimSaid(voice, "the second one");
    expect(reclaimCalls(calls)).toEqual([[last.id, true]]);
  });
});

describe("details and reports by voice (reviewer 11)", () => {
  /** Scenario: "show the details of the first one" opens fix-login's details; "show its reports" asks for the first page of its 33 reports, "next page of reports" for the page from 16 and "previous page of reports" for the page from 0 again — all without a pointer. "Show the reports of release team" from the list opens that item's details and asks for its first page. */
  it("opens an item's details and pages its reports", async () => {
    /* The orchestration keeps a report too, so its reports can be asked for by name. */
    const list: InterruptedList = { items: [agent(), unit(), { ...team(), reports: [{ report_id: "r-release-team", kept_until_ms: KEPT_TS }] }] };
    const { bridge, calls } = recordingBridge({ list, show: (id, from) => pagedShow(list.items.find((candidate) => candidate.id === id)!, 33)(id, from) });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    await screen.findByTestId("interrupted-banner");
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(rows()).toHaveLength(3));

    await act(async () => { VOICE_ACTIONS.showInterruptedDetails.run(voice.context(), { filterText: "the first one" }); });
    expect(screen.getByTestId("interrupted-details-view")).toHaveTextContent("fix-login");
    await act(async () => { VOICE_ACTIONS.showInterruptedReports.run(voice.context(), {}); });
    await waitFor(() => expect(screen.getAllByTestId("interrupted-report")).toHaveLength(16));
    await act(async () => { VOICE_ACTIONS.nextInterruptedReports.run(voice.context()); });
    await waitFor(() => expect(screen.getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(17, 32, 33)));
    await act(async () => { VOICE_ACTIONS.previousInterruptedReports.run(voice.context()); });
    await waitFor(() => expect(screen.getByTestId("interrupted-report-range")).toHaveTextContent(reportRangeSentence(1, 16, 33)));
    expect(calls.filter((call) => call.verb === "show").map((call) => call.args)).toEqual([[UNIT_ID, 0], [UNIT_ID, 16], [UNIT_ID, 0]]);
    expect(voice.reportRefused).not.toHaveBeenCalled();

    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-back")); });
    await act(async () => { VOICE_ACTIONS.showInterruptedReports.run(voice.context(), { filterText: "release team" }); });
    expect(screen.getByTestId("interrupted-details-view")).toHaveTextContent("release-team");
    await waitFor(() => expect(calls.at(-1)).toMatchObject({ verb: "show", args: [TEAM_ID, 0] }));
  });
});

describe("the TUI's keys on the desktop (reviewer 8, CLAUDE.md rule 22)", () => {
  /** Wait until the open confirmation's question counts as presented. */
  async function presented(testId: string) {
    await waitFor(() => expect(screen.getByTestId(testId)).toHaveAttribute("data-presented", "true"), { timeout: 3_000 });
  }

  /** Scenario: on the dashboard `I` opens the one daemon's Interrupted work, as the TUI's `I` does; in the list `q` closes it; in an item's details `q` goes back to the list. With focus in a text field, `I` is typed into it and opens nothing. */
  it("opens with I and closes or goes back with q, and leaves a text field's keys alone", async () => {
    const { bridge } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    await screen.findByTestId("interrupted-banner");
    const field = document.createElement("input");
    document.body.append(field);
    field.focus();
    await act(async () => { fireEvent.keyDown(field, { key: "I" }); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
    field.remove();

    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    const dialog = await screen.findByTestId("interrupted-dialog");
    await waitFor(() => expect(rows()).toHaveLength(3));
    await act(async () => { fireEvent.keyDown(dialog, { key: "Enter" }); });
    expect(screen.getByTestId("interrupted-details-view")).toBeInTheDocument();
    await act(async () => { fireEvent.keyDown(dialog, { key: "q" }); });
    expect(screen.queryByTestId("interrupted-details-view")).toBeNull();
    expect(rows()).toHaveLength(3);
    await act(async () => { fireEvent.keyDown(dialog, { key: "q" }); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
  });

  /** Scenario: two daemons both list interrupted work; `I` opens neither and the dashboard says that several daemons have it, as voice's "show interrupted work" does. */
  it("asks which daemon when several list items", async () => {
    const { bridge } = recordingBridge({ list: three() });
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    function Two() {
      const interruptedWork = useInterruptedWork(bridge, fleet);
      return <AgentOverview runtime={{ ...baseRuntime(fleet[0]), fleet, ...(interruptedWork ? { interruptedWork } : {}) }} onNavigate={vi.fn()} />;
    }
    render(<Two />);
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2));
    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
    expect(screen.getByText(/Several daemons have interrupted work/)).toBeInTheDocument();
  });

  /** Scenario: `D` asks the Dismiss all question; a `y` pressed at once, before the question has been presented, dismisses nothing, and `n` cancels it. Asked again, a `y` once the question has been presented dismisses exactly the listed items. */
  it("answers Dismiss all's question with y only once it has been presented, and n cancels", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.keyDown(dialog, { key: "D" }); fireEvent.keyDown(dialog, { key: "y" }); });
    await act(async () => { fireEvent.keyDown(dialog, { key: "y" }); });
    expect(dismissCalls(calls)).toEqual([]);
    expect(screen.getByTestId("interrupted-dismiss-all-question")).toBeInTheDocument();
    await act(async () => { fireEvent.keyDown(dialog, { key: "n" }); });
    expect(screen.queryByTestId("interrupted-dismiss-all-question")).toBeNull();
    expect(dismissCalls(calls)).toEqual([]);

    await act(async () => { fireEvent.keyDown(dialog, { key: "D" }); });
    await presented("interrupted-dismiss-all");
    await act(async () => { fireEvent.keyDown(dialog, { key: "y" }); });
    expect(dismissCalls(calls)).toEqual([[UNIT_ID, TEAM_ID, AGENT_ID]]);
    await waitFor(() => expect(screen.getByTestId("interrupted-empty")).toHaveTextContent(NOTHING_INTERRUPTED));
  });

  /** Scenario: `r` runs the reclaim's dry run; once its verdict asks to confirm, a `y` before the question has been presented reclaims nothing, and one after it reclaims for real. A spoken dismissal's question is answered by `y` the same way. */
  it("answers a reclaim's and a spoken dismissal's question with y only once presented", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    await screen.findByTestId("interrupted-banner");
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(rows()).toHaveLength(3));
    const dialog = screen.getByTestId("interrupted-dialog");
    await act(async () => { fireEvent.keyDown(dialog, { key: "r" }); });
    await screen.findByTestId("interrupted-reclaim-confirm");
    await act(async () => { fireEvent.keyDown(dialog, { key: "y" }); });
    expect(reclaimCalls(calls)).toEqual([[UNIT_ID, true]]);
    await presented("interrupted-reclaim-view");
    await act(async () => { fireEvent.keyDown(dialog, { key: "y" }); });
    expect(reclaimCalls(calls)).toEqual([[UNIT_ID, true], [UNIT_ID, false]]);
    await waitFor(() => expect(rows()).toHaveLength(2));

    await act(async () => { VOICE_ACTIONS.dismissInterrupted.run(voice.context(), { filterText: "scratch shell" }); });
    await act(async () => { fireEvent.keyDown(dialog, { key: "y" }); });
    expect(dismissCalls(calls)).toEqual([]);
    await presented("interrupted-dismiss-one");
    await act(async () => { fireEvent.keyDown(dialog, { key: "y" }); });
    expect(dismissCalls(calls)).toEqual([[AGENT_ID]]);
  });
});

/** A bridge whose list answers per daemon process from `lists`, rejecting where it holds an `Error`; the requests are recorded. */
function perProcessBridge(lists: Record<string, InterruptedList | Error>) {
  const { bridge, calls } = recordingBridge({ list: three() });
  const listed: InterruptedTarget[] = [];
  const perProcess = {
    ...(bridge as unknown as Record<string, unknown>),
    interruptedWorkList: vi.fn(async (target: InterruptedTarget) => {
      listed.push(target);
      const answer = lists[target.instance];
      if (answer instanceof Error) throw answer;
      return answered(target, structuredClone(answer ?? { items: [] }));
    }),
  } as unknown as DeckBridge;
  return { bridge: perProcess, calls, listed };
}

describe("opening the list asks the daemon again (final reviewer 2)", () => {
  /** Scenario: the daemon's first answer lists nothing, so no banner is drawn; later a unit's report is kept for an orchestrator that went away. `I` on the dashboard asks the daemon again rather than trusting the empty answer, and the dialog shows the unit. Saying "show interrupted work" does the same. */
  it("refetches an empty first answer on I and on voice", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [] } };
    const { bridge, listed } = perProcessBridge(lists);
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(1));
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();
    lists["process-a"] = { items: [unit()] };

    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(2));
    const dialog = await screen.findByTestId("interrupted-dialog");
    await waitFor(() => expect(within(dialog).getAllByTestId("interrupted-row")).toHaveLength(1));
    expect(rows()[0]).toHaveTextContent("fix-login");
    expect(screen.queryByText(NOTHING_INTERRUPTED)).toBeNull();
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-close")); });

    lists["process-a"] = { items: [unit(), team()] };
    const before = listed.length;
    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(listed.length).toBeGreaterThan(before));
    await waitFor(() => expect(rows()).toHaveLength(2));
    expect(voice.reportRefused).not.toHaveBeenCalled();
  });

  /** Scenario: the daemon's first answer to the list is an error. `I` asks again, and when that answer lists a unit the dialog shows it. */
  it("refetches after a first answer that failed", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": new Error("daemon busy") };
    const { bridge, listed } = perProcessBridge(lists);
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    await waitFor(() => expect(listed).toHaveLength(1));
    lists["process-a"] = { items: [unit()] };

    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(2));
    await waitFor(() => expect(rows()).toHaveLength(1));
    expect(rows()[0]).toHaveTextContent("fix-login");
  });

  /** Scenario: two daemons both first answered with nothing. A unit is then listed on the second. `I` asks both again before deciding, and opens the second daemon's dialog showing the unit; nothing says "No interrupted work". */
  it("asks every daemon again before deciding which ones have work", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [] }, "process-b": { items: [] } };
    const { bridge, listed } = perProcessBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    lists["process-b"] = { items: [unit()] };

    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    await waitFor(() => expect(listed.filter((target) => target.instance === "process-a").length).toBeGreaterThanOrEqual(2));
    expect(listed.filter((target) => target.instance === "process-b").length).toBeGreaterThanOrEqual(2);
    const dialog = await screen.findByTestId("interrupted-dialog");
    await waitFor(() => expect(within(dialog).getAllByTestId("interrupted-row")).toHaveLength(1));
    expect(dialog).toHaveTextContent("beta");
    expect(screen.queryByText(NOTHING_INTERRUPTED)).toBeNull();
  });

  /** Scenario: two daemons first answered with nothing; asked again, one still lists nothing and the other's answer fails. `I` opens nothing and says which daemon could not be asked, never that there is no interrupted work. */
  it("never says there is none when a daemon could not be asked", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [] }, "process-b": { items: [] } };
    const { bridge, listed } = perProcessBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    lists["process-b"] = new Error("connection reset");

    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(4));
    await waitFor(() => expect(voice.reportRefused).toHaveBeenCalled());
    const said = voice.reportRefused.mock.calls.at(-1)?.[0] as string;
    expect(said).not.toBe(NOTHING_INTERRUPTED);
    expect(said).toContain("beta");
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
  });
});

describe("an incomplete empty answer is not an answer that there is none (verification finding C)", () => {
  /** Scenario: two daemons; asked again, one lists nothing and the other lists nothing but says its journal could not be read whole. `I` never says there is no interrupted work: it opens the second daemon's dialog, which says the list may be missing items. */
  it("opens the daemon whose empty list is incomplete rather than saying there is none", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [] }, "process-b": { items: [] } };
    const { bridge, listed } = perProcessBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    lists["process-b"] = { items: [], incomplete: true };

    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(4));
    const dialog = await screen.findByTestId("interrupted-dialog");
    expect(dialog).toHaveTextContent("beta");
    await waitFor(() => expect(dialog).toHaveTextContent(LIST_INCOMPLETE));
    expect(voice.reportRefused.mock.calls.map((call) => call[0])).not.toContain(NOTHING_INTERRUPTED);
  });
});

describe("several daemons whose lists are incomplete each offer a way in (verification finding N4)", () => {
  /** Scenario: two daemons both answer with an empty list that says their journal could not be read whole. Each daemon's section shows a banner saying it may have interrupted work it could not list, with Show. `I` opens nothing and never says the daemons have interrupted work, only that they may have; pressing one banner's Show opens that daemon's dialog, which says the list is incomplete. */
  it("draws a banner for each incomplete empty list, and its Show opens that daemon", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [], incomplete: true }, "process-b": { items: [], incomplete: true } };
    const { bridge, listed } = perProcessBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2));
    for (const banner of screen.getAllByTestId("interrupted-banner")) expect(banner).toHaveTextContent(INTERRUPTED_UNREAD);

    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(4));
    await waitFor(() => expect(screen.getByText(/may have interrupted work/)).toBeInTheDocument());
    const said = screen.getByText(/may have interrupted work/).textContent ?? "";
    expect(said).toContain("beta");
    expect(said).not.toMatch(/daemons have interrupted work/);
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();

    const beta = screen.getAllByTestId("interrupted-banner").find((banner) => banner.getAttribute("data-deck-id") === "deck-b");
    expect(beta, "the beta daemon's section has its banner").toBeDefined();
    await act(async () => { fireEvent.click(within(beta as HTMLElement).getByTestId("interrupted-banner-show")); });
    const dialog = await screen.findByTestId("interrupted-dialog");
    expect(dialog).toHaveTextContent("beta");
    await waitFor(() => expect(dialog).toHaveTextContent(LIST_INCOMPLETE));
  });

  /** Scenario: two daemons both answer with an empty list that says their journal could not be read whole, and both banners are hidden with their Hide buttons. `I` brings both banners back before telling the user to open one from its banner, so a keyboard or mouse user has a Show to press without voice; pressing the beta daemon's Show opens its dialog. */
  it("brings hidden banners back when I tells the user to open one from its banner", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [], incomplete: true }, "process-b": { items: [], incomplete: true } };
    const { bridge, listed } = perProcessBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2));
    for (const banner of screen.getAllByTestId("interrupted-banner")) {
      await act(async () => { fireEvent.click(within(banner).getByTestId("interrupted-banner-hide")); });
    }
    await waitFor(() => expect(screen.queryAllByTestId("interrupted-banner")).toHaveLength(0));

    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(4));
    await waitFor(() => expect(screen.getByText(/Open one from its banner/)).toBeInTheDocument());
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2));

    const beta = screen.getAllByTestId("interrupted-banner").find((banner) => banner.getAttribute("data-deck-id") === "deck-b");
    expect(beta, "the beta daemon's banner is back").toBeDefined();
    await act(async () => { fireEvent.click(within(beta as HTMLElement).getByTestId("interrupted-banner-show")); });
    const dialog = await screen.findByTestId("interrupted-dialog");
    expect(dialog).toHaveTextContent("beta");
  });

  /** Scenario: one daemon lists a unit and another answers with an empty list that says it could not be read whole. Both sections show a banner; `I` names both, says they may have interrupted work rather than that they have it, and opens nothing. */
  it("says may have when one of several daemons only could not read its list", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": { items: [unit()] }, "process-b": { items: [], incomplete: true } };
    const { bridge, listed } = perProcessBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2));

    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(voice.reportRefused).toHaveBeenCalled());
    const said = voice.reportRefused.mock.calls.at(-1)?.[0] as string;
    expect(said).toContain("may have interrupted work");
    expect(said).toContain("beta");
    expect(said).not.toMatch(/daemons have interrupted work/);
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
  });
});

describe("X hides a banner, as the TUI's X hides its row (final reviewer 3)", () => {
  /** Scenario: one daemon's banner is up; `X` on the dashboard hides it and sends nothing; `I` then still lists every item, so nothing was dismissed. With focus in a text field `X` is typed there and hides nothing. */
  it("hides the one banner without dismissing anything", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    await screen.findByTestId("interrupted-banner");
    const field = document.createElement("input");
    document.body.append(field);
    field.focus();
    await act(async () => { fireEvent.keyDown(field, { key: "X" }); });
    expect(screen.getByTestId("interrupted-banner")).toBeInTheDocument();
    field.remove();

    const before = calls.length;
    await act(async () => { fireEvent.keyDown(document.body, { key: "X" }); });
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();
    expect(calls.length).toBe(before);
    await act(async () => { fireEvent.keyDown(document.body, { key: "I" }); });
    await waitFor(() => expect(rows()).toHaveLength(3));
    expect(dismissCalls(calls)).toEqual([]);
  });

  /** Scenario: two daemons' lists arrive while nothing else is happening, and `X` is pressed at the very moment both banners are first on screen, before React has run its later effects. `X` acts on the banners shown: it hides neither and says that several daemons have one. */
  it("acts on the banners on screen when X is pressed as they first appear", async () => {
    const { bridge } = recordingBridge({ list: three() });
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    let pressed = false;
    /* A MutationObserver runs as a microtask right after the commit that draws the banners: earlier than React's passive effects, which it schedules as a later task. */
    const observer = new MutationObserver(() => {
      if (pressed || document.querySelectorAll("[data-testid='interrupted-banner']").length < 2) return;
      pressed = true;
      fireEvent.keyDown(document.body, { key: "X" });
    });
    observer.observe(document.body, { childList: true, subtree: true });
    try {
      render(<voice.Voiced />);
      await waitFor(() => expect(pressed).toBe(true));
    } finally {
      observer.disconnect();
    }
    await waitFor(() => expect(screen.getByText(/Several daemons have interrupted work/)).toBeInTheDocument());
    expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2);
  });

  /** Scenario: two daemons' banners are up; `X` hides neither and says that several daemons have one. Once the first daemon's banner is hidden with its Hide button, `X` hides the other one. */
  it("asks which banner when several are up", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2));
    await act(async () => { fireEvent.keyDown(document.body, { key: "X" }); });
    expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(2);
    expect(screen.getByText(/Several daemons have interrupted work/)).toBeInTheDocument();

    await act(async () => { fireEvent.click(screen.getAllByTestId("interrupted-banner-hide")[0]); });
    expect(screen.getAllByTestId("interrupted-banner")).toHaveLength(1);
    await act(async () => { fireEvent.keyDown(document.body, { key: "X" }); });
    expect(screen.queryByTestId("interrupted-banner")).toBeNull();
    expect(dismissCalls(calls)).toEqual([]);
  });
});

describe("a refresh that failed while a list is shown (PR #2431 round 4, Qodo)", () => {
  /** Scenario: the dialog shows three items. Closed and opened again, the daemon's answer to the new list request is an error, so the dialog still shows the three items and says the list could not be refreshed and may be out of date, with the error. Closed and opened again once the daemon answers, the sentence is gone. */
  it("says a failed refresh may leave the list out of date", async () => {
    const lists: Record<string, InterruptedList | Error> = { "process-a": three() };
    const { bridge, listed } = perProcessBridge(lists);
    render(<Overview bridge={bridge} deck={deckOf("process-a")} />);
    let dialog = await openDialog();
    expect(dialog).not.toHaveTextContent(LIST_REFRESH_FAILED);
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-close")); });

    lists["process-a"] = new Error("daemon busy");
    const before = listed.length;
    dialog = await openDialog();
    await waitFor(() => expect(listed.length).toBe(before + 1));
    await waitFor(() => expect(dialog).toHaveTextContent(LIST_REFRESH_FAILED));
    expect(dialog).toHaveTextContent("daemon busy");
    expect(rows()).toHaveLength(3);
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-close")); });

    lists["process-a"] = three();
    const again = listed.length;
    dialog = await openDialog();
    await waitFor(() => expect(listed.length).toBe(again + 1));
    await waitFor(() => expect(dialog).not.toHaveTextContent(LIST_REFRESH_FAILED));
    expect(rows()).toHaveLength(3);
  });
});

describe("a request to show interrupted work that the person moved on from (PR #2431 round 4, Qodo)", () => {
  /** A bridge whose list answers for process-b wait until the test releases them, while process-a answers at once. */
  function heldBridge(lists: Record<string, InterruptedList>) {
    const { bridge } = recordingBridge({ list: three() });
    const held: { hold: boolean; release?: (list: InterruptedList) => void } = { hold: false };
    const listed: InterruptedTarget[] = [];
    const answering = {
      ...(bridge as unknown as Record<string, unknown>),
      interruptedWorkList: vi.fn((target: InterruptedTarget) => {
        listed.push(target);
        if (target.instance === "process-b" && held.hold) {
          return new Promise((resolve) => { held.release = (list) => resolve(answered(target, list)); });
        }
        return Promise.resolve(answered(target, structuredClone(lists[target.instance] ?? { items: [] })));
      }),
    } as unknown as DeckBridge;
    return { bridge: answering, held, listed };
  }

  /** Scenario: two daemons, the first listing a unit. "Show interrupted work" asks both again; while the second has not answered, the person opens the first daemon's dialog from its banner and closes it. When the second answers with nothing, no dialog opens again. */
  it("does not reopen a dialog the person closed meanwhile", async () => {
    const { bridge, held, listed } = heldBridge({ "process-a": { items: [unit()] }, "process-b": { items: [] } });
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    held.hold = true;

    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(held.release).toBeDefined());
    const dialog = await openDialog();
    expect(dialog).toBeInTheDocument();
    await act(async () => { fireEvent.click(screen.getByTestId("interrupted-close")); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();

    await act(async () => { held.release?.({ items: [] }); await Promise.resolve(); });
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 20)); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
    expect(voice.reportRefused).not.toHaveBeenCalled();
  });

  /** Scenario: as above, but while the second daemon has not answered another modal is drawn over the dashboard. When it answers, no interrupted work dialog opens under or over that modal. */
  it("does not open a dialog while another modal covers the dashboard", async () => {
    const { bridge, held, listed } = heldBridge({ "process-a": { items: [unit()] }, "process-b": { items: [] } });
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    const view = render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    held.hold = true;

    await act(async () => { VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(held.release).toBeDefined());
    view.rerender(<voice.Voiced cover />);
    expect(screen.getByTestId("cover-modal")).toBeInTheDocument();

    await act(async () => { held.release?.({ items: [] }); await Promise.resolve(); });
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 20)); });
    expect(screen.queryByTestId("interrupted-dialog")).toBeNull();
  });

  /** Scenario (PR #2431 round 5): two daemons, and the first daemon's Interrupted work dialog is open. Its last item goes and the second daemon gains work; "show interrupted work" with no daemon named asks both again, and the open dialog turns to the second daemon's list rather than staying on the first's empty one. */
  it("retargets the Interrupted work dialog that was already open", async () => {
    const lists: Record<string, InterruptedList> = { "process-a": { items: [unit()] }, "process-b": { items: [] } };
    const { bridge, listed } = heldBridge(lists);
    const fleet = [deckOf("process-a"), remoteDeck("deck-b", "beta", "process-b")];
    const voice = voiced(bridge, fleet);
    render(<voice.Voiced />);
    await waitFor(() => expect(listed).toHaveLength(2));
    const dialog = await openDialog();
    expect(rowOf(UNIT_ID)).toBeDefined();

    lists["process-a"] = { items: [] };
    lists["process-b"] = { items: [team()] };
    const before = listed.length;
    await act(async () => { await VOICE_ACTIONS.showInterruptedWork.run(voice.context(), {}); });
    await waitFor(() => expect(listed.length).toBeGreaterThanOrEqual(before + 2));
    await waitFor(() => expect(within(screen.getByTestId("interrupted-dialog")).queryAllByTestId("interrupted-row").map((row) => row.getAttribute("data-item-id"))).toEqual([TEAM_ID]));
    expect(dialog).toBeDefined();
    expect(voice.reportRefused).not.toHaveBeenCalled();
  });
});

const RESTORE_ATTEMPT = "restore-desktop/8";
const RESTORE_OTHER = "restore-desktop/7";
const DESKTOP_RESTORE_STILL_RUNNING = "Refused: the agent's old process, or a process it started, is still running. Stop it first, so two agents do not write to one conversation.";
/** `legacy`: a Claude whose launch record an older deck wrote, without its process (review R#2). */
type DesktopRestoreKind = "resume" | "start_over" | "none" | "legacy";

function desktopRestoreItem(kind: DesktopRestoreKind = "resume", first = false): InterruptedItem {
  const base = first ? unit() : agent();
  return {
    ...base,
    name: first ? "restore-first" : "restore-second",
    agents: [{
      attempt: first ? RESTORE_OTHER : RESTORE_ATTEMPT,
      name: first ? "restore-first" : "restore-second",
      agent_type: kind === "resume" || kind === "legacy" ? "claude_code" : "pi",
      folder: base.worktree,
      restore: kind === "resume" ? { kind } : kind === "legacy" ? { kind: "start_over", reason: "old-process-unknown" } : {
        kind, reason: kind === "none" ? "orchestration-later" : "agent-cannot-resume",
      },
    }],
  };
}

// A final domain refusal keeps the row visible when the pending promise is
// released. The successful spawn/list transition is covered by the browser test.
function desktopRestoreBridge(kind: DesktopRestoreKind = "resume") {
  const state = { list: { items: [desktopRestoreItem("resume", true), desktopRestoreItem(kind)] } };
  const recorded = recordingBridge(state);
  const held: (() => void)[] = [];
  const bridge = {
    ...recorded.bridge,
    interruptedWorkRestore: vi.fn(async (target: InterruptedTarget, id: string, agents: string[], mode: RestoreMode, dryRun: boolean): Promise<InterruptedReply<RestoreOutcome>> => {
      recorded.calls.push({ verb: "restore", target, args: [id, agents, mode, dryRun] });
      await new Promise<void>((done) => { held.push(done); });
      return answered(target, { dry_run: false, restored: [], refused: [{ attempt: RESTORE_ATTEMPT, reason: "still-running", message: DESKTOP_RESTORE_STILL_RUNNING }] });
    }),
    interruptedWorkStartOver: vi.fn(async (target: InterruptedTarget, id: string, attempt: string, dryRun: boolean): Promise<InterruptedReply<StartOverOutcome>> => {
      recorded.calls.push({ verb: "start-over", target, args: [id, attempt, dryRun] });
      await new Promise<void>((done) => { held.push(done); });
      return answered(target, { dry_run: false, started: null, refused: { attempt: RESTORE_ATTEMPT, reason: "still-running", message: DESKTOP_RESTORE_STILL_RUNNING } });
    }),
  } satisfies DeckBridge;
  return {
    bridge,
    calls: recorded.calls,
    actions: () => recorded.calls.filter((call) => call.verb === "restore" || call.verb === "start-over"),
    release: () => held.splice(0).forEach((done) => done()),
  };
}

/** Every experimental surface off: restore is not behind the flag (issue #2494). */
function RestoreOverview({ bridge, voiceChannel }: { bridge: DeckBridge; voiceChannel?: VoiceOverviewChannel }) {
  const deck = deckOf("restore-process");
  const interruptedWork = useInterruptedWork(bridge, [deck]);
  return <AgentOverview runtime={{ ...baseRuntime(deck), desktopFeatures: fixtureDesktopFeatures(""), ...(interruptedWork ? { interruptedWork } : {}) }} onNavigate={vi.fn()} voiceChannel={voiceChannel} />;
}

async function restoreKey(dialog: HTMLElement, key: string) {
  await act(async () => { fireEvent.keyDown(dialog, { key }); });
}

describe("journal/restore/009 — the desktop restore question", () => {
  /** Scenario: select the second stopped agent and press s from the list or its details. The question names that agent, shows precisely its available choices and explanations, and starts nothing. */
  it.each([
    ["resume", false], ["resume", true], ["start_over", false], ["start_over", true],
  ] as const)("journal/restore/009 asks from %s, details=%s, with the TUI's words", async (kind, details) => {
    const recorded = desktopRestoreBridge(kind);
    render(<RestoreOverview bridge={recorded.bridge} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    if (details) await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    expect(within(dialog).getByTestId("interrupted-restore")).toHaveTextContent("Restore");
    await restoreKey(dialog, "s");
    const question = within(dialog).getByTestId("interrupted-restore-question");
    expect(question).toHaveTextContent("restore-second");
    if (kind === "resume") {
      expect(question).toHaveTextContent("Restore restore-second with its conversation?");
      expect(question).toHaveTextContent("It checks what already happened and carries on where it stopped.");
      expect(question).toHaveTextContent("It comes back and waits until you tell it to continue.");
      expect(within(question).getByTestId("interrupted-restore-continue")).toHaveTextContent("Restore and continue");
      expect(within(question).getByTestId("interrupted-restore-only")).toHaveTextContent("Restore only");
      expect(within(question).queryByTestId("interrupted-start-over")).toBeNull();
    } else {
      expect(question).toHaveTextContent("Start restore-second over in this worktree, without its conversation?");
      expect(question).toHaveTextContent("The agent cannot be restored with its conversation yet.");
      expect(within(question).getByTestId("interrupted-start-over")).toHaveTextContent("Start over in this worktree");
      expect(within(question).queryByTestId("interrupted-restore-continue")).toBeNull();
      expect(within(question).queryByTestId("interrupted-restore-only")).toBeNull();
    }
    for (const unavailable of kind === "resume" ? ["w"] : ["c", "o"]) {
      await restoreKey(dialog, unavailable);
      expect(within(dialog).getByTestId("interrupted-restore-question")).toBeInTheDocument();
    }
    expect(recorded.actions()).toEqual([]);
  });

  /** Scenario: ask about the second item, then confirm by c, o or w, or the matching button. While its answer is held, more confirm keys and clicks send no second request, and the first request names the selected item's attempt and serving daemon. */
  it.each([
    ["resume", "c", "interrupted-restore-continue", "continue"],
    ["resume", "o", "interrupted-restore-only", "only"],
    ["start_over", "w", "interrupted-start-over", undefined],
  ] as const)("journal/restore/009 confirms %s with %s exactly once while pending", async (kind, key, testId, mode) => {
    for (const byButton of [false, true]) {
      const recorded = desktopRestoreBridge(kind);
      const mounted = render(<RestoreOverview bridge={recorded.bridge} />);
      const dialog = await openDialog();
      await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
      await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-restore")); });
      const question = within(dialog).getByTestId("interrupted-restore-question");
      // Match the Stage 2 presentation barrier. A key burst must not confirm a
      // question that has not painted; use its established data-presented seam.
      await waitFor(() => expect(question).toHaveAttribute("data-presented", "true"));
      const button = within(question).getByTestId(testId);
      if (byButton) await act(async () => { fireEvent.click(button); });
      else await restoreKey(dialog, key);
      await waitFor(() => expect(recorded.actions()).toHaveLength(1));
      expect(button).toBeDisabled();
      for (const again of [key, "c", "o", "w", "s"]) await restoreKey(dialog, again);
      await act(async () => { fireEvent.click(button); });
      const target = { deckId: deckOf("restore-process").connection.deckId!, instance: "restore-process" };
      expect(recorded.actions()).toEqual([{
        verb: mode ? "restore" : "start-over", target,
        args: mode ? [AGENT_ID, [RESTORE_ATTEMPT], mode, false] : [AGENT_ID, RESTORE_ATTEMPT, false],
      }]);
      await act(async () => { recorded.release(); });
      expect(await within(dialog).findByTestId("interrupted-message")).toHaveTextContent(DESKTOP_RESTORE_STILL_RUNNING);
      mounted.unmount();
    }
  });

  /** Scenario: s opens a question, and Escape, q, n or Back returns to the list with the stopped agents still listed. A choice that was not offered sends nothing. */
  it.each(["Escape", "q", "n", "button"])("journal/restore/009 backs out with %s without acting", async (back) => {
    const recorded = desktopRestoreBridge();
    render(<RestoreOverview bridge={recorded.bridge} />);
    const dialog = await openDialog();
    await restoreKey(dialog, "s");
    await restoreKey(dialog, "w");
    expect(recorded.actions()).toEqual([]);
    if (back === "button") await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });
    else await restoreKey(dialog, back);
    expect(within(dialog).queryByTestId("interrupted-restore-question")).toBeNull();
    expect(within(dialog).getAllByTestId("interrupted-row")).toHaveLength(2);
    expect(recorded.actions()).toEqual([]);
  });

  /** Scenario: s and c arrive before the restore question is presented. The question remains on screen and no restore is sent until the user sees it and confirms again. */
  it("journal/restore/009 does not confirm a question in the opening key burst", async () => {
    const recorded = desktopRestoreBridge();
    render(<RestoreOverview bridge={recorded.bridge} />);
    const dialog = await openDialog();
    await act(async () => {
      for (const key of ["s", "c", "c"]) fireEvent.keyDown(dialog, { key });
    });
    expect(within(dialog).getByTestId("interrupted-restore-question")).toBeInTheDocument();
    expect(recorded.actions()).toEqual([]);
  });

  /** Scenario: select a Claude that an older deck started without recording its process, and press s. The question explains that and offers Start over only: c and o send nothing, and w sends one start over of that attempt. */
  it("journal/restore/009 offers only Start over for an agent an older deck started", async () => {
    const recorded = desktopRestoreBridge("legacy");
    render(<RestoreOverview bridge={recorded.bridge} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await restoreKey(dialog, "s");
    const question = within(dialog).getByTestId("interrupted-restore-question");
    expect(question).toHaveTextContent("Start restore-second over in this worktree, without its conversation?");
    expect(question).toHaveTextContent("An older version of the deck started this agent, so the deck cannot tell whether its old process is gone.");
    expect(within(question).getByTestId("interrupted-start-over")).toHaveTextContent("Start over in this worktree");
    expect(within(question).queryByTestId("interrupted-restore-continue")).toBeNull();
    expect(within(question).queryByTestId("interrupted-restore-only")).toBeNull();
    await waitFor(() => expect(question).toHaveAttribute("data-presented", "true"));
    for (const unavailable of ["c", "o"]) await restoreKey(dialog, unavailable);
    expect(recorded.actions()).toEqual([]);
    await restoreKey(dialog, "w");
    await waitFor(() => expect(recorded.actions()).toHaveLength(1));
    expect(recorded.actions()[0]).toMatchObject({ verb: "start-over", args: [AGENT_ID, RESTORE_ATTEMPT, false] });
    await act(async () => { recorded.release(); });
  });

  /** Scenario: the dialog lists an orchestration agent that cannot yet come back. Its reason is visible in details, and s never offers a resume or Start-over confirm for that agent. */
  it("journal/restore/009 does not offer a confirm for a none verdict", async () => {
    const recorded = desktopRestoreBridge("none");
    render(<RestoreOverview bridge={recorded.bridge} />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    expect(dialog).toHaveTextContent("Orchestrations come back in a later version.");
    await restoreKey(dialog, "s");
    expect(within(dialog).queryByTestId("interrupted-restore-continue")).toBeNull();
    expect(within(dialog).queryByTestId("interrupted-restore-only")).toBeNull();
    expect(within(dialog).queryByTestId("interrupted-start-over")).toBeNull();
    expect(recorded.actions()).toEqual([]);
  });
});

describe("journal/restore/010 — spoken restore asks and stops", () => {
  /** Scenario: with the interrupted dialog open, voice chooses the second stopped item by its name, place or current selection. The named question appears and no agent is started before a person's confirmation. */
  it.each([undefined, "restore second", "the second one"])("journal/restore/010 asks about the selected/spoken item: %s", async (said) => {
    const recorded = desktopRestoreBridge();
    const voice = voiced(recorded.bridge);
    render(<voice.Voiced />);
    const dialog = await openDialog();
    if (said === undefined) await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), said === undefined ? {} : { filterText: said }); });
    expect(within(dialog).getByTestId("interrupted-restore-question")).toHaveTextContent("Restore restore-second with its conversation?");
    expect(voice.reportRefused).not.toHaveBeenCalled();
    expect(recorded.actions()).toEqual([]);
    // Repeating the spoken ask is still not a confirm.
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), {}); });
    expect(recorded.actions()).toEqual([]);
  });

  /** Scenario: a spoken restore naming an absent item starts nothing and opens no confirmation. The voice surface reports why the request was refused. */
  it("journal/restore/010 refuses an unknown item", async () => {
    const recorded = desktopRestoreBridge();
    const voice = voiced(recorded.bridge);
    render(<voice.Voiced />);
    const dialog = await openDialog();
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), { filterText: "missing item" }); });
    expect(voice.reportRefused).toHaveBeenCalledWith(expect.any(String));
    expect(within(dialog).queryByTestId("interrupted-restore-question")).toBeNull();
    expect(recorded.actions()).toEqual([]);
  });
});

describe("journal/restore/044 — an item that lists no agent offers no restore", () => {
  /** Scenario: open Interrupted work on items that list no agent and select each, in the list and in its details. No Restore button is drawn and s opens no question, as the TUI offers no s for such an item. */
  it.each([false, true])("journal/restore/044 hides Restore and leaves s inert, details=%s", async (details) => {
    const { bridge, calls } = recordingBridge({ list: three() });
    render(<RestoreOverview bridge={bridge} />);
    const dialog = await openDialog();
    for (const id of [AGENT_ID, UNIT_ID, TEAM_ID]) {
      await act(async () => { fireEvent.click(rowOf(id)); });
      if (details) await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
      expect(within(dialog).queryByTestId("interrupted-restore")).toBeNull();
      await restoreKey(dialog, "s");
      expect(within(dialog).queryByTestId("interrupted-restore-question")).toBeNull();
      if (details) await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });
    }
    expect(calls.filter((call) => call.verb === "restore" || call.verb === "start-over")).toEqual([]);
  });

  /** Scenario: with an item that lists no agent selected, voice asks to restore it. No question opens, nothing is started, and the voice surface answers that the item lists no agent to restore. */
  it("journal/restore/044 a spoken restore answers that there is no agent", async () => {
    const { bridge, calls } = recordingBridge({ list: three() });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(UNIT_ID)); });
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), {}); });
    expect(voice.reportRefused).toHaveBeenCalledWith(NO_AGENT_TO_RESTORE);
    expect(within(dialog).queryByTestId("interrupted-restore-question")).toBeNull();
    expect(calls.filter((call) => call.verb === "restore" || call.verb === "start-over")).toEqual([]);
  });
});

/** `recordingBridge` whose report pages and reclaim dry runs wait until released, so a spoken ask can arrive while one is pending. */
function heldPagesBridge(state: { list: InterruptedList }, restore?: () => InterruptedReply<RestoreOutcome>) {
  const recorded = recordingBridge(state);
  const base = recorded.bridge as unknown as Record<string, (...args: unknown[]) => Promise<unknown>>;
  const held: (() => void)[] = [];
  const hold = <A extends unknown[]>(verb: string) => vi.fn(async (...args: A) => {
    await new Promise<void>((done) => { held.push(done); });
    return base[verb](...args);
  });
  const bridge = {
    ...base,
    interruptedWorkShow: hold("interruptedWorkShow"),
    interruptedWorkReclaim: hold("interruptedWorkReclaim"),
    ...(restore ? { interruptedWorkRestore: vi.fn(async () => restore()) } : {}),
  } as unknown as DeckBridge;
  return { bridge, calls: recorded.calls, release: async () => { await act(async () => { held.splice(0).forEach((done) => done()); }); } };
}

const isSelected = (id: string) => rowOf(id).classList.contains("is-selected");

describe("a refused spoken restore leaves the open dialog as it was (PR #2497 review, Qodo)", () => {
  /** Scenario: open the fix-login unit's details and ask for its reports; while the page is loading, voice asks to restore scratch-shell, an item that lists no agent. The voice surface answers that there is no agent, the report page still arrives, and fix-login stays selected. */
  it("journal/restore/044 keeps a loading report page and the selection", async () => {
    const { bridge, calls, release } = heldPagesBridge({ list: three() });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(UNIT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), { filterText: "scratch-shell" }); });
    expect(voice.reportRefused).toHaveBeenCalledWith(NO_AGENT_TO_RESTORE);
    expect(within(dialog).queryByTestId("interrupted-restore-question")).toBeNull();
    await release();
    expect(await within(dialog).findByTestId("interrupted-report-body")).toHaveTextContent("[UNTRUSTED-WORKER-REPORT:");
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });
    expect(isSelected(UNIT_ID)).toBe(true);
    expect(isSelected(AGENT_ID)).toBe(false);
    expect(calls.filter((call) => call.verb === "restore" || call.verb === "start-over")).toEqual([]);
  });

  /** Scenario: ask to reclaim the fix-login unit; while its dry run is pending, voice asks to restore scratch-shell, an item that lists no agent. The ask is refused with the wait sentence, the reclaim question still gets its dry-run answer, and fix-login stays selected. */
  it("journal/restore/044 keeps a reclaim question's pending dry run and the selection", async () => {
    const { bridge, release } = heldPagesBridge({ list: three() });
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(UNIT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-reclaim")); });
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), { filterText: "scratch-shell" }); });
    expect(voice.reportRefused).toHaveBeenCalledWith(INTERRUPTED_WAIT);
    expect(within(dialog).queryByTestId("interrupted-restore-question")).toBeNull();
    await release();
    expect(await within(dialog).findByTestId("interrupted-reclaim-verdict")).toHaveTextContent(RECLAIM_WOULD_REMOVE);
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });
    expect(isSelected(UNIT_ID)).toBe(true);
    expect(isSelected(AGENT_ID)).toBe(false);
  });

  /** Scenario: a restore answered that the daemon does not restore agents; then, while restore-first's report page is loading in its details, voice asks to restore restore-second. The dialog says the daemon does not restore agents, and the details stay open, get their page, and keep restore-first selected. */
  it("journal/restore/010 a daemon that does not restore agents keeps a loading report page and the selection", async () => {
    const state = { list: { items: [desktopRestoreItem("resume", true), desktopRestoreItem("resume")] } };
    const { bridge, release } = heldPagesBridge(state, () => ({ outcome: "unsupported" }));
    const voice = voiced(bridge);
    render(<voice.Voiced />);
    const dialog = await openDialog();
    await act(async () => { fireEvent.click(rowOf(AGENT_ID)); });
    await restoreKey(dialog, "s");
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-restore-question")).toHaveAttribute("data-presented", "true"));
    await restoreKey(dialog, "c");
    await waitFor(() => expect(within(dialog).getByTestId("interrupted-message")).toHaveTextContent(RESTORE_UNSUPPORTED));
    await act(async () => { fireEvent.click(rowOf(UNIT_ID)); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-details")); });
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-show-reports")); });
    await act(async () => { VOICE_ACTIONS.askRestoreInterrupted.run(voice.context(), { filterText: "restore-second" }); });
    expect(voice.reportRefused).toHaveBeenCalledWith(RESTORE_UNSUPPORTED);
    expect(within(dialog).getByTestId("interrupted-message")).toHaveTextContent(RESTORE_UNSUPPORTED);
    await release();
    expect(await within(dialog).findByTestId("interrupted-report-body")).toHaveTextContent("[UNTRUSTED-WORKER-REPORT:");
    await act(async () => { fireEvent.click(within(dialog).getByTestId("interrupted-back")); });
    expect(isSelected(UNIT_ID)).toBe(true);
    expect(isSelected(AGENT_ID)).toBe(false);
  });
});
