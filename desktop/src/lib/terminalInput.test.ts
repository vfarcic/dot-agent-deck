import { describe, expect, it } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { sendResultReason } from "../types";
import type { AgentSession, SendResult } from "../types";
import { agentStatusInputReason, terminalInputState } from "./terminalInput";

/** A real fixture agent with only the two fields this derivation reads moved. */
function pane(writeLease: AgentSession["writeLease"], status: AgentSession["status"] = "running"): AgentSession {
  return { ...createFixtureSnapshot("connected").agents[0], writeLease, status };
}

describe("terminalInputState", () => {
  /**
   * Scenario: a running agent whose daemon-reported lease says the pane is
   * live. The terminal accepts input, claims the delivered state, and says
   * nothing — an input that works needs no sentence beside it.
   */
  it("accepts input on a live pane and shows no notice", () => {
    expect(terminalInputState(pane("write"))).toEqual({ state: "applied", readOnly: false, tone: "open" });
  });

  /**
   * Scenario: the daemon declared no `live_target` at all, so the desktop crate
   * omits the key and the bridge reads it as `unknown`. That is the LEGACY LIVE
   * default — every native PTY historically declared none — so the input stays
   * open rather than being blocked on a missing field.
   */
  it("treats an unknown lease as writable, because absence is the legacy live default", () => {
    expect(terminalInputState(pane("unknown"))).toEqual({ state: "applied", readOnly: false, tone: "open" });
  });

  /**
   * Scenario: the two conditions the snapshot alone proves. Each disables the
   * terminal and states the reason in the vocabulary `sendResultReason` already
   * owns, rather than in a second set of strings that could drift from it.
   */
  it.each([
    ["read", "history-only"],
    ["none", "no-live-target"],
  ] satisfies [AgentSession["writeLease"], SendResult][])(
    "blocks a %s lease as %s with the shared reason",
    (writeLease, state) => {
      expect(terminalInputState(pane(writeLease))).toEqual({
        state,
        readOnly: true,
        tone: "blocked",
        notice: `Terminal input unavailable — ${sendResultReason(state)}.`,
      });
    },
  );

  /**
   * Scenario: a pane that reads perfectly writable returns `wrong-session` from
   * a guarded send. That is the whole reason this derivation is hybrid — after
   * a rollover the lease says `write` precisely when a send would fail — so the
   * returned verdict has to be able to block a lease that says otherwise.
   */
  it("blocks on a wrong-session verdict even though the lease says write", () => {
    expect(terminalInputState(pane("write"), "wrong-session")).toEqual({
      state: "wrong-session",
      readOnly: true,
      tone: "blocked",
      notice: "Terminal input unavailable — the pane handle no longer maps to that agent's session.",
    });
  });

  /**
   * Scenario: the three verdicts that mean delivery was not CONFIRMED rather
   * than that the pane is dead. They warn and leave the input working: the
   * daemon flattens a refusal caused by the user typing into the pane into
   * `stale`, so disabling on it would shut the input precisely because somebody
   * was using it.
   */
  it.each(["stale", "ambiguous", "unknown"] satisfies SendResult[])(
    "warns without disabling on %s",
    (verdict) => {
      expect(terminalInputState(pane("write"), verdict)).toEqual({
        state: verdict,
        readOnly: false,
        tone: "unconfirmed",
        notice: `Delivery was not confirmed — ${sendResultReason(verdict)}.`,
      });
    },
  );

  /**
   * Scenario: a delivered verdict is not a condition at all — the pane is in
   * the same state it would be in had nothing been sent.
   */
  it.each(["applied", "queued"] satisfies SendResult[])("leaves the input open after a delivered %s", (verdict) => {
    expect(terminalInputState(pane("write"), verdict)).toEqual({ state: "applied", readOnly: false, tone: "open" });
  });

  /**
   * Scenario: the three agent statuses that had no work in progress to type at
   * before this change, and still do not. The terminal is disabled and carries
   * the status sentence, and no `data-input-state` is claimed — nothing here
   * says anything about the pane's lease.
   */
  it.each([
    ["queued", "This agent has not started yet."],
    ["passed", "This agent has finished its work."],
    ["stopped", "This agent is stopped."],
  ] satisfies [AgentSession["status"], string][])("blocks a %s agent on the status gate alone", (status, notice) => {
    expect(terminalInputState(pane("write", status))).toEqual({ state: undefined, readOnly: true, tone: "blocked", notice });
    expect(agentStatusInputReason(pane("write", status))).toBe(notice);
  });

  /**
   * Scenario: an agent that is both finished and paneless. The pane's own fact
   * wins, because "the agent has no live pane" is what explains why typing
   * cannot work — which is what #1042 asked the terminal to say.
   */
  it("prefers the pane's reason over the status gate when both apply", () => {
    expect(terminalInputState(pane("read", "passed"))).toMatchObject({ state: "history-only", readOnly: true });
  });

  /**
   * Scenario: a running agent nothing has been submitted to. `waiting` and
   * `failed` are not gate statuses — an agent that failed a check still owns a
   * live pane the operator may want to talk to.
   */
  it.each(["running", "waiting", "failed"] satisfies AgentSession["status"][])("leaves a %s agent's input open", (status) => {
    expect(terminalInputState(pane("write", status))).toMatchObject({ readOnly: false });
  });
});
