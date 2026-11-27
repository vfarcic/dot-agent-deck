import { GitMergeIcon, IssueClosedIcon, IssueOpenedIcon, QuestionIcon, type Icon } from "@primer/octicons-react";
import { BellRing, ChevronDown, ChevronUp, X } from "lucide-react";
import { useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from "react";

import { cardStatusClass, cardStatusLabel, disabledReasonSentence, exactTextSegments, hasMarkers, MARKER_CLOSE, MARKER_OPEN, markerTitle, resultSentence, type CardResults, needsYouSentence, placedCardKey, type PlacedCard } from "../lib/agentCards";
import { cardTone, issueLabels, referenceUrl, referenceView, rollupSegments, rollupSentence, type CardKind, type IssueLabelView } from "../lib/agentCardsTree";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { CardActionIcon, cardActionHover, cardIconName, hoverCutsPrompt } from "../lib/cardIcons";
import { endedRoleLine } from "../lib/endedRoles";
import type { AgentCardAction, AgentCardCheckedRef, AgentCardEntry, AgentCardRollup, EndedRole } from "../types";
import { ActionIconGroup, ActionTipButton } from "./ActionTipButton";
import { PullRequestBadge } from "./PullRequestBadge";

/**
 * The members a card's own controls dispatch through. `navigate` (and the
 * optional `revealAgent`) are the dashboard's, so the card's name opens the
 * agent through `openAgent` exactly as a table row does (issue #2418, D12).
 */
export type CardControls = Pick<VoiceActionContext, "showAgentCard" | "runCardAction" | "setCardTree" | "openCardReference" | "navigate"> & Partial<Pick<VoiceActionContext, "reportRefused" | "revealAgent" | "toggleCardSelected">>;

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
const MARKER_NOTE = `${MARKER_OPEN}…${MARKER_CLOSE} marks a character you cannot see; it is sent as is.`;

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
 * its reference line, and the agent pane's Card tab in its list of every
 * reference (`scope: "panel"`, whose test ids carry `-panel`); both open a
 * page through the same action, so the same address checks apply.
 */
export function CardReference({ card, reference, controls, scope = "card", hidden = false }: { card: PlacedCard; reference: AgentCardCheckedRef; controls: ReferenceControls; scope?: "card" | "panel"; hidden?: boolean }) {
  const view = referenceView(reference);
  const url = referenceUrl(reference);
  const id = scope === "panel" ? "agent-card-panel" : "agent-card";
  const stale = view.status === "stale" ? <small className="agent-card-ref-stale">stale</small> : null;
  let body: ReactNode;
  /* Whether `body` is a control that opens the reference's page. */
  let opensPage = false;
  if (view.status === "checked" || view.status === "stale") {
    if (view.kind === "pull_request" && "pullRequest" in view) {
      opensPage = Boolean(url);
      body = <PullRequestBadge pullRequest={view.pullRequest} testId={`${id}-pr`} onOpen={url ? () => VOICE_ACTIONS.openCardPullRequest.run(controls, { ...cardRef(card), referenceUrl: url }) : undefined} />;
    } else if ("number" in view) {
      const StateIcon = ISSUE_STATE_ICON[view.state] ?? QuestionIcon;
      const label = `Issue #${view.number}: ${view.state}`;
      const content = <><StateIcon className="pr-badge-state" size={11} aria-hidden="true" /><span className="pr-badge-number">#{view.number}</span></>;
      opensPage = Boolean(url);
      body = url
        ? <button type="button" className={`pr-badge issue-badge issue-state-${domIdentity(view.state)}`} data-testid={`${id}-issue`} data-issue-state={view.state} aria-label={label} title={`${label} — open it in the app`} onMouseDown={(event) => event.stopPropagation()} onClick={() => VOICE_ACTIONS.openCardIssue.run(controls, { ...cardRef(card), referenceUrl: url })}>{content}</button>
        : <span className={`pr-badge issue-badge issue-state-${domIdentity(view.state)}`} data-testid={`${id}-issue`} data-issue-state={view.state} role="img" aria-label={label}>{content}</span>;
    }
  } else if ("text" in view) {
    body = <small className="agent-card-ref-text">{displayText(view.text, DISPLAY_LIMITS.name)}</small>;
  }
  /* A badge that opens a page does only that: its click does not reach the
     card, which would open the agent's pane too (issue #2418, D12). Text
     saying a reference is still to be checked, or why it cannot be, opens
     nothing, so its click reaches the card and opens the pane like the rest
     of it. */
  if (opensPage) body = <span className="agent-card-control" onClick={(event) => event.stopPropagation()}>{body}</span>;
  return (
    <span className={`agent-card-ref is-${view.status}${hidden ? " is-overflow" : ""}`} data-testid={scope === "panel" ? "agent-card-panel-ref-badge" : "agent-card-ref"} data-field={reference.field} data-kind={reference.kind} data-ref-status={view.status} data-overflow={hidden ? "true" : undefined} aria-hidden={hidden ? true : undefined} title={displayTitle(view.title)}>
      {body}{stale}
    </span>
  );
}

/** The width the `+N` at the end of a one-line row takes, with the gap before it, in CSS pixels (kept in step with `.agent-card-refs-more` and `.agent-card-labels-more`). */
const MORE_WIDTH = 34;
const MORE_GAP = 6;

/**
 * Issue #2418 review (D-R1) — how many of a one-line row's items fit whole,
 * leaving room for a `+N` when not all of them do. Measured after layout, and
 * again when the row's width changes. Every item stays in the row's flow, the
 * hidden ones invisible, so a measurement never moves the ones before it.
 * Where nothing has a width (no layout), every item counts as fitting.
 */
function useFitOnOneLine(line: RefObject<HTMLElement | null>, itemSelector: string, count: number, items: unknown): number {
  const [fit, setFit] = useState(count);
  useLayoutEffect(() => {
    const node = line.current;
    if (!node) return undefined;
    const measure = () => {
      const width = node.clientWidth;
      const ends = [...node.querySelectorAll<HTMLElement>(itemSelector)].map((item) => item.offsetLeft + item.offsetWidth);
      const within = (room: number) => { const first = ends.findIndex((end) => end > room + 0.5); return first < 0 ? ends.length : first; };
      const whole = within(width);
      setFit(whole === ends.length ? whole : within(width - MORE_WIDTH - MORE_GAP));
    };
    measure();
    if (typeof ResizeObserver !== "function") return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [line, itemSelector, items]);
  return Math.min(fit, count);
}

/**
 * Issue #2418 review (D-R1) — a card's reference line: as many reference
 * badges as fit whole on its one line, in the daemon's order, and a `+N`
 * control counting the rest, which opens the agent's pane with its Card
 * panel at its list of
 * every reference. The line has a fixed height, so references arriving never
 * move a root; a badge that would be cut is hidden whole (not drawn, not
 * focusable) rather than cut, and the count says it is there.
 */
function CardReferences({ card, refs, name, controls }: { card: PlacedCard; refs: AgentCardCheckedRef[]; name: string; controls: CardControls }) {
  const line = useRef<HTMLDivElement>(null);
  const shown = useFitOnOneLine(line, ":scope > .agent-card-ref", refs.length, refs);
  const more = refs.length - shown;
  const label = `Show all ${refs.length} pull requests and issues on ${name}'s card`;
  return (
    <div ref={line} className="agent-card-refs" aria-label="Pull requests and issues the agent reported, as the deck checked them" data-shown={shown}>
      {refs.map((reference, index) => <CardReference key={`${reference.field}\u0000${index}`} card={card} reference={reference} controls={controls} hidden={index >= shown} />)}
      {more > 0 && (
        <span className="agent-card-control" onClick={(event) => event.stopPropagation()}>
          <button
            type="button"
            className="agent-card-refs-more"
            data-testid="agent-card-refs-more"
            style={{ width: MORE_WIDTH }}
            aria-label={`${more} more: ${label}`}
            title={`${more} more that do not fit here. ${label}`}
            onMouseDown={(event) => event.stopPropagation()}
            onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, { ...cardRef(card), opensOn: "references" })}
          >+{more}</button>
        </span>
      )}
    </div>
  );
}

