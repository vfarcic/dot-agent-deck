import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MutableRefObject } from "react";
import { History, X } from "lucide-react";
import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { InterruptedDeck, InterruptedWorkRuntime } from "../hooks/useInterruptedWork";
import {
  DAEMON_FAILED, DISMISS_ACTION, DISMISS_ALL_ACTION, INTERRUPTED_WORK_TITLE, ITEM_CUT, LABEL_BRANCH, LABEL_GIT, LABEL_ISSUE, LABEL_LAST_ENTRY, LABEL_NESTED_WORKTREE, LABEL_PROGRESS, LABEL_REPORT, LABEL_STARTED_BY, LABEL_WORKTREE,
  LIST_INCOMPLETE, NOTHING_INTERRUPTED, OTHER_AGENTS_GROUP, RECLAIM_ACTION, RECLAIM_EXPLANATION, REFUSED_AMBIGUOUS, REFUSED_NOT_LISTED, REFUSED_PLAIN_AGENT, REPORT_CUT, REPORT_UNREADABLE, UNSUPPORTED,
  directoryLabel, dismissAllQuestion, dismissMessage, displayValue, formatTime, gitFactsLine, interruptedFooter, itemName, keptUntil, kindLabel, lastEntryLine, omittedSentence, orderedItems, progressFact, progressLine,
  reclaimMessage, reportLines, reportRangeSentence, unpushedSentence, wouldRemove,
  type InterruptedDialogControls, type InterruptedItem, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome,
} from "../lib/interruptedWork";
import { ordinal, spokenWords } from "../lib/voiceChoice";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";

/**
 * PRD #1790 stage 2 — the banner on a deck's section of the dashboard while
 * its daemon lists interrupted work: the TUI footer row's words, with Show
 * and Hide. Hide records nothing; the banner comes back with another daemon
 * process.
 */
export function InterruptedWorkBanner({ deckId, count, controls }: { deckId: string; count: number; controls: Pick<VoiceActionContext, "showInterruptedWork" | "hideInterruptedWork"> }) {
  return (
    <div className="interrupted-banner" data-testid="interrupted-banner" role="status">
      <History size={14} aria-hidden="true" />
      <span data-testid="interrupted-banner-message">{interruptedFooter(count)}</span>
      <button type="button" className="button secondary compact" data-testid="interrupted-banner-show" onClick={() => VOICE_ACTIONS.showInterruptedWork.run(controls, { preselectDeckId: deckId })}>Show</button>
      <button type="button" className="button secondary compact" data-testid="interrupted-banner-hide" onClick={() => VOICE_ACTIONS.hideInterruptedWork.run(controls, deckId)}>Hide</button>
    </div>
  );
}

/** What the dialog shows. */
type View =
  | { kind: "list" }
  | { kind: "details"; id: string; show?: InterruptedShow; asked: boolean; failed?: string }
  | { kind: "reclaim"; id: string; dry?: ReclaimOutcome }
  | { kind: "dismissAll"; ids: string[] };

/** A reclaim or dismissal in flight: the actions wait for its answer. */
interface Pending { op: number; dryRun: boolean }

/** Why a request failed, as the TUI says it. */
function failure(error: unknown): string {
  return `${DAEMON_FAILED} ${displayValue(error instanceof Error ? error.message : String(error))}`;
}

/** Back out of a view to the list, as the TUI's Esc and `n` do. */
const LIST: View = { kind: "list" };

/**
 * PRD #1790 stage 2 — the "Interrupted work" dialog for one deck: its items,
 * work first and plain agents under "Other agents" (decision D1), an item's
 * details and kept reports' text on demand, reclaim with a dry run and a
 * confirm offered only for a would-remove verdict, dismiss, and dismiss all
 * with its question. Mounted per daemon process (its key names it), so an
 * answer from a process the deck has left never reaches it.
 */
