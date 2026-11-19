import { useCallback, useEffect, useState } from "react";

/**
 * PRD #211 — the Antigravity experimental notice, the TUI's notice in the
 * desktop app.
 *
 * When the user starts Antigravity from the New agent dialog, a dialog says
 * Antigravity support is experimental and asks for an issue: a problem, or
 * word that it works. It shows on the first such start and then on every
 * hundredth after the last showing. **Open issues** (`o`) opens the
 * repository's new-issue page; **Later** (`l`, `Esc`) closes it until the next
 * interval; **Don't show again** (`d`) stops it.
 *
 * The state is one per-user file shared with the TUI (the crate's
 * `desktop/src-tauri/src/antigravity_notice.rs` says where), so a start in
 * either client counts and a choice made in either holds in both. The webview
 * never reads the file: the New agent dialog announces each start it made,
 * the crate answers whether that start shows the notice, and the shell
 * reports the choice.
 *
 * The words are the TUI's (`src/antigravity_notice.rs`, CLAUDE.md rule 22),
 * and a Rust test in the desktop crate reads this file to keep them so.
 */

/** The dialog's title, the TUI notice's. */
export const NOTICE_TITLE = "Antigravity support is experimental";
/** The request, the TUI notice's. */
export const NOTICE_BODY = "Tell us how it works for you: report a problem, or let us know it works as expected.";
/** The repository's issues, as the notice shows them. */
export const NOTICE_ISSUES_DISPLAY = "github.com/vfarcic/dot-agent-deck/issues";
/**
 * Not exported, because nothing in TypeScript uses this or `NOTICE_VISIT`: the
 * crate opens the URL and words the result
 * (`desktop/src-tauri/src/antigravity_notice.rs`). They are here because that
 * file's `the_dialog_uses_the_tui_notices_words` reads this file as text for
 * the TUI's words, so deleting them fails it.
 */
const NOTICE_NEW_ISSUE_URL = "https://github.com/vfarcic/dot-agent-deck/issues/new";
/** The three choices, in the TUI's words and key order. */
export const OPEN_ISSUES = "Open issues";
export const NOTICE_LATER = "Later";
export const DONT_SHOW_AGAIN = "Don't show again";
/** What Open issues reports: the TUI's status-line sentences. */
export const NOTICE_THANKS = "Thanks for trying Antigravity in dot-agent-deck!";
/** Not exported; see `NOTICE_NEW_ISSUE_URL`. */
const NOTICE_VISIT = "Visit github.com/vfarcic/dot-agent-deck/issues to tell us how Antigravity works";
/**
 * How long the dialog's keys wait after it is drawn before they choose. It
 * opens on its own, right after a start, while the user may already be typing
 * into the new agent, so a key on its way to the agent must not answer it.
 */
export const NOTICE_KEYS_ARM_MS = 400;
/** What a spoken choice answers while the notice is not showing. */
export const NO_ANTIGRAVITY_NOTICE = "The Antigravity notice is not showing.";
/** What a spoken choice answers while something is drawn over the notice. */
export const ANTIGRAVITY_NOTICE_COVERED = "Nothing was chosen: something is open over the Antigravity notice.";

/** The three choices, as the bridge names them. */
export type NoticeChoice = "open" | "later" | "dismiss";

/**
 * What a New agent start answers: whether it shows the notice, and the first
 * problem with the notice's state file this app has met, to say once.
 */
export type NoticeStart = { show: boolean; problem?: string };

/**
 * The crate's answer to a start, read defensively: only a literal `true`
 * shows the notice, for the star reminder's reason, and only a string is a
 * problem to say.
 */
export function noticeStartFrom(value: unknown): NoticeStart {
  if (typeof value !== "object" || value === null) return { show: false };
  const { show, problem } = value as { show?: unknown; problem?: unknown };
  return typeof problem === "string" ? { show: show === true, problem } : { show: show === true };
}

/** The runtime calls the notice needs; a runtime without them never shows it. */
export type AntigravityNoticeRuntime = {
  antigravityNoticeStart?: (command: string | undefined) => Promise<NoticeStart>;
  antigravityNoticeChoose?: (choice: NoticeChoice) => Promise<string | undefined>;
};

/** The notice on screen, as the voice surface reaches it. */
type PublishedNotice = { choose: (choice: NoticeChoice) => string | undefined };

let published: PublishedNotice | undefined;

/**
 * A spoken choice: make it in the notice on screen, or say why not. With no
 * notice showing it does nothing and answers {@link NO_ANTIGRAVITY_NOTICE}.
 */
export function chooseShownAntigravityNotice(choice: NoticeChoice): string | undefined {
  if (!published) return NO_ANTIGRAVITY_NOTICE;
  return published.choose(choice);
}

/** Whether the notice is on screen. */
export function antigravityNoticeIsShown(): boolean {
  return published !== undefined;
}

/** Publish the notice on screen; answers the unpublish, which only takes back its own. */
export function publishAntigravityNotice(notice: PublishedNotice): () => void {
  published = notice;
  return () => {
    if (published === notice) published = undefined;
  };
}

/** Who hears of a New agent start: the shell's {@link useAntigravityNotice}. */
const startListeners = new Set<(command: string | undefined) => void>();

/**
 * The New agent dialog's start of a plain agent succeeded: the deck accepted
 * `command` (`undefined` for the deck's default shell). The shell asks the
 * crate whether it starts Antigravity and whether this start shows the
 * notice. Orchestration starts, restores and dispatches do not announce.
 */
export function announceNewAgentStart(command: string | undefined): void {
  for (const listener of startListeners) listener(command);
}

/**
 * The notice's flow, for the app shell: hears each New agent start, asks the
 * runtime whether it shows the notice, and answers `due` until a choice is
 * made. `report` is given Open issues' sentence and the first problem with
 * the notice's state file, which the crate hands out once. A choice closes the
 * notice whether or not it could be saved, as in the TUI: the crate holds an
 * unsaved Don't show again until the app restarts.
 */
export function useAntigravityNotice(runtime: AntigravityNoticeRuntime, report: (message: string) => void): { due: boolean; choose: (choice: NoticeChoice) => void } {
  const [due, setDue] = useState(false);
  const start = runtime.antigravityNoticeStart;
  useEffect(() => {
    if (!start) return;
    const listener = (command: string | undefined) => {
      /* A start the crate could not answer shows no notice, and says why in the console rather than nowhere. */
      void start(command).then(({ show, problem }) => {
        if (problem) report(problem);
        if (show) setDue(true);
      }, (cause: unknown) => console.warn("The Antigravity notice's start check failed:", cause));
    };
    startListeners.add(listener);
    return () => { startListeners.delete(listener); };
  }, [report, start]);
  const chooseNotice = runtime.antigravityNoticeChoose;
  const choose = useCallback((choice: NoticeChoice) => {
    setDue(false);
    if (!chooseNotice) return;
    void chooseNotice(choice).then(
      (message) => { if (message) report(message); },
      (cause: unknown) => report(cause instanceof Error ? cause.message : String(cause)),
    );
  }, [chooseNotice, report]);
  return { due, choose };
}
