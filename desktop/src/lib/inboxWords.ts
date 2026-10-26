import type { AgentInbox, AgentSession, InboxActionVerb, InboxAttentionItem } from "../types";

/**
 * The words both clients use for a message's journey through an agent's inbox
 * (PRD #1666, CLAUDE.md rule 22). The TUI prints the same strings from
 * `src/inbox_display.rs`, and `tests/fixtures/inbox-display.json` pins the two
 * to each other: change a word here and there together.
 */
export const INBOX_WORDS = {
  /** Prefixes the card's pending count (`✉2`) and a deck message's prompt line. */
  glyph: "✉",
  /** Messages waiting for the agent, taken or not. */
  queued: "queued",
  /** The agent's side acknowledged the message: its content reached the agent. */
  delivered: "delivered",
  /** The message could not reach the agent. */
  notDelivered: "not delivered",
  /** The worker reported back on a delegated task. */
  workDone: "work-done",
  /** Messages have waited longer than a wake-up takes while the agent was not working. */
  wakeStalled: "waiting to wake",
  /** What the user can do about `wakeStalled`. */
  wakeStalledHint: "send it a prompt",
  /**
   * PRD #1666 part 3, round 5-wake: why the deck holds the doorbell, after the
   * glyph and the pending count, by the daemon's wire value.
   */
  wakeHolds: {
    possible_draft: "waiting: there may be an unsent draft. Send it, or type x then Ctrl+C.",
    awaiting_prompt: "not yet taken: the agent may be waiting at a prompt. Check its pane.",
    paste_open: "waiting: a paste into its input has not finished.",
    cannot_type: "waiting for the agent to fetch it. Send it a prompt.",
    not_taken: "waiting: the agent did not take the deck's Enter. Send it a prompt.",
    uncertain_input: "waiting: the deck's notice may be in its input. Check it before Enter.",
    dialog_during_ring: "waiting: a dialog may have taken the deck's keys. Check its pane.",
  },
  /**
   * The two holds that warn even with nothing waiting, in the words they
   * take then.
   */
  wakeHoldsAlone: {
    uncertain_input: "The deck's notice may be in its input. Check it before pressing Enter.",
    dialog_during_ring: "A dialog may have taken the deck's keys. Check the agent's pane.",
  } as Partial<Record<string, string>>,
  uncertain: "delivery uncertain",
  uncertainMany: "deliveries uncertain",
  uncertainReason: "a deck message may or may not have reached this agent. Retry sends it again marked as a possible repeat; Cancel stops delivering it.",
  outOfAttempts: "out of attempts: the deck sent this message as many times as it allows. Cancel it, or post it again.",
  heldLong: "the agent has had a deck message for an hour without confirming it. It may still be working on it.",
  notStarted: "may not have reached the agent",
  notStartedReason: "the agent has been idle for a few minutes and the deck has not seen it start on this deck message, so the message may not have reached it. Retry sends it again, so the agent may get it twice. Cancel stops further delivery but cannot take back a copy the agent already has.",
  retry: "Retry",
  cancel: "Cancel",
  retryLimit: "retry limit reached; Cancel it, or post it again",
  actionStale: "another client already acted on this message; the card shows where it stands now",
  actionSettled: "it reached the agent after all",
  actionGone: "this message no longer needs a decision",
  retryApplied: "sent again, marked as a possible repeat",
  cancelApplied: "delivery cancelled",
  actionUnsupported: "this deck cannot Retry or Cancel deck messages: upgrade dot-agent-deck",
  attentionTitle: "Deck messages",
  /**
   * PRD #1666 part 3, round 5-wake (Q6): the header's attention item counts
   * the held recipients other than the orchestrator it names (`· 1 other
   * held`), or all of them when it names none (`✉ 2 held`).
   */
  attentionOtherHeld: "other held",
  /** `attentionOtherHeld`'s count when no orchestrator is named. */
  attentionHeld: "held",
  /**
   * PRD #1666 part 3, round 5-wake: what ended an agent's pane before it took
   * a message, by the daemon's `termination.kind`; `{code}` is the exit code.
   * They name the command the deck started in the pane, never the agent.
   */
  terminations: {
    pane_command_exited: "the command started in the agent's pane exited (code {code}) before taking this message",
    connection_ended: "the deck's connection to the agent's pane ended before it took this message; whether the agent is still running is unknown",
    replaced: "the agent's pane was replaced before it took this message; the message stays with the previous agent",
    closed: "the pane was closed before the agent took this message",
    unknown: "the agent's pane ended before it took this message",
  },
  adapterTooOld: "this agent's deck integration is out of date; restart the agent to update it",
  cliTooOld: "the dot-agent-deck command this agent runs does not support this deck's integration: upgrade dot-agent-deck, then restart the agent",
  notArmed: "this agent's deck integration has not connected yet; its messages wait until it does",
  refreshFailed: "the deck could not update this agent's deck integration: run dot-agent-deck hooks install --agent opencode to see why, then restart the agent",
  refreshFailedPi: "the deck could not update this agent's Pi extension: run dot-agent-deck orchestrator setup with the same HOME and PI_CODING_AGENT_DIR as the agent to see why, then restart the agent",
  /** The agent's deck hooks run an older dot-agent-deck; followed by the command that fixes them. */
  hooksTooOld: "this agent's deck hooks run a dot-agent-deck too old for its messages",
  /** `hooksTooOld`'s fix when the deck names no command. */
  hooksTooOldUpgrade: "upgrade the dot-agent-deck the hooks run, or run hooks install from the copy you want them to use, then restart the agent",
  receiptsDegraded: "messages may repeat: the deck cannot record message receipts for this agent",
  evidencePaused: "deck messages paused: {n} delivered messages still wait for proof that older copies cannot run; restart the agent to clear them",
  /** PRD #1666 part 3 (R-2): the deck refused the agent's own request for its waiting messages. */
  admissionRefused: "Messages can't reach this agent",
  /** `admissionRefused`'s remedy, after the cause. */
  admissionRefusedRemedy: "Restart it from the deck; messages addressed to it are not handed to the restarted agent, so have them sent again.",
  /** `admissionRefused`'s cause, by the refusal code; an unknown code names none. */
  admissionCauses: {
    inbox_other_user: "a process running as another user asked for them",
    inbox_peer_unidentified: "the deck could not identify the process that asked for them",
    inbox_component_not_caller: "its integration named a process other than its own",
    inbox_nested_agent: "an agent of the same kind running inside it asked for them",
    inbox_not_agent_process: "a process outside the agent asked for them",
    inbox_receiver_above_agent: "its integration named a process the agent itself runs under",
  },
} as const;

