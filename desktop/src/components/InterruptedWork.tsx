import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type MutableRefObject, type RefObject } from "react";
import { History, X } from "lucide-react";
import { OVERLAY_LAYER, overlayAbove, useInertBackground } from "../hooks/useInertBackground";
import type { InterruptedDeck, InterruptedWorkRuntime } from "../hooks/useInterruptedWork";
import {
  DAEMON_FAILED, DISMISS_ACTION, DISMISS_ALL_ACTION, INTERRUPTED_NO_NEXT_PAGE, INTERRUPTED_NO_PREVIOUS_PAGE, INTERRUPTED_NO_REPORTS, INTERRUPTED_WAIT, INTERRUPTED_WORK_TITLE, ITEM_CUT, LABEL_BRANCH, LABEL_GIT, LABEL_ISSUE,
  LABEL_LAST_ENTRY, LABEL_NESTED_WORKTREE, LABEL_PROGRESS, LABEL_REPORT, LABEL_STARTED_BY, LABEL_WORKTREE, LIST_INCOMPLETE, MAX_SHOWN_REPORTS, NOTHING_INTERRUPTED, OTHER_AGENTS_GROUP, RECLAIM_ACTION, RECLAIM_EXPLANATION,
  REFUSED_NOT_LISTED, REFUSED_PLAIN_AGENT, REPORT_CUT, REPORT_UNREADABLE, UNSUPPORTED,
  directoryLabel, dismissAllQuestion, dismissMessage, dismissOneQuestion, displayValue, formatTime, gitFactsLine, interruptedFooter, itemName, keptUntil, kindLabel, lastEntryLine, omittedSentence, orderedItems, progressFact,
  progressLine, publishInterruptedDialog, reclaimMessage, reportLines, reportRangeSentence, resolveSpokenItem, unpushedSentence, wouldRemove,
  type InterruptedDialogControls, type InterruptedItem, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome,
} from "../lib/interruptedWork";
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

/** What the dialog shows. `trail` is the offsets of the report pages shown before this one, for Previous reports. */
type View =
  | { kind: "list" }
  | { kind: "details"; id: string; show?: InterruptedShow; asked: boolean; failed?: string; trail: number[] }
  | { kind: "reclaim"; id: string; dry?: ReclaimOutcome }
  | { kind: "dismissOne"; id: string }
  | { kind: "dismissAll"; ids: string[] };

/** Why a request failed, as the TUI says it. */
function failure(error: unknown): string {
  return `${DAEMON_FAILED} ${displayValue(error instanceof Error ? error.message : String(error))}`;
}

/** Back out of a view to the list, as the TUI's Esc and `n` do. */
const LIST: View = { kind: "list" };

/** A field that types the keys pressed in it: no shortcut of the dialog takes them. */
const TEXT_FIELD = "input, textarea, select, [contenteditable='true']";

/**
 * How long a y/n question has been fully on screen before `y` answers it
 * (reviewer 8): the desktop's counterpart of the TUI's "every row of the
 * question has been drawn". A `y` typed in the same burst as the key that
 * asked the question lands before that and answers nothing.
 */
const QUESTION_PRESENTED_MS = 300;

/** Each opening of a dialog, so voice can tell a reopened one from the one it spoke to. */
let openings = 0;

/** Whether `element` is fully inside its dialog's visible box and the window. */
function fullyShown(element: HTMLElement): boolean {
  const shown = element.getBoundingClientRect();
  const box = element.closest(".interrupted-dialog")?.getBoundingClientRect();
  const top = Math.max(box?.top ?? 0, 0);
  const bottom = Math.min(box?.bottom ?? window.innerHeight, window.innerHeight);
  return shown.top >= top - 1 && shown.bottom <= bottom + 1;
}

/**
 * Whether the y/n question `key` names has been presented: scrolled into
 * view, painted, and fully shown for {@link QUESTION_PRESENTED_MS}. A new
 * `key` (another question) starts over; `undefined` is no question.
 */
function usePresented(key: string | undefined, question: RefObject<HTMLElement | null>): boolean {
  const [presented, setPresented] = useState<string | undefined>();
  useEffect(() => {
    if (key === undefined) return;
    question.current?.scrollIntoView?.({ block: "nearest" });
    let frame = 0;
    let timer: number | undefined;
    const look = () => {
      frame = window.requestAnimationFrame(() => {
        const element = question.current;
        if (!element || !fullyShown(element)) {
          look();
          return;
        }
        timer = window.setTimeout(() => {
          if (question.current && fullyShown(question.current)) setPresented(key);
          else look();
        }, QUESTION_PRESENTED_MS);
      });
    };
    /* Two frames: the first is the one the question is painted in. */
    frame = window.requestAnimationFrame(look);
    return () => {
      window.cancelAnimationFrame(frame);
      if (timer !== undefined) window.clearTimeout(timer);
      /* The same question asked again is presented again, from the start. */
      setPresented(undefined);
    };
  }, [key, question]);
  return key !== undefined && presented === key;
}

