import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { CARD_ICON_NAMES, searchCardIcons } from "../lib/cardIcons";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { CardActionsEditorMember, VoiceActionContext } from "../lib/voiceActions";
import type { CardActionInput, CardCatalogEdit, DeckRuntimeState, EditableCardAction, EditableCardCatalog } from "../types";

/** A daemon the editor can show: its deck id, the name the dashboard gives it, and whether it advertised the editor. */
export interface CardActionsDaemon {
  deckId: string;
  name: string;
  editable: boolean;
  /** The daemon of this machine: the editor opens on it, wherever the dashboard lists it. */
  local?: boolean;
}

/** Where the catalog places an action, as the form offers it. */
export type CardActionPlacement = "compact" | "card" | "detail";

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
export interface CardActionForm {
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
  /** Why the daemon refused the last Save; the form keeps what was typed. */
  error?: string;
}

/** The editor on screen. */
export interface CardActionsEditorView {
  daemons: readonly CardActionsDaemon[];
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
}

/** What {@link useCardActionsEditor} hands the overview. */
export interface CardActionsEditor {
  /** Whether the runtime can read card actions at all, so the Cards toolbar offers the editor. */
  shown: boolean;
  view: CardActionsEditorView | undefined;
  /** The editor's members, for its controls and for voice. */
  controls: Pick<VoiceActionContext, CardActionsEditorMember>;
  /** A keystroke into one of the form's fields. */
  setField: <K extends keyof CardActionFormValues>(key: K, value: CardActionFormValues[K]) => void;
}

const NOT_AVAILABLE = "The Card actions editor is not available here: no daemon on screen shows cards.";
const NOT_OPEN = "The Card actions editor is not open.";
const NO_FORM = "No card action form is open.";
const NOT_ASKING = "The editor is not asking to remove anything.";
const STILL_SAVING = "The last change is still being saved; wait for it before making another.";
const readOnly = (name: string) => `${name}'s daemon is from a release whose card actions cannot be edited from the app.`;
const NOT_LOADED = "The card actions are still being read; try again in a moment.";
const NO_DAEMON_NAMED = "Say which daemon's card actions to show, by the name the dashboard gives it.";
const SAVING_FORM = "The form is being saved; wait for it before opening another.";

const EMPTY_FORM: CardActionFormValues = { name: "", label: "", prompt: "", description: "", icon: null, placement: "card", confirm: false, always: false };

/**
 * An action's placement as the form holds it: the catalog's own, `card` when
 * it names none the form knows. The form offers two choices, Show on the card
 * (`card`) and Only in the overlay (`detail`); a `compact` action shows as on
 * the card and keeps `compact` unless the user picks the other choice.
 */
function formPlacement(placement: string | null | undefined): CardActionPlacement {
  return placement === "compact" || placement === "detail" ? placement : "card";
}

/** The daemon the editor opens on: the local one, else the first the dashboard lists. */
function homeDaemon(daemons: readonly CardActionsDaemon[]): CardActionsDaemon | undefined {
  return daemons.find((candidate) => candidate.local) ?? daemons[0];
}

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

const reason = (error: unknown) => displayText(error instanceof Error ? error.message : String(error), DISPLAY_LIMITS.detail);

/**
 * Whether a failed edit may have been saved: the bridge marks an edit whose
 * answer was lost after it was sent (`outcome: "unknown"`); every other
 * failure is a refusal, and nothing was written (issue #2399 review 8).
 */
const outcomeUnknown = (error: unknown) => typeof error === "object" && error !== null && (error as { outcome?: unknown }).outcome === "unknown";

/** The icon a spoken or pressed name means: the name itself, its words joined by dashes, or the one icon whose name holds every word. */
export function cardIconByName(said: string): string | undefined {
  const wanted = said.trim().toLowerCase();
  if (!wanted) return undefined;
  const dashed = wanted.replace(/\s+/g, "-");
  if (CARD_ICON_NAMES.includes(wanted)) return wanted;
  if (CARD_ICON_NAMES.includes(dashed)) return dashed;
  const found = searchCardIcons(wanted);
  return found.length === 1 ? found[0] : undefined;
}

