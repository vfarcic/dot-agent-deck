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

/**
 * The bare dot-agent-deck CLI name — the FALLBACK the extension shells when the
 * deck did not name itself (see {@link resolveDeckBin}). A bare name is looked
 * up in Pi's own `$PATH`, which can reach a different `dot-agent-deck` than the
 * deck that spawned this pane (issue #1385, following #549).
 */
export const DECK_BIN = "dot-agent-deck";

/**
 * The environment variable the deck sets on every agent it spawns, holding its
 * own absolute executable path (`platform::paths::DOT_AGENT_DECK_EXE` on the
 * Rust side). MUST stay in sync with that constant.
 */
export const DECK_EXE_ENV = "DOT_AGENT_DECK_EXE";

/**
 * The CLI the extension shells: the deck's own absolute path from
 * {@link DECK_EXE_ENV} when the deck set one, otherwise the bare
 * {@link DECK_BIN} — so an older deck that sets nothing keeps working exactly
 * as before. The value is used verbatim (argv exec, no shell), so a path with
 * spaces needs no quoting. `env` is a parameter rather than `process.env` so
 * this module stays import- and global-free and unit-testable.
 */
export function resolveDeckBin(env: Readonly<Record<string, string | undefined>>): string {
	const exe = env[DECK_EXE_ENV];
	return typeof exe === "string" && exe.trim().length > 0 ? exe : DECK_BIN;
}

/**
 * Canonical agent lifecycle states accepted by
 * `dot-agent-deck agent-event --type <state>`.
 *
 * MUST stay in sync with the Rust `agent_event_type_from_state` vocabulary
 * (src/event.rs): `running` → Thinking, `waiting` → WaitingForInput,
 * `finished` → Idle.
 */
export const AGENT_STATES = ["running", "waiting", "finished"] as const;
export type AgentState = (typeof AGENT_STATES)[number];

/** Type guard: is `value` one of the three canonical lifecycle states? */
export function isAgentState(value: string): value is AgentState {
	return (AGENT_STATES as readonly string[]).includes(value);
}

/**
 * The card-detail reports `agent-event` accepts beside the lifecycle states
 * (issue #622): `prompt` → Thinking carrying the submitted prompt,
 * `tool-start` → ToolStart, `tool-end` → ToolEnd. Together with
 * {@link AGENT_STATES} these are every `--type` the CLI accepts, and they MUST
 * stay in sync with the Rust `AGENT_EVENT_TYPES` (src/event.rs).
 */
export const DETAIL_TYPES = ["prompt", "tool-start", "tool-end"] as const;
export const AGENT_EVENT_TYPES = [...AGENT_STATES, ...DETAIL_TYPES] as const;
export type AgentEventType = (typeof AGENT_EVENT_TYPES)[number];

/** Type guard: is `value` a `--type` the CLI accepts? */
export function isAgentEventType(value: string): value is AgentEventType {
	return (AGENT_EVENT_TYPES as readonly string[]).includes(value);
}

/**
 * The optional card detail an `agent-event` report carries (issue #622). Each
 * becomes its own flag; a blank or missing value is left off entirely.
 */
