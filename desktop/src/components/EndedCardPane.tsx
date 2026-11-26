import { useEffect, useMemo, useRef } from "react";
import { BellRing, ChevronLeft, ChevronRight, IdCard, SquareTerminal, Unplug, X } from "lucide-react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import { cardStatusClass, cardStatusLabel } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity } from "../lib/displayText";
import { usePaneCards, type PaneCardsChannel } from "../lib/paneCards";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { DeckView, PanelTab } from "../types";
import { AgentCardPanel } from "./AgentCardPanel";

/**
 * PR #2491 review J1 — the pane a card opens when the deck keeps the card but
 * its agent is gone: an orchestration role that exited, whose card the daemon
 * retains with why it ended, its last output, its references and its report.
 * There is no agent to attach a terminal to or to close, so this is a
 * read-only pane backed by the card alone:
 *
 * - its header names the agent with its last status, and offers the moves
 *   between agents and Back to dashboard, but no Close agent;
 * - it has the live pane's Terminal and Card tabs and opens on Card, since
 *   there is no terminal to show (PR #2491 decision 2026-11-26). Its Terminal
 *   tab is kept rather than dropped, so the pane reads like every other
 *   agent's, and it is where the pane says, in one sentence, that the agent
 *   has ended and its terminal is gone;
 * - the Card tab is read-only (PR #2491 audit M1): it offers none of the
 *   card's actions or answers, however the card marks them — the agent list
 *   and the cards are separate listings, so a card can still say enabled for
 *   a moment after its agent has gone — and a press that reached the tab's
 *   members anyway is refused here, before the send.
 *
 * While the card's daemon is not answering, the Card tab stays and says the
 * card is not available (J2), under the name it last showed.
 */
export function EndedCardPane({ view, paneCards, tab, onTabChange, onTabShown, onClose }: {
  view: Extract<DeckView, { kind: "agent" }>;
  paneCards: PaneCardsChannel;
  /** The shell's tab for this opening: Card unless the user switched to Terminal. */
  tab: PanelTab;
  onTabChange: (tab: PanelTab) => void;
  /** Reports the tab shown, for voice's "close the card" (see `paneTab` in `DeckShell`). */
  onTabShown?: (tab: PanelTab) => void;
  onClose: () => void;
}) {
  const paneRef = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.pane);
  const cards = usePaneCards(paneCards);
  const card = cards?.shownFor(view.deckId) ? cards.cardFor(view.deckId, view.agentId, view.incarnation) : undefined;
  /* The name and status the card last showed, for while it is not available. */
  const last = useRef<{ name: string; status: string }>({ name: view.agentId, status: "stopped" });
  if (card) last.current = { name: displayIdentity(card.entry.name, DISPLAY_LIMITS.name, card.entry.pane_id), status: card.entry.status };
  const { name, status } = last.current;
  const controls = cards?.controls;
  /* The panel's members with every card action refused (audit M1): the moves and the references still work. */
  const readOnlyControls = useMemo(() => (controls ? { ...controls, runCardAction: () => `${name} has ended, so nothing was sent.` } : undefined), [controls, name]);
  const shown: PanelTab = tab === "terminal" || !cards ? "terminal" : "card";
  const tabShown = useRef(onTabShown);
  tabShown.current = onTabShown;
  useEffect(() => { tabShown.current?.(shown); }, [shown]);
  useEffect(() => () => { tabShown.current?.("terminal"); }, []);
  const needsYou = card?.entry.needs_you === true;
  const tabButton = (id: "terminal" | "card", label: string, Icon: typeof IdCard) => {
    const named = id === "card" && needsYou ? "Card, needs you" : label;
    return (
      <button key={id} className={shown === id ? "is-active" : ""} role="tab" aria-selected={shown === id} aria-label={named} title={named} data-testid={id === "card" ? "agent-card-tab" : undefined} onMouseDown={(event) => event.stopPropagation()} onClick={() => onTabChange(id)}>
        <Icon size={13} aria-hidden="true" />
        <span>{label}</span>
        {id === "card" && needsYou && <em className="agent-tab-bell" data-testid="card-tab-bell"><BellRing size={10} aria-hidden="true" /></em>}
      </button>
    );
  };
  return (
    <div ref={paneRef} className="agent-pane-overlay" data-testid="agent-pane-overlay" role="dialog" aria-modal="true" aria-label={`${name} agent`} tabIndex={-1}>
      <article className="agent-tile is-selected" data-presentation="overlay" data-agent-record="ended" data-testid="ended-card-pane" aria-label={name}>
        <header className="agent-header">
          <div className="agent-identity">
            <span className={`agent-state-mark ${cardStatusClass(status)} is-held`} aria-hidden="true" />
            <div>
              <div className="agent-title-line">
                <h2 title={name}>{name}</h2>
                <span className={`status-label ${cardStatusClass(status)} is-held`} data-testid="ended-pane-status">{`last seen: ${cardStatusLabel(status)}`}</span>
              </div>
            </div>
          </div>
          <div className="agent-header-actions">
            {controls && (
              <div className="agent-pane-card-nav" role="group" aria-label="Agents">
                <button type="button" className="agent-pane-control" data-testid="agent-pane-previous" aria-label="Previous agent" title="Previous agent ([)" onMouseDown={(event) => event.stopPropagation()} onClick={() => VOICE_ACTIONS.previousAgentCard.run(controls)}><ChevronLeft size={14} /></button>
                <button type="button" className="agent-pane-control" data-testid="agent-pane-next" aria-label="Next agent" title="Next agent (])" onMouseDown={(event) => event.stopPropagation()} onClick={() => VOICE_ACTIONS.nextAgentCard.run(controls)}><ChevronRight size={14} /></button>
                <button type="button" className="agent-pane-needs-you" data-testid="agent-pane-next-needs-you" title="Next agent that needs you (n)" onMouseDown={(event) => event.stopPropagation()} onClick={() => VOICE_ACTIONS.nextNeedsYouCard.run(controls)}><BellRing size={13} aria-hidden="true" /><span className="agent-pane-needs-you-words">Next needing you</span></button>
              </div>
            )}
            <button type="button" className="agent-pane-control" aria-label="Back to dashboard" title="Back to dashboard" onMouseDown={(event) => event.stopPropagation()} onClick={onClose}><X size={14} /></button>
          </div>
        </header>
        <div className="agent-tabs" role="tablist" aria-label={`${name} details`}>
          {tabButton("terminal", "Terminal", SquareTerminal)}
          {cards && tabButton("card", "Card", IdCard)}
        </div>
        <div className="agent-panel" role="tabpanel" aria-label={shown === "card" ? `${name}'s card` : `${name}'s terminal`}>
          {shown === "terminal" && (
            <div className="agent-terminal-stack" data-terminal-state="ended">
              <div className="panel-empty terminal-absent" data-testid="terminal-ended" role="status">
                <Unplug size={15} aria-hidden="true" />
                <span>{`${name} has ended, and its terminal is gone.`}</span>
              </div>
            </div>
          )}
          {shown === "card" && cards && (
            <div className="agent-card-tab">
              <AgentCardPanel
                card={card}
                name={name}
                results={card ? cards.resultsFor(card) : {}}
                tree={card !== undefined && cards.treeFor(view.deckId)}
                ended={card ? cards.endedFor(card) : undefined}
                notice={card ? cards.notice : undefined}
                section={view.cardSection}
                controls={readOnlyControls ?? cards.controls}
                readOnly
              />
            </div>
          )}
        </div>
      </article>
    </div>
  );
}
