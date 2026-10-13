/**
 * dot-agent-deck orchestrator extension for Pi (PRD #201, M2.1 + M2.2).
 *
 * This is the thin Pi-API glue. It:
 *   1. registers `delegate` and `work_done` as native, schema-validated tools
 *      whose bodies shell the `dot-agent-deck` CLI, and
 *   2. subscribes to Pi's event bus and reports the pane's status
 *      (running / waiting / finished) and card detail (directory, submitted
 *      prompt, tool calls — issue #622) via `dot-agent-deck agent-event` — so
 *      a Pi pane's card is driven with NO Claude-Code hook installed and NO
 *      `~/.claude/settings.json` mutation.
 *
 * All the testable decisions (argv construction, event→state mapping, error
 * classification) live in the pure, import-free `./orchestrator.ts`. This file
 * only wires them to Pi. The CLI routes over the daemon socket using the pane
 * env vars the daemon already injects (DOT_AGENT_DECK_PANE_ID / _AGENT_ID /
 * _VIA_DAEMON); the extension does not set them.
 *
 * The `@earendil-works/pi-coding-agent` and `typebox` imports are resolved from
 * Pi's own runtime when the extension is loaded (Pi loads extensions via jiti),
 * which is why they are not dependencies of this package.
 */

import { spawn } from "node:child_process";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import {
	buildDelegateArgv,
	buildInboxAckArgv,
	buildInboxAnnounceArgv,
	buildInboxDoorbellFetchArgv,
	buildInboxWaitArgv,
	buildWorkDoneArgv,
	createLineSplitter,
	createReporter,
	createSerialQueue,
	createTurnReplyTracker,
	DeckExecError,
	doorbellIdsIn,
	execFailureMessage,
	execWithStdin,
	inboxIdsToAck,
	inboxUserText,
	nextWatcherBackoff,
	parseInboxFetch,
	parseInboxLine,
	piEventReport,
	resolveDeckBin,
	spawnFailureMessage,
	WATCHER_BACKOFF_MIN_MS,
	WATCHER_NOT_IN_DECK_EXIT,
	watcherBackoffOverride,
} from "./orchestrator.ts";

/**
 * The deck CLI this pane shells: the operator's absolute `DOT_AGENT_DECK_BIN`
 * (PRD #1497), else the absolute path the spawning deck exported in
 * `DOT_AGENT_DECK_EXE`, or the bare name for an older deck that did not (issue
 * #1385). Read once — the value is fixed for the life of the process.
 */
const deckBin = resolveDeckBin(process.env, process.platform);

/**
 * Shell `dot-agent-deck <argv>` via Pi's exec helper, or with `stdin` written
 * to it, run in `stdin.cwd` (the session's directory). Throws a clear Error on a
 * spawn failure (missing binary) or a non-zero exit, so tool callers surface
 * `isError` to the LLM. Returns the exec result on success.
 */
async function runDeck(
	pi: ExtensionAPI,
	argv: string[],
	signal: AbortSignal | undefined,
	stdin?: { text: string; cwd: string | undefined },
) {
	let outcome: { code: number; stdout: string; stderr: string; killed: boolean };
	try {
		// PRD #1497: `pi.exec` cannot write a child's stdin, so a report that
		// carries a turn's reply spawns the CLI itself and hands it the reply
		// there rather than on the command line.
		outcome =
			stdin === undefined
				? await pi.exec(deckBin, argv, { signal })
				: await execWithStdin(spawn, deckBin, argv, stdin.text, { signal, cwd: stdin.cwd });
	} catch (err) {
		throw new Error(spawnFailureMessage(argv, err, deckBin));
	}
	const failure = execFailureMessage(argv, outcome, deckBin);
	if (failure) {
		throw new DeckExecError(failure, outcome);
	}
	return outcome;
}

// --- PRD #1666: the deck's inbox ------------------------------------------

