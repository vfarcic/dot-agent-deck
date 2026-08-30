import { useEffect, useState } from "react";
import type { DeckPrompt } from "../types";
import { modeScopedKey } from "../lib/bridge";

const PROMPTS_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.prompts.v1");

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

function mintPromptId(): string {
  return typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `prompt-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

/**
 * Device-local prompt library, mirroring `useProjects`. Unlike projects there
 * is nothing sensible to seed from the runtime, so an empty library stays
 * empty until the operator saves something.
 */
export function usePromptLibrary() {
  const [prompts, setPrompts] = useState<DeckPrompt[]>(() => {
    try {
      const stored = JSON.parse(window.localStorage.getItem(PROMPTS_STORAGE_KEY) ?? "null") as StoredPrompts | null;
      return stored?.prompts?.filter(validPrompt) ?? [];
    } catch {
      return [];
    }
  });

  useEffect(() => {
    try {
      window.localStorage.setItem(PROMPTS_STORAGE_KEY, JSON.stringify({ prompts } satisfies StoredPrompts));
    } catch {
      // The library remains usable for the current session when storage is unavailable.
    }
  }, [prompts]);

  const addPrompt = () => {
    const prompt: DeckPrompt = { id: mintPromptId(), name: "New prompt", body: "", note: "" };
    setPrompts((current) => [...current, prompt]);
    return prompt.id;
  };

  const updatePrompt = (id: string, updates: Partial<DeckPrompt>) => {
    setPrompts((current) => current.map((prompt) => prompt.id === id ? { ...prompt, ...updates, id: prompt.id } : prompt));
  };

  const removePrompt = (id: string) => {
    setPrompts((current) => current.filter((prompt) => prompt.id !== id));
  };

  return { prompts, addPrompt, updatePrompt, removePrompt };
}
