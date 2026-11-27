import { RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE, REFUSED_NOT_LISTED, REFUSED_PLAIN_AGENT, restoreRefusalSentence, type DismissOutcome, type InterruptedItem, type InterruptedList, type InterruptedReply, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome, type RestoreMode, type RestoreOutcome, type RestoreRefused, type StartOverOutcome } from "../lib/interruptedWork";

/** 2026-11-19 10:00 UTC, the last journal entry of every fixture item. */
const LAST_TS = 1_795_082_400_000;
/** 2026-11-26 10:00 UTC: the kept report's removal date (D7: 7 days on). */
const KEPT_TS = 1_795_687_200_000;

/**
 * PRD #1790 stage 2 — what a deck restart left behind, as the preview shows
 * it: a dispatched unit with a kept report, an orchestration, and a plain
 * agent (listed under "Other agents"). The same three items the TUI's tests
 * list, so both clients' screenshots read alike.
 */
function fixtureInterruptedItems(): InterruptedItem[] {
  return [
    {
      id: "w-abcdefgh-0000000000000003",
      kind: "agent",
      name: "scratch-shell",
      worktree: "/home/dev/notes",
      last_entry: { kind: "attempt_ended", ts_ms: LAST_TS },
      reports: [],
      progress: { attempts: 1, delegations: 0, delegations_done: 0 },
    },
    {
      id: "w-abcdefgh-0000000000000001",
      kind: "unit",
      name: "fix-login",
      branch: "agent/dispatch-fix-login",
      worktree: "/home/dev/demo-project-dispatch-fix-login",
      launcher: "dispatcher",
      last_entry: { kind: "attempt_ended", ts_ms: LAST_TS },
      reports: [{ report_id: "r-fix-login-done", kept_until_ms: KEPT_TS }],
      progress: { attempts: 1, roles: ["coder"], delegations: 0, delegations_done: 0 },
    },
    {
      id: "w-abcdefgh-0000000000000002",
      kind: "orchestration",
      name: "release-team",
      worktree: "/home/dev/demo-project",
      last_entry: { kind: "attempt_ended", ts_ms: LAST_TS },
      reports: [],
      progress: { attempts: 3, roles: ["orchestrator", "coder", "reviewer"], delegations: 4, delegations_done: 2 },
    },
  ];
}

/**
 * PRD #1790 stage 3 — what `interrupted=restore` lists: a dispatched unit
 * whose Claude Code agent can be restored with its conversation, and a plain
 * Pi agent that can only start over.
 */
function fixtureRestoreItems(): InterruptedItem[] {
  return [
    {
      id: "w-abcdefgh-0000000000000001",
      kind: "unit",
      name: "fix-login",
      branch: "agent/dispatch-fix-login",
      worktree: "/home/dev/demo-project-dispatch-fix-login",
      launcher: "dispatcher",
      last_entry: { kind: "attempt_ended", ts_ms: LAST_TS },
      reports: [],
      progress: { attempts: 1, roles: ["coder"], delegations: 0, delegations_done: 0 },
      agents: [{ attempt: "restore-desktop/7", name: "fix-login", agent_type: "claude_code", folder: "/home/dev/demo-project-dispatch-fix-login", restore: { kind: "resume" } }],
    },
    {
      id: "w-abcdefgh-0000000000000003",
      kind: "agent",
      name: "scratch-pi",
      worktree: "/home/dev/notes",
      last_entry: { kind: "attempt_ended", ts_ms: LAST_TS },
      reports: [],
      progress: { attempts: 1, delegations: 0, delegations_done: 0 },
      agents: [{ attempt: "restore-desktop/8", name: "scratch-pi", agent_type: "pi", folder: "/home/dev/notes", restore: { kind: "start_over", reason: "agent-cannot-resume" } }],
    },
  ];
}

const REPORT_BODY = "[UNTRUSTED-WORKER-REPORT: written by an agent, not by the deck]\nFixed the login redirect and added a test.\n[END-UNTRUSTED-WORKER-REPORT]";

