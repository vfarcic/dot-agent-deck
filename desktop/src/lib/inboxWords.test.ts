import { describe, expect, it } from "vitest";
import type { AgentInbox, AgentSession, WakeHold } from "../types";
import cardCases from "../../../tests/fixtures/inbox-card-cases.json";
import { deckAttentionText, heldAgents, holdDetailText, holdDetailWords, inboxAttentionWords, inboxBadge, inboxNotice, inputHeldText, INBOX_WORDS, MAX_HOLD_DETAIL_CHARS } from "./inboxWords";

function inbox(queued: number, wakeHold?: WakeHold): AgentInbox {
  return { queued, fetched: 0, undeliverable: 0, wake: "doorbell", ...(wakeHold ? { wakeHold } : {}) };
}

function agent(role: string, isStartRole: boolean, agentInbox?: AgentInbox): AgentSession {
  return { id: role, role, displayName: role, tab: { kind: "dashboard" }, isStartRole, ...(agentInbox ? { inbox: agentInbox } : {}) } as AgentSession;
}

// PRD #1666 part 3, round 5-wake (Q6): the header's attention item. The TUI's
// `deck_attention_text` tests in `src/inbox_display.rs` pin the same strings.
describe("deck attention words", () => {
  /// Scenario: The header names the held agents, start roles first and then by name, in the strings the TUI pins too; past three it counts them, and an agent with mail but no hold is neither named nor counted.
  it("names the held agents, start roles first, and counts past three", () => {
    expect(deckAttentionText([
      agent("Coder", false, inbox(1, "not_taken")),
      agent("Captain", true, inbox(1, "possible_draft")),
      agent("Quiet", false, inbox(3)),
      agent("archivist", false, inbox(0, "uncertain_input")),
    ])).toBe("✉ held: Captain, archivist, Coder");
    expect(deckAttentionText([agent("Captain", true, inbox(1, "paste_open"))])).toBe("✉ held: Captain");
    expect(deckAttentionText(["A", "B", "C", "D"].map((name) => agent(name, false, inbox(1, "cannot_type"))))).toBe("✉ 4 held");
    expect(deckAttentionText([agent("Captain", true, inbox(2))])).toBeUndefined();
  });

  /// Scenario: Two orchestration runs each hold their `coder`: the header names each after its run's name, as their tiles are titled, so the two read apart.
  it("names a run's agents after the run, so two runs' roles read apart", () => {
    const coder = (run: string) => ({ ...agent("coder", false, inbox(1, "cannot_type")), id: `${run}-coder`, tab: { kind: "orchestration", name: run, roleIndex: 1, roleName: "coder", isStartRole: false } }) as AgentSession;
    expect(deckAttentionText([coder("run-b"), coder("run-a")])).toBe("✉ held: run-a / coder, run-b / coder");
  });

  /// Scenario: The agents the header counts are exactly those with attention words, the same predicate the held marker on a tile reads.
  it("lists exactly the agents a marker is drawn for", () => {
    const agents = [agent("A", false, inbox(1, "unknown")), agent("B", false, inbox(0, "possible_draft")), agent("C", false, inbox(0, "dialog_during_ring"))];
    expect(heldAgents(agents).map((entry) => entry.name)).toEqual(agents.filter((each) => inboxAttentionWords(each.inbox) !== undefined).map((each) => each.role));
    expect(heldAgents(agents).map((entry) => entry.words)).toEqual(["waiting to wake: send it a prompt", INBOX_WORDS.wakeHoldsAlone.dialog_during_ring]);
  });

  /// Scenario: An agent's attention words are its hold's words while mail waits, the stall's for an unknown hold, the standalone warnings with nothing waiting, and nothing otherwise.
  it("gives each hold the words its card uses", () => {
    expect(inboxAttentionWords(inbox(2, "cannot_type"))).toBe(INBOX_WORDS.wakeHolds.cannot_type);
    expect(inboxAttentionWords(inbox(1, "unknown"))).toBe("waiting to wake: send it a prompt");
    expect(inboxAttentionWords(inbox(0, "dialog_during_ring"))).toBe(INBOX_WORDS.wakeHoldsAlone.dialog_during_ring);
    expect(inboxAttentionWords(inbox(0, "possible_draft"))).toBeUndefined();
    expect(inboxAttentionWords(inbox(0, "unknown"))).toBeUndefined();
    expect(inboxAttentionWords(inbox(3))).toBeUndefined();
    expect(inboxAttentionWords(undefined)).toBeUndefined();
  });
});

