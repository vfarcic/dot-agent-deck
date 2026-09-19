/**
 * Issue #1143 — the last agent record an open pane's deck gave, kept for
 * exactly as long as that pane is open, so the pane can still be a pane while
 * its deck is not answering.
 *
 * # The gap this closes
 *
 * PRD #1105 decided that an agent pane whose deck has no live link *"replaces
 * its terminal with a sentence rather than closing the pane"*. Only the second
 * half of that was true: the view survived (`paneAgentRetired` is gated on
 * `paneDeckAttachable`), but nothing rendered, because **everything the pane
 * draws is a property of the agent RECORD** — heading, role, status, prompt,
 * tool, the five panel tabs — and a deck with no live link reports no agents at
 * all. Both empty agent lists in the crate are on that path
 * (`disconnected_snapshot`, and `snapshot_with`'s non-connected early return)
 * and `mapDesktopSnapshot` carries none over from the previous snapshot. So the
 * lookup failed, `DeckShell` rendered no pane, and the state built for exactly
 * this case was unreachable.
 *
 * # What is held, and what is deliberately NOT
 *
 * One record, for the `(deckId, agentId)` the open pane names, and nothing
 * else. The overview and the deck grid keep listing what their snapshots list,
 * which for a deck with no live link is nothing — a deck that is not answering
 * cannot vouch for what it was running, and a screen that went on listing its
 * agents would be making that claim on its behalf. The pane is different
 * because the user opened it: it is a thing already on screen, and the question
 * there is not *"what is running"* but *"what happened to the thing I was
 * looking at"*.
 *
 * This is also why the hold lives in the webview rather than in the crate. The
 * crate cannot scope it: it does not know which pane is open — the view is
 * frontend state — so holding it there would mean putting last-known agents
 * back into `disconnected_snapshot`, where every consumer of `snapshot.agents`
 * would pick them up and the overview would start listing them.
 *
 * # The key guard is load-bearing
 *
 * The held record carries the `(deckId, agentId)` it is FOR, and is read back
 * only on an exact match — through {@link agentKey}, whose `NUL` separator is
 * the one byte neither component can contain, so two identities cannot spell
 * one key. Without that fence, closing a pane on a live deck and opening one
 * on an unreachable deck would render the first agent's record under the
 * second agent's view — the composite-identity confusion PRD #1105's security
 * audit is about, arriving by a new route.
 *
 * Comparing during render rather than clearing in an effect is what makes the
 * fence hold on the **first** commit of the new pane, which is the commit an
 * effect runs after. The ref is written during render for the same reason
 * `useShownTerminals` writes its own (`latest.current = targets`): the value
 * has to be current *for this render*, not for the one after it. Both writes
 * are idempotent — re-running this body with the same inputs stores the same
 * record — so a render React discards or repeats costs nothing, and the only
 * field that would differ is `confirmedAt`, which is a real instant at which
 * the deck really had reported that record either way.
 *
 * # How stale may it be — unbounded in time, bounded by the pane
 *
 * There is no expiry timer, and that is a decision rather than an omission.
 * An expiry would make the pane vanish at a moment the app picked and the user
 * cannot see, which is the same failure `paneAgentRetired` already refuses one
 * condition away: it declines to close on a silence, because a silence is not
 * an answer. A thirty-second outage and a thirty-minute one are the same state
 * of knowledge — the app knows nothing in both — so any threshold would be an
 * arbitrary point at which "unknown" is redefined as "gone". What the pane does
 * instead is **say how old the record is** and stop asserting the parts of it
 * that decay; see `AgentTile`'s `data-agent-record` and
 * `unreachableDeckTerminalState`. A timestamp is information; a timeout is a
 * guess.
 *
 * The record is dropped when the pane closes, when it changes identity, and
 * whenever the deck answers again — a live record always wins, because `held`
 * is only ever read where there is no live one.
 */
import { useRef } from "react";
import { agentKey } from "../lib/agentKey";
import type { AgentSession } from "../types";

/** A record that was true at {@link HeldAgentRecord.confirmedAt} and may not be now. */
export interface HeldAgentRecord {
  agent: AgentSession;
  /**
   * When this app last saw its deck report the record, stamped from the
   * WEBVIEW's clock — so unlike `lastActivityMs` and `spawnedAtMs` it is not a
   * daemon-supplied instant and carries no cross-clock skew. That matters at
   * the render seam, which relativises it: the skew policy the app applies to
   * the daemon's instants is satisfied here by construction rather than by
   * tolerance.
   */
  confirmedAt: number;
}

/**
 * Keep the last record `live` carried for `deckId`/`agentId`, and hand it back
 * on the renders where `live` is `undefined`.
 *
 * Returns `undefined` when there is no pane, when the pane names an identity
 * nothing has been held for, and on every render where `live` is present —
 * a caller reads `live ?? held?.agent`, so a present `held` always means the
 * record on screen is the older one.
 *
 * `now` is injectable so a test can drive the age without a fake timer, the
 * same way `displayActivity` and `displayUptime` take theirs.
 */
export function useHeldAgentRecord(
  target: { deckId: string; agentId: string } | undefined,
  live: AgentSession | undefined,
  now: () => number = Date.now,
): HeldAgentRecord | undefined {
  const key = target && agentKey(target.deckId, target.agentId);
  const holder = useRef<{ key: string; record: HeldAgentRecord } | undefined>(undefined);
  if (key === undefined) {
    // No pane: nothing to hold FOR, and holding across a close would be a
    // record waiting to be shown under whatever is opened next.
    holder.current = undefined;
    return undefined;
  }
  if (live !== undefined) {
    /*
      Re-stamped on every render that carries a record rather than only on the
      first, because `mapDesktopSnapshot` builds fresh objects per snapshot: an
      identity comparison here would re-stamp per snapshot anyway, and a deep
      one would claim the record was last confirmed at the moment it last
      CHANGED, which is a different and much older fact. What the pane says is
      when the deck last reported, not when the report last differed.
    */
    holder.current = { key, record: { agent: live, confirmedAt: now() } };
    return undefined;
  }
  // The identity fence: a record held for another pane is not this pane's.
  return holder.current?.key === key ? holder.current.record : undefined;
}
