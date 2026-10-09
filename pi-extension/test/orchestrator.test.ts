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
import { spawn } from "node:child_process";
import { describe, test } from "node:test";
import {
	AGENT_EVENT_TYPES,
	AGENT_STATES,
	buildAgentEventArgv,
	DETAIL_EVENTS,
	buildDelegateArgv,
	buildGetSeedArgv,
	buildWorkDoneArgv,
	createReporter,
	createSerialQueue,
	createTurnReplyTracker,
	DECK_BIN,
	DECK_BIN_OVERRIDE_ENV,
	DECK_EXE_ENV,
	DeckExecError,
	DECLARE_PROMPT_REPORTS_FLAG,
	execFailureMessage,
	execWithStdin,
	isAgentState,
	isUnsupportedFlagFailure,
	legacyAgentEventArgv,
	MAX_PROMPT_CHARS,
	MAX_TURN_REPLY_BYTES,
	piAssistantReply,
	piEventReport,
	piEventToAgentState,
	piToolDetail,
	REPORT_LEVELS,
	reportArgvAt,
	resolveDeckBin,
	isAbsoluteOverride,
	SEED_DELIVER_AS,
	seedToDeliver,
	spawnFailureMessage,
	STATUS_EVENTS,
	TURN_REPLY_FAILED_FLAG,
	TURN_REPLY_STDIN_FLAG,
	turnReplyStdin,
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

	// Issue #1567: the declaration goes FIRST among the flags, so a CLI that
	// predates it names it, and not `--cwd`, as the argument it does not know.
	test("the prompt-report declaration is the first flag when asked for", () => {
		assert.equal(DECLARE_PROMPT_REPORTS_FLAG, "--reports-prompts");
		assert.deepEqual(buildAgentEventArgv("prompt", { cwd: "/w", prompt: "go" }, true), [
			"agent-event",
			"--type",
			"prompt",
			"--reports-prompts",
			"--cwd=/w",
			"--prompt=go",
		]);
		assert.deepEqual(buildAgentEventArgv("finished", {}, true), [
			"agent-event",
			"--type",
			"finished",
			"--reports-prompts",
		]);
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

	// Issue #1567, measured on Pi 0.87.1: a prompt submitted while Pi is busy is
	// queued as a steering or follow-up message and never reaches
	// `before_agent_start`. Pi raises `input` for it with `streamingBehavior`.
	test("input reports a prompt Pi queues because it is busy", () => {
		for (const streamingBehavior of ["steer", "followUp"]) {
			assert.deepEqual(
				piEventReport("input", { text: "also do this", source: "interactive", streamingBehavior }, "/w"),
				{ type: "prompt", detail: { cwd: "/w", prompt: "also do this" } },
				streamingBehavior,
			);
		}
		assert.equal(
			piEventReport(
				"input",
				{ text: "p".repeat(MAX_PROMPT_CHARS + 50), source: "extension", streamingBehavior: "followUp" },
				"/w",
			)?.detail.prompt?.length,
			MAX_PROMPT_CHARS,
		);
	});

	test("input leaves an idle submission to before_agent_start, so it is reported once", () => {
		assert.equal(piEventReport("input", { text: "go", source: "interactive" }, "/w"), null);
		assert.equal(
			piEventReport("input", { text: "go", source: "interactive", streamingBehavior: undefined }, "/w"),
			null,
		);
	});

	test("input with no usable text reports nothing", () => {
		assert.equal(piEventReport("input", { text: "  ", streamingBehavior: "steer" }, "/w"), null);
		assert.equal(piEventReport("input", { streamingBehavior: "steer" }, "/w"), null);
		assert.equal(piEventReport("input", undefined, "/w"), null);
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
			input: { text: "go", source: "interactive", streamingBehavior: "steer" },
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
			resolveDeckBin({ DOT_AGENT_DECK_EXE: "/home/me/.local/bin/dot-agent-deck" }, "linux"),
			"/home/me/.local/bin/dot-agent-deck",
		);
		// argv exec, no shell: a path with spaces is passed unquoted.
		assert.equal(
			resolveDeckBin({ DOT_AGENT_DECK_EXE: "/opt/my tools/dot-agent-deck" }, "linux"),
			"/opt/my tools/dot-agent-deck",
		);
	});

	test("falls back to the bare name when an older deck sets nothing", () => {
		assert.equal(resolveDeckBin({}, "linux"), DECK_BIN);
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_EXE: "" }, "linux"), DECK_BIN);
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_EXE: "   " }, "linux"), DECK_BIN);
		assert.equal(DECK_BIN, "dot-agent-deck");
	});

	test("PRD #1497: the operator's DOT_AGENT_DECK_BIN wins over the deck's own path", () => {
		assert.equal(DECK_BIN_OVERRIDE_ENV, "DOT_AGENT_DECK_BIN");
		assert.equal(
			resolveDeckBin({
				DOT_AGENT_DECK_BIN: "/src/target/debug/dot-agent-deck",
				DOT_AGENT_DECK_EXE: "/opt/homebrew/bin/dot-agent-deck",
			}, "linux"),
			"/src/target/debug/dot-agent-deck",
		);
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: "/my build/dot-agent-deck" }, "linux"), "/my build/dot-agent-deck");
		// Empty or blank is unset, so the deck's own path (or the bare name) is used.
		assert.equal(
			resolveDeckBin({ DOT_AGENT_DECK_BIN: "", DOT_AGENT_DECK_EXE: "/opt/homebrew/bin/dot-agent-deck" }, "linux"),
			"/opt/homebrew/bin/dot-agent-deck",
		);
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: "  " }, "linux"), DECK_BIN);
	});

	test("PRD #1497 H2: a DOT_AGENT_DECK_BIN that is not absolute is ignored", () => {
		const exe = "/opt/homebrew/bin/dot-agent-deck";
		// A bare name would be looked up in Pi's PATH and a relative path
		// against its cwd, so each falls back exactly as if it were unset.
		for (const value of ["deck-hook", "./x", "target/debug/dot-agent-deck", " /abs/with-leading-space", "  "]) {
			assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: value, DOT_AGENT_DECK_EXE: exe }, "linux"), exe, value);
			assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: value }, "linux"), DECK_BIN, value);
		}
		assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: "/abs/deck", DOT_AGENT_DECK_EXE: exe }, "linux"), "/abs/deck");
	});

	test("PRD #1497 H2: absolute on Windows means drive-qualified or UNC", () => {
		const exe = "C:\\deck\\dot-agent-deck.exe";
		for (const value of ["C:\\build\\dot-agent-deck.exe", "d:/build/dot-agent-deck.exe", "\\\\server\\share\\deck.exe"]) {
			assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: value, DOT_AGENT_DECK_EXE: exe }, "win32"), value, value);
		}
		for (const value of ["deck.exe", ".\\deck.exe", "\\deck.exe", "C:deck.exe", ""]) {
			assert.equal(resolveDeckBin({ DOT_AGENT_DECK_BIN: value, DOT_AGENT_DECK_EXE: exe }, "win32"), exe, value);
		}
		assert.equal(isAbsoluteOverride("/abs", "linux"), true);
		assert.equal(isAbsoluteOverride("C:\\abs", "linux"), false);
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

