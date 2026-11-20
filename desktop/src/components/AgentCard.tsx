import { GitMergeIcon, IssueClosedIcon, IssueOpenedIcon, QuestionIcon, type Icon } from "@primer/octicons-react";
import { BellRing, ChevronDown, ChevronUp } from "lucide-react";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";

import { actionLabel, cardStatusClass, cardStatusLabel, deliverySentence, disabledReasonSentence, exactTextSegments, exactTextString, hasMarkers, MARKER_CLOSE, MARKER_OPEN, markerTitle, resultSentence, type CardResults, needsYouSentence, placedCardKey, primaryAction, primaryNeedsReview, promptPreview, type PlacedCard } from "../lib/agentCards";
import { agentTypeText, cardTone, referenceUrl, referenceView, rollupSegments, rollupSentence, type CardKind } from "../lib/agentCardsTree";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import type { AgentCardCheckedRef, AgentCardRollup } from "../types";
import { PullRequestBadge } from "./PullRequestBadge";

/** The members a card's own controls dispatch through. */
export type CardControls = Pick<VoiceActionContext, "showAgentCard" | "runCardAction" | "setCardTree" | "openCardReference"> & Partial<Pick<VoiceActionContext, "reportRefused">>;

/** The card a control names: its deck, agent and the daemon process that listed it. */
export function cardRef(card: PlacedCard): { deckId: string; agentId: string; incarnation: string } {
  return { deckId: card.deckId, agentId: card.entry.agent_id, incarnation: card.entry.instance_id };
}

/** A small label saying who a piece of text comes from: the agent's own word, or something the deck checked. */
export function SourceBadge({ source }: { source: "agent" | "deck" }) {
  return source === "agent"
    ? <span className="card-source is-agent" title="Written by the agent; the deck did not check it">agent says</span>
    : <span className="card-source is-deck" title="Checked by the deck">deck checked</span>;
}

/**
 * The exact text a prompt action sends, as it is sent: line breaks kept, and
 * every invisible or reordering character shown as a marker, `⟦…⟧`, styled
 * apart from the text (a boxed code) rather than dropped or applied (audit
 * F3, A3; review R3). Rendered as text, never as markup.
 */
export function ExactText({ text }: { text: string }) {
  return (
    <>
      {exactTextSegments(text).map((segment, index) => (segment.escaped
        ? <span key={index} className="exact-text-escape" title={markerTitle(segment.raw)}>{segment.shown}</span>
        : <span key={index}>{segment.shown}</span>))}
    </>
  );
}

/** What the markers in a shown prompt mean, said under it when it has any. */
export const MARKER_NOTE = `${MARKER_OPEN}…${MARKER_CLOSE} marks a character you cannot see; it is sent as is.`;

/** {@link MARKER_NOTE}, when `text` shows any marker. */
export function ExactTextNote({ text }: { text: string }) {
  return hasMarkers(text) ? <small className="exact-text-note" data-testid="exact-text-note">{MARKER_NOTE}</small> : null;
}

/** Octicons for a checked issue's state, as GitHub draws them. */
const ISSUE_STATE_ICON: Record<string, Icon> = {
  open: IssueOpenedIcon,
  closed: IssueClosedIcon,
  merged: GitMergeIcon,
};

/** The members a reference's link dispatches through. */
export type ReferenceControls = Pick<VoiceActionContext, "openCardReference"> & Partial<Pick<VoiceActionContext, "reportRefused">>;

/**
 * Issue #2418 — one reference the agent reported, drawn from the deck's check
 * of it: a pull request or issue badge that opens its page, or a short text
 * saying it is still to be checked or why it cannot be. The card draws it on
 * its reference line, and the overlay in its list of every reference
 * (`scope: "overlay"`, whose test ids carry `-overlay`); both open a page
 * through the same action, so the same address checks apply.
 */