/** The part of a spawned watcher the inbox uses (Node's `ChildProcess`). */
interface WatcherChild {
	stdin: {
		write(line: string): unknown;
		end(): unknown;
		on?(event: string, listener: (...args: any[]) => void): unknown;
		writableEnded?: boolean;
		destroyed?: boolean;
	} | null;
	stdout: { setEncoding?(encoding: string): unknown; on(event: string, listener: (...args: any[]) => void): unknown } | null;
	on(event: string, listener: (...args: any[]) => void): unknown;
	kill(signal?: NodeJS.Signals): boolean;
}

/**
 * State shared by every instance of this extension in the process. Pi builds a
 * new instance on each session replacement (`/new`, fork, resume, reload) and
 * the old one's `pi` then throws on every call; the generation lets the old
 * instance's late callbacks do nothing at all, and `watcher` lets a new
 * instance stop a watcher an old one left behind. Kept on `globalThis` so a
 * reload that evaluates this module again still sees it.
 */
interface InboxShared {
	generation: number;
	watcher?: WatcherChild;
}
const INBOX_SHARED = Symbol.for("dot-agent-deck.pi-inbox");
function inboxShared(): InboxShared {
	const holder = globalThis as unknown as Record<symbol, InboxShared | undefined>;
	return (holder[INBOX_SHARED] ??= { generation: 0 });
}

/** Close a watcher's stdin (it exits on EOF), then SIGTERM it. */
function stopWatcher(child: WatcherChild | undefined): void {
	if (!child) {
		return;
	}
	try {
		child.stdin?.end();
	} catch {
		// Already closed.
	}
	try {
		child.kill("SIGTERM");
	} catch {
		// Already gone.
	}
}

/** How many doorbell-fetched ids an instance remembers until Pi commits them. */
const MAX_DOORBELL_IDS = 256;

/**
 * One extension instance's inbox: the watcher it runs for its session, the
 * hand-over of each message to Pi, and the acknowledgement once Pi commits it.
 *
 * - Every message goes in with `deliverAs: "followUp"`: when Pi is idle the
 *   option is ignored and a turn starts; when it is streaming the message
 *   waits for the turn to end. Without it, a send while streaming is dropped.
 * - A message is acknowledged when Pi starts the user message carrying it
 *   (`message_start`), never on `input`, which fires before Pi commits it: a
 *   queued follow-up can still be lost to Escape after `input`.
 * - When the deck rings the doorbell instead (no watcher was armed), the line
 *   is caught on `input` before the model sees it and the waiting content is
 *   handed over the same way.
 */
