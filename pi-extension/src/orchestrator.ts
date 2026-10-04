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

// ---------------------------------------------------------------------------
// PRD #1542: answering another extension's dialog from the deck
// ---------------------------------------------------------------------------

/**
 * A dialog another Pi extension raised, as `dot-agent-deck await-answer --agent
 * pi --question <json>` takes it (the Rust `question::PiDialog`). MUST stay in
 * sync with that struct.
 */
export interface PiDialog {
	id: string;
	kind: "select" | "confirm" | "input";
	title: string;
	message?: string;
	options?: string[];
	placeholder?: string;
}

/** The argv that holds `dialog` with the deck until it answers. */
export function buildAwaitAnswerArgv(dialog: PiDialog): string[] {
	return ["await-answer", "--agent", "pi", "--question", JSON.stringify(dialog)];
}

/**
 * The deck's answer from `await-answer`'s stdout: `{ value }` on its one line,
 * or `undefined` — print-nothing, an unparseable line, or a value of the wrong
 * type for the dialog — which means "leave the dialog to the keyboard".
 */
export function parseAwaitAnswer(
	kind: PiDialog["kind"],
	stdout: string | undefined | null,
): { value: string | boolean } | undefined {
	const line = (stdout ?? "").split("\n").find((l) => l.trim().length > 0);
	if (!line) {
		return undefined;
	}
	let parsed: unknown;
	try {
		parsed = JSON.parse(line);
	} catch {
		return undefined;
	}
	if (!parsed || typeof parsed !== "object" || !("value" in parsed)) {
		return undefined;
	}
	const value = (parsed as { value: unknown }).value;
	if (kind === "confirm" ? typeof value === "boolean" : typeof value === "string") {
		return { value: value as string | boolean };
	}
	return undefined;
}

/** What the wrapper needs from Pi: run the deck CLI, mint an id. */
export interface QuestionDeps {
	exec(argv: string[], signal: AbortSignal): Promise<{ code: number; stdout: string }>;
	mintId(): string;
}

/** The part of Pi's shared `ctx.ui` object the wrapper replaces. */
export interface DialogUi {
	select?: (title: string, options: string[], opts?: DialogOptions) => Promise<unknown>;
	confirm?: (title: string, message?: string, opts?: DialogOptions) => Promise<unknown>;
	input?: (title: string, placeholder?: string, opts?: DialogOptions) => Promise<unknown>;
}

/** Pi's `ExtensionUIDialogOptions`, as far as the wrapper reads it. */
export interface DialogOptions {
	signal?: AbortSignal;
	[key: string]: unknown;
}

const WRAPPED = "__dotAgentDeckQuestionWrapper";

/**
 * Race the original dialog against the deck's answer. Whichever settles first
 * wins: a deck answer aborts the dialog through the signal Pi documents for
 * dismissing it programmatically, and a keyboard answer aborts the
 * `await-answer` child, whose closed connection tells the deck the question is
 * gone. A caller's own signal still dismisses the dialog.
 */
function raceDialog(
	open: (signal: AbortSignal) => Promise<unknown>,
	dialog: PiDialog,
	deps: QuestionDeps,
	callerSignal: AbortSignal | undefined,
): Promise<unknown> {
	const dialogAbort = new AbortController();
	const execAbort = new AbortController();
	if (callerSignal) {
		if (callerSignal.aborted) {
			dialogAbort.abort();
		} else {
			callerSignal.addEventListener("abort", () => dialogAbort.abort(), { once: true });
		}
	}
	return new Promise((resolve, reject) => {
		let settled = false;
		let keyboard: Promise<unknown>;
		try {
			keyboard = open(dialogAbort.signal);
		} catch (err) {
			reject(err);
			return;
		}
		keyboard.then(
			(value) => {
				if (!settled) {
					settled = true;
					execAbort.abort();
					resolve(value);
				}
			},
			(err) => {
				if (!settled) {
					settled = true;
					execAbort.abort();
					reject(err);
				}
			},
		);
		let deck: Promise<{ code: number; stdout: string }>;
		try {
			deck = deps.exec(buildAwaitAnswerArgv(dialog), execAbort.signal);
		} catch {
			return;
		}
		deck.then(
			(outcome) => {
				if (settled) {
					return;
				}
				const answer = parseAwaitAnswer(dialog.kind, outcome?.stdout);
				if (!answer) {
					return;
				}
				settled = true;
				dialogAbort.abort();
				resolve(answer.value);
			},
			() => {
				// No deck, or the child was stopped: the keyboard answers.
			},
		);
	});
}

/**
 * PRD #1542: replace `select`, `confirm` and `input` on Pi's shared `ctx.ui`
 * with wrappers that let the deck answer them. This rests on behaviour Pi does
 * NOT document — the object is shared by every extension and its methods are
 * writable [observed on 0.87.1] — so every step is guarded: anything missing or
 * read-only leaves that method alone, and the function never throws. Returns
 * how many methods it wrapped. Idempotent: a method already wrapped is left as
 * it is, so re-applying on every `session_start` (Pi rebuilds the object when
 * it rebinds its UI) is safe.
 */
export function installQuestionWrappers(ui: unknown, deps: QuestionDeps): number {
	if (!ui || typeof ui !== "object") {
		return 0;
	}
	const target = ui as DialogUi & Record<string, unknown>;
	let wrapped = 0;
	const replace = (name: "select" | "confirm" | "input", wrapper: (...args: never[]) => Promise<unknown>) => {
		try {
			const original = target[name];
			if (typeof original !== "function" || (original as unknown as Record<string, unknown>)[WRAPPED]) {
				return;
			}
			Object.defineProperty(wrapper, WRAPPED, { value: true });
			target[name] = wrapper as never;
			if (target[name] === wrapper) {
				wrapped += 1;
			}
		} catch {
			// Read-only or otherwise unwritable: leave Pi's own method alone.
		}
	};
	const select = target.select;
	if (typeof select === "function") {
		replace("select", ((title: string, options: string[], opts?: DialogOptions) =>
			raceDialog(
				(signal) => select.call(ui, title, options, { ...opts, signal }),
				{ id: deps.mintId(), kind: "select", title: String(title ?? ""), options: (options ?? []).map(String) },
				deps,
				opts?.signal,
			)) as never);
	}
	const confirm = target.confirm;
	if (typeof confirm === "function") {
		replace("confirm", ((title: string, message?: string, opts?: DialogOptions) =>
			raceDialog(
				(signal) => confirm.call(ui, title, message, { ...opts, signal }),
				{
					id: deps.mintId(),
					kind: "confirm",
					title: String(title ?? ""),
					...(typeof message === "string" ? { message } : {}),
				},
				deps,
				opts?.signal,
			)) as never);
	}
	const input = target.input;
	if (typeof input === "function") {
		replace("input", ((title: string, placeholder?: string, opts?: DialogOptions) =>
			raceDialog(
				(signal) => input.call(ui, title, placeholder, { ...opts, signal }),
				{
					id: deps.mintId(),
					kind: "input",
					title: String(title ?? ""),
					...(typeof placeholder === "string" ? { placeholder } : {}),
				},
				deps,
				opts?.signal,
			)) as never);
	}
	return wrapped;
}
