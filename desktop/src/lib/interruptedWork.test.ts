import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import * as words from "./interruptedWork";
import type { InterruptedItem, ItemAgent, RestoreVerdict } from "./interruptedWork";

/**
 * PRD #1790 stage 2 (decision D2, CLAUDE.md rule 22) — the desktop's copy of
 * the interrupted work wording is pinned to the Rust source it was copied
 * from, `src/interrupted_work.rs`, which the CLI and the TUI read. A sentence
 * changed on one side and not the other fails here.
 */
const rust = readFileSync(resolve("../src/interrupted_work.rs"), "utf8");

/** Every `pub const NAME: &str = "…";` in the Rust module, by name, its string unescaped. */
function rustStrings(): Map<string, string> {
  const found = new Map<string, string>();
  for (const match of rust.matchAll(/pub const ([A-Z_]+): &str =\s*"((?:[^"\\]|\\.)*)";/g)) {
    found.set(match[1], JSON.parse(`"${match[2]}"`) as string);
  }
  return found;
}

/** The constants this module copies: every Rust wording constant the clients show. */
const COPIED = [
  "INTERRUPTED_WORK_TITLE", "OTHER_AGENTS_GROUP", "RECLAIM_ACTION", "RECLAIM_EXPLANATION", "DISMISS_ACTION", "DISMISS_ALL_ACTION", "KEPT_UNTIL",
  "RECLAIM_REMOVED", "RECLAIM_WOULD_REMOVE", "RECLAIM_KEPT_DIRTY", "RECLAIM_KEPT_HOLDS_WORKTREES", "RECLAIM_KEPT_COULD_NOT_CHECK", "RECLAIM_REMOVE_FAILED",
  "RECLAIM_TIMED_OUT", "RECLAIM_KEPT_CHANGED", "RECLAIM_KEPT_BUSY", "RECLAIM_KEPT_COULD_NOT_LOCK", "RECLAIM_NOT_RECORDED", "DISMISSED",
  "REFUSED_AGENT_CALLER", "REFUSED_BAD_REQUEST", "REFUSED_TOO_LARGE", "REFUSED_UNREADABLE", "REFUSED_INCOMPLETE_READ", "REFUSED_NOT_LISTED",
  "REFUSED_AMBIGUOUS", "REFUSED_PLAIN_AGENT", "REFUSED_NO_WORKTREE", "REFUSED_NOT_LINKED_WORKTREE", "REFUSED_BRANCH_MISMATCH", "REFUSED_NOT_OWNED", "REFUSED_NEWER_RUN",
  "REFUSED_OWNER_UNCONFIRMED",
  "REFUSED_STILL_IN_USE", "REFUSED_IN_USE_UNKNOWN", "REFUSED_NOT_RECORDED", "REFUSED_UNKNOWN",
  "NOTHING_INTERRUPTED", "LIST_INCOMPLETE", "LIST_REFRESH_FAILED", "INTERRUPTED_UNREAD", "LABEL_ISSUE", "LABEL_BRANCH", "LABEL_WORKTREE", "LABEL_FOLDER", "LABEL_STARTED_BY", "LABEL_LAST_ENTRY",
  "LABEL_GIT", "LABEL_REPORT", "LABEL_PROGRESS", "ITEM_CUT", "LABEL_NESTED_WORKTREE", "REPORT_CUT", "REPORT_UNREADABLE", "NOTHING_DISMISSED",
  "UNSUPPORTED", "DAEMON_FAILED", "DISMISS_STOPPED_REPLACED", "DISMISS_STOPPED_FAILED",
  "RESTORE_ACTION", "RESTORE_AND_CONTINUE", "RESTORE_ONLY", "START_OVER",
  "RESTORE_AND_CONTINUE_EXPLANATION", "RESTORE_ONLY_EXPLANATION",
  "RESTORE_VERDICT_RESUME", "RESTORE_UNSUPPORTED", "RESTART_NOTE_NOT_SENT",
  "SAVED_TASK_NOT_SENT", "RESTORE_NOT_RECORDED", "NOTHING_RESTORED",
  "RESTORE_WOULD", "START_OVER_WOULD", "LABEL_AGENT", "LABEL_ATTEMPT",
  "RESTORE_PR_OPEN",
] as const;

