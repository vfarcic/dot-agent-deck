import { actionLabel, primaryAction, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { cardRef } from "./AgentCard";
import { MoreMenu, type MoreMenuItem } from "./MoreMenu";

/** The members the row menu dispatches through. */
export type RowMenuControls = Pick<VoiceActionContext, "toggleOverviewRowMenu" | "showAgentCard" | "runCardAction">;

/**
 * PRD #2374 — a dashboard table row's ⋯ menu while the cards are shown,
 * holding **Show card**, which opens the agent's pane on its Terminal tab, as
 * every way into an agent does (PR #2491 decision 2026-11-26), and the card's
 * primary action — which opens that pane, on Terminal, under the action's
 * review, showing the exact text: nothing is sent
 * from the label. The
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
  /* The menu shows only the action's label, so it never sends: it opens the
     agent's pane under a review showing exactly what the action does, and the
     user sends it from there (review B8, audit F2). */
  const items: MoreMenuItem[] = [
    { key: "show-card", label: "Show card", testId: "overview-row-show-card", onSelect: () => VOICE_ACTIONS.showAgentCard.run(controls, { ...cardRef(card), opensOn: "terminal" }) },
    ...(primary && primary.id !== "open"
      ? [{
        key: "primary",
        label: displayText(actionLabel(card.entry, primary), DISPLAY_LIMITS.name),
        testId: "overview-row-primary",
        disabled: !primary.enabled,
        title: "Shows exactly what it sends before anything is sent",
        onSelect: () => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: primary.id, cardActionReview: true, cardActionOpenPane: true }),
      }]
      : []),
  ];
  /* The open state is the cards view's (`rowMenu`), changed through the same
     registry entry the menu always used; a press outside it or `Escape`
     closes it, as every ⋯ menu does (PR #2491 decision 2026-11-25). */
  return (
    <MoreMenu
      label={`More for ${name}`}
      title="Show card and more"
      menuLabel={`${name} card`}
      testId="overview-row-menu"
      items={items}
      open={open}
      onToggle={(next) => { if (next !== open) VOICE_ACTIONS.toggleOverviewRowMenu.run(controls, rowKey); }}
    />
  );
}
