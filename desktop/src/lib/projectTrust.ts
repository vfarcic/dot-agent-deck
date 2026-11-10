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

/** The review on screen: what it shows, and what approving it starts. */
export interface ShownReview {
  target: ProjectTrustTarget;
  view: ProjectTrustView;
  start: () => Promise<void> | void;
  busy: boolean;
  error?: string;
  notice?: string;
}

/**
 * The one review the app can show at a time, for voice's `approve_project_commands`:
 * the dialog publishes its approve while it is on screen, and the shell's voice
 * context reads it here. One at a time because the dialog is modal.
 */
let publishedApprove: (() => string | undefined) | undefined;

/** Voice's approve: approve the review on screen, or say why not. */
export function approveShownReview(): string | undefined {
  return publishedApprove ? publishedApprove() : NO_REVIEW_OPEN;
}

/** Whether a review is on screen (for the shell's voice context). */
export function reviewIsShown(): boolean {
  return publishedApprove !== undefined;
}

/** Publish `approve` while a review is on screen; answers the unpublish, which only takes back its own. */
export function publishReviewApprove(approve: () => string | undefined): () => void {
  publishedApprove = approve;
  return () => {
    if (publishedApprove === approve) publishedApprove = undefined;
  };
}

/**
 * The review flow, for a screen that starts orchestrations.
 *
 * `check(target, start)` asks the deck first. It answers `approved` (the caller
 * starts, exactly as before), `refused` with the sentence to show (the deck is
 * older than this app, or the question failed), or `review`: the dialog is now
 * showing, nothing has started, and `start` runs only after the user's Approve
 * and start has been recorded by the deck. Cancel drops it: nothing is started
 * and nothing is stored. A caller whose start the deck still refused with
 * `not-approved` (the config changed between the check and the start) calls
 * `check` again, which shows the new text.
 */
export function useProjectTrustReview(runtime: ProjectTrustRuntime) {
  const [shown, setShown] = useState<ShownReview>();
  const current = useRef<ShownReview | undefined>(undefined);
  current.current = shown;
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  const check = useCallback(async (target: ProjectTrustTarget, start: () => Promise<void> | void): Promise<TrustCheck> => {
    if (!runtime.projectTrustState || !runtime.approveProjectTrust) return { kind: "refused", reason: RUNTIME_CANNOT_REVIEW };
    let answer: ProjectTrust;
    try {
      answer = await runtime.projectTrustState(target);
    } catch (cause) {
      return { kind: "refused", reason: `Nothing was started: the orchestration's approval could not be checked (${messageOf(cause)}).` };
    }
    if (answer.kind === "unsupported") return { kind: "refused", reason: answer.reason };
    if (answer.state === "approved") return { kind: "approved" };
    const { kind: _kind, ...view } = answer;
    if (mounted.current) setShown({ target, view, start, busy: false });
    return { kind: "review" };
  }, [runtime]);

  const cancel = useCallback(() => {
    setShown(undefined);
  }, []);

  const approve = useCallback(async () => {
    const asked = current.current;
    if (!asked || asked.busy || !runtime.approveProjectTrust || !runtime.projectTrustState) return;
    const still = (next: ShownReview) => {
      if (!mounted.current) return;
      setShown((now) => (now?.view.digest === asked.view.digest && now.target === asked.target ? next : now));
    };
    still({ ...asked, busy: true, error: undefined });
    try {
      const answer = await runtime.approveProjectTrust(asked.target, asked.view.digest);
      if (answer.kind === "unsupported") {
        still({ ...asked, busy: false, error: answer.reason });
        return;
      }
      if (answer.state !== "approved") {
        const { kind: _kind, ...view } = answer;
        still({ ...asked, view, busy: false, notice: TRUST_TEXT_CHANGED_NOTICE });
        return;
      }
      if (mounted.current) setShown((now) => (now?.target === asked.target ? undefined : now));
      await asked.start();
    } catch (cause) {
      const message = messageOf(cause);
      if (isTrustTextChanged(message)) {
        try {
          const fresh = await runtime.projectTrustState(asked.target);
          if (fresh.kind === "review") {
            const { kind: _kind, ...view } = fresh;
            still({ ...asked, view, busy: false, notice: TRUST_TEXT_CHANGED_NOTICE });
            return;
          }
        } catch {
          // Said below, with the deck's own refusal.
        }
      }
      still({ ...asked, busy: false, error: message });
    }
  }, [runtime]);

  return { shown, check, approve, cancel };
}