export function CardReference({ card, reference, controls, scope = "card", hidden = false }: { card: PlacedCard; reference: AgentCardCheckedRef; controls: ReferenceControls; scope?: "card" | "overlay"; hidden?: boolean }) {
  const view = referenceView(reference);
  const url = referenceUrl(reference);
  const id = scope === "overlay" ? "agent-card-overlay" : "agent-card";
  const stale = view.status === "stale" ? <small className="agent-card-ref-stale">stale</small> : null;
  let body: ReactNode;
  if (view.status === "checked" || view.status === "stale") {
    if (view.kind === "pull_request" && "pullRequest" in view) {
      body = <PullRequestBadge pullRequest={view.pullRequest} testId={`${id}-pr`} onOpen={url ? () => VOICE_ACTIONS.openCardPullRequest.run(controls, { ...cardRef(card), referenceUrl: url }) : undefined} />;
    } else if ("number" in view) {
      const StateIcon = ISSUE_STATE_ICON[view.state] ?? QuestionIcon;
      const label = `Issue #${view.number}: ${view.state}`;
      const content = <><StateIcon className="pr-badge-state" size={11} aria-hidden="true" /><span className="pr-badge-number">#{view.number}</span></>;
      body = url
        ? <button type="button" className={`pr-badge issue-badge issue-state-${domIdentity(view.state)}`} data-testid={`${id}-issue`} data-issue-state={view.state} aria-label={label} title={`${label} — open it in the app`} onMouseDown={(event) => event.stopPropagation()} onClick={() => VOICE_ACTIONS.openCardIssue.run(controls, { ...cardRef(card), referenceUrl: url })}>{content}</button>
        : <span className={`pr-badge issue-badge issue-state-${domIdentity(view.state)}`} data-testid={`${id}-issue`} data-issue-state={view.state} role="img" aria-label={label}>{content}</span>;
    }
  } else if ("text" in view) {
    body = <small className="agent-card-ref-text">{displayText(view.text, DISPLAY_LIMITS.name)}</small>;
  }
  return (
    <span className={`agent-card-ref is-${view.status}${hidden ? " is-overflow" : ""}`} data-testid={scope === "overlay" ? "agent-card-overlay-ref-badge" : "agent-card-ref"} data-field={reference.field} data-kind={reference.kind} data-ref-status={view.status} data-overflow={hidden ? "true" : undefined} aria-hidden={hidden ? true : undefined} title={displayTitle(view.title)}>
      {body}{stale}
    </span>
  );
}

/** The width the `+N` control takes at the end of a card's reference line, with the gap before it, in CSS pixels (kept in step with `.agent-card-refs-more`). */
const MORE_WIDTH = 34;
const MORE_GAP = 6;

/**
 * Issue #2418 review (D-R1) — a card's reference line: as many reference
 * badges as fit whole on its one line, in the daemon's order, and a `+N`
 * control counting the rest, which opens the card's overlay at its list of
 * every reference. The line has a fixed height, so references arriving never
 * move a root; a badge that would be cut is hidden whole (not drawn, not
 * focusable) rather than cut, and the count says it is there.
 *
 * Which badges fit is measured after layout, and again when the line's width
 * changes. Every badge stays in the line's flow, the hidden ones invisible, so
 * a measurement never moves the ones before it. Where nothing has a width (no
 * layout), every badge counts as fitting.
 */
function CardReferences({ card, refs, name, controls }: { card: PlacedCard; refs: AgentCardCheckedRef[]; name: string; controls: CardControls }) {
  const line = useRef<HTMLDivElement>(null);
  const [fit, setFit] = useState(refs.length);
  useLayoutEffect(() => {
    const node = line.current;
    if (!node) return undefined;
    const measure = () => {
      const width = node.clientWidth;
      const ends = [...node.querySelectorAll<HTMLElement>(":scope > .agent-card-ref")].map((item) => item.offsetLeft + item.offsetWidth);
      const within = (room: number) => { const first = ends.findIndex((end) => end > room + 0.5); return first < 0 ? ends.length : first; };
      const whole = within(width);
      setFit(whole === ends.length ? whole : within(width - MORE_WIDTH - MORE_GAP));
    };
    measure();
    if (typeof ResizeObserver !== "function") return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [refs]);
  const shown = Math.min(fit, refs.length);
  const more = refs.length - shown;
  const label = `Show all ${refs.length} pull requests and issues on ${name}'s card`;
  return (
    <div ref={line} className="agent-card-refs" aria-label="Pull requests and issues the agent reported, as the deck checked them" data-shown={shown}>
      {refs.map((reference, index) => <CardReference key={`${reference.field}\u0000${index}`} card={card} reference={reference} controls={controls} hidden={index >= shown} />)}
      {more > 0 && (
        <button
          type="button"
          className="agent-card-refs-more"
          data-testid="agent-card-refs-more"
          style={{ width: MORE_WIDTH }}
          aria-label={`${more} more: ${label}`}
          title={`${more} more that do not fit here. ${label}`}
          onMouseDown={(event) => event.stopPropagation()}
          onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, { ...cardRef(card), cardSection: "references" })}
        >+{more}</button>
      )}
    </div>
  );
}

