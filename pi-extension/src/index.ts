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
	inboxIdsIn,
	inboxIdsToAck,
	INBOX_ACK_CONFIRM_ATTEMPTS,
	INBOX_ACK_CONFIRM_DELAY_MS,
	INBOX_ACK_CONFIRM_MAX_DELAY_MS,
	INBOX_ADMISSION_RECHECK_MS,
	type InboxMessage,
	inboxUserText,
	nextWatcherBackoff,
	parseInboxAckReply,
	parseInboxFetch,
	parseInboxLine,
	installQuestionWrappers,
	piEventReport,
	resolveDeckBin,
	spawnFailureMessage,
	tagHandOver,
	untagHandOver,
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
/** How many committed message ids an instance remembers, to consume a late hand-over of one. */
const MAX_COMMITTED_IDS = 256;

/** One hand-over of deck messages to Pi, from `sendUserMessage` until Pi commits it or another replaces it. */
interface HandOver {
	/** Names this hand-over on the first line of the text Pi's `input` hooks see. */
	attempt: string;
	messages: InboxMessage[];
	/** The text Pi goes on with once this extension's `input` hook removed that line. */
	text: string;
	/** Runs Pi had started when it was handed over. */
	run: number;
	/**
	 * How far Pi has taken it, as far as this extension can see: `handed` until
	 * its `input` hook sees it, `input` after that, and `start` once its
	 * `before_agent_start` hook has seen it.
	 */
	stage: "handed" | "input" | "start";
	/** At `start`: a run settled while another one was active, the way Pi settles a prompt it refused. */
	suspect: boolean;
}

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
 * - Pi admits a prompt only after its input hooks and preflight resolve, and
 *   refuses a second one that is still in preflight when the first starts —
 *   reporting the refusal itself, never to the caller. So while Pi is idle at
 *   most one hand-over is in flight, and later messages wait until it commits;
 *   while Pi is busy each one waits as a follow-up, which Pi queues safely.
 * - Each hand-over names its attempt on a first line that this extension's
 *   `input` hook removes. The hook consumes a hand-over another one replaced,
 *   and one whose messages Pi already committed, so neither reaches the model.
 * - A hand-over the `input` hook has not seen can be handed over again safely:
 *   once Pi starts a run after it and settles with nothing queued, or once Pi
 *   stays idle with nothing queued for `INBOX_ADMISSION_RECHECK_MS`.
 * - A hand-over the hook has seen is past that guard, so it is handed over
 *   again only on a sign that Pi refused it: Pi settles a refused prompt while
 *   the run that beat it is still active, which a run's own end never does. So
 *   after `before_agent_start` saw it, a settle during an active run and then
 *   a settle with no run, with no commit of it between, is that sign. Without
 *   one it waits for the deck, which hands an unacknowledged message over
 *   again: that redelivery replaces it once Pi has nothing queued.
 */