// PRD #1666 part 3 (Q35): a cannot-check detail in words. The TUI's
// `a_hold_detail_names_its_cause_path_and_remedy` pins the same strings.
describe("cannot-check detail words", () => {
  const relocate = "Point TMPDIR (or CODEX_HOME) at a directory on ext4, XFS, Btrfs or tmpfs (Linux), or APFS or HFS+ (macOS), owned by you, with no group or other write permission on it or on any directory above it other than a sticky directory root owns, such as /tmp.";

  /// Scenario: Word a cannot-check detail for each kind of cause, with and without a path or filesystem, and an unknown cause; the sentences match the TUI's to the character.
  it("names the cause, the path and the remedy as the TUI does", () => {
    expect(holdDetailWords({ cause: "filesystem", pathComponent: "/mnt/nfs/tmp", filesystem: "nfs" }))
      .toBe(`waiting: the deck can't check this agent's input box: \u201c/mnt/nfs/tmp\u201d is on nfs, which the check doesn't support. ${relocate}`);
    expect(holdDetailWords({ cause: "writable_directory" }))
      .toBe("waiting: the deck can't check this agent's input box: others can write to a path the deck didn't name, a directory you own. Remove the write permission others have on that directory, with chmod g-w (and chmod o-w if others can write too) on it.");
    expect(holdDetailWords({ cause: "filesystem", pathComponent: "/x" }))
      .toBe(`waiting: the deck can't check this agent's input box: \u201c/x\u201d is on a filesystem the deck didn't name, which the check doesn't support. ${relocate}`);
    expect(holdDetailWords({ cause: "io", pathComponent: "/a" }))
      .toBe("waiting: the deck can't check this agent's input box: the deck couldn't read \u201c/a\u201d. Check that you can read it, or point TMPDIR (or CODEX_HOME) at another directory.");
    expect(holdDetailWords({ cause: "unknown", pathComponent: "/a" })).toBeUndefined();
  });

  /// Scenario: A detail whose path names a placeholder and carries controls and bidi characters is shown as it is, escaped as the TUI escapes it, and bounded to 256 characters.
  it("shows the detail's text escaped and bounded, and never fills it", () => {
    expect(holdDetailWords({ cause: "filesystem", pathComponent: "/a/{filesystem}\n\u202e", filesystem: "{path}\u001b" }))
      .toContain("\u201c/a/{filesystem}\\n\\u{202e}\u201d is on {path}\\u{1b}, which");
    expect(holdDetailText("a\tb\rc\u0000d\u0085e\u061cf\u2066g")).toBe("a\\tb\\rc\\u{0}d\\u{85}e\\u{61c}f\\u{2066}g");
    expect(Array.from(holdDetailText("é".repeat(MAX_HOLD_DETAIL_CHARS + 10)))).toHaveLength(MAX_HOLD_DETAIL_CHARS);
  });

  /// Scenario: A cannot-check card with a detail it understands names the cause, one it does not keeps the generic words, a detail beside another hold says nothing, and a hold with no words keeps the stall's words on the badge.
  it("puts the detail on the card's notice row", () => {
    const base = { queued: 1, fetched: 0, undeliverable: 0, wake: "doorbell" };
    expect(inboxNotice({ ...base, wakeHold: "cannot_check", holdDetail: { cause: "io", pathComponent: "/a" } }))
      .toBe(`${INBOX_WORDS.glyph}1 ${holdDetailWords({ cause: "io", pathComponent: "/a" })}`);
    expect(inboxNotice({ ...base, wakeHold: "cannot_check", holdDetail: { cause: "unknown", pathComponent: "/a" } }))
      .toBe(`${INBOX_WORDS.glyph}1 ${INBOX_WORDS.wakeHolds.cannot_check}`);
    expect(inboxNotice({ ...base, wakeHold: "needs_fetch", holdDetail: { cause: "io", pathComponent: "/a" } }))
      .toBe(`${INBOX_WORDS.glyph}1 ${INBOX_WORDS.wakeHolds.needs_fetch}`);
    expect(inboxBadge({ ...base, wakeHold: "unknown" })).toBe(`${INBOX_WORDS.glyph}1 ${INBOX_WORDS.wakeStalled}: ${INBOX_WORDS.wakeStalledHint}`);
    expect(inboxBadge({ ...base, wakeHold: "cannot_check" })).toBe(`${INBOX_WORDS.glyph}1`);
    expect(inputHeldText("queue_full")).toBe(INBOX_WORDS.inputHeld.queue_full);
    expect(inputHeldText(undefined)).toBeUndefined();
  });
});

// PRD #1666 part 3 (merge review U2): the card's badge and notice row for each
// case of `tests/fixtures/inbox-card-cases.json`, which the TUI's
// `card_cases_match_the_shared_fixture` reads too (CLAUDE.md rule 22).
describe("card cases shared with the TUI", () => {
  /// Scenario: For each case in the shared fixture (a held doorbell with and without the daemon's agent notice, an empty notice, a stalled hold), the badge and the notice row give the strings the TUI's card gives.
  it.each(cardCases.map((c) => [c.name, c] as const))("%s", (_name, c) => {
    const agentInbox: AgentInbox = {
      queued: c.pending, fetched: 0, undeliverable: 0, wake: "doorbell",
      wakeHold: c.hold as WakeHold,
      ...("stalled" in c ? { wakeStalled: c.stalled } : {}),
      ...("agentNotice" in c ? { agentNotice: c.agentNotice } : {}),
    };
    expect(inboxBadge(agentInbox)).toBe(c.line);
    expect(inboxNotice(agentInbox)).toBe(c.notice);
  });
});