export function InterruptedWorkDialog({ deckLabel, deck, target, runtime, controlsRef }: { deckLabel: string; deck: InterruptedDeck; target: InterruptedTarget; runtime: InterruptedWorkRuntime; controlsRef?: MutableRefObject<InterruptedDialogControls | undefined> }) {
  const ordered = useMemo(() => orderedItems(deck.list), [deck.list]);
  const [selectedId, setSelectedId] = useState<string | undefined>(() => ordered[0]?.id);
  const lastIndex = useRef(0);
  const [view, setView] = useState<View>(LIST);
  const [message, setMessage] = useState<string | undefined>();
  const [pending, setPending] = useState<Pending | undefined>();
  const ops = useRef({ last: 0, pending: undefined as number | undefined, show: undefined as number | undefined });
  const mounted = useRef(true);
  useEffect(() => () => { mounted.current = false; }, []);

  /* The selection follows its item; one that went is replaced by the row that took its place, or the last. */
  const index = selectedId === undefined ? -1 : ordered.findIndex((item) => item.id === selectedId);
  const selectedIndex = index >= 0 ? index : Math.min(lastIndex.current, Math.max(ordered.length - 1, 0));
  const selected: InterruptedItem | undefined = ordered[selectedIndex];
  useEffect(() => { lastIndex.current = selectedIndex; });
  const item = (id: string) => deck.list?.items.find((candidate) => candidate.id === id);
  /* An item that is no longer listed takes its details or confirmation with it, unless an action on it is in flight. */
  const gone = (view.kind === "details" || view.kind === "reclaim") && item(view.id) === undefined;
  useEffect(() => {
    if (!gone || pending) return;
    ops.current.show = undefined;
    setView(LIST);
  }, [gone, pending]);

  const busy = pending !== undefined;
  const nextOp = () => ++ops.current.last;
  const settle = (op: number) => {
    if (!mounted.current || ops.current.pending !== op) return false;
    ops.current.pending = undefined;
    setPending(undefined);
    return true;
  };
  const begin = (dryRun: boolean) => {
    const op = nextOp();
    ops.current.pending = op;
    setPending({ op, dryRun });
    return op;
  };
  const abandonDryRun = () => {
    if (pending?.dryRun) {
      ops.current.pending = undefined;
      setPending(undefined);
    }
  };

  /** The item voice named — an ordinal ("the second one"), or its name, branch or id — or the selected one. */
  const resolve = (said?: string): InterruptedItem | string => {
    if (said === undefined || said.trim() === "") return selected ?? REFUSED_NOT_LISTED;
    const position = ordinal(spokenWords(said));
    if (position === "last") return ordered.at(-1) ?? REFUSED_NOT_LISTED;
    if (typeof position === "number") return ordered[position - 1] ?? REFUSED_NOT_LISTED;
    const wanted = said.trim().toLowerCase();
    const matches = ordered.filter((candidate) => [candidate.name, candidate.branch, candidate.id].some((value) => value?.toLowerCase().includes(wanted)));
    if (matches.length === 1) return matches[0];
    return matches.length > 1 ? REFUSED_AMBIGUOUS : REFUSED_NOT_LISTED;
  };

  const reclaim = (said?: string): string | Promise<string | undefined> => {
    if (busy) return "Wait for the last action to finish.";
    const chosen = resolve(said);
    if (typeof chosen === "string") return chosen;
    if (chosen.kind === "agent") return REFUSED_PLAIN_AGENT;
    setSelectedId(chosen.id);
    setMessage(undefined);
    setView({ kind: "reclaim", id: chosen.id });
    const op = begin(true);
    return runtime.reclaim(target, chosen.id, true).then((reply) => {
      if (!settle(op)) return undefined;
      if (reply.outcome === "unsupported") {
        setMessage(UNSUPPORTED);
        setView(LIST);
        return UNSUPPORTED;
      }
      setView((open) => (open.kind === "reclaim" && open.id === chosen.id ? { ...open, dry: reply.answer } : open));
      return reclaimMessage(reply.answer);
    }, (error: unknown) => {
      if (!settle(op)) return undefined;
      const why = failure(error);
      setMessage(why);
      setView(LIST);
      return why;
    });
  };

  const confirmReclaim = (): string | undefined => {
    if (view.kind !== "reclaim" || !view.dry || !wouldRemove(view.dry) || busy) return "There is no reclaim to confirm.";
    const id = view.id;
    const op = begin(false);
    void runtime.reclaim(target, id, false).then((reply) => {
      if (!settle(op)) return;
      setMessage(reply.outcome === "answered" ? reclaimMessage(reply.answer) : UNSUPPORTED);
      setView((open) => (open.kind === "reclaim" && open.id === id ? LIST : open));
      runtime.refresh(target.deckId);
    }, (error: unknown) => {
      if (!settle(op)) return;
      setMessage(failure(error));
      setView((open) => (open.kind === "reclaim" && open.id === id ? LIST : open));
      runtime.refresh(target.deckId);
    });
    return undefined;
  };

  const sendDismiss = (ids: string[]): Promise<string | undefined> => {
    const op = begin(false);
    return runtime.dismiss(target, ids).then((reply) => {
      if (!settle(op)) return undefined;
      const said = reply.outcome === "answered" ? dismissMessage(reply.answer) : UNSUPPORTED;
      setMessage(said);
      runtime.refresh(target.deckId);
      return said;
    }, (error: unknown) => {
      if (!settle(op)) return undefined;
      const why = failure(error);
      setMessage(why);
      runtime.refresh(target.deckId);
      return why;
    });
  };

  const dismiss = (said?: string): string | Promise<string | undefined> => {
    if (busy) return "Wait for the last action to finish.";
    const chosen = resolve(said);
    if (typeof chosen === "string") return chosen;
    setMessage(undefined);
    return sendDismiss([chosen.id]);
  };

  const dismissAll = (): string | undefined => {
    if (busy) return "Wait for the last action to finish.";
    if (ordered.length === 0) return NOTHING_INTERRUPTED;
    setMessage(undefined);
    setView({ kind: "dismissAll", ids: ordered.map((candidate) => candidate.id) });
    return undefined;
  };

  const confirmDismissAll = (): string | undefined => {
    if (view.kind !== "dismissAll" || busy) return "There is nothing to dismiss.";
    const ids = view.ids;
    setView(LIST);
    void sendDismiss(ids);
    return undefined;
  };

  const showReports = (from: number) => {
    if (view.kind !== "details") return;
    const id = view.id;
    const op = nextOp();
    ops.current.show = op;
    setView({ ...view, asked: true, failed: undefined });
    void runtime.show(target, id, from).then((reply) => {
      if (!mounted.current || ops.current.show !== op) return;
      ops.current.show = undefined;
      setView((open) => {
        if (open.kind !== "details" || open.id !== id) return open;
        if (reply.outcome === "unsupported") return { ...open, failed: UNSUPPORTED, asked: open.show !== undefined };
        return { ...open, show: reply.answer, failed: undefined };
      });
    }, (error: unknown) => {
      if (!mounted.current || ops.current.show !== op) return;
      ops.current.show = undefined;
      setView((open) => (open.kind === "details" && open.id === id ? { ...open, failed: failure(error), asked: open.show !== undefined } : open));
    });
  };

  const back = () => {
    abandonDryRun();
    ops.current.show = undefined;
    setView(LIST);
  };
  const close = () => {
    abandonDryRun();
    runtime.closeDialog();
  };
  const openDetails = (id: string) => {
    setSelectedId(id);
    ops.current.show = undefined;
    setView({ kind: "details", id, asked: false });
  };
  const select = (id: string) => setSelectedId(id);

  const controls: InterruptedDialogControls = { reclaim, confirmReclaim, dismiss, dismissAll, confirmDismissAll, select, openDetails, showReports, back, close };
  const latest = useRef(controls);
  latest.current = controls;
  useEffect(() => {
    if (!controlsRef) return;
    controlsRef.current = {
      reclaim: (said) => latest.current.reclaim(said),
      confirmReclaim: () => latest.current.confirmReclaim(),
      dismiss: (said) => latest.current.dismiss(said),
      dismissAll: () => latest.current.dismissAll(),
      confirmDismissAll: () => latest.current.confirmDismissAll(),
      select: (id) => latest.current.select(id),
      openDetails: (id) => latest.current.openDetails(id),
      showReports: (from) => latest.current.showReports(from),
      back: () => latest.current.back(),
      close: () => latest.current.close(),
    };
    return () => { controlsRef.current = undefined; };
  }, [controlsRef]);

  const escape = useCallback(() => {
    if (latest.current) {
      if (view.kind === "list") latest.current.close();
      else latest.current.back();
    }
  }, [view.kind]);
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, escape);
  useEffect(() => { ref.current?.focus(); }, [ref]);

  const registry = { interruptedDialog: controls };
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const handled = (() => {
      switch (event.key) {
        case "Escape": escape(); return true;
        case "ArrowDown": case "j": case "ArrowUp": case "k": {
          if (view.kind !== "list" || ordered.length === 0) return false;
          const step = event.key === "ArrowDown" || event.key === "j" ? 1 : -1;
          const next = ordered[Math.min(Math.max(selectedIndex + step, 0), ordered.length - 1)];
          VOICE_ACTIONS.selectInterruptedItem.run(registry, next.id);
          return true;
        }
        case "Enter":
          if (view.kind === "list" && selected) { VOICE_ACTIONS.showInterruptedDetails.run(registry, selected.id); return true; }
          if (view.kind === "details" && !view.asked && (item(view.id)?.reports.length ?? 0) > 0) { VOICE_ACTIONS.showInterruptedReports.run(registry, 0); return true; }
          return false;
        case "r": if (view.kind !== "list" || !selected || selected.kind === "agent") return false; VOICE_ACTIONS.reclaimInterrupted.run(registry, {}); return true;
        case "d": if (view.kind !== "list") return false; VOICE_ACTIONS.dismissInterrupted.run(registry, {}); return true;
        case "D": if (view.kind !== "list") return false; VOICE_ACTIONS.dismissAllInterrupted.run(registry); return true;
        case "n": if (view.kind !== "details" || typeof view.show?.next_reports_from !== "number") return false; VOICE_ACTIONS.showInterruptedReports.run(registry, view.show.next_reports_from); return true;
        case "p": if (view.kind !== "details" || !view.show?.reports_from) return false; VOICE_ACTIONS.showInterruptedReports.run(registry, previousPage(view.show)); return true;
        default: return false;
      }
    })();
    if (handled) {
      event.preventDefault();
      event.stopPropagation();
    }
  };

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={() => VOICE_ACTIONS.closeInterruptedWork.run(registry)}>
      <section
        ref={ref}
        tabIndex={-1}
        className="interrupted-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="interrupted-dialog-title"
        data-testid="interrupted-dialog"
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <header className="interrupted-dialog-header">
          <h2 id="interrupted-dialog-title">{INTERRUPTED_WORK_TITLE}</h2>
          <span className="interrupted-dialog-deck">{deckLabel}</span>
          <button type="button" className="icon-button" aria-label="Close" title="Close (Escape)" data-testid="interrupted-close" onClick={() => VOICE_ACTIONS.closeInterruptedWork.run(registry)}><X size={15} /></button>
        </header>
        {message && <p className="interrupted-message" role="status" data-testid="interrupted-message">{message}</p>}
        {view.kind === "list" && <ListView deck={deck} ordered={ordered} selected={selected} busy={busy} registry={registry} />}
        {view.kind === "details" && <DetailsView view={view} item={view.show?.item ?? item(view.id)} registry={registry} />}
        {view.kind === "reclaim" && <ReclaimView view={view} item={item(view.id)} busy={busy} registry={registry} />}
        {view.kind === "dismissAll" && (
          <div className="interrupted-confirm" role="alertdialog" aria-labelledby="interrupted-dismiss-all-question" data-testid="interrupted-dismiss-all">
            <p id="interrupted-dismiss-all-question" data-testid="interrupted-dismiss-all-question">{dismissAllQuestion(view.ids.length)}</p>
            <div className="interrupted-actions">
              <button type="button" className="button secondary" data-testid="interrupted-back" onClick={() => VOICE_ACTIONS.backInterruptedWork.run(registry)}>Back</button>
              <button type="button" className="button danger" data-testid="interrupted-dismiss-all-confirm" disabled={busy} onClick={() => VOICE_ACTIONS.confirmDismissAllInterrupted.run(registry)}>{DISMISS_ALL_ACTION}</button>
            </div>
          </div>
        )}
      </section>
    </div>
  );
}