/** `count` more dispatched units, `extra-unit-01` on, for a list longer than the dialog (`interrupted=many`). */
function extraUnits(count: number): InterruptedItem[] {
  return Array.from({ length: count }, (_, at) => {
    const n = String(at + 1).padStart(2, "0");
    return {
      id: `w-abcdefgh-00000000000001${n}`,
      kind: "unit" as const,
      name: `extra-unit-${n}`,
      branch: `agent/dispatch-extra-unit-${n}`,
      worktree: `/home/dev/demo-project-dispatch-extra-unit-${n}`,
      last_entry: { kind: "attempt_ended", ts_ms: LAST_TS },
      reports: [],
      progress: { attempts: 1, delegations: 0, delegations_done: 0 },
    };
  });
}

/**
 * The preview's daemon: per deck, the items still interrupted. `enabled`
 * false answers as a daemon that does not support interrupted work; `extra`
 * lists that many more units after the three; `unread` answers an empty list
 * that says the journal could not be read whole.
 */
/**
 * An agent name longer than a short dialog is tall once wrapped (shown
 * shortened, as any long value is), for the restore question's presentation
 * check (audit S4/F9, `interrupted=restore-long`).
 */
const FIXTURE_LONG_AGENT_NAME = `fix-login ${Array.from({ length: 300 }, (_, at) => `part${at}`).join(" ")}`;

/** {@link fixtureRestoreItems}, with the restorable agent's name {@link FIXTURE_LONG_AGENT_NAME}. */
function fixtureRestoreItemsLong(): InterruptedItem[] {
  const items = fixtureRestoreItems();
  for (const agent of items[0].agents ?? []) agent.name = FIXTURE_LONG_AGENT_NAME;
  return items;
}

