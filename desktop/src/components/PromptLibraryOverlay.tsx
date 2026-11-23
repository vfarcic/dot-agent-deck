import { useEffect, useState } from "react";
import type { DesktopSettingsState } from "../hooks/useDesktopSettings";
import { usePromptLibrary } from "../hooks/usePromptLibrary";
import { selectsAllDecks } from "../lib/endpoints";
import type { DeckRuntimeState } from "../types";
import { PromptLibraryPanel } from "./ConfigurationPanels";

export type ShellPromptLibrary = ReturnType<typeof usePromptLibrary>;

/**
 * Issue #1310 — the prompt library, held by the shell rather than by the deck,
 * so its panel opens over whichever screen is up, as Settings does. The deck
 * screen is behind the experimental flag and the library is not, so a library
 * only the deck mounted would be a rail entry that opens nothing.
 *
 * It is the selected daemon's library (issue #824): none under All Daemons or
 * while that daemon is not connected. All Daemons is read from the stored
 * selection and from the snapshot, as the deck reads it, so a start with All
 * Daemons stored does not load the local daemon's library first.
 *
 * Call it ONCE per tree and hand `prompts` to whatever else reads them (the
 * orchestration editor): each instance loads, and hands the old device copy
 * over to the daemon, on its own.
 *
 * However the panel closes (its button, Escape, a voice action that dismisses
 * every overlay), a pending edit is saved at once rather than on its timer.
 */
export function useShellPromptLibrary(runtime: DeckRuntimeState, settings: DesktopSettingsState, open: boolean): ShellPromptLibrary {
  const allDecks = selectsAllDecks(settings.settings.endpoints) || runtime.snapshot.allDecks === true;
  const connection = runtime.snapshot.connection;
  const library = usePromptLibrary({
    deckId: !allDecks && connection.status === "connected" ? connection.deckId : undefined,
    listPrompts: runtime.listPrompts,
    savePrompt: runtime.savePrompt,
    deletePrompt: runtime.deletePrompt,
    seedPrompts: runtime.seedPrompts,
  });
  const { flushPending } = library;
  useEffect(() => {
    if (!open) flushPending();
  }, [open, flushPending]);
  return library;
}

/** The Prompts panel over the shell's screen, editing {@link useShellPromptLibrary}'s library. */
export function PromptLibraryOverlay({ library, open, onClose }: { library: ShellPromptLibrary; open: boolean; onClose: () => void }) {
  const { prompts, source, notice, addPrompt, updatePrompt, removePrompt } = library;
  const [selectedId, setSelectedId] = useState(""); // voice-registry-exempt: which stored prompt the library panel is editing — a selection within that panel
  useEffect(() => {
    if (!selectedId || !prompts.some((prompt) => prompt.id === selectedId)) setSelectedId(prompts[0]?.id ?? "");
  }, [prompts, selectedId]);
  return (
    <PromptLibraryPanel
      open={open}
      prompts={prompts}
      selectedId={selectedId}
      onSelect={setSelectedId}
      onClose={onClose}
      source={source}
      notice={notice}
      onAdd={() => { const id = addPrompt(); if (id) setSelectedId(id); }}
      onUpdate={updatePrompt}
      onRemove={removePrompt}
    />
  );
}
