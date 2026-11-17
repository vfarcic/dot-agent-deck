import { useEffect, useRef } from "react";
import { BellRing, Ellipsis } from "lucide-react";

import { actionLabel, needsYouSentence, primaryAction, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { cardRef } from "./AgentCard";

/** The members the row menu dispatches through. */
export type RowMenuControls = Pick<VoiceActionContext, "toggleOverviewRowMenu" | "showAgentCard" | "runCardAction">;

/**
 * PRD #2374 — a dashboard table row's additions while the cards are shown:
 * the needs-you marker the daemon set, and a ⋯ menu holding **Show card**
 * and the card's primary action. The row's own click still opens the agent;
 * both controls are buttons, which the row's click handler leaves alone.
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
    <>
      {card.entry.needs_you && <span className="overview-needs-you" data-testid="overview-needs-you" title={needsYouSentence(card.entry)} aria-label="Needs you"><BellRing size={11} aria-hidden="true" /></span>}
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
            {primary && primary.id !== "open" && (
              <button
                type="button"
                role="menuitem"
                data-testid="overview-row-primary"
                disabled={!primary.enabled}
                onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: primary.id })}
              >{displayText(actionLabel(card.entry, primary), DISPLAY_LIMITS.name)}</button>
            )}
          </div>
        )}
      </div>
    </>
  );
}
