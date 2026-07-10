/**
 * Pure orchestrator logic for the dot-agent-deck Pi extension (PRD #201).
 *
 * This module has ZERO imports — no Pi API, no Node built-ins — so every
 * function here is unit-testable without a running `pi` and without installing
 * the Pi toolchain. The Pi-API glue in `index.ts` wires these functions to
 * `pi.registerTool()` / `pi.on()` and shells the `dot-agent-deck` CLI; the
 * decisions worth testing (argv construction, event→state mapping, error
 * classification) all live here.
 *
 * This mirrors how the Rust side keeps `agent_event_type_from_state` pure
 * (src/event.rs): the canonical status vocabulary lives in exactly one place
 * and everything maps into it.
 */

/** The dot-agent-deck CLI binary the extension shells. */
export const DECK_BIN = "dot-agent-deck";

/**
 * Canonical agent lifecycle states accepted by
 * `dot-agent-deck agent-event --type <state>`.
 *
 * MUST stay in sync with the Rust `agent_event_type_from_state` vocabulary
 * (src/event.rs): `running` → Thinking, `waiting` → WaitingForInput,
 * `finished` → Idle. Anything else is rejected by the CLI, so the extension
 * must only ever emit one of these three strings.
 */
export const AGENT_STATES = ["running", "waiting", "finished"] as const;
export type AgentState = (typeof AGENT_STATES)[number];

/** Type guard: is `value` one of the three canonical states? */
export function isAgentState(value: string): value is AgentState {
	return (AGENT_STATES as readonly string[]).includes(value);
}

function requireNonBlank(value: unknown, label: string): string {
	if (typeof value !== "string" || value.trim().length === 0) {
		throw new Error(`dot-agent-deck: ${label} must be a non-empty string.`);
	}
	return value;
}

/**
 * Build the argv for `dot-agent-deck delegate`.
 *
 * `--to` is repeatable on the CLI (clap `Vec<String>`), so `to` accepts either
 * a single role or a list. Blank roles are dropped; delegating with no usable
 * role, or with a blank task, throws a clear error before anything is spawned.
 *
 * @example buildDelegateArgv("coder", "fix the bug")
 *   → ["delegate", "--to", "coder", "--task", "fix the bug"]
 */
export function buildDelegateArgv(to: string | string[], task: string): string[] {
	const roles = (Array.isArray(to) ? to : [to])
		.map((role) => (typeof role === "string" ? role.trim() : ""))
		.filter((role) => role.length > 0);
	if (roles.length === 0) {
		throw new Error("dot-agent-deck delegate: at least one non-empty --to <role> is required.");
	}
	requireNonBlank(task, "delegate task");
	const argv = ["delegate"];
	for (const role of roles) {
		argv.push("--to", role);
	}
	argv.push("--task", task);
	return argv;
}

/**
 * Build the argv for `dot-agent-deck work-done`. Pass `done: true` to also
 * signal that the entire orchestration is complete (orchestrator only).
 *
 * @example buildWorkDoneArgv("added tests")
 *   → ["work-done", "--task", "added tests"]
 */
export function buildWorkDoneArgv(summary: string, done = false): string[] {
	requireNonBlank(summary, "work-done summary");
	const argv = ["work-done", "--task", summary];
	if (done) {
		argv.push("--done");
	}
	return argv;
}

/**
 * Build the argv for `dot-agent-deck agent-event`. Rejects any non-canonical
 * state so a bogus `--type` can never reach the CLI.
 *
 * @example buildAgentEventArgv("running")
 *   → ["agent-event", "--type", "running"]
 */
export function buildAgentEventArgv(state: string): string[] {
	if (!isAgentState(state)) {
		throw new Error(
			`dot-agent-deck agent-event: unknown state "${state}". Expected one of: ${AGENT_STATES.join(", ")}.`,
		);
	}
	return ["agent-event", "--type", state];
}

/** The Pi lifecycle events the extension subscribes to for status reporting. */
export const STATUS_EVENTS = [
	"session_start",
	"agent_start",
	"agent_settled",
	"session_shutdown",
] as const;
export type StatusEvent = (typeof STATUS_EVENTS)[number];

/**
 * Map a Pi lifecycle event name to the canonical agent state the extension
 * reports via `agent-event`, or `null` for events we intentionally ignore.
 *
 *   session_start    → waiting   (agent is up, awaiting the first prompt)
 *   agent_start      → running   (an agent run has begun)
 *   agent_settled    → waiting   (Pi will not continue automatically; awaiting input)
 *   session_shutdown → finished  (the Pi session is exiting)
 *
 * `agent_end` is deliberately NOT mapped: after it Pi may still auto-retry,
 * auto-compact, or drain queued follow-up messages, so it is not a reliable
 * idle signal — `agent_settled` is (see Pi's extension docs). Every unmapped
 * event returns `null`, so the caller emits no `agent-event` at all rather than
 * a wrong or default status.
 */
export function piEventToAgentState(eventName: string): AgentState | null {
	switch (eventName) {
		case "session_start":
			return "waiting";
		case "agent_start":
			return "running";
		case "agent_settled":
			return "waiting";
		case "session_shutdown":
			return "finished";
		default:
			return null;
	}
}

/** Minimal shape of a `dot-agent-deck` CLI exec result (subset of Pi's ExecResult). */
export interface ExecOutcome {
	code: number;
	stdout?: string;
	stderr?: string;
}

/**
 * Classify a completed CLI exec. Returns a clear, human-readable error message
 * when the command failed (non-zero exit), or `null` on success. Kept pure so
 * the exact error text is unit-testable.
 */
export function execFailureMessage(argv: string[], outcome: ExecOutcome): string | null {
	if (outcome.code === 0) {
		return null;
	}
	const cmd = [DECK_BIN, ...argv].join(" ");
	const detail = (outcome.stderr ?? "").trim() || (outcome.stdout ?? "").trim();
	const suffix = detail ? `: ${detail}` : "";
	return `\`${cmd}\` failed with exit code ${outcome.code}${suffix}`;
}

/**
 * Build a clear error message for a spawn failure — e.g. the `dot-agent-deck`
 * binary is not on PATH (ENOENT). Pure and unit-testable.
 */
export function spawnFailureMessage(argv: string[], err: unknown): string {
	const cmd = [DECK_BIN, ...argv].join(" ");
	const reason = err instanceof Error ? err.message : String(err);
	const hint = reason.includes("ENOENT") ? ` (is \`${DECK_BIN}\` installed and on PATH?)` : "";
	return `Failed to run \`${cmd}\`${hint}: ${reason}`;
}
