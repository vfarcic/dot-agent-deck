/*
 * PRD 2557 milestone 1 — MOCK-UP, never merged. The fleet view as the
 * desktop would draw it: the dispatcher's A2UI messages, rendered through the
 * deck's small fleet catalog with the app's own components and styles, the
 * deck's checked facts filled in from its own lookups.
 */
import { Fragment, useMemo, useState, type ReactNode } from "react";
import { BellRing, Check, Clock, CornerDownRight, MessageSquare, Send, SquareTerminal, TriangleAlert, X } from "lucide-react";
import { GitMergeIcon, IssueClosedIcon, IssueOpenedIcon, QuestionIcon, type Icon } from "@primer/octicons-react";

import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";
import { PullRequestBadge } from "./PullRequestBadge";
import { cardStatusClass, cardStatusLabel } from "../lib/agentCards";
import { referenceView } from "../lib/agentCardsTree";
import type { AgentCardEntry } from "../types";
import {
  DELIVERED, FLEET_CATALOG, LIVE_DECK, STALE, clockAt, deckCheck, dispatcherView, fleetMockCards,
  type A2uiComponent, type FleetMockState,
} from "../data/fleetMock";
import "./FleetViewMock.css";

const ISSUE_ICON: Record<string, Icon> = { open: IssueOpenedIcon, closed: IssueClosedIcon, merged: GitMergeIcon };

