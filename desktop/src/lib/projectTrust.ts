import { useCallback, useEffect, useRef, useState } from "react";

/**
 * PRD #1885 — the desktop half of the trust prompt, `Review project commands`.
 *
 * Before the app starts an orchestration from a project's config (the New
 * agent dialog's orchestration launch and the Runs screen's Activate), it asks
 * the deck that launch goes to whether the orchestration's commands and
 * prompts are approved as they now read. An approved, unchanged one starts as
 * before, with no dialog. Otherwise the review dialog shows what would run and
 * starts nothing while it is open: **Approve and start** approves exactly the
 * text shown (its digest goes back to the deck, which refuses if the config
 * changed meanwhile) and then starts it; **Cancel** starts and stores nothing.
 *
 * The words are the TUI's (`src/trust_prompt.rs`, CLAUDE.md rule 22), and a
 * Rust test in `desktop/src-tauri/src/trust_review.rs` reads this file to keep
 * them so. Every piece of config text arrives already escaped by the bridge
 * (control characters and bidi overrides shown as their escapes), and the
 * dialog renders it as plain text.
 */

/** The dialog's title, the TUI prompt's. */
export const TRUST_TITLE = "Review project commands";
/** What it says of an orchestration nothing was ever approved for. */
export const NOT_APPROVED_BEFORE = "not approved before";
/** What it says of one whose approved text changed. */
export const CHANGED_SINCE_APPROVAL = "changed since you approved it";
/** The sentence above the roles, the TUI prompt's. */
export const TRUST_INTRO = "Starting it runs these commands and gives these prompts to its agents. Approve only what you trust.";
/** The approve button, the TUI's `a approve and start`. */
export const APPROVE_AND_START = "Approve and start";
/** The approve button while the approval is under way. */
export const APPROVING = "Approving…";
/** What the dialog says when the config changed while it was being shown. */
export const TRUST_TEXT_CHANGED_NOTICE = "The project's commands or prompts changed while they were shown, so nothing was approved. This is the new text: review it again.";
/** What voice's approve answers with no review open. */
export const NO_REVIEW_OPEN = "No project commands are waiting for review.";
/** What voice's "start it" answers while a review is open over the New agent form. */
export const REVIEW_FIRST = "Nothing was started: review the project commands first — approve them, or cancel.";
/** What voice's approve answers while an approval is already under way. */
export const REVIEW_APPROVING = "Those project commands are already being approved.";
/**
 * What voice's approve answers when the review on screen is not the one the
 * words were spoken against: it was cancelled and another opened, or its text
 * changed and was shown again.
 */
export const REVIEW_CHANGED = "Nothing was approved: the project commands on screen changed after you spoke. Review them, then approve again.";
/** What voice's approve answers when something is drawn over the review. */
export const REVIEW_COVERED = "Nothing was approved: something is open over the project commands review.";
/** Shown in the dialog while an approval is on its way to the deck. */
export const APPROVAL_IN_FLIGHT_NOTE = "The approval has been sent to the deck. Cancel now and nothing starts, but the deck may still record the approval.";
/**
 * Said when the Runs screen's review closes because the selected deck changed
 * under it before Approve and start was pressed. Once it was pressed the
 * approval may already be on the deck: `reviewDeckChangedNotice` says that.
 */
export const REVIEW_DECK_CHANGED = "The project commands review was closed because the selected deck changed. Nothing was approved or started.";

/** The deck a review is for, as the dialog labels it. */
export const DECK_LABEL = "Deck";
/** A role's `start` flag, in the TUI prompt's words (`src/trust_prompt.rs`). */
export const START_LABEL = "start";
export const START_TRUE = "true — the orchestration starts with this role";
export const START_FALSE = "false";
/** A role's `clear` flag, in the TUI prompt's words. */
export const CLEAR_LABEL = "clear";
export const CLEAR_TRUE = "true — restarted before each task";
export const CLEAR_FALSE = "false — keeps running between tasks";

/** The deck's refusal codes this flow reads (`src/daemon_protocol.rs`). */
export const NOT_APPROVED_CODE = "not-approved";
export const TRUST_TEXT_CHANGED_CODE = "trust-text-changed";

/** One role as the dialog shows it. Every string is already escaped. */
export interface ProjectTrustRole {
  name: string;
  /** `changed` or `new` relative to the stored approval. */
  change?: "changed" | "new";
  command: string;
  agent?: string;
  /** The role's `start` flag: the orchestration starts with this role. */
  start: boolean;
  /** The role's `clear` flag: its agent is restarted before each task. */
  clear: boolean;
  /** One string per line. */
  description?: string[];
  /** One string per line. */
  prompt?: string[];
}