/**
 * PRD #1790 stage 2 — the "Interrupted work" dialog for one deck: its items,
 * work first and plain agents under "Other agents" (decision D1), an item's
 * details and kept reports' text on demand, reclaim with a dry run and a
 * confirm offered only for a would-remove verdict, dismiss, and dismiss all
 * with its question. Mounted per daemon process (its key names it), so an
 * answer from a process the deck has left never reaches it.
 *
 * A real reclaim or a dismissal is held by the runtime, not here: it stays
 * pending, and the list is refreshed when it answers, though this dialog was
 * closed meanwhile (reviewer 2).
 */
export function InterruptedWorkDialog({ deckLabel, deck, target, runtime, controlsRef }: { deckLabel: string; deck: InterruptedDeck; target: InterruptedTarget; runtime: InterruptedWorkRuntime; controlsRef?: MutableRefObject<InterruptedDialogControls | undefined> }) {
  const ordered = useMemo(() => orderedItems(deck.list), [deck.list]);
  const [selectedId, setSelectedId] = useState<string | undefined>(() => ordered[0]?.id);
  const lastIndex = useRef(0);
  const [view, setView] = useState<View>(LIST);
  const [message, setMessage] = useState<string | undefined>();
  /* The dry run in flight, by its op: abandoned by Back and Close, as the TUI abandons one. */
  const [dryRun, setDryRun] = useState<number | undefined>();
  const ops = useRef({ last: 0, dry: undefined as number | undefined, mutation: undefined as number | undefined, show: undefined as number | undefined });
  const [opening] = useState(() => ++openings);
  /* Set on every mount, StrictMode's second one included (reviewer 1). */
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  /* The selection follows its item; one that went is replaced by the row that took its place, or the last. */
  const index = selectedId === undefined ? -1 : ordered.findIndex((item) => item.id === selectedId);
  const selectedIndex = index >= 0 ? index : Math.min(lastIndex.current, Math.max(ordered.length - 1, 0));
  const selected: InterruptedItem | undefined = ordered[selectedIndex];
  useEffect(() => { lastIndex.current = selectedIndex; });
  const item = (id: string) => deck.list?.items.find((candidate) => candidate.id === id);
  const busy = dryRun !== undefined || runtime.busy(target);
  /* An item that is no longer listed takes its details or confirmation with it, unless an action on it is in flight. */
  const gone = (view.kind === "details" || view.kind === "reclaim" || view.kind === "dismissOne") && item(view.id) === undefined;
  useEffect(() => {
    if (!gone || busy) return;
    ops.current.show = undefined;
    setView(LIST);
  }, [gone, busy]);

  const nextOp = () => ++ops.current.last;
  const abandonDryRun = () => {
    ops.current.dry = undefined;
    setDryRun(undefined);
  };

  /** The item `said` names, or the selected one; see `resolveSpokenItem`. */
  const resolve = (said?: string): InterruptedItem | string => resolveSpokenItem(ordered, selected, said);

  const reclaim = (said?: string): string | Promise<string | undefined> => {
    if (busy) return INTERRUPTED_WAIT;
    const chosen = resolve(said);
    if (typeof chosen === "string") return chosen;
    if (chosen.kind === "agent") return REFUSED_PLAIN_AGENT;
    setSelectedId(chosen.id);
    setMessage(undefined);
    setView({ kind: "reclaim", id: chosen.id });
    const op = nextOp();
    ops.current.dry = op;
    setDryRun(op);
    const settle = () => {
      if (!mounted.current || ops.current.dry !== op) return false;
      ops.current.dry = undefined;
      setDryRun(undefined);
      return true;
    };
    return runtime.reclaim(target, chosen.id, true).then((reply) => {
      if (!settle()) return undefined;
      if (reply.outcome === "unsupported") {
        setMessage(UNSUPPORTED);
        setView(LIST);
        return UNSUPPORTED;
      }
      setView((open) => (open.kind === "reclaim" && open.id === chosen.id ? { ...open, dry: reply.answer } : open));
      return reclaimMessage(reply.answer);
    }, (error: unknown) => {
      if (!settle()) return undefined;
      const why = failure(error);
      setMessage(why);
      setView(LIST);
      return why;
    });
  };

  /** Show the answer to a real reclaim or a dismissal the runtime sent, if this dialog is still the one that sent it. */
  const mutation = <T,>(sent: Promise<T>, said: (reply: T) => string, leave: () => void): Promise<string | undefined> => {
    const op = nextOp();
    ops.current.mutation = op;
    const mine = () => mounted.current && ops.current.mutation === op;
    return sent.then((reply) => {
      if (!mine()) return undefined;
      const words = said(reply);
      setMessage(words);
      leave();
      return words;
    }, (error: unknown) => {
      if (!mine()) return undefined;
      const why = failure(error);
      setMessage(why);
      leave();
      return why;
    });
  };

  const confirmReclaim = (): string | undefined => {
    if (view.kind !== "reclaim" || !view.dry || !wouldRemove(view.dry) || busy) return "There is no reclaim to confirm.";
    const id = view.id;
    const leave = () => setView((open) => (open.kind === "reclaim" && open.id === id ? LIST : open));
    void mutation(runtime.reclaim(target, id, false), (reply) => (reply.outcome === "answered" ? reclaimMessage(reply.answer) : UNSUPPORTED), leave);
    return undefined;
  };

  const sendDismiss = (ids: string[]): Promise<string | undefined> => {
    setMessage(undefined);
    return mutation(runtime.dismiss(target, ids), (reply) => (reply.outcome === "answered" ? dismissMessage(reply.answer) : UNSUPPORTED), () => undefined);
  };

  const dismiss = (): string | Promise<string | undefined> => {
    if (busy) return INTERRUPTED_WAIT;
    if (!selected) return REFUSED_NOT_LISTED;
    return sendDismiss([selected.id]);
  };

  const askDismiss = (said?: string): string | undefined => {
    if (busy) return INTERRUPTED_WAIT;
    const chosen = resolve(said);
    if (typeof chosen === "string") return chosen;
    setSelectedId(chosen.id);
    setMessage(undefined);
    ops.current.show = undefined;
    setView({ kind: "dismissOne", id: chosen.id });
    return undefined;
  };

  const confirmDismiss = (): string | undefined => {
    if (view.kind !== "dismissOne" || busy) return "There is nothing to dismiss.";
    const ids = [view.id];
    setView(LIST);
    void sendDismiss(ids);
    return undefined;
  };

  const dismissAll = (): string | undefined => {
    if (busy) return INTERRUPTED_WAIT;
    if (ordered.length === 0) return NOTHING_INTERRUPTED;
    setMessage(undefined);
    ops.current.show = undefined;
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

  /** Ask for the page of `id`'s reports from `from`; `trail` is the pages before it, kept once that page is shown. */
  const requestPage = (id: string, from: number, trail: number[]) => {
    const op = nextOp();
    ops.current.show = op;
    setView((open) => (open.kind === "details" && open.id === id
      ? { ...open, asked: true, failed: undefined }
      : { kind: "details", id, asked: true, trail: [] }));
    void runtime.show(target, id, from).then((reply) => {
      if (!mounted.current || ops.current.show !== op) return;
      ops.current.show = undefined;
      setView((open) => {
        if (open.kind !== "details" || open.id !== id) return open;
        if (reply.outcome === "unsupported") return { ...open, failed: UNSUPPORTED, asked: open.show !== undefined };
        return { ...open, show: reply.answer, trail, failed: undefined };
      });
    }, (error: unknown) => {
      if (!mounted.current || ops.current.show !== op) return;
      ops.current.show = undefined;
      setView((open) => (open.kind === "details" && open.id === id ? { ...open, failed: failure(error), asked: open.show !== undefined } : open));
    });
  };

  const showReports = (said?: string): string | undefined => {
    const named = said !== undefined && said.trim() !== "";
    const chosen = named || view.kind !== "details" ? resolve(said) : item(view.id) ?? REFUSED_NOT_LISTED;
    if (typeof chosen === "string") return chosen;
    if (chosen.reports.length === 0) return INTERRUPTED_NO_REPORTS;
    abandonDryRun();
    setSelectedId(chosen.id);
    requestPage(chosen.id, 0, []);
    return undefined;
  };

  const nextReports = (): string | undefined => {
    if (view.kind !== "details" || !view.show || typeof view.show.next_reports_from !== "number") return INTERRUPTED_NO_NEXT_PAGE;
    requestPage(view.id, view.show.next_reports_from, [...view.trail, view.show.reports_from ?? 0]);
    return undefined;
  };

  const previousReports = (): string | undefined => {
    if (view.kind !== "details" || !view.show || !view.show.reports_from) return INTERRUPTED_NO_PREVIOUS_PAGE;
    const from = view.show.reports_from;
    const trail = [...view.trail];
    let before = trail.pop();
    /* With no page to go back to, or a trail that does not lead back, the daemon's page size, as the TUI's `p`. */
    if (before === undefined || before >= from) {
      before = Math.max(from - MAX_SHOWN_REPORTS, 0);
      trail.length = 0;
    }
    requestPage(view.id, before, trail);
    return undefined;
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
  const openDetails = (which?: { item?: string; id?: string }): string | undefined => {
    const chosen = which?.id !== undefined ? item(which.id) ?? REFUSED_NOT_LISTED : resolve(which?.item);
    if (typeof chosen === "string") return chosen;
    abandonDryRun();
    setSelectedId(chosen.id);
    ops.current.show = undefined;
    setView({ kind: "details", id: chosen.id, asked: false, trail: [] });
    return undefined;
  };
  const select = (id: string) => setSelectedId(id);

  const controls: InterruptedDialogControls = { reclaim, confirmReclaim, dismiss, askDismiss, confirmDismiss, dismissAll, confirmDismissAll, select, openDetails, showReports, nextReports, previousReports, back, close };
  const latest = useRef(controls);
  latest.current = controls;
  useEffect(() => {
    if (!controlsRef) return;
    controlsRef.current = {
      reclaim: (said) => latest.current.reclaim(said),
      confirmReclaim: () => latest.current.confirmReclaim(),
      dismiss: () => latest.current.dismiss(),
      askDismiss: (said) => latest.current.askDismiss(said),
      confirmDismiss: () => latest.current.confirmDismiss(),
      dismissAll: () => latest.current.dismissAll(),
      confirmDismissAll: () => latest.current.confirmDismissAll(),
      select: (id) => latest.current.select(id),
      openDetails: (which) => latest.current.openDetails(which),
      showReports: (said) => latest.current.showReports(said),
      nextReports: () => latest.current.nextReports(),
      previousReports: () => latest.current.previousReports(),
      back: () => latest.current.back(),
      close: () => latest.current.close(),
    };
    return () => { controlsRef.current = undefined; };
  }, [controlsRef]);

  const escape = useCallback(() => {
    if (view.kind === "list") latest.current.close();
    else latest.current.back();
  }, [view.kind]);
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, escape);
  useEffect(() => { ref.current?.focus(); }, [ref]);

  /*
    Auditor 2 — what voice binds an utterance to: this opening of this deck's
    dialog for this daemon process, the items as listed, the selection and
    what is open. Published from a layout effect, so it is current when an
    utterance is declared after the commit that changed it.
  */
  const viewKey = view.kind === "list" || view.kind === "dismissAll" ? view.kind : view.kind === "details" ? `details:${view.id}:${view.show?.reports_from ?? "-"}` : `${view.kind}:${view.id}`;
  const token = [target.deckId, target.instance, opening, ordered.map((candidate) => candidate.id).join(","), selected?.id ?? "", viewKey].join("\u0000");
  useLayoutEffect(() => publishInterruptedDialog({ token, covered: () => overlayAbove(ref.current) }), [ref, token]);

  /* Reviewer 5 — the selected row stays in view; a key that moved the selection from a row moves focus with it. */
  const listRef = useRef<HTMLUListElement>(null);
  const focusRow = useRef(false);
  const selectedShown = selected?.id;
  useLayoutEffect(() => {
    if (view.kind !== "list" || selectedShown === undefined) return;
    const row = [...(listRef.current?.querySelectorAll<HTMLElement>("[data-testid='interrupted-row']") ?? [])].find((candidate) => candidate.dataset.itemId === selectedShown);
    row?.scrollIntoView?.({ block: "nearest" });
    if (focusRow.current) row?.focus();
    focusRow.current = false;
  }, [selectedShown, view.kind]);

  /* Reviewer 8 — the y/n question on screen, and whether it has been presented. */
  const question = useRef<HTMLParagraphElement>(null);
  const asking = view.kind === "dismissAll" ? `dismissAll:${view.ids.join(",")}`
    : view.kind === "dismissOne" ? `dismissOne:${view.id}`
      : view.kind === "reclaim" && view.dry && wouldRemove(view.dry) && !busy ? `reclaim:${view.id}` : undefined;
  const presented = usePresented(asking, question);

  const registry = { interruptedDialog: controls };
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const from = event.target instanceof HTMLElement ? event.target : undefined;
    const onSurface = from === event.currentTarget;
    /* A text field keeps every key but Escape. */
    if (from && !onSurface && from.matches(TEXT_FIELD) && event.key !== "Escape") return;
    const confirming = view.kind === "dismissAll" || view.kind === "dismissOne" || view.kind === "reclaim";
    const handled = (() => {
      switch (event.key) {
        case "Escape": case "q": escape(); return true;
        case "I": if (view.kind !== "list") return false; VOICE_ACTIONS.closeInterruptedWork.run(registry); return true;
        case "ArrowDown": case "j": case "ArrowUp": case "k": {
          if (view.kind !== "list" || ordered.length === 0) return false;
          const step = event.key === "ArrowDown" || event.key === "j" ? 1 : -1;
          const next = ordered[Math.min(Math.max(selectedIndex + step, 0), ordered.length - 1)];
          focusRow.current = from?.dataset.testid === "interrupted-row";
          VOICE_ACTIONS.selectInterruptedItem.run(registry, next.id);
          return true;
        }
        case "Enter": {
          /* A focused control keeps its own Enter (reviewer 3); a focused row opens its own details. */
          if (!onSurface) {
            const row = from?.closest<HTMLElement>("[data-testid='interrupted-row']");
            if (view.kind !== "list" || !row?.dataset.itemId) return false;
            VOICE_ACTIONS.showInterruptedDetails.run(registry, { itemId: row.dataset.itemId });
            return true;
          }
          if (view.kind === "list" && selected) { VOICE_ACTIONS.showInterruptedDetails.run(registry, { itemId: selected.id }); return true; }
          if (view.kind === "details" && !view.asked && (item(view.id)?.reports.length ?? 0) > 0) { VOICE_ACTIONS.showInterruptedReports.run(registry, {}); return true; }
          return false;
        }
        case "r": if (view.kind !== "list" || !selected || selected.kind === "agent") return false; VOICE_ACTIONS.reclaimInterrupted.run(registry, {}); return true;
        case "d": if (view.kind !== "list") return false; VOICE_ACTIONS.dismissSelectedInterrupted.run(registry); return true;
        case "D": if (view.kind !== "list") return false; VOICE_ACTIONS.dismissAllInterrupted.run(registry); return true;
        case "y": case "Y": {
          if (!confirming) return false;
          /* Only a question that has been presented: a `y` typed in the burst that asked it answers nothing. */
          if (!presented) return true;
          if (view.kind === "dismissAll") VOICE_ACTIONS.confirmDismissAllInterrupted.run(registry);
          else if (view.kind === "dismissOne") VOICE_ACTIONS.confirmDismissInterrupted.run(registry);
          else VOICE_ACTIONS.confirmReclaimInterrupted.run(registry);
          return true;
        }
        case "n": case "N":
          if (confirming) { VOICE_ACTIONS.backInterruptedWork.run(registry); return true; }
          if (event.key === "n" && view.kind === "details" && typeof view.show?.next_reports_from === "number") { VOICE_ACTIONS.nextInterruptedReports.run(registry); return true; }
          return false;
        case "p": if (view.kind !== "details" || !view.show?.reports_from) return false; VOICE_ACTIONS.previousInterruptedReports.run(registry); return true;
        default: return false;
      }
    })();
    if (handled) {
      event.preventDefault();
      event.stopPropagation();
    }
  };

  const confirmItem = view.kind === "dismissOne" ? item(view.id) : undefined;
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
        {view.kind === "list" && <ListView deck={deck} ordered={ordered} selected={selected} busy={busy} registry={registry} listRef={listRef} />}
        {view.kind === "details" && <DetailsView view={view} item={view.show?.item ?? item(view.id)} registry={registry} />}
        {view.kind === "reclaim" && <ReclaimView view={view} item={item(view.id)} busy={busy} registry={registry} question={question} presented={presented} />}
        {view.kind === "dismissOne" && confirmItem && (
          <div className="interrupted-confirm" role="alertdialog" aria-labelledby="interrupted-dismiss-question" data-testid="interrupted-dismiss-one" data-presented={presented ? "true" : undefined}>
            <p ref={question} id="interrupted-dismiss-question" data-testid="interrupted-dismiss-question">{dismissOneQuestion(confirmItem)}</p>
            <div className="interrupted-actions">
              <button type="button" className="button secondary" data-testid="interrupted-back" onClick={() => VOICE_ACTIONS.backInterruptedWork.run(registry)}>Back</button>
              <button type="button" className="button danger" data-testid="interrupted-dismiss-confirm" disabled={busy} onClick={() => VOICE_ACTIONS.confirmDismissInterrupted.run(registry)}>{DISMISS_ACTION}</button>
            </div>
          </div>
        )}
        {view.kind === "dismissAll" && (
          <div className="interrupted-confirm" role="alertdialog" aria-labelledby="interrupted-dismiss-all-question" data-testid="interrupted-dismiss-all" data-presented={presented ? "true" : undefined}>
            <p ref={question} id="interrupted-dismiss-all-question" data-testid="interrupted-dismiss-all-question">{dismissAllQuestion(view.ids.length)}</p>
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

type Registry = { interruptedDialog: InterruptedDialogControls };

function ListView({ deck, ordered, selected, busy, registry, listRef }: { deck: InterruptedDeck; ordered: InterruptedItem[]; selected?: InterruptedItem; busy: boolean; registry: Registry; listRef: RefObject<HTMLUListElement | null> }) {
  /* Reviewer 7, auditor 4: the daemon's error text is escaped like every other failure. */
  if (!deck.list) return <p className="interrupted-note" data-testid="interrupted-unavailable">{deck.failed !== undefined ? failure(new Error(deck.failed)) : UNSUPPORTED}</p>;
  const firstAgent = ordered.findIndex((candidate) => candidate.kind === "agent");
  return (
    <>
      {deck.list.incomplete && <p className="interrupted-note">{LIST_INCOMPLETE}</p>}
      {ordered.length === 0
        ? <p className="interrupted-note" data-testid="interrupted-empty">{NOTHING_INTERRUPTED}</p>
        : (
          <ul ref={listRef} className="interrupted-list" data-testid="interrupted-list" aria-label={INTERRUPTED_WORK_TITLE}>
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
          <button type="button" className="button secondary compact" data-testid="interrupted-details" onClick={() => VOICE_ACTIONS.showInterruptedDetails.run(registry, { itemId: selected.id })}>Details</button>
          {selected.kind !== "agent" && <button type="button" className="button secondary compact" data-testid="interrupted-reclaim" disabled={busy} title={`${RECLAIM_ACTION}: ${RECLAIM_EXPLANATION}`} onClick={() => VOICE_ACTIONS.reclaimInterrupted.run(registry, {})}>{RECLAIM_ACTION}</button>}
          <button type="button" className="button secondary compact" data-testid="interrupted-dismiss" disabled={busy} onClick={() => VOICE_ACTIONS.dismissSelectedInterrupted.run(registry)}>{DISMISS_ACTION}</button>
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
        {!view.asked && item.reports.length > 0 && <button type="button" className="button secondary compact" data-testid="interrupted-show-reports" onClick={() => VOICE_ACTIONS.showInterruptedReports.run(registry, {})}>Show reports</button>}
        {show && from > 0 && <button type="button" className="button secondary compact" data-testid="interrupted-previous-reports" onClick={() => VOICE_ACTIONS.previousInterruptedReports.run(registry)}>Previous reports</button>}
        {show && typeof show.next_reports_from === "number" && <button type="button" className="button secondary compact" data-testid="interrupted-next-reports" onClick={() => VOICE_ACTIONS.nextInterruptedReports.run(registry)}>Next reports</button>}
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

function ReclaimView({ view, item, busy, registry, question, presented }: { view: Extract<View, { kind: "reclaim" }>; item?: InterruptedItem; busy: boolean; registry: Registry; question: RefObject<HTMLParagraphElement | null>; presented: boolean }) {
  const dry = view.dry;
  const asks = dry !== undefined && wouldRemove(dry) && !busy;
  return (
    <div className="interrupted-confirm" role="alertdialog" aria-labelledby="interrupted-reclaim-title" data-testid="interrupted-reclaim-view" data-presented={presented ? "true" : undefined}>
      {item && <h3 id="interrupted-reclaim-title">{RECLAIM_ACTION} {itemName(item)} · {kindLabel(item.kind)}</h3>}
      <p>{RECLAIM_ACTION}: {RECLAIM_EXPLANATION}</p>
      {dry && (
        <div data-testid="interrupted-reclaim-verdict">
          <p ref={question} className="interrupted-verdict">{reclaimMessage(dry)}</p>
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
