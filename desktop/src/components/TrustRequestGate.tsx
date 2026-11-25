import { ShieldAlert } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

import { APPROVAL_PEER_PROPS, useOverlayStackEmpty } from "../hooks/useInertBackground";
import { approvalsWaitingLabel, REVIEW_WAITING } from "../lib/projectTrust";
import { publishHeldTrustRequest, useTrustRequests, type TrustRequestNotice, type TrustRequestRuntime } from "../lib/trustRequests";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { DeckFleet } from "../types";
import { ProjectTrustDialog } from "./ProjectTrustDialog";

/**
 * Issue #2330 — the review a deck asked for, shown over whichever screen is
 * up: one request at a time, from any deck the app observes, once `ready`
 * (the shell's list of what it waits behind, the star reminder's) and no
 * overlay is open at all, read from the overlay stack so whatever the user is
 * in is never covered. Once up it stays up until answered, since it is then on
 * the stack itself.
 *
 * Issue #2511 — a review held back that way is not silent. While a request is
 * waiting and something holds its review back (an agent's pane, any dialog or
 * sheet, the connection banner), a marker over whatever is open says how many
 * approvals are waiting, and clicking it (or voice's "show the approval")
 * opens the review over what is open. Without that, the review opens by itself
 * once nothing is open, as before. The marker is a peer of the open overlay
 * ({@link APPROVAL_PEER_PROPS}) rather than background, so it stays clickable
 * over the agent's pane.
 *
 * Its own component, like `StarPromptGate`, so that only it re-renders as
 * overlays come and go.
 */
export function TrustRequestGate({ runtime, fleet, ready, summonable = true, onNotice }: { runtime: TrustRequestRuntime; fleet: DeckFleet; ready: boolean; summonable?: boolean; onNotice: (notice: TrustRequestNotice) => void }) {
  const nothingOpen = useOverlayStackEmpty();
  const free = ready && nothingOpen;
  /* The user asked for the review over whatever is open: set by the marker,
     cleared once the review is drawn or nothing is left to show. */
  // voice-registry-exempt: whether the held review was asked for, set only through `VOICE_ACTIONS.showApprovalRequest`
  const [summoned, setSummoned] = useState(false);
  /* `summonable` is false while something is drawn over every overlay layer
     (voice's numbered choice): a review asked for then waits for it to close,
     rather than opening beneath it, and the marker stays up meanwhile. */
  const asked = summoned && summonable;
  const { review, approve, deny, waiting, opening } = useTrustRequests(runtime, fleet, free, onNotice, asked);
  /* The review that is up, by its number: drawn once the app was free (or the
     user asked for it) while it was shown, and kept drawn while its own
     dialog is what is open. */
  const [up, setUp] = useState<number>();
  if (review && up !== review.id && (free || asked)) setUp(review.id);
  const drawn = review !== undefined && up === review.id;
  /* Requests not shown yet, and a review read but held back undrawn. */
  const held = waiting + (review && !drawn ? 1 : 0);
  if (summoned && (drawn || (held === 0 && !opening))) setSummoned(false);
  /* One attempt per ask (Qodo, PR #2525): an attempt that ends without a
     review (the deck did not answer, or answered it approved) clears the ask,
     so the marker comes back and a later retry does not open over what the
     user is in without being asked again. */
  const wasOpening = useRef(false);
  useEffect(() => {
    if (wasOpening.current && !opening && !review) setSummoned(false);
    wasOpening.current = opening;
  }, [opening, review]);
  /* Not while a review is up: it is what is open, and says so itself. */
  const marker = held > 0 && !free && !asked && !drawn;
  const summon = useCallback((): string | undefined => { setSummoned(true); return undefined; }, []);
  useEffect(() => (marker ? publishHeldTrustRequest(() => { setSummoned(true); }) : undefined), [marker]);
  const approveShown = useCallback((token: string) => { void approve(token); }, [approve]);
  if (marker) {
    const label = approvalsWaitingLabel(held);
    return (
      <div className="trust-waiting-marker" data-testid="trust-waiting-marker" {...APPROVAL_PEER_PROPS}>
        <button className="button primary compact" title={`${label}: open the review`} onClick={() => VOICE_ACTIONS.showApprovalRequest.run({ showApprovalRequest: summon })}>
          <ShieldAlert size={14} aria-hidden="true" /> {label} <span className="trust-waiting-action">· {REVIEW_WAITING}</span>
        </button>
      </div>
    );
  }
  if (!review || !drawn) return null;
  return <ProjectTrustDialog key={review.id} review={review} onApprove={approveShown} onCancel={deny} />;
}