/** The deck's view of one orchestration's commands and prompts. */
export interface ProjectTrustView {
  project: string;
  /** The directory the config was read from, when it is not `project`. */
  configDir?: string;
  orchestration: string;
  state: "approved" | "never_approved" | "changed";
  roles: ProjectTrustRole[];
  removedRoles: string[];
  /** The digest of exactly this text, which an approval carries back. */
  digest: string;
  /**
   * The config revision of the bytes this text was read from. A launch after
   * approving this text uses it, so text re-shown after a change starts
   * against its own revision rather than the one listed before the change.
   */
  configRevision?: string;
}

/** The bridge's answer (`DesktopProjectTrust` in `trust_review.rs`). */
export type ProjectTrust = ({ kind: "review" } & ProjectTrustView) | { kind: "unsupported"; reason: string };

/** Which orchestration of which project, on which deck. `deckId` absent is the selected deck (the Runs screen's launch). */
export interface ProjectTrustTarget {
  deckId?: string;
  path: string;
  orchestration: string;
}

/** The two runtime calls the review needs. Optional on the runtime, and a runtime without them starts nothing. */
export interface ProjectTrustRuntime {
  projectTrustState?: (target: ProjectTrustTarget) => Promise<ProjectTrust>;
  approveProjectTrust?: (target: ProjectTrustTarget, digest: string) => Promise<ProjectTrust>;
}

/** A start the deck refused because the orchestration is not approved: the race the review re-shows for. */
export function isNotApproved(message: string): boolean {
  return message.includes(`${NOT_APPROVED_CODE}:`);
}

/** An approval the deck refused because the text changed since it was shown. */
export function isTrustTextChanged(message: string): boolean {
  return message.includes(TRUST_TEXT_CHANGED_CODE);
}

/** The state line's words for `view`, the TUI prompt's. */
export function trustStateWords(view: Pick<ProjectTrustView, "state">): string {
  if (view.state === "never_approved") return NOT_APPROVED_BEFORE;
  if (view.state === "changed") return CHANGED_SINCE_APPROVAL;
  return "approved";
}

const RUNTIME_CANNOT_REVIEW = "Nothing was started: this app cannot review the orchestration's commands and prompts here.";

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/** What asking first decided. `review` means the dialog is now showing, and nothing has started. */
export type TrustCheck = { kind: "approved" } | { kind: "review" } | { kind: "refused"; reason: string };

/** What starts once the shown text is approved: given the deck's view of what it approved. */
export type ApprovedStart = (approved: ProjectTrustView) => Promise<void> | void;

/** The review on screen: what it shows, and what approving it starts. */
export interface ShownReview {
  /** This review's own number, never reused: what an approval is bound to. */
  id: number;
  target: ProjectTrustTarget;
  /** The deck's name, shown so the user knows which deck they approve on. */
  deckName?: string;
  view: ProjectTrustView;
  start: ApprovedStart;
  busy: boolean;
  error?: string;
  notice?: string;
}

/**
 * The identity of exactly the review on screen: its number and the digest of
 * the text it shows. An approval spoken or clicked against one token approves
 * nothing once the token on screen is another — the review was cancelled and
 * another opened, or its text changed and was shown again.
 */
export function reviewToken(review: Pick<ShownReview, "id" | "view">): string {
  return `${review.id}:${review.view.digest}`;
}

/**
 * What the Runs screen says when `review` closes because the selected deck
 * changed. Before Approve and start was pressed, nothing was approved or
 * started. While the approval is on its way (`busy`), nothing starts either,
 * but the deck it was sent to may still record it — so the sentence names
 * that deck, and its `detail` gives the command that takes the approval back
 * (a detail, because the toast clips its sentence and a clipped command is no
 * use).
 */
export function reviewDeckChangedNotice(review: Pick<ShownReview, "busy" | "deckName"> & { view: Pick<ProjectTrustView, "project" | "orchestration"> }): { message: string; detail?: string } {
  if (!review.busy) return { message: REVIEW_DECK_CHANGED };
  const deck = review.deckName === undefined ? "the deck it was sent to" : `deck ${review.deckName}`;
  return {
    message: `The project commands review was closed because the selected deck changed. Nothing was started, but the approval already sent may still be recorded on ${deck}`,
    detail: `to undo it, run dot-agent-deck trust revoke ${review.view.project} --orchestration ${review.view.orchestration} on that deck's host`,
  };
}

/** Numbers every review the app shows, so no two share a token. */
let reviewsShown = 0;

/**
 * The one review the app can show at a time, for voice's `approve_project_commands`:
 * the dialog publishes its token and its approve while it is on screen, and
 * the voice surface reads the token when an utterance is DECLARED. One at a
 * time because the dialog is modal.
 */
interface PublishedReview {
  token: string;
  approve: () => string | undefined;
}
let published: PublishedReview | undefined;

/** The token of the review on screen, or `undefined` with none (read when an utterance is declared). */
export function shownReviewToken(): string | undefined {
  return published?.token;
}

/**
 * Voice's approve: approve the review on screen when it is the one `declared`
 * names — the token read when the utterance was declared — or say why not. An
 * utterance declared with no review open (`undefined`) approves nothing either.
 */
