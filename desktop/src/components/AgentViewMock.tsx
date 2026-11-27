/*
 * PRD 2557 — MOCK-UP, never merged. The fleet view with nothing predefined:
 * the agent's A2UI messages drawn component by component through the catalog
 * in `fleet-mock/agent-view-catalog.md`. This renderer knows no sections,
 * items or order; the only thing the deck adds is the strip at the top (who
 * composed the view and when) and the state inside DeckRef and AgentRef, which
 * it checks itself.
 */
import { useMemo, type ReactNode } from "react";
import {
  BellRing, Check, CircleHelp, Clock, Eye, FlaskConical, GitMerge, MessageSquare, Package, Pause, Pin, Send, SquareTerminal, TriangleAlert, X,
  type LucideIcon,
} from "lucide-react";

import { DeckRef, Prose, StatusPill } from "./FleetViewMock";
import type { AgentCardEntry } from "../types";
import { LIVE_DECK, clockAt, fleetMockCards, type A2uiComponent, type A2uiMessage } from "../data/fleetMock";
import VIEW_A from "../data/fleetMock.agentView.a.json";
import VIEW_B from "../data/fleetMock.agentView.b.json";
import "./AgentViewMock.css";

export const AGENT_VIEW_CATALOG = ["Text", "Column", "Row", "List", "Card", "Divider", "Icon", "Stat", "DeckRef", "AgentRef", "PromptButton", "ReplyButton", "DeckAction"] as const;
const VIEWS: Record<string, A2uiMessage[]> = { a: VIEW_A as unknown as A2uiMessage[], b: VIEW_B as unknown as A2uiMessage[] };

const ICONS: Record<string, LucideIcon> = {
  bell: BellRing, alert: TriangleAlert, check: Check, clock: Clock, merge: GitMerge, pause: Pause, question: CircleHelp,
  terminal: SquareTerminal, flask: FlaskConical, package: Package, eye: Eye, pin: Pin,
};

interface Ctx { byId: Map<string, A2uiComponent>; cards: Map<string, AgentCardEntry> }

const str = (value: unknown, fallback = "") => (value === undefined || value === null ? fallback : String(value));
const ids = (value: unknown): string[] => (Array.isArray(value) ? value.map(String) : value === undefined ? [] : [String(value)]);

