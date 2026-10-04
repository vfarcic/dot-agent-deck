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
	AGENT_EVENT_TYPES,
	AGENT_STATES,
	buildAgentEventArgv,
	DETAIL_EVENTS,
	buildDelegateArgv,
	buildGetSeedArgv,
	buildWorkDoneArgv,
	createSerialQueue,
	DECK_BIN,
	DECK_EXE_ENV,
	execFailureMessage,
	isAgentState,
	isUnsupportedFlagFailure,
	legacyAgentEventArgv,
	MAX_PROMPT_CHARS,
	piEventReport,
	piEventToAgentState,
	piToolDetail,
	resolveDeckBin,
	SEED_DELIVER_AS,
	seedToDeliver,
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

	test("throws a clear error on a non-canonical type, listing the allowed ones", () => {
		assert.throws(
			() => buildAgentEventArgv("idle"),
			/unknown type "idle".*running, waiting, finished, prompt, tool-start, tool-end/s,
		);
		assert.throws(() => buildAgentEventArgv("Running"), /unknown type "Running"/);
		assert.throws(() => buildAgentEventArgv("tool_start"), /unknown type "tool_start"/);
		assert.throws(() => buildAgentEventArgv(""), /unknown type ""/);
	});

	// Issue #622: the card detail rides the same verb as optional flags.
	test("appends each supplied detail as its own flag, in a fixed order", () => {
		assert.deepEqual(
			buildAgentEventArgv("tool-start", {
				cwd: "/work/repo",
				toolName: "bash",
				toolDetail: "touch x.txt",
			}),
			["agent-event", "--type", "tool-start", "--cwd=/work/repo", "--tool-name=bash", "--tool-detail=touch x.txt"],
		);
		assert.deepEqual(buildAgentEventArgv("prompt", { cwd: "/w", prompt: "fix it" }), [
			"agent-event",
			"--type",
			"prompt",
			"--cwd=/w",
			"--prompt=fix it",
		]);
	});

	test("a value starting with a dash stays inside its own flag", () => {
		assert.deepEqual(buildAgentEventArgv("prompt", { prompt: "--help me" }), [
			"agent-event",
			"--type",
			"prompt",
			"--prompt=--help me",
		]);
		assert.deepEqual(buildAgentEventArgv("tool-start", { toolName: "bash", toolDetail: "-rf build" }), [
			"agent-event",
			"--type",
			"tool-start",
			"--tool-name=bash",
			"--tool-detail=-rf build",
		]);
	});

	test("omits blank or missing details rather than sending empty flags", () => {
		assert.deepEqual(buildAgentEventArgv("running", { cwd: "  ", prompt: "", toolName: undefined }), [
			"agent-event",
			"--type",
			"running",
		]);
	});

	test("a lifecycle report keeps its exact legacy argv when no detail is given", () => {
		assert.deepEqual(buildAgentEventArgv("finished", {}), ["agent-event", "--type", "finished"]);
	});
});

describe("issue #622: Pi tool detail", () => {
	test("bash shows the first line of its command", () => {
		assert.equal(piToolDetail("bash", { command: "touch a.txt\necho done", timeout: 5 }), "touch a.txt");
	});

	test("bash clips a long command to 120 characters", () => {
		assert.equal(piToolDetail("bash", { command: "x".repeat(300) })?.length, 120);
	});

	test("file tools show their path, search tools their pattern", () => {
		assert.equal(piToolDetail("read", { path: "src/a.rs", offset: 3 }), "src/a.rs");
		assert.equal(piToolDetail("write", { content: "body first", path: "out.txt" }), "out.txt");
		assert.equal(piToolDetail("edit", { path: "b.ts", edits: [] }), "b.ts");
		assert.equal(piToolDetail("ls", { path: "docs" }), "docs");
		assert.equal(piToolDetail("grep", { path: "src", pattern: "fn main" }), "fn main");
		assert.equal(piToolDetail("find", { pattern: "*.md" }), "*.md");
	});

	test("an unknown tool falls back to its first string argument, clipped to 80", () => {
		assert.equal(piToolDetail("delegate", { role: "coder", task: "t" }), "coder");
		assert.equal(piToolDetail("custom", { n: 1, s: "y".repeat(200) })?.length, 80);
	});

	test("arguments that carry nothing to show yield no detail", () => {
		assert.equal(piToolDetail("ls", {}), undefined);
		assert.equal(piToolDetail("bash", null), undefined);
		assert.equal(piToolDetail("bash", "touch x"), undefined);
		assert.equal(piToolDetail("custom", { n: 1 }), undefined);
	});

	test("clipping never splits a surrogate pair", () => {
		const detail = piToolDetail("bash", { command: "😀".repeat(200) }) as string;
		assert.equal(Array.from(detail).length, 120);
		assert.ok(!/[\uD800-\uDBFF]$/.test(detail));
	});
});