export function approveShownReview(declared: string | undefined): string | undefined {
  if (!published) return NO_REVIEW_OPEN;
  if (declared !== published.token) return REVIEW_CHANGED;
  return published.approve();
}

/** Whether a review is on screen (for the shell's voice context). */
export function reviewIsShown(): boolean {
  return published !== undefined;
}

/** Publish the review on screen; answers the unpublish, which only takes back its own. */
export function publishReview(review: PublishedReview): () => void {
  published = review;
  return () => {
    if (published === review) published = undefined;
  };
}

/**
 * The review flow, for a screen that starts orchestrations.
 *
 * `check(target, start)` asks the deck first. It answers `approved` (the caller
 * starts, exactly as before), `refused` with the sentence to show (the deck is
 * older than this app, or the question failed), or `review`: the dialog is now
 * showing, nothing has started, and `start` runs only after the user's Approve
 * and start has been recorded by the deck — given the deck's view of what it
 * approved, whose `configRevision` is the one to launch with. Cancel drops it:
 * nothing is started and nothing is stored. A caller whose start the deck
 * still refused with `not-approved` (the config changed between the check and
 * the start) calls `check` again, which shows the new text.
 *
 * **The commit point is the start, not the approval.** Cancel stays available
 * while an approval is on its way to the deck, and the dialog says so: a
 * cancelled, replaced or unmounted review starts nothing when the deck's answer
 * arrives, although the deck may already have recorded the approval. Every
 * continuation checks that the review it was asked for is still the one shown.
 */
export function useProjectTrustReview(runtime: ProjectTrustRuntime) {
  const [shown, setShown] = useState<ShownReview>();
  /* The review on screen, written synchronously with every change, so a
     continuation that resumes after an await reads what is shown NOW rather
     than what the last render saw. */
  const active = useRef<ShownReview | undefined>(undefined);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      active.current = undefined;
    };
  }, []);

  const show = useCallback((next: ShownReview | undefined) => {
    active.current = next;
    if (mounted.current) setShown(next);
  }, []);

  const check = useCallback(async (target: ProjectTrustTarget, start: ApprovedStart, display?: { deckName?: string }): Promise<TrustCheck> => {
    if (!runtime.projectTrustState || !runtime.approveProjectTrust) return { kind: "refused", reason: RUNTIME_CANNOT_REVIEW };
    let answer: ProjectTrust;
    try {
      answer = await runtime.projectTrustState(target);
    } catch (cause) {
      return { kind: "refused", reason: `Nothing was started: the orchestration's approval could not be checked (${messageOf(cause)}).` };
    }
    if (answer.kind === "unsupported") return { kind: "refused", reason: answer.reason };
    if (answer.state === "approved") return { kind: "approved" };
    if (!mounted.current) return { kind: "refused", reason: RUNTIME_CANNOT_REVIEW };
    const { kind: _kind, ...view } = answer;
    reviewsShown += 1;
    show({ id: reviewsShown, target, ...(display?.deckName === undefined ? {} : { deckName: display.deckName }), view, start, busy: false });
    return { kind: "review" };
  }, [runtime, show]);

  const cancel = useCallback(() => {
    show(undefined);
  }, [show]);

  /**
   * Approve the review on screen — only when it is still the one `expected`
   * names, when given (the dialog passes the token it rendered).
   */
  const approve = useCallback(async (expected?: string) => {
    const asked = active.current;
    if (!asked || asked.busy || !runtime.approveProjectTrust || !runtime.projectTrustState) return;
    if (expected !== undefined && expected !== reviewToken(asked)) return;
    /* Whether the review this approval was asked for is still on screen. */
    const still = () => mounted.current && active.current?.id === asked.id;
    const update = (next: ShownReview) => { if (still()) show(next); };
    update({ ...asked, busy: true, error: undefined, notice: undefined });
    try {
      const answer = await runtime.approveProjectTrust(asked.target, asked.view.digest);
      if (!still()) return;
      if (answer.kind === "unsupported") {
        update({ ...asked, busy: false, error: answer.reason });
        return;
      }
      const { kind: _kind, ...view } = answer;
      if (view.state !== "approved") {
        update({ ...asked, view, busy: false, notice: TRUST_TEXT_CHANGED_NOTICE });
        return;
      }
      show(undefined);
      await asked.start(view);
    } catch (cause) {
      if (!still()) return;
      const message = messageOf(cause);
      if (isTrustTextChanged(message)) {
        try {
          const fresh = await runtime.projectTrustState(asked.target);
          if (!still()) return;
          if (fresh.kind === "review") {
            const { kind: _kind, ...view } = fresh;
            update({ ...asked, view, busy: false, notice: TRUST_TEXT_CHANGED_NOTICE });
            return;
          }
        } catch {
          // Said below, with the deck's own refusal.
        }
      }
      update({ ...asked, busy: false, error: message });
    }
  }, [runtime, show]);

  return { shown, check, approve, cancel };
}
