import { RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE, REFUSED_NOT_LISTED, REFUSED_PLAIN_AGENT, type DismissOutcome, type InterruptedItem, type InterruptedList, type InterruptedReply, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome } from "../lib/interruptedWork";

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
export function fixtureInterruptedItems(): InterruptedItem[] {
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
 * lists that many more units after the three.
 */
export function fixtureInterruptedWork(enabled: boolean, extra = 0) {
  const decks = new Map<string, InterruptedItem[]>();
  const items = (target: InterruptedTarget) => {
    let held = decks.get(target.deckId);
    if (!held) {
      held = [...fixtureInterruptedItems(), ...extraUnits(extra)];
      decks.set(target.deckId, held);
    }
    return held;
  };
  const answered = <T>(target: InterruptedTarget, answer: T): InterruptedReply<T> => ({ outcome: "answered", instance: target.instance, answer });
  return {
    list(target: InterruptedTarget): InterruptedReply<InterruptedList> {
      if (!enabled) return { outcome: "unsupported" };
      return answered(target, { items: items(target).map((item) => structuredClone(item)) });
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