/**
 * PRD #1666 part 3 (R-2): the card's notice for a refusal with `code`, the
 * row the TUI's card and Deck messages picker print too.
 */
export function admissionRefusedText(code: string): string {
  const causes: Record<string, string> = INBOX_WORDS.admissionCauses;
  const cause = Object.prototype.hasOwnProperty.call(causes, code) ? causes[code] : undefined;
  return cause
    ? `${INBOX_WORDS.admissionRefused}: its connection to the deck was refused (${cause}). ${INBOX_WORDS.admissionRefusedRemedy}`
    : `${INBOX_WORDS.admissionRefused}: its connection to the deck was refused. ${INBOX_WORDS.admissionRefusedRemedy}`;
}

const countOf = (inbox: AgentInbox, kinds: ReadonlyArray<InboxAttentionItem["kind"]>) =>
  (inbox.attention ?? []).filter((item) => kinds.includes(item.kind)).length;
/** Uncertain deliveries, out of attempts included. */
const uncertainCount = (inbox: AgentInbox) => countOf(inbox, ["uncertain", "out_of_attempts"]);
/** PRD #1666 §7: uncertain deliveries out of attempts, whose rows offer Cancel alone. */
const outOfAttemptsCount = (inbox: AgentInbox) => countOf(inbox, ["out_of_attempts"]);
/** PRD #1666 G3: messages handed to an agent that has started no work since. */
const notStartedCount = (inbox: AgentInbox) => countOf(inbox, ["not_started"]);
const heldLongCount = (inbox: AgentInbox) => countOf(inbox, ["held_long"]);

/**
 * PRD #1666 part 3, round 5-wake: why the deck holds the doorbell, with the
 * pending count, while messages wait (`✉2 waiting: there may be an unsent
 * draft. Send it, or type x then Ctrl+C.`). With none pending, only the two
 * warnings that stand on their own (the deck's line may be in the input, a
 * dialog may have taken its keys); nothing otherwise, or for a hold this
 * build does not know. The TUI's card prints the same row.
 */
function inboxHoldText(inbox: AgentInbox): string | undefined {
  const pending = inbox.queued + inbox.fetched;
  if (inbox.wakeHold === undefined || inbox.wakeHold === "unknown") return undefined;
  if (pending <= 0) return INBOX_WORDS.wakeHoldsAlone[inbox.wakeHold];
  return `${INBOX_WORDS.glyph}${pending} ${INBOX_WORDS.wakeHolds[inbox.wakeHold]}`;
}