/** Where the page before `show`'s starts. */
function previousPage(show: InterruptedShow): number {
  const size = Math.max(show.item.reports.length, 1);
  return Math.max((show.reports_from ?? 0) - size, 0);
}

type Registry = { interruptedDialog: InterruptedDialogControls };

function ListView({ deck, ordered, selected, busy, registry }: { deck: InterruptedDeck; ordered: InterruptedItem[]; selected?: InterruptedItem; busy: boolean; registry: Registry }) {
  if (!deck.list) return <p className="interrupted-note" data-testid="interrupted-unavailable">{deck.failed ?? UNSUPPORTED}</p>;
  const firstAgent = ordered.findIndex((candidate) => candidate.kind === "agent");
  return (
    <>
      {deck.list.incomplete && <p className="interrupted-note">{LIST_INCOMPLETE}</p>}
      {ordered.length === 0
        ? <p className="interrupted-note" data-testid="interrupted-empty">{NOTHING_INTERRUPTED}</p>
        : (
          <ul className="interrupted-list" data-testid="interrupted-list" aria-label={INTERRUPTED_WORK_TITLE}>
            {ordered.map((candidate, at) => (
              <li key={candidate.id} className="interrupted-list-entry">
                {at === firstAgent && <h3 className="interrupted-group" data-testid="interrupted-other-agents">{OTHER_AGENTS_GROUP}</h3>}
                <ItemRow item={candidate} selected={candidate.id === selected?.id} registry={registry} />
              </li>
            ))}
          </ul>
        )}
      {(deck.list.omitted ?? 0) > 0 && <p className="interrupted-note">{omittedSentence(deck.list.omitted ?? 0)}</p>}
      {selected && (
        <div className="interrupted-actions">
          <button type="button" className="button secondary compact" data-testid="interrupted-details" onClick={() => VOICE_ACTIONS.showInterruptedDetails.run(registry, selected.id)}>Details</button>
          {selected.kind !== "agent" && <button type="button" className="button secondary compact" data-testid="interrupted-reclaim" disabled={busy} title={`${RECLAIM_ACTION}: ${RECLAIM_EXPLANATION}`} onClick={() => VOICE_ACTIONS.reclaimInterrupted.run(registry, {})}>{RECLAIM_ACTION}</button>}
          <button type="button" className="button secondary compact" data-testid="interrupted-dismiss" disabled={busy} onClick={() => VOICE_ACTIONS.dismissInterrupted.run(registry, {})}>{DISMISS_ACTION}</button>
          <button type="button" className="button secondary compact" data-testid="interrupted-dismiss-all-open" disabled={busy} onClick={() => VOICE_ACTIONS.dismissAllInterrupted.run(registry)}>{DISMISS_ALL_ACTION}</button>
        </div>
      )}
    </>
  );
}

