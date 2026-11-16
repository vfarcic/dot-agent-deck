import { useCallback, useState } from "react";

import { useOverlayStackEmpty } from "../hooks/useInertBackground";
import { useTrustRequests, type TrustRequestNotice, type TrustRequestRuntime } from "../lib/trustRequests";
import type { DeckFleet } from "../types";
import { ProjectTrustDialog } from "./ProjectTrustDialog";

/**
 * Issue #2330 — the review a deck asked for, shown over whichever screen is
 * up: one request at a time, from any deck the app observes, once `ready`
 * (the shell's list of what it waits behind, the star reminder's) and no
 * overlay is open at all, read from the overlay stack so whatever the user is
 * in is never covered. Once up it stays up until answered, since it is then on
 * the stack itself. A review whose deck answered while something else was open
 * waits, undrawn, for that to close.
 *
 * Its own component, like `StarPromptGate`, so that only it re-renders as
 * overlays come and go.
 */
export function TrustRequestGate({ runtime, fleet, ready, onNotice }: { runtime: TrustRequestRuntime; fleet: DeckFleet; ready: boolean; onNotice: (notice: TrustRequestNotice) => void }) {
  const nothingOpen = useOverlayStackEmpty();
  const free = ready && nothingOpen;
  const { review, approve, deny } = useTrustRequests(runtime, fleet, free, onNotice);
  /* The review that is up, by its number: drawn once the app was free while
     it was shown, and kept drawn while its own dialog is what is open. */
  const [up, setUp] = useState<number>();
  if (review && up !== review.id && free) setUp(review.id);
  const approveShown = useCallback((token: string) => { void approve(token); }, [approve]);
  if (!review || (up !== review.id && !free)) return null;
  return <ProjectTrustDialog key={review.id} review={review} onApprove={approveShown} onCancel={deny} />;
}
