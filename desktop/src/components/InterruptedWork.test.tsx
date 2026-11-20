import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DeckBridge, type DesktopSettingsDto } from "../lib/bridge";
import { AGENTS_STOPPED_BY_RESTART } from "../lib/agentIdentity";
import {
  DISMISSED, INTERRUPTED_WORK_TITLE, OTHER_AGENTS_GROUP, RECLAIM_KEPT_DIRTY, RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE, dismissAllQuestion, interruptedFooter,
  type DismissOutcome, type InterruptedItem, type InterruptedList, type InterruptedReply, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome,
} from "../lib/interruptedWork";
import { VOICE_ACTIONS, INTERRUPTED_RECLAIM_CHECKING, type VoiceOverviewChannel, type VoiceOverviewContext } from "../lib/voiceActions";
import { useInterruptedWork } from "../hooks/useInterruptedWork";
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

  /** Scenario: showing, reclaiming, dismissing and dismissing all are spoken commands, each a `voice: true` registry entry with its row in the command table; the two confirms, the selection, the details, the report pages, Back, Close and Hide are registry entries that say why they are not. */
  it("registers every control, with a row for each spoken one", () => {
    const spoken = { show_interrupted_work: "showInterruptedWork", reclaim_interrupted: "reclaimInterrupted", dismiss_interrupted: "dismissInterrupted", dismiss_all_interrupted: "dismissAllInterrupted" } as const;
    for (const [id, invoke] of Object.entries(spoken)) {
      expect((VOICE_ACTIONS as Record<string, { voice?: true }>)[invoke].voice, invoke).toBe(true);
      const row = rowFor(id);
      expect(row, id).toBeDefined();
      expect(row).toMatch(new RegExp(`^invoke\\s*=\\s*"${invoke}"`, "m"));
      expect(row).toMatch(/^screens\s*=\s*\["overview"\]/m);
    }
    for (const invoke of ["confirmReclaimInterrupted", "confirmDismissAllInterrupted", "selectInterruptedItem", "showInterruptedDetails", "showInterruptedReports", "backInterruptedWork", "closeInterruptedWork", "hideInterruptedWork"]) {
      expect((VOICE_ACTIONS as Record<string, { no_voice?: string }>)[invoke].no_voice?.length ?? 0, invoke).toBeGreaterThan(20);
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
