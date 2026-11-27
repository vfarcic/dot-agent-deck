import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { CARD_ICON_NAMES, searchCardIcons } from "../lib/cardIcons";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { CardActionsEditorMember, CardActionsWhich, VoiceActionContext } from "../lib/voiceActions";
import type { CardActionInput, CardCatalogEdit, DeckRuntimeState, EditableCardAction, EditableCardCatalog } from "../types";

/** A daemon the editor can show: its deck id, the name the dashboard gives it, and whether it advertised the editor. */
export interface CardActionsDaemon {
  deckId: string;
  name: string;
  editable: boolean;
  /**
   * PR #2491 review (Qodo 2) — the daemon process whose cards the dashboard
   * shows for the deck. Another process under the same deck id is a restarted
   * daemon: the open editor drops what it read and reads its list again.
   */
  instanceId?: string;
}

/** Where the catalog places an action, as the form offers it. */
type CardActionPlacement = "compact" | "card" | "detail";

/** The form's fields, as typed. */
export interface CardActionFormValues {
  name: string;
  label: string;
  prompt: string;
  description: string;
  icon: string | null;
  placement: CardActionPlacement;
  confirm: boolean;
  always: boolean;
}

/** The form: a new action, or one listed action being edited (its name fixed). */
interface CardActionForm {
  /** Which form this is: a Save's answer changes only the form it was sent from (issue #2399 review 6). */
  generation: number;
  mode: "add" | "edit";
  values: CardActionFormValues;
  /**
   * The action as the editor last read it (edit): as listed when the form
   * opened, then as a Refresh read it. A Save sends it as the edit's `base`,
   * so the daemon refuses the save when the file's action is no longer this
   * one (issue #2399 re-review N5). `undefined` once a read no longer lists it.
   */
  original?: EditableCardAction;
  /**
   * Why the daemon refused the last Save, or that a read since the form
   * opened lists its action changed (PR #2491 re-audit, F1); the form keeps
   * what was typed.
   */
  error?: string;
  /**
   * PR #2491 re-audit (F1, Edit) — counts the changed-since alerts this form
   * has shown. Each read that moves `original` raises it, and the form draws
   * its message keyed by it, so every change is announced as a new alert.
   */
  alertKey?: number;
  /**
   * The draft of this form's last Save whose answer was lost, so it may have
   * been saved: a read listing the action exactly as sent shows it was.
   */
  sent?: CardActionInput;
  /**
   * PR #2491 audit (A2) — the daemon process whose list the form was made
   * from: a Save is bound to it, and the form is not offered under another.
   */
  instanceId?: string;
}

/** The editor on screen. */
export interface CardActionsEditorView {
  /** The daemon the editor is bound to: the one whose ⋯ menu opened it, or the one voice named. */
  daemon: CardActionsDaemon;
  /** The daemon's card actions, once read. */
  catalog?: EditableCardCatalog;
  loading: boolean;
  /** Why the last read or write failed (not a form's Save, which says so in the form). */
  error?: string;
  /** The catalog is being read again, keeping the form (Refresh, or after an edit whose outcome is unknown). */
  refreshing: boolean;
  /**
   * The last reading-again of the list, said apart from any write's error
   * (issue #2399 re-review N3): under way, read, or failed and why. `check`
   * is the reading-again after an edit whose outcome is unknown.
   */
  reread?: CardActionsReread;
  form?: CardActionForm;
  /** The action whose removal the editor is asking to confirm. */
  removing?: EditableCardAction;
  /** An edit is being written. */
  busy: boolean;
}

/** A reading-again of the list and how it went. */
export interface CardActionsReread {
  /** `refresh`: Refresh asked for it; `check`: an edit whose answer was lost did. */
  kind: "refresh" | "check";
  status: "reading" | "read" | "failed";
  /** Why the read failed. */
  reason?: string;
  /** The label of an action a form's uncertain Save saved: the read lists it exactly as the form sent it. */
  saved?: string;
}

/** What {@link useCardActionsEditor} hands the overview. */
export interface CardActionsEditor {
  /** Whether the runtime can read card actions at all, so a daemon's ⋯ menu offers the editor. */
  shown: boolean;
  /** Whether `deckId`'s ⋯ menu offers Card actions: its daemon shows cards and the runtime can read them. */
  offeredFor: (deckId: string) => boolean;
  view: CardActionsEditorView | undefined;
  /** The editor's members, for its controls and for voice. */
  controls: Pick<VoiceActionContext, CardActionsEditorMember>;
  /** A keystroke into one of the form's fields. */
  setField: <K extends keyof CardActionFormValues>(key: K, value: CardActionFormValues[K]) => void;
}