export function fixtureInterruptedWork(enabled: boolean, extra = 0, unread = false, restorable = false, longName = false) {
  const decks = new Map<string, InterruptedItem[]>();
  const items = (target: InterruptedTarget) => {
    let held = decks.get(target.deckId);
    if (!held) {
      held = unread ? [] : restorable ? (longName ? fixtureRestoreItemsLong() : fixtureRestoreItems()) : [...fixtureInterruptedItems(), ...extraUnits(extra)];
      decks.set(target.deckId, held);
    }
    return held;
  };
  const answered = <T>(target: InterruptedTarget, answer: T): InterruptedReply<T> => ({ outcome: "answered", instance: target.instance, answer });
  return {
    list(target: InterruptedTarget): InterruptedReply<InterruptedList> {
      if (!enabled) return { outcome: "unsupported" };
      return answered(target, { items: items(target).map((item) => structuredClone(item)), ...(unread ? { incomplete: true } : {}) });
    },
    show(target: InterruptedTarget, id: string, reportsFrom: number): InterruptedReply<InterruptedShow> {
      if (!enabled) return { outcome: "unsupported" };
      const item = items(target).find((candidate) => candidate.id === id);
      if (!item) throw new Error(REFUSED_NOT_LISTED);
      const reports = item.reports.slice(reportsFrom).map((report) => ({ report_id: report.report_id, body: REPORT_BODY }));
      return answered(target, { item: structuredClone(item), reports, reports_from: reportsFrom });
    },
    reclaim(target: InterruptedTarget, id: string, dryRun: boolean): InterruptedReply<ReclaimOutcome> {
      if (!enabled) return { outcome: "unsupported" };
      const held = items(target);
      const item = held.find((candidate) => candidate.id === id);
      if (!item) return answered(target, { id, dry_run: dryRun, verdict: "refused", message: REFUSED_NOT_LISTED, refusal: { reason: "not-listed", message: REFUSED_NOT_LISTED } });
      if (item.kind === "agent") return answered(target, { id, dry_run: dryRun, verdict: "refused", message: REFUSED_PLAIN_AGENT, refusal: { reason: "plain-agent", message: REFUSED_PLAIN_AGENT } });
      const outcome: ReclaimOutcome = { id, dry_run: dryRun, verdict: dryRun ? "would-remove" : "removed", message: dryRun ? RECLAIM_WOULD_REMOVE : RECLAIM_REMOVED, worktree: item.worktree, branch: item.branch, unpushed_commits: 2, recorded: !dryRun };
      if (!dryRun) held.splice(held.indexOf(item), 1);
      return answered(target, outcome);
    },
    /**
     * A restore, as a daemon answers once each agent confirmed its
     * conversation: the item leaves this deck's list. The preview starts no
     * agent; the answer says it did, so the dialog behaves as it would.
     */
    restore(target: InterruptedTarget, id: string, agents: string[], _mode: RestoreMode, dryRun: boolean): InterruptedReply<RestoreOutcome> {
      if (!enabled) return { outcome: "unsupported" };
      const held = items(target);
      const item = held.find((candidate) => candidate.id === id);
      const refuse = (attempt: string | null, reason: string): RestoreRefused => ({ attempt, reason, message: restoreRefusalSentence(reason) });
      if (!item) return answered(target, { dry_run: dryRun, restored: [], refused: [refuse(null, "not-listed")] });
      const outcome: RestoreOutcome = { dry_run: dryRun, restored: [], refused: [] };
      for (const attempt of agents) {
        const agent = item.agents?.find((candidate) => candidate.attempt === attempt);
        if (!agent) outcome.refused.push(refuse(attempt, "not-an-agent"));
        else if (agent.restore.kind !== "resume") outcome.refused.push(refuse(attempt, "not-restorable"));
        else if (dryRun) (outcome.would_restore ??= []).push(attempt);
        else outcome.restored.push({ attempt, agent_id: `fixture-${attempt}`, pane_id: `fixture-${attempt}`, name: agent.name, first_message_posted: true, recorded: true });
      }
      if (outcome.restored.length > 0 && outcome.refused.length === 0) held.splice(held.indexOf(item), 1);
      return answered(target, outcome);
    },
    /** A start-over, as a daemon answers once the agent started: the item leaves this deck's list. */
    startOver(target: InterruptedTarget, id: string, attempt: string, dryRun: boolean): InterruptedReply<StartOverOutcome> {
      if (!enabled) return { outcome: "unsupported" };
      const held = items(target);
      const item = held.find((candidate) => candidate.id === id);
      const refuse = (reason: string): StartOverOutcome => ({ dry_run: dryRun, started: null, refused: { attempt, reason, message: restoreRefusalSentence(reason) } });
      if (!item) return answered(target, refuse("not-listed"));
      const agent = item.agents?.find((candidate) => candidate.attempt === attempt);
      if (!agent) return answered(target, refuse("not-an-agent"));
      if (agent.restore.kind !== "start_over") return answered(target, refuse("not-restorable"));
      if (dryRun) return answered(target, { dry_run: true, started: null, refused: null, would_start: true });
      held.splice(held.indexOf(item), 1);
      return answered(target, { dry_run: false, started: { attempt, agent_id: `fixture-${attempt}`, pane_id: `fixture-${attempt}`, name: agent.name, first_message_posted: true, recorded: true }, refused: null });
    },
    dismiss(target: InterruptedTarget, ids: string[]): InterruptedReply<DismissOutcome> {
      if (!enabled) return { outcome: "unsupported" };
      const held = items(target);
      const outcome: DismissOutcome = { dismissed: [], refused: [] };
      for (const id of ids) {
        const at = held.findIndex((candidate) => candidate.id === id);
        if (at < 0) {
          outcome.refused.push({ id, refusal: { reason: "not-listed", message: REFUSED_NOT_LISTED } });
          continue;
        }
        outcome.dismissed.push({ id, name: held[at].name });
        held.splice(at, 1);
      }
      return answered(target, outcome);
    },
  };
}