/**
 * Issue #2418 (D13) — one issue label as a chip: its name, and a dot in the
 * repository's colour, or a neutral dot when the label has no colour the app
 * can draw. The name keeps the theme's text colour, so it reads the same in
 * light and dark. Display only: a label is not a control.
 */
export function IssueLabelChip({ label, testId, hidden = false }: { label: IssueLabelView; testId: string; hidden?: boolean }) {
  return (
    <span className={`agent-card-label${hidden ? " is-overflow" : ""}`} data-testid={testId} data-label-color={label.color} aria-hidden={hidden ? true : undefined} title={displayTitle(label.name)}>
      <i className="agent-card-label-dot" aria-hidden="true" style={label.color ? { backgroundColor: `#${label.color}` } : undefined} />
      {displayText(label.name, DISPLAY_LIMITS.name)}
    </span>
  );
}

/**
 * Issue #2418 (D13) — the labels of the first issue on a card that has any,
 * on one fixed-height line under the references: as many chips as fit whole,
 * and a `+N` counting the rest. The `+N` is text, not a control: like the rest
 * of the card, clicking it opens the agent's pane, whose Card tab lists
 * every label.
 */
function CardLabels({ labels, issue }: { labels: IssueLabelView[]; issue: string }) {
  const line = useRef<HTMLDivElement>(null);
  const shown = useFitOnOneLine(line, ":scope > .agent-card-label", labels.length, labels);
  const more = labels.length - shown;
  const hiddenNames = labels.slice(shown).map((label) => label.name).join(", ");
  return (
    <div ref={line} className="agent-card-labels" data-testid="agent-card-issue-labels" aria-label={`Labels of issue ${issue}`} data-shown={shown}>
      {labels.map((label, index) => <IssueLabelChip key={`${label.name}\u0000${index}`} label={label} testId="agent-card-issue-label" hidden={index >= shown} />)}
      {more > 0 && <span className="agent-card-labels-more" data-testid="agent-card-issue-labels-more" style={{ width: MORE_WIDTH }} title={displayTitle(`${more} more: ${hiddenNames}. Open the card to see every label.`)}>+{more}</span>}
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

/**
 * PRD #2374 decision 2026-11-24 — the most action icons a card draws in its
 * bottom row; past it, one `+N` control opens the agent's pane, whose Card
 * panel lists
 * every action. Three, with `+N`, leave room for the roll-up beside them on a
 * 224-pixel card.
 */
export const CARD_ICON_LIMIT = 3;

/**
 * The actions a card draws as icons: every prompt action its agent offers now,
 * in the daemon's order — never a reply (or any action that takes the user's
 * own words), Open, an answer to the agent's own question, a close, which
 * keeps its own control, or an action whose catalog `placement` is `detail`,
 * which is offered only in the agent pane's Card tab. They are the panel's
 * actions, pressed the same way.
 */
function cardIconActions(entry: AgentCardEntry): AgentCardAction[] {
  const options = new Set(entry.questions.flatMap((question) => question.options.map((option) => option.action_id)));
  return entry.actions.filter((action) => action.kind === "prompt" && !action.text_input && !options.has(action.id) && action.placement !== "detail");
}

/**
 * PR #2491 decision 2026-11-25 — the actions the agent pane's Card tab
 * draws as icons: every prompt action a card's icons would draw, and also the
 * ones whose catalog `placement` is `detail`, which are offered only there.
 */
export function panelIconActions(entry: AgentCardEntry): AgentCardAction[] {
  const options = new Set(entry.questions.flatMap((question) => question.options.map((option) => option.action_id)));
  return entry.actions.filter((action) => action.kind === "prompt" && !action.text_input && !options.has(action.id));
}

/**
 * The grouped row of action icons a card draws in its bottom row and the
 * agent pane's Card tab draws whole: each icon's hover gives the action's
 * label and exact text, and a press takes the same path from both — sent, or,
 * when it asks first, its text is the agent's, or its hover could not show
 * the text whole, the single-action review opens instead (audit 1).
 */
export function CardActionIconRow({ card, actions, controls, label, children }: { card: PlacedCard; actions: readonly AgentCardAction[]; controls: Pick<VoiceActionContext, "runCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>; label: string; children?: ReactNode }) {
  const ref = cardRef(card);
  return (
    <ActionIconGroup className="agent-card-action-icons" label={label}>
      {actions.map((action) => (
        <ActionTipButton
          key={action.id}
          className="icon-button agent-card-action-icon"
          data-testid="agent-card-action-icon"
          data-action={action.id}
          data-icon={cardIconName(action.icon)}
          aria-label={displayText(action.label, DISPLAY_LIMITS.name)}
          tip={actionHover(action)}
          unavailable={!action.enabled}
          onClick={(_, shown) => VOICE_ACTIONS.runCardAction.run(controls, { ...ref, cardActionId: action.id, cardActionToken: action.token, ...(hoverCutsPrompt(action.prompt) || !shown.whole ? { cardActionReview: true } : {}) })}
        ><CardActionIcon name={action.icon} /></ActionTipButton>
      ))}
      {children}
    </ActionIconGroup>
  );
}

/** A card action's hover: its label, the exact text, whether it asks first, why it is disabled. */
function actionHover(action: AgentCardAction): string {
  return cardActionHover(action.label, action.prompt, [
    action.source === "agent" && "Suggested by the agent, not in the catalog",
    action.confirm && action.enabled && "Asks you to confirm first.",
    !action.enabled && `Disabled: ${disabledReasonSentence(action.disabled_reason)}`,
  ]);
}

/**
 * Issue #2399, decision 2026-11-24 — a batch button's hover: the action's
 * label, the exact text when every agent it can reach offers the same one (or
 * that the text differs per agent), and who it is for. The confirmation shows
 * what each agent gets before anything is sent.
 */
export function batchChoiceHover(choice: { label: string; text?: string; differs?: boolean }, forWhom: string): string {
  return cardActionHover(choice.label, choice.text, [
    choice.differs && "The text differs per agent: the confirmation shows what each one gets.",
    `Review it for ${forWhom} before anything is sent.`,
  ]);
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
  /** Issue #2399 — whether this card is selected, when its daemon does batch actions; absent otherwise, and the card has no checkbox. */
  selected?: boolean;
}

/**
 * Issue #2418 — one agent's compact card on the Cards tree: who it is (name
 * and kind), its status in the daemon's words, whether it needs the user, the
 * pull requests and issues its agent reported as the deck checked them, the
 * first checked issue's labels, what is under it (closed, with its roll-up),
 * an icon for each prompt action its agent offers (up to
 * {@link CARD_ICON_LIMIT}, then a `+N`) in its bottom row, beside the
 * roll-up, and Close. Everything comes from the daemon's entry as received;
 * nothing here decides status, needs-you, roll-ups or references.
 *
 * PR #2491 decision 2026-11-25: a click on the card, its name or anywhere
 * else, opens the agent's pane on Terminal, whose Card tab holds what the
 * agent said, its agent type and every action. Each other control — a badge,
 * the `+N`, the tree toggle, the checkbox, an action icon and Close — does
 * only its own thing: it sits in a wrapper that keeps its click from reaching
 * the card. The status line is a button with no handler of its own, so the
 * keyboard reaches the pane too: its click is the card's. No control is
 * nested in another.
 *
 * Issue #1965: an exited orchestration role's card says why on its status
 * line, after the daemon's status word, in the words its table row and the
 * TUI's card use: `Exited: <reason>` and its last output. The line truncates
 * like every other, so a report arriving never resizes the card; the Card tab
 * shows the reason and the last output in full.
 */
export function AgentCard({ card, controls, results, place, ended }: {
  card: PlacedCard;
  controls: CardControls;
  results?: CardResults;
  place: CardPlace;
  /** Issue #1965 — the exited orchestration role this card is for, when its agent exited on its own. */
  ended?: EndedRole;
}) {
  const { entry } = card;
  const name = displayIdentity(entry.name, DISPLAY_LIMITS.name, entry.pane_id);
  const needsYou = needsYouSentence(entry);
  const rollup = place.tree && entry.rollup ? entry.rollup : undefined;
  const tone = cardTone({ status: entry.status, rollup }, place.open);
  const refs = place.tree ? entry.checked_refs ?? [] : [];
  const labelled = refs.map((reference) => ({ reference, labels: issueLabels(reference) })).find(({ labels }) => labels.length > 0);
  const ref = cardRef(card);
  const statusText = cardStatusLabel(entry.status);
  /* Close is the card's own Close action, the same token and the same
     confirmation (D12); it is drawn only when the daemon offers one. */
  const close = entry.actions.find((action) => action.kind === "close");
  const closeResult = close ? results?.[close.token] : undefined;
  const closeTitle = close
    ? [`Close ${name}`, !close.enabled ? `disabled: ${disabledReasonSentence(close.disabled_reason)}` : undefined, closeResult ? resultSentence(closeResult) : undefined].filter(Boolean).join(" — ")
    : undefined;
  /* Every line has a fixed height and truncates rather than wraps, so a status
     word, the needs-you marker, a reference, a label or a roll-up appearing
     never resizes a card or moves a root (issue #2418 review). A cut line's
     text is in its tooltip. References and labels that do not fit are not
     cut: a `+N` counts them, and the Card tab lists them all. */
  /* The prompt actions the agent offers now, as icons in the card's bottom
     row (PRD #2374 decision 2026-11-24, placed there by issue #2399 review 2
     so the name and the status keep their lines): each is pressed exactly as
     its Card tab icon is, so one that asks first, or whose text the agent
     wrote, opens the single-action review instead of sending — and so does
     one whose text is too long for its hover to quote whole (audit 1). */
  const iconActions = cardIconActions(entry);
  const icons = iconActions.slice(0, CARD_ICON_LIMIT);
  const moreActions = iconActions.length - icons.length;
  const endedText = ended ? endedRoleLine(ended) : undefined;
  const metaTitle = `${place.kind} · ${statusText}${endedText ? ` · ${endedText}` : ""} — show ${name}'s card`;
  return (
    <article
      className={`agent-card tone-${tone}${entry.needs_you ? " needs-you" : ""}${place.faded ? " is-faded" : ""}${place.selected ? " is-selected" : ""}`}
      data-testid={`agent-card-${domIdentity(placedCardKey(card))}`}
      data-card-key={entry.key}
      data-depth={place.depth}
      data-status={entry.status}
      data-tone={tone}
      data-faded={place.faded ? "true" : undefined}
      aria-label={`${name}${entry.needs_you ? ", needs you" : ""}`}
      onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, { ...ref, opensOn: "terminal" })}
    >
      <header className="agent-card-header">
        {place.selected !== undefined && controls.toggleCardSelected && (
          <span className="agent-card-control agent-card-select" onClick={(event) => event.stopPropagation()}>
            <input
              type="checkbox"
              data-testid="agent-card-select"
              aria-label={`Select ${name}`}
              title={place.hasChildren && !place.open ? `Select ${name} and every card under it` : `Select ${name}`}
              checked={place.selected}
              onChange={() => VOICE_ACTIONS.toggleCardSelected.run(controls as Pick<VoiceActionContext, "toggleCardSelected">, ref)}
            />
          </span>
        )}
        <span className="agent-card-control" onClick={(event) => event.stopPropagation()}>
          <button
            type="button"
            className="agent-card-name"
            data-testid="agent-card-open-agent"
            aria-label={`Open ${name}`}
            title={`Open ${name}`}
            onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, { ...ref, opensOn: "terminal" })}
          >{name}</button>
        </span>
        {/* Issue #2399 re-review N6, PR #2491 review: Needs you is its bell alone on every card, so the name keeps its room beside the checkbox; its words stay its tooltip and what a screen reader reads. */}
        {entry.needs_you && <span className="agent-card-needs-you is-compact" data-testid="agent-card-needs-you" title={needsYou}><BellRing size={11} aria-hidden="true" /><span className="overview-sr-only">Needs you</span></span>}
        {place.hasChildren && (
          <span className="agent-card-control" onClick={(event) => event.stopPropagation()}>
            <button
              type="button"
              className="icon-button agent-card-toggle"
              data-testid="agent-card-toggle"
              aria-expanded={place.open}
              aria-label={`${place.open ? "Close" : "Open"} ${name}'s tree`}
              title={place.open ? "Hide the agents under it" : "Show the agents under it"}
              onClick={() => VOICE_ACTIONS[place.open ? "closeCardTree" : "openCardTree"].run(controls, ref)}
            >{place.open ? <ChevronUp size={14} aria-hidden="true" /> : <ChevronDown size={14} aria-hidden="true" />}</button>
          </span>
        )}
        {close && (
          <span className="agent-card-control" onClick={(event) => event.stopPropagation()}>
            <button
              type="button"
              className="icon-button agent-card-close"
              data-testid="agent-card-close-agent"
              aria-label={`Close ${name}`}
              title={displayTitle(closeTitle ?? "")}
              disabled={!close.enabled}
              onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...ref, cardActionId: close.id, cardActionToken: close.token })}
            ><X size={14} aria-hidden="true" /></button>
          </span>
        )}
      </header>
      <div className="agent-card-meta-row">
        {/* No handler of its own: its click (a press of Enter or Space included) is the card's, which opens the agent's pane on its Terminal tab.
            Its name starts with the kind and status it shows, so a screen reader announces them and a spoken label of what is visible matches it (round 2 review). */}
        <button type="button" className="agent-card-meta" data-testid="agent-card-details" aria-label={`${place.kind} ${statusText}${endedText ? `, ${endedText}` : ""}: open ${name}`} title={displayTitle(metaTitle)}>
          <span className="agent-card-kind" data-testid="agent-card-kind">{place.kind}</span>
          <span className={`status-label card-status ${cardStatusClass(entry.status)}`} data-testid="agent-card-status">{statusText}</span>
          {ended && endedText && <span className="agent-card-ended" data-testid="agent-card-ended" title={displayTitle(ended.headline)}>{endedText}</span>}
        </button>
      </div>
      {place.tree && (refs.length > 0
        ? <CardReferences card={card} refs={refs} name={name} controls={controls} />
        : <div className="agent-card-refs is-empty" aria-hidden="true" />)}
      {place.tree && (labelled
        ? <CardLabels labels={labelled.labels} issue={labelled.reference.number !== undefined ? `#${labelled.reference.number}` : labelled.reference.claim} />
        : <div className="agent-card-labels is-empty" aria-hidden="true" />)}
      {/* Issue #2399 review 2 — the bottom row: the roll-up where the card has one, and the action icons at its right end. Drawn on every card, empty or not, so a report never resizes one. */}
      <div className="agent-card-foot" data-testid="agent-card-foot">
        {place.tree && (rollup ? <CardRollup rollup={rollup} /> : <div className="agent-card-rollup-block is-empty" aria-hidden="true" />)}
        {icons.length > 0 && (
          <span className="agent-card-control agent-card-foot-actions" onClick={(event) => event.stopPropagation()}>
            <CardActionIconRow card={card} actions={icons} controls={controls} label={`${name}'s actions`}>
              {moreActions > 0 && (
                <button
                  type="button"
                  className="agent-card-actions-more"
                  data-testid="agent-card-actions-more"
                  aria-label={`${moreActions} more actions: show ${name}'s card`}
                  title={displayTitle(`${moreActions} more ${moreActions === 1 ? "action" : "actions"}: ${iconActions.slice(CARD_ICON_LIMIT).map((action) => action.label).join(", ")}. Opens the agent's Card tab, where every action is.`)}
                  onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, { ...ref, opensOn: "actions" })}
                >+{moreActions}</button>
              )}
            </CardActionIconRow>
          </span>
        )}
      </div>
    </article>
  );
}
