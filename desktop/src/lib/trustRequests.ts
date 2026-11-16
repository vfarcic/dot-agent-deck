import { useCallback, useEffect, useRef, useState } from "react";

import { overlayStackIsEmpty } from "../hooks/useInertBackground";
import type { DeckFleet, TrustRequest } from "../types";
import { deckName } from "./displayText";
import {
  requestAnsweredElsewhereNotice,
  requestApprovedNotice,
  requestDeniedNotice,
  REVIEW_APPROVING,
  reviewDeckChangedNotice,
  useProjectTrustReview,
  type ProjectTrustRuntime,
} from "./projectTrust";

/**
 * Issue #2330 — the desktop half of the deck asking for an approval.
 *
 * When something other than the New agent form tries to start an
 * orchestration that is not approved as its config now reads — an agent's
 * `dispatch --orchestration`, an orchestrator restarting a worker, a schedule
 * firing, Run now — the deck refuses it and lists a **request** on every
 * attached client. This app shows the review the New agent form already has
 * (`useProjectTrustReview`) for each request of every deck it observes, naming
 * the deck that asked, one at a time and only while nothing else is open:
 *
 * - **Approve** records the approval exactly as the New agent form does, and
 *   starts nothing here — the attempt that was refused starts when whoever
 *   made it tries again, which the notice says.
 * - **Deny** (and `Esc`, and voice's `close`) tells that deck the user
 *   declined it.
 * - A request answered anywhere else — the TUI, another app window, the CLI,
 *   or settled because the config changed — leaves its deck's list, and its
 *   review closes with a notice saying so.
 *
 * The app shows each request once: a request it has shown is not shown again
 * when a later snapshot still lists it. A request is known by its deck, its
 * number there and the digest of the text it is about; a deck whose daemon
 * restarted (another process answers for it) has forgotten what it asked and
 * what was declined, so its requests are new ones.
 */

/** The runtime calls the flow needs. A runtime without `denyTrustRequest` shows no request. */
export type TrustRequestRuntime = ProjectTrustRuntime & {
  denyTrustRequest?: (deckId: string, id: number) => Promise<void>;
};

/** A sentence the flow leaves behind, for the shell's toast. */
export interface TrustRequestNotice {
  message: string;
  detail?: string;
}

/** One request of one observed deck, as the flow opens it. */
interface PendingTrustRequest {
  /** This app's name for it (see {@link requestKey}). */
  key: string;
  deckId: string;
  deckName: string;
  request: TrustRequest;
}

/** A request's identity in this app: its deck, its number there, and the text it is about. */
function requestKey(deckId: string, request: Pick<TrustRequest, "id" | "digest">): string {
  return `${deckId}\u0000${request.id}\u0000${request.digest}`;
}

/** Every request the observed decks list, deck by deck in fleet order, each deck's oldest first. Only a connected deck's. */
export function pendingTrustRequests(fleet: DeckFleet): PendingTrustRequest[] {
  return fleet.flatMap((deck) => {
    const { connection } = deck;
    const deckId = connection.deckId;
    if (deckId === undefined || connection.status !== "connected") return [];
    return (connection.trustRequests ?? []).map((request) => ({ key: requestKey(deckId, request), deckId, deckName: deckName(connection), request }));
  });
}

/** The review on screen is for a request, and which. */
type OpenRequest = { key: string; deckId: string; request: TrustRequest };

/**
 * The flow, for the app shell. `free` is whether a review may open now (no
 * other dialog or overlay is up). `report` is given each sentence the flow
 * leaves behind. Answers the review on screen and its two answers, for the
 * dialog: `approve` takes the token the dialog rendered, and `deny` answers
 * the sentence saying why it cannot decline now, if it cannot.
 */
