/**
 * Unit tests for the pure orchestrator logic of the dot-agent-deck Pi
 * extension (PRD #201, test-plan rows 8 & 9).
 *
 * These target the import-free `src/orchestrator.ts`, so they run with no
 * running `pi` and no Pi toolchain installed — just Node's built-in test
 * runner via `tsx`. Run: `npm test` (inside pi-extension/).
 *
 *   ROW 8 — delegate / work-done / agent-event build the correct
 *           `dot-agent-deck ...` argv, and error paths (blank/missing args,
 *           non-zero exit, missing binary) produce clear errors.
 *   ROW 9 — the Pi-event → state mapping produces exactly running/waiting/
 *           finished for the mapped events and ignores everything else, so no
 *           bogus `--type` is ever emitted.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import {
	AGENT_STATES,
	buildAgentEventArgv,
	buildDelegateArgv,
	buildWorkDoneArgv,
	DECK_BIN,
	execFailureMessage,
	isAgentState,
	piEventToAgentState,
	spawnFailureMessage,
	STATUS_EVENTS,
} from "../src/orchestrator.ts";

// ---------------------------------------------------------------------------
// ROW 8 — invocation building
// ---------------------------------------------------------------------------

describe("row 8: delegate argv", () => {
	test("builds argv for a single role", () => {
		assert.deepEqual(buildDelegateArgv("coder", "fix the login bug"), [
			"delegate",
			"--to",
			"coder",
			"--task",
			"fix the login bug",
		]);
	});

	test("repeats --to for multiple roles and preserves order", () => {
		assert.deepEqual(buildDelegateArgv(["coder", "tester"], "ship it"), [
			"delegate",
			"--to",
			"coder",
			"--to",
			"tester",
			"--task",
			"ship it",
		]);
	});

	test("trims roles and drops blank ones", () => {
		assert.deepEqual(buildDelegateArgv(["  coder  ", "", "  "], "task"), [
			"delegate",
			"--to",
			"coder",
			"--task",
			"task",
		]);
	});

	test("throws a clear error when no usable role is given", () => {
		assert.throws(() => buildDelegateArgv([], "task"), /at least one non-empty --to/);
		assert.throws(() => buildDelegateArgv("   ", "task"), /at least one non-empty --to/);
	});

	test("throws a clear error when the task is blank", () => {
		assert.throws(() => buildDelegateArgv("coder", ""), /delegate task must be a non-empty string/);
		assert.throws(() => buildDelegateArgv("coder", "   "), /delegate task must be a non-empty string/);
	});
});

describe("row 8: work-done argv", () => {
	test("builds argv without --done by default", () => {
		assert.deepEqual(buildWorkDoneArgv("added tests"), ["work-done", "--task", "added tests"]);
	});

	test("appends --done when requested", () => {
		assert.deepEqual(buildWorkDoneArgv("all milestones complete", true), [
			"work-done",
			"--task",
			"all milestones complete",
			"--done",
		]);
	});

	test("omits --done when explicitly false", () => {
		assert.deepEqual(buildWorkDoneArgv("summary", false), ["work-done", "--task", "summary"]);
	});

	test("throws a clear error when the summary is blank", () => {
		assert.throws(() => buildWorkDoneArgv(""), /work-done summary must be a non-empty string/);
		assert.throws(() => buildWorkDoneArgv("   "), /work-done summary must be a non-empty string/);
	});
});

describe("row 8: agent-event argv", () => {
	test("builds argv for each canonical state", () => {
		assert.deepEqual(buildAgentEventArgv("running"), ["agent-event", "--type", "running"]);
		assert.deepEqual(buildAgentEventArgv("waiting"), ["agent-event", "--type", "waiting"]);
		assert.deepEqual(buildAgentEventArgv("finished"), ["agent-event", "--type", "finished"]);
	});

	test("throws a clear error on a non-canonical state, listing the allowed ones", () => {
		assert.throws(() => buildAgentEventArgv("idle"), /unknown state "idle".*running, waiting, finished/s);
		assert.throws(() => buildAgentEventArgv("Running"), /unknown state "Running"/);
		assert.throws(() => buildAgentEventArgv(""), /unknown state ""/);
	});
});

describe("row 8: exec error classification", () => {
	test("execFailureMessage returns null on success (exit 0)", () => {
		assert.equal(execFailureMessage(["delegate", "--to", "coder", "--task", "x"], { code: 0 }), null);
	});

	test("execFailureMessage reports the command, exit code, and stderr on failure", () => {
		const msg = execFailureMessage(["work-done", "--task", "x"], {
			code: 1,
			stderr: "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.",
		});
		assert.match(msg ?? "", /`dot-agent-deck work-done --task x`/);
		assert.match(msg ?? "", /exit code 1/);
		assert.match(msg ?? "", /DOT_AGENT_DECK_PANE_ID/);
	});

	test("execFailureMessage falls back to stdout when stderr is empty", () => {
		const msg = execFailureMessage(["agent-event", "--type", "running"], {
			code: 2,
			stderr: "   ",
			stdout: "boom on stdout",
		});
		assert.match(msg ?? "", /boom on stdout/);
	});

	test("spawnFailureMessage adds a PATH hint for ENOENT", () => {
		const msg = spawnFailureMessage(["delegate", "--to", "coder", "--task", "x"], new Error("spawn dot-agent-deck ENOENT"));
		assert.match(msg, /on PATH\?/);
		assert.match(msg, new RegExp(DECK_BIN));
	});

	test("spawnFailureMessage handles non-ENOENT and non-Error causes", () => {
		assert.match(spawnFailureMessage(["work-done", "--task", "x"], new Error("EACCES")), /EACCES/);
		assert.doesNotMatch(spawnFailureMessage(["work-done", "--task", "x"], new Error("EACCES")), /on PATH\?/);
		assert.match(spawnFailureMessage(["work-done", "--task", "x"], "weird"), /weird/);
	});
});

// ---------------------------------------------------------------------------
// ROW 9 — event bus → state mapping
// ---------------------------------------------------------------------------

describe("row 9: Pi event → agent state mapping", () => {
	test("maps the four subscribed lifecycle events to canonical states", () => {
		assert.equal(piEventToAgentState("session_start"), "waiting");
		assert.equal(piEventToAgentState("agent_start"), "running");
		assert.equal(piEventToAgentState("agent_settled"), "waiting");
		assert.equal(piEventToAgentState("session_shutdown"), "finished");
	});

	test("returns null for unmapped events so no agent-event is emitted", () => {
		// agent_end is intentionally unmapped (Pi may still auto-retry/compact).
		assert.equal(piEventToAgentState("agent_end"), null);
		assert.equal(piEventToAgentState("turn_start"), null);
		assert.equal(piEventToAgentState("turn_end"), null);
		assert.equal(piEventToAgentState("message_update"), null);
		assert.equal(piEventToAgentState("tool_call"), null);
		assert.equal(piEventToAgentState("model_select"), null);
		assert.equal(piEventToAgentState(""), null);
		assert.equal(piEventToAgentState("running"), null); // a state name, not an event name
	});

	test("every subscribed STATUS_EVENT maps to a canonical state", () => {
		for (const event of STATUS_EVENTS) {
			const state = piEventToAgentState(event);
			assert.notEqual(state, null, `${event} should map to a state`);
			assert.ok(isAgentState(state as string), `${event} → ${state} must be canonical`);
		}
	});

	test("mapped states only ever produce a canonical --type argv (no bogus type)", () => {
		for (const event of STATUS_EVENTS) {
			const state = piEventToAgentState(event);
			// Must not throw, and must yield an allowed --type.
			const argv = buildAgentEventArgv(state as string);
			assert.equal(argv[0], "agent-event");
			assert.equal(argv[1], "--type");
			assert.ok((AGENT_STATES as readonly string[]).includes(argv[2]));
		}
	});

	test("isAgentState accepts only the three canonical strings", () => {
		assert.deepEqual([...AGENT_STATES], ["running", "waiting", "finished"]);
		assert.ok(isAgentState("running"));
		assert.ok(!isAgentState("idle"));
		assert.ok(!isAgentState("RUNNING"));
	});
});
