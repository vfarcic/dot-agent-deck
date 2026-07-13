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

/**
 * Build the argv for the read-only `dot-agent-deck get-seed` verb (PRD #201).
 *
 * `get-seed` asks the daemon (over the hook socket, scoped by
 * `DOT_AGENT_DECK_PANE_ID`) for the seed/prompt it prepared for this pane and
 * prints it to stdout (empty = nothing pending). The extension shells this on
 * `session_start` and, if the output is a real seed, delivers it NATIVELY via
 * `pi.sendUserMessage` — dissolving the last workaround (PTY keystroke
 * injection) for a Pi pane's first prompt.
 *
 * @example buildGetSeedArgv() → ["get-seed"]
 */
export function buildGetSeedArgv(): string[] {
	return ["get-seed"];
}

/**
 * How the native seed is queued into Pi. `"followUp"` means "deliver the
 * message and, if a turn is already streaming, queue it until the agent
 * finishes" — and `sendUserMessage` "always triggers a turn", so on
 * `session_start` (the agent is idle) this seeds AND runs the prompt
 * deterministically, with none of the keystroke-timing fragility the PTY
 * injection path had (no SUBMIT_DELAY, no readiness guess, no discarded early
 * CR). `"steer"` would interrupt an in-flight turn — wrong for a first prompt.
 */
export const SEED_DELIVER_AS = "followUp" as const;

/**
 * Decide what to deliver from a `get-seed` result. Returns the trimmed seed
 * when the CLI printed a non-blank one, or `null` when there is nothing to
 * deliver (empty output, whitespace-only, or the CLI produced no stdout) — in
 * which case the extension sends nothing and the daemon's PTY-injection safety
 * net remains responsible for delivery. Trimming drops any trailing newline a
 * shell layer might add without altering a single-line prompt's meaning.
 */
export function seedToDeliver(stdout: string | undefined | null): string | null {
	if (typeof stdout !== "string") {
		return null;
	}
	const seed = stdout.trim();
	return seed.length > 0 ? seed : null;
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
