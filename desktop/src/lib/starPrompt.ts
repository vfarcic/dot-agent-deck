import { useCallback, useEffect, useRef, useState } from "react";

/**
 * PRD #2174 — the star-repo reminder, the TUI's star prompt (PRD #37) in the
 * desktop app.
 *
 * About once every ten launches, until the user stars the repository or says
 * not to ask again, the app opens a small dialog when it starts. **Star**
 * (`s`) opens the repository page in the default browser and stops the
 * reminder; **Later** (`l`, `Esc`) asks again after another ten launches;
 * **Don't ask again** (`d`) stops it.
 *
 * The reminder's state is one per-user file shared with the TUI (the crate's
 * `desktop/src-tauri/src/star_prompt.rs` says where), so a launch of either
 * client counts and a choice made in either holds in both. The webview never
 * reads the file: it asks once at startup whether this launch is one the
 * reminder shows at, and reports the choice.
 *
 * The words are the TUI's (`src/star_prompt.rs`, CLAUDE.md rule 22), and a
 * Rust test in the desktop crate reads this file to keep them so.
 */

/** The dialog's title, the TUI prompt's. */
export const STAR_TITLE = "Enjoying dot-agent-deck?";
/** The invitation's two lines, the TUI prompt's. */
export const STAR_BODY_FIRST = "If you find dot-agent-deck useful,";
export const STAR_BODY_SECOND = "please consider starring the repo!";
/** The repository, as the prompt shows it and as Star opens it. */
export const STAR_REPO_DISPLAY = "github.com/vfarcic/dot-agent-deck";
export const STAR_REPO_URL = "https://github.com/vfarcic/dot-agent-deck";
/** The three choices, in the TUI's words and key order. */
export const STAR = "Star";
export const LATER = "Later";
export const DONT_ASK_AGAIN = "Don't ask again";
/** What Star reports: the TUI's status-line sentences. */
export const STAR_THANKS = "Thanks for starring! ⭐";
export const STAR_VISIT = "Visit github.com/vfarcic/dot-agent-deck to star ⭐";
/** What a spoken choice answers while the reminder is not showing. */
export const NO_STAR_PROMPT = "The star reminder is not showing.";
/** What a spoken choice answers while something is drawn over the reminder. */
export const STAR_PROMPT_COVERED = "Nothing was chosen: something is open over the star reminder.";

/** The three choices, as the bridge names them. */
export type StarChoice = "star" | "later" | "dismiss";

/** The runtime calls the reminder needs; a runtime without them never shows it. */
export type StarPromptRuntime = {
  starPromptLaunch?: () => Promise<boolean>;
  starPromptChoose?: (choice: StarChoice) => Promise<string | undefined>;
};

/** The reminder on screen, as the voice surface reaches it. */
type PublishedStarPrompt = { choose: (choice: StarChoice) => string | undefined };

let published: PublishedStarPrompt | undefined;

/**
 * A spoken choice: make it in the reminder on screen, or say why not. With no
 * reminder showing it does nothing and answers {@link NO_STAR_PROMPT}.
 */
export function chooseShownStarPrompt(choice: StarChoice): string | undefined {
  if (!published) return NO_STAR_PROMPT;
  return published.choose(choice);
}

/** Whether the reminder is on screen. */
export function starPromptIsShown(): boolean {
  return published !== undefined;
}

/** Publish the reminder on screen; answers the unpublish, which only takes back its own. */
export function publishStarPrompt(prompt: PublishedStarPrompt): () => void {
  published = prompt;
  return () => {
    if (published === prompt) published = undefined;
  };
}

/**
 * The reminder's flow, for the app shell: asks once at startup whether this
 * launch shows the reminder, and answers `due` until a choice is made.
 * `report` is given Star's sentence, or the reason a choice could not be
 * saved.
 */
export function useStarPrompt(runtime: StarPromptRuntime, report: (message: string) => void): { due: boolean; choose: (choice: StarChoice) => void } {
  const [due, setDue] = useState(false);
  /* Asked once per mount. The crate counts one launch per app process however
     often it is asked, so a development double-mount or a reload counts no
     extra launch; this only saves the second round trip. */
  const asked = useRef(false);
  const launch = runtime.starPromptLaunch;
  useEffect(() => {
    if (asked.current || !launch) return;
    asked.current = true;
    void launch().then((show) => { if (show) setDue(true); }, () => undefined);
  }, [launch]);
  const choosePrompt = runtime.starPromptChoose;
  const choose = useCallback((choice: StarChoice) => {
    setDue(false);
    if (!choosePrompt) return;
    void choosePrompt(choice).then(
      (message) => { if (message) report(message); },
      (cause: unknown) => report(cause instanceof Error ? cause.message : String(cause)),
    );
  }, [choosePrompt, report]);
  return { due, choose };
}
