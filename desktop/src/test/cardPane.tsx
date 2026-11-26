import { useCallback, useRef, useState, type ComponentProps } from "react";

import { AgentOverview } from "../components/AgentOverview";
import { AgentCardPanel } from "../components/AgentCardPanel";
import { createPaneCardsChannel, usePaneCards } from "../lib/paneCards";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { DeckView } from "../types";

/**
 * PR #2491 decision 2026-11-25 — the dashboard with the agent pane's Card
 * tab beside it, for the component tests of what a card shows and does.
 *
 * The shell (`DeckShell`) draws the pane as the dashboard's sibling and feeds
 * its Card tab from the channel the dashboard publishes to; this renders the
 * same dashboard, the same channel and the real `AgentCardPanel` (the Card
 * tab's content) for the agent the last navigation opened, always shown, and
 * the pane header's three moves as buttons dispatching what the header's do.
 * `data-card-tab` says whether the navigation asked for the Card tab. What the
 * pane itself does — which tab it opens on, the header, the keys, the terminal
 * under the Card tab — is `AgentPaneCardTab.test.tsx`'s, on the real shell.
 */
export function OverviewWithCardPane({ onNavigate, ...props }: Omit<ComponentProps<typeof AgentOverview>, "onNavigate" | "agentPane" | "agentPaneOpen" | "paneCards"> & { onNavigate?: (view: DeckView) => void }) {
  const [channel] = useState(createPaneCardsChannel);
  // The last view navigated to, as the shell holds it.
  const [view, setView] = useState<DeckView>({ kind: "overview" });
  /* Stable, as the shell's `setView` is: the dashboard's members are built from it. */
  const latestNavigate = useRef(onNavigate);
  latestNavigate.current = onNavigate;
  const navigate = useCallback((next: DeckView) => {
    latestNavigate.current?.(next);
    setView(next);
  }, []);
  const pane = view.kind === "agent" ? { deckId: view.deckId, agentId: view.agentId, ...(view.incarnation !== undefined ? { incarnation: view.incarnation } : {}) } : undefined;
  const cards = usePaneCards(channel);
  const card = pane && cards?.cardFor(pane.deckId, pane.agentId, pane.incarnation);
  return (
    <>
      <AgentOverview {...props} onNavigate={navigate} agentPane={pane} agentPaneOpen={pane !== undefined} paneCards={channel} />
      {pane && cards && cards.shownFor(pane.deckId) && (
        <section data-testid="test-agent-pane" data-agent-id={pane.agentId} data-card-tab={view.kind === "agent" && view.cardTab ? "open" : undefined} data-card-section={view.kind === "agent" ? view.cardSection : undefined}>
          <button type="button" aria-label="Previous agent" onClick={() => VOICE_ACTIONS.previousAgentCard.run(cards.controls)}>Previous</button>
          <button type="button" aria-label="Next agent" onClick={() => VOICE_ACTIONS.nextAgentCard.run(cards.controls)}>Next</button>
          <button type="button" aria-label="Next agent that needs you" onClick={() => VOICE_ACTIONS.nextNeedsYouCard.run(cards.controls)}>Next needing you</button>
          <AgentCardPanel card={card} name={pane.agentId} results={card ? cards.resultsFor(card) : {}} tree={cards.treeFor(pane.deckId)} ended={card ? cards.endedFor(card) : undefined} notice={cards.notice} section={view.kind === "agent" ? view.cardSection : undefined} controls={cards.controls} cardActions={cards.cardActionsFor?.(pane.deckId) ?? false} />
        </section>
      )}
    </>
  );
}