/** One item's row, with the same fields the TUI's list shows. */
function ItemRow({ item, selected, registry }: { item: InterruptedItem; selected: boolean; registry: Registry }) {
  const facts = [item.launcher ? `${LABEL_STARTED_BY}: ${displayValue(item.launcher)}` : undefined, lastEntryLine(item)].filter((fact): fact is string => fact !== undefined);
  return (
    <button
      type="button"
      className={selected ? "interrupted-row is-selected" : "interrupted-row"}
      data-testid="interrupted-row"
      data-item-id={item.id}
      aria-pressed={selected}
      onClick={() => VOICE_ACTIONS.selectInterruptedItem.run(registry, item.id)}
    >
      <strong>{itemName(item)} · {kindLabel(item.kind)}{item.name && item.branch ? ` · ${displayValue(item.branch)}` : ""}</strong>
      {item.worktree && <span>{directoryLabel(item)}: {displayValue(item.worktree)}</span>}
      <span className="interrupted-row-facts">{facts.join(" · ")}</span>
      <span data-testid="interrupted-row-progress">{progressFact(item)}</span>
    </button>
  );
}

function DetailsView({ view, item, registry }: { view: Extract<View, { kind: "details" }>; item?: InterruptedItem; registry: Registry }) {
  if (!item) return null;
  const show = view.asked ? view.show : undefined;
  const total = item.reports.length + (item.reports_omitted ?? 0);
  const from = show?.reports_from ?? 0;
  const fact = (label: string, value: string) => <div className="interrupted-fact"><dt>{label}</dt><dd>{value}</dd></div>;
  return (
    <div className="interrupted-details" data-testid="interrupted-details-view">
      <h3>{itemName(item)} · {kindLabel(item.kind)}</h3>
      <dl>
        {typeof item.issue === "number" && fact(LABEL_ISSUE, `${displayValue(item.repo ?? "")}#${item.issue}`)}
        {item.branch && fact(LABEL_BRANCH, displayValue(item.branch))}
        {item.worktree && fact(directoryLabel(item), displayValue(item.worktree))}
        {item.launcher && fact(LABEL_STARTED_BY, displayValue(item.launcher))}
        {fact(LABEL_LAST_ENTRY, `${displayValue(item.last_entry.kind)}, ${formatTime(item.last_entry.ts_ms)}`)}
        {fact(LABEL_PROGRESS, displayValue(progressLine(item.progress)))}
        {item.git && fact(LABEL_GIT, gitFactsLine(item.git))}
        {item.reports.map((report) => <div key={report.report_id} className="interrupted-fact" data-testid="interrupted-report-entry"><dt>{LABEL_REPORT}</dt><dd>{displayValue(report.report_id)}, {keptUntil(report.kept_until_ms)}</dd></div>)}
      </dl>
      {show && (from > 0 || typeof show.next_reports_from === "number") && item.reports.length > 0 && <p className="interrupted-note" data-testid="interrupted-report-range">{reportRangeSentence(from + 1, from + item.reports.length, total)}</p>}
      {item.cut && <p className="interrupted-note">{ITEM_CUT}</p>}
      {view.failed && <p className="interrupted-message" role="status" data-testid="interrupted-report-failed">{view.failed}</p>}
      <div className="interrupted-actions">
        <button type="button" className="button secondary compact" data-testid="interrupted-back" onClick={() => VOICE_ACTIONS.backInterruptedWork.run(registry)}>Back</button>
        {!view.asked && item.reports.length > 0 && <button type="button" className="button secondary compact" data-testid="interrupted-show-reports" onClick={() => VOICE_ACTIONS.showInterruptedReports.run(registry, 0)}>Show reports</button>}
        {show && from > 0 && <button type="button" className="button secondary compact" data-testid="interrupted-previous-reports" onClick={() => VOICE_ACTIONS.showInterruptedReports.run(registry, previousPage(show))}>Previous reports</button>}
        {show && typeof show.next_reports_from === "number" && <button type="button" className="button secondary compact" data-testid="interrupted-next-reports" onClick={() => VOICE_ACTIONS.showInterruptedReports.run(registry, show.next_reports_from as number)}>Next reports</button>}
      </div>
      {show?.reports.map((report) => (
        <section key={report.report_id} className="interrupted-report" data-testid="interrupted-report" aria-label={`${LABEL_REPORT} ${displayValue(report.report_id)}`}>
          <h4>{LABEL_REPORT} {displayValue(report.report_id)}</h4>
          {typeof report.body === "string"
            ? <>{report.cut && <p className="interrupted-note">{REPORT_CUT}</p>}<pre className="interrupted-report-body" data-testid="interrupted-report-body">{reportLines(report.body).join("\n")}</pre></>
            : <p className="interrupted-note">{REPORT_UNREADABLE} {displayValue(report.error ?? "")}</p>}
        </section>
      ))}
    </div>
  );
}

