import { useCallback, useEffect, useRef, useState } from "react";
import type { DeckPrompt, DeckPromptLibrary } from "../types";
import { modeScopedKey } from "../lib/bridge";

/**
 * The library as this app kept it on this device before decks owned it
 * (issue #824). Read to hand it over to a deck that keeps prompts, and removed
 * once that deck has taken it; still written for a deck from before the
 * library, which has nowhere else to keep it.
 */
const PROMPTS_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.prompts.v1");

/** How long an edit waits for the next keystroke before it is saved to the deck. */
export const PROMPT_SAVE_DELAY_MS = 400;

interface StoredPrompts {
  prompts?: DeckPrompt[];
}

function validPrompt(value: unknown): value is DeckPrompt {
  if (!value || typeof value !== "object") return false;
  const prompt = value as Partial<DeckPrompt>;
  return typeof prompt.id === "string"
    && typeof prompt.name === "string"
    && typeof prompt.body === "string"
    && (prompt.note === undefined || typeof prompt.note === "string");
}

function readDevicePrompts(): DeckPrompt[] {
  try {
    const stored = JSON.parse(window.localStorage.getItem(PROMPTS_STORAGE_KEY) ?? "null") as StoredPrompts | null;
    return stored?.prompts?.filter(validPrompt) ?? [];
  } catch {
    return [];
  }
}

function writeDevicePrompts(prompts: DeckPrompt[]) {
  try {
    if (prompts.length) window.localStorage.setItem(PROMPTS_STORAGE_KEY, JSON.stringify({ prompts } satisfies StoredPrompts));
    else window.localStorage.removeItem(PROMPTS_STORAGE_KEY);
  } catch {
    // The library remains usable for the current session when storage is unavailable.
  }
}

function removeDevicePrompts() {
  try {
    window.localStorage.removeItem(PROMPTS_STORAGE_KEY);
  } catch {
    // Nothing to do: a key that cannot be removed cannot be read either.
  }
}

