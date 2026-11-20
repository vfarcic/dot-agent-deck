import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { StrictMode } from "react";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DeckBridge, type DesktopSettingsDto } from "../lib/bridge";
import { AGENTS_STOPPED_BY_RESTART } from "../lib/agentIdentity";
import {
  DISMISSED, INTERRUPTED_WORK_TITLE, LIST_INCOMPLETE, NOTHING_INTERRUPTED, OTHER_AGENTS_GROUP, RECLAIM_KEPT_DIRTY, RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE, REFUSED_AMBIGUOUS, REFUSED_INCOMPLETE_READ, REFUSED_NOT_LISTED, dismissAllQuestion, interruptedFooter, reportRangeSentence,
  type DismissOutcome, type InterruptedItem, type InterruptedList, type InterruptedReply, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome,
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