/**
 * PRD #1666 part 3, round 5-wake (Q6): the words the header's attention item
 * gives an agent's hold, without the count: the hold's words while messages
 * wait (`wakeStalled`'s for a hold this build does not know), the two warnings
 * that stand alone with nothing waiting, and nothing when the deck holds
 * nothing for it. The TUI's `CardInbox::attention_words` is the same.
 */
export function inboxAttentionWords(inbox: AgentInbox | undefined): string | undefined {
  if (inbox?.wakeHold === undefined) return undefined;
  const pending = inbox.queued + inbox.fetched;
  if (pending <= 0) return inbox.wakeHold === "unknown" ? undefined : INBOX_WORDS.wakeHoldsAlone[inbox.wakeHold];
  return inbox.wakeHold === "unknown"
    ? `${INBOX_WORDS.wakeStalled}: ${INBOX_WORDS.wakeStalledHint}`
    : INBOX_WORDS.wakeHolds[inbox.wakeHold];
}

/**
 * PRD #1666 part 3, round 5-wake (Q6): the deck-wide attention item both
 * clients show in their header: `✉ <orchestrator>: <its hold> · 1 other held`,
 * or `✉ 2 held` with no orchestrator held, or nothing. The first held agent of
 * an orchestration's start role is named, whatever the role is called; every
 * other held agent is counted. The TUI's `deck_attention_text` is the same.
 */
export function deckAttentionText(agents: readonly AgentSession[]): string | undefined {
  const held = agents.flatMap((agent) => {
    const words = inboxAttentionWords(agent.inbox);
    return words === undefined ? [] : [{ agent, words }];
  });
  const named = held.find((entry) => entry.agent.isStartRole);
  const others = held.length - (named ? 1 : 0);
  if (named) {
    const head = `${INBOX_WORDS.glyph} ${named.agent.role}: ${named.words}`;
    return others > 0 ? `${head} · ${others} ${INBOX_WORDS.attentionOtherHeld}` : head;
  }
  return others > 0 ? `${INBOX_WORDS.glyph} ${others} ${INBOX_WORDS.attentionHeld}` : undefined;
}

/** Whether the notice row is the hold's, so the badge keeps only the count. */
function holdInNotice(inbox: AgentInbox): boolean {
  return inboxHoldText(inbox) !== undefined
    && inbox.admissionRefused === undefined
    && inbox.integration === undefined
    && inbox.evidencePaused === undefined
    && uncertainCount(inbox) === 0
    && notStartedCount(inbox) === 0
    && heldLongCount(inbox) === 0;
}

/**
 * The card's inbox line (PRD #1666 §2.8): `✉ delivery uncertain` while
 * deliveries are uncertain, then `✉ may not have reached the agent` (G3), then
 * `✉2` alone while the notice row says why the deck holds the doorbell
 * (`inboxHoldText`), `✉2 waiting to wake: send it a prompt` while the deck
 * reports nothing has woken the agent for them (and for a hold this build does
 * not know), `✉2 queued` while messages are queued or taken but not
 * acknowledged, `✉ delivered` once the agent has acknowledged one and nothing
 * is pending, and nothing for an inbox that has held neither. The TUI's card
 * prints the same line.
 */
export function inboxBadge(inbox: AgentInbox | undefined): string | undefined {
  if (!inbox) return undefined;
  const uncertain = uncertainCount(inbox);
  if (uncertain === 1) return `${INBOX_WORDS.glyph} ${INBOX_WORDS.uncertain}`;
  if (uncertain > 1) return `${INBOX_WORDS.glyph} ${uncertain} ${INBOX_WORDS.uncertainMany}`;
  const notStarted = notStartedCount(inbox);
  if (notStarted === 1) return `${INBOX_WORDS.glyph} ${INBOX_WORDS.notStarted}`;
  if (notStarted > 1) return `${INBOX_WORDS.glyph} ${notStarted} ${INBOX_WORDS.notStarted}`;
  const pending = inbox.queued + inbox.fetched;
  if (holdInNotice(inbox) && pending > 0) return `${INBOX_WORDS.glyph}${pending}`;
  if (pending > 0 && (inbox.wakeStalled || inbox.wakeHold === "unknown")) {
    return `${INBOX_WORDS.glyph}${pending} ${INBOX_WORDS.wakeStalled}: ${INBOX_WORDS.wakeStalledHint}`;
  }
  if (pending > 0) return `${INBOX_WORDS.glyph}${pending} ${INBOX_WORDS.queued}`;
  return inbox.lastAckedAt ? `${INBOX_WORDS.glyph} ${INBOX_WORDS.delivered}` : undefined;
}