export const CARD_ACTIONS_NOT_AVAILABLE = "The Card actions editor is not available here: no daemon on screen shows cards.";
const NOT_AVAILABLE = CARD_ACTIONS_NOT_AVAILABLE;
/** PR #2491 review T1 — over an agent's pane, an opening naming no daemon means the pane's, and that daemon has no editor. */
const PANE_DAEMON_NO_EDITOR = "This agent's daemon shows no cards, so it has no Card actions editor.";
const NOT_OPEN = "The Card actions editor is not open.";
const NO_FORM = "No card action form is open.";
const NOT_ASKING = "The editor is not asking to remove anything.";
const STILL_SAVING = "The last change is still being saved; wait for it before making another.";
const readOnly = (name: string) => `${name}'s daemon is from a release whose card actions cannot be edited from the app.`;
const NOT_LOADED = "The card actions are still being read; try again in a moment.";
const NOT_SHOWN = "That daemon is not on the dashboard, or shows no cards, so its card actions cannot be shown.";
/** Several daemons show cards and none was named: the user is asked which, as a spoken "show host" asks. */
export function cardActionsWhich(names: readonly string[]): string {
  return `Several daemons are on the dashboard: say which one, like “open card actions on ${displayText(names[0] ?? "", DISPLAY_LIMITS.name)}”.`;
}
const SAVING_FORM = "The form is being saved; wait for it before opening another.";

/* PR #2491 review (Qodo 3): why a spoken edit or removal that waited for the list did not happen, when the editor stops waiting for another reason. */
const CLOSED_BEFORE_READ = "The Card actions editor was closed before its list was read.";
const OTHER_DAEMON = "Another daemon's card actions were shown before the list was read.";
const SUPERSEDED = "A later request replaced this one.";

/* PR #2491 re-audit (F1): a Refresh read the action a confirmation or a form was made from, and it is not what the user saw any more. */
const changedBeforeRemoval = (seen: EditableCardAction, listed: boolean) => (listed
  ? `${displayText(seen.label, DISPLAY_LIMITS.name)} changed since you asked to remove it, so it was not removed: the list shows it as just read; press Remove again to remove it.`
  : `${displayText(seen.label, DISPLAY_LIMITS.name)} is no longer listed, so it was not removed.`);
/* PR #2491 re-audit (hidden default): an action hidden since is said to be hidden; a Save un-hides a deck default (`src/card_catalog.rs:1318-1320`). */
const changedUnderForm = (seen: EditableCardAction, fresh: EditableCardAction | undefined) => {
  const label = displayText(seen.label, DISPLAY_LIMITS.name);
  if (fresh === undefined) return `${label} is no longer listed, so this form cannot save it.`;
  if (fresh.hidden && !seen.hidden) return `${label} changed since this form was opened: it is now hidden. Save replaces it with what this form holds and shows it again.`;
  return `${label} changed since this form was opened: the list shows it as just read. Save replaces it with what this form holds.`;
};

