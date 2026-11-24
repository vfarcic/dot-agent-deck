import { useCallback, useEffect, useMemo, useRef, useState } from "react";

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
  mode: "add" | "edit";
  values: CardActionFormValues;
  /** The action as listed when its form opened (edit). */
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
  form?: CardActionForm;
  /** The action whose removal the editor is asking to confirm. */
  removing?: EditableCardAction;
  /** An edit is being written. */
  busy: boolean;
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

const EMPTY_FORM: CardActionFormValues = { name: "", label: "", prompt: "", description: "", icon: null, placement: "card", confirm: false, always: false };

/** An action's placement as the form holds it: the catalog's own, `card` when it names none the form knows. */
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
 * PRD #2374 decision 2026-11-24 — the Card actions editor: which daemon it
 * shows (the local one, wherever the dashboard lists it, unless the user picks
 * another), that daemon's card actions as it lists them, the form for a new
 * or listed action, and the removal it is asking to confirm. Every change is
 * the daemon's: the editor sends one edit and shows the actions the daemon
 * answers, or the reason it refused, and decides nothing about the catalog
 * itself. A daemon that did not advertise the editor is shown read-only and
 * sent nothing.
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
  const [error, setError] = useState<string | undefined>(undefined);
  const [form, setForm] = useState<CardActionForm | undefined>(undefined);
  const [removing, setRemoving] = useState<EditableCardAction | undefined>(undefined);
  const [busy, setBusy] = useState(false);
  /* Which read is current: an answer to an earlier one (another daemon, a
     closed editor) is dropped. */
  const readSeq = useRef(0);
  /* What voice asked for before the catalog was read, done once it is. */
  const pending = useRef<{ kind: "edit" | "remove"; wanted: { name?: string; label?: string } } | undefined>(undefined);
  const daemon = daemons.find((candidate) => candidate.deckId === deckId) ?? homeDaemon(daemons);

  const load = useCallback((target: CardActionsDaemon | undefined) => {
    const seq = ++readSeq.current;
    setCatalog(undefined);
    setError(undefined);
    setForm(undefined);
    setRemoving(undefined);
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

  /* A daemon that leaves the dashboard while it is shown: show the first one. */
  useEffect(() => {
    if (!open || deckId === undefined || daemons.some((candidate) => candidate.deckId === deckId)) return;
    const home = homeDaemon(daemons);
    setDeckId(home?.deckId);
    load(home);
  }, [daemons, deckId, load, open]);

  const latest = useRef({ open, daemon, catalog, form, removing, busy, shown });
  latest.current = { open, daemon, catalog, form, removing, busy, shown };

  /* Send one edit to the daemon shown; `onDone` runs with its answer. */
  const send = useCallback((edit: CardCatalogEdit, onRefused: (why: string) => void, onDone?: () => void) => {
    const target = latest.current.daemon;
    if (!target || !write) return;
    const seq = readSeq.current;
    setBusy(true);
    write(target.deckId, edit).then((answer) => {
      setBusy(false);
      if (seq !== readSeq.current) return;
      setCatalog(answer);
      setError(undefined);
      onDone?.();
    }, (failure: unknown) => {
      setBusy(false);
      if (seq !== readSeq.current) return;
      onRefused(reason(failure));
    });
  }, [write]);

  const startForm = useCallback((action: EditableCardAction | undefined) => {
    setRemoving(undefined);
    setError(undefined);
    setForm(action
      ? { mode: "edit", original: action, values: { name: action.name, label: action.label, prompt: action.prompt, description: action.description, icon: action.icon, placement: formPlacement(action.placement), confirm: action.confirm, always: action.always } }
      : { mode: "add", values: { ...EMPTY_FORM } });
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

  const controls: Pick<VoiceActionContext, CardActionsEditorMember> = {
    openCardActionsEditor: openEditor,
    closeCardActionsEditor: () => {
      if (!latest.current.open) return NOT_OPEN;
      if (latest.current.removing) { setRemoving(undefined); return undefined; }
      readSeq.current += 1;
      pending.current = undefined;
      setOpen(false);
      setCatalog(undefined);
      setForm(undefined);
      setError(undefined);
      setLoading(false);
      return undefined;
    },
    chooseCardActionsDaemon: (wanted) => {
      if (!latest.current.open) return NOT_OPEN;
      const target = daemons.find((candidate) => candidate.deckId === wanted);
      if (!target) return "That daemon is not on the dashboard.";
      if (target.deckId === latest.current.daemon?.deckId) return undefined;
      pending.current = undefined;
      setDeckId(target.deckId);
      load(target);
      return undefined;
    },
    addCardAction: () => {
      const now = latest.current;
      if (!now.shown) return NOT_AVAILABLE;
      const target = now.open ? now.daemon : homeDaemon(daemons);
      if (!now.open) openEditor();
      if (!target?.editable) return readOnly(displayText(target?.name ?? "This", DISPLAY_LIMITS.name));
      startForm(undefined);
      return undefined;
    },
    editCardAction: (wanted) => editing((listed) => {
      const action = findAction(listed, wanted);
      if (!action) return unknown(wanted);
      startForm(action);
      return undefined;
    }, { kind: "edit", wanted }),
    removeCardAction: (wanted) => editing((listed) => {
      const action = findAction(listed, wanted);
      if (!action) return unknown(wanted);
      return askRemove(action);
    }, { kind: "remove", wanted }),
    confirmRemoveCardAction: () => {
      const action = latest.current.removing;
      if (!action) return NOT_ASKING;
      if (latest.current.busy) return STILL_SAVING;
      setRemoving(undefined);
      send({ kind: "remove", name: action.name }, (why) => setError(`${displayText(action.label, DISPLAY_LIMITS.name)} was not removed: ${why}`), () => {
        setForm((current) => (current?.original?.name === action.name ? undefined : current));
      });
      return undefined;
    },
    hideCardAction: (name) => editing((listed) => {
      const action = listed.actions.find((candidate) => candidate.name === name);
      if (!action?.default) return unknown({ name });
      if (latest.current.busy) return STILL_SAVING;
      send({ kind: "hide", name }, (why) => setError(`${displayText(action.label, DISPLAY_LIMITS.name)} was not hidden: ${why}`));
      return undefined;
    }),
    restoreCardAction: (name) => editing((listed) => {
      const action = listed.actions.find((candidate) => candidate.name === name);
      if (!action?.default) return unknown({ name });
      if (latest.current.busy) return STILL_SAVING;
      send({ kind: "restore", name }, (why) => setError(`${displayText(action.label, DISPLAY_LIMITS.name)} was not restored: ${why}`), () => {
        setForm((current) => (current?.original?.name === name ? undefined : current));
      });
      return undefined;
    }),
    saveCardAction: () => {
      const current = latest.current.form;
      if (!current) return NO_FORM;
      if (latest.current.busy) return STILL_SAVING;
      if (!latest.current.daemon?.editable) return readOnly(displayText(latest.current.daemon?.name ?? "This", DISPLAY_LIMITS.name));
      const edit: CardCatalogEdit = { kind: current.mode === "add" ? "add" : "update", action: formInput(current) };
      setForm({ ...current, error: undefined });
      send(edit, (why) => setForm((now) => (now ? { ...now, error: why } : now)), () => setForm(undefined));
      return undefined;
    },
    cancelCardActionForm: () => {
      if (!latest.current.form) return NO_FORM;
      setForm(undefined);
      return undefined;
    },
    chooseCardActionIcon: (name) => {
      if (!latest.current.form) return NO_FORM;
      setForm((current) => (current ? { ...current, values: { ...current.values, icon: name } } : current));
      return undefined;
    },
  };

  const setField = useCallback(<K extends keyof CardActionFormValues>(key: K, value: CardActionFormValues[K]) => {
    setForm((current) => (current ? { ...current, values: { ...current.values, [key]: value } } : current));
  }, []);

  const view = useMemo<CardActionsEditorView | undefined>(() => (open && shown && daemon
    ? { daemons, daemon, ...(catalog ? { catalog } : {}), loading, ...(error ? { error } : {}), ...(form ? { form } : {}), ...(removing ? { removing } : {}), busy }
    : undefined), [busy, catalog, daemon, daemons, error, form, loading, open, removing, shown]);

  return { shown, view, controls, setField };
}
