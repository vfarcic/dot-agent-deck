import { useCallback, useEffect, useRef, useState } from "react";
import type { DeckFleet } from "../types";
import { agentsStoppedByRestartNotice, type StoppedAgent } from "../lib/agentIdentity";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { observeRestarts, type HeldListings } from "../lib/restartDrop";

/** `agent` with each name clamped as a name, since the toast clamps none of the sentence. */
function clampNames({ label, role, orchestration }: StoppedAgent): StoppedAgent {
  const name = (value: string) => displayText(value, DISPLAY_LIMITS.name);
  return { label: name(label), role: role === undefined ? undefined : name(role), orchestration: orchestration === undefined ? undefined : name(orchestration) };
}

/**
 * PRD #1666 part 2 (G-fix7c) — the one notice the app shows when a deck it
 * observes restarts: the agents the restart stopped, by label, role and
 * orchestration ({@link agentsStoppedByRestartNotice}). Their rows are already
 * gone from the fleet, and an open pane of one closes on its own; this says
 * why. Held until dismissed (G-fix7d): the agents a later restart stops join
 * the notice rather than replace the ones the user has not acknowledged, as
 * in the TUI (CLAUDE.md rule 22).
 */
export function useRestartNotice(fleet: DeckFleet): { notice?: string; dismiss: () => void } {
  const held = useRef<HeldListings>(new Map());
  // voice-registry-exempt: the agents the restart notice names, reporting what deck restarts stopped
  const [stopped, setStopped] = useState<readonly StoppedAgent[]>([]);
  useEffect(() => {
    const { held: next, stopped: more } = observeRestarts(held.current, fleet);
    held.current = next;
    if (more.length > 0) setStopped((shown) => [...shown, ...more.map(clampNames)]);
  }, [fleet]);
  const dismiss = useCallback(() => setStopped([]), []);
  return { notice: stopped.length > 0 ? agentsStoppedByRestartNotice(stopped) : undefined, dismiss };
}