/** Issue #2418 — a roll-up's counts by status, as a bar, and its totals. */
function CardRollup({ rollup }: { rollup: AgentCardRollup }) {
  const segments = rollupSegments(rollup.counts);
  return (
    <div className="agent-card-rollup-block">
      <div className="agent-card-rollup-bar" data-testid="agent-card-rollup-bar" role="img" aria-label={segments.map((segment) => `${segment.count} ${segment.label}`).join(", ")}>
        {segments.map((segment) => <i key={segment.label} className={`tone-${segment.word}`} data-status={segment.word} style={{ flexGrow: segment.count }} />)}
      </div>
      <small className="agent-card-rollup" data-testid="agent-card-rollup">{rollupSentence(rollup)}</small>
    </div>
  );
}

/** How a card sits in the tree: its depth, what is under it, whether its tree is open, and whether Needs you only fades it. */
export interface CardPlace {
  depth: number;
  kind: CardKind;
  /** Whether the daemon hung any shown card under it. */
  hasChildren: boolean;
  open: boolean;
  faded: boolean;
  /** Whether the deck's daemon builds the tree: an older one's cards carry no roll-up or checked references. */
  tree: boolean;
}

/**
 * Issue #2418 — one agent's compact card on the Cards tree: who it is (name,
 * kind, agent type), its status in the daemon's words, whether it needs the
 * user, the pull requests and issues its agent reported as the deck checked
 * them, what is under it (closed, with its roll-up) and its primary action.
 * What the agent said, its progress and its next step are the overlay's, which
 * a click on the name opens. Everything comes from the daemon's entry as
 * received; nothing here decides status, needs-you, roll-ups or references.
 */