describe("issue #622: Pi event → agent-event report", () => {
	test("a lifecycle event reports its state plus the session cwd", () => {
		assert.deepEqual(piEventReport("agent_start", {}, "/w"), { type: "running", detail: { cwd: "/w" } });
		assert.deepEqual(piEventReport("session_start", {}, "/w"), { type: "finished", detail: { cwd: "/w" } });
	});

	test("before_agent_start reports the submitted prompt", () => {
		assert.deepEqual(piEventReport("before_agent_start", { prompt: "list the files" }, "/w"), {
			type: "prompt",
			detail: { cwd: "/w", prompt: "list the files" },
		});
	});

	test("a prompt is clipped before it reaches argv", () => {
		const report = piEventReport("before_agent_start", { prompt: "p".repeat(MAX_PROMPT_CHARS + 50) }, "/w");
		assert.equal(report?.detail.prompt?.length, MAX_PROMPT_CHARS);
	});

	test("a blank or missing prompt reports nothing (agent_start still reports the turn)", () => {
		assert.equal(piEventReport("before_agent_start", { prompt: "   " }, "/w"), null);
		assert.equal(piEventReport("before_agent_start", {}, "/w"), null);
		assert.equal(piEventReport("before_agent_start", undefined, "/w"), null);
	});

	test("tool_execution_start reports the tool and its detail", () => {
		assert.deepEqual(
			piEventReport("tool_execution_start", { toolCallId: "c1", toolName: "bash", args: { command: "ls -la" } }, "/w"),
			{ type: "tool-start", detail: { cwd: "/w", toolName: "bash", toolDetail: "ls -la" } },
		);
	});

	test("tool_execution_end reports the tool finishing, failed or not", () => {
		assert.deepEqual(
			piEventReport("tool_execution_end", { toolCallId: "c1", toolName: "bash", result: {}, isError: true }, "/w"),
			{ type: "tool-end", detail: { cwd: "/w", toolName: "bash" } },
		);
	});

	test("a missing cwd is simply left off", () => {
		assert.deepEqual(piEventReport("agent_settled", {}, undefined), { type: "finished", detail: {} });
	});

	test("unsubscribed events report nothing", () => {
		assert.equal(piEventReport("agent_end", {}, "/w"), null);
		assert.equal(piEventReport("tool_execution_update", { toolName: "bash" }, "/w"), null);
		assert.equal(piEventReport("tool_call", { toolName: "bash" }, "/w"), null);
	});

	test("every subscribed event yields a report whose argv the CLI accepts", () => {
		const payloads: Record<string, unknown> = {
			before_agent_start: { prompt: "go" },
			tool_execution_start: { toolName: "bash", args: { command: "ls" } },
			tool_execution_end: { toolName: "bash" },
		};
		for (const event of [...STATUS_EVENTS, ...DETAIL_EVENTS]) {
			const report = piEventReport(event, payloads[event] ?? {}, "/w");
			assert.notEqual(report, null, `${event} should report`);
			const argv = buildAgentEventArgv(report!.type, report!.detail);
			assert.ok((AGENT_EVENT_TYPES as readonly string[]).includes(argv[2]));
		}
	});
});

