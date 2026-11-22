import { useEffect, useRef } from "react";
import { Ellipsis } from "lucide-react";

import { actionLabel, primaryAction, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { cardRef } from "./AgentCard";

/** The members the row menu dispatches through. */
export type RowMenuControls = Pick<VoiceActionContext, "toggleOverviewRowMenu" | "showAgentCard" | "runCardAction">;

/**
 * PRD #2374 — a dashboard table row's ⋯ menu while the cards are shown,
 * holding **Show card** and the card's primary action — which opens the card
 * at a review step showing the exact text, never sends from the label. The
 * row's own click still opens the agent; the menu's controls are buttons,
 * which the row's click handler leaves alone. The needs-you badge the daemon
 * sets is drawn beside the row's name, not here (issue #2454).
 */
export function OverviewRowCard({ card, name, rowKey, open, controls }: {
  card: PlacedCard;
  name: string;
  rowKey: string;
  open: boolean;
  controls: RowMenuControls;
}) {
  const primary = primaryAction(card.entry);
  const menuRef = useRef<HTMLDivElement>(null);
  /* A click anywhere outside the open menu, or `Escape`, closes it: the
     dismissal every menu here has, through the same registry entry as the
     button that opened it. */
  useEffect(() => {
    if (!open) return;
    const onPointer = (event: MouseEvent) => {
      if (menuRef.current && event.target instanceof Node && menuRef.current.contains(event.target)) return;
      VOICE_ACTIONS.toggleOverviewRowMenu.run(controls, rowKey);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") VOICE_ACTIONS.toggleOverviewRowMenu.run(controls, rowKey);
    };
    document.addEventListener("mousedown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [controls, open, rowKey]);
  return (
    <div className="overview-card-menu" ref={menuRef}>
      <button
        type="button"
        className="overview-card-menu-button"
        data-testid="overview-row-menu"
        aria-label={`More for ${name}`}
        aria-haspopup="menu"
        aria-expanded={open}
        title="Show card and more"
        onClick={() => VOICE_ACTIONS.toggleOverviewRowMenu.run(controls, rowKey)}
      ><Ellipsis size={12} /></button>
      {open && (
        <div className="overview-card-menu-list" role="menu" aria-label={`${name} card`}>
          <button type="button" role="menuitem" data-testid="overview-row-show-card" onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, cardRef(card))}>Show card</button>
          {/* The menu shows only the action's label, so it never sends: it
              opens the card at a step showing exactly what the action does,
              and the user sends it from there (review B8, audit F2). */}
          {primary && primary.id !== "open" && (
            <button
              type="button"
              role="menuitem"
              data-testid="overview-row-primary"
              disabled={!primary.enabled}
              title="Shows exactly what it sends before anything is sent"
              onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: primary.id, cardActionReview: true })}
            >{displayText(actionLabel(card.entry, primary), DISPLAY_LIMITS.name)}</button>
          )}
        </div>
      )}
    </div>
  );
}