export function AgentCard({ card, controls, results, place }: { card: PlacedCard; controls: CardControls; results?: CardResults; place: CardPlace }) {
  const { entry } = card;
  const name = displayIdentity(entry.name, DISPLAY_LIMITS.name, entry.pane_id);
  /* The deck's own Open is not a card's primary here: the name opens the
     card, whose overlay opens the pane (the table's row menu leaves it out
     the same way). */
  const daemonPrimary = primaryAction(entry);
  const primary = daemonPrimary?.kind === "open" ? undefined : daemonPrimary;
  const needsYou = needsYouSentence(entry);
  const primaryTitle = primary && !primary.enabled ? `Disabled: ${disabledReasonSentence(primary.disabled_reason)}` : undefined;
  const rollup = place.tree && entry.rollup ? entry.rollup : undefined;
  const tone = cardTone({ status: entry.status, rollup }, place.open);
  const agentType = agentTypeText(entry.agent_type);
  const refs = place.tree ? entry.checked_refs ?? [] : [];
  const ref = cardRef(card);
  const statusText = cardStatusLabel(entry.status);
  /* Every line has a fixed height and truncates rather than wraps, so a status
     word, the needs-you marker, a reference, a roll-up or a primary action
     appearing never resizes a card or moves a root (issue #2418 review). A
     cut line's text is in its tooltip. References that do not fit are not
     cut: they are counted by a `+N` that opens the overlay's list of them
     all. A prompt the primary's line cuts is shown whole at the review step
     before it is sent, unless it is the catalog's (`primaryNeedsReview`). */
  const metaTitle = [place.kind, statusText, agentType].filter(Boolean).join(" · ");
  const primaryNotes = primary ? [
    primary.prompt ? exactTextString(primary.prompt) : undefined,
    !primary.enabled ? disabledReasonSentence(primary.disabled_reason) : undefined,
    results?.[primary.token] ? resultSentence(results[primary.token]) : undefined,
    primary.last_delivery ? deliverySentence(primary.last_delivery) : undefined,
  ].filter((note): note is string => note !== undefined) : [];
  return (
    <article
      className={`agent-card tone-${tone}${entry.needs_you ? " needs-you" : ""}${place.faded ? " is-faded" : ""}`}
      data-testid={`agent-card-${domIdentity(placedCardKey(card))}`}
      data-card-key={entry.key}
      data-depth={place.depth}
      data-status={entry.status}
      data-tone={tone}
      data-faded={place.faded ? "true" : undefined}
      aria-label={`${name}${entry.needs_you ? ", needs you" : ""}`}
    >
      <header className="agent-card-header">
        <button
          type="button"
          className="agent-card-name"
          data-testid="agent-card-show"
          aria-label={`Show ${name}'s card`}
          title={`Show ${name}'s card`}
          onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, ref)}
        >{name}</button>
        {entry.needs_you && <span className="agent-card-needs-you" data-testid="agent-card-needs-you" title={needsYou}><BellRing size={11} aria-hidden="true" />Needs you</span>}
        {place.hasChildren && (
          <button
            type="button"
            className="icon-button agent-card-toggle"
            data-testid="agent-card-toggle"
            aria-expanded={place.open}
            aria-label={`${place.open ? "Close" : "Open"} ${name}'s tree`}
            title={place.open ? "Hide the agents under it" : "Show the agents under it"}
            onClick={() => VOICE_ACTIONS[place.open ? "closeCardTree" : "openCardTree"].run(controls, ref)}
          >{place.open ? <ChevronUp size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}</button>
        )}
      </header>
      <div className="agent-card-meta" title={displayTitle(metaTitle)}>
        <span className="agent-card-kind" data-testid="agent-card-kind">{place.kind}</span>
        <span className={`status-label card-status ${cardStatusClass(entry.status)}`} data-testid="agent-card-status" title="Status checked by the deck">{statusText}</span>
        {agentType && <span className="agent-card-agent-type" data-testid="agent-card-agent-type">{displayText(agentType, DISPLAY_LIMITS.name)}</span>}
      </div>
      {place.tree && (refs.length > 0
        ? <CardReferences card={card} refs={refs} name={name} controls={controls} />
        : <div className="agent-card-refs is-empty" aria-hidden="true" />)}
      {place.tree && (rollup ? <CardRollup rollup={rollup} /> : <div className="agent-card-rollup-block is-empty" aria-hidden="true" />)}
      {!primary && <div className="agent-card-primary is-empty" aria-hidden="true" />}
      {primary && (
        <div className="agent-card-primary" title={primaryNotes.length ? displayTitle(primaryNotes.join("\n")) : undefined}>
          <button
            type="button"
            className="button primary compact"
            data-testid="agent-card-primary"
            disabled={!primary.enabled}
            title={primaryTitle}
            onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...ref, cardActionId: primary.id, cardActionToken: primary.token, ...(primaryNeedsReview(primary) ? { cardActionReview: true } : {}) })}
          >{displayText(actionLabel(entry, primary), DISPLAY_LIMITS.name)}</button>
          {primary.source === "agent" && <span data-testid="agent-card-primary-source"><SourceBadge source="agent" /></span>}
          {primary.prompt && <small className="agent-card-prompt" title={exactTextString(primary.prompt)}>{promptPreview(primary.prompt)}</small>}
          {!primary.enabled && <small className="agent-card-disabled">{disabledReasonSentence(primary.disabled_reason)}</small>}
          {results?.[primary.token] && <small className="agent-card-result" role="status" data-testid="agent-card-result">{displayText(resultSentence(results[primary.token]), DISPLAY_LIMITS.detail)}</small>}
          {primary.last_delivery && <small className="agent-card-delivery">{displayText(deliverySentence(primary.last_delivery), DISPLAY_LIMITS.message)}</small>}
        </div>
      )}
    </article>
  );
}