/**
 * PRD #2374 decision 2026-11-24 — the Card actions editor: which daemon it
 * shows (the local one, wherever the dashboard lists it, unless the user picks
 * another), that daemon's card actions as it lists them, the form for a new
 * or listed action, and the removal it is asking to confirm. Every change is
 * the daemon's: the editor sends one edit and shows the actions the daemon
 * answers, or the reason it refused, and decides nothing about the catalog
 * itself. An edit whose answer was lost may have been saved: the editor says
 * so, keeps what the user typed, and reads the list once more, never sending
 * the edit again. A daemon that did not advertise the editor is shown
 * read-only and sent nothing.
 */
export function useCardActionsEditor({ runtime, daemons }: {
  runtime: Pick<DeckRuntimeState, "readCardCatalog" | "editCardCatalog">;
  daemons: readonly CardActionsDaemon[];
}): CardActionsEditor {
  const read = runtime.readCardCatalog;
  const write = runtime.editCardCatalog;
  const shown = typeof read === "function" && typeof write === "function" && daemons.length > 0;
  const [open, setOpen] = useState(false);
  const [deckId, setDeckId] = useState<string | undefined>(undefined);
  const [catalog, setCatalog] = useState<EditableCardCatalog | undefined>(undefined);
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [rereadState, setReread] = useState<CardActionsReread | undefined>(undefined);
  const [error, setError] = useState<string | undefined>(undefined);
  const [form, setForm] = useState<CardActionForm | undefined>(undefined);
  const [removing, setRemoving] = useState<EditableCardAction | undefined>(undefined);
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
  /* What voice asked for before the catalog was read, done once it is. */
  const pending = useRef<{ kind: "edit" | "remove"; wanted: { name?: string; label?: string } } | undefined>(undefined);
  const daemon = daemons.find((candidate) => candidate.deckId === deckId) ?? homeDaemon(daemons);

  const load = useCallback((target: CardActionsDaemon | undefined) => {
    const seq = ++readSeq.current;
    setCatalog(undefined);
    setError(undefined);
    setForm(undefined);
    setRemoving(undefined);
    setRefreshing(false);
    setReread(undefined);
    rereadSeq.current += 1;
    if (!target || !target.editable || !read) { setLoading(false); return; }
    setLoading(true);
    read(target.deckId).then((answer) => {
      if (seq !== readSeq.current) return;
      setCatalog(answer);
      setLoading(false);
    }, (failure: unknown) => {
      if (seq !== readSeq.current) return;
      setError(`The card actions could not be read: ${reason(failure)}`);
      setLoading(false);
    });
  }, [read]);

  /* Read the list again, keeping the form and what it holds: Refresh, and
     once after an edit whose outcome is unknown (`check`). One read, never
     the edit. Its progress and outcome are said on their own line, whatever
     error a form shows (issue #2399 re-review N3). A newer reading-again
     supersedes it, and a write answered since it began makes its answer
     older than the list shown, so it is dropped (N1). An open form's base
     becomes the action as read, so a Save after Refresh is checked against
     what the user now sees (N5). */
  const reread = useCallback((target: CardActionsDaemon, kind: CardActionsReread["kind"]) => {
    if (!read || !target.editable) return;
    const seq = readSeq.current;
    const mine = ++rereadSeq.current;
    const epoch = writeEpoch.current;
    setRefreshing(true);
    setReread({ kind, status: "reading" });
    read(target.deckId).then((answer) => {
      if (seq !== readSeq.current || mine !== rereadSeq.current) return;
      setRefreshing(false);
      if (epoch !== writeEpoch.current) { setReread(undefined); return; }
      setCatalog(answer);
      if (kind === "refresh") setError(undefined);
      setReread(kind === "check" ? { kind, status: "read" } : undefined);
      setForm((current) => (current?.mode === "edit"
        ? { ...current, original: answer.actions.find((action) => action.name === current.values.name) }
        : current));
    }, (failure: unknown) => {
      if (seq !== readSeq.current || mine !== rereadSeq.current) return;
      setRefreshing(false);
      setReread({ kind, status: "failed", reason: reason(failure) });
    });
  }, [read]);

  /* The daemons on the dashboard changed while the editor is open: one that
     is shown and leaves is replaced by the local (or first) one; when every
     daemon left, the editor waits, and loads the first that comes back
     (issue #2399 review 5). */
  useEffect(() => {
    if (!open) return;
    if (deckId === undefined) {
      if (daemons.length === 0) return;
      const home = homeDaemon(daemons);
      setDeckId(home?.deckId);
      load(home);
      return;
    }
    if (daemons.some((candidate) => candidate.deckId === deckId)) return;
    const home = homeDaemon(daemons);
    setDeckId(home?.deckId);
    load(home);
  }, [daemons, deckId, load, open]);

  const latest = useRef({ open, daemon, catalog, form, removing, busy, shown, refreshing });
  latest.current = { open, daemon, catalog, form, removing, busy, shown, refreshing };

  /* The action of `name` as the editor last read it, the `base` an edit sends; `null`: none. */
  const asRead = (name: string): EditableCardAction | null => latest.current.catalog?.actions.find((action) => action.name === name) ?? null;

  /*
   * Send one edit to the daemon shown. `onDone` runs with its answer;
   * `onFailed` with the reason and whether the edit may have been saved
   * anyway (its answer was lost after it was sent), in which case the list is
   * read once more and the editor says it "may have been" done.
   */
  const send = useCallback((edit: CardCatalogEdit, onFailed: (why: string, unknown: boolean) => void, onDone?: () => void) => {
    const target = latest.current.daemon;
    if (!target || !write) return;
    const seq = readSeq.current;
    setBusy(true);
    setReread(undefined);
    write(target.deckId, edit).then((answer) => {
      setBusy(false);
      if (seq !== readSeq.current) return;
      writeEpoch.current += 1;
      setCatalog(answer);
      setError(undefined);
      onDone?.();
    }, (failure: unknown) => {
      setBusy(false);
      if (seq !== readSeq.current) return;
      const unknown = outcomeUnknown(failure);
      onFailed(reason(failure), unknown);
      if (unknown) reread(target, "check");
    });
  }, [reread, write]);

  const startForm = useCallback((action: EditableCardAction | undefined) => {
    setRemoving(undefined);
    setError(undefined);
    const generation = ++formSeq.current;
    setForm(action
      ? { generation, mode: "edit", original: action, values: { name: action.name, label: action.label, prompt: action.prompt, description: action.description, icon: action.icon, placement: formPlacement(action.placement), confirm: action.confirm, always: action.always } }
      : { generation, mode: "add", values: { ...EMPTY_FORM } });
  }, []);

  const askRemove = useCallback((action: EditableCardAction): string | undefined => {
    if (action.default) return `${displayText(action.label, DISPLAY_LIMITS.name)} is a deck default: hide it instead.`;
    setRemoving(action);
    return undefined;
  }, []);

  /* What voice asked for before the catalog was read. */
  useEffect(() => {
    const asked = pending.current;
    if (!asked || !catalog) return;
    pending.current = undefined;
    if (latest.current.busy) return;
    const action = findAction(catalog, asked.wanted);
    if (!action) return;
    if (asked.kind === "edit") startForm(action); else askRemove(action);
  }, [askRemove, catalog, startForm]);

  /* An editor member that needs the editor open on an editable daemon with its actions read. */
  const editing = (then: (catalog: EditableCardCatalog) => string | undefined, voiceAsk?: { kind: "edit" | "remove"; wanted: { name?: string; label?: string } }): string | undefined => {
    const now = latest.current;
    if (!now.shown) return NOT_AVAILABLE;
    if (!now.open) {
      if (!voiceAsk) return NOT_OPEN;
      openEditor();
      pending.current = voiceAsk;
      return undefined;
    }
    if (!now.daemon?.editable) return readOnly(displayText(now.daemon?.name ?? "This", DISPLAY_LIMITS.name));
    if (!now.catalog) {
      if (!voiceAsk) return NOT_LOADED;
      pending.current = voiceAsk;
      return undefined;
    }
    return then(now.catalog);
  };

  /* A member that sets one of the open form's values; refused with no form, or while it is being saved. */
  const formValue = (change: (values: CardActionFormValues) => CardActionFormValues): string | undefined => {
    if (!latest.current.form) return NO_FORM;
    if (latest.current.busy) return STILL_SAVING;
    setForm((current) => (current ? { ...current, values: change(current.values) } : current));
    return undefined;
  };

  function openEditor(): string | undefined {
    if (!latest.current.shown) return NOT_AVAILABLE;
    if (latest.current.open) return undefined;
    const home = homeDaemon(daemons);
    setOpen(true);
    setDeckId(home?.deckId);
    load(home);
    return undefined;
  }

  const unknown = (wanted: { name?: string; label?: string }) => `No single card action is called "${displayText(wanted.label ?? wanted.name ?? "", DISPLAY_LIMITS.name)}" in the editor.`;

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
      pending.current = undefined;
      setReread(undefined);
      setOpen(false);
      setCatalog(undefined);
      setForm(undefined);
      setError(undefined);
      setLoading(false);
      setRefreshing(false);
      return undefined;
    },
    chooseCardActionsDaemon: (wanted) => {
      if (!latest.current.open) return NOT_OPEN;
      if (!wanted) return NO_DAEMON_NAMED;
      const target = daemons.find((candidate) => candidate.deckId === wanted);
      if (!target) return "That daemon is not on the dashboard.";
      if (target.deckId === latest.current.daemon?.deckId) return undefined;
      pending.current = undefined;
      setDeckId(target.deckId);
      load(target);
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
      const target = now.open ? now.daemon : homeDaemon(daemons);
      if (!now.open) openEditor();
      if (!target?.editable) return readOnly(displayText(target?.name ?? "This", DISPLAY_LIMITS.name));
      startForm(undefined);
      return undefined;
    },
    editCardAction: (wanted) => editing((listed) => {
      const action = findAction(listed, wanted);
      if (!action) return unknown(wanted);
      if (latest.current.busy) return SAVING_FORM;
      startForm(action);
      return undefined;
    }, { kind: "edit", wanted }),
    removeCardAction: (wanted) => editing((listed) => {
      const action = findAction(listed, wanted);
      if (!action) return unknown(wanted);
      if (latest.current.busy) return STILL_SAVING;
      return askRemove(action);
    }, { kind: "remove", wanted }),
    confirmRemoveCardAction: () => {
      const action = latest.current.removing;
      if (!action) return NOT_ASKING;
      if (latest.current.busy) return STILL_SAVING;
      setRemoving(undefined);
      send({ kind: "remove", name: action.name, base: asRead(action.name) }, outcome(action.label, "removed"), () => {
        setForm((current) => (current?.original?.name === action.name ? undefined : current));
      });
      return undefined;
    },
    hideCardAction: (name) => editing((listed) => {
      const action = listed.actions.find((candidate) => candidate.name === name);
      if (!action?.default) return unknown({ name });
      if (latest.current.busy) return STILL_SAVING;
      send({ kind: "hide", name, base: action }, outcome(action.label, "hidden"));
      return undefined;
    }),
    restoreCardAction: (name) => editing((listed) => {
      const action = listed.actions.find((candidate) => candidate.name === name);
      if (!action?.default) return unknown({ name });
      if (latest.current.busy) return STILL_SAVING;
      send({ kind: "restore", name, base: action }, outcome(action.label, "restored"), () => {
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
      setForm({ ...current, error: undefined });
      send(edit, (why, unsure) => setForm((now) => (ours(now) ? { ...now!, error: unsure ? `This may have been saved: ${why}` : why } : now)), () => setForm((now) => (ours(now) ? undefined : now)));
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
  };

  const setField = useCallback(<K extends keyof CardActionFormValues>(key: K, value: CardActionFormValues[K]) => {
    if (latest.current.busy) return;
    setForm((current) => (current ? { ...current, values: { ...current.values, [key]: value } } : current));
  }, []);

  const view = useMemo<CardActionsEditorView | undefined>(() => (open && shown && daemon
    ? { daemons, daemon, ...(catalog ? { catalog } : {}), loading, refreshing, ...(rereadState ? { reread: rereadState } : {}), ...(error ? { error } : {}), ...(form ? { form } : {}), ...(removing ? { removing } : {}), busy }
    : undefined), [busy, catalog, daemon, daemons, error, form, loading, open, refreshing, rereadState, removing, shown]);

  return { shown, view, controls, setField };
}