/** The label the daemon gives an action that names none (`default_label` in `src/card_catalog.rs`). */
function defaultLabel(name: string): string {
  const spaced = name.replace(/-/g, " ");
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/**
 * Whether `listed` is the action `sent` asked for, in the state a successful
 * Save leaves it in: every field a form's Save sends, as the daemon lists it,
 * and the action in effect as the user's own entry. PR #2491 re-audit (hidden
 * default): a hidden deck default is listed with the deck's own values
 * (`editable_catalog`, `src/card_catalog.rs:1032-1037`), which can equal the
 * draft while the Save is not in effect. What a Save writes, per
 * `src/card_catalog.rs`: an Update of a deck default's name un-hides it
 * (`:1318-1320`) and writes a user entry, which parses as `source: user`
 * (`parse_action`, `:536`), so it is listed `default: true`,
 * `overridden: true`, `hidden: false` (`:1026-1030`); an Update or Add of any
 * other name writes a user entry, listed `default: false`,
 * `overridden: false`, `hidden: false` (`:1046-1049`). `listed.default` says
 * which kind of name it is, so `overridden` must equal it.
 */
function listedAsSent(listed: EditableCardAction, sent: CardActionInput): boolean {
  return listed.name === sent.name && listed.label === (sent.label ?? defaultLabel(sent.name)) && listed.description === sent.description
    && listed.prompt === sent.prompt && (listed.icon ?? null) === (sent.icon ?? null) && (listed.placement || "card") === (sent.placement ?? "card")
    && listed.confirm === sent.confirm && listed.always === sent.always
    && !listed.hidden && listed.source === "user" && listed.overridden === listed.default;
}

/** Whether two drafts send the same edit. */
function sameInput(a: CardActionInput, b: CardActionInput): boolean {
  return a.name === b.name && a.label === b.label && a.description === b.description && a.prompt === b.prompt && a.icon === b.icon
    && a.placement === b.placement && a.confirm === b.confirm && a.always === b.always;
}

const savedEarlier = (label: string) => `${displayText(label, DISPLAY_LIMITS.name)} was saved as this form sent it earlier. Save now saves what the form holds since.`;

/** Whether two reads of an action list it the same: every field the daemon compares against an edit's `base`. */
function sameAction(a: EditableCardAction, b: EditableCardAction): boolean {
  return a.name === b.name && a.label === b.label && a.description === b.description && a.prompt === b.prompt && a.icon === b.icon
    && a.placement === b.placement && a.confirm === b.confirm && a.always === b.always && a.source === b.source
    && a.default === b.default && a.overridden === b.overridden && a.hidden === b.hidden;
}

const unknownAction = (wanted: { name?: string; label?: string }) => `No single card action is called "${displayText(wanted.label ?? wanted.name ?? "", DISPLAY_LIMITS.name)}" in the editor.`;

const EMPTY_FORM: CardActionFormValues = { name: "", label: "", prompt: "", description: "", icon: null, placement: "card", confirm: false, always: false };

/**
 * An action's placement as the form holds it: the catalog's own, `card` when
 * it names none the form knows. The form offers two choices, Show on the card
 * (`card`) and Only in the Card tab (`detail`); a `compact` action shows as on
 * the card and keeps `compact` unless the user picks the other choice.
 */
function formPlacement(placement: string | null | undefined): CardActionPlacement {
  return placement === "compact" || placement === "detail" ? placement : "card";
}

/**
 * PR #2491 decision 2026-11-25 — the daemon an opening means: the one named
 * (by its ⋯ menu, or by voice), else the only daemon showing cards. With
 * several and none named, the user is asked which rather than one being
 * guessed, the way a spoken "show host" asks. PR #2491 (2026-11-25): with
 * none named, `preferred` — the one daemon the selected cards are on — is
 * meant while it is on the dashboard. PR #2491 review T1: with an agent's
 * pane open over the dashboard, `pane` — that agent's daemon — is meant
 * instead, and nothing else: a selection behind the pane, or the only other
 * daemon showing cards, is not what the user is looking at.
 */
function chooseDaemon(daemons: readonly CardActionsDaemon[], wanted: string | undefined, preferred?: string, pane?: string): { daemon: CardActionsDaemon } | { refused: string; which?: CardActionsWhich["which"] } {
  if (wanted !== undefined) {
    const named = daemons.find((candidate) => candidate.deckId === wanted);
    return named ? { daemon: named } : { refused: NOT_SHOWN };
  }
  if (pane !== undefined) {
    const own = daemons.find((candidate) => candidate.deckId === pane);
    return own ? { daemon: own } : { refused: PANE_DAEMON_NO_EDITOR };
  }
  const selected = preferred === undefined ? undefined : daemons.find((candidate) => candidate.deckId === preferred);
  if (selected) return { daemon: selected };
  if (daemons.length === 0) return { refused: NOT_AVAILABLE };
  if (daemons.length > 1) return { refused: cardActionsWhich(daemons.map((candidate) => candidate.name)), which: daemons.map(({ deckId, name }) => ({ deckId, name })) };
  return { daemon: daemons[0] };
}

/** PR #2491 audit (A2) — something read from one daemon process, with that process. */
type Held<T> = { value: T; instanceId: string | undefined };

/** A spoken edit or removal, by the action's name or label. */
type VoiceAsk = { kind: "edit" | "remove"; wanted: { name?: string; label?: string } };

/** The listed action a spoken or pressed name means: by name, else by label, ignoring case; `undefined` when none or several match. */
function findAction(catalog: EditableCardCatalog, wanted: { name?: string; label?: string }): EditableCardAction | undefined {
  if (wanted.name !== undefined) return catalog.actions.find((action) => action.name === wanted.name);
  const said = (wanted.label ?? "").trim().toLowerCase();
  if (!said) return undefined;
  const byLabel = catalog.actions.filter((action) => action.label.trim().toLowerCase() === said);
  if (byLabel.length === 1) return byLabel[0];
  const byName = catalog.actions.filter((action) => action.name === said.replace(/\s+/g, "-"));
  return byName.length === 1 ? byName[0] : undefined;
}

/** The edit the form's Save sends: the keys an action entry carries, an empty label, no icon or the default placement left out. */
function formInput(form: CardActionForm): CardActionInput {
  const { values } = form;
  const placement = values.placement;
  return {
    name: values.name.trim(),
    ...(values.label.trim() ? { label: values.label.trim() } : {}),
    description: values.description,
    prompt: values.prompt,
    ...(values.icon ? { icon: values.icon } : {}),
    ...(placement !== "card" ? { placement } : {}),
    confirm: values.confirm,
    always: values.always,
  };
}

/** The form with no uncertain Save to look for. */
function withoutSent(form: CardActionForm): CardActionForm {
  const rest = { ...form };
  delete rest.sent;
  return rest;
}

/**
 * PR #2491 re-audit (F1, Edit) — an open form after a read (Refresh, the
 * check after an edit whose answer was lost, or a write's answer) from
 * process `from`. The form's base moves only together with a changed-since
 * alert new for that change, whatever message the form showed before. A form
 * whose uncertain Save the read lists exactly as sent was saved: it closes as
 * a Save answered does (`saved` names the action), or, typed in since, keeps
 * the new draft with the saved action as its base and says so.
 */
function formAfterRead(current: CardActionForm | undefined, answer: EditableCardCatalog, from: string | undefined): { form: CardActionForm | undefined; saved?: string } {
  if (current === undefined || current.instanceId !== from) return { form: current };
  const alertKey = (current.alertKey ?? 0) + 1;
  const { sent } = current;
  if (sent !== undefined) {
    const listed = answer.actions.find((action) => action.name === sent.name);
    if (listed !== undefined && listedAsSent(listed, sent)) {
      if (sameInput(formInput(current), sent)) return { form: undefined, saved: listed.label };
      return { form: { ...withoutSent(current), ...(current.mode === "edit" ? { original: listed } : {}), error: savedEarlier(listed.label), alertKey }, saved: listed.label };
    }
  }
  if (current.mode !== "edit") return { form: current };
  const fresh = answer.actions.find((action) => action.name === current.values.name);
  const seen = current.original;
  if (seen === undefined ? fresh === undefined : fresh !== undefined && sameAction(seen, fresh)) return { form: current };
  return { form: { ...withoutSent(current), original: fresh, error: changedUnderForm((seen ?? fresh)!, fresh), alertKey } };
}

const reason = (error: unknown) => displayText(error instanceof Error ? error.message : String(error), DISPLAY_LIMITS.detail);

/**
 * Whether a failed edit may have been saved: the bridge marks an edit whose
 * answer was lost after it was sent (`outcome: "unknown"`); every other
 * failure is a refusal, and nothing was written (issue #2399 review 8).
 */
const outcomeUnknown = (error: unknown) => typeof error === "object" && error !== null && (error as { outcome?: unknown }).outcome === "unknown";

/** The icon a spoken or pressed name means: the name itself, its words joined by dashes, or the one icon whose name holds every word. */
function cardIconByName(said: string): string | undefined {
  const wanted = said.trim().toLowerCase();
  if (!wanted) return undefined;
  const dashed = wanted.replace(/\s+/g, "-");
  if (CARD_ICON_NAMES.includes(wanted)) return wanted;
  if (CARD_ICON_NAMES.includes(dashed)) return dashed;
  const found = searchCardIcons(wanted);
  return found.length === 1 ? found[0] : undefined;
}

/**
 * PRD #2374 decision 2026-11-24 — the Card actions editor: the daemon it is
 * bound to (PR #2491 decision 2026-11-25: the one whose ⋯ menu opened it, or
 * the one voice named), that daemon's card actions as it lists them, the form for a new
 * or listed action, and the removal it is asking to confirm. Every change is
 * the daemon's: the editor sends one edit and shows the actions the daemon
 * answers, or the reason it refused, and decides nothing about the catalog
 * itself. An edit whose answer was lost may have been saved: the editor says
 * so, keeps what the user typed, and reads the list once more, never sending
 * the edit again. A daemon that did not advertise the editor is shown
 * read-only and sent nothing.
 */
export function useCardActionsEditor({ runtime, daemons, preferredDeckId, paneDeckId }: {
  runtime: Pick<DeckRuntimeState, "readCardCatalog" | "editCardCatalog">;
  daemons: readonly CardActionsDaemon[];
  /** PR #2491 (2026-11-25) — the daemon an opening that names none means, before asking which: the one the selected cards on screen are all on. */
  preferredDeckId?: string;
  /** PR #2491 review T1 — the daemon of the agent pane open over the dashboard, if one is: an opening that names none means it, and only it. */
  paneDeckId?: string;
}): CardActionsEditor {
  const read = runtime.readCardCatalog;
  const write = runtime.editCardCatalog;
  const shown = typeof read === "function" && typeof write === "function" && daemons.length > 0;
  const [open, setOpen] = useState(false);
  const [deckId, setDeckId] = useState<string | undefined>(undefined);
  /* PR #2491 audit (A2): the list as read, with the daemon process it was
     read from. It counts only while that process is the one shown. */
  const [held, setHeld] = useState<Held<EditableCardCatalog> | undefined>(undefined);
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [rereadState, setReread] = useState<CardActionsReread | undefined>(undefined);
  const [error, setError] = useState<string | undefined>(undefined);
  const [heldForm, setForm] = useState<CardActionForm | undefined>(undefined);
  const [heldRemoving, setRemoving] = useState<Held<EditableCardAction> | undefined>(undefined);
  const [busy, setBusy] = useState(false);
  /* Which daemon's answers count: an answer for an earlier one (another
     daemon, a closed editor) is dropped. */
  const readSeq = useRef(0);
  /* Issue #2399 re-review N1: the order of one daemon's answers. Each applied
     write answer bumps `writeEpoch`, and a read that began before it is
     dropped, because the write's answer is newer; each reading-again bumps
     `rereadSeq`, and only the newest one's answer counts. */
  const writeEpoch = useRef(0);
  const rereadSeq = useRef(0);
  /* The generation the next form gets: a Save's answer changes only the form
     it was sent from (issue #2399 review 6). */
  const formSeq = useRef(0);
  /* What voice asked for before the catalog was read, done once it is.
     `settle` ends the spoken request: `undefined` once its form or
     confirmation is open, or the sentence saying why it was not (PR #2491
     review, Qodo 3), so a request never ends in silence. */
  const pending = useRef<(VoiceAsk & { settle: (refused: string | undefined) => void }) | undefined>(undefined);
  /* End the waiting request, if any, with `refused`. */
  const dropPending = useCallback((refused: string) => {
    const asked = pending.current;
    pending.current = undefined;
    asked?.settle(refused);
  }, []);
  const daemon = daemons.find((candidate) => candidate.deckId === deckId);
  /* PR #2491 audit (A2): what was read, and what was made from it, is
     offered only under the process it came from. An answer from the process
     before a restart can land in the render that adopts its successor, before
     the restart's reset runs; it is then simply not there. */
  const instanceId = daemon?.instanceId;
  const catalog = held !== undefined && held.instanceId === instanceId ? held.value : undefined;
  const removing = heldRemoving !== undefined && heldRemoving.instanceId === instanceId ? heldRemoving.value : undefined;
  const form = heldForm !== undefined && heldForm.instanceId === instanceId ? heldForm : undefined;

  const load = useCallback((target: CardActionsDaemon | undefined) => {
    const seq = ++readSeq.current;
    setHeld(undefined);
    setError(undefined);
    setForm(undefined);
    setRemoving(undefined);
    setRefreshing(false);
    setReread(undefined);
    rereadSeq.current += 1;
    if (!target || !target.editable || !read) {
      setLoading(false);
      dropPending(target ? readOnly(displayText(target.name, DISPLAY_LIMITS.name)) : NOT_AVAILABLE);
      return;
    }
    setLoading(true);
    const from = target.instanceId;
    read(target.deckId, from).then((answer) => {
      if (seq !== readSeq.current) return;
      setHeld({ value: answer, instanceId: from });
      setLoading(false);
    }, (failure: unknown) => {
      if (seq !== readSeq.current) return;
      const why = `The card actions could not be read: ${reason(failure)}`;
      setError(why);
      setLoading(false);
      dropPending(why);
    });
  }, [dropPending, read]);

  /* Read the list again, keeping the form and what it holds: Refresh, and
     once after an edit whose outcome is unknown (`check`). One read, never
     the edit. Its progress and outcome are said on their own line, whatever
     error a form shows (issue #2399 re-review N3). A newer reading-again
     supersedes it, and a write answered since it began makes its answer
     older than the list shown, so it is dropped (N1). An open form's base
     becomes the action as read, so a Save after Refresh is checked against
     what the user now sees (N5), and the form says so when that changed it;
     a removal confirmation whose action it changed closes (PR #2491
     re-audit, F1). */
  const reread = useCallback((target: CardActionsDaemon, kind: CardActionsReread["kind"]) => {
    if (!read || !target.editable) return;
    const seq = readSeq.current;
    const mine = ++rereadSeq.current;
    const epoch = writeEpoch.current;
    setRefreshing(true);
    setReread({ kind, status: "reading" });
    const from = target.instanceId;
    read(target.deckId, from).then((answer) => {
      if (seq !== readSeq.current || mine !== rereadSeq.current) return;
      setRefreshing(false);
      if (epoch !== writeEpoch.current) { setReread(undefined); return; }
      setHeld({ value: answer, instanceId: from });
      if (kind === "refresh") setError(undefined);
      const { saved } = formAfterRead(latest.current.form, answer, from);
      setReread(kind === "check" || saved !== undefined ? { kind, status: "read", ...(saved !== undefined ? { saved } : {}) } : undefined);
      /* PR #2491 re-audit (F1): a removal asking about an action this read
         lists changed, or no longer lists, is not confirmed under the old
         words: it closes, and the editor says why. */
      const asked = latest.current.removing;
      if (asked !== undefined && asked.instanceId === from) {
        const fresh = answer.actions.find((action) => action.name === asked.value.name);
        if (fresh === undefined || !sameAction(asked.value, fresh)) {
          setRemoving((current) => (current === asked ? undefined : current));
          setError(changedBeforeRemoval(asked.value, fresh !== undefined));
        }
      }
      /* ...and an open form whose action this read lists changed is told so,
         in the form, in place of whatever it said, before its Save sends the
         action as just read; one whose uncertain Save the read lists as sent
         was saved (PR #2491 re-audit, F1, Edit). */
      setForm((current) => formAfterRead(current, answer, from).form);
    }, (failure: unknown) => {
      if (seq !== readSeq.current || mine !== rereadSeq.current) return;
      setRefreshing(false);
      setReread({ kind, status: "failed", reason: reason(failure) });
    });
  }, [read]);

  /* The editor is bound to its daemon (PR #2491 decision 2026-11-25): while
     that daemon is off the dashboard the editor is not drawn, and when it
     comes back the still-open editor reads its card actions again (issue
     #2399 review 5). It never moves to another daemon on its own. */
  const present = daemon !== undefined;
  const wasPresent = useRef(present);
  useEffect(() => {
    const before = wasPresent.current;
    wasPresent.current = present;
    if (!open || !present || before) return;
    load(daemon);
  }, [daemon, load, open, present]);
  /* PR #2491 review (Qodo 2) — the daemon restarted while its deck stayed on
     the dashboard: another process now answers under the same deck id. What
     the editor read, the form made from it and every answer still on its way
     from the old process are dropped (`load` moves `readSeq` on), and the new
     process's list is read. A spoken edit or removal still waiting for a list
     waits for that one: the user never saw the old one. A process becoming
     known for the first time is read the same way (PR #2491 audit, A2): a
     list read naming none was refused, and one held from no process is not
     offered under a named one. */
  const shownInstance = useRef({ deckId: daemon?.deckId, instanceId });
  useEffect(() => {
    const before = shownInstance.current;
    shownInstance.current = { deckId: daemon?.deckId, instanceId };
    if (!open || !present || before.deckId !== daemon?.deckId || before.instanceId === instanceId) return;
    load(daemon);
  }, [daemon, instanceId, load, open, present]);

  /* The confirmation on screen, with the process its action was read from. */
  const asking = removing !== undefined ? heldRemoving : undefined;
  const latest = useRef({ open, daemon, catalog, form, removing: asking, busy, shown, refreshing, loading });
  latest.current = { open, daemon, catalog, form, removing: asking, busy, shown, refreshing, loading };

  /*
   * Send one edit to the daemon shown, bound to process `from` — the one
   * whose list the edit was made from (PR #2491 audit, A2): a process that
   * replaced it is sent nothing. `onDone` runs with its answer;
   * `onFailed` with the reason and whether the edit may have been saved
   * anyway (its answer was lost after it was sent), in which case the list is
   * read once more and the editor says it "may have been" done.
   */
  const send = useCallback((edit: CardCatalogEdit, from: string | undefined, onFailed: (why: string, unknown: boolean) => void, onDone?: () => void) => {
    const target = latest.current.daemon;
    if (!target || !write) return;
    const seq = readSeq.current;
    setBusy(true);
    setReread(undefined);
    write(target.deckId, edit, from).then((answer) => {
      setBusy(false);
      if (seq !== readSeq.current) return;
      writeEpoch.current += 1;
      setHeld({ value: answer, instanceId: from });
      setError(undefined);
      const { saved } = formAfterRead(latest.current.form, answer, from);
      onDone?.();
      /* The answer is a read too: an open form whose action it lists changed
         is told so, after `onDone` closed a form this edit ends (PR #2491
         re-audit, F1, Edit). */
      setForm((current) => formAfterRead(current, answer, from).form);
      if (saved !== undefined) setReread({ kind: "check", status: "read", saved });
    }, (failure: unknown) => {
      setBusy(false);
      if (seq !== readSeq.current) return;
      const unknown = outcomeUnknown(failure);
      onFailed(reason(failure), unknown);
      if (unknown) reread(target, "check");
    });
  }, [reread, write]);

  /* `from`: the process whose list the form is made from, which its Save is bound to. */
  const startForm = useCallback((action: EditableCardAction | undefined, from: string | undefined) => {
    setRemoving(undefined);
    setError(undefined);
    const generation = ++formSeq.current;
    const bound = from !== undefined ? { instanceId: from } : {};
    setForm(action
      ? { generation, mode: "edit", original: action, values: { name: action.name, label: action.label, prompt: action.prompt, description: action.description, icon: action.icon, placement: formPlacement(action.placement), confirm: action.confirm, always: action.always }, ...bound }
      : { generation, mode: "add", values: { ...EMPTY_FORM }, ...bound });
  }, []);

  /* `from`: the process whose list `action` was read from, which the removal is bound to. */
  const askRemove = useCallback((action: EditableCardAction, from: string | undefined): string | undefined => {
    if (action.default) return `${displayText(action.label, DISPLAY_LIMITS.name)} is a deck default: hide it instead.`;
    setRemoving({ value: action, instanceId: from });
    return undefined;
  }, []);

  /* What voice asked for before the catalog was read: done now, or refused
     in words (an action the list does not name, a save under way). Only a
     list read from the process shown settles it (`catalog` is no other's,
     PR #2491 audit A2), and what it opens is bound to that process. */
  useEffect(() => {
    const asked = pending.current;
    if (!asked || !catalog) return;
    pending.current = undefined;
    if (latest.current.busy) { asked.settle(asked.kind === "edit" ? SAVING_FORM : STILL_SAVING); return; }
    const action = findAction(catalog, asked.wanted);
    if (!action) { asked.settle(unknownAction(asked.wanted)); return; }
    if (asked.kind === "edit") { startForm(action, instanceId); asked.settle(undefined); } else asked.settle(askRemove(action, instanceId));
  }, [askRemove, catalog, instanceId, startForm]);

  /* Wait for the list, then do `voiceAsk`: what it answers settles once the list is read, or the read fails. */
  const queue = (voiceAsk: VoiceAsk): Promise<string | undefined> => new Promise((settle) => {
    dropPending(SUPERSEDED);
    pending.current = { ...voiceAsk, settle };
  });

  /*
   * An editor member that needs the editor open on an editable daemon with
   * its actions read. A spoken edit or removal (`voiceAsk`) asked before the
   * list is read waits for it, and answers a promise of how it ended; one for
   * a daemon whose card actions cannot be edited is refused at once, and
   * nothing waits (PR #2491 review, Qodo 3).
   */
  function editing(then: (catalog: EditableCardCatalog) => string | undefined): string | undefined;
  function editing(then: (catalog: EditableCardCatalog) => string | undefined, voiceAsk: VoiceAsk): string | undefined | Promise<string | undefined>;
  function editing(then: (catalog: EditableCardCatalog) => string | undefined, voiceAsk?: VoiceAsk): string | undefined | Promise<string | undefined> {
    const now = latest.current;
    if (!now.shown) return NOT_AVAILABLE;
    if (!now.open) {
      if (!voiceAsk) return NOT_OPEN;
      const chosen = chooseDaemon(daemons, undefined, preferredDeckId, paneDeckId);
      if ("refused" in chosen) return chosen.refused;
      openEditor(chosen.daemon.deckId);
      if (!chosen.daemon.editable) return readOnly(displayText(chosen.daemon.name, DISPLAY_LIMITS.name));
      return queue(voiceAsk);
    }
    if (!now.daemon?.editable) return readOnly(displayText(now.daemon?.name ?? "This", DISPLAY_LIMITS.name));
    if (!now.catalog) {
      if (!voiceAsk) return NOT_LOADED;
      /* A read that failed is not waited on: it is read again (as Refresh does). */
      if (!now.loading) load(now.daemon);
      return queue(voiceAsk);
    }
    return then(now.catalog);
  }

  /* A member that sets one of the open form's values; refused with no form, or while it is being saved. */
  const formValue = (change: (values: CardActionFormValues) => CardActionFormValues): string | undefined => {
    if (!latest.current.form) return NO_FORM;
    if (latest.current.busy) return STILL_SAVING;
    setForm((current) => (current ? { ...current, values: change(current.values) } : current));
    return undefined;
  };

  /* Open the editor bound to `wanted` (its ⋯ menu's daemon, or voice's), or
     to the only daemon showing cards. Already open on another daemon, it
     moves to this one, closing an open form unsaved; on the same one, it
     stays as it is. */
  function openEditor(wanted?: string): string | CardActionsWhich | undefined {
    if (!latest.current.shown) return NOT_AVAILABLE;
    const chosen = chooseDaemon(daemons, wanted, preferredDeckId, paneDeckId);
    if ("refused" in chosen) return chosen.which ? { refused: chosen.refused, which: chosen.which } : chosen.refused;
    const target = chosen.daemon;
    if (latest.current.open && latest.current.daemon?.deckId === target.deckId) return undefined;
    if (latest.current.open) dropPending(OTHER_DAEMON);
    setOpen(true);
    setDeckId(target.deckId);
    wasPresent.current = true;
    load(target);
    return undefined;
  }

  const unknown = unknownAction;

  /* What a failed Remove, Hide or Restore says: it was not done (a refusal), or it may have been; the reading-again says the rest. */
  const outcome = (label: string, done: string) => (why: string, unsure: boolean) => setError(unsure
    ? `${displayText(label, DISPLAY_LIMITS.name)} may have been ${done}: ${why}`
    : `${displayText(label, DISPLAY_LIMITS.name)} was not ${done}: ${why}`);

  const controls: Pick<VoiceActionContext, CardActionsEditorMember> = {
    openCardActionsEditor: openEditor,
    closeCardActionsEditor: () => {
      if (!latest.current.open) return NOT_OPEN;
      if (latest.current.removing) { setRemoving(undefined); return undefined; }
      readSeq.current += 1;
      rereadSeq.current += 1;
      dropPending(CLOSED_BEFORE_READ);
      setReread(undefined);
      setOpen(false);
      setHeld(undefined);
      setForm(undefined);
      setError(undefined);
      setLoading(false);
      setRefreshing(false);
      return undefined;
    },
    refreshCardActions: () => {
      const now = latest.current;
      if (!now.open) return NOT_OPEN;
      if (!now.daemon?.editable) return readOnly(displayText(now.daemon?.name ?? "This", DISPLAY_LIMITS.name));
      if (!now.catalog) { load(now.daemon); return undefined; }
      reread(now.daemon, "refresh");
      return undefined;
    },
    addCardAction: () => {
      const now = latest.current;
      if (!now.shown) return NOT_AVAILABLE;
      if (now.busy) return SAVING_FORM;
      let target = now.daemon;
      if (!now.open) {
        const chosen = chooseDaemon(daemons, undefined, preferredDeckId, paneDeckId);
        if ("refused" in chosen) return chosen.refused;
        target = chosen.daemon;
        openEditor(target.deckId);
      }
      if (!target?.editable) return readOnly(displayText(target?.name ?? "This", DISPLAY_LIMITS.name));
      startForm(undefined, target.instanceId);
      return undefined;
    },
    editCardAction: (wanted) => editing((listed) => {
      const action = findAction(listed, wanted);
      if (!action) return unknown(wanted);
      if (latest.current.busy) return SAVING_FORM;
      startForm(action, latest.current.daemon?.instanceId);
      return undefined;
    }, { kind: "edit", wanted }),
    removeCardAction: (wanted) => editing((listed) => {
      const action = findAction(listed, wanted);
      if (!action) return unknown(wanted);
      if (latest.current.busy) return STILL_SAVING;
      return askRemove(action, latest.current.daemon?.instanceId);
    }, { kind: "remove", wanted }),
    confirmRemoveCardAction: () => {
      const asking = latest.current.removing;
      if (!asking) return NOT_ASKING;
      if (latest.current.busy) return STILL_SAVING;
      const { value: action, instanceId: from } = asking;
      setRemoving(undefined);
      /* PR #2491 re-audit (F1): the base is the action the confirmation
         showed, never a newer read of it; a read that changed it closed the
         confirmation, and a file changed since is refused by the daemon. */
      send({ kind: "remove", name: action.name, base: action }, from, outcome(action.label, "removed"), () => {
        setForm((current) => (current?.original?.name === action.name ? undefined : current));
      });
      return undefined;
    },
    hideCardAction: (name) => editing((listed) => {
      const action = listed.actions.find((candidate) => candidate.name === name);
      if (!action?.default) return unknown({ name });
      if (latest.current.busy) return STILL_SAVING;
      send({ kind: "hide", name, base: action }, latest.current.daemon?.instanceId, outcome(action.label, "hidden"));
      return undefined;
    }),
    restoreCardAction: (name) => editing((listed) => {
      const action = listed.actions.find((candidate) => candidate.name === name);
      if (!action?.default) return unknown({ name });
      if (latest.current.busy) return STILL_SAVING;
      send({ kind: "restore", name, base: action }, latest.current.daemon?.instanceId, outcome(action.label, "restored"), () => {
        setForm((current) => (current?.original?.name === name ? undefined : current));
      });
      return undefined;
    }),
    saveCardAction: () => {
      const current = latest.current.form;
      if (!current) return NO_FORM;
      if (latest.current.busy) return STILL_SAVING;
      if (!latest.current.daemon?.editable) return readOnly(displayText(latest.current.daemon?.name ?? "This", DISPLAY_LIMITS.name));
      const edit: CardCatalogEdit = current.mode === "add"
        ? { kind: "add", action: formInput(current) }
        : { kind: "update", action: formInput(current), base: current.original ?? null };
      const sentFrom = current.generation;
      const ours = (now: CardActionForm | undefined) => now !== undefined && now.generation === sentFrom;
      setForm({ ...withoutSent(current), error: undefined });
      /* An answer lost after sending keeps the draft sent, so the reading-again can tell whether it was saved. */
      send(edit, current.instanceId, (why, unsure) => setForm((now) => (ours(now) ? { ...now!, error: unsure ? `This may have been saved: ${why}` : why, ...(unsure ? { sent: edit.action } : {}) } : now)), () => setForm((now) => (ours(now) ? undefined : now)));
      return undefined;
    },
    cancelCardActionForm: () => {
      if (!latest.current.form) return NO_FORM;
      setForm(undefined);
      return undefined;
    },
    chooseCardActionIcon: (said) => {
      if (!latest.current.form) return NO_FORM;
      const name = cardIconByName(said);
      if (!name) return `No single icon is called "${displayText(said, DISPLAY_LIMITS.name)}": Choose icon in the form shows them all, with their names.`;
      return formValue((values) => ({ ...values, icon: name }));
    },
    setCardActionFlag: (flag, on) => formValue((values) => ({ ...values, [flag]: on })),
    /* "Show on the card" keeps a file's `compact`, which shows on the card too. */
    setCardActionPlacement: (placement) => formValue((values) => ({ ...values, placement: placement === "card" && values.placement === "compact" ? "compact" : placement })),
  };

  const setField = useCallback(<K extends keyof CardActionFormValues>(key: K, value: CardActionFormValues[K]) => {
    if (latest.current.busy) return;
    setForm((current) => (current ? { ...current, values: { ...current.values, [key]: value } } : current));
  }, []);

  const view = useMemo<CardActionsEditorView | undefined>(() => (open && shown && daemon
    ? { daemon, ...(catalog ? { catalog } : {}), loading, refreshing, ...(rereadState ? { reread: rereadState } : {}), ...(error ? { error } : {}), ...(form ? { form } : {}), ...(removing ? { removing } : {}), busy }
    : undefined), [busy, catalog, daemon, error, form, loading, open, refreshing, rereadState, removing, shown]);

  const offeredFor = useCallback((wanted: string) => shown && daemons.some((candidate) => candidate.deckId === wanted), [daemons, shown]);

  return { shown, offeredFor, view, controls, setField };
}