export function useTrustRequests(runtime: TrustRequestRuntime, fleet: DeckFleet, free: boolean, report: (notice: TrustRequestNotice) => void) {
  const review = useProjectTrustReview(runtime);
  const { shown, check, cancel } = review;
  /* Every request this app has shown, or decided not to (the deck answered
     it approved): none of them opens again. */
  const handled = useRef(new Set<string>());
  /* The daemon process each deck was last seen answering from: another one
     is a restart, which forgot every request it raised and every denial. */
  const incarnations = useRef(new Map<string, string>());
  /* The request the review on screen is for, set before the review shows. */
  const open = useRef<OpenRequest | undefined>(undefined);
  /* Whether a request's review is being asked for: the deck has not answered yet. */
  const [opening, setOpening] = useState(false);
  const canDeny = runtime.denyTrustRequest !== undefined;

  /* The next request opens once nothing is open and no review is on screen
     or on its way. */
  useEffect(() => {
    forgetRestartedDecks(fleet, incarnations.current, handled.current);
    /* `free` is as of the render; an overlay that came up in this same commit
       (the star reminder, say) is on the stack already, and holds it back. */
    if (!canDeny || !free || opening || shown || !overlayStackIsEmpty()) return;
    const next = pendingTrustRequests(fleet).find((candidate) => !handled.current.has(candidate.key));
    if (!next) return;
    handled.current.add(next.key);
    const { request, deckId } = next;
    open.current = { key: next.key, deckId, request };
    setOpening(true);
    /* Approving starts nothing: the refused attempt starts when it is retried. */
    const approved = () => report({ message: requestApprovedNotice(request.orchestration, request.askedBy) });
    void check({ deckId, path: request.configDir, orchestration: request.orchestration }, approved, { deckName: next.deckName, request: { id: request.id, askedBy: request.askedBy } })
      .then((answer) => {
        if (answer.kind !== "review" && open.current?.key === next.key) open.current = undefined;
        /* The deck answered it approved: nothing to ask. A review that could
           not be asked for says why, and is not asked for again. */
        if (answer.kind === "refused") report({ message: answer.reason });
      })
      .finally(() => setOpening(false));
  }, [canDeny, check, fleet, free, opening, report, shown]);

  /* A request's review closes when its deck no longer lists it: another
     client answered it, it was approved from the CLI, or its text changed. A
     review whose approval is on its way waits for the deck's answer instead,
     which closes it. A deck that left the observed fleet (the selection
     moved) closes it too, and its request may open again when the deck is
     observed again. */
  useEffect(() => {
    const asked = open.current;
    if (!shown?.request || !asked || shown.busy) return;
    const deck = fleet.find((candidate) => candidate.connection.deckId === asked.deckId);
    if (!deck) {
      open.current = undefined;
      handled.current.delete(asked.key);
      cancel();
      report(reviewDeckChangedNotice(shown));
      return;
    }
    if (deck.connection.status !== "connected") return;
    if ((deck.connection.trustRequests ?? []).some((request) => requestKey(asked.deckId, request) === asked.key)) return;
    open.current = undefined;
    cancel();
    report({ message: requestAnsweredElsewhereNotice(shown.view.orchestration) });
  }, [cancel, fleet, report, shown]);

  const deny = useCallback((): string | void => {
    const asked = open.current;
    const current = shown;
    if (!current?.request || !asked) {
      cancel();
      return;
    }
    if (current.busy) return REVIEW_APPROVING;
    open.current = undefined;
    cancel();
    const denyRequest = runtime.denyTrustRequest;
    if (!denyRequest) return;
    void denyRequest(asked.deckId, asked.request.id).then(
      () => report({ message: requestDeniedNotice(current.view.orchestration) }),
      (cause: unknown) => {
        /* Not declined, so it may open again once nothing is open. */
        handled.current.delete(asked.key);
        report({ message: cause instanceof Error ? cause.message : String(cause) });
      },
    );
  }, [cancel, report, runtime.denyTrustRequest, shown]);

  return { review: shown, approve: review.approve, deny };
}

/** Forget what was shown of a deck whose daemon restarted, so its requests open again. */
function forgetRestartedDecks(fleet: DeckFleet, incarnations: Map<string, string>, handled: Set<string>): void {
  for (const { connection } of fleet) {
    const { deckId, incarnation } = connection;
    if (deckId === undefined || connection.status !== "connected" || !incarnation) continue;
    const before = incarnations.get(deckId);
    incarnations.set(deckId, incarnation);
    if (before === undefined || before === incarnation) continue;
    const prefix = `${deckId}\u0000`;
    for (const key of Array.from(handled)) if (key.startsWith(prefix)) handled.delete(key);
  }
}