export interface AgentEventDetail {
	cwd?: string;
	prompt?: string;
	toolName?: string;
	toolDetail?: string;
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
 * Build the argv for `dot-agent-deck agent-event`. Rejects any type the CLI
 * does not accept so a bogus `--type` can never reach it, and appends each
 * non-blank detail as its own flag in a fixed order. With no detail, a
 * lifecycle report is exactly the argv it has always been.
 *
 * @example buildAgentEventArgv("running")
 *   → ["agent-event", "--type", "running"]
 * @example buildAgentEventArgv("tool-start", { toolName: "bash", toolDetail: "ls" })
 *   → ["agent-event", "--type", "tool-start", "--tool-name=bash", "--tool-detail=ls"]
 */
export function buildAgentEventArgv(type: string, detail: AgentEventDetail = {}): string[] {
	if (!isAgentEventType(type)) {
		throw new Error(
			`dot-agent-deck agent-event: unknown type "${type}". Expected one of: ${AGENT_EVENT_TYPES.join(", ")}.`,
		);
	}
	const argv = ["agent-event", "--type", type];
	const flags: Array<[string, string | undefined]> = [
		["--cwd", detail.cwd],
		["--prompt", detail.prompt],
		["--tool-name", detail.toolName],
		["--tool-detail", detail.toolDetail],
	];
	// `--flag=value` as ONE argv element: these values are free text (a prompt
	// like `--help me`, a command like `-rf x`), and as a separate element the
	// CLI's parser would read a leading dash as another flag and refuse the
	// whole report.
	for (const [flag, value] of flags) {
		if (typeof value === "string" && value.trim().length > 0) {
			argv.push(`${flag}=${value}`);
		}
	}
	return argv;
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
 *   session_start    → finished  (agent is up, awaiting the first prompt → Idle)
 *   agent_start      → running   (an agent run has begun)
 *   agent_settled    → finished  (Pi settled its turn → Idle)
 *   session_shutdown → finished  (the Pi session is exiting)
 *
 * Parity with the other backends (Claude / OpenCode / Codex): a turn ending is
 * `Idle`, NOT "Needs Input". Those agents map their turn-end signal
 * (`Stop` / `session.idle`) to Idle and their session-start to Idle, and they
 * surface "Needs Input" (`waiting`) ONLY on a genuine user-blocking signal —
 * a permission prompt / attention notification. Pi's `agent_settled` is its
 * turn-end analog and `session_start` is its pre-first-prompt state, so both
 * report `finished` → Idle. Pi does not currently expose a permission/attention
 * lifecycle event, so it never reports `waiting` — exactly like a Claude agent
 * that never hits a permission prompt. `waiting` stays in the vocabulary
 * (AGENT_STATES) so that a future Pi user-blocking event can map to it without
 * a wire change.
 *
 * `agent_end` is deliberately NOT mapped: after it Pi may still auto-retry,
 * auto-compact, or drain queued follow-up messages, so it is not a reliable
 * turn-end signal — `agent_settled` is (see Pi's extension docs). Every unmapped
 * event returns `null`, so the caller emits no `agent-event` at all rather than
 * a wrong or default status.
 */
export function piEventToAgentState(eventName: string): AgentState | null {
	switch (eventName) {
		case "session_start":
			return "finished";
		case "agent_start":
			return "running";
		case "agent_settled":
			return "finished";
		case "session_shutdown":
			return "finished";
		default:
			return null;
	}
}

/**
 * The Pi events the extension subscribes to for card detail (issue #622), on
 * top of {@link STATUS_EVENTS}. Pi hands each handler what the card needs:
 * `before_agent_start` carries the submitted prompt before the agent loop
 * begins, `tool_execution_start` the tool name and its arguments, and
 * `tool_execution_end` marks the call finished; every handler's context
 * carries the session's `cwd`.
 */
export const DETAIL_EVENTS = ["before_agent_start", "tool_execution_start", "tool_execution_end"] as const;
export type DetailEvent = (typeof DETAIL_EVENTS)[number];

/**
 * The longest prompt put on argv. The deck keeps only the first 200 characters
 * of a reported prompt, so this bound only keeps a huge paste well clear of the
 * OS's per-argument limit; it never decides what the card shows.
 */
export const MAX_PROMPT_CHARS = 4000;

/** Cut `text` to at most `max` code points, never splitting a surrogate pair. */
function clip(text: string, max: number): string {
	const chars = Array.from(text);
	return chars.length <= max ? text : chars.slice(0, max).join("");
}

function asRecord(value: unknown): Record<string, unknown> | null {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: null;
}

function nonBlankString(value: unknown): string | undefined {
	return typeof value === "string" && value.trim().length > 0 ? value : undefined;
}

/**
 * A short description of a Pi tool call for the card's tool line — the same
 * shape the deck derives for other agents' tools: a shell call's first command
 * line (120 characters), a file tool's path, a search tool's pattern, and for
 * anything else its first string argument (80 characters). `undefined` when the
 * arguments carry nothing to show. Keyed on Pi's built-in tool names and
 * argument shapes (`bash {command}`, `read|write|edit|ls {path}`,
 * `grep|find {pattern}`).
 */
export function piToolDetail(toolName: string, args: unknown): string | undefined {
	const input = asRecord(args);
	if (!input) {
		return undefined;
	}
	switch (toolName) {
		case "bash": {
			const command = nonBlankString(input.command);
			return command === undefined ? undefined : clip(command.split("\n")[0], 120);
		}
		case "read":
		case "write":
		case "edit":
		case "ls":
			return nonBlankString(input.path);
		case "grep":
		case "find":
			return nonBlankString(input.pattern);
		default: {
			const first = Object.values(input).find((v) => nonBlankString(v) !== undefined) as string | undefined;
			return first === undefined ? undefined : clip(first, 80);
		}
	}
}

/** What the extension reports for one Pi event: the `--type` and its detail. */
export interface AgentEventReport {
	type: AgentEventType;
	detail: AgentEventDetail;
}

/**
 * Decide what to report for a Pi event, or `null` to report nothing. Every
 * report carries the session `cwd` (so the card keeps its directory however it
 * was created). Lifecycle events map through {@link piEventToAgentState}; the
 * detail events report the prompt, or the tool and its detail. A
 * `before_agent_start` with no usable prompt reports nothing — the
 * `agent_start` that follows still moves the card to Thinking.
 */
export function piEventReport(eventName: string, event: unknown, cwd: string | undefined): AgentEventReport | null {
	const detail: AgentEventDetail = {};
	const dir = nonBlankString(cwd);
	if (dir !== undefined) {
		detail.cwd = dir;
	}
	const state = piEventToAgentState(eventName);
	if (state) {
		return { type: state, detail };
	}
	const payload = asRecord(event) ?? {};
	switch (eventName) {
		case "before_agent_start": {
			const prompt = nonBlankString(payload.prompt);
			if (prompt === undefined) {
				return null;
			}
			detail.prompt = clip(prompt, MAX_PROMPT_CHARS);
			return { type: "prompt", detail };
		}
		case "tool_execution_start":
		case "tool_execution_end": {
			const toolName = nonBlankString(payload.toolName);
			if (toolName !== undefined) {
				detail.toolName = toolName;
			}
			if (eventName === "tool_execution_end") {
				return { type: "tool-end", detail };
			}
			const toolDetail = toolName === undefined ? undefined : piToolDetail(toolName, payload.args);
			if (toolDetail !== undefined) {
				detail.toolDetail = toolDetail;
			}
			return { type: "tool-start", detail };
		}
		default:
			return null;
	}
}

/**
 * The argv to retry a report with when the full one failed, or `null` when
 * there is nothing to fall back to. The extension can run against a CLI older
 * than itself: the daemon writes the extension into Pi's directory when it
 * starts, so a newer daemon starting on the same machine hands its extension
 * to the Pi panes an older, still-running daemon spawns, and those name the
 * older binary in `DOT_AGENT_DECK_EXE`. That CLI refuses every flag added for
 * issue #622, which would cost the card its status too, so a lifecycle report
 * is retried as the bare `--type <state>` every CLI accepts. A detail report
 * has no older equivalent and is dropped.
 */
export function legacyAgentEventArgv(report: AgentEventReport): string[] | null {
	if (!isAgentState(report.type)) {
		return null;
	}
	const bare = buildAgentEventArgv(report.type);
	return buildAgentEventArgv(report.type, report.detail).length > bare.length ? bare : null;
}

/**
 * Whether a failed report's error says the CLI does not know one of the flags
 * — clap's `unexpected argument` — i.e. the CLI is older than this extension.
 * Only that may switch the session to lifecycle-only reporting: a transient
 * failure (daemon restarting, socket busy) must not cost the card its detail
 * for the rest of the session.
 */
export function isUnsupportedFlagFailure(message: string): boolean {
	return message.includes("unexpected argument");
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
export function execFailureMessage(argv: string[], outcome: ExecOutcome, bin: string = DECK_BIN): string | null {
	if (outcome.code === 0) {
		return null;
	}
	const cmd = [bin, ...argv].join(" ");
	const detail = (outcome.stderr ?? "").trim() || (outcome.stdout ?? "").trim();
	const suffix = detail ? `: ${detail}` : "";
	return `\`${cmd}\` failed with exit code ${outcome.code}${suffix}`;
}

/**
 * Build a clear error message for a spawn failure — e.g. the `dot-agent-deck`
 * binary is not on PATH, or the path the deck named is gone (ENOENT). Pure and
 * unit-testable.
 */
export function spawnFailureMessage(argv: string[], err: unknown, bin: string = DECK_BIN): string {
	const cmd = [bin, ...argv].join(" ");
	const reason = err instanceof Error ? err.message : String(err);
	// The PATH hint only makes sense for a bare name; a path the deck supplied
	// that fails with ENOENT is a missing file, not a PATH miss.
	const hint = !reason.includes("ENOENT")
		? ""
		: bin === DECK_BIN
			? ` (is \`${DECK_BIN}\` installed and on PATH?)`
			: ` (does \`${bin}\` still exist?)`;
	return `Failed to run \`${cmd}\`${hint}: ${reason}`;
}