describe("the interrupted work wording", () => {
  /** Scenario: every wording constant the desktop shows is read from the Rust module and compared with the desktop's copy, word for word. */
  it("copies every Rust constant word for word", () => {
    const strings = rustStrings();
    for (const name of COPIED) {
      expect(strings.has(name), `src/interrupted_work.rs defines ${name}`).toBe(true);
      expect((words as unknown as Record<string, string>)[name], name).toBe(strings.get(name));
    }
    expect(words.MAX_SHOWN_CHARS).toBe(Number(rust.match(/pub const MAX_SHOWN_CHARS: usize = (\d+);/)?.[1]));
    /* Reviewer 4: the page size Previous reports falls back on, the daemon's. */
    expect((words as unknown as Record<string, number>).MAX_SHOWN_REPORTS).toBe(Number(rust.match(/pub const MAX_SHOWN_REPORTS: usize = (\d+);/)?.[1]));
  });

  /** Scenario: the sentences the Rust helpers build — the footer, the dismiss-all question, a progress line, a reclaim's unpushed commits — read the same from the desktop's helpers, and each fixed fragment is in the Rust source. */
  it("builds the helpers' sentences as the Rust helpers do", () => {
    expect(words.interruptedFooter(1)).toBe("1 interrupted item from before the restart");
    expect(words.interruptedFooter(3)).toBe("3 interrupted items from before the restart");
    expect(rust).toContain('"1 interrupted item from before the restart"');
    expect(rust).toContain('"{count} interrupted items from before the restart"');
    expect(words.dismissAllQuestion(3)).toBe("Dismiss all: 3 interrupted items from before the restart? [y/N]");
    expect(rust).toContain('"{DISMISS_ALL_ACTION}: {}? [y/N] "');
    expect(words.kindLabel("unit")).toBe("Unit");
    expect(words.kindLabel("issue_dispatch")).toBe("Issue dispatch");
    expect(rust).toContain('ItemKind::IssueDispatch => "Issue dispatch"');
    expect(words.progressLine({ attempts: 3, roles: ["orchestrator", "coder"], delegations: 4, delegations_done: 2 })).toBe("3 attempts; roles orchestrator, coder; 2 of 4 delegations done");
    expect(words.progressLine({ attempts: 1, delegations: 1, delegations_done: 0 })).toBe("1 attempt; 0 of 1 delegation done");
    expect(rust).toContain('"{} of {} {noun} done"');
    expect(words.keptUntil(1_795_687_200_000)).toBe("kept until 2026-11-26 10:00 UTC");
    expect(words.keptUntil(undefined)).toBe("kept, with no removal date yet");
    expect(rust).toContain('"kept, with no removal date yet"');
    expect(rust).toContain('"%Y-%m-%d %H:%M UTC"');
    expect(words.reportRangeSentence(1, 3, 8)).toBe("Reports 1 to 3 of 8.");
    expect(words.reportRangeSentence(4, 4, 8)).toBe("Report 4 of 8.");
    expect(rust).toContain('"Reports {first} to {last} of {total}."');
    expect(words.unpushedSentence(1)).toBe("1 commit on its branch is on no remote; the branch keeps it.");
    expect(words.unpushedSentence(2)).toBe("2 commits on its branch are on no remote; the branch keeps them.");
    expect(words.unpushedSentence(0)).toBeUndefined();
    expect(rust).toContain('"1 commit on its branch is on no remote; the branch keeps it."');
    expect(words.gitFactsLine({ worktree_exists: true, dirty: false, unpushed_commits: 2, branch_exists: true, pr: "open" })).toBe("worktree on disk, no uncommitted changes, 2 commits on no remote, branch kept, pull request open");
    for (const fact of ["worktree on disk", "no uncommitted changes", "commits on no remote", "branch kept", "pull request open", "pull request unknown"]) expect(rust).toContain(fact);
  });

  /** Scenario: a reclaim's and a dismissal's outcome read as the TUI words them: the verdict's sentence, a refusal's sentence, and the not-recorded removal. A verdict this app does not know shows the daemon's own words, escaped. */
  it("words outcomes as the TUI does", () => {
    expect(words.reclaimMessage({ verdict: "would-remove", message: "x" })).toBe(words.RECLAIM_WOULD_REMOVE);
    expect(words.reclaimMessage({ verdict: "kept-busy", message: "x" })).toBe(words.RECLAIM_KEPT_BUSY);
    expect(words.reclaimMessage({ verdict: "removed", message: "x", recorded: false })).toBe(words.RECLAIM_NOT_RECORDED);
    expect(words.reclaimMessage({ verdict: "refused", message: "x", refusal: { reason: "plain-agent", message: "x" } })).toBe(words.REFUSED_PLAIN_AGENT);
    expect(words.reclaimMessage({ verdict: "newer", message: "a newer ‮ daemon" })).toBe("a newer \\u{202e} daemon");
    expect(words.wouldRemove({ verdict: "would-remove", message: "" })).toBe(true);
    expect(words.wouldRemove({ verdict: "would-remove", message: "", refusal: { reason: "still-in-use", message: "" } })).toBe(false);
    expect(words.wouldRemove({ verdict: "kept-dirty", message: "" })).toBe(false);
    expect(words.dismissMessage({ dismissed: [{ id: "w" }], refused: [] })).toBe(words.DISMISSED);
    expect(words.dismissMessage({ dismissed: [], refused: [] })).toBe(words.NOTHING_DISMISSED);
    expect(words.dismissMessage({ dismissed: [], refused: [{ id: "w", refusal: { reason: "not-listed", message: "" } }] })).toBe(words.REFUSED_NOT_LISTED);
    /* Docs review R2: a dismissal refused whole on an incomplete journal read, one item's or a Dismiss all batch's. */
    expect(words.dismissMessage({ dismissed: [], refused: [], refusal: { reason: "incomplete-read", message: "x" } })).toBe(words.REFUSED_INCOMPLETE_READ);
    expect(words.dismissMessage({ dismissed: [{ id: "w" }], refused: [], stopped: { reason: "refused", refusal: "incomplete-read", not_sent: 1 } })).toBe(`${words.DISMISSED} 1 more item was not dismissed. ${words.REFUSED_INCOMPLETE_READ}`);
  });

  /** Scenario: Dismiss all over more items than one request names is sent in batches by the app's bridge; when a later batch stops — the daemon replaced, no answer, or refused — the sentence after "Dismissed." says how many more items were or may not have been dismissed and why, as the TUI and the CLI say it. */
  it("says why a dismissal sent in batches stopped, as the Rust helper does", () => {
    const done = { dismissed: [{ id: "w" }], refused: [] };
    expect(words.dismissMessage({ ...done, stopped: { reason: "replaced", not_sent: 1 } })).toBe(`${words.DISMISSED} 1 more item may not have been dismissed: ${words.DISMISS_STOPPED_REPLACED}`);
    expect(words.dismissMessage({ ...done, stopped: { reason: "failed", not_sent: 3 } })).toBe(`${words.DISMISSED} 3 more items may not have been dismissed: ${words.DISMISS_STOPPED_FAILED}`);
    expect(words.dismissMessage({ ...done, stopped: { reason: "refused", refusal: "unreadable", not_sent: 257 } })).toBe(`${words.DISMISSED} 257 more items were not dismissed. ${words.REFUSED_UNREADABLE}`);
    expect(words.dismissMessage({ ...done, stopped: { reason: "a-newer-reason", not_sent: 1 } })).toBe(`${words.DISMISSED} 1 more item was not dismissed.`);
    /* Verification finding B: a refusal in an earlier batch does not hide that a later batch stopped. */
    const refused = { dismissed: [{ id: "w" }], refused: [{ id: "x", refusal: { reason: "not-listed", message: "" } }] };
    expect(words.dismissMessage({ ...refused, stopped: { reason: "failed", not_sent: 1 } })).toBe(`${words.REFUSED_NOT_LISTED} 1 more item may not have been dismissed: ${words.DISMISS_STOPPED_FAILED}`);
    expect(words.dismissMessage(refused)).toBe(words.REFUSED_NOT_LISTED);
    expect(rust).toContain('"1 more item may not have been dismissed"');
    expect(rust).toContain('"more items may not have been dismissed"');
    expect(rust).toContain('"1 more item was not dismissed"');
    expect(rust).toContain('"more items were not dismissed"');
    expect(rust).toContain('format!("{lead}: {DISMISS_STOPPED_REPLACED}")');
    expect(rust).toContain('format!("{lead}: {DISMISS_STOPPED_FAILED}")');
  });

  /** Scenario: daemon-supplied text carrying C0 and C1 controls and bidi formatting characters is shown with each written as its escape, as the CLI and the TUI show it, and a long value is cut to 300 characters with an ellipsis. */
  it("escapes control and bidi characters as the CLI and the TUI do", () => {
    expect(words.escapeControlAndBidi("ze\u001b[31mta\u0000-li‮ve\u007f-\u008577\r\n")).toBe("ze\\u{1b}[31mta\\u{0}-li\\u{202e}ve\\u{7f}-\\u{85}77\\r\\n");
    expect(rust.length).toBeGreaterThan(0);
    for (const ch of ["‪", "‫", "‬", "‭", "‮", "⁦", "⁧", "⁨", "⁩", "‎", "‏", "؜", " ", " "]) {
      expect(words.displayValue(`a${ch}b`)).toBe(`a\\u{${ch.codePointAt(0)!.toString(16)}}b`);
    }
    const long = words.displayValue("x".repeat(400));
    expect([...long]).toHaveLength(301);
    expect(long.endsWith("…")).toBe(true);
    expect(words.reportLines("one\ttwo\r\nthree‮")).toEqual(["one two", "three\\u{202e}"]);
  });

  /** Scenario: the app's backend answers an interrupted work request with something that is not one of its replies — `{ ok: true }`, an answer without items, nothing — and the app reads it as a daemon without interrupted work, showing nothing, rather than breaking the dashboard. */
  it("reads a malformed reply as unsupported", () => {
    for (const value of [{ ok: true }, null, undefined, "x", { outcome: "answered", instance: "a" }, { outcome: "answered", instance: "a", answer: {} }, { outcome: "answered", answer: { items: [] } }]) {
      expect(words.normalizeInterruptedReply(value, words.isInterruptedList), JSON.stringify(value)).toEqual({ outcome: "unsupported" });
    }
    expect(words.normalizeInterruptedReply({ outcome: "answered", instance: "a", answer: { items: [] } }, words.isInterruptedList)).toEqual({ outcome: "answered", instance: "a", answer: { items: [] } });
    expect(words.normalizeInterruptedReply({ outcome: "answered", instance: "a", answer: { verdict: "removed" } }, words.isReclaimOutcome)).toEqual({ outcome: "unsupported" });
  });

  /** Scenario: the desktop restore question and its choices use the same words as the TUI and CLI, including the explanations under each resume choice. The sentence saying a conversation can be restored stays identical in both clients. */
  it("journal/restore/009 copies the restore question and choices verbatim", () => {
    const expected = {
      RESTORE_ACTION: "Restore",
      RESTORE_AND_CONTINUE: "Restore and continue",
      RESTORE_ONLY: "Restore only",
      START_OVER: "Start over in this worktree",
      RESTORE_AND_CONTINUE_EXPLANATION: "It checks what already happened and carries on where it stopped.",
      RESTORE_ONLY_EXPLANATION: "It comes back and waits until you tell it to continue.",
      RESTORE_VERDICT_RESUME: "Can be restored with its conversation.",
    } as const;
    for (const [name, sentence] of Object.entries(expected)) {
      expect(rustStrings().get(name), name).toBe(sentence);
      expect((words as unknown as Record<string, unknown>)[name], name).toBe(sentence);
    }
    expect(words.restoreQuestion("fix-login")).toBe("Restore fix-login with its conversation?");
    expect(rust).toMatch(/pub fn restore_question\(name: &str\) -> String\s*\{\s*format!\("Restore \{name\} with its conversation\?"\)/);
    expect(words.restoreVerdictSentence({ kind: "resume" })).toBe("Can be restored with its conversation.");
    // Shared Rust constant used by CLI/TUI; this is not a second desktop copy.
    expect(rust).toContain('"Can be restored with its conversation."');
    expect(words.startOverQuestion("scratch-pi")).toBe("Start scratch-pi over in this worktree, without its conversation?");
    expect(rust).toContain('format!("Start {name} over in this worktree, without its conversation?")');
  });

  const RESTORE_REASON_WORDS = [
    ["AgentCannotResume", "agent-cannot-resume", "The agent cannot be restored with its conversation yet."],
    ["NoSessionId", "no-session-id", "The deck does not know which conversation the agent was in."],
    ["SessionIdInvalid", "session-id-invalid", "The conversation id the deck holds for the agent is not one it can resume."],
    ["NoLaunchRecord", "no-launch-record", "The deck has no record of how the agent was started."],
    ["AmbiguousRecord", "ambiguous-record", "The deck's records do not say for certain how the agent was started."],
    ["FolderMoved", "folder-moved", "The agent's folder was moved, is gone, or others can write to it."],
    ["FolderUnknown", "folder-unknown", "The deck does not know which folder the agent ran in."],
    ["OrchestrationLater", "orchestration-later", "Orchestrations come back in a later version."],
    ["ClearWorker", "clear-worker", "This worker starts fresh for each task and comes back with its orchestration."],
    ["DefaultShell", "default-shell", "The pane ran a shell, not an agent, so there is no conversation to restore."],
    ["IncompleteRead", "incomplete-read", "The deck could not read its whole work journal or all its records of how agents were started."],
    ["NotInterrupted", "not-interrupted", "The restart did not stop this agent."],
    ["NotResumed", "not-resumed", "The deck tried to restore the agent's conversation and could not."],
    ["OldProcessUnknown", "old-process-unknown", "An older version of the deck started this agent, so the deck cannot tell whether its old process is gone."],
    ["Restored", "restored", "The deck already brought this agent back."],
    ["Unknown", "unknown", "The agent cannot be restored."],
  ] as const;

  /** Scenario: details and the restore question explain why each recorded agent cannot resume, with the same sentence as Rust. A reason from a newer daemon uses the shared unknown-reason sentence. */
  it("journal/restore/009 copies each recorded restore reason word for word", () => {
    const reasonBody = rust.split("pub fn restore_reason_sentence(")[1]?.split("\n}\n")[0] ?? "";
    const sourceReasons = [...reasonBody.matchAll(/RestoreReason::([A-Za-z]+)\s*=>\s*\{?\s*"((?:[^"\\]|\\.)*)"/g)]
      .map((match) => [match[1], JSON.parse(`"${match[2]}"`) as string]);
    expect(sourceReasons).toHaveLength(RESTORE_REASON_WORDS.length);
    for (const [variant, code, sentence] of RESTORE_REASON_WORDS) {
      expect(sourceReasons.find(([name]) => name === variant)?.[1], variant).toBe(sentence);
      expect(words.restoreReasonSentence(code), code).toBe(sentence);
      expect(words.restoreVerdictSentence({ kind: "start_over", reason: code }), code)
        .toBe(`${sentence} It can be started again without it: Start over in this worktree.`);
      expect(words.restoreVerdictSentence({ kind: "none", reason: code }), code).toBe(sentence);
    }
    expect(words.restoreReasonSentence("a-newer-reason")).toBe("The agent cannot be restored.");
    expect(words.restoreVerdictSentence({ kind: "a-newer-verdict" })).toBe("The agent cannot be restored.");
  });

  /** Scenario: an item whose first agent the deck already brought back and whose second is still interrupted asks about the second, as the TUI does, and says of the first that it is back. */
  it("journal/restore/009 asks about the agent still interrupted, not the one brought back", () => {
    const agent = (attempt: string, restore: RestoreVerdict) => ({ attempt, name: attempt, restore }) as ItemAgent;
    const back = agent("restore-back/1", { kind: "none", reason: "restored" });
    const waiting = agent("restore-waiting/2", { kind: "resume" });
    const item = { id: "w-partial", agents: [back, waiting] } as unknown as InterruptedItem;
    expect(words.restoreAgent(item)).toBe(waiting);
    expect(words.offersChoice(back.restore)).toBe(false);
    expect(words.restoreVerdictSentence(back.restore)).toBe("The deck already brought this agent back.");
  });

  /** Scenario: a refused restore or Start over shows the same explanation as Rust for the recorded refusal code, including old processes, unsafe tasks and an unsuccessful conversation resume. A newer refusal uses the shared unknown-refusal sentence. */
  it("journal/restore/009 copies the action refusal sentences word for word", () => {
    const body = rust.split("pub fn restore_refusal_sentence(")[1]?.split("\n}\n")[0] ?? "";
    const arms = [...body.matchAll(/RestoreRefusal::([A-Za-z]+)\s*=>\s*\{?\s*(?:"((?:[^"\\]|\\.)*)"|([A-Z_]+))/g)];
    // 28 + the 8 refusals of the full review and audit (bad-selection,
    // command-reopens, not-fresh, not-stopped, pr-open, directory-in-use,
    // note-refused, note-not-delivered) + not-recorded (round 3, audit B1)
    // + restore-busy (round 5) + task-not-delivered (PR #2497 review).
    expect(arms).toHaveLength(39);
    for (const [, variant, literal, constant] of arms) {
      const code = variant.replace(/([a-z0-9])([A-Z])/g, "$1-$2").toLowerCase();
      const sentence = literal === undefined ? rustStrings().get(constant) : JSON.parse(`"${literal}"`) as string;
      expect(sentence, variant).toBeTypeOf("string");
      expect(words.restoreRefusalSentence(code), code).toBe(sentence);
    }
    expect(words.restoreRefusalSentence("a-newer-refusal")).toBe(words.REFUSED_UNKNOWN);
  });
});