function createPiInbox(pi: ExtensionAPI) {
	let active = false;
	let generation = 0;
	let watcher: WatcherChild | undefined;
	let respawn: ReturnType<typeof setTimeout> | undefined;
	const fromDoorbell = new Set<string>();

	const current = () => active && generation === inboxShared().generation;

	const deliver = (text: string): void => {
		pi.sendUserMessage(text, { deliverAs: "followUp" });
	};

	const ackByCli = (id: string): void => {
		try {
			void pi.exec(deckBin, buildInboxAckArgv(id), { timeout: 5000 }).catch(() => undefined);
		} catch {
			// A stale `pi`: the message stays unacknowledged and is handed over again.
		}
	};

	const writable = (child: WatcherChild | undefined): child is WatcherChild =>
		!!child?.stdin && !child.stdin.writableEnded && !child.stdin.destroyed;

	/** The fixed respawn delay the environment sets, read at each session start. */
	let fixedBackoff: number | undefined;
	const nextBackoff = (current: number, ranForMs: number): number =>
		fixedBackoff ?? nextWatcherBackoff(current, ranForMs);

	const scheduleRespawn = (delay: number, next: number): void => {
		if (!current()) {
			return;
		}
		respawn = setTimeout(() => {
			respawn = undefined;
			try {
				spawnWatcher(next);
			} catch {
				// Never let a timer callback throw into Pi.
			}
		}, delay);
		try {
			(respawn as { unref?: () => void }).unref?.();
		} catch {
			// A timer without unref.
		}
	};

	const spawnWatcher = (backoff: number): void => {
		if (!current()) {
			return;
		}
		const started = Date.now();
		let child: WatcherChild;
		try {
			child = spawn(deckBin, buildInboxWaitArgv(), { stdio: ["pipe", "pipe", "inherit"] }) as unknown as WatcherChild;
		} catch {
			scheduleRespawn(backoff, nextBackoff(backoff, 0));
			return;
		}
		watcher = child;
		inboxShared().watcher = child;
		const lines = createLineSplitter();
		child.stdout?.setEncoding?.("utf8");
		child.stdout?.on("data", (chunk: unknown) => {
			try {
				if (!current() || watcher !== child) {
					return;
				}
				for (const line of lines(String(chunk))) {
					const message = parseInboxLine(line);
					if (!message || !current()) {
						continue;
					}
					try {
						deliver(inboxUserText([message]));
					} catch {
						// Not handed over, so not acknowledged: the deck hands it over again.
					}
				}
			} catch {
				// An uncaught throw in a child-process callback would end Pi.
			}
		});
		let gone = false;
		const exited = (code?: unknown) => {
			try {
				if (gone) {
					return;
				}
				gone = true;
				if (watcher === child) {
					watcher = undefined;
				}
				if (inboxShared().watcher === child) {
					inboxShared().watcher = undefined;
				}
				if (code === WATCHER_NOT_IN_DECK_EXIT) {
					return;
				}
				scheduleRespawn(backoff, nextBackoff(backoff, Date.now() - started));
			} catch {
				// An uncaught throw in a child-process callback would end Pi.
			}
		};
		child.on("exit", (code: unknown) => exited(code));
		child.on("error", () => exited());
		try {
			child.stdin?.on?.("error", () => undefined);
		} catch {
			// No stdin to watch.
		}
	};

	return {
		/** A session began in this instance: retire every older instance and arm a watcher. */
		start(): void {
			const shared = inboxShared();
			shared.generation += 1;
			generation = shared.generation;
			active = true;
			stopWatcher(shared.watcher);
			shared.watcher = undefined;
			fixedBackoff = watcherBackoffOverride(process.env);
			spawnWatcher(fixedBackoff ?? WATCHER_BACKOFF_MIN_MS);
		},

		/** This instance's session ended: stop its watcher and never respawn it. */
		stop(): void {
			active = false;
			if (respawn !== undefined) {
				clearTimeout(respawn);
				respawn = undefined;
			}
			const child = watcher;
			watcher = undefined;
			if (child && inboxShared().watcher === child) {
				inboxShared().watcher = undefined;
			}
			stopWatcher(child);
		},

		/**
		 * Pi committed `message`: acknowledge every deck message a user message
		 * carries — over the watcher's stdin, or with `dot-agent-deck ack` for
		 * content a doorbell fetched or when no watcher is running.
		 */
		committed(message: unknown): void {
			if (!current()) {
				return;
			}
			const ids = inboxIdsToAck(message);
			if (ids.length === 0) {
				return;
			}
			const viaCli = ids.filter((id) => fromDoorbell.delete(id));
			const viaWatcher = ids.filter((id) => !viaCli.includes(id));
			if (viaWatcher.length > 0) {
				let written = false;
				if (writable(watcher)) {
					try {
						watcher.stdin!.write(`${JSON.stringify({ ack: viaWatcher })}\n`);
						written = true;
					} catch {
						// Falls back to the CLI below.
					}
				}
				if (!written) {
					viaCli.push(...viaWatcher);
				}
			}
			for (const id of viaCli) {
				ackByCli(id);
			}
		},

		/**
		 * A submitted input that is nothing but the deck's doorbell: fetch what
		 * it announced without acknowledging it and hand it to Pi, so the model
		 * is never asked to fetch. Answers whether the input was consumed. A
		 * doorbell whose message was already handed over (a line recalled from
		 * history) is consumed with nothing to deliver. When the deck cannot be
		 * asked, the line goes to the model as typed, and it fetches.
		 */
		async interceptDoorbell(event: unknown, signal: AbortSignal | undefined): Promise<boolean> {
			if (!current()) {
				return false;
			}
			const input = event as { text?: unknown; source?: unknown } | undefined;
			if (input?.source === "extension") {
				return false;
			}
			const ids = doorbellIdsIn(input?.text);
			for (const id of ids) {
				let outcome: { code: number; stdout: string };
				try {
					outcome = await pi.exec(deckBin, buildInboxDoorbellFetchArgv(id), { signal, timeout: 5000 });
				} catch {
					return false;
				}
				const messages = outcome.code === 0 ? parseInboxFetch(outcome.stdout) : undefined;
				if (!messages || !current()) {
					return false;
				}
				if (messages.length === 0) {
					continue;
				}
				for (const message of messages) {
					fromDoorbell.add(message.id);
					while (fromDoorbell.size > MAX_DOORBELL_IDS) {
						fromDoorbell.delete(fromDoorbell.values().next().value as string);
					}
				}
				try {
					deliver(inboxUserText(messages));
				} catch {
					for (const message of messages) {
						fromDoorbell.delete(message.id);
					}
					return false;
				}
				return true;
			}
			return ids.length > 0;
		},
	};
}