function createPiInbox(pi: ExtensionAPI) {
	let active = false;
	let generation = 0;
	let watcher: WatcherChild | undefined;
	let respawn: ReturnType<typeof setTimeout> | undefined;
	const fromDoorbell = new Set<string>();
	const committedIds = new Set<string>();
	/** The latest context Pi handed this instance, to ask whether it is idle. */
	let context: ExtensionContext | undefined;
	/** Runs Pi started in this session (`agent_start`), to date each hand-over. */
	let runs = 0;
	/** Names this instance's hand-overs apart from any other instance's. */
	const instance = Math.random().toString(36).slice(2, 10) || "0";
	let attempts = 0;
	/** Handed to Pi and not yet committed or replaced, in the order they were handed over. */
	let outstanding: HandOver[] = [];
	/** The hand-over made while Pi was idle, until it commits or is replaced. */
	let admitting: HandOver | undefined;
	/** Waiting for `admitting` to settle, in inbox order. */
	let held: InboxMessage[][] = [];
	let recheck: ReturnType<typeof setTimeout> | undefined;
	/**
	 * Committed ids whose acknowledgement the deck has not confirmed, with the
	 * confirmations asked so far. The watcher's stdin answers nothing, and the
	 * deck may never hand a message over again (it ran out of hand-overs, or
	 * allows one), so each is confirmed with the CLI until the deck records it.
	 */
	const unconfirmed = new Map<string, number>();
	let confirmTimer: ReturnType<typeof setTimeout> | undefined;
	let confirming = false;
	let confirmAgain = false;

	const current = () => active && generation === inboxShared().generation;

	/** Pi is idle, as far as it can be asked; an unanswerable Pi counts as idle, which only holds messages back. */
	const piIdle = (): boolean => askIdle() ?? true;
	/** Pi's answers, or `undefined` when it cannot be asked: never taken as a reason to hand anything over again. */
	const askIdle = (): boolean | undefined => {
		try {
			return context?.isIdle();
		} catch {
			return undefined;
		}
	};
	const askQueued = (): boolean | undefined => {
		try {
			return context?.hasPendingMessages?.();
		} catch {
			return undefined;
		}
	};
	/** An agent run is active: Pi's `signal` is set only while one is. */
	const askRunning = (ctx: ExtensionContext): boolean | undefined => {
		try {
			return ctx.signal !== undefined;
		} catch {
			return undefined;
		}
	};

	const isHeld = (id: string): boolean => held.some((batch) => batch.some((m) => m.id === id));
	const carrying = (id: string): HandOver | undefined => outstanding.find((o) => o.messages.some((m) => m.id === id));
	const unseen = (): HandOver[] => outstanding.filter((o) => o.stage === "handed");

	const clearRecheck = (): void => {
		if (recheck !== undefined) {
			clearTimeout(recheck);
			recheck = undefined;
		}
	};

	/** (Re)start the idle recheck: armed from the latest hand-over while one the `input` hook has not seen remains. */
	const armRecheck = (): void => {
		clearRecheck();
		if (!current() || unseen().length === 0) {
			return;
		}
		recheck = setTimeout(() => {
			recheck = undefined;
			try {
				if (!current() || unseen().length === 0) {
					return;
				}
				if (askIdle() === true && askQueued() === false) {
					replace(unseen());
				} else {
					armRecheck();
				}
			} catch {
				// Never let a timer callback throw into Pi.
			}
		}, INBOX_ADMISSION_RECHECK_MS);
		try {
			(recheck as { unref?: () => void }).unref?.();
		} catch {
			// A timer without unref.
		}
	};

	/** Hand `messages` to Pi as one user message. False when Pi threw at once: nothing was handed over. */
	const send = (messages: InboxMessage[]): boolean => {
		const idle = piIdle();
		attempts += 1;
		const handed: HandOver = {
			attempt: `${instance}.${attempts}`,
			messages,
			text: inboxUserText(messages),
			run: runs,
			stage: "handed",
			suspect: false,
		};
		// Pi can run this extension's `input` hook before `sendUserMessage`
		// returns, so the hand-over is on record first.
		outstanding.push(handed);
		try {
			pi.sendUserMessage(tagHandOver(handed.attempt, handed.text), { deliverAs: "followUp" });
		} catch {
			outstanding = outstanding.filter((o) => o !== handed);
			return false;
		}
		if (idle && outstanding.includes(handed)) {
			admitting = handed;
		}
		armRecheck();
		return true;
	};

	/** Hand over what is held, until a hand-over to an idle Pi has to settle first. */
	const flush = (): void => {
		while (current() && admitting === undefined && held.length > 0) {
			// Not handed over, so not acknowledged: the deck hands it over again.
			send(held.shift()!);
		}
	};

	/**
	 * These hand-overs will not commit: hand their messages over again, ahead
	 * of anything held. Each one is forgotten, so its own late arrival at the
	 * `input` hook is consumed there.
	 */
	const replace = (stale: HandOver[]): void => {
		if (stale.length === 0) {
			return;
		}
		outstanding = outstanding.filter((o) => !stale.includes(o));
		if (admitting !== undefined && stale.includes(admitting)) {
			admitting = undefined;
		}
		held = [...stale.map((o) => o.messages), ...held];
		armRecheck();
		flush();
	};

	/**
	 * Give `messages` to Pi, once each. A message already held, or carried by
	 * a hand-over that may still commit, is not handed over again, and the
	 * deck is told it is held (`hold`), so a turn longer than the deck's
	 * hand-over limit does not use that limit up. One the deck hands over again
	 * while its hand-over is past the `input` hook and Pi has nothing queued
	 * replaces that hand-over, and is not a hold: the hand-over it replaces did
	 * not reach the agent. One Pi already committed is acknowledged again and
	 * not handed over: the deck sends it again only because it never recorded
	 * the acknowledgement. False only when Pi threw at once.
	 */
	const deliver = (messages: InboxMessage[]): boolean => {
		const fresh: InboxMessage[] = [];
		const stale = new Set<HandOver>();
		const holding: string[] = [];
		const committedAgain: string[] = [];
		for (const m of messages) {
			if (committedIds.has(m.id)) {
				committedAgain.push(m.id);
				continue;
			}
			if (isHeld(m.id)) {
				holding.push(m.id);
				continue;
			}
			const handed = carrying(m.id);
			if (handed === undefined) {
				fresh.push(m);
			} else if (handed.stage !== "handed" && askQueued() === false) {
				stale.add(handed);
			} else {
				holding.push(m.id);
			}
		}
		if (holding.length > 0 && writable(watcher)) {
			try {
				watcher.stdin!.write(`${JSON.stringify({ hold: holding })}\n`);
			} catch {
				// The deck counts the redelivery, as it does without a hold.
			}
		}
		// Pi already committed these: the deck hands them over again because
		// their acknowledgement never reached it (the watcher exited with it
		// unforwarded, or the CLI failed). They are acknowledged again, never
		// handed to Pi a second time.
		for (const id of committedAgain) {
			fromDoorbell.delete(id);
		}
		acknowledge(committedAgain);
		replace([...stale]);
		if (fresh.length === 0) {
			return true;
		}
		if (admitting !== undefined) {
			held.push(fresh);
			return true;
		}
		return send(fresh);
	};

	/** Pi committed these ids: their hand-overs are done, and a settled admission lets the held ones go. */
	const settleCommitted = (ids: string[]): void => {
		for (const id of ids) {
			committedIds.add(id);
			while (committedIds.size > MAX_COMMITTED_IDS) {
				committedIds.delete(committedIds.values().next().value as string);
			}
		}
		for (const o of outstanding) {
			o.messages = o.messages.filter((m) => !ids.includes(m.id));
		}
		outstanding = outstanding.filter((o) => o.messages.length > 0);
		if (admitting !== undefined && admitting.messages.length === 0) {
			admitting = undefined;
		}
		if (unseen().length === 0) {
			clearRecheck();
		}
		flush();
	};

	const clearConfirm = (): void => {
		if (confirmTimer !== undefined) {
			clearTimeout(confirmTimer);
			confirmTimer = undefined;
		}
	};

	/** Confirm the unconfirmed acknowledgements after `delay`, replacing any confirmation already due. */
	const scheduleConfirm = (delay: number): void => {
		clearConfirm();
		if (!current() || unconfirmed.size === 0) {
			return;
		}
		confirmTimer = setTimeout(() => {
			confirmTimer = undefined;
			void confirm();
		}, delay);
		try {
			(confirmTimer as { unref?: () => void }).unref?.();
		} catch {
			// A timer without unref.
		}
	};

	/** Tell the user in Pi that the deck could not record message `id` as delivered. */
	const reportUnconfirmed = (id: string, why: string): void => {
		try {
			context?.ui?.notify?.(
				`dot-agent-deck could not record message ${id} as delivered (${why}). Pi received it; the deck may show it as not delivered.`,
				"warning",
			);
		} catch {
			// A stale context: nothing more to tell.
		}
	};

	/**
	 * Acknowledge every unconfirmed id with `dot-agent-deck ack --json`, one at
	 * a time. An id is done once the deck records it, or refuses it for good,
	 * or after `INBOX_ACK_CONFIRM_ATTEMPTS` unanswered asks; the last two are
	 * reported to the user. The rest are asked again after a backoff.
	 */
	const confirm = async (): Promise<void> => {
		if (confirming) {
			confirmAgain = true;
			return;
		}
		clearConfirm();
		confirming = true;
		try {
			for (const [id, asked] of [...unconfirmed]) {
				if (!current()) {
					return;
				}
				let stdout: unknown;
				try {
					stdout = (await pi.exec(deckBin, buildInboxAckArgv(id), { timeout: 5000 })).stdout;
				} catch {
					// A stale `pi`, or the CLI did not run: not confirmed.
				}
				if (!unconfirmed.has(id)) {
					continue;
				}
				const outcome = parseInboxAckReply(stdout);
				if (outcome === "recorded") {
					unconfirmed.delete(id);
				} else if (outcome !== "unconfirmed") {
					unconfirmed.delete(id);
					reportUnconfirmed(id, outcome.refused);
				} else if (asked + 1 >= INBOX_ACK_CONFIRM_ATTEMPTS) {
					unconfirmed.delete(id);
					reportUnconfirmed(id, "the deck did not confirm it");
				} else {
					unconfirmed.set(id, asked + 1);
				}
			}
		} finally {
			confirming = false;
		}
		if (confirmAgain) {
			confirmAgain = false;
			void confirm();
			return;
		}
		if (unconfirmed.size > 0) {
			const fewest = Math.min(...unconfirmed.values());
			scheduleConfirm(Math.min(INBOX_ACK_CONFIRM_DELAY_MS * 2 ** fewest, INBOX_ACK_CONFIRM_MAX_DELAY_MS));
		}
	};

	/**
	 * Acknowledge `ids` over the watcher's stdin, or with the CLI for `viaCli`
	 * and when no watcher takes the line, and keep each until the deck confirms
	 * it ([`confirm`]): at once when the CLI carries it, else after
	 * `INBOX_ACK_CONFIRM_DELAY_MS`.
	 */
	const acknowledge = (ids: string[], viaCli: string[] = []): void => {
		let written = false;
		if (ids.length > 0 && writable(watcher)) {
			try {
				watcher.stdin!.write(`${JSON.stringify({ ack: ids })}\n`);
				written = true;
			} catch {
				// Confirmed with the CLI below.
			}
		}
		for (const id of [...ids, ...viaCli]) {
			if (!unconfirmed.has(id)) {
				unconfirmed.set(id, 0);
			}
		}
		if (viaCli.length > 0 || (ids.length > 0 && !written)) {
			void confirm();
		} else if (ids.length > 0) {
			scheduleConfirm(INBOX_ACK_CONFIRM_DELAY_MS);
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
					// When Pi throws, nothing is handed over or acknowledged: the deck hands it over again.
					deliver([message]);
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
		start(ctx: ExtensionContext): void {
			context = ctx;
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
			clearRecheck();
			clearConfirm();
			unconfirmed.clear();
			outstanding = [];
			admitting = undefined;
			held = [];
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
			acknowledge(
				ids.filter((id) => !viaCli.includes(id)),
				viaCli,
			);
			settleCommitted(ids);
		},

		/** Pi started a run: a hand-over from before it that never commits was refused. */
		runStarted(ctx: ExtensionContext): void {
			context = ctx;
			runs += 1;
		},

		/**
		 * One of this instance's hand-overs reached Pi's `input` hooks. A
		 * hand-over that was replaced, or whose messages Pi already committed,
		 * is consumed; any other one goes on without the line naming it.
		 * `undefined` for an input that is not a hand-over.
		 */
		handOverInput(event: unknown): { action: "handled" } | { action: "transform"; text: string } | undefined {
			const input = event as { text?: unknown; source?: unknown } | undefined;
			if (input?.source !== "extension") {
				return undefined;
			}
			const tagged = untagHandOver(input.text);
			if (!tagged) {
				return undefined;
			}
			const handed = current() ? outstanding.find((o) => o.attempt === tagged.attempt) : undefined;
			if (handed === undefined) {
				const ids = inboxIdsIn(tagged.text);
				if (tagged.attempt.startsWith(`${instance}.`) || (ids.length > 0 && ids.every((id) => committedIds.has(id)))) {
					return { action: "handled" };
				}
				return { action: "transform", text: tagged.text };
			}
			if (handed.stage === "handed") {
				handed.stage = "input";
				armRecheck();
			}
			return { action: "transform", text: tagged.text };
		},

		/** Pi's `before_agent_start` hooks reached `prompt`: a hand-over carrying it is about to run or be refused. */
		starting(prompt: unknown): void {
			if (!current()) {
				return;
			}
			const handed = outstanding.find((o) => o.stage === "input" && o.text === prompt);
			if (handed !== undefined) {
				handed.stage = "start";
				handed.suspect = false;
			}
		},

		/**
		 * Pi settled a run, or a prompt it refused. A refused prompt settles
		 * while the run that beat it is still active; a run's own end settles
		 * with none. A hand-over past `before_agent_start` that sees the first
		 * and then the second without committing was refused. With nothing
		 * queued, a hand-over the `input` hook never saw and that was made
		 * before the latest run started is handed over again too.
		 */
		settled(ctx: ExtensionContext): void {
			context = ctx;
			if (!current()) {
				return;
			}
			const running = askRunning(ctx);
			const refused: HandOver[] = [];
			for (const o of outstanding) {
				if (o.stage !== "start") {
					continue;
				}
				if (running === true) {
					o.suspect = true;
				} else if (running === false && o.suspect) {
					refused.push(o);
				}
			}
			if (askQueued() === false) {
				refused.push(...unseen().filter((o) => o.run < runs));
			}
			replace(refused);
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
				if (!deliver(messages)) {
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
	// Every report starts out carrying the detail and steps down only when the
	// deck's CLI refuses a flag as unknown — an older deck; see
	// `createReporter`. A deck that predates the detail still gets the status.
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

	// --- PRD #1631: answer other extensions' dialogs from the deck --------
	// Pi's `ctx.ui` is one object shared by every extension and is rebuilt when
	// Pi rebinds its UI, so the wrappers are (re)applied on every
	// session_start. Only inside a deck pane, where `await-answer` has a daemon
	// to talk to. Best-effort and guarded: a Pi whose `ctx.ui` is not what
	// 0.87.1 had leaves its dialogs to the keyboard, never crashes.
	const wrapDialogs = (ctx: ExtensionContext): void => {
		if (!process.env.DOT_AGENT_DECK_PANE_ID) {
			return;
		}
		try {
			installQuestionWrappers((ctx as unknown as { ui?: unknown }).ui, {
				exec: (argv, signal) => pi.exec(deckBin, argv, { signal }),
				mintId: () => `q-${globalThis.crypto.randomUUID().replace(/-/g, "")}`,
			});
		} catch {
			// Unsupported on this Pi: the keyboard answers.
		}
	};

	pi.on(
		"session_start",
		guarded(async (event, ctx) => {
			// The watcher first, then the announcement that holds the doorbell
			// off until it arms, then the status (Idle — a Pi pane awaiting its
			// first prompt is idle, matching every other backend's
			// session-start), so the deck never sees an idle, unarmed Pi.
			wrapDialogs(ctx);
			inbox.start(ctx);
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
			try {
				await report("before_agent_start", event, ctx);
			} finally {
				// Once its own hook has run, a hand-over goes on to run or be refused.
				inbox.starting(event.prompt);
			}
		}),
	);
	// PRD #1666: the extension's own hand-over is consumed here when another
	// replaced it, and otherwise goes on without the line naming it; a
	// submitted doorbell line is consumed and its content handed over instead.
	// Issue #1567: otherwise, a prompt submitted while Pi is busy is queued and
	// never reaches `before_agent_start`; `piEventReport` reports exactly that
	// `input` and ignores every other. Returns nothing then, so Pi carries on
	// with the input unchanged.
	pi.on(
		"input",
		guarded(async (event, ctx) => {
			const handOver = inbox.handOverInput(event);
			if (handOver?.action === "handled") {
				return handOver;
			}
			if (await inbox.interceptDoorbell(event, ctx.signal)) {
				return { action: "handled" as const };
			}
			await report("input", handOver ? { ...event, text: handOver.text } : event, ctx);
			return handOver;
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
			inbox.runStarted(ctx);
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
			inbox.settled(ctx);
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