/** Backticked spans as code, everything else as plain text (A2UI Text is text; the deck draws no markup but this). */
export function Prose({ text }: { text: string }) {
  const parts = text.split(/(`[^`]+`)/g);
  return <>{parts.map((part, at) => part.startsWith("`") && part.endsWith("`") ? <code key={at}>{part.slice(1, -1)}</code> : <Fragment key={at}>{part}</Fragment>)}</>;
}

export function StatusPill({ status }: { status: string }) {
  return <span className={`status-label card-status ${cardStatusClass(status)}`}>{cardStatusLabel(status)}</span>;
}

/** A pull request or issue the dispatcher named, drawn from the deck's own check — never from the dispatcher's words. */
export function DeckRef({ kind, number }: { kind: "pull_request" | "issue"; number: number }) {
  const ref = deckCheck(kind, number);
  const view = referenceView(ref);
  if (kind === "pull_request" && "pullRequest" in view) {
    const pr = LIVE_DECK.pull_requests[String(number)];
    const words = [
      pr.state,
      pr.state === "open" ? pr.review.replace(/_/g, " ") : undefined,
      pr.ci.failing ? `${pr.failing.join(", ")} failing` : `CI ${pr.ci.passing} passing`,
    ].filter(Boolean).join(" · ");
    return (
      <span className="fleet-ref" title={view.title}>
        <PullRequestBadge pullRequest={view.pullRequest} />
        <span className="fleet-ref-words">{words}</span>
        {pr.mergeable === "dirty" && <span className="fleet-ref-alert"><TriangleAlert size={11} aria-hidden="true" />conflicts with main</span>}
      </span>
    );
  }
  if ("number" in view) {
    const StateIcon = ISSUE_ICON[view.state] ?? QuestionIcon;
    return (
      <span className="fleet-ref" title={view.title}>
        <span className={`pr-badge issue-badge issue-state-${view.state}`}><StateIcon className="pr-badge-state" size={11} aria-hidden="true" /><span className="pr-badge-number">#{view.number}</span></span>
        <span className="fleet-ref-words">{view.state}</span>
      </span>
    );
  }
  return <span className="fleet-ref"><small className="agent-card-ref-text">{"text" in view ? view.text : ""}</small></span>;
}

function DeckChecked({ at }: { at: string }) {
  return <span className="card-source is-deck fleet-checked" title="Checked by the deck">deck checked {at}</span>;
}

interface Ctx {
  byId: Map<string, A2uiComponent>;
  cards: Map<string, AgentCardEntry>;
  state: FleetMockState;
  closeState: ConfirmState;
}

function agentName(ctx: Ctx, key: unknown): string {
  return ctx.cards.get(String(key))?.name ?? String(key);
}

/** One "needs you" item: a Card whose child Column the dispatcher laid out. */
function Item({ ctx, component }: { ctx: Ctx; component: A2uiComponent }) {
  const column = ctx.byId.get(String(component.child));
  const children = ((column?.children as string[]) ?? []).map((id) => ctx.byId.get(id)).filter(Boolean) as A2uiComponent[];
  const title = children.find((child) => child.component === "Text" && child.variant === "h4");
  const text = children.find((child) => child.component === "Text" && child.variant === "body");
  const refs = children.find((child) => child.id.endsWith("-refs"));
  const actions = children.find((child) => child.id.endsWith("-actions"));
  const prompts = ((actions?.children as string[]) ?? []).map((id) => ctx.byId.get(id)).filter((child): child is A2uiComponent => child?.component === "PromptButton");
  const delivered = ctx.state === "delivered" && component.id === DELIVERED.item;
  const target = prompts[0] ? agentName(ctx, prompts[0].agent) : undefined;
  const stale = ctx.state === "stale";
  return (
    <article className={`agent-card fleet-item tone-${delivered ? "working" : "needs_input"}${delivered ? "" : " needs-you"}${stale ? " is-stale" : ""}`} data-testid={`fleet-item-${component.id}`}>
      <header className="fleet-item-head">
        {delivered ? <StatusPill status="working" /> : <BellRing size={13} className="fleet-item-bell" aria-hidden="true" />}
        <strong className="fleet-item-title"><Prose text={String(title?.text ?? "")} /></strong>
      </header>
      {refs && (
        <div className="fleet-item-refs">
          {((refs.children as string[]) ?? []).map((id) => {
            const ref = ctx.byId.get(id);
            return ref?.component === "DeckRef" ? <DeckRef key={id} kind={ref.kind as "pull_request" | "issue"} number={Number(ref.number)} /> : null;
          })}
          <DeckChecked at={stale ? clockAt(STALE.shiftMinutes) : clockAt(ctx.state === "delivered" ? DELIVERED.atMinutes : 0)} />
        </div>
      )}
      <p className="fleet-item-text"><Prose text={String(text?.text ?? "")} /></p>
      {delivered ? (
        <div className="fleet-delivered" data-testid="fleet-delivered">
          <Check size={13} aria-hidden="true" />
          <span>Delivered to <strong>{target}</strong> at {clockAt(DELIVERED.atMinutes)}:</span>
          <q>{DELIVERED.prompt}</q>
          <span className="fleet-delivered-wait"><Clock size={12} aria-hidden="true" />working on it, until {target} reports again</span>
        </div>
      ) : (
        <div className="fleet-actions">
          <span className="fleet-actions-to"><CornerDownRight size={12} aria-hidden="true" />to {target}</span>
          {prompts.map((prompt) => (
            <button key={prompt.id} type="button" className="button secondary compact fleet-prompt" title={`Sends exactly this text to ${target}`}>
              <Send size={12} aria-hidden="true" /><span>{String(prompt.text)}</span>
            </button>
          ))}
          <button type="button" className="button secondary compact fleet-reply" title={`Type your own reply to ${target}`}><MessageSquare size={12} aria-hidden="true" /><span>Reply…</span></button>
        </div>
      )}
    </article>
  );
}

function AgentRow({ ctx, component, refs = true }: { ctx: Ctx; component: A2uiComponent; refs?: boolean }) {
  const card = ctx.cards.get(String(component.agent));
  return (
    <div className="fleet-agent-row">
      <strong className="fleet-agent-name">{card?.name ?? String(component.agent)}</strong>
      {card && <StatusPill status={card.status} />}
      {component.text !== undefined && <span className="fleet-agent-text"><Prose text={String(component.text)} /></span>}
      {refs && card?.checked_refs?.map((ref) => <DeckRef key={`${ref.kind}-${ref.number}`} kind={ref.kind as "pull_request" | "issue"} number={Number(ref.number)} />)}
    </div>
  );
}

function Section({ title, count, tone, aside, children, collapsed = false }: { title: string; count: number; tone: string; aside?: ReactNode; children?: ReactNode; collapsed?: boolean }) {
  return (
    <section className={`fleet-section tone-${tone}${collapsed ? " is-collapsed" : ""}`}>
      <header className="fleet-section-head">
        <h2>{title}<span className="fleet-count">{count}</span></h2>
        {aside}
      </header>
      {children}
    </section>
  );
}

/** The dispatcher's composition, drawn through the catalog. */
function Composed({ ctx }: { ctx: Ctx }) {
  const root = ctx.byId.get("root");
  const lists = ((root?.children as string[]) ?? []).map((id) => ctx.byId.get(id)).filter(Boolean) as A2uiComponent[];
  return (
    <>
      {lists.map((list) => {
        const kids = ((list.children as string[]) ?? []).map((id) => ctx.byId.get(id)).filter(Boolean) as A2uiComponent[];
        if (list.id === "needs") {
          return (
            <Section key={list.id} title={String(list.title)} count={kids.length} tone="needs">
              <div className="fleet-items">{kids.map((kid) => <Item key={kid.id} ctx={ctx} component={kid} />)}</div>
            </Section>
          );
        }
        if (list.id === "close") {
          const action = ctx.byId.get(String(list.action));
          const agents = (action?.agents as string[]) ?? [];
          return (
            <Section key={list.id} title={String(list.title)} count={kids.length} tone="done" aside={(
              <span className="fleet-section-actions">
                <button type="button" className="button danger compact" data-testid="fleet-close-all" title={`Close ${agents.map((key) => agentName(ctx, key)).join(", ")}`}><X size={13} aria-hidden="true" /><span>Close {agents.length}</span></button>
              </span>
            )}>
              <div className={`fleet-rows${ctx.state === "stale" ? " is-stale" : ""}`}>{kids.map((kid) => <AgentRow key={kid.id} ctx={ctx} component={kid} />)}</div>
            </Section>
          );
        }
        return (
          <Section key={list.id} title={String(list.title)} count={kids.length} tone="working" collapsed>
            <div className="fleet-working">
              {kids.map((kid) => {
                const card = ctx.cards.get(String(kid.agent));
                return <span key={kid.id} className="fleet-working-chip" title={String(kid.text)}><i className={`fleet-dot ${cardStatusClass(card?.status ?? "working")}`} aria-hidden="true" />{card?.name}<small><Prose text={String(kid.text)} /></small></span>;
              })}
            </div>
          </Section>
        );
      })}
    </>
  );
}

/** What the deck knows on its own: every agent, its status, what it reported, and the references it named as the deck checked them. */
function DeckFacts({ ctx, heading }: { ctx: Ctx; heading: string }) {
  const entries = [...ctx.cards.values()];
  return (
    <Section title={heading} count={entries.length} tone="facts">
      <div className="fleet-facts">
        {entries.map((card) => (
          <div key={card.key} className={`agent-card fleet-fact tone-${card.status}`}>
            <div className="fleet-agent-row">
              <strong className="fleet-agent-name">{card.name}</strong>
              <span className="agent-card-kind">{card.kind}</span>
              <StatusPill status={card.status} />
              {card.checked_refs?.map((ref) => <DeckRef key={`${ref.kind}-${ref.number}`} kind={ref.kind as "pull_request" | "issue"} number={Number(ref.number)} />)}
            </div>
            {card.report
              ? <p className="fleet-fact-report"><span className="card-source is-agent" title="Written by the agent; the deck did not check it">agent says</span> {card.report.summary}{card.fields.find((field) => field.name === "next-step") && <span className="fleet-fact-next"> · next: {String(card.fields.find((field) => field.name === "next-step")?.value)}</span>}</p>
              : <p className="fleet-fact-report is-none">No report yet</p>}
          </div>
        ))}
      </div>
    </Section>
  );
}

export function FleetViewMock({ state, deckId }: { state: FleetMockState; deckId: string }) {
  const [dismissed, setDismissed] = useState(false);
  const messages = useMemo(() => dispatcherView(), []);
  const ctx = useMemo<Ctx>(() => {
    const byId = new Map<string, A2uiComponent>();
    for (const message of messages) {
      if ("updateComponents" in message) {
        for (const component of message.updateComponents.components) {
          // The deck draws only what its catalog registers; anything else is refused before it is stored.
          if ((FLEET_CATALOG as readonly string[]).includes(component.component)) byId.set(component.id, component);
        }
      }
    }
    const cards = new Map(fleetMockCards(deckId, state).entries.map((entry) => [entry.key, entry]));
    const closeState: ConfirmState = {
      title: "Close 1 agent?",
      body: `dispatch-issue-2533 stops and its pane closes. The deck checked that its pull request #${2550} is merged and #${2533} is closed.`,
      label: "Close 1 agent",
      busyLabel: "Closing…",
      action: async () => undefined,
      keepWorktree: true,
      worktreePreview: async () => ({ kept: null, keepWorktree: "~/code/dot-agent-deck-dispatch-issue-2533" }),
    } as ConfirmState;
    return { byId, cards, state, closeState };
  }, [messages, deckId, state]);
  const dispatcher = [...ctx.cards.values()].find((card) => card.kind === "dispatcher");
  const composed = state !== "first-open";
  let freshness: ReactNode;
  if (state === "first-open") {
    freshness = <span className="fleet-fresh is-asking" data-testid="fleet-freshness"><Clock size={13} aria-hidden="true" />Asked the dispatcher at {clockAt()} · its view replaces this when it arrives</span>;
  } else if (state === "stale") {
    freshness = <span className="fleet-fresh is-stale" data-testid="fleet-freshness"><TriangleAlert size={13} aria-hidden="true" />Dispatcher's view from 13:00 · out of date</span>;
  } else {
    freshness = <span className="fleet-fresh" data-testid="fleet-freshness"><Check size={13} aria-hidden="true" />Dispatcher's view, 13:00 · pull request and issue states checked by the deck at {clockAt(state === "delivered" ? DELIVERED.atMinutes : 0)}</span>;
  }
  return (
    <div className="fleet-view" data-testid="fleet-view" data-mock-state={state}>
      <div className="fleet-head">
        <div className="fleet-head-who">
          <strong>{dispatcher?.name ?? "dispatcher"}</strong>
          {dispatcher && <StatusPill status={dispatcher.status} />}
          <span className="fleet-head-scope">{ctx.cards.size - 1} units</span>
        </div>
        {freshness}
        <button type="button" className="button secondary compact" title={`Open ${dispatcher?.name ?? "dispatcher"}`}><SquareTerminal size={13} aria-hidden="true" /><span>Open dispatcher</span></button>
      </div>
      {state === "stale" && (
        <div className="fleet-stale-banner" role="status" data-testid="fleet-stale">
          <TriangleAlert size={15} aria-hidden="true" />
          <div>
            <strong>The dispatcher has not answered since {clockAt(STALE.lastAnsweredMinutes)}.</strong>
            <span> The deck asked it to bring this view up to date at {clockAt(STALE.askedMinutes)}; it has been working since then. Its last view is below, with the deck's own checks still live, and the agents' own reports under it. Buttons still reach its inbox and wait there until it answers.</span>
          </div>
        </div>
      )}
      {composed && <div className={state === "stale" ? "fleet-stale-body" : undefined}><Composed ctx={ctx} /></div>}
      {state === "first-open" && <DeckFacts ctx={ctx} heading="What the deck knows now" />}
      {state === "stale" && <DeckFacts ctx={ctx} heading="The agents' own reports" />}
      {/* The `safe-to-close` state is drawn with the group's one close action pressed: the deck's own close confirmation. */}
      {state === "safe-to-close" && !dismissed && <ConfirmDialog state={ctx.closeState} onClose={() => setDismissed(true)} />}
    </div>
  );
}