function ReclaimView({ view, item, busy, registry }: { view: Extract<View, { kind: "reclaim" }>; item?: InterruptedItem; busy: boolean; registry: Registry }) {
  const dry = view.dry;
  const asks = dry !== undefined && wouldRemove(dry) && !busy;
  return (
    <div className="interrupted-confirm" role="alertdialog" aria-labelledby="interrupted-reclaim-title" data-testid="interrupted-reclaim-view">
      {item && <h3 id="interrupted-reclaim-title">{RECLAIM_ACTION} {itemName(item)} · {kindLabel(item.kind)}</h3>}
      <p>{RECLAIM_ACTION}: {RECLAIM_EXPLANATION}</p>
      {dry && (
        <div data-testid="interrupted-reclaim-verdict">
          <p className="interrupted-verdict">{reclaimMessage(dry)}</p>
          {dry.worktree && <p>{LABEL_WORKTREE}: {displayValue(dry.worktree)}</p>}
          {dry.branch && <p>{LABEL_BRANCH}: {displayValue(dry.branch)}</p>}
          {dry.nested_worktrees?.map((nested) => <p key={nested}>{LABEL_NESTED_WORKTREE}: {displayValue(nested)}</p>)}
          {unpushedSentence(dry.unpushed_commits) && <p>{unpushedSentence(dry.unpushed_commits)}</p>}
        </div>
      )}
      <div className="interrupted-actions">
        <button type="button" className="button secondary" data-testid="interrupted-back" onClick={() => VOICE_ACTIONS.backInterruptedWork.run(registry)}>Back</button>
        {asks && <button type="button" className="button danger" data-testid="interrupted-reclaim-confirm" onClick={() => VOICE_ACTIONS.confirmReclaimInterrupted.run(registry)}>{RECLAIM_ACTION}</button>}
      </div>
    </div>
  );
}