function mintPromptId(): string {
  return typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `prompt-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Where the library on screen comes from.
 *
 * - `deck` — the selected daemon keeps it, and every change is saved there.
 * - `device` — kept on this device: the selected daemon is from before the
 *   library (`older-deck`), or this runtime has no deck to ask (`no-runtime`).
 * - `loading` — the daemon is being asked.
 * - `no-deck` — no single connected daemon is selected, so there is no library
 *   to show.
 * - `unavailable` — the daemon could not be asked; `message` says why.
 */
export type PromptLibrarySource =
  | { kind: "deck" }
  | { kind: "device"; reason: "older-deck" | "no-runtime" }
  | { kind: "loading" }
  | { kind: "no-deck" }
  | { kind: "unavailable"; message: string };

/** What the library needs from the deck runtime: the deck to ask, and the four verbs. */
export interface PromptLibraryAccess {
  /** The selected, connected deck. Absent under All Decks or with no connected deck. */
  deckId?: string;
  listPrompts?: (deckId: string) => Promise<DeckPromptLibrary>;
  savePrompt?: (deckId: string, prompt: DeckPrompt) => Promise<DeckPromptLibrary>;
  deletePrompt?: (deckId: string, id: string) => Promise<DeckPromptLibrary>;
  seedPrompts?: (deckId: string, prompts: DeckPrompt[]) => Promise<DeckPromptLibrary>;
}

interface LibraryState {
  prompts: DeckPrompt[];
  source: PromptLibrarySource;
}

function plural(count: number): string {
  return count === 1 ? "1 prompt" : `${count} prompts`;
}

/**
 * The prompt library — reusable text for an orchestration's task prompt —
 * as the selected daemon keeps it (issue #824).
 *
 * It used to be kept in this device's `localStorage`, on the wrong side of PRD
 * #803's ownership boundary: text that describes the work belongs to the deck
 * the work runs on, so the same deck shows the same library from any client.
 * The first time a deck that keeps prompts is selected, the library this
 * device kept is handed to it ({@link PromptLibraryAccess.seedPrompts}, which
 * adds only what the deck lacks and replaces nothing) and removed from this
 * device once the deck has taken it. If the hand-over is refused, nothing is
 * removed and it is tried again on the next load.
 *
 * Edits show at once and are saved to the deck {@link PROMPT_SAVE_DELAY_MS}
 * after the last keystroke; a pending save is sent when the deck changes or
 * the app closes the library. A deck from before the library keeps the old
 * behaviour: the library on this device, written on every change.
 */
export function usePromptLibrary(access: PromptLibraryAccess = {}) {
  const { deckId, listPrompts, savePrompt, deletePrompt, seedPrompts } = access;
  const onDevice = !listPrompts;
  const [state, setState] = useState<LibraryState>(() => (onDevice
    ? { prompts: readDevicePrompts(), source: { kind: "device", reason: "no-runtime" } }
    : { prompts: [], source: deckId ? { kind: "loading" } : { kind: "no-deck" } }));
  const [notice, setNotice] = useState<string>();
  const promptsRef = useRef(state.prompts);
  promptsRef.current = state.prompts;
  const saveRef = useRef(savePrompt);
  saveRef.current = savePrompt;
  const pending = useRef(new Map<string, { deckId: string; timer: ReturnType<typeof setTimeout> }>());

  const reportSaveFailure = useCallback((error: unknown) => {
    setNotice(`Not saved to the daemon: ${errorText(error)}`);
  }, []);

  /** Send the pending save for `id` now, to the deck it was made on. */
  const flush = useCallback((id: string) => {
    const entry = pending.current.get(id);
    if (!entry) return;
    clearTimeout(entry.timer);
    pending.current.delete(id);
    const prompt = promptsRef.current.find((candidate) => candidate.id === id);
    const save = saveRef.current;
    if (prompt && save) save(entry.deckId, prompt).catch(reportSaveFailure);
  }, [reportSaveFailure]);

  useEffect(() => {
    if (onDevice) return;
    if (!deckId) {
      setState({ prompts: [], source: { kind: "no-deck" } });
      setNotice(undefined);
      return;
    }
    let current = true;
    setState({ prompts: [], source: { kind: "loading" } });
    setNotice(undefined);
    void (async () => {
      let library: DeckPromptLibrary;
      try {
        library = await listPrompts(deckId);
      } catch (error) {
        if (current) setState({ prompts: [], source: { kind: "unavailable", message: errorText(error) } });
        return;
      }
      if (library.kind === "unsupported") {
        if (current) setState({ prompts: readDevicePrompts(), source: { kind: "device", reason: "older-deck" } });
        return;
      }
      const held = readDevicePrompts();
      if (!held.length || !seedPrompts) {
        if (current) setState({ prompts: library.prompts, source: { kind: "deck" } });
        return;
      }
      try {
        const seeded = await seedPrompts(deckId, held);
        // Removed even if the user has moved on: the deck has taken them.
        if (seeded.kind === "deck") removeDevicePrompts();
        if (!current) return;
        if (seeded.kind === "deck") {
          setState({ prompts: seeded.prompts, source: { kind: "deck" } });
          setNotice(`Moved ${plural(held.length)} kept on this device into this daemon's library.`);
        } else {
          setState({ prompts: readDevicePrompts(), source: { kind: "device", reason: "older-deck" } });
        }
      } catch (error) {
        if (!current) return;
        setState({ prompts: library.prompts, source: { kind: "deck" } });
        setNotice(`The ${plural(held.length)} kept on this device could not be moved to this daemon: ${errorText(error)} They stay on this device, and the move is tried again the next time this library loads.`);
      }
    })();
    const saves = pending.current;
    return () => {
      current = false;
      for (const id of [...saves.keys()]) flush(id);
    };
  }, [deckId, onDevice, listPrompts, seedPrompts, flush]);

  // A deck from before the library has nowhere else to keep it.
  useEffect(() => {
    if (state.source.kind === "device") writeDevicePrompts(state.prompts);
  }, [state]);

  const onDeck = state.source.kind === "deck" && deckId !== undefined;
  const editable = state.source.kind === "deck" || state.source.kind === "device";

  const addPrompt = (): string | undefined => {
    if (!editable) return undefined;
    const prompt: DeckPrompt = { id: mintPromptId(), name: "New prompt", body: "", note: "" };
    setState((current) => ({ ...current, prompts: [...current.prompts, prompt] }));
    if (onDeck && savePrompt) savePrompt(deckId, prompt).catch(reportSaveFailure);
    return prompt.id;
  };

  const updatePrompt = (id: string, updates: Partial<DeckPrompt>) => {
    if (!editable) return;
    setState((current) => ({ ...current, prompts: current.prompts.map((prompt) => (prompt.id === id ? { ...prompt, ...updates, id: prompt.id } : prompt)) }));
    if (!onDeck) return;
    const waiting = pending.current.get(id);
    if (waiting) clearTimeout(waiting.timer);
    pending.current.set(id, { deckId, timer: setTimeout(() => flush(id), PROMPT_SAVE_DELAY_MS) });
  };

  const removePrompt = (id: string) => {
    if (!editable) return;
    const waiting = pending.current.get(id);
    if (waiting) {
      clearTimeout(waiting.timer);
      pending.current.delete(id);
    }
    setState((current) => ({ ...current, prompts: current.prompts.filter((prompt) => prompt.id !== id) }));
    if (onDeck && deletePrompt) deletePrompt(deckId, id).catch(reportSaveFailure);
  };

  return { prompts: state.prompts, source: state.source, notice, addPrompt, updatePrompt, removePrompt };
}
