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
 * The `agent-event` flag that declares this extension reports every prompt Pi
 * submits (issue #1567). The deck counts a Pi pane as confirming its own
 * prompts only while its reports carry this, so a pane running an extension
 * that does not report them keeps being treated as one that cannot.
 */
export const DECLARE_PROMPT_REPORTS_FLAG = "--reports-prompts";

/**
 * The `agent-event` flags that announce a settled turn's final reply (PRD
 * #1497): the reply itself comes on the CLI's stdin, and the second flag says
 * the message it came from ended in an error. The deck reads a short summary
 * of it aloud while reading mode is on.
 *
 * The text never goes on the command line, where any local user who can list
 * processes could read it (`/proc/<pid>/cmdline`). `pi.exec` spawns with stdin
 * ignored and takes no stdin option (Pi 0.84.4 and 1.1.0), so a report that
 * carries a reply is spawned by {@link execWithStdin} instead.
 */
export const TURN_REPLY_STDIN_FLAG = "--turn-reply-stdin";
export const TURN_REPLY_FAILED_FLAG = "--turn-reply-failed";

/**
 * The longest reply written to the CLI's stdin, in UTF-8 bytes: what the deck
 * keeps of a reply (`MAX_TURN_REPLY_BYTES`), so nothing past it is ever sent.
 */
export const MAX_TURN_REPLY_BYTES = 8192;

/** A settled turn's final reply, as the extension reports it. */
export interface TurnReply {
	text: string;
	failed: boolean;
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
export function buildAgentEventArgv(
	type: string,
	detail: AgentEventDetail = {},
	declarePromptReports = false,
	reply?: TurnReply,
): string[] {
	if (!isAgentEventType(type)) {
		throw new Error(
			`dot-agent-deck agent-event: unknown type "${type}". Expected one of: ${AGENT_EVENT_TYPES.join(", ")}.`,
		);
	}
	const argv = ["agent-event", "--type", type];
	// Issue #1567: FIRST among the flags on purpose. A CLI that predates the
	// declaration names the first argument it does not know in its usage
	// error, so it reports this one, and the extension steps down one level
	// (`REPORT_LEVELS`) instead of dropping straight to lifecycle-only.
	if (declarePromptReports) {
		argv.push(DECLARE_PROMPT_REPORTS_FLAG);
	}
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
	// PRD #1497: LAST, and only when there is text — an older CLI's refusal
	// of it is handled by `createReporter`, which drops the reply first. Only
	// the flags: the text goes on stdin (`turnReplyStdin`).
	if (turnReplyStdin(reply) !== undefined) {
		argv.push(TURN_REPLY_STDIN_FLAG);
		if (reply!.failed) {
			argv.push(TURN_REPLY_FAILED_FLAG);
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
 * `before_agent_start` carries a prompt submitted to an idle Pi before the
 * agent loop begins, `input` one submitted while Pi is busy (issue #1567),
 * `tool_execution_start` the tool name and its arguments, and
 * `tool_execution_end` marks the call finished; every handler's context
 * carries the session's `cwd`.
 */
export const DETAIL_EVENTS = [
	"before_agent_start",
	"input",
	"tool_execution_start",
	"tool_execution_end",
] as const;
export type DetailEvent = (typeof DETAIL_EVENTS)[number];

/**
 * The longest prompt put on argv. The deck keeps only the first 200 characters
 * of a reported prompt, so this bound only keeps a huge paste well clear of the
 * OS's per-argument limit; it never decides what the card shows.
 */
export const MAX_PROMPT_CHARS = 4000;

/** Cut `text` to at most `max` UTF-8 bytes, never splitting a code point. */
function clipUtf8(text: string, max: number): string {
	let bytes = 0;
	let end = 0;
	for (const char of text) {
		const size = char.codePointAt(0)! < 0x80 ? 1 : char.codePointAt(0)! < 0x800 ? 2 : char.codePointAt(0)! < 0x10000 ? 3 : 4;
		if (bytes + size > max) {
			return text.slice(0, end);
		}
		bytes += size;
		end += char.length;
	}
	return text;
}

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

/**
 * The final reply in one of Pi's messages (PRD #1497): the text blocks of an
 * assistant message, joined, with its thinking and tool calls left out, and
 * `failed` when the message ended in an error (`stopReason: "error"`). An
 * errored message with no text carries its `errorMessage` instead. `undefined`
 * for any other message, and for one with nothing to read.
 */
export function piAssistantReply(message: unknown): TurnReply | undefined {
	const record = asRecord(message);
	if (!record || record.role !== "assistant") {
		return undefined;
	}
	const blocks = Array.isArray(record.content) ? record.content : [];
	const text = blocks
		.map((block) => asRecord(block))
		.filter((block): block is Record<string, unknown> => block !== null && block.type === "text")
		.map((block) => nonBlankString(block.text)?.trim())
		.filter((part): part is string => part !== undefined)
		.join("\n\n");
	const failed = record.stopReason === "error";
	const said = text.length > 0 ? text : failed ? nonBlankString(record.errorMessage)?.trim() : undefined;
	return said === undefined ? undefined : { text: clipUtf8(said, MAX_TURN_REPLY_BYTES), failed };
}

/**
 * The stdin a report carrying `reply` writes for {@link TURN_REPLY_STDIN_FLAG}:
 * its text, at most {@link MAX_TURN_REPLY_BYTES}, or `undefined` when there is
 * nothing to read (no reply, or a blank one), in which case no reply flag is
 * sent either.
 */
export function turnReplyStdin(reply: TurnReply | undefined): string | undefined {
	return reply !== undefined && reply.text.trim().length > 0 ? clipUtf8(reply.text, MAX_TURN_REPLY_BYTES) : undefined;
}

/**
 * Keeps the last assistant reply of the run Pi is in, for its `agent_settled`
 * report (PRD #1497). `observe` is handed every `agent_start` and
 * `message_end` in the order Pi emits them: a run starting forgets the last
 * run's reply, and each assistant message replaces the one before, so what
 * `take` answers at `agent_settled` is the run's final reply. An assistant
 * message with nothing to read (only thinking or tool calls, empty, or an
 * error with no message) replaces it too, with nothing: the commentary an
 * earlier message carried is not the turn's reply. `take` clears it, so a
 * reply is reported once.
 */
export function createTurnReplyTracker(): {
	observe: (eventName: string, event: unknown) => void;
	take: () => TurnReply | undefined;
} {
	let last: TurnReply | undefined;
	return {
		observe(eventName, event) {
			if (eventName === "agent_start") {
				last = undefined;
			} else if (eventName === "message_end") {
				const message = asRecord(event)?.message;
				if (asRecord(message)?.role === "assistant") {
					last = piAssistantReply(message);
				}
			}
		},
		take() {
			const reply = last;
			last = undefined;
			return reply;
		},
	};
}

/** What the extension reports for one Pi event: the `--type` and its detail. */
export interface AgentEventReport {
	type: AgentEventType;
	detail: AgentEventDetail;
	/** A settled turn's final reply (PRD #1497), on `agent_settled` only. */
	reply?: TurnReply;
}

/**
 * Decide what to report for a Pi event, or `null` to report nothing. Every
 * report carries the session `cwd` (so the card keeps its directory however it
 * was created). Lifecycle events map through {@link piEventToAgentState}; the
 * detail events report the prompt, or the tool and its detail. A
 * `before_agent_start` with no usable prompt reports nothing — the
 * `agent_start` that follows still moves the card to Thinking — and so does an
 * `input` Pi is not going to queue (see the `input` arm).
 */
export function piEventReport(
	eventName: string,
	event: unknown,
	cwd: string | undefined,
	reply?: TurnReply,
): AgentEventReport | null {
	const detail: AgentEventDetail = {};
	const dir = nonBlankString(cwd);
	if (dir !== undefined) {
		detail.cwd = dir;
	}
	const state = piEventToAgentState(eventName);
	if (state) {
		// PRD #1497: only the settled turn carries its reply.
		return eventName === "agent_settled" && reply !== undefined ? { type: state, detail, reply } : { type: state, detail };
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
		case "input": {
			// Issue #1567: a prompt submitted while Pi is busy is QUEUED (Pi's
			// steer / follow-up queue) rather than started, so it never reaches
			// `before_agent_start`; measured on Pi 0.87.1, a prompt typed during
			// a running tool was submitted, acted on, and never reported. Pi sets
			// `streamingBehavior` on the `input` event exactly when it is busy, so
			// that is the one `input` reported here. An idle submission is left to
			// `before_agent_start`, which follows only once Pi has accepted it.
			if (payload.streamingBehavior !== "steer" && payload.streamingBehavior !== "followUp") {
				return null;
			}
			const prompt = nonBlankString(payload.text);
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
 * How much an `agent-event` report carries, most first (issue #1567):
 *
 *   declared  — the detail plus {@link DECLARE_PROMPT_REPORTS_FLAG};
 *   detail    — the detail alone (a deck from issue #622 up to #1567);
 *   lifecycle — the bare `--type <state>` every deck accepts.
 *
 * The extension starts at `declared` and steps down only when the deck's CLI
 * refused a flag as unknown ({@link isUnsupportedFlagFailure}).
 */
export const REPORT_LEVELS = ["declared", "detail", "lifecycle"] as const;
export type ReportLevel = (typeof REPORT_LEVELS)[number];

/**
 * The argv for `report` at `level`, or `null` when that level has nothing to
 * send for it — a detail report at `lifecycle`. The report's reply (PRD
 * #1497) is added only with `withReply`, and only at `declared`: the CLI that
 * takes it is newer than the declaration, so a deck that cannot take the
 * declaration cannot take the reply either.
 */
export function reportArgvAt(report: AgentEventReport, level: ReportLevel, withReply = false): string[] | null {
	switch (level) {
		case "declared":
			return buildAgentEventArgv(report.type, report.detail, true, withReply ? report.reply : undefined);
		case "detail":
			return buildAgentEventArgv(report.type, report.detail);
		case "lifecycle":
			return isAgentState(report.type) ? buildAgentEventArgv(report.type) : null;
	}
}

/** The level below `level`, or `null` below `lifecycle`. */
function levelBelow(level: ReportLevel): ReportLevel | null {
	const index = REPORT_LEVELS.indexOf(level);
	return index + 1 < REPORT_LEVELS.length ? REPORT_LEVELS[index + 1] : null;
}

/** A failed CLI run, keeping the exec result it came from. */
export class DeckExecError extends Error {
	readonly outcome: ExecOutcome;

	constructor(message: string, outcome: ExecOutcome) {
		super(message);
		this.outcome = outcome;
	}
}

/**
 * Send card reports at the highest level the deck's CLI accepts, and remember
 * it for the session. `run` shells the CLI and throws a {@link DeckExecError}
 * on a non-zero exit.
 *
 * A report refused because the CLI does not know one of its flags is sent
 * again one level down, and the session stays at the level that then got
 * through — so a deck that predates the declaration still gets the prompt and
 * tool detail it understands, and one that predates the detail still gets its
 * status. Any other failure (no daemon, a transient socket error) leaves the
 * level alone and retries a lifecycle report once, bare, so the card keeps its
 * status. Every report is best-effort: nothing here throws. `signal` is handed
 * to every `run` for that report.
 *
 * A settled turn's reply (PRD #1497) rides on top of whatever level the
 * session is at. The reply flags are the newest the extension sends, so a CLI
 * that refuses any flag of a report carrying a reply cannot take the reply:
 * the first such refusal drops the reply for the rest of the session and
 * sends the report again at the same level, and only a refusal of that steps
 * the level down. The levels themselves move exactly as they did before. A
 * report carrying the reply hands `run` its text as `stdin`
 * ({@link turnReplyStdin}); every other report hands it none.
 */
export function createReporter(
	run: (argv: string[], signal?: AbortSignal, stdin?: string) => Promise<unknown>,
): {
	send: (report: AgentEventReport, signal?: AbortSignal) => Promise<void>;
	level: () => ReportLevel;
	replies: () => boolean;
} {
	let level: ReportLevel = "declared";
	let replies = true;
	const send = async (report: AgentEventReport, signal?: AbortSignal): Promise<void> => {
		let tried: ReportLevel | null = level;
		let withReply = replies && report.reply !== undefined;
		while (tried !== null) {
			const argv = reportArgvAt(report, tried, withReply);
			if (argv === null) {
				return;
			}
			// The reply flags are on `argv` exactly when this is defined.
			const stdin = tried === "declared" && withReply ? turnReplyStdin(report.reply) : undefined;
			try {
				await (stdin === undefined ? run(argv, signal) : run(argv, signal, stdin));
				level = tried;
				return;
			} catch (err) {
				if (err instanceof DeckExecError && isUnsupportedFlagFailure(err.outcome)) {
					if (withReply) {
						withReply = false;
						replies = false;
						continue;
					}
					tried = levelBelow(tried);
					continue;
				}
				const fallback = legacyAgentEventArgv(report);
				if (fallback && tried !== "lifecycle") {
					await run(fallback, signal).catch(() => {});
				}
				return;
			}
		}
	};
	return { send, level: () => level, replies: () => replies };
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
 * Whether a failed report was the CLI refusing one of its flags as unknown —
 * clap's usage error, exit code 2 with stderr opening `error: unexpected
 * argument '--…` — i.e. the CLI is older than this extension. Only that may
 * switch the session to lifecycle-only reporting: a transient failure (daemon
 * restarting, socket busy) must not cost the card its detail for the rest of
 * the session. Reads the CLI's own exit code and stderr, never a message built
 * from the argv, so a directory or prompt that happens to contain the phrase
 * cannot trigger it.
 */
export function isUnsupportedFlagFailure(outcome: ExecOutcome): boolean {
	return outcome.code === 2 && /^error: unexpected argument '--/.test((outcome.stderr ?? "").trimStart());
}

/**
 * A queue that runs each task only after every earlier one has finished, in
 * the order they were enqueued, whatever each one's outcome. The extension
 * sends every card report through one, so a report — including an older-CLI
 * retry — can never reach the deck after a report Pi emitted later (a slow
 * `agent_start` retry landing after `agent_settled` would leave a settled card
 * on Thinking). Pi awaits each handler before emitting its next event today;
 * the queue keeps the order from depending on that.
 */
export function createSerialQueue(): <T>(task: () => Promise<T>) => Promise<T> {
	let tail: Promise<unknown> = Promise.resolve();
	return <T>(task: () => Promise<T>): Promise<T> => {
		const run = tail.then(task, task);
		tail = run.catch(() => undefined);
		return run;
	};
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

/** The part of a spawned child {@link execWithStdin} uses (Node's `ChildProcess`). */
export interface StdinChild {
	stdin: { on(event: string, listener: (...args: any[]) => void): unknown; end(data: string): unknown } | null;
	stdout: { on(event: string, listener: (...args: any[]) => void): unknown } | null;
	stderr: { on(event: string, listener: (...args: any[]) => void): unknown } | null;
	kill(signal?: "SIGTERM" | "SIGKILL"): boolean;
	once(event: string, listener: (...args: any[]) => void): unknown;
}

/** Node's `child_process.spawn`, as {@link execWithStdin} calls it. */
export type SpawnWithStdin = (
	command: string,
	args: string[],
	options: { cwd?: string; shell: false; stdio: ["pipe", "pipe", "pipe"]; windowsHide: boolean },
) => StdinChild;

/** How long {@link execWithStdin} waits for a SIGTERMed child before SIGKILL, as `pi.exec` does. */
const FORCE_KILL_AFTER_MS = 5000;
/** How long after `exit` it waits for the output pipes to close, as `pi.exec` does. */
const EXIT_STDIO_GRACE_MS = 100;

/**
 * Run `bin argv` and write `stdin` to it (PRD #1497), resolving with the same
 * `{code, stdout, stderr, killed}` `pi.exec` gives, which cannot write a
 * child's stdin. Spawned the way `pi.exec` spawns: an argv array, no shell,
 * in `cwd`, killed on `signal` (SIGTERM, then SIGKILL after 5s) or after
 * `timeout` ms when one is given. Rejects only when the child cannot be
 * spawned (e.g. ENOENT), which callers report as a spawn failure. A child that
 * exits without reading its stdin — an older CLI refusing a flag — is not an
 * error here: the write's EPIPE is ignored and its exit code answers. A child
 * killed by a signal reports a non-zero code.
 */
export function execWithStdin(
	spawn: SpawnWithStdin,
	bin: string,
	argv: string[],
	stdin: string,
	options: { signal?: AbortSignal; cwd?: string; timeout?: number } = {},
): Promise<{ code: number; stdout: string; stderr: string; killed: boolean }> {
	return new Promise((resolve, reject) => {
		let child: StdinChild;
		try {
			child = spawn(bin, argv, { cwd: options.cwd, shell: false, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
		} catch (err) {
			reject(err);
			return;
		}
		let stdout = "";
		let stderr = "";
		let killed = false;
		let settled = false;
		let exitCode: number | null = null;
		const timers: Array<ReturnType<typeof setTimeout>> = [];
		const kill = () => {
			if (killed) {
				return;
			}
			killed = true;
			child.kill("SIGTERM");
			timers.push(setTimeout(() => child.kill("SIGKILL"), FORCE_KILL_AFTER_MS));
		};
		const cleanup = () => {
			settled = true;
			for (const timer of timers) {
				clearTimeout(timer);
			}
			options.signal?.removeEventListener("abort", kill);
		};
		const finish = (code: number | null) => {
			if (settled) {
				return;
			}
			cleanup();
			resolve({ stdout, stderr, code: code ?? 1, killed });
		};
		child.once("error", (err: unknown) => {
			if (settled) {
				return;
			}
			cleanup();
			reject(err);
		});
		child.once("exit", (code: number | null) => {
			if (settled) {
				return;
			}
			exitCode = code;
			timers.push(setTimeout(() => finish(exitCode), EXIT_STDIO_GRACE_MS));
		});
		child.once("close", (code: number | null) => finish(code ?? exitCode));
		child.stdout?.on("data", (chunk: unknown) => {
			stdout += String(chunk);
		});
		child.stderr?.on("data", (chunk: unknown) => {
			stderr += String(chunk);
		});
		if (options.signal?.aborted) {
			kill();
		} else {
			options.signal?.addEventListener("abort", kill, { once: true });
		}
		if (options.timeout !== undefined && options.timeout > 0) {
			timers.push(setTimeout(kill, options.timeout));
		}
		// An older CLI exits on its usage error without reading stdin, and the
		// write then fails with EPIPE; its exit code is the answer, not this.
		child.stdin?.on("error", () => {});
		child.stdin?.end(stdin);
	});
}
