import { useCallback, useEffect, useRef, useState } from "react";
import type { DeckFleet } from "../types";
import { agentsStoppedByRestartNotice } from "../lib/agentIdentity";
import { observeRestarts, type HeldListings } from "../lib/restartDrop";

/**
 * PRD #1666 part 2 (G-fix7c) — the one notice the app shows when a deck it
 * observes restarts: the agents the restart stopped, by label, role and
 * orchestration ({@link agentsStoppedByRestartNotice}). Their rows are already
 * gone from the fleet, and an open pane of one closes on its own; this says
 * why. Held until dismissed, or until a later restart replaces it.
 */
export function useRestartNotice(fleet: DeckFleet): { notice?: string; dismiss: () => void } {
  const held = useRef<HeldListings>(new Map());
  // voice-registry-exempt: the restart notice's sentence, reporting what a deck restart stopped
  const [notice, setNotice] = useState<string>();
  useEffect(() => {
    const { held: next, stopped } = observeRestarts(held.current, fleet);
    held.current = next;
    if (stopped.length > 0) setNotice(agentsStoppedByRestartNotice(stopped));
  }, [fleet]);
  const dismiss = useCallback(() => setNotice(undefined), []);
  return { notice, dismiss };
}