describe("issue #1385: which deck binary the extension shells", () => {
	test("uses the absolute path the spawning deck exported, verbatim", () => {
		assert.equal(DECK_EXE_ENV, "DOT_AGENT_DECK_EXE");
		assert.equal(
			resolveDeckBin({ DOT_AGENT_DECK_EXE: "/home/me/.local/bin/dot-agent-deck" }),
			"/home/me/.local/bin/dot-agent-deck",
		);
		// argv exec, no shell: a path with spaces is passed unquoted.
		assert.equal(
			resolveDeckBin({ DOT_AGENT_DECK_EXE: "/opt/my tools/dot-agent-deck" }),
			"/opt/my tools/dot-agent-deck",
		);
	});

	test("falls back to the bare name when an older deck sets nothing", () => {
		assert.equal(resolveDeckBin({}), DECK_BIN);
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_EXE: "" }), DECK_BIN);
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_EXE: "   " }), DECK_BIN);
		assert.equal(DECK_BIN, "dot-agent-deck");
	});

	test("failure messages name the binary actually shelled", () => {
		const bin = "/home/me/.local/bin/dot-agent-deck";
		const msg = execFailureMessage(["work-done", "--task", "x"], { code: 1, stderr: "nope" }, bin);
		assert.match(msg ?? "", /`\/home\/me\/\.local\/bin\/dot-agent-deck work-done --task x`/);
		const spawn = spawnFailureMessage(["get-seed"], new Error(`spawn ${bin} ENOENT`), bin);
		assert.doesNotMatch(spawn, /on PATH\?/, "a supplied path that is missing is not a PATH miss");
		assert.match(spawn, /still exist\?/);
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
// PRD #201 native prompt delivery — get-seed argv + seed decision
// ---------------------------------------------------------------------------

describe("native seed delivery: get-seed argv", () => {
	test("builds the read-only get-seed argv", () => {
		assert.deepEqual(buildGetSeedArgv(), ["get-seed"]);
	});
});

describe("native seed delivery: seedToDeliver", () => {
	test("returns the trimmed seed when the CLI printed a real one", () => {
		assert.equal(
			seedToDeliver("Read .dot-agent-deck/worker-task-coder.md for your task."),
			"Read .dot-agent-deck/worker-task-coder.md for your task.",
		);
	});

	test("trims a trailing newline a shell layer might add", () => {
		assert.equal(seedToDeliver("Acknowledge your role and wait.\n"), "Acknowledge your role and wait.");
	});

	test("returns null for an empty seed (no seed pending → no send)", () => {
		assert.equal(seedToDeliver(""), null);
	});

	test("returns null for whitespace-only output", () => {
		assert.equal(seedToDeliver("   \n\t "), null);
	});

	test("returns null for missing stdout (undefined / null)", () => {
		assert.equal(seedToDeliver(undefined), null);
		assert.equal(seedToDeliver(null), null);
	});
});

describe("native seed delivery: deliverAs mode", () => {
	test("delivers as followUp — triggers a turn, never steers an in-flight one", () => {
		assert.equal(SEED_DELIVER_AS, "followUp");
	});
});

// ---------------------------------------------------------------------------
// ROW 9 — event bus → state mapping
// ---------------------------------------------------------------------------

describe("row 9: Pi event → agent state mapping", () => {
	test("maps the four subscribed lifecycle events to canonical states", () => {
		// Parity with Claude/OpenCode/Codex: a turn ending (agent_settled) and the
		// pre-first-prompt state (session_start) are Idle (`finished`), NOT "Needs
		// Input". Pi exposes no permission/attention event, so it never reports
		// `waiting` — like a Claude agent that never hits a permission prompt.
		assert.equal(piEventToAgentState("session_start"), "finished");
		assert.equal(piEventToAgentState("agent_start"), "running");
		assert.equal(piEventToAgentState("agent_settled"), "finished");
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

describe("issue #622: falling back for a CLI older than the extension", () => {
	test("a lifecycle report with detail falls back to the bare argv every CLI accepts", () => {
		assert.deepEqual(legacyAgentEventArgv({ type: "running", detail: { cwd: "/w" } }), [
			"agent-event",
			"--type",
			"running",
		]);
	});

	test("a lifecycle report that is already bare has nothing to fall back to", () => {
		assert.equal(legacyAgentEventArgv({ type: "finished", detail: {} }), null);
		assert.equal(legacyAgentEventArgv({ type: "finished", detail: { cwd: "  " } }), null);
	});

	test("a detail report has no older equivalent and is dropped", () => {
		assert.equal(legacyAgentEventArgv({ type: "prompt", detail: { prompt: "p" } }), null);
		assert.equal(legacyAgentEventArgv({ type: "tool-start", detail: { toolName: "bash" } }), null);
		assert.equal(legacyAgentEventArgv({ type: "tool-end", detail: {} }), null);
	});

	test("only the CLI's own unknown-flag refusal marks it as older", () => {
		// The released 0.45.1 CLI's exact refusal.
		const refusal =
			"error: unexpected argument '--cwd' found\n\nUsage: dot-agent-deck agent-event --type <TYPE>\n";
		assert.ok(isUnsupportedFlagFailure({ code: 2, stderr: refusal }));
		// Not a usage error, or not this one.
		assert.ok(!isUnsupportedFlagFailure({ code: 1, stderr: "Failed to send agent-event to daemon socket." }));
		assert.ok(!isUnsupportedFlagFailure({ code: 1, stderr: refusal }));
		assert.ok(!isUnsupportedFlagFailure({ code: 2, stderr: "error: invalid value 'x' for '--type <TYPE>'" }));
		assert.ok(!isUnsupportedFlagFailure({ code: 2 }));
		// The phrase appearing elsewhere (e.g. in a directory echoed back) is not a refusal.
		assert.ok(
			!isUnsupportedFlagFailure({
				code: 1,
				stderr: "Failed to send agent-event for /work/error: unexpected argument '--x'",
			}),
		);
	});
});

describe("issue #622: reports reach the deck in the order Pi emitted them", () => {
	const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

	test("a slow earlier report finishes before a fast later one starts", async () => {
		const inOrder = createSerialQueue();
		const log: string[] = [];
		const first = inOrder(async () => {
			log.push("agent_start:begin");
			await delay(30); // e.g. a failed detailed report plus its bare retry
			log.push("agent_start:end");
		});
		const second = inOrder(async () => {
			log.push("agent_settled:begin");
			log.push("agent_settled:end");
		});
		await Promise.all([first, second]);
		assert.deepEqual(log, ["agent_start:begin", "agent_start:end", "agent_settled:begin", "agent_settled:end"]);
	});

	test("a failed report does not stop the ones after it", async () => {
		const inOrder = createSerialQueue();
		const failed = inOrder(async () => {
			throw new Error("daemon down");
		});
		await assert.rejects(failed, /daemon down/);
		assert.equal(await inOrder(async () => "next"), "next");
	});
});