/**
 * PRD #1666 §6.5: the card's inbox notice row, when the deck has something to
 * say beyond the badge — the first that holds of: a refused request of the
 * agent's own for its messages (part 3, R-2), why the integration takes
 * nothing yet, paused hand-outs, what an uncertain delivery means (what running
 * out of attempts means, when every uncertain one has: its row offers no
 * Retry), what a message the agent may not have received means, held_long
 * attention, why the deck holds the doorbell (part 3, round 5-wake), failed
 * receipt storage. The TUI's card prints the same row.
 */
export function inboxNotice(inbox: AgentInbox | undefined): string | undefined {
  if (!inbox) return undefined;
  // PRD #1666 part 3 (R-2): nothing else reaches the agent until this is fixed.
  if (inbox.admissionRefused !== undefined) return admissionRefusedText(inbox.admissionRefused);
  if (inbox.integration === "cli_too_old") return INBOX_WORDS.cliTooOld;
  if (inbox.integration === "adapter_too_old") return INBOX_WORDS.adapterTooOld;
  if (inbox.integration === "not_armed") return INBOX_WORDS.notArmed;
  if (inbox.integration === "refresh_failed") return INBOX_WORDS.refreshFailed;
  if (inbox.integration === "refresh_failed_pi") return INBOX_WORDS.refreshFailedPi;
  if (inbox.integration === "hooks_too_old") {
    return inbox.integrationCommand
      ? `${INBOX_WORDS.hooksTooOld}: run ${inbox.integrationCommand}, then restart the agent`
      : `${INBOX_WORDS.hooksTooOld}: ${INBOX_WORDS.hooksTooOldUpgrade}`;
  }
  if (inbox.evidencePaused !== undefined) return INBOX_WORDS.evidencePaused.replace("{n}", String(inbox.evidencePaused));
  if (uncertainCount(inbox) > outOfAttemptsCount(inbox)) return INBOX_WORDS.uncertainReason;
  if (outOfAttemptsCount(inbox) > 0) return INBOX_WORDS.outOfAttempts;
  if (notStartedCount(inbox) > 0) return INBOX_WORDS.notStartedReason;
  if (heldLongCount(inbox) > 0) return INBOX_WORDS.heldLong;
  const hold = inboxHoldText(inbox);
  if (hold !== undefined) return hold;
  if (inbox.receiptsDegraded !== undefined) return `${INBOX_WORDS.receiptsDegraded} (${inbox.receiptsDegraded})`;
  return undefined;
}

/**
 * PRD #1666 part 3, round 5-wake: the words for a notice's `termination`, what
 * ended the agent's pane before it took the message; a kind this build does
 * not know, or none at all, reads as the pane having ended. The TUI's
 * `PaneTermination::words` is the same.
 */
export function terminationText(termination: unknown): string {
  const kind = typeof termination === "object" && termination !== null ? (termination as { kind?: unknown }).kind : undefined;
  const code = typeof termination === "object" && termination !== null ? (termination as { code?: unknown }).code : undefined;
  switch (kind) {
    case "pane_command_exited":
      return typeof code === "number" && Number.isInteger(code) && code >= 0
        ? INBOX_WORDS.terminations.pane_command_exited.replace("{code}", String(code))
        : INBOX_WORDS.terminations.unknown;
    case "connection_ended": return INBOX_WORDS.terminations.connection_ended;
    case "replaced": return INBOX_WORDS.terminations.replaced;
    case "closed": return INBOX_WORDS.terminations.closed;
    default: return INBOX_WORDS.terminations.unknown;
  }
}

/** PRD #1666 §6.4: what a listed item's row explains. */
export function attentionReason(kind: InboxAttentionItem["kind"]): string {
  switch (kind) {
    case "uncertain": return INBOX_WORDS.uncertainReason;
    case "out_of_attempts": return INBOX_WORDS.outOfAttempts;
    case "held_long": return INBOX_WORDS.heldLong;
    case "not_started": return INBOX_WORDS.notStartedReason;
  }
}

/** PRD #1666 §2: the row's words for the daemon's answer to Retry or Cancel. */
export function inboxActionText(verb: InboxActionVerb, outcome: string): string {
  switch (outcome) {
    case "applied": return verb === "retry" ? INBOX_WORDS.retryApplied : INBOX_WORDS.cancelApplied;
    case "stale": return INBOX_WORDS.actionStale;
    case "already_settled": return INBOX_WORDS.actionSettled;
    case "retry_limit": return INBOX_WORDS.retryLimit;
    case "unsupported": return INBOX_WORDS.actionUnsupported;
    default: return INBOX_WORDS.actionGone;
  }
}