/**
 * Run a Pi event handler so that nothing it throws reaches Pi: after a
 * session replacement the old instance's `pi` and `ctx` throw on every call,
 * and an extension's failure must never break the agent loop.
 */
function guarded<E, C, R>(handler: (event: E, ctx: C) => Promise<R> | R): (event: E, ctx: C) => Promise<R | undefined> {
	return async (event, ctx) => {
		try {
			return await handler(event, ctx);
		} catch {
			return undefined;
		}
	};
}

export default function orchestratorExtension(pi: ExtensionAPI): void {
	// --- M2.1: native delegate tool --------------------------------------
	pi.registerTool({
		name: "delegate",
		label: "Delegate",
		description:
			"Delegate a task to a worker role in the current dot-agent-deck orchestration. " +
			"The dot-agent-deck daemon routes the task to that role's agent pane. Orchestrator panes only.",
		promptSnippet: "Delegate a task to a dot-agent-deck worker role",
		promptGuidelines: [
			"Use delegate to hand a scoped task to a worker role instead of doing the work yourself when you are the orchestrator.",
		],
		parameters: Type.Object({
			role: Type.String({ description: 'Worker role name to delegate to (e.g. "coder").' }),
			task: Type.String({
				description: "Full task description with the context, file paths, and constraints the worker needs.",
			}),
		}),
		async execute(_toolCallId, params, signal) {
			const argv = buildDelegateArgv(params.role, params.task);
			await runDeck(pi, argv, signal);
			return {
				content: [{ type: "text", text: `Delegated task to role "${params.role}".` }],
				details: { role: params.role },
			};
		},
	});

	// --- M2.1: native work-done tool -------------------------------------
	pi.registerTool({
		name: "work_done",
		label: "Work Done",
		description:
			"Signal task completion back to the orchestrator via dot-agent-deck, with a summary of what was accomplished.",
		promptSnippet: "Report task completion back to the orchestrator",
		promptGuidelines: [
			"Use work_done when you have finished the delegated task, passing a concise summary of what changed.",
		],
		parameters: Type.Object({
			summary: Type.String({ description: "Summary of what was accomplished, including file paths and outcomes." }),
			done: Type.Optional(
				Type.Boolean({
					description: "Set true ONLY to signal the entire orchestration is complete (orchestrator only).",
				}),
			),
		}),
		async execute(_toolCallId, params, signal) {
			const argv = buildWorkDoneArgv(params.summary, params.done ?? false);
			await runDeck(pi, argv, signal);
			return {
				content: [{ type: "text", text: "Reported work-done to dot-agent-deck." }],
				details: { done: params.done ?? false },
			};
		},
	});

	// --- M2.2 / issue #622: event bus → card reports ---------------------
	// Reporting is best-effort: a failed report (e.g. no pane env vars, daemon
	// down) must never break the agent loop, so failures are swallowed here —
	// unlike the tools above, which surface errors to the LLM.
	//
	// Every report starts out carrying the detail and the prompt-report
	// declaration (issue #1567) and steps down only when the deck's CLI refuses
	// a flag as unknown — an older deck; see `createReporter`. A deck that
	// predates the declaration still gets the detail, and one that predates the
	// detail still gets the status.
	//
	// The session directory the last report was sent from, for the one report
	// spawned outside `pi.exec` (a settled turn's reply on stdin), which runs
	// there the way `pi.exec` runs a command in the session's directory.
	let reportCwd: string | undefined;
	const reporter = createReporter((argv, signal, stdin) =>
		runDeck(pi, argv, signal, stdin === undefined ? undefined : { text: stdin, cwd: reportCwd }),
	);
	// Every report, with its retries, runs to completion before the next one
	// starts, so the deck receives them in the order Pi emitted them.
	const inOrder = createSerialQueue();
	// PRD #1497: the run's last assistant reply, attached to `agent_settled`.
	// Observed synchronously in each handler, so it follows Pi's event order.
	const replies = createTurnReplyTracker();
	const report = (eventName: string, event: unknown, ctx: ExtensionContext): Promise<void> => {
		replies.observe(eventName, event);
		const reply = eventName === "agent_settled" ? replies.take() : undefined;
		return inOrder(async () => {
			const decided = piEventReport(eventName, event, ctx.cwd, reply);
			if (!decided) {
				return;
			}
			reportCwd = ctx.cwd;
			await reporter.send(decided, ctx.signal);
		});
	};

	// --- PRD #1666: the deck's inbox -----------------------------------------
	// Every message for this agent — its first task included — arrives through
	// the watcher this instance runs for its session. The deck no longer types
	// a prompt into the pane, and `get-seed` is no longer asked: it would
	// acknowledge the seed before Pi committed it.
	const inbox = createPiInbox(pi);

	pi.on(
		"session_start",
		guarded(async (event, ctx) => {
			// The watcher first, then the announcement that holds the doorbell
			// off until it arms, then the status (Idle — a Pi pane awaiting its
			// first prompt is idle, matching every other backend's
			// session-start), so the deck never sees an idle, unarmed Pi.
			inbox.start();
			try {
				await pi.exec(deckBin, buildInboxAnnounceArgv(), { signal: ctx.signal, timeout: 5000 });
			} catch {
				// An older deck, or none: the doorbell still reaches this pane.
			}
			await report("session_start", event, ctx);
		}),
	);
	// Issue #622: the prompt lands before the agent loop begins, so `Prmt:`
	// appears as the turn starts; each tool call shows as the active tool and
	// counts once it finishes. Awaited like the lifecycle reports, so a tool's
	// end can never reach the deck ahead of its start.
	pi.on(
		"before_agent_start",
		guarded(async (event, ctx) => {
			await report("before_agent_start", event, ctx);
		}),
	);
	// PRD #1666: a submitted doorbell line is consumed here and its content
	// handed over instead. Issue #1567: otherwise, a prompt submitted while Pi
	// is busy is queued and never reaches `before_agent_start`;
	// `piEventReport` reports exactly that `input` and ignores every other.
	// Returns nothing then, so Pi carries on with the input unchanged.
	pi.on(
		"input",
		guarded(async (event, ctx) => {
			if (await inbox.interceptDoorbell(event, ctx.signal)) {
				return { action: "handled" as const };
			}
			await report("input", event, ctx);
			return undefined;
		}),
	);
	// PRD #1666: the user message carrying a deck message has been committed
	// — the acknowledgement.
	pi.on(
		"message_start",
		guarded((event) => {
			inbox.committed(event.message);
		}),
	);
	pi.on(
		"agent_start",
		guarded(async (event, ctx) => {
			await report("agent_start", event, ctx);
		}),
	);
	pi.on(
		"tool_execution_start",
		guarded(async (event, ctx) => {
			await report("tool_execution_start", event, ctx);
		}),
	);
	pi.on(
		"tool_execution_end",
		guarded(async (event, ctx) => {
			await report("tool_execution_end", event, ctx);
		}),
	);
	// PRD #1497: each finished message, for the settled turn's final reply.
	// Reports nothing by itself.
	pi.on(
		"message_end",
		guarded((event) => {
			replies.observe("message_end", event);
		}),
	);
	pi.on(
		"agent_settled",
		guarded(async (event, ctx) => {
			await report("agent_settled", event, ctx);
		}),
	);
	pi.on(
		"session_shutdown",
		guarded(async (event, ctx) => {
			inbox.stop();
			await report("session_shutdown", event, ctx);
		}),
	);
}