describe("issue #1567: report levels for decks of every age", () => {
	const prompt = { type: "prompt" as const, detail: { cwd: "/w", prompt: "go" } };
	const lifecycle = { type: "running" as const, detail: { cwd: "/w" } };

	test("each level sends what it names, and a detail report has no lifecycle form", () => {
		assert.deepEqual([...REPORT_LEVELS], ["declared", "detail", "lifecycle"]);
		assert.deepEqual(reportArgvAt(prompt, "declared"), [
			"agent-event",
			"--type",
			"prompt",
			"--reports-prompts",
			"--cwd=/w",
			"--prompt=go",
		]);
		assert.deepEqual(reportArgvAt(prompt, "detail"), ["agent-event", "--type", "prompt", "--cwd=/w", "--prompt=go"]);
		assert.equal(reportArgvAt(prompt, "lifecycle"), null);
		assert.deepEqual(reportArgvAt(lifecycle, "lifecycle"), ["agent-event", "--type", "running"]);
	});

	/**
	 * A fake deck CLI that knows only `known` flags and refuses the first
	 * other one exactly as clap does (exit 2, `error: unexpected argument …`).
	 * `down` makes every call fail the way an unreachable daemon does.
	 */
	function fakeCli(known: string[], options: { down?: boolean } = {}) {
		const calls: string[][] = [];
		const run = async (argv: string[]) => {
			calls.push(argv);
			const unknown = argv.slice(3).find((arg) => !known.includes(arg.split("=")[0]));
			if (unknown !== undefined) {
				const flag = unknown.split("=")[0];
				throw new DeckExecError("refused", { code: 2, stderr: `error: unexpected argument '${flag}' found\n` });
			}
			if (options.down) {
				throw new DeckExecError("down", { code: 1, stderr: "Failed to send agent-event to daemon socket." });
			}
			return { code: 0, stdout: "", stderr: "" };
		};
		return { calls, run };
	}
	const DETAIL_FLAGS = ["--cwd", "--prompt", "--tool-name", "--tool-detail"];

	test("a current deck gets every report declared, first time", async () => {
		const cli = fakeCli([DECLARE_PROMPT_REPORTS_FLAG, ...DETAIL_FLAGS]);
		const reporter = createReporter(cli.run);
		await reporter.send(lifecycle);
		await reporter.send(prompt);
		assert.equal(reporter.level(), "declared");
		assert.deepEqual(
			cli.calls.map((argv) => argv.includes("--reports-prompts")),
			[true, true],
		);
	});

	test("a deck from #622 to #1567 keeps the detail and loses only the declaration", async () => {
		const cli = fakeCli(DETAIL_FLAGS);
		const reporter = createReporter(cli.run);
		await reporter.send(prompt);
		assert.equal(reporter.level(), "detail");
		assert.deepEqual(cli.calls.at(-1), ["agent-event", "--type", "prompt", "--cwd=/w", "--prompt=go"]);
		await reporter.send(lifecycle);
		assert.deepEqual(cli.calls.at(-1), ["agent-event", "--type", "running", "--cwd=/w"]);
		assert.equal(cli.calls.length, 3, "after the first refusal no report is spent on the declaration");
	});

	test("a deck from before #622 falls all the way to bare lifecycle reports", async () => {
		const cli = fakeCli([]);
		const reporter = createReporter(cli.run);
		await reporter.send(lifecycle);
		assert.equal(reporter.level(), "lifecycle");
		assert.deepEqual(cli.calls, [
			["agent-event", "--type", "running", "--reports-prompts", "--cwd=/w"],
			["agent-event", "--type", "running", "--cwd=/w"],
			["agent-event", "--type", "running"],
		]);
		await reporter.send(prompt);
		assert.equal(cli.calls.length, 3, "a detail report has nothing to send to such a deck");
	});

	test("a transient failure keeps the level and retries a lifecycle report bare", async () => {
		const cli = fakeCli([DECLARE_PROMPT_REPORTS_FLAG, ...DETAIL_FLAGS], { down: true });
		const reporter = createReporter(cli.run);
		await reporter.send(lifecycle);
		assert.equal(reporter.level(), "declared");
		assert.deepEqual(cli.calls, [
			["agent-event", "--type", "running", "--reports-prompts", "--cwd=/w"],
			["agent-event", "--type", "running"],
		]);
		await reporter.send(prompt);
		assert.equal(cli.calls.length, 3, "a failed detail report is not retried");
	});

	test("a failure that is not a DeckExecError is treated as transient", async () => {
		const calls: string[][] = [];
		const reporter = createReporter(async (argv) => {
			calls.push(argv);
			throw new Error("spawn ENOENT");
		});
		await reporter.send(lifecycle);
		assert.equal(reporter.level(), "declared");
		assert.equal(calls.length, 2);
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

// ---------------------------------------------------------------------------
// PRD #1497 — a settled turn's final reply, for the deck's reading mode
// ---------------------------------------------------------------------------

describe("PRD #1497: the settled turn's final reply", () => {
	/** An assistant message as Pi 0.84's `message_end` carries it. */
	function assistant(content: unknown[], extra: Record<string, unknown> = {}) {
		return {
			type: "message_end",
			message: {
				role: "assistant",
				content,
				api: "anthropic-messages",
				provider: "anthropic",
				model: "claude-haiku-4-5",
				usage: { input: 10, output: 20, cacheRead: 0, cacheWrite: 0, totalTokens: 30 },
				stopReason: "stop",
				timestamp: 1_760_000_000_000,
				...extra,
			},
		};
	}
	const toolResult = {
		type: "message_end",
		message: {
			role: "toolResult",
			toolCallId: "call-1",
			toolName: "bash",
			content: [{ type: "text", text: "ok 42 tests" }],
			isError: false,
			timestamp: 1_760_000_000_001,
		},
	};
	const user = { type: "message_end", message: { role: "user", content: "run the tests", timestamp: 1 } };

	test("an assistant message's text blocks are its reply; thinking and tool calls are not", () => {
		const event = assistant([
			{ type: "thinking", thinking: "let me check", thinkingSignature: "sig" },
			{ type: "text", text: "All 42 tests pass." },
			{ type: "toolCall", id: "call-2", name: "bash", arguments: { command: "cargo test" } },
			{ type: "text", text: "  Nothing changed.  " },
		]);
		assert.deepEqual(piAssistantReply(event.message), { text: "All 42 tests pass.\n\nNothing changed.", failed: false });
		assert.equal(piAssistantReply(toolResult.message), undefined);
		assert.equal(piAssistantReply(user.message), undefined);
		assert.equal(piAssistantReply(assistant([{ type: "toolCall", id: "c", name: "ls", arguments: {} }]).message), undefined);
		assert.equal(piAssistantReply(null), undefined);
	});

	test("an errored message is failed, and says its error when it has no text", () => {
		const errored = assistant([], { stopReason: "error", errorMessage: "429 rate limited" });
		assert.deepEqual(piAssistantReply(errored.message), { text: "429 rate limited", failed: true });
		const partial = assistant([{ type: "text", text: "I fixed half." }], { stopReason: "error", errorMessage: "boom" });
		assert.deepEqual(piAssistantReply(partial.message), { text: "I fixed half.", failed: true });
	});

	test("a very long reply is bounded to what the deck keeps, never splitting a character", () => {
		const long = assistant([{ type: "text", text: "a" + "é".repeat(MAX_TURN_REPLY_BYTES) }]);
		const text = piAssistantReply(long.message)?.text ?? "";
		// One ASCII byte, then two-byte characters: the last whole one ends a byte short of the bound.
		assert.equal(Buffer.byteLength(text, "utf8"), MAX_TURN_REPLY_BYTES - 1);
		assert.ok(text.endsWith("é"));
		const emoji = turnReplyStdin({ text: "😀".repeat(MAX_TURN_REPLY_BYTES), failed: false }) ?? "";
		assert.equal(Buffer.byteLength(emoji, "utf8"), MAX_TURN_REPLY_BYTES);
		assert.equal(emoji, "😀".repeat(MAX_TURN_REPLY_BYTES / 4), "no surrogate pair is split");
	});

	test("the tracker keeps the run's last assistant reply and hands it over once", () => {
		const tracker = createTurnReplyTracker();
		tracker.observe("agent_start", { type: "agent_start" });
		tracker.observe("message_end", user);
		tracker.observe("message_end", assistant([{ type: "text", text: "Running the tests." }, { type: "toolCall", id: "c", name: "bash", arguments: {} }]));
		tracker.observe("message_end", toolResult);
		tracker.observe("message_end", assistant([{ type: "text", text: "All 42 tests pass." }]));
		tracker.observe("tool_execution_end", { toolName: "bash" });
		assert.deepEqual(tracker.take(), { text: "All 42 tests pass.", failed: false });
		assert.equal(tracker.take(), undefined, "a reply is reported once");

		// A new run forgets a reply the last run never settled with: it ended
		// with nothing to read.
		tracker.observe("message_end", assistant([{ type: "text", text: "stale" }]));
		tracker.observe("agent_start", { type: "agent_start" });
		assert.deepEqual(tracker.take(), { text: "", failed: false });
		assert.equal(tracker.take(), undefined, "an empty turn end is reported once too");
	});

	test("a final assistant message with nothing to read leaves no earlier reply behind", () => {
		const commentary = assistant([{ type: "text", text: "Running the tests." }]);
		const finals: Array<[string, ReturnType<typeof assistant>, boolean]> = [
			["thinking only", assistant([{ type: "thinking", thinking: "done", thinkingSignature: "sig" }]), false],
			["a tool call only", assistant([{ type: "toolCall", id: "c", name: "bash", arguments: {} }]), false],
			["empty", assistant([]), false],
			["an error with no errorMessage", assistant([], { stopReason: "error" }), true],
		];
		for (const [label, final, failed] of finals) {
			const tracker = createTurnReplyTracker();
			tracker.observe("agent_start", { type: "agent_start" });
			tracker.observe("message_end", commentary);
			tracker.observe("message_end", toolResult);
			tracker.observe("message_end", final);
			assert.deepEqual(tracker.take(), { text: "", failed }, `${label}: the earlier commentary is not the reply; the turn ended with nothing to read`);
		}
		assert.equal(createTurnReplyTracker().take(), undefined, "no run, no turn end to report");
		// A non-assistant message after the reply leaves it in place.
		const tracker = createTurnReplyTracker();
		tracker.observe("message_end", assistant([{ type: "text", text: "All 42 tests pass." }]));
		tracker.observe("message_end", toolResult);
		tracker.observe("message_end", user);
		assert.deepEqual(tracker.take(), { text: "All 42 tests pass.", failed: false });
	});

	test("only agent_settled carries the reply, as the last flags of a declared report, its text never on argv", () => {
		const reply = { text: "All 42 tests pass.", failed: false };
		const settled = piEventReport("agent_settled", { type: "agent_settled" }, "/w", reply);
		assert.deepEqual(settled, { type: "finished", detail: { cwd: "/w" }, reply });
		assert.deepEqual(piEventReport("session_shutdown", {}, "/w", reply), { type: "finished", detail: { cwd: "/w" } });
		assert.deepEqual(piEventReport("agent_settled", {}, "/w"), { type: "finished", detail: { cwd: "/w" } });
		assert.deepEqual(reportArgvAt(settled!, "declared", true), [
			"agent-event",
			"--type",
			"finished",
			"--reports-prompts",
			"--cwd=/w",
			TURN_REPLY_STDIN_FLAG,
		]);
		const failed = { ...settled!, reply: { text: "--boom", failed: true } };
		assert.deepEqual(reportArgvAt(failed, "declared", true)?.slice(-2), [TURN_REPLY_STDIN_FLAG, TURN_REPLY_FAILED_FLAG]);
		assert.equal(turnReplyStdin(settled!.reply), "All 42 tests pass.");
		assert.equal(turnReplyStdin(failed.reply), "--boom");
		assert.equal(turnReplyStdin({ text: " \n ", failed: false }), "", "a blank reply is an empty turn end");
		assert.equal(turnReplyStdin(undefined), undefined);
		// Without the reply, and below `declared`, the argv is what it always was.
		assert.deepEqual(reportArgvAt(settled!, "declared"), ["agent-event", "--type", "finished", "--reports-prompts", "--cwd=/w"]);
		assert.deepEqual(reportArgvAt(settled!, "detail", true), ["agent-event", "--type", "finished", "--cwd=/w"]);
		assert.deepEqual(reportArgvAt(settled!, "lifecycle", true), ["agent-event", "--type", "finished"]);
		// A blank reply still carries the flags: the turn ended with nothing to read.
		assert.deepEqual(buildAgentEventArgv("finished", {}, true, { text: "   ", failed: true }), [
			"agent-event",
			"--type",
			"finished",
			"--reports-prompts",
			TURN_REPLY_STDIN_FLAG,
			TURN_REPLY_FAILED_FLAG,
		]);
	});

	/**
	 * The fake CLI from the report-level tests, refusing unknown flags as clap
	 * does, and recording what each call wrote to its stdin.
	 */
	function cliKnowing(known: string[]) {
		const calls: string[][] = [];
		const stdins: Array<string | undefined> = [];
		const run = async (argv: string[], _signal?: AbortSignal, stdin?: string) => {
			calls.push(argv);
			stdins.push(stdin);
			const unknown = argv.slice(3).find((arg) => !known.includes(arg.split("=")[0]));
			if (unknown !== undefined) {
				throw new DeckExecError("refused", {
					code: 2,
					stderr: `error: unexpected argument '${unknown.split("=")[0]}' found\n`,
				});
			}
			return { code: 0, stdout: "", stderr: "" };
		};
		return { calls, stdins, run };
	}
	const DETAIL = ["--cwd", "--prompt", "--tool-name", "--tool-detail"];
	const settled = { type: "finished" as const, detail: { cwd: "/w" }, reply: { text: "done", failed: false } };

	test("a current deck takes the reply on the declared report", async () => {
		const cli = cliKnowing([DECLARE_PROMPT_REPORTS_FLAG, ...DETAIL, TURN_REPLY_STDIN_FLAG, TURN_REPLY_FAILED_FLAG]);
		const reporter = createReporter(cli.run);
		await reporter.send(settled);
		assert.deepEqual(cli.calls, [["agent-event", "--type", "finished", "--reports-prompts", "--cwd=/w", "--turn-reply-stdin"]]);
		assert.deepEqual(cli.stdins, ["done"], "the reply goes on stdin");
		assert.equal(reporter.level(), "declared");
		assert.equal(reporter.replies(), true);
		// A report with no reply writes no stdin.
		await reporter.send({ type: "running", detail: { cwd: "/w" } });
		assert.equal(cli.stdins.at(-1), undefined);
	});

	test("a reply's text is never on any argv the reporter sends, at any level", async () => {
		const secret = "the-reply-text-" + "x".repeat(20);
		const report = { ...settled, reply: { text: secret, failed: true } };
		for (const known of [
			[DECLARE_PROMPT_REPORTS_FLAG, ...DETAIL, TURN_REPLY_STDIN_FLAG, TURN_REPLY_FAILED_FLAG],
			[DECLARE_PROMPT_REPORTS_FLAG, ...DETAIL],
			DETAIL,
			[],
		]) {
			const cli = cliKnowing(known);
			await createReporter(cli.run).send(report);
			for (const argv of cli.calls) {
				assert.ok(!argv.some((arg) => arg.includes(secret)), `argv ${JSON.stringify(argv)}`);
			}
			// Stdin carries the reply exactly when the argv announces it.
			cli.calls.forEach((argv, i) => {
				assert.equal(cli.stdins[i] === secret, argv.includes(TURN_REPLY_STDIN_FLAG), JSON.stringify(argv));
			});
		}
	});

	test("a deck from #1567 drops the reply once and keeps the declaration", async () => {
		const cli = cliKnowing([DECLARE_PROMPT_REPORTS_FLAG, ...DETAIL]);
		const reporter = createReporter(cli.run);
		await reporter.send(settled);
		assert.deepEqual(cli.calls, [
			["agent-event", "--type", "finished", "--reports-prompts", "--cwd=/w", "--turn-reply-stdin"],
			["agent-event", "--type", "finished", "--reports-prompts", "--cwd=/w"],
		]);
		assert.deepEqual(cli.stdins, ["done", undefined], "the resend at the same level carries no reply");
		assert.equal(reporter.level(), "declared");
		assert.equal(reporter.replies(), false);
		await reporter.send(settled);
		assert.equal(cli.calls.length, 3, "no later report is spent on the reply");
		assert.deepEqual(cli.calls.at(-1), ["agent-event", "--type", "finished", "--reports-prompts", "--cwd=/w"]);
	});

	test("an older deck still steps down the levels exactly as before", async () => {
		const detailOnly = cliKnowing(DETAIL);
		const reporter = createReporter(detailOnly.run);
		await reporter.send(settled);
		assert.equal(reporter.level(), "detail");
		assert.deepEqual(detailOnly.calls.at(-1), ["agent-event", "--type", "finished", "--cwd=/w"]);

		const bare = cliKnowing([]);
		const oldest = createReporter(bare.run);
		await oldest.send(settled);
		assert.equal(oldest.level(), "lifecycle");
		assert.deepEqual(bare.calls.at(-1), ["agent-event", "--type", "finished"]);
	});
});

describe("PRD #1497: a reply reaches the CLI on stdin, never on its command line", () => {
	/**
	 * A stand-in CLI: a Node child that prints its argv and the stdin it read
	 * as JSON, or with `exitAt` exits with that code before reading anything,
	 * the way an older CLI's usage error does.
	 */
	const echo = `
		const exitAt = process.env.EXIT_AT;
		if (exitAt) { process.stderr.write("error: unexpected argument '--turn-reply-stdin' found\\n"); process.exit(Number(exitAt)); }
		let input = "";
		process.stdin.setEncoding("utf8");
		process.stdin.on("data", (c) => { input += c; });
		process.stdin.on("end", () => { process.stdout.write(JSON.stringify({ argv: process.argv.slice(1), stdin: input, cwd: process.cwd() })); });
	`;
	const node = process.execPath;

	test("the reply is written to the child's stdin, and the argv is only what was passed", async () => {
		const reply = "All 42 tests pass.\n\n--not-a-flag é😀";
		const outcome = await execWithStdin(spawn, node, ["-e", echo, "--", "agent-event", TURN_REPLY_STDIN_FLAG], reply, {
			cwd: "/",
		});
		assert.equal(outcome.code, 0, outcome.stderr);
		const seen = JSON.parse(outcome.stdout);
		assert.equal(seen.stdin, reply);
		assert.deepEqual(seen.argv, ["agent-event", TURN_REPLY_STDIN_FLAG]);
		assert.equal(seen.cwd, "/");
		assert.equal(outcome.killed, false);
	});

	test("a reply as long as the deck keeps reaches the child whole", async () => {
		const reply = turnReplyStdin({ text: "é".repeat(MAX_TURN_REPLY_BYTES), failed: false })!;
		const outcome = await execWithStdin(spawn, node, ["-e", echo], reply);
		assert.equal(JSON.parse(outcome.stdout).stdin, reply);
	});

	test("an older CLI that exits without reading stdin answers with its exit code, not an error", async () => {
		const prior = process.env.EXIT_AT;
		process.env.EXIT_AT = "2";
		try {
			const outcome = await execWithStdin(spawn, node, ["-e", echo], "x".repeat(MAX_TURN_REPLY_BYTES));
			assert.equal(outcome.code, 2);
			assert.ok(isUnsupportedFlagFailure(outcome), outcome.stderr);
		} finally {
			if (prior === undefined) {
				delete process.env.EXIT_AT;
			} else {
				process.env.EXIT_AT = prior;
			}
		}
	});

	test("a missing binary rejects, as a spawn failure", async () => {
		await assert.rejects(execWithStdin(spawn, "/nonexistent/dot-agent-deck", ["agent-event"], "x"), /ENOENT/);
	});

	test("an aborted report kills the child", async () => {
		const controller = new AbortController();
		const pending = execWithStdin(spawn, node, ["-e", "setTimeout(() => {}, 60000)"], "x", { signal: controller.signal });
		controller.abort();
		const outcome = await pending;
		assert.equal(outcome.killed, true);
		assert.notEqual(outcome.code, 0);
	});

	test("a timeout kills a child that never finishes", async () => {
		const outcome = await execWithStdin(spawn, node, ["-e", "setTimeout(() => {}, 60000)"], "x", { timeout: 50 });
		assert.equal(outcome.killed, true);
		assert.notEqual(outcome.code, 0);
	});
});