function Node({ ctx, id, seen }: { ctx: Ctx; id: string; seen: Set<string> }): ReactNode {
  const node = ctx.byId.get(id);
  if (!node || seen.has(id)) return null;
  const path = new Set(seen).add(id);
  const kids = (list: string[]) => list.map((child) => <Node key={child} ctx={ctx} id={child} seen={path} />);
  const weighted = (list: string[]) => list.map((child) => {
    const weight = Number(ctx.byId.get(child)?.weight);
    return <div key={child} className="av-cell" style={weight > 0 ? { flex: `${weight} 1 0`, minWidth: 0 } : undefined}><Node ctx={ctx} id={child} seen={path} /></div>;
  });
  const gap = `gap-${str(node.gap, "m")}`;
  switch (node.component) {
    case "Text": {
      const variant = str(node.variant, "body");
      const Tag = (variant === "h1" ? "h1" : variant === "h2" ? "h2" : variant === "h3" ? "h3" : variant === "quote" ? "blockquote" : "p") as "p";
      return <Tag className={`av-text v-${variant} tone-${str(node.tone, "default")}`}><Prose text={str(node.text)} /></Tag>;
    }
    case "Column": return <div className={`av-column ${gap}`}>{kids(ids(node.children))}</div>;
    case "Row": return <div className={`av-row ${gap} align-${str(node.align, "center")}${node.wrap === false ? " no-wrap" : ""}`}>{weighted(ids(node.children))}</div>;
    case "List": return <div className={`av-list dir-${str(node.direction, "vertical")}`}>{ids(node.children).map((child) => <div key={child} className="av-list-item"><Node ctx={ctx} id={child} seen={path} /></div>)}</div>;
    case "Card": return <section className={`av-card tone-${str(node.tone, "default")}`}>{kids(ids(node.children ?? node.child))}</section>;
    case "Divider": return <hr className="av-divider" />;
    case "Icon": {
      const Glyph = ICONS[str(node.name)] ?? CircleHelp;
      return <Glyph size={14} className={`av-icon icon-${str(node.name)}`} aria-hidden="true" />;
    }
    case "Stat": return <div className={`av-stat tone-${str(node.tone, "default")}`}><strong>{str(node.value)}</strong><span>{str(node.label)}</span></div>;
    case "DeckRef": return <span className={`av-ref detail-${str(node.detail, "full")}`}><DeckRef kind={node.kind === "issue" ? "issue" : "pull_request"} number={Number(node.number)} /></span>;
    case "AgentRef": {
      const card = [...ctx.cards.values()].find((entry) => entry.name === str(node.agent));
      const pane = LIVE_DECK.daemon_status.find((row) => row.LABEL === str(node.agent));
      return (
        <span className="av-agent">
          <strong>{str(node.agent)}</strong>
          {card && <StatusPill status={card.status} />}
          {node.detail !== "name" && pane && pane.TOOL !== "-" && <small>{pane.TOOL}</small>}
        </span>
      );
    }
    case "PromptButton":
      return (
        <button type="button" className={`button ${node.primary ? "primary" : "secondary"} compact av-prompt`} title={`Sends exactly this text to ${str(node.agent)}`}>
          <Send size={12} aria-hidden="true" /><span>{str(node.text)}</span>
        </button>
      );
    case "ReplyButton":
      return <button type="button" className="button secondary compact av-reply" title={`Type your own reply to ${str(node.agent)}`}><MessageSquare size={12} aria-hidden="true" /><span>{str(node.label, "Reply")}…</span></button>;
    case "DeckAction": {
      const close = node.operation === "close";
      const Glyph = close ? X : SquareTerminal;
      return <button type="button" className={`button ${close ? "danger" : "secondary"} compact`} title={`${close ? "Close" : "Open"} ${ids(node.agents).join(", ")}`}><Glyph size={13} aria-hidden="true" /><span>{str(node.label, close ? "Close" : "Open")}</span></button>;
    }
    default: return null;
  }
}

export function AgentViewMock({ deckId, composer }: { deckId: string; composer: string }) {
  const messages = VIEWS[composer] ?? VIEWS.a;
  const ctx = useMemo(() => {
    const byId = new Map<string, A2uiComponent>();
    const refused: string[] = [];
    for (const message of messages) {
      if (!("updateComponents" in message)) continue;
      for (const component of message.updateComponents.components) {
        // The daemon refuses a type outside the catalog before the view reaches the desktop.
        if ((AGENT_VIEW_CATALOG as readonly string[]).includes(component.component)) byId.set(component.id, component);
        else refused.push(component.component);
      }
    }
    const cards = new Map(fleetMockCards(deckId, "composed").entries.map((entry) => [entry.key, entry]));
    return { byId, cards, refused };
  }, [messages, deckId]);
  const composedAt = messages.flatMap((message) => ("updateDataModel" in message && message.updateDataModel.path === "/composedAt" ? [str(message.updateDataModel.value)] : []))[0];
  const dispatcher = [...ctx.cards.values()].find((card) => card.kind === "dispatcher");
  return (
    <div className="agent-view" data-testid="fleet-view" data-composer={composer}>
      <div className="av-strip">
        <strong>{dispatcher?.name ?? "dispatcher"}</strong>
        {dispatcher && <StatusPill status={dispatcher.status} />}
        <span className="av-strip-fresh"><Check size={13} aria-hidden="true" />Composed by the dispatcher at {composedAt ? clockAt(0, composedAt) : "?"} · everything below is its layout · pull request, issue and agent states checked by the deck at {clockAt()}</span>
        {ctx.refused.length > 0 && <span className="av-strip-refused">{ctx.refused.length} component(s) refused</span>}
        <button type="button" className="button secondary compact" title={`Open ${dispatcher?.name ?? "dispatcher"}`}><SquareTerminal size={13} aria-hidden="true" /><span>Open dispatcher</span></button>
      </div>
      <div className="av-surface"><Node ctx={ctx} id="root" seen={new Set()} /></div>
    </div>
  );
}
